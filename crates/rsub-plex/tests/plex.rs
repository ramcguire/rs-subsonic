//! PlexBackend against a fake Plex server serving the fixtures in `fixtures/`.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rsub_core::Kind;
use rsub_core::backend::{
    ArtRef, BackendError, ByteRange, CatalogBatch, CatalogSource, CreditRole, Discovery,
    MediaRequest, MediaSource, PlayState, PlaylistEntry, RemoteLibrary, RemotePlaylist, RemoteRef,
    RemoteState, StateOrder, TagKind, TrackRemote, UserCtx, UserState,
};
use rsub_plex::{PlexBackend, PlexConfig};

const TOKEN: &str = "admin-token";

/// A track with an audio stream, LyricFind's timed lyrics, a sidecar's
/// untimed ones, and a LyricFind stream Plex lists but answers 404 for.
const TRACK_WITH_LYRICS: &str = r#"{"MediaContainer":{"size":1,"Metadata":[
    {"ratingKey":"400","type":"track","title":"x","Media":[{"Part":[{"key":"/library/parts/9/1/f.mp3",
     "Stream":[{"id":76,"streamType":2,"codec":"mp3"},
               {"id":77,"key":"/library/streams/77","streamType":4,"codec":"lrc","timed":"1",
                "provider":"com.plexapp.agents.lyricfind"},
               {"id":78,"key":"/library/streams/78","streamType":4,"codec":"txt"},
               {"id":79,"key":"/library/streams/79","streamType":4,"codec":"lrc",
                "provider":"com.plexapp.agents.lyricfind"}]}]}]}]}}"#;

/// Untimed lyrics, with `timed` as XML-style `"0"`.
const UNTIMED_LYRICS: &str = r#"{"MediaContainer":{"size":1,"Lyrics":[{"timed":"0",
    "Line":[{"Span":[{"text":"First"}]},{"Span":[{"text":"Sec"},{"text":"ond"}]}]}]}}"#;

/// Plex's similar artists that are in the library, shaped like 1.43's.
const SIMILAR: &str = r#"{"MediaContainer":{"size":2,"Metadata":[
    {"ratingKey":"9561","type":"artist","title":"GRiZ"},
    {"ratingKey":"9562","type":"artist","title":"Pretty Lights"}]}}"#;

/// Artist styles: the section's style directory, and each style's artists.
const STYLES: &str = r#"{"MediaContainer":{"size":2,"Directory":[
    {"fastKey":"/library/sections/3/all?style=502","key":"502","title":"Pop/Rock"},
    {"fastKey":"/library/sections/3/all?style=501","key":"501","title":"British Invasion"}]}}"#;
const STYLE_501: &str = r#"{"MediaContainer":{"size":1,"Metadata":[
    {"ratingKey":"100","type":"artist","title":"The Beatles"}]}}"#;
const STYLE_502: &str = r#"{"MediaContainer":{"size":2,"Metadata":[
    {"ratingKey":"100","type":"artist","title":"The Beatles"},
    {"ratingKey":"555","type":"artist","title":"Not in this page"}]}}"#;

/// Two tracks with user state, as Plex sends them: a love (10) and 3.5 stars.
const STATES: &str = r#"{"MediaContainer":{"size":2,"Metadata":[
    {"ratingKey":"300","type":"track","title":"a","userRating":10.0,"lastRatedAt":1700000000,
     "viewCount":3,"lastViewedAt":1700000100},
    {"ratingKey":"301","type":"track","title":"b","userRating":7.0,"lastRatedAt":1700000200}]}}"#;

/// A built-in smart playlist and one of the user's, shaped like Plex 1.43's.
const PLAYLISTS: &str = r#"{"MediaContainer":{"size":2,"Metadata":[
    {"ratingKey":"28711","type":"playlist","title":"All Music","summary":"Everything.",
     "smart":true,"playlistType":"audio","duration":5252432000,"leafCount":20180,
     "addedAt":1790446100,"updatedAt":1790446101},
    {"ratingKey":"4242","type":"playlist","title":"Road trip","summary":"","smart":false,
     "playlistType":"audio","composite":"/playlists/4242/composite/1790457779",
     "duration":410000,"leafCount":2,"addedAt":1790457779,"updatedAt":1790457780}]}}"#;

