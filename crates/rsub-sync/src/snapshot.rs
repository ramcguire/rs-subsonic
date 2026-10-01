//! The identity ledger snapshot: the ledger as JSON lines outside the database,
//! so ids survive losing or replacing it.
//!
//! The first line is a header, `{"rsub_identity":1}`; each further line is
//! one [`LedgerEntry`].

use std::path::{Path, PathBuf};

use rsub_store::{Db, LedgerEntry, StoreError};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufWriter};

/// The snapshot's file name in the data directory.
pub const SNAPSHOT_FILE: &str = "identity.jsonl";
const VERSION: u32 = 1;
const PAGE: u64 = 5000;

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("line {line}: {msg}")]
    Format { line: usize, msg: String },
}

#[derive(Serialize, Deserialize)]
struct Header {
    rsub_identity: u32,
}

/// Write the whole ledger to `w`. Returns the number of entries.
pub async fn export<W: AsyncWrite + Unpin>(db: &Db, w: W) -> Result<u64, SnapshotError> {
    let mut w = BufWriter::new(w);
    let mut line = serde_json::to_vec(&Header {
        rsub_identity: VERSION,
    })
    .expect("header serialises");
    line.push(b'\n');
    w.write_all(&line).await?;
    let mut n = 0;
    let mut after: Option<(String, String)> = None;
    loop {
        let page = db
            .ledger_page(after.as_ref().map(|(k, v)| (k.as_str(), v.as_str())), PAGE)
            .await?;
        let Some(last) = page.last() else { break };
        after = Some((last.kind.clone(), last.key.clone()));
        for e in &page {
            line.clear();
            serde_json::to_writer(&mut line, e).expect("entries serialise");
            line.push(b'\n');
            w.write_all(&line).await?;
        }
        n += page.len() as u64;
        if (page.len() as u64) < PAGE {
            break;
        }
    }
    w.flush().await?;
    Ok(n)
}

/// What an import read and added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    pub read: u64,
    /// Entries whose key the ledger didn't have yet.
    pub added: u64,
}

/// Read a snapshot from `r` into the ledger, in one transaction. Keys the
/// ledger already has keep their ids. Nothing is written unless the whole
/// snapshot parses.
pub async fn import<R: AsyncBufRead + Unpin>(db: &Db, r: R) -> Result<Imported, SnapshotError> {
    let mut lines = r.lines();
    let mut entries = Vec::new();
    let mut n = 0;
    let mut header = false;
    while let Some(line) = lines.next_line().await? {
        n += 1;
        if line.trim().is_empty() {
            continue;
        }
        let bad = |msg: String| SnapshotError::Format { line: n, msg };
        if !header {
            let h: Header = serde_json::from_str(&line)
                .map_err(|_| bad("not an rs-subsonic identity snapshot".into()))?;
            if h.rsub_identity != VERSION {
                return Err(bad(format!(
                    "snapshot version {} is not supported",
                    h.rsub_identity
                )));
            }
            header = true;
            continue;
        }
        entries.push(serde_json::from_str::<LedgerEntry>(&line).map_err(|e| bad(e.to_string()))?);
    }
    if !header {
        return Err(SnapshotError::Format {
            line: 1,
            msg: "empty snapshot".into(),
        });
    }
    let added = db.import_ledger(&entries).await?;
    Ok(Imported {
        read: entries.len() as u64,
        added,
    })
}

/// Rewrite the snapshot at `path`: written beside it, then renamed over it,
/// so a crash leaves the previous one. An empty ledger never replaces a
/// snapshot. Returns the number of entries written.
pub async fn write_snapshot(db: &Db, path: &Path) -> Result<u64, SnapshotError> {
    if db.ledger_is_empty().await? {
        return Ok(0);
    }
    let tmp = tmp_path(path);
    let mut file = tokio::fs::File::create(&tmp).await?;
    let n = export(db, &mut file).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;
    Ok(n)
}

/// Restore the snapshot at `path` into an empty ledger (a new or rebuilt
/// database), before anything is synced. Returns what was imported, or
/// `None` when the ledger isn't empty or there is no snapshot.
pub async fn restore_snapshot(db: &Db, path: &Path) -> Result<Option<Imported>, SnapshotError> {
    if !db.ledger_is_empty().await? {
        return Ok(None);
    }
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    import(db, tokio::io::BufReader::new(file)).await.map(Some)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".tmp");
    path.with_file_name(name)
}
