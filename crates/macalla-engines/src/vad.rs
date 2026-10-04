// SPDX-License-Identifier: MIT
//! The speech detector Macalla listens with: earshot, a small neural
//! voice-activity model in pure Rust, behind the core's [`SpeechDetector`].
//!
//! The core's energy detector hears loudness, not speech. Loud noise opened
//! its gate, and a Whisper engine handed that noise wrote "Thanks for
//! watching!" into the dictation. earshot passed none of the same noise, and
//! dropped no word of real dictation, in the comparison recorded in
//! docs/benchmarks.md.
//!
//! Its weights, about 40 kB, are compiled into the crate: there is nothing to
//! download, no runtime to load, and no state in which the detector is missing.

use macalla_core::vad::{SpeechDetector, FRAME};

/// earshot decides on 16 ms of 16 kHz audio; the core decides on [`FRAME`],
/// twice that, so each frame is two of earshot's.
const HALF: usize = 256;
const _: () = assert!(HALF * 2 == FRAME);

/// A frame is speech when either half scores at least this, earshot's own
/// suggested cut. At 0.35 it let a burst of noise through in the comparison;
/// at 0.5 it let none through and lost no word.
const THRESHOLD: f32 = 0.5;

/// earshot behind the core's port. One per dictation or file, as for any
/// detector: [`SpeechDetector::reset`] forgets the audio it has heard.
pub struct Earshot {
    /// On the heap: the detector keeps about 8 kB of state inline.
    detector: Box<earshot::Detector>,
}

impl Earshot {
    pub fn new() -> Self {
        Self {
            detector: earshot::Detector::default_boxed(),
        }
    }
}

impl Default for Earshot {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeechDetector for Earshot {
    fn is_speech(&mut self, frame: &[f32]) -> bool {
        let mut half = [0.0; HALF];
        let mut score = 0.0_f32;
        // Both halves, in order, so the model's view of the audio runs on
        // unbroken; either one hearing speech makes the frame speech, so a
        // word starting late in a frame is not held back a frame.
        let (halves, _) = frame.as_chunks::<HALF>();
        for chunk in halves {
            // earshot asserts in debug builds that no sample is above full
            // scale, and decoded or resampled audio can overshoot a little.
            for (to, from) in half.iter_mut().zip(chunk) {
                *to = if from.is_nan() {
                    0.0
                } else {
                    from.clamp(-1.0, 1.0)
                };
            }
            score = score.max(self.detector.predict_f32(&half));
        }
        score >= THRESHOLD
    }

    fn reset(&mut self) {
        self.detector.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::audio::AudioSegment;
    use macalla_core::vad::energy::EnergyDetector;
    use macalla_core::vad::{Vad, VadConfig};

    const SECOND: usize = 16_000;

    /// The same noise on every run.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        }
    }

    /// Nine seconds of quiet room with three loud things in it that are not
    /// speech: a burst of hiss, a run of clicks like typing, and louder hiss.
    fn noise_bursts() -> Vec<f32> {
        let mut rng = Lcg(7);
        let mut audio: Vec<f32> = (0..SECOND * 9).map(|_| rng.next() * 0.001).collect();
        for s in &mut audio[SECOND..SECOND * 18 / 10] {
            *s += rng.next() * 0.25;
        }
        for (i, s) in audio[SECOND * 3..SECOND * 42 / 10].iter_mut().enumerate() {
            if i % 800 < 40 {
                *s += rng.next() * 0.6;
            }
        }
        for s in &mut audio[SECOND * 6..SECOND * 64 / 10] {
            *s += rng.next() * 0.4;
        }
        audio
    }

    fn decisions(detector: &mut Earshot, audio: &[f32]) -> Vec<bool> {
        let (frames, _) = audio.as_chunks::<FRAME>();
        frames.iter().map(|f| detector.is_speech(f)).collect()
    }

    #[test]
    fn noise_that_opens_the_energy_gate_is_not_speech() {
        let audio = AudioSegment::new(noise_bursts(), 0);
        let energy = Vad::new(EnergyDetector::new(), VadConfig::default()).segment(&audio);
        assert!(
            !energy.is_empty(),
            "the bursts are meant to be loud enough to fool an energy detector"
        );
        let heard = Vad::new(Earshot::new(), VadConfig::default()).segment(&audio);
        assert!(heard.is_empty(), "noise was taken for speech: {heard:?}");
    }

    #[test]
    fn audio_past_full_scale_is_decided_rather_than_tripping_the_model() {
        let mut detector = Earshot::new();
        let frame: Vec<f32> = (0..FRAME)
            .map(|i| match i % 3 {
                0 => 1.4,
                1 => -1.4,
                _ => f32::NAN,
            })
            .collect();
        detector.is_speech(&frame);
    }

    #[test]
    fn after_a_reset_the_same_audio_is_decided_the_same_way() {
        let audio = noise_bursts();
        let mut detector = Earshot::new();
        let first = decisions(&mut detector, &audio);
        detector.reset();
        assert_eq!(first, decisions(&mut detector, &audio));
    }
}
