//! Catalog writes driven by the sync engine: sources, libraries, and paged
//! upserts of artists, albums and tracks with generation-based mark and sweep.

use std::collections::{HashMap, HashSet};

use rsub_core::backend::{
    AlbumRecord, ArtistRecord, CreditRecord, CreditRole, RemoteLibrary, RemoteRef, TagKind,
    TrackEnrichment, TrackRecord,
};
use rsub_core::identity::{
    album_tag_key, artist_key, name_key, plex_key, rating_key, release_key, release_track_key,
    track_tag_key,
};
use rsub_core::tags::FileTags;
use rsub_core::text::{IgnoredArticles, content_type, normalize, search_norm};
use rsub_core::{Kind, now_ms};
use sea_query::{Expr, ExprTrait, OnConflict, Order, Query};

use crate::identity::{Claimant, Holder, Resolver, prune_rating_keys};
use crate::tags::{FileKey, tags_by_key};
use crate::{Db, IdRow, Result, Tx};

/// A Subsonic music folder.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Library {
    pub id: i64,
    pub source_id: i64,
    pub remote_key: String,
    pub name: String,
    pub generation: i64,
    pub last_full_sync_at: Option<i64>,
    /// The backend's change marker as of the last completed sync.
    pub change_marker: Option<String>,
    /// The newest backend change stamp seen, where an incremental sync starts.
    pub changes_cursor: Option<i64>,
}

/// Options for one sync pass.
#[derive(Debug, Clone, Copy)]
pub struct SyncCtx<'a> {
    /// The configured name of the library's source, part of `rk:` keys.
    pub source: &'a str,
    pub library: &'a Library,
    pub generation: i64,
    pub articles: &'a IgnoredArticles,
    /// Rewrite rows even if the backend's `updated_at` is unchanged
    /// (e.g. after the ignored articles changed).
    pub force: bool,
    /// The pass lists only changed items, so an item missing from it may
    /// still exist. Resolving an id that hinges on that fails with
    /// [`StoreError::NeedsFullSync`](crate::StoreError::NeedsFullSync).
    pub incremental: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub artists: u64,
    pub albums: u64,
    pub tracks: u64,
}

#[derive(sqlx::FromRow)]
struct KeyedId {
    id: i64,
    k: String,
}

#[derive(sqlx::FromRow)]
struct NamedArtist {
    id: i64,
    public_id: String,
    name: String,
    remote_key: Option<String>,
    deleted_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct TagRow {
    id: i64,
    kind: String,
    name: String,
}

const LIBRARY_COLS: [&str; 8] = [
    "id",
    "source_id",
    "remote_key",
    "name",
    "generation",
    "last_full_sync_at",
    "change_marker",
    "changes_cursor",
];

pub(crate) fn tag_kind_str(k: TagKind) -> &'static str {
    match k {
        TagKind::Genre => "genre",
        TagKind::Mood => "mood",
        TagKind::Style => "style",
    }
}

fn role_str(r: CreditRole) -> &'static str {
    match r {
        CreditRole::Artist => "artist",
        CreditRole::AlbumArtist => "albumartist",
        CreditRole::Composer => "composer",
        CreditRole::Other => "other",
    }
}

fn opt_u32(v: Option<u32>) -> Expr {
    v.map(i64::from).into()
}

/// How a batch record maps onto rows, once resolved to a public id.
enum Plan {
    /// The id's row is live and unchanged: only stamp the generation.
    Touch(i64),
    /// The id has a row (possibly soft-deleted or re-keyed): rewrite it.
    Update(i64),
    /// No row has the id yet.
    Insert(String),
}

impl Db {
    /// Register a configured backend; returns its id.
    pub async fn ensure_source(&self, name: &str, kind: &str) -> Result<i64> {
        let q = Query::insert()
            .into_table("sources")
            .columns(["name", "kind", "created_at"])
            .values_panic([name.into(), kind.into(), now_ms().into()])
            .on_conflict(OnConflict::column("name").update_column("kind").to_owned())
            .returning_col("id")
            .to_owned();
        Ok(self.write_fetch_one::<IdRow>(&q).await?.id)
    }

