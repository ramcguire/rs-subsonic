//! User-state endpoints: star/unstar, setRating, scrobble and getNowPlaying,
//! plus the backend queries behind lists ordered by user state.
//!
//! Ratings and plays belong to the Plex account and are written live: a failed
//! backend call fails the request, and nothing is queued. A star is a 5-star
//! rating. Virtual artists have no backend counterpart, so their ratings are
//! kept locally.
//!
//! Until users link their own Plex account (M5), every user writes to the
//! configured one: backend ratings need the `rating` role and plays the
//! `scrobbling` role. Local ratings are each user's own and need neither.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Mutex;

use rsub_api::model::NowPlaying;
use rsub_api::{ApiError, Envelope};
use rsub_core::backend::{PlayState, RemoteState, StateOrder};
use rsub_core::{Kind, Roles, now_ms};
use rsub_store::{AlbumRow, ArtistRow, Db, RemoteItem, StoreError, TrackRow};

use crate::browse::item;
use crate::handlers::{Ctx, backend_error, remote};
use crate::ids::lookup;
use crate::{convert, db_error};

/// Entries older than the track's length plus this are dropped.
const NOW_PLAYING_GRACE_MS: i64 = 5 * 60_000;

/// What each user's players are playing, from `scrobble?submission=false`.
#[derive(Default)]
pub struct NowPlayingMap(Mutex<HashMap<(i64, String), Playing>>);

#[derive(Clone)]
struct Playing {
    track_id: i64,
    username: String,
    started_at: i64,
    duration_ms: i64,
}

impl NowPlayingMap {
    fn set(&self, user: i64, username: &str, player: &str, t: &TrackRow) {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.insert(
            (user, player.to_owned()),
            Playing {
                track_id: t.id,
                username: username.to_owned(),
                started_at: now_ms(),
                duration_ms: t.duration_ms,
            },
        );
    }

    /// Current entries, newest first; expired ones are dropped.
    fn current(&self) -> Vec<(String, Playing)> {
        let now = now_ms();
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.retain(|_, p| p.started_at + p.duration_ms + NOW_PLAYING_GRACE_MS >= now);
        let mut v: Vec<_> = m
            .iter()
            .map(|((_, player), p)| (player.clone(), p.clone()))
            .collect();
        v.sort_by_key(|(_, p)| Reverse(p.started_at));
        v
    }
}

/// The `id`, `albumId` and `artistId` parameters, resolved to where each item
/// lives. Every item must exist.
async fn items(ctx: &Ctx<'_>) -> Result<Vec<(Kind, RemoteItem)>, ApiError> {
    let p = ctx.params;
    let mut by_kind: HashMap<Kind, Vec<i64>> = HashMap::new();
    for raw in p.get_all("id") {
        let (k, id) = item(ctx, raw).await?;
        by_kind.entry(k).or_default().push(id);
    }
    for (param, want) in [("albumId", Kind::Album), ("artistId", Kind::Artist)] {
        for raw in p.get_all(param) {
            match item(ctx, raw).await? {
                (k, id) if k == want => by_kind.entry(k).or_default().push(id),
                _ => return Err(ApiError::not_found("Item")),
            }
        }
    }
    let mut out = Vec::new();
    for (kind, mut ids) in by_kind {
        ids.sort_unstable();
        ids.dedup();
        let found = ctx
            .state
            .db
            .remote_items(kind, &ids)
            .await
            .map_err(db_error)?;
        if found.len() != ids.len() {
            return Err(ApiError::not_found("Item"));
        }
        out.extend(found.into_iter().map(|r| (kind, r)));
    }
    Ok(out)
}

/// The item's current rating: from its backend, or locally for a virtual artist.
async fn rating_of(ctx: &Ctx<'_>, kind: Kind, it: &RemoteItem) -> Result<Option<u8>, ApiError> {
    let db = &ctx.state.db;
    let Some(key) = &it.remote_key else {
        let local = db
            .local_ratings(ctx.user.id, &[it.id])
            .await
            .map_err(db_error)?;
        return Ok(local.first().map(|r| r.rating.clamp(0, 5) as u8));
    };
    let lib = db
        .library(it.library_id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("Item"))?;
    let found = ctx
        .runtime(it.source_id)?
        .backend
        .state
        .states(
            &ctx.remote(),
            &lib.remote_key,
            kind,
            std::slice::from_ref(key),
        )
        .await
        .map_err(backend_error)?;
    Ok(found
        .into_iter()
        .find(|s| &s.key == key)
        .and_then(|s| s.rating))
}

/// Refuse before writing anything if any of the items is rated in the
/// backend and the user may not rate there.
fn may_rate<'a>(
    ctx: &Ctx<'_>,
    mut items: impl Iterator<Item = &'a RemoteItem>,
) -> Result<(), ApiError> {
    if !ctx.user.roles.contains(Roles::RATING) && items.any(|it| it.remote_key.is_some()) {
        return Err(ApiError::not_authorized());
    }
    Ok(())
}

