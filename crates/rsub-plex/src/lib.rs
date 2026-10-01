//! Plex Media Server backend: implements the `rsub_core::backend` traits on top
//! of the Plex HTTP API.

pub mod client;
mod map;
pub mod model;

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt, TryStreamExt, stream};
use reqwest::Method;
use reqwest::header::{self, HeaderMap, HeaderValue};
use rsub_core::Kind;
use rsub_core::backend::{
    ArtRef, BackendError, BackendHandle, BoxStream, ByteRange, CatalogBatch, CatalogSource,
    Discovery, LyricsDoc, MediaRequest, MediaSource, MediaStream, PlayState, PlaylistEntry,
    RemoteLibrary, RemotePlaylist, RemoteRef, RemoteState, Result, SourceId, StateOrder, TagKind,
    TrackEnrichment, TrackFile, TrackRemote, UserCtx, UserState,
};

pub use client::PlexClient;
use model::{
    Identity, LyricsPage, Metadata, MetadataPage, STREAM_LYRICS, Sections, TagDirectories,
};

/// Plex metadata type ids used by `/library/sections/{id}/all?type=`.
const TYPE_ARTIST: u8 = 8;
const TYPE_ALBUM: u8 = 9;
const TYPE_TRACK: u8 = 10;

/// Library agent identifier for `/:/rate` and `/:/scrobble`.
const LIBRARY_AGENT: &str = "com.plexapp.plugins.library";
/// Items per playlist request, keeping URLs a reasonable length.
const PLAYLIST_CHUNK: usize = 200;
/// Pages of a long list fetched at once.
const PAGE_CONCURRENCY: usize = 4;
/// Artists fetched per style when gathering artist styles.
const STYLE_PAGE: u64 = 5000;
/// Items per by-key state request.
const STATE_CHUNK: usize = 100;
/// Most items whose state is fetched by key; beyond this the library's items
/// with state are listed instead.
const STATE_BY_KEY: usize = 300;

#[derive(Debug, Clone)]
pub struct PlexConfig {
    pub url: String,
    pub token: String,
    pub client_id: String,
    /// Items per catalog page.
    pub page_size: u64,
}

pub struct PlexBackend {
    id: SourceId,
    client: PlexClient,
    page_size: u64,
    /// `machineIdentifier`, needed for playlist item URIs; fetched once.
    machine_id: OnceLock<String>,
}

impl PlexBackend {
    pub fn new(id: &str, cfg: &PlexConfig) -> Result<Arc<Self>> {
        Ok(Arc::new(PlexBackend {
            id: SourceId(id.to_owned()),
            client: PlexClient::new(&cfg.url, &cfg.token, &cfg.client_id)?,
            page_size: cfg.page_size.clamp(1, 5000),
            machine_id: OnceLock::new(),
        }))
    }

    pub fn handle(self: &Arc<Self>) -> BackendHandle {
        BackendHandle {
            catalog: self.clone(),
            media: self.clone(),
            discovery: self.clone(),
            state: self.clone(),
        }
    }

    /// `(machineIdentifier, version)`; also a cheap connectivity/credentials check.
    pub async fn identity(&self) -> Result<(String, String)> {
        let id: Identity = self.client.get_json("/identity", None).await?;
        let machine = id.machine_identifier.unwrap_or_default();
        if !machine.is_empty() {
            let _ = self.machine_id.set(machine.clone());
        }
        Ok((machine, id.version.unwrap_or_default()))
    }

    async fn machine_id(&self) -> Result<String> {
        if let Some(m) = self.machine_id.get() {
            return Ok(m.clone());
        }
        match self.identity().await?.0 {
            m if m.is_empty() => Err(BackendError::Protocol("no machineIdentifier".into())),
            m => Ok(m),
        }
    }

