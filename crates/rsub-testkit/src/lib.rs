//! Test helpers: builders for backend records and an in-memory backend that
//! implements every `rsub_core::backend` trait.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use rsub_core::Kind;
use rsub_core::backend::{
    AlbumRecord, ArtRef, ArtistRecord, BackendError, BackendHandle, BoxStream, ByteRange,
    CatalogBatch, CatalogSource, CreditRecord, CreditRole, Discovery, LyricsDoc, MediaRequest,
    MediaSource, MediaStream, PlayState, PlaylistEntry, RemoteLibrary, RemotePlaylist, RemoteRef,
    RemoteState, Result, SourceId, StateOrder, TrackEnrichment, TrackFile, TrackRecord,
    TrackRemote, UserCtx, UserState,
};
use rsub_core::tags::{FileStamp, FileTags, TagReader};

pub fn remote(key: &str) -> RemoteRef {
    RemoteRef {
        key: key.to_owned(),
        guid: Some(format!("guid://{key}")),
    }
}

pub fn artist(key: &str, name: &str) -> ArtistRecord {
    ArtistRecord {
        remote: remote(key),
        name: name.to_owned(),
        sort_name: None,
        mbid: None,
        summary: None,
        thumb: Some(format!("/thumb/{key}")),
        tags: Vec::new(),
        updated_at: 1_000,
    }
}

pub fn album(key: &str, artist: &ArtistRecord, title: &str) -> AlbumRecord {
    AlbumRecord {
        remote: remote(key),
        artist: Some(artist.remote.clone()),
        display_artist: artist.name.clone(),
        title: title.to_owned(),
        sort_title: None,
        year: Some(2000),
        release_date: None,
        original_release_date: None,
        label: None,
        release_types: vec!["album".into()],
        is_compilation: false,
        mbid: None,
        thumb: Some(format!("/thumb/{key}")),
        tags: Vec::new(),
        added_at: 1_000,
        updated_at: 1_000,
    }
}

/// A track credited to the album's artist.
pub fn track(key: &str, album: &AlbumRecord, no: u32, title: &str) -> TrackRecord {
    let credits = album
        .artist
        .iter()
        .flat_map(|a| {
            [CreditRole::Artist, CreditRole::AlbumArtist].map(|role| CreditRecord {
                remote: Some(a.clone()),
                name: album.display_artist.clone(),
                role,
            })
        })
        .collect();
    TrackRecord {
        remote: remote(key),
        album: album.remote.clone(),
        title: title.to_owned(),
        sort_title: None,
        display_artist: album.display_artist.clone(),
        credits,
        track_no: Some(no),
        disc_no: Some(1),
        year: album.year,
        duration_ms: 180_000,
        part_key: format!("/library/parts/{key}/1/file.flac"),
        remote_path: Some(format!("/music/{}/{title}.flac", album.title)),
        size: Some(FILE_LEN as u64),
        codec: Some("flac".into()),
        container: Some("flac".into()),
        bitrate_kbps: Some(900),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        channels: Some(2),
        popularity: None,
        mbid: None,
        added_at: 1_000,
        updated_at: 1_000,
    }
}

/// Size of every fake media file.
pub const FILE_LEN: usize = 1000;

/// Deterministic content of fake media files: byte `i` is `i % 251`.
pub fn file_bytes() -> Vec<u8> {
    (0..FILE_LEN).map(|i| (i % 251) as u8).collect()
}

#[derive(Debug, Clone, Default)]
pub struct FakeLibrary {
    pub artists: Vec<ArtistRecord>,
    pub albums: Vec<AlbumRecord>,
    pub tracks: Vec<TrackRecord>,
    pub enrichment: HashMap<String, TrackEnrichment>,
}

