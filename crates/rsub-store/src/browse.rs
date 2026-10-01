//! Catalog reads for the API. Everything here filters out soft-deleted rows.

use std::collections::HashMap;

use rsub_core::text::normalize;
use sea_query::{Expr, ExprTrait, JoinType, LikeExpr, Order, Query, SelectStatement};

use crate::{Db, Result};

/// Most tracks read to rank one artist's songs by popularity, before copies of a
/// song are merged.
const POPULAR_CANDIDATES: u64 = 2_000;

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ArtistRow {
    pub id: i64,
    pub public_id: String,
    pub library_id: i64,
    /// `None` for virtual artists.
    pub remote_key: Option<String>,
    pub name: String,
    pub sort_key: String,
    pub mbid: Option<String>,
    pub summary: Option<String>,
    pub thumb_ref: Option<String>,
    pub album_count: i64,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct AlbumRow {
    pub id: i64,
    pub public_id: String,
    pub library_id: i64,
    pub remote_key: Option<String>,
    pub title: String,
    pub sort_key: String,
    pub display_artist: String,
    /// First album artist.
    pub artist_id: Option<i64>,
    pub artist_public_id: Option<String>,
    pub year: Option<i64>,
    pub release_date: Option<String>,
    pub orig_release_date: Option<String>,
    pub label: Option<String>,
    /// JSON array of strings.
    pub release_types: String,
    pub is_compilation: bool,
    pub mbid: Option<String>,
    pub thumb_ref: Option<String>,
    pub song_count: i64,
    pub duration_ms: i64,
    pub added_at: i64,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct TrackRow {
    pub id: i64,
    pub public_id: String,
    pub library_id: i64,
    pub album_id: i64,
    pub album_public_id: String,
    pub album_title: String,
    pub album_display_artist: String,
    pub album_thumb_ref: Option<String>,
    /// First track artist.
    pub artist_id: Option<i64>,
    pub artist_public_id: Option<String>,
    /// First album artist.
    pub album_artist_id: Option<i64>,
    pub album_artist_public_id: Option<String>,
    pub remote_key: String,
    pub part_key: String,
    pub remote_path: Option<String>,
    pub title: String,
    pub sort_key: String,
    pub display_artist: String,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub year: Option<i64>,
    pub duration_ms: i64,
    pub bitrate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    pub codec: Option<String>,
    pub suffix: Option<String>,
    pub content_type: Option<String>,
    pub size: Option<i64>,
    pub bpm: Option<i64>,
    pub comment: Option<String>,
    pub mbid: Option<String>,
    pub popularity: Option<i64>,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    pub has_lyrics: bool,
    pub added_at: i64,
}

/// An artist credit on a track or album.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CreditRow {
    pub owner_id: i64,
    pub artist_id: i64,
    pub artist_public_id: String,
    pub name: String,
    pub role: String,
    pub pos: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TagLink {
    pub owner_id: i64,
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct GenreRow {
    pub name: String,
    pub album_count: i64,
    pub song_count: i64,
}

/// Where an item's artwork lives in the backend.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ArtSource {
    pub library_id: i64,
    pub remote_key: String,
    pub thumb_ref: Option<String>,
}

/// `getAlbumList(2)` orderings served from the catalog. The per-user ones
/// (frequent, recent, starred, highest) are backend queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumOrder {
    Random,
    Newest,
    ByName,
    ByArtist,
    ByYear { from: i64, to: i64 },
    ByGenre(String),
}

/// Common list filters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Page {
    pub limit: u64,
    pub offset: u64,
    pub library: Option<i64>,
}

impl Page {
    /// The first `limit` rows, from every library.
    pub fn first(limit: u64) -> Self {
        Page {
            limit,
            offset: 0,
            library: None,
        }
    }

    /// This page within one library (every library with `None`).
    pub fn in_library(self, library: Option<i64>) -> Self {
        Page { library, ..self }
    }
}

pub(crate) fn artists_select() -> SelectStatement {
    Query::select()
        .columns([
            "id",
            "public_id",
            "library_id",
            "remote_key",
            "name",
            "sort_key",
            "mbid",
            "summary",
            "thumb_ref",
            "album_count",
        ])
        .from("artists")
        .and_where(Expr::col("deleted_at").is_null())
        .to_owned()
}

