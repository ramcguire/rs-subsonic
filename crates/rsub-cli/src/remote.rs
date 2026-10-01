//! `push` and `pull`: move analysis to and from a server's admin API
//! (`/api/v1/analysis`), authenticated with an admin's API key; and [`Api`],
//! the admin API client `admin` shares.

use std::path::PathBuf;
use std::time::Duration;

use argh::FromArgs;
use reqwest::blocking::{Client, Response};
use rsub_analysis::transfer::{self, Header};
use serde::Deserialize;

use crate::Error;

/// Where the key is read from when `--key` isn't given.
const KEY_ENV: &str = "RSUB_API_KEY";
/// Where the server URL is read from when `--server` isn't given.
const SERVER_ENV: &str = "RSUB_SERVER";

/// Send a transfer file's analysis to a server.
#[derive(FromArgs)]
#[argh(subcommand, name = "push")]
pub struct Push {
    /// the server's base URL, e.g. http://music.local:4533
    #[argh(option)]
    server: String,
    /// an admin's API key (default: the RSUB_API_KEY environment variable)
    #[argh(option)]
    key: Option<String>,
    /// the server path the file's paths are relative to (default: the
    /// server's only path_map root)
    #[argh(option)]
    remote_root: Option<String>,
    /// rows per request
    #[argh(option, default = "1000")]
    batch: usize,
    /// leave out files that couldn't be analysed, so the server tries them
    #[argh(switch)]
    skip_failed: bool,
    /// report what would be stored without storing it
    #[argh(switch)]
    dry_run: bool,
    /// the transfer file `scan` wrote
    #[argh(positional)]
    file: PathBuf,
}

/// Fetch a server's analysis into a transfer file.
#[derive(FromArgs)]
#[argh(subcommand, name = "pull")]
pub struct Pull {
    /// the server's base URL, e.g. http://music.local:4533
    #[argh(option)]
    server: String,
    /// an admin's API key (default: the RSUB_API_KEY environment variable)
    #[argh(option)]
    key: Option<String>,
    /// the analyzer id (default: the one the server runs)
    #[argh(option)]
    analyzer: Option<String>,
    /// the server path to export under (default: its only path_map root)
    #[argh(option)]
    root: Option<String>,
    /// the transfer file to write
    #[argh(option, short = 'o')]
    out: PathBuf,
}

#[derive(Deserialize)]
struct Status {
    analyzer: Option<String>,
    roots: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Imported {
    stored: u64,
    failed: u64,
    size_mismatch: u64,
    unknown: u64,
}

/// A server's admin API (`/api/v1`), with an admin's API key.
pub struct Api {
    pub http: Client,
    /// `<server>/api/v1`.
    pub base: String,
    key: String,
}

impl Api {
    /// `server` and `key` default to `RSUB_SERVER` and `RSUB_API_KEY`.
    pub fn connect(server: Option<String>, key: Option<String>) -> Result<Api, Error> {
        let server = server
            .or_else(|| std::env::var(SERVER_ENV).ok())
            .filter(|s| !s.trim().is_empty())
            .ok_or(format!(
                "which server? --server or {SERVER_ENV}, e.g. http://music.local:4533"
            ))?;
        Api::new(server.trim(), key)
    }

    fn new(server: &str, key: Option<String>) -> Result<Api, Error> {
        let key = key
            .or_else(|| std::env::var(KEY_ENV).ok())
            .filter(|k| !k.trim().is_empty())
            .ok_or(format!("an API key is needed: --key or {KEY_ENV}"))?;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Api {
            http,
            base: format!("{}/api/v1", server.trim_end_matches('/')),
            key: key.trim().to_owned(),
        })
    }

    /// `<server>/api/v1/<path>`.
    pub fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }

    /// Send `req` with the key; a failure status is an error with the
    /// server's message.
    pub fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Response, Error> {
        let resp = req.bearer_auth(&self.key).send()?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or(body);
        Err(format!("server: {status}: {msg}").into())
    }

    fn status(&self) -> Result<Status, Error> {
        Ok(self.send(self.http.get(self.url("analysis")))?.json()?)
    }
}

pub fn push(p: Push) -> Result<(), Error> {
    let text =
        std::fs::read_to_string(&p.file).map_err(|e| format!("{}: {e}", p.file.display()))?;
    let (head, rows) = transfer::parse(&text).map_err(|e| format!("{}: {e}", p.file.display()))?;
    let api = Api::new(&p.server, p.key)?;
    let status = api.status()?;
    let root = match (p.remote_root, status.roots.as_slice()) {
        (Some(r), _) => r,
        (None, [only]) => only.clone(),
        (None, []) => {
            return Err("the server has no path_map: nothing to match files against".into());
        }
        (None, roots) => {
            return Err(format!(
                "the server has several roots; pick one with --remote-root: {}",
                roots.join(", ")
            )
            .into());
        }
    };
    match &status.analyzer {
        Some(a) if *a == head.analyzer => {}
        Some(a) => eprintln!(
            "note: the server runs {a}; these {} vectors are stored but unused until it runs that",
            head.analyzer
        ),
        None => {
            eprintln!("note: analysis is off on the server; vectors are stored for when it's on")
        }
    }
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| !(p.skip_failed && r.v.is_none()))
        .collect();
    let wire = Header::new(&head.analyzer, &root);
    let url = format!("{}?dry_run={}", api.url("analysis/vectors"), p.dry_run);
    let batch = p.batch.max(1);
    let mut total = Imported::default();
    for (i, chunk) in rows.chunks(batch).enumerate() {
        let mut body = transfer::line(&wire);
        for r in chunk {
            body.push_str(&transfer::line(r));
        }
        let got: Imported = api
            .send(
                api.http
                    .post(&url)
                    .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
                    .body(body),
            )?
            .json()?;
        total.stored += got.stored;
        total.failed += got.failed;
        total.size_mismatch += got.size_mismatch;
        total.unknown += got.unknown;
        eprintln!(
            "{}/{} rows sent",
            (i * batch + chunk.len()).min(rows.len()),
            rows.len()
        );
    }
    println!(
        "{}{} stored, {} stored as not analysable, {} changed since analysis (size differs), {} not in the library under {root}",
        if p.dry_run { "dry run: " } else { "" },
        total.stored,
        total.failed,
        total.size_mismatch,
        total.unknown
    );
    Ok(())
}

pub fn pull(p: Pull) -> Result<(), Error> {
    let api = Api::new(&p.server, p.key)?;
    let mut req = api.http.get(api.url("analysis/vectors"));
    if let Some(a) = &p.analyzer {
        req = req.query(&[("analyzer", a)]);
    }
    if let Some(r) = &p.root {
        req = req.query(&[("root", r)]);
    }
    let body = api.send(req)?.text()?;
    let (head, rows) = transfer::parse(&body)?;
    std::fs::write(&p.out, body)?;
    println!(
        "{}: {} files of {} under {}",
        p.out.display(),
        rows.len(),
        head.analyzer,
        head.root
    );
    Ok(())
}