/// In-memory backend. Mutate the libraries between syncs to simulate changes.
pub struct FakeBackend {
    id: SourceId,
    libs: Mutex<Vec<(RemoteLibrary, FakeLibrary)>>,
    page_size: usize,
    /// Fail full scans of this library key (to test that nothing is swept).
    pub fail_scan_of: Mutex<Option<String>>,
    /// Scans run, as `full {key}` or `changes {key} {since}`.
    pub scans: Mutex<Vec<String>>,
    /// Answer `changes_since` with `Unsupported`.
    pub no_changes: Mutex<bool>,
    /// Bumped by every library change; each library's marker is its value
    /// when the library last changed.
    markers: Mutex<u64>,
    /// Requests seen by `open`, for assertions.
    pub opened: Mutex<Vec<(String, Option<ByteRange>)>>,
    /// The account's ratings, plays and playlists.
    pub account: Mutex<FakeAccount>,
    /// Fail this many upcoming user-state calls with `Unavailable`.
    pub fail_state: Mutex<u32>,
    /// Never answer state reads, like a Plex that accepts connections and
    /// hangs.
    pub hang_state: Mutex<bool>,
    /// Fail this many upcoming `add_to_playlist` calls with `Unavailable`,
    /// after the calls before them succeeded.
    pub fail_adds: Mutex<u32>,
    /// Similar artists' keys by artist key, as Plex's metadata lists them.
    pub similar: Mutex<HashMap<String, Vec<String>>>,
    /// Lyrics by track key. Reads are logged in the account's `calls`.
    pub lyrics: Mutex<HashMap<String, Vec<LyricsDoc>>>,
}

impl FakeBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeBackend {
            id: SourceId("fake".into()),
            libs: Mutex::new(Vec::new()),
            page_size: 2,
            fail_scan_of: Mutex::new(None),
            scans: Mutex::default(),
            no_changes: Mutex::new(false),
            markers: Mutex::new(0),
            opened: Mutex::new(Vec::new()),
            account: Mutex::default(),
            fail_state: Mutex::new(0),
            hang_state: Mutex::new(false),
            fail_adds: Mutex::new(0),
            similar: Mutex::default(),
            lyrics: Mutex::default(),
        })
    }

    /// Add or replace a library; its change marker moves.
    pub fn set_library(&self, key: &str, name: &str, lib: FakeLibrary) {
        let marker = {
            let mut m = self.markers.lock().unwrap();
            *m += 1;
            m.to_string()
        };
        let mut libs = self.libs.lock().unwrap();
        let remote = RemoteLibrary {
            key: key.to_owned(),
            name: name.to_owned(),
            locations: vec!["/music".into()],
            change_marker: Some(marker),
            scanning: false,
        };
        match libs.iter_mut().find(|(r, _)| r.key == key) {
            Some(slot) => *slot = (remote, lib),
            None => libs.push((remote, lib)),
        }
    }

    /// Report the library as being scanned, or not; the marker stays.
    pub fn set_scanning(&self, key: &str, scanning: bool) {
        let mut libs = self.libs.lock().unwrap();
        if let Some((r, _)) = libs.iter_mut().find(|(r, _)| r.key == key) {
            r.scanning = scanning;
        }
    }

    pub fn take_scans(&self) -> Vec<String> {
        std::mem::take(&mut self.scans.lock().unwrap())
    }

    pub fn remove_library(&self, key: &str) {
        self.libs.lock().unwrap().retain(|(r, _)| r.key != key);
    }

    pub fn handle(self: &Arc<Self>) -> BackendHandle {
        BackendHandle {
            catalog: self.clone(),
            media: self.clone(),
            discovery: self.clone(),
            state: self.clone(),
        }
    }

    fn library(&self, key: &str) -> Option<FakeLibrary> {
        self.libs
            .lock()
            .unwrap()
            .iter()
            .find(|(r, _)| r.key == key)
            .map(|(_, l)| l.clone())
    }
}

#[async_trait]
impl CatalogSource for FakeBackend {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    async fn libraries(&self) -> Result<Vec<RemoteLibrary>> {
        Ok(self
            .libs
            .lock()
            .unwrap()
            .iter()
            .map(|(r, _)| r.clone())
            .collect())
    }