    /// Record the backend's libraries: upsert the given ones and soft-delete the
    /// source's others together with their contents. Returns the live libraries.
    pub async fn sync_libraries(
        &self,
        source_id: i64,
        remote: &[RemoteLibrary],
    ) -> Result<Vec<Library>> {
        let mut tx = self.begin().await?;
        for lib in remote {
            let q = Query::insert()
                .into_table("libraries")
                .columns(["source_id", "remote_key", "name"])
                .values_panic([
                    source_id.into(),
                    lib.key.as_str().into(),
                    lib.name.as_str().into(),
                ])
                .on_conflict(
                    OnConflict::columns(["source_id", "remote_key"])
                        .update_column("name")
                        .value("deleted_at", Expr::val(None::<i64>))
                        .to_owned(),
                )
                .to_owned();
            tx.execute(&q).await?;
        }
        let gone: Vec<IdRow> = tx
            .fetch_all(
                &Query::select()
                    .column("id")
                    .from("libraries")
                    .and_where(Expr::col("source_id").eq(source_id))
                    .and_where(Expr::col("deleted_at").is_null())
                    .and_where(
                        Expr::col("remote_key").is_not_in(remote.iter().map(|l| l.key.as_str())),
                    )
                    .to_owned(),
            )
            .await?;
        let now = now_ms();
        for IdRow { id } in gone {
            tracing::info!(library = id, "library removed from backend");
            retire_library(&mut tx, id, now).await?;
        }
        tx.commit().await?;
        self.fetch_all(
            &Query::select()
                .columns(LIBRARY_COLS)
                .from("libraries")
                .and_where(Expr::col("source_id").eq(source_id))
                .and_where(Expr::col("deleted_at").is_null())
                .order_by("id", Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Soft-delete the libraries, and their contents, of every source not in
    /// `configured`: a backend taken out of the config (or renamed) leaves the
    /// catalog. Its rows come back if it is configured again. Returns how many
    /// libraries were retired.
    pub async fn retire_sources(&self, configured: &[i64]) -> Result<usize> {
        let mut tx = self.begin().await?;
        let gone: Vec<IdRow> = tx
            .fetch_all(
                &Query::select()
                    .column("id")
                    .from("libraries")
                    .and_where(Expr::col("source_id").is_not_in(configured.iter().copied()))
                    .and_where(Expr::col("deleted_at").is_null())
                    .to_owned(),
            )
            .await?;
        let now = now_ms();
        for IdRow { id } in &gone {
            retire_library(&mut tx, *id, now).await?;
        }
        tx.commit().await?;
        Ok(gone.len())
    }

    /// Live libraries across all sources (the Subsonic music folders).
    pub async fn libraries(&self) -> Result<Vec<Library>> {
        self.fetch_all(
            &Query::select()
                .columns(LIBRARY_COLS)
                .from("libraries")
                .and_where(Expr::col("deleted_at").is_null())
                .order_by("id", Order::Asc)
                .to_owned(),
        )
        .await
    }

    pub async fn upsert_artists(&self, cx: SyncCtx<'_>, batch: &[ArtistRecord]) -> Result<()> {
        let mut tx = self.begin().await?;
        let keys: Vec<&str> = batch.iter().map(|a| a.remote.key.as_str()).collect();
        let files = tags_by_key(&mut tx, cx.library.id, FileKey::Artist, &keys).await?;
        let claimants: Vec<Claimant> = batch
            .iter()
            .map(|a| {
                // The MBID the artist's albums' files pair with its name as
                // album artist, else Plex's. Plex may file an album under
                // another artist than its tags name, whose MBID isn't this
                // artist's.
                let name = normalize(&a.name);
                let tagged = consensus(
                    files_of(&files, &a.remote.key).filter_map(|t| album_artist_mbid(t, &name)),
                );
                let mbid = tagged.or_else(|| a.mbid.clone());
                claimant(
                    cx,
                    &a.remote,
                    mbid.as_deref().map(artist_key),
                    plex_key(a.remote.guid.as_deref()),
                    name_key(&a.name),
                    a.name.clone(),
                )
            })
            .collect();
        let updated: Vec<i64> = batch.iter().map(|a| a.updated_at).collect();
        let plans = plan(&mut tx, cx, Kind::Artist, &claimants, &updated).await?;
        let mut holders = KeyHolders::load(&mut tx, "artists", cx, &keys).await?;
        let mut touched = Vec::new();
        // Every row's tags are rewritten, changed or not: a backend may list
        // them apart from the item (Plex styles), so they don't move its
        // change stamp.
        let mut rows: Vec<(i64, &ArtistRecord)> = Vec::with_capacity(batch.len());
        for (a, plan) in batch.iter().zip(plans) {
            let unchanged = match plan {
                Plan::Touch(id) => Some(id),
                _ => None,
            };
            let values: Vec<(&'static str, Expr)> = vec![
                ("library_id", cx.library.id.into()),
                ("remote_key", a.remote.key.as_str().into()),
                ("guid", a.remote.guid.as_deref().into()),
                ("name", a.name.as_str().into()),
                (
                    "sort_key",
                    cx.articles.sort_key(&a.name, a.sort_name.as_deref()).into(),
                ),
                ("search_norm", search_norm([a.name.as_str()]).into()),
                ("mbid", a.mbid.as_deref().into()),
                ("summary", a.summary.as_deref().into()),
                ("thumb_ref", a.thumb.as_deref().into()),
                ("remote_updated_at", a.updated_at.into()),
                ("generation", cx.generation.into()),
                ("deleted_at", Expr::val(None::<i64>)),
            ];
            let written = holders
                .apply(&mut tx, plan, &a.remote.key, values, &mut touched)
                .await?;
            if let Some(id) = written.or(unchanged) {
                rows.push((id, a));
            }
        }
        touch(&mut tx, "artists", cx.generation, touched).await?;
        let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
        delete_links(&mut tx, "artist_tags", "artist_id", &ids).await?;
        let tags: Vec<(i64, &(TagKind, String))> = rows
            .iter()
            .flat_map(|(id, a)| a.tags.iter().map(move |t| (*id, t)))
            .collect();
        insert_tags(&mut tx, "artist_tags", "artist_id", &tags).await?;
        tx.commit().await
    }

    pub async fn upsert_albums(&self, cx: SyncCtx<'_>, batch: &[AlbumRecord]) -> Result<()> {
        let mut tx = self.begin().await?;
        let keys: Vec<&str> = batch.iter().map(|a| a.remote.key.as_str()).collect();
        let files = tags_by_key(&mut tx, cx.library.id, FileKey::Album, &keys).await?;
        let claimants: Vec<Claimant> = batch
            .iter()
            .map(|a| {
                let tags = || files_of(&files, &a.remote.key);
                // The release its tracks agree on. Plex's album MBID isn't
                // used: it may name the release group.
                let mbid = consensus(tags().filter_map(|t| t.release_mbid.clone()));
                let album_artist = consensus(tags().filter_map(|t| joined(&t.album_artists)))
                    .unwrap_or_else(|| a.display_artist.clone());
                let title = consensus(tags().filter_map(|t| t.album.clone()))
                    .unwrap_or_else(|| a.title.clone());
                let tiebreak = files
                    .get(&a.remote.key)
                    .and_then(|f| f.first())
                    .map_or_else(
                        || format!("{}|{}", a.display_artist, a.title),
                        |f| f.0.clone(),
                    );
                claimant(
                    cx,
                    &a.remote,
                    mbid.as_deref().map(release_key),
                    plex_key(a.remote.guid.as_deref()),
                    album_tag_key(&album_artist, &title, a.year),
                    tiebreak,
                )
            })
            .collect();
        let updated: Vec<i64> = batch.iter().map(|a| a.updated_at).collect();
        let plans = plan(&mut tx, cx, Kind::Album, &claimants, &updated).await?;
        let mut holders = KeyHolders::load(&mut tx, "albums", cx, &keys).await?;
        let mbids = paired_mbids(files.values().flatten().map(|(_, t)| t));

        let mut touched = Vec::new();
        let mut changed: Vec<(i64, &AlbumRecord)> = Vec::new();
        for (a, plan) in batch.iter().zip(plans) {
            let values: Vec<(&'static str, Expr)> = vec![
                ("library_id", cx.library.id.into()),
                ("remote_key", a.remote.key.as_str().into()),
                ("guid", a.remote.guid.as_deref().into()),
                ("title", a.title.as_str().into()),
                (
                    "sort_key",
                    cx.articles
                        .sort_key(&a.title, a.sort_title.as_deref())
                        .into(),
                ),
                (
                    "search_norm",
                    search_norm([a.title.as_str(), a.display_artist.as_str()]).into(),
                ),
                ("display_artist", a.display_artist.as_str().into()),
                (
                    "artist_sort_key",
                    cx.articles.sort_key(&a.display_artist, None).into(),
                ),
                ("year", a.year.map(i64::from).into()),
                ("release_date", a.release_date.as_deref().into()),
                (
                    "orig_release_date",
                    a.original_release_date.as_deref().into(),
                ),
                ("label", a.label.as_deref().into()),
                (
                    "release_types",
                    serde_json::to_string(&a.release_types)
                        .unwrap_or_else(|_| "[]".into())
                        .into(),
                ),
                ("is_compilation", a.is_compilation.into()),
                ("mbid", a.mbid.as_deref().into()),
                ("thumb_ref", a.thumb.as_deref().into()),
                ("added_at", a.added_at.into()),
                ("remote_updated_at", a.updated_at.into()),
                ("generation", cx.generation.into()),
                ("deleted_at", Expr::val(None::<i64>)),
            ];
            if let Some(id) = holders
                .apply(&mut tx, plan, &a.remote.key, values, &mut touched)
                .await?
            {
                changed.push((id, a));
            }
        }
        touch(&mut tx, "albums", cx.generation, touched).await?;

        if !changed.is_empty() {
            let ids: Vec<i64> = changed.iter().map(|(id, _)| *id).collect();
            delete_links(&mut tx, "album_artists", "album_id", &ids).await?;
            delete_links(&mut tx, "album_tags", "album_id", &ids).await?;

            let credits: Vec<CreditRecord> = changed
                .iter()
                .map(|(_, a)| CreditRecord {
                    remote: a.artist.clone(),
                    name: a.display_artist.clone(),
                    role: CreditRole::AlbumArtist,
                })
                .collect();
            let artist_ids = resolve_artists(&mut tx, cx, &credits, &mbids).await?;
            let mut links = Query::insert()
                .into_table("album_artists")
                .columns(["album_id", "artist_id", "pos"])
                .to_owned();
            let mut any = false;
            for ((album_id, _), artist_id) in changed.iter().zip(&artist_ids) {
                if let Some(artist_id) = artist_id {
                    links.values_panic([(*album_id).into(), (*artist_id).into(), 0i64.into()]);
                    any = true;
                }
            }
            if any {
                tx.execute(&links).await?;
            }

            let tags: Vec<(i64, &(TagKind, String))> = changed
                .iter()
                .flat_map(|(id, a)| a.tags.iter().map(move |t| (*id, t)))
                .collect();
            insert_tags(&mut tx, "album_tags", "album_id", &tags).await?;
        }
        tx.commit().await
    }

    /// Returns the number of tracks written (inserted or updated).
    pub async fn upsert_tracks(&self, cx: SyncCtx<'_>, batch: &[TrackRecord]) -> Result<usize> {
        let mut tx = self.begin().await?;

        // Parent albums (their titles are part of the track search text).
        #[derive(sqlx::FromRow)]
        struct ParentAlbum {
            id: i64,
            k: String,
            title: String,
            display_artist: String,
        }
        let albums: HashMap<String, ParentAlbum> = tx
            .fetch_all::<ParentAlbum>(
                &Query::select()
                    .columns(["id", "title", "display_artist"])
                    .expr_as(Expr::col("remote_key"), "k")
                    .from("albums")
                    .and_where(Expr::col("library_id").eq(cx.library.id))
                    .and_where(
                        Expr::col("remote_key").is_in(batch.iter().map(|t| t.album.key.as_str())),
                    )
                    .to_owned(),
            )
            .await?
            .into_iter()
            .map(|r| (r.k.clone(), r))
            .collect();

        let batch: Vec<&TrackRecord> = batch
            .iter()
            .filter(|t| {
                let ok = albums.contains_key(&t.album.key);
                if !ok {
                    tracing::warn!(track = %t.remote.key, album = %t.album.key, "skipping track with unknown album");
                }
                ok
            })
            .collect();
        let keys: Vec<&str> = batch.iter().map(|t| t.remote.key.as_str()).collect();
        let files = tags_by_key(&mut tx, cx.library.id, FileKey::Track, &keys).await?;
        let claimants: Vec<Claimant> = batch
            .iter()
            .map(|t| {
                let album = &albums[&t.album.key];
                let tags = played_tags(&files, t);
                let tag = |f: fn(&FileTags) -> Option<String>| tags.and_then(f);
                let key = track_tag_key(
                    &tag(|t| joined(&t.album_artists))
                        .unwrap_or_else(|| album.display_artist.clone()),
                    &tag(|t| t.album.clone()).unwrap_or_else(|| album.title.clone()),
                    tags.and_then(|x| x.disc_no).or(t.disc_no),
                    tags.and_then(|x| x.track_no).or(t.track_no),
                    &tag(|t| t.title.clone()).unwrap_or_else(|| t.title.clone()),
                );
                claimant(
                    cx,
                    &t.remote,
                    tag(|t| t.release_track_mbid.clone())
                        .as_deref()
                        .map(release_track_key),
                    // Plex track guids name the song, which a single and its
                    // album share, not the track on one release.
                    None,
                    key,
                    t.remote_path.clone().unwrap_or_else(|| t.title.clone()),
                )
            })
            .collect();
        let updated: Vec<i64> = batch.iter().map(|t| t.updated_at).collect();
        let plans = plan(&mut tx, cx, Kind::Track, &claimants, &updated).await?;
        let mut holders = KeyHolders::load(&mut tx, "tracks", cx, &keys).await?;
        let mbids = paired_mbids(files.values().flatten().map(|(_, t)| t));

        let mut touched = Vec::new();
        let mut changed: Vec<(i64, &TrackRecord)> = Vec::new();
        for (t, plan) in batch.iter().copied().zip(plans) {
            let suffix = track_suffix(t);
            let album = &albums[&t.album.key];
            let values: Vec<(&'static str, Expr)> = vec![
                ("library_id", cx.library.id.into()),
                ("album_id", album.id.into()),
                ("remote_key", t.remote.key.as_str().into()),
                ("guid", t.remote.guid.as_deref().into()),
                ("part_key", t.part_key.as_str().into()),
                ("remote_path", t.remote_path.as_deref().into()),
                ("title", t.title.as_str().into()),
                (
                    "sort_key",
                    cx.articles
                        .sort_key(&t.title, t.sort_title.as_deref())
                        .into(),
                ),
                (
                    "search_norm",
                    search_norm([
                        t.title.as_str(),
                        t.display_artist.as_str(),
                        album.title.as_str(),
                    ])
                    .into(),
                ),
                ("display_artist", t.display_artist.as_str().into()),
                ("track_no", opt_u32(t.track_no)),
                ("disc_no", opt_u32(t.disc_no)),
                ("year", t.year.map(i64::from).into()),
                (
                    "duration_ms",
                    i64::try_from(t.duration_ms).unwrap_or(i64::MAX).into(),
                ),
                ("bitrate", opt_u32(t.bitrate_kbps)),
                ("sample_rate", opt_u32(t.sample_rate)),
                ("bit_depth", opt_u32(t.bit_depth)),
                ("channels", opt_u32(t.channels)),
                ("codec", t.codec.as_deref().into()),
                ("content_type", suffix.as_deref().map(content_type).into()),
                ("suffix", suffix.into()),
                ("size", t.size.and_then(|s| i64::try_from(s).ok()).into()),
                ("mbid", t.mbid.as_deref().into()),
                ("popularity", opt_u32(t.popularity)),
                ("added_at", t.added_at.into()),
                ("remote_updated_at", t.updated_at.into()),
                ("enriched_at", Expr::val(None::<i64>)),
                ("generation", cx.generation.into()),
                ("deleted_at", Expr::val(None::<i64>)),
            ];
            if let Some(id) = holders
                .apply(&mut tx, plan, &t.remote.key, values, &mut touched)
                .await?
            {
                changed.push((id, t));
            }
        }
        touch(&mut tx, "tracks", cx.generation, touched).await?;

        if !changed.is_empty() {
            let ids: Vec<i64> = changed.iter().map(|(id, _)| *id).collect();
            delete_links(&mut tx, "track_credits", "track_id", &ids).await?;

            let mut owners = Vec::new();
            let mut credits = Vec::new();
            for (id, t) in &changed {
                let tagged = played_tags(&files, t).map(|f| tag_credits(t, f));
                let track_credits = match tagged {
                    Some(artists) if !artists.is_empty() => artists
                        .into_iter()
                        .chain(
                            t.credits
                                .iter()
                                .filter(|c| c.role != CreditRole::Artist)
                                .cloned(),
                        )
                        .collect(),
                    _ => t.credits.clone(),
                };
                if track_credits.is_empty() {
                    owners.push((*id, 0));
                    credits.push(CreditRecord {
                        remote: None,
                        name: t.display_artist.clone(),
                        role: CreditRole::Artist,
                    });
                }
                let mut pos: HashMap<CreditRole, i64> = HashMap::new();
                for c in track_credits {
                    let p = pos.entry(c.role).or_default();
                    owners.push((*id, *p));
                    *p += 1;
                    credits.push(c);
                }
            }
            let artist_ids = resolve_artists(&mut tx, cx, &credits, &mbids).await?;
            let mut q = Query::insert()
                .into_table("track_credits")
                .columns(["track_id", "artist_id", "role", "pos"])
                .to_owned();
            let mut seen = HashSet::new();
            for (((track_id, pos), c), artist_id) in owners.iter().zip(&credits).zip(artist_ids) {
                let Some(artist_id) = artist_id else { continue };
                if seen.insert((*track_id, role_str(c.role), *pos)) {
                    q.values_panic([
                        (*track_id).into(),
                        artist_id.into(),
                        role_str(c.role).into(),
                        (*pos).into(),
                    ]);
                }
            }
            if !seen.is_empty() {
                tx.execute(&q).await?;
            }
        }
        let n = changed.len();
        tx.commit().await?;
        Ok(n)
    }

    /// Soft-delete everything in the library not seen in this pass, refresh
    /// aggregates, and record the new generation and the backend's `marker`.
    pub async fn finish_full_sync(
        &self,
        cx: SyncCtx<'_>,
        marker: Option<&str>,
    ) -> Result<SweepStats> {
        let lib = cx.library.id;
        let generation = cx.generation;
        let now = now_ms();
        let mut tx = self.begin().await?;
        let mut stats = SweepStats::default();
        for (table, slot) in [
            ("tracks", &mut stats.tracks),
            ("albums", &mut stats.albums),
            ("artists", &mut stats.artists),
        ] {
            let mut q = Query::update()
                .table(table)
                .value("deleted_at", now)
                .and_where(Expr::col("library_id").eq(lib))
                .and_where(Expr::col("generation").lt(generation))
                .and_where(Expr::col("deleted_at").is_null())
                .to_owned();
            if table == "artists" {
                q.and_where(Expr::col("remote_key").is_not_null());
            }
            *slot = tx.execute(&q).await?;
        }
        stats.artists += refresh_aggregates(&mut tx, lib, now).await?;
        let pruned = prune_rating_keys(&mut tx).await?;
        if pruned > 0 {
            tracing::debug!(pruned, "pruned stale ratingKeys from the identity ledger");
        }
        record_pass(&mut tx, cx, marker, now, true).await?;
        tx.commit().await?;
        Ok(stats)
    }

    /// Finish an incremental pass: nothing is swept, as it can't tell what
    /// the backend deleted, but credits and aggregates are refreshed.
    pub async fn finish_incremental_sync(
        &self,
        cx: SyncCtx<'_>,
        marker: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let mut tx = self.begin().await?;
        refresh_aggregates(&mut tx, cx.library.id, now).await?;
        record_pass(&mut tx, cx, marker, now, false).await?;
        tx.commit().await
    }

    /// Record `generation` as the library's last, after a failed incremental
    /// pass that stamped rows with it, so the next pass uses a newer one.
    pub async fn set_library_generation(&self, library_id: i64, generation: i64) -> Result<()> {
        self.execute(
            &Query::update()
                .table("libraries")
                .value("generation", generation)
                .and_where(Expr::col("id").eq(library_id))
                .to_owned(),
        )
        .await?;
        Ok(())
    }

    /// Remote keys of live tracks that still need the enrichment pass.
    pub async fn tracks_to_enrich(&self, library_id: i64, limit: u64) -> Result<Vec<String>> {
        let rows: Vec<KeyedId> = self
            .fetch_all(
                &Query::select()
                    .column("id")
                    .expr_as(Expr::col("remote_key"), "k")
                    .from("tracks")
                    .and_where(Expr::col("library_id").eq(library_id))
                    .and_where(Expr::col("enriched_at").is_null())
                    .and_where(Expr::col("deleted_at").is_null())
                    .order_by("id", Order::Asc)
                    .limit(limit)
                    .to_owned(),
            )
            .await?;
        Ok(rows.into_iter().map(|r| r.k).collect())
    }

    /// Apply enrichment results. Every key in `requested` is marked enriched, so
    /// items the backend could not describe are not retried forever.
    pub async fn apply_enrichment(
        &self,
        library_id: i64,
        requested: &[String],
        results: &[TrackEnrichment],
    ) -> Result<()> {
        let now = now_ms();
        let mut tx = self.begin().await?;
        tx.execute(
            &Query::update()
                .table("tracks")
                .value("enriched_at", now)
                .and_where(Expr::col("library_id").eq(library_id))
                .and_where(Expr::col("remote_key").is_in(requested.iter().map(String::as_str)))
                .to_owned(),
        )
        .await?;
        let ids: HashMap<String, i64> = tx
            .fetch_all::<KeyedId>(
                &Query::select()
                    .column("id")
                    .expr_as(Expr::col("remote_key"), "k")
                    .from("tracks")
                    .and_where(Expr::col("library_id").eq(library_id))
                    .and_where(
                        Expr::col("remote_key").is_in(results.iter().map(|e| e.key.as_str())),
                    )
                    .to_owned(),
            )
            .await?
            .into_iter()
            .map(|r| (r.k, r.id))
            .collect();
        let mut tags = Vec::new();
        for e in results {
            let Some(&id) = ids.get(&e.key) else { continue };
            let rg = &e.replay_gain;
            let mut q = Query::update()
                .table("tracks")
                .value("rg_track_gain", rg.track_gain)
                .value("rg_track_peak", rg.track_peak)
                .value("rg_album_gain", rg.album_gain)
                .value("rg_album_peak", rg.album_peak)
                .value("has_lyrics", e.has_lyrics)
                .value("bpm", opt_u32(e.bpm))
                .value("comment", e.comment.as_deref())
                .and_where(Expr::col("id").eq(id))
                .to_owned();
            // Only overwrite list-view values when the detail view has them.
            if let Some(v) = e.bit_depth {
                q.value("bit_depth", i64::from(v));
            }
            if let Some(v) = e.sample_rate {
                q.value("sample_rate", i64::from(v));
            }
            tx.execute(&q).await?;
            tags.extend(e.tags.iter().map(|t| (id, t)));
        }
        let with_tags: Vec<i64> = ids.values().copied().collect();
        delete_links(&mut tx, "track_tags", "track_id", &with_tags).await?;
        insert_tags(&mut tx, "track_tags", "track_id", &tags).await?;
        tx.commit().await
    }
}

fn track_suffix(t: &TrackRecord) -> Option<String> {
    t.remote_path
        .as_deref()
        .and_then(|p| p.rsplit(['/', '\\']).next())
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 5)
        .or_else(|| t.container.as_deref().map(str::to_ascii_lowercase))
}

/// An item's identity keys, strongest first.
fn claimant(
    cx: SyncCtx<'_>,
    remote: &RemoteRef,
    mbid: Option<String>,
    plex: Option<String>,
    tag: String,
    tiebreak: String,
) -> Claimant {
    let keys = mbid
        .into_iter()
        .chain(plex)
        .chain([rating_key(cx.source, &remote.key), tag])
        .collect();
    Claimant {
        remote_key: Some(remote.key.clone()),
        keys,
        tiebreak,
    }
}

/// The cached tags of the file Plex plays for `t`, else of its first file.
fn played_tags<'a>(
    files: &'a HashMap<String, Vec<(String, FileTags)>>,
    t: &TrackRecord,
) -> Option<&'a FileTags> {
    let f = files.get(&t.remote.key)?;
    f.iter()
        .find(|(path, _)| Some(path) == t.remote_path.as_ref())
        .or(f.first())
        .map(|(_, tags)| tags)
}

