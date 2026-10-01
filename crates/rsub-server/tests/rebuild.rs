//! Ids as a client sees them survive rebuilding the database from scratch: no
//! ledger, no snapshot, only a new sync. Every endpoint that returns ids is
//! crawled on two independently built databases and the results compared, with
//! the file tags of a mounted library and without, and with Plex unchanged or
//! rebuilt (new ratingKeys, no guids, items listed in another order).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Router;
use rsub_core::backend::TrackRecord;
use rsub_core::tags::FileTags;
use rsub_testkit::{FakeLibrary, FakeTags, track};
use serde_json::Value;

use common::*;

/// Fields holding ids.
const ID_FIELDS: [&str; 5] = ["id", "artistId", "albumId", "coverArt", "parent"];

/// The track whose `ARTISTS` tag also credits a guest, a virtual artist.
const FEATURING: &str = "Joga";

/// Two albums, one with a second disc.
fn catalog() -> FakeLibrary {
    let mut lib = library();
    let homo = lib.albums[1].clone();
    let mut bachelorette = track("t4", &homo, 2, "Bachelorette");
    bachelorette.disc_no = Some(2);
    lib.tracks
        .extend([bachelorette, track("t5", &homo, 3, FEATURING)]);
    lib
}

/// Plex rebuilt: every item re-added under a new ratingKey, without guids, and
/// listed in reverse.
fn readded(mut l: FakeLibrary) -> FakeLibrary {
    let rekey = |r: &mut rsub_core::backend::RemoteRef| {
        r.key = format!("new-{}", r.key);
        r.guid = None;
    };
    l.artists.iter_mut().for_each(|a| rekey(&mut a.remote));
    for a in &mut l.albums {
        rekey(&mut a.remote);
        a.artist.iter_mut().for_each(rekey);
    }
    for t in &mut l.tracks {
        rekey(&mut t.remote);
        rekey(&mut t.album);
        t.credits
            .iter_mut()
            .flat_map(|c| c.remote.as_mut())
            .for_each(rekey);
    }
    l.artists.reverse();
    l.albums.reverse();
    l.tracks.reverse();
    l
}

fn slug(s: &str) -> String {
    s.to_lowercase().replace(' ', "-")
}

/// Lidarr-style tags: MBIDs derived from names, and the guest in `ARTISTS`.
fn tags(l: &FakeLibrary) -> Arc<FakeTags> {
    let reader = FakeTags::new();
    for t in &l.tracks {
        reader.tag(t.remote_path.as_deref().unwrap(), 1, tags_of(l, t));
    }
    reader
}

fn tags_of(l: &FakeLibrary, t: &TrackRecord) -> FileTags {
    let album = l
        .albums
        .iter()
        .find(|a| a.remote.key == t.album.key)
        .unwrap();
    let lead = album.display_artist.clone();
    let mut artists = vec![lead.clone()];
    if t.title == FEATURING {
        artists.push("Guest Singer".into());
    }
    FileTags {
        release_track_mbid: Some(format!("rt-{}", slug(&t.title))),
        recording_mbid: Some(format!("rec-{}", slug(&t.title))),
        release_mbid: Some(format!("rel-{}", slug(&album.title))),
        artist_mbids: artists.iter().map(|a| format!("ar-{}", slug(a))).collect(),
        album_artist_mbids: vec![format!("ar-{}", slug(&lead))],
        artists,
        album_artists: vec![lead],
        title: Some(t.title.clone()),
        album: Some(album.title.clone()),
        disc_no: t.disc_no,
        track_no: t.track_no,
    }
}

async fn build(lib: FakeLibrary, tagged: bool) -> Env {
    let tags = tagged.then(|| tags(&lib));
    let keys: Vec<String> = lib
        .tracks
        .iter()
        .filter(|t| ["Come Together", "Hunter"].contains(&t.title.as_str()))
        .map(|t| t.remote.key.clone())
        .collect();
    let e = setup_library(lib, tags).await;
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    e.fake.add_smart_playlist("Favourites", &keys);
    e
}

/// Every id the crawl saw, keyed by where: endpoint, the item's name or
/// title (with its album and disc), and the field.
#[derive(Default)]
struct Ids(BTreeMap<String, String>);

impl Ids {
    fn walk(&mut self, endpoint: &str, v: &Value) {
        match v {
            Value::Array(items) => items.iter().for_each(|x| self.walk(endpoint, x)),
            Value::Object(o) => {
                let label: Vec<&str> = ["name", "title", "album", "discNumber"]
                    .iter()
                    .filter_map(|k| o.get(*k))
                    .map(|x| x.as_str().unwrap_or("#"))
                    .collect();
                let disc = o.get("discNumber").map(Value::to_string);
                for field in ID_FIELDS {
                    // Music folder ids are numbers.
                    let id = match o.get(field) {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Number(n)) => n.to_string(),
                        _ => continue,
                    };
                    let key = format!("{endpoint} {} {disc:?} .{field}", label.join(" / "));
                    if let Some(old) = self.0.insert(key.clone(), id.clone()) {
                        assert_eq!(old, id, "{key}: two ids in one database");
                    }
                }
                o.values().for_each(|x| self.walk(endpoint, x));
            }
            _ => {}
        }
    }

    /// Only these fields' ids.
    fn only(&self, field: &str) -> BTreeSet<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k.ends_with(&format!(".{field}")))
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// The objects under `key` anywhere in `v`.
fn all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Array(items) => items.iter().for_each(|x| all(x, key, out)),
        Value::Object(o) => {
            if let Some(x) = o.get(key) {
                match x {
                    Value::Array(items) => out.extend(items),
                    x => out.push(x),
                }
            }
            o.values().for_each(|x| all(x, key, out));
        }
        _ => {}
    }
}

