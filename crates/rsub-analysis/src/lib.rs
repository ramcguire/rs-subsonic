//! Sonic similarity from rs-subsonic's own audio analysis: an [`Analyzer`]
//! turns each mounted library file into a vector, the [`AnalysisEngine`] keeps
//! a vector per file in the store, and [`SonicSearch`] answers
//! nearest-neighbour and path queries, which the database runs (sqlite-vec or
//! pgvector).
//!
//! Two analyzers exist, each behind a cargo feature and chosen through
//! [`Settings`]: `bliss` (bliss-rs's 23 hand-built features; GPL-3.0) and
//! `clap` (512-d CLAP audio embeddings, the model subwave uses). The crate is optional in
//! rs-subsonic: either feature pulls it in.

#[cfg(feature = "bliss")]
mod bliss;
#[cfg(feature = "clap")]
mod clap;
#[cfg(feature = "decode")]
pub mod decode;
mod engine;
#[cfg(feature = "clap")]
mod mel;
mod search;
mod settings;
pub mod transfer;

use std::path::Path;

pub use engine::{AnalysisEngine, PassStats};
pub use rsub_store::Metric;
pub use search::{Match, SonicSearch};
pub use settings::{AnalyzerKind, BUILT, MAX_CLAP_CROPS, MIN_BLISS_SECONDS, Settings};

#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    /// The file can't be read from here (unmapped, missing, a failing mount).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The file was read but isn't audio this build can decode.
    #[error("cannot decode: {0}")]
    Decode(String),
    #[error("analysis failed: {0}")]
    Analysis(String),
    /// Settings that name no analyzer this build can run.
    #[error("{0}")]
    Settings(String),
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Turns an audio file into a vector. Analysis is CPU-bound and blocking.
pub trait Analyzer: Send + Sync {
    /// Names the analyzer and its version in stored results. Results stored
    /// under another id are not used, and files are analysed again.
    fn id(&self) -> &str;
    fn metric(&self) -> Metric;
    fn analyze(&self, path: &Path) -> Result<Vec<f32>, AnalysisError>;
}

/// Scale `v` to unit length (left as is when zero).
pub fn normalize(v: &mut [f32]) {
    let n = dot(v, v).sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics() {
        let mut a = [3.0, 4.0];
        normalize(&mut a);
        assert_eq!(a, [0.6, 0.8]);
        assert!(Metric::Cosine.distance(&a, &a).abs() < 1e-6);
        assert!(Metric::Cosine.distance(&a, &[6.0, 8.0]).abs() < 1e-6);
        assert_eq!(Metric::Cosine.similarity(0.0), 1.0);
        assert_eq!(Metric::Cosine.similarity(1.5), 0.0);
        assert_eq!(Metric::Euclidean.distance(&[0.0, 0.0], &[3.0, 4.0]), 5.0);
        assert_eq!(Metric::Euclidean.similarity(0.0), 1.0);
    }
}