/// Rate an item 0–5 (0 clears) where its state lives.
async fn rate(ctx: &Ctx<'_>, it: &RemoteItem, rating: u8) -> Result<(), ApiError> {
    match &it.remote_key {
        Some(key) => ctx
            .runtime(it.source_id)?
            .backend
            .state
            .rate(&ctx.remote(), key, rating)
            .await
            .map_err(backend_error),
        None => ctx
            .state
            .db
            .rate_locally(ctx.user.id, it.id, rating)
            .await
            .map_err(db_error),
    }
}

/// Starring rates 5. Unstarring clears only a 5-star rating, so it never
/// erases a lower rating the user gave.
pub async fn star(ctx: &Ctx<'_>, starred: bool) -> Result<Envelope, ApiError> {
    let targets = items(ctx).await?;
    if targets.is_empty() {
        return Err(ApiError::missing("id"));
    }
    may_rate(ctx, targets.iter().map(|(_, it)| it))?;
    for (kind, it) in &targets {
        if starred {
            rate(ctx, it, 5).await?;
        } else if rating_of(ctx, *kind, it).await? == Some(5) {
            rate(ctx, it, 0).await?;
        }
    }
    Ok(Envelope::ok())
}

pub async fn set_rating(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let (kind, id) = item(ctx, ctx.params.required("id")?).await?;
    let rating: u8 = ctx.params.parse_required("rating")?;
    if rating > 5 {
        return Err(ApiError::generic("Rating must be between 0 and 5."));
    }
    let it = ctx
        .state
        .db
        .remote_items(kind, &[id])
        .await
        .map_err(db_error)?
        .pop()
        .ok_or_else(|| ApiError::not_found("Item"))?;
    may_rate(ctx, std::iter::once(&it))?;
    rate(ctx, &it, rating).await?;
    Ok(Envelope::ok())
}

/// Completed plays go to the backend, which dates them now: Plex takes no
/// timestamp, so `time` is accepted and ignored. Now-playing updates are
/// kept in memory for `getNowPlaying` and passed on without waiting. Without
/// the scrobbling role nothing reaches the backend, and the call still
/// succeeds so clients don't report an error for every song.
pub async fn scrobble(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let p = ctx.params;
    let submission: bool = p.parse_or("submission", true)?;
    for raw in p.get_all("time") {
        raw.parse::<i64>()
            .map_err(|_| ApiError::generic("Invalid value for parameter 'time'."))?;
    }
    let player = p.get("c").unwrap_or("unknown").to_owned();
    let mut tracks = Vec::new();
    for raw in p.get_all("id") {
        let t = match lookup(ctx, raw, Kind::Track).await? {
            Some(id) => ctx.state.db.track(id).await.map_err(db_error)?,
            None => None,
        };
        tracks.push(t.ok_or_else(|| ApiError::not_found("Song"))?);
    }
    if tracks.is_empty() {
        return Err(ApiError::missing("id"));
    }
    let db = &ctx.state.db;
    let scrobbling = ctx.user.roles.contains(Roles::SCROBBLING);
    for t in &tracks {
        if !submission {
            ctx.state
                .now_playing
                .set(ctx.user.id, &ctx.user.username, &player, t);
        }
        if !scrobbling {
            continue;
        }
        let lib = db
            .library(t.library_id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| ApiError::not_found("Song"))?;
        let rt = ctx.runtime(lib.source_id)?;
        if submission {
            rt.backend
                .state
                .scrobble(&ctx.remote(), &t.remote_key)
                .await
                .map_err(backend_error)?;
            continue;
        }
        let state = rt.backend.state.clone();
        let (user, key, duration) = (ctx.user.id, t.remote_key.clone(), t.duration_ms);
        tokio::spawn(async move {
            let duration = duration.max(0) as u64;
            if let Err(e) = state
                .now_playing(&remote(user), &key, PlayState::Playing, 0, duration)
                .await
            {
                tracing::debug!("now-playing update failed: {e}");
            }
        });
    }
    Ok(Envelope::ok())
}

pub async fn now_playing(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let now = now_ms();
    let playing = ctx.state.now_playing.current();
    let ids: Vec<i64> = playing.iter().map(|(_, p)| p.track_id).collect();
    let rows: HashMap<i64, TrackRow> = ctx
        .state
        .db
        .tracks_by_ids(&ids)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|t| (t.id, t))
        .collect();
    // A track can play on several players.
    let (tracks, playing): (Vec<TrackRow>, Vec<_>) = playing
        .into_iter()
        .filter_map(|(player, p)| {
            let t = rows.get(&p.track_id)?.clone();
            Some((t, (player, p)))
        })
        .unzip();
    let songs = convert::songs(ctx.view(), &tracks).await?;
    let entry = songs
        .into_iter()
        .zip(playing)
        .enumerate()
        .map(|(n, (mut song, (player, p)))| {
            song.username = Some(p.username);
            song.minutes_ago = Some((now - p.started_at).max(0) / 60_000);
            song.player_id = Some(n as i64 + 1);
            song.player_name = Some(player);
            song
        })
        .collect();
    Ok(Envelope::with(NowPlaying { entry }))
}

