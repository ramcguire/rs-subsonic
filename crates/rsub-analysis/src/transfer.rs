//! The analysis transfer format: JSON lines, the same in a file written by
//! `rsub-cli scan` and in the admin API's import and export bodies.
//!
//! The first line is a [`Header`]: the analyzer id and the root the paths are
//! relative to (a local folder in a scan file, the backend's path prefix on
//! the wire). Each further line is a [`Row`]: a path relative to the root with
//! `/` separators, the file's size, and its vector as base64 of little-endian
//! `f32`s, or `null` with the error when the file couldn't be analysed. A path
//! may appear more than once (a scan appends when a file changes); the last
//! row wins.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "rsub-analysis";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub format: String,
    pub version: u32,
    /// [`crate::Analyzer::id`]: vectors are only comparable under one id.
    pub analyzer: String,
    pub root: String,
}

impl Header {
    pub fn new(analyzer: &str, root: &str) -> Header {
        Header {
            format: FORMAT.into(),
            version: VERSION,
            analyzer: analyzer.into(),
            root: root.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub path: String,
    pub size: u64,
    /// Base64 of the vector's little-endian `f32`s; `None` when the file
    /// couldn't be analysed.
    pub v: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Row {
    pub fn analysed(path: &str, size: u64, vector: &[f32]) -> Row {
        let bytes: Vec<u8> = vector.iter().flat_map(|x| x.to_le_bytes()).collect();
        Row {
            path: path.into(),
            size,
            v: Some(STANDARD.encode(bytes)),
            error: None,
        }
    }

    pub fn failed(path: &str, size: u64, error: &str) -> Row {
        Row {
            path: path.into(),
            size,
            v: None,
            error: Some(error.into()),
        }
    }

    /// The vector, `None` for a failed file.
    pub fn vector(&self) -> Result<Option<Vec<f32>>, String> {
        let Some(v) = &self.v else {
            return Ok(None);
        };
        let b = STANDARD
            .decode(v)
            .map_err(|e| format!("{}: bad vector: {e}", self.path))?;
        let (floats, rest) = b.as_chunks::<4>();
        if floats.is_empty() || !rest.is_empty() {
            return Err(format!("{}: bad vector length {}", self.path, b.len()));
        }
        Ok(Some(
            floats.iter().map(|c| f32::from_le_bytes(*c)).collect(),
        ))
    }
}

/// One JSON line, with its newline.
pub fn line<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string(value).expect("serializable");
    s.push('\n');
    s
}

/// Parse a whole document: the header, then the rows with the last row for a
/// path winning, in first-seen order. Blank lines are skipped.
pub fn parse(text: &str) -> Result<(Header, Vec<Row>), String> {
    let mut lines = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty());
    let (_, first) = lines.next().ok_or("empty analysis document")?;
    let header: Header =
        serde_json::from_str(first).map_err(|e| format!("line 1: not a header: {e}"))?;
    if header.format != FORMAT {
        return Err(format!("not an {FORMAT} document"));
    }
    if header.version != VERSION {
        return Err(format!(
            "{FORMAT} version {} isn't supported (only {VERSION})",
            header.version
        ));
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (n, l) in lines {
        let row: Row = serde_json::from_str(l).map_err(|e| format!("line {}: {e}", n + 1))?;
        match at.get(&row.path) {
            Some(&i) => rows[i] = row,
            None => {
                at.insert(row.path.clone(), rows.len());
                rows.push(row);
            }
        }
    }
    Ok((header, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut doc = line(&Header::new("fake-1", "/music"));
        doc += &line(&Row::analysed("a/One.flac", 10, &[0.5, -1.0]));
        doc += &line(&Row::failed("a/Two.mp3", 20, "cannot decode"));
        doc += "\n";
        doc += &line(&Row::analysed("a/One.flac", 11, &[1.0, 2.0]));
        let (h, rows) = parse(&doc).unwrap();
        assert_eq!(h.analyzer, "fake-1");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].size, 11);
        assert_eq!(rows[0].vector().unwrap(), Some(vec![1.0, 2.0]));
        assert_eq!(rows[1].vector().unwrap(), None);
        assert!(
            parse("{\"format\":\"x\",\"version\":1,\"analyzer\":\"a\",\"root\":\"/\"}").is_err()
        );
        let bad = Row {
            v: Some("AAA=".into()),
            ..Row::failed("x", 1, "")
        };
        assert!(bad.vector().is_err());
    }
}
