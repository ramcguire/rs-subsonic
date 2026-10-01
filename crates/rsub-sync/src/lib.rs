//! Catalog sync engine: mirrors each backend's libraries into the local store.
//!
//! Full syncs use generation mark-and-sweep; between them, a library whose
//! backend change marker moves gets an incremental sync of the items changed
//! since the last one. Each is followed by the enrichment pass. [`snapshot`]
//! keeps the identity ledger outside the database. User state is not synced: it
//! is read and written live.

pub mod backup;
pub mod snapshot;
mod tags;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use rsub_core::backend::{BackendError, BoxStream, CatalogBatch, CatalogSource, RemoteLibrary};
use rsub_core::now_ms;
use rsub_core::tags::TagReader;
use rsub_core::text::IgnoredArticles;
use rsub_store::{Db, Library, StoreError, SyncCtx};
use tokio::sync::{Mutex, Notify};

pub use tags::TagPassStats;

/// How the catalog was last written: [`INDEX_VERSION`] and the ignored
/// articles. When either changes, the next sync rewrites every row.
const INDEX_SETTING: &str = "index_format";
/// Bump when the rows derived from a backend item change (sort keys,
/// credits), so existing rows are rewritten.
const INDEX_VERSION: u32 = 2;
const ENRICH_BATCH: u64 = 100;
/// How long a moved change marker must hold still, with the library not
/// being scanned, before the library syncs: a metadata refresh moves it for
/// minutes.
pub const SETTLE: Duration = Duration::from_secs(10);
/// How far an incremental sync reaches back before the newest change stamp
/// already seen: items changed in the same second as it, and clock steps.
const CURSOR_OVERLAP_MS: i64 = 60_000;

/// Longest wait before retrying a failed full sync.
const MAX_RETRY: Duration = Duration::from_secs(3600);