pub(crate) fn albums_select() -> SelectStatement {
    Query::select()
        .columns(
            [
                "id",
                "public_id",
                "library_id",
                "remote_key",
                "title",
                "sort_key",
                "display_artist",
                "year",
                "release_date",
                "orig_release_date",
                "label",
                "release_types",
                "is_compilation",
                "mbid",
                "thumb_ref",
                "song_count",
                "duration_ms",
                "added_at",
            ]
            .map(|c| ("a", c)),
        )
        .expr_as(
            Expr::cust(
                "(SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id = a.id \
                 ORDER BY aa.pos LIMIT 1)",
            ),
            "artist_id",
        )
        .expr_as(
            Expr::cust(
                "(SELECT ar.public_id FROM album_artists aa JOIN artists ar ON ar.id = aa.artist_id \
                 WHERE aa.album_id = a.id ORDER BY aa.pos LIMIT 1)",
            ),
            "artist_public_id",
        )
        .from_as("albums", "a")
        .and_where(Expr::col(("a", "deleted_at")).is_null())
        .to_owned()
}

pub(crate) fn tracks_select() -> SelectStatement {
    Query::select()
        .columns(
            [
                "id",
                "public_id",
                "library_id",
                "album_id",
                "remote_key",
                "part_key",
                "remote_path",
                "title",
                "sort_key",
                "display_artist",
                "track_no",
                "disc_no",
                "year",
                "duration_ms",
                "bitrate",
                "sample_rate",
                "bit_depth",
                "channels",
                "codec",
                "suffix",
                "content_type",
                "size",
                "bpm",
                "comment",
                "mbid",
                "popularity",
                "rg_track_gain",
                "rg_track_peak",
                "rg_album_gain",
                "rg_album_peak",
                "has_lyrics",
                "added_at",
            ]
            .map(|c| ("t", c)),
        )
        .expr_as(Expr::col(("a", "public_id")), "album_public_id")
        .expr_as(Expr::col(("a", "title")), "album_title")
        .expr_as(Expr::col(("a", "display_artist")), "album_display_artist")
        .expr_as(Expr::col(("a", "thumb_ref")), "album_thumb_ref")
        .expr_as(
            Expr::cust(
                "(SELECT c.artist_id FROM track_credits c WHERE c.track_id = t.id \
                 AND c.role = 'artist' ORDER BY c.pos LIMIT 1)",
            ),
            "artist_id",
        )
        .expr_as(
            Expr::cust(
                "(SELECT ar.public_id FROM track_credits c JOIN artists ar ON ar.id = c.artist_id \
                 WHERE c.track_id = t.id AND c.role = 'artist' ORDER BY c.pos LIMIT 1)",
            ),
            "artist_public_id",
        )
        .expr_as(
            Expr::cust(
                "(SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id = t.album_id \
                 ORDER BY aa.pos LIMIT 1)",
            ),
            "album_artist_id",
        )
        .expr_as(
            Expr::cust(
                "(SELECT ar.public_id FROM album_artists aa JOIN artists ar ON ar.id = aa.artist_id \
                 WHERE aa.album_id = t.album_id ORDER BY aa.pos LIMIT 1)",
            ),
            "album_artist_public_id",
        )
        .from_as("tracks", "t")
        .join_as(
            JoinType::InnerJoin,
            "albums",
            "a",
            Expr::col(("a", "id")).equals(("t", "album_id")),
        )
        .and_where(Expr::col(("t", "deleted_at")).is_null())
        .to_owned()
}

fn page(q: &mut SelectStatement, p: Page, library_col: (&'static str, &'static str)) {
    if let Some(lib) = p.library {
        q.and_where(Expr::col(library_col).eq(lib));
    }
    q.limit(p.limit).offset(p.offset);
}

/// `search_norm LIKE '%term%'` for every term.
fn match_terms(q: &mut SelectStatement, col: (&'static str, &'static str), terms: &[String]) {
    for t in terms {
        let escaped = t
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        q.and_where(Expr::col(col).like(LikeExpr::new(format!("%{escaped}%")).escape('\\')));
    }
}