    async fn page(
        &self,
        lib: &str,
        kind: u8,
        filter: &str,
        start: u64,
        size: u64,
    ) -> Result<MetadataPage> {
        let path = format!("/library/sections/{lib}/all?type={kind}&includeGuids=1{filter}");
        self.client.get_json(&path, Some((start, size))).await
    }

    /// A library's items of one type, a page at a time; an error ends it.
    fn pages<'a>(
        &'a self,
        lib: &'a str,
        kind: u8,
    ) -> impl Stream<Item = Result<MetadataPage>> + Send + 'a {
        self.filtered_pages(lib, kind, String::new())
    }

    /// Items of one type added or changed at or after `since` (epoch ms).
    /// Plex leaves `updatedAt` out until an item first changes, so those
    /// added since are listed apart; an item that has it is at least as new
    /// as its `addedAt`, so the second listing keeps only those without it.
    fn changed_pages<'a>(
        &'a self,
        lib: &'a str,
        kind: u8,
        since: i64,
    ) -> impl Stream<Item = Result<MetadataPage>> + Send + 'a {
        let secs = since.div_euclid(1000);
        let added = self
            .filtered_pages(lib, kind, format!("&addedAt>>={secs}"))
            .map_ok(|mut page| {
                page.metadata.retain(|m| m.updated_at.is_none());
                page
            });
        self.filtered_pages(lib, kind, format!("&updatedAt>>={secs}"))
            .chain(added)
            .try_filter(|page| std::future::ready(!page.metadata.is_empty()))
    }

    /// [`PlexBackend::pages`] with `filter` appended to the query.
    fn filtered_pages<'a>(
        &'a self,
        lib: &'a str,
        kind: u8,
        filter: String,
    ) -> impl Stream<Item = Result<MetadataPage>> + Send + 'a {
        let size = self.page_size;
        stream::unfold(Some(0u64), move |start| {
            let filter = filter.clone();
            async move {
                let start = start?;
                match self.page(lib, kind, &filter, start, size).await {
                    Err(e) => Some((Err(e), None)),
                    Ok(page) => {
                        let got = page.metadata.len() as u64;
                        let total = page.total_size.or(page.size).unwrap_or(0);
                        let next = (got > 0 && start + got < total).then_some(start + got);
                        Some((Ok(page), next))
                    }
                }
            }
        })
    }

    /// Artist styles by rating key, sorted. The artist listing leaves styles
    /// out, so they're gathered one style at a time (a library has a hundred
    /// or two).
    async fn artist_styles(&self, lib: &str) -> Result<Styles> {
        let path = format!("/library/sections/{lib}/style?type={TYPE_ARTIST}");
        let dirs: TagDirectories = self.client.get_json(&path, None).await?;
        let mut pages = stream::iter(dirs.directory)
            .map(|d| async move {
                let path = format!(
                    "/library/sections/{lib}/all?type={TYPE_ARTIST}&style={}",
                    d.key
                );
                let page: MetadataPage = self.client.get_json(&path, Some((0, STYLE_PAGE))).await?;
                Ok::<_, BackendError>((d.title, page))
            })
            .buffer_unordered(PAGE_CONCURRENCY);
        let mut styles = Styles::new();
        while let Some(r) = pages.next().await {
            let (style, page) = r?;
            for m in page.metadata {
                styles.entry(m.rating_key).or_default().push(style.clone());
            }
        }
        styles.values_mut().for_each(|v| v.sort());
        Ok(styles)
    }

    /// [`PlexBackend::artist_styles`], or none when they can't be listed:
    /// styles only refine similar artists, so they never fail a sync.
    async fn artist_styles_or_none(&self, lib: &str) -> Arc<Styles> {
        match self.artist_styles(lib).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(library = lib, "cannot list artist styles: {e}");
                Arc::default()
            }
        }
    }
}

/// Artist styles by rating key.
type Styles = HashMap<String, Vec<String>>;

