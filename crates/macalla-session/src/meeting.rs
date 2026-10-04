// SPDX-License-Identifier: MIT
//! A meeting from a file: a recording heard on this machine, or a transcript
//! file someone received, as turns for the Files page.
//!
//! A meeting and dictation share one loaded engine. The session lends
//! [`MeetingTools`] ([`crate::Session::meeting_tools`]), and the meeting runs
//! on a thread of its own with them. Measured with two streams at once
//! (docs/benchmarks.md): Apple's engine gives each stream its own analyzer,
//! and a second Parakeet load doubled memory for no speed. A borrowed engine
//! stays loaded until the meeting ends, even when the session switches engine
//! meanwhile, so two models are briefly in memory then.
//!
//! - **Progress is audio time**, not pieces: an engine that segments for
//!   itself has no pieces to count.
//! - **Stop** is asked before each push to a stream, or before each piece for
//!   a batch engine. A stopped meeting keeps nothing.

use crate::summary::PART_BUDGET_CHARS;
use crate::{cleanup, decode_file, summary, Error, Result};
use macalla_core::audio::{AudioSegment, SAMPLE_RATE};
use macalla_core::meeting_parts::{self, Labels};
use macalla_core::pipeline::{Pipeline, SegmentState};
use macalla_core::stt::{self, SttContext, SttEngine, Transcript, STREAM_CHUNK};
use macalla_core::vad::{SpeechDetector, Vad, VadConfig};
use macalla_core::vtt::Turn;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What a meeting borrows from the open session: the on-device engine, the
/// pipeline fitted to it with the dictionary, the speech detector, and the
/// cleanup provider. Cloning is cheap, and a clone keeps the engine loaded.
#[derive(Clone)]
pub struct MeetingTools {
    pub(crate) engine: Arc<dyn SttEngine>,
    pub(crate) pipeline: Arc<Pipeline>,
    pub(crate) context: SttContext,
    pub(crate) detector: fn() -> Box<dyn SpeechDetector>,
    /// `None` with cleanup off, when there is nothing to tidy with.
    pub(crate) cleanup: Option<cleanup::Cleanup>,
    /// Opens another instance of `engine`, for a live meeting's second stream
    /// on an engine whose streams cannot share one
    /// ([`macalla_core::stt::EngineCaps::concurrent_streams`]). Built by
    /// [`crate::Session::meeting_tools`] from the registry with the session's
    /// engine id and language; a test puts in its own.
    pub(crate) open_another: OpenEngine,
}

/// Which engine instance a [`MeetingTools`] lent, from [`MeetingTools::pin`].
/// Holds no strong reference, so the model can be freed while it is kept,
/// and the instance is never mistaken for another made at the same address.
#[derive(Clone)]
pub struct EnginePin {
    engine: std::sync::Weak<dyn SttEngine>,
    id: &'static str,
    language: Option<String>,
}

impl EnginePin {
    /// Whether `tools` lends the pinned instance.
    pub fn lends(&self, tools: &MeetingTools) -> bool {
        std::sync::Weak::ptr_eq(&self.engine, &Arc::downgrade(&tools.engine))
    }

    /// Whether `tools` lends another instance of the same engine, in the same
    /// language: what reopening a session without changing its engine gives,
    /// as saving a provider's key or choosing the same engine again does.
    pub fn same_kind(&self, tools: &MeetingTools) -> bool {
        self.id == tools.engine.id() && self.language == tools.context.language
    }

    /// Whether the pinned instance has been let go of everywhere: the session
    /// that lent it was closed, as it is while the engine is being changed.
    pub fn is_gone(&self) -> bool {
        self.engine.strong_count() == 0
    }
}

/// Opens a new instance of an engine. Called only when one is needed: loading
/// an on-device model costs its full memory again.
pub(crate) type OpenEngine =
    Arc<dyn Fn() -> macalla_core::Result<Box<dyn SttEngine>> + Send + Sync>;

impl MeetingTools {
    /// A note of which engine instance this lends, that does not keep it
    /// loaded: a batch of files takes one as it starts, lets the engine go
    /// between files, and checks each later loan against it
    /// ([`EnginePin::lends`]).
    pub fn pin(&self) -> EnginePin {
        EnginePin {
            engine: Arc::downgrade(&self.engine),
            id: self.engine.id(),
            language: self.context.language.clone(),
        }
    }

    /// The engine the meeting is heard by, for the page to name.
    pub fn engine_id(&self) -> &'static str {
        self.engine.id()
    }

    /// The language the session hears in, as a BCP-47 code: what a meeting
    /// heard on this engine was spoken in, and so what a translation of it
    /// translates from. `None` when the session was opened without one.
    pub fn language(&self) -> Option<&str> {
        self.context.language.as_deref()
    }

    /// Where tidying sends a meeting's text, in the words cleanup's label
    /// uses, for the page to say before anything is sent. `None` with cleanup
    /// off.
    pub fn tidy_destination(&self) -> Option<&str> {
        self.cleanup.as_ref().map(cleanup::Cleanup::destination)
    }

    /// The cleanup choice tidying would use, as an opaque fingerprint
    /// ([`crate::choice::fingerprint`]), for a page to pass back so that a
    /// changed choice is refused even when its label reads the same. `None`
    /// with cleanup off.
    pub fn tidy_choice(&self) -> Option<&str> {
        self.cleanup.as_ref().map(cleanup::Cleanup::fingerprint)
    }

    /// Why a meeting cannot be tidied with the cleanup provider, as a sentence
    /// for the page, or `None` when it can. With cleanup off this is `None` too,
    /// and [`MeetingTools::tidy_destination`] says there is nothing to tidy with.
    pub fn tidy_unavailable(&self) -> Option<&'static str> {
        self.cleanup
            .as_ref()
            .and_then(cleanup::Cleanup::tidy_unavailable)
    }

    /// What tidying needs and nothing more. The engine is let go here, so a
    /// tidy waiting on a slow request holds no model. `None` with cleanup off.
    pub fn into_tidy(self) -> Option<TidyTools> {
        Some(TidyTools {
            pipeline: self.pipeline,
            cleanup: self.cleanup?,
        })
    }
}

/// What tidying a meeting borrows: the cleanup provider, and the pipeline for
/// its dictionary. No engine, so a quit has no model to wait for.
pub struct TidyTools {
    pipeline: Arc<Pipeline>,
    cleanup: cleanup::Cleanup,
}

impl TidyTools {
    /// Where tidying sends the text, in the words cleanup's label uses.
    pub fn destination(&self) -> &str {
        self.cleanup.destination()
    }

    /// The cleanup choice, as [`MeetingTools::tidy_choice`] gives it.
    pub fn fingerprint(&self) -> &str {
        self.cleanup.fingerprint()
    }

