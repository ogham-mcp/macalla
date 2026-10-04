// SPDX-License-Identifier: MIT
//! A live meeting: two voices heard while they speak, as turns for the Files
//! page.
//!
//! The microphone is **You** and the call's audio is **Others**, each its own
//! stream, heard until a person stops the meeting. The result is the same
//! [`MeetingTranscript`] of the same turns a file meeting gives
//! ([`crate::meeting`]), finished by the same path, so tidying, summaries and
//! export cannot tell where the turns came from.
//!
//! - **Sources.** Anything that yields 16 kHz mono over time is a [`Source`]:
//!   the microphone's [`Capture`], the call's audio from [`SystemAudio`], and
//!   in a test a fixture served a chunk at a time. A meeting with no Others
//!   source is a microphone-only meeting: in a room, or with the call's audio
//!   unavailable ([`call_audio`]). That is a smaller meeting, not an error.
//! - **Engines.** Both streams use the engine borrowed from the session
//!   ([`MeetingTools`]) when it can serve two streams at once
//!   ([`EngineCaps::concurrent_streams`]). When it cannot, the Others stream
//!   gets one new instance of it, opened before either stream is. The engine's
//!   capability decides this, never its id.
//! - **Starting.** [`prepare`] does everything slow while no source plays:
//!   it decides which instance each stream is heard on and opens a second
//!   one if the engine needs it. The sources come to it opened and not
//!   started. [`Prepared::record`] then starts them, and the meeting's clock
//!   starts only once every source has: what a source delivered before that
//!   is not the meeting's, and is let go.
//! - **Ending.** [`Ending::stop`] ends the meeting and keeps it: both streams
//!   are finished and drained, and the transcript is returned.
//!   [`Ending::cancel`] keeps nothing, and the meeting is [`Error::Stopped`],
//!   as a stopped file meeting is. A failure on either stream ends both and
//!   keeps what they heard, with the failure beside it
//!   ([`LiveMeeting::ended_early`]).
//! - **Progress** is audio time, as a file meeting's is, with each source's
//!   signal health: whether it has delivered anything but silence, and how
//!   long ago. A system-audio tap macOS refused delivers correctly-clocked
//!   zeros and no error, so health is judged by signal, never by how many
//!   samples arrived.
//! - **No audio is kept.** Samples go to the engine a poll at a time and are
//!   let go; only the text is held until the meeting ends.

use crate::meeting::{self, MeetingTools, MeetingTranscript};
use crate::{Error, Result, POLL};
use macalla_core::audio::SAMPLE_RATE;
use macalla_core::pipeline::Pipeline;
use macalla_core::stt::{self, EngineCaps, SttEngine, SttStream, Transcript, STREAM_CHUNK};
use macalla_core::vtt::Turn;
use macalla_platform::audio::Capture;
use macalla_platform::system_audio::{CaptureError, OpenError, SystemAudio};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The speaker a microphone's turns carry when the call has its own stream.
/// A name, as a VTT file's speaker names are: data, not wording.
pub const YOU: &str = "You";

/// The speaker the call audio's turns carry.
pub const OTHERS: &str = "Others";

/// How often progress is reported while the meeting runs, by the wall clock:
/// a stalled source delivers no audio, and its health must still be heard.
const PROGRESS_TICK: Duration = Duration::from_secs(1);

/// Something that yields audio over time, as 16 kHz mono: the contract
/// [`macalla_core::audio`] holds every capture to.
///
/// **Timing contract.** A turn's time is its position in its own stream's
/// samples, and the two streams' turns are merged by those times, so every
/// source in one meeting must keep the same clock:
/// - Sample 0 of every source in a meeting is the same moment: the
///   meeting's start. [`Prepared::record`] makes it so, and a source need
///   not: it starts every source, then lets go of whatever each has
///   delivered by then, and the meeting starts at that moment. A source may
///   deliver audio from before its [`Source::start`] returned, or from
///   before another source started; none of it is heard.
/// - A source delivers its audio without gaps. When it loses audio (a
///   dropped buffer, a device that went away and came back), it fills the
///   gap with zeros of the same length, so its sample count keeps pace with
///   the other source's.
///
/// **Known and not corrected: the clocks drift.** Each source counts samples
/// by its own device's clock, and two devices' clocks disagree by tens of
/// parts per million, more over Bluetooth. Over an hour that is tenths of a
/// second between the voices, and nothing here measures or corrects it.
pub trait Source: Send {
    /// Start capturing. A source is opened, not started, until the meeting
    /// is ready to hear it: [`Prepared::record`] calls this once, for every
    /// source, before it polls any. It may take a while, and may deliver
    /// audio while it does; that audio is let go.
    fn start(&mut self) -> Result<()>;

    /// Append whatever has arrived since the last call to `out`, and say how
    /// many samples that was. Zero is not an end: a source has nothing yet.
    ///
    /// A source may append audio to `out` and then fail on the same call, as
    /// the microphone's [`Capture::poll_into`] does when its device faults:
    /// that audio is heard before the failure ends the meeting.
    fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize>;

    /// Stop, and append the tail to `out`.
    fn finish_into(self: Box<Self>, out: &mut Vec<f32>) -> Result<usize>;
}

impl Source for Capture {
    fn start(&mut self) -> Result<()> {
        Ok(Capture::play(self)?)
    }

    fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
        Ok(Capture::poll_into(self, out)?)
    }

    fn finish_into(self: Box<Self>, out: &mut Vec<f32>) -> Result<usize> {
        Ok(Capture::finish_into(*self, out)?)
    }
}

/// The call's audio. A tap macOS refused delivers zeros and no error, and
/// nothing here makes one of them: the zeros are heard like any audio, and
/// [`SignalHealth`] says there is no signal. The tap keeps running for the
/// whole meeting, so if a permission granted partway takes effect in the
/// running process, the call is heard from then on. Whether it does is
/// untested.
///
/// **Its first start asks the person for permission to record system
/// audio, and whether that start waits for the answer is not known.** Both
/// ways are correct, because [`Prepared::record`] starts the call's audio
/// before the microphone and lets go of everything delivered before the
/// meeting's clock starts:
/// - If the start waits for the answer, the microphone has not started yet,
///   so nothing the person says while they read the prompt is heard, and the
///   meeting's clock starts once they have answered.
/// - If it does not wait, the tap delivers zeros until the answer and the
///   meeting runs meanwhile; the call is heard from the grant on, if a grant
///   reaches the running process at all.
///
/// **Unobserved, and a manual check:** if the start does wait for the answer
/// and the person never gives one, the meeting waits in its start for as
/// long as the prompt stays up. A caller that holds anything for the
/// meeting's length holds it meanwhile: the desktop app's claim on the
/// microphone, so dictation stays refused until the prompt is answered. A
/// stop or a cancel pressed then is seen only once the start returns.
///
/// **Observed on 2026-09-23 on a Mac mini (Apple M1, macOS 27.0), in a
/// Macalla Dev-signed build with the hardened runtime on:** the system
/// audio prompt appeared, was granted, and audio played through the
/// speakers came through as Others. A process tap under the hardened
/// runtime needs no entitlement beyond `NSAudioCaptureUsageDescription` in
/// `Info.plist`.
impl Source for SystemAudio {
    fn start(&mut self) -> Result<()> {
        SystemAudio::play(self).map_err(Error::CallAudioStart)
    }

    fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
        Ok(SystemAudio::poll_into(self, out)?)
    }

    fn finish_into(self: Box<Self>, out: &mut Vec<f32>) -> Result<usize> {
        Ok(SystemAudio::finish_into(*self, out)?)
    }
}

/// A running capture's failure, as the meeting's: a format change is typed
/// for the app to say, and a resampler's failure is the core's.
impl From<CaptureError> for Error {
    fn from(error: CaptureError) -> Self {
        match error {
            CaptureError::FormatChanged => Error::CallAudioFormatChanged,
            CaptureError::Chain(error) => Error::Core(error),
        }
    }
}

/// How far behind its source a live meeting's stream may fall before audio
/// is lost, in seconds: the ring each source is opened with. The same as
/// dictation's microphone, and above the call audio's own floor
/// ([`macalla_platform::system_audio::MIN_RING_SECONDS`]).
pub const RING_SECONDS: f32 = 30.0;

/// The microphone, `device` or the default, opened for a live meeting and
/// not started: the You source for [`prepare`].
pub fn open_microphone(device: Option<&str>) -> Result<Box<dyn Source>> {
    Ok(Box::new(Capture::open(device, RING_SECONDS)?))
}

/// The call's audio, opened for a live meeting and not started: the Others
/// source for [`prepare`], or why there is none ([`call_audio`]).
pub fn open_call_audio() -> CallAudio {
    call_audio(SystemAudio::open(RING_SECONDS))
}

/// The Others source for [`prepare`], from an attempt to open the call's
/// audio.
pub struct CallAudio {
    /// `others` for [`prepare`]: `None` when the call's audio could not open,
    /// which makes the meeting microphone-only rather than failed.
    pub others: Option<Box<dyn Source>>,
    /// Why there is no call audio, for the app to put into words: the
    /// person should know the meeting will not hear the other side.
    pub unavailable: Option<OpenError>,
}

/// Turn an attempt to open the call's audio, such as
/// `SystemAudio::open(ring_seconds)`, into what [`prepare`] takes: any
/// failure to open, [`OpenError::Unsupported`] included, becomes no Others
/// source and the reason beside it. Only opening is judged here; a tap that
/// opened and hears nothing is a signal-health matter, not this one's.
pub fn call_audio<S: Source + 'static>(opened: std::result::Result<S, OpenError>) -> CallAudio {
    match opened {
        Ok(source) => CallAudio {
            others: Some(Box::new(source)),
            unavailable: None,
        },
        Err(reason) => {
            tracing::info!(%reason, "no call audio: the live meeting hears the microphone alone");
            CallAudio {
                others: None,
                unavailable: Some(reason),
            }
        }
    }
}

/// How a person ends a live meeting: two signals, because keeping it and
/// throwing it away are different requests. Shared with whatever asks, on
/// any thread. A cancel wins over a stop.
#[derive(Debug, Default)]
pub struct Ending {
    stop: AtomicBool,
    cancel: AtomicBool,
}

impl Ending {
    pub fn new() -> Self {
        Self::default()
    }

    /// End the meeting and keep it: both streams finish, and what they heard
    /// is the transcript.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// End the meeting and keep nothing.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// Whether a source is delivering anything but silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalHealth {
    /// Whether any sample that is not exactly zero has arrived since the
    /// meeting started.
    pub signal: bool,
    /// Milliseconds by the wall clock since the last poll that brought a
    /// sample that is not exactly zero. `None` until one has.
    pub since_signal_ms: Option<u64>,
}

