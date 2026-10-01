//! The admin API's user administration (`/api/v1/users`) and backups
//! (`/api/v1/backup`).

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rsub_core::crypto::{self, SecretBox};
use rsub_server::{AppState, AuthOptions, router};
use rsub_sync::backup;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::*;

async fn call(app: &Router, method: &str, uri: &str, key: &str, body: &str) -> (StatusCode, Value) {
    let (status, text) = call_text(app, method, uri, key, body.to_owned()).await;
    let v = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, v)
}

async fn call_text(
    app: &Router,
    method: &str,
    uri: &str,
    key: &str,
    body: String,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1 << 26).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn key(e: &Env, user: &str) -> String {
    let u = e.db.user_by_username(user).await.unwrap().unwrap();
    let key = format!("{user}-key");
    e.db.create_api_key(u.id, "test", &crypto::sha256(key.as_bytes()))
        .await
        .unwrap();
    key
}

fn names(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn users_are_administered() {
    let e = setup(&[]).await;
    let app = &e.app;
    let admin = key(&e, "admin").await;
    let bob = key(&e, "bob").await;

    assert_eq!(
        call(app, "GET", "/api/v1/users", &bob, "").await.0,
        StatusCode::FORBIDDEN
    );
    let (code, list) = call(app, "GET", "/api/v1/users", &admin, "").await;
    assert_eq!(code, StatusCode::OK, "{list}");
    assert_eq!(names(&list), ["admin", "bob"]);
    assert_eq!(list[0]["admin"], true);
    assert!(list[0].get("password").is_none() && list[0].get("password_enc").is_none());

    // Create: a regular user unless told otherwise, and able to log in.
    let body = json!({"username": "carol", "password": "pw", "email": "c@x", "max_bitrate": 192});
    let (code, carol) = call(app, "POST", "/api/v1/users", &admin, &body.to_string()).await;
    assert_eq!(code, StatusCode::CREATED, "{carol}");
    assert_eq!(carol["admin"], false);
    assert_eq!(carol["max_bitrate"], 192);
    assert!(
        carol["roles"]
            .as_array()
            .unwrap()
            .contains(&json!("stream"))
    );
    ok(&get(app, "ping", "u=carol&p=pw&f=json", "").await);
    let (code, err) = call(app, "POST", "/api/v1/users", &admin, &body.to_string()).await;
    assert_eq!(code, StatusCode::CONFLICT, "{err}");
    for bad in [
        json!({"username": " ", "password": "pw"}),
        json!({"username": "dave", "password": ""}),
        json!({"username": "dave", "password": "pw", "roles": ["root"]}),
        json!({"username": "dave", "password": "pw", "pasword": "typo"}),
    ] {
        let (code, err) = call(app, "POST", "/api/v1/users", &admin, &bad.to_string()).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}: {err}");
        assert!(err["error"].is_string());
    }

    // Update: a new password, email cleared, promoted; untouched fields stay.
    let change = json!({"password": "new", "email": null, "admin": true});
    let (code, carol) = call(
        app,
        "PATCH",
        "/api/v1/users/carol",
        &admin,
        &change.to_string(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{carol}");
    assert_eq!(
        (&carol["email"], &carol["admin"]),
        (&Value::Null, &json!(true))
    );
    assert_eq!(carol["max_bitrate"], 192);
    ok(&get(app, "ping", "u=carol&p=new&f=json", "").await);
    assert_eq!(
        code_of(&get(app, "ping", "u=carol&p=pw&f=json", "").await),
        40
    );
    let (code, one) = call(app, "GET", "/api/v1/users/carol", &admin, "").await;
    assert_eq!((code, &one["username"]), (StatusCode::OK, &json!("carol")));
    assert_eq!(
        call(app, "GET", "/api/v1/users/nobody", &admin, "").await.0,
        StatusCode::NOT_FOUND
    );

    // API keys: shown once, listed without the key, revocable.
    let (code, k) = call(
        app,
        "POST",
        "/api/v1/users/carol/api-keys",
        &admin,
        r#"{"name":"phone"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{k}");
    let carol_key = k["key"].as_str().unwrap().to_owned();
    assert_eq!(
        call(app, "GET", "/api/v1/users", &carol_key, "").await.0,
        StatusCode::OK,
        "carol is an admin now"
    );
    let (_, keys) = call(app, "GET", "/api/v1/users/carol/api-keys", &admin, "").await;
    assert_eq!(keys[0]["name"], "phone");
    assert!(keys[0].get("key").is_none());
    let revoke = format!("/api/v1/users/carol/api-keys/{}", k["id"]);
    assert_eq!(
        call(app, "DELETE", &revoke, &admin, "").await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(app, "DELETE", &revoke, &admin, "").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(app, "GET", "/api/v1/users", &carol_key, "").await.0,
        StatusCode::UNAUTHORIZED
    );

    // The last admin can't go; with carol also an admin, admin can.
    assert_eq!(
        call(app, "DELETE", "/api/v1/users/carol", &admin, "")
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let demote = json!({"admin": false}).to_string();
    let (code, err) = call(app, "PATCH", "/api/v1/users/admin", &admin, &demote).await;
    assert_eq!(code, StatusCode::CONFLICT, "{err}");
    assert_eq!(
        call(app, "DELETE", "/api/v1/users/admin", &admin, "")
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (_, list) = call(app, "GET", "/api/v1/users", &admin, "").await;
    assert_eq!(names(&list), ["admin", "bob"]);
}

fn code_of(v: &Value) -> i64 {
    v["error"]["code"].as_i64().unwrap_or(0)
}

#[tokio::test]
async fn backups_restore_into_a_new_database() {
    let e = setup(&[]).await;
    let app = &e.app;
    let admin = key(&e, "admin").await;
    let bob = e.db.user_by_username("bob").await.unwrap().unwrap();
    e.db.rate_locally(bob.id, 1, 4).await.unwrap();

    assert_eq!(
        call_text(app, "GET", "/api/v1/backup", "bob-nope", String::new())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (code, doc) = call_text(app, "GET", "/api/v1/backup", &admin, String::new()).await;
    assert_eq!(code, StatusCode::OK, "{doc}");
    let lines: Vec<Value> = doc
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["rsub_backup"], 1);
    let count = |k: &str| lines.iter().filter(|l| l.get(k).is_some()).count();
    assert_eq!(
        (count("user"), count("api_key"), count("artist_rating")),
        (2, 1, 1)
    );
    let ledger = e.db.ledger_page(None, 10_000).await.unwrap();
    assert!(!ledger.is_empty());
    assert_eq!(count("identity"), ledger.len());

    // Restoring into the same server adds nothing.
    let (code, r) = call(app, "POST", "/api/v1/backup", &admin, &doc).await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(
        (&r["users_added"], &r["users_kept"]),
        (&json!(0), &json!(2))
    );
    assert_eq!(r["identity_keys_added"], 0);

    // A new database under the same key gets the users, keys, ids and the
    // rating, before its artist is synced.
    let secrets = Arc::new(SecretBox::new(&[42; 32]).unwrap());
    let fresh = rsub_store::testing::sqlite().await;
    let r = backup::restore(&fresh, &secrets, doc.as_bytes())
        .await
        .unwrap();
    assert_eq!(r.accounts.users_added, 2);
    assert_eq!(r.accounts.api_keys_added, 1);
    assert_eq!(
        (r.accounts.ratings_added, r.accounts.ratings_skipped),
        (1, 0)
    );
    assert_eq!(r.identity.added as usize, ledger.len());
    assert_eq!(fresh.ledger_page(None, 10_000).await.unwrap(), ledger);
    let app2 = router(AppState::new(
        fresh.clone(),
        secrets,
        AuthOptions::default(),
    ));
    ok(&get(&app2, "ping", "u=bob&p=sesame&f=json", "").await);
    assert_eq!(
        call(&app2, "GET", "/api/v1/users", &admin, "").await.0,
        StatusCode::OK,
        "the admin's key came along"
    );

    // Under another key the passwords can't be read: refused, nothing written.
    let other = SecretBox::new(&[7; 32]).unwrap();
    let empty = rsub_store::testing::sqlite().await;
    let err = backup::restore(&empty, &other, doc.as_bytes())
        .await
        .unwrap_err();
    assert!(matches!(err, backup::BackupError::WrongKey(_)), "{err}");
    assert_eq!(empty.count_users().await.unwrap(), 0);
    assert!(empty.ledger_is_empty().await.unwrap());

    let (code, err) = call(app, "POST", "/api/v1/backup", &admin, "{\"nope\":1}\n").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{err}");
}