/// A track's artist credits from its file's `ARTISTS` tag: one per name,
/// where Plex has only a display string for them all ("A x B feat. C"). A
/// name Plex credits by key keeps the key.
fn tag_credits(t: &TrackRecord, tags: &FileTags) -> Vec<CreditRecord> {
    let mut seen = HashSet::new();
    tags.artists
        .iter()
        .filter(|n| !n.trim().is_empty() && seen.insert(normalize(n)))
        .map(|name| CreditRecord {
            remote: t
                .credits
                .iter()
                .find(|c| c.remote.is_some() && normalize(&c.name) == normalize(name))
                .and_then(|c| c.remote.clone()),
            name: name.clone(),
            role: CreditRole::Artist,
        })
        .collect()
}

/// The cached tags of an item's files.
fn files_of<'a>(
    files: &'a HashMap<String, Vec<(String, FileTags)>>,
    key: &str,
) -> impl Iterator<Item = &'a FileTags> {
    files.get(key).into_iter().flatten().map(|(_, t)| t)
}

/// A multi-value tag as one value.
fn joined(v: &[String]) -> Option<String> {
    (!v.is_empty()).then(|| v.join("; "))
}

/// The value more than half of `vals` agree on.
fn consensus(vals: impl IntoIterator<Item = String>) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut n = 0;
    for v in vals {
        n += 1;
        *counts.entry(v).or_default() += 1;
    }
    counts.into_iter().find(|(_, c)| c * 2 > n).map(|(v, _)| v)
}