/// Tracks whose own genre or whose album's genre is `genre`.
fn genre_filter(genre: &str) -> Expr {
    let tag = Query::select()
        .column("id")
        .from("tags")
        .and_where(Expr::col("kind").eq("genre"))
        .and_where(Expr::col("name").eq(genre))
        .to_owned();
    Expr::col(("t", "id"))
        .in_subquery(
            Query::select()
                .column("track_id")
                .from("track_tags")
                .and_where(Expr::col("tag_id").in_subquery(tag.clone()))
                .to_owned(),
        )
        .or(Expr::col(("t", "album_id")).in_subquery(
            Query::select()
                .column("album_id")
                .from("album_tags")
                .and_where(Expr::col("tag_id").in_subquery(tag))
                .to_owned(),
        ))
}

impl Db {
    pub async fn library(&self, id: i64) -> Result<Option<crate::Library>> {
        Ok(self.libraries().await?.into_iter().find(|l| l.id == id))
    }

    /// Artists with at least one album, ordered for the index.
    pub async fn index_artists(&self, library: Option<i64>) -> Result<Vec<ArtistRow>> {
        let mut q = artists_select()
            .and_where(Expr::col("album_count").gt(0))
            .order_by("sort_key", Order::Asc)
            .order_by("id", Order::Asc)
            .to_owned();
        if let Some(lib) = library {
            q.and_where(Expr::col("library_id").eq(lib));
        }
        self.fetch_all(&q).await
    }

    pub async fn artist(&self, id: i64) -> Result<Option<ArtistRow>> {
        self.fetch_optional(
            &artists_select()
                .and_where(Expr::col("id").eq(id))
                .to_owned(),
        )
        .await
    }

