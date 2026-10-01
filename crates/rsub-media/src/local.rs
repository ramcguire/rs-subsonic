//! Serve a local file, honouring a single byte range.

use std::io::{self, SeekFrom};
use std::path::Path;

use futures_util::StreamExt;
use rsub_core::backend::{BoxStream, ByteRange};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::range::resolve_range;

const CHUNK: usize = 64 * 1024;

pub struct LocalFile {
    /// 200, 206 or 416.
    pub status: u16,
    pub total_len: u64,
    pub content_length: u64,
    /// `bytes a-b/len` for 206, `bytes */len` for 416.
    pub content_range: Option<String>,
    /// Weak validator from size and mtime.
    pub etag: String,
    /// The mtime as an HTTP date, when the filesystem has one.
    pub last_modified: Option<String>,
    pub body: BoxStream<'static, io::Result<bytes::Bytes>>,
}

pub async fn open_local(path: &Path, range: Option<ByteRange>) -> io::Result<LocalFile> {
    let mut file = tokio::fs::File::open(path).await?;
    let meta = file.metadata().await?;
    let len = meta.len();
    let modified = meta.modified().ok();
    let mtime = modified
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let etag = format!("W/\"{len:x}-{mtime:x}\"");
    let last_modified = modified.map(httpdate::fmt_http_date);

    let Some(range) = range else {
        return Ok(LocalFile {
            status: 200,
            total_len: len,
            content_length: len,
            content_range: None,
            etag,
            last_modified,
            body: ReaderStream::with_capacity(file, CHUNK).boxed(),
        });
    };
    match resolve_range(range, len) {
        Ok((start, end)) => {
            file.seek(SeekFrom::Start(start)).await?;
            let n = end - start + 1;
            Ok(LocalFile {
                status: 206,
                total_len: len,
                content_length: n,
                content_range: Some(format!("bytes {start}-{end}/{len}")),
                etag,
                last_modified,
                body: ReaderStream::with_capacity(file.take(n), CHUNK).boxed(),
            })
        }
        Err(_) => Ok(LocalFile {
            status: 416,
            total_len: len,
            content_length: 0,
            content_range: Some(format!("bytes */{len}")),
            etag,
            last_modified,
            body: futures_util::stream::empty().boxed(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(f: LocalFile) -> Vec<u8> {
        let mut out = Vec::new();
        let mut b = f.body;
        while let Some(c) = b.next().await {
            out.extend_from_slice(&c.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn serves_ranges() {
        let dir = std::env::temp_dir().join(format!("rsub-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.bin");
        let data: Vec<u8> = (0..=255u8).cycle().take(200_000).collect();
        std::fs::write(&path, &data).unwrap();

        let full = open_local(&path, None).await.unwrap();
        assert_eq!((full.status, full.content_length), (200, 200_000));
        assert_eq!(read(full).await, data);

        let r = open_local(
            &path,
            Some(ByteRange::From {
                start: 70_000,
                end: Some(140_000),
            }),
        )
        .await
        .unwrap();
        assert_eq!(r.status, 206);
        assert_eq!(
            r.content_range.as_deref(),
            Some("bytes 70000-140000/200000")
        );
        assert_eq!(read(r).await, &data[70_000..=140_000]);

        let tail = open_local(&path, Some(ByteRange::Suffix(500)))
            .await
            .unwrap();
        assert_eq!(read(tail).await, &data[199_500..]);

        let bad = open_local(
            &path,
            Some(ByteRange::From {
                start: 200_000,
                end: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(bad.status, 416);
        assert_eq!(bad.content_range.as_deref(), Some("bytes */200000"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
