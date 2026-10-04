// SPDX-License-Identifier: MIT
//! A stream over a batch engine.
//!
//! A stream-first engine gets a gate, a batch engine gets a
//! segmenter. This is the segmenter side, shaped as an [`SttStream`] so the
//! record loop drives Parakeet exactly as it drives Apple and never learns
//! which kind it has. Samples go in as they are captured; the VAD cuts them
//! into utterances at the pauses; each utterance goes to
//! [`SttEngine::transcribe`] as it closes and comes back as one final
//! transcript. There are no partials: a batch engine has nothing to say
//! until an utterance ends, and inventing a preview would be a lie the
//! overlay then shows.
//!
//! The engine call runs on the pipeline thread, inside `push`, which holds up
//! the next poll by the cost of one utterance -- a few hundred milliseconds on
//! a CPU for a five-second segment. The capture ring is sized for far more
//! than that, so no audio is lost; only the waveform pauses.

use super::{SttContext, SttEngine, SttStream, Transcript};
use crate::audio::{AudioSegment, SAMPLE_RATE};
use crate::vad::{SpeechDetector, Vad, VadConfig, VadEvent};
use crate::Result;

pub struct BatchStream<'a> {
    engine: &'a dyn SttEngine,
    ctx: SttContext,
    vad: Vad<Box<dyn SpeechDetector>>,
    /// Samples since the stream opened, less the prefix already handed to the
    /// engine. `dropped` is how many samples that prefix held, so VAD offsets,
    /// which count from the stream's start, still index into `kept`.
    kept: Vec<f32>,
    dropped: usize,
    /// Where the utterance in progress began, from the stream's start.
    open_at_ms: Option<u64>,
    pending: Vec<Transcript>,
}

impl<'a> BatchStream<'a> {
    pub fn new(
        engine: &'a dyn SttEngine,
        ctx: &SttContext,
        detector: Box<dyn SpeechDetector>,
    ) -> Self {
        Self {
            engine,
            ctx: ctx.clone(),
            vad: Vad::new(detector, VadConfig::default()),
            kept: Vec::new(),
            dropped: 0,
            open_at_ms: None,
            pending: Vec::new(),
        }
    }

    fn sample_at(ms: u64) -> usize {
        (ms * SAMPLE_RATE as u64 / 1_000) as usize
    }

