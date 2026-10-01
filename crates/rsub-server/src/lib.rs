//! HTTP server exposing the OpenSubsonic API under `/rest/{method}`, and
//! rs-subsonic's own admin API under `/api/v1` (`admin`).

mod admin;
mod auth;
mod browse;
mod convert;
mod discover;
mod handlers;
mod ids;
mod limit;
mod media;
mod playlists;
#[cfg(feature = "sonic")]
mod sonic;
mod state;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
#[cfg(feature = "sonic")]
use rsub_analysis::{AnalysisEngine, SonicSearch};
use rsub_api::{ApiError, Envelope, ErrorCode, Format, Params, render};
use rsub_core::backend::BackendHandle;
use rsub_core::crypto::SecretBox;
use rsub_core::text::IgnoredArticles;
use rsub_media::{CoverCache, PathMapper};
use rsub_store::{Db, StoreError};
use rsub_sync::SyncEngine;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{Predicate, SizeAbove};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use handlers::Ctx;
use limit::{AuthLimiter, ClientIp};
pub use state::NowPlayingMap;

#[derive(Debug, Clone, Copy)]
pub struct AuthOptions {
    pub allow_plaintext: bool,
    pub allow_token_auth: bool,
}

impl Default for AuthOptions {
    fn default() -> Self {
        AuthOptions {
            allow_plaintext: true,
            allow_token_auth: true,
        }
    }
}

/// A configured backend at runtime.
pub struct SourceRuntime {
    pub backend: BackendHandle,
    pub paths: PathMapper,
    /// Serve mapped local files instead of proxying the backend.
    pub serve_local: bool,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub secrets: Arc<SecretBox>,
    pub auth: AuthOptions,
    /// Keyed by `sources.id`.
    pub sources: Arc<HashMap<i64, SourceRuntime>>,
    /// `None` when no backend is configured.
    pub sync: Option<Arc<SyncEngine>>,
    pub covers: Option<Arc<CoverCache>>,
    pub articles: Arc<IgnoredArticles>,
    pub now_playing: Arc<NowPlayingMap>,
    pub auth_limiter: Arc<AuthLimiter>,
    /// Local audio analysis for sonic similarity; `None` when it's off.
    #[cfg(feature = "sonic")]
    pub sonic: Option<SonicSearch>,
    /// The analysis engine, for the admin API; `None` when analysis is off.
    #[cfg(feature = "sonic")]
    pub analysis: Option<Arc<AnalysisEngine>>,
}

impl AppState {
    /// State without backends; fill in the rest as needed.
    pub fn new(db: Db, secrets: Arc<SecretBox>, auth: AuthOptions) -> Self {
        AppState {
            db,
            secrets,
            auth,
            sources: Arc::default(),
            sync: None,
            covers: None,
            articles: Arc::default(),
            now_playing: Arc::default(),
            auth_limiter: Arc::default(),
            #[cfg(feature = "sonic")]
            sonic: None,
            #[cfg(feature = "sonic")]
            analysis: None,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
        ])
        .allow_headers(Any)
        .expose_headers([
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
        ]);
    // Spans carry only method and path: query strings contain credentials.
    let trace = TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
        tracing::debug_span!("request", method = %req.method(), path = %req.uri().path())
    });

    Router::new()
        .route("/rest/{method}", any(rest))
        .nest("/api/v1", admin::router())
        .layer(CompressionLayer::new().compress_when(SizeAbove::new(1024).and(is_document)))
        .layer(cors)
        .layer(trace)
        .layer(CatchPanicLayer::new())
        .with_state(state)
}

/// Compress API responses (XML, JSON, JSONP), never media or cover art.
fn is_document(
    _: StatusCode,
    _: axum::http::Version,
    h: &HeaderMap,
    _: &axum::http::Extensions,
) -> bool {
    h.get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            [
                "application/xml",
                "application/json",
                "application/javascript",
            ]
            .iter()
            .any(|t| ct.starts_with(t))
        })
}

