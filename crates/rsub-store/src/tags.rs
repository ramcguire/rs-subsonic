//! The tag-pass cache, `file_tags`: tags read from each track's file, keyed by
//! backend path and re-read only when the file's size or mtime change.

use std::collections::HashMap;

use rsub_core::backend::TrackFile;
use rsub_core::now_ms;
use rsub_core::tags::{FileStamp, FileTags};
use sea_query::{Expr, ExprTrait, OnConflict, Query};

use crate::{CHUNK, Db, Result, Tx};

/// What the cache holds for one path, enough to decide whether to re-read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    pub stamp: FileStamp,
    pub track_key: String,
    pub album_key: String,
    pub artist_key: Option<String>,
    pub release_track_mbid: Option<String>,
}

impl CachedFile {
    /// Whether the cached item keys still match the backend's listing.
    pub fn keys_match(&self, f: &TrackFile) -> bool {
        self.track_key == f.key && self.album_key == f.album_key && self.artist_key == f.artist_key
    }
}

/// One change to the cache in a tag-pass page.
#[derive(Debug, Clone, Copy)]
pub enum FileTagsWrite<'a> {
    /// Still listed and unchanged: stamp the generation only.
    Touch(&'a str),
    /// Still listed under different item keys; keep the cached tags.
    Rekey(&'a TrackFile),
    /// Tags (re)read from the file.
    Read(&'a TrackFile, FileStamp, &'a FileTags),
}

#[derive(sqlx::FromRow)]
struct CachedRow {
    remote_path: String,
    size: i64,
    mtime: i64,
    track_key: String,
    album_key: String,
    artist_key: Option<String>,
    release_track_mbid: Option<String>,
}

#[derive(sqlx::FromRow)]
struct TagsRow {
    #[sqlx(default)]
    k: String,
    #[sqlx(default)]
    remote_path: String,
    release_track_mbid: Option<String>,
    recording_mbid: Option<String>,
    release_mbid: Option<String>,
    artist_mbids: String,
    album_artist_mbids: String,
    artists: String,
    album_artists: String,
    title: Option<String>,
    album: Option<String>,
    disc_no: Option<i64>,
    track_no: Option<i64>,
}

fn json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn list(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

const TAG_COLS: [&str; 11] = [
    "release_track_mbid",
    "recording_mbid",
    "release_mbid",
    "artist_mbids",
    "album_artist_mbids",
    "artists",
    "album_artists",
    "title",
    "album",
    "disc_no",
    "track_no",
];

impl TagsRow {
    fn into_tags(self) -> FileTags {
        FileTags {
            release_track_mbid: self.release_track_mbid,
            recording_mbid: self.recording_mbid,
            release_mbid: self.release_mbid,
            artist_mbids: list(&self.artist_mbids),
            album_artist_mbids: list(&self.album_artist_mbids),
            artists: list(&self.artists),
            album_artists: list(&self.album_artists),
            title: self.title,
            album: self.album,
            disc_no: self.disc_no.and_then(|n| u32::try_from(n).ok()),
            track_no: self.track_no.and_then(|n| u32::try_from(n).ok()),
        }
    }
}

/// Which backend key of a file to look tags up by.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FileKey {
    Track,
    Album,
    /// The album artist.
    Artist,
}

impl FileKey {
    pub(crate) fn col(self) -> &'static str {
        match self {
            FileKey::Track => "track_key",
            FileKey::Album => "album_key",
            FileKey::Artist => "artist_key",
        }
    }
}

/// The cached tags of every file of the items with backend keys `keys`, as
/// `(remote path, tags)` in path order, by key.
pub(crate) async fn tags_by_key(
    tx: &mut Tx,
    library_id: i64,
    by: FileKey,
    keys: &[&str],
) -> Result<HashMap<String, Vec<(String, FileTags)>>> {
    let mut out: HashMap<String, Vec<(String, FileTags)>> = HashMap::new();
    for chunk in keys.chunks(CHUNK) {
        let rows: Vec<TagsRow> = tx
            .fetch_all(
                &Query::select()
                    .expr_as(Expr::col(by.col()), "k")
                    .column("remote_path")
                    .columns(TAG_COLS)
                    .from("file_tags")
                    .and_where(Expr::col("library_id").eq(library_id))
                    .and_where(Expr::col(by.col()).is_in(chunk.iter().copied()))
                    .order_by("remote_path", sea_query::Order::Asc)
                    .to_owned(),
            )
            .await?;
        for mut r in rows {
            let k = std::mem::take(&mut r.k);
            let path = std::mem::take(&mut r.remote_path);
            out.entry(k).or_default().push((path, r.into_tags()));
        }
    }
    Ok(out)
}

/// Whether the tag pass stamped with `generation` listed a file of the item
/// with backend key `key`.
pub(crate) async fn listed(
    tx: &mut Tx,
    library_id: i64,
    generation: i64,
    by: FileKey,
    key: &str,
) -> Result<bool> {
    #[derive(sqlx::FromRow)]
    struct One {
        #[allow(dead_code)]
        remote_path: String,
    }
    let rows: Vec<One> = tx
        .fetch_all(
            &Query::select()
                .column("remote_path")
                .from("file_tags")
                .and_where(Expr::col("library_id").eq(library_id))
                .and_where(Expr::col("generation").eq(generation))
                .and_where(Expr::col(by.col()).eq(key))
                .limit(1)
                .to_owned(),
        )
        .await?;
    Ok(!rows.is_empty())
}

