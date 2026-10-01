//! The analysis engine against a synced fake library, with a fake analyzer, and
//! searches over stored vectors. On SQLite, and on Postgres with
//! `RSUB_TEST_PG_URL`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rsub_analysis::{AnalysisEngine, AnalysisError, Analyzer, Metric, PassStats, SonicSearch};
use rsub_core::tags::FileTags;
use rsub_core::text::IgnoredArticles;
use rsub_media::PathMapper;
use rsub_store::{AnalysisWrite, Db, FileToAnalyze, Page, StoreError, store_tests};
use rsub_sync::{SyncEngine, SyncSource};
use rsub_testkit::{FakeBackend, FakeLibrary, FakeTags, album, artist, track};

/// Vectors by file name; "Broken" can't be decoded and "Offline" isn't
/// reachable. Records the files it's asked for.
#[derive(Default)]
struct FakeAnalyzer {
    calls: Mutex<Vec<String>>,
}

impl Analyzer for FakeAnalyzer {
    fn id(&self) -> &str {
        "fake-1"
    }

    fn metric(&self) -> Metric {
        Metric::Euclidean
    }

    fn analyze(&self, path: &Path) -> Result<Vec<f32>, AnalysisError> {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        self.calls.lock().unwrap().push(name.clone());
        match name.as_str() {
            "Broken" => Err(AnalysisError::Decode("not audio".into())),
            "Offline" => Err(AnalysisError::Io(std::io::ErrorKind::TimedOut.into())),
            n => Ok(vec![n.len() as f32, 0.0]),
        }
    }
}

impl FakeAnalyzer {
    fn take(&self) -> Vec<String> {
        let mut v = std::mem::take(&mut *self.calls.lock().unwrap());
        v.sort();
        v
    }
}

struct Env {
    db: Db,
    fake: Arc<FakeBackend>,
    sync: Arc<SyncEngine>,
    tags: Arc<FakeTags>,
    analyzer: Arc<FakeAnalyzer>,
    engine: Arc<AnalysisEngine>,
}

const TITLES: [&str; 4] = ["Hunter", "Joga", "Broken", "Offline"];

fn path(title: &str) -> String {
    format!("/music/Homogenic/{title}.flac")
}

async fn env(db: Db) -> Env {
    let bjork = artist("a1", "Björk");
    let homo = album("b1", &bjork, "Homogenic");
    let tracks = TITLES
        .iter()
        .enumerate()
        .map(|(i, t)| track(&format!("t{i}"), &homo, i as u32 + 1, t))
        .collect();
    let fake = FakeBackend::new();
    fake.set_library(
        "1",
        "Music",
        FakeLibrary {
            artists: vec![bjork],
            albums: vec![homo],
            tracks,
            enrichment: Default::default(),
        },
    );
    let tags = FakeTags::new();
    for t in TITLES {
        tags.tag(&path(t), 1, FileTags::default());
    }
    let source = db.ensure_source("home", "fake").await.unwrap();
    let sync = SyncEngine::new(
        db.clone(),
        vec![SyncSource {
            id: source,
            name: "home".into(),
            catalog: fake.clone(),
            libraries: vec![],
            enrich: false,
            tags: Some(tags.clone()),
        }],
        IgnoredArticles::default(),
        None,
    );
    sync.sync_all().await.unwrap();
    let analyzer = Arc::new(FakeAnalyzer::default());
    let paths = HashMap::from([(
        source,
        PathMapper::new([("/music".to_string(), PathBuf::from("/mnt/music"))]),
    )]);
    let engine = AnalysisEngine::new(db.clone(), analyzer.clone(), paths, 2);
    Env {
        db,
        fake,
        sync,
        tags,
        analyzer,
        engine,
    }
}