/// The wait before retrying a full sync after `failures` in a row: a minute,
/// doubling, at most [`MAX_RETRY`] or the full-sync interval.
fn retry_backoff(failures: u32, full_interval: Duration) -> Duration {
    let doublings = failures.saturating_sub(1).min(10);
    Duration::from_secs(60 << doublings)
        .min(MAX_RETRY)
        .min(full_interval)
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Backend(#[from] BackendError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A configured backend as the sync engine sees it.
pub struct SyncSource {
    /// `sources.id` in the store.
    pub id: i64,
    pub name: String,
    pub catalog: Arc<dyn CatalogSource>,
    /// Library names (or keys) to mirror; empty means all.
    pub libraries: Vec<String>,
    pub enrich: bool,
    /// Reads tags from the mounted library for the tag pass; `None` skips it.
    pub tags: Option<Arc<dyn TagReader>>,
}

impl SyncSource {
    fn wants(&self, lib: &RemoteLibrary) -> bool {
        self.libraries.is_empty()
            || self
                .libraries
                .iter()
                .any(|w| w.eq_ignore_ascii_case(&lib.name) || *w == lib.key)
    }
}

/// Live scan progress for `getScanStatus`.
#[derive(Debug, Default)]
pub struct ScanStatus {
    scanning: AtomicBool,
    count: AtomicU64,
}

impl ScanStatus {
    pub fn scanning(&self) -> bool {
        self.scanning.load(Ordering::Relaxed)
    }

    /// Items processed by the running scan.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

pub struct SyncEngine {
    db: Db,
    sources: Vec<SyncSource>,
    articles: IgnoredArticles,
    /// Where the identity ledger snapshot is rewritten after each sync.
    snapshot: Option<PathBuf>,
    status: ScanStatus,
    trigger: Notify,
    running: Mutex<()>,
    /// Libraries whose change marker moved at the last check, with the
    /// marker then seen, by library id.
    moved: std::sync::Mutex<HashMap<i64, String>>,
    /// Called after a sync that completed at least one library.
    after_sync: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl SyncEngine {
    pub fn new(
        db: Db,
        sources: Vec<SyncSource>,
        articles: IgnoredArticles,
        snapshot: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(SyncEngine {
            db,
            sources,
            articles,
            snapshot,
            status: ScanStatus::default(),
            trigger: Notify::new(),
            running: Mutex::new(()),
            moved: std::sync::Mutex::default(),
            after_sync: OnceLock::new(),
        })
    }

    /// Run `f` after each sync that completed at least one library (to start
    /// work that follows the catalog, like audio analysis). Set once.
    pub fn on_synced(&self, f: impl Fn() + Send + Sync + 'static) {
        let _ = self.after_sync.set(Box::new(f));
    }

    pub fn status(&self) -> &ScanStatus {
        &self.status
    }

    /// Ask the background loop for a full sync now (`startScan`).
    pub fn trigger(&self) {
        self.trigger.notify_one();
    }

    /// Background loop: a full sync when due (every `full_interval`) or
    /// triggered, and between them, every `check_interval`, a check for
    /// libraries whose content changed ([`SyncEngine::check_changes`]).
    pub async fn run(self: Arc<Self>, full_interval: Duration, check_interval: Option<Duration>) {
        // Consecutive failed full syncs, and when the next may start: a
        // library that keeps failing stays due, and would otherwise resync
        // every library back to back.
        let mut failures = 0u32;
        let mut retry_at: Option<Instant> = None;
        loop {
            let mut due = match self.next_due(full_interval).await {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!("cannot read sync state: {e}");
                    Duration::from_secs(60)
                }
            };
            if let Some(at) = retry_at {
                due = due.max(at.saturating_duration_since(Instant::now()));
            }
            // A moved marker is looked at again soon, to see it settle.
            let settling = !self.moved.lock().unwrap().is_empty();
            let check = check_interval.map(|c| if settling { SETTLE.min(c) } else { c });
            let wait = check.map_or(due, |c| c.min(due));
            let mut triggered = false;
            if !wait.is_zero() {
                tracing::debug!(secs = wait.as_secs(), "next sync or check");
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = self.trigger.notified() => triggered = true,
                }
            }
            // A triggered sync (`startScan`) doesn't wait out the backoff.
            let full = triggered || due <= wait;
            let result = if full {
                self.sync_all().await
            } else {
                self.check_changes().await
            };
            match result {
                Ok(()) if full => (failures, retry_at) = (0, None),
                Ok(()) => {}
                Err(e) => {
                    tracing::error!("sync failed: {e}");
                    if full {
                        failures += 1;
                        let backoff = retry_backoff(failures, full_interval);
                        tracing::warn!(
                            failures,
                            retry_secs = backoff.as_secs(),
                            "full sync failed; retrying later"
                        );
                        retry_at = Some(Instant::now() + backoff);
                    }
                    // Avoid a hot loop when the backend is down.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            }
        }
    }

    /// Compare each library's change marker with the one recorded at its last
    /// sync. A library whose marker moved syncs incrementally once two checks
    /// in a row (see [`SETTLE`]) found the same marker and it isn't being
    /// scanned. A library added or removed in the backend triggers a full
    /// sync. Errors in one library are logged and do not stop the others; the
    /// first error is returned.
    pub async fn check_changes(&self) -> Result<(), SyncError> {
        let stored = self.db.libraries().await?;
        let mut first_err = None;
        for src in &self.sources {
            let remote: Vec<RemoteLibrary> = src
                .catalog
                .libraries()
                .await?
                .into_iter()
                .filter(|l| src.wants(l))
                .collect();
            let known: Vec<&Library> = stored.iter().filter(|l| l.source_id == src.id).collect();
            let added = remote
                .iter()
                .any(|r| !known.iter().any(|l| l.remote_key == r.key));
            let removed = known
                .iter()
                .any(|l| !remote.iter().any(|r| r.key == l.remote_key));
            if added || removed {
                tracing::info!(source = %src.name, "libraries added or removed; full sync");
                self.moved.lock().unwrap().clear();
                return self.sync_all().await;
            }
            for lib in known {
                let Some(r) = remote.iter().find(|r| r.key == lib.remote_key) else {
                    continue;
                };
                let Some(marker) = &r.change_marker else {
                    continue;
                };
                if lib.change_marker.as_ref() == Some(marker) {
                    self.moved.lock().unwrap().remove(&lib.id);
                    continue;
                }
                let settled = {
                    let mut moved = self.moved.lock().unwrap();
                    let settled = !r.scanning && moved.get(&lib.id) == Some(marker);
                    if settled {
                        moved.remove(&lib.id);
                    } else {
                        moved.insert(lib.id, marker.clone());
                    }
                    settled
                };
                if !settled {
                    tracing::debug!(
                        library = %lib.name,
                        %marker,
                        scanning = r.scanning,
                        "library changed; waiting for it to settle"
                    );
                    continue;
                }
                if let Err(e) = self.sync_library_changes(src, lib, r).await {
                    tracing::error!(source = %src.name, library = %lib.name, "incremental sync failed: {e}");
                    first_err.get_or_insert(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Sync what changed in one library since its last sync, or run a full
    /// sync when that can't be done.
    async fn sync_library_changes(
        &self,
        src: &SyncSource,
        lib: &Library,
        remote: &RemoteLibrary,
    ) -> Result<(), SyncError> {
        let why = match self.try_library_changes(src, lib, remote).await {
            Ok(None) => return Ok(()),
            Ok(Some(why)) => why.to_owned(),
            Err(SyncError::Store(StoreError::NeedsFullSync(why))) => why,
            Err(SyncError::Backend(BackendError::Unsupported)) => {
                "the backend can't list changes".to_owned()
            }
            Err(e) => return Err(e),
        };
        tracing::info!(library = %lib.name, "{why}; full sync");
        self.sync_all().await
    }

    /// `Ok(Some(reason))` when only a full sync will do.
    async fn try_library_changes(
        &self,
        src: &SyncSource,
        lib: &Library,
        remote: &RemoteLibrary,
    ) -> Result<Option<&'static str>, SyncError> {
        let _guard = self.running.lock().await;
        let Some(cursor) = lib.changes_cursor else {
            return Ok(Some("no incremental starting point yet"));
        };
        if self.index_format_changed().await? {
            return Ok(Some("the index format changed"));
        }
        let cx = SyncCtx {
            source: &src.name,
            library: lib,
            generation: lib.generation + 1,
            articles: &self.articles,
            force: false,
            incremental: true,
        };
        self.status.count.store(0, Ordering::Relaxed);
        self.status.scanning.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let result = self
            .apply_library_changes(src, cx, remote, cursor - CURSOR_OVERLAP_MS)
            .await;
        if result.is_err() {
            // Rows this pass wrote carry its generation; the next pass must
            // not take them for rows it saw itself.
            if let Err(e) = self.db.set_library_generation(lib.id, cx.generation).await {
                tracing::error!(library = %lib.name, "cannot record the generation: {e}");
            }
        }
        self.status.scanning.store(false, Ordering::Relaxed);
        let (items, written) = result?;
        tracing::info!(
            source = %src.name,
            library = %lib.name,
            items,
            written,
            ms = started.elapsed().as_millis() as u64,
            "library changes synced"
        );
        if items > 0 {
            self.write_snapshot().await;
            if let Some(f) = self.after_sync.get() {
                f();
            }
        }
        Ok(None)
    }

    /// Returns the items listed and the tracks written.
    async fn apply_library_changes(
        &self,
        src: &SyncSource,
        cx: SyncCtx<'_>,
        remote: &RemoteLibrary,
        since: i64,
    ) -> Result<(usize, usize), SyncError> {
        let lib = cx.library;
        if let Some(reader) = &src.tags {
            let stats = tags::tag_pass(
                &self.db,
                src,
                reader.as_ref(),
                lib,
                remote,
                cx.generation,
                Some(since),
            )
            .await?;
            if let Some(s) = stats {
                tracing::debug!(
                    library = %lib.name,
                    files = s.files,
                    read = s.read,
                    unavailable = s.unavailable,
                    "tags of changed tracks read"
                );
            }
        }
        let counts = self
            .apply_batches(cx, src.catalog.changes_since(remote, since))
            .await?;
        self.db
            .finish_incremental_sync(cx, remote.change_marker.as_deref())
            .await?;
        if src.enrich {
            self.enrich_library(src, lib).await?;
        }
        Ok(counts)
    }

    /// Time until the stalest library is due, or zero if one has never synced.
    async fn next_due(&self, interval: Duration) -> Result<Duration, StoreError> {
        let libs = self.db.libraries().await?;
        let oldest = libs.iter().map(|l| l.last_full_sync_at).min().flatten();
        let Some(oldest) = oldest.filter(|_| !libs.is_empty()) else {
            return Ok(Duration::ZERO);
        };
        let due = oldest.saturating_add(interval.as_millis() as i64);
        Ok(Duration::from_millis(
            due.saturating_sub(now_ms()).max(0) as u64
        ))
    }

    /// Full sync of every source. Errors in one library are logged and do not
    /// stop the others; the first error is returned.
    pub async fn sync_all(&self) -> Result<(), SyncError> {
        let _guard = self.running.lock().await;
        self.status.count.store(0, Ordering::Relaxed);
        self.status.scanning.store(true, Ordering::Relaxed);
        let mut synced = 0;
        let result = self.sync_all_inner(&mut synced).await;
        if synced > 0 {
            self.write_snapshot().await;
            if let Some(f) = self.after_sync.get() {
                f();
            }
        }
        self.status.scanning.store(false, Ordering::Relaxed);
        result
    }

    /// Rewrite the ledger snapshot. A failure is logged, not returned: the
    /// ledger itself is safe in the database, and the next sync retries.
    async fn write_snapshot(&self) {
        let Some(path) = &self.snapshot else { return };
        match snapshot::write_snapshot(&self.db, path).await {
            Ok(n) => {
                tracing::debug!(entries = n, path = %path.display(), "identity snapshot written")
            }
            Err(e) => {
                tracing::error!(path = %path.display(), "cannot write identity snapshot: {e}")
            }
        }
    }

    fn index_format(&self) -> String {
        format!("{INDEX_VERSION}|{}", self.articles.list)
    }

    /// Whether the rows derived from backend items must all be rewritten.
    async fn index_format_changed(&self) -> Result<bool, SyncError> {
        let format = self.index_format();
        Ok(self.db.get_setting(INDEX_SETTING).await?.as_deref() != Some(format.as_str()))
    }

    /// `synced` counts the libraries whose full sync completed.
    async fn sync_all_inner(&self, synced: &mut usize) -> Result<(), SyncError> {
        let format = self.index_format();
        let force = self.index_format_changed().await?;
        let mut first_err = None;
        for src in &self.sources {
            if let Err(e) = self.sync_source(src, force, synced).await {
                tracing::error!(source = %src.name, "sync failed: {e}");
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => {
                if force {
                    self.db.set_setting(INDEX_SETTING, &format).await?;
                }
                Ok(())
            }
        }
    }

    async fn sync_source(
        &self,
        src: &SyncSource,
        force: bool,
        synced: &mut usize,
    ) -> Result<(), SyncError> {
        let remote: Vec<RemoteLibrary> = src
            .catalog
            .libraries()
            .await?
            .into_iter()
            .filter(|l| src.wants(l))
            .collect();
        if remote.is_empty() {
            tracing::warn!(source = %src.name, "no music libraries to sync");
        }
        let libs = self.db.sync_libraries(src.id, &remote).await?;
        let mut first_err = None;
        for lib in &libs {
            let Some(r) = remote.iter().find(|r| r.key == lib.remote_key) else {
                continue;
            };
            let started = std::time::Instant::now();
            match self.full_sync_library(src, lib, r, force).await {
                Ok(()) => {
                    *synced += 1;
                    tracing::info!(
                        source = %src.name,
                        library = %lib.name,
                        secs = started.elapsed().as_secs(),
                        "library synced"
                    )
                }
                Err(e) => {
                    tracing::error!(source = %src.name, library = %lib.name, "library sync failed: {e}");
                    // Rows this pass wrote carry its generation; the retry
                    // must not take them for rows it saw itself, or items
                    // gone by then are never swept.
                    if let Err(err) = self
                        .db
                        .set_library_generation(lib.id, lib.generation + 1)
                        .await
                    {
                        tracing::error!(library = %lib.name, "cannot record the generation: {err}");
                    }
                    first_err.get_or_insert(e);
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    async fn full_sync_library(
        &self,
        src: &SyncSource,
        lib: &Library,
        remote: &RemoteLibrary,
        force: bool,
    ) -> Result<(), SyncError> {
        let cx = SyncCtx {
            source: &src.name,
            library: lib,
            generation: lib.generation + 1,
            articles: &self.articles,
            force,
            incremental: false,
        };
        if let Some(reader) = &src.tags {
            let started = std::time::Instant::now();
            match tags::tag_pass(
                &self.db,
                src,
                reader.as_ref(),
                lib,
                remote,
                cx.generation,
                None,
            )
            .await?
            {
                Some(s) => tracing::info!(
                    library = %lib.name,
                    files = s.files,
                    read = s.read,
                    unparsable = s.unparsable,
                    unavailable = s.unavailable,
                    without_mbid = s.no_mbid,
                    secs = started.elapsed().as_secs(),
                    "tag pass done"
                ),
                None => {
                    tracing::debug!(library = %lib.name, "backend cannot list files; no tag pass")
                }
            }
        }
        let (_, written) = self
            .apply_batches(cx, src.catalog.full_scan(remote))
            .await?;
        let swept = self
            .db
            .finish_full_sync(cx, remote.change_marker.as_deref())
            .await?;
        tracing::debug!(library = %lib.name, written, ?swept, "full scan applied");
        if src.enrich {
            self.enrich_library(src, lib).await?;
        }
        Ok(())
    }

    /// Write a scan's batches. Returns the items listed and the tracks
    /// written (inserted or updated).
    async fn apply_batches(
        &self,
        cx: SyncCtx<'_>,
        mut pages: BoxStream<'_, Result<CatalogBatch, BackendError>>,
    ) -> Result<(usize, usize), SyncError> {
        let (mut items, mut written) = (0, 0);
        while let Some(batch) = pages.next().await {
            let n = match batch? {
                CatalogBatch::Artists(b) => {
                    self.db.upsert_artists(cx, &b).await?;
                    b.len()
                }
                CatalogBatch::Albums(b) => {
                    self.db.upsert_albums(cx, &b).await?;
                    b.len()
                }
                CatalogBatch::Tracks(b) => {
                    written += self.db.upsert_tracks(cx, &b).await?;
                    b.len()
                }
            };
            items += n;
            self.status.count.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok((items, written))
    }

    async fn enrich_library(&self, src: &SyncSource, lib: &Library) -> Result<(), SyncError> {
        loop {
            let keys = self.db.tracks_to_enrich(lib.id, ENRICH_BATCH).await?;
            if keys.is_empty() {
                return Ok(());
            }
            let results = match src.catalog.enrich(&keys).await {
                Ok(r) => r,
                Err(BackendError::Unsupported) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            self.db.apply_enrichment(lib.id, &keys, &results).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off() {
        let day = Duration::from_secs(86_400);
        let secs = |n| retry_backoff(n, day).as_secs();
        assert_eq!([secs(1), secs(2), secs(3), secs(6)], [60, 120, 240, 1920]);
        assert_eq!(secs(7), 3600);
        assert_eq!(secs(u32::MAX), 3600);
        let short = Duration::from_secs(300);
        assert_eq!(retry_backoff(5, short), short);
    }
}