fn batch(kind: u8, page: MetadataPage, styles: &Styles) -> CatalogBatch {
    let items = page.metadata.into_iter();
    match kind {
        TYPE_ARTIST => CatalogBatch::Artists(
            items
                .map(|m| {
                    let listed = styles.get(&m.rating_key).cloned().unwrap_or_default();
                    let mut a = map::artist(m);
                    a.tags
                        .extend(listed.into_iter().map(|s| (TagKind::Style, s)));
                    a
                })
                .collect(),
        ),
        TYPE_ALBUM => CatalogBatch::Albums(items.map(map::album).collect()),
        _ => CatalogBatch::Tracks(items.filter_map(map::track).collect()),
    }
}

#[async_trait]
impl CatalogSource for PlexBackend {
    fn source_id(&self) -> &SourceId {
        &self.id
    }

    async fn libraries(&self) -> Result<Vec<RemoteLibrary>> {
        let s: Sections = self.client.get_json("/library/sections", None).await?;
        Ok(s.directory
            .into_iter()
            .filter(|d| d.kind == "artist")
            .map(|d| RemoteLibrary {
                key: d.key,
                name: d.title,
                locations: d.location.into_iter().map(|l| l.path).collect(),
                change_marker: d.content_changed_at,
                scanning: d.refreshing,
            })
            .collect())
    }

    fn full_scan<'a>(&'a self, lib: &'a RemoteLibrary) -> BoxStream<'a, Result<CatalogBatch>> {
        // Artist styles are listed once, before the artists they go with.
        let styles = stream::once(self.artist_styles_or_none(&lib.key));
        Box::pin(styles.flat_map(move |styles| {
            stream::iter([TYPE_ARTIST, TYPE_ALBUM, TYPE_TRACK]).flat_map(move |kind| {
                let styles = styles.clone();
                self.pages(&lib.key, kind)
                    .map_ok(move |page| batch(kind, page, &styles))
            })
        }))
    }

    fn changes_since<'a>(
        &'a self,
        lib: &'a RemoteLibrary,
        since: i64,
    ) -> BoxStream<'a, Result<CatalogBatch>> {
        // Artist styles are fetched only when an artist changed: listing them
        // takes a request per style.
        let artists = stream::once(async move {
            let pages: Vec<MetadataPage> = self
                .changed_pages(&lib.key, TYPE_ARTIST, since)
                .try_collect()
                .await?;
            let styles = match pages.is_empty() {
                true => Arc::default(),
                false => self.artist_styles_or_none(&lib.key).await,
            };
            Ok::<_, BackendError>(stream::iter(
                pages
                    .into_iter()
                    .map(move |p| Ok(batch(TYPE_ARTIST, p, &styles))),
            ))
        })
        .try_flatten();
        let rest = stream::iter([TYPE_ALBUM, TYPE_TRACK]).flat_map(move |kind| {
            self.changed_pages(&lib.key, kind, since)
                .map_ok(move |page| batch(kind, page, &Styles::new()))
        });
        Box::pin(artists.chain(rest))
    }

    fn track_files<'a>(
        &'a self,
        lib: &'a RemoteLibrary,
        since: Option<i64>,
    ) -> BoxStream<'a, Result<Vec<TrackFile>>> {
        let pages = match since {
            Some(since) => self.changed_pages(&lib.key, TYPE_TRACK, since).boxed(),
            None => self.pages(&lib.key, TYPE_TRACK).boxed(),
        };
        Box::pin(pages.map_ok(|page| {
            page.metadata
                .into_iter()
                .filter_map(map::track_file)
                .collect()
        }))
    }

    async fn enrich(&self, keys: &[String]) -> Result<Vec<TrackEnrichment>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let path = format!("/library/metadata/{}", keys.join(","));
        let page: MetadataPage = self.client.get_json(&path, None).await?;
        Ok(page.metadata.into_iter().map(map::enrichment).collect())
    }
}

