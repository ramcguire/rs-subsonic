//! `bench`: analyse files the way the server's engine does and report
//! throughput, CPU, peak memory and neighbour quality.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use argh::FromArgs;
use rsub_analysis::Analyzer;

use crate::index::SonicIndex;
use crate::{Error, analyzer, usage, walk};

/// Neighbours per file scored for label agreement.
const NEIGHBOURS: usize = 10;

/// Analyse files and report speed, resource use and neighbour quality.
#[derive(FromArgs)]
#[argh(subcommand, name = "bench")]
pub struct Bench {
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
    /// files analysed at once (default 1, which measures the cost per file)
    #[argh(option, default = "1")]
    threads: usize,
    /// a file listing audio paths, one per line
    #[argh(option)]
    list: Option<PathBuf>,
    /// analyse this many of the files, chosen by a fixed hash of each path
    #[argh(option)]
    sample: Option<usize>,
    /// tab-separated `path, labels (';'-separated)[, group]`: score how many
    /// of each file's nearest neighbours share a label, leaving out its group
    /// (the artist, say)
    #[argh(option)]
    labels: Option<PathBuf>,
    /// write each vector as a `{"path", "v"}` JSON line
    #[argh(option)]
    vectors: Option<PathBuf>,
    /// audio files, or directories to search for them
    #[argh(positional)]
    paths: Vec<PathBuf>,
}

/// Every audio file named, found under a named directory, or listed.
fn collect(b: &Bench) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    for p in &b.paths {
        walk(p, &mut files)?;
    }
    if let Some(list) = &b.list {
        let text = std::fs::read_to_string(list).map_err(|e| format!("{}: {e}", list.display()))?;
        files.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(PathBuf::from),
        );
    }
    if let Some(n) = b.sample {
        files.sort_by_key(|f| fnv(f.to_string_lossy().as_bytes()));
        files.truncate(n);
    }
    Ok(files)
}

/// FNV-1a: a hash that stays the same across builds, for `--sample`.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
    })
}

/// A file's index, how long it took, and its vector or error.
type Timed = (usize, Duration, Result<Vec<f32>, String>);

pub fn run(b: Bench) -> Result<(), Error> {
    let (analyzer, load) = analyzer(
        b.analyzer.as_deref(),
        b.model.as_deref(),
        b.crops,
        b.seconds,
    )?;
    let files = collect(&b)?;
    if files.is_empty() {
        return Err("no audio files given".into());
    }
    let threads = b.threads.max(1);
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Timed>> = Mutex::new(Vec::new());
    let started = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(f) = files.get(i) else { break };
                    let t = Instant::now();
                    let r = analyzer.analyze(f).map_err(|e| e.to_string());
                    results.lock().unwrap().push((i, t.elapsed(), r));
                }
            });
        }
    });
    let wall = started.elapsed();
    let usage = usage::usage();
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.0);

    let mut times = Vec::new();
    let mut vectors = Vec::new();
    for (i, d, r) in results {
        match r {
            Ok(v) => {
                times.push(d.as_secs_f64());
                vectors.push((i as i64, v));
            }
            Err(e) => eprintln!("failed: {}: {e}", files[i].display()),
        }
    }
    times.sort_by(f64::total_cmp);
    let pct = |p: f64| times[((times.len() as f64 - 1.0) * p).round() as usize];

    println!(
        "analyzer   {} (loaded in {:.2} s)",
        analyzer.id(),
        load.as_secs_f64()
    );
    println!(
        "files      {} ({} analysed, {} failed), {threads} at a time",
        files.len(),
        vectors.len(),
        files.len() - vectors.len()
    );
    println!(
        "wall       {:.1} s, {:.2} files/s",
        wall.as_secs_f64(),
        files.len() as f64 / wall.as_secs_f64()
    );
    if !times.is_empty() {
        println!(
            "per file   mean {:.2} s, p50 {:.2} s, p95 {:.2} s",
            times.iter().sum::<f64>() / times.len() as f64,
            pct(0.5),
            pct(0.95)
        );
    }
    if let Some(u) = usage {
        let cpu = u.cpu.as_secs_f64();
        println!(
            "cpu        {cpu:.0} s, {:.2} s a file, {:.1} cores busy",
            cpu / files.len() as f64,
            cpu / wall.as_secs_f64()
        );
        println!("peak mem   {} MB", u.peak_bytes >> 20);
    }
    if let Some(labels) = &b.labels {
        quality(analyzer.as_ref(), &files, &vectors, labels)?;
    }
    if let Some(out) = &b.vectors {
        let mut w = std::io::BufWriter::new(std::fs::File::create(out)?);
        for (i, v) in &vectors {
            let path = files[*i as usize]
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            let nums: Vec<String> = v.iter().map(f32::to_string).collect();
            writeln!(w, "{{\"path\":\"{path}\",\"v\":[{}]}}", nums.join(","))?;
        }
    }
    Ok(())
}

/// How often a file's nearest neighbours (outside its group) share one of its
/// labels, against neighbours picked at random.
fn quality(
    analyzer: &dyn Analyzer,
    files: &[PathBuf],
    vectors: &[(i64, Vec<f32>)],
    labels: &Path,
) -> Result<(), Error> {
    let text = std::fs::read_to_string(labels).map_err(|e| format!("{}: {e}", labels.display()))?;
    let by_path: HashMap<&str, (Vec<&str>, &str)> = text
        .lines()
        .filter_map(|l| {
            let mut c = l.split('\t');
            let path = c.next()?.trim();
            let labels = c
                .next()?
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            Some((path, (labels, c.next().unwrap_or("").trim())))
        })
        .collect();
    let info: HashMap<i64, &(Vec<&str>, &str)> = vectors
        .iter()
        .filter_map(|(i, _)| Some((*i, by_path.get(files[*i as usize].to_str()?)?)))
        .collect();
    let other = |a: i64, b: i64| {
        let (ga, gb) = (info.get(&a).map(|x| x.1), info.get(&b).map(|x| x.1));
        ga.is_none_or(|g| g.is_empty() || Some(g) != gb)
    };
    let share = |a: i64, b: i64| {
        let (la, lb) = (&info[&a].0, &info[&b].0);
        la.iter().any(|l| lb.contains(l))
    };
    let index = SonicIndex::new(analyzer.metric(), vectors.iter().cloned());
    let labelled: Vec<i64> = info
        .iter()
        .filter(|(_, x)| !x.0.is_empty())
        .map(|(i, _)| *i)
        .collect();
    let (mut near, mut rand) = (Vec::new(), Vec::new());
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    for &i in &labelled {
        let n = index
            .similar(i, NEIGHBOURS * 4, |j| other(i, j))
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.track_id)
            .filter(|j| info.get(j).is_some_and(|x| !x.0.is_empty()))
            .take(NEIGHBOURS)
            .collect::<Vec<_>>();
        if n.is_empty() {
            continue;
        }
        near.push(n.iter().filter(|&&j| share(i, j)).count() as f64 / n.len() as f64);
        let pool: Vec<i64> = labelled
            .iter()
            .copied()
            .filter(|&j| j != i && other(i, j))
            .collect();
        let picks = (0..NEIGHBOURS).map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            pool[(seed % pool.len() as u64) as usize]
        });
        rand.push(picks.filter(|&j| share(i, j)).count() as f64 / NEIGHBOURS as f64);
    }
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    println!(
        "quality    {:.0}% of top-{NEIGHBOURS} neighbours share a label (random {:.0}%), over {} labelled files",
        mean(&near) * 100.0,
        mean(&rand) * 100.0,
        near.len()
    );
    Ok(())
}