#[derive(Default)]
struct Seen {
    /// `(path and query, container start, container size)`
    requests: Vec<(String, Option<String>, Option<String>)>,
    /// `"METHOD uri"` of every request.
    calls: Vec<String>,
    range: Option<String>,
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Slice a fixture's `Metadata` array like Plex paging does, keeping only the
/// items an `updatedAt>>=` or `addedAt>>=` filter in `query` matches.
fn paged(name: &str, query: &str, h: &HeaderMap) -> String {
    let mut v: serde_json::Value = serde_json::from_str(&fixture(name)).unwrap();
    let get = |k: &str| {
        h.get(k)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
    };
    let mut items = v["MediaContainer"]["Metadata"].as_array().unwrap().clone();
    let query = query.replace("%3E", ">");
    for field in ["updatedAt", "addedAt"] {
        let bound = query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{field}>>=")));
        if let Some(bound) = bound {
            let bound: i64 = bound.parse().unwrap();
            items.retain(|m| m[field].as_i64().is_some_and(|t| t >= bound));
            v["MediaContainer"]["totalSize"] = items.len().into();
        }
    }
    let start = get("X-Plex-Container-Start").unwrap_or(0);
    let size = get("X-Plex-Container-Size").unwrap_or(items.len());
    let page: Vec<_> = items.iter().skip(start).take(size).cloned().collect();
    v["MediaContainer"]["size"] = page.len().into();
    v["MediaContainer"]["offset"] = start.into();
    v["MediaContainer"]["Metadata"] = page.into();
    v.to_string()
}

/// [`paged`] for an inline document.
fn page_of(doc: &str, h: &HeaderMap) -> String {
    let mut v: serde_json::Value = serde_json::from_str(doc).unwrap();
    let get = |k: &str| {
        h.get(k)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
    };
    let items = v["MediaContainer"]["Metadata"].as_array().unwrap().clone();
    let start = get("X-Plex-Container-Start").unwrap_or(0);
    let size = get("X-Plex-Container-Size").unwrap_or(items.len());
    let page: Vec<_> = items.iter().skip(start).take(size).cloned().collect();
    v["MediaContainer"]["totalSize"] = items.len().into();
    v["MediaContainer"]["size"] = page.len().into();
    v["MediaContainer"]["Metadata"] = page.into();
    v.to_string()
}

async fn fake_plex(seen: Arc<Mutex<Seen>>, req: Request<Body>) -> Response {
    let h = req.headers().clone();
    let uri = req.uri().clone();
    let path = uri.path();
    let query = uri.query().unwrap_or("");
    assert!(!query.contains(TOKEN), "token leaked into the URL: {uri}");
    assert!(h.contains_key("X-Plex-Client-Identifier"));
    let hs = |k: &str| h.get(k).map(|v| v.to_str().unwrap().to_owned());
    let method = req.method().clone();
    {
        let mut seen = seen.lock().unwrap();
        seen.requests.push((
            uri.to_string(),
            hs("X-Plex-Container-Start"),
            hs("X-Plex-Container-Size"),
        ));
        seen.calls.push(format!("{method} {uri}"));
    }
    let token = hs("X-Plex-Token");
    if token.as_deref() != Some(TOKEN) && token.as_deref() != Some("user-token") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let json = |s: String| ([(header::CONTENT_TYPE, "application/json")], s).into_response();

    match path {
        "/identity" => json(
            r#"{"MediaContainer":{"size":0,"machineIdentifier":"abc123","version":"1.41.0"}}"#
                .into(),
        ),
        "/library/sections" => json(fixture("sections")),
        // User state: by-key reads, and filtered, sorted section queries.
        "/library/metadata/300,301" => json(STATES.into()),
        "/library/sections/3/style" => {
            assert!(query.contains("type=8"));
            json(STYLES.into())
        }
        "/library/sections/3/all" if query.contains("style=501") => json(STYLE_501.into()),
        "/library/sections/3/all" if query.contains("style=502") => json(STYLE_502.into()),
        "/library/sections/3/all" if !query.contains("includeGuids") => json(page_of(STATES, &h)),
        "/playlists" if method == Method::GET => json(PLAYLISTS.into()),
        "/playlists/4242/items" if method == Method::GET => {
            json(paged("playlist-items", query, &h))
        }
        "/library/sections/3/all" => {
            let fx = match query.split('&').find_map(|kv| kv.strip_prefix("type=")) {
                Some("8") => "artists",
                Some("9") => "albums",
                Some("10") => "tracks",
                other => panic!("unexpected type {other:?}"),
            };
            assert!(query.contains("includeGuids=1"));
            json(paged(fx, query, &h))
        }
        "/library/sections/6/all" => {
            let fx = match query.split('&').find_map(|kv| kv.strip_prefix("type=")) {
                Some("8") => "recorded/artists",
                Some("9") => "recorded/albums",
                Some("10") => "recorded/tracks",
                other => panic!("unexpected type {other:?}"),
            };
            json(paged(fx, query, &h))
        }
        "/library/metadata/300,301,999" => json(fixture("metadata")),
        // Discovery: a track with two lyrics streams, and similar artists.
        "/library/metadata/400" => json(TRACK_WITH_LYRICS.into()),
        "/library/streams/77" => json(fixture("lyrics")),
        "/library/streams/78" => json(UNTIMED_LYRICS.into()),
        "/library/metadata/10207/similar" => json(SIMILAR.into()),
        p if p.starts_with("/library/parts/") => {
            let data: Vec<u8> = (0..100u8).collect();
            let range = hs("range");
            seen.lock().unwrap().range = range.clone();
            match range.as_deref() {
                Some("bytes=10-19") => (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (header::CONTENT_TYPE, "audio/flac".to_owned()),
                        (header::CONTENT_RANGE, "bytes 10-19/100".to_owned()),
                    ],
                    data[10..20].to_vec(),
                )
                    .into_response(),
                None => ([(header::CONTENT_TYPE, "audio/flac")], data).into_response(),
                Some(r) => panic!("unexpected range {r}"),
            }
        }
        "/photo/:/transcode" => (
            [(header::CONTENT_TYPE, "image/jpeg")],
            format!("resized?{query}"),
        )
            .into_response(),
        "/library/metadata/200/thumb/1700000200" => {
            ([(header::CONTENT_TYPE, "image/jpeg")], "original").into_response()
        }
        "/playlists" if method == Method::POST => json(
            r#"{"MediaContainer":{"size":1,"Metadata":[{"ratingKey":"4242","type":"playlist"}]}}"#
                .into(),
        ),
        "/:/rate" | "/:/scrobble" | "/:/timeline" => StatusCode::OK.into_response(),
        p if p.starts_with("/playlists/4242") => StatusCode::OK.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn start(page_size: u64) -> (Arc<PlexBackend>, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let s = seen.clone();
    let app = Router::new().fallback(move |req: Request<Body>| fake_plex(s.clone(), req));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let backend = PlexBackend::new(
        "home",
        &PlexConfig {
            url: format!("http://{addr}/"),
            token: TOKEN.into(),
            client_id: "test-client".into(),
            page_size,
        },
    )
    .unwrap();
    (backend, seen)
}

fn music() -> RemoteLibrary {
    RemoteLibrary {
        key: "3".into(),
        name: "Music".into(),
        locations: vec![],
        change_marker: None,
        scanning: false,
    }
}

const ADMIN: UserCtx<'static> = UserCtx {
    user_id: 1,
    remote_token: None,
};

#[tokio::test]
async fn identity_and_libraries() {
    let (plex, _) = start(500).await;
    assert_eq!(
        plex.identity().await.unwrap(),
        ("abc123".into(), "1.41.0".into())
    );
    let libs = plex.libraries().await.unwrap();
    assert_eq!(
        libs,
        [RemoteLibrary {
            key: "3".into(),
            name: "Music".into(),
            locations: vec!["/data/music".into()],
            change_marker: Some("412193".into()),
            scanning: true,
        }]
    );
}

#[tokio::test]
async fn full_scan_pages_and_maps() {
    let (plex, seen) = start(2).await;
    let lib = music();
    let batches: Vec<_> = plex.full_scan(&lib).collect().await;
    let (mut artists, mut albums, mut tracks) = (vec![], vec![], vec![]);
    for b in batches {
        match b.unwrap() {
            CatalogBatch::Artists(v) => artists.extend(v),
            CatalogBatch::Albums(v) => albums.extend(v),
            CatalogBatch::Tracks(v) => tracks.extend(v),
        }
    }
    // Pages of two: artists 1, albums 2, tracks 2 requests; styles aside.
    let pages: Vec<_> = seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|(uri, _, _)| !uri.contains("style"))
        .map(|(_, start, size)| (start.clone().unwrap(), size.clone().unwrap()))
        .collect();
    assert_eq!(
        pages,
        [("0", "2"), ("0", "2"), ("2", "2"), ("0", "2"), ("2", "2")]
            .map(|(a, b)| (a.to_string(), b.to_string()))
    );