    fn full_scan<'a>(&'a self, lib: &'a RemoteLibrary) -> BoxStream<'a, Result<CatalogBatch>> {
        self.scans.lock().unwrap().push(format!("full {}", lib.key));
        let fail = self.fail_scan_of.lock().unwrap().as_deref() == Some(lib.key.as_str());
        let Some(l) = self.library(&lib.key) else {
            return Box::pin(futures_util::stream::iter([Err(BackendError::NotFound)]));
        };
        let n = self.page_size;
        let mut batches: Vec<Result<CatalogBatch>> = Vec::new();
        batches.extend(
            l.artists
                .chunks(n)
                .map(|c| Ok(CatalogBatch::Artists(c.to_vec()))),
        );
        batches.extend(
            l.albums
                .chunks(n)
                .map(|c| Ok(CatalogBatch::Albums(c.to_vec()))),
        );
        if fail {
            batches.push(Err(BackendError::Unavailable("injected failure".into())));
        }
        batches.extend(
            l.tracks
                .chunks(n)
                .map(|c| Ok(CatalogBatch::Tracks(c.to_vec()))),
        );
        Box::pin(futures_util::stream::iter(batches))
    }

    fn changes_since<'a>(
        &'a self,
        lib: &'a RemoteLibrary,
        since: i64,
    ) -> BoxStream<'a, Result<CatalogBatch>> {
        if *self.no_changes.lock().unwrap() {
            return Box::pin(futures_util::stream::iter([Err(BackendError::Unsupported)]));
        }
        self.scans
            .lock()
            .unwrap()
            .push(format!("changes {} {since}", lib.key));
        let Some(l) = self.library(&lib.key) else {
            return Box::pin(futures_util::stream::iter([Err(BackendError::NotFound)]));
        };
        let n = self.page_size;
        let mut batches: Vec<Result<CatalogBatch>> = Vec::new();
        let artists: Vec<_> = l
            .artists
            .into_iter()
            .filter(|a| a.updated_at >= since)
            .collect();
        batches.extend(
            artists
                .chunks(n)
                .map(|c| Ok(CatalogBatch::Artists(c.to_vec()))),
        );
        let albums: Vec<_> = l
            .albums
            .into_iter()
            .filter(|a| a.updated_at >= since || a.added_at >= since)
            .collect();
        batches.extend(
            albums
                .chunks(n)
                .map(|c| Ok(CatalogBatch::Albums(c.to_vec()))),
        );
        let tracks: Vec<_> = l
            .tracks
            .into_iter()
            .filter(|t| t.updated_at >= since || t.added_at >= since)
            .collect();
        batches.extend(
            tracks
                .chunks(n)
                .map(|c| Ok(CatalogBatch::Tracks(c.to_vec()))),
        );
        Box::pin(futures_util::stream::iter(batches))
    }

    fn track_files<'a>(
        &'a self,
        lib: &'a RemoteLibrary,
        since: Option<i64>,
    ) -> BoxStream<'a, Result<Vec<TrackFile>>> {
        let Some(l) = self.library(&lib.key) else {
            return Box::pin(futures_util::stream::iter([Err(BackendError::NotFound)]));
        };
        let since = since.unwrap_or(i64::MIN);
        let files: Vec<TrackFile> = l
            .tracks
            .iter()
            .filter(|t| t.updated_at >= since || t.added_at >= since)
            .filter_map(|t| {
                Some(TrackFile {
                    key: t.remote.key.clone(),
                    album_key: t.album.key.clone(),
                    artist_key: t
                        .credits
                        .iter()
                        .find(|c| c.role == CreditRole::AlbumArtist)
                        .and_then(|c| c.remote.as_ref())
                        .map(|r| r.key.clone()),
                    remote_path: t.remote_path.clone()?,
                })
            })
            .collect();
        let pages: Vec<_> = files
            .chunks(self.page_size)
            .map(|c| Ok(c.to_vec()))
            .collect();
        Box::pin(futures_util::stream::iter(pages))
    }

    async fn enrich(&self, keys: &[String]) -> Result<Vec<TrackEnrichment>> {
        let libs = self.libs.lock().unwrap();
        Ok(keys
            .iter()
            .filter_map(|k| libs.iter().find_map(|(_, l)| l.enrichment.get(k).cloned()))
            .collect())
    }
}