    pub async fn artists_by_ids(&self, ids: &[i64]) -> Result<Vec<ArtistRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all(
            &artists_select()
                .and_where(Expr::col("id").is_in(ids.iter().copied()))
                .to_owned(),
        )
        .await
    }

    /// Albums on which the artist is an album artist.
    pub async fn albums_by_artist(&self, artist_id: i64) -> Result<Vec<AlbumRow>> {
        self.fetch_all(
            &albums_select()
                .and_where(
                    Expr::col(("a", "id")).in_subquery(
                        Query::select()
                            .column("album_id")
                            .from("album_artists")
                            .and_where(Expr::col("artist_id").eq(artist_id))
                            .to_owned(),
                    ),
                )
                .order_by(("a", "year"), Order::Asc)
                .order_by(("a", "sort_key"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    pub async fn album(&self, id: i64) -> Result<Option<AlbumRow>> {
        self.fetch_optional(
            &albums_select()
                .and_where(Expr::col(("a", "id")).eq(id))
                .to_owned(),
        )
        .await
    }

    pub async fn album_list(&self, order: &AlbumOrder, p: Page) -> Result<Vec<AlbumRow>> {
        let mut q = albums_select();
        match order {
            AlbumOrder::Random => {
                q.order_by_expr(Expr::cust("RANDOM()"), Order::Asc);
            }
            AlbumOrder::Newest => {
                q.order_by(("a", "added_at"), Order::Desc);
            }
            AlbumOrder::ByName => {
                q.order_by(("a", "sort_key"), Order::Asc);
            }
            AlbumOrder::ByArtist => {
                q.order_by(("a", "artist_sort_key"), Order::Asc)
                    .order_by(("a", "year"), Order::Asc)
                    .order_by(("a", "sort_key"), Order::Asc);
            }
            AlbumOrder::ByYear { from, to } => {
                let (lo, hi, ord) = if from <= to {
                    (*from, *to, Order::Asc)
                } else {
                    (*to, *from, Order::Desc)
                };
                q.and_where(Expr::col(("a", "year")).between(lo, hi))
                    .order_by(("a", "year"), ord)
                    .order_by(("a", "sort_key"), Order::Asc);
            }
            AlbumOrder::ByGenre(genre) => {
                q.and_where(
                    Expr::col(("a", "id")).in_subquery(
                        Query::select()
                            .column("album_id")
                            .from("album_tags")
                            .and_where(
                                Expr::col("tag_id").in_subquery(
                                    Query::select()
                                        .column("id")
                                        .from("tags")
                                        .and_where(Expr::col("kind").eq("genre"))
                                        .and_where(Expr::col("name").eq(genre.as_str()))
                                        .to_owned(),
                                ),
                            )
                            .to_owned(),
                    ),
                )
                .order_by(("a", "sort_key"), Order::Asc);
            }
        }
        q.order_by(("a", "id"), Order::Asc);
        page(&mut q, p, ("a", "library_id"));
        self.fetch_all(&q).await
    }

    pub async fn tracks_by_album(&self, album_id: i64) -> Result<Vec<TrackRow>> {
        self.fetch_all(
            &tracks_select()
                .and_where(Expr::col(("t", "album_id")).eq(album_id))
                .order_by_expr(Expr::cust("COALESCE(t.disc_no, 0)"), Order::Asc)
                .order_by_expr(Expr::cust("COALESCE(t.track_no, 0)"), Order::Asc)
                .order_by(("t", "sort_key"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    pub async fn track(&self, id: i64) -> Result<Option<TrackRow>> {
        self.fetch_optional(
            &tracks_select()
                .and_where(Expr::col(("t", "id")).eq(id))
                .to_owned(),
        )
        .await
    }

    /// Live tracks among `ids`, in no particular order.
    pub async fn tracks_by_ids(&self, ids: &[i64]) -> Result<Vec<TrackRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all(
            &tracks_select()
                .and_where(Expr::col(("t", "id")).is_in(ids.iter().copied()))
                .to_owned(),
        )
        .await
    }

    pub async fn random_tracks(
        &self,
        genre: Option<&str>,
        years: (Option<i64>, Option<i64>),
        p: Page,
    ) -> Result<Vec<TrackRow>> {
        let mut q = tracks_select();
        if let Some(g) = genre {
            q.and_where(genre_filter(g));
        }
        let year = Expr::cust("COALESCE(t.year, a.year)");
        if let Some(from) = years.0 {
            q.and_where(year.clone().gte(from));
        }
        if let Some(to) = years.1 {
            q.and_where(year.lte(to));
        }
        q.order_by_expr(Expr::cust("RANDOM()"), Order::Asc);
        page(&mut q, p, ("t", "library_id"));
        self.fetch_all(&q).await
    }

    pub async fn tracks_by_genre(&self, genre: &str, p: Page) -> Result<Vec<TrackRow>> {
        let mut q = tracks_select()
            .and_where(genre_filter(genre))
            .order_by(("a", "artist_sort_key"), Order::Asc)
            .order_by(("a", "sort_key"), Order::Asc)
            .order_by_expr(Expr::cust("COALESCE(t.disc_no, 0)"), Order::Asc)
            .order_by_expr(Expr::cust("COALESCE(t.track_no, 0)"), Order::Asc)
            .order_by(("t", "id"), Order::Asc)
            .to_owned();
        page(&mut q, p, ("t", "library_id"));
        self.fetch_all(&q).await
    }

    pub async fn genres(&self) -> Result<Vec<GenreRow>> {
        self.fetch_all(
            &Query::select()
                .column(("g", "name"))
                .expr_as(
                    Expr::cust(
                        "(SELECT COUNT(*) FROM album_tags x JOIN albums a ON a.id = x.album_id \
                         WHERE x.tag_id = g.id AND a.deleted_at IS NULL)",
                    ),
                    "album_count",
                )
                .expr_as(
                    Expr::cust(
                        "(SELECT COUNT(*) FROM tracks t WHERE t.deleted_at IS NULL AND \
                         (t.album_id IN (SELECT x.album_id FROM album_tags x WHERE x.tag_id = g.id) \
                          OR t.id IN (SELECT y.track_id FROM track_tags y WHERE y.tag_id = g.id)))",
                    ),
                    "song_count",
                )
                .from_as("tags", "g")
                .and_where(Expr::col(("g", "kind")).eq("genre"))
                .order_by(("g", "name"), Order::Asc)
                .to_owned(),
        )
        .await
        .map(|rows: Vec<GenreRow>| {
            rows.into_iter()
                .filter(|g| g.album_count > 0 || g.song_count > 0)
                .collect()
        })
    }

    pub async fn search_artists(&self, terms: &[String], p: Page) -> Result<Vec<ArtistRow>> {
        let mut q = artists_select()
            .and_where(Expr::col("album_count").gt(0))
            .order_by("sort_key", Order::Asc)
            .order_by("id", Order::Asc)
            .to_owned();
        match_terms(&mut q, ("artists", "search_norm"), terms);
        page(&mut q, p, ("artists", "library_id"));
        self.fetch_all(&q).await
    }

    pub async fn search_albums(&self, terms: &[String], p: Page) -> Result<Vec<AlbumRow>> {
        let mut q = albums_select()
            .order_by(("a", "sort_key"), Order::Asc)
            .order_by(("a", "id"), Order::Asc)
            .to_owned();
        match_terms(&mut q, ("a", "search_norm"), terms);
        page(&mut q, p, ("a", "library_id"));
        self.fetch_all(&q).await
    }

    pub async fn search_tracks(&self, terms: &[String], p: Page) -> Result<Vec<TrackRow>> {
        let mut q = tracks_select()
            .order_by(("t", "sort_key"), Order::Asc)
            .order_by(("t", "id"), Order::Asc)
            .to_owned();
        match_terms(&mut q, ("t", "search_norm"), terms);
        page(&mut q, p, ("t", "library_id"));
        self.fetch_all(&q).await
    }

    /// Artist credits of the given tracks, ordered by role and position.
    pub async fn track_credits(&self, track_ids: &[i64]) -> Result<Vec<CreditRow>> {
        if track_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all(
            &Query::select()
                .expr_as(Expr::col(("c", "track_id")), "owner_id")
                .column(("c", "artist_id"))
                .expr_as(Expr::col(("ar", "public_id")), "artist_public_id")
                .column(("ar", "name"))
                .column(("c", "role"))
                .column(("c", "pos"))
                .from_as("track_credits", "c")
                .join_as(
                    JoinType::InnerJoin,
                    "artists",
                    "ar",
                    Expr::col(("ar", "id")).equals(("c", "artist_id")),
                )
                .and_where(Expr::col(("c", "track_id")).is_in(track_ids.iter().copied()))
                .order_by(("c", "track_id"), Order::Asc)
                .order_by(("c", "role"), Order::Asc)
                .order_by(("c", "pos"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    pub async fn album_credits(&self, album_ids: &[i64]) -> Result<Vec<CreditRow>> {
        if album_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all(
            &Query::select()
                .expr_as(Expr::col(("aa", "album_id")), "owner_id")
                .column(("aa", "artist_id"))
                .expr_as(Expr::col(("ar", "public_id")), "artist_public_id")
                .column(("ar", "name"))
                .expr_as(Expr::val("albumartist"), "role")
                .column(("aa", "pos"))
                .from_as("album_artists", "aa")
                .join_as(
                    JoinType::InnerJoin,
                    "artists",
                    "ar",
                    Expr::col(("ar", "id")).equals(("aa", "artist_id")),
                )
                .and_where(Expr::col(("aa", "album_id")).is_in(album_ids.iter().copied()))
                .order_by(("aa", "album_id"), Order::Asc)
                .order_by(("aa", "pos"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    pub async fn album_tags(&self, album_ids: &[i64]) -> Result<Vec<TagLink>> {
        self.tag_links("album_tags", "album_id", album_ids).await
    }

    pub async fn track_tags(&self, track_ids: &[i64]) -> Result<Vec<TagLink>> {
        self.tag_links("track_tags", "track_id", track_ids).await
    }

    async fn tag_links(
        &self,
        table: &'static str,
        col: &'static str,
        ids: &[i64],
    ) -> Result<Vec<TagLink>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_all(
            &Query::select()
                .expr_as(Expr::col(("l", col)), "owner_id")
                .column(("g", "kind"))
                .column(("g", "name"))
                .from_as(table, "l")
                .join_as(
                    JoinType::InnerJoin,
                    "tags",
                    "g",
                    Expr::col(("g", "id")).equals(("l", "tag_id")),
                )
                .and_where(Expr::col(("l", col)).is_in(ids.iter().copied()))
                .order_by(("l", col), Order::Asc)
                .order_by(("g", "name"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Artwork of an artist or album; tracks use their album's.
    pub async fn art_source(&self, kind: rsub_core::Kind, id: i64) -> Result<Option<ArtSource>> {
        use rsub_core::Kind;
        let (table, id) = match kind {
            Kind::Artist => ("artists", id),
            Kind::Album => ("albums", id),
            Kind::Track => match self.track(id).await? {
                Some(t) => ("albums", t.album_id),
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
        self.fetch_optional(
            &Query::select()
                .columns(["library_id", "remote_key", "thumb_ref"])
                .from(table)
                .and_where(Expr::col("id").eq(id))
                .and_where(Expr::col("deleted_at").is_null())
                .and_where(Expr::col("remote_key").is_not_null())
                .to_owned(),
        )
        .await
    }

    /// Live artists, real or virtual, whose normalized name is `name`'s.
    pub async fn artists_named(&self, name: &str, library: Option<i64>) -> Result<Vec<ArtistRow>> {
        let mut q = artists_select()
            .and_where(Expr::col("search_norm").eq(normalize(name)))
            .order_by("id", Order::Asc)
            .to_owned();
        if let Some(lib) = library {
            q.and_where(Expr::col("library_id").eq(lib));
        }
        self.fetch_all(&q).await
    }

    /// The most popular songs credited to any of `artist_ids` (as track artist,
    /// or through the album artist), most popular first. Popularity is the
    /// backend's global listener count, not anyone's plays; tracks without one
    /// are left out. A song on several releases (a single, its album, a
    /// compilation) or in several versions ("feat." or not) appears once:
    /// tracks sharing a recording MBID or a normalized title count as one
    /// song, as Plex's "Popular Tracks" groups by title. The copy kept is from
    /// an album that isn't a compilation where possible, else the most popular.
    pub async fn popular_tracks(
        &self,
        artist_ids: &[i64],
        library: Option<i64>,
        limit: usize,
    ) -> Result<Vec<TrackRow>> {
        #[derive(sqlx::FromRow)]
        struct Candidate {
            #[sqlx(flatten)]
            track: TrackRow,
            recording_mbid: Option<String>,
            is_compilation: bool,
        }
        if artist_ids.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let ids = || artist_ids.iter().copied();
        let mut q = tracks_select()
            .expr_as(
                Expr::cust(
                    "(SELECT f.recording_mbid FROM file_tags f WHERE f.library_id = t.library_id \
                     AND f.remote_path = t.remote_path)",
                ),
                "recording_mbid",
            )
            .expr_as(Expr::col(("a", "is_compilation")), "is_compilation")
            .and_where(Expr::col(("t", "popularity")).gt(0))
            .and_where(
                Expr::col(("t", "id"))
                    .in_subquery(
                        Query::select()
                            .column("track_id")
                            .from("track_credits")
                            .and_where(Expr::col("role").eq("artist"))
                            .and_where(Expr::col("artist_id").is_in(ids()))
                            .to_owned(),
                    )
                    .or(Expr::col(("t", "album_id")).in_subquery(
                        Query::select()
                            .column("album_id")
                            .from("album_artists")
                            .and_where(Expr::col("artist_id").is_in(ids()))
                            .to_owned(),
                    )),
            )
            .order_by(("t", "popularity"), Order::Desc)
            .order_by(("t", "id"), Order::Asc)
            .to_owned();
        page(
            &mut q,
            Page::first(POPULAR_CANDIDATES).in_library(library),
            ("t", "library_id"),
        );
        // Songs in order of their most popular copy; the copy kept per song.
        let mut songs: Vec<Candidate> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for c in self.fetch_all::<Candidate>(&q).await? {
            let keys: Vec<String> = c
                .recording_mbid
                .iter()
                .map(|m| format!("mb:{m}"))
                .chain([format!("t:{}", normalize(&c.track.title))])
                .collect();
            let i = match keys.iter().find_map(|k| index.get(k).copied()) {
                Some(i) => {
                    if songs[i].is_compilation && !c.is_compilation {
                        songs[i] = c;
                    }
                    i
                }
                None if songs.len() < limit => {
                    songs.push(c);
                    songs.len() - 1
                }
                None => continue,
            };
            for k in keys {
                index.entry(k).or_insert(i);
            }
        }
        Ok(songs.into_iter().map(|c| c.track).collect())
    }

    pub async fn track_count(&self) -> Result<i64> {
        #[derive(sqlx::FromRow)]
        struct N {
            n: i64,
        }
        Ok(self
            .fetch_one::<N>(
                &Query::select()
                    .expr_as(Expr::col("id").count(), "n")
                    .from("tracks")
                    .and_where(Expr::col("deleted_at").is_null())
                    .to_owned(),
            )
            .await?
            .n)
    }

    /// Newest change time across the catalog (for `getIndexes lastModified`):
    /// the last sync pass of any library, full or incremental, or the last
    /// library removal.
    pub async fn catalog_modified_at(&self) -> Result<i64> {
        #[derive(sqlx::FromRow)]
        struct N {
            n: Option<i64>,
        }
        Ok(self
            .fetch_one::<N>(
                &Query::select()
                    .expr_as(Expr::col("last_sync_at").max(), "n")
                    .from("libraries")
                    .to_owned(),
            )
            .await?
            .n
            .unwrap_or(0))
    }
}