async fn rest(
    State(state): State<AppState>,
    Path(method): Path<String>,
    ClientIp(ip): ClientIp,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let form = is_form(&headers).then_some(&body[..]);
    let params = Params::parse(query.as_deref(), form);
    let format = match Format::from_params(&params) {
        Ok(f) => f,
        Err(e) => return respond(StatusCode::OK, &Format::Json, &Envelope::error(e)),
    };
    let name = method.strip_suffix(".view").unwrap_or(&method);
    match dispatch(&state, name, &params, &headers, ip).await {
        Some(Reply::Api(result)) => respond(StatusCode::OK, &format, &result.into()),
        Some(Reply::Raw(Ok(resp))) => resp,
        Some(Reply::Raw(Err(e))) => respond(StatusCode::OK, &format, &Envelope::error(e)),
        None => respond(
            StatusCode::NOT_FOUND,
            &format,
            &Envelope::error(ApiError::generic(format!("Unknown method '{name}'."))),
        ),
    }
}

// One per request and immediately consumed; boxing would only add an allocation.
#[allow(clippy::large_enum_variant)]
enum Reply {
    Api(Result<Envelope, ApiError>),
    /// Binary endpoints; errors still render as a Subsonic envelope.
    Raw(Result<Response, ApiError>),
}

/// Every method we answer. Anything else is a 404 with an error envelope.
pub const METHODS: &[&str] = &[
    "ping",
    "getLicense",
    "getOpenSubsonicExtensions",
    "getUser",
    "getUsers",
    "changePassword",
    "tokenInfo",
    "getMusicFolders",
    "getIndexes",
    "getArtists",
    "getArtist",
    "getAlbum",
    "getSong",
    "getMusicDirectory",
    "getAlbumList",
    "getAlbumList2",
    "getRandomSongs",
    "getSongsByGenre",
    "getGenres",
    "search2",
    "search3",
    "getArtistInfo",
    "getArtistInfo2",
    "getAlbumInfo",
    "getAlbumInfo2",
    "getScanStatus",
    "startScan",
    "stream",
    "download",
    "getCoverArt",
    "star",
    "unstar",
    "setRating",
    "scrobble",
    "getNowPlaying",
    "getStarred",
    "getStarred2",
    "getPlaylists",
    "getPlaylist",
    "createPlaylist",
    "updatePlaylist",
    "deletePlaylist",
    "getTopSongs",
    "getSimilarSongs",
    "getSimilarSongs2",
    "getLyrics",
    "getLyricsBySongId",
    #[cfg(feature = "sonic")]
    "getSonicSimilarTracks",
    #[cfg(feature = "sonic")]
    "findSonicPath",
    // Valid but empty until their milestone.
    "getPlayQueue",
    "getPlayQueueByIndex",
    "getBookmarks",
    "getInternetRadioStations",
    "getPodcasts",
    "getNewestPodcasts",
    "getShares",
    // Accepted and not kept until their milestone, so clients that save these
    // routinely don't show errors.
    "savePlayQueue",
    "savePlayQueueByIndex",
    "createBookmark",
    "deleteBookmark",
];

