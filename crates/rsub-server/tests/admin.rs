//! The admin API (`/api/v1`): moving audio analysis in and out.

mod common;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use rsub_analysis::transfer::{self, Header, Row};
use rsub_core::crypto;
use rsub_core::tags::FileTags;
use rsub_testkit::{FILE_LEN, FakeTags};
use serde_json::Value;
use tower::ServiceExt;

use common::*;

const PATHS: [&str; 3] = [
    "/music/Abbey Road/Come Together.flac",
    "/music/Abbey Road/Something.flac",
    "/music/Homogenic/Hunter.flac",
];

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    key: Option<&str>,
    body: String,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(k) = key {
        req = req.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), 1 << 24).await.unwrap();
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

#[tokio::test]
async fn analysis_moves_in_and_out() {
    let tags = FakeTags::new();
    for p in PATHS {
        tags.tag(p, 5, FileTags::default());
    }
    let e = setup_tagged(tags).await;
    let app = &e.app;
    let admin = key(&e, "admin").await;
    let bob = key(&e, "bob").await;

    let status = "/api/v1/analysis";
    assert_eq!(
        call(app, "GET", status, None, String::new()).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app, "GET", status, Some("nope"), String::new())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(app, "GET", status, Some(&bob), String::new()).await.0,
        StatusCode::FORBIDDEN
    );
    let (code, body) = call(app, "GET", status, Some(&admin), String::new()).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["analyzer"], Value::Null);
    assert_eq!(v["roots"], serde_json::json!(["/music"]));
    assert_eq!(v["analyzers"], serde_json::json!([]));

    let size = FILE_LEN as u64;
    let mut doc = transfer::line(&Header::new("fake-1", "/music/"));
    doc += &transfer::line(&Row::analysed(
        "Abbey Road/Come Together.flac",
        size,
        &[0.6, 0.8],
    ));
    doc += &transfer::line(&Row::analysed(
        "Abbey Road/Something.flac",
        size + 1,
        &[1.0, 0.0],
    ));
    doc += &transfer::line(&Row::failed("Homogenic/Hunter.flac", size, "cannot decode"));
    doc += &transfer::line(&Row::analysed("Homogenic/Nope.flac", size, &[0.0, 1.0]));
    let counts = serde_json::json!({"stored": 1, "failed": 1, "size_mismatch": 1, "unknown": 1});

    let vectors = "/api/v1/analysis/vectors";
    let (code, body) = call(
        app,
        "POST",
        &format!("{vectors}?dry_run=true"),
        Some(&admin),
        doc.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), counts);
    let (_, body) = call(app, "GET", status, Some(&admin), String::new()).await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["analyzers"],
        serde_json::json!([])
    );

    let (code, body) = call(app, "POST", vectors, Some(&admin), doc).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), counts);
    let (_, body) = call(app, "GET", status, Some(&admin), String::new()).await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["analyzers"],
        serde_json::json!([{"id": "fake-1", "analysed": 1, "failed": 1}])
    );

    // Without analysis on, the analyzer must be named.
    let (code, _) = call(app, "GET", vectors, Some(&admin), String::new()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, body) = call(
        app,
        "GET",
        &format!("{vectors}?analyzer=fake-1"),
        Some(&admin),
        String::new(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (head, rows) = transfer::parse(&body).unwrap();
    assert_eq!(head, Header::new("fake-1", "/music"));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].path, "Abbey Road/Come Together.flac");
    assert_eq!(rows[0].vector().unwrap(), Some(vec![0.6, 0.8]));
    assert_eq!(rows[1].path, "Homogenic/Hunter.flac");
    assert_eq!(rows[1].vector().unwrap(), None);

    let (code, body) = call(app, "POST", vectors, Some(&admin), "not json".into()).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(serde_json::from_str::<Value>(&body).unwrap()["error"].is_string());
}
