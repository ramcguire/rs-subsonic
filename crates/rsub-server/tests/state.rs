//! End-to-end user-state tests: stars, ratings, scrobbles, now playing and
//! playlists, read and written live against the fake backend's account.

mod common;

use rsub_core::Roles;
use rsub_core::backend::{CreditRecord, CreditRole};
use rsub_core::crypto::{Purpose, SecretBox};
use rsub_store::NewUser;

use common::*;

/// Subsonic id of the first song matching `query`.
async fn song(e: &Env, query: &str) -> String {
    let v = get(&e.app, "search3", BOB, &format!("query={query}")).await;
    v["searchResult3"]["song"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn ids(list: &serde_json::Value) -> Vec<String> {
    list.as_array()
        .map(|a| {
            a.iter()
                .map(|x| x["id"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn stars_are_five_star_ratings() {
    let e = setup(&[]).await;
    let app = &e.app;
    let hunter = song(&e, "hunter").await;
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    let (album, artist) = (
        v["song"]["albumId"].as_str().unwrap().to_owned(),
        v["song"]["artistId"].as_str().unwrap().to_owned(),
    );

    let v = get(
        app,
        "star",
        BOB,
        &format!("id={hunter}&albumId={album}&artistId={artist}"),
    )
    .await;
    ok(&v);
    let mut calls = e.fake.take_calls();
    calls.sort();
    assert_eq!(calls, ["rate a2 5", "rate b2 5", "rate t3 5"]);
    let v = get(app, "getStarred2", BOB, "").await;
    let s = &ok(&v)["starred2"];
    assert_eq!(ids(&s["song"]), [hunter.as_str()]);
    assert_eq!(ids(&s["album"]), [album.as_str()]);
    assert_eq!(ids(&s["artist"]), [artist.as_str()]);
    assert!(s["song"][0]["starred"].as_str().unwrap().ends_with('Z'));
    assert_eq!(s["song"][0]["userRating"], 5);
    let v = get(app, "getStarred", BOB, "").await;
    assert_eq!(ids(&ok(&v)["starred"]["album"]), [album.as_str()]);
    let v = get(app, "getAlbum", BOB, &format!("id={album}")).await;
    assert!(v["album"]["starred"].is_string());
    assert!(v["album"]["song"][0]["starred"].is_string());
    let v = get(app, "getMusicDirectory", BOB, &format!("id={artist}")).await;
    assert!(v["directory"]["starred"].is_string());
    let v = get(app, "getAlbumList2", BOB, "type=starred").await;
    assert_eq!(ids(&v["albumList2"]["album"]), [album.as_str()]);
    // Until account linking, every user shares the one Plex account.
    let v = get(app, "getStarred2", ADMIN, "").await;
    assert_eq!(ids(&v["starred2"]["song"]), [hunter.as_str()]);

    // Folder-style clients star albums through `id`. Unstarring clears the
    // rating; a rating below 5 unstars.
    ok(&get(app, "unstar", BOB, &format!("id={hunter}&id={album}")).await);
    ok(&get(app, "setRating", BOB, &format!("id={artist}&rating=3")).await);
    let v = get(app, "getStarred2", BOB, "").await;
    assert!(ids(&v["starred2"]["song"]).is_empty());
    assert!(ids(&v["starred2"]["album"]).is_empty());
    assert!(ids(&v["starred2"]["artist"]).is_empty());
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    assert!(v["song"]["userRating"].is_null());
    assert!(v["song"]["starred"].is_null());

    // Unstarring an item that isn't starred keeps its rating.
    e.fake.take_calls();
    ok(&get(app, "unstar", BOB, &format!("artistId={artist}")).await);
    assert!(e.fake.take_calls().is_empty());
    let v = get(app, "getArtist", BOB, &format!("id={artist}")).await;
    assert!(v["artist"]["starred"].is_null());

    ok(&get(app, "setRating", BOB, &format!("id={album}&rating=4")).await);
    ok(&get(app, "setRating", BOB, &format!("id={hunter}&rating=5")).await);
    let v = get(app, "getAlbum", BOB, &format!("id={album}")).await;
    assert_eq!(v["album"]["userRating"], 4);
    assert!(v["album"]["starred"].is_null());
    assert_eq!(v["album"]["song"][0]["userRating"], 5);
    assert!(
        v["album"]["song"][0]["starred"].is_string(),
        "5 stars is starred"
    );
    let v = get(app, "getAlbumList2", BOB, "type=highest").await;
    assert_eq!(ids(&v["albumList2"]["album"]), [album.as_str()]);
    ok(&get(app, "setRating", BOB, &format!("id={hunter}&rating=0")).await);
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    assert!(v["song"]["userRating"].is_null());

    // Errors.
    assert_eq!(
        code(&get(app, "setRating", BOB, &format!("id={album}&rating=6")).await),
        0
    );
    assert_eq!(
        code(&get(app, "setRating", BOB, "id=tr999&rating=1").await),
        70
    );
    assert_eq!(
        code(&get(app, "setRating", BOB, &format!("id={album}")).await),
        10
    );
    assert_eq!(code(&get(app, "star", BOB, "id=tr999").await), 70);
    assert_eq!(
        code(&get(app, "star", BOB, &format!("albumId={hunter}")).await),
        70
    );
    assert_eq!(code(&get(app, "star", BOB, "").await), 10);
}

#[tokio::test]
async fn virtual_artists_are_rated_locally() {
    let e = setup(&[]).await;
    let mut lib = library();
    lib.tracks[2].credits.push(CreditRecord {
        remote: None,
        name: "Guest".into(),
        role: CreditRole::Artist,
    });
    lib.tracks[2].updated_at = 2_000;
    e.fake.set_library("1", "Music", lib);
    e.sync.sync_all().await.unwrap();
    let app = &e.app;
    let hunter = song(&e, "hunter").await;
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    let guest = ok(&v)["song"]["artists"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "Guest")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    ok(&get(app, "star", BOB, &format!("id={guest}")).await);
    assert!(
        e.fake.take_calls().is_empty(),
        "nothing reaches the backend"
    );
    let v = get(app, "getStarred2", BOB, "").await;
    assert_eq!(ids(&v["starred2"]["artist"]), [guest.as_str()]);
    let v = get(app, "getArtist", BOB, &format!("id={guest}")).await;
    assert!(v["artist"]["starred"].is_string());
    // Local ratings are per user.
    let v = get(app, "getStarred2", ADMIN, "").await;
    assert!(ids(&v["starred2"]["artist"]).is_empty());

    ok(&get(app, "setRating", BOB, &format!("id={guest}&rating=2")).await);
    let v = get(app, "getStarred2", BOB, "").await;
    assert!(ids(&v["starred2"]["artist"]).is_empty());
    ok(&get(app, "star", BOB, &format!("id={guest}")).await);
    ok(&get(app, "unstar", BOB, &format!("id={guest}")).await);
    let v = get(app, "getArtist", BOB, &format!("id={guest}")).await;
    assert!(v["artist"]["starred"].is_null());
}

#[tokio::test]
async fn local_stars_follow_the_artist_id() {
    let e = setup(&[]).await;
    let guest_credit = |remote| CreditRecord {
        remote,
        name: "Guest".into(),
        role: CreditRole::Artist,
    };
    let mut lib = library();
    lib.tracks[2].credits.push(guest_credit(None));
    lib.tracks[2].updated_at = 2_000;
    e.fake.set_library("1", "Music", lib.clone());
    e.sync.sync_all().await.unwrap();
    let app = &e.app;
    let starred = async || ids(&get(app, "getStarred2", BOB, "").await["starred2"]["artist"]);
    let hunter = song(&e, "hunter").await;
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    let guest = ok(&v)["song"]["artists"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "Guest")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&get(app, "star", BOB, &format!("id={guest}")).await);

    // Swept and revived with the same id, it keeps its star.
    let mut plain = library();
    plain.tracks[2].updated_at = 3_000;
    e.fake.set_library("1", "Music", plain);
    e.sync.sync_all().await.unwrap();
    assert!(starred().await.is_empty());
    lib.tracks[2].updated_at = 4_000;
    e.fake.set_library("1", "Music", lib.clone());
    e.sync.sync_all().await.unwrap();
    assert_eq!(starred().await, [guest.as_str()]);

    // Once the backend has the artist, its star is the backend's.
    let real = rsub_testkit::artist("a9", "Guest");
    lib.tracks[2].credits.retain(|c| c.name != "Guest");
    lib.tracks[2]
        .credits
        .push(guest_credit(Some(real.remote.clone())));
    lib.tracks[2].updated_at = 5_000;
    lib.artists.push(real);
    e.fake.set_library("1", "Music", lib);
    e.sync.sync_all().await.unwrap();
    assert!(starred().await.is_empty());
}

/// A regular user reads the shared account but doesn't write to it unless an
/// admin grants the roles.
#[tokio::test]
async fn default_roles_keep_the_shared_account_read_only() {
    let e = setup(&[]).await;
    let secrets = SecretBox::new(&[42; 32]).unwrap();
    e.db.create_user(NewUser {
        username: "carol",
        password_enc: secrets
            .encrypt(Purpose::Password, "carol", b"sesame")
            .unwrap(),
        email: None,
        roles: Roles::USER_DEFAULT,
        max_bitrate: 0,
    })
    .await
    .unwrap();
    const CAROL: &str = "u=carol&p=sesame&f=json";
    let app = &e.app;
    let hunter = song(&e, "hunter").await;
    e.fake.take_calls();

    let v = get(app, "getUser", CAROL, "username=carol").await;
    assert_eq!(ok(&v)["user"]["playlistRole"], false);
    assert_eq!(v["user"]["scrobblingEnabled"], false);
    for (method, params) in [
        ("star", format!("id={hunter}")),
        ("unstar", format!("id={hunter}")),
        ("setRating", format!("id={hunter}&rating=3")),
        ("createPlaylist", format!("name=x&songId={hunter}")),
    ] {
        assert_eq!(
            code(&get(app, method, CAROL, &params).await),
            50,
            "{method}"
        );
    }
    // Scrobbles are accepted so clients don't show errors, but go nowhere.
    ok(&get(app, "scrobble", CAROL, &format!("id={hunter}")).await);
    ok(&get(
        app,
        "scrobble",
        CAROL,
        &format!("id={hunter}&submission=false"),
    )
    .await);
    let v = get(app, "getNowPlaying", CAROL, "").await;
    assert_eq!(ids(&ok(&v)["nowPlaying"]["entry"]), [hunter.as_str()]);
    assert!(
        e.fake.take_calls().is_empty(),
        "nothing reaches the backend"
    );
}

#[tokio::test]
async fn plex_down_fails_writes_but_not_browsing() {
    let e = setup(&[]).await;
    let app = &e.app;
    let hunter = song(&e, "hunter").await;
    *e.fake.fail_state.lock().unwrap() = 1;
    assert_eq!(
        code(&get(app, "star", BOB, &format!("id={hunter}")).await),
        0
    );
    assert!(e.fake.take_calls().is_empty(), "nothing queued to retry");

    *e.fake.fail_state.lock().unwrap() = 1;
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    assert_eq!(ok(&v)["song"]["title"], "Hunter");
    *e.fake.fail_state.lock().unwrap() = 1;
    assert_eq!(code(&get(app, "getStarred2", BOB, "").await), 0);
    *e.fake.fail_state.lock().unwrap() = 1;
    assert_eq!(code(&get(app, "getPlaylists", BOB, "").await), 0);
}

#[tokio::test]
async fn a_hanging_plex_only_delays_browsing() {
    let e = setup(&[]).await;
    let hunter = song(&e, "hunter").await;
    ok(&get(&e.app, "star", BOB, &format!("id={hunter}")).await);
    *e.fake.hang_state.lock().unwrap() = true;
    let started = std::time::Instant::now();
    let v = get(&e.app, "getSong", BOB, &format!("id={hunter}")).await;
    assert_eq!(ok(&v)["song"]["title"], "Hunter");
    assert!(v["song"]["starred"].is_null(), "rendered without state");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[tokio::test]
async fn scrobbles_and_now_playing() {
    let e = setup(&[]).await;
    let app = &e.app;
    let (hunter, something) = (song(&e, "hunter").await, song(&e, "something").await);

    let v = get(
        app,
        "scrobble",
        BOB,
        &format!("id={hunter}&submission=false&c=Feishin"),
    )
    .await;
    ok(&v);
    let v = get(app, "getNowPlaying", ADMIN, "").await;
    let entry = &ok(&v)["nowPlaying"]["entry"];
    assert_eq!(ids(entry), [hunter.as_str()]);
    assert_eq!(entry[0]["username"], "bob");
    assert_eq!(entry[0]["playerName"], "Feishin");
    assert_eq!(entry[0]["minutesAgo"], 0);
    // Now playing alone doesn't count as a play.
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    assert!(v["song"]["playCount"].is_null());

    // `time` is accepted, but Plex dates every play now.
    let v = get(
        app,
        "scrobble",
        BOB,
        &format!("id={hunter}&id={something}&time=1700000000000&time=1700000300000"),
    )
    .await;
    ok(&v);
    ok(&get(app, "scrobble", BOB, &format!("id={hunter}")).await);
    let calls: Vec<_> = e
        .fake
        .take_calls()
        .into_iter()
        .filter(|c| c.starts_with("scrobble"))
        .collect();
    assert_eq!(calls, ["scrobble t3", "scrobble t2", "scrobble t3"]);
    let v = get(app, "getSong", BOB, &format!("id={hunter}")).await;
    assert_eq!(v["song"]["playCount"], 2);
    assert!(v["song"]["played"].is_string());
    let album = v["song"]["albumId"].as_str().unwrap().to_owned();
    let v = get(app, "getAlbum", BOB, &format!("id={album}")).await;
    assert_eq!(v["album"]["playCount"], 2);
    let v = get(app, "getAlbumList2", BOB, "type=recent").await;
    let recent = ids(&v["albumList2"]["album"]);
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0], album, "most recently played first");
    let v = get(app, "getAlbumList2", BOB, "type=frequent").await;
    assert_eq!(ids(&v["albumList2"]["album"])[0], album);
    let v = get(app, "getAlbumList2", BOB, "type=frequent&size=1&offset=1").await;
    assert_eq!(ids(&v["albumList2"]["album"]).len(), 1);
    assert_ne!(ids(&v["albumList2"]["album"])[0], album);

    assert_eq!(code(&get(app, "scrobble", BOB, "id=tr999").await), 70);
    assert_eq!(code(&get(app, "scrobble", BOB, "").await), 10);
    assert_eq!(
        code(&get(app, "scrobble", BOB, &format!("id={hunter}&time=soon")).await),
        0
    );
}

#[tokio::test]
async fn playlists() {
    let e = setup(&[]).await;
    let app = &e.app;
    let (a, b, c) = (
        song(&e, "come+together").await,
        song(&e, "something").await,
        song(&e, "hunter").await,
    );

    let v = get(
        app,
        "createPlaylist",
        BOB,
        &format!("name=Mix&songId={a}&songId={b}"),
    )
    .await;
    let p = &ok(&v)["playlist"];
    assert_eq!(p["name"], "Mix");
    assert_eq!(p["owner"], "bob");
    assert_eq!(p["songCount"], 2);
    assert_eq!(p["duration"], 360);
    assert_eq!(p["public"], false);
    let id = p["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("pl"));
    assert!(p["coverArt"].as_str().unwrap().starts_with(&id));
    assert_eq!(ids(&p["entry"]), [a.clone(), b.clone()]);
    assert_eq!(e.fake.take_calls(), ["create Mix [t1,t2]"]);

    // The one Plex account's playlists, shared by every user.
    let v = get(app, "getPlaylists", BOB, "").await;
    assert_eq!(ids(&ok(&v)["playlists"]["playlist"]), [id.as_str()]);
    assert!(v["playlists"]["playlist"][0]["entry"].is_null());
    let v = get(app, "getPlaylists", ADMIN, "").await;
    assert_eq!(ids(&v["playlists"]["playlist"]), [id.as_str()]);
    assert_eq!(v["playlists"]["playlist"][0]["owner"], "admin");
    assert_eq!(
        code(&get(app, "getPlaylists", BOB, "username=admin").await),
        50
    );
    // The id is minted from the Plex playlist: the same on every read.
    let v = get(app, "getPlaylist", ADMIN, &format!("id={id}")).await;
    assert_eq!(ok(&v)["playlist"]["id"], id.as_str());

    let v = get(
        app,
        "updatePlaylist",
        BOB,
        &format!("playlistId={id}&name=Mix+2&comment=road&songIdToAdd={c}&songIndexToRemove=0"),
    )
    .await;
    ok(&v);
    let v = get(app, "getPlaylist", ADMIN, &format!("id={id}")).await;
    let p = &ok(&v)["playlist"];
    assert_eq!(p["name"], "Mix 2");
    assert_eq!(p["comment"], "road");
    assert_eq!(ids(&p["entry"]), [b.clone(), c.clone()]);

    // createPlaylist with playlistId replaces the songs.
    let v = get(
        app,
        "createPlaylist",
        BOB,
        &format!("playlistId={id}&songId={c}"),
    )
    .await;
    assert_eq!(ids(&ok(&v)["playlist"]["entry"]), [c.as_str()]);

    // A replacement that fails after the clear puts the old songs back.
    *e.fake.fail_adds.lock().unwrap() = 1;
    let v = get(
        app,
        "createPlaylist",
        BOB,
        &format!("playlistId={id}&songId={a}&songId={b}"),
    )
    .await;
    assert_eq!(code(&v), 0);
    let v = get(app, "getPlaylist", BOB, &format!("id={id}")).await;
    assert_eq!(ids(&ok(&v)["playlist"]["entry"]), [c.as_str()]);

    // An empty playlist, as clients create before adding songs.
    let v = get(app, "createPlaylist", BOB, "name=Later").await;
    let later = ok(&v)["playlist"]["id"].as_str().unwrap().to_owned();
    assert_eq!(v["playlist"]["songCount"], 0);
    ok(&get(
        app,
        "updatePlaylist",
        BOB,
        &format!("playlistId={later}&songIdToAdd={a}"),
    )
    .await);
    let v = get(app, "getPlaylist", BOB, &format!("id={later}")).await;
    assert_eq!(ids(&v["playlist"]["entry"]), [a.as_str()]);

    // The playlist's cover comes from the backend.
    let cover = p["coverArt"].as_str().unwrap();
    let (status, _, body) = raw(app, &format!("/rest/getCoverArt?{BOB}&id={cover}"), None).await;
    assert_eq!(status, 200);
    assert!(body.starts_with(b"image:/playlists/"));

    // Smart playlists (Plex's built-in ones) are read-only.
    let smart = e.fake.add_smart_playlist("All Music", &["t1", "t2", "t3"]);
    let v = get(app, "getPlaylists", BOB, "").await;
    let lists = &ok(&v)["playlists"]["playlist"];
    let all = lists
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "All Music")
        .unwrap();
    assert_eq!(all["readonly"], true);
    let all_id = all["id"].as_str().unwrap().to_owned();
    assert!(!all_id.contains(&smart), "ids are opaque");
    let v = get(app, "getPlaylist", BOB, &format!("id={all_id}")).await;
    assert_eq!(ids(&ok(&v)["playlist"]["entry"]).len(), 3);
    assert_eq!(
        code(&get(app, "deletePlaylist", BOB, &format!("id={all_id}")).await),
        50
    );

    ok(&get(app, "deletePlaylist", BOB, &format!("id={id}")).await);
    assert_eq!(
        code(&get(app, "getPlaylist", BOB, &format!("id={id}")).await),
        70
    );

    // Errors.
    assert_eq!(code(&get(app, "createPlaylist", BOB, "").await), 10);
    assert_eq!(
        code(&get(app, "createPlaylist", BOB, "name=x&songId=tr999").await),
        70
    );
    assert_eq!(code(&get(app, "getPlaylist", BOB, "id=pl999").await), 70);
    assert_eq!(
        code(
            &get(
                app,
                "updatePlaylist",
                BOB,
                &format!("playlistId={later}&songIndexToRemove=x")
            )
            .await
        ),
        0
    );
}