    /// How many requests tidying `meeting` sends: one per part, cut at the
    /// summary's part size but without speaker names, so it can be fewer than
    /// the summary's parts.
    pub fn requests(&self, meeting: &MeetingTranscript) -> usize {
        meeting_parts::split(&meeting.turns, PART_BUDGET_CHARS, Labels::Bare).len()
    }
}

/// How far through the recording the meeting has been heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeetingProgress {
    pub done_ms: u64,
    pub of_ms: u64,
}

/// A meeting, as turns in meeting order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingTranscript {
    pub turns: Vec<Turn>,
    /// Whether any turn has a speaker. A recording heard here has none.
    pub named_speakers: bool,
    /// How long the recording was. `None` for a transcript file, which has
    /// no audio.
    pub audio_ms: Option<u64>,
}

/// How many parts of a meeting the cleanup provider has answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TidyProgress {
    pub done: usize,
    pub of: usize,
}

/// A meeting after tidying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TidiedMeeting {
    /// The same turns, speakers and times, with the words as the provider
    /// left them.
    pub meeting: MeetingTranscript,
    /// Requests sent: one per part.
    pub parts: usize,
    /// Parts whose words came back as they were heard: the provider was not
    /// available, failed, changed nothing, or sent back a reply that could
    /// not be matched to its lines.
    pub unchanged: usize,
}

/// `meeting` tidied by the cleanup provider, part by part in meeting order,
/// with parts cut at the summary's part size, without speaker names. A turn
/// no reply covered keeps its text exactly. Only asked for when the person ticks
/// it for this file; [`MeetingTools::tidy_destination`] says where the text
/// goes first. `stop` is asked before each part, and a stopped tidy keeps
/// nothing: the meeting as heard is still the caller's.
pub fn tidy(
    tools: &TidyTools,
    meeting: &MeetingTranscript,
    on_progress: &mut dyn FnMut(TidyProgress),
    stop: &AtomicBool,
) -> Result<TidiedMeeting> {
    tidy_in(tools, meeting, PART_BUDGET_CHARS, on_progress, stop)
}

fn tidy_in(
    tools: &TidyTools,
    meeting: &MeetingTranscript,
    budget: usize,
    on_progress: &mut dyn FnMut(TidyProgress),
    stop: &AtomicBool,
) -> Result<TidiedMeeting> {
    let tidied = cleanup::tidy_turns(
        &tools.cleanup,
        tools.pipeline.dictionary(),
        &meeting.turns,
        budget,
        &mut |done, of| on_progress(TidyProgress { done, of }),
        stop,
    )
    .ok_or(Error::Stopped)?;
    Ok(TidiedMeeting {
        meeting: MeetingTranscript {
            turns: tidied.turns,
            ..meeting.clone()
        },
        parts: tidied.parts,
        unchanged: tidied.unchanged,
    })
}

/// What a file brought to the Files page holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeetingFile {
    /// A transcript someone received, already read: nothing to hear, and no
    /// engine needed.
    Transcript(MeetingTranscript),
    /// Audio, to hear with [`hear_file`] and the session's [`MeetingTools`].
    Recording,
}

/// What the file at `path` holds. Needs no session, so a received transcript
/// opens even when no on-device engine could.
///
/// The content decides first: a file that starts with WebVTT's header, after
/// an optional byte order mark, is a transcript whatever it is named. Then the
/// name: a `.vtt` without the header is refused rather than read as plain text,
/// which would lose its speakers and times without saying so; an `.srt` is not
/// read yet; a `.txt` is plain text. Anything else is a recording.
pub fn open_file(path: &Path) -> Result<MeetingFile> {
    if starts_with_webvtt(path)? {
        return read_transcript(path);
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("vtt") => Err(Error::NotWebVtt {
            path: path.to_path_buf(),
        }),
        Some("srt") => Err(Error::SrtNotSupported {
            path: path.to_path_buf(),
        }),
        Some("txt") => read_transcript(path),
        _ => Ok(MeetingFile::Recording),
    }
}

/// Whether the file starts with WebVTT's header, after an optional byte order
/// mark: `WEBVTT` followed by the end of the file, a space, a tab or a line
/// break, the same test the parser applies, so a file this lets through is
/// never read as plain text. Reads only those first bytes: a recording can be
/// an hour long.
fn starts_with_webvtt(path: &Path) -> Result<bool> {
    use std::io::Read;
    const BYTE_ORDER_MARK: &[u8] = &[0xEF, 0xBB, 0xBF];
    const HEADER: &[u8] = b"WEBVTT";
    let unreadable = |source| Error::MeetingFileRead {
        path: path.to_path_buf(),
        source,
    };
    let mut head = Vec::with_capacity(BYTE_ORDER_MARK.len() + HEADER.len() + 1);
    std::fs::File::open(path)
        .map_err(unreadable)?
        .take((BYTE_ORDER_MARK.len() + HEADER.len() + 1) as u64)
        .read_to_end(&mut head)
        .map_err(unreadable)?;
    let text = head.strip_prefix(BYTE_ORDER_MARK).unwrap_or(&head);
    Ok(match text.strip_prefix(HEADER) {
        Some(after) => matches!(after.first(), None | Some(b' ' | b'\t' | b'\n' | b'\r')),
        None => false,
    })
}

fn read_transcript(path: &Path) -> Result<MeetingFile> {
    let file = summary::read_transcript(path)?;
    Ok(MeetingFile::Transcript(MeetingTranscript {
        turns: file.turns,
        named_speakers: file.named_speakers,
        audio_ms: None,
    }))
}

/// The meeting in the recording at `path`, decoded and heard. [`open_file`]
/// says first whether the file is a recording.
pub fn hear_file(
    tools: &MeetingTools,
    path: &Path,
    on_progress: &mut dyn FnMut(MeetingProgress),
    stop: &AtomicBool,
) -> Result<MeetingTranscript> {
    if stop.load(Ordering::Relaxed) {
        return Err(Error::Stopped);
    }
    let samples = decode_recording(path)?;
    // A decode can take a while, and a stop asked for meanwhile hears nothing.
    if stop.load(Ordering::Relaxed) {
        return Err(Error::Stopped);
    }
    transcribe_samples(tools, samples, on_progress, stop)
}

/// The recording at `path` as 16 kHz mono samples, for [`transcribe_samples`].
/// Needs no engine, so a caller can decode before it borrows one.
pub fn decode_recording(path: &Path) -> Result<Vec<f32>> {
    decode_file(path)
}