    assert_eq!(artists.len(), 2);
    let beatles = &artists[0];
    assert_eq!(beatles.remote.key, "100");
    assert_eq!(beatles.sort_name.as_deref(), Some("Beatles"));
    assert_eq!(
        beatles.mbid.as_deref(),
        Some("b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d")
    );
    assert_eq!(beatles.updated_at, 1_700_000_100_000);
    // Styles come from the style listing, which the artist listing lacks.
    assert_eq!(
        beatles.tags,
        [
            (TagKind::Genre, "Rock".to_string()),
            (TagKind::Style, "British Invasion".to_string()),
            (TagKind::Style, "Pop/Rock".to_string()),
        ]
    );

    assert_eq!(albums.len(), 3);
    assert_eq!(albums[0].label.as_deref(), Some("Apple"));
    assert_eq!(albums[0].release_date.as_deref(), Some("1969-09-26"));
    assert_eq!(albums[0].artist.as_ref().unwrap().key, "100");
    assert_eq!(albums[1].year, Some(1997));
    assert_eq!(albums[1].release_types, ["album", "remastered"]);
    assert!(!albums[1].is_compilation);
    assert!(albums[2].is_compilation);

    // The track without media is skipped.
    assert_eq!(tracks.len(), 3);
    let t = &tracks[0];
    assert_eq!(t.part_key, "/library/parts/500/1600000020/file.flac");
    assert_eq!(t.duration_ms, 259_946);
    assert_eq!(t.popularity, Some(1_500_000));
    assert_eq!(t.year, Some(1969));
    assert_eq!(t.bitrate_kbps, Some(1015));
    assert_eq!(t.credits.len(), 2);
    assert_eq!(t.credits[0].remote.as_ref().unwrap().key, "100");
    assert_eq!(tracks[1].track_no, Some(2));
    assert_eq!(tracks[1].duration_ms, 182_000);
    // A track artist that differs from the album artist is a name-only credit.
    let chumba = &tracks[2];
    assert_eq!(chumba.display_artist, "Chumbawamba");
    assert_eq!(chumba.credits[0].role, CreditRole::Artist);
    assert!(chumba.credits[0].remote.is_none());
    assert_eq!(chumba.credits[1].role, CreditRole::AlbumArtist);
    assert_eq!(chumba.credits[1].name, "Various Artists");
}

