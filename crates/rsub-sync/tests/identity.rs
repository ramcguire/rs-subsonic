//! Identity resolution through the ledger: which public ids survive which
//! library changes, driven by the FakeBackend and an in-memory tag reader.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rsub_core::backend::{CreditRecord, CreditRole, TrackRecord};
use rsub_core::tags::FileTags;
use rsub_core::text::IgnoredArticles;
use rsub_core::{Kind, PublicId};
use rsub_store::{Db, Page, store_tests};
use rsub_sync::snapshot::{self, SnapshotError};
use rsub_sync::{SyncEngine, SyncSource};
use rsub_testkit::{FakeBackend, FakeLibrary, FakeTags, album, artist, track};

/// Two artists, two albums, three tracks.
fn library() -> FakeLibrary {
    let beatles = artist("a1", "The Beatles");
    let bjork = artist("a2", "Björk");
    let abbey = album("b1", &beatles, "Abbey Road");
    let homo = album("b2", &bjork, "Homogenic");
    FakeLibrary {
        tracks: vec![
            track("t1", &abbey, 1, "Come Together"),
            track("t2", &abbey, 2, "Something"),
            track("t3", &homo, 1, "Hunter"),
        ],
        artists: vec![beatles, bjork],
        albums: vec![abbey, homo],
        enrichment: Default::default(),
    }
}

fn slug(s: &str) -> String {
    s.to_lowercase().replace(' ', "-")
}

/// Lidarr-style tags for a track: MBIDs derived from its names.
fn tags_of(l: &FakeLibrary, t: &TrackRecord) -> FileTags {
    let album = l
        .albums
        .iter()
        .find(|a| a.remote.key == t.album.key)
        .unwrap();
    let artist = &album.display_artist;
    FileTags {
        release_track_mbid: Some(format!("rt-{}", slug(&t.title))),
        recording_mbid: Some(format!("rec-{}", slug(&t.title))),
        release_mbid: Some(format!("rel-{}", slug(&album.title))),
        artist_mbids: vec![format!("ar-{}", slug(artist))],
        album_artist_mbids: vec![format!("ar-{}", slug(artist))],
        artists: vec![artist.clone()],
        album_artists: vec![artist.clone()],
        title: Some(t.title.clone()),
        album: Some(album.title.clone()),
        disc_no: t.disc_no,
        track_no: t.track_no,
    }
}

/// Tag every track's file, modified at `mtime`.
fn tag_all(reader: &FakeTags, l: &FakeLibrary, mtime: i64) {
    for t in &l.tracks {
        reader.tag(t.remote_path.as_deref().unwrap(), mtime, tags_of(l, t));
    }
}

async fn engine(db: &Db, fake: &Arc<FakeBackend>, reader: &Arc<FakeTags>) -> Arc<SyncEngine> {
    engine_with_snapshot(db, fake, reader, None).await
}

async fn engine_with_snapshot(
    db: &Db,
    fake: &Arc<FakeBackend>,
    reader: &Arc<FakeTags>,
    snapshot: Option<PathBuf>,
) -> Arc<SyncEngine> {
    let id = db.ensure_source("home", "fake").await.unwrap();
    SyncEngine::new(
        db.clone(),
        vec![SyncSource {
            id,
            name: "home".into(),
            catalog: fake.clone(),
            libraries: Vec::new(),
            enrich: false,
            tags: Some(reader.clone()),
        }],
        IgnoredArticles::default(),
        snapshot,
    )
}

/// A new empty directory for snapshot files.
fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rsub-identity-{}",
        PublicId::random(Kind::Playlist).unwrap()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn ledger(db: &Db) -> Vec<rsub_store::LedgerEntry> {
    db.ledger_page(None, 10_000).await.unwrap()
}

async fn export(db: &Db) -> Vec<u8> {
    let mut out = Vec::new();
    snapshot::export(db, &mut out).await.unwrap();
    out
}

fn read_snapshot(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn sync(db: &Db, l: FakeLibrary, reader: &Arc<FakeTags>) {
    let fake = FakeBackend::new();
    fake.set_library("1", "Music", l);
    engine(db, &fake, reader).await.sync_all().await.unwrap();
}

fn all() -> Page {
    Page::first(100)
}

/// Every live catalog item's public id, by name, sorted.
async fn public_ids(db: &Db) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for a in db.search_artists(&[], all()).await.unwrap() {
        v.push((a.name, a.public_id));
    }
    for a in db.search_albums(&[], all()).await.unwrap() {
        v.push((a.title, a.public_id));
    }
    for t in db.search_tracks(&[], all()).await.unwrap() {
        v.push((t.title, t.public_id));
    }
    v.sort();
    v
}

/// Every live track's file with its public id and its album's and album
/// artist's, sorted: shows ids swapped between copies with the same names.
async fn by_path(db: &Db) -> Vec<[String; 4]> {
    let mut v: Vec<[String; 4]> = db
        .search_tracks(&[], all())
        .await
        .unwrap()
        .into_iter()
        .map(|t| {
            [
                t.remote_path.unwrap_or_default(),
                t.public_id,
                t.album_public_id,
                t.album_artist_public_id.unwrap_or_default(),
            ]
        })
        .collect();
    v.sort();
    v
}

