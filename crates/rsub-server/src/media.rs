//! Binary endpoints: `stream`, `download` and `getCoverArt`.
//!
//! Tracks are served from a mapped local file when one exists and matches the
//! catalog size; otherwise the backend stream is relayed without buffering.
//! Transcoding arrives in M4, so `stream` always sends the original file.

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rsub_api::ApiError;
use rsub_core::backend::{ArtRef, MediaRequest, TrackRemote};
use rsub_core::{CoverArtId, Kind, Roles};
use rsub_media::parse_range;
use rsub_store::TrackRow;

use crate::handlers::{Ctx, backend_error};
use crate::ids::track;
use crate::{SourceRuntime, db_error};

/// Covers larger than this are relayed but not cached.
const MAX_COVER: usize = 8 << 20;

/// What the backend needs to open or describe a track.
pub(crate) fn track_remote(t: &TrackRow) -> TrackRemote {
    TrackRemote {
        key: t.remote_key.clone(),
        part_key: t.part_key.clone(),
        remote_path: t.remote_path.clone(),
        suffix: t.suffix.clone(),
        duration_ms: t.duration_ms.max(0) as u64,
    }
}

pub async fn stream(ctx: &Ctx<'_>, download: bool) -> Result<Response, ApiError> {
    let role = if download {
        Roles::DOWNLOAD
    } else {
        Roles::STREAM
    };
    if !ctx.user.roles.contains(role) {
        return Err(ApiError::not_authorized());
    }
    let t = track(ctx, "id").await?;
    let rt = ctx.library_runtime(t.library_id).await?;
    let range = ctx
        .headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range);
    let fallback_type = t
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".into());

    let mut resp = match local_file(rt, &t).await {
        Some(path) => {
            let f = rsub_media::open_local(&path, range).await.map_err(|e| {
                tracing::warn!(path = %path.display(), "local file unreadable: {e}");
                ApiError::generic("Cannot read media file.")
            })?;
            let mut r = Response::new(Body::from_stream(f.body));
            *r.status_mut() = StatusCode::from_u16(f.status).unwrap_or(StatusCode::OK);
            let h = r.headers_mut();
            h.insert(header::CONTENT_LENGTH, f.content_length.into());
            if let Some(cr) = f.content_range {
                h.insert(header::CONTENT_RANGE, hv(&cr));
            }
            h.insert(header::ETAG, hv(&f.etag));
            if let Some(lm) = &f.last_modified {
                h.insert(header::LAST_MODIFIED, hv(lm));
            }
            h.insert(header::CONTENT_TYPE, hv(&fallback_type));
            r
        }
        None => {
            let remote = track_remote(&t);
            let req = MediaRequest { range };
            let m = rt
                .backend
                .media
                .open(&ctx.remote(), &remote, req)
                .await
                .map_err(backend_error)?;
            let mut r = Response::new(Body::from_stream(m.body));
            *r.status_mut() = StatusCode::from_u16(m.status).unwrap_or(StatusCode::OK);
            let h = r.headers_mut();
            if let Some(n) = m.content_length {
                h.insert(header::CONTENT_LENGTH, n.into());
            }
            if let Some(cr) = m.content_range {
                h.insert(header::CONTENT_RANGE, hv(&cr));
            }
            // Prefer our type: Plex may send a generic one for some containers.
            h.insert(header::CONTENT_TYPE, hv(&fallback_type));
            r
        }
    };
    let h = resp.headers_mut();
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if download {
        let name = format!(
            "{}{}",
            t.title,
            t.suffix
                .as_deref()
                .map(|s| format!(".{s}"))
                .unwrap_or_default()
        );
        h.insert(header::CONTENT_DISPOSITION, hv(&content_disposition(&name)));
    }
    Ok(resp)
}

/// The mapped local path, if local serving is enabled and the file matches.
async fn local_file(rt: &SourceRuntime, t: &TrackRow) -> Option<std::path::PathBuf> {
    if !rt.serve_local {
        return None;
    }
    let path = rt.paths.map(t.remote_path.as_deref()?)?;
    let expected = t.size.and_then(|s| u64::try_from(s).ok());
    match rsub_media::paths::verify(&path, expected).await {
        Some(_) => Some(path),
        None => {
            tracing::debug!(path = %path.display(), "local file missing or changed; using backend");
            None
        }
    }
}

fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static("invalid"))
}

/// `attachment` with an ASCII fallback and the UTF-8 name per RFC 6266/5987.
fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = form_urlencoded::byte_serialize(name.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

pub async fn cover_art(ctx: &Ctx<'_>) -> Result<Response, ApiError> {
    let not_found = || ApiError::not_found("Cover art");
    let id: CoverArtId = ctx
        .params
        .required("id")?
        .parse()
        .map_err(|_| not_found())?;
    let size: Option<u32> = ctx.params.parse_opt("size")?;
    let size = size.filter(|s| *s > 0).map(|s| s.min(2048));
    // Cache keys are scoped by library, or by source for playlists.
    let (rt, art, scope) = if id.entity.kind() == Kind::Playlist {
        let (source, key, thumb) = crate::playlists::cover(ctx, id.entity).await?;
        (
            ctx.runtime(source)?,
            ArtRef { key, thumb },
            format!("s{source}"),
        )
    } else {
        let item = ctx
            .state
            .db
            .internal_id(id.entity)
            .await
            .map_err(db_error)?
            .ok_or_else(not_found)?;
        let src = ctx
            .state
            .db
            .art_source(id.entity.kind(), item)
            .await
            .map_err(db_error)?
            .ok_or_else(not_found)?;
        let thumb = src.thumb_ref.ok_or_else(not_found)?;
        let art = ArtRef {
            key: src.remote_key,
            thumb,
        };
        (
            ctx.library_runtime(src.library_id).await?,
            art,
            src.library_id.to_string(),
        )
    };

    let key = format!("{scope}:{}:{}", art.thumb, size.unwrap_or(0));
    if let Some(cache) = &ctx.state.covers
        && let Some((data, ct)) = cache.get(&key).await
    {
        return Ok(image(Body::from(data), ct));
    }
    let m = rt
        .backend
        .media
        .cover_art(&ctx.remote(), &art, size)
        .await
        .map_err(backend_error)?;
    let content_type = |data: &[u8], sent: Option<String>| match rsub_media::cover::sniff(data) {
        "application/octet-stream" => sent.unwrap_or_else(|| "image/jpeg".into()),
        known => known.to_owned(),
    };
    let mut data = Vec::with_capacity(m.content_length.unwrap_or(0).min(MAX_COVER as u64) as usize);
    let mut body = m.body;
    while let Some(chunk) = body.next().await {
        let chunk =
            chunk.map_err(|e| ApiError::generic(format!("Reading cover art failed: {e}")))?;
        data.extend_from_slice(&chunk);
        if data.len() > MAX_COVER {
            // Relay the rest without caching it.
            let ct = content_type(&data, m.content_type);
            let head = futures_util::stream::once(async move { Ok(Bytes::from(data)) });
            return Ok(image(Body::from_stream(head.chain(body)), &ct));
        }
    }
    if let Some(cache) = &ctx.state.covers {
        cache.put(&key, &data).await;
    }
    let ct = content_type(&data, m.content_type);
    Ok(image(Body::from(data), &ct))
}

fn image(body: Body, content_type: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, hv(content_type)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition() {
        assert_eq!(
            content_disposition("Jóga \"live\".flac"),
            "attachment; filename=\"J_ga _live_.flac\"; filename*=UTF-8''J%C3%B3ga%20%22live%22.flac"
        );
    }
}
