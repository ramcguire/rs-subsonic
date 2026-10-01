//! Backend abstraction. A backend (Plex today; Jellyfin, a filesystem scanner, …
//! later) implements these traits and is otherwise invisible to the API layer.
//!
//! The local database is the catalog's read path: `CatalogSource` feeds the
//! sync engine. `MediaSource`, `Discovery` and `UserState` are called live.
//!
//! Default methods return [`BackendError::Unsupported`] so a backend only
//! implements what it can.
//!
//! Every time here (`*_at`) is Unix epoch milliseconds, UTC: backends convert
//! their own units when parsing. `updated_at` is the backend's change stamp; the
//! sync engine compares it for equality to spot changed items, so a backend must
//! convert it the same way every time.

use std::io;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;

use crate::id::Kind;

pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;
pub type Result<T, E = BackendError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("operation not supported by this backend")]
    Unsupported,
    #[error("not found")]
    NotFound,
    #[error("backend rejected credentials")]
    Unauthorized,
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("unexpected backend response: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Configured backend instance id (`[[backend]] id = "home"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceId(pub String);

/// Credentials for a call made on behalf of a user.
#[derive(Debug, Clone, Copy)]
pub struct UserCtx<'a> {
    pub user_id: i64,
    /// The user's own backend token; `None` means the backend's shared/admin credentials.
    pub remote_token: Option<&'a str>,
}

