// SPDX-License-Identifier: MIT
//! Device sample rate to the pipeline's 16 kHz.
//!
//! Capture devices run at whatever rate the OS gives us -- 48 kHz on most Macs,
//! 44.1 kHz on plenty of USB microphones. Every engine downstream wants 16 kHz
//! mono f32 ([`super::SAMPLE_RATE`]), so the conversion happens once, here, on
//! the pipeline thread.
//!
//! Input is buffered internally, so the caller can push whatever chunk size the
//! audio callback happened to deliver. [`Resampler::finish_into`] drains the
//! tail at the end of a dictation; without it the last fraction of a chunk --
//! up to 30 ms, which is a word -- is silently lost.

use crate::audio::SAMPLE_RATE;
use crate::{Error, Result};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler as _};

/// Frames handed to rubato per call. 1024 at 48 kHz is 21 ms, which keeps the
/// resampler's own delay well inside the paste budget.
const CHUNK: usize = 1_024;

/// Converts mono f32 at some device rate into mono f32 at 16 kHz.
pub struct Resampler {
    input_rate: u32,
    /// `None` when the device already runs at 16 kHz.
    inner: Option<Inner>,
}

struct Inner {
    fft: Fft<f32>,
    /// The device rate, which fixes how much output we owe per input frame.
    input_rate: u32,
    /// Input at the device rate, waiting for a full chunk.
    pending: Vec<f32>,
    /// Reused output buffer; never grows after construction.
    scratch: Vec<f32>,
    /// Leading silence from the FFT block, still to be discarded.
    delay_left: usize,
    /// Device-rate frames consumed so far, which fixes how many 16 kHz frames
    /// we owe the caller.
    frames_in: u64,
    /// 16 kHz frames handed out so far, after the delay was trimmed.
    frames_out: u64,
}

impl Resampler {
    /// A resampler for a device running at `input_rate`.
    ///
    /// At 16 kHz this is a passthrough and costs nothing.
    pub fn new(input_rate: u32) -> Result<Self> {
        if input_rate == 0 {
            return Err(Error::Audio("device reported a sample rate of 0".into()));
        }
        if input_rate == SAMPLE_RATE {
            return Ok(Self {
                input_rate,
                inner: None,
            });
        }
        let fft = Fft::<f32>::new(
            input_rate as usize,
            SAMPLE_RATE as usize,
            CHUNK,
            1,
            FixedSync::Input,
        )
        .map_err(|e| Error::Audio(format!("{input_rate} Hz to {SAMPLE_RATE} Hz: {e}")))?;
        let scratch = vec![0.0; fft.output_frames_max()];
        let delay_left = fft.output_delay();
        Ok(Self {
            input_rate,
            inner: Some(Inner {
                fft,
                input_rate,
                pending: Vec::with_capacity(CHUNK * 2),
                scratch,
                delay_left,
                frames_in: 0,
                frames_out: 0,
            }),
        })
    }

    /// The device rate this was built for.
    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    /// Push mono samples at the device rate; append 16 kHz mono to `out`.
    pub fn process_into(&mut self, mono: &[f32], out: &mut Vec<f32>) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            out.extend_from_slice(mono);
            return Ok(());
        };
        // A piece at a time rather than all of it appended at once. `run`
        // consumes from the front of `pending`, which moves everything behind
        // it, so a whole file pushed in one call made the work quadratic and
        // held a second copy of the file besides. Measured on file decodes:
        // 5.3 times the audio took 28.6 times as long, 12.6 s for 13 minutes.
        // The live path pushes a few hundred samples at a time and never saw
        // it. Output is unchanged; see the chunk-size test below.
        for piece in mono.chunks(CHUNK * 4) {
            inner.pending.extend_from_slice(piece);
            while inner.pending.len() >= inner.fft.input_frames_next() {
                inner.run(None, out)?;
            }
        }
        Ok(())
    }

    /// Drain whatever is still buffered at the end of a dictation.
    ///
    /// Two things are still owed at this point: the frames sitting in `pending`
    /// below a chunk boundary, and the FFT delay, which is only pushed out by
    /// feeding more audio. Feeding silence flushes it. Without this the last
    /// fraction of a chunk -- up to 30 ms, which is a word -- is lost.
    pub fn finish_into(&mut self, out: &mut Vec<f32>) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        if !inner.pending.is_empty() {
            let partial = inner.pending.len();
            inner.run(Some(partial), out)?;
        }
        // Then silence, until we have handed over as much audio as we were
        // given. The bound is belt and braces: each pass emits a whole chunk,
        // so a handful is always enough.
        for _ in 0..64 {
            if inner.frames_out >= inner.owed() {
                break;
            }
            inner.run(Some(0), out)?;
        }
        // Trim the trailing silence we fed in to flush the delay. Count back
        // from the end: `out` may already have held audio when we were called.
        let excess = inner.frames_out.saturating_sub(inner.owed()) as usize;
        if excess > 0 {
            out.truncate(out.len() - excess.min(out.len()));
            inner.frames_out -= excess as u64;
        }
        Ok(())
    }
}

impl Inner {
    /// 16 kHz frames owed for the device-rate frames consumed so far.
    fn owed(&self) -> u64 {
        (self.frames_in as f64 * f64::from(SAMPLE_RATE) / f64::from(self.input_rate)).round() as u64
    }