/// The meeting in `samples`, 16 kHz mono. The last report is always the
/// whole recording.
pub fn transcribe_samples(
    tools: &MeetingTools,
    samples: Vec<f32>,
    on_progress: &mut dyn FnMut(MeetingProgress),
    stop: &AtomicBool,
) -> Result<MeetingTranscript> {
    let audio = AudioSegment::new(samples, 0);
    let of_ms = audio.duration_ms();
    let transcripts = if tools.engine.caps().owns_segmentation {
        heard_whole(tools, &audio, on_progress, stop)?
    } else {
        heard_in_pieces(tools, &audio, on_progress, stop)?
    };
    on_progress(MeetingProgress {
        done_ms: of_ms,
        of_ms,
    });
    // A stop asked for by now still means stopped. Silence cuts no piece to
    // ask it before, so a silent recording would otherwise run to its end and
    // be reported as nothing heard.
    if stop.load(Ordering::Relaxed) {
        return Err(Error::Stopped);
    }
    let turns = turns(&tools.pipeline, &transcripts, None);
    // Heard to the end and nothing in it: said, not shown as an empty meeting.
    if turns.is_empty() {
        return Err(Error::NothingHeard);
    }
    Ok(MeetingTranscript {
        turns,
        named_speakers: false,
        audio_ms: Some(of_ms),
    })
}

/// An engine that segments for itself hears the recording as one stream,
/// asked to stop before each push. Progress is the audio pushed, at most once
/// a second of it: an hour would otherwise be 36,000 reports.
fn heard_whole(
    tools: &MeetingTools,
    audio: &AudioSegment,
    on_progress: &mut dyn FnMut(MeetingProgress),
    stop: &AtomicBool,
) -> Result<Vec<Transcript>> {
    if stop.load(Ordering::Relaxed) {
        return Err(Error::Stopped);
    }
    let engine = tools.engine.as_ref();
    let opened = engine
        .caps()
        .streaming
        .then(|| engine.stream(&tools.context));
    let mut stream = match opened {
        Some(Ok(stream)) => stream,
        // No stream: the whole recording in one call, as a file always was.
        None | Some(Err(macalla_core::Error::Unsupported { .. })) => {
            return Ok(vec![engine.transcribe(audio, &tools.context)?]);
        }
        Some(Err(other)) => return Err(other.into()),
    };
    let of_ms = audio.duration_ms();
    let mut heard = Vec::new();
    let (mut pushed, mut reported_s) = (0usize, 0u64);
    for chunk in audio.samples.chunks(STREAM_CHUNK) {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Stopped);
        }
        stream.push(chunk)?;
        // Only what the engine committed is kept as it arrives: each preview
        // is superseded by the next, and an hour of them would be held until
        // the end for nothing.
        heard.extend(stream.poll()?.into_iter().filter(|t| t.is_final));
        pushed += chunk.len();
        let done_ms = pushed as u64 * 1_000 / SAMPLE_RATE as u64;
        if done_ms / 1_000 > reported_s {
            reported_s = done_ms / 1_000;
            on_progress(MeetingProgress { done_ms, of_ms });
        }
    }
    heard.extend(stream.finish()?.into_iter().filter(|t| t.is_final));
    Ok(heard)
}

/// A batch engine hears the recording piece by piece, cut at the pauses as a
/// file always was, asked to stop before each piece. Progress is the end of
/// the last piece heard.
fn heard_in_pieces(
    tools: &MeetingTools,
    audio: &AudioSegment,
    on_progress: &mut dyn FnMut(MeetingProgress),
    stop: &AtomicBool,
) -> Result<Vec<Transcript>> {
    if stop.load(Ordering::Relaxed) {
        return Err(Error::Stopped);
    }
    let of_ms = audio.duration_ms();
    let mut vad = Vad::new((tools.detector)(), VadConfig::default());
    let mut heard = Vec::new();
    for piece in vad.segments(audio) {
        if stop.load(Ordering::Relaxed) {
            return Err(Error::Stopped);
        }
        let done_ms = (piece.start_ms + piece.duration_ms()).min(of_ms);
        heard.extend(stt::transcribe_segment(
            tools.engine.as_ref(),
            &piece,
            &tools.context,
        )?);
        on_progress(MeetingProgress { done_ms, of_ms });
    }
    Ok(heard)
}