/// A reference to an item in the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    pub key: String,
    /// Stable-ish global id (e.g. `plex://track/…`, `mbid://…`), used to revive
    /// soft-deleted rows when the backend re-keys an item.
    pub guid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLibrary {
    pub key: String,
    pub name: String,
    /// Filesystem roots as the backend sees them (for local path mapping).
    pub locations: Vec<String>,
    /// A value that changes whenever the library's content does (Plex's
    /// `contentChangedAt`), compared for equality; `None` if the backend has
    /// none, and then only full syncs run.
    pub change_marker: Option<String>,
    /// The backend is scanning the library, so the marker may still move.
    pub scanning: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagKind {
    Genre,
    Mood,
    Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CreditRole {
    Artist,
    AlbumArtist,
    Composer,
    Other,
}

/// An artist credit; `remote` is `None` for name-only credits (virtual artists).
#[derive(Debug, Clone, PartialEq)]
pub struct CreditRecord {
    pub remote: Option<RemoteRef>,
    pub name: String,
    pub role: CreditRole,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReplayGain {
    pub track_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain: Option<f32>,
    pub album_peak: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtistRecord {
    pub remote: RemoteRef,
    pub name: String,
    pub sort_name: Option<String>,
    pub mbid: Option<String>,
    pub summary: Option<String>,
    pub thumb: Option<String>,
    pub tags: Vec<(TagKind, String)>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlbumRecord {
    pub remote: RemoteRef,
    pub artist: Option<RemoteRef>,
    pub display_artist: String,
    pub title: String,
    pub sort_title: Option<String>,
    pub year: Option<i32>,
    pub release_date: Option<String>,
    pub original_release_date: Option<String>,
    pub label: Option<String>,
    pub release_types: Vec<String>,
    pub is_compilation: bool,
    pub mbid: Option<String>,
    pub thumb: Option<String>,
    pub tags: Vec<(TagKind, String)>,
    pub added_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRecord {
    pub remote: RemoteRef,
    pub album: RemoteRef,
    pub title: String,
    pub sort_title: Option<String>,
    pub display_artist: String,
    pub credits: Vec<CreditRecord>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub duration_ms: u64,
    /// Backend-specific handle for the media file (Plex part key).
    pub part_key: String,
    /// File path as the backend sees it.
    pub remote_path: Option<String>,
    pub size: Option<u64>,
    pub codec: Option<String>,
    pub container: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    pub popularity: Option<u32>,
    pub mbid: Option<String>,
    pub added_at: i64,
    pub updated_at: i64,
}

/// A track's media file, listed by the tag pass before the catalog pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackFile {
    pub key: String,
    pub album_key: String,
    /// The album artist's key, when the backend has one.
    pub artist_key: Option<String>,
    /// File path as the backend sees it.
    pub remote_path: String,
}

/// Detail not present in list views, fetched in the low-priority enrichment pass.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackEnrichment {
    pub key: String,
    pub replay_gain: ReplayGain,
    pub tags: Vec<(TagKind, String)>,
    pub bit_depth: Option<u32>,
    pub sample_rate: Option<u32>,
    pub has_lyrics: bool,
    pub bpm: Option<u32>,
    pub comment: Option<String>,
}

pub enum CatalogBatch {
    Artists(Vec<ArtistRecord>),
    Albums(Vec<AlbumRecord>),
    Tracks(Vec<TrackRecord>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    Playing,
    Paused,
    Stopped,
}

#[async_trait]
pub trait CatalogSource: Send + Sync {
    fn source_id(&self) -> &SourceId;
    async fn libraries(&self) -> Result<Vec<RemoteLibrary>>;
    /// Every item in the library, paged. Must not buffer the whole library.
    fn full_scan<'a>(&'a self, lib: &'a RemoteLibrary) -> BoxStream<'a, Result<CatalogBatch>>;
    /// Items added or changed at or after `since` (a backend change stamp, ms),
    /// artists then albums then tracks, paged. It can't list deletions.
    fn changes_since<'a>(
        &'a self,
        lib: &'a RemoteLibrary,
        since: i64,
    ) -> BoxStream<'a, Result<CatalogBatch>> {
        let _ = (lib, since);
        Box::pin(Once(Some(Err(BackendError::Unsupported))))
    }
    /// Every track's file, or with `since` those of the tracks
    /// [`CatalogSource::changes_since`] lists, paged. Must not buffer the
    /// whole library.
    fn track_files<'a>(
        &'a self,
        lib: &'a RemoteLibrary,
        since: Option<i64>,
    ) -> BoxStream<'a, Result<Vec<TrackFile>>> {
        let _ = (lib, since);
        Box::pin(Once(Some(Err(BackendError::Unsupported))))
    }
    async fn enrich(&self, keys: &[String]) -> Result<Vec<TrackEnrichment>> {
        let _ = keys;
        Err(BackendError::Unsupported)
    }
}

/// What the store knows about a track that a media backend needs.
#[derive(Debug, Clone)]
pub struct TrackRemote {
    pub key: String,
    pub part_key: String,
    pub remote_path: Option<String>,
    pub suffix: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ArtRef {
    pub key: String,
    pub thumb: String,
}

/// Inclusive byte range; `end = None` means to EOF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    From { start: u64, end: Option<u64> },
    Suffix(u64),
}

#[derive(Debug, Clone, Default)]
pub struct MediaRequest {
    pub range: Option<ByteRange>,
}

pub struct MediaStream {
    pub status: u16,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub content_range: Option<String>,
    pub body: BoxStream<'static, io::Result<Bytes>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LyricLine {
    pub start_ms: Option<u64>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LyricsDoc {
    pub lang: String,
    pub synced: bool,
    pub display_artist: Option<String>,
    pub display_title: Option<String>,
    pub offset_ms: i64,
    pub lines: Vec<LyricLine>,
}

#[async_trait]
pub trait MediaSource: Send + Sync {
    async fn open(
        &self,
        ctx: &UserCtx<'_>,
        track: &TrackRemote,
        req: MediaRequest,
    ) -> Result<MediaStream>;
    async fn cover_art(
        &self,
        ctx: &UserCtx<'_>,
        art: &ArtRef,
        size: Option<u32>,
    ) -> Result<MediaStream>;
    async fn lyrics(&self, ctx: &UserCtx<'_>, track: &TrackRemote) -> Result<Vec<LyricsDoc>> {
        let _ = (ctx, track);
        Err(BackendError::Unsupported)
    }
}

#[async_trait]
pub trait Discovery: Send + Sync {
    /// Remote keys of artists similar to `artist`, best first.
    async fn similar_artists(
        &self,
        ctx: &UserCtx<'_>,
        artist: &RemoteRef,
        n: usize,
    ) -> Result<Vec<String>> {
        let _ = (ctx, artist, n);
        Err(BackendError::Unsupported)
    }
}

/// One user's state for one backend item. Ratings are 1–5; a backend with a
/// finer scale rounds down, so 5 means exactly its top rating: a star.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteState {
    pub key: String,
    pub rating: Option<u8>,
    /// When the rating was set (epoch ms); a star's date.
    pub rated_at: Option<i64>,
    pub play_count: Option<u32>,
    pub last_played_at: Option<i64>,
}

impl RemoteState {
    pub fn starred(&self) -> bool {
        self.rating == Some(5)
    }
}

/// Orders of a library's items that have user state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateOrder {
    /// Played, most plays first.
    Frequent,
    /// Played, latest play first.
    Recent,
    /// Starred (rated 5), latest rating first.
    Starred,
    /// Rated, highest first.
    Highest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlaylist {
    pub id: String,
    pub name: String,
    pub comment: Option<String>,
    /// Smart playlists are read-only.
    pub smart: bool,
    pub song_count: u64,
    pub duration_ms: u64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Artwork reference for [`MediaSource::cover_art`].
    pub thumb: Option<String>,
}

/// A playlist entry: the backend's id for the entry (for read-only playlists,
/// possibly just the track key) and the track it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub id: String,
    pub key: String,
}

