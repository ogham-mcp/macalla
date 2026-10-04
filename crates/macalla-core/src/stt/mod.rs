// SPDX-License-Identifier: MIT
//! The STT engine trait.
//!
//! Streaming is the primary path. Apple SpeechTranscriber
//! and Parakeet Realtime take a continuous stream and do their own
//! endpointing. Batch transcription is the fallback for engines that want a
//! discrete blob.
//!
//! [`EngineCaps::owns_segmentation`] picks the path, and it decides what VAD
//! is *for*: a **gate** on the stream-first path, a **segmenter** on the batch
//! path. VAD is never removed -- on the stream-first path it stops silence
//! being processed.
//!
//! Cloud transcription is on neither path. It gets the whole recording in one
//! request when the key is released: the batch path calls the
//! engine inside the record loop, and a network request there would hold the
//! loop up for seconds per utterance.

mod batch;
pub use batch::BatchStream;

use crate::audio::AudioSegment;
use crate::vad::SpeechDetector;
use crate::Result;

/// What an engine can do. The pipeline reads this to choose a path; it never
/// special-cases an engine by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCaps {
    /// The engine can consume a continuous sample stream.
    pub streaming: bool,
    /// The engine performs its own endpointing and emits its own segments.
    /// When true, VAD gates the stream rather than cutting it.
    pub owns_segmentation: bool,
    /// The engine accepts vocabulary biasing through [`SttContext::terms`].
    pub vocab_bias: bool,
    /// The engine is usable without a GPU or neural accelerator.
    pub cpu_ok: bool,
    /// BCP-47 tags the engine supports, or empty for "detects automatically".
    pub languages: Vec<String>,
    /// Two streams can run on ONE instance at once without either waiting on
    /// the other for the stream's whole life, as a live meeting's two voices
    /// need. A live meeting reads this, never the engine's id, to decide
    /// whether its second stream needs an instance of its own.
    ///
    /// Never assumed. Each engine's value is either measured on the path a
    /// live meeting uses (docs/benchmarks.md, "At real-time pace") or
    /// inferred from a measured engine on the same code path, and the comment
    /// where the engine sets it says which. False is the safe answer while
    /// neither holds: it costs a second instance, never a stalled stream.
    pub concurrent_streams: bool,
}

impl EngineCaps {
    /// A stream-first engine that endpoints for itself: Apple
    /// SpeechTranscriber, Parakeet Realtime.
    pub fn stream_first() -> Self {
        Self {
            streaming: true,
            owns_segmentation: true,
            vocab_bias: false,
            cpu_ok: true,
            languages: Vec::new(),
            concurrent_streams: false,
        }
    }

    /// A batch engine that needs VAD to cut segments for it: Parakeet TDT,
    /// Whisper, Moonshine.
    pub fn batch() -> Self {
        Self {
            streaming: false,
            owns_segmentation: false,
            vocab_bias: false,
            cpu_ok: true,
            languages: Vec::new(),
            concurrent_streams: false,
        }
    }
}

/// Per-dictation context handed to the engine.
#[derive(Debug, Clone, Default)]
pub struct SttContext {
    /// Dictionary `written` forms, for engines with `vocab_bias`. Engines
    /// without it ignore these; the phonetic layer catches them afterwards.
    pub terms: Vec<String>,
    /// BCP-47 tag, or `None` for automatic detection.
    pub language: Option<String>,
}

/// One engine result. `is_final` distinguishes a volatile preview from a
/// committed segment: preview text is shown but never pasted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    pub is_final: bool,
    pub start_ms: u64,
    pub end_ms: u64,
}

/// An open streaming session. Push samples, pull transcripts.
pub trait SttStream: Send {
    /// Feed captured samples. Called from the pipeline thread, never from the
    /// audio callback.
    fn push(&mut self, samples: &[f32]) -> Result<()>;

    /// Drain whatever the engine has produced since the last call.
    fn poll(&mut self) -> Result<Vec<Transcript>>;

    /// Close the stream and return any trailing results.
    fn finish(self: Box<Self>) -> Result<Vec<Transcript>>;
}

pub trait SttEngine: Send + Sync {
    fn id(&self) -> &'static str;

