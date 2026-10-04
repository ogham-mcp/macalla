// SPDX-License-Identifier: MIT
//! Voice activity detection: when someone is speaking, and when they stopped.
//!
//! Two roles, and the engine picks which. For a **stream-first**
//! engine this is a *gate*: it decides when to open and close the stream, keeps
//! silence out of the engine, and for a cloud endpoint is what caps the bill.
//! For a **batch** engine it is a *segmenter*: it produces the
//! [`AudioSegment`]s. [`crate::pipeline::PipelinePlan::vad_segments`] already
//! carries that choice.
//!
//! Split the same way as capture, paste and the hotkey: the POLICY here is
//! pure and driven from fixtures, and the detector behind
//! [`SpeechDetector`] is the part that needs a model.
//!
//! **The model is not here, deliberately.** Dictation listens with a neural
//! detector in `macalla-engines`, beside the other models, and the core keeps
//! the rules and no model dependency. [`energy::EnergyDetector`] needs no
//! model; the core's own tests run on it.
//!
//! ORDERING CONSTRAINT, recorded before the code existed: detect silence
//! **before** anything changes the level. A gain stage ahead of this lifts the
//! noise floor and the gate simply stops closing, silently. See the notes on
//! the audio-conditioning work.

pub mod energy;

use crate::audio::{AudioSegment, SAMPLE_RATE};
use std::time::Duration;

/// Samples per decision. 512 at 16 kHz is 32 ms. Fixed here rather than by
/// the detector, so swapping the detector cannot change the timing of
/// everything downstream: a detector that decides on less asks itself twice.
pub const FRAME: usize = 512;

/// Decides whether one frame contains speech.
///
/// The port. The detector dictation listens with is a model in
/// `macalla-engines`; [`energy::EnergyDetector`] implements it here with none.
pub trait SpeechDetector: Send {
    /// `frame` is exactly [`FRAME`] samples of 16 kHz mono.
    fn is_speech(&mut self, frame: &[f32]) -> bool;

    /// Forget any adaptation. Called between dictations.
    fn reset(&mut self);
}

/// So a caller can hold whichever detector it was given.
impl SpeechDetector for Box<dyn SpeechDetector> {
    fn is_speech(&mut self, frame: &[f32]) -> bool {
        (**self).is_speech(frame)
    }
    fn reset(&mut self) {
        (**self).reset()
    }
}

/// How eagerly the gate opens and closes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    /// Keep the gate open this long after speech stops, so an ordinary pause
    /// between words does not end the dictation.
    pub hangover: Duration,
    /// Ignore bursts shorter than this. A cough, a key press, a chair.
    pub min_speech: Duration,
    /// Audio kept from BEFORE the detector fired. Detection always lags the
    /// first phoneme, and without this the segment starts mid-word.
    pub pre_roll: Duration,
    /// The longest segment the segmenter hands on. Speech that never pauses
    /// long enough to close the gate is cut anyway, at the quietest frame in
    /// the last sixth before the cap, so a batch engine is never given an
    /// unbounded stretch in one call.
    pub max_segment: Duration,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            // Long enough for the pause between two sentences, not just a
            // comma. At 600 ms a slow, deliberate dictation closed the gate
            // mid-thought and a batch engine was handed fragments. Measured
            // from 600 ms to 2.5 s on three engines (docs/benchmarks.md),
            // starting from the 1,650 ms Handy uses for streaming models
            // (cjpais/Handy, MIT, src-tauri/src/audio_toolkit/vad/mod.rs):
            // errors fall until about 1.5 s and stay flat to 2.5 s, and this is
            // the shortest value at which slow speech reached its best. The
            // paste does not wait on it; releasing the key ends the recording.
            hangover: Duration::from_millis(1_650),
            min_speech: Duration::from_millis(120),
            pre_roll: Duration::from_millis(300),
            // The most a batch engine is handed in one call, from a file or a
            // dictation, however long someone talks without a pause. Given
            // the whole of a 13-minute reading in one call, Parakeet failed;
            // given an hour, it crashed (docs/benchmarks.md).
            max_segment: Duration::from_secs(30),
        }
    }
}