/// The album-artist MBID a file pairs with the normalised `name`, when its
/// album-artist names and MBIDs pair up (same count, by position).
fn album_artist_mbid(t: &FileTags, name: &str) -> Option<String> {
    if t.album_artists.len() != t.album_artist_mbids.len() {
        return None;
    }
    t.album_artists
        .iter()
        .zip(&t.album_artist_mbids)
        .find(|(n, _)| normalize(n) == name)
        .map(|(_, id)| id.clone())
}

/// Artist MBIDs by normalised name, from files whose artist names and MBIDs
/// pair up (same count, by position). Names paired with different MBIDs are
/// left out.
fn paired_mbids<'a>(files: impl Iterator<Item = &'a FileTags>) -> HashMap<String, String> {
    let mut out: HashMap<String, Option<String>> = HashMap::new();
    for t in files {
        for (names, ids) in [
            (&t.artists, &t.artist_mbids),
            (&t.album_artists, &t.album_artist_mbids),
        ] {
            if names.len() != ids.len() {
                continue;
            }
            for (name, id) in names.iter().zip(ids) {
                out.entry(normalize(name))
                    .and_modify(|v| {
                        if v.as_ref() != Some(id) {
                            *v = None;
                        }
                    })
                    .or_insert_with(|| Some(id.clone()));
            }
        }
    }
    out.into_iter().filter_map(|(k, v)| Some((k, v?))).collect()
}