fn range_header(r: ByteRange) -> String {
    match r {
        ByteRange::From {
            start,
            end: Some(end),
        } => format!("bytes={start}-{end}"),
        ByteRange::From { start, end: None } => format!("bytes={start}-"),
        ByteRange::Suffix(n) => format!("bytes=-{n}"),
    }
}

/// Relay a Plex response as a media stream without buffering it.
fn media_stream(resp: reqwest::Response) -> MediaStream {
    let h = resp.headers();
    let text =
        |name: header::HeaderName| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    MediaStream {
        status: resp.status().as_u16(),
        content_type: text(header::CONTENT_TYPE),
        content_length: resp.content_length(),
        content_range: text(header::CONTENT_RANGE),
        body: resp
            .bytes_stream()
            .map_err(|e| io::Error::other(e.without_url()))
            .boxed(),
    }
}

#[async_trait]
impl MediaSource for PlexBackend {
    async fn open(
        &self,
        ctx: &UserCtx<'_>,
        track: &TrackRemote,
        req: MediaRequest,
    ) -> Result<MediaStream> {
        if !track.part_key.starts_with('/') {
            return Err(BackendError::Protocol("invalid part key".into()));
        }
        let mut extra = HeaderMap::new();
        if let Some(r) = req.range {
            extra.insert(
                header::RANGE,
                HeaderValue::from_str(&range_header(r)).expect("ascii"),
            );
        }
        let resp = self
            .client
            .send(Method::GET, &track.part_key, ctx.remote_token, extra, None)
            .await?;
        Ok(media_stream(resp))
    }

    async fn cover_art(
        &self,
        ctx: &UserCtx<'_>,
        art: &ArtRef,
        size: Option<u32>,
    ) -> Result<MediaStream> {
        if !art.thumb.starts_with('/') {
            return Err(BackendError::Protocol("invalid thumb reference".into()));
        }
        let path = match size {
            Some(s) => {
                let url = enc(&art.thumb);
                format!("/photo/:/transcode?url={url}&width={s}&height={s}&minSize=1&upscale=1")
            }
            None => art.thumb.clone(),
        };
        let resp = self
            .client
            .send(Method::GET, &path, ctx.remote_token, HeaderMap::new(), None)
            .await?;
        Ok(media_stream(resp))
    }

    /// Every lyrics stream of the track: its sidecar or embedded lyrics, and
    /// those Plex fetched (LyricFind, when the library allows external lyrics).
    async fn lyrics(&self, ctx: &UserCtx<'_>, track: &TrackRemote) -> Result<Vec<LyricsDoc>> {
        let path = format!("/library/metadata/{}", plex_id(&track.key)?);
        let page: MetadataPage = self
            .client
            .get_json_as(&path, None, ctx.remote_token)
            .await?;
        let streams: Vec<String> = page
            .metadata
            .iter()
            .flat_map(|m| &m.media)
            .flat_map(|m| &m.part)
            .flat_map(|p| &p.stream)
            .filter(|s| s.stream_type == Some(STREAM_LYRICS))
            .filter_map(|s| s.key.clone())
            .filter(|k| k.starts_with("/library/streams/"))
            .collect();
        let mut docs = Vec::new();
        for key in streams {
            // Plex lists LyricFind streams it can no longer serve (404).
            let page: LyricsPage = match self.client.get_json_as(&key, None, ctx.remote_token).await
            {
                Err(BackendError::NotFound) => continue,
                other => other?,
            };
            docs.extend(page.lyrics.into_iter().map(map::lyrics));
        }
        Ok(docs)
    }
}

