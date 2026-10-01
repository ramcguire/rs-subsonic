//! Byte-bounded disk cache for cover art. Entries are keyed by cover id (which
//! embeds the art version) and size; eviction removes least-recently-used files,
//! using file mtimes (refreshed on hit) as the access clock.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use tokio::sync::Mutex;

pub struct CoverCache {
    dir: PathBuf,
    max_bytes: u64,
    total: AtomicU64,
    evicting: Mutex<()>,
}

impl CoverCache {
    /// Open (creating) the cache directory and measure its current size.
    pub async fn open(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(&dir).await?;
        let total = entries(&dir).await?.iter().map(|e| e.1).sum();
        Ok(CoverCache {
            dir,
            max_bytes,
            total: AtomicU64::new(total),
            evicting: Mutex::new(()),
        })
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir
            .join(hex(&rsub_core::crypto::sha256(key.as_bytes())[..16]))
    }

    /// Cached image bytes and their sniffed content type.
    pub async fn get(&self, key: &str) -> Option<(Vec<u8>, &'static str)> {
        let path = self.path(key);
        let data = tokio::fs::read(&path).await.ok()?;
        // Refresh the access time used for LRU; failures only affect eviction order.
        let _ = tokio::task::spawn_blocking(move || {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(SystemTime::now()))
        })
        .await;
        let ct = sniff(&data);
        Some((data, ct))
    }

    pub async fn put(&self, key: &str, data: &[u8]) {
        if self.max_bytes == 0 || data.len() as u64 > self.max_bytes / 4 {
            return;
        }
        let path = self.path(key);
        let tmp = path.with_extension("tmp");
        let write = async {
            tokio::fs::write(&tmp, data).await?;
            tokio::fs::rename(&tmp, &path).await
        };
        if let Err(e) = write.await {
            tracing::warn!("cover cache write failed: {e}");
            let _ = tokio::fs::remove_file(&tmp).await;
            return;
        }
        let total = self.total.fetch_add(data.len() as u64, Ordering::Relaxed) + data.len() as u64;
        if total > self.max_bytes {
            self.evict().await;
        }
    }

    /// Remove the least recently used entries until at most 80% full.
    async fn evict(&self) {
        let Ok(_guard) = self.evicting.try_lock() else {
            return;
        };
        let Ok(mut all) = entries(&self.dir).await else {
            return;
        };
        all.sort_by_key(|e| e.2);
        let mut total: u64 = all.iter().map(|e| e.1).sum();
        let target = self.max_bytes / 5 * 4;
        for (path, len, _) in all {
            if total <= target {
                break;
            }
            if tokio::fs::remove_file(&path).await.is_ok() {
                total -= len;
            }
        }
        self.total.store(total, Ordering::Relaxed);
    }
}

async fn entries(dir: &Path) -> std::io::Result<Vec<(PathBuf, u64, SystemTime)>> {
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(e) = rd.next_entry().await? {
        let Ok(meta) = e.metadata().await else {
            continue;
        };
        if meta.is_file() {
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            out.push((e.path(), meta.len(), mtime));
        }
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Image content type from magic bytes.
pub fn sniff(data: &[u8]) -> &'static str {
    match data {
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn caches_and_evicts() {
        let dir = std::env::temp_dir().join(format!("rsub-covers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = CoverCache::open(dir.clone(), 1000).await.unwrap();
        let jpeg = |n: usize| {
            let mut v = vec![0xFF, 0xD8, 0xFF];
            v.resize(n, 0);
            v
        };
        cache.put("al1-0:300", &jpeg(200)).await;
        assert_eq!(
            cache.get("al1-0:300").await,
            Some((jpeg(200), "image/jpeg"))
        );
        assert!(cache.get("al1-0:600").await.is_none());
        // Oversized entries are not cached.
        cache.put("big", &jpeg(600)).await;
        assert!(cache.get("big").await.is_none());

        for i in 0..6 {
            cache.put(&format!("k{i}"), &jpeg(200)).await;
            // Distinct mtimes for a deterministic LRU order.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let kept = cache.total.load(Ordering::Relaxed);
        assert!(kept <= 1000, "{kept}");
        assert!(cache.get("k5").await.is_some());
        assert!(cache.get("al1-0:300").await.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