/// How a live meeting is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveProgress {
    /// Audio heard since the meeting started: the longer of the two streams.
    pub heard_ms: u64,
    pub you: SignalHealth,
    /// `None` in a microphone-only meeting.
    pub others: Option<SignalHealth>,
}

/// A live meeting ready to record: its sources opened and not started, and
/// the engine instances its streams will be heard on, loaded. Made by
/// [`prepare`]; nothing plays until [`Prepared::record`].
pub struct Prepared<'t> {
    tools: &'t MeetingTools,
    you: Box<dyn Source>,
    others: Option<Box<dyn Source>>,
    /// The second instance, when the plan needs one.
    another: Option<Box<dyn SttEngine>>,
    plan: Instances,
    committed: Committed,
}

/// Which of a meeting's two streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    You,
    Others,
}

/// What a live meeting has committed so far, readable from any thread while
/// it records: the words a caller can still keep when the meeting cannot
/// finish in time, as a quitting app can. Filled as each stream commits, so
/// it holds everything the finished meeting would, less the piece still
/// being heard and any stream's finish. Only text; no audio.
#[derive(Clone)]
pub struct Committed {
    pipeline: Arc<Pipeline>,
    inner: Arc<Mutex<CommittedSoFar>>,
}

#[derive(Default)]
struct CommittedSoFar {
    you: Vec<Transcript>,
    others: Vec<Transcript>,
    named_speakers: bool,
    heard_ms: u64,
}

impl Committed {
    fn new(pipeline: Arc<Pipeline>, named_speakers: bool) -> Self {
        Self {
            pipeline,
            inner: Arc::new(Mutex::new(CommittedSoFar {
                named_speakers,
                ..CommittedSoFar::default()
            })),
        }
    }

    /// Keep what `side` has committed that is not kept yet, and how much it
    /// has heard.
    fn note(&self, side: Side, finals: &[Transcript], heard_ms: u64) {
        let mut so_far = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        so_far.heard_ms = so_far.heard_ms.max(heard_ms);
        let kept = match side {
            Side::You => &mut so_far.you,
            Side::Others => &mut so_far.others,
        };
        if finals.len() > kept.len() {
            kept.extend_from_slice(&finals[kept.len()..]);
        }
    }

    /// The meeting as committed so far, turned into turns exactly as the
    /// finished meeting's are; `None` while nothing is committed.
    pub fn meeting(&self) -> Option<MeetingTranscript> {
        let so_far = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let named = so_far.named_speakers;
        let turns = assemble(
            &self.pipeline,
            &so_far.you,
            named.then_some(&so_far.others[..]),
        );
        (!turns.is_empty()).then(|| MeetingTranscript {
            turns,
            named_speakers: named,
            audio_ms: Some(so_far.heard_ms),
        })
    }
}

/// The two streams' committed transcripts as one meeting's turns, in
/// meeting order. `others` is `None` in a microphone-only meeting, whose
/// turns name no speaker.
fn assemble(pipeline: &Pipeline, you: &[Transcript], others: Option<&[Transcript]>) -> Vec<Turn> {
    let mut turns = meeting::turns(pipeline, you, others.is_some().then_some(YOU));
    if let Some(others) = others {
        turns.extend(meeting::turns(pipeline, others, Some(OTHERS)));
    }
    // Stable, so at the same moment You comes before Others every time.
    turns.sort_by_key(|turn| (turn.start_ms, turn.end_ms));
    turns
}

/// Get a live meeting ready to hear `you` and, when there is one, `others`:
/// everything slow, done while no source plays. Which instance each stream
/// is heard on is decided here, from the engine's capability alone, and a
/// second instance is opened when the engine's streams cannot share one: a
/// cold load can take seconds, and a source that played through it would
/// fill its buffer with audio from before the meeting. The sources must
/// come opened and not started, as [`open_microphone`] and
/// [`open_call_audio`] give them.
///
/// Errors: [`Error::SecondEngine`] when the second instance cannot open.
/// Nothing has played and nothing was heard.
pub fn prepare<'t>(
    tools: &'t MeetingTools,
    you: Box<dyn Source>,
    others: Option<Box<dyn Source>>,
) -> Result<Prepared<'t>> {
    let plan = instances(&tools.engine.caps(), others.is_some());
    let another = if plan.needs_another() {
        Some((tools.open_another)().map_err(Error::SecondEngine)?)
    } else {
        None
    };
    let committed = Committed::new(Arc::clone(&tools.pipeline), plan.others.is_some());
    Ok(Prepared {
        tools,
        you,
        others,
        another,
        plan,
        committed,
    })
}

impl Prepared<'_> {
    /// What this meeting has committed so far, for another thread to read
    /// while [`Prepared::record`] runs. Taken before `record`, which
    /// consumes the meeting.
    pub fn committed(&self) -> Committed {
        self.committed.clone()
    }

    /// Hear the meeting until `ending` says to stop or cancel.
    ///
    /// Blocks for the meeting's length; a caller with a window runs it on a
    /// worker. `on_progress` is called on the calling thread about once a
    /// second while the meeting runs, and once more when both streams have
    /// finished.
    ///
    /// **Starting.** Both streams are opened on their engines first, then
    /// the sources are started, the call's audio before the microphone (see
    /// the note on [`SystemAudio`]'s [`Source`] impl), and only then does
    /// the meeting's clock start: whatever each source delivered up to that
    /// moment is let go, so both share one sample 0 and neither hears
    /// anything from before it, however long a start took. Sample 0 is
    /// shared to within the moments the two are read at, which are one
    /// after the other, and the few milliseconds a resampler holds back.
    ///
    /// Each stream is heard the way a file meeting hears a recording, by
    /// [`EngineCaps::owns_segmentation`]: an engine that endpoints for itself
    /// gets its own stream, and any other engine is given the audio a piece
    /// at a time, cut at the pauses.
    ///
    /// Turns carry [`YOU`] and [`OTHERS`], with times in milliseconds from
    /// the meeting's start, in meeting order, and `named_speakers` is true.
    /// A microphone-only meeting names no speaker and `named_speakers` is
    /// false, as a recording heard from a file: in a room the microphone
    /// hears everyone, so calling its turns "You" would put words in the
    /// person's mouth. `audio_ms` is the meeting's length in audio time.
    ///
    /// **A meeting that fails keeps what was heard.** A source's or an
    /// engine's error on either stream, or a panic on a stream's thread
    /// ([`Error::StreamPanicked`]), ends the meeting on both streams. Each
    /// stream is still finished, and everything it transcribed up to then
    /// is kept: the result is a [`LiveMeeting`] whose
    /// [`LiveMeeting::ended_early`] carries the error. Only a cancel keeps
    /// nothing, because that is the person's choice.
    ///
    /// Errors:
    /// - [`Error::NothingHeard`], with no source started, when the meeting
    ///   was stopped before this was called.
    /// - A source's own error when it cannot start, or fails as the
    ///   meeting's clock starts ([`Error::CallAudioStart`] for the call's
    ///   audio). Nothing was heard.
    /// - [`Error::Stopped`] when the meeting was cancelled, however it was
    ///   ending by then.
    /// - [`Error::NothingHeard`] when the meeting was stopped with no words
    ///   in it, as a file meeting says.
    /// - The failure itself, when a stream failed and no words at all were
    ///   heard: returning [`Error::NothingHeard`] instead would lose the
    ///   cause.
    ///
    /// A panic in `on_progress` ends both streams and is then carried on to
    /// the caller.
    pub fn record(
        self,
        on_progress: &mut dyn FnMut(LiveProgress),
        ending: &Ending,
    ) -> Result<LiveMeeting> {
        let Prepared {
            tools,
            mut you,
            others,
            another,
            plan,
            committed,
        } = self;
        if ending.cancelled() {
            return Err(Error::Stopped);
        }
        // Stopped before it started, as a Stop pressed while the meeting was
        // getting ready: nothing is started, so no permission prompt is
        // raised and no microphone lights, and the meeting ends as one with
        // no words in it does.
        if ending.stopped() {
            return Err(Error::NothingHeard);
        }
        let engine_for = |instance| engine_for(tools, another.as_deref(), instance);
        tracing::info!(
            engine = tools.engine.id(),
            others = others.is_some(),
            another_instance = another.is_some(),
            "live meeting starting"
        );

        let you_stream = open(engine_for(plan.you), tools)?;
        let mut others = match (others, plan.others) {
            (Some(source), Some(instance)) => Some((source, open(engine_for(instance), tools)?)),
            _ => None,
        };
        let started = start_together(you.as_mut(), others.as_mut().map(|(source, _)| source))?;
        tracing::info!("live meeting started");
        hear_both(
            tools,
            (you, you_stream),
            others,
            started,
            on_progress,
            ending,
            &committed,
        )
    }
}

/// Start every source, the call's audio first, and then take the meeting's
/// sample 0: whatever each source delivered up to now is read and let go,
/// and the moment after is the meeting's start. The one place pre-roll is
/// discarded. A source that cannot start, or fails as it is read here, ends
/// the meeting before anything is heard.
fn start_together(
    you: &mut dyn Source,
    mut others: Option<&mut Box<dyn Source>>,
) -> Result<Instant> {
    if let Some(others) = others.as_mut() {
        others.start()?;
    }
    you.start()?;
    let mut before = Vec::new();
    if let Some(others) = others.as_mut() {
        others.poll_into(&mut before)?;
    }
    let others_before = before.len();
    before.clear();
    you.poll_into(&mut before)?;
    tracing::debug!(
        you_ms = before.len() as u64 * 1_000 / SAMPLE_RATE as u64,
        others_ms = others_before as u64 * 1_000 / SAMPLE_RATE as u64,
        "let go of what the sources delivered before the meeting's start"
    );
    Ok(Instant::now())
}

/// One source and the stream it is heard on.
type SourceAndStream<'e> = (Box<dyn Source>, Box<dyn SttStream + 'e>);