store_tests! {
    analyses_each_file_once(db) {
    let e = env(db).await;
    let stats = e.engine.pass().await.unwrap();
    assert_eq!(
        stats,
        PassStats {
            analysed: 2,
            failed: 1,
            unavailable: 1,
        }
    );
    assert_eq!(e.analyzer.take(), ["Broken", "Hunter", "Joga", "Offline"]);
    let counts = e.db.analyzer_counts().await.unwrap();
    assert_eq!(counts.len(), 1);
    assert_eq!((counts[0].analysed, counts[0].failed), (2, 1));

    // Only the unreachable file is tried again.
    let stats = e.engine.pass().await.unwrap();
    assert_eq!(stats.unavailable, 1);
    assert_eq!(e.analyzer.take(), ["Offline"]);

    // A changed file is analysed again; its old vector stands until then.
    e.tags.tag(&path("Joga"), 2, FileTags::default());
    e.sync.sync_all().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(e.analyzer.take(), ["Joga", "Offline"]);

    // A fresh engine starts from the stored vectors.
    let paths = HashMap::new();
    let again = AnalysisEngine::new(e.db.clone(), e.analyzer.clone(), paths, 1);
    assert_eq!(again.pass().await.unwrap(), PassStats::default());
    assert_eq!(analysed(&e.db).await, 2);
    }

    removed_tracks_leave_the_search(db) {
    let e = env(db).await;
    e.engine.pass().await.unwrap();
    let search = e.engine.search();
    let hunter = ids(&e.db).await["Hunter"];
    assert_eq!(search.similar(hunter, 5, &[]).await.unwrap().unwrap().len(), 1);
    let bjork = artist("a1", "Björk");
    let homo = album("b1", &bjork, "Homogenic");
    e.fake.set_library(
        "1",
        "Music",
        FakeLibrary {
            artists: vec![bjork],
            albums: vec![homo.clone()],
            tracks: vec![track("t1", &homo, 2, "Joga")],
            enrichment: Default::default(),
        },
    );
    e.sync.sync_all().await.unwrap();
    e.engine.pass().await.unwrap();
    assert_eq!(analysed(&e.db).await, 1);
    let joga = ids(&e.db).await["Joga"];
    assert!(search.similar(hunter, 5, &[]).await.unwrap().is_none());
    assert_eq!(search.similar(joga, 5, &[]).await.unwrap(), Some(vec![]));
    // The export holds what the status counts: live tracks' files only, not a
    // result left for a file no track has (as when a library leaves the config).
    let library = e.db.libraries().await.unwrap()[0].id;
    let orphan = AnalysisWrite {
        file: FileToAnalyze {
            remote_path: path("Unravel"),
            size: 1,
            mtime: 0,
        },
        vector: Some(vec![1.0, 0.0]),
    };
    e.db.write_analysis(library, "fake-1", &[orphan])
        .await
        .unwrap();
    let exported = e.db.stored_analysis("fake-1").await.unwrap();
    let paths: Vec<&str> = exported.iter().map(|a| a.remote_path.as_str()).collect();
    assert_eq!(paths, [path("Joga")]);
    let counts = e.db.analyzer_counts().await.unwrap();
    assert_eq!((counts[0].analysed, counts[0].failed), (1, 0));
    }

    searches_nearest_first(db) {
        let e = env(db).await;
        let ids = ids(&e.db).await;
        // On the unit circle, 30 degrees apart.
        let deg = |d: f32| vec![d.to_radians().cos(), d.to_radians().sin()];
        let angles = TITLES.iter().zip([0.0, 30.0, 60.0, 90.0]);
        write(&e.db, "unit-1", angles.map(|(t, d)| (*t, deg(d)))).await;
        let search = SonicSearch::new(e.db.clone(), "unit-1", Metric::Cosine);
        assert!(search.is_ready().await.unwrap());
        let m = search.similar(ids["Hunter"], 5, &[]).await.unwrap().unwrap();
        assert_eq!(titles(&ids, &m), ["Joga", "Broken", "Offline"]);
        assert!((m[0].similarity - 0.866).abs() < 1e-3 && m[2].similarity < 1e-3);
        let m = search.similar(ids["Hunter"], 1, &[ids["Joga"]]).await.unwrap().unwrap();
        assert_eq!(titles(&ids, &m), ["Broken"]);
        // Evenly spaced points on the chord are nearest Joga, then Broken.
        let m = search.path(ids["Hunter"], ids["Offline"], 4, &[]).await.unwrap().unwrap();
        assert_eq!(titles(&ids, &m), ["Hunter", "Joga", "Broken", "Offline"]);
        assert_eq!(m[0].similarity, 1.0);
        assert!(m[3].similarity < 1e-3);
        let m = search.path(ids["Hunter"], ids["Offline"], 4, &[ids["Joga"]]).await.unwrap().unwrap();
        assert_eq!(titles(&ids, &m), ["Hunter", "Broken", "Offline"]);

        // Euclidean, on a line.
        let line = TITLES.iter().zip([0.0, 1.0, 2.0, 3.0]);
        write(&e.db, "line-1", line.map(|(t, x)| (*t, vec![x, 0.0]))).await;
        let search = SonicSearch::new(e.db.clone(), "line-1", Metric::Euclidean);
        let m = search.similar(ids["Hunter"], 2, &[]).await.unwrap().unwrap();
        assert_eq!(titles(&ids, &m), ["Joga", "Broken"]);
        assert!((m[0].similarity - (-2.0f32).exp()).abs() < 1e-5);
        let m = search.path(ids["Hunter"], ids["Offline"], 4, &[]).await.unwrap().unwrap();
        assert_eq!(titles(&ids, &m), ["Hunter", "Joga", "Broken", "Offline"]);

        // Nothing for an analyzer with no vectors, or a track without one.
        let none = SonicSearch::new(e.db.clone(), "other-1", Metric::Cosine);
        assert!(!none.is_ready().await.unwrap());
        assert!(none.similar(ids["Hunter"], 5, &[]).await.unwrap().is_none());
        assert!(none.path(ids["Hunter"], ids["Joga"], 5, &[]).await.unwrap().is_none());
    }

    vectors_keep_their_analyzers_length(db) {
        let e = env(db).await;
        write(&e.db, "unit-1", [("Hunter", vec![1.0, 0.0])]).await;
        let library = e.db.libraries().await.unwrap()[0].id;
        let joga = |vector: Option<Vec<f32>>| [AnalysisWrite {
            file: FileToAnalyze { remote_path: path("Joga"), size: 1, mtime: 0 },
            vector,
        }];
        for v in [vec![1.0, 0.0, 0.0], vec![f32::NAN, 0.0], vec![]] {
            let r = e.db.write_analysis(library, "unit-1", &joga(Some(v))).await;
            assert!(matches!(r, Err(StoreError::Invalid(_))), "{r:?}");
        }
        // An analyzer's first results may all be failures; its length comes later.
        e.db.write_analysis(library, "late-1", &joga(None)).await.unwrap();
        write(&e.db, "late-1", [("Hunter", vec![0.5; 3])]).await;
        let r = e.db.write_analysis(library, "late-1", &joga(Some(vec![1.0]))).await;
        assert!(matches!(r, Err(StoreError::Invalid(_))), "{r:?}");
    }
}

