//! End-to-end discovery tests: top songs, similar songs and artists, lyrics.

mod common;

use rsub_core::backend::{
    CreditRecord, CreditRole, LyricLine, LyricsDoc, TagKind, TrackEnrichment,
};
use rsub_testkit::{FakeLibrary, album, artist, track};
use serde_json::Value;

use common::*;

/// Gramatik has an album, a compilation repeating its biggest song, a song
/// without popularity and a guest; GRiZ, which Plex calls similar, has an
/// album too.
async fn discovery_env() -> Env {
    let e = setup(&[]).await;
    let gramatik = artist("ga1", "Gramatik");
    let griz = artist("ga2", "GRiZ");
    let bangerz = album("gb1", &gramatik, "Street Bangerz, Vol. 3");
    let mut coffee = album("gb2", &gramatik, "Coffee Shop Selection");
    coffee.is_compilation = true;
    let rebel = album("gb3", &griz, "Rebellion");
    let pop = |mut t: rsub_core::backend::TrackRecord, n: Option<u32>| {
        t.popularity = n;
        t
    };
    let mut guest = pop(track("gt4", &bangerz, 3, "With a Guest"), Some(500));
    guest.credits.push(CreditRecord {
        remote: None,
        name: "Guest".into(),
        role: CreditRole::Artist,
    });
    let lib = FakeLibrary {
        artists: vec![gramatik, griz],
        albums: vec![bangerz.clone(), coffee.clone(), rebel.clone()],
        tracks: vec![
            pop(track("gt1", &bangerz, 1, "Muy Tranquilo"), Some(155_461)),
            pop(track("gt2", &bangerz, 2, "Dungeon Sound"), Some(101_983)),
            pop(track("gt3", &bangerz, 4, "Obscure"), None),
            guest,
            // More popular than the album copy, but on a compilation.
            pop(track("gt5", &coffee, 1, "Muy Tranquilo"), Some(155_470)),
            pop(track("gt6", &rebel, 1, "Rebellion"), Some(90_000)),
            pop(track("gt7", &rebel, 2, "Bang"), Some(80_000)),
        ],
        enrichment: [(
            "gt1".to_string(),
            TrackEnrichment {
                key: "gt1".into(),
                has_lyrics: true,
                ..Default::default()
            },
        )]
        .into(),
    };
    e.fake.set_library("1", "Music", lib);
    e.sync.sync_all().await.unwrap();
    e.fake
        .similar
        .lock()
        .unwrap()
        .insert("ga1".into(), vec!["ga2".into(), "ga99".into()]);
    e.fake.lyrics.lock().unwrap().insert(
        "gt1".into(),
        vec![LyricsDoc {
            lang: String::new(),
            synced: true,
            display_artist: None,
            display_title: None,
            offset_ms: 0,
            lines: vec![
                LyricLine {
                    start_ms: Some(1_500),
                    text: "Tranquilo".into(),
                },
                LyricLine {
                    start_ms: Some(4_520),
                    text: "Muy tranquilo".into(),
                },
            ],
        }],
    );
    e
}