#[async_trait]
impl MediaSource for FakeBackend {
    async fn open(
        &self,
        _ctx: &UserCtx<'_>,
        track: &TrackRemote,
        req: MediaRequest,
    ) -> Result<MediaStream> {
        self.opened
            .lock()
            .unwrap()
            .push((track.part_key.clone(), req.range));
        let data = file_bytes();
        let len = data.len() as u64;
        let (status, body, range) = match req.range {
            None => (200, data, None),
            Some(r) => {
                let (start, end) = match r {
                    ByteRange::From { start, end } => (start, end.unwrap_or(len - 1).min(len - 1)),
                    ByteRange::Suffix(n) => (len.saturating_sub(n), len - 1),
                };
                if start > end {
                    return Err(BackendError::Protocol("unsatisfiable range".into()));
                }
                (
                    206,
                    data[start as usize..=end as usize].to_vec(),
                    Some(format!("bytes {start}-{end}/{len}")),
                )
            }
        };
        Ok(MediaStream {
            status,
            content_type: Some("audio/flac".into()),
            content_length: Some(body.len() as u64),
            content_range: range,
            body: Box::pin(futures_util::stream::iter([Ok(Bytes::from(body))])),
        })
    }

    async fn cover_art(
        &self,
        _ctx: &UserCtx<'_>,
        art: &ArtRef,
        size: Option<u32>,
    ) -> Result<MediaStream> {
        let body = format!("image:{}:{}", art.thumb, size.unwrap_or(0)).into_bytes();
        Ok(MediaStream {
            status: 200,
            content_type: Some("image/jpeg".into()),
            content_length: Some(body.len() as u64),
            content_range: None,
            body: Box::pin(futures_util::stream::iter([Ok(Bytes::from(body))])),
        })
    }

    async fn lyrics(&self, _ctx: &UserCtx<'_>, track: &TrackRemote) -> Result<Vec<LyricsDoc>> {
        self.account().calls.push(format!("lyrics {}", track.key));
        Ok(self
            .lyrics
            .lock()
            .unwrap()
            .get(&track.key)
            .cloned()
            .unwrap_or_default())
    }
}

#[async_trait]
impl Discovery for FakeBackend {
    async fn similar_artists(
        &self,
        _ctx: &UserCtx<'_>,
        artist: &RemoteRef,
        n: usize,
    ) -> Result<Vec<String>> {
        let mut keys = self
            .similar
            .lock()
            .unwrap()
            .get(&artist.key)
            .cloned()
            .unwrap_or_default();
        keys.truncate(n);
        Ok(keys)
    }
}

/// The account's state as [`FakeBackend`] keeps it, shaped like Plex's.
#[derive(Debug, Default)]
pub struct FakeAccount {
    pub items: HashMap<String, RemoteState>,
    pub playlists: Vec<FakePlaylist>,
    /// Writes in order, e.g. `"rate t1 5"`, for assertions.
    pub calls: Vec<String>,
    clock: i64,
    next_id: u64,
}

#[derive(Debug, Clone)]
pub struct FakePlaylist {
    pub info: RemotePlaylist,
    pub entries: Vec<PlaylistEntry>,
}

impl FakeAccount {
    /// Distinct, increasing timestamps.
    fn tick(&mut self) -> i64 {
        self.clock += 1_000;
        1_700_000_000_000 + self.clock
    }

    fn id(&mut self) -> String {
        self.next_id += 1;
        (900 + self.next_id).to_string()
    }

    fn playlist(&mut self, id: &str) -> Result<&mut FakePlaylist> {
        self.playlists
            .iter_mut()
            .find(|p| p.info.id == id)
            .ok_or(BackendError::NotFound)
    }

    /// Append keys the playlist doesn't hold yet: Plex ignores repeats.
    fn append(&mut self, id: &str, keys: &[String]) -> Result<()> {
        let mut fresh = Vec::new();
        for k in keys {
            fresh.push((self.id(), k.clone()));
        }
        let at = self.tick();
        let p = self.playlist(id)?;
        if p.info.smart {
            return Err(BackendError::Unauthorized);
        }
        for (entry, key) in fresh {
            if !p.entries.iter().any(|e| e.key == key) {
                p.entries.push(PlaylistEntry { id: entry, key });
            }
        }
        p.info.song_count = p.entries.len() as u64;
        p.info.updated_at = at;
        Ok(())
    }
}