/// `None` for unknown methods.
async fn dispatch(
    state: &AppState,
    name: &str,
    params: &Params,
    headers: &HeaderMap,
    ip: Option<IpAddr>,
) -> Option<Reply> {
    if !METHODS.contains(&name) {
        return None;
    }
    // The only endpoint the spec allows without authentication.
    if name == "getOpenSubsonicExtensions" {
        return Some(Reply::Api(Ok(
            handlers::open_subsonic_extensions(state).await
        )));
    }
    let user = match login(state, params, ip).await {
        Ok(u) => u,
        Err(e) => return Some(Reply::Api(Err(e))),
    };
    let ctx = Ctx {
        state,
        user,
        params,
        headers,
    };
    let api = match name {
        "stream" => return Some(Reply::Raw(media::stream(&ctx, false).await)),
        "download" => return Some(Reply::Raw(media::stream(&ctx, true).await)),
        "getCoverArt" => return Some(Reply::Raw(media::cover_art(&ctx).await)),
        "ping" => Ok(Envelope::ok()),
        "getLicense" => Ok(handlers::license()),
        "getUser" => handlers::get_user(&ctx).await,
        "getUsers" => handlers::get_users(&ctx).await,
        "changePassword" => handlers::change_password(&ctx).await,
        "tokenInfo" => Ok(handlers::token_info(&ctx)),
        "getMusicFolders" => browse::music_folders(&ctx).await,
        "getIndexes" => browse::indexes(&ctx).await,
        "getArtists" => browse::artists(&ctx).await,
        "getArtist" => browse::artist(&ctx).await,
        "getAlbum" => browse::album(&ctx).await,
        "getSong" => browse::song(&ctx).await,
        "getMusicDirectory" => browse::music_directory(&ctx).await,
        "getAlbumList" => browse::album_list(&ctx).await,
        "getAlbumList2" => browse::album_list2(&ctx).await,
        "getRandomSongs" => browse::random_songs(&ctx).await,
        "getSongsByGenre" => browse::songs_by_genre(&ctx).await,
        "getGenres" => browse::genres(&ctx).await,
        "search2" => browse::search2(&ctx).await,
        "search3" => browse::search3(&ctx).await,
        "getArtistInfo" => browse::artist_info(&ctx, false).await,
        "getArtistInfo2" => browse::artist_info(&ctx, true).await,
        "getAlbumInfo" | "getAlbumInfo2" => browse::album_info(&ctx).await,
        "getScanStatus" => browse::scan_status(&ctx).await,
        "startScan" => browse::start_scan(&ctx).await,
        "star" => state::star(&ctx, true).await,
        "unstar" => state::star(&ctx, false).await,
        "setRating" => state::set_rating(&ctx).await,
        "scrobble" => state::scrobble(&ctx).await,
        "getNowPlaying" => state::now_playing(&ctx).await,
        "getStarred" => browse::starred(&ctx, false).await,
        "getStarred2" => browse::starred(&ctx, true).await,
        "getPlaylists" => playlists::get_playlists(&ctx).await,
        "getPlaylist" => playlists::get_playlist(&ctx).await,
        "createPlaylist" => playlists::create_playlist(&ctx).await,
        "updatePlaylist" => playlists::update_playlist(&ctx).await,
        "deletePlaylist" => playlists::delete_playlist(&ctx).await,
        "getTopSongs" => discover::top_songs(&ctx).await,
        "getSimilarSongs" => discover::similar_songs(&ctx, false).await,
        "getSimilarSongs2" => discover::similar_songs(&ctx, true).await,
        "getLyrics" => discover::lyrics(&ctx).await,
        "getLyricsBySongId" => discover::lyrics_by_song_id(&ctx).await,
        #[cfg(feature = "sonic")]
        "getSonicSimilarTracks" => sonic::similar_tracks(&ctx).await,
        #[cfg(feature = "sonic")]
        "findSonicPath" => sonic::path(&ctx).await,
        other => Ok(browse::empty(other)),
    };
    Some(Reply::Api(api))
}

/// Authenticate, counting wrong passwords and keys against the address and
/// username they came with.
async fn login(
    state: &AppState,
    params: &Params,
    ip: Option<IpAddr>,
) -> Result<rsub_core::User, ApiError> {
    let limiter = &state.auth_limiter;
    let username = params.get("u");
    if limiter.blocked(ip, username) {
        return Err(ApiError::with_message(
            ErrorCode::WrongCredentials,
            "Too many failed attempts; try again in a few minutes.",
        ));
    }
    let r = auth::authenticate(state, params).await;
    match &r {
        Ok(u) => limiter.succeeded(&u.username),
        Err(e)
            if matches!(
                e.code,
                ErrorCode::WrongCredentials | ErrorCode::InvalidApiKey
            ) =>
        {
            tracing::info!(ip = ?ip, user = username, "failed login");
            limiter.failed(ip, username);
        }
        Err(_) => {}
    }
    r
}

fn is_form(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"))
}

fn respond(status: StatusCode, format: &Format, env: &Envelope) -> Response {
    let body = render(format, env);
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(format.content_type()),
        )],
        body,
    )
        .into_response()
}

pub(crate) fn db_error(e: StoreError) -> ApiError {
    tracing::error!("database error: {e}");
    ApiError::generic("Database error.")
}
