//! Playlist endpoints. Playlists are the Plex account's, read and written live;
//! rs-subsonic stores none. A playlist's public id is minted from its backend
//! id, so it is the same on every read and survives a rebuilt database.
//!
//! Until account linking (M5) every user shares the one account, so every
//! user sees every playlist and is reported as its owner. Plex has no public
//! flag, its smart playlists are read-only, and a Plex playlist holds each
//! track at most once.

use std::collections::HashMap;

use rsub_api::model::{Playlist, Playlists};
use rsub_api::{ApiError, Envelope};
use rsub_core::backend::{PlaylistEntry, RemotePlaylist};
use rsub_core::identity::playlist_key;
use rsub_core::text::iso8601;
use rsub_core::{Kind, PublicId, Roles};
use rsub_store::TrackRow;

use crate::handlers::{Ctx, backend_error};
use crate::ids::lookup_all;
use crate::{convert, db_error};

/// A backend playlist and the source it lives in.
struct Found {
    source: i64,
    public_id: String,
    p: RemotePlaylist,
}

fn public_id(ctx: &Ctx<'_>, source: i64, id: &str) -> Result<String, ApiError> {
    let name = &ctx.runtime(source)?.backend.catalog.source_id().0;
    Ok(PublicId::mint(Kind::Playlist, &playlist_key(name, id)).to_string())
}

/// Every source's playlists, by source then name.
async fn all(ctx: &Ctx<'_>) -> Result<Vec<Found>, ApiError> {
    let mut sources: Vec<i64> = ctx.state.sources.keys().copied().collect();
    sources.sort_unstable();
    let mut out = Vec::new();
    for source in sources {
        let mut list = ctx
            .runtime(source)?
            .backend
            .state
            .playlists(&ctx.remote())
            .await
            .map_err(backend_error)?;
        list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        for p in list {
            out.push(Found {
                source,
                public_id: public_id(ctx, source, &p.id)?,
                p,
            });
        }
    }
    Ok(out)
}

/// The playlist a public id names.
async fn find(ctx: &Ctx<'_>, raw: &str) -> Result<Found, ApiError> {
    let not_found = || ApiError::not_found("Playlist");
    let id: PublicId = raw.parse().map_err(|_| not_found())?;
    if id.kind() != Kind::Playlist {
        return Err(not_found());
    }
    let wanted = id.to_string();
    all(ctx)
        .await?
        .into_iter()
        .find(|f| f.public_id == wanted)
        .ok_or_else(not_found)
}

/// A playlist the user may change.
async fn editable(ctx: &Ctx<'_>, raw: &str) -> Result<Found, ApiError> {
    if !ctx.user.roles.contains(Roles::PLAYLIST) {
        return Err(ApiError::not_authorized());
    }
    let f = find(ctx, raw).await?;
    if f.p.smart {
        return Err(ApiError::not_authorized());
    }
    Ok(f)
}

fn to_api(ctx: &Ctx<'_>, f: &Found) -> Playlist {
    let p = &f.p;
    Playlist {
        id: f.public_id.clone(),
        name: p.name.clone(),
        comment: p.comment.clone(),
        owner: ctx.user.username.clone(),
        public: false,
        song_count: p.song_count as i64,
        duration: convert::secs(p.duration_ms as i64),
        created: iso8601(p.created_at),
        changed: iso8601(p.updated_at),
        cover_art: convert::cover(&f.public_id, p.thumb.as_deref()),
        readonly: p.smart,
        entry: None,
    }
}

/// The playlist's entries that are live tracks in the index, in order: what
/// clients see, and what `songIndexToRemove` counts.
async fn visible(ctx: &Ctx<'_>, f: &Found) -> Result<Vec<(PlaylistEntry, TrackRow)>, ApiError> {
    let entries = ctx
        .runtime(f.source)?
        .backend
        .state
        .playlist_entries(&ctx.remote(), &f.p.id)
        .await
        .map_err(backend_error)?;
    let mut keys: Vec<String> = entries.iter().map(|e| e.key.clone()).collect();
    keys.sort_unstable();
    keys.dedup();
    let rows: HashMap<String, TrackRow> = ctx
        .state
        .db
        .tracks_by_remote(f.source, &keys)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|t| (t.remote_key.clone(), t))
        .collect();
    Ok(entries
        .into_iter()
        .filter_map(|e| {
            let t = rows.get(&e.key)?.clone();
            Some((e, t))
        })
        .collect())
}

async fn with_entries(ctx: &Ctx<'_>, f: &Found) -> Result<Envelope, ApiError> {
    let tracks: Vec<TrackRow> = visible(ctx, f).await?.into_iter().map(|(_, t)| t).collect();
    let mut out = to_api(ctx, f);
    out.song_count = tracks.len() as i64;
    out.duration = convert::secs(tracks.iter().map(|t| t.duration_ms).sum());
    out.entry = Some(convert::songs(ctx.view(), &tracks).await?);
    Ok(Envelope::with(out))
}

/// Backend keys of the songs in parameter `param`, in order, and their source.
/// Every song must exist, and all must share a source.
async fn songs(ctx: &Ctx<'_>, param: &str) -> Result<(Option<i64>, Vec<String>), ApiError> {
    let ids = lookup_all(ctx, ctx.params.get_all(param), Kind::Track)
        .await?
        .ok_or_else(|| ApiError::not_found("Song"))?;
    let found = ctx
        .state
        .db
        .remote_items(Kind::Track, &ids)
        .await
        .map_err(db_error)?;
    let by_id: HashMap<i64, _> = found.into_iter().map(|r| (r.id, r)).collect();
    let mut source = None;
    let mut keys = Vec::with_capacity(ids.len());
    for id in ids {
        let r = by_id.get(&id).ok_or_else(|| ApiError::not_found("Song"))?;
        if source.is_some_and(|s| s != r.source_id) {
            return Err(ApiError::generic(
                "A playlist can only hold songs from one backend.",
            ));
        }
        source = Some(r.source_id);
        keys.extend(r.remote_key.clone());
    }
    Ok((source, keys))
}

