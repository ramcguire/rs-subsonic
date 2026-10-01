//! Subsonic authentication: `u`+`p` (plaintext or `enc:`), `u`+`t`+`s` (token), and
//! OpenSubsonic `apiKey`.

use rsub_api::params::decode_password;
use rsub_api::{ApiError, ErrorCode, Params};
use rsub_core::User;
use rsub_core::crypto::{self, Purpose};

use crate::{AppState, db_error};

pub async fn authenticate(state: &AppState, p: &Params) -> Result<User, ApiError> {
    if let Some(key) = p.get("apiKey") {
        if p.contains("u") {
            return Err(ApiError::new(ErrorCode::ConflictingAuth));
        }
        let hash = crypto::sha256(key.as_bytes());
        return state
            .db
            .user_by_api_key_hash(&hash)
            .await
            .map_err(db_error)?
            .ok_or_else(|| ApiError::new(ErrorCode::InvalidApiKey));
    }

    let username = p.required("u")?;
    let method = match (p.get("p"), p.get("t"), p.get("s")) {
        (Some(pw), None, None) => Method::Password(pw),
        (None, Some(t), Some(s)) => Method::Token { token: t, salt: s },
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(ApiError::new(ErrorCode::ConflictingAuth));
        }
        (None, Some(_), None) => return Err(ApiError::missing("s")),
        (None, None, Some(_)) => return Err(ApiError::missing("t")),
        (None, None, None) => return Err(ApiError::missing("p")),
    };
    match method {
        Method::Password(_) if !state.auth.allow_plaintext => {
            return Err(ApiError::new(ErrorCode::AuthMechanismNotSupported));
        }
        Method::Token { .. } if !state.auth.allow_token_auth => {
            return Err(ApiError::new(ErrorCode::TokenAuthNotSupported));
        }
        _ => {}
    }

    let wrong = || ApiError::new(ErrorCode::WrongCredentials);
    let user = state
        .db
        .user_by_username(username)
        .await
        .map_err(db_error)?
        .ok_or_else(wrong)?;
    let stored = state
        .secrets
        .decrypt_string(Purpose::Password, &user.username, &user.password_enc)
        .map_err(|e| {
            tracing::error!(user = %user.username, "cannot decrypt stored password: {e}");
            wrong()
        })?;

    let ok = match method {
        Method::Password(pw) => {
            decode_password(pw).is_some_and(|pw| crypto::ct_eq(pw.as_bytes(), stored.as_bytes()))
        }
        Method::Token { token, salt } => crypto::verify_subsonic_token(&stored, salt, token),
    };
    if ok { Ok(user) } else { Err(wrong()) }
}

enum Method<'a> {
    Password(&'a str),
    Token { token: &'a str, salt: &'a str },
}