#[async_trait]
impl Discovery for PlexBackend {
    /// The artists in the library that Plex's metadata lists as similar, in
    /// Plex's order. Plex lists more, but only those in the library come back.
    async fn similar_artists(
        &self,
        ctx: &UserCtx<'_>,
        artist: &RemoteRef,
        n: usize,
    ) -> Result<Vec<String>> {
        let path = format!(
            "/library/metadata/{}/similar?count={n}",
            plex_id(&artist.key)?
        );
        let page: MetadataPage = self
            .client
            .get_json_as(&path, None, ctx.remote_token)
            .await?;
        Ok(page
            .metadata
            .into_iter()
            .filter(|m| m.kind == "artist")
            .map(|m| m.rating_key)
            .take(n)
            .collect())
    }
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Plex ids go into URL paths: accept only what Plex issues (digits).
fn plex_id(id: &str) -> Result<&str> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(BackendError::Protocol(format!("invalid Plex id {id:?}")))
    }
}

impl PlexBackend {
    /// `server://…` URI naming library items, for playlist requests.
    fn items_uri(machine: &str, keys: &[String]) -> Result<String> {
        let keys = keys
            .iter()
            .map(|k| plex_id(k))
            .collect::<Result<Vec<_>>>()?
            .join(",");
        Ok(format!(
            "server://{machine}/{LIBRARY_AGENT}/library/metadata/{keys}"
        ))
    }

    async fn add_items(&self, token: Option<&str>, id: &str, keys: &[String]) -> Result<()> {
        let id = plex_id(id)?;
        let machine = self.machine_id().await?;
        for chunk in keys.chunks(PLAYLIST_CHUNK) {
            let path = format!(
                "/playlists/{id}/items?uri={}",
                enc(&Self::items_uri(&machine, chunk)?)
            );
            self.client.call(Method::PUT, &path, token).await?;
        }
        Ok(())
    }

    /// Any track, to create a playlist with: Plex can't create an empty one.
    async fn any_track(&self) -> Result<String> {
        for lib in self.libraries().await? {
            let page = self.page(&lib.key, TYPE_TRACK, "", 0, 1).await?;
            if let Some(m) = page.metadata.into_iter().next() {
                return Ok(m.rating_key);
            }
        }
        Err(BackendError::Protocol(
            "no track to create a playlist with".into(),
        ))
    }

    /// Every item of a paged list, as `token`. Once the first page gives the
    /// total, the rest are fetched a few at a time.
    async fn all_pages(&self, path: &str, token: Option<&str>) -> Result<Vec<Metadata>> {
        let size = self.page_size;
        let get = |start: u64| async move {
            self.client
                .get_json_as::<MetadataPage>(path, Some((start, size)), token)
                .await
        };
        let first = get(0).await?;
        let mut out = first.metadata;
        let mut start = out.len() as u64;
        if start < size {
            return Ok(out);
        }
        if let Some(total) = first.total_size {
            let starts: Vec<u64> = (start..total).step_by(size as usize).collect();
            let pages: Vec<MetadataPage> = stream::iter(starts)
                .map(get)
                .buffered(PAGE_CONCURRENCY)
                .try_collect()
                .await?;
            out.extend(pages.into_iter().flat_map(|p| p.metadata));
            return Ok(out);
        }
        loop {
            let page = get(start).await?;
            let got = page.metadata.len() as u64;
            out.extend(page.metadata);
            start += got;
            if got < size {
                return Ok(out);
            }
        }
    }
}

fn plex_type(kind: Kind) -> Result<u8> {
    match kind {
        Kind::Artist => Ok(TYPE_ARTIST),
        Kind::Album => Ok(TYPE_ALBUM),
        Kind::Track => Ok(TYPE_TRACK),
        _ => Err(BackendError::Unsupported),
    }
}

/// Section filter and sort for items with state. Plex's `>>=` is "greater than".
fn ranking(order: StateOrder) -> (&'static str, &'static str) {
    match order {
        StateOrder::Frequent => ("viewCount>>=0", "viewCount:desc,lastViewedAt:desc"),
        StateOrder::Recent => ("lastViewedAt>>=0", "lastViewedAt:desc"),
        StateOrder::Starred => ("userRating=10", "lastRatedAt:desc"),
        StateOrder::Highest => ("userRating>>=0", "userRating:desc,viewCount:desc"),
    }
}

