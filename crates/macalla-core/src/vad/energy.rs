// SPDX-License-Identifier: MIT
//! An energy detector: no model, no download, no accelerator.
//!
//! Not what dictation listens with. It is a real detector, not a stub, but it
//! is an ENERGY detector -- it cannot tell speech from a door slam, and loud
//! noise it let through became words on a Whisper engine (docs/benchmarks.md).
//! The core's own tests run on it because it needs nothing. Its quality on
//! real speech is measured by an ignored test rather than assumed from the
//! tone-based unit tests below. A pure tone is trivially easy for it and
//! proves almost nothing.
//!
//! The noise floor adapts, because a room is not a constant and a headset is
//! not a laptop microphone. It adapts DOWNWARD quickly and upward slowly: a
//! floor that chases speech upward stops hearing the speaker.

use super::SpeechDetector;

/// How far above the noise floor a frame must be, in decibels, to count.
const SPEECH_MARGIN_DB: f32 = 9.0;

/// Frames below this are silence whatever the floor says. A room at -63 dB and
/// a muted microphone are different problems and only one of them is speech.
const ABSOLUTE_FLOOR_DB: f32 = -55.0;

pub struct EnergyDetector {
    floor_db: f32,
    /// Set on the first frame, so the floor starts where the room is rather
    /// than at an invented constant.
    seeded: bool,
}

impl Default for EnergyDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl EnergyDetector {
    pub fn new() -> Self {
        Self {
            floor_db: ABSOLUTE_FLOOR_DB,
            seeded: false,
        }
    }

    /// RMS of a frame in dBFS. Silence is very negative.
    pub fn level_db(frame: &[f32]) -> f32 {
        if frame.is_empty() {
            return f32::NEG_INFINITY;
        }
        let sum: f32 = frame.iter().map(|s| s * s).sum();
        let rms = (sum / frame.len() as f32).sqrt();
        20.0 * rms.max(1e-10).log10()
    }

    /// The floor it has settled on, for logs and the level meter.
    pub fn noise_floor_db(&self) -> f32 {
        self.floor_db
    }
}

impl SpeechDetector for EnergyDetector {
    fn is_speech(&mut self, frame: &[f32]) -> bool {
        let level = Self::level_db(frame);
        if !self.seeded {
            self.floor_db = level.min(ABSOLUTE_FLOOR_DB);
            self.seeded = true;
        }
        let speech = level > self.floor_db + SPEECH_MARGIN_DB && level > ABSOLUTE_FLOOR_DB;
        if !speech {
            // Track the room. Fast down, slow up -- a floor that chases speech
            // upward goes deaf to the speaker within a sentence.
            self.floor_db = if level < self.floor_db {
                self.floor_db * 0.8 + level * 0.2
            } else {
                self.floor_db * 0.99 + level * 0.01
            };
        }
        speech
    }

    fn reset(&mut self) {
        self.floor_db = ABSOLUTE_FLOOR_DB;
        self.seeded = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vad::FRAME;

    fn tone(amplitude: f32) -> Vec<f32> {
        (0..FRAME)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / 16_000.0).sin() * amplitude)
            .collect()
    }

    fn quiet_room() -> Vec<f32> {
        // -63 dB, which is what the reference machine actually measured in a
        // quiet room, not an invented zero.
        (0..FRAME)
            .map(|i| ((i * 7919) % 13) as f32 * 0.00005 - 0.0003)
            .collect()
    }

    #[test]
    fn silence_is_not_speech() {
        let mut d = EnergyDetector::new();
        for _ in 0..20 {
            assert!(!d.is_speech(&quiet_room()));
        }
    }

    #[test]
    fn a_loud_frame_is_speech() {
        let mut d = EnergyDetector::new();
        d.is_speech(&quiet_room());
        assert!(d.is_speech(&tone(0.3)));
    }

    #[test]
    fn digital_silence_is_not_speech_however_the_floor_moves() {
        // An unplugged or muted device gives exact zeros. Without the absolute
        // floor the adaptive one collapses toward -infinity and every frame
        // afterwards looks loud by comparison.
        let mut d = EnergyDetector::new();
        let silence = vec![0.0; FRAME];
        for _ in 0..50 {
            assert!(!d.is_speech(&silence));
        }
        assert!(
            !d.is_speech(&quiet_room()),
            "room tone is not speech either"
        );
    }

    #[test]
    fn the_floor_does_not_climb_to_meet_a_long_utterance() {
        // If the floor chases speech, a speaker goes inaudible mid-sentence.
        let mut d = EnergyDetector::new();
        d.is_speech(&quiet_room());
        let before = d.noise_floor_db();
        for _ in 0..200 {
            assert!(d.is_speech(&tone(0.3)), "should still hear the speaker");
        }
        assert!(
            (d.noise_floor_db() - before).abs() < 1.0,
            "floor moved from {before} to {} during speech",
            d.noise_floor_db()
        );
    }

    #[test]
    fn level_is_reported_in_dbfs_so_a_human_can_read_it() {
        assert!(EnergyDetector::level_db(&tone(1.0)) > -6.0);
        assert!(EnergyDetector::level_db(&vec![0.0; FRAME]) < -100.0);
    }
}
