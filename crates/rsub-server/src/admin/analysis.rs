//! `/api/v1/analysis`: moving audio analysis in and out, with the `sonic`
//! feature.

use std::collections::{BTreeSet, HashMap};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rsub_analysis::transfer::{self, Header, Row};
use rsub_store::{AnalysisWrite, AnalyzerCounts, FileStampRow, FileToAnalyze};
use serde::{Deserialize, Serialize};

use super::{Error, admin, json};
use crate::AppState;

/// Largest import body: a push sends batches well under this.
const MAX_IMPORT: usize = 64 << 20;

pub fn router() -> Router<AppState> {
    Router::new().route("/analysis", get(status)).route(
        "/analysis/vectors",
        get(export)
            .post(import)
            .layer(DefaultBodyLimit::max(MAX_IMPORT)),
    )
}

/// The remote path prefixes of every backend's `path_map`.
fn roots(state: &AppState) -> Vec<String> {
    let set: BTreeSet<&String> = state
        .sources
        .values()
        .flat_map(|s| s.paths.remote_roots())
        .collect();
    set.into_iter().cloned().collect()
}

#[derive(Serialize)]
struct Status {
    /// The analyzer the server runs, `null` with analysis off.
    analyzer: Option<String>,
    /// Roots the paths of an import may be relative to.
    roots: Vec<String>,
    /// Stored results of each analyzer, for files of live tracks.
    analyzers: Vec<AnalyzerCounts>,
}

async fn status(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let analyzers = state.db.analyzer_counts().await?;
    Ok(json(&Status {
        analyzer: state.analysis.as_ref().map(|e| e.analyzer_id().to_owned()),
        roots: roots(&state),
        analyzers,
    }))
}

#[derive(Deserialize)]
struct ImportQuery {
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize, Default)]
struct Imported {
    /// Vectors stored.
    stored: u64,
    /// Files stored as not analysable.
    failed: u64,
    /// Files whose size differs from the library's: changed since analysis.
    size_mismatch: u64,
    /// Paths that aren't library files with read tags.
    unknown: u64,
}

/// `root` joined with a `/`-separated relative path, in the root's style.
fn join(root: &str, rel: &str) -> Option<String> {
    if rel.split('/').any(|s| s == ".." || s.is_empty()) {
        return None;
    }
    let sep = if root.contains('/') || !root.contains('\\') {
        "/"
    } else {
        "\\"
    };
    Some(format!(
        "{}{sep}{}",
        root.trim_end_matches(['/', '\\']),
        rel.replace('/', sep)
    ))
}

/// Store the vectors of a transfer document ([`transfer`]) whose root is a
/// backend path prefix. A row is matched to the library file at its path and
/// taken when the sizes agree; it's stored with the file's tag-pass stamp, so
/// the analysis engine counts it as current. Vectors of any analyzer are
/// accepted, analysis on or off; the server uses those of its own analyzer.
async fn import(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ImportQuery>,
    body: Bytes,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let text = std::str::from_utf8(&body).map_err(|_| Error::bad("body isn't UTF-8"))?;
    let (head, rows) = transfer::parse(text).map_err(Error::bad)?;
    let mut dim = None;
    let mut wanted: Vec<(String, u64, Option<Vec<f32>>)> = Vec::with_capacity(rows.len());
    let mut out = Imported::default();
    for row in rows {
        let vector = row.vector().map_err(Error::bad)?;
        if let Some(v) = &vector
            && *dim.get_or_insert(v.len()) != v.len()
        {
            return Err(Error::bad(format!("{}: vector lengths differ", row.path)));
        }
        match join(&head.root, &row.path) {
            Some(path) => wanted.push((path, row.size, vector)),
            None => out.unknown += 1,
        }
    }
    let paths: Vec<String> = wanted.iter().map(|w| w.0.clone()).collect();
    // A path can be a file of more than one library.
    let mut stamps: HashMap<String, Vec<FileStampRow>> = HashMap::new();
    for s in state.db.file_stamps(&paths).await? {
        stamps.entry(s.remote_path.clone()).or_default().push(s);
    }
    let mut writes: HashMap<i64, Vec<AnalysisWrite>> = HashMap::new();
    for (path, size, vector) in wanted {
        let Some(files) = stamps.get(&path) else {
            out.unknown += 1;
            continue;
        };
        let mut matched = false;
        for f in files.iter().filter(|f| f.size == size as i64) {
            matched = true;
            writes.entry(f.library_id).or_default().push(AnalysisWrite {
                file: FileToAnalyze {
                    remote_path: f.remote_path.clone(),
                    size: f.size,
                    mtime: f.mtime,
                },
                vector: vector.clone(),
            });
        }
        match (matched, &vector) {
            (false, _) => out.size_mismatch += 1,
            (true, Some(_)) => out.stored += 1,
            (true, None) => out.failed += 1,
        }
    }
    if !q.dry_run {
        for (lib, w) in &writes {
            state.db.write_analysis(*lib, &head.analyzer, w).await?;
        }
        if let Some(engine) = &state.analysis
            && engine.analyzer_id() == head.analyzer
            && !writes.is_empty()
        {
            // A pass finds nothing to do for these files and reloads the index.
            engine.trigger();
        }
    }
    Ok(json(&out))
}

#[derive(Deserialize)]
struct ExportQuery {
    /// Default: the server's analyzer.
    analyzer: Option<String>,
    /// Default: the only root.
    root: Option<String>,
}

/// Every stored result of an analyzer under a root, as a transfer document.
async fn export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ExportQuery>,
) -> Result<Response, Error> {
    admin(&state, &headers).await?;
    let analyzer = q
        .analyzer
        .or_else(|| state.analysis.as_ref().map(|e| e.analyzer_id().to_owned()))
        .ok_or_else(|| Error::bad("analysis is off: name an analyzer"))?;
    let roots = roots(&state);
    let root = match (q.root, roots.as_slice()) {
        (Some(r), _) => r,
        (None, [only]) => only.clone(),
        (None, _) => {
            return Err(Error::bad(format!(
                "name a root, one of: {}",
                roots.join(", ")
            )));
        }
    };
    let prefix = format!("{}/", root.trim_end_matches(['/', '\\']).replace('\\', "/"));
    let mut body = transfer::line(&Header::new(&analyzer, &root));
    let mut seen = BTreeSet::new();
    for a in state.db.stored_analysis(&analyzer).await? {
        let path = a.remote_path.replace('\\', "/");
        let Some(rel) = path.strip_prefix(&prefix) else {
            continue;
        };
        if !seen.insert(rel.to_owned()) {
            continue;
        }
        let size = a.size as u64;
        let row = match &a.vector {
            Some(v) => Row::analysed(rel, size, &v.0),
            None => Row::failed(rel, size, "not analysable"),
        };
        body.push_str(&transfer::line(&row));
    }
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-ndjson"),
        )],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_in_the_roots_style() {
        assert_eq!(join("/music/", "a/b.flac").unwrap(), "/music/a/b.flac");
        assert_eq!(
            join("D:\\Music", "a/b.flac").unwrap(),
            "D:\\Music\\a\\b.flac"
        );
        assert!(join("/music", "a/../../etc").is_none());
        assert!(join("/music", "/abs").is_none());
    }
}