/// Track ids by title.
async fn ids(db: &Db) -> HashMap<String, i64> {
    db.random_tracks(None, (None, None), Page::first(10))
        .await
        .unwrap()
        .into_iter()
        .map(|t| (t.title, t.id))
        .collect()
}

fn titles(ids: &HashMap<String, i64>, m: &[rsub_analysis::Match]) -> Vec<String> {
    m.iter()
        .map(|m| {
            ids.iter()
                .find(|(_, id)| **id == m.track_id)
                .unwrap()
                .0
                .clone()
        })
        .collect()
}

/// Store these vectors for the files of these titles.
async fn write(db: &Db, analyzer: &str, vectors: impl IntoIterator<Item = (&str, Vec<f32>)>) {
    let library = db.libraries().await.unwrap()[0].id;
    let writes: Vec<AnalysisWrite> = vectors
        .into_iter()
        .map(|(t, v)| AnalysisWrite {
            file: FileToAnalyze {
                remote_path: path(t),
                size: 1,
                mtime: 0,
            },
            vector: Some(v),
        })
        .collect();
    db.write_analysis(library, analyzer, &writes).await.unwrap();
}

/// Vectors of the fake analyzer for live tracks.
async fn analysed(db: &Db) -> i64 {
    db.analyzer_counts()
        .await
        .unwrap()
        .iter()
        .find(|c| c.analyzer == "fake-1")
        .map_or(0, |c| c.analysed)
}
