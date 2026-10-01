//! Tags read from local media files, the source of MusicBrainz ids for
//! identity.
//!
//! The sync engine reads through [`TagReader`]; `rsub-media` implements it on
//! top of a mounted library, and tests use an in-memory one.

use std::io;

use async_trait::async_trait;

/// The identity-relevant tags of one file. Empty strings are never stored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileTags {
    /// `MUSICBRAINZ_RELEASETRACKID`: this track on this release.
    pub release_track_mbid: Option<String>,
    /// `MUSICBRAINZ_TRACKID`: the recording, shared across releases.
    pub recording_mbid: Option<String>,
    /// `MUSICBRAINZ_ALBUMID`: the release.
    pub release_mbid: Option<String>,
    pub artist_mbids: Vec<String>,
    pub album_artist_mbids: Vec<String>,
    /// The multi-value `ARTISTS` tag (falls back to `ARTIST`).
    pub artists: Vec<String>,
    /// The multi-value `ALBUMARTISTS` tag (falls back to `ALBUMARTIST`).
    pub album_artists: Vec<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub disc_no: Option<u32>,
    pub track_no: Option<u32>,
}

/// Size and modification time of a file; the tag cache is keyed by both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub mtime_ms: i64,
}

/// Reads tags of files named by their backend path.
#[async_trait]
pub trait TagReader: Send + Sync {
    /// Fails when the file is not reachable from here: unmapped (`NotFound`),
    /// missing, or behind a failing mount.
    async fn stat(&self, remote_path: &str) -> io::Result<FileStamp>;
    async fn read(&self, remote_path: &str) -> io::Result<FileTags>;
}

/// Every MBID-shaped substring of `s`, lowercased. Multi-value tags arrive
/// joined by `\0`, `/`, `;` or spaces depending on the format and tagger;
/// MBIDs are UUIDs, so extracting them sidesteps the separator question.
pub fn extract_mbids(s: &str) -> Vec<String> {
    const LEN: usize = 36;
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + LEN <= b.len() {
        let cand = &b[i..i + LEN];
        let ok = cand.iter().enumerate().all(|(j, c)| match j {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        });
        let bounded = |c: Option<&u8>| c.is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'-');
        if ok && bounded(i.checked_sub(1).and_then(|p| b.get(p))) && bounded(b.get(i + LEN)) {
            let id = String::from_utf8_lossy(cand).to_ascii_lowercase();
            if !out.contains(&id) {
                out.push(id);
            }
            i += LEN;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_mbids() {
        let a = "5b11f4ce-a62d-471e-81fc-a69a8278c7da";
        let b = "B7FFD2AF-418F-4BE2-BDD1-22F8B48613DA";
        assert_eq!(extract_mbids(a), [a]);
        for sep in ["\0", "/", "; ", " "] {
            assert_eq!(
                extract_mbids(&format!("{a}{sep}{b}")),
                [a.to_string(), b.to_lowercase()]
            );
        }
        assert_eq!(extract_mbids(&format!("{a}{a}")), Vec::<String>::new());
        assert_eq!(extract_mbids(&format!("{a} {a}")), [a]);
        assert!(extract_mbids("not-an-mbid").is_empty());
        assert!(extract_mbids(&a[1..]).is_empty());
    }
}