fn titles(songs: &Value) -> Vec<String> {
    songs
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    format!(
                        "{} [{}]",
                        s["title"].as_str().unwrap(),
                        s["album"].as_str().unwrap()
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Subsonic id of the song with this title on this album.
async fn song(e: &Env, title: &str, album: &str) -> String {
    let v = get(&e.app, "search3", BOB, &format!("query={title}")).await;
    v["searchResult3"]["song"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["album"] == album)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn top_songs_rank_by_popularity() {
    let e = discovery_env().await;
    let app = &e.app;
    // One copy per song, from the artist's own album; no popularity, no entry.
    let v = get(app, "getTopSongs", BOB, "artist=gramatik").await;
    assert_eq!(
        titles(&ok(&v)["topSongs"]["song"]),
        [
            "Muy Tranquilo [Street Bangerz, Vol. 3]",
            "Dungeon Sound [Street Bangerz, Vol. 3]",
            "With a Guest [Street Bangerz, Vol. 3]",
        ]
    );
    let v = get(app, "getTopSongs", BOB, "artist=Gramatik&count=1").await;
    assert_eq!(v["topSongs"]["song"].as_array().unwrap().len(), 1);
    // Virtual artists have top songs too.
    let v = get(app, "getTopSongs", BOB, "artist=GUEST").await;
    assert_eq!(
        titles(&v["topSongs"]["song"]),
        ["With a Guest [Street Bangerz, Vol. 3]"]
    );
    let v = get(app, "getTopSongs", BOB, "artist=Nobody").await;
    assert_eq!(ok(&v)["topSongs"]["song"], serde_json::json!([]));
    let v = get(app, "getTopSongs", BOB, "").await;
    assert_eq!(v["status"], "failed");
}

#[tokio::test]
async fn similar_songs_mix_similar_artists() {
    let e = discovery_env().await;
    let app = &e.app;
    let seed = song(&e, "tranquilo", "Street Bangerz, Vol. 3").await;
    // The seed's artist and GRiZ take turns; the seed itself is left out, and
    // a similar artist missing from the catalog is ignored.
    let v = get(app, "getSimilarSongs2", BOB, &format!("id={seed}&count=4")).await;
    assert_eq!(
        titles(&ok(&v)["similarSongs2"]["song"]),
        [
            "Dungeon Sound [Street Bangerz, Vol. 3]",
            "Rebellion [Rebellion]",
            "With a Guest [Street Bangerz, Vol. 3]",
            "Bang [Rebellion]",
        ]
    );
    // By artist, the artist's best song leads.
    let v = get(app, "search3", BOB, "query=gramatik").await;
    let gramatik = v["searchResult3"]["artist"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let v = get(
        app,
        "getSimilarSongs",
        BOB,
        &format!("id={gramatik}&count=2"),
    )
    .await;
    assert_eq!(
        titles(&ok(&v)["similarSongs"]["song"]),
        [
            "Muy Tranquilo [Street Bangerz, Vol. 3]",
            "Rebellion [Rebellion]"
        ]
    );
    // GRiZ has no similar artists of its own.
    let v = get(app, "search3", BOB, "query=griz").await;
    let griz = v["searchResult3"]["artist"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let v = get(app, "getSimilarSongs2", BOB, &format!("id={griz}")).await;
    assert_eq!(
        titles(&v["similarSongs2"]["song"]),
        ["Rebellion [Rebellion]", "Bang [Rebellion]"]
    );

    let v = get(app, "getArtistInfo2", BOB, &format!("id={gramatik}")).await;
    let similar = &ok(&v)["artistInfo2"]["similarArtist"];
    assert_eq!(similar.as_array().unwrap().len(), 1);
    assert_eq!(similar[0]["id"], griz.as_str());
    assert_eq!(similar[0]["name"], "GRiZ");
}

/// An artist Plex knows no similar artists for, with a collaborator and
/// styled neighbours: one sharing rare styles, others only "Electronic".
async fn fallback_env() -> Env {
    let e = setup(&[]).await;
    let styled = |key: &str, name: &str, styles: &[&str]| {
        let mut a = artist(key, name);
        a.tags = styles
            .iter()
            .map(|s| (TagKind::Style, s.to_string()))
            .collect();
        a
    };
    let sword = styled(
        "fa1",
        "Magic Sword",
        &["Synthwave", "Alternative Dance", "Electronic"],
    );
    let bishop = artist("fa2", "Droid Bishop");
    let chromeo = styled("fa3", "Chromeo", &["Synthwave", "Alternative Dance"]);
    let vini = styled("fa4", "Vini Vici", &["Electronic"]);
    let neelix = styled("fa5", "Neelix", &["Electronic"]);
    let doors = styled("fa6", "The Doors", &["Rock"]);
    let elton = styled("fa7", "Elton John", &["Rock", "Electronic"]);
    let artists = [&sword, &bishop, &chromeo, &vini, &neelix, &doors, &elton];
    let albums: Vec<_> = artists
        .iter()
        .enumerate()
        .map(|(i, a)| album(&format!("fb{i}"), a, &format!("{} LP", a.name)))
        .collect();
    let mut tracks = Vec::new();
    for (i, al) in albums.iter().enumerate() {
        for n in 1..=2 {
            let mut t = track(&format!("ft{i}{n}"), al, n, &format!("{} {n}", al.title));
            t.popularity = Some(1000 - n);
            tracks.push(t);
        }
    }
    // Droid Bishop features on a Magic Sword song.
    tracks[1].credits.push(CreditRecord {
        remote: None,
        name: "Droid Bishop".into(),
        role: CreditRole::Artist,
    });
    let lib = FakeLibrary {
        artists: artists.into_iter().cloned().collect(),
        albums,
        tracks,
        enrichment: Default::default(),
    };
    e.fake.set_library("1", "Music", lib);
    e.sync.sync_all().await.unwrap();
    e
}

#[tokio::test]
async fn similar_artists_fall_back_on_the_catalog() {
    let e = fallback_env().await;
    let app = &e.app;
    let v = get(app, "search3", BOB, "query=magic%20sword").await;
    let sword = v["searchResult3"]["artist"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The collaborator first, then the artist sharing rare styles; sharing
    // only "Electronic" isn't enough.
    let v = get(app, "getArtistInfo2", BOB, &format!("id={sword}")).await;
    let names: Vec<&str> = ok(&v)["artistInfo2"]["similarArtist"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Droid Bishop", "Chromeo"]);

    let v = get(app, "getSimilarSongs2", BOB, &format!("id={sword}&count=5")).await;
    assert_eq!(
        titles(&ok(&v)["similarSongs2"]["song"]),
        [
            "Magic Sword LP 1 [Magic Sword LP]",
            "Droid Bishop LP 1 [Droid Bishop LP]",
            "Chromeo LP 1 [Chromeo LP]",
            "Magic Sword LP 2 [Magic Sword LP]",
            "Droid Bishop LP 2 [Droid Bishop LP]",
        ]
    );
}

#[tokio::test]
async fn lyrics() {
    let e = discovery_env().await;
    let app = &e.app;
    let v = get(app, "getOpenSubsonicExtensions", "f=json", "").await;
    assert!(
        v["openSubsonicExtensions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["name"] == "songLyrics")
    );

    let t1 = song(&e, "tranquilo", "Street Bangerz, Vol. 3").await;
    let v = get(app, "getLyricsBySongId", BOB, &format!("id={t1}")).await;
    let docs = &ok(&v)["lyricsList"]["structuredLyrics"];
    assert_eq!(docs.as_array().unwrap().len(), 1);
    assert_eq!(docs[0]["lang"], "und");
    assert_eq!(docs[0]["synced"], true);
    assert_eq!(docs[0]["line"][1]["start"], 4520);
    assert_eq!(docs[0]["line"][1]["value"], "Muy tranquilo");
    assert_eq!(e.fake.take_calls(), ["lyrics gt1"]);

    // A song the catalog knows has no lyrics costs no backend call.
    let t2 = song(&e, "dungeon", "Street Bangerz, Vol. 3").await;
    let v = get(app, "getLyricsBySongId", BOB, &format!("id={t2}")).await;
    assert_eq!(
        ok(&v)["lyricsList"]["structuredLyrics"],
        serde_json::json!([])
    );
    assert!(e.fake.take_calls().is_empty());

    let v = get(
        app,
        "getLyrics",
        BOB,
        "artist=gramatik&title=muy%20tranquilo",
    )
    .await;
    let l = &ok(&v)["lyrics"];
    assert_eq!(l["artist"], "Gramatik");
    assert_eq!(l["title"], "Muy Tranquilo");
    assert_eq!(l["value"], "Tranquilo\nMuy tranquilo");
    let v = get(app, "getLyrics", BOB, "artist=nobody&title=muy%20tranquilo").await;
    assert!(ok(&v)["lyrics"].get("value").is_none());
}
