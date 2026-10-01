//! `scan`: analyse a folder into a transfer file, resuming one already
//! started. Every line is written whole and flushed, so an interrupted scan
//! loses at most the files in flight; a later scan skips files already in the
//! file with the same size.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use argh::FromArgs;
use rsub_analysis::AnalysisError;
use rsub_analysis::transfer::{self, Header, Row};

use crate::{Error, analyzer, walk};

/// How often progress is reported.
const PROGRESS: Duration = Duration::from_secs(10);

/// Analyse a folder into a transfer file for `push`, resuming if it exists.
#[derive(FromArgs)]
#[argh(subcommand, name = "scan")]
pub struct Scan {
    /// bliss or clap (default: clap when built in)
    #[argh(option)]
    analyzer: Option<String>,
    /// clap: the exported model (tools/export_clap.py in rsub-analysis)
    #[argh(option)]
    model: Option<PathBuf>,
    /// clap: 10 s crops per track, 1 to 4
    #[argh(option, default = "4")]
    crops: usize,
    /// bliss: seconds from the middle of each track (0 = the whole track)
    #[argh(option, default = "0")]
    seconds: u32,
    /// files analysed at once (default: one per CPU)
    #[argh(option)]
    threads: Option<usize>,
    /// the transfer file to write (JSON lines)
    #[argh(option, short = 'o')]
    out: PathBuf,
    /// the music folder, the one a server's path_map points at
    #[argh(positional)]
    root: PathBuf,
}

/// `path` relative to `root`, `/`-separated.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// The rows already in `out`, after checking it was written by this analyzer
/// for this root. A last line cut short by an interruption is dropped.
fn resume(out: &Path, header: &Header) -> Result<HashMap<String, u64>, Error> {
    let mut text = std::fs::read_to_string(out).map_err(|e| format!("{}: {e}", out.display()))?;
    if !text.ends_with('\n') {
        text.truncate(text.rfind('\n').map_or(0, |i| i + 1));
        std::fs::write(out, &text)?;
    }
    if text.is_empty() {
        std::fs::write(out, transfer::line(header))?;
        return Ok(HashMap::new());
    }
    let (found, rows) = transfer::parse(&text).map_err(|e| format!("{}: {e}", out.display()))?;
    if found.analyzer != header.analyzer || found.root != header.root {
        return Err(format!(
            "{} holds {} results for {}; write {} results for {} to another file",
            out.display(),
            found.analyzer,
            found.root,
            header.analyzer,
            header.root
        )
        .into());
    }
    Ok(rows.into_iter().map(|r| (r.path, r.size)).collect())
}

pub fn run(s: Scan) -> Result<(), Error> {
    if !s.root.is_dir() {
        return Err(format!("{} isn't a folder", s.root.display()).into());
    }
    let (analyzer, _) = analyzer(
        s.analyzer.as_deref(),
        s.model.as_deref(),
        s.crops,
        s.seconds,
    )?;
    let header = Header::new(analyzer.id(), &s.root.to_string_lossy());
    let done = if s.out.exists() {
        resume(&s.out, &header)?
    } else {
        std::fs::write(&s.out, transfer::line(&header))?;
        HashMap::new()
    };

    let mut found = Vec::new();
    walk(&s.root, &mut found).map_err(|e| format!("{}: {e}", s.root.display()))?;
    let total = found.len();
    let todo: Vec<(PathBuf, String, u64)> = found
        .into_iter()
        .filter_map(|p| {
            let rel = relative(&s.root, &p)?;
            let size = std::fs::metadata(&p).ok()?.len();
            (done.get(&rel) != Some(&size)).then_some((p, rel, size))
        })
        .collect();
    eprintln!(
        "{}: {total} audio files, {} to analyse with {}",
        s.root.display(),
        todo.len(),
        analyzer.id()
    );
    if todo.is_empty() {
        return Ok(());
    }

    let threads = s
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .max(1);
    let mut file = OpenOptions::new().append(true).open(&s.out)?;
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Option<Row>>();
    let started = Instant::now();
    let (mut written, mut failed, mut unreadable) = (0usize, 0usize, 0usize);
    std::thread::scope(|scope| -> Result<(), Error> {
        for _ in 0..threads {
            let tx = tx.clone();
            let (todo, next, analyzer) = (&todo, &next, &analyzer);
            scope.spawn(move || {
                while let Some((path, rel, size)) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let row = match analyzer.analyze(path) {
                        Ok(v) => Some(Row::analysed(rel, *size, &v)),
                        // Not recorded: tried again by the next scan.
                        Err(AnalysisError::Io(e)) => {
                            eprintln!("unreadable: {}: {e}", path.display());
                            None
                        }
                        Err(e) => {
                            eprintln!("failed: {}: {e}", path.display());
                            Some(Row::failed(rel, *size, &e.to_string()))
                        }
                    };
                    if tx.send(row).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut reported = Instant::now();
        for row in rx {
            match row {
                Some(r) => {
                    if r.v.is_none() {
                        failed += 1;
                    }
                    file.write_all(transfer::line(&r).as_bytes())?;
                    file.flush()?;
                    written += 1;
                }
                None => unreadable += 1,
            }
            if reported.elapsed() >= PROGRESS {
                reported = Instant::now();
                let n = written + unreadable;
                let rate = n as f64 / started.elapsed().as_secs_f64();
                let eta = (todo.len() - n) as f64 / rate.max(1e-9);
                eprintln!(
                    "{n}/{} files, {rate:.1}/s, about {:.0} min left",
                    todo.len(),
                    eta / 60.0
                );
            }
        }
        Ok(())
    })?;
    eprintln!(
        "done in {:.0} s: {} analysed, {failed} not analysable, {unreadable} unreadable (tried again next scan)",
        started.elapsed().as_secs_f64(),
        written - failed
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_use_slashes() {
        let root = Path::new("music");
        assert_eq!(
            relative(root, &root.join("A").join("b.flac")).as_deref(),
            Some("A/b.flac")
        );
        assert_eq!(relative(root, Path::new("elsewhere/b.flac")), None);
    }

    #[test]
    fn resumes_and_drops_a_cut_line() {
        let dir = std::env::temp_dir().join(format!("rsub-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("scan.ndjson");
        let header = Header::new("fake-1", "music");
        let mut text = transfer::line(&header);
        text += &transfer::line(&Row::analysed("a.flac", 3, &[1.0]));
        text += "{\"path\":\"b.fl";
        std::fs::write(&out, text).unwrap();
        let done = resume(&out, &header).unwrap();
        assert_eq!(done, HashMap::from([("a.flac".to_string(), 3)]));
        assert!(std::fs::read_to_string(&out).unwrap().ends_with("\n"));
        assert!(resume(&out, &Header::new("other-1", "music")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