/// What the gate did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    /// Speech began. The offset is where the segment should start, which is
    /// earlier than the detection by the pre-roll.
    SpeechStart {
        at_ms: u64,
    },
    SpeechEnd {
        at_ms: u64,
    },
}

/// The gate and the segmenter, over any [`SpeechDetector`].
pub struct Vad<D> {
    detector: D,
    config: VadConfig,
    /// Samples not yet a whole frame.
    partial: Vec<f32>,
    /// Ring of recent audio, for the pre-roll.
    pre_roll: std::collections::VecDeque<f32>,
    open: bool,
    heard_speech: bool,
    /// Consecutive speech frames while closed, for `min_speech`.
    speech_run: usize,
    /// Consecutive silent frames while open, for `hangover`.
    silence_run: usize,
    /// Samples consumed, which is the clock.
    consumed: u64,
    /// Where the open segment started, including pre-roll.
    segment_start: u64,
    /// The quietest frame so far in the stretch just before the segment cap:
    /// where it starts, and its energy. That is where a cut at the cap lands.
    quietest: Option<(u64, f32)>,
}

impl<D: SpeechDetector> Vad<D> {
    pub fn new(detector: D, config: VadConfig) -> Self {
        Self {
            detector,
            config,
            partial: Vec::with_capacity(FRAME * 2),
            pre_roll: std::collections::VecDeque::new(),
            open: false,
            heard_speech: false,
            speech_run: 0,
            silence_run: 0,
            consumed: 0,
            segment_start: 0,
            quietest: None,
        }
    }

    fn pre_roll_samples(&self) -> u64 {
        (self.config.pre_roll.as_millis() as u64 * SAMPLE_RATE as u64) / 1_000
    }

    /// The cap in samples, in whole frames and never under two, so a cut
    /// always lands on a frame boundary and always leaves something behind.
    fn max_segment_samples(&self) -> u64 {
        let samples = self.config.max_segment.as_millis() as u64 * SAMPLE_RATE as u64 / 1_000;
        (samples / FRAME as u64).max(2) * FRAME as u64
    }

    /// Whether the gate is currently open.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether any speech was heard at all since the last [`Vad::reset`].
    ///
    /// This is what turns into "no speech detected" rather than an empty
    /// paste, which is the one piece of feedback a user needs when nothing
    /// happened.
    pub fn heard_speech(&self) -> bool {
        self.heard_speech
    }

    fn ms(&self, samples: u64) -> u64 {
        samples * 1_000 / SAMPLE_RATE as u64
    }

    fn frames(&self, d: Duration) -> usize {
        ((d.as_millis() as usize * SAMPLE_RATE as usize) / 1_000).div_ceil(FRAME)
    }

