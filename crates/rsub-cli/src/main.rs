//! `rsub-cli`: rs-subsonic's companion CLI. It runs the audio analyzers
//! outside the server, and administers a server through its admin API.
//!
//! - `bench` analyses files the way the server's engine does and reports
//!   throughput, CPU, peak memory and neighbour quality.
//! - `scan` analyses a folder into a transfer file (`rsub_analysis::transfer`),
//!   so a faster machine can do the first pass; `push` sends it to a server's
//!   admin API, and `pull` fetches a server's analysis.
//! - `user` and `backup` administer users and take or restore backups
//!   (`admin`).

mod admin;
mod bench;
mod index;
mod remote;
mod scan;
mod usage;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use argh::FromArgs;
use rsub_analysis::{Analyzer, Settings};

/// Extensions looked for when a directory is given.
const AUDIO: &[&str] = &[
    "flac", "mp3", "m4a", "mp4", "aac", "ogg", "oga", "opus", "wav", "aif", "aiff", "caf", "mka",
    "webm",
];

/// rs-subsonic's companion CLI.
#[derive(FromArgs)]
struct Cli {
    #[argh(subcommand)]
    cmd: Cmd,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Cmd {
    Bench(bench::Bench),
    Scan(scan::Scan),
    Push(remote::Push),
    Pull(remote::Pull),
    User(admin::User),
    Backup(admin::Backup),
}

fn main() -> ExitCode {
    let cli: Cli = argh::from_env();
    let result = match cli.cmd {
        Cmd::Bench(b) => bench::run(b),
        Cmd::Scan(s) => scan::run(s),
        Cmd::Push(p) => remote::push(p),
        Cmd::Pull(p) => remote::pull(p),
        Cmd::User(u) => admin::user(u),
        Cmd::Backup(b) => admin::backup(b),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type Error = Box<dyn std::error::Error>;

/// The analyzer the options name, and how long it took to load.
pub fn analyzer(
    name: Option<&str>,
    model: Option<&Path>,
    crops: usize,
    seconds: u32,
) -> Result<(Arc<dyn Analyzer>, Duration), Error> {
    let t = Instant::now();
    let analyzer = Settings {
        analyzer: name.map(str::parse).transpose()?,
        clap_model: model.map(Path::to_owned),
        clap_crops: crops,
        bliss_seconds: seconds,
    }
    .build()?;
    Ok((analyzer, t.elapsed()))
}

/// `p` if it's a file, else the audio files under it, in path order.
pub fn walk(p: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !p.is_dir() {
        out.push(p.to_owned());
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(p)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for e in entries {
        if e.is_dir() {
            walk(&e, out)?;
        } else if e
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| AUDIO.contains(&x.to_ascii_lowercase().as_str()))
        {
            out.push(e);
        }
    }
    Ok(())
}