/// The user's ratings, plays and playlists, read and written live. Nothing is
/// queued: an error fails the call.
#[async_trait]
pub trait UserState: Send + Sync {
    /// Rate an item 1–5; 0 clears the rating.
    async fn rate(&self, ctx: &UserCtx<'_>, key: &str, rating: u8) -> Result<()> {
        let _ = (ctx, key, rating);
        Err(BackendError::Unsupported)
    }
    /// Record a completed play, dated now.
    async fn scrobble(&self, ctx: &UserCtx<'_>, key: &str) -> Result<()> {
        let _ = (ctx, key);
        Err(BackendError::Unsupported)
    }
    async fn now_playing(
        &self,
        ctx: &UserCtx<'_>,
        key: &str,
        state: PlayState,
        offset_ms: u64,
        duration_ms: u64,
    ) -> Result<()> {
        let _ = (ctx, key, state, offset_ms, duration_ms);
        Err(BackendError::Unsupported)
    }
    /// State of these items of library `lib`; items without state may be left out.
    async fn states(
        &self,
        ctx: &UserCtx<'_>,
        lib: &str,
        kind: Kind,
        keys: &[String],
    ) -> Result<Vec<RemoteState>> {
        let _ = (ctx, lib, kind, keys);
        Err(BackendError::Unsupported)
    }
    /// A page of library `lib`'s items of `kind` that have state, in `order`.
    async fn ranked(
        &self,
        ctx: &UserCtx<'_>,
        lib: &str,
        kind: Kind,
        order: StateOrder,
        start: u64,
        size: u64,
    ) -> Result<Vec<RemoteState>> {
        let _ = (ctx, lib, kind, order, start, size);
        Err(BackendError::Unsupported)
    }
    async fn playlists(&self, ctx: &UserCtx<'_>) -> Result<Vec<RemotePlaylist>> {
        let _ = ctx;
        Err(BackendError::Unsupported)
    }
    /// Entries in order.
    async fn playlist_entries(&self, ctx: &UserCtx<'_>, id: &str) -> Result<Vec<PlaylistEntry>> {
        let _ = (ctx, id);
        Err(BackendError::Unsupported)
    }
    /// Create a playlist holding `keys` (possibly none); returns its id.
    async fn create_playlist(
        &self,
        ctx: &UserCtx<'_>,
        name: &str,
        keys: &[String],
    ) -> Result<String> {
        let _ = (ctx, name, keys);
        Err(BackendError::Unsupported)
    }
    /// Change the name and/or comment (`Some("")` clears the comment).
    async fn edit_playlist(
        &self,
        ctx: &UserCtx<'_>,
        id: &str,
        name: Option<&str>,
        comment: Option<&str>,
    ) -> Result<()> {
        let _ = (ctx, id, name, comment);
        Err(BackendError::Unsupported)
    }
    async fn add_to_playlist(&self, ctx: &UserCtx<'_>, id: &str, keys: &[String]) -> Result<()> {
        let _ = (ctx, id, keys);
        Err(BackendError::Unsupported)
    }
    /// Remove entries by [`PlaylistEntry::id`].
    async fn remove_from_playlist(
        &self,
        ctx: &UserCtx<'_>,
        id: &str,
        entries: &[String],
    ) -> Result<()> {
        let _ = (ctx, id, entries);
        Err(BackendError::Unsupported)
    }
    async fn clear_playlist(&self, ctx: &UserCtx<'_>, id: &str) -> Result<()> {
        let _ = (ctx, id);
        Err(BackendError::Unsupported)
    }
    async fn delete_playlist(&self, ctx: &UserCtx<'_>, id: &str) -> Result<()> {
        let _ = (ctx, id);
        Err(BackendError::Unsupported)
    }
}

/// A configured backend: one implementation per concern.
#[derive(Clone)]
pub struct BackendHandle {
    pub catalog: Arc<dyn CatalogSource>,
    pub media: Arc<dyn MediaSource>,
    pub discovery: Arc<dyn Discovery>,
    pub state: Arc<dyn UserState>,
}

/// Stream yielding at most one item; avoids pulling in `futures-util` for defaults.
struct Once<T>(Option<T>);

impl<T: Unpin> Stream for Once<T> {
    type Item = T;
    fn poll_next(
        mut self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        std::task::Poll::Ready(self.0.take())
    }
}