/// Hear both streams from `started` until the meeting ends, and put what
/// they committed together as the meeting. See [`Prepared::record`].
fn hear_both(
    tools: &MeetingTools,
    (you, you_stream): SourceAndStream<'_>,
    others: Option<SourceAndStream<'_>>,
    started: Instant,
    on_progress: &mut dyn FnMut(LiveProgress),
    ending: &Ending,
    committed: &Committed,
) -> Result<LiveMeeting> {
    let named_speakers = others.is_some();
    let failed = AtomicBool::new(false);
    let (you_heard, others_heard) = (Heard::default(), Heard::default());
    let progress = || LiveProgress {
        heard_ms: you_heard.heard_ms().max(others_heard.heard_ms()),
        you: you_heard.health(started),
        others: named_speakers.then(|| others_heard.health(started)),
    };

    // A thread per stream, because a push can block: a piece is transcribed
    // inside the push that closes it, and on a shared instance it may wait
    // for the other stream's piece first. On one thread, that would starve
    // the other stream of its polls while its capture ring filled. Scoped, so each stream can
    // borrow the engine it was opened on, and so both have ended, one way or
    // another, before this returns.
    let (mut you_heard_out, mut others_heard_out) = std::thread::scope(|scope| {
        let you_thread = {
            let failed = &failed;
            let tally = Tally {
                heard: &you_heard,
                committed,
                side: Side::You,
            };
            scope.spawn(move || hear(you, you_stream, tally, ending, failed, started))
        };
        let others_thread = others.map(|(source, stream)| {
            let failed = &failed;
            let tally = Tally {
                heard: &others_heard,
                committed,
                side: Side::Others,
            };
            scope.spawn(move || hear(source, stream, tally, ending, failed, started))
        });
        let mut reported = Instant::now();
        // A panic in `on_progress` is held here, not let out of the scope:
        // the scope would wait for both streams, and they would wait for a
        // stop nobody is left to ask for. `failed` ends them; the panic goes
        // on once they have.
        let mut progress_panic = None;
        while !you_thread.is_finished() || others_thread.as_ref().is_some_and(|t| !t.is_finished())
        {
            std::thread::sleep(POLL);
            if progress_panic.is_none() && reported.elapsed() >= PROGRESS_TICK {
                reported = Instant::now();
                let report = std::panic::catch_unwind(AssertUnwindSafe(|| on_progress(progress())));
                if let Err(panic) = report {
                    failed.store(true, Ordering::SeqCst);
                    progress_panic = Some(panic);
                }
            }
        }
        let heard = (joined(you_thread), others_thread.map(joined));
        if let Some(panic) = progress_panic {
            std::panic::resume_unwind(panic);
        }
        heard
    });
    on_progress(progress());

    if ending.cancelled() {
        return Err(Error::Stopped);
    }
    let ended_early = first_failure(&mut you_heard_out, others_heard_out.as_mut());

    let turns = assemble(
        &tools.pipeline,
        &you_heard_out.finals,
        others_heard_out.as_ref().map(|others| &others.finals[..]),
    );
    if turns.is_empty() {
        return Err(ended_early.unwrap_or(Error::NothingHeard));
    }
    Ok(LiveMeeting {
        meeting: MeetingTranscript {
            turns,
            named_speakers,
            audio_ms: Some(progress().heard_ms),
        },
        ended_early,
    })
}

/// A live meeting as it ended.
#[derive(Debug)]
pub struct LiveMeeting {
    /// What was heard, up to the stop or up to the failure.
    pub meeting: MeetingTranscript,
    /// The first stream to fail's first error, typed, for the app to put
    /// into words; `None` when nothing failed. Usually that means the person
    /// stopped the meeting, but a stop does not rule a failure out: a
    /// source's tail or an engine's finish, both reached after the stop, can
    /// still fail, and that failure is here. Either way `meeting` holds
    /// everything heard.
    pub ended_early: Option<Error>,
}

/// The error that ended the meeting early, if one did: the stream that
/// failed first, and its first error. Taken out of the outcomes, which keep
/// their text.
fn first_failure(you: &mut Outcome, others: Option<&mut Outcome>) -> Option<Error> {
    let others = others.filter(|others| others.error.is_some());
    match others {
        Some(others) if others.first || you.error.is_none() => others.error.take(),
        _ => you.error.take(),
    }
}

/// Which engine instance a stream is heard on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Instance {
    /// The engine the session lent the meeting.
    Borrowed,
    /// A new instance of the same engine, opened for the meeting.
    Another,
}

/// The instance each stream is heard on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Instances {
    you: Instance,
    /// `None` in a microphone-only meeting.
    others: Option<Instance>,
}

impl Instances {
    fn needs_another(&self) -> bool {
        self.you == Instance::Another || self.others == Some(Instance::Another)
    }
}

/// The one place that decides which instance each stream is heard on, from
/// the engine's capability alone. An engine whose streams wait on each other
/// gives the Others stream a new instance; the microphone keeps the one the
/// session lent, which is already loaded.
fn instances(caps: &EngineCaps, others: bool) -> Instances {
    let for_others = if caps.concurrent_streams {
        Instance::Borrowed
    } else {
        Instance::Another
    };
    Instances {
        you: Instance::Borrowed,
        others: others.then_some(for_others),
    }
}

/// How a stream is heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    /// The engine's own stream, which endpoints and commits as it goes:
    /// Apple's. A file meeting's `heard_whole`.
    Endpointed,
    /// Pieces cut at the pauses by the speech detector, each transcribed as
    /// it closes ([`stt::BatchStream`]). A file meeting's `heard_in_pieces`.
    Pieces,
}

/// The one place that decides how a stream is heard, by the rule a file
/// meeting uses: [`EngineCaps::owns_segmentation`], not
/// [`EngineCaps::streaming`]. An engine that streams without endpointing
/// (Nemotron) commits nothing until its stream ends, so a meeting-long
/// stream would hold every word until the stop, as one turn from the
/// meeting's start, and lose them all to one error. Cut into pieces, each
/// piece holds the engine only while it is transcribed.
fn stream_kind(caps: &EngineCaps) -> StreamKind {
    if caps.owns_segmentation {
        StreamKind::Endpointed
    } else {
        StreamKind::Pieces
    }
}

/// A stream on `engine`, of the kind [`stream_kind`] picks.
fn open<'a>(engine: &'a dyn SttEngine, tools: &MeetingTools) -> Result<Box<dyn SttStream + 'a>> {
    Ok(match stream_kind(&engine.caps()) {
        // An engine that endpoints for itself but cannot stream (none that a
        // meeting borrows today) is cut into pieces here too: open_stream
        // falls back to them.
        StreamKind::Endpointed => stt::open_stream(engine, &tools.context, (tools.detector)())?,
        StreamKind::Pieces => Box::new(stt::BatchStream::new(
            engine,
            &tools.context,
            (tools.detector)(),
        )),
    })
}

/// The engine `instance` names: the borrowed one, or `another` when it was
/// opened.
fn engine_for<'a>(
    tools: &'a MeetingTools,
    another: Option<&'a dyn SttEngine>,
    instance: Instance,
) -> &'a dyn SttEngine {
    match (instance, another) {
        (Instance::Another, Some(another)) => another,
        _ => tools.engine.as_ref(),
    }
}

/// What one stream has heard, shared with the thread that reports progress.
struct Heard {
    samples: AtomicU64,
    /// Wall milliseconds from the meeting's start to the last poll that
    /// brought a sample that is not exactly zero, or [`Heard::NO_SIGNAL`].
    last_signal_ms: AtomicU64,
}

impl Default for Heard {
    fn default() -> Self {
        Self {
            samples: AtomicU64::new(0),
            last_signal_ms: AtomicU64::new(Self::NO_SIGNAL),
        }
    }
}

impl Heard {
    const NO_SIGNAL: u64 = u64::MAX;

    fn note(&self, samples: &[f32], started: Instant) {
        self.samples
            .fetch_add(samples.len() as u64, Ordering::Relaxed);
        if samples.iter().any(|&sample| sample != 0.0) {
            self.last_signal_ms
                .store(elapsed_ms(started), Ordering::Relaxed);
        }
    }

    fn heard_ms(&self) -> u64 {
        self.samples.load(Ordering::Relaxed) * 1_000 / SAMPLE_RATE as u64
    }

    fn health(&self, started: Instant) -> SignalHealth {
        match self.last_signal_ms.load(Ordering::Relaxed) {
            Self::NO_SIGNAL => SignalHealth {
                signal: false,
                since_signal_ms: None,
            },
            at => SignalHealth {
                signal: true,
                since_signal_ms: Some(elapsed_ms(started).saturating_sub(at)),
            },
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX - 1)
}

/// A stream thread's answer, or its panic carried on to the caller.
fn joined<T>(thread: std::thread::ScopedJoinHandle<'_, T>) -> T {
    thread
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// What one stream heard, and why it ended early if it did.
#[derive(Default)]
struct Outcome {
    /// Committed transcripts, in the order the engine gave them.
    finals: Vec<Transcript>,
    /// The stream's first error, if it failed. Later ones are only
    /// consequences of it.
    error: Option<Error>,
    /// Whether this stream was the first of the two to fail.
    first: bool,
}

impl Outcome {
    /// Keep `error` if it is this stream's first, and tell the other stream
    /// at once, so it stops too.
    fn fail(&mut self, error: Error, failed: &AtomicBool) {
        if self.error.is_none() {
            tracing::warn!(%error, "a live meeting's stream failed; keeping what was heard");
            self.first = failed
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
            self.error = Some(error);
        }
    }
}

/// One stream, heard until the meeting ends: everything it committed, and
/// its failure if it had one. A failure, a panic included, is flagged to
/// the other stream, which then ends as if stopped and keeps its words.
fn hear(
    source: Box<dyn Source>,
    stream: Box<dyn SttStream + '_>,
    tally: Tally<'_>,
    ending: &Ending,
    failed: &AtomicBool,
    started: Instant,
) -> Outcome {
    let mut outcome = Outcome::default();
    // A panic let out of this thread would reach the caller only at the
    // join, after the other stream had run on to the stop. Caught, it is an
    // error like any other: the other stream ends now, and what this one
    // committed before the panic is kept, because it lives out here.
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        hear_until_ended(source, stream, tally, ending, failed, started, &mut outcome);
    }));
    if let Err(panic) = caught {
        outcome.fail(Error::StreamPanicked(panic_message(panic.as_ref())), failed);
    }
    outcome
}

/// A panic's own message, for the log, when it carried one.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// Where one stream counts what it heard, and keeps what it committed
/// within another thread's reach ([`Committed`]).
#[derive(Clone, Copy)]
struct Tally<'a> {
    heard: &'a Heard,
    committed: &'a Committed,
    side: Side,
}