#[async_trait]
impl UserState for PlexBackend {
    async fn rate(&self, ctx: &UserCtx<'_>, key: &str, rating: u8) -> Result<()> {
        // Plex rates 0-10; -1 clears.
        let r = match rating {
            0 => -1,
            r => i32::from(r.min(5)) * 2,
        };
        let path = format!(
            "/:/rate?key={}&identifier={LIBRARY_AGENT}&rating={r}",
            plex_id(key)?
        );
        self.client.call(Method::PUT, &path, ctx.remote_token).await
    }

    async fn scrobble(&self, ctx: &UserCtx<'_>, key: &str) -> Result<()> {
        // Plex takes no timestamp: the play is recorded as now.
        let path = format!(
            "/:/scrobble?key={}&identifier={LIBRARY_AGENT}",
            plex_id(key)?
        );
        self.client.call(Method::GET, &path, ctx.remote_token).await
    }

    async fn now_playing(
        &self,
        ctx: &UserCtx<'_>,
        key: &str,
        state: PlayState,
        offset_ms: u64,
        duration_ms: u64,
    ) -> Result<()> {
        let key = plex_id(key)?;
        let state = match state {
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
            PlayState::Stopped => "stopped",
        };
        let path = format!(
            "/:/timeline?ratingKey={key}&key=%2Flibrary%2Fmetadata%2F{key}\
             &state={state}&time={offset_ms}&duration={duration_ms}"
        );
        self.client.call(Method::GET, &path, ctx.remote_token).await
    }

    /// Few items are fetched by key; for many, the library's items with any
    /// state are listed instead, which are usually far fewer.
    async fn states(
        &self,
        ctx: &UserCtx<'_>,
        lib: &str,
        kind: Kind,
        keys: &[String],
    ) -> Result<Vec<RemoteState>> {
        let token = ctx.remote_token;
        if keys.len() <= STATE_BY_KEY {
            let paths = keys
                .chunks(STATE_CHUNK)
                .map(|chunk| {
                    let ids = chunk
                        .iter()
                        .map(|k| plex_id(k))
                        .collect::<Result<Vec<_>>>()?;
                    Ok(format!("/library/metadata/{}", ids.join(",")))
                })
                .collect::<Result<Vec<_>>>()?;
            let pages: Vec<Option<MetadataPage>> = stream::iter(paths)
                .map(|path| async move {
                    match self.client.get_json_as(&path, None, token).await {
                        Err(BackendError::NotFound) => Ok(None),
                        other => other.map(Some),
                    }
                })
                .buffered(PAGE_CONCURRENCY)
                .try_collect()
                .await?;
            return Ok(pages
                .into_iter()
                .flatten()
                .flat_map(|p| p.metadata)
                .map(map::state)
                .collect());
        }
        let (lib, kind) = (plex_id(lib)?, plex_type(kind)?);
        let wanted: HashSet<&str> = keys.iter().map(String::as_str).collect();
        let list = |filter: &str| {
            let path = format!("/library/sections/{lib}/all?type={kind}&{filter}");
            async move { self.all_pages(&path, token).await }
        };
        let (rated, played) =
            futures_util::future::try_join(list("userRating>>=0"), list("viewCount>>=0")).await?;
        let mut seen = HashSet::new();
        Ok(rated
            .into_iter()
            .chain(played)
            .filter(|m| wanted.contains(m.rating_key.as_str()) && seen.insert(m.rating_key.clone()))
            .map(map::state)
            .collect())
    }

    async fn ranked(
        &self,
        ctx: &UserCtx<'_>,
        lib: &str,
        kind: Kind,
        order: StateOrder,
        start: u64,
        size: u64,
    ) -> Result<Vec<RemoteState>> {
        let (filter, sort) = ranking(order);
        let path = format!(
            "/library/sections/{}/all?type={}&{filter}&sort={sort}",
            plex_id(lib)?,
            plex_type(kind)?
        );
        let page: MetadataPage = self
            .client
            .get_json_as(&path, Some((start, size)), ctx.remote_token)
            .await?;
        Ok(page.metadata.into_iter().map(map::state).collect())
    }