/// Sanitized responses from a real Plex Media Server (1.43), with paths moved
/// under `/music`: they map as the hand-written fixtures do.
#[tokio::test]
async fn recorded_responses_map() {
    let (plex, _) = start(500).await;
    let lib = RemoteLibrary {
        key: "6".into(),
        ..music()
    };
    let (mut artists, mut albums, mut tracks) = (vec![], vec![], vec![]);
    for b in plex.full_scan(&lib).collect::<Vec<_>>().await {
        match b.unwrap() {
            CatalogBatch::Artists(v) => artists.extend(v),
            CatalogBatch::Albums(v) => albums.extend(v),
            CatalogBatch::Tracks(v) => tracks.extend(v),
        }
    }
    assert_eq!(artists.len(), 2);
    // List items carry `Guid` arrays when asked with `includeGuids=1`.
    assert!(artists.iter().all(|a| a.mbid.is_some()));
    assert!(artists.iter().all(|a| {
        a.remote
            .guid
            .as_deref()
            .is_some_and(|g| g.starts_with("plex://artist/"))
    }));
    assert_eq!(albums.len(), 3);
    assert!(albums.iter().all(|a| a.mbid.is_some() && a.year.is_some()));
    assert!(
        albums
            .iter()
            .all(|a| a.artist.as_ref().is_some_and(|r| r.key == "23329"))
    );
    assert_eq!(tracks.len(), 4);
    for t in &tracks {
        assert!(
            t.remote_path
                .as_deref()
                .is_some_and(|p| p.starts_with("/music/ZHU/")),
            "{t:?}"
        );
        assert!(t.part_key.starts_with("/library/parts/"));
        assert!(t.duration_ms > 0 && t.size.is_some());
    }
    // Not every item has a `Guid` array.
    assert_eq!(tracks.iter().filter(|t| t.mbid.is_some()).count(), 3);
    // A featured artist arrives as one display string ("ZHU x A‐Trak x
    // Keznamdi"), credited by name only.
    let feat = &tracks[0];
    assert_ne!(feat.display_artist, "ZHU");
    assert!(
        feat.credits
            .iter()
            .any(|c| c.role == CreditRole::Artist && c.remote.is_none())
    );
}

