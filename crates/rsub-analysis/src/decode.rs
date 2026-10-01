//! Decode local audio files to mono `f32` PCM at a given sample rate.

use std::f32::consts::SQRT_2;
use std::fs::File;
use std::path::Path;

use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler, WindowFunction};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, Timestamp};

use crate::AnalysisError;

/// Decode errors tolerated in a row before a file is given up on.
const MAX_DECODE_ERRORS: usize = 3;
/// Input frames per resampler chunk.
const CHUNK: usize = 4096;

/// How channels are folded to mono.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downmix {
    /// The mean of the channels (librosa's `mono=True`).
    Mean,
    /// For stereo, the sum scaled by √2/2, which is what ffmpeg's `-ac 1`
    /// does and what bliss's features were tuned on.
    Ffmpeg,
}

/// An open audio file: its first decodable audio track.
pub struct Source {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    rate: u32,
    /// Seconds, when the container knows.
    pub duration: Option<f64>,
}

impl Source {
    pub fn open(path: &Path) -> Result<Source, AnalysisError> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(decode_error)?;
        // The first audio track a decoder exists for: files can carry cover
        // art or other tracks beside the audio.
        let codecs = symphonia::default::get_codecs();
        let opts = AudioDecoderOptions::default();
        let (track, decoder) = format
            .tracks()
            .iter()
            .find_map(|t| {
                let params = t.codec_params.as_ref()?.audio()?;
                let decoder = codecs.make_audio_decoder(params, &opts).ok()?;
                Some((t, decoder))
            })
            .ok_or_else(|| AnalysisError::Decode("no decodable audio track".into()))?;
        let params = track.codec_params.as_ref().and_then(|p| p.audio());
        let rate = params
            .and_then(|p| p.sample_rate)
            .ok_or_else(|| AnalysisError::Decode("unknown sample rate".into()))?;
        let duration = track
            .time_base
            .zip(track.duration)
            .and_then(|(tb, d)| tb.calc_time(Timestamp::ZERO.saturating_add(d)))
            .map(|t| t.as_secs_f64());
        let track_id = track.id;
        Ok(Source {
            format,
            decoder,
            track_id,
            rate,
            duration,
        })
    }

    /// Mono samples at `rate` from `start` seconds, at most `max` seconds of
    /// them (all the rest when `None`).
    pub fn read(
        &mut self,
        rate: u32,
        downmix: Downmix,
        start: f64,
        max: Option<f64>,
    ) -> Result<Vec<f32>, AnalysisError> {
        if start > 0.0 {
            let time = Time::try_from_secs_f64(start)
                .ok_or_else(|| AnalysisError::Decode("bad seek time".into()))?;
            self.format
                .seek(
                    SeekMode::Accurate,
                    SeekTo::Time {
                        time,
                        track_id: Some(self.track_id),
                    },
                )
                .map_err(decode_error)?;
            self.decoder.reset();
        }
        let want = max.map(|m| (m * f64::from(self.rate)).ceil() as usize);
        let mut mono: Vec<f32> = Vec::new();
        let mut buf: Vec<f32> = Vec::new();
        let mut errors = 0;
        while want.is_none_or(|w| mono.len() < w) {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => break,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(decode_error(e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => {
                    errors = 0;
                    d
                }
                Err(SymError::DecodeError(_)) if errors < MAX_DECODE_ERRORS => {
                    errors += 1;
                    continue;
                }
                Err(e) => return Err(decode_error(e)),
            };
            let channels = decoded.spec().channels().count().max(1);
            buf.resize(decoded.samples_interleaved(), 0.0);
            decoded.copy_to_slice_interleaved(&mut buf[..]);
            let fold = |frame: &[f32]| -> f32 {
                match (channels, downmix) {
                    (1, _) => frame[0],
                    (2, Downmix::Ffmpeg) => (frame[0] + frame[1]) * SQRT_2 / 2.0,
                    _ => frame.iter().sum::<f32>() / channels as f32,
                }
            };
            mono.extend(buf.chunks_exact(channels).map(fold));
        }
        if let Some(w) = want {
            mono.truncate(w);
        }
        resample(mono, self.rate, rate)
    }
}

/// Read errors are `Io` (worth retrying: a mount can come back), except a
/// file ending early, which is a broken file.
fn decode_error(e: SymError) -> AnalysisError {
    match e {
        SymError::IoError(e) if e.kind() != std::io::ErrorKind::UnexpectedEof => {
            AnalysisError::Io(e)
        }
        e => AnalysisError::Decode(e.to_string()),
    }
}

/// Resample mono `samples` from `from` Hz to `to` Hz (FFT resampler, as bliss
/// uses), trimming the resampler's delay.
pub fn resample(samples: Vec<f32>, from: u32, to: u32) -> Result<Vec<f32>, AnalysisError> {
    if from == to || samples.is_empty() {
        return Ok(samples);
    }
    let err = |e: &dyn std::fmt::Display| AnalysisError::Decode(format!("resample: {e}"));
    let mut r = Fft::<f32>::new_custom(
        from as usize,
        to as usize,
        CHUNK,
        4,
        1,
        WindowFunction::BlackmanHarris2,
        FixedSync::Input,
    )
    .map_err(|e| err(&e))?;
    let delay = r.output_delay();
    let expected = (r.resample_ratio() * samples.len() as f64).ceil() as usize;
    let out_max = r.output_frames_max();
    let mut out = vec![0.0; out_max];
    let mut resampled = Vec::with_capacity(expected + delay + out_max);
    let mut process = |r: &mut Fft<f32>,
                       input: &[f32],
                       len: usize,
                       indexing: Option<&Indexing>,
                       resampled: &mut Vec<f32>|
     -> Result<(), AnalysisError> {
        let input = InterleavedSlice::new(input, 1, len).map_err(|e| err(&e))?;
        let mut output = InterleavedSlice::new_mut(&mut out, 1, out_max).map_err(|e| err(&e))?;
        let (_, written) = r
            .process_into_buffer(&input, &mut output, indexing)
            .map_err(|e| err(&e))?;
        resampled.extend_from_slice(&out[..written]);
        Ok(())
    };
    let (chunks, rest) = samples.as_chunks::<CHUNK>();
    for chunk in chunks {
        process(&mut r, chunk, CHUNK, None, &mut resampled)?;
    }
    let partial = |len| Indexing {
        input_offset: 0,
        output_offset: 0,
        partial_len: Some(len),
        active_channels_mask: None,
    };
    if !rest.is_empty() {
        process(
            &mut r,
            rest,
            rest.len(),
            Some(&partial(rest.len())),
            &mut resampled,
        )?;
    }
    let zeros = vec![0.0; CHUNK];
    while resampled.len() < expected + delay {
        process(&mut r, &zeros, CHUNK, Some(&partial(0)), &mut resampled)?;
    }
    Ok(resampled[delay..expected + delay].to_vec())
}
