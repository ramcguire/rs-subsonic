//! `/api/v1/users`: user administration.
//!
//! - `GET /users`, `GET /users/{name}`: users, without their passwords.
//! - `POST /users`: create one (201).
//! - `PATCH /users/{name}`: change a password, email, roles or bitrate limit.
//! - `DELETE /users/{name}` (204), with their API keys and local ratings.
//! - `GET /users/{name}/api-keys`, `POST` to create one (201; the key is in
//!   the response and nowhere else), `DELETE …/api-keys/{id}` to revoke one.
//!
//! No change may leave the server without an admin (409).

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use rsub_core::crypto::{self, Purpose};
use rsub_core::{Roles, User};
use rsub_store::{ApiKeyInfo, NewUser, StoreError, UserUpdate};
use serde::{Deserialize, Deserializer, Serialize};

use super::{Error, admin, json, parse};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", get(list).post(create))
        .route("/users/{name}", get(show).patch(update).delete(remove))
        .route("/users/{name}/api-keys", get(keys).post(create_key))
        .route("/users/{name}/api-keys/{id}", delete(revoke_key))
}

/// A user as the admin API shows them.
#[derive(Serialize)]
struct UserOut {
    id: i64,
    username: String,
    email: Option<String>,
    admin: bool,
    /// [`Roles::NAMES`].
    roles: Vec<&'static str>,
    /// kbps; 0 = unlimited.
    max_bitrate: u32,
    created_at: i64,
    updated_at: i64,
}

impl From<User> for UserOut {
    fn from(u: User) -> Self {
        UserOut {
            id: u.id,
            admin: u.is_admin(),
            roles: u.roles.names(),
            username: u.username,
            email: u.email,
            max_bitrate: u.max_bitrate,
            created_at: u.created_at,
            updated_at: u.updated_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    username: String,
    password: String,
    email: Option<String>,
    /// Default: the roles of a regular user, or all of them for an admin.
    roles: Option<Vec<String>>,
    #[serde(default)]
    admin: bool,
    #[serde(default)]
    max_bitrate: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateBody {
    password: Option<String>,
    /// `null` or `""` clears it.
    #[serde(default, deserialize_with = "present")]
    email: Option<Option<String>>,
    /// Replaces the roles; `admin` then adds or removes admin.
    roles: Option<Vec<String>>,
    admin: Option<bool>,
    max_bitrate: Option<u32>,
}

/// A field that is present, even as `null`, as `Some`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

fn email(e: Option<String>) -> Option<String> {
    e.map(|e| e.trim().to_owned()).filter(|e| !e.is_empty())
}

fn roles(names: &[String]) -> Result<Roles, Error> {
    Roles::from_names(names).map_err(Error::bad)
}

fn check_username(name: &str) -> Result<(), Error> {
    if name.trim().is_empty() || name != name.trim() || name.chars().any(char::is_control) {
        return Err(Error::bad(
            "a username must be non-empty, without surrounding spaces or control characters",
        ));
    }
    Ok(())
}

fn encrypt(state: &AppState, username: &str, password: &str) -> Result<Vec<u8>, Error> {
    if password.is_empty() {
        return Err(Error::bad("the password must not be empty"));
    }
    state
        .secrets
        .encrypt(Purpose::Password, username, password.as_bytes())
        .map_err(|e| {
            tracing::error!("cannot encrypt a password: {e}");
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "encryption failed".into(),
            )
        })
}

async fn user(state: &AppState, name: &str) -> Result<User, Error> {
    state
        .db
        .user_by_username(name)
        .await?
        .ok_or_else(|| Error::not_found(format!("no user '{name}'")))
}

async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let users: Vec<UserOut> = state
        .db
        .list_users()
        .await?
        .into_iter()
        .map(UserOut::from)
        .collect();
    Ok(json(&users))
}

async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    Ok(json(&UserOut::from(user(&state, &name).await?)))
}

async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let b: CreateBody = parse(&body)?;
    check_username(&b.username)?;
    let base = match &b.roles {
        Some(names) => roles(names)?,
        None if b.admin => Roles::ALL,
        None => Roles::USER_DEFAULT,
    };
    let address = email(b.email.clone());
    let new = NewUser {
        username: &b.username,
        password_enc: encrypt(&state, &b.username, &b.password)?,
        email: address.as_deref(),
        roles: if b.admin { base.with_admin(true) } else { base },
        max_bitrate: b.max_bitrate,
    };
    match state.db.create_user(new).await {
        Ok(_) => {}
        Err(StoreError::Conflict(_)) => {
            return Err(Error::conflict(format!(
                "user '{}' already exists",
                b.username
            )));
        }
        Err(e) => return Err(e.into()),
    }
    let u = user(&state, &b.username).await?;
    Ok((StatusCode::CREATED, json(&UserOut::from(u))).into_response())
}

async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let b: UpdateBody = parse(&body)?;
    let u = user(&state, &name).await?;
    let mut new_roles = match &b.roles {
        Some(names) => Some(roles(names)?),
        None => None,
    };
    if let Some(a) = b.admin {
        new_roles = Some(new_roles.unwrap_or(u.roles).with_admin(a));
    }
    let change = UserUpdate {
        password_enc: b
            .password
            .as_deref()
            .map(|p| encrypt(&state, &u.username, p))
            .transpose()?,
        email: b.email.map(email),
        roles: new_roles,
        max_bitrate: b.max_bitrate,
    };
    if !state.db.update_user(u.id, change).await? {
        return Err(Error::not_found(format!("no user '{name}'")));
    }
    Ok(json(&UserOut::from(user(&state, &name).await?)))
}

async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let u = user(&state, &name).await?;
    if !state.db.delete_user(u.id).await? {
        return Err(Error::not_found(format!("no user '{name}'")));
    }
    tracing::info!(user = %name, "user deleted");
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn keys(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let u = user(&state, &name).await?;
    let keys: Vec<ApiKeyInfo> = state.db.api_keys(u.id).await?;
    Ok(json(
        &keys
            .into_iter()
            .map(|k| KeyOut {
                id: k.id,
                name: k.name,
                created_at: k.created_at,
                key: None,
            })
            .collect::<Vec<_>>(),
    ))
}

#[derive(Serialize)]
struct KeyOut {
    id: i64,
    name: String,
    created_at: i64,
    /// Only when it's created.
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct KeyBody {
    name: Option<String>,
}

async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let b: KeyBody = if body.iter().all(u8::is_ascii_whitespace) {
        KeyBody::default()
    } else {
        parse(&body)?
    };
    let u = user(&state, &name).await?;
    let label = b
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".into());
    let (key, hash) = crypto::new_api_key().map_err(|e| {
        tracing::error!("cannot make an API key: {e}");
        Error(StatusCode::INTERNAL_SERVER_ERROR, "no randomness".into())
    })?;
    let id = state.db.create_api_key(u.id, &label, &hash).await?;
    let created_at = state
        .db
        .api_keys(u.id)
        .await?
        .into_iter()
        .find(|k| k.id == id)
        .map_or(0, |k| k.created_at);
    let out = KeyOut {
        id,
        name: label,
        created_at,
        key: Some(key),
    };
    Ok((StatusCode::CREATED, json(&out)).into_response())
}

async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((name, id)): Path<(String, i64)>,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let u = user(&state, &name).await?;
    if !state.db.delete_api_key(u.id, id).await? {
        return Err(Error::not_found(format!("'{name}' has no API key {id}")));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