/// Changed items are those with `updatedAt` from `since` on, and those added
/// since that Plex hasn't given an `updatedAt` yet, each listed once.
#[tokio::test]
async fn changes_since_lists_changed_and_added() {
    let (plex, seen) = start(2).await;
    let lib = music();
    // Since second 1600000012: every artist, album and track changed at
    // 17000001xx or later, except album 202 and tracks 302 and 303, which
    // have only an `addedAt` from then on (and 303 no media, so no record).
    let (mut artists, mut albums, mut tracks) = (vec![], vec![], vec![]);
    for b in plex
        .changes_since(&lib, 1_600_000_012_000)
        .collect::<Vec<_>>()
        .await
    {
        match b.unwrap() {
            CatalogBatch::Artists(v) => artists.extend(v),
            CatalogBatch::Albums(v) => albums.extend(v),
            CatalogBatch::Tracks(v) => tracks.extend(v),
        }
    }
    let keys = |v: Vec<RemoteRef>| v.into_iter().map(|r| r.key).collect::<Vec<_>>();
    assert_eq!(
        keys(artists.iter().map(|a| a.remote.clone()).collect()),
        ["100", "101"]
    );
    // Styles still come with changed artists.
    assert!(
        artists[0]
            .tags
            .contains(&(TagKind::Style, "Pop/Rock".into()))
    );
    assert_eq!(
        keys(albums.iter().map(|a| a.remote.clone()).collect()),
        ["200", "201", "202"]
    );
    assert_eq!(
        keys(tracks.iter().map(|t| t.remote.clone()).collect()),
        ["300", "301", "302"]
    );
    let files: Vec<_> = plex
        .track_files(&lib, Some(1_600_000_012_000))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flat_map(|p| p.unwrap())
        .collect();
    assert_eq!(files.len(), 3, "the track without media is skipped");

    // Nothing changed since: one request per type and listing, and no styles.
    seen.lock().unwrap().requests.clear();
    let since = 1_800_000_000_000;
    assert!(
        plex.changes_since(&lib, since)
            .collect::<Vec<_>>()
            .await
            .is_empty()
    );
    let uris: Vec<String> = seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .map(|(uri, _, _)| uri.replace("%3E", ">"))
        .collect();
    assert_eq!(
        uris,
        [
            (8, "updatedAt"),
            (8, "addedAt"),
            (9, "updatedAt"),
            (9, "addedAt"),
            (10, "updatedAt"),
            (10, "addedAt")
        ]
        .map(|(t, f)| format!("/library/sections/3/all?type={t}&includeGuids=1&{f}>>=1800000000"))
    );
}

