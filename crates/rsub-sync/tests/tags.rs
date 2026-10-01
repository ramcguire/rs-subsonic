//! The tag pass, driven by the FakeBackend and an in-memory tag reader.

use std::sync::Arc;

use rsub_core::tags::FileTags;
use rsub_core::text::IgnoredArticles;
use rsub_store::{Db, store_tests};
use rsub_sync::{SyncEngine, SyncSource};
use rsub_testkit::{FakeBackend, FakeFile, FakeLibrary, FakeTags, album, artist, stamp, track};

const A: &str = "/music/Abbey Road/Come Together.flac";
const B: &str = "/music/Abbey Road/Something.flac";
const C: &str = "/music/Abbey Road/Octopus's Garden.flac";

fn library() -> FakeLibrary {
    let beatles = artist("a1", "The Beatles");
    let abbey = album("b1", &beatles, "Abbey Road");
    FakeLibrary {
        tracks: vec![
            track("t1", &abbey, 1, "Come Together"),
            track("t2", &abbey, 2, "Something"),
            track("t3", &abbey, 3, "Octopus's Garden"),
        ],
        artists: vec![beatles],
        albums: vec![abbey],
        enrichment: Default::default(),
    }
}

fn tags(mbid: &str) -> FileTags {
    FileTags {
        release_track_mbid: Some(mbid.into()),
        release_mbid: Some("rel".into()),
        album_artist_mbids: vec!["ar".into()],
        title: Some("x".into()),
        ..Default::default()
    }
}

async fn setup(db: &Db) -> (Arc<FakeBackend>, Arc<FakeTags>, Arc<SyncEngine>) {
    let fake = FakeBackend::new();
    fake.set_library("1", "Music", library());
    let reader = FakeTags::new();
    let id = db.ensure_source("home", "fake").await.unwrap();
    let eng = SyncEngine::new(
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
        None,
    );
    (fake, reader, eng)
}

async fn lib_id(db: &Db) -> i64 {
    db.libraries().await.unwrap()[0].id
}

store_tests! {
    reads_once_then_only_changed_files(db) {
        let (_, reader, eng) = setup(&db).await;
        reader.tag(A, 1, tags("mb-a"));
        reader.tag(B, 1, tags("mb-b"));
        reader.tag(C, 1, FileTags::default());
        eng.sync_all().await.unwrap();
        let mut reads = reader.take_reads();
        reads.sort();
        assert_eq!(reads, [A, C, B]);
        let lib = lib_id(&db).await;
        assert_eq!(db.file_tags(lib, A).await.unwrap(), Some(tags("mb-a")));
        assert_eq!(db.file_tags(lib, C).await.unwrap(), Some(FileTags::default()));

        // Warm cache: nothing is read.
        eng.sync_all().await.unwrap();
        assert!(reader.take_reads().is_empty());

        // A retag bumps the mtime: only that file is read again.
        reader.tag(B, 2, tags("mb-b2"));
        eng.sync_all().await.unwrap();
        assert_eq!(reader.take_reads(), [B]);
        assert_eq!(db.file_tags(lib, B).await.unwrap(), Some(tags("mb-b2")));
    }

    unavailable_files_keep_their_cache(db) {
        let (_, reader, eng) = setup(&db).await;
        reader.tag(A, 1, tags("mb-a"));
        reader.tag(B, 1, tags("mb-b"));
        eng.sync_all().await.unwrap();
        reader.take_reads();
        let lib = lib_id(&db).await;

        // The mount goes away, or a read fails with an I/O error: cached MBIDs
        // must survive, or ids would lose their strongest key.
        reader.remove(A);
        reader.set(B, FakeFile::Failing(stamp(2)));
        eng.sync_all().await.unwrap();
        assert_eq!(db.file_tags(lib, A).await.unwrap(), Some(tags("mb-a")));
        assert_eq!(db.file_tags(lib, B).await.unwrap(), Some(tags("mb-b")));
        // Unknown and unreachable files get no entry.
        assert_eq!(db.file_tags(lib, C).await.unwrap(), None);

        // Once readable again, the changed file is re-read.
        reader.tag(B, 2, tags("mb-b2"));
        eng.sync_all().await.unwrap();
        // B was tried in the failing pass and read in this one.
        assert_eq!(reader.take_reads(), [B, B]);
        assert_eq!(db.file_tags(lib, B).await.unwrap(), Some(tags("mb-b2")));
    }

    corrupt_files_are_cached_as_untagged(db) {
        let (_, reader, eng) = setup(&db).await;
        reader.tag(A, 1, tags("mb-a"));
        eng.sync_all().await.unwrap();
        reader.set(A, FakeFile::Corrupt(stamp(2)));
        eng.sync_all().await.unwrap();
        let lib = lib_id(&db).await;
        assert_eq!(db.file_tags(lib, A).await.unwrap(), Some(FileTags::default()));
        reader.take_reads();
        // Not retried until the file changes again.
        eng.sync_all().await.unwrap();
        assert!(reader.take_reads().is_empty());
    }

    follows_rekeys_and_prunes_removed_tracks(db) {
        let (fake, reader, eng) = setup(&db).await;
        reader.tag(A, 1, tags("mb-a"));
        reader.tag(B, 1, tags("mb-b"));
        eng.sync_all().await.unwrap();
        let lib = lib_id(&db).await;

        // Plex re-adds t1 under a new ratingKey and drops t2.
        let mut l = library();
        l.tracks[0].remote.key = "t9".into();
        l.tracks.remove(1);
        fake.set_library("1", "Music", l);
        eng.sync_all().await.unwrap();
        assert_eq!(reader.take_reads().len(), 2, "only the first sync reads");

        let cached = db.cached_file_tags(lib, &[A, B]).await.unwrap();
        assert_eq!(cached[A].track_key, "t9");
        assert_eq!(cached[A].album_key, "b1");
        assert_eq!(cached[A].artist_key.as_deref(), Some("a1"));
        assert!(!cached.contains_key(B));
        assert_eq!(db.file_tags(lib, A).await.unwrap(), Some(tags("mb-a")));
    }

    failed_catalog_pass_keeps_the_cache(db) {
        let (fake, reader, eng) = setup(&db).await;
        reader.tag(A, 1, tags("mb-a"));
        eng.sync_all().await.unwrap();
        *fake.fail_scan_of.lock().unwrap() = Some("1".into());
        assert!(eng.sync_all().await.is_err());
        *fake.fail_scan_of.lock().unwrap() = None;
        eng.sync_all().await.unwrap();
        let lib = lib_id(&db).await;
        assert_eq!(db.file_tags(lib, A).await.unwrap(), Some(tags("mb-a")));
        assert_eq!(reader.take_reads(), [A]);
    }
}
