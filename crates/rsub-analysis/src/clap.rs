//! CLAP audio embeddings: `laion/clap-htsat-unfused`'s audio tower, the model
//! subwave uses, exported to ONNX (`tools/export_clap.py`) and run with tract.
//!
//! The model sees 10 s at a time. One to four crops spread over the track
//! (four by default, centred at 15, 38, 62 and 85%) are embedded in one batch
//! and averaged, so the vector reflects the whole track, not its intro.
//! (Hugging Face's extractor crops at random; fixed crops keep results
//! reproducible.) The model's cost is linear in the crops. Vectors are unit
//! length.

use std::path::Path;
use std::sync::Arc;

use tract_onnx::prelude::*;

use crate::decode::{Downmix, Source};
use crate::mel::{self, MelFrontend};
use crate::{AnalysisError, Analyzer, MAX_CLAP_CROPS, Metric, normalize};

/// Crop centres, as fractions of the track, by the number of crops.
const CROP_SETS: [&[f64]; MAX_CLAP_CROPS] = [
    &[0.5],
    &[0.3, 0.7],
    &[0.2, 0.5, 0.8],
    &[0.15, 0.38, 0.62, 0.85],
];
const CROP_SECS: f64 = 10.0;
const DIM: usize = 512;

pub struct Clap {
    model: Arc<TypedRunnableModel>,
    mel: MelFrontend,
    crops: &'static [f64],
    id: String,
}

impl Clap {
    /// Load and optimise the exported audio tower (about a second), to embed
    /// `crops` crops (1 to [`MAX_CLAP_CROPS`]) of each track.
    pub fn load(path: &Path, crops: usize) -> Result<Clap, AnalysisError> {
        let crops = CROP_SETS[crops - 1];
        let err =
            |e: TractError| AnalysisError::Analysis(format!("CLAP model {}: {e}", path.display()));
        if !path.is_file() {
            return Err(AnalysisError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("CLAP model {} not found", path.display()),
            )));
        }
        let model = tract_onnx::onnx()
            .model_for_path(path)
            .and_then(|m| {
                m.with_input_fact(
                    0,
                    f32::fact([crops.len(), 1, mel::FRAMES, mel::MELS]).into(),
                )
            })
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .map_err(err)?;
        // Bump the version when the front end changes. The model file's hash
        // tells exports apart, and fewer crops give other vectors, so each is
        // stored apart.
        let hash = model_hash(path)?;
        let id = match crops.len() {
            MAX_CLAP_CROPS => format!("clap-htsat-unfused-1+{hash}"),
            n => format!("clap-htsat-unfused-1+{hash}/{n}crops"),
        };
        Ok(Clap {
            model,
            mel: MelFrontend::new(),
            crops,
            id,
        })
    }

    /// One crop of samples per crop centre; a short track is used
    /// whole for each. Crops are read by seeking when the container knows the
    /// track's length; otherwise, or when a seek fails (an MP3 whose header
    /// overstates its length), the whole track is decoded and cut.
    fn crops(&self, path: &Path) -> Result<Vec<Vec<f32>>, AnalysisError> {
        let mut src = Source::open(path)?;
        if let Some(duration) = src.duration.filter(|d| *d > CROP_SECS * 1.5) {
            match seek_crops(&mut src, self.crops, duration) {
                Err(AnalysisError::Decode(e)) => {
                    tracing::debug!(path = %path.display(), "cropping by seeking failed ({e}); decoding it all");
                    src = Source::open(path)?;
                }
                r => return r,
            }
        }
        let all = src.read(mel::SAMPLE_RATE, Downmix::Mean, 0.0, None)?;
        Ok(cut_crops(&all, self.crops))
    }
}

/// The first 8 hex digits of the model file's SHA-256.
fn model_hash(path: &Path) -> Result<String, AnalysisError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0; 1 << 20];
    loop {
        match file.read(&mut buf)? {
            0 => break,
            n => hasher.update(&buf[..n]),
        }
    }
    Ok(hasher.finalize()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn seek_crops(
    src: &mut Source,
    centres: &[f64],
    duration: f64,
) -> Result<Vec<Vec<f32>>, AnalysisError> {
    centres
        .iter()
        .map(|c| {
            let start = (c * duration - CROP_SECS / 2.0).clamp(0.0, duration - CROP_SECS);
            let samples = src.read(mel::SAMPLE_RATE, Downmix::Mean, start, Some(CROP_SECS))?;
            Ok(mel::fit(&samples))
        })
        .collect()
}

/// Crops at `centres` cut from a whole decoded track.
fn cut_crops(all: &[f32], centres: &[f64]) -> Vec<Vec<f32>> {
    if all.len() <= mel::CROP * 3 / 2 {
        return vec![mel::fit(all); centres.len()];
    }
    let n = all.len() as f64;
    let max_start = all.len() - mel::CROP;
    centres
        .iter()
        .map(|c| {
            let start = ((c * n) as usize)
                .saturating_sub(mel::CROP / 2)
                .min(max_start);
            all[start..start + mel::CROP].to_vec()
        })
        .collect()
}

impl Analyzer for Clap {
    fn id(&self) -> &str {
        &self.id
    }

    fn metric(&self) -> Metric {
        Metric::Cosine
    }

    fn analyze(&self, path: &Path) -> Result<Vec<f32>, AnalysisError> {
        let crops = self.crops(path)?;
        let mut input = Vec::with_capacity(self.crops.len() * mel::FRAMES * mel::MELS);
        for c in &crops {
            input.extend(self.mel.log_mel(c));
        }
        let tensor = tract_ndarray::Array4::from_shape_vec(
            (self.crops.len(), 1, mel::FRAMES, mel::MELS),
            input,
        )
        .map_err(|e| AnalysisError::Analysis(e.to_string()))?;
        let out = self
            .model
            .run(tvec!(Tensor::from(tensor).into()))
            .map_err(|e| AnalysisError::Analysis(format!("CLAP: {e}")))?;
        let emb = out[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| AnalysisError::Analysis(format!("CLAP output: {e}")))?;
        if emb.len() != self.crops.len() * DIM {
            return Err(AnalysisError::Analysis(format!(
                "CLAP output has {} values, expected {}",
                emb.len(),
                self.crops.len() * DIM
            )));
        }
        // Each crop's embedding is unit length (the export normalises);
        // average and normalise again.
        let flat: Vec<f32> = emb.iter().copied().collect();
        let mut v = vec![0f32; DIM];
        for row in flat.as_chunks::<DIM>().0 {
            v.iter_mut().zip(row).for_each(|(a, b)| *a += b);
        }
        normalize(&mut v);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_cover_the_track() {
        let short: Vec<f32> = vec![0.5; mel::CROP];
        let four = CROP_SETS[3];
        assert!(cut_crops(&short, four).iter().all(|c| c.len() == mel::CROP));
        let long: Vec<f32> = (0..mel::CROP * 10).map(|i| i as f32).collect();
        let crops = cut_crops(&long, four);
        assert_eq!(crops.len(), 4);
        // Centred on 15% and 85% of the track.
        let centre = |c: &Vec<f32>| c[mel::CROP / 2] / long.len() as f32;
        assert!((centre(&crops[0]) - 0.15).abs() < 0.01);
        assert!((centre(&crops[3]) - 0.85).abs() < 0.01);
    }
}