/// Sort key for merging ranked pages from several libraries.
fn rank(order: StateOrder, s: &RemoteState) -> Reverse<(i64, i64)> {
    let n = |v: Option<u32>| v.map_or(0, i64::from);
    Reverse(match order {
        StateOrder::Frequent => (n(s.play_count), s.last_played_at.unwrap_or(0)),
        StateOrder::Recent => (s.last_played_at.unwrap_or(0), 0),
        StateOrder::Starred => (s.rated_at.unwrap_or(0), 0),
        StateOrder::Highest => (s.rating.map_or(0, i64::from), n(s.play_count)),
    })
}

/// A page of items of `kind` with user state, in `order`, from every library
/// (or only `library`): `(source, state)`. These are backend queries, so an
/// unreachable backend fails the request.
pub async fn ranked(
    ctx: &Ctx<'_>,
    kind: Kind,
    order: StateOrder,
    library: Option<i64>,
    offset: u64,
    size: u64,
) -> Result<Vec<(i64, RemoteState)>, ApiError> {
    let libs: Vec<_> = ctx
        .state
        .db
        .libraries()
        .await
        .map_err(db_error)?
        .into_iter()
        .filter(|l| library.is_none_or(|id| l.id == id))
        .collect();
    // One library pages in the backend; several are merged here.
    let (start, want) = match libs.len() {
        1 => (offset, size),
        _ => (0, offset.saturating_add(size)),
    };
    let mut all = Vec::new();
    for lib in &libs {
        let Some(rt) = ctx.state.sources.get(&lib.source_id) else {
            continue;
        };
        let page = rt
            .backend
            .state
            .ranked(&ctx.remote(), &lib.remote_key, kind, order, start, want)
            .await
            .map_err(backend_error)?;
        all.extend(page.into_iter().map(|s| (lib.source_id, s)));
    }
    if libs.len() == 1 {
        return Ok(all);
    }
    all.sort_by_key(|(_, s)| rank(order, s));
    Ok(all
        .into_iter()
        .skip(offset as usize)
        .take(size as usize)
        .collect())
}

/// Index rows that backend keys map to.
pub trait Indexed: Sized + Send {
    fn fetch(
        db: &Db,
        source: i64,
        keys: &[String],
    ) -> impl Future<Output = Result<Vec<Self>, StoreError>> + Send;
    fn remote_key(&self) -> Option<&str>;
}

impl Indexed for ArtistRow {
    fn fetch(
        db: &Db,
        source: i64,
        keys: &[String],
    ) -> impl Future<Output = Result<Vec<Self>, StoreError>> + Send {
        db.artists_by_remote(source, keys)
    }
    fn remote_key(&self) -> Option<&str> {
        self.remote_key.as_deref()
    }
}

impl Indexed for AlbumRow {
    fn fetch(
        db: &Db,
        source: i64,
        keys: &[String],
    ) -> impl Future<Output = Result<Vec<Self>, StoreError>> + Send {
        db.albums_by_remote(source, keys)
    }
    fn remote_key(&self) -> Option<&str> {
        self.remote_key.as_deref()
    }
}

impl Indexed for TrackRow {
    fn fetch(
        db: &Db,
        source: i64,
        keys: &[String],
    ) -> impl Future<Output = Result<Vec<Self>, StoreError>> + Send {
        db.tracks_by_remote(source, keys)
    }
    fn remote_key(&self) -> Option<&str> {
        Some(&self.remote_key)
    }
}

/// Index rows for ranked items, in rank order; items not in the index yet
/// are skipped.
pub async fn rows_in_order<T: Indexed>(
    db: &Db,
    ranked: &[(i64, RemoteState)],
) -> Result<Vec<T>, ApiError> {
    let mut by_source: HashMap<i64, Vec<String>> = HashMap::new();
    for (source, s) in ranked {
        by_source.entry(*source).or_default().push(s.key.clone());
    }
    let mut found: HashMap<(i64, String), T> = HashMap::new();
    for (source, keys) in by_source {
        for row in T::fetch(db, source, &keys).await.map_err(db_error)? {
            if let Some(k) = row.remote_key().map(str::to_owned) {
                found.insert((source, k), row);
            }
        }
    }
    Ok(ranked
        .iter()
        .filter_map(|(source, s)| found.remove(&(*source, s.key.clone())))
        .collect())
}