/// Resolve a batch to public ids through the identity ledger and plan the
/// writes. `updated` holds each item's backend `updated_at`.
async fn plan(
    tx: &mut Tx,
    cx: SyncCtx<'_>,
    kind: Kind,
    claimants: &[Claimant],
    updated: &[i64],
) -> Result<Vec<Plan>> {
    let mut r = Resolver::load(tx, cx, kind, claimants).await?;
    let mut plans: Vec<Option<Plan>> = claimants.iter().map(|_| None).collect();
    for i in r.order(claimants) {
        let c = &claimants[i];
        let (public_id, holder) = r.resolve(tx, c).await?;
        plans[i] = Some(match holder {
            Some(h)
                if !cx.force
                    && h.deleted_at.is_none()
                    && h.library_id == cx.library.id
                    && h.remote_key == c.remote_key
                    && h.remote_updated_at == updated[i] =>
            {
                Plan::Touch(h.id)
            }
            Some(h) => Plan::Update(h.id),
            None => Plan::Insert(public_id),
        });
    }
    r.finish(tx).await?;
    Ok(plans.into_iter().flatten().collect())
}

/// Rows of the library by backend key, to keep `(library_id, remote_key)`
/// unique when an item's id resolves to another row than the one holding its
/// key: that row's item has gone, so it is soft-deleted and its key freed.
struct KeyHolders {
    table: &'static str,
    by_key: HashMap<String, i64>,
}