/// The public id of the artist credited as "Guest" on "Hunter".
async fn guest_id(db: &Db) -> String {
    let hunter = db
        .search_tracks(&[], all())
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.title == "Hunter")
        .unwrap();
    let credits = db.track_credits(&[hunter.id]).await.unwrap();
    credits
        .into_iter()
        .find(|c| c.name == "Guest")
        .unwrap()
        .artist_public_id
}

async fn track_ids(db: &Db, title: &str) -> Vec<String> {
    let mut ids: Vec<String> = db
        .search_tracks(&[], all())
        .await
        .unwrap()
        .into_iter()
        .filter(|t| t.title == title)
        .map(|t| t.public_id)
        .collect();
    ids.sort();
    ids
}

/// The library after Plex re-adds everything: new ratingKeys, no guids.
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
    l
}

store_tests! {
    mbids_mint_ids(db) {
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        sync(&db, library(), &reader).await;
        let mint = |kind, key: &str| PublicId::mint(kind, key).to_string();
        assert_eq!(track_ids(&db, "Something").await, [mint(Kind::Track, "mb:rt:rt-something")]);
        let ids = public_ids(&db).await;
        let id = |name: &str| ids.iter().find(|(n, _)| n == name).unwrap().1.clone();
        assert_eq!(id("Abbey Road"), mint(Kind::Album, "mb:rel:rel-abbey-road"));
        assert_eq!(id("The Beatles"), mint(Kind::Artist, "mb:ar:ar-the-beatles"));

        // Every key is in the ledger.
        let keys = db.identity_keys(&id("Something")).await.unwrap();
        assert_eq!(
            keys,
            ["mb:rt:rt-something", "rk:home:t2", "tag:the beatles|abbey road|1|2|something"]
        );
    }

    ids_survive_a_rebuilt_database_and_plex(db) {
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        sync(&db, library(), &reader).await;
        let before = public_ids(&db).await;
        assert_eq!(before.len(), 2 + 2 + 3);

        // Both our database and Plex's are rebuilt: only the tags remain.
        let fresh = rsub_store::testing::sqlite().await;
        sync(&fresh, readded(library()), &reader).await;
        assert_eq!(public_ids(&fresh).await, before);
    }

    retag_adding_mbids_keeps_ids(db) {
        let reader = FakeTags::new();
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake, &reader).await;
        // No files reachable yet: ids come from tag keys built from Plex's
        // metadata.
        eng.sync_all().await.unwrap();
        let before = public_ids(&db).await;

        // Lidarr tags everything: the new keys join the existing ids.
        tag_all(&reader, &library(), 1);
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, before);
        let something = &track_ids(&db, "Something").await[0];
        let keys = db.identity_keys(something).await.unwrap();
        assert!(keys.contains(&"mb:rt:rt-something".to_string()), "{keys:?}");

        // Plex then re-adds everything: the MBIDs lead back to the same ids.
        fake.set_library("1", "Music", readded(library()));
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, before);
    }

    fix_match_keeps_ids(db) {
        let mut l = library();
        for a in &mut l.albums {
            a.remote.guid = Some(format!("plex://album/{}", a.remote.key));
        }
        let reader = FakeTags::new();
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = public_ids(&db).await;

        // "Fix Match" gives the album a new guid, and a later re-add a new
        // ratingKey too: its tag key still holds the id.
        l.albums[0].remote.guid = Some("plex://album/other".into());
        l.albums[0].updated_at = 2_000;
        fake.set_library("1", "Music", l.clone());
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, before);
        fake.set_library("1", "Music", readded(l));
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, before);
    }

    quality_upgrade_keeps_ids(db) {
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = track_ids(&db, "Something").await;

        // Lidarr replaces the file: new path, new ratingKey, and Plex picks up
        // a slightly different title. The MBIDs are unchanged.
        let mut l = library();
        let old = l.tracks[1].remote_path.clone().unwrap();
        let t = &mut l.tracks[1];
        t.remote.key = "t2-flac".into();
        t.remote_path = Some("/music/Abbey Road/02 Something.flac".into());
        let tags = tags_of(&library(), &library().tracks[1]);
        t.title = "Something (2019 Mix)".into();
        reader.remove(&old);
        reader.tag(t.remote_path.as_deref().unwrap(), 2, tags);
        fake.set_library("1", "Music", l);
        eng.sync_all().await.unwrap();
        assert_eq!(track_ids(&db, "Something (2019 Mix)").await, before);
        assert!(track_ids(&db, "Something").await.is_empty());
    }

    musicbrainz_merge_keeps_ids(db) {
        let reader = FakeTags::new();
        let l = library();
        tag_all(&reader, &l, 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = public_ids(&db).await;

        // The recording is merged in MusicBrainz and the file retagged.
        let mut tags = tags_of(&l, &l.tracks[1]);
        tags.release_track_mbid = Some("rt-merged".into());
        reader.tag(l.tracks[1].remote_path.as_deref().unwrap(), 2, tags);
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, before);
        let something = &track_ids(&db, "Something").await[0];
        let keys = db.identity_keys(something).await.unwrap();
        assert!(keys.contains(&"mb:rt:rt-merged".to_string()), "{keys:?}");
        // The old MBID still leads to it too.
        assert!(keys.contains(&"mb:rt:rt-something".to_string()), "{keys:?}");
    }

    a_new_copy_does_not_take_existing_ids(db) {
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = by_path(&db).await;

        // A second copy of Abbey Road (same tags, other files) is added, and
        // Plex lists it before the original.
        let mut l = library();
        let beatles = l.artists[0].clone();
        let copy = album("b9", &beatles, "Abbey Road");
        let mut copies = vec![
            track("t8", &copy, 1, "Come Together"),
            track("t9", &copy, 2, "Something"),
        ];
        for (c, orig) in copies.iter_mut().zip(&l.tracks) {
            c.remote_path = Some(format!("/music/copy/{}.mp3", c.title));
            reader.tag(c.remote_path.as_deref().unwrap(), 1, tags_of(&l, orig));
        }
        l.albums.insert(0, copy);
        copies.append(&mut l.tracks);
        l.tracks = copies;
        fake.set_library("1", "Music", l);
        eng.sync_all().await.unwrap();

        let after = by_path(&db).await;
        for item in &before {
            assert!(after.contains(item), "{item:?} lost its id");
        }
        assert_eq!(after.len(), before.len() + 2);
        let ids = track_ids(&db, "Something").await;
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);

        // And it stays that way.
        eng.sync_all().await.unwrap();
        assert_eq!(by_path(&db).await, after);
    }

    duplicates_keep_their_ids_whatever_the_order(db) {
        // Two copies from the start; then Plex lists them the other way round.
        let reader = FakeTags::new();
        let l = library();
        let beatles = l.artists[0].clone();
        let copy = album("b9", &beatles, "Abbey Road");
        let mut t = track("t9", &copy, 2, "Something");
        t.remote_path = Some("/music/copy/Something.mp3".into());
        reader.tag(t.remote_path.as_deref().unwrap(), 1, tags_of(&l, &l.tracks[1]));
        tag_all(&reader, &l, 1);
        let mut both = l.clone();
        both.albums.push(copy);
        both.tracks.push(t);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", both.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = by_path(&db).await;
        assert_eq!(before.len(), 4);

        both.albums.reverse();
        both.tracks.reverse();
        fake.set_library("1", "Music", both);
        eng.sync_all().await.unwrap();
        assert_eq!(by_path(&db).await, before);
    }

    duplicate_survivor_takes_the_id_back(db) {
        // Lidarr upgrades "Something", and Plex lists the new file before the
        // old one is gone: for one sync there are two copies.
        let reader = FakeTags::new();
        let l = library();
        tag_all(&reader, &l, 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let original = track_ids(&db, "Something").await;

        let mut both = l.clone();
        let mut new = both.tracks[1].clone();
        new.remote.key = "t2-flac".into();
        new.remote_path = Some("/music/Abbey Road/02 Something.flac".into());
        reader.tag(new.remote_path.as_deref().unwrap(), 2, tags_of(&l, &l.tracks[1]));
        both.tracks.push(new.clone());
        fake.set_library("1", "Music", both);
        eng.sync_all().await.unwrap();
        let ids = track_ids(&db, "Something").await;
        assert_eq!(ids.len(), 2);
        let stand_in = ids.iter().find(|id| **id != original[0]).unwrap().clone();

        // The old file goes: the new one takes the original id back, and the
        // id it had meanwhile still finds it.
        let mut after = l.clone();
        reader.remove(l.tracks[1].remote_path.as_deref().unwrap());
        after.tracks[1] = new;
        fake.set_library("1", "Music", after);
        eng.sync_all().await.unwrap();
        assert_eq!(track_ids(&db, "Something").await, original);
        let row = db.internal_id(stand_in.parse().unwrap()).await.unwrap().unwrap();
        assert_eq!(db.track(row).await.unwrap().unwrap().public_id, original[0]);
        // Settled: the next sync changes nothing.
        let settled = by_path(&db).await;
        eng.sync_all().await.unwrap();
        assert_eq!(by_path(&db).await, settled);
    }

    duplicates_stay_apart_while_both_exist(db) {
        // Two copies listed side by side keep distinct ids across syncs: the
        // stand-in never takes an id a live copy holds.
        let reader = FakeTags::new();
        let l = library();
        tag_all(&reader, &l, 1);
        let mut both = l.clone();
        let mut copy = both.tracks[1].clone();
        copy.remote.key = "t2-copy".into();
        copy.remote_path = Some("/music/copy/Something.mp3".into());
        reader.tag(copy.remote_path.as_deref().unwrap(), 1, tags_of(&l, &l.tracks[1]));
        both.tracks.push(copy);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", both);
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = by_path(&db).await;
        for _ in 0..2 {
            eng.sync_all().await.unwrap();
            assert_eq!(by_path(&db).await, before);
        }
        let ids = track_ids(&db, "Something").await;
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
    }

    ids_of_gone_items_lead_to_their_successor(db) {
        let reader = FakeTags::new();
        let l = library();
        tag_all(&reader, &l, 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let something = track_ids(&db, "Something").await[0].clone();
        let hunter = track_ids(&db, "Hunter").await[0].clone();

        // "Something" goes, and its release-track MBID turns up on "Hunter"'s
        // file (a MusicBrainz merge): Hunter keeps its id, and Something's id
        // now leads to Hunter instead of nowhere.
        let mut after = l.clone();
        reader.remove(l.tracks[1].remote_path.as_deref().unwrap());
        after.tracks.remove(1);
        let mut tags = tags_of(&l, &l.tracks[2]);
        tags.release_track_mbid = Some("rt-something".into());
        reader.tag(l.tracks[2].remote_path.as_deref().unwrap(), 2, tags);
        fake.set_library("1", "Music", after);
        eng.sync_all().await.unwrap();
        assert_eq!(track_ids(&db, "Hunter").await, std::slice::from_ref(&hunter));
        let row = db.internal_id(something.parse().unwrap()).await.unwrap().unwrap();
        assert_eq!(db.track(row).await.unwrap().unwrap().public_id, hunter);
        // The alias is part of the ledger, so snapshots and exports keep it.
        let alias = format!("id:{something}");
        assert!(ledger(&db).await.iter().any(|e| e.key == alias && e.public_id == hunter));

        // Ids of items gone without a successor stay "not found".
        let come = track_ids(&db, "Come Together").await[0].clone();
        let mut fewer = library();
        fewer.tracks.retain(|t| t.title == "Hunter");
        fake.set_library("1", "Music", fewer);
        eng.sync_all().await.unwrap();
        let row = db.internal_id(come.parse().unwrap()).await.unwrap().unwrap();
        assert!(db.track(row).await.unwrap().is_none());
    }

    untagged_duplicates_keep_their_ids_whatever_the_order(db) {
        // As above without a mount: the copies share every key but their
        // ratingKeys (Plex matched both to the same release), and are listed
        // in different pages.
        let mut l = library();
        let beatles = l.artists[0].clone();
        let mut copy = album("b9", &beatles, "Abbey Road");
        let mut t = track("t9", &copy, 2, "Something");
        t.remote_path = Some("/music/copy/Something.mp3".into());
        t.remote.guid = Some("plex://track/something".into());
        l.tracks[1].remote.guid = t.remote.guid.clone();
        copy.remote.guid = Some("plex://album/abbey-road".into());
        l.albums[0].remote.guid = copy.remote.guid.clone();
        let mut both = l.clone();
        both.albums.push(copy);
        both.tracks.push(t);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", both.clone());
        let eng = engine(&db, &fake, &FakeTags::new()).await;
        eng.sync_all().await.unwrap();
        let before = by_path(&db).await;
        assert_eq!(before.len(), 4);

        both.albums.reverse();
        both.tracks.reverse();
        fake.set_library("1", "Music", both);
        eng.sync_all().await.unwrap();
        assert_eq!(by_path(&db).await, before);
    }

    removed_items_get_their_ids_back(db) {
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = public_ids(&db).await;

        let mut gone = library();
        gone.albums.truncate(1);
        gone.tracks.truncate(2);
        gone.artists.truncate(1);
        fake.set_library("1", "Music", gone);
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await.len(), 1 + 1 + 2);

        // Back months later under new ratingKeys.
        fake.set_library("1", "Music", readded(library()));
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, before);
    }

    virtual_artists_keep_their_ids_when_plex_adds_them(db) {
        // "Guest" is only a track credit, so a virtual artist, with an MBID
        // from the file's paired ARTISTS / MUSICBRAINZ_ARTISTID tags.
        let reader = FakeTags::new();
        let mut l = library();
        let hunter = l.tracks.iter_mut().find(|t| t.title == "Hunter").unwrap();
        hunter.credits.push(CreditRecord { remote: None, name: "Guest".into(), role: CreditRole::Artist });
        let path = hunter.remote_path.clone().unwrap();
        tag_all(&reader, &l, 1);
        let mut tags = tags_of(&l, &l.tracks[2]);
        tags.artists.push("Guest".into());
        tags.artist_mbids.push("ar-guest".into());
        reader.tag(&path, 1, tags);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let guest = guest_id(&db).await;
        assert_eq!(guest, PublicId::mint(Kind::Artist, "mb:ar:ar-guest").to_string());

        // Plex gets an album by Guest, and with it a real artist entry.
        let real = artist("a3", "Guest");
        let own = album("b3", &real, "Guest Album");
        let t = track("t4", &own, 1, "Solo");
        l.artists.push(real);
        l.albums.push(own);
        l.tracks.push(t);
        tag_all(&reader, &l, 1);
        reader.tag(&path, 1, {
            let mut tags = tags_of(&l, &l.tracks[2]);
            tags.artists.push("Guest".into());
            tags.artist_mbids.push("ar-guest".into());
            tags
        });
        fake.set_library("1", "Music", l);
        eng.sync_all().await.unwrap();
        assert_eq!(guest_id(&db).await, guest);
        let ids = public_ids(&db).await;
        assert!(ids.contains(&("Guest".into(), guest)), "{ids:?}");
    }

    virtual_artist_spellings_in_one_page_share_an_artist(db) {
        // Plex credits "Alok, Bhaskar feat. MAGNUS" on one track and "Alok,
        // Bhaskar feat. Magnus" on another in the same page: two names, one
        // name key, so one virtual artist. (The fake pages two tracks at a time.)
        let mut l = library();
        for (title, name) in [("Come Together", "Guest"), ("Something", "GUEST")] {
            let t = l.tracks.iter_mut().find(|t| t.title == title).unwrap();
            t.credits.push(CreditRecord { remote: None, name: name.into(), role: CreditRole::Artist });
        }
        sync(&db, l, &FakeTags::new()).await;
        let tracks = db.search_tracks(&[], all()).await.unwrap();
        let ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
        let mut guests: Vec<String> = db
            .track_credits(&ids)
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.name.eq_ignore_ascii_case("guest"))
            .map(|c| c.artist_public_id)
            .collect();
        guests.dedup();
        assert_eq!(guests.len(), 1, "{guests:?}");
    }

    artists_take_only_their_own_mbid(db) {
        // Plex files ZHU's remix album under Bob Moses, its only album there;
        // the files name ZHU as album artist. Bob Moses must not take ZHU's MBID.
        let mut l = library();
        let bob = artist("a3", "Bob Moses");
        let zhu = artist("a4", "ZHU");
        let desire = album("b3", &bob, "Desire (Remixes)");
        let why = album("b4", &zhu, "Generationwhy");
        l.tracks.push(track("t4", &desire, 1, "Desire"));
        l.tracks.push(track("t5", &why, 1, "Faded"));
        l.artists.extend([bob, zhu]);
        l.albums.extend([desire, why]);
        let reader = FakeTags::new();
        tag_all(&reader, &l, 1);
        let t4 = l.tracks[3].clone();
        let mut tags = tags_of(&l, &t4);
        tags.album_artists = vec!["ZHU".into()];
        tags.album_artist_mbids = vec!["ar-zhu".into()];
        reader.tag(t4.remote_path.as_deref().unwrap(), 1, tags);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        engine(&db, &fake, &reader).await.sync_all().await.unwrap();

        let ids = public_ids(&db).await;
        let id = |name: &str| ids.iter().find(|(n, _)| n == name).unwrap().1.clone();
        assert_eq!(id("ZHU"), PublicId::mint(Kind::Artist, "mb:ar:ar-zhu").to_string());
        let bob_keys = db.identity_keys(&id("Bob Moses")).await.unwrap();
        assert!(!bob_keys.iter().any(|k| k.starts_with("mb:ar:")), "{bob_keys:?}");
    }

    mbids_leave_an_artist_whose_files_dropped_them(db) {
        // Bob Moses's files first carry ZHU's MBID by mistake; a retag fixes
        // them just as ZHU arrives. The MBID moves to ZHU; both keep their ids.
        let mut l = library();
        let bob = artist("a3", "Bob Moses");
        let desire = album("b3", &bob, "Days Gone By");
        l.tracks.push(track("t4", &desire, 1, "Tearing Me Up"));
        l.artists.push(bob);
        l.albums.push(desire);
        let reader = FakeTags::new();
        tag_all(&reader, &l, 1);
        let t4 = l.tracks[3].clone();
        let mut wrong = tags_of(&l, &t4);
        wrong.album_artist_mbids = vec!["ar-zhu".into()];
        reader.tag(t4.remote_path.as_deref().unwrap(), 1, wrong);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let bob_id = PublicId::mint(Kind::Artist, "mb:ar:ar-zhu").to_string();
        assert!(public_ids(&db).await.contains(&("Bob Moses".into(), bob_id.clone())));

        let zhu = artist("a4", "ZHU");
        let why = album("b4", &zhu, "Generationwhy");
        l.tracks.push(track("t5", &why, 1, "Faded"));
        l.artists.push(zhu);
        l.albums.push(why);
        tag_all(&reader, &l, 2);
        fake.set_library("1", "Music", l);
        eng.sync_all().await.unwrap();
        let ids = public_ids(&db).await;
        let id = |name: &str| ids.iter().find(|(n, _)| n == name).unwrap().1.clone();
        assert_eq!(id("Bob Moses"), bob_id);
        let zhu_keys = db.identity_keys(&id("ZHU")).await.unwrap();
        assert!(zhu_keys.contains(&"mb:ar:ar-zhu".to_owned()), "{zhu_keys:?}");
        let bob_keys = db.identity_keys(&bob_id).await.unwrap();
        assert!(bob_keys.contains(&"mb:ar:ar-bob-moses".to_owned()), "{bob_keys:?}");
        assert!(!bob_keys.contains(&"mb:ar:ar-zhu".to_owned()), "{bob_keys:?}");
    }

    plex_track_guids_are_not_keys(db) {
        // Plex gives a single and the album that includes it one track guid
        // (it names the song). Album guids are keys; track guids aren't.
        let mut l = library();
        let beatles = l.artists[0].clone();
        let mut single = album("b3", &beatles, "Something (Single)");
        single.remote.guid = Some("plex://album/single".into());
        let mut t = track("t4", &single, 1, "Something");
        t.remote.guid = Some("plex://track/something".into());
        l.tracks[1].remote.guid = t.remote.guid.clone();
        l.albums.push(single);
        l.tracks.push(t);
        sync(&db, l, &FakeTags::new()).await;
        let ids = track_ids(&db, "Something").await;
        assert_eq!(ids.len(), 2);
        for id in &ids {
            let keys = db.identity_keys(id).await.unwrap();
            assert!(!keys.iter().any(|k| k.starts_with("plex:")), "{keys:?}");
        }
        let albums = db.search_albums(&[], all()).await.unwrap();
        let single = albums.iter().find(|a| a.title == "Something (Single)").unwrap();
        assert_eq!(single.public_id, PublicId::mint(Kind::Album, "plex:plex://album/single").to_string());
    }

    rebuilds_leave_contested_keys_where_they_were(db) {
        // "Zhu" is tagged first and takes the MBID; "ZHU" arrives later with
        // the same MBID and name key, so it gets another id. After a rebuild
        // from the snapshot, "ZHU" sorts first: the MBID must stay with "Zhu".
        let mut l = library();
        let zhu = artist("a3", "Zhu");
        let one = album("b3", &zhu, "One");
        l.tracks.push(track("t4", &one, 1, "First"));
        l.artists.push(zhu);
        l.albums.push(one);
        let reader = FakeTags::new();
        tag_all(&reader, &l, 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let upper = artist("a4", "ZHU");
        let two = album("b4", &upper, "Two");
        l.tracks.push(track("t5", &two, 1, "Second"));
        l.artists.push(upper);
        l.albums.push(two);
        tag_all(&reader, &l, 1);
        let t5 = l.tracks[4].clone();
        let mut tags = tags_of(&l, &t5);
        tags.album_artist_mbids = vec!["ar-zhu".into()];
        reader.tag(t5.remote_path.as_deref().unwrap(), 1, tags);
        fake.set_library("1", "Music", l.clone());
        eng.sync_all().await.unwrap();
        let keys = |entries: Vec<rsub_store::LedgerEntry>| -> Vec<(String, String)> {
            entries.into_iter().map(|e| (e.key, e.public_id)).collect()
        };
        let before = keys(ledger(&db).await);
        let ids = public_ids(&db).await;

        let fresh = rsub_store::testing::sqlite().await;
        snapshot::import(&fresh, export(&db).await.as_slice()).await.unwrap();
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l);
        engine(&fresh, &fake, &reader).await.sync_all().await.unwrap();
        assert_eq!(public_ids(&fresh).await, ids);
        assert_eq!(keys(ledger(&fresh).await), before);
    }

    credits_come_from_artists_tags(db) {
        // Plex credits "Hunter" to "Björk x Guest feat. Other" as one string;
        // the file's ARTISTS tag names each artist.
        let mut l = library();
        let hunter = l.tracks.iter_mut().find(|t| t.title == "Hunter").unwrap();
        hunter.display_artist = "Björk x Guest feat. Other".into();
        hunter.credits[0] = CreditRecord { remote: None, name: hunter.display_artist.clone(), role: CreditRole::Artist };
        let hunter = hunter.clone();
        let reader = FakeTags::new();
        tag_all(&reader, &l, 1);
        let mut tags = tags_of(&l, &hunter);
        tags.artists = vec!["Björk".into(), "Guest".into(), "Other".into()];
        tags.artist_mbids = vec!["ar-bjork".into(), "ar-guest".into(), "ar-other".into()];
        reader.tag(hunter.remote_path.as_deref().unwrap(), 1, tags);
        // "Something" has no tags: Plex's credit stands.
        let something = l.tracks.iter_mut().find(|t| t.title == "Something").unwrap();
        something.display_artist = "The Beatles & Friends".into();
        something.credits[0] = CreditRecord { remote: None, name: something.display_artist.clone(), role: CreditRole::Artist };
        reader.remove(something.remote_path.as_deref().unwrap());
        sync(&db, l, &reader).await;

        let tracks = db.search_tracks(&[], all()).await.unwrap();
        let credited = |title: &str| {
            let t = tracks.iter().find(|t| t.title == title).unwrap();
            let db = db.clone();
            let id = t.id;
            async move {
                db.track_credits(&[id]).await.unwrap().into_iter()
                    .filter(|c| c.role == "artist")
                    .map(|c| (c.name, c.artist_public_id))
                    .collect::<Vec<_>>()
            }
        };
        let hunter = credited("Hunter").await;
        let names: Vec<&str> = hunter.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Björk", "Guest", "Other"]);
        // Björk is the real artist; the others are virtual artists with MBIDs.
        let ids = public_ids(&db).await;
        assert!(ids.contains(&("Björk".into(), hunter[0].1.clone())), "{ids:?}");
        assert_eq!(hunter[1].1, PublicId::mint(Kind::Artist, "mb:ar:ar-guest").to_string());
        let names: Vec<String> = credited("Something").await.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["The Beatles & Friends"]);
    }

    snapshot_restores_ids_into_a_new_database(db) {
        let dir = temp_dir();
        let path = dir.join(snapshot::SNAPSHOT_FILE);
        let reader = FakeTags::new();
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine_with_snapshot(&db, &fake, &reader, Some(path.clone())).await;
        // Ids minted from tag keys, then Lidarr tags everything: the ids'
        // strongest keys are now MBIDs they weren't minted from.
        eng.sync_all().await.unwrap();
        tag_all(&reader, &library(), 1);
        eng.sync_all().await.unwrap();
        let before = public_ids(&db).await;
        let lines = read_snapshot(&path);
        assert_eq!(lines[0], r#"{"rsub_identity":1}"#);
        assert_eq!(lines.len() - 1, ledger(&db).await.len());

        // Both databases are rebuilt. Without the snapshot, the MBIDs mint
        // new ids.
        let bare = rsub_store::testing::sqlite().await;
        sync(&bare, readded(library()), &reader).await;
        assert_ne!(public_ids(&bare).await, before);

        // With it, every id comes back.
        let fresh = rsub_store::testing::sqlite().await;
        let restored = snapshot::restore_snapshot(&fresh, &path).await.unwrap().unwrap();
        assert_eq!(restored.read, (lines.len() - 1) as u64);
        assert_eq!(restored.added, restored.read);
        sync(&fresh, readded(library()), &reader).await;
        assert_eq!(public_ids(&fresh).await, before);

        // A ledger that isn't empty is left alone.
        assert_eq!(snapshot::restore_snapshot(&fresh, &path).await.unwrap(), None);
        // So is a missing snapshot.
        let empty = rsub_store::testing::sqlite().await;
        assert_eq!(snapshot::restore_snapshot(&empty, &dir.join("none.jsonl")).await.unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    ledger_export_and_import_round_trip(db) {
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        sync(&db, library(), &reader).await;
        let entries = ledger(&db).await;
        let bytes = export(&db).await;
        // Paging picks up where the last page ended, across kinds.
        let mut paged = Vec::new();
        loop {
            let after = paged.last().map(|e: &rsub_store::LedgerEntry| (e.kind.clone(), e.key.clone()));
            let page = db
                .ledger_page(after.as_ref().map(|(k, v)| (k.as_str(), v.as_str())), 2)
                .await
                .unwrap();
            if page.is_empty() {
                break;
            }
            paged.extend(page);
        }
        assert_eq!(paged, entries);
        assert!(entries.iter().any(|e| e.kind == "artist") && entries.iter().any(|e| e.kind == "track"));

        let other = rsub_store::testing::sqlite().await;
        let n = entries.len() as u64;
        let imported = snapshot::import(&other, bytes.as_slice()).await.unwrap();
        assert_eq!((imported.read, imported.added), (n, n));
        assert_eq!(ledger(&other).await, entries);
        assert_eq!(export(&other).await, bytes);
        // Importing again adds nothing.
        let again = snapshot::import(&other, bytes.as_slice()).await.unwrap();
        assert_eq!((again.read, again.added), (n, 0));
    }

    ledger_import_refuses_bad_snapshots(db) {
        let header = r#"{"rsub_identity":1}"#;
        let entry = |kind: &str, id: &str| {
            format!(r#"{{"kind":"{kind}","key":"mb:rt:x","public_id":"{id}","first_seen_at":1,"last_seen_at":2}}"#)
        };
        let track = PublicId::mint(Kind::Track, "mb:rt:x").to_string();
        let album = PublicId::mint(Kind::Album, "mb:rt:x").to_string();
        let bad = [
            // Empty, not JSON, an unknown version, no header.
            String::new(),
            "not json".into(),
            r#"{"rsub_identity":2}"#.into(),
            entry("track", &track),
            // A truncated last line.
            format!("{header}\n{}\n{{", entry("track", &track)),
            // An album id for a track key, an unknown kind, a malformed id.
            format!("{header}\n{}\n{}", entry("track", &track), entry("track", &album)),
            format!("{header}\n{}", entry("playlist", &track)),
            format!("{header}\n{}", entry("track", "tr123")),
        ];
        for input in &bad {
            let err = snapshot::import(&db, input.as_bytes()).await.unwrap_err();
            assert!(
                matches!(err, SnapshotError::Format { .. } | SnapshotError::Store(_)),
                "{input}: {err}"
            );
            // Nothing is imported from a bad snapshot.
            assert!(db.ledger_is_empty().await.unwrap(), "{input}");
        }
        // Blank lines are skipped.
        let good = format!("{header}\n\n{}\n", entry("track", &track));
        let imported = snapshot::import(&db, good.as_bytes()).await.unwrap();
        assert_eq!((imported.read, imported.added), (1, 1));
    }

    unmatched_local_items_keep_their_ids(db) {
        // Nothing is matched in Plex and nothing is tagged: every guid is a
        // `local://` one, which changes on re-add.
        let local = |mut l: FakeLibrary, n: &str| {
            let guid = |r: &mut rsub_core::backend::RemoteRef| {
                r.guid = Some(format!("local://{n}{}", r.key));
            };
            l.artists.iter_mut().for_each(|a| guid(&mut a.remote));
            l.albums.iter_mut().for_each(|a| guid(&mut a.remote));
            l.tracks.iter_mut().for_each(|t| guid(&mut t.remote));
            l
        };
        let reader = FakeTags::new();
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", local(library(), "1"));
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = public_ids(&db).await;
        assert_eq!(before.len(), 2 + 2 + 3);

        // The guid is not a key: the id is minted from the tag key.
        let something = &track_ids(&db, "Something").await[0];
        let tag = "tag:the beatles|abbey road|1|2|something";
        assert_eq!(*something, PublicId::mint(Kind::Track, tag).to_string());
        assert_eq!(db.identity_keys(something).await.unwrap(), ["rk:home:t2", tag]);

        // Re-added: new ratingKeys and new `local://` guids.
        fake.set_library("1", "Music", local(readded(library()), "2"));
        eng.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, before);
    }

    hash_collisions_mint_from_the_next_key(db) {
        // No real collision of 80-bit hashes can be found, so the ledger is
        // seeded with other keys holding the ids "Something" would mint.
        let mb = "mb:rt:rt-something";
        let tag = "tag:the beatles|abbey road|1|2|something";
        let mint = |key: &str| PublicId::mint(Kind::Track, key).to_string();
        let squat = |key: &str, public_id: String| rsub_store::LedgerEntry {
            kind: "track".into(),
            key: key.into(),
            public_id,
            first_seen_at: 1,
            last_seen_at: 1,
        };
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);

        // Its MBID's id is taken: the tag key mints it.
        db.import_ledger(&[squat("mb:rt:other", mint(mb))]).await.unwrap();
        sync(&db, library(), &reader).await;
        assert_eq!(track_ids(&db, "Something").await, [mint(tag)]);
        // The other key keeps its id, and a later sync keeps both.
        assert_eq!(db.identity_keys(&mint(mb)).await.unwrap(), ["mb:rt:other"]);
        sync(&db, library(), &reader).await;
        assert_eq!(track_ids(&db, "Something").await, [mint(tag)]);

        // Both stable keys' ids are taken: a duplicate of the MBID is minted.
        let other = rsub_store::testing::sqlite().await;
        other
            .import_ledger(&[squat("mb:rt:other", mint(mb)), squat("tag:other", mint(tag))])
            .await
            .unwrap();
        sync(&other, library(), &reader).await;
        assert_eq!(track_ids(&other, "Something").await, [mint(&format!("{mb}#2"))]);
        // The rest of the library is unaffected.
        assert_eq!(track_ids(&other, "Hunter").await, [mint("mb:rt:rt-hunter")]);
    }

    changes_get_the_ids_a_full_sync_would(db) {
        // Homogenic arrives after the first sync. Synced incrementally, it
        // gets the ids two full syncs give.
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        let mut first = library();
        first.albums.retain(|a| a.title != "Homogenic");
        first.tracks.retain(|t| t.title != "Hunter");
        let mut l = library();
        for a in l.albums.iter_mut().filter(|a| a.title == "Homogenic") {
            (a.added_at, a.updated_at) = (90_000, 90_000);
        }
        for t in l.tracks.iter_mut().filter(|t| t.title == "Hunter") {
            (t.added_at, t.updated_at) = (90_000, 90_000);
        }
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", first.clone());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        fake.set_library("1", "Music", l.clone());
        eng.check_changes().await.unwrap();
        eng.check_changes().await.unwrap();
        assert_eq!(fake.take_scans(), ["full 1", "changes 1 -59000"]);

        let fresh = rsub_store::testing::sqlite().await;
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", first);
        let full = engine(&fresh, &fake, &reader).await;
        full.sync_all().await.unwrap();
        fake.set_library("1", "Music", l);
        full.sync_all().await.unwrap();
        assert_eq!(public_ids(&db).await, public_ids(&fresh).await);
    }

    contested_ids_wait_for_a_full_sync(db) {
        let reader = FakeTags::new();
        tag_all(&reader, &library(), 1);
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake, &reader).await;
        eng.sync_all().await.unwrap();
        let before = track_ids(&db, "Something").await;
        fake.take_scans();

        // Lidarr replaces a file: a new item with the old one's MBIDs. An
        // incremental pass can't tell whether the old item is gone, so a full
        // sync settles who keeps the id.
        let mut l = library();
        let old = l.tracks[1].remote_path.clone().unwrap();
        let tags = tags_of(&library(), &library().tracks[1]);
        let t = &mut l.tracks[1];
        t.remote.key = "t2-flac".into();
        t.remote_path = Some("/music/Abbey Road/02 Something.flac".into());
        (t.added_at, t.updated_at) = (90_000, 90_000);
        reader.remove(&old);
        reader.tag(t.remote_path.as_deref().unwrap(), 2, tags);
        fake.set_library("1", "Music", l);
        eng.check_changes().await.unwrap();
        eng.check_changes().await.unwrap();
        assert_eq!(fake.take_scans(), ["changes 1 -59000", "full 1"]);
        assert_eq!(track_ids(&db, "Something").await, before);
        assert_eq!(db.track_count().await.unwrap(), 3);
    }
}
