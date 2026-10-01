//! Sync engine + catalog store, driven by the in-memory FakeBackend.

use std::sync::Arc;

use rsub_core::backend::{CreditRecord, CreditRole, ReplayGain, TagKind, TrackEnrichment};
use rsub_core::text::IgnoredArticles;
use rsub_core::{Kind, PublicId};
use rsub_store::{AlbumOrder, Db, Page, store_tests};
use rsub_sync::{SyncEngine, SyncSource};
use rsub_testkit::{FakeBackend, FakeLibrary, album, artist, track};

/// Two artists, three albums, five tracks.
fn library() -> FakeLibrary {
    let beatles = artist("a1", "The Beatles");
    let bjork = artist("a2", "Björk");
    let mut abbey = album("b1", &beatles, "Abbey Road");
    abbey.year = Some(1969);
    abbey.tags = vec![(TagKind::Genre, "Rock".into())];
    let mut help = album("b2", &beatles, "Help!");
    help.year = Some(1965);
    help.added_at = 5_000;
    help.tags = vec![
        (TagKind::Genre, "Rock".into()),
        (TagKind::Genre, "Pop".into()),
    ];
    let mut homo = album("b3", &bjork, "Homogenic");
    homo.year = Some(1997);
    homo.tags = vec![(TagKind::Genre, "Electronic".into())];
    let tracks = vec![
        track("t1", &abbey, 1, "Come Together"),
        track("t2", &abbey, 2, "Something"),
        track("t3", &help, 1, "Help!"),
        track("t4", &homo, 1, "Hunter"),
        track("t5", &homo, 2, "Jóga"),
    ];
    FakeLibrary {
        artists: vec![beatles, bjork],
        albums: vec![abbey, help, homo],
        tracks,
        enrichment: Default::default(),
    }
}

async fn engine(db: &Db, fake: &Arc<FakeBackend>) -> Arc<SyncEngine> {
    let id = db.ensure_source("home", "fake").await.unwrap();
    SyncEngine::new(
        db.clone(),
        vec![SyncSource {
            id,
            name: "home".into(),
            catalog: fake.clone(),
            libraries: Vec::new(),
            enrich: true,
            tags: None,
        }],
        IgnoredArticles::default(),
        None,
    )
}

fn all() -> Page {
    Page::first(100)
}

async fn track_id(db: &Db, title: &str) -> Option<i64> {
    db.search_tracks(&rsub_core::text::search_terms(title), all())
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.title == title)
        .map(|t| t.id)
}

/// Every catalog item's public id, by name, sorted.
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