#[tokio::test]
async fn track_files_for_tag_pass() {
    let (plex, seen) = start(2).await;
    let lib = music();
    let pages: Vec<_> = plex.track_files(&lib, None).collect().await;
    let files: Vec<_> = pages.into_iter().flat_map(|p| p.unwrap()).collect();
    // Two pages of the track listing; the track without media is skipped.
    assert_eq!(seen.lock().unwrap().requests.len(), 2);
    assert_eq!(files.len(), 3);
    assert_eq!(files[0].key, "300");
    assert_eq!(files[0].album_key, "200");
    assert_eq!(files[0].artist_key.as_deref(), Some("100"));
    assert_eq!(
        files[0].remote_path,
        "/data/music/The Beatles/Abbey Road/01 Come Together.flac"
    );
    assert_eq!(files[1].remote_path, r"C:\Music\Beatles\02 Something.MP3");
}

#[tokio::test]
async fn enrichment() {
    let (plex, _) = start(500).await;
    let keys = ["300", "301", "999"].map(String::from);
    let e = plex.enrich(&keys).await.unwrap();
    assert_eq!(e.len(), 2);
    assert_eq!(e[0].key, "300");
    assert_eq!(e[0].bit_depth, Some(24));
    assert_eq!(e[0].sample_rate, Some(96_000));
    assert_eq!(e[0].replay_gain.track_gain, Some(-9.36));
    assert_eq!(e[0].replay_gain.album_peak, Some(1.0));
    assert!(e[0].has_lyrics);
    assert_eq!(e[0].tags, [(TagKind::Mood, "Groovy".to_string())]);
    assert_eq!(e[1].sample_rate, Some(44_100));
    assert!(!e[1].has_lyrics);
}