    /// Feed samples; get whatever the gate decided.
    pub fn push(&mut self, samples: &[f32]) -> Vec<VadEvent> {
        self.partial.extend_from_slice(samples);
        let mut events = Vec::new();
        let min_speech_frames = self.frames(self.config.min_speech).max(1);
        let hangover_frames = self.frames(self.config.hangover).max(1);
        let pre_roll = self.pre_roll_samples() as usize;
        let max_segment = self.max_segment_samples();
        let look_back_from = max_segment - max_segment / 6;

        // Walk the buffer by index and drop what was used once, at the end.
        // Draining a frame at a time from the front shifted everything behind
        // it on every frame: harmless for a poll's worth of live audio, and
        // quadratic for a whole file pushed in one call -- an hour of audio
        // spent minutes here before a single segment came out.
        let mut at = 0;
        while self.partial.len() - at >= FRAME {
            let is_speech = self.detector.is_speech(&self.partial[at..at + FRAME]);
            let frame_start = self.consumed;
            // Only the stretch just before the cap can take a cut, so only
            // there is a frame's energy worth adding up.
            let energy = (self.open
                && frame_start + FRAME as u64 - self.segment_start > look_back_from)
                .then(|| {
                    self.partial[at..at + FRAME]
                        .iter()
                        .map(|s| s * s)
                        .sum::<f32>()
                });

            // Remember this frame for the pre-roll BEFORE advancing the clock,
            // so the ring always holds the audio immediately behind us.
            if pre_roll > 0 {
                self.pre_roll
                    .extend(self.partial[at..at + FRAME].iter().copied());
                while self.pre_roll.len() > pre_roll {
                    self.pre_roll.pop_front();
                }
            }
            at += FRAME;
            self.consumed += FRAME as u64;

            if self.open {
                if is_speech {
                    self.silence_run = 0;
                } else {
                    self.silence_run += 1;
                    if self.silence_run >= hangover_frames {
                        // End where the speech actually stopped, not where we
                        // finished waiting -- the hangover is patience, not
                        // content.
                        // Never before the segment's own start: a cut at the
                        // cap can land inside the pause that is now closing.
                        let quiet_for = (self.silence_run * FRAME) as u64;
                        let at = self
                            .consumed
                            .saturating_sub(quiet_for)
                            .max(self.segment_start);
                        events.push(VadEvent::SpeechEnd { at_ms: self.ms(at) });
                        self.open = false;
                        self.silence_run = 0;
                        self.speech_run = 0;
                        self.quietest = None;
                    }
                }
                if self.open {
                    if let Some(energy) = energy {
                        if self.quietest.is_none_or(|(_, quiet)| energy < quiet) {
                            self.quietest = Some((frame_start, energy));
                        }
                    }
                    if self.consumed - self.segment_start >= max_segment {
                        // No pause long enough to close the gate, and the
                        // segment is at its cap. End it where it was quietest
                        // and carry straight on: the next segment starts at
                        // the same sample, with no pre-roll, because nothing
                        // between the two was missed. The frame just heard is
                        // always a candidate, so there is always a quietest.
                        let cut = self.quietest.take().map_or(frame_start, |(at, _)| at);
                        events.push(VadEvent::SpeechEnd {
                            at_ms: self.ms(cut),
                        });
                        events.push(VadEvent::SpeechStart {
                            at_ms: self.ms(cut),
                        });
                        self.segment_start = cut;
                    }
                }
            } else if is_speech {
                self.speech_run += 1;
                if self.speech_run >= min_speech_frames {
                    // Reach back by the pre-roll: detection always lags the
                    // first phoneme, and the run we needed to be sure is
                    // itself already in the past.
                    let heard_for = (self.speech_run * FRAME) as u64;
                    let start = self
                        .consumed
                        .saturating_sub(heard_for)
                        .saturating_sub(self.pre_roll_samples());
                    self.segment_start = start;
                    events.push(VadEvent::SpeechStart {
                        at_ms: self.ms(start),
                    });
                    self.open = true;
                    self.heard_speech = true;
                    self.silence_run = 0;
                }
            } else {
                self.speech_run = 0;
            }
        }
        self.partial.drain(..at);
        events
    }

    /// Close the gate at end of input, so a dictation that ends while someone
    /// is still speaking still produces an end.
    pub fn finish(&mut self) -> Vec<VadEvent> {
        // Whatever is left is shorter than a frame and cannot change a
        // decision, so it is dropped rather than zero-padded into a fake one.
        self.partial.clear();
        if !self.open {
            return Vec::new();
        }
        self.open = false;
        self.silence_run = 0;
        self.speech_run = 0;
        self.quietest = None;
        vec![VadEvent::SpeechEnd {
            at_ms: self.ms(self.consumed),
        }]
    }

    /// Cut a whole recording into speech segments. The batch path.
    ///
    /// Holds every segment at once, which for a long recording is a second
    /// copy of its speech. A caller that handles one segment at a time should
    /// use [`Vad::segments`], which cuts the same segments.
    pub fn segment(&mut self, audio: &AudioSegment) -> Vec<AudioSegment> {
        self.segments(audio).collect()
    }

    /// Cut a whole recording into speech segments, one at a time.
    ///
    /// The recording goes through the gate about a second at a time, and a
    /// segment is cut as soon as the gate reaches its end. So nothing the size
    /// of the recording is allocated here, and a caller can transcribe and
    /// drop each segment before the next is cut. The gate decides the same
    /// whatever size its pushes are, so these are the segments one push of
    /// the whole recording would give.
    ///
    /// Why not one push: the frame buffer copied the whole recording and kept
    /// that allocation, and memory a process has freed still counts towards
    /// its footprint, so for an hour-long file it cost a copy of every sample.
    pub fn segments<'a>(&'a mut self, audio: &'a AudioSegment) -> Segments<'a, D> {
        self.reset();
        Segments {
            vad: self,
            audio,
            pushed: 0,
            start_ms: None,
            pending: std::collections::VecDeque::new(),
            finished: false,
        }
    }

