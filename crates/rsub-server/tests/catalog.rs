//! End-to-end catalog tests: a FakeBackend synced into SQLite, then the router.

mod common;

use axum::http::{StatusCode, header};
use rsub_core::PublicId;
use rsub_core::backend::ByteRange;
use rsub_server::METHODS;
use rsub_testkit::file_bytes;
use serde_json::Value;

use common::*;

/// Well-formed, but no album has it.
const UNKNOWN_ALBUM: &str = "alaaaaaaaaaaaaaaaa";

#[tokio::test]
async fn browse_by_tags() {
    let e = setup(&[]).await;
    let app = &e.app;

    let v = get(app, "getMusicFolders", BOB, "").await;
    assert_eq!(ok(&v)["musicFolders"]["musicFolder"][0]["name"], "Music");
    let folder = v["musicFolders"]["musicFolder"][0]["id"].as_i64().unwrap();
    let v = get(app, "getUser", BOB, "username=bob").await;
    assert_eq!(v["user"]["folder"][0], folder);

    let v = get(app, "getArtists", BOB, "").await;
    let idx = &ok(&v)["artists"]["index"];
    assert_eq!(v["artists"]["ignoredArticles"], "The El La Los Las Le Les");
    assert_eq!(idx[0]["name"], "B");
    let names = titles(&idx[0]["artist"]);
    assert_eq!(names, ["The Beatles", "Björk"]);
    let beatles = idx[0]["artist"][0].clone();
    assert_eq!(beatles["albumCount"], 1);
    let beatles_id = beatles["id"].as_str().unwrap();
    assert!(beatles_id.parse::<PublicId>().is_ok(), "{beatles_id}");
    let cover = beatles["coverArt"].as_str().unwrap();
    assert!(cover.starts_with(&format!("{beatles_id}-")), "{cover}");

    let v = get(
        app,
        "getArtist",
        BOB,
        &format!("id={}", beatles["id"].as_str().unwrap()),
    )
    .await;
    let a = &ok(&v)["artist"];
    assert_eq!(a["name"], "The Beatles");
    assert_eq!(titles(&a["album"]), ["Abbey Road"]);
    let album_id = a["album"][0]["id"].as_str().unwrap().to_owned();

    let v = get(app, "getAlbum", BOB, &format!("id={album_id}")).await;
    let al = &ok(&v)["album"];
    assert_eq!(al["songCount"], 2);
    assert_eq!(al["duration"], 360);
    assert_eq!(al["genre"], "Rock");
    assert_eq!(al["artistId"], beatles["id"]);
    assert_eq!(
        al["releaseDate"],
        serde_json::json!({"year": 1969, "month": 9, "day": 26})
    );
    assert_eq!(al["artists"][0]["name"], "The Beatles");
    let songs = &al["song"];
    assert_eq!(titles(songs), ["Come Together", "Something"]);
    let s = &songs[0];
    assert_eq!(s["parent"], album_id.as_str());
    assert_eq!(s["albumId"], album_id.as_str());
    assert_eq!(s["artistId"], beatles["id"]);
    assert_eq!(s["duration"], 180);
    assert_eq!(s["suffix"], "flac");
    assert_eq!(s["contentType"], "audio/flac");
    assert_eq!(s["genre"], "Rock");
    assert_eq!(s["genres"][0]["name"], "Rock");
    assert_eq!(s["artists"][0]["id"], beatles["id"]);
    assert_eq!(s["albumArtists"][0]["name"], "The Beatles");
    assert_eq!(s["path"], "The Beatles/Abbey Road/01 - Come Together.flac");
    assert_eq!(s["coverArt"], al["coverArt"]);
    assert_eq!(s["type"], "music");
    assert_eq!(s["mediaType"], "song");

    let v = get(
        app,
        "getSong",
        BOB,
        &format!("id={}", s["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(ok(&v)["song"]["title"], "Come Together");

    // Wrong kinds, unknown and malformed ids are "not found"; missing ids are
    // error 10.
    let song_id = s["id"].as_str().unwrap();
    for (m, p) in [
        ("getAlbum", format!("id={song_id}")),
        ("getArtist", format!("id={album_id}")),
        ("getAlbum", format!("id={UNKNOWN_ALBUM}")),
        ("getSong", "id=x".into()),
        ("getAlbum", "id=al1".into()),
    ] {
        assert_eq!(code(&get(app, m, BOB, &p).await), 70, "{m} {p}");
    }
    assert_eq!(code(&get(app, "getAlbum", BOB, "").await), 10);
}

#[tokio::test]
async fn browse_by_folders() {
    let e = setup(&[]).await;
    let app = &e.app;
    let v = get(app, "getIndexes", BOB, "").await;
    let idx = &ok(&v)["indexes"];
    assert!(idx["lastModified"].as_i64().unwrap() > 0);
    let beatles = idx["index"][0]["artist"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let modified = idx["lastModified"].as_i64().unwrap();
    let v = get(
        app,
        "getIndexes",
        BOB,
        &format!("ifModifiedSince={modified}"),
    )
    .await;
    assert!(ok(&v)["indexes"]["index"].as_array().unwrap().is_empty());

    let v = get(app, "getMusicDirectory", BOB, &format!("id={beatles}")).await;
    let dir = &ok(&v)["directory"];
    assert_eq!(dir["name"], "The Beatles");
    assert_eq!(dir["child"][0]["isDir"], true);
    assert_eq!(dir["child"][0]["title"], "Abbey Road");
    let album = dir["child"][0]["id"].as_str().unwrap().to_owned();
    let v = get(app, "getMusicDirectory", BOB, &format!("id={album}")).await;
    let dir = &ok(&v)["directory"];
    assert_eq!(dir["parent"], beatles.as_str());
    assert_eq!(titles(&dir["child"]), ["Come Together", "Something"]);
    assert_eq!(dir["child"][0]["isDir"], false);
}

#[tokio::test]
async fn lists_search_and_genres() {
    let e = setup(&[]).await;
    let app = &e.app;
    let v = get(app, "getAlbumList2", BOB, "type=newest&size=1").await;
    assert_eq!(titles(&ok(&v)["albumList2"]["album"]), ["Homogenic"]);
    let v = get(app, "getAlbumList", BOB, "type=alphabeticalByName").await;
    assert_eq!(
        titles(&ok(&v)["albumList"]["album"]),
        ["Abbey Road", "Homogenic"]
    );
    let v = get(app, "getAlbumList2", BOB, "type=byGenre&genre=Electronic").await;
    assert_eq!(titles(&ok(&v)["albumList2"]["album"]), ["Homogenic"]);
    let v = get(
        app,
        "getAlbumList2",
        BOB,
        "type=byYear&fromYear=2000&toYear=1990",
    )
    .await;
    assert_eq!(titles(&ok(&v)["albumList2"]["album"]), ["Homogenic"]);
    // Lists that need user state are empty until M2.
    let v = get(app, "getAlbumList2", BOB, "type=frequent").await;
    assert!(ok(&v)["albumList2"]["album"].as_array().unwrap().is_empty());
    assert_eq!(code(&get(app, "getAlbumList2", BOB, "").await), 10);
    assert_eq!(code(&get(app, "getAlbumList2", BOB, "type=bogus").await), 0);
    assert_eq!(
        code(&get(app, "getAlbumList2", BOB, "type=byYear&fromYear=1").await),
        10
    );

    let v = get(app, "getRandomSongs", BOB, "size=10&genre=Rock").await;
    assert_eq!(ok(&v)["randomSongs"]["song"].as_array().unwrap().len(), 2);
    let v = get(app, "getSongsByGenre", BOB, "genre=Electronic").await;
    assert_eq!(titles(&ok(&v)["songsByGenre"]["song"]), ["Hunter"]);
    let v = get(app, "getGenres", BOB, "").await;
    let g = &ok(&v)["genres"]["genre"];
    assert_eq!(g[0]["value"], "Electronic");
    assert_eq!(g[1]["value"], "Rock");
    assert_eq!(g[1]["songCount"], 2);
    assert_eq!(g[1]["albumCount"], 1);

    // search3 with an empty query returns everything (used by clients to sync).
    let v = get(app, "search3", BOB, "query=%22%22&songCount=500").await;
    let r = &ok(&v)["searchResult3"];
    assert_eq!(r["song"].as_array().unwrap().len(), 3);
    assert_eq!(r["album"].as_array().unwrap().len(), 2);
    assert_eq!(r["artist"].as_array().unwrap().len(), 2);
    let v = get(app, "search3", BOB, "query=bjork").await;
    let r = &ok(&v)["searchResult3"];
    assert_eq!(titles(&r["artist"]), ["Björk"]);
    assert_eq!(titles(&r["song"]), ["Hunter"]);
    let v = get(app, "search2", BOB, "query=abbey&songCount=1&songOffset=1").await;
    let r = &ok(&v)["searchResult2"];
    assert_eq!(titles(&r["album"]), ["Abbey Road"]);
    assert_eq!(titles(&r["song"]), ["Something"]);
}

#[tokio::test]
async fn empty_but_valid_endpoints() {
    let e = setup(&[]).await;
    let app = &e.app;
    for (m, key) in [
        ("getStarred2", "starred2"),
        ("getStarred", "starred"),
        ("getPlaylists", "playlists"),
        ("getNowPlaying", "nowPlaying"),
        ("getInternetRadioStations", "internetRadioStations"),
        ("getBookmarks", "bookmarks"),
        ("getPodcasts", "podcasts"),
        ("getShares", "shares"),
    ] {
        let v = get(app, m, BOB, "").await;
        assert!(ok(&v)[key].is_object(), "{m}: {v}");
    }
    let v = get(app, "getPlayQueue", BOB, "").await;
    assert!(ok(&v)["playQueue"].is_null());
}

/// Every advertised method answers with a Subsonic envelope (no panics, no 404).
#[tokio::test]
async fn every_method_dispatches() {
    let e = setup(&[]).await;
    for m in METHODS {
        let (status, _, body) = raw(&e.app, &format!("/rest/{m}?{ADMIN}&id=tr1"), None).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT,
            "{m}: {status}"
        );
        assert!(!body.is_empty(), "{m}");
    }
}

#[tokio::test]
async fn stream_proxies_backend_with_ranges() {
    let e = setup(&[]).await;
    let app = &e.app;
    let v = get(app, "search3", BOB, "query=hunter").await;
    let id = v["searchResult3"]["song"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let (status, h, body) = raw(app, &format!("/rest/stream?{BOB}&id={id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "audio/flac");
    assert_eq!(h[header::ACCEPT_RANGES], "bytes");
    assert_eq!(body, file_bytes());

    let (status, h, body) = raw(
        app,
        &format!("/rest/stream?{BOB}&id={id}"),
        Some("bytes=10-19"),
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(h[header::CONTENT_RANGE], "bytes 10-19/1000");
    assert_eq!(body, &file_bytes()[10..20]);
    let opened = e.fake.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 2);
    assert_eq!(
        opened[1].1,
        Some(ByteRange::From {
            start: 10,
            end: Some(19)
        })
    );

    let (status, h, _) = raw(app, &format!("/rest/download?{BOB}&id={id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h[header::CONTENT_DISPOSITION],
        "attachment; filename=\"Hunter.flac\"; filename*=UTF-8''Hunter.flac"
    );

    // Errors render as envelopes.
    let (_, h, body) = raw(app, &format!("/rest/stream?{BOB}&id=tr999"), None).await;
    assert!(
        h[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["subsonic-response"]["error"]["code"], 70);
}

#[tokio::test]
async fn stream_serves_verified_local_files() {
    // "Hunter" exists locally with the catalog size; "Something" has the wrong size.
    let e = setup(&[
        ("Homogenic/Hunter.flac", file_bytes()),
        ("Abbey Road/Something.flac", vec![1, 2, 3]),
    ])
    .await;
    let app = &e.app;
    let id_of = |t: &str| {
        let t = t.to_owned();
        async move {
            let v = get(app, "search3", BOB, &format!("query={t}")).await;
            v["searchResult3"]["song"][0]["id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
    let hunter = id_of("hunter").await;
    let (status, h, body) = raw(
        app,
        &format!("/rest/stream?{BOB}&id={hunter}"),
        Some("bytes=-100"),
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(h[header::CONTENT_RANGE], "bytes 900-999/1000");
    assert!(h.contains_key(header::ETAG));
    assert!(h.contains_key(header::LAST_MODIFIED));
    assert_eq!(body, &file_bytes()[900..]);
    let (status, _, _) = raw(
        app,
        &format!("/rest/stream?{BOB}&id={hunter}"),
        Some("bytes=5000-"),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(
        e.fake.opened.lock().unwrap().is_empty(),
        "backend should not be used"
    );

    let something = id_of("something").await;
    let (status, _, body) = raw(app, &format!("/rest/stream?{BOB}&id={something}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, file_bytes());
    assert_eq!(e.fake.opened.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cover_art_is_resized_and_cached() {
    let e = setup(&[]).await;
    let app = &e.app;
    let v = get(app, "getAlbumList2", BOB, "type=alphabeticalByName").await;
    let cover = v["albumList2"]["album"][0]["coverArt"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(cover.starts_with("al") && cover.contains('-'), "{cover}");

    let (status, h, body) = raw(
        app,
        &format!("/rest/getCoverArt?{BOB}&id={cover}&size=300"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(body, b"image:/thumb/b1:300");
    let cached = std::fs::read_dir(e.tmp.join("covers")).unwrap().count();
    assert_eq!(cached, 1);
    let (_, _, again) = raw(
        app,
        &format!("/rest/getCoverArt?{BOB}&id={cover}&size=300"),
        None,
    )
    .await;
    assert_eq!(again, body);

    // Songs resolve to their album's art; the version suffix is optional.
    let v = get(app, "search3", BOB, "query=hunter").await;
    let song = v["searchResult3"]["song"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, _, body) = raw(app, &format!("/rest/getCoverArt?{BOB}&id={song}"), None).await;
    assert_eq!(body, b"image:/thumb/b2:0");
    let (_, _, body) = raw(
        app,
        &format!("/rest/getCoverArt?{BOB}&id={UNKNOWN_ALBUM}"),
        None,
    )
    .await;
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["subsonic-response"]["error"]["code"], 70);
}

#[tokio::test]
async fn scanning() {
    let e = setup(&[]).await;
    let app = &e.app;
    assert_eq!(code(&get(app, "startScan", BOB, "").await), 50);
    let v = get(app, "startScan", ADMIN, "").await;
    assert_eq!(ok(&v)["scanStatus"]["scanning"], true);
    let v = get(app, "getScanStatus", BOB, "").await;
    assert!(ok(&v)["scanStatus"]["count"].as_i64().unwrap() >= 3);
}

#[tokio::test]
async fn api_responses_are_compressed_and_media_is_not() {
    let e = setup(&[]).await;
    let send = |uri: String| {
        let app = e.app.clone();
        async move {
            let req = axum::http::Request::get(uri)
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(axum::body::Body::empty())
                .unwrap();
            tower::ServiceExt::oneshot(app, req)
                .await
                .unwrap()
                .headers()
                .clone()
        }
    };
    let h = send(format!("/rest/search3?{BOB}&query=")).await;
    assert_eq!(h[header::CONTENT_ENCODING], "gzip");
    let v = get(&e.app, "search3", BOB, "query=hunter").await;
    let hunter = v["searchResult3"]["song"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let h = send(format!("/rest/stream?{BOB}&id={hunter}")).await;
    assert!(!h.contains_key(header::CONTENT_ENCODING));
}

#[tokio::test]
async fn unconfigured_backends_are_hidden_until_they_return() {
    let e = setup(&[]).await;
    let app = &e.app;
    let folders =
        async || get(app, "getMusicFolders", BOB, "").await["musicFolders"]["musicFolder"].clone();
    let before = folders().await;
    let hunter =
        get(app, "search3", BOB, "query=hunter").await["searchResult3"]["song"][0]["id"].clone();

    assert_eq!(e.db.retire_sources(&[]).await.unwrap(), 1);
    assert!(folders().await.as_array().is_none_or(Vec::is_empty));
    let v = get(app, "search3", BOB, "query=hunter").await;
    assert!(
        v["searchResult3"]["song"]
            .as_array()
            .is_none_or(Vec::is_empty)
    );

    // Configured again: the next full sync brings everything back, same ids.
    e.sync.sync_all().await.unwrap();
    assert_eq!(folders().await, before);
    let v = get(app, "search3", BOB, "query=hunter").await;
    assert_eq!(v["searchResult3"]["song"][0]["id"], hunter);
}
