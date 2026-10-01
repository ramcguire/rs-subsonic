//! Map file paths as the backend sees them to paths on this machine.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct PathMapper {
    /// `(remote prefix split into segments, local root)`, longest prefix first.
    maps: Vec<(Vec<String>, PathBuf)>,
    /// The remote prefixes as configured.
    remotes: Vec<String>,
}

/// Split on both separators so Windows and Unix backend paths work alike.
fn segments(p: &str) -> Vec<String> {
    p.split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .map(str::to_owned)
        .collect()
}

impl PathMapper {
    pub fn new(maps: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
        let (remotes, mut maps): (Vec<String>, Vec<_>) = maps
            .into_iter()
            .map(|(remote, local)| {
                let segs = segments(&remote);
                (remote, (segs, local))
            })
            .unzip();
        maps.sort_by_key(|(r, _)| std::cmp::Reverse(r.len()));
        PathMapper { maps, remotes }
    }

    pub fn is_empty(&self) -> bool {
        self.maps.is_empty()
    }

    /// The remote prefixes, as configured.
    pub fn remote_roots(&self) -> &[String] {
        &self.remotes
    }

    /// The local roots, in no particular order.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.maps.iter().map(|(_, root)| root.as_path())
    }

    /// The local path for `remote`, if a prefix matches. Paths that would escape
    /// the local root (`..`) are rejected.
    pub fn map(&self, remote: &str) -> Option<PathBuf> {
        let segs = segments(remote);
        if segs.iter().any(|s| s == "..") {
            return None;
        }
        let (prefix, root) = self.maps.iter().find(|(prefix, _)| {
            segs.len() > prefix.len()
                && prefix
                    .iter()
                    .zip(&segs)
                    .all(|(a, b)| a == b || (is_drive(a) && a.eq_ignore_ascii_case(b)))
        })?;
        let mut out = root.clone();
        for s in &segs[prefix.len()..] {
            out.push(s);
        }
        // Belt and braces: the result must stay under the root.
        let rel = out.strip_prefix(root).ok()?;
        rel.components()
            .all(|c| matches!(c, Component::Normal(_)))
            .then_some(out)
    }
}

/// `C:`-style drive segments compare case-insensitively.
fn is_drive(s: &str) -> bool {
    s.len() == 2 && s.ends_with(':')
}

/// Whether `path` is an existing regular file with the expected size.
pub async fn verify(path: &Path, expected_size: Option<u64>) -> Option<std::fs::Metadata> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    (meta.is_file() && expected_size.is_none_or(|s| s == meta.len())).then_some(meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_prefixes() {
        let m = PathMapper::new([
            ("/data/music".to_string(), PathBuf::from("/mnt/music")),
            (
                "/data/music/classical/".to_string(),
                PathBuf::from("/mnt/classical"),
            ),
            ("C:\\Music".to_string(), PathBuf::from("/srv/win")),
        ]);
        assert_eq!(
            m.map("/data/music/A/b.flac"),
            Some(PathBuf::from("/mnt/music/A/b.flac"))
        );
        assert_eq!(
            m.map("/data/music/classical/x.flac"),
            Some(PathBuf::from("/mnt/classical/x.flac"))
        );
        assert_eq!(
            m.map("c:\\Music\\A\\b.mp3"),
            Some(PathBuf::from("/srv/win/A/b.mp3"))
        );
        assert_eq!(m.map("/data/musical/x.flac"), None);
        assert_eq!(m.map("/data/music"), None);
        assert_eq!(m.map("/data/music/../../etc/passwd"), None);
        assert_eq!(m.map("/other/x.flac"), None);
    }
}
