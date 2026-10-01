//! bliss-rs analysis: 23 features (tempo, zero-crossing rate, spectral shape,
//! loudness, chroma) of the whole track, or of an excerpt from its middle.
//! bliss-audio is GPL-3.0-only.

use std::path::Path;

use bliss_audio::Song;

use crate::decode::{Downmix, Source};
use crate::{AnalysisError, Analyzer, Metric};

/// bliss's sample rate.
const RATE: u32 = 22_050;

/// bliss's feature weights (`FeaturesVersion::Version2`): tempo counts a
/// quarter (its beat tracker is unreliable on onset-poor music) and the 13
/// chroma features share the weight of 3. Stored vectors are scaled by their
/// square roots, so plain Euclidean distance is bliss's weighted distance.
const WEIGHTS: [f32; 23] = {
    let mut w = [1.0; 23];
    w[0] = 0.25;
    let mut i = 10;
    while i < 23 {
        w[i] = 3.0 / 13.0;
        i += 1;
    }
    w
};

pub struct Bliss {
    /// Analyse at most this many seconds from the middle of each track.
    excerpt: Option<f64>,
    id: String,
}

impl Bliss {
    /// `excerpt`: analyse at most this many seconds (at least
    /// [`crate::MIN_BLISS_SECONDS`]) from the middle of each track instead of
    /// all of it. Decoding and analysis cost is linear in the length analysed.
    pub fn new(excerpt: Option<u32>) -> Bliss {
        // Bump the version when the features or weights change. Excerpts
        // give other vectors, so they're stored apart.
        let id = match excerpt {
            None => "bliss-2".to_owned(),
            Some(s) => format!("bliss-2/{s}s"),
        };
        Bliss {
            excerpt: excerpt.map(f64::from),
            id,
        }
    }

    /// The mono samples to analyse. An excerpt is read by seeking when the
    /// container knows the track's length; otherwise, or when the seek fails,
    /// the whole track is decoded and cut.
    fn samples(&self, path: &Path) -> Result<Vec<f32>, AnalysisError> {
        let mut src = Source::open(path)?;
        let Some(excerpt) = self.excerpt else {
            return src.read(RATE, Downmix::Ffmpeg, 0.0, None);
        };
        if let Some(duration) = src.duration.filter(|d| *d > excerpt) {
            let start = (duration - excerpt) / 2.0;
            match src.read(RATE, Downmix::Ffmpeg, start, Some(excerpt)) {
                Err(AnalysisError::Decode(e)) => {
                    tracing::debug!(path = %path.display(), "seeking to the excerpt failed ({e}); decoding it all");
                    src = Source::open(path)?;
                }
                r => return r,
            }
        }
        let all = src.read(RATE, Downmix::Ffmpeg, 0.0, None)?;
        let len = (excerpt * f64::from(RATE)) as usize;
        let start = all.len().saturating_sub(len) / 2;
        Ok(all[start..all.len().min(start + len)].to_vec())
    }
}

impl Analyzer for Bliss {
    fn id(&self) -> &str {
        &self.id
    }

    fn metric(&self) -> Metric {
        Metric::Euclidean
    }

    fn analyze(&self, path: &Path) -> Result<Vec<f32>, AnalysisError> {
        let samples = self.samples(path)?;
        let analysis =
            Song::analyze(&samples).map_err(|e| AnalysisError::Analysis(e.to_string()))?;
        let features = analysis.as_vec();
        if features.len() != WEIGHTS.len() {
            return Err(AnalysisError::Analysis(format!(
                "expected {} features, got {}",
                WEIGHTS.len(),
                features.len()
            )));
        }
        Ok(features
            .iter()
            .zip(WEIGHTS)
            .map(|(f, w)| f * w.sqrt())
            .collect())
    }
}
