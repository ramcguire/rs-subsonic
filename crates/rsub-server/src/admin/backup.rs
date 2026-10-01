//! `/api/v1/backup`: `GET` downloads a backup ([`rsub_sync::backup`]);
//! `POST` restores one, merging it into this database.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rsub_sync::backup::{self, BackupError};
use serde::Serialize;

use super::{Error, admin, json};
use crate::AppState;

/// Largest backup accepted: the identity ledger is ~150 bytes a key.
const MAX_RESTORE: usize = 512 << 20;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/backup",
        get(download)
            .post(restore)
            .layer(DefaultBodyLimit::max(MAX_RESTORE)),
    )
}

impl From<BackupError> for Error {
    fn from(e: BackupError) -> Error {
        match e {
            BackupError::Store(e) => e.into(),
            e @ (BackupError::Format { .. } | BackupError::WrongKey(_)) => {
                Error::bad(e.to_string())
            }
            BackupError::Io(e) => {
                tracing::error!("backup: {e}");
                Error(StatusCode::INTERNAL_SERVER_ERROR, "I/O error".into())
            }
        }
    }
}

async fn download(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Error> {
    let who = admin(&state, &headers).await?;
    let mut body = Vec::new();
    let n = backup::export(&state.db, &mut body).await?;
    tracing::info!(
        by = %who.username,
        users = n.users,
        identity_keys = n.identity_keys,
        "backup downloaded"
    );
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"rs-subsonic-backup.jsonl\""),
            ),
        ],
        body,
    )
        .into_response())
}

#[derive(Serialize)]
struct Restored {
    users_added: u64,
    /// Users the server already had, left as they were.
    users_kept: u64,
    api_keys_added: u64,
    ratings_added: u64,
    /// Ratings of artists the catalog doesn't have (yet).
    ratings_skipped: u64,
    settings_added: u64,
    identity_keys_read: u64,
    identity_keys_added: u64,
}

async fn restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    let who = admin(&state, &headers).await?;
    let r = backup::restore(&state.db, &state.secrets, &body[..]).await?;
    tracing::info!(
        by = %who.username,
        users_added = r.accounts.users_added,
        identity_keys_added = r.identity.added,
        "backup restored"
    );
    Ok(json(&Restored {
        users_added: r.accounts.users_added,
        users_kept: r.accounts.users_kept,
        api_keys_added: r.accounts.api_keys_added,
        ratings_added: r.accounts.ratings_added,
        ratings_skipped: r.accounts.ratings_skipped,
        settings_added: r.settings_added,
        identity_keys_read: r.identity.read,
        identity_keys_added: r.identity.added,
    }))
}
