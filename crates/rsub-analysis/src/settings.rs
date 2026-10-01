//! Choosing and tuning an analyzer: the one place the server's `[analysis]`
//! config and `rsub-cli` turn settings into an [`Analyzer`].

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use crate::{AnalysisError, Analyzer};

/// Most CLAP crops, and the default.
pub const MAX_CLAP_CROPS: usize = 4;
/// Shortest bliss excerpt, in seconds.
pub const MIN_BLISS_SECONDS: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzerKind {
    /// CLAP audio embeddings (the `clap` feature).
    Clap,
    /// bliss-rs features (the `bliss` feature; GPL-3.0).
    Bliss,
}

/// The analyzers this build has, preferred first.
pub const BUILT: &[AnalyzerKind] = &[
    #[cfg(feature = "clap")]
    AnalyzerKind::Clap,
    #[cfg(feature = "bliss")]
    AnalyzerKind::Bliss,
];

impl AnalyzerKind {
    pub fn name(self) -> &'static str {
        match self {
            AnalyzerKind::Clap => "clap",
            AnalyzerKind::Bliss => "bliss",
        }
    }
}

impl FromStr for AnalyzerKind {
    type Err = AnalysisError;

    fn from_str(s: &str) -> Result<Self, AnalysisError> {
        match s {
            "clap" => Ok(AnalyzerKind::Clap),
            "bliss" => Ok(AnalyzerKind::Bliss),
            _ => Err(AnalysisError::Settings(format!(
                "unknown analyzer '{s}' (bliss or clap)"
            ))),
        }
    }
}

/// An analyzer and its performance settings. Every setting is part of the
/// analyzer's id, so changing one analyses the library again.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Default: the first of [`BUILT`].
    pub analyzer: Option<AnalyzerKind>,
    /// CLAP: the audio model exported to ONNX (`tools/export_clap.py`).
    pub clap_model: Option<PathBuf>,
    /// CLAP: 10 s crops embedded per track, 1 to [`MAX_CLAP_CROPS`]; the time
    /// per track is about linear in them.
    pub clap_crops: usize,
    /// bliss: analyse only this many seconds from the middle of each track (at
    /// least [`MIN_BLISS_SECONDS`]); 0 analyses all of it.
    pub bliss_seconds: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            analyzer: None,
            clap_model: None,
            clap_crops: MAX_CLAP_CROPS,
            bliss_seconds: 0,
        }
    }
}

impl Settings {
    /// The analyzer to run, after checking every setting.
    pub fn kind(&self) -> Result<AnalyzerKind, AnalysisError> {
        let bad = |m: String| Err(AnalysisError::Settings(m));
        let Some(kind) = self.analyzer.or(BUILT.first().copied()) else {
            return bad(
                "no analyzer is built in (build with the `bliss` or `clap` feature)".into(),
            );
        };
        if !BUILT.contains(&kind) {
            let name = kind.name();
            return bad(format!(
                "the {name} analyzer isn't built in (build with the `{name}` feature)"
            ));
        }
        if !(1..=MAX_CLAP_CROPS).contains(&self.clap_crops) {
            return bad(format!(
                "CLAP crops must be 1 to {MAX_CLAP_CROPS}, not {}",
                self.clap_crops
            ));
        }
        if self.bliss_seconds != 0 && self.bliss_seconds < MIN_BLISS_SECONDS {
            return bad(format!(
                "the bliss excerpt must be 0 (the whole track) or at least \
                 {MIN_BLISS_SECONDS} seconds, not {}",
                self.bliss_seconds
            ));
        }
        if kind == AnalyzerKind::Clap && self.clap_model.is_none() {
            return bad("the clap analyzer needs its model file".into());
        }
        Ok(kind)
    }

    /// Check the settings and load the analyzer (about a second for CLAP).
    pub fn build(&self) -> Result<Arc<dyn Analyzer>, AnalysisError> {
        match self.kind()? {
            #[cfg(feature = "clap")]
            AnalyzerKind::Clap => {
                let model = self.clap_model.as_deref().expect("checked");
                Ok(Arc::new(crate::clap::Clap::load(model, self.clap_crops)?))
            }
            #[cfg(feature = "bliss")]
            AnalyzerKind::Bliss => Ok(Arc::new(crate::bliss::Bliss::new(
                (self.bliss_seconds > 0).then_some(self.bliss_seconds),
            ))),
            #[allow(unreachable_patterns)]
            _ => unreachable!("kind() only returns built analyzers"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(s: Settings) -> String {
        s.kind().unwrap_err().to_string()
    }

    #[cfg(any(feature = "bliss", feature = "clap"))]
    #[test]
    fn settings_are_checked() {
        let ok = Settings {
            clap_model: Some("model.onnx".into()),
            ..Settings::default()
        };
        assert_eq!(ok.kind().unwrap(), BUILT[0]);
        let with = |crops, seconds| Settings {
            clap_crops: crops,
            bliss_seconds: seconds,
            ..ok.clone()
        };
        assert!(err(with(0, 0)).contains("crops"));
        assert!(err(with(5, 0)).contains("crops"));
        assert!(err(with(4, 5)).contains("bliss"));
        assert!(with(1, 30).kind().is_ok());
    }

    #[cfg(not(any(feature = "bliss", feature = "clap")))]
    #[test]
    fn nothing_to_run_without_an_analyzer() {
        assert!(err(Settings::default()).contains("no analyzer"));
    }

    #[test]
    fn analyzer_names() {
        assert!("mfcc".parse::<AnalyzerKind>().is_err());
        for kind in [AnalyzerKind::Clap, AnalyzerKind::Bliss] {
            assert_eq!(kind.name().parse::<AnalyzerKind>().unwrap(), kind);
        }
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_needs_a_model() {
        let s = Settings {
            analyzer: Some(AnalyzerKind::Clap),
            ..Settings::default()
        };
        assert!(err(s).contains("model"));
    }

    #[cfg(not(feature = "bliss"))]
    #[test]
    fn unbuilt_analyzers_are_refused() {
        let s = Settings {
            analyzer: Some(AnalyzerKind::Bliss),
            ..Settings::default()
        };
        assert!(err(s).contains("`bliss` feature"));
    }
}