impl KeyHolders {
    async fn load(
        tx: &mut Tx,
        table: &'static str,
        cx: SyncCtx<'_>,
        keys: &[&str],
    ) -> Result<KeyHolders> {
        let rows: Vec<KeyedId> = tx
            .fetch_all(
                &Query::select()
                    .column("id")
                    .expr_as(Expr::col("remote_key"), "k")
                    .from(table)
                    .and_where(Expr::col("library_id").eq(cx.library.id))
                    .and_where(Expr::col("remote_key").is_in(keys.iter().copied()))
                    .to_owned(),
            )
            .await?;
        Ok(KeyHolders {
            table,
            by_key: rows.into_iter().map(|r| (r.k, r.id)).collect(),
        })
    }

    /// Write one planned row with backend key `key`. Returns the row id
    /// unless it was only touched.
    async fn apply(
        &mut self,
        tx: &mut Tx,
        plan: Plan,
        key: &str,
        mut values: Vec<(&'static str, Expr)>,
        touched: &mut Vec<i64>,
    ) -> Result<Option<i64>> {
        let id = match plan {
            Plan::Touch(id) => {
                touched.push(id);
                return Ok(None);
            }
            Plan::Update(id) => {
                self.free(tx, key, Some(id)).await?;
                update_row(tx, self.table, id, values).await?;
                id
            }
            Plan::Insert(public_id) => {
                self.free(tx, key, None).await?;
                values.push(("public_id", public_id.into()));
                insert_row(tx, self.table, values).await?
            }
        };
        self.by_key.retain(|_, v| *v != id);
        self.by_key.insert(key.to_owned(), id);
        Ok(Some(id))
    }

    /// Free `key` for row `target` (`None`: a new row).
    async fn free(&mut self, tx: &mut Tx, key: &str, target: Option<i64>) -> Result<()> {
        let Some(&other) = self.by_key.get(key) else {
            return Ok(());
        };
        if Some(other) == target {
            return Ok(());
        }
        tracing::info!(
            table = self.table,
            key,
            row = other,
            "item resolved to another id; retiring its old row"
        );
        tx.execute(
            &Query::update()
                .table(self.table)
                .value("remote_key", format!("~{other}"))
                .value("deleted_at", now_ms())
                .and_where(Expr::col("id").eq(other))
                .to_owned(),
        )
        .await?;
        self.by_key.remove(key);
        Ok(())
    }
}

async fn insert_row(
    tx: &mut Tx,
    table: &'static str,
    values: Vec<(&'static str, Expr)>,
) -> Result<i64> {
    let (cols, vals): (Vec<_>, Vec<_>) = values.into_iter().unzip();
    let q = Query::insert()
        .into_table(table)
        .columns(cols)
        .values_panic(vals)
        .returning_col("id")
        .to_owned();
    Ok(tx.fetch_one::<IdRow>(&q).await?.id)
}

async fn update_row(
    tx: &mut Tx,
    table: &'static str,
    id: i64,
    values: Vec<(&'static str, Expr)>,
) -> Result<()> {
    tx.execute(
        &Query::update()
            .table(table)
            .values(values)
            .and_where(Expr::col("id").eq(id))
            .to_owned(),
    )
    .await?;
    Ok(())
}

/// Sweep virtual artists nothing live credits any more, then recount the
/// library's albums' songs and duration and its artists' albums. Returns the
/// virtual artists swept.
/// Soft-delete a library and its contents.
async fn retire_library(tx: &mut Tx, id: i64, now: i64) -> Result<()> {
    for table in ["libraries", "artists", "albums", "tracks"] {
        let col = if table == "libraries" {
            "id"
        } else {
            "library_id"
        };
        let mut q = Query::update()
            .table(table)
            .value("deleted_at", now)
            .and_where(Expr::col(col).eq(id))
            .and_where(Expr::col("deleted_at").is_null())
            .to_owned();
        if table == "libraries" {
            // Its contents left the catalog: `getIndexes` must say so. If it
            // comes back, a full sync is due at once to revive them.
            q.value("last_sync_at", now)
                .value("last_full_sync_at", None::<i64>)
                .value("change_marker", None::<String>)
                .value("changes_cursor", None::<i64>);
        }
        tx.execute(&q).await?;
    }
    Ok(())
}

async fn refresh_aggregates(tx: &mut Tx, lib: i64, now: i64) -> Result<u64> {
    // Virtual artists live as long as something live credits them.
    let swept = tx
        .execute_portable(format!(
            "UPDATE artists SET deleted_at = {now}              WHERE library_id = {lib} AND remote_key IS NULL AND deleted_at IS NULL              AND NOT EXISTS (SELECT 1 FROM track_credits c JOIN tracks t ON t.id = c.track_id                              WHERE c.artist_id = artists.id AND t.deleted_at IS NULL)              AND NOT EXISTS (SELECT 1 FROM album_artists aa JOIN albums a ON a.id = aa.album_id                              WHERE aa.artist_id = artists.id AND a.deleted_at IS NULL)"
        ))
        .await?;
    tx.execute_portable(format!(
        "UPDATE albums SET            song_count = (SELECT COUNT(*) FROM tracks t                          WHERE t.album_id = albums.id AND t.deleted_at IS NULL),            duration_ms = (SELECT COALESCE(SUM(t.duration_ms), 0) FROM tracks t                           WHERE t.album_id = albums.id AND t.deleted_at IS NULL)          WHERE library_id = {lib}"
    ))
    .await?;
    tx.execute_portable(format!(
        "UPDATE artists SET album_count = (SELECT COUNT(*) FROM album_artists aa            JOIN albums a ON a.id = aa.album_id            WHERE aa.artist_id = artists.id AND a.deleted_at IS NULL)          WHERE library_id = {lib}"
    ))
    .await?;
    Ok(swept)
}

/// Record a completed pass at `now`: its generation, the backend's change
/// marker, and as the next incremental start the newest change stamp of any
/// row (the backend's clock, not ours). A `full` pass also sets
/// `last_full_sync_at`; every pass sets `last_sync_at`.
async fn record_pass(
    tx: &mut Tx,
    cx: SyncCtx<'_>,
    marker: Option<&str>,
    now: i64,
    full: bool,
) -> Result<()> {
    let lib = cx.library.id;
    let mut newest = None;
    for table in ["tracks", "albums", "artists"] {
        let mut q = Query::select()
            .expr_as(Expr::col("remote_updated_at").max(), "m")
            .from(table)
            .and_where(Expr::col("library_id").eq(lib))
            .to_owned();
        if table == "artists" {
            q.and_where(Expr::col("remote_key").is_not_null());
        }
        let row: CursorRow = tx.fetch_one(&q).await?;
        newest = Ord::max(newest, row.m);
    }
    let mut q = Query::update()
        .table("libraries")
        .value("generation", cx.generation)
        .value("change_marker", marker)
        .value("changes_cursor", newest)
        .value("last_sync_at", now)
        .and_where(Expr::col("id").eq(lib))
        .to_owned();
    if full {
        q.value("last_full_sync_at", now);
    }
    tx.execute(&q).await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct CursorRow {
    m: Option<i64>,
}

async fn touch(tx: &mut Tx, table: &'static str, generation: i64, ids: Vec<i64>) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    tx.execute(
        &Query::update()
            .table(table)
            .value("generation", generation)
            .and_where(Expr::col("id").is_in(ids))
            .to_owned(),
    )
    .await?;
    Ok(())
}

async fn delete_links(
    tx: &mut Tx,
    table: &'static str,
    col: &'static str,
    ids: &[i64],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    tx.execute(
        &Query::delete()
            .from_table(table)
            .and_where(Expr::col(col).is_in(ids.iter().copied()))
            .to_owned(),
    )
    .await?;
    Ok(())
}

/// Resolve artist credits to local ids: by remote key, else by name (a live
/// real artist first, then a virtual one, revived as needed), else through the
/// identity ledger, creating a virtual artist when nothing of this library
/// matches. `mbids` holds artist MBIDs by normalised name, from file tags.
async fn resolve_artists(
    tx: &mut Tx,
    cx: SyncCtx<'_>,
    credits: &[CreditRecord],
    mbids: &HashMap<String, String>,
) -> Result<Vec<Option<i64>>> {
    let lib = cx.library.id;
    let by_key: HashMap<String, i64> = tx
        .fetch_all::<KeyedId>(
            &Query::select()
                .column("id")
                .expr_as(Expr::col("remote_key"), "k")
                .from("artists")
                .and_where(Expr::col("library_id").eq(lib))
                .and_where(
                    Expr::col("remote_key").is_in(
                        credits
                            .iter()
                            .filter_map(|c| c.remote.as_ref().map(|r| r.key.as_str())),
                    ),
                )
                .to_owned(),
        )
        .await?
        .into_iter()
        .map(|r| (r.k, r.id))
        .collect();

    let resolved_key =
        |c: &CreditRecord| c.remote.as_ref().and_then(|r| by_key.get(&r.key).copied());
    let mut names: Vec<&str> = credits
        .iter()
        .filter(|c| resolved_key(c).is_none() && !c.name.trim().is_empty())
        .map(|c| c.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut by_name: HashMap<String, i64> = HashMap::new();
    if !names.is_empty() {
        let rows: Vec<NamedArtist> = tx
            .fetch_all(
                &Query::select()
                    .columns(["id", "public_id", "name", "remote_key", "deleted_at"])
                    .from("artists")
                    .and_where(Expr::col("library_id").eq(lib))
                    .and_where(Expr::col("name").is_in(names.iter().copied()))
                    .and_where(
                        Expr::col("deleted_at")
                            .is_null()
                            .or(Expr::col("remote_key").is_null()),
                    )
                    .order_by("id", Order::Asc)
                    .to_owned(),
            )
            .await?;
        let mut virtual_deleted = Vec::new();
        // Virtual artists found by name, whose keys are recorded below.
        let mut own: HashMap<String, String> = HashMap::new();
        // Real live artists win over virtual ones.
        for r in rows.iter().filter(|r| r.remote_key.is_some()) {
            by_name.entry(r.name.clone()).or_insert(r.id);
        }
        for r in rows.iter().filter(|r| r.remote_key.is_none()) {
            if !by_name.contains_key(&r.name) {
                by_name.insert(r.name.clone(), r.id);
                own.insert(r.name.clone(), r.public_id.clone());
                if r.deleted_at.is_some() {
                    virtual_deleted.push(r.id);
                }
            }
        }
        if !virtual_deleted.is_empty() {
            tx.execute(
                &Query::update()
                    .table("artists")
                    .value("deleted_at", Expr::val(None::<i64>))
                    .value("generation", cx.generation)
                    .and_where(Expr::col("id").is_in(virtual_deleted))
                    .to_owned(),
            )
            .await?;
        }

        let claimants: Vec<Claimant> = names
            .iter()
            .map(|name| Claimant {
                remote_key: None,
                keys: mbids
                    .get(&normalize(name))
                    .map(|m| artist_key(m))
                    .into_iter()
                    .chain([name_key(name)])
                    .collect(),
                tiebreak: (*name).to_owned(),
            })
            .collect();
        let mut r = Resolver::load(tx, cx, Kind::Artist, &claimants).await?;
        for (name, c) in names.iter().zip(&claimants) {
            if by_name.contains_key(*name) {
                if let Some(p) = own.get(*name) {
                    r.record(tx, c, p).await?;
                }
                continue;
            }
            let (public_id, holder) = r.resolve_link(tx, c).await?;
            let virtual_values = || -> Vec<(&'static str, Expr)> {
                vec![
                    ("name", (*name).into()),
                    ("sort_key", cx.articles.sort_key(name, None).into()),
                    ("search_norm", search_norm([*name]).into()),
                    ("generation", cx.generation.into()),
                ]
            };
            let id = match holder {
                // A live artist of this library under another spelling: a real
                // one keeps its own keys.
                Some(h) if h.deleted_at.is_none() => {
                    if h.remote_key.is_none() {
                        r.record(tx, c, &public_id).await?;
                    }
                    h.id
                }
                // A gone artist of this library returns as a virtual one.
                Some(h) => {
                    let mut values = virtual_values();
                    values.extend([
                        ("remote_key", Expr::val(None::<String>)),
                        ("guid", Expr::val(None::<String>)),
                        ("deleted_at", Expr::val(None::<i64>)),
                    ]);
                    update_row(tx, "artists", h.id, values).await?;
                    r.record(tx, c, &public_id).await?;
                    r.hold(virtual_holder(h.id, &public_id, cx));
                    h.id
                }
                None => {
                    let mut values = virtual_values();
                    values.extend([
                        ("library_id", lib.into()),
                        ("public_id", public_id.as_str().into()),
                    ]);
                    let id = insert_row(tx, "artists", values).await?;
                    r.record(tx, c, &public_id).await?;
                    // Another spelling in this batch may reach the same id.
                    r.hold(virtual_holder(id, &public_id, cx));
                    id
                }
            };
            by_name.insert((*name).to_owned(), id);
        }
        r.finish(tx).await?;
    }

    Ok(credits
        .iter()
        .map(|c| resolved_key(c).or_else(|| by_name.get(&c.name).copied()))
        .collect())
}

/// The row of a virtual artist just written in this pass.
fn virtual_holder(id: i64, public_id: &str, cx: SyncCtx<'_>) -> Holder {
    Holder {
        id,
        public_id: public_id.to_owned(),
        library_id: cx.library.id,
        remote_key: None,
        remote_updated_at: 0,
        generation: cx.generation,
        deleted_at: None,
    }
}

/// Insert `(owner id, tag)` links, creating tags as needed.
async fn insert_tags(
    tx: &mut Tx,
    table: &'static str,
    owner_col: &'static str,
    tags: &[(i64, &(TagKind, String))],
) -> Result<()> {
    if tags.is_empty() {
        return Ok(());
    }
    let select = Query::select()
        .columns(["id", "kind", "name"])
        .from("tags")
        .and_where(Expr::col("name").is_in(tags.iter().map(|(_, (_, n))| n.as_str())))
        .to_owned();
    let mut known: HashMap<(String, String), i64> = tx
        .fetch_all::<TagRow>(&select)
        .await?
        .into_iter()
        .map(|t| ((t.kind, t.name), t.id))
        .collect();
    let mut missing = Query::insert()
        .into_table("tags")
        .columns(["kind", "name"])
        .on_conflict(
            OnConflict::columns(["kind", "name"])
                .do_nothing()
                .to_owned(),
        )
        .to_owned();
    let mut seen = HashSet::new();
    for (_, (kind, name)) in tags {
        let k = (tag_kind_str(*kind).to_owned(), name.clone());
        if !known.contains_key(&k) && seen.insert(k) {
            missing.values_panic([tag_kind_str(*kind).into(), name.as_str().into()]);
        }
    }
    if !seen.is_empty() {
        tx.execute(&missing).await?;
        known = tx
            .fetch_all::<TagRow>(&select)
            .await?
            .into_iter()
            .map(|t| ((t.kind, t.name), t.id))
            .collect();
    }
    let mut q = Query::insert()
        .into_table(table)
        .columns([owner_col, "tag_id"])
        .to_owned();
    let mut links = std::collections::HashSet::new();
    for (owner, (kind, name)) in tags {
        if let Some(&tag) = known.get(&(tag_kind_str(*kind).to_owned(), name.clone()))
            && links.insert((*owner, tag))
        {
            q.values_panic([(*owner).into(), tag.into()]);
        }
    }
    if !links.is_empty() {
        tx.execute(&q).await?;
    }
    Ok(())
}