/// Hear until a stop, a cancel, a failure here, or the other stream's
/// failure, which ends this one as a stop does. Everything is written into
/// `out` as it happens, so a panic partway keeps what came before it.
fn hear_until_ended(
    mut source: Box<dyn Source>,
    mut stream: Box<dyn SttStream + '_>,
    Tally {
        heard,
        committed,
        side,
    }: Tally<'_>,
    ending: &Ending,
    failed: &AtomicBool,
    started: Instant,
    out: &mut Outcome,
) {
    let keep = |out: &Outcome| committed.note(side, &out.finals, heard.heard_ms());
    let mut samples = Vec::with_capacity(STREAM_CHUNK);
    loop {
        // A cancelled meeting keeps nothing, so there is nothing to finish.
        if ending.cancelled() {
            return;
        }
        if ending.stopped() || failed.load(Ordering::SeqCst) {
            break;
        }
        samples.clear();
        // A source may put audio in `samples` and fail on the same poll, as
        // the microphone does; that audio is heard before the failure counts.
        let polled = source.poll_into(&mut samples);
        let fed = feed(stream.as_mut(), &samples, heard, started, &mut out.finals);
        keep(out);
        if let Err(error) = polled.and(fed) {
            out.fail(error, failed);
            break;
        }
        // Less than a chunk means the source has caught up, so wait a poll
        // for more. A backlog, left by a push that took a while, is drained
        // without waiting.
        if samples.len() < STREAM_CHUNK {
            std::thread::sleep(POLL);
        }
    }
    if ending.cancelled() {
        return;
    }
    // Every source is asked for its tail, a failed one too: its resampler
    // still holds up to a chunk and its delay, which is a word. A failure
    // here counts only if nothing failed before it; `fail` keeps the first.
    samples.clear();
    let tail = source.finish_into(&mut samples);
    let fed = feed(stream.as_mut(), &samples, heard, started, &mut out.finals);
    if let Err(error) = tail.and(fed) {
        out.fail(error, failed);
    }
    // Finished once, whatever happened: it commits the piece in progress.
    match stream.finish() {
        Ok(transcripts) => out
            .finals
            .extend(transcripts.into_iter().filter(|t| t.is_final)),
        Err(error) => out.fail(error.into(), failed),
    }
    keep(out);
}