pub async fn get_playlists(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let other = ctx
        .params
        .get("username")
        .is_some_and(|u| !u.is_empty() && u != ctx.user.username);
    if other && !ctx.user.is_admin() {
        return Err(ApiError::not_authorized());
    }
    let found = all(ctx).await?;
    Ok(Envelope::with(Playlists {
        playlist: found.iter().map(|f| to_api(ctx, f)).collect(),
    }))
}

pub async fn get_playlist(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let f = find(ctx, ctx.params.required("id")?).await?;
    with_entries(ctx, &f).await
}

/// Create a playlist, or replace the songs of `playlistId`.
pub async fn create_playlist(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    if !ctx.user.roles.contains(Roles::PLAYLIST) {
        return Err(ApiError::not_authorized());
    }
    let (source, keys) = songs(ctx, "songId").await?;
    let name = ctx.params.get("name").filter(|n| !n.is_empty());
    let cx = ctx.remote();
    let f = match ctx.params.get("playlistId").filter(|p| !p.is_empty()) {
        Some(raw) => {
            let f = editable(ctx, raw).await?;
            if source.is_some_and(|s| s != f.source) {
                return Err(ApiError::generic(
                    "A playlist can only hold songs from one backend.",
                ));
            }
            let state = &ctx.runtime(f.source)?.backend.state;
            state
                .edit_playlist(&cx, &f.p.id, name, None)
                .await
                .map_err(backend_error)?;
            // Plex can't replace a playlist's items in one call. The old
            // entries are kept, so a failure after the clear can put them back:
            // rs-subsonic keeps no copy of the playlist.
            let old: Vec<String> = state
                .playlist_entries(&cx, &f.p.id)
                .await
                .map_err(backend_error)?
                .into_iter()
                .map(|e| e.key)
                .collect();
            state
                .clear_playlist(&cx, &f.p.id)
                .await
                .map_err(backend_error)?;
            if let Err(e) = state.add_to_playlist(&cx, &f.p.id, &keys).await {
                let restore = async {
                    state.clear_playlist(&cx, &f.p.id).await?;
                    state.add_to_playlist(&cx, &f.p.id, &old).await
                };
                match restore.await {
                    Ok(()) => tracing::warn!(
                        playlist = %f.p.id,
                        "replacing a playlist's songs failed; its old songs were put back"
                    ),
                    Err(r) => tracing::error!(
                        playlist = %f.p.id,
                        songs = old.len(),
                        "replacing a playlist's songs failed, and so did putting its old songs back: {r}"
                    ),
                }
                return Err(backend_error(e));
            }
            f.public_id
        }
        None => {
            let name = name.ok_or_else(|| ApiError::missing("name"))?;
            let source = match source {
                Some(s) => s,
                None => *ctx
                    .state
                    .sources
                    .keys()
                    .min()
                    .ok_or_else(|| ApiError::generic("No backend is configured."))?,
            };
            let id = ctx
                .runtime(source)?
                .backend
                .state
                .create_playlist(&cx, name, &keys)
                .await
                .map_err(backend_error)?;
            public_id(ctx, source, &id)?
        }
    };
    let f = find(ctx, &f).await?;
    with_entries(ctx, &f).await
}

pub async fn update_playlist(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let p = ctx.params;
    let f = editable(ctx, p.required("playlistId")?).await?;
    let (source, append) = songs(ctx, "songIdToAdd").await?;
    if source.is_some_and(|s| s != f.source) {
        return Err(ApiError::generic(
            "A playlist can only hold songs from one backend.",
        ));
    }
    let remove = p
        .get_all("songIndexToRemove")
        .map(|i| {
            i.parse::<usize>()
                .map_err(|_| ApiError::generic("Invalid value for parameter 'songIndexToRemove'."))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cx = ctx.remote();
    let state = &ctx.runtime(f.source)?.backend.state;
    let name = p.get("name").filter(|n| !n.is_empty());
    state
        .edit_playlist(&cx, &f.p.id, name, p.get("comment"))
        .await
        .map_err(backend_error)?;
    if !remove.is_empty() {
        let shown = visible(ctx, &f).await?;
        let entries: Vec<String> = remove
            .iter()
            .filter_map(|i| shown.get(*i))
            .map(|(e, _)| e.id.clone())
            .collect();
        state
            .remove_from_playlist(&cx, &f.p.id, &entries)
            .await
            .map_err(backend_error)?;
    }
    if !append.is_empty() {
        state
            .add_to_playlist(&cx, &f.p.id, &append)
            .await
            .map_err(backend_error)?;
    }
    Ok(Envelope::ok())
}

pub async fn delete_playlist(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let f = editable(ctx, ctx.params.required("id")?).await?;
    ctx.runtime(f.source)?
        .backend
        .state
        .delete_playlist(&ctx.remote(), &f.p.id)
        .await
        .map_err(backend_error)?;
    Ok(Envelope::ok())
}

/// Where a playlist's cover comes from: `(source, backend id, thumb)`.
pub async fn cover(ctx: &Ctx<'_>, id: PublicId) -> Result<(i64, String, String), ApiError> {
    let f = find(ctx, &id.to_string()).await?;
    let thumb = f.p.thumb.ok_or_else(|| ApiError::not_found("Cover art"))?;
    Ok((f.source, f.p.id, thumb))
}
