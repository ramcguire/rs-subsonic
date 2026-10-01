//! The log-mel spectrogram CLAP's audio tower takes, as Hugging Face's
//! `ClapFeatureExtractor` computes it for `laion/clap-htsat-unfused`
//! (truncation `rand_trunc`, which uses librosa's Slaney mel filters): 48 kHz
//! mono, 1024-sample periodic Hann frames every 480 samples, centred with
//! reflect padding, power spectrum through 64 Slaney-normalised mel filters
//! from 50 Hz to 14 kHz, in dB.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per crop: 10 s.
pub const CROP: usize = 480_000;
pub const MELS: usize = 64;
/// Frames per crop: `1 + CROP / HOP`.
pub const FRAMES: usize = 1 + CROP / HOP;
const N_FFT: usize = 1024;
const HOP: usize = 480;
const BINS: usize = N_FFT / 2 + 1;
const F_MIN: f64 = 50.0;
const F_MAX: f64 = 14_000.0;
const FLOOR: f32 = 1e-10;

pub struct MelFrontend {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// `MELS × BINS`, row-major.
    filters: Vec<f32>,
}

impl MelFrontend {
    pub fn new() -> Self {
        let window = (0..N_FFT)
            .map(|n| {
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / N_FFT as f64).cos()) as f32
            })
            .collect();
        MelFrontend {
            fft: FftPlanner::new().plan_fft_forward(N_FFT),
            window,
            filters: slaney_filters(),
        }
    }

    /// `FRAMES × MELS` dB values for exactly `CROP` samples (see [`fit`]).
    pub fn log_mel(&self, samples: &[f32]) -> Vec<f32> {
        debug_assert_eq!(samples.len(), CROP);
        let pad = N_FFT / 2;
        // numpy's "reflect": mirror without repeating the edge sample.
        let at = |i: isize| -> f32 {
            let n = samples.len() as isize;
            let j = if i < 0 {
                -i
            } else if i >= n {
                2 * (n - 1) - i
            } else {
                i
            };
            samples[j as usize]
        };
        let mut out = Vec::with_capacity(FRAMES * MELS);
        let mut buf = vec![Complex32::default(); N_FFT];
        let mut power = [0f32; BINS];
        for f in 0..FRAMES {
            let start = (f * HOP) as isize - pad as isize;
            for (k, b) in buf.iter_mut().enumerate() {
                *b = Complex32::new(at(start + k as isize) * self.window[k], 0.0);
            }
            self.fft.process(&mut buf);
            for (p, c) in power.iter_mut().zip(&buf) {
                *p = c.norm_sqr();
            }
            for m in 0..MELS {
                let row = &self.filters[m * BINS..(m + 1) * BINS];
                let e: f32 = row.iter().zip(&power).map(|(w, p)| w * p).sum();
                out.push(10.0 * e.max(FLOOR).log10());
            }
        }
        out
    }
}

/// Make `samples` exactly one crop long, as the extractor's `repeatpad`
/// does for shorter audio: repeat it whole as often as it fits, then pad
/// with silence. Longer audio is cut.
pub fn fit(samples: &[f32]) -> Vec<f32> {
    if samples.len() >= CROP {
        return samples[..CROP].to_vec();
    }
    let mut out = Vec::with_capacity(CROP);
    if !samples.is_empty() {
        for _ in 0..CROP / samples.len() {
            out.extend_from_slice(samples);
        }
    }
    out.resize(CROP, 0.0);
    out
}

fn hz_to_mel(f: f64) -> f64 {
    const LOG_STEP: f64 = 27.0 / 1.856_297_990_365_626; // 27 / ln 6.4
    if f >= 1000.0 {
        15.0 + (f / 1000.0).ln() * LOG_STEP
    } else {
        3.0 * f / 200.0
    }
}

fn mel_to_hz(m: f64) -> f64 {
    const LOG_STEP: f64 = 1.856_297_990_365_626 / 27.0;
    if m >= 15.0 {
        1000.0 * (LOG_STEP * (m - 15.0)).exp()
    } else {
        200.0 * m / 3.0
    }
}

/// Triangular filters between mel-spaced edges, each scaled by 2 / its
/// width in Hz (Slaney normalisation).
fn slaney_filters() -> Vec<f32> {
    let (lo, hi) = (hz_to_mel(F_MIN), hz_to_mel(F_MAX));
    let edges: Vec<f64> = (0..MELS + 2)
        .map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (MELS + 1) as f64))
        .collect();
    let nyquist = f64::from(SAMPLE_RATE / 2);
    let mut filters = vec![0f32; MELS * BINS];
    for m in 0..MELS {
        let (l, c, r) = (edges[m], edges[m + 1], edges[m + 2]);
        let norm = 2.0 / (r - l);
        for b in 0..BINS {
            let f = nyquist * b as f64 / (BINS - 1) as f64;
            let down = (f - l) / (c - l);
            let up = (r - f) / (r - c);
            filters[m * BINS + b] = (down.min(up).max(0.0) * norm) as f32;
        }
    }
    filters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_padding() {
        let fe = MelFrontend::new();
        let tone: Vec<f32> = (0..CROP)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48_000.0).sin())
            .collect();
        let mel = fe.log_mel(&tone);
        assert_eq!(mel.len(), FRAMES * MELS);
        // A 1 kHz tone peaks in the same mel band in every frame.
        let peak = |f: usize| {
            (0..MELS)
                .max_by(|&a, &b| mel[f * MELS + a].total_cmp(&mel[f * MELS + b]))
                .unwrap()
        };
        assert_eq!(peak(10), peak(500));
        assert_eq!(fit(&[1.0, 2.0]).len(), CROP);
        assert_eq!(fit(&[1.0; 300_000])[300_000], 0.0);
        assert_eq!(fit(&[1.0; 200_000])[300_000], 1.0);
    }
}