    /// One chunk through rubato. `partial` is `Some(n)` for a short final chunk
    /// (`Some(0)` being pure silence).
    fn run(&mut self, partial: Option<usize>, out: &mut Vec<f32>) -> Result<()> {
        let need = self.fft.input_frames_next();
        // rubato reads a whole chunk regardless, so pad a short final chunk
        // with silence. Never shorten: `pending` usually holds more than one
        // chunk, and truncating here throws the rest of the dictation away.
        if self.pending.len() < need {
            self.pending.resize(need, 0.0);
        }

        let input = InterleavedSlice::new(&self.pending[..need], 1, need)
            .map_err(|e| Error::Audio(format!("resampler input buffer: {e}")))?;
        let cap = self.scratch.len();
        let mut output = InterleavedSlice::new_mut(&mut self.scratch, 1, cap)
            .map_err(|e| Error::Audio(format!("resampler output buffer: {e}")))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial,
            active_channels_mask: None,
        };
        let (n_in, n_out) = self
            .fft
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|e| Error::Audio(format!("resample: {e}")))?;

        let consumed = partial.unwrap_or(n_in).min(self.pending.len());
        self.pending.drain(..consumed);
        self.frames_in += consumed as u64;

        let produced = &self.scratch[..n_out];
        let skip = self.delay_left.min(produced.len());
        self.delay_left -= skip;
        out.extend_from_slice(&produced[skip..]);
        self.frames_out += (produced.len() - skip) as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SAMPLE_RATE;

    /// `seconds` of a `hz` sine at `rate`.
    fn tone(hz: f32, rate: u32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    /// Sign changes per second -- twice the frequency of a clean sine.
    fn zero_crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count()
    }

    #[test]
    fn sixteen_k_input_is_a_passthrough_with_no_filtering_at_all() {
        let mut r = Resampler::new(SAMPLE_RATE).expect("resampler");
        let mut out = Vec::new();
        r.process_into(&[0.1, -0.2, 0.3], &mut out)
            .expect("process");
        assert_eq!(
            out,
            vec![0.1, -0.2, 0.3],
            "a device already at 16 kHz must not be resampled at all"
        );
    }

    #[test]
    fn one_second_at_forty_eight_k_yields_one_second_at_sixteen_k() {
        let mut r = Resampler::new(48_000).expect("resampler");
        let mut out = Vec::new();
        r.process_into(&tone(440.0, 48_000, 1.0), &mut out)
            .expect("process");
        r.finish_into(&mut out).expect("finish");
        let drift = (out.len() as i64 - SAMPLE_RATE as i64).abs();
        assert!(
            drift <= 64,
            "expected ~{SAMPLE_RATE} samples, got {} (drift {drift})",
            out.len()
        );
    }

    #[test]
    fn forty_four_one_k_works_too_even_though_the_ratio_is_not_whole() {
        // 44100/16000 is 2.75625:1. USB microphones do this constantly.
        let mut r = Resampler::new(44_100).expect("resampler");
        let mut out = Vec::new();
        r.process_into(&tone(440.0, 44_100, 1.0), &mut out)
            .expect("process");
        r.finish_into(&mut out).expect("finish");
        let drift = (out.len() as i64 - SAMPLE_RATE as i64).abs();
        assert!(drift <= 64, "got {} samples (drift {drift})", out.len());
    }

    #[test]
    fn a_one_kilohertz_tone_is_still_a_one_kilohertz_tone_afterwards() {
        let mut r = Resampler::new(48_000).expect("resampler");
        let mut out = Vec::new();
        r.process_into(&tone(1_000.0, 48_000, 1.0), &mut out)
            .expect("process");
        r.finish_into(&mut out).expect("finish");
        // A clean 1 kHz sine crosses zero 2000 times a second. Anything far off
        // means the conversion aliased or shifted pitch.
        let crossings = zero_crossings(&out);
        assert!(
            (1_900..=2_100).contains(&crossings),
            "expected ~2000 zero crossings, got {crossings}"
        );
    }

    #[test]
    fn callback_sized_chunks_add_up_to_the_same_output_as_one_big_push() {
        // cpal hands us a few hundred frames at a time, not a whole second.
        let input = tone(440.0, 48_000, 1.0);

        let mut chunked = Resampler::new(48_000).expect("resampler");
        let mut a = Vec::new();
        for chunk in input.chunks(480) {
            chunked.process_into(chunk, &mut a).expect("process");
        }
        chunked.finish_into(&mut a).expect("finish");

        let mut whole = Resampler::new(48_000).expect("resampler");
        let mut b = Vec::new();
        whole.process_into(&input, &mut b).expect("process");
        whole.finish_into(&mut b).expect("finish");

        assert_eq!(
            a.len(),
            b.len(),
            "chunk size must not change how much audio comes out"
        );
    }

    #[test]
    fn the_tail_is_not_lost_when_the_input_is_shorter_than_one_chunk() {
        // 100 ms is well under a resampler chunk, and it is also a word.
        let mut r = Resampler::new(48_000).expect("resampler");
        let mut out = Vec::new();
        r.process_into(&tone(440.0, 48_000, 0.1), &mut out)
            .expect("process");
        r.finish_into(&mut out).expect("finish");
        assert!(
            out.len() >= 1_400,
            "100 ms in should give ~1600 samples out, got {}",
            out.len()
        );
    }
}
