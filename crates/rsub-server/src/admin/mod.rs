//! rs-subsonic's own admin API under `/api/v1`, apart from the Subsonic API:
//! JSON in and out (JSON lines for bulk data), errors as `{"error": "..."}`
//! with an HTTP status, and authentication by an admin's API key
//! (`rs-subsonic user api-key`) sent as `Authorization: Bearer <key>`.
//!
//! - `/users`: user administration ([`users`]).
//! - `/backup`: backups ([`backup`]).
//! - `/analysis`: moving audio analysis in and out, with the `sonic` feature.

#[cfg(feature = "sonic")]
mod analysis;
mod backup;
mod users;

use axum::Router;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rsub_core::User;
use rsub_core::crypto;
use rsub_store::StoreError;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::AppState;

pub fn router() -> Router<AppState> {
    let app = Router::new().merge(users::router()).merge(backup::router());
    #[cfg(feature = "sonic")]
    let app = app.merge(analysis::router());
    app
}

struct Error(StatusCode, String);

impl Error {
    fn bad(msg: impl Into<String>) -> Error {
        Error(StatusCode::BAD_REQUEST, msg.into())
    }

    fn not_found(msg: impl Into<String>) -> Error {
        Error(StatusCode::NOT_FOUND, msg.into())
    }

    fn conflict(msg: impl Into<String>) -> Error {
        Error(StatusCode::CONFLICT, msg.into())
    }
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Error {
        match e {
            StoreError::LastAdmin => Error::conflict(e.to_string()),
            StoreError::Invalid(m) => Error::bad(m),
            e => {
                tracing::error!("database error: {e}");
                Error(StatusCode::INTERNAL_SERVER_ERROR, "database error".into())
            }
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut r = json(&serde_json::json!({ "error": self.1 }));
        *r.status_mut() = self.0;
        r
    }
}

fn json<T: Serialize>(value: &T) -> Response {
    let body = serde_json::to_vec(value).expect("serializable");
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

/// A JSON request body.
fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(body).map_err(|e| Error::bad(format!("invalid JSON body: {e}")))
}

/// The admin the bearer key belongs to.
async fn admin(state: &AppState, headers: &HeaderMap) -> Result<User, Error> {
    let unauthorized = |m: &str| Error(StatusCode::UNAUTHORIZED, m.into());
    let key = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(|| unauthorized("an API key is required as `Authorization: Bearer <key>`"))?;
    let user = state
        .db
        .user_by_api_key_hash(&crypto::sha256(key.as_bytes()))
        .await?
        .ok_or_else(|| unauthorized("invalid API key"))?;
    if !user.is_admin() {
        return Err(Error(StatusCode::FORBIDDEN, "admin only".into()));
    }
    Ok(user)
}