    /// Forget everything, between dictations.
    pub fn reset(&mut self) {
        self.detector.reset();
        self.partial.clear();
        self.pre_roll.clear();
        self.open = false;
        self.heard_speech = false;
        self.speech_run = 0;
        self.silence_run = 0;
        self.consumed = 0;
        self.segment_start = 0;
        self.quietest = None;
    }
}

/// How much of a recording [`Vad::segments`] pushes through the gate at once:
/// 32 frames, about a second. Small enough that the frame buffer never holds
/// more than that, and large enough that the gate is not called sample by
/// sample.
const SEGMENTING_CHUNK: usize = FRAME * 32;

/// The segments of one recording, each cut as the gate reaches its end. See
/// [`Vad::segments`].
pub struct Segments<'a, D> {
    vad: &'a mut Vad<D>,
    audio: &'a AudioSegment,
    /// Samples of the recording already pushed through the gate.
    pushed: usize,
    /// Where the open segment started, once the gate has said.
    start_ms: Option<u64>,
    /// Decisions from the gate not yet turned into segments.
    pending: std::collections::VecDeque<VadEvent>,
    /// Whether the gate has been closed at the end of the recording.
    finished: bool,
}

impl<D: SpeechDetector> Iterator for Segments<'_, D> {
    type Item = AudioSegment;

    fn next(&mut self) -> Option<AudioSegment> {
        let audio = self.audio;
        let samples = &audio.samples;
        loop {
            while let Some(event) = self.pending.pop_front() {
                match event {
                    VadEvent::SpeechStart { at_ms } => self.start_ms = Some(at_ms),
                    VadEvent::SpeechEnd { at_ms } => {
                        let Some(from) = self.start_ms.take() else {
                            continue;
                        };
                        let a = (from * SAMPLE_RATE as u64 / 1_000) as usize;
                        let b = ((at_ms * SAMPLE_RATE as u64 / 1_000) as usize).min(samples.len());
                        if b > a {
                            return Some(AudioSegment::new(
                                samples[a..b].to_vec(),
                                audio.start_ms + from,
                            ));
                        }
                    }
                }
            }
            if self.finished {
                return None;
            }
            if self.pushed < samples.len() {
                let end = (self.pushed + SEGMENTING_CHUNK).min(samples.len());
                self.pending
                    .extend(self.vad.push(&samples[self.pushed..end]));
                self.pushed = end;
            } else {
                self.pending.extend(self.vad.finish());
                self.finished = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::energy::EnergyDetector;
    use super::*;

    fn frames_of(n: usize, amplitude: f32) -> Vec<f32> {
        (0..n * FRAME)
            .map(|i| (std::f32::consts::TAU * 440.0 * i as f32 / 16_000.0).sin() * amplitude)
            .collect()
    }

    fn speech(frames: usize) -> Vec<f32> {
        frames_of(frames, 0.3)
    }

    fn room(frames: usize) -> Vec<f32> {
        (0..frames * FRAME)
            .map(|i| ((i * 7919) % 13) as f32 * 0.00005 - 0.0003)
            .collect()
    }

    /// The rules under test, at the hangover these fixtures were sized for:
    /// tones a second apart are two utterances here. The default is pinned
    /// by its own test.
    fn rules() -> VadConfig {
        VadConfig {
            hangover: Duration::from_millis(600),
            ..VadConfig::default()
        }
    }

    fn vad() -> Vad<EnergyDetector> {
        Vad::new(EnergyDetector::new(), rules())
    }

    #[test]
    fn by_default_a_pause_between_sentences_does_not_end_the_utterance() {
        // A slow speaker pauses well over a second between sentences; closing
        // the gate there handed a batch engine fragments.
        let mut v = Vad::new(EnergyDetector::new(), VadConfig::default());
        v.push(&room(10));
        v.push(&speech(20));
        let pause = v.push(&room(40)); // ~1.3 s
        assert!(pause.is_empty(), "a 1.3 s pause closed the gate: {pause:?}");
        assert!(v.is_open());
        let closing = v.push(&room(20)); // ~1.9 s in all
        assert!(
            matches!(closing.last(), Some(VadEvent::SpeechEnd { .. })),
            "two seconds of quiet should close it: {closing:?}"
        );
    }

    #[test]
    fn pushing_a_recording_whole_or_in_pieces_decides_the_same() {
        // A file arrives in one push; live audio arrives a poll at a time.
        // The gate must not care which, down to a single sample per push.
        let audio = [
            room(20),
            speech(40),
            room(40),
            speech(10),
            room(5),
            speech(30),
            room(30),
        ]
        .concat();
        let mut whole = vad();
        let mut expected = whole.push(&audio);
        expected.extend(whole.finish());
        assert_eq!(
            expected.len(),
            4,
            "two utterances should open and close the gate twice: {expected:?}"
        );

        for size in [1, 333, 4_096, 16_000] {
            let mut pieces = vad();
            let mut got = Vec::new();
            for chunk in audio.chunks(size) {
                got.extend(pieces.push(chunk));
            }
            got.extend(pieces.finish());
            assert_eq!(got, expected, "pushed {size} samples at a time");
        }
    }

    /// Segments cut the way the batch path cut them before it pushed a chunk
    /// at a time: every decision from one push of the whole recording, then
    /// each stretch from a start to its end copied out.
    fn cut_from_one_push<D: SpeechDetector>(
        v: &mut Vad<D>,
        audio: &AudioSegment,
    ) -> Vec<(u64, Vec<f32>)> {
        v.reset();
        let mut events = v.push(&audio.samples);
        events.extend(v.finish());
        let mut out = Vec::new();
        let mut start_ms = None;
        for event in events {
            match event {
                VadEvent::SpeechStart { at_ms } => start_ms = Some(at_ms),
                VadEvent::SpeechEnd { at_ms } => {
                    let Some(from) = start_ms.take() else {
                        continue;
                    };
                    let a = (from * SAMPLE_RATE as u64 / 1_000) as usize;
                    let b =
                        ((at_ms * SAMPLE_RATE as u64 / 1_000) as usize).min(audio.samples.len());
                    if b > a {
                        out.push((audio.start_ms + from, audio.samples[a..b].to_vec()));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn segments_one_at_a_time_are_the_segments_of_one_whole_push() {
        let two_utterances = [
            room(20),
            speech(40),
            room(40),
            speech(10),
            room(5),
            speech(30),
            room(30),
        ]
        .concat();
        let no_pause = [room(10), speech(300), room(40)].concat();
        let two_second_cap: fn() -> Vad<EnergyDetector> = || capped(Duration::from_secs(2));
        // A name, a recording, and the gate to cut it with.
        type Case = (&'static str, Vec<f32>, fn() -> Vad<EnergyDetector>);
        let cases: [Case; 5] = [
            ("two utterances", two_utterances, vad),
            (
                "no pause, under a 2 s cap",
                no_pause.clone(),
                two_second_cap,
            ),
            ("no pause, under the default cap", no_pause, vad),
            ("silent", room(60), vad),
            (
                "ends while still speaking",
                [room(20), speech(90)].concat(),
                two_second_cap,
            ),
        ];
        for (name, samples, make) in cases {
            // A start offset that is not zero, so segment offsets are checked.
            let audio = AudioSegment::new(samples, 250);
            let expected = cut_from_one_push(&mut make(), &audio);
            assert!(
                name == "silent" || !expected.is_empty(),
                "{name}: no segments to compare"
            );

            let one_at_a_time: Vec<(u64, Vec<f32>)> = make()
                .segments(&audio)
                .map(|s| (s.start_ms, s.samples))
                .collect();
            assert_eq!(one_at_a_time, expected, "{name}, through segments()");

            let batch: Vec<(u64, Vec<f32>)> = make()
                .segment(&audio)
                .into_iter()
                .map(|s| (s.start_ms, s.samples))
                .collect();
            assert_eq!(batch, expected, "{name}, through segment()");
        }
    }

    #[test]
    fn a_segment_comes_out_before_the_rest_of_the_recording_is_pushed() {
        // What one at a time is for: the first segment is cut while most of a
        // long recording has not been through the gate at all.
        let audio = AudioSegment::new([room(20), speech(40), room(40), room(1_000)].concat(), 0);
        let mut v = vad();
        let mut segments = v.segments(&audio);
        assert!(segments.next().is_some());
        assert!(
            segments.pushed < audio.samples.len() / 4,
            "the first segment waited until {} of {} samples had been pushed",
            segments.pushed,
            audio.samples.len()
        );
        assert!(segments.next().is_none());
    }

    #[test]
    fn a_quiet_room_opens_nothing() {
        let mut v = vad();
        assert!(v.push(&room(60)).is_empty());
        assert!(!v.is_open());
        assert!(!v.heard_speech(), "this is the 'no speech detected' signal");
    }

    #[test]
    fn speech_opens_the_gate_and_silence_closes_it() {
        let mut v = vad();
        let mut events = v.push(&room(10));
        events.extend(v.push(&speech(20)));
        assert!(
            matches!(events.first(), Some(VadEvent::SpeechStart { .. })),
            "expected a start, got {events:?}"
        );
        assert!(v.is_open());
        assert!(v.heard_speech());

        // Long enough to exceed the hangover.
        let closing = v.push(&room(40));
        assert!(
            matches!(closing.last(), Some(VadEvent::SpeechEnd { .. })),
            "expected an end, got {closing:?}"
        );
        assert!(!v.is_open());
    }

    #[test]
    fn a_click_shorter_than_min_speech_is_ignored() {
        // A key press, a cough, a chair. One loud frame is not a dictation.
        let mut v = vad();
        v.push(&room(10));
        let events = v.push(&speech(1));
        assert!(
            events.is_empty(),
            "one frame should not open a gate: {events:?}"
        );
        assert!(!v.is_open());
    }

    #[test]
    fn a_pause_between_words_does_not_end_the_dictation() {
        // The whole point of the hangover. A comma is not the end of a
        // sentence and a breath is not the end of a dictation.
        let mut v = vad();
        v.push(&room(10));
        v.push(&speech(20));
        assert!(v.is_open());

        let mid = v.push(&room(8)); // ~256 ms, well under the 600 ms hangover
        assert!(
            mid.is_empty(),
            "a short pause must not close the gate: {mid:?}"
        );
        assert!(v.is_open(), "still mid-sentence");

        v.push(&speech(10));
        assert!(v.is_open());
    }

    #[test]
    fn the_segment_starts_before_the_detector_fired() {
        // Detection lags the first phoneme. Without pre-roll the paste begins
        // mid-word, which is the most obviously broken thing a dictation tool
        // can do.
        let mut v = vad();
        v.push(&room(30));
        let events = v.push(&speech(20));
        let VadEvent::SpeechStart { at_ms } = events[0] else {
            panic!("expected a start, got {events:?}");
        };
        // 30 frames of room is ~960 ms, so detection lands after that. The
        // start must be meaningfully earlier.
        let detected_at = 30 * FRAME as u64 * 1_000 / SAMPLE_RATE as u64;
        assert!(
            at_ms < detected_at,
            "start {at_ms} ms is not before detection at ~{detected_at} ms"
        );
    }

    #[test]
    fn finishing_while_someone_is_still_talking_still_produces_an_end() {
        let mut v = vad();
        v.push(&room(10));
        v.push(&speech(20));
        assert!(v.is_open());
        let events = v.finish();
        assert!(
            matches!(events.last(), Some(VadEvent::SpeechEnd { .. })),
            "a dictation cut short must still close: {events:?}"
        );
        assert!(!v.is_open());
    }

    #[test]
    fn finishing_a_quiet_recording_produces_nothing() {
        let mut v = vad();
        v.push(&room(20));
        assert!(v.finish().is_empty());
    }

    #[test]
    fn the_batch_path_cuts_one_segment_per_utterance() {
        let mut audio = room(20);
        audio.extend(speech(20));
        audio.extend(room(40));
        audio.extend(speech(20));
        audio.extend(room(20));

        let mut v = vad();
        let segments = v.segment(&AudioSegment::new(audio, 0));
        assert_eq!(segments.len(), 2, "two utterances, two segments");
        for s in &segments {
            assert!(
                s.duration_ms() > 300,
                "a segment of {} ms is too short to be an utterance",
                s.duration_ms()
            );
        }
    }

    #[test]
    fn the_batch_path_returns_nothing_for_a_silent_recording() {
        let mut v = vad();
        assert!(v.segment(&AudioSegment::new(room(60), 0)).is_empty());
    }

    #[test]
    fn segmenting_a_long_recording_never_buffers_it_whole() {
        // A file pushed in one call was copied whole into the frame buffer,
        // and the buffer kept that allocation for as long as the gate lived:
        // for an hour-long file, a second copy of every sample.
        let audio = [room(20), speech(600), room(40), speech(600), room(40)].concat();
        let mut v = vad();
        let segments = v.segment(&AudioSegment::new(audio.clone(), 0));
        assert!(!segments.is_empty());
        assert!(
            v.partial.capacity() < audio.len() / 4,
            "the frame buffer grew to {} samples for a {}-sample recording",
            v.partial.capacity(),
            audio.len()
        );
    }

    #[test]
    fn segments_carry_their_offset_so_transcripts_can_be_placed() {
        let mut audio = room(20);
        audio.extend(speech(20));
        audio.extend(room(30));
        let mut v = vad();
        let segments = v.segment(&AudioSegment::new(audio, 0));
        assert_eq!(segments.len(), 1);
        assert!(
            segments[0].start_ms > 0,
            "a segment starting after silence should not claim offset 0"
        );
    }

    fn capped(max_segment: Duration) -> Vad<EnergyDetector> {
        Vad::new(
            EnergyDetector::new(),
            VadConfig {
                max_segment,
                ..rules()
            },
        )
    }

    #[test]
    fn speech_with_no_pause_is_cut_at_the_cap_and_none_of_it_is_lost() {
        // A reading or a meeting can run for minutes without a pause long
        // enough to close the gate. Handed to a batch engine whole, an hour of
        // it crashed Parakeet. The cap cuts it, and the pieces must abut.
        let cap = Duration::from_secs(2);
        let audio = [room(10), speech(300), room(40)].concat();
        let segments = capped(cap).segment(&AudioSegment::new(audio.clone(), 0));
        assert!(
            segments.len() >= 5,
            "9.6 s under a 2 s cap came out as {} segments",
            segments.len()
        );
        for s in &segments {
            assert!(s.duration_ms() <= cap.as_millis() as u64, "{s:?}");
        }
        for pair in segments.windows(2) {
            assert_eq!(
                pair[0].start_ms + pair[0].duration_ms(),
                pair[1].start_ms,
                "the pieces abut"
            );
        }

        // Where the cut lands does not depend on how the audio arrived.
        let mut whole = capped(cap);
        let mut expected = whole.push(&audio);
        expected.extend(whole.finish());
        let mut pieces = capped(cap);
        let mut got = Vec::new();
        for chunk in audio.chunks(333) {
            got.extend(pieces.push(chunk));
        }
        got.extend(pieces.finish());
        assert_eq!(got, expected);
    }

    #[test]
    fn the_cut_lands_at_the_quietest_moment_before_the_cap() {
        // A cut exactly at the cap would split a word. Inside the stretch just
        // before it, the quietest frame is the gap between two words.
        let dip = 58; // inside the look-back of a 2 s cap
        let audio = [
            room(10),
            speech(dip - 10),
            frames_of(1, 0.1),
            speech(100),
            room(40),
        ]
        .concat();
        let segments = capped(Duration::from_secs(2)).segment(&AudioSegment::new(audio, 0));
        let first = &segments[0];
        assert_eq!(
            first.start_ms + first.duration_ms(),
            (dip * FRAME) as u64 * 1_000 / SAMPLE_RATE as u64
        );
    }

    #[test]
    fn reset_forgets_the_previous_dictation() {
        let mut v = vad();
        v.push(&room(10));
        v.push(&speech(20));
        assert!(v.heard_speech() && v.is_open());
        v.reset();
        assert!(
            !v.heard_speech(),
            "a new dictation starts having heard nothing"
        );
        assert!(!v.is_open());
    }
}