/// Whether the tag pass stamped with `generation` listed any file of the
/// library: without one (no mounted library), [`listed`] can't tell an item
/// that is gone from one that wasn't looked for.
pub(crate) async fn any_listed(tx: &mut Tx, library_id: i64, generation: i64) -> Result<bool> {
    #[derive(sqlx::FromRow)]
    struct One {
        #[allow(dead_code)]
        remote_path: String,
    }
    let rows: Vec<One> = tx
        .fetch_all(
            &Query::select()
                .column("remote_path")
                .from("file_tags")
                .and_where(Expr::col("library_id").eq(library_id))
                .and_where(Expr::col("generation").eq(generation))
                .limit(1)
                .to_owned(),
        )
        .await?;
    Ok(!rows.is_empty())
}

const WRITE_COLS: [&str; 20] = [
    "library_id",
    "remote_path",
    "track_key",
    "album_key",
    "artist_key",
    "size",
    "mtime",
    "read_at",
    "generation",
    "release_track_mbid",
    "recording_mbid",
    "release_mbid",
    "artist_mbids",
    "album_artist_mbids",
    "artists",
    "album_artists",
    "title",
    "album",
    "disc_no",
    "track_no",
];

impl Db {
    /// Cache entries for `paths` in the library, by path.
    pub async fn cached_file_tags(
        &self,
        library_id: i64,
        paths: &[&str],
    ) -> Result<HashMap<String, CachedFile>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<CachedRow> = self
            .fetch_all(
                &Query::select()
                    .columns([
                        "remote_path",
                        "size",
                        "mtime",
                        "track_key",
                        "album_key",
                        "artist_key",
                        "release_track_mbid",
                    ])
                    .from("file_tags")
                    .and_where(Expr::col("library_id").eq(library_id))
                    .and_where(Expr::col("remote_path").is_in(paths.iter().copied()))
                    .to_owned(),
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let cached = CachedFile {
                    stamp: FileStamp {
                        size: u64::try_from(r.size).unwrap_or(0),
                        mtime_ms: r.mtime,
                    },
                    track_key: r.track_key,
                    album_key: r.album_key,
                    artist_key: r.artist_key,
                    release_track_mbid: r.release_track_mbid,
                };
                (r.remote_path, cached)
            })
            .collect())
    }

    /// Apply one page of the tag pass in a transaction, stamping every entry
    /// with `generation`.
    pub async fn write_file_tags(
        &self,
        library_id: i64,
        generation: i64,
        writes: &[FileTagsWrite<'_>],
    ) -> Result<()> {
        let mut tx = self.begin().await?;
        let now = now_ms();
        let mut touched = Vec::new();
        for w in writes {
            match *w {
                FileTagsWrite::Touch(path) => touched.push(path),
                FileTagsWrite::Rekey(f) => {
                    tx.execute(
                        &Query::update()
                            .table("file_tags")
                            .value("track_key", f.key.as_str())
                            .value("album_key", f.album_key.as_str())
                            .value("artist_key", f.artist_key.as_deref())
                            .value("generation", generation)
                            .and_where(Expr::col("library_id").eq(library_id))
                            .and_where(Expr::col("remote_path").eq(f.remote_path.as_str()))
                            .to_owned(),
                    )
                    .await?;
                }
                FileTagsWrite::Read(f, stamp, t) => {
                    let q = Query::insert()
                        .into_table("file_tags")
                        .columns(WRITE_COLS)
                        .values_panic([
                            library_id.into(),
                            f.remote_path.as_str().into(),
                            f.key.as_str().into(),
                            f.album_key.as_str().into(),
                            f.artist_key.as_deref().into(),
                            i64::try_from(stamp.size).unwrap_or(i64::MAX).into(),
                            stamp.mtime_ms.into(),
                            now.into(),
                            generation.into(),
                            t.release_track_mbid.as_deref().into(),
                            t.recording_mbid.as_deref().into(),
                            t.release_mbid.as_deref().into(),
                            json(&t.artist_mbids).into(),
                            json(&t.album_artist_mbids).into(),
                            json(&t.artists).into(),
                            json(&t.album_artists).into(),
                            t.title.as_deref().into(),
                            t.album.as_deref().into(),
                            t.disc_no.map(i64::from).into(),
                            t.track_no.map(i64::from).into(),
                        ])
                        .on_conflict(
                            OnConflict::columns(["library_id", "remote_path"])
                                .update_columns(WRITE_COLS[2..].iter().copied())
                                .to_owned(),
                        )
                        .to_owned();
                    tx.execute(&q).await?;
                }
            }
        }
        for chunk in touched.chunks(CHUNK) {
            tx.execute(
                &Query::update()
                    .table("file_tags")
                    .value("generation", generation)
                    .and_where(Expr::col("library_id").eq(library_id))
                    .and_where(Expr::col("remote_path").is_in(chunk.iter().copied()))
                    .to_owned(),
            )
            .await?;
        }
        tx.commit().await
    }

    /// Drop entries for files the backend no longer lists, after a complete
    /// tag pass stamped the live ones with `generation`.
    pub async fn prune_file_tags(&self, library_id: i64, generation: i64) -> Result<u64> {
        self.execute(
            &Query::delete()
                .from_table("file_tags")
                .and_where(Expr::col("library_id").eq(library_id))
                .and_where(Expr::col("generation").lt(generation))
                .to_owned(),
        )
        .await
    }

    /// The cached tags of one file.
    pub async fn file_tags(&self, library_id: i64, remote_path: &str) -> Result<Option<FileTags>> {
        let row: Option<TagsRow> = self
            .fetch_optional(
                &Query::select()
                    .columns(TAG_COLS)
                    .from("file_tags")
                    .and_where(Expr::col("library_id").eq(library_id))
                    .and_where(Expr::col("remote_path").eq(remote_path))
                    .to_owned(),
            )
            .await?;
        Ok(row.map(TagsRow::into_tags))
    }
}