    async fn playlists(&self, ctx: &UserCtx<'_>) -> Result<Vec<RemotePlaylist>> {
        let page: MetadataPage = self
            .client
            .get_json_as("/playlists?playlistType=audio", None, ctx.remote_token)
            .await?;
        Ok(page
            .metadata
            .into_iter()
            .filter(|m| plex_id(&m.rating_key).is_ok())
            .map(map::playlist)
            .collect())
    }

    async fn playlist_entries(&self, ctx: &UserCtx<'_>, id: &str) -> Result<Vec<PlaylistEntry>> {
        let path = format!("/playlists/{}/items", plex_id(id)?);
        let items = self.all_pages(&path, ctx.remote_token).await?;
        Ok(items.into_iter().map(map::playlist_entry).collect())
    }

    async fn create_playlist(
        &self,
        ctx: &UserCtx<'_>,
        name: &str,
        keys: &[String],
    ) -> Result<String> {
        let token = ctx.remote_token;
        let seed = match keys.first() {
            Some(k) => k.clone(),
            None => self.any_track().await?,
        };
        let machine = self.machine_id().await?;
        let path = format!(
            "/playlists?type=audio&smart=0&title={}&uri={}",
            enc(name),
            enc(&Self::items_uri(&machine, std::slice::from_ref(&seed))?)
        );
        let page: MetadataPage = self.client.json(Method::POST, &path, token).await?;
        let id = page
            .metadata
            .into_iter()
            .next()
            .map(|m| m.rating_key)
            .filter(|k| plex_id(k).is_ok())
            .ok_or_else(|| BackendError::Protocol("playlist create returned no id".into()))?;
        if keys.is_empty() {
            self.clear_playlist(ctx, &id).await?;
        } else {
            self.add_items(token, &id, &keys[1..]).await?;
        }
        Ok(id)
    }

    async fn edit_playlist(
        &self,
        ctx: &UserCtx<'_>,
        id: &str,
        name: Option<&str>,
        comment: Option<&str>,
    ) -> Result<()> {
        let mut query = Vec::new();
        if let Some(n) = name {
            query.push(format!("title={}", enc(n)));
        }
        if let Some(c) = comment {
            query.push(format!("summary={}", enc(c)));
        }
        if query.is_empty() {
            return Ok(());
        }
        let path = format!("/playlists/{}?{}", plex_id(id)?, query.join("&"));
        self.client.call(Method::PUT, &path, ctx.remote_token).await
    }

    async fn add_to_playlist(&self, ctx: &UserCtx<'_>, id: &str, keys: &[String]) -> Result<()> {
        self.add_items(ctx.remote_token, id, keys).await
    }

    async fn remove_from_playlist(
        &self,
        ctx: &UserCtx<'_>,
        id: &str,
        entries: &[String],
    ) -> Result<()> {
        let id = plex_id(id)?;
        for e in entries {
            let path = format!("/playlists/{id}/items/{}", plex_id(e)?);
            self.client
                .call(Method::DELETE, &path, ctx.remote_token)
                .await?;
        }
        Ok(())
    }

    async fn clear_playlist(&self, ctx: &UserCtx<'_>, id: &str) -> Result<()> {
        let path = format!("/playlists/{}/items", plex_id(id)?);
        self.client
            .call(Method::DELETE, &path, ctx.remote_token)
            .await
    }

    async fn delete_playlist(&self, ctx: &UserCtx<'_>, id: &str) -> Result<()> {
        let path = format!("/playlists/{}", plex_id(id)?);
        self.client
            .call(Method::DELETE, &path, ctx.remote_token)
            .await
    }
}