/// One turn per committed transcript, its text corrected by the dictionary
/// and Tier 0 as a dictation's segment is, each turn given `speaker`. A
/// recording heard from a file names no speaker; a live meeting names each of
/// its streams ([`crate::live_meeting`]), one call per stream. A piece left
/// with no words, such as one Tier 0 emptied of fillers, is no turn.
pub(crate) fn turns(
    pipeline: &Pipeline,
    transcripts: &[Transcript],
    speaker: Option<&str>,
) -> Vec<Turn> {
    let mut state = SegmentState::default();
    transcripts
        .iter()
        .filter_map(|transcript| {
            let segment = pipeline.segment(transcript, &mut state)?;
            let text = pipeline.finish(vec![segment]).to_plain();
            if text.trim().is_empty() {
                return None;
            }
            Some(Turn {
                speaker: speaker.map(str::to_string),
                start_ms: transcript.start_ms,
                end_ms: transcript.end_ms,
                text,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choice::CleanupReach;
    use crate::cleanup::Cleanup;
    use crate::{Config, Session};
    use macalla_core::audio::{AudioSegment, SAMPLE_RATE};
    use macalla_core::block::Block;
    use macalla_core::dictionary::Dictionary;
    use macalla_core::enhance::{Enhancer, EnhancerCaps};
    use macalla_core::stt::{EngineCaps, SttStream, Transcript, STREAM_CHUNK};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A batch engine that says which piece it was given.
    struct Pieces {
        calls: Arc<AtomicUsize>,
    }

    impl SttEngine for Pieces {
        fn id(&self) -> &'static str {
            "pieces"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::batch()
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Err(macalla_core::Error::Unsupported {
                engine: "pieces",
                what: "a stream",
            })
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Transcript {
                text: format!("piece number {n}"),
                is_final: true,
                start_ms: 0,
                end_ms: audio.duration_ms(),
            })
        }
    }

    /// An engine that segments for itself, as Apple's does: a preview while
    /// it hears, and the whole file's text when finished.
    struct Whole {
        pushes: Arc<AtomicUsize>,
    }

    struct WholeStream {
        pushes: Arc<AtomicUsize>,
        previewed: bool,
    }

    impl SttStream for WholeStream {
        fn push(&mut self, _samples: &[f32]) -> macalla_core::Result<()> {
            self.pushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            if std::mem::replace(&mut self.previewed, true) {
                return Ok(Vec::new());
            }
            Ok(vec![Transcript {
                text: "a preview never kept".into(),
                is_final: false,
                start_ms: 0,
                end_ms: 100,
            }])
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            Ok(vec![Transcript {
                text: "the whole meeting".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 9_000,
            }])
        }
    }

    impl SttEngine for Whole {
        fn id(&self) -> &'static str {
            "whole"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps {
                streaming: true,
                owns_segmentation: true,
                ..EngineCaps::batch()
            }
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(WholeStream {
                pushes: Arc::clone(&self.pushes),
                previewed: false,
            }))
        }
        fn transcribe(
            &self,
            _audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            unreachable!("a meeting streams to an engine that segments for itself")
        }
    }

    fn energy() -> Box<dyn SpeechDetector> {
        Box::new(macalla_core::vad::energy::EnergyDetector::new())
    }

    /// Tools lent by a session on `engine`, hearing through the energy
    /// detector: the recordings here are tones, which a speech model rightly
    /// does not hear.
    fn tools(engine: impl SttEngine + 'static) -> MeetingTools {
        session(engine).meeting_tools().unwrap()
    }

    fn session(engine: impl SttEngine + 'static) -> Session {
        let mut session = Session::with_engine(
            Box::new(engine),
            Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );
        session.detector = energy;
        session
    }

    /// A second of quiet, then `n` times a second of tone and three of quiet:
    /// three seconds is past the gate's hangover, so each tone is a piece.
    fn bursts(n: usize) -> Vec<f32> {
        let rate = SAMPLE_RATE as usize;
        let mut samples = vec![0.0; rate];
        for _ in 0..n {
            samples.extend(
                (0..rate)
                    .map(|i| 0.3 * (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin()),
            );
            samples.extend(std::iter::repeat_n(0.0, 3 * rate));
        }
        samples
    }

    fn quiet() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn a_recording_with_no_speech_says_nothing_was_heard() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tools(Pieces {
            calls: Arc::clone(&calls),
        });
        let mut seen = Vec::new();

        // Five seconds of silence: the gate cuts no piece, so no turn.
        let result = transcribe_samples(
            &tools,
            vec![0.0; 5 * SAMPLE_RATE as usize],
            &mut |p| seen.push(p),
            &quiet(),
        );

        assert!(matches!(result, Err(Error::NothingHeard)), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // It was still heard to the end before saying so.
        assert_eq!(seen.last().map(|p| p.done_ms), Some(5_000));
    }

    #[test]
    fn a_recording_of_only_fillers_says_nothing_was_heard() {
        // Tier 0 removes both fillers, which leaves a piece with no words.
        let tools = session_with(Saying("um uh"), Dictionary::from_json("{}").unwrap())
            .meeting_tools()
            .unwrap();

        let result = transcribe_samples(&tools, bursts(1), &mut |_| {}, &quiet());

        assert!(matches!(result, Err(Error::NothingHeard)), "{result:?}");
    }

    #[test]
    fn an_engine_that_commits_nothing_says_nothing_was_heard() {
        /// Segments for itself, and only ever previews.
        struct Previews;
        struct PreviewStream;

        impl SttStream for PreviewStream {
            fn push(&mut self, _samples: &[f32]) -> macalla_core::Result<()> {
                Ok(())
            }
            fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
                Ok(vec![Transcript {
                    text: "maybe this".into(),
                    is_final: false,
                    start_ms: 0,
                    end_ms: 100,
                }])
            }
            fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
                Ok(vec![Transcript {
                    text: "or maybe that".into(),
                    is_final: false,
                    start_ms: 0,
                    end_ms: 5_000,
                }])
            }
        }

        impl SttEngine for Previews {
            fn id(&self) -> &'static str {
                "previews"
            }
            fn caps(&self) -> EngineCaps {
                EngineCaps {
                    streaming: true,
                    owns_segmentation: true,
                    ..EngineCaps::batch()
                }
            }
            fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
                Ok(Box::new(PreviewStream))
            }
            fn transcribe(
                &self,
                _audio: &AudioSegment,
                _ctx: &SttContext,
            ) -> macalla_core::Result<Transcript> {
                unreachable!("a meeting streams to an engine that segments for itself")
            }
        }

        let mut seen = Vec::new();
        let result =
            transcribe_samples(&tools(Previews), bursts(1), &mut |p| seen.push(p), &quiet());

        assert!(matches!(result, Err(Error::NothingHeard)), "{result:?}");
        assert_eq!(seen.last().map(|p| p.done_ms), Some(5_000));
    }

    #[test]
    fn a_stop_on_a_silent_recording_says_stopped_not_nothing_heard() {
        let tools = tools(Pieces {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let stop = quiet();

        // Silence cuts no piece, so the first chance to see the stop is the
        // final report.
        let result = transcribe_samples(
            &tools,
            vec![0.0; 5 * SAMPLE_RATE as usize],
            &mut |p| {
                if p.done_ms == p.of_ms {
                    stop.store(true, Ordering::SeqCst);
                }
            },
            &stop,
        );

        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
    }

    /// A batch engine that hears `self.0` in every piece.
    struct Saying(&'static str);

    impl SttEngine for Saying {
        fn id(&self) -> &'static str {
            "saying"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::batch()
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Err(macalla_core::Error::Unsupported {
                engine: "saying",
                what: "a stream",
            })
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            Ok(Transcript {
                text: self.0.into(),
                is_final: true,
                start_ms: audio.start_ms,
                end_ms: audio.start_ms + audio.duration_ms(),
            })
        }
    }

    /// An engine that segments for itself and commits a line part way
    /// through, with a preview on every other poll.
    struct Talking;

    struct TalkingStream {
        polls: usize,
    }

    impl SttStream for TalkingStream {
        fn push(&mut self, _samples: &[f32]) -> macalla_core::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            self.polls += 1;
            Ok(vec![Transcript {
                text: if self.polls == 30 {
                    "the first half is settled".into()
                } else {
                    format!("preview {}", self.polls)
                },
                is_final: self.polls == 30,
                start_ms: 0,
                end_ms: self.polls as u64 * 100,
            }])
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            Ok(vec![
                Transcript {
                    text: "a last preview".into(),
                    is_final: false,
                    start_ms: 3_000,
                    end_ms: 9_000,
                },
                Transcript {
                    text: "and so is the second".into(),
                    is_final: true,
                    start_ms: 3_000,
                    end_ms: 9_000,
                },
            ])
        }
    }

    impl SttEngine for Talking {
        fn id(&self) -> &'static str {
            "talking"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps {
                streaming: true,
                owns_segmentation: true,
                ..EngineCaps::batch()
            }
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(TalkingStream { polls: 0 }))
        }
        fn transcribe(
            &self,
            _audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            unreachable!("a meeting streams to an engine that segments for itself")
        }
    }

    /// A dictionary of one term: what the engine hears as "cube control" is
    /// written "kubectl".
    fn kubectl() -> Dictionary {
        Dictionary::from_json(
            r#"{"terms":[{"written":"kubectl","spoken":["cube control"],"case_sensitive":true,"threshold":0.85}]}"#,
        )
        .unwrap()
    }

    fn session_with(engine: impl SttEngine + 'static, dictionary: Dictionary) -> Session {
        let mut session =
            Session::with_engine(Box::new(engine), dictionary, "test", Config::default());
        session.detector = energy;
        session
    }

    #[test]
    fn a_turn_is_its_piece_corrected_by_the_dictionary_and_tier_0() {
        let tools = session_with(Saying("um cube control shows the pods period"), kubectl())
            .meeting_tools()
            .unwrap();

        let meeting = transcribe_samples(&tools, bursts(1), &mut |_| {}, &quiet()).unwrap();

        assert_eq!(meeting.turns.len(), 1);
        // The dictionary writes "cube control" as "kubectl". Tier 0 removes the
        // filler, turns the spoken "period" into a full stop (it adds no
        // punctuation that was not said) and capitalises the first word, and
        // the written form's own case is put back after it. Without Tier 0,
        // "um" and "period" would still be there.
        assert_eq!(meeting.turns[0].text, "kubectl shows the pods.");
    }

    #[test]
    fn borrowed_tools_keep_the_dictionary_they_were_lent() {
        let mut session = session_with(Saying("cube control shows the pods"), kubectl());
        let lent_before = session.meeting_tools().unwrap();
        // An edit to the dictionary while a meeting is heard: the meeting keeps
        // the dictionary it started with, and the next meeting gets the new one.
        session.replace_dictionary(Dictionary::from_json("{}").unwrap(), "none");
        let lent_after = session.meeting_tools().unwrap();

        let before = transcribe_samples(&lent_before, bursts(1), &mut |_| {}, &quiet()).unwrap();
        let after = transcribe_samples(&lent_after, bursts(1), &mut |_| {}, &quiet()).unwrap();

        assert!(
            before.turns[0].text.contains("kubectl"),
            "{:?}",
            before.turns
        );
        assert!(
            after.turns[0].text.to_lowercase().contains("cube control"),
            "{:?}",
            after.turns
        );
    }

    #[test]
    fn finals_that_arrive_while_hearing_are_kept_in_order_and_previews_are_not() {
        let tools = tools(Talking);

        let meeting = transcribe_samples(&tools, bursts(1), &mut |_| {}, &quiet()).unwrap();

        let texts: Vec<&str> = meeting
            .turns
            .iter()
            .map(|turn| turn.text.as_str())
            .collect();
        assert_eq!(texts, ["The first half is settled", "And so is the second"]);
    }

    #[test]
    fn a_stream_keeps_only_what_the_engine_committed() {
        // Previews never became turns, so only this shows the filter: without
        // it, every preview polled would be held until the end.
        let heard = heard_whole(
            &tools(Talking),
            &AudioSegment::new(bursts(1), 0),
            &mut |_| {},
            &quiet(),
        )
        .unwrap();

        let texts: Vec<&str> = heard.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["the first half is settled", "and so is the second"]);
        assert!(heard.iter().all(|t| t.is_final));
    }

    /// A cleanup provider that capitalises every word it is sent. It fails
    /// the requests numbered in `fail_on`, and with `extra_line` replies with
    /// one more line than it was sent.
    struct Shouting {
        calls: Arc<AtomicUsize>,
        fail_on: Vec<usize>,
        extra_line: bool,
        /// Replies with exactly what it was sent, as a provider with nothing
        /// to change does.
        echo: bool,
    }

    impl Enhancer for Shouting {
        fn caps(&self) -> EnhancerCaps {
            EnhancerCaps {
                id: "shouting",
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            true
        }
        fn enhance(&self, blocks: &[Block], _terms: &[String]) -> macalla_core::Result<Vec<Block>> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_on.contains(&n) {
                return Err(macalla_core::Error::Enhance {
                    backend: "shouting",
                    message: "not today".into(),
                });
            }
            if self.echo {
                return Ok(blocks.to_vec());
            }
            let mut reply: Vec<Block> = blocks
                .iter()
                .map(|block| Block::Paragraph {
                    text: block.text().to_uppercase(),
                })
                .collect();
            if self.extra_line {
                reply.push(Block::Paragraph {
                    text: "AND MORE".into(),
                });
            }
            Ok(reply)
        }
    }

    fn shouting(calls: &Arc<AtomicUsize>) -> Shouting {
        Shouting {
            calls: Arc::clone(calls),
            fail_on: Vec::new(),
            extra_line: false,
            echo: false,
        }
    }

    /// Tools whose cleanup is `provider`, sending the text to `destination`.
    fn tidying(
        provider: impl Enhancer + 'static,
        reach: CleanupReach,
        destination: &str,
    ) -> TidyTools {
        let mut session = session(Pieces {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        session.cleanup = Some(Cleanup::new(Arc::new(provider), reach, destination));
        session.meeting_tools().unwrap().into_tidy().unwrap()
    }

    /// A provider that sends every block as a request of its own, as the real
    /// providers do through [`macalla_core::enhance::rewrite_each`], and ends
    /// every line of a reply with "!".
    struct PerBlock {
        requests: Arc<AtomicUsize>,
    }

    impl Enhancer for PerBlock {
        fn caps(&self) -> EnhancerCaps {
            EnhancerCaps {
                id: "per-block",
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            true
        }
        fn enhance(&self, blocks: &[Block], _terms: &[String]) -> macalla_core::Result<Vec<Block>> {
            macalla_core::enhance::rewrite_each(
                blocks,
                macalla_core::enhance::Guard::Ratio,
                |text| {
                    self.requests.fetch_add(1, Ordering::SeqCst);
                    Ok(text
                        .lines()
                        .map(|line| format!("{line}!"))
                        .collect::<Vec<_>>()
                        .join("\n"))
                },
            )
        }
    }

    /// A provider that answers every part with the same text, as one block.
    struct Replying(&'static str);

    impl Enhancer for Replying {
        fn caps(&self) -> EnhancerCaps {
            EnhancerCaps {
                id: "replying",
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            true
        }
        fn enhance(
            &self,
            _blocks: &[Block],
            _terms: &[String],
        ) -> macalla_core::Result<Vec<Block>> {
            Ok(vec![Block::Paragraph {
                text: self.0.to_string(),
            }])
        }
    }

    #[test]
    fn words_moved_across_a_line_break_stay_with_the_speaker_who_said_them() {
        let meeting = MeetingTranscript {
            turns: vec![
                spoken(Some("Ann"), 0, "we ship on friday"),
                spoken(Some("Bob"), 2_000, "then we test it"),
            ],
            named_speakers: true,
            audio_ms: None,
        };

        // Every word is there and the line count matches, but "Friday" moved
        // from Ann's line to Bob's.
        let shifted = tidying(
            Replying("We ship on\nFriday. Then we test it."),
            CleanupReach::Remote,
            "Your server",
        );
        let tidied = tidy_in(&shifted, &meeting, 100, &mut |_| {}, &quiet()).unwrap();
        assert_eq!(tidied.meeting.turns, meeting.turns);
        assert_eq!((tidied.parts, tidied.unchanged), (1, 1));

        // The same words kept on their own lines are taken.
        let kept = tidying(
            Replying("We ship on Friday.\nThen we test it."),
            CleanupReach::Remote,
            "Your server",
        );
        let tidied = tidy_in(&kept, &meeting, 100, &mut |_| {}, &quiet()).unwrap();
        let texts: Vec<&str> = tidied
            .meeting
            .turns
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        assert_eq!(texts, ["We ship on Friday.", "Then we test it."]);
        assert_eq!(tidied.unchanged, 0);
    }

    #[test]
    fn a_dictionary_with_terms_keeps_a_tidied_part_line_by_line() {
        // Settling rejoins a block's words with spaces, so a part settled
        // before its reply is split would come back as one line, fail the
        // line count, and be kept as heard.
        let requests = Arc::new(AtomicUsize::new(0));
        let mut session = session_with(
            Pieces {
                calls: Arc::new(AtomicUsize::new(0)),
            },
            kubectl(),
        );
        session.cleanup = Some(Cleanup::new(
            Arc::new(PerBlock {
                requests: Arc::clone(&requests),
            }),
            CleanupReach::Remote,
            "Your server",
        ));
        let tools = session.meeting_tools().unwrap().into_tidy().unwrap();
        let meeting = MeetingTranscript {
            turns: vec![
                spoken(None, 0, "run cube control"),
                spoken(None, 2_000, "then check the pods"),
            ],
            named_speakers: false,
            audio_ms: None,
        };

        let tidied = tidy_in(&tools, &meeting, 100, &mut |_| {}, &quiet()).unwrap();

        assert_eq!((tidied.parts, tidied.unchanged), (1, 0));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        let texts: Vec<&str> = tidied
            .meeting
            .turns
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        assert!(texts[0].contains("kubectl"), "{texts:?}");
        assert!(texts[0].ends_with('!'), "{texts:?}");
        assert_eq!(texts[1], "then check the pods!");
    }

    #[test]
    fn tidying_sends_one_request_per_part_not_one_per_line() {
        let requests = Arc::new(AtomicUsize::new(0));
        let tools = tidying(
            PerBlock {
                requests: Arc::clone(&requests),
            },
            CleanupReach::Remote,
            "Your server",
        );
        let meeting = MeetingTranscript {
            turns: (0..6)
                .map(|at| spoken(None, at * 2_000, &format!("turn number {at}")))
                .collect(),
            named_speakers: false,
            audio_ms: None,
        };

        // Each turn is 13 characters, so a budget of 30 puts two in a part.
        let tidied = tidy_in(&tools, &meeting, 30, &mut |_| {}, &quiet()).unwrap();

        assert_eq!(tidied.parts, 3);
        assert_eq!(requests.load(Ordering::SeqCst), tidied.parts);
        assert_eq!(tidied.unchanged, 0);
        let texts: Vec<&str> = tidied
            .meeting
            .turns
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        assert_eq!(
            texts,
            [
                "turn number 0!",
                "turn number 1!",
                "turn number 2!",
                "turn number 3!",
                "turn number 4!",
                "turn number 5!",
            ]
        );
    }

    fn spoken(speaker: Option<&str>, at_ms: u64, text: &str) -> Turn {
        Turn {
            speaker: speaker.map(str::to_string),
            start_ms: at_ms,
            end_ms: at_ms + 1_000,
            text: text.into(),
        }
    }

    /// Three turns of eight to eleven characters: at a budget of 12, each is
    /// a part of its own.
    fn three_turns() -> MeetingTranscript {
        MeetingTranscript {
            turns: vec![
                spoken(Some("Ann"), 0, "one two."),
                spoken(Some("Bob"), 2_000, "three four."),
                spoken(None, 4_000, "five six."),
            ],
            named_speakers: true,
            audio_ms: None,
        }
    }

    #[test]
    fn a_meeting_is_tidied_one_part_at_a_time_in_meeting_order() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tidying(shouting(&calls), CleanupReach::Remote, "Your server");
        let meeting = three_turns();
        let mut seen = Vec::new();

        let tidied = tidy_in(&tools, &meeting, 12, &mut |p| seen.push(p), &quiet()).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            seen,
            [
                TidyProgress { done: 0, of: 3 },
                TidyProgress { done: 1, of: 3 },
                TidyProgress { done: 2, of: 3 },
                TidyProgress { done: 3, of: 3 },
            ],
            "said once as tidying starts, then as each part is answered"
        );
        assert_eq!((tidied.parts, tidied.unchanged), (3, 0));
        let texts: Vec<&str> = tidied
            .meeting
            .turns
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        assert_eq!(texts, ["ONE TWO.", "THREE FOUR.", "FIVE SIX."]);
        // Only the words change.
        for (before, after) in meeting.turns.iter().zip(&tidied.meeting.turns) {
            assert_eq!(before.speaker, after.speaker);
            assert_eq!(
                (before.start_ms, before.end_ms),
                (after.start_ms, after.end_ms)
            );
        }
        assert_eq!(tidied.meeting.named_speakers, meeting.named_speakers);
        assert_eq!(tidied.meeting.audio_ms, meeting.audio_ms);
    }

    #[test]
    fn a_turn_cut_across_parts_comes_back_as_one_turn() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tidying(shouting(&calls), CleanupReach::Local, "Apple Intelligence");
        let meeting = MeetingTranscript {
            turns: vec![spoken(
                Some("Ann"),
                0,
                "First sentence here. Second sentence here.",
            )],
            named_speakers: true,
            audio_ms: None,
        };

        let tidied = tidy_in(&tools, &meeting, 25, &mut |_| {}, &quiet()).unwrap();

        assert_eq!(tidied.parts, 2);
        assert_eq!(tidied.meeting.turns.len(), 1);
        assert_eq!(
            tidied.meeting.turns[0].text,
            "FIRST SENTENCE HERE. SECOND SENTENCE HERE."
        );
    }

    #[test]
    fn a_part_the_provider_fails_keeps_its_words_as_heard() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Shouting {
            fail_on: vec![2],
            ..shouting(&calls)
        };
        let tools = tidying(provider, CleanupReach::Remote, "Your server");

        let tidied = tidy_in(&tools, &three_turns(), 12, &mut |_| {}, &quiet()).unwrap();

        let texts: Vec<&str> = tidied
            .meeting
            .turns
            .iter()
            .map(|t| t.text.as_str())
            .collect();
        assert_eq!(texts, ["ONE TWO.", "three four.", "FIVE SIX."]);
        assert_eq!((tidied.parts, tidied.unchanged), (3, 1));
    }

    #[test]
    fn a_reply_that_cannot_be_matched_to_its_lines_keeps_the_part_as_heard() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Shouting {
            extra_line: true,
            ..shouting(&calls)
        };
        let tools = tidying(provider, CleanupReach::Remote, "Your server");
        let meeting = three_turns();

        let tidied = tidy_in(&tools, &meeting, 12, &mut |_| {}, &quiet()).unwrap();

        assert_eq!(tidied.meeting.turns, meeting.turns);
        assert_eq!((tidied.parts, tidied.unchanged), (3, 3));
    }

    #[test]
    fn a_meeting_with_no_parts_reports_nothing_and_asks_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tidying(shouting(&calls), CleanupReach::Remote, "Your server");
        let empty = MeetingTranscript {
            turns: Vec::new(),
            named_speakers: false,
            audio_ms: None,
        };
        let mut seen = Vec::new();

        let tidied = tidy_in(&tools, &empty, 12, &mut |p| seen.push(p), &quiet()).unwrap();

        assert!(seen.is_empty(), "no (0, 0) report: {seen:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!((tidied.parts, tidied.unchanged), (0, 0));
    }

    #[test]
    fn a_stop_sends_no_further_part() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tidying(shouting(&calls), CleanupReach::Remote, "Your server");
        let stop = quiet();

        // Stopped as the first part is answered: the report as tidying starts
        // (done 0) comes before anything is sent, so it is passed over.
        let result = tidy_in(
            &tools,
            &three_turns(),
            12,
            &mut |p| {
                if p.done > 0 {
                    stop.store(true, Ordering::SeqCst);
                }
            },
            &stop,
        );

        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_stop_while_the_last_part_is_out_still_means_stopped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tidying(shouting(&calls), CleanupReach::Remote, "Your server");
        let stop = quiet();

        let result = tidy_in(
            &tools,
            &three_turns(),
            12,
            &mut |p| {
                if p.done == p.of - 1 {
                    stop.store(true, Ordering::SeqCst);
                }
            },
            &stop,
        );

        // The stop came after part 2 was answered; part 3 is not sent.
        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // And set during the last part itself, it still reads as stopped.
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tidying(shouting(&calls), CleanupReach::Remote, "Your server");
        let stop = quiet();
        let result = tidy_in(
            &tools,
            &three_turns(),
            12,
            &mut |p| {
                if p.done == p.of {
                    stop.store(true, Ordering::SeqCst);
                }
            },
            &stop,
        );
        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_provider_with_nothing_to_change_leaves_every_part_unchanged() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Shouting {
            echo: true,
            ..shouting(&calls)
        };
        let tools = tidying(provider, CleanupReach::Local, "Apple Intelligence");
        let meeting = three_turns();

        let tidied = tidy_in(&tools, &meeting, 12, &mut |_| {}, &quiet()).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!((tidied.parts, tidied.unchanged), (3, 3));
        assert_eq!(tidied.meeting.turns, meeting.turns);
    }

    #[test]
    fn a_turn_no_reply_covered_keeps_its_text_exactly() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Shouting {
            fail_on: vec![1, 2],
            ..shouting(&calls)
        };
        let tools = tidying(provider, CleanupReach::Remote, "Your server");
        // At a budget of 8 the sentence is longer than a line, so it is cut
        // at its spaces into "three" and "four.", which rejoin as
        // "three four.": the second space would be lost if the turn were
        // rebuilt from its lines.
        let meeting = MeetingTranscript {
            turns: vec![spoken(Some("Ann"), 0, "three  four.")],
            named_speakers: true,
            audio_ms: None,
        };

        let tidied = tidy_in(&tools, &meeting, 8, &mut |_| {}, &quiet()).unwrap();

        assert_eq!((tidied.parts, tidied.unchanged), (2, 2));
        assert_eq!(tidied.meeting.turns[0].text, "three  four.");
    }

    #[test]
    fn with_cleanup_off_there_is_nothing_to_tidy_with() {
        let tools = tools(Pieces {
            calls: Arc::new(AtomicUsize::new(0)),
        });

        assert_eq!(tools.tidy_destination(), None);
        assert_eq!(tools.tidy_choice(), None);
        assert!(tools.into_tidy().is_none());
    }

    #[test]
    fn tidying_says_where_the_text_goes_and_how_many_requests_it_sends() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tidying(shouting(&calls), CleanupReach::Remote, "Your server");

        assert_eq!(tools.destination(), "Your server");
        // At the summary's part size, three short turns are one request.
        assert_eq!(tools.requests(&three_turns()), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "saying so sends nothing");
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macalla-session-meeting-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 16 kHz mono 16-bit PCM, the plainest WAV there is.
    fn write_wav(path: &Path, samples: &[f32]) {
        let data_len = (samples.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_recording_is_heard_piece_by_piece_with_progress_in_audio_time() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tools(Pieces {
            calls: Arc::clone(&calls),
        });
        let samples = bursts(3);
        let of_ms = samples.len() as u64 * 1_000 / SAMPLE_RATE as u64;
        let mut seen = Vec::new();

        let meeting = transcribe_samples(&tools, samples, &mut |p| seen.push(p), &quiet()).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(meeting.turns.len(), 3);
        assert!(meeting.turns[0]
            .text
            .to_lowercase()
            .contains("piece number 1"));
        assert!(meeting.turns.iter().all(|t| t.speaker.is_none()));
        assert!(!meeting.named_speakers);
        assert_eq!(meeting.audio_ms, Some(of_ms));
        assert!(seen.len() >= 3, "{seen:?}");
        assert!(seen.iter().all(|p| p.of_ms == of_ms && p.done_ms <= of_ms));
        assert!(
            seen.windows(2).all(|w| w[0].done_ms <= w[1].done_ms),
            "{seen:?}"
        );
        assert_eq!(seen.last().unwrap().done_ms, of_ms);
    }

    #[test]
    fn an_engine_that_segments_for_itself_hears_the_whole_file_as_one_stream() {
        let pushes = Arc::new(AtomicUsize::new(0));
        let tools = tools(Whole {
            pushes: Arc::clone(&pushes),
        });
        let samples = bursts(2);
        let chunks = samples.len().div_ceil(STREAM_CHUNK);
        let seconds = samples.len() / SAMPLE_RATE as usize;
        let of_ms = samples.len() as u64 * 1_000 / SAMPLE_RATE as u64;
        let mut seen = Vec::new();

        let meeting = transcribe_samples(&tools, samples, &mut |p| seen.push(p), &quiet()).unwrap();

        assert_eq!(pushes.load(Ordering::SeqCst), chunks);
        // The preview is never a turn.
        assert_eq!(meeting.turns.len(), 1, "{:?}", meeting.turns);
        assert!(meeting.turns[0]
            .text
            .to_lowercase()
            .contains("the whole meeting"));
        // About once a second of audio, not once a chunk.
        assert!(seen.len() <= seconds + 1, "{} reports", seen.len());
        assert!(
            seen.windows(2).all(|w| w[0].done_ms <= w[1].done_ms),
            "{seen:?}"
        );
        assert_eq!(seen.last().unwrap().done_ms, of_ms);
    }

    #[test]
    fn a_stop_before_it_starts_hears_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tools(Pieces {
            calls: Arc::clone(&calls),
        });

        let result = transcribe_samples(&tools, bursts(2), &mut |_| {}, &AtomicBool::new(true));

        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_stop_during_a_batch_recording_ends_it_before_the_next_piece() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tools(Pieces {
            calls: Arc::clone(&calls),
        });
        let stop = quiet();

        let result = transcribe_samples(
            &tools,
            bursts(3),
            &mut |_| stop.store(true, Ordering::SeqCst),
            &stop,
        );

        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_stop_during_a_stream_ends_it_before_the_next_push() {
        let pushes = Arc::new(AtomicUsize::new(0));
        let tools = tools(Whole {
            pushes: Arc::clone(&pushes),
        });
        let stop = quiet();

        let result = transcribe_samples(
            &tools,
            bursts(2),
            &mut |_| stop.store(true, Ordering::SeqCst),
            &stop,
        );

        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
        // The first report comes after a second of audio: ten chunks.
        assert_eq!(
            pushes.load(Ordering::SeqCst),
            SAMPLE_RATE as usize / STREAM_CHUNK
        );
    }

    const VTT: &str =
        "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\n<v Aoife Byrne>We ship on Friday.\n";

    /// `contents` written as `file` in a fresh directory for test `name`.
    fn written(name: &str, file: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = temp_dir(name).join(file);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn transcript(opened: Result<MeetingFile>) -> MeetingTranscript {
        match opened {
            Ok(MeetingFile::Transcript(meeting)) => meeting,
            other => panic!("expected a transcript, got {other:?}"),
        }
    }

    #[test]
    fn a_transcript_file_is_read_with_no_session_and_nothing_heard() {
        // No tools and no engine: the desktop has no session at all when no
        // on-device engine opened, and a received transcript must still open.
        let meeting = transcript(open_file(&written("vtt", "meeting.vtt", VTT.as_bytes())));

        assert_eq!(meeting.audio_ms, None);
        assert!(meeting.named_speakers);
        assert_eq!(meeting.turns[0].speaker.as_deref(), Some("Aoife Byrne"));
        assert_eq!(meeting.turns[0].text, "We ship on Friday.");
    }

    #[test]
    fn a_webvtt_file_is_a_transcript_whatever_its_name() {
        let path = written("vtt-named-srt", "meeting.srt", VTT.as_bytes());

        assert!(transcript(open_file(&path)).named_speakers);
    }

    #[test]
    fn a_byte_order_mark_before_the_header_is_allowed() {
        let mut contents = vec![0xEF, 0xBB, 0xBF];
        contents.extend_from_slice(VTT.as_bytes());

        let meeting = transcript(open_file(&written("bom", "meeting.vtt", &contents)));

        assert!(meeting.named_speakers, "{meeting:?}");
    }

    #[test]
    fn a_vtt_file_without_the_header_is_refused_not_read_as_plain_text() {
        let path = written(
            "headerless",
            "meeting.vtt",
            b"00:00:01.000 --> 00:00:03.000\nWe ship on Friday.\n",
        );

        let result = open_file(&path);

        assert!(matches!(result, Err(Error::NotWebVtt { .. })), "{result:?}");
    }

    #[test]
    fn a_header_followed_by_anything_but_a_space_or_line_break_is_not_webvtt() {
        let path = written(
            "webvttx",
            "meeting.vtt",
            b"WEBVTTX\n\n00:00:01.000 --> 00:00:03.000\nWe ship on Friday.\n",
        );

        let result = open_file(&path);

        assert!(matches!(result, Err(Error::NotWebVtt { .. })), "{result:?}");
    }

    #[test]
    fn a_header_alone_with_nothing_after_it_is_webvtt() {
        // Only the header, which the parser accepts as an empty transcript
        // and summary reading then reports as having no words.
        let result = open_file(&written("header-only", "meeting.vtt", b"WEBVTT"));

        assert!(
            matches!(result, Err(Error::EmptyTranscript { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn an_upper_case_extension_is_the_same_extension() {
        let path = written("upper", "MEETING.VTT", b"We ship on Friday.\n");

        let result = open_file(&path);

        assert!(matches!(result, Err(Error::NotWebVtt { .. })), "{result:?}");
    }

    #[test]
    fn a_directory_is_not_a_file_to_open() {
        let result = open_file(&temp_dir("a-directory"));

        assert!(
            matches!(result, Err(Error::MeetingFileRead { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_file_shorter_than_the_header_is_still_read() {
        let meeting = transcript(open_file(&written("short", "short.txt", b"Hi.")));

        assert_eq!(meeting.turns.len(), 1);
        assert_eq!(meeting.turns[0].text, "Hi.");
    }

    #[test]
    fn an_srt_file_is_not_supported_yet() {
        let path = written(
            "srt",
            "meeting.srt",
            b"1\n00:00:01,000 --> 00:00:03,000\nWe ship on Friday.\n",
        );

        let result = open_file(&path);

        assert!(
            matches!(result, Err(Error::SrtNotSupported { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_plain_text_file_is_a_transcript_with_no_speakers() {
        let meeting = transcript(open_file(&written(
            "txt",
            "notes.txt",
            b"We ship on Friday.",
        )));

        assert!(!meeting.named_speakers);
        assert_eq!(meeting.turns.len(), 1);
    }

    #[test]
    fn a_file_that_cannot_be_opened_says_which() {
        let path = temp_dir("missing").join("nowhere.m4a");

        let result = open_file(&path);

        assert!(
            matches!(result, Err(Error::MeetingFileRead { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_recording_is_opened_then_decoded_and_heard() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tools = tools(Pieces {
            calls: Arc::clone(&calls),
        });
        let path = temp_dir("wav").join("meeting.wav");
        write_wav(&path, &bursts(2));

        assert_eq!(open_file(&path).unwrap(), MeetingFile::Recording);
        let meeting = hear_file(&tools, &path, &mut |_| {}, &quiet()).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(meeting.turns.len(), 2);
        assert_eq!(meeting.audio_ms, Some(9_000));
    }

    #[test]
    fn borrowed_tools_keep_their_engine_after_the_session_goes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = session(Pieces {
            calls: Arc::clone(&calls),
        });
        let tools = session.meeting_tools().unwrap();
        // A switch of engine drops the session's model; a meeting in progress
        // keeps the one it borrowed.
        drop(session);

        let meeting = transcribe_samples(&tools, bursts(1), &mut |_| {}, &quiet()).unwrap();

        assert_eq!(tools.engine_id(), "pieces");
        assert_eq!(meeting.turns.len(), 1);
    }

    #[test]
    fn a_session_with_no_engine_of_its_own_lends_no_tools() {
        let session = Session::from_opened(
            Err(macalla_core::Error::Unsupported {
                engine: "none",
                what: "this machine",
            }),
            Some((
                "cloud".into(),
                Box::new(Pieces {
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            )),
            Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        )
        .unwrap();

        assert!(matches!(
            session.meeting_tools(),
            Err(Error::NoPreviewEngine(_))
        ));
    }
}
