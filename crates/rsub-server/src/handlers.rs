//! Endpoint handlers. Each takes the request context and returns an envelope;
//! errors become `status="failed"` responses.

use rsub_api::params::decode_password;
use rsub_api::response::{License, OpenSubsonicExtension, TokenInfo, User as ApiUser, Users};
use rsub_api::{ApiError, Envelope, Params};
use rsub_core::crypto::Purpose;
use rsub_core::{Roles, User};
use rsub_store::UserUpdate;

use rsub_core::backend::{BackendError, UserCtx};

use crate::{AppState, SourceRuntime, db_error};

pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub user: User,
    pub params: &'a Params,
    pub headers: &'a axum::http::HeaderMap,
}

impl Ctx<'_> {
    /// Render catalog items as this user sees them (stars, ratings, plays).
    pub fn view(&self) -> crate::convert::View<'_> {
        crate::convert::View {
            db: &self.state.db,
            user: self.user.id,
            sources: &self.state.sources,
        }
    }

    /// Credentials for backend calls on this user's behalf.
    pub fn remote(&self) -> UserCtx<'static> {
        remote(self.user.id)
    }

    /// The configured backend of a source.
    pub fn runtime(&self, source: i64) -> Result<&SourceRuntime, ApiError> {
        self.state
            .sources
            .get(&source)
            .ok_or_else(|| ApiError::generic("The backend for this library is not configured."))
    }

    /// The configured backend of a library.
    pub async fn library_runtime(&self, library: i64) -> Result<&SourceRuntime, ApiError> {
        let lib = self
            .state
            .db
            .library(library)
            .await
            .map_err(db_error)?
            .ok_or_else(|| ApiError::not_found("Library"))?;
        self.runtime(lib.source_id)
    }

    /// The `musicFolderId` a request is limited to, if any.
    pub fn folder(&self) -> Result<Option<i64>, ApiError> {
        self.params.parse_opt("musicFolderId")
    }
}

/// Per-user backend tokens arrive with account linking (M5); until then every
/// user acts as the configured token's account.
pub fn remote(user_id: i64) -> UserCtx<'static> {
    UserCtx {
        user_id,
        remote_token: None,
    }
}

/// A failed backend call as an API error. Unreachable backends fail the call
/// rather than serving stale data.
pub fn backend_error(e: BackendError) -> ApiError {
    match e {
        BackendError::NotFound => ApiError::not_found("Item"),
        other => {
            tracing::warn!("backend error: {other}");
            ApiError::generic(format!("Backend error: {other}"))
        }
    }
}

/// Extensions we fully implement. Only advertise what works end to end.
#[cfg_attr(not(feature = "sonic"), allow(unused_variables, unused_mut))]
pub async fn open_subsonic_extensions(state: &AppState) -> Envelope {
    let mut exts = vec![
        OpenSubsonicExtension {
            name: "apiKeyAuthentication",
            versions: vec![1],
        },
        OpenSubsonicExtension {
            name: "formPost",
            versions: vec![1],
        },
        OpenSubsonicExtension {
            name: "songLyrics",
            versions: vec![1],
        },
    ];
    // Only once some tracks have been analysed: before then every answer
    // would be empty, and clients probe once and cache the result.
    #[cfg(feature = "sonic")]
    if crate::sonic::ready(state).await {
        exts.push(OpenSubsonicExtension {
            name: "sonicSimilarity",
            versions: vec![1],
        });
    }
    Envelope::with(exts)
}

pub fn license() -> Envelope {
    Envelope::with(License {
        valid: true,
        email: None,
        license_expires: None,
        trial_expires: None,
    })
}

pub async fn get_user(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let username = ctx.params.required("username")?;
    let folders = folder_ids(ctx).await?;
    if rsub_store::username_key(username) == rsub_store::username_key(&ctx.user.username) {
        return Ok(Envelope::with(to_api_user(&ctx.user, &folders)));
    }
    if !ctx.user.is_admin() {
        return Err(ApiError::not_authorized());
    }
    let user = ctx
        .state
        .db
        .user_by_username(username)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("User"))?;
    Ok(Envelope::with(to_api_user(&user, &folders)))
}

/// Users change their own password; admins anyone's.
pub async fn change_password(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let username = ctx.params.required("username")?;
    let password = decode_password(ctx.params.required("password")?)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| ApiError::generic("Invalid password."))?;
    let own = rsub_store::username_key(username) == rsub_store::username_key(&ctx.user.username);
    if !own && !ctx.user.is_admin() {
        return Err(ApiError::not_authorized());
    }
    let db = &ctx.state.db;
    let user = if own {
        ctx.user.clone()
    } else {
        db.user_by_username(username)
            .await
            .map_err(db_error)?
            .ok_or_else(|| ApiError::not_found("User"))?
    };
    // The stored username is the encryption context, as authentication reads it.
    let password_enc = ctx
        .state
        .secrets
        .encrypt(Purpose::Password, &user.username, password.as_bytes())
        .map_err(|e| {
            tracing::error!("cannot encrypt a password: {e}");
            ApiError::generic("Cannot store the password.")
        })?;
    let change = UserUpdate {
        password_enc: Some(password_enc),
        ..UserUpdate::default()
    };
    if !db.update_user(user.id, change).await.map_err(db_error)? {
        return Err(ApiError::not_found("User"));
    }
    Ok(Envelope::ok())
}

/// The user an API key belongs to. Any authentication is accepted, so clients
/// can check the credentials they hold.
pub fn token_info(ctx: &Ctx<'_>) -> Envelope {
    Envelope::with(TokenInfo {
        username: ctx.user.username.clone(),
    })
}

pub async fn get_users(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    if !ctx.user.is_admin() {
        return Err(ApiError::not_authorized());
    }
    let users = ctx.state.db.list_users().await.map_err(db_error)?;
    let folders = folder_ids(ctx).await?;
    Ok(Envelope::with(Users {
        user: users.iter().map(|u| to_api_user(u, &folders)).collect(),
    }))
}

/// Every user sees every library until per-user access arrives (M5).
async fn folder_ids(ctx: &Ctx<'_>) -> Result<Vec<i64>, ApiError> {
    Ok(ctx
        .state
        .db
        .libraries()
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|l| l.id)
        .collect())
}

fn to_api_user(u: &User, folders: &[i64]) -> ApiUser {
    let has = |r| u.roles.contains(r);
    ApiUser {
        username: u.username.clone(),
        email: u.email.clone(),
        scrobbling_enabled: has(Roles::SCROBBLING),
        max_bit_rate: (u.max_bitrate > 0).then_some(u.max_bitrate),
        admin_role: has(Roles::ADMIN),
        settings_role: has(Roles::SETTINGS),
        download_role: has(Roles::DOWNLOAD),
        upload_role: has(Roles::UPLOAD),
        playlist_role: has(Roles::PLAYLIST),
        cover_art_role: has(Roles::COVER_ART),
        comment_role: has(Roles::COMMENT),
        podcast_role: has(Roles::PODCAST),
        stream_role: has(Roles::STREAM),
        jukebox_role: has(Roles::JUKEBOX),
        share_role: has(Roles::SHARE),
        video_conversion_role: has(Roles::VIDEO_CONVERSION),
        folder: folders.to_vec(),
    }
}