async fn body_of(s: rsub_core::backend::MediaStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut b = s.body;
    while let Some(chunk) = b.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

#[tokio::test]
async fn stream_forwards_range() {
    let (plex, seen) = start(500).await;
    let track = TrackRemote {
        key: "300".into(),
        part_key: "/library/parts/500/1600000020/file.flac".into(),
        remote_path: None,
        suffix: Some("flac".into()),
        duration_ms: 1,
    };
    let full = plex
        .open(&ADMIN, &track, MediaRequest::default())
        .await
        .unwrap();
    assert_eq!(full.status, 200);
    assert_eq!(full.content_length, Some(100));
    assert_eq!(body_of(full).await, (0..100u8).collect::<Vec<_>>());

    let req = MediaRequest {
        range: Some(ByteRange::From {
            start: 10,
            end: Some(19),
        }),
    };
    let user = UserCtx {
        user_id: 2,
        remote_token: Some("user-token"),
    };
    let part = plex.open(&user, &track, req).await.unwrap();
    assert_eq!(seen.lock().unwrap().range.as_deref(), Some("bytes=10-19"));
    assert_eq!(part.status, 206);
    assert_eq!(part.content_range.as_deref(), Some("bytes 10-19/100"));
    assert_eq!(body_of(part).await, (10..20u8).collect::<Vec<_>>());

    let bad = UserCtx {
        user_id: 3,
        remote_token: Some("revoked"),
    };
    assert!(matches!(
        plex.open(&bad, &track, MediaRequest::default()).await,
        Err(BackendError::Unauthorized)
    ));
}

#[tokio::test]
async fn cover_art() {
    let (plex, _) = start(500).await;
    let art = ArtRef {
        key: "200".into(),
        thumb: "/library/metadata/200/thumb/1700000200".into(),
    };
    let original = plex.cover_art(&ADMIN, &art, None).await.unwrap();
    assert_eq!(body_of(original).await, b"original");
    let resized = plex.cover_art(&ADMIN, &art, Some(300)).await.unwrap();
    assert_eq!(resized.content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(
        String::from_utf8(body_of(resized).await).unwrap(),
        "resized?url=%2Flibrary%2Fmetadata%2F200%2Fthumb%2F1700000200&width=300&height=300&minSize=1&upscale=1"
    );
    let missing = ArtRef {
        key: "9".into(),
        thumb: "/nope".into(),
    };
    assert!(matches!(
        plex.cover_art(&ADMIN, &missing, None).await,
        Err(BackendError::NotFound)
    ));
}

#[tokio::test]
async fn user_state_calls() {
    let (plex, seen) = start(1).await;
    let user = UserCtx {
        user_id: 2,
        remote_token: Some("user-token"),
    };
    let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    plex.rate(&user, "300", 4).await.unwrap();
    plex.rate(&user, "300", 0).await.unwrap();
    plex.scrobble(&user, "300").await.unwrap();
    plex.now_playing(&user, "300", PlayState::Playing, 0, 1000)
        .await
        .unwrap();
    // Keys go into URLs: anything but a Plex id is refused.
    assert!(matches!(
        plex.scrobble(&user, "../x").await,
        Err(BackendError::Protocol(_))
    ));
    let calls = std::mem::take(&mut seen.lock().unwrap().calls);
    seen.lock().unwrap().requests.clear();
    let lib = "identifier=com.plexapp.plugins.library";
    assert_eq!(
        calls,
        [
            format!("PUT /:/rate?key=300&{lib}&rating=8"),
            format!("PUT /:/rate?key=300&{lib}&rating=-1"),
            format!("GET /:/scrobble?key=300&{lib}"),
            "GET /:/timeline?ratingKey=300&key=%2Flibrary%2Fmetadata%2F300&state=playing&time=0&duration=1000".into(),
        ]
    );

    // Reads: a love is a star, half stars round down, times are in ms.
    let love = RemoteState {
        key: "300".into(),
        rating: Some(5),
        rated_at: Some(1_700_000_000_000),
        play_count: Some(3),
        last_played_at: Some(1_700_000_100_000),
    };
    let half = RemoteState {
        key: "301".into(),
        rating: Some(3),
        rated_at: Some(1_700_000_200_000),
        ..Default::default()
    };
    let two = keys(&["300", "301"]);
    let got = plex.states(&user, "3", Kind::Track, &two).await.unwrap();
    assert_eq!(got, [love.clone(), half.clone()]);
    // Many keys: the library's items with state are listed instead.
    let many: Vec<String> = (300..700).map(|k| k.to_string()).collect();
    let got = plex.states(&user, "3", Kind::Track, &many).await.unwrap();
    assert_eq!(got, [love.clone(), half.clone()]);
    let got = plex
        .ranked(&user, "3", Kind::Album, StateOrder::Starred, 1, 20)
        .await
        .unwrap();
    assert_eq!(got, [half]);
    let reqs = std::mem::take(&mut seen.lock().unwrap().requests);
    let mut paths: Vec<_> = reqs
        .iter()
        .map(|(u, start, size)| format!("{u} {start:?} {size:?}"))
        .collect();
    // The two listings run at once.
    paths[1..5].sort();
    assert_eq!(
        paths,
        [
            "/library/metadata/300,301 None None",
            // Listed a page at a time (page size 1 here) until the total.
            "/library/sections/3/all?type=10&userRating%3E%3E=0 Some(\"0\") Some(\"1\")",
            "/library/sections/3/all?type=10&userRating%3E%3E=0 Some(\"1\") Some(\"1\")",
            "/library/sections/3/all?type=10&viewCount%3E%3E=0 Some(\"0\") Some(\"1\")",
            "/library/sections/3/all?type=10&viewCount%3E%3E=0 Some(\"1\") Some(\"1\")",
            "/library/sections/3/all?type=9&userRating=10&sort=lastRatedAt:desc Some(\"1\") Some(\"20\")",
        ]
    );

    // Playlists.
    let lists = plex.playlists(&user).await.unwrap();
    assert_eq!(lists.len(), 2);
    assert!(lists[0].smart);
    assert_eq!(
        lists[1],
        RemotePlaylist {
            id: "4242".into(),
            name: "Road trip".into(),
            comment: None,
            smart: false,
            song_count: 2,
            duration_ms: 410_000,
            created_at: 1_790_457_779_000,
            updated_at: 1_790_457_780_000,
            thumb: Some("/playlists/4242/composite/1790457779".into()),
        }
    );
    let entries = plex.playlist_entries(&user, "4242").await.unwrap();
    assert_eq!(
        entries,
        [
            PlaylistEntry {
                id: "377".into(),
                key: "300".into()
            },
            PlaylistEntry {
                id: "378".into(),
                key: "301".into()
            },
        ]
    );
    seen.lock().unwrap().calls.clear();

    let id = plex
        .create_playlist(&user, "Road trip", &keys(&["300", "301"]))
        .await
        .unwrap();
    assert_eq!(id, "4242");
    // Plex can't create an empty playlist: seed it with a track, then clear it.
    plex.create_playlist(&user, "Empty", &[]).await.unwrap();
    plex.edit_playlist(&user, "4242", Some("Road trip 2"), Some(""))
        .await
        .unwrap();
    plex.edit_playlist(&user, "4242", None, None).await.unwrap();
    plex.add_to_playlist(&user, "4242", &keys(&["302"]))
        .await
        .unwrap();
    plex.remove_from_playlist(&user, "4242", &keys(&["377"]))
        .await
        .unwrap();
    plex.delete_playlist(&user, "4242").await.unwrap();

    let uri = |k: &str| {
        format!("server%3A%2F%2Fabc123%2Fcom.plexapp.plugins.library%2Flibrary%2Fmetadata%2F{k}")
    };
    assert_eq!(
        seen.lock().unwrap().calls,
        [
            "GET /identity".to_owned(),
            format!(
                "POST /playlists?type=audio&smart=0&title=Road+trip&uri={}",
                uri("300")
            ),
            format!("PUT /playlists/4242/items?uri={}", uri("301")),
            "GET /library/sections".into(),
            "GET /library/sections/3/all?type=10&includeGuids=1".into(),
            format!(
                "POST /playlists?type=audio&smart=0&title=Empty&uri={}",
                uri("300")
            ),
            "DELETE /playlists/4242/items".into(),
            "PUT /playlists/4242?title=Road+trip+2&summary=".into(),
            format!("PUT /playlists/4242/items?uri={}", uri("302")),
            "DELETE /playlists/4242/items/377".into(),
            "DELETE /playlists/4242".into(),
        ]
    );
}

#[tokio::test]
async fn discovery_calls() {
    let (plex, seen) = start(1).await;
    let user = UserCtx {
        user_id: 2,
        remote_token: Some("user-token"),
    };
    let track = TrackRemote {
        key: "400".into(),
        part_key: "/library/parts/9/1/f.mp3".into(),
        remote_path: None,
        suffix: None,
        duration_ms: 0,
    };
    let docs = plex.lyrics(&user, &track).await.unwrap();
    assert_eq!(docs.len(), 2);
    assert!(docs[0].synced);
    assert_eq!(docs[0].lines[0].start_ms, Some(1500));
    assert_eq!(docs[0].lines[0].text, "First line");
    assert_eq!(docs[0].lines[2].text, "");
    assert!(!docs[1].synced);
    assert_eq!(docs[1].lines[1].start_ms, None);
    assert_eq!(docs[1].lines[1].text, "Second");
    assert!(docs.iter().all(|d| d.lang.is_empty()));

    let artist = RemoteRef {
        key: "10207".into(),
        guid: None,
    };
    assert_eq!(
        plex.similar_artists(&user, &artist, 1).await.unwrap(),
        ["9561"]
    );
    let bad = RemoteRef {
        key: "../x".into(),
        guid: None,
    };
    assert!(plex.similar_artists(&user, &bad, 5).await.is_err());
    assert_eq!(
        seen.lock().unwrap().calls,
        [
            "GET /library/metadata/400",
            "GET /library/streams/77",
            "GET /library/streams/78",
            "GET /library/streams/79",
            "GET /library/metadata/10207/similar?count=1",
        ]
    );
}