impl FakeBackend {
    /// Fail the next state call (read or write) with `Unavailable`.
    fn check_up(&self) -> Result<()> {
        let mut fail = self.fail_state.lock().unwrap();
        if *fail > 0 {
            *fail -= 1;
            return Err(BackendError::Unavailable("injected failure".into()));
        }
        Ok(())
    }

    fn account(&self) -> std::sync::MutexGuard<'_, FakeAccount> {
        self.account.lock().unwrap()
    }

    /// Take the recorded writes.
    pub fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.account().calls)
    }

    /// Add a smart playlist, as Plex has built in.
    pub fn add_smart_playlist(&self, name: &str, keys: &[&str]) -> String {
        let mut a = self.account();
        let id = a.id();
        let entries = keys
            .iter()
            .map(|k| PlaylistEntry {
                id: a.id(),
                key: (*k).to_owned(),
            })
            .collect::<Vec<_>>();
        let at = a.tick();
        a.playlists.push(FakePlaylist {
            info: RemotePlaylist {
                id: id.clone(),
                name: name.to_owned(),
                comment: None,
                smart: true,
                song_count: entries.len() as u64,
                duration_ms: 0,
                created_at: at,
                updated_at: at,
                thumb: Some(format!("/playlists/{id}/composite/1")),
            },
            entries,
        });
        id
    }

    /// Keys of library `lib`'s items of `kind`.
    fn keys_of(&self, lib: &str, kind: Kind) -> Vec<String> {
        let Some(l) = self.library(lib) else {
            return Vec::new();
        };
        match kind {
            Kind::Artist => l.artists.iter().map(|a| a.remote.key.clone()).collect(),
            Kind::Album => l.albums.iter().map(|a| a.remote.key.clone()).collect(),
            Kind::Track => l.tracks.iter().map(|t| t.remote.key.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// A track's album and album artist, which Plex also counts a play for.
    fn parents(&self, key: &str) -> Vec<String> {
        let libs = self.libs.lock().unwrap();
        let Some((t, l)) = libs
            .iter()
            .find_map(|(_, l)| Some((l.tracks.iter().find(|t| t.remote.key == key)?, l)))
        else {
            return Vec::new();
        };
        let mut out = vec![t.album.key.clone()];
        if let Some(a) = l.albums.iter().find(|a| a.remote == t.album)
            && let Some(artist) = &a.artist
        {
            out.push(artist.key.clone());
        }
        out
    }
}

/// Plex's user state, in memory: ratings round-trip, a play also counts for
/// the album and its artist, playlist ids and entry ids are numbers from 901
/// up, and a playlist holds each track once.
#[async_trait]
impl UserState for FakeBackend {
    async fn rate(&self, _ctx: &UserCtx<'_>, key: &str, rating: u8) -> Result<()> {
        self.check_up()?;
        let mut a = self.account();
        let at = a.tick();
        a.calls.push(format!("rate {key} {rating}"));
        let s = a
            .items
            .entry(key.to_owned())
            .or_insert_with(|| RemoteState {
                key: key.to_owned(),
                ..Default::default()
            });
        (s.rating, s.rated_at) = match rating {
            0 => (None, None),
            r => (Some(r.min(5)), Some(at)),
        };
        Ok(())
    }

    async fn scrobble(&self, _ctx: &UserCtx<'_>, key: &str) -> Result<()> {
        self.check_up()?;
        let parents = self.parents(key);
        let mut a = self.account();
        let at = a.tick();
        a.calls.push(format!("scrobble {key}"));
        for k in std::iter::once(key.to_owned()).chain(parents) {
            let s = a.items.entry(k.clone()).or_insert_with(|| RemoteState {
                key: k,
                ..Default::default()
            });
            s.play_count = Some(s.play_count.unwrap_or(0) + 1);
            s.last_played_at = Some(at);
        }
        Ok(())
    }

    async fn now_playing(
        &self,
        _ctx: &UserCtx<'_>,
        key: &str,
        _state: PlayState,
        _offset_ms: u64,
        _duration_ms: u64,
    ) -> Result<()> {
        self.check_up()?;
        self.account().calls.push(format!("now_playing {key}"));
        Ok(())
    }

    async fn states(
        &self,
        _ctx: &UserCtx<'_>,
        _lib: &str,
        _kind: Kind,
        keys: &[String],
    ) -> Result<Vec<RemoteState>> {
        if *self.hang_state.lock().unwrap() {
            std::future::pending::<()>().await;
        }
        self.check_up()?;
        let a = self.account();
        Ok(keys
            .iter()
            .filter_map(|k| a.items.get(k).cloned())
            .collect())
    }

    async fn ranked(
        &self,
        _ctx: &UserCtx<'_>,
        lib: &str,
        kind: Kind,
        order: StateOrder,
        start: u64,
        size: u64,
    ) -> Result<Vec<RemoteState>> {
        self.check_up()?;
        let keys = self.keys_of(lib, kind);
        let a = self.account();
        let mut v: Vec<RemoteState> = keys
            .iter()
            .filter_map(|k| a.items.get(k).cloned())
            .filter(|s| match order {
                StateOrder::Frequent | StateOrder::Recent => s.play_count.is_some(),
                StateOrder::Starred => s.starred(),
                StateOrder::Highest => s.rating.is_some(),
            })
            .collect();
        match order {
            StateOrder::Frequent => {
                v.sort_by_key(|s| std::cmp::Reverse((s.play_count, s.last_played_at)))
            }
            StateOrder::Recent => v.sort_by_key(|s| std::cmp::Reverse(s.last_played_at)),
            StateOrder::Starred => v.sort_by_key(|s| std::cmp::Reverse(s.rated_at)),
            StateOrder::Highest => v.sort_by_key(|s| std::cmp::Reverse((s.rating, s.play_count))),
        }
        Ok(v.into_iter()
            .skip(start as usize)
            .take(size as usize)
            .collect())
    }

    async fn playlists(&self, _ctx: &UserCtx<'_>) -> Result<Vec<RemotePlaylist>> {
        self.check_up()?;
        Ok(self
            .account()
            .playlists
            .iter()
            .map(|p| p.info.clone())
            .collect())
    }

    async fn playlist_entries(&self, _ctx: &UserCtx<'_>, id: &str) -> Result<Vec<PlaylistEntry>> {
        self.check_up()?;
        Ok(self.account().playlist(id)?.entries.clone())
    }

    async fn create_playlist(
        &self,
        _ctx: &UserCtx<'_>,
        name: &str,
        keys: &[String],
    ) -> Result<String> {
        self.check_up()?;
        let mut a = self.account();
        let id = a.id();
        let at = a.tick();
        a.calls.push(format!("create {name} [{}]", keys.join(",")));
        a.playlists.push(FakePlaylist {
            info: RemotePlaylist {
                id: id.clone(),
                name: name.to_owned(),
                comment: None,
                smart: false,
                song_count: 0,
                duration_ms: 0,
                created_at: at,
                updated_at: at,
                thumb: Some(format!("/playlists/{id}/composite/1")),
            },
            entries: Vec::new(),
        });
        a.append(&id, keys)?;
        Ok(id)
    }

    async fn edit_playlist(
        &self,
        _ctx: &UserCtx<'_>,
        id: &str,
        name: Option<&str>,
        comment: Option<&str>,
    ) -> Result<()> {
        self.check_up()?;
        let mut a = self.account();
        a.calls.push(format!("edit {id} {name:?} {comment:?}"));
        let p = a.playlist(id)?;
        if let Some(n) = name {
            p.info.name = n.to_owned();
        }
        if let Some(c) = comment {
            p.info.comment = Some(c.to_owned()).filter(|c| !c.is_empty());
        }
        Ok(())
    }

    async fn add_to_playlist(&self, _ctx: &UserCtx<'_>, id: &str, keys: &[String]) -> Result<()> {
        self.check_up()?;
        {
            let mut fail = self.fail_adds.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(BackendError::Unavailable("injected failure".into()));
            }
        }
        let mut a = self.account();
        a.calls.push(format!("add {id} [{}]", keys.join(",")));
        a.append(id, keys)
    }

    async fn remove_from_playlist(
        &self,
        _ctx: &UserCtx<'_>,
        id: &str,
        entries: &[String],
    ) -> Result<()> {
        self.check_up()?;
        let mut a = self.account();
        a.calls.push(format!("remove {id} [{}]", entries.join(",")));
        let p = a.playlist(id)?;
        p.entries.retain(|e| !entries.contains(&e.id));
        p.info.song_count = p.entries.len() as u64;
        Ok(())
    }

    async fn clear_playlist(&self, _ctx: &UserCtx<'_>, id: &str) -> Result<()> {
        self.check_up()?;
        let mut a = self.account();
        a.calls.push(format!("clear {id}"));
        let p = a.playlist(id)?;
        p.entries.clear();
        p.info.song_count = 0;
        Ok(())
    }

    async fn delete_playlist(&self, _ctx: &UserCtx<'_>, id: &str) -> Result<()> {
        self.check_up()?;
        let mut a = self.account();
        a.calls.push(format!("delete {id}"));
        a.playlist(id)?;
        a.playlists.retain(|p| p.info.id != id);
        Ok(())
    }
}

/// A file as [`FakeTags`] serves it.
#[derive(Debug, Clone)]
pub enum FakeFile {
    Tagged(FileStamp, Box<FileTags>),
    /// Present but unparsable (`InvalidData`).
    Corrupt(FileStamp),
    /// Stat works, reading fails with an I/O error (a flaky mount).
    Failing(FileStamp),
}

/// In-memory [`TagReader`] keyed by backend path; unknown paths are
/// unavailable, as if unmapped.
#[derive(Default)]
pub struct FakeTags {
    files: Mutex<HashMap<String, FakeFile>>,
    /// Paths read, in order.
    pub reads: Mutex<Vec<String>>,
}

impl FakeTags {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeTags::default())
    }

    pub fn set(&self, path: &str, file: FakeFile) {
        self.files.lock().unwrap().insert(path.to_owned(), file);
    }

    /// A readable file with these tags, modified at `mtime_ms`.
    pub fn tag(&self, path: &str, mtime_ms: i64, tags: FileTags) {
        self.set(path, FakeFile::Tagged(stamp(mtime_ms), Box::new(tags)));
    }

    pub fn remove(&self, path: &str) {
        self.files.lock().unwrap().remove(path);
    }

    pub fn take_reads(&self) -> Vec<String> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}

