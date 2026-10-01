//! End-to-end tests against the router with an in-memory SQLite store.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rsub_core::Roles;
use rsub_core::crypto::{self, Purpose, SecretBox};
use rsub_server::{AppState, AuthOptions, router};
use rsub_store::{Db, NewUser};
use serde_json::Value;
use tower::ServiceExt;

async fn setup_with_auth(auth: AuthOptions) -> Router {
    let db = Db::connect("sqlite::memory:", 1).await.unwrap();
    db.migrate().await.unwrap();
    let secrets = Arc::new(SecretBox::new(&[42; 32]).unwrap());
    for (name, roles) in [("admin", Roles::ALL), ("bob", Roles::USER_DEFAULT)] {
        let password_enc = secrets.encrypt(Purpose::Password, name, b"sesame").unwrap();
        let id = db
            .create_user(NewUser {
                username: name,
                password_enc,
                email: None,
                roles,
                max_bitrate: 0,
            })
            .await
            .unwrap();
        if name == "bob" {
            db.create_api_key(id, "test", &crypto::sha256(b"bob-key"))
                .await
                .unwrap();
        }
    }
    router(AppState::new(db, secrets, auth))
}

async fn setup() -> Router {
    setup_with_auth(AuthOptions::default()).await
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, String, String) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ct = resp.headers()[header::CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_owned();
    let body =
        String::from_utf8(to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    (status, ct, body)
}

async fn get_json(app: &Router, uri: &str) -> Value {
    let (status, _, body) = call(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v["subsonic-response"].clone()
}

fn error_code(v: &Value) -> Option<i64> {
    v["error"]["code"].as_i64()
}

#[tokio::test]
async fn auth_matrix() {
    let app = setup().await;
    let cases: &[(&str, Option<i64>)] = &[
        ("u=bob&p=sesame", None),
        ("u=bob&p=enc:736573616d65", None),
        // md5("sesame" + "c19b2d")
        ("u=bob&t=26719a1196d2a940705a59634eb18eab&s=c19b2d", None),
        ("apiKey=bob-key", None),
        ("u=bob&p=wrong", Some(40)),
        ("u=nobody&p=sesame", Some(40)),
        ("u=bob&t=26719a1196d2a940705a59634eb18eab&s=other", Some(40)),
        ("u=bob", Some(10)),
        ("u=bob&t=abc", Some(10)),
        ("p=sesame", Some(10)),
        ("u=bob&p=sesame&t=x&s=y", Some(43)),
        ("u=bob&apiKey=bob-key", Some(43)),
        ("apiKey=nope", Some(44)),
    ];
    for (creds, expected) in cases {
        let v = get_json(
            &app,
            &format!("/rest/ping.view?f=json&v=1.16.1&c=test&{creds}"),
        )
        .await;
        assert_eq!(error_code(&v), *expected, "{creds}: {v}");
        assert_eq!(
            v["status"],
            if expected.is_some() { "failed" } else { "ok" }
        );
        assert_eq!(v["openSubsonic"], true);
    }
}

#[tokio::test]
async fn disabled_mechanisms() {
    let app = setup_with_auth(AuthOptions {
        allow_plaintext: false,
        allow_token_auth: false,
    })
    .await;
    let v = get_json(&app, "/rest/ping?f=json&u=bob&p=sesame").await;
    assert_eq!(error_code(&v), Some(42));
    let v = get_json(
        &app,
        "/rest/ping?f=json&u=bob&t=26719a1196d2a940705a59634eb18eab&s=c19b2d",
    )
    .await;
    assert_eq!(error_code(&v), Some(41));
    let v = get_json(&app, "/rest/ping?f=json&apiKey=bob-key").await;
    assert_eq!(error_code(&v), None);
}

#[tokio::test]
async fn extensions_need_no_auth() {
    let app = setup().await;
    let v = get_json(&app, "/rest/getOpenSubsonicExtensions?f=json").await;
    let names: Vec<_> = v["openSubsonicExtensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["apiKeyAuthentication", "formPost", "songLyrics"]);
}

#[tokio::test]
async fn form_post_and_xml_default() {
    let app = setup().await;
    let req = Request::post("/rest/getLicense.view")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("u=bob&p=sesame&v=1.16.1&c=test"))
        .unwrap();
    let (status, ct, body) = call(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("application/xml"), "{ct}");
    assert!(
        body.contains(r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok""#),
        "{body}"
    );
    assert!(body.contains(r#"<license valid="true"/>"#), "{body}");
}

#[tokio::test]
async fn jsonp() {
    let app = setup().await;
    let (_, ct, body) = call(
        &app,
        Request::get("/rest/ping?f=jsonp&callback=cb_1&u=bob&p=sesame")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(ct.starts_with("application/javascript"));
    assert!(
        body.starts_with("cb_1({\"subsonic-response\":") && body.ends_with(");"),
        "{body}"
    );
    let v = get_json(&app, "/rest/ping?f=jsonp&callback=alert(1)&u=bob&p=sesame").await;
    assert_eq!(error_code(&v), Some(0));
}

#[tokio::test]
async fn users_and_permissions() {
    let app = setup().await;
    let v = get_json(&app, "/rest/getUser?f=json&u=bob&p=sesame&username=bob").await;
    assert_eq!(v["user"]["username"], "bob");
    assert_eq!(v["user"]["adminRole"], false);
    assert_eq!(v["user"]["streamRole"], true);

    let v = get_json(&app, "/rest/getUser?f=json&u=bob&p=sesame&username=admin").await;
    assert_eq!(error_code(&v), Some(50));
    let v = get_json(&app, "/rest/getUser?f=json&u=bob&p=sesame").await;
    assert_eq!(error_code(&v), Some(10));
    let v = get_json(&app, "/rest/getUsers?f=json&u=bob&p=sesame").await;
    assert_eq!(error_code(&v), Some(50));

    let v = get_json(
        &app,
        "/rest/getUser?f=json&u=admin&p=sesame&username=nobody",
    )
    .await;
    assert_eq!(error_code(&v), Some(70));
    let v = get_json(&app, "/rest/getUsers?f=json&u=admin&p=sesame").await;
    assert_eq!(v["users"]["user"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_logins_are_limited() {
    let app = setup().await;
    for _ in 0..10 {
        let v = get_json(&app, "/rest/ping?f=json&u=bob&p=wrong").await;
        assert_eq!(error_code(&v), Some(40));
    }
    let v = get_json(&app, "/rest/ping?f=json&u=BOB&p=sesame").await;
    assert_eq!(
        error_code(&v),
        Some(40),
        "refused even with the right password"
    );
    assert!(v["error"]["message"].as_str().unwrap().contains("Too many"));
    let v = get_json(&app, "/rest/ping?f=json&u=admin&p=sesame").await;
    assert_eq!(error_code(&v), None, "other users aren't affected");
}

#[tokio::test]
async fn change_password() {
    let app = setup().await;
    let v = get_json(
        &app,
        "/rest/changePassword?f=json&u=bob&p=sesame&username=admin&password=x",
    )
    .await;
    assert_eq!(error_code(&v), Some(50));
    // Hex-encoded, like `p`; the username in any case.
    let v = get_json(
        &app,
        "/rest/changePassword?f=json&u=bob&p=sesame&username=BOB&password=enc:6f70656e",
    )
    .await;
    assert_eq!(error_code(&v), None, "{v}");
    let v = get_json(&app, "/rest/ping?f=json&u=bob&p=sesame").await;
    assert_eq!(error_code(&v), Some(40));
    let v = get_json(&app, "/rest/ping?f=json&u=bob&p=open").await;
    assert_eq!(error_code(&v), None);

    let v = get_json(
        &app,
        "/rest/changePassword?f=json&u=admin&p=sesame&username=bob&password=again",
    )
    .await;
    assert_eq!(error_code(&v), None);
    let v = get_json(&app, "/rest/ping?f=json&u=bob&p=again").await;
    assert_eq!(error_code(&v), None);
    let v = get_json(
        &app,
        "/rest/changePassword?f=json&u=admin&p=sesame&username=nobody&password=x",
    )
    .await;
    assert_eq!(error_code(&v), Some(70));
    let v = get_json(
        &app,
        "/rest/changePassword?f=json&u=admin&p=sesame&username=bob&password=",
    )
    .await;
    assert!(error_code(&v).is_some(), "an empty password is refused");
}

#[tokio::test]
async fn token_info_and_accepted_saves() {
    let app = setup().await;
    let v = get_json(&app, "/rest/tokenInfo?f=json&apiKey=bob-key").await;
    assert_eq!(v["tokenInfo"]["username"], "bob");
    for method in ["savePlayQueue", "createBookmark", "deleteBookmark"] {
        let v = get_json(&app, &format!("/rest/{method}?f=json&apiKey=bob-key&id=1")).await;
        assert_eq!(error_code(&v), None, "{method}");
    }
}

#[tokio::test]
async fn unknown_method_is_404_with_envelope() {
    let app = setup().await;
    let (status, _, body) = call(
        &app,
        Request::get("/rest/doesNotExist?f=json&u=bob&p=sesame")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("\"status\":\"failed\""));
}
