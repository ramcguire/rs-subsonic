//! Shared harness for the end-to-end tests: a FakeBackend synced into SQLite,
//! served by the router in-process.
#![allow(dead_code)] // each test binary uses a different subset

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
#[cfg(feature = "sonic")]
use rsub_analysis::{Metric, SonicSearch};
use rsub_core::Roles;
use rsub_core::backend::TagKind;
use rsub_core::crypto::{Purpose, SecretBox};
use rsub_core::text::IgnoredArticles;
use rsub_media::{CoverCache, PathMapper};
use rsub_server::{AppState, AuthOptions, SourceRuntime, router};
use rsub_store::{Db, NewUser};
use rsub_sync::{SyncEngine, SyncSource};
use rsub_testkit::{FakeBackend, FakeLibrary, FakeTags, album, artist, track};
use serde_json::Value;
use tower::ServiceExt;

pub const ADMIN: &str = "u=admin&p=sesame&f=json";
pub const BOB: &str = "u=bob&p=sesame&f=json";

pub fn library() -> FakeLibrary {
    let beatles = artist("a1", "The Beatles");
    let bjork = artist("a2", "Björk");
    let mut abbey = album("b1", &beatles, "Abbey Road");
    abbey.year = Some(1969);
    abbey.release_date = Some("1969-09-26".into());
    abbey.tags = vec![(TagKind::Genre, "Rock".into())];
    let mut homo = album("b2", &bjork, "Homogenic");
    homo.year = Some(1997);
    homo.added_at = 9_000;
    homo.tags = vec![(TagKind::Genre, "Electronic".into())];
    let tracks = vec![
        track("t1", &abbey, 1, "Come Together"),
        track("t2", &abbey, 2, "Something"),
        track("t3", &homo, 1, "Hunter"),
    ];
    FakeLibrary {
        artists: vec![beatles, bjork],
        albums: vec![abbey, homo],
        tracks,
        enrichment: Default::default(),
    }
}

pub struct Env {
    pub app: Router,
    pub fake: Arc<FakeBackend>,
    pub db: Db,
    pub sync: Arc<SyncEngine>,
    pub tmp: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// `local`: map the backend's `/music` to a temp dir holding these files.
pub async fn setup(local: &[(&str, Vec<u8>)]) -> Env {
    setup_state(local, None, library(), |_| {}).await
}

/// [`setup`] with these file tags read by the sync.
pub async fn setup_tagged(tags: Arc<FakeTags>) -> Env {
    setup_state(&[], Some(tags), library(), |_| {}).await
}

/// [`setup`] over this library, with these file tags when given.
pub async fn setup_library(lib: FakeLibrary, tags: Option<Arc<FakeTags>>) -> Env {
    setup_state(&[], tags, lib, |_| {}).await
}

/// Analyzer id of the vectors [`setup_sonic`] searches.
#[cfg(feature = "sonic")]
pub const SONIC: &str = "test-1";

/// [`setup`] with sonic similarity over the vectors of [`SONIC`].
#[cfg(feature = "sonic")]
pub async fn setup_sonic() -> Env {
    setup_state(&[], None, library(), |state| {
        state.sonic = Some(SonicSearch::new(state.db.clone(), SONIC, Metric::Cosine));
    })
    .await
}

/// [`setup`], with `f` adjusting the state before the router is built.
async fn setup_state(
    local: &[(&str, Vec<u8>)],
    tags: Option<Arc<FakeTags>>,
    lib: FakeLibrary,
    f: impl FnOnce(&mut AppState),
) -> Env {
    let tmp = std::env::temp_dir().join(format!(
        "rsub-e2e-{}",
        hex::encode(rsub_core::crypto::random_bytes::<6>().unwrap())
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    for (rel, data) in local {
        let p = tmp.join("music").join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    }

    let db = rsub_store::testing::sqlite().await;
    let secrets = Arc::new(SecretBox::new(&[42; 32]).unwrap());
    // Bob may write to the shared account; `USER_DEFAULT` alone may not.
    let bob = Roles::USER_DEFAULT | Roles::PLAYLIST | Roles::SCROBBLING | Roles::RATING;
    for (name, roles) in [("admin", Roles::ALL), ("bob", bob)] {
        db.create_user(NewUser {
            username: name,
            password_enc: secrets.encrypt(Purpose::Password, name, b"sesame").unwrap(),
            email: None,
            roles,
            max_bitrate: 0,
        })
        .await
        .unwrap();
    }

    let fake = FakeBackend::new();
    fake.set_library("1", "Music", lib);
    let source_id = db.ensure_source("home", "fake").await.unwrap();
    let sync = SyncEngine::new(
        db.clone(),
        vec![SyncSource {
            id: source_id,
            name: "home".into(),
            catalog: fake.clone(),
            libraries: vec![],
            enrich: true,
            tags: tags.map(|t| t as _),
        }],
        IgnoredArticles::default(),
        None,
    );
    sync.sync_all().await.unwrap();

    let mut sources = HashMap::new();
    sources.insert(
        source_id,
        SourceRuntime {
            backend: fake.handle(),
            paths: PathMapper::new([("/music".to_string(), tmp.join("music"))]),
            serve_local: !local.is_empty(),
        },
    );
    let mut state = AppState::new(db.clone(), secrets, AuthOptions::default());
    state.sources = Arc::new(sources);
    state.sync = Some(sync.clone());
    state.covers = Some(Arc::new(
        CoverCache::open(tmp.join("covers"), 1 << 20).await.unwrap(),
    ));
    f(&mut state);
    Env {
        app: router(state),
        fake,
        db,
        sync,
        tmp,
    }
}

pub async fn raw(
    app: &Router,
    uri: &str,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::get(uri);
    if let Some(r) = range {
        req = req.header(header::RANGE, r);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = to_bytes(resp.into_body(), 1 << 24).await.unwrap().to_vec();
    (status, headers, body)
}

pub async fn get(app: &Router, method: &str, auth: &str, params: &str) -> Value {
    let (status, _, body) = raw(app, &format!("/rest/{method}?{auth}&{params}"), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{method}: {}",
        String::from_utf8_lossy(&body)
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    v["subsonic-response"].clone()
}

pub fn ok(v: &Value) -> &Value {
    assert_eq!(v["status"], "ok", "{v}");
    v
}

pub fn code(v: &Value) -> i64 {
    v["error"]["code"].as_i64().unwrap_or(-1)
}

pub fn titles(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|x| {
            x["title"]
                .as_str()
                .or(x["name"].as_str())
                .unwrap()
                .to_owned()
        })
        .collect()
}