/// The library after a Plex database rebuild: every ratingKey is new and
/// unmatched items' guids are gone.
fn rebuilt(mut l: FakeLibrary) -> FakeLibrary {
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
    initial_sync(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();

        let libs = db.libraries().await.unwrap();
        assert_eq!(libs.len(), 1);
        assert_eq!(libs[0].name, "Music");
        assert_eq!(libs[0].generation, 1);

        let artists = db.index_artists(None).await.unwrap();
        let names: Vec<_> = artists.iter().map(|a| (a.name.as_str(), a.sort_key.as_str(), a.album_count)).collect();
        assert_eq!(names, [("The Beatles", "beatles", 2), ("Björk", "bjork", 1)]);

        let albums = db.albums_by_artist(artists[0].id).await.unwrap();
        let titles: Vec<_> = albums.iter().map(|a| (a.title.as_str(), a.song_count)).collect();
        assert_eq!(titles, [("Help!", 1), ("Abbey Road", 2)]);
        assert_eq!(albums[1].duration_ms, 360_000);
        assert_eq!(albums[1].artist_id, Some(artists[0].id));

        let tracks = db.tracks_by_album(albums[1].id).await.unwrap();
        assert_eq!(tracks.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), ["Come Together", "Something"]);
        let t = &tracks[0];
        assert_eq!(t.artist_id, Some(artists[0].id));
        assert_eq!(t.album_artist_id, Some(artists[0].id));
        assert_eq!(t.suffix.as_deref(), Some("flac"));
        assert_eq!(t.content_type.as_deref(), Some("audio/flac"));
        assert_eq!(t.album_title, "Abbey Road");
        assert_eq!(db.track_count().await.unwrap(), 5);
        assert!(!eng.status().scanning());
        assert_eq!(eng.status().count(), 10);
    }

    resync_keeps_ids_and_sweeps_deletions(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();
        let hunter = track_id(&db, "Hunter").await.unwrap();

        eng.sync_all().await.unwrap();
        assert_eq!(track_id(&db, "Hunter").await, Some(hunter));
        assert_eq!(db.track_count().await.unwrap(), 5);

        // Remove "Help!" (album and track) and "Jóga".
        let mut lib = library();
        lib.albums.retain(|a| a.title != "Help!");
        lib.tracks.retain(|t| t.title != "Help!" && t.title != "Jóga");
        fake.set_library("1", "Music", lib);
        eng.sync_all().await.unwrap();

        assert_eq!(db.track_count().await.unwrap(), 3);
        assert!(track_id(&db, "Jóga").await.is_none());
        let artists = db.index_artists(None).await.unwrap();
        assert_eq!(artists[0].album_count, 1);
        let homogenic = db.albums_by_artist(artists[1].id).await.unwrap();
        assert_eq!(homogenic[0].song_count, 1);
    }

    rekey_and_revive_keep_ids(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();
        let something = track_id(&db, "Something").await.unwrap();

        // Plex re-keys the item (new ratingKey; its guid isn't a `plex://` one,
        // so its tag key carries the id).
        let mut lib = library();
        let t = lib.tracks.iter_mut().find(|t| t.title == "Something").unwrap();
        t.remote.key = "t2-new".into();
        t.updated_at = 2_000;
        fake.set_library("1", "Music", lib.clone());
        eng.sync_all().await.unwrap();
        assert_eq!(track_id(&db, "Something").await, Some(something));

        // Removed, then re-added under yet another key: revived with the same id.
        let mut gone = lib.clone();
        gone.tracks.retain(|t| t.title != "Something");
        fake.set_library("1", "Music", gone);
        eng.sync_all().await.unwrap();
        assert!(track_id(&db, "Something").await.is_none());

        let t = lib.tracks.iter_mut().find(|t| t.title == "Something").unwrap();
        t.remote.key = "t2-again".into();
        fake.set_library("1", "Music", lib);
        eng.sync_all().await.unwrap();
        assert_eq!(track_id(&db, "Something").await, Some(something));
        assert_eq!(db.track_count().await.unwrap(), 5);
    }

    virtual_artists(db) {
        let fake = FakeBackend::new();
        let mut lib = library();
        let t = lib.tracks.iter_mut().find(|t| t.title == "Hunter").unwrap();
        t.display_artist = "Björk feat. Guest".into();
        t.credits.retain(|c| c.role == CreditRole::AlbumArtist);
        t.credits.push(CreditRecord { remote: None, name: "Guest".into(), role: CreditRole::Artist });
        // Name-only credit matching a real artist resolves to that artist.
        t.credits.push(CreditRecord { remote: None, name: "The Beatles".into(), role: CreditRole::Artist });
        fake.set_library("1", "Music", lib.clone());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();

        let hunter = track_id(&db, "Hunter").await.unwrap();
        let credits = db.track_credits(&[hunter]).await.unwrap();
        let names: Vec<_> = credits.iter().map(|c| (c.role.as_str(), c.name.as_str())).collect();
        assert_eq!(names, [("albumartist", "Björk"), ("artist", "Guest"), ("artist", "The Beatles")]);
        let beatles = db.index_artists(None).await.unwrap()[0].id;
        assert_eq!(credits[2].artist_id, beatles);
        let guest = credits[1].artist_id;
        assert!(db.artist(guest).await.unwrap().is_some());
        // Virtual artists without albums are not in the index.
        assert_eq!(db.index_artists(None).await.unwrap().len(), 2);

        // Once nothing credits the guest, the virtual artist is swept...
        let mut plain = library();
        plain.tracks.iter_mut().find(|t| t.title == "Hunter").unwrap().updated_at = 3_000;
        fake.set_library("1", "Music", plain);
        eng.sync_all().await.unwrap();
        assert!(db.artist(guest).await.unwrap().is_none());

        // ...and revived with the same id when credited again.
        lib.tracks.iter_mut().find(|t| t.title == "Hunter").unwrap().updated_at = 4_000;
        fake.set_library("1", "Music", lib);
        eng.sync_all().await.unwrap();
        let credits = db.track_credits(&[hunter]).await.unwrap();
        assert_eq!(credits[1].artist_id, guest);
    }

    failed_scan_sweeps_nothing(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();

        let mut lib = library();
        lib.tracks.clear();
        fake.set_library("1", "Music", lib);
        *fake.fail_scan_of.lock().unwrap() = Some("1".into());
        assert!(eng.sync_all().await.is_err());
        assert_eq!(db.track_count().await.unwrap(), 5);
        // The failed pass stamped artists and albums; its generation is used
        // up so the next pass doesn't take them for items it saw itself.
        assert_eq!(db.libraries().await.unwrap()[0].generation, 2);

        // "Help!" goes away before the retry, which must sweep it.
        let mut lib = library();
        lib.albums.retain(|a| a.title != "Help!");
        lib.tracks.retain(|t| t.title != "Help!");
        fake.set_library("1", "Music", lib);
        *fake.fail_scan_of.lock().unwrap() = None;
        eng.sync_all().await.unwrap();
        let albums: Vec<String> = db.search_albums(&[], all()).await.unwrap()
            .into_iter().map(|a| a.title).collect();
        assert!(!albums.contains(&"Help!".to_owned()), "{albums:?}");
        assert_eq!(db.track_count().await.unwrap(), 4);
    }

    library_removal(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let mut other = library();
        for t in &mut other.tracks { t.remote.guid = None; }
        fake.set_library("2", "Other", other);
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();
        assert_eq!(db.libraries().await.unwrap().len(), 2);
        assert_eq!(db.track_count().await.unwrap(), 10);
        let second = db.libraries().await.unwrap()[1].id;
        let only_second = Page { library: Some(second), ..all() };
        assert_eq!(db.index_artists(Some(second)).await.unwrap().len(), 2);
        assert_eq!(db.search_tracks(&[], only_second).await.unwrap().len(), 5);

        fake.remove_library("2");
        eng.sync_all().await.unwrap();
        assert_eq!(db.libraries().await.unwrap().len(), 1);
        assert_eq!(db.track_count().await.unwrap(), 5);
    }

    enrichment(db) {
        let fake = FakeBackend::new();
        let mut lib = library();
        lib.enrichment.insert("t4".into(), TrackEnrichment {
            key: "t4".into(),
            replay_gain: ReplayGain { track_gain: Some(-7.5), track_peak: Some(0.98), album_gain: Some(-8.0), album_peak: Some(1.0) },
            tags: vec![(TagKind::Genre, "Trip-Hop".into()), (TagKind::Mood, "Brooding".into())],
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            has_lyrics: true,
            bpm: Some(92),
            comment: None,
        });
        fake.set_library("1", "Music", lib);
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();

        let id = track_id(&db, "Hunter").await.unwrap();
        let t = db.track(id).await.unwrap().unwrap();
        assert_eq!(t.rg_track_gain, Some(-7.5));
        assert_eq!(t.bit_depth, Some(24));
        assert_eq!(t.sample_rate, Some(96_000));
        assert!(t.has_lyrics);
        let tags: Vec<_> = db.track_tags(&[id]).await.unwrap().into_iter().map(|t| (t.kind, t.name)).collect();
        assert_eq!(tags, [("mood".to_string(), "Brooding".to_string()), ("genre".into(), "Trip-Hop".into())]);
        assert!(db.tracks_to_enrich(t.library_id, 10).await.unwrap().is_empty());

        // Genre lookups combine track and album genres.
        let by_genre = db.tracks_by_genre("Trip-Hop", all()).await.unwrap();
        assert_eq!(by_genre.len(), 1);
        assert_eq!(db.tracks_by_genre("Rock", all()).await.unwrap().len(), 3);
        let genres: Vec<_> = db.genres().await.unwrap().into_iter().map(|g| (g.name, g.album_count, g.song_count)).collect();
        assert_eq!(genres, [
            ("Electronic".to_string(), 1, 2),
            ("Pop".into(), 1, 1),
            ("Rock".into(), 2, 3),
            ("Trip-Hop".into(), 0, 1),
        ]);
    }

    album_lists_and_search(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();

        let titles = |v: Vec<rsub_store::AlbumRow>| v.into_iter().map(|a| a.title).collect::<Vec<_>>();
        assert_eq!(titles(db.album_list(&AlbumOrder::ByName, all()).await.unwrap()), ["Abbey Road", "Help!", "Homogenic"]);
        assert_eq!(titles(db.album_list(&AlbumOrder::ByArtist, all()).await.unwrap()), ["Help!", "Abbey Road", "Homogenic"]);
        assert_eq!(titles(db.album_list(&AlbumOrder::Newest, Page { limit: 1, ..all() }).await.unwrap()), ["Help!"]);
        assert_eq!(titles(db.album_list(&AlbumOrder::ByYear { from: 1970, to: 1960 }, all()).await.unwrap()), ["Abbey Road", "Help!"]);
        assert_eq!(titles(db.album_list(&AlbumOrder::ByGenre("Pop".into()), all()).await.unwrap()), ["Help!"]);
        assert_eq!(db.album_list(&AlbumOrder::Random, Page { limit: 2, offset: 0, library: None }).await.unwrap().len(), 2);
        assert_eq!(titles(db.album_list(&AlbumOrder::ByName, Page { limit: 10, offset: 2, library: None }).await.unwrap()), ["Homogenic"]);

        let terms = rsub_core::text::search_terms;
        assert_eq!(db.search_tracks(&terms("joga"), all()).await.unwrap()[0].title, "Jóga");
        assert_eq!(db.search_tracks(&terms("bjork hun"), all()).await.unwrap()[0].title, "Hunter");
        assert_eq!(db.search_tracks(&terms("\"\""), all()).await.unwrap().len(), 5);
        assert!(db.search_tracks(&terms("100%"), all()).await.unwrap().is_empty());
        assert_eq!(db.search_albums(&terms("beatles"), all()).await.unwrap().len(), 2);
        assert_eq!(db.search_artists(&terms("björk"), all()).await.unwrap()[0].name, "Björk");

        let random = db.random_tracks(Some("Electronic"), (Some(1990), Some(2000)), Page { limit: 10, ..all() }).await.unwrap();
        assert_eq!(random.len(), 2);
        assert!(db.random_tracks(None, (Some(2001), None), all()).await.unwrap().is_empty());
    }

    public_ids_survive_a_rebuilt_database(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        engine(&db, &fake).await.sync_all().await.unwrap();
        let before = public_ids(&db).await;
        assert_eq!(before.len(), 2 + 3 + 5);
        for (name, id) in &before {
            assert!(id.parse::<PublicId>().is_ok(), "{name}: {id}");
        }

        // Our database is lost and Plex's rebuilt: no key survives, yet items
        // minted from tag keys come back with the same ids.
        let fresh = rsub_store::testing::sqlite().await;
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", rebuilt(library()));
        engine(&fresh, &fake).await.sync_all().await.unwrap();
        assert_eq!(public_ids(&fresh).await, before);
    }

    plex_guids_mint_ids(db) {
        let mut l = library();
        let guid = "plex://artist/5d07bbfc403c6402904a5ec9";
        l.artists[0].remote.guid = Some(guid.into());
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l);
        engine(&db, &fake).await.sync_all().await.unwrap();
        let beatles = db.search_artists(&[], all()).await.unwrap().into_iter().find(|a| a.name == "The Beatles").unwrap();
        assert_eq!(beatles.public_id, PublicId::mint(Kind::Artist, &format!("plex:{guid}")).to_string());
        // Without a plex:// guid, the artist's name is the key.
        let bjork = db.search_artists(&[], all()).await.unwrap().into_iter().find(|a| a.name == "Björk").unwrap();
        assert_eq!(bjork.public_id, PublicId::mint(Kind::Artist, "name:bjork").to_string());
    }

    duplicate_copies_get_their_own_ids(db) {
        // The same release twice (a FLAC and an MP3 copy): same tag keys.
        let beatles = artist("a1", "The Beatles");
        let flac = album("b1", &beatles, "Abbey Road");
        let mp3 = album("b2", &beatles, "Abbey Road");
        let l = FakeLibrary {
            tracks: vec![track("t1", &flac, 1, "Come Together"), track("t2", &mp3, 1, "Come Together")],
            artists: vec![beatles],
            albums: vec![flac, mp3],
            enrichment: Default::default(),
        };
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", l);
        engine(&db, &fake).await.sync_all().await.unwrap();
        let albums = db.search_albums(&[], all()).await.unwrap();
        let tracks = db.search_tracks(&[], all()).await.unwrap();
        assert_eq!(albums.len(), 2);
        assert_ne!(albums[0].public_id, albums[1].public_id);
        assert_ne!(tracks[0].public_id, tracks[1].public_id);
        let key = rsub_core::identity::album_tag_key("The Beatles", "Abbey Road", Some(2000));
        let first = PublicId::mint(Kind::Album, &key).to_string();
        let second = PublicId::mint(Kind::Album, &format!("{key}#2")).to_string();
        let mut ids: Vec<_> = albums.into_iter().map(|a| a.public_id).collect();
        ids.sort();
        let mut want = vec![first, second];
        want.sort();
        assert_eq!(ids, want);
    }

    changes_sync_incrementally(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();
        assert_eq!(fake.take_scans(), ["full 1"]);
        let lib = &db.libraries().await.unwrap()[0];
        assert_eq!(lib.changes_cursor, Some(1_000), "the newest change stamp");
        let before = public_ids(&db).await;
        let modified = db.catalog_modified_at().await.unwrap();
        assert!(modified > 0);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        // Nothing changed: no scan.
        eng.check_changes().await.unwrap();
        eng.check_changes().await.unwrap();
        assert!(fake.take_scans().is_empty());

        // A new album with a track, and a retitled track.
        let mut l = library();
        let mut post = album("b4", &l.artists[1], "Post");
        post.added_at = 90_000;
        post.updated_at = 90_000;
        let mut army = track("t6", &post, 1, "Army of Me");
        army.added_at = 90_000;
        army.updated_at = 90_000;
        l.albums.push(post);
        l.tracks.push(army);
        l.tracks[1].title = "Something (Remastered)".into();
        l.tracks[1].updated_at = 90_000;
        fake.set_library("1", "Music", l.clone());
        eng.check_changes().await.unwrap();
        assert!(fake.take_scans().is_empty(), "waits for the marker to hold still");
        eng.check_changes().await.unwrap();
        assert_eq!(fake.take_scans(), ["changes 1 -59000"]);
        // `getIndexes` clients see the change without waiting for a full sync.
        assert!(db.catalog_modified_at().await.unwrap() > modified);

        let after = public_ids(&db).await;
        assert_eq!(after.len(), before.len() + 2);
        // Every id is kept, the retitled track's too.
        for (name, pid) in &before {
            let name = if name == "Something" { "Something (Remastered)" } else { name };
            assert!(after.contains(&(name.to_owned(), pid.clone())), "{name}");
        }
        let bjork = db.index_artists(None).await.unwrap().into_iter().find(|a| a.name == "Björk").unwrap();
        assert_eq!(bjork.album_count, 2);
        let albums = db.albums_by_artist(bjork.id).await.unwrap();
        let post = albums.iter().find(|a| a.title == "Post").unwrap();
        assert_eq!((post.song_count, post.duration_ms), (1, 180_000));
        assert_eq!(db.track_count().await.unwrap(), 6);

        // The next one starts from the newest stamp seen.
        eng.check_changes().await.unwrap();
        assert!(fake.take_scans().is_empty());
        l.tracks[0].title = "Come Together (Remastered)".into();
        l.tracks[0].updated_at = 200_000;
        fake.set_library("1", "Music", l);
        eng.check_changes().await.unwrap();
        eng.check_changes().await.unwrap();
        assert_eq!(fake.take_scans(), ["changes 1 30000"]);
        assert!(track_id(&db, "Come Together (Remastered)").await.is_some());
    }

    changes_wait_for_a_scan_to_finish(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();
        fake.take_scans();

        let mut l = library();
        l.tracks[0].updated_at = 90_000;
        fake.set_library("1", "Music", l);
        fake.set_scanning("1", true);
        for _ in 0..3 {
            eng.check_changes().await.unwrap();
        }
        assert!(fake.take_scans().is_empty());
        fake.set_scanning("1", false);
        eng.check_changes().await.unwrap();
        assert_eq!(fake.take_scans(), ["changes 1 -59000"]);
    }

    changes_fall_back_to_full_syncs(db) {
        let fake = FakeBackend::new();
        fake.set_library("1", "Music", library());
        let eng = engine(&db, &fake).await;
        eng.sync_all().await.unwrap();
        fake.take_scans();

        // A backend that can't list changes gets a full sync instead.
        *fake.no_changes.lock().unwrap() = true;
        let mut l = library();
        l.tracks.retain(|t| t.title != "Jóga");
        fake.set_library("1", "Music", l.clone());
        eng.check_changes().await.unwrap();
        eng.check_changes().await.unwrap();
        assert_eq!(fake.take_scans(), ["full 1"]);
        assert!(track_id(&db, "Jóga").await.is_none());

        // A new library gets a full sync at once.
        *fake.no_changes.lock().unwrap() = false;
        fake.set_library("2", "Audiobooks", FakeLibrary::default());
        eng.check_changes().await.unwrap();
        assert_eq!(fake.take_scans(), ["full 1", "full 2"]);
        eng.check_changes().await.unwrap();
        assert!(fake.take_scans().is_empty());
    }
}
