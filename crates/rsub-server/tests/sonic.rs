//! End-to-end sonic similarity: `getSonicSimilarTracks`, `findSonicPath` and
//! the `sonicSimilarity` extension, over hand-written vectors.

mod common;

use rsub_store::{AnalysisWrite, FileToAnalyze, Page};
use rsub_testkit::{album, artist, track};
use serde_json::Value;

use common::*;

/// Row ids by title; a copy of "Come Together" on a best-of is "Come Together
/// (1)".
async fn ids(e: &Env) -> std::collections::HashMap<String, i64> {
    let page = Page::first(100);
    e.db.random_tracks(None, (None, None), page)
        .await
        .unwrap()
        .into_iter()
        .map(|t| {
            let key = if t.remote_key == "t4" {
                format!("{} (1)", t.title)
            } else {
                t.title.clone()
            };
            (key, t.id)
        })
        .collect()
}

/// Store these tracks' vectors (`None`: not analysable).
async fn analyse(e: &Env, vectors: &[(i64, Option<Vec<f32>>)]) {
    let ids: Vec<i64> = vectors.iter().map(|v| v.0).collect();
    let rows = e.db.tracks_by_ids(&ids).await.unwrap();
    for (id, vector) in vectors {
        let t = rows.iter().find(|t| t.id == *id).unwrap();
        let file = FileToAnalyze {
            remote_path: t.remote_path.clone().unwrap(),
            size: 1,
            mtime: 0,
        };
        let w = AnalysisWrite {
            file,
            vector: vector.clone(),
        };
        e.db.write_analysis(t.library_id, SONIC, &[w])
            .await
            .unwrap();
    }
}

async fn sonic_env() -> Env {
    let e = setup_sonic().await;
    let mut lib = library();
    let beatles = artist("a1", "The Beatles");
    let one = album("b9", &beatles, "1");
    lib.albums.push(one.clone());
    lib.tracks.push(track("t4", &one, 1, "Come Together"));
    // A new change stamp, so the sync rewrites the tracks it has.
    for t in &mut lib.tracks {
        t.popularity = Some(100);
        t.updated_at += 1;
    }
    e.fake.set_library("1", "Music", lib);
    e.sync.sync_all().await.unwrap();
    e
}

async fn public_id(e: &Env, title: &str, album: &str) -> String {
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

fn matches(v: &Value) -> Vec<(String, f64)> {
    ok(v)["sonicMatch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["entry"]["title"].as_str().unwrap().to_owned(),
                m["similarity"].as_f64().unwrap(),
            )
        })
        .collect()
}

fn advertised(v: &Value) -> bool {
    v["openSubsonicExtensions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["name"] == "sonicSimilarity")
}

#[tokio::test]
async fn sonic_similarity() {
    let e = sonic_env().await;
    let app = &e.app;
    // Nothing analysed yet: not advertised, and a song has no matches.
    let v = get(app, "getOpenSubsonicExtensions", "f=json", "").await;
    assert!(!advertised(&v));
    let come = public_id(&e, "come", "Abbey Road").await;
    let v = get(app, "getSonicSimilarTracks", BOB, &format!("id={come}")).await;
    assert_eq!(ok(&v)["sonicMatch"].as_array().map_or(0, Vec::len), 0);

    let ids = ids(&e).await;
    let unit = |x: f32| Some(vec![x, (1.0 - x * x).sqrt()]);
    analyse(
        &e,
        &[
            (ids["Come Together"], unit(1.0)),
            (ids["Come Together (1)"], unit(0.999)),
            (ids["Something"], unit(0.9)),
            (ids["Hunter"], unit(0.0)),
        ],
    )
    .await;
    let v = get(app, "getOpenSubsonicExtensions", "f=json", "").await;
    assert!(advertised(&v));

    // Most similar first; the copy on the best-of is left out.
    let v = get(app, "getSonicSimilarTracks", BOB, &format!("id={come}")).await;
    let m = matches(&v);
    let titles: Vec<&str> = m.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(titles, ["Something", "Hunter"]);
    assert!((m[0].1 - 0.9).abs() < 1e-3 && m[1].1 < 0.01);
    let v = get(
        app,
        "getSonicSimilarTracks",
        BOB,
        &format!("id={come}&count=1"),
    )
    .await;
    assert_eq!(matches(&v).len(), 1);

    // Björk has no similar artists, collaborators or styles; the Beatles
    // sound nearest to her most popular analysed song.
    let v = get(app, "search3", BOB, "query=bjork").await;
    let bjork = v["searchResult3"]["artist"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let v = get(app, "getArtistInfo2", BOB, &format!("id={bjork}")).await;
    assert_eq!(
        ok(&v)["artistInfo2"]["similarArtist"][0]["name"],
        "The Beatles"
    );

    // From Come Together to Hunter by way of Something.
    let hunter = public_id(&e, "hunter", "Homogenic").await;
    let v = get(
        app,
        "findSonicPath",
        BOB,
        &format!("startSongId={come}&endSongId={hunter}&count=3"),
    )
    .await;
    let m = matches(&v);
    let titles: Vec<&str> = m.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(titles, ["Come Together", "Something", "Hunter"]);
    assert_eq!(m[0].1, 1.0);

    // A song not analysed has no neighbours; an album isn't a song.
    let dropped = [
        (ids["Come Together"], None),
        (ids["Come Together (1)"], None),
    ];
    analyse(&e, &dropped).await;
    let v = get(app, "getSonicSimilarTracks", BOB, &format!("id={come}")).await;
    assert!(matches(&v).is_empty());
    let v = get(
        app,
        "findSonicPath",
        BOB,
        &format!("startSongId={come}&endSongId={hunter}"),
    )
    .await;
    assert_eq!(v["status"], "failed");
    let v = get(app, "search3", BOB, "query=abbey").await;
    let abbey = v["searchResult3"]["album"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let v = get(app, "getSonicSimilarTracks", BOB, &format!("id={abbey}")).await;
    assert_eq!(code(&v), 70);
}