    fn caps(&self) -> EngineCaps;

    /// Primary path. Errors with [`crate::Error::Unsupported`] on a
    /// batch-only engine.
    fn stream(&self, ctx: &SttContext) -> Result<Box<dyn SttStream>>;

    /// Fallback path, for engines that take discrete audio.
    fn transcribe(&self, audio: &AudioSegment, ctx: &SttContext) -> Result<Transcript>;
}

/// Samples handed to a stream per push: 100 ms at 16 kHz.
///
/// This is roughly what one poll of the capture ring delivers, so driving a
/// fixture through the same size exercises the engine the way live audio will.
pub const STREAM_CHUNK: usize = (crate::audio::SAMPLE_RATE as usize) / 10;

/// Transcribe a whole recording down whichever path the engine's capabilities
/// call for.
///
/// Streaming is primary and batch is the fallback. The choice
/// comes from [`EngineCaps::streaming`] and never from the engine's name. An
/// engine whose caps claim streaming but whose [`SttEngine::stream`] reports
/// [`crate::Error::Unsupported`] falls back to batch rather than failing the
/// dictation -- that is what "fallback" means here. Any other error is real and
/// is returned.
///
/// Returns every transcript the engine emitted, partials included, in order.
/// The overlay wants the partials; use [`final_text`] for the text to paste.
pub fn transcribe_segment(
    engine: &dyn SttEngine,
    audio: &AudioSegment,
    ctx: &SttContext,
) -> Result<Vec<Transcript>> {
    if engine.caps().streaming {
        match engine.stream(ctx) {
            Ok(mut session) => {
                let mut out = Vec::new();
                for chunk in audio.samples.chunks(STREAM_CHUNK) {
                    session.push(chunk)?;
                    out.extend(session.poll()?);
                }
                out.extend(session.finish()?);
                return Ok(out);
            }
            // The documented fallback, and only this error. Anything else is a
            // real failure and hiding it behind a slower path would hide a bug.
            Err(crate::Error::Unsupported { engine: id, what }) => {
                tracing::debug!(engine = id, what, "no stream; falling back to batch");
            }
            Err(other) => return Err(other),
        }
    }
    Ok(vec![engine.transcribe(audio, ctx)?])
}

/// A live stream from any engine: the engine's own for a stream-first one,
/// and a [`BatchStream`] cut by the VAD segmenter for a batch one. The record
/// loop calls this and never learns which it got. The same fallback rule as
/// [`transcribe_segment`]: an engine whose caps claim streaming but which
/// reports [`crate::Error::Unsupported`] is treated as batch.
///
/// `detector` does the cutting for a batch engine and is unused otherwise.
/// The caller picks it: the core holds the rules for cutting, not the model
/// that hears speech.
pub fn open_stream<'a>(
    engine: &'a dyn SttEngine,
    ctx: &SttContext,
    detector: Box<dyn SpeechDetector>,
) -> Result<Box<dyn SttStream + 'a>> {
    if engine.caps().streaming {
        match engine.stream(ctx) {
            Ok(stream) => return Ok(stream),
            Err(crate::Error::Unsupported { engine: id, what }) => {
                tracing::debug!(engine = id, what, "no stream; segmenting for batch");
            }
            Err(other) => return Err(other),
        }
    }
    Ok(Box::new(BatchStream::new(engine, ctx, detector)))
}