    fn take(&mut self, events: Vec<VadEvent>) -> Result<()> {
        for event in events {
            match event {
                VadEvent::SpeechStart { at_ms } => self.open_at_ms = Some(at_ms),
                VadEvent::SpeechEnd { at_ms } => {
                    let Some(from_ms) = self.open_at_ms.take() else {
                        continue;
                    };
                    let a = Self::sample_at(from_ms).saturating_sub(self.dropped);
                    let b = Self::sample_at(at_ms)
                        .saturating_sub(self.dropped)
                        .min(self.kept.len());
                    if b <= a {
                        continue;
                    }
                    let segment = AudioSegment::new(self.kept[a..b].to_vec(), from_ms);
                    // Everything before the cut is spoken for. Keep the tail:
                    // the next utterance's pre-roll may reach into it.
                    self.kept.drain(..a);
                    self.dropped += a;
                    let transcript = self.engine.transcribe(&segment, &self.ctx)?;
                    if !transcript.text.trim().is_empty() {
                        self.pending.push(Transcript {
                            is_final: true,
                            ..transcript
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

impl SttStream for BatchStream<'_> {
    fn push(&mut self, samples: &[f32]) -> Result<()> {
        self.kept.extend_from_slice(samples);
        let events = self.vad.push(samples);
        self.take(events)
    }

    fn poll(&mut self) -> Result<Vec<Transcript>> {
        Ok(std::mem::take(&mut self.pending))
    }

    fn finish(mut self: Box<Self>) -> Result<Vec<Transcript>> {
        let events = self.vad.finish();
        self.take(events)?;
        Ok(std::mem::take(&mut self.pending))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::{EngineCaps, STREAM_CHUNK};
    use crate::vad::FRAME;
    use std::sync::{Arc, Mutex};

    const FRAME_MS: u64 = FRAME as u64 * 1_000 / SAMPLE_RATE as u64;

    /// Says which frames are speech from a script, so the cut points are
    /// known in advance and the test is about the adapter, not the detector.
    struct Scripted {
        frames: usize,
        speech: Vec<std::ops::Range<usize>>,
    }
    impl SpeechDetector for Scripted {
        fn is_speech(&mut self, _frame: &[f32]) -> bool {
            let i = self.frames;
            self.frames += 1;
            self.speech.iter().any(|r| r.contains(&i))
        }
        fn reset(&mut self) {
            self.frames = 0;
        }
    }

    /// Records every segment it is given and answers with its length.
    struct Counting {
        seen: Arc<Mutex<Vec<AudioSegment>>>,
    }
    impl SttEngine for Counting {
        fn id(&self) -> &'static str {
            "counting"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::batch()
        }
        fn stream(&self, _: &SttContext) -> Result<Box<dyn SttStream>> {
            Err(crate::Error::Unsupported {
                engine: "counting",
                what: "streaming",
            })
        }
        fn transcribe(&self, audio: &AudioSegment, _: &SttContext) -> Result<Transcript> {
            self.seen.lock().unwrap().push(audio.clone());
            Ok(Transcript {
                text: format!("{} ms", audio.duration_ms()),
                is_final: true,
                start_ms: audio.start_ms,
                end_ms: audio.start_ms + audio.duration_ms(),
            })
        }
    }

    fn stream_over<'a>(
        engine: &'a Counting,
        speech: Vec<std::ops::Range<usize>>,
    ) -> Box<dyn SttStream + 'a> {
        let detector = Scripted { frames: 0, speech };
        Box::new(BatchStream::new(
            engine,
            &SttContext::default(),
            Box::new(detector),
        ))
    }

    #[test]
    fn utterances_are_cut_at_the_pauses_and_each_goes_to_the_engine_once() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let engine = Counting { seen: seen.clone() };
        // Two utterances, frames 10..40 and 100..130, in a 200-frame stream,
        // pushed in 100 ms chunks as the record loop does.
        let mut stream = stream_over(&engine, vec![10..40, 100..130]);
        let chunk = vec![0.0f32; STREAM_CHUNK];
        let mut out = Vec::new();
        let mut pushed = 0;
        while pushed < 200 * FRAME {
            stream.push(&chunk).unwrap();
            pushed += chunk.len();
            out.extend(stream.poll().unwrap());
        }
        out.extend(stream.finish().unwrap());

        let segments = seen.lock().unwrap();
        assert_eq!(segments.len(), 2, "one call per utterance: {out:?}");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|t| t.is_final));
        // The first starts at or before frame 10 (pre-roll) and ends after
        // frame 40 (hangover); the second starts after the first ended and
        // at or before frame 100.
        assert!(
            segments[0].start_ms <= 10 * FRAME_MS,
            "{}",
            segments[0].start_ms
        );
        assert!(segments[0].start_ms + segments[0].duration_ms() >= 40 * FRAME_MS);
        assert!(segments[1].start_ms <= 100 * FRAME_MS);
        assert!(segments[1].start_ms >= segments[0].start_ms + segments[0].duration_ms());
        assert_eq!(out[0].start_ms, segments[0].start_ms);
        // The samples handed over are exactly the window the VAD named,
        // found in the right place even after the first prefix was dropped.
        for s in segments.iter() {
            assert_eq!(
                s.samples.len() as u64 * 1_000 / SAMPLE_RATE as u64,
                s.duration_ms()
            );
        }
    }

    #[test]
    fn an_utterance_still_open_at_the_end_is_flushed_by_finish() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let engine = Counting { seen: seen.clone() };
        let mut stream = stream_over(&engine, std::iter::once(5..1_000).collect());
        let chunk = vec![0.0f32; STREAM_CHUNK];
        for _ in 0..30 {
            stream.push(&chunk).unwrap();
        }
        assert!(
            stream.poll().unwrap().is_empty(),
            "nothing until the utterance closes"
        );
        let out = stream.finish().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_utterance_longer_than_the_cap_reaches_the_engine_in_abutting_pieces() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let engine = Counting { seen: seen.clone() };
        // About 64 seconds with no pause, under the default cap.
        let mut stream = stream_over(&engine, std::iter::once(5..2_000).collect());
        let chunk = vec![0.0f32; STREAM_CHUNK];
        for _ in 0..(2_050 * FRAME).div_ceil(STREAM_CHUNK) {
            stream.push(&chunk).unwrap();
            stream.poll().unwrap();
        }
        stream.finish().unwrap();

        let segments = seen.lock().unwrap();
        let cap = VadConfig::default().max_segment.as_millis() as u64;
        assert!(segments.len() >= 3, "{} pieces", segments.len());
        for s in segments.iter() {
            assert!(s.duration_ms() <= cap, "{} ms", s.duration_ms());
        }
        for pair in segments.windows(2) {
            assert_eq!(pair[0].start_ms + pair[0].duration_ms(), pair[1].start_ms);
        }
    }

    #[test]
    fn silence_reaches_the_engine_never() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let engine = Counting { seen: seen.clone() };
        let mut stream = stream_over(&engine, vec![]);
        let chunk = vec![0.0f32; STREAM_CHUNK];
        for _ in 0..30 {
            stream.push(&chunk).unwrap();
        }
        assert!(stream.finish().unwrap().is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }
}
