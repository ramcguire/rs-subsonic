//! The tag pass: before the catalog pass, read the tags of every track's file
//! whose size or mtime changed since the cache entry, so MBIDs are known before
//! artists and albums are resolved.

use std::io;

use futures_util::{StreamExt, stream};
use rsub_core::backend::{BackendError, RemoteLibrary, TrackFile};
use rsub_core::tags::{FileStamp, FileTags, TagReader};
use rsub_store::{CachedFile, Db, FileTagsWrite, Library};

use crate::{SyncError, SyncSource};

/// Files stat'ed and read concurrently.
const CONCURRENCY: usize = 8;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TagPassStats {
    /// Files the backend listed.
    pub files: u64,
    /// Files whose tags were (re)read.
    pub read: u64,
    /// Files that could not be parsed (cached as having no tags).
    pub unparsable: u64,
    /// Files not reachable from here, or that failed with an I/O error.
    /// Their cache entries, if any, are kept as they are.
    pub unavailable: u64,
    /// Files with no known release-track MBID after the pass.
    pub no_mbid: u64,
}

/// What reading one file produced.
enum Outcome {
    /// Cache entry is current.
    Fresh,
    Read(FileStamp, Box<FileTags>),
    Unparsable(FileStamp),
    Unavailable(io::Error),
}

/// Run the tag pass for one library, or with `since` for the tracks changed
/// since then. Returns `None` when the backend can't list track files, in
/// which case the cache is left alone.
pub(crate) async fn tag_pass(
    db: &Db,
    src: &SyncSource,
    reader: &dyn TagReader,
    lib: &Library,
    remote: &RemoteLibrary,
    generation: i64,
    since: Option<i64>,
) -> Result<Option<TagPassStats>, SyncError> {
    let mut stats = TagPassStats::default();
    // Unavailable files usually share one cause (a wrong `path_map`, a
    // dropped mount), so the first one's error is logged and the rest only
    // counted, or logged at debug.
    let mut pages = src.catalog.track_files(remote, since);
    let empty = FileTags::default();
    while let Some(page) = pages.next().await {
        let page = match page {
            Ok(p) => p,
            Err(BackendError::Unsupported) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let paths: Vec<&str> = page.iter().map(|f| f.remote_path.as_str()).collect();
        let cached = db.cached_file_tags(lib.id, &paths).await?;
        // In page order, with up to CONCURRENCY files in flight. The checks
        // are built first: a stream mapping them in a closure makes the sync
        // task not `Send`.
        let checks: Vec<_> = page
            .iter()
            .map(|f| check(reader, f, cached.get(&f.remote_path).map(|c| c.stamp)))
            .collect();
        let outcomes: Vec<Outcome> = stream::iter(checks).buffered(CONCURRENCY).collect().await;

        let mut writes = Vec::with_capacity(page.len());
        for (f, outcome) in page.iter().zip(&outcomes) {
            stats.files += 1;
            let cached = cached.get(&f.remote_path);
            let no_mbid = match outcome {
                Outcome::Fresh => keep(&mut writes, f, cached),
                Outcome::Unavailable(e) => {
                    stats.unavailable += 1;
                    if stats.unavailable == 1 {
                        tracing::warn!(
                            library = %lib.name,
                            path = %f.remote_path,
                            "file unavailable: {e}; later ones are logged at debug"
                        );
                    } else {
                        tracing::debug!(path = %f.remote_path, "file unavailable: {e}");
                    }
                    keep(&mut writes, f, cached)
                }
                Outcome::Read(stamp, tags) => {
                    stats.read += 1;
                    writes.push(FileTagsWrite::Read(f, *stamp, tags));
                    tags.release_track_mbid.is_none()
                }
                Outcome::Unparsable(stamp) => {
                    stats.unparsable += 1;
                    writes.push(FileTagsWrite::Read(f, *stamp, &empty));
                    true
                }
            };
            if no_mbid {
                stats.no_mbid += 1;
                tracing::debug!(path = %f.remote_path, "no release-track MBID");
            }
        }
        db.write_file_tags(lib.id, generation, &writes).await?;
    }
    drop(pages);
    // Only a complete listing shows which files are gone.
    if since.is_none() {
        db.prune_file_tags(lib.id, generation).await?;
    }
    Ok(Some(stats))
}

/// Keep the cache entry as it is, following the item keys. Returns whether
/// the file is without a release-track MBID.
fn keep<'a>(
    writes: &mut Vec<FileTagsWrite<'a>>,
    f: &'a TrackFile,
    cached: Option<&CachedFile>,
) -> bool {
    if let Some(c) = cached {
        writes.push(if c.keys_match(f) {
            FileTagsWrite::Touch(&f.remote_path)
        } else {
            FileTagsWrite::Rekey(f)
        });
    }
    cached.is_none_or(|c| c.release_track_mbid.is_none())
}

async fn check(reader: &dyn TagReader, f: &TrackFile, cached: Option<FileStamp>) -> Outcome {
    let stamp = match reader.stat(&f.remote_path).await {
        Ok(s) => s,
        Err(e) => return Outcome::Unavailable(e),
    };
    if cached == Some(stamp) {
        return Outcome::Fresh;
    }
    match reader.read(&f.remote_path).await {
        Ok(tags) => Outcome::Read(stamp, Box::new(tags)),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            tracing::warn!(path = %f.remote_path, "cannot parse tags: {e}");
            Outcome::Unparsable(stamp)
        }
        Err(e) => Outcome::Unavailable(e),
    }
}