/// The text to paste: committed segments only, joined with single spaces.
///
/// Partials are volatile previews. They are shown and never pasted.
pub fn final_text(transcripts: &[Transcript]) -> String {
    let mut out = String::new();
    for t in transcripts.iter().filter(|t| t.is_final) {
        let piece = t.text.trim();
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(piece);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream-first engine: records how it was driven, and emits one partial
    /// per push plus a final on close.
    #[derive(Default)]
    struct FakeStream {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct FakeSession {
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        pending: Vec<Transcript>,
        seen: usize,
    }

    impl SttStream for FakeSession {
        fn push(&mut self, samples: &[f32]) -> Result<()> {
            self.log
                .lock()
                .unwrap()
                .push(format!("push {}", samples.len()));
            self.seen += samples.len();
            self.pending.push(Transcript {
                text: format!("partial at {}", self.seen),
                is_final: false,
                start_ms: 0,
                end_ms: 0,
            });
            Ok(())
        }
        fn poll(&mut self) -> Result<Vec<Transcript>> {
            self.log.lock().unwrap().push("poll".into());
            Ok(std::mem::take(&mut self.pending))
        }
        fn finish(self: Box<Self>) -> Result<Vec<Transcript>> {
            self.log.lock().unwrap().push("finish".into());
            Ok(vec![Transcript {
                text: "the whole thing".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 1_000,
            }])
        }
    }

    impl SttEngine for FakeStream {
        fn id(&self) -> &'static str {
            "fake-stream"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::stream_first()
        }
        fn stream(&self, _ctx: &SttContext) -> Result<Box<dyn SttStream>> {
            self.log.lock().unwrap().push("stream opened".into());
            Ok(Box::new(FakeSession {
                log: std::sync::Arc::clone(&self.log),
                pending: Vec::new(),
                seen: 0,
            }))
        }
        fn transcribe(&self, _audio: &AudioSegment, _ctx: &SttContext) -> Result<Transcript> {
            self.log.lock().unwrap().push("BATCH".into());
            panic!("a stream-first engine must not be driven down the batch path");
        }
    }

    /// Claims to stream, but refuses when asked. This is the fallback case.
    struct BrokenStream;

    impl SttEngine for BrokenStream {
        fn id(&self) -> &'static str {
            "broken-stream"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::stream_first()
        }
        fn stream(&self, _ctx: &SttContext) -> Result<Box<dyn SttStream>> {
            Err(crate::Error::Unsupported {
                engine: "broken-stream",
                what: "streaming",
            })
        }
        fn transcribe(&self, _audio: &AudioSegment, _ctx: &SttContext) -> Result<Transcript> {
            Ok(Transcript {
                text: "fell back to batch".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 0,
            })
        }
    }

    fn one_second() -> AudioSegment {
        AudioSegment::new(vec![0.0; crate::audio::SAMPLE_RATE as usize], 0)
    }

    #[test]
    fn a_stream_first_engine_is_driven_down_the_stream_path() {
        let engine = FakeStream::default();
        let log = std::sync::Arc::clone(&engine.log);
        let out =
            transcribe_segment(&engine, &one_second(), &SttContext::default()).expect("transcribe");

        let steps = log.lock().unwrap().clone();
        assert_eq!(steps.first().map(String::as_str), Some("stream opened"));
        assert_eq!(steps.last().map(String::as_str), Some("finish"));
        assert!(
            !steps.iter().any(|s| s == "BATCH"),
            "the batch path must not be touched"
        );
        assert_eq!(final_text(&out), "the whole thing");
    }

    #[test]
    fn a_second_of_audio_arrives_in_chunks_rather_than_one_push() {
        // Live audio comes a poll at a time. A fixture fed in one lump would
        // not exercise the engine the way the pipeline will.
        let engine = FakeStream::default();
        let log = std::sync::Arc::clone(&engine.log);
        transcribe_segment(&engine, &one_second(), &SttContext::default()).expect("transcribe");

        let pushes: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("push "))
            .cloned()
            .collect();
        assert_eq!(pushes.len(), 10, "one second at 100 ms a push");
        assert!(pushes.iter().all(|p| p == &format!("push {STREAM_CHUNK}")));
    }

    #[test]
    fn partials_are_kept_for_the_overlay_but_never_reach_the_paste() {
        let engine = FakeStream::default();
        let out =
            transcribe_segment(&engine, &one_second(), &SttContext::default()).expect("transcribe");
        assert!(
            out.iter().filter(|t| !t.is_final).count() >= 10,
            "the overlay needs the partials"
        );
        assert_eq!(
            final_text(&out),
            "the whole thing",
            "only committed segments are pasted"
        );
    }

    #[test]
    fn a_batch_engine_is_driven_down_the_batch_path() {
        let out = transcribe_segment(&FakeBatch, &one_second(), &SttContext::default())
            .expect("transcribe");
        assert_eq!(out.len(), 1);
        assert!(out[0].is_final);
        assert_eq!(final_text(&out), "16000 samples");
    }

    #[test]
    fn an_engine_that_cannot_open_a_stream_falls_back_to_batch() {
        // Stream-first, batch as fallback. Refusing the stream is not a reason
        // to lose the dictation.
        let out = transcribe_segment(&BrokenStream, &one_second(), &SttContext::default())
            .expect("transcribe");
        assert_eq!(final_text(&out), "fell back to batch");
    }

    #[test]
    fn a_trailing_partial_chunk_is_still_pushed() {
        // 1.05 s is ten whole chunks and a half one. The half is a word.
        let engine = FakeStream::default();
        let log = std::sync::Arc::clone(&engine.log);
        let audio = AudioSegment::new(vec![0.0; STREAM_CHUNK * 10 + 800], 0);
        transcribe_segment(&engine, &audio, &SttContext::default()).expect("transcribe");

        let pushes: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.starts_with("push "))
            .cloned()
            .collect();
        assert_eq!(pushes.len(), 11);
        assert_eq!(pushes[10], "push 800");
    }

    #[test]
    fn empty_audio_still_closes_the_stream_cleanly() {
        let engine = FakeStream::default();
        let log = std::sync::Arc::clone(&engine.log);
        let out = transcribe_segment(
            &engine,
            &AudioSegment::new(vec![], 0),
            &SttContext::default(),
        )
        .expect("transcribe");
        assert!(log.lock().unwrap().iter().any(|s| s == "finish"));
        assert_eq!(final_text(&out), "the whole thing");
    }

    #[test]
    fn final_text_joins_committed_segments_with_single_spaces() {
        let ts = vec![
            Transcript {
                text: "hello".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 1,
            },
            Transcript {
                text: "wor".into(),
                is_final: false,
                start_ms: 0,
                end_ms: 1,
            },
            Transcript {
                text: "world".into(),
                is_final: true,
                start_ms: 1,
                end_ms: 2,
            },
        ];
        assert_eq!(final_text(&ts), "hello world");
        assert_eq!(final_text(&[]), "");
    }

    /// A batch-only engine: uppercases, and refuses to stream.
    struct FakeBatch;

    impl SttEngine for FakeBatch {
        fn id(&self) -> &'static str {
            "fake-batch"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::batch()
        }
        fn stream(&self, _ctx: &SttContext) -> Result<Box<dyn SttStream>> {
            Err(crate::Error::Unsupported {
                engine: "fake-batch",
                what: "streaming",
            })
        }
        fn transcribe(&self, audio: &AudioSegment, _ctx: &SttContext) -> Result<Transcript> {
            Ok(Transcript {
                text: format!("{} samples", audio.samples.len()),
                is_final: true,
                start_ms: audio.start_ms,
                end_ms: audio.start_ms + audio.duration_ms(),
            })
        }
    }

    #[test]
    fn the_caps_decide_the_path_not_the_engine_name() {
        assert!(
            !FakeBatch.caps().owns_segmentation,
            "batch engines need VAD to segment"
        );
        assert!(
            EngineCaps::stream_first().owns_segmentation,
            "stream-first engines segment themselves"
        );
    }

    #[test]
    fn the_helpers_claim_no_concurrent_streams_until_an_engine_measures_it() {
        // Claimed wrongly, a live meeting would put its second stream on an
        // instance that makes it wait for the whole of the first.
        assert!(!EngineCaps::stream_first().concurrent_streams);
        assert!(!EngineCaps::batch().concurrent_streams);
    }

    #[test]
    fn a_batch_engine_refuses_to_stream_rather_than_faking_it() {
        match FakeBatch.stream(&SttContext::default()) {
            Err(crate::Error::Unsupported {
                what: "streaming", ..
            }) => {}
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("a batch engine must not return a stream"),
        }
    }

    #[test]
    fn batch_transcription_carries_the_segment_timing_through() {
        let seg = AudioSegment::new(vec![0.0; 16_000], 2_000);
        let t = FakeBatch
            .transcribe(&seg, &SttContext::default())
            .expect("transcribe");
        assert_eq!(t.start_ms, 2_000);
        assert_eq!(t.end_ms, 3_000);
        assert!(t.is_final);
    }
}
