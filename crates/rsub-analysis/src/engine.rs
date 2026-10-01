//! The background analysis loop: finds library files whose analysis is missing
//! or stale, analyses them on blocking threads, and stores the vectors.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use rsub_media::PathMapper;
use rsub_store::{AnalysisWrite, Db, FileToAnalyze, StoreError};
use tokio::sync::Notify;

use crate::{AnalysisError, Analyzer, SonicSearch};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PassStats {
    /// Files given a vector.
    pub analysed: u64,
    /// Files that couldn't be decoded or analysed (stored as such).
    pub failed: u64,
    /// Files not reachable from here; retried next pass.
    pub unavailable: u64,
}

pub struct AnalysisEngine {
    db: Db,
    analyzer: Arc<dyn Analyzer>,
    /// Path mapping by source id.
    paths: HashMap<i64, PathMapper>,
    threads: usize,
    trigger: Notify,
}

enum Outcome {
    Vector(Vec<f32>),
    Failed(AnalysisError),
    Unavailable(AnalysisError),
}

impl AnalysisEngine {
    /// `paths` maps each source id to where its files are mounted; libraries
    /// of other sources are skipped. `threads` analyses run at once.
    pub fn new(
        db: Db,
        analyzer: Arc<dyn Analyzer>,
        paths: HashMap<i64, PathMapper>,
        threads: usize,
    ) -> Arc<Self> {
        Arc::new(AnalysisEngine {
            db,
            analyzer,
            paths,
            threads: threads.max(1),
            trigger: Notify::new(),
        })
    }

    /// Searches over this analyzer's vectors.
    pub fn search(&self) -> SonicSearch {
        SonicSearch::new(self.db.clone(), self.analyzer.id(), self.analyzer.metric())
    }

    pub fn analyzer_id(&self) -> &str {
        self.analyzer.id()
    }

    /// Ask the loop for a pass now (after a sync, say).
    pub fn trigger(&self) {
        self.trigger.notify_one();
    }

    /// Background loop: a pass now, then every `interval` or when triggered.
    pub async fn run(self: Arc<Self>, interval: Duration) {
        loop {
            let started = Instant::now();
            match self.pass().await {
                Ok(s) if s == PassStats::default() => {}
                Ok(s) => tracing::info!(
                    analyzer = self.analyzer.id(),
                    analysed = s.analysed,
                    failed = s.failed,
                    unavailable = s.unavailable,
                    secs = started.elapsed().as_secs(),
                    "analysis pass done"
                ),
                Err(e) => tracing::error!("analysis pass failed: {e}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = self.trigger.notified() => {}
            }
        }
    }

    /// Analyse every file that needs it, library by library.
    pub async fn pass(&self) -> Result<PassStats, StoreError> {
        let mut stats = PassStats::default();
        for lib in self.db.libraries().await? {
            let Some(paths) = self.paths.get(&lib.source_id) else {
                continue;
            };
            self.db.prune_analysis(lib.id).await?;
            let batch = (self.threads * 8) as u64;
            // Unavailable files stay unanalysed, so they'd be listed again.
            let mut skip: HashSet<String> = HashSet::new();
            loop {
                let files: Vec<FileToAnalyze> = self
                    .db
                    .files_to_analyze(lib.id, self.analyzer.id(), batch + skip.len() as u64)
                    .await?
                    .into_iter()
                    .filter(|f| !skip.contains(&f.remote_path))
                    .collect();
                if files.is_empty() {
                    break;
                }
                let outcomes = self.analyze_all(paths, files).await;
                let mut writes = Vec::with_capacity(outcomes.len());
                let mut reached = false;
                for (file, outcome) in outcomes {
                    match outcome {
                        Outcome::Vector(v) => {
                            stats.analysed += 1;
                            reached = true;
                            writes.push(AnalysisWrite {
                                file,
                                vector: Some(v),
                            });
                        }
                        Outcome::Failed(e) => {
                            stats.failed += 1;
                            reached = true;
                            tracing::warn!(path = %file.remote_path, "cannot analyse: {e}");
                            writes.push(AnalysisWrite { file, vector: None });
                        }
                        Outcome::Unavailable(e) => {
                            if stats.unavailable == 0 {
                                tracing::warn!(
                                    library = %lib.name,
                                    path = %file.remote_path,
                                    "file unavailable for analysis: {e}; later ones are logged at debug"
                                );
                            } else {
                                tracing::debug!(path = %file.remote_path, "file unavailable: {e}");
                            }
                            stats.unavailable += 1;
                            skip.insert(file.remote_path);
                        }
                    }
                }
                self.db
                    .write_analysis(lib.id, self.analyzer.id(), &writes)
                    .await?;
                // A batch with nothing readable means the mount is gone (or the
                // path map is wrong); the same files would come back forever.
                if !reached {
                    break;
                }
            }
        }
        Ok(stats)
    }

    async fn analyze_all(
        &self,
        paths: &PathMapper,
        files: Vec<FileToAnalyze>,
    ) -> Vec<(FileToAnalyze, Outcome)> {
        stream::iter(files)
            .map(|file| {
                let local = paths.map(&file.remote_path);
                let analyzer = self.analyzer.clone();
                async move {
                    let Some(path) = local else {
                        let e = std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no path_map matches",
                        );
                        return (file, Outcome::Unavailable(AnalysisError::Io(e)));
                    };
                    let outcome =
                        match tokio::task::spawn_blocking(move || analyzer.analyze(&path)).await {
                            Ok(Ok(v)) => Outcome::Vector(v),
                            Ok(Err(e @ AnalysisError::Io(_))) => Outcome::Unavailable(e),
                            Ok(Err(e)) => Outcome::Failed(e),
                            Err(e) => Outcome::Failed(AnalysisError::Analysis(format!(
                                "analyzer panicked: {e}"
                            ))),
                        };
                    (file, outcome)
                }
            })
            .buffer_unordered(self.threads)
            .collect()
            .await
    }
}