/// The stamp of a fake file of [`FILE_LEN`] bytes.
pub fn stamp(mtime_ms: i64) -> FileStamp {
    FileStamp {
        size: FILE_LEN as u64,
        mtime_ms,
    }
}

#[async_trait]
impl TagReader for FakeTags {
    async fn stat(&self, remote_path: &str) -> std::io::Result<FileStamp> {
        match self.files.lock().unwrap().get(remote_path) {
            Some(FakeFile::Tagged(s, _) | FakeFile::Corrupt(s) | FakeFile::Failing(s)) => Ok(*s),
            None => Err(std::io::ErrorKind::NotFound.into()),
        }
    }

    async fn read(&self, remote_path: &str) -> std::io::Result<FileTags> {
        use std::io::{Error, ErrorKind};
        self.reads.lock().unwrap().push(remote_path.to_owned());
        match self.files.lock().unwrap().get(remote_path) {
            Some(FakeFile::Tagged(_, t)) => Ok((**t).clone()),
            Some(FakeFile::Corrupt(_)) => Err(Error::new(ErrorKind::InvalidData, "corrupt")),
            Some(FakeFile::Failing(_)) => Err(Error::new(ErrorKind::TimedOut, "mount stalled")),
            None => Err(Error::from(ErrorKind::NotFound)),
        }
    }
}