/// Push `samples` a chunk at a time and keep what the engine committed. Only
/// committed text is kept as it arrives: each preview is superseded by the
/// next, as in a file meeting. A push that fails stops the pushing, and the
/// engine is still polled for what it had already committed.
fn feed(
    stream: &mut (dyn SttStream + '_),
    samples: &[f32],
    heard: &Heard,
    started: Instant,
    finals: &mut Vec<Transcript>,
) -> Result<()> {
    heard.note(samples, started);
    let pushed = samples
        .chunks(STREAM_CHUNK)
        .try_for_each(|chunk| stream.push(chunk));
    let polled = stream
        .poll()
        .map(|transcripts| finals.extend(transcripts.into_iter().filter(|t| t.is_final)));
    Ok(pushed.and(polled)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choice::CleanupReach;
    use crate::cleanup::Cleanup;
    use crate::summary::PART_BUDGET_CHARS;
    use crate::{Config, Session};
    use macalla_core::audio::AudioSegment;
    use macalla_core::block::Block;
    use macalla_core::dictionary::Dictionary;
    use macalla_core::enhance::{Enhancer, EnhancerCaps};
    use macalla_core::meeting_parts::{self, Labels};
    use macalla_core::stt::SttContext;
    use macalla_core::vad::SpeechDetector;
    use macalla_core::vtt::Turn;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    /// A batch engine that says `what` for every piece, timed as the piece
    /// was, and counts the pieces it was given.
    struct Says {
        what: &'static str,
        concurrent: bool,
        pieces: Arc<AtomicUsize>,
        /// A piece that starts within this range of milliseconds is held
        /// until the gate opens: a push that transcribes a piece blocks, as a
        /// batch engine's does.
        gate: Option<(Gate, std::ops::Range<u64>)>,
        /// Pieces from this one on (counting from 1) fail, as an engine
        /// whose model broke partway would.
        fails_from: Option<usize>,
    }

    impl Says {
        fn new(what: &'static str, concurrent: bool) -> Self {
            Self {
                what,
                concurrent,
                pieces: Arc::new(AtomicUsize::new(0)),
                gate: None,
                fails_from: None,
            }
        }
    }

    impl SttEngine for Says {
        fn id(&self) -> &'static str {
            "says"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps {
                concurrent_streams: self.concurrent,
                ..EngineCaps::batch()
            }
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Err(macalla_core::Error::Unsupported {
                engine: "says",
                what: "a stream",
            })
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            let piece = self.pieces.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((gate, held)) = &self.gate {
                if held.contains(&audio.start_ms) {
                    gate.wait();
                }
            }
            if self.fails_from.is_some_and(|from| piece >= from) {
                return Err(macalla_core::Error::Stt {
                    engine: "says",
                    message: "the model broke".into(),
                });
            }
            Ok(Transcript {
                text: self.what.into(),
                is_final: true,
                start_ms: audio.start_ms,
                end_ms: audio.start_ms + audio.duration_ms(),
            })
        }
    }

    /// Opens once; a waiter gives up after five seconds and says it starved.
    #[derive(Clone, Default)]
    struct Gate {
        open: Arc<AtomicBool>,
        starved: Arc<AtomicBool>,
        /// Waits begun.
        entered: Arc<AtomicUsize>,
    }

    impl Gate {
        fn open(&self) {
            self.open.store(true, Ordering::SeqCst);
        }
        fn wait(&self) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.open.load(Ordering::SeqCst) {
                if Instant::now() > deadline {
                    self.starved.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// How long a test source serves before it fails the meeting: a test
    /// whose meeting never ends fails instead of hanging CI.
    const TEST_DEADLINE: Duration = Duration::from_secs(10);

    /// Serves `samples` a chunk a poll, as fast as it is asked, then nothing,
    /// and calls `drained` once when it runs dry. Counts its polls. Fails
    /// with a lost device once [`TEST_DEADLINE`] has passed since it was
    /// made.
    struct Scripted {
        samples: Vec<f32>,
        served: usize,
        polls: Arc<AtomicUsize>,
        drained: Option<Box<dyn FnOnce() + Send>>,
        /// Holds its last chunk back, serving nothing, until the count is
        /// above zero, or for five seconds, after which it serves it anyway
        /// and sets the flag: starved.
        last_waits_for: Option<(Arc<AtomicUsize>, Arc<AtomicBool>)>,
        held_since: Option<Instant>,
        /// Sleeps this long at the start of every poll, to pace the audio by
        /// the wall clock.
        pace: Option<Duration>,
        /// Called once, after this many polls have been served.
        at_poll: Option<(usize, Box<dyn FnOnce() + Send>)>,
        /// Its finish fails with a lost microphone.
        fails_at_finish: bool,
        made: Instant,
    }

    impl Scripted {
        fn new(samples: Vec<f32>) -> Self {
            Self {
                samples,
                served: 0,
                polls: Arc::new(AtomicUsize::new(0)),
                drained: None,
                last_waits_for: None,
                held_since: None,
                pace: None,
                at_poll: None,
                fails_at_finish: false,
                made: Instant::now(),
            }
        }
        fn then(mut self, drained: impl FnOnce() + Send + 'static) -> Self {
            self.drained = Some(Box::new(drained));
            self
        }
    }

    impl Source for Scripted {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            if self.made.elapsed() > TEST_DEADLINE {
                return Err(macalla_platform::Error::DeviceLost("test deadline".into()).into());
            }
            if let Some(pace) = self.pace {
                std::thread::sleep(pace);
            }
            let last = self.served + STREAM_CHUNK >= self.samples.len();
            if let (true, Some((count, starved))) = (last, &self.last_waits_for) {
                if count.load(Ordering::SeqCst) == 0 {
                    let since = *self.held_since.get_or_insert_with(Instant::now);
                    if since.elapsed() < Duration::from_secs(5) {
                        return Ok(0);
                    }
                    starved.store(true, Ordering::SeqCst);
                }
            }
            let end = (self.served + STREAM_CHUNK).min(self.samples.len());
            out.extend_from_slice(&self.samples[self.served..end]);
            let n = end - self.served;
            self.served = end;
            if self
                .at_poll
                .as_ref()
                .is_some_and(|(polls, _)| self.served >= polls * STREAM_CHUNK)
            {
                if let Some((_, then)) = self.at_poll.take() {
                    then();
                }
            }
            if self.served == self.samples.len() {
                if let Some(drained) = self.drained.take() {
                    drained();
                }
            }
            Ok(n)
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> Result<usize> {
            if self.fails_at_finish {
                return Err(macalla_platform::Error::DeviceLost("microphone".into()).into());
            }
            Ok(0)
        }
    }

    /// Ends the meeting, with `how`, once `sources` sources have run dry.
    fn when_all_drained(
        ending: &Arc<Ending>,
        sources: usize,
        how: fn(&Ending),
    ) -> impl Fn() -> Box<dyn FnOnce() + Send> {
        let left = Arc::new(AtomicUsize::new(sources));
        let ending = Arc::clone(ending);
        move || {
            let (left, ending) = (Arc::clone(&left), Arc::clone(&ending));
            Box::new(move || {
                if left.fetch_sub(1, Ordering::SeqCst) == 1 {
                    how(&ending);
                }
            })
        }
    }

    /// `seconds` of quiet with a second of tone starting at each of `at_s`:
    /// the energy detector cuts each tone into a piece of its own.
    fn tones(seconds: usize, at_s: &[f32]) -> Vec<f32> {
        let rate = SAMPLE_RATE as usize;
        let mut samples = vec![0.0; seconds * rate];
        for &at in at_s {
            let from = (at * rate as f32) as usize;
            for (i, sample) in samples[from..from + rate].iter_mut().enumerate() {
                *sample = 0.3 * (i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin();
            }
        }
        samples
    }

    fn energy() -> Box<dyn SpeechDetector> {
        Box::new(macalla_core::vad::energy::EnergyDetector::new())
    }

    /// Tools lent by a session on `engine`, hearing through the energy
    /// detector, whose second instance comes from `another`.
    fn tools(engine: impl SttEngine + 'static, another: OpenEngineFake) -> MeetingTools {
        let mut session = Session::with_engine(
            Box::new(engine),
            Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );
        session.detector = energy;
        let mut tools = session.meeting_tools().unwrap();
        tools.open_another = another.opener();
        tools
    }

    /// Stands in for opening another instance, and counts the calls.
    #[derive(Clone)]
    struct OpenEngineFake {
        calls: Arc<AtomicUsize>,
        engine: Arc<Mutex<Option<Says>>>,
        /// How long opening takes, as a model's cold load does.
        takes: Duration,
    }

    impl OpenEngineFake {
        /// Opens `engine`, once.
        fn opening(engine: Says) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                engine: Arc::new(Mutex::new(Some(engine))),
                takes: Duration::ZERO,
            }
        }
        /// Fails, as a model that cannot load a second time would.
        fn failing() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                engine: Arc::new(Mutex::new(None)),
                takes: Duration::ZERO,
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn opener(&self) -> meeting::OpenEngine {
            let this = self.clone();
            Arc::new(move || {
                this.calls.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(this.takes);
                match this.engine.lock().unwrap().take() {
                    Some(engine) => Ok(Box::new(engine) as Box<dyn SttEngine>),
                    None => Err(macalla_core::Error::Stt {
                        engine: "says",
                        message: "no second load".into(),
                    }),
                }
            })
        }
    }

    /// `inner`, as a source that has delivered nothing by the time the
    /// meeting's clock starts: the first poll after its start, the one the
    /// meeting lets go of, is empty, and every later one is `inner`'s. The
    /// fakes here serve their audio from their first poll on, at once; a
    /// device's pre-roll is what [`Paced`] is for.
    struct Fresh {
        inner: Box<dyn Source>,
        read_at_start: bool,
    }

    fn fresh(inner: Box<dyn Source>) -> Box<dyn Source> {
        Box::new(Fresh {
            inner,
            read_at_start: false,
        })
    }

    impl Source for Fresh {
        fn start(&mut self) -> Result<()> {
            self.inner.start()
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
            if !self.read_at_start {
                self.read_at_start = true;
                return Ok(0);
            }
            self.inner.poll_into(out)
        }
        fn finish_into(self: Box<Self>, out: &mut Vec<f32>) -> Result<usize> {
            self.inner.finish_into(out)
        }
    }

    /// Prepare the meeting and record it, as a caller does, with every
    /// source [`fresh`].
    fn record(
        tools: &MeetingTools,
        you: Box<dyn Source>,
        others: Option<Box<dyn Source>>,
        on_progress: &mut dyn FnMut(LiveProgress),
        ending: &Ending,
    ) -> Result<LiveMeeting> {
        prepare(tools, fresh(you), others.map(fresh))?.record(on_progress, ending)
    }

    fn speakers(meeting: &MeetingTranscript) -> Vec<Option<&str>> {
        meeting
            .turns
            .iter()
            .map(|turn| turn.speaker.as_deref())
            .collect()
    }

    fn in_meeting_order(turns: &[Turn]) -> bool {
        turns.windows(2).all(|w| w[0].start_ms <= w[1].start_ms)
    }

    /// You speak at 1 s and 6 s of 10 s; Others at 3.5 s of 8 s. Both run to
    /// the end, and the meeting stops when both have.
    fn two_voices(tools: &MeetingTools) -> (Result<LiveMeeting>, Vec<LiveProgress>) {
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let you = Scripted::new(tones(10, &[1.0, 6.0])).then(drained());
        let others = Scripted::new(tones(8, &[3.5])).then(drained());
        let mut seen = Vec::new();
        let result = record(
            tools,
            Box::new(you),
            Some(Box::new(others)),
            &mut |p| seen.push(p),
            &ending,
        );
        (result, seen)
    }

    #[test]
    fn two_sources_make_one_meeting_with_both_speakers_in_meeting_order() {
        let tools = tools(
            Says::new("we ship on friday", true),
            OpenEngineFake::failing(),
        );

        let (result, _) = two_voices(&tools);
        let meeting = result.unwrap().meeting;

        assert_eq!(
            speakers(&meeting),
            [Some(YOU), Some(OTHERS), Some(YOU)],
            "{:?}",
            meeting.turns
        );
        assert!(in_meeting_order(&meeting.turns), "{:?}", meeting.turns);
        // Times are from the meeting's start: Others' tone was at 3.5 s.
        assert!(
            (3_000..3_600).contains(&meeting.turns[1].start_ms),
            "{:?}",
            meeting.turns
        );
        assert!(meeting.named_speakers);
        assert_eq!(meeting.audio_ms, Some(10_000), "the longer stream");
        // Finished as a file meeting's turns are: Tier 0 capitalised it.
        assert!(meeting.turns.iter().all(|t| t.text.starts_with("We ship")));
    }

    #[test]
    fn two_fixture_recordings_make_one_meeting_with_both_speakers() {
        // fixtures/generate.sh makes these on macOS; skipped where it has not
        // been run, as the other fixture tests are.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/audio");
        let (you_path, others_path) = (
            dir.join("three-sentences.aiff"),
            dir.join("tech-terms.aiff"),
        );
        if !you_path.exists() || !others_path.exists() {
            eprintln!("skipped: fixture audio not generated here; run fixtures/generate.sh");
            return;
        }
        let you_samples = crate::decode_file(&you_path).unwrap();
        let others_samples = crate::decode_file(&others_path).unwrap();
        let longer_ms =
            you_samples.len().max(others_samples.len()) as u64 * 1_000 / SAMPLE_RATE as u64;
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let mut seen = Vec::new();

        let meeting = record(
            &tools,
            Box::new(Scripted::new(you_samples).then(drained())),
            Some(Box::new(Scripted::new(others_samples).then(drained()))),
            &mut |p| seen.push(p),
            &ending,
        )
        .unwrap()
        .meeting;

        let said = speakers(&meeting);
        assert!(said.contains(&Some(YOU)), "{said:?}");
        assert!(said.contains(&Some(OTHERS)), "{said:?}");
        assert!(in_meeting_order(&meeting.turns), "{:?}", meeting.turns);
        assert!(meeting.named_speakers);
        assert_eq!(meeting.audio_ms, Some(longer_ms));
        let last = seen.last().unwrap();
        assert!(last.you.signal && last.others.unwrap().signal, "{last:?}");
    }

    #[test]
    fn a_stop_mid_meeting_keeps_what_was_heard_and_nothing_after() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let stop_when_both_held = when_all_drained(&ending, 2, Ending::stop);
        // Both are 10 s meetings, but the stop comes at 5 s: served the first
        // five seconds, each source then has nothing more to give.
        let first_5s = |at: &[f32]| tones(10, at)[..5 * SAMPLE_RATE as usize].to_vec();
        let you = Scripted::new(first_5s(&[1.0, 6.0])).then(stop_when_both_held());
        let others = Scripted::new(first_5s(&[3.5, 7.5])).then(stop_when_both_held());

        let meeting = record(
            &tools,
            Box::new(you),
            Some(Box::new(others)),
            &mut |_| {},
            &ending,
        )
        .unwrap()
        .meeting;

        assert_eq!(speakers(&meeting), [Some(YOU), Some(OTHERS)]);
        assert_eq!(meeting.audio_ms, Some(5_000));
        assert!(meeting.turns.iter().all(|t| t.end_ms <= 5_000));
    }

    #[test]
    fn a_cancel_keeps_nothing_and_says_stopped() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let cancel = when_all_drained(&ending, 2, Ending::cancel);

        let result = record(
            &tools,
            Box::new(Scripted::new(tones(5, &[1.0])).then(cancel())),
            Some(Box::new(Scripted::new(tones(5, &[3.0])).then(cancel()))),
            &mut |_| {},
            &ending,
        );

        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
    }

    #[test]
    fn a_cancel_before_the_start_opens_nothing() {
        let another = OpenEngineFake::failing();
        let tools = tools(Says::new("heard", false), another.clone());
        let ending = Ending::new();
        ending.cancel();
        let you = Scripted::new(tones(2, &[0.5]));
        let polls = Arc::clone(&you.polls);

        let result = record(&tools, Box::new(you), None, &mut |_| {}, &ending);

        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(another.calls(), 0);
    }

    #[test]
    fn the_capability_alone_decides_which_instance_each_stream_is_heard_on() {
        let shared = EngineCaps {
            concurrent_streams: true,
            ..EngineCaps::batch()
        };
        let apart = EngineCaps::batch();

        assert_eq!(
            instances(&shared, true),
            Instances {
                you: Instance::Borrowed,
                others: Some(Instance::Borrowed),
            }
        );
        assert_eq!(
            instances(&apart, true),
            Instances {
                you: Instance::Borrowed,
                others: Some(Instance::Another),
            }
        );
        for caps in [&shared, &apart] {
            assert_eq!(
                instances(caps, false),
                Instances {
                    you: Instance::Borrowed,
                    others: None,
                },
                "a microphone-only meeting needs no second instance"
            );
        }
    }

    #[test]
    fn a_second_instance_is_opened_once_for_others_only_when_streams_cannot_share() {
        // Streams cannot share: Others is heard on a new instance, You on the
        // borrowed one.
        let another = OpenEngineFake::opening(Says::new("another instance", false));
        let (result, _) = two_voices(&tools(
            Says::new("the borrowed engine", false),
            another.clone(),
        ));
        let meeting = result.unwrap().meeting;
        assert_eq!(another.calls(), 1);
        for turn in &meeting.turns {
            let expected = match turn.speaker.as_deref() {
                Some(OTHERS) => "Another instance",
                _ => "The borrowed engine",
            };
            assert_eq!(turn.text, expected, "{:?}", meeting.turns);
        }

        // Streams can share: nothing is opened.
        let another = OpenEngineFake::opening(Says::new("another instance", true));
        let (result, _) = two_voices(&tools(
            Says::new("the borrowed engine", true),
            another.clone(),
        ));
        assert!(result
            .unwrap()
            .meeting
            .turns
            .iter()
            .all(|t| t.text == "The borrowed engine"));
        assert_eq!(another.calls(), 0);

        // Microphone only, on an engine that cannot share: nothing is opened.
        let another = OpenEngineFake::opening(Says::new("another instance", false));
        let tools_alone = tools(Says::new("the borrowed engine", false), another.clone());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 1, Ending::stop);
        let meeting = record(
            &tools_alone,
            Box::new(Scripted::new(tones(4, &[1.0])).then(drained())),
            None,
            &mut |_| {},
            &ending,
        )
        .unwrap()
        .meeting;
        assert_eq!(another.calls(), 0);
        assert_eq!(meeting.turns[0].text, "The borrowed engine");
    }

    #[test]
    fn a_second_instance_that_cannot_open_is_a_typed_error_before_anything_is_heard() {
        let borrowed = Says::new("the borrowed engine", false);
        let pieces = Arc::clone(&borrowed.pieces);
        let another = OpenEngineFake::failing();
        let tools = tools(borrowed, another.clone());
        // They end the meeting if ever heard, so a broken gate fails this
        // test rather than hanging it.
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let (you, others) = (
            Scripted::new(tones(4, &[1.0])).then(drained()),
            Scripted::new(tones(4, &[2.0])).then(drained()),
        );
        let polls = [Arc::clone(&you.polls), Arc::clone(&others.polls)];
        let mut reports = 0;

        let result = record(
            &tools,
            Box::new(you),
            Some(Box::new(others)),
            &mut |_| reports += 1,
            &ending,
        );

        assert!(matches!(result, Err(Error::SecondEngine(_))), "{result:?}");
        assert_eq!(another.calls(), 1);
        assert!(polls.iter().all(|p| p.load(Ordering::SeqCst) == 0));
        assert_eq!(pieces.load(Ordering::SeqCst), 0);
        assert_eq!(reports, 0);
    }

    #[test]
    fn a_blocked_push_on_one_stream_does_not_hold_up_the_other() {
        // One engine shared by both streams. Others speaks at 0.5 s, and the
        // engine holds any piece that starts before 2 s until You's source
        // has been heard to its end; You speaks at 3 s and 6 s and holds its
        // end back until Others' piece is being held. Heard one after the
        // other on one thread, You would never get there and the gate would
        // starve.
        let gate = Gate::default();
        let mut shared = Says::new("heard", true);
        shared.gate = Some((gate.clone(), 0..2_000));
        let tools = tools(shared, OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let you_done = {
            let (gate, drained) = (gate.clone(), drained());
            move || {
                gate.open();
                drained();
            }
        };

        let mut you = Scripted::new(tones(9, &[3.0, 6.0])).then(you_done);
        you.last_waits_for = Some((Arc::clone(&gate.entered), Arc::clone(&gate.starved)));

        let meeting = record(
            &tools,
            Box::new(you),
            // Quiet for long enough after the tone that the piece closes
            // while the stream is still being fed, not at its finish.
            Some(Box::new(Scripted::new(tones(6, &[0.5])).then(drained()))),
            &mut |_| {},
            &ending,
        )
        .unwrap()
        .meeting;

        assert!(!gate.starved.load(Ordering::SeqCst), "You waited on Others");
        assert_eq!(gate.entered.load(Ordering::SeqCst), 1, "only Others' piece");
        assert_eq!(speakers(&meeting), [Some(OTHERS), Some(YOU), Some(YOU)]);
    }

    #[test]
    fn a_silent_source_reports_no_signal_and_a_sounding_one_does() {
        // The call's audio refused: correctly-clocked zeros, and no error.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let mut seen = Vec::new();

        let meeting = record(
            &tools,
            Box::new(Scripted::new(tones(4, &[1.0])).then(drained())),
            Some(Box::new(
                Scripted::new(vec![0.0; 4 * SAMPLE_RATE as usize]).then(drained()),
            )),
            &mut |p| seen.push(p),
            &ending,
        )
        .unwrap()
        .meeting;

        let last = seen.last().copied().unwrap();
        assert!(last.you.signal);
        assert!(last.you.since_signal_ms.is_some());
        let others = last.others.unwrap();
        assert!(!others.signal, "zeros are no signal, however many arrive");
        assert_eq!(others.since_signal_ms, None);
        // Samples still arrived: health is not judged by them.
        assert_eq!(last.heard_ms, 4_000);
        assert_eq!(speakers(&meeting), [Some(YOU)]);
        assert!(
            meeting.named_speakers,
            "the call had a stream, heard or not"
        );
    }

    #[test]
    fn a_microphone_only_meeting_names_no_speaker() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 1, Ending::stop);
        let mut seen = Vec::new();

        let meeting = record(
            &tools,
            Box::new(Scripted::new(tones(4, &[1.0])).then(drained())),
            None,
            &mut |p| seen.push(p),
            &ending,
        )
        .unwrap()
        .meeting;

        assert_eq!(speakers(&meeting), [None]);
        assert!(!meeting.named_speakers);
        assert_eq!(seen.last().unwrap().others, None);
    }

    /// A device that is lost at its first poll, after putting `with` in
    /// `out` on that same poll, as the microphone does when it faults, and
    /// whose finish gives `tail`: what its resampler still held.
    struct Lost {
        with: Vec<f32>,
        tail: Vec<f32>,
    }

    impl Lost {
        fn with(with: Vec<f32>) -> Self {
            Self {
                with,
                tail: Vec::new(),
            }
        }
    }

    impl Source for Lost {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
            out.append(&mut self.with);
            Err(macalla_platform::Error::DeviceLost("call audio".into()).into())
        }
        fn finish_into(mut self: Box<Self>, out: &mut Vec<f32>) -> Result<usize> {
            let n = self.tail.len();
            out.append(&mut self.tail);
            Ok(n)
        }
    }

    fn lost_device(result: &Result<LiveMeeting>) -> bool {
        matches!(
            result,
            Ok(LiveMeeting {
                ended_early: Some(Error::Platform(macalla_platform::Error::DeviceLost(_))),
                ..
            })
        )
    }

    #[test]
    fn a_source_that_fails_partway_keeps_the_audio_it_delivered_with_the_failure() {
        // Every word here arrives on the poll that fails.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let lost = Lost::with(tones(3, &[1.0]));

        let result = record(&tools, Box::new(lost), None, &mut |_| {}, &Ending::new());

        assert!(lost_device(&result), "{result:?}");
        let meeting = result.unwrap().meeting;
        assert_eq!(speakers(&meeting), [None]);
        assert_eq!(meeting.turns[0].text, "Heard");
        assert_eq!(meeting.audio_ms, Some(3_000));
    }

    #[test]
    fn a_stream_that_fails_before_any_words_returns_its_error_not_nothing_heard() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        // You never runs dry and hears only silence.
        let you = Scripted::new(vec![0.0; 60 * SAMPLE_RATE as usize]);

        let result = record(
            &tools,
            Box::new(you),
            Some(Box::new(Lost::with(Vec::new()))),
            &mut |_| {},
            &Ending::new(),
        );

        assert!(
            matches!(
                result,
                Err(Error::Platform(macalla_platform::Error::DeviceLost(_)))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn a_source_that_fails_still_gives_its_tail_to_the_engine() {
        // Nothing arrives before the failure; every word is in the tail the
        // source's resampler still held.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let lost = Lost {
            with: Vec::new(),
            tail: tones(3, &[1.0]),
        };

        let result = record(&tools, Box::new(lost), None, &mut |_| {}, &Ending::new());

        assert!(lost_device(&result), "{result:?}");
        let meeting = result.unwrap().meeting;
        assert_eq!(meeting.turns.len(), 1, "{:?}", meeting.turns);
        assert_eq!(meeting.turns[0].text, "Heard");
        assert_eq!(meeting.audio_ms, Some(3_000));
    }

    /// Delivers nothing until `go` is set, or for five seconds, and then
    /// fails with a lost device named "call audio".
    struct LostWhen {
        go: Arc<AtomicBool>,
        made: Instant,
    }

    impl Source for LostWhen {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, _out: &mut Vec<f32>) -> Result<usize> {
            if !self.go.load(Ordering::SeqCst) && self.made.elapsed() < Duration::from_secs(5) {
                return Ok(0);
            }
            Err(macalla_platform::Error::DeviceLost("call audio".into()).into())
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn when_both_streams_fail_the_one_that_failed_first_ended_the_meeting() {
        // Others fails once You's tone at 1 s has been served. You fails
        // too, but only at its finish, and its finish is behind a gate that
        // only Others' failure opens: nothing else ends this meeting. So You
        // fails second, every time, and the meeting says Others ended it.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let go = Arc::new(AtomicBool::new(false));
        let mut you = Scripted::new(tones(60, &[1.0]));
        you.fails_at_finish = true;
        let go_now = Arc::clone(&go);
        you.at_poll = Some((30, Box::new(move || go_now.store(true, Ordering::SeqCst))));
        let others = LostWhen {
            go,
            made: Instant::now(),
        };

        let result = record(
            &tools,
            Box::new(you),
            Some(Box::new(others)),
            &mut |_| {},
            &Ending::new(),
        );

        match &result {
            Ok(LiveMeeting {
                ended_early: Some(Error::Platform(macalla_platform::Error::DeviceLost(device))),
                meeting,
            }) => {
                assert_eq!(device, "call audio", "the first failure, not You's");
                assert_eq!(speakers(meeting), [Some(YOU)]);
            }
            other => panic!("expected the meeting kept with Others' failure, got {other:?}"),
        }
    }

    /// Serves `first` on its first poll, then nothing until `go` is set (or
    /// five seconds pass), then fails as system audio does when its format
    /// changes.
    struct FormatChangesWhen {
        first: Vec<f32>,
        go: Arc<AtomicBool>,
        made: Instant,
    }

    impl Source for FormatChangesWhen {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
            if !self.first.is_empty() {
                let n = self.first.len();
                out.append(&mut self.first);
                return Ok(n);
            }
            if !self.go.load(Ordering::SeqCst) && self.made.elapsed() < Duration::from_secs(5) {
                return Ok(0);
            }
            Err(CaptureError::FormatChanged.into())
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn a_format_change_in_the_call_audio_ends_the_meeting_and_keeps_both_voices() {
        // Others speaks at 0.5 s, then its format changes once You's tone
        // at 1 s has been served.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let go = Arc::new(AtomicBool::new(false));
        let mut you = Scripted::new(tones(60, &[1.0]));
        let go_now = Arc::clone(&go);
        you.at_poll = Some((30, Box::new(move || go_now.store(true, Ordering::SeqCst))));
        let others = FormatChangesWhen {
            first: tones(2, &[0.5]),
            go,
            made: Instant::now(),
        };

        let result = record(
            &tools,
            Box::new(you),
            Some(Box::new(others)),
            &mut |_| {},
            &Ending::new(),
        );

        match &result {
            Ok(LiveMeeting {
                ended_early: Some(Error::CallAudioFormatChanged),
                meeting,
            }) => assert_eq!(speakers(meeting), [Some(OTHERS), Some(YOU)]),
            other => panic!("expected the meeting kept with the format change, got {other:?}"),
        }
    }

    #[test]
    fn a_capture_failure_is_typed_as_the_meetings() {
        assert!(matches!(
            Error::from(CaptureError::FormatChanged),
            Error::CallAudioFormatChanged
        ));
        let resampler = macalla_core::Error::Audio("resampler".into());
        assert!(matches!(
            Error::from(CaptureError::Chain(resampler)),
            Error::Core(macalla_core::Error::Audio(_))
        ));
    }

    /// Stands in for the call's audio, opened.
    struct Opened;

    impl Source for Opened {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, _out: &mut Vec<f32>) -> Result<usize> {
            Ok(0)
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn call_audio_that_opened_is_the_others_source() {
        let call = call_audio(Ok(Opened));
        assert!(call.others.is_some());
        assert!(call.unavailable.is_none());
    }

    #[test]
    fn call_audio_that_could_not_open_makes_a_microphone_only_meeting_and_says_why() {
        let failures = [
            OpenError::Unsupported,
            OpenError::OwnProcessUnknown,
            OpenError::CoreAudio {
                step: macalla_platform::system_audio::Step::CreateTap,
                status: -1,
            },
        ];
        for failure in failures {
            let expected = format!("{failure:?}");
            let call = call_audio::<SystemAudio>(Err(failure));
            assert!(call.others.is_none());
            assert_eq!(format!("{:?}", call.unavailable.unwrap()), expected);
        }

        // And the meeting it makes is heard, not failed.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 1, Ending::stop);
        let call = call_audio::<SystemAudio>(Err(OpenError::Unsupported));
        let meeting = record(
            &tools,
            Box::new(Scripted::new(tones(4, &[1.0])).then(drained())),
            call.others,
            &mut |_| {},
            &ending,
        )
        .unwrap();
        assert!(meeting.ended_early.is_none());
        assert_eq!(speakers(&meeting.meeting), [None]);
        assert!(!meeting.meeting.named_speakers);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn elsewhere_the_call_audio_is_unavailable_as_unsupported() {
        let call = open_call_audio();
        assert!(call.others.is_none());
        assert!(matches!(call.unavailable, Some(OpenError::Unsupported)));
    }

    /// Streams without endpointing, as Nemotron does: while pushed, only
    /// previews, and one final for the whole stream at its finish. A piece
    /// given to `transcribe` is heard as that piece.
    struct Rolling;

    struct RollingStream {
        pushed: u64,
    }

    impl RollingStream {
        fn pushed_ms(&self) -> u64 {
            self.pushed * 1_000 / SAMPLE_RATE as u64
        }
    }

    impl SttStream for RollingStream {
        fn push(&mut self, samples: &[f32]) -> macalla_core::Result<()> {
            self.pushed += samples.len() as u64;
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            Ok(vec![Transcript {
                text: "a preview".into(),
                is_final: false,
                start_ms: 0,
                end_ms: self.pushed_ms(),
            }])
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            Ok(vec![Transcript {
                text: "the whole stream".into(),
                is_final: true,
                start_ms: 0,
                end_ms: self.pushed_ms(),
            }])
        }
    }

    impl SttEngine for Rolling {
        fn id(&self) -> &'static str {
            "rolling"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps {
                streaming: true,
                owns_segmentation: false,
                concurrent_streams: true,
                ..EngineCaps::batch()
            }
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(RollingStream { pushed: 0 }))
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            Ok(Transcript {
                text: "a piece".into(),
                is_final: true,
                start_ms: audio.start_ms,
                end_ms: audio.start_ms + audio.duration_ms(),
            })
        }
    }

    #[test]
    fn the_stream_kind_follows_segmentation_not_streaming() {
        let endpointed = EngineCaps::stream_first();
        let rolling = Rolling.caps();
        assert_eq!(stream_kind(&endpointed), StreamKind::Endpointed);
        assert_eq!(stream_kind(&rolling), StreamKind::Pieces);
        assert_eq!(stream_kind(&EngineCaps::batch()), StreamKind::Pieces);
    }

    #[test]
    fn an_engine_that_streams_without_endpointing_is_heard_in_pieces() {
        // On its own stream this engine would give each voice one turn from
        // the meeting's start, at the stop. In pieces, each voice has a turn
        // per thing said, and the two interleave.
        let (result, _) = two_voices(&tools(Rolling, OpenEngineFake::failing()));
        let meeting = result.unwrap().meeting;

        assert_eq!(
            speakers(&meeting),
            [Some(YOU), Some(OTHERS), Some(YOU)],
            "{:?}",
            meeting.turns
        );
        assert!(
            meeting.turns.iter().all(|t| t.text == "A piece"),
            "{:?}",
            meeting.turns
        );
        assert!(in_meeting_order(&meeting.turns));
    }

    /// Delivers nothing until `go` is set, or for five seconds, and then
    /// panics, as a broken tap adapter might.
    struct Panics {
        go: Arc<AtomicBool>,
        made: Instant,
    }

    impl Source for Panics {
        fn start(&mut self) -> Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, _out: &mut Vec<f32>) -> Result<usize> {
            if !self.go.load(Ordering::SeqCst) && self.made.elapsed() < Duration::from_secs(5) {
                return Ok(0);
            }
            panic!("the tap fell over");
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn a_stream_that_panics_keeps_the_other_streams_words_and_says_why() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        // You never runs dry: only the panic can end this meeting. Others
        // panics once You's tone, at 1 s, has been served.
        let go = Arc::new(AtomicBool::new(false));
        let mut you = Scripted::new(tones(60, &[1.0]));
        let go_now = Arc::clone(&go);
        you.at_poll = Some((30, Box::new(move || go_now.store(true, Ordering::SeqCst))));
        let panics = Panics {
            go,
            made: Instant::now(),
        };
        let begun = Instant::now();

        let result = record(
            &tools,
            Box::new(you),
            Some(Box::new(panics)),
            &mut |_| {},
            &Ending::new(),
        );

        match &result {
            Ok(LiveMeeting {
                ended_early: Some(Error::StreamPanicked(message)),
                ..
            }) => assert!(message.contains("tap fell over"), "{message}"),
            other => panic!("expected the meeting kept with StreamPanicked, got {other:?}"),
        }
        let meeting = result.unwrap().meeting;
        assert_eq!(speakers(&meeting), [Some(YOU)]);
        assert_eq!(meeting.turns[0].text, "Heard");
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
    }

    #[test]
    fn a_progress_report_that_panics_does_not_hang_the_meeting() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        // Never runs dry: without the guard, both would wait for a stop until
        // the test deadline.
        let you = Scripted::new(tones(60, &[1.0]));
        let begun = Instant::now();

        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            record(
                &tools,
                Box::new(you),
                None,
                &mut |_| panic!("the page went away"),
                &Ending::new(),
            )
        }));

        assert!(outcome.is_err(), "the panic reaches the caller");
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
    }

    #[test]
    fn a_cancel_during_a_stops_finish_keeps_nothing() {
        // The stop is asked for, and while the last piece is being heard at
        // the finish, a cancel arrives. The cancel wins.
        let gate = Gate::default();
        let mut engine = Says::new("heard", true);
        engine.gate = Some((gate.clone(), 1_000..u64::MAX));
        let tools = tools(engine, OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 1, Ending::stop);
        // The tone runs to the end, so its piece is still open at the finish.
        let you = Scripted::new(tones(3, &[2.0])).then(drained());
        let canceller = {
            let (gate, ending) = (gate.clone(), Arc::clone(&ending));
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while gate.entered.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                ending.cancel();
                gate.open();
            })
        };

        let result = record(&tools, Box::new(you), None, &mut |_| {}, &ending);
        canceller.join().unwrap();

        assert_eq!(gate.entered.load(Ordering::SeqCst), 1, "held at the finish");
        assert!(ending.stopped());
        assert!(matches!(result, Err(Error::Stopped)), "{result:?}");
    }

    #[test]
    fn an_engine_that_fails_on_one_stream_keeps_both_streams_words_from_before() {
        // Others' engine hears its first piece, at 0.5 s, and fails on its
        // second, at 4 s, which it holds until You's tone at 1 s has been
        // served, so You has words to keep.
        let gate = Gate::default();
        let mut breaks = Says::new("others", false);
        breaks.fails_from = Some(2);
        breaks.gate = Some((gate.clone(), 3_000..u64::MAX));
        let tools = tools(Says::new("you", false), OpenEngineFake::opening(breaks));
        // You never runs dry: only Others' failure can end this meeting.
        let mut you = Scripted::new(tones(60, &[1.0]));
        let open_gate = gate.clone();
        you.at_poll = Some((30, Box::new(move || open_gate.open())));
        let others = Scripted::new(tones(9, &[0.5, 4.0]));
        let begun = Instant::now();

        let result = record(
            &tools,
            Box::new(you),
            Some(Box::new(others)),
            &mut |_| {},
            &Ending::new(),
        );

        assert!(
            matches!(
                result,
                Ok(LiveMeeting {
                    ended_early: Some(Error::Core(macalla_core::Error::Stt { engine: "says", .. })),
                    ..
                })
            ),
            "{result:?}"
        );
        assert!(!gate.starved.load(Ordering::SeqCst));
        let meeting = result.unwrap().meeting;
        assert_eq!(speakers(&meeting), [Some(OTHERS), Some(YOU)]);
        let texts: Vec<&str> = meeting.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["Others", "You"]);
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "{:?}",
            begun.elapsed()
        );
    }

    #[test]
    fn signal_health_says_how_long_ago_the_last_sound_was() {
        // Half a second of sound, then a second of zeros, served at 30 ms a
        // poll by the wall clock: at least ten polls, 300 ms, after the last
        // sound.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 1, Ending::stop);
        let mut samples = vec![0.0; 3 * SAMPLE_RATE as usize / 2];
        samples[..SAMPLE_RATE as usize / 2].fill(0.3);
        let mut you = Scripted::new(samples).then(drained());
        you.pace = Some(Duration::from_millis(30));
        let mut seen = Vec::new();

        let _ = record(&tools, Box::new(you), None, &mut |p| seen.push(p), &ending);

        let since = seen.last().unwrap().you.since_signal_ms.unwrap();
        assert!((295..2_000).contains(&since), "{since} ms");
    }

    #[test]
    fn a_meeting_with_no_words_says_nothing_was_heard() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 1, Ending::stop);

        let result = record(
            &tools,
            Box::new(Scripted::new(vec![0.0; 2 * SAMPLE_RATE as usize]).then(drained())),
            None,
            &mut |_| {},
            &ending,
        );

        assert!(matches!(result, Err(Error::NothingHeard)), "{result:?}");
    }

    /// Replies with every block in capitals, as meeting.rs's tests do.
    struct Shouting;

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
            Ok(blocks
                .iter()
                .map(|block| Block::Paragraph {
                    text: block.text().to_uppercase(),
                })
                .collect())
        }
    }

    #[test]
    fn tidying_and_the_summary_split_take_a_live_meeting_unchanged() {
        let tools = tools(
            Says::new("we ship on friday", true),
            OpenEngineFake::failing(),
        );
        let (result, _) = two_voices(&tools);
        let live = result.unwrap().meeting;

        // The summary's split: each line sent with its speaker.
        let parts = meeting_parts::split(&live.turns, PART_BUDGET_CHARS, Labels::Named);
        let sent: Vec<String> = parts
            .iter()
            .flatten()
            .map(|line| line.sent(Labels::Named))
            .collect();
        assert_eq!(
            sent,
            [
                "You: We ship on friday",
                "Others: We ship on friday",
                "You: We ship on friday",
            ]
        );

        // Tidying: the same turns and speakers back, with the words changed.
        let mut tidy_tools = tools.clone();
        tidy_tools.cleanup = Some(Cleanup::new(
            Arc::new(Shouting),
            CleanupReach::Remote,
            "Your server",
        ));
        let tidied = meeting::tidy(
            &tidy_tools.into_tidy().unwrap(),
            &live,
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(speakers(&tidied.meeting), speakers(&live));
        assert!(tidied
            .meeting
            .turns
            .iter()
            .all(|t| t.text == "WE SHIP ON FRIDAY"));
        assert_eq!(tidied.meeting.audio_ms, live.audio_ms);
    }

    /// A world every source in a test meeting hears: a second of tone from
    /// one second after `epoch`, and quiet otherwise.
    fn tone_a_second_in(since_epoch: Duration) -> f32 {
        let t = since_epoch.as_secs_f32();
        if (1.0..2.0).contains(&t) {
            0.3 * (t * 440.0 * std::f32::consts::TAU).sin()
        } else {
            0.0
        }
    }

    /// Delivers [`tone_a_second_in`] as a device would: paced by the wall
    /// clock from the moment it is started, each sample taken from the world
    /// at the moment it was captured, so everything between its start and
    /// the meeting's clock is pre-roll. Two sources put the tone at the same
    /// meeting time only if they share sample 0. Its start can take a while,
    /// capturing all the while, as a start that waits on a permission prompt
    /// would. Delivers nothing before its start, and runs dry `seconds` after
    /// it, calling `drained`.
    struct Paced {
        epoch: Instant,
        seconds: f64,
        start_takes: Duration,
        started_at: Arc<Mutex<Option<Instant>>>,
        served: usize,
        drained: Option<Box<dyn FnOnce() + Send>>,
        made: Instant,
        /// Its name, written into the log as it is started.
        logged_as: Option<(&'static str, StartLog)>,
    }

    /// The order sources were started in, by name.
    type StartLog = Arc<Mutex<Vec<&'static str>>>;

    impl Paced {
        fn new(epoch: Instant, seconds: f64) -> Self {
            Self {
                epoch,
                seconds,
                start_takes: Duration::ZERO,
                started_at: Arc::new(Mutex::new(None)),
                served: 0,
                drained: None,
                made: Instant::now(),
                logged_as: None,
            }
        }
        fn logged(mut self, name: &'static str, log: &StartLog) -> Self {
            self.logged_as = Some((name, Arc::clone(log)));
            self
        }
        fn taking(mut self, start_takes: Duration) -> Self {
            self.start_takes = start_takes;
            self
        }
        fn then(mut self, drained: impl FnOnce() + Send + 'static) -> Self {
            self.drained = Some(Box::new(drained));
            self
        }
    }

    impl Source for Paced {
        fn start(&mut self) -> Result<()> {
            if let Some((name, log)) = &self.logged_as {
                log.lock().unwrap().push(name);
            }
            *self.started_at.lock().unwrap() = Some(Instant::now());
            std::thread::sleep(self.start_takes);
            Ok(())
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
            if self.made.elapsed() > TEST_DEADLINE {
                return Err(macalla_platform::Error::DeviceLost("test deadline".into()).into());
            }
            let Some(started) = *self.started_at.lock().unwrap() else {
                return Ok(0);
            };
            let rate = f64::from(SAMPLE_RATE);
            let total = (self.seconds * rate) as usize;
            let captured = ((started.elapsed().as_secs_f64() * rate) as usize).min(total);
            for i in self.served..captured {
                let at = started + Duration::from_secs_f64(i as f64 / rate);
                out.push(tone_a_second_in(at.saturating_duration_since(self.epoch)));
            }
            let n = captured - self.served;
            self.served = captured;
            if self.served == total {
                if let Some(drained) = self.drained.take() {
                    drained();
                }
            }
            Ok(n)
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> Result<usize> {
            Ok(0)
        }
    }

    /// The start of the one turn `speaker` has.
    fn start_of(meeting: &MeetingTranscript, speaker: &str) -> u64 {
        let theirs: Vec<&Turn> = meeting
            .turns
            .iter()
            .filter(|turn| turn.speaker.as_deref() == Some(speaker))
            .collect();
        assert_eq!(theirs.len(), 1, "{speaker}: {:?}", meeting.turns);
        theirs[0].start_ms
    }

    #[test]
    fn two_sources_started_at_different_moments_still_share_sample_0() {
        // The call's audio is started first, and its start takes 400 ms,
        // capturing all the while; the microphone starts after it. Without
        // a common sample 0, the call's copy of the tone would land 400 ms
        // later in the meeting than the microphone's.
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let epoch = Instant::now();
        let started = StartLog::default();
        let others = Paced::new(epoch, 3.0)
            .taking(Duration::from_millis(400))
            .logged(OTHERS, &started)
            .then(drained());
        let you = Paced::new(epoch, 3.0).logged(YOU, &started).then(drained());

        let meeting = prepare(&tools, Box::new(you), Some(Box::new(others)))
            .unwrap()
            .record(&mut |_| {}, &ending)
            .unwrap()
            .meeting;

        assert_eq!(
            *started.lock().unwrap(),
            [OTHERS, YOU],
            "the call's audio first: a start that waits on its prompt leaves the microphone off"
        );

        let (you_at, others_at) = (start_of(&meeting, YOU), start_of(&meeting, OTHERS));
        assert!(
            you_at.abs_diff(others_at) < 150,
            "one sound, heard {you_at} ms and {others_at} ms into the meeting"
        );
    }

    #[test]
    fn a_slow_second_instance_load_starts_no_source_and_leaves_no_pre_roll() {
        // The second instance takes half a second to load. Nothing plays
        // while it does: the sources start once it has, the tone a second
        // after `epoch` is heard about half a second into the meeting on
        // both streams, not a second in, and every second each source
        // captured from its start is heard.
        let mut another = OpenEngineFake::opening(Says::new("another instance", false));
        another.takes = Duration::from_millis(500);
        let tools = tools(Says::new("the borrowed engine", false), another.clone());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let epoch = Instant::now();
        let you = Paced::new(epoch, 3.0).then(drained());
        let others = Paced::new(epoch, 3.0).then(drained());
        let started = [Arc::clone(&you.started_at), Arc::clone(&others.started_at)];

        let prepared = prepare(&tools, Box::new(you), Some(Box::new(others))).unwrap();
        let loaded = Instant::now();
        assert_eq!(another.calls(), 1);
        assert!(
            started.iter().all(|at| at.lock().unwrap().is_none()),
            "nothing plays while the instance loads"
        );
        let meeting = prepared.record(&mut |_| {}, &ending).unwrap().meeting;

        assert!(started
            .iter()
            .all(|at| at.lock().unwrap().is_some_and(|at| at >= loaded)));
        for speaker in [YOU, OTHERS] {
            let at = start_of(&meeting, speaker);
            assert!(at < 900, "{speaker} heard the tone {at} ms in");
        }
        let texts: Vec<&str> = meeting.turns.iter().map(|t| t.text.as_str()).collect();
        assert!(texts.contains(&"The borrowed engine") && texts.contains(&"Another instance"));
        let heard = meeting.audio_ms.unwrap();
        assert!((2_900..=3_000).contains(&heard), "{heard} ms heard");
    }

    /// A source that cannot start, with `error`, and counts its polls.
    struct WontStart {
        error: fn() -> Error,
        polls: Arc<AtomicUsize>,
    }

    impl WontStart {
        fn with(error: fn() -> Error) -> Self {
            Self {
                error,
                polls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Source for WontStart {
        fn start(&mut self) -> Result<()> {
            Err((self.error)())
        }
        fn poll_into(&mut self, _out: &mut Vec<f32>) -> Result<usize> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> Result<usize> {
            Ok(0)
        }
    }

    #[test]
    fn a_source_whose_start_fails_ends_as_a_typed_error_before_anything_is_heard() {
        use macalla_platform::system_audio::Step;
        let engine = Says::new("heard", true);
        let pieces = Arc::clone(&engine.pieces);
        let tools = tools(engine, OpenEngineFake::failing());

        // The call's audio will not start: the microphone is never started
        // or read.
        let you = Scripted::new(tones(4, &[1.0]));
        let others = WontStart::with(|| {
            Error::CallAudioStart(OpenError::CoreAudio {
                step: Step::Start,
                status: -1,
            })
        });
        let polls = [Arc::clone(&you.polls), Arc::clone(&others.polls)];
        let mut reports = 0;
        let result = prepare(&tools, Box::new(you), Some(Box::new(others)))
            .unwrap()
            .record(&mut |_| reports += 1, &Ending::new());
        assert!(
            matches!(
                result,
                Err(Error::CallAudioStart(OpenError::CoreAudio {
                    step: Step::Start,
                    ..
                }))
            ),
            "{result:?}"
        );
        assert!(polls.iter().all(|p| p.load(Ordering::SeqCst) == 0));

        // The microphone will not start: the call's audio, started first,
        // is never read.
        let you = WontStart::with(|| {
            Error::Platform(macalla_platform::Error::Audio("starting the stream".into()))
        });
        let others = Scripted::new(tones(4, &[1.0]));
        let polls = [Arc::clone(&you.polls), Arc::clone(&others.polls)];
        let result = prepare(&tools, Box::new(you), Some(Box::new(others)))
            .unwrap()
            .record(&mut |_| reports += 1, &Ending::new());
        assert!(
            matches!(
                result,
                Err(Error::Platform(macalla_platform::Error::Audio(_)))
            ),
            "{result:?}"
        );
        assert!(polls.iter().all(|p| p.load(Ordering::SeqCst) == 0));
        assert_eq!(pieces.load(Ordering::SeqCst), 0);
        assert_eq!(reports, 0);
    }

    #[test]
    fn a_meeting_stopped_before_it_starts_starts_no_source_and_heard_nothing() {
        let tools = tools(Says::new("heard", true), OpenEngineFake::failing());
        let epoch = Instant::now();
        let (you, others) = (Paced::new(epoch, 3.0), Paced::new(epoch, 3.0));
        let started = [Arc::clone(&you.started_at), Arc::clone(&others.started_at)];
        let prepared = prepare(&tools, Box::new(you), Some(Box::new(others))).unwrap();
        let ending = Ending::new();
        ending.stop();
        let mut reports = 0;

        let result = prepared.record(&mut |_| reports += 1, &ending);

        assert!(matches!(result, Err(Error::NothingHeard)), "{result:?}");
        assert!(
            started.iter().all(|at| at.lock().unwrap().is_none()),
            "no source was started, so no prompt was raised"
        );
        assert_eq!(reports, 0);
    }

    #[test]
    fn what_a_meeting_has_committed_is_readable_while_it_records() {
        // Others commits a piece at 0.5 s and then holds its next one, at
        // 4 s, until the test has read what is committed: a caller that
        // cannot wait for the end still has the first piece's words.
        let gate = Gate::default();
        let mut engine = Says::new("heard", true);
        engine.gate = Some((gate.clone(), 3_000..u64::MAX));
        let tools = tools(engine, OpenEngineFake::failing());
        let ending = Arc::new(Ending::new());
        let drained = when_all_drained(&ending, 2, Ending::stop);
        let prepared = prepare(
            &tools,
            Box::new(Scripted::new(vec![0.0; 6 * SAMPLE_RATE as usize]).then(drained())),
            Some(Box::new(
                Scripted::new(tones(8, &[0.5, 4.0])).then(drained()),
            )),
        )
        .unwrap();
        let committed = prepared.committed();
        assert!(committed.meeting().is_none(), "nothing yet");

        let reader = {
            let (committed, gate) = (committed.clone(), gate.clone());
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while gate.entered.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let seen = committed.meeting();
                gate.open();
                seen
            })
        };
        let whole = prepared.record(&mut |_| {}, &ending).unwrap().meeting;
        let seen = reader
            .join()
            .unwrap()
            .expect("the first piece was committed");

        assert_eq!(speakers(&seen), [Some(OTHERS)]);
        assert!(seen.named_speakers);
        assert_eq!(speakers(&whole), [Some(OTHERS), Some(OTHERS)]);
        assert_eq!(
            committed.meeting().unwrap().turns,
            whole.turns,
            "all of it by the end"
        );
    }
}