fn ids_under(v: &Value, key: &str) -> Vec<String> {
    let mut found = Vec::new();
    all(v, key, &mut found);
    found
        .iter()
        .filter_map(|x| x["id"].as_str().map(str::to_owned))
        .collect()
}

async fn call(app: &Router, ids: &mut Ids, method: &str, params: &str) -> Value {
    let v = ok(&get(app, method, BOB, params).await).clone();
    ids.walk(method, &v);
    v
}

/// Crawl every endpoint that hands out ids.
async fn crawl(app: &Router) -> Ids {
    let mut ids = Ids::default();
    call(app, &mut ids, "getMusicFolders", "").await;
    call(app, &mut ids, "getGenres", "").await;
    call(
        app,
        &mut ids,
        "search3",
        "query=&artistCount=500&albumCount=500&songCount=500",
    )
    .await;
    call(
        app,
        &mut ids,
        "getAlbumList2",
        "type=alphabeticalByName&size=500",
    )
    .await;
    call(app, &mut ids, "getSongsByGenre", "genre=Rock&count=500").await;

    let v = call(app, &mut ids, "getArtists", "").await;
    let mut albums = BTreeSet::new();
    for id in ids_under(&v, "artist") {
        let v = call(app, &mut ids, "getArtist", &format!("id={id}")).await;
        albums.extend(ids_under(&v, "album"));
        call(app, &mut ids, "getArtistInfo2", &format!("id={id}")).await;
    }
    let mut songs = BTreeSet::new();
    for id in &albums {
        let v = call(app, &mut ids, "getAlbum", &format!("id={id}")).await;
        songs.extend(ids_under(&v, "song"));
        call(app, &mut ids, "getAlbumInfo2", &format!("id={id}")).await;
    }
    for id in &songs {
        call(app, &mut ids, "getSong", &format!("id={id}")).await;
    }

    // Folder browsing, from the indexes down.
    let v = call(app, &mut ids, "getIndexes", "").await;
    let mut dirs: Vec<String> = ids_under(&v, "artist");
    let mut seen = BTreeSet::new();
    while let Some(id) = dirs.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let v = call(app, &mut ids, "getMusicDirectory", &format!("id={id}")).await;
        let mut children = Vec::new();
        all(&v, "child", &mut children);
        dirs.extend(
            children
                .iter()
                .filter(|c| c["isDir"] == true)
                .filter_map(|c| c["id"].as_str().map(str::to_owned)),
        );
    }

    let v = call(app, &mut ids, "getPlaylists", "").await;
    for id in ids_under(&v, "playlist") {
        call(app, &mut ids, "getPlaylist", &format!("id={id}")).await;
    }
    ids
}

/// The ids clients see in a database built from `a`, and in a new one built
/// from `b`, match.
async fn same_ids(a: FakeLibrary, b: FakeLibrary, tagged: bool) -> Ids {
    let first = crawl(&build(a, tagged).await.app).await;
    let second = crawl(&build(b, tagged).await.app).await;
    let (x, y) = (&first.0, &second.0);
    let only_first: Vec<_> = x.iter().filter(|(k, v)| y.get(*k) != Some(v)).collect();
    let only_second: Vec<_> = y.iter().filter(|(k, v)| x.get(*k) != Some(v)).collect();
    assert!(
        only_first.is_empty() && only_second.is_empty(),
        "ids differ after a rebuild:\nbefore: {only_first:#?}\nafter: {only_second:#?}"
    );
    first
}

/// The crawl reached every kind of item: `ids` of them at least.
fn covers_everything(ids: &Ids, count: usize) {
    let all = ids.only("id");
    for prefix in ["ar", "al", "tr", "pl"] {
        assert!(
            all.iter().any(|id| id.starts_with(prefix)),
            "no {prefix} ids in {all:?}"
        );
    }
    assert!(all.len() >= count, "{all:?}");
    assert!(!ids.only("coverArt").is_empty());
}

/// Items with an id: 2 artists, 2 albums, 5 songs and a playlist, and with
/// tags, the guest.
const UNTAGGED: usize = 10;
const TAGGED: usize = UNTAGGED + 1;

/// The comparison notices an id that changes: without tags an album's id
/// comes from its artist, title and year.
#[tokio::test]
#[should_panic(expected = "ids differ after a rebuild")]
async fn a_changed_id_is_noticed() {
    let mut changed = catalog();
    changed.albums[1].year = Some(1998);
    same_ids(catalog(), changed, false).await;
}

#[tokio::test]
async fn a_rebuilt_database_hands_out_the_same_ids() {
    let ids = same_ids(catalog(), catalog(), true).await;
    covers_everything(&ids, TAGGED);
    assert!(
        ids.0.keys().any(|k| k.contains("Guest Singer")),
        "the guest is a virtual artist"
    );
}

#[tokio::test]
async fn a_rebuilt_database_hands_out_the_same_ids_without_tags() {
    covers_everything(&same_ids(catalog(), catalog(), false).await, UNTAGGED);
}

#[tokio::test]
async fn rebuilt_databases_of_ours_and_plex_hand_out_the_same_ids() {
    // The fake gives the playlist the same Plex id again; a real rebuilt Plex
    // would give it a new one, and so a new id here.
    let ids = same_ids(catalog(), readded(catalog()), true).await;
    covers_everything(&ids, TAGGED);
}

#[tokio::test]
async fn rebuilt_databases_of_ours_and_plex_hand_out_the_same_ids_without_tags() {
    covers_everything(
        &same_ids(catalog(), readded(catalog()), false).await,
        UNTAGGED,
    );
}
