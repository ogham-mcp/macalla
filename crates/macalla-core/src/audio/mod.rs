// SPDX-License-Identifier: MIT
//! Audio primitives. Everything downstream of capture is 16 kHz mono f32.
//!
//! There is no device in here and there must not be: the microphone lives in
//! `macalla-platform::audio`, and this crate owns only the contract
//! it produces -- the ring, the downmix, the resampler and [`AudioSegment`].
//! That is what lets `macalla-core` build and test on a machine with no audio
//! stack.

pub mod chain;
pub mod mix;
pub mod resample;
pub mod ring;

/// The one sample rate the pipeline works in. Capture resamples to this.
pub const SAMPLE_RATE: u32 = 16_000;

/// A contiguous run of speech, in 16 kHz mono f32.
///
/// Produced by VAD when the engine does not own segmentation, and consumed by
/// the batch path of [`crate::stt::SttEngine`]. Stream-first engines never see
/// one of these -- they take the sample stream directly.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSegment {
    pub samples: Vec<f32>,
    /// Offset from the start of the dictation, in milliseconds.
    pub start_ms: u64,
}

impl AudioSegment {
    pub fn new(samples: Vec<f32>, start_ms: u64) -> Self {
        Self { samples, start_ms }
    }

    pub fn duration_ms(&self) -> u64 {
        (self.samples.len() as u64 * 1_000) / SAMPLE_RATE as u64
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_is_derived_from_the_sample_rate() {
        let one_second = AudioSegment::new(vec![0.0; SAMPLE_RATE as usize], 0);
        assert_eq!(one_second.duration_ms(), 1_000);

        let half = AudioSegment::new(vec![0.0; SAMPLE_RATE as usize / 2], 500);
        assert_eq!(half.duration_ms(), 500);
        assert_eq!(half.start_ms, 500);
    }

    #[test]
    fn an_empty_segment_has_no_duration() {
        assert_eq!(AudioSegment::new(vec![], 0).duration_ms(), 0);
        assert!(AudioSegment::new(vec![], 0).is_empty());
    }
}
