// SPDX-License-Identifier: MIT
//! One dictation, from the trigger to the text.
//!
//! This is the use case the CLI and the desktop app share, and it exists so
//! that neither has to own it. Before it, `macalla listen` carried the whole
//! sequence -- pick the engine, find the dictionary, build the pipeline, arm
//! the microphone, stream to the engine until the trigger says stop, keep the
//! tail, assemble, time every stage -- and the desktop app was about to need
//! an identical copy. A sequence with this many rules in it (keep the audio
//! when the device dies; time the device apart from the engine; arm before
//! the key, not after) does not survive being in two places.
//!
//! What lives here: everything between "start" and a [`Dictation`]. What does
//! not: the trigger (a key, Enter, an agent), and what to do with the text
//! (print it, paste it, show it). [`paste`] is offered because both apps paste
//! and the rule about empty text belongs with it, but calling it is the app's
//! decision.
//!
//! Dependencies point inward: this crate knows the engines, the platform and
//! the core; nothing in those knows it.

use macalla_core::audio::{AudioSegment, SAMPLE_RATE};
use macalla_core::block::Document;
use macalla_core::dictionary::Dictionary;
use macalla_core::latency;
use macalla_core::pipeline::{Pipeline, PipelinePlan, TextCaps};
use macalla_core::stt::{self, EngineCaps, SttContext, SttEngine, SttStream, Transcript};
use macalla_core::vad::{SpeechDetector, Vad, VadConfig, VadEvent};
use macalla_engines::registry;
use macalla_platform::audio::Capture;
use macalla_platform::paste::Inserted;
use macalla_platform::secrets::{SecretStore, SystemSecrets};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub mod choice;
mod cleanup;
pub mod cloud_pool;
pub mod dictionary;
mod failover;
pub mod history;
pub mod live_meeting;
pub mod meeting;
pub mod openrouter;
pub mod output;
pub mod providers;
pub mod summary;
pub mod translate;

pub use cleanup::{CleanupNotice, LateCleanup, LateDocument};

/// Enhancement as a dictation starting now would find it:
/// [`Session::cleanup_now`]. Typed, never words: a trigger turns it into its
/// own wording.
#[derive(Debug, Clone, Copy)]
pub enum CleanupNow<'a> {
    /// Nothing is ticked.
    Off,
    /// Nothing ticked built: one error per ticked choice, in ticked order,
    /// as found when the session opened or cleanup was last replaced.
    Failed(&'a [Error]),
    /// What built cannot serve right now (Apple Intelligence switched off,
    /// or its model not ready). The choices that built, in ticked order.
    /// Carries no skipped errors: Apple Intelligence is only ever a single
    /// choice and never in a pool, so nothing is skipped beside it.
    NotReady(&'a [choice::EnhancerChoice]),
    /// What built will be asked: `built` in ticked order, and the ticked
    /// choices that did not build beside it, one error each.
    Serving {
        built: &'a [choice::EnhancerChoice],
        skipped: &'a [Error],
    },
}
/// Key release to paste with Enhancement off, when nothing is waited for:
/// the paste budget, defined once, in the core, and what the latency
/// report measures a dictation against. With Enhancement on, the paste waits
/// up to [`ENHANCEMENT_WAIT`] instead.
pub use macalla_core::latency::PASTE_BUDGET;

/// How long the paste waits for Enhancement's answer, counted from the final
/// transcript: key release, or a cloud engine's answer.
/// Every provider is held to it -- Apple Intelligence, a
/// server, a cloud model -- and a pool shares it across its members. What has
/// not come back by then is pasted as Tier 0, and reaches History through
/// [`LateCleanup`]. Use-case policy, so it lives here and not beside
/// [`PASTE_BUDGET`], which is a latency measurement.
pub const ENHANCEMENT_WAIT: Duration = Duration::from_secs(5);

/// Where the dictionary is looked for when none is named, relative to the
/// working directory. The repo's seed comes last so a checkout works out of
/// the box.
pub const DICTIONARY_CANDIDATES: &[&str] =
    &["macalla-dictionary.json", "fixtures/dictionary/seed.json"];

/// Seconds of audio the capture ring can hold. The loop drains it every
/// [`POLL`], so this is headroom for a stalled pipeline thread, not a budget.
const RING_SECONDS: f32 = 30.0;

/// How often the loop drains the microphone and asks the engine for text.
/// Half of [`stt::STREAM_CHUNK`], so a push never waits on a full chunk.
const POLL: Duration = Duration::from_millis(50);

/// What can go wrong between a trigger and a document.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The platform's default engine could not be opened. The message says
    /// why; `macalla doctor` has the rest.
    #[error("no usable speech engine: {0}. `macalla doctor` has the detail")]
    NoEngine(#[source] macalla_core::Error),

    /// A cloud session opened without its on-device engine. A file does not
    /// need one; a dictation does, for the live preview while the person
    /// speaks. The message is why that engine could not open.
    #[error("no usable speech engine for the live preview: {0}. `macalla doctor` has the detail")]
    NoPreviewEngine(String),

    #[error(transparent)]
    Core(#[from] macalla_core::Error),

    #[error(transparent)]
    Platform(#[from] macalla_platform::Error),

    /// A dictionary that was named and is not there. A typo in a path must
    /// not silently dictate with no vocabulary.
    #[error("reading the dictionary at {path}: {source}")]
    DictionaryRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("parsing the dictionary at {path}: {source}")]
    DictionaryParse {
        path: PathBuf,
        #[source]
        source: macalla_core::Error,
    },

    #[error("writing the dictionary at {path}: {source}")]
    DictionaryWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A provider id that [`providers::PROVIDERS`] does not list.
    #[error("no cloud provider named {0:?}")]
    UnknownProvider(String),

    /// A provider that does not offer the kind of model being asked for --
    /// ElevenLabs has no text model, Anthropic has no transcription one.
    #[error("{provider} does not offer {kind:?}")]
    WrongKind {
        provider: String,
        kind: providers::Kind,
    },

    /// A provider needs a key and none is stored for it.
    #[error("{provider} needs a key: run `macalla key set {provider}`")]
    NeedsKey { provider: String },

    /// Cloud transcription was asked for in a build with no cloud engines
    /// compiled in.
    #[error("cloud transcription is not compiled into this build")]
    CloudNotCompiledIn,

    /// [`choice::KeySource::Env`] named a variable that is not set. Never
    /// carries a value -- there is nothing to carry when it is unset.
    #[error("${0} is not set")]
    EnvVarMissing(String),

    /// A meeting was stopped before it finished: while it was being heard,
    /// while it was being tidied, or while it was being summarised and the
    /// stop held a part back. Nothing from the stopped work is kept.
    #[error("the meeting was stopped before it finished")]
    Stopped,

    /// [`providers::store_key`] refused a key before it ever reached the
    /// store: empty, only whitespace, or spanning more than one line. `reason`
    /// is plain English -- never the key itself, so this error is always safe
    /// to print or log.
    #[error("the key for {provider} {reason}")]
    InvalidKey {
        provider: String,
        reason: &'static str,
    },

    /// [`openrouter::cleanup_offers`] or [`openrouter::transcription_offers`]
    /// found no cache and the network fetch failed.
    #[error("fetching OpenRouter's model listing: {0}")]
    OpenRouterFetch(String),

    /// [`cloud_pool::pool_offers`] found no cached list for a provider and
    /// the network fetch failed. `provider` is the provider's display name,
    /// as a person sees it on the page ("OpenAI"). `reason` comes from the
    /// provider's adapter, which never puts the key in a message.
    #[error("fetching {provider}'s model list: {reason}")]
    ModelListFetch { provider: String, reason: String },

    /// [`choice::validate_cleanup_choice`] refused a choice before it is
    /// saved: a `Server` choice with an empty address or model, or a `Pool`
    /// choice with something wrong in it. `reason` is plain English, never
    /// UI copy -- a caller turns it into whatever the page shows. Kept apart
    /// from [`Error::CleanupChoiceUnavailable`] because some of these
    /// reasons embed a provider id, which a page must not show, and that one
    /// never does.
    #[error("{0}")]
    InvalidCleanupChoice(String),

    /// [`choice::validate_cleanup_choice`] refused `AppleIntelligence`
    /// because [`crate::choice::AppleIntelligenceAvailability::now`] says it
    /// is not available right now. `reason` is the system's own fixed
    /// explanation (`Availability::explain`) -- already a complete sentence,
    /// safe to show on a page verbatim, and naming no id.
    #[error("{0}")]
    CleanupChoiceUnavailable(&'static str),

    /// [`summary::Summariser::build`] was given no provider. A summary is
    /// asked of the provider chosen for cleanup, and none is chosen.
    #[error("a summary needs a provider: the one chosen for cleanup, and none is chosen")]
    SummaryNeedsProvider,

    /// No provider chosen for the summary could start. The reasons, joined.
    #[error("no provider for the summary could start: {0}")]
    SummaryProviderUnavailable(String),

    /// [`summary::read_transcript`] could not read the file.
    #[error("reading the transcript at {path}: {source}")]
    TranscriptRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The transcript file holds no words to summarise.
    #[error("the transcript at {path} has no words in it")]
    EmptyTranscript { path: PathBuf },

    /// A file named `.vtt` that does not start with the `WEBVTT` header. Read
    /// as plain text it would lose its speakers and times without saying so.
    #[error("{path} is not a WebVTT file: it does not start with WEBVTT")]
    NotWebVtt { path: PathBuf },

    /// An SRT subtitle file, which Macalla does not read yet.
    #[error("{path} is an SRT file, and SRT is not supported yet")]
    SrtNotSupported { path: PathBuf },

    /// A file brought to a meeting could not be opened to see what it holds.
    #[error("reading {path}: {source}")]
    MeetingFileRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A recording was heard to the end and held no words: silence, noise, or
    /// speech the engine could not make out. Said rather than shown as an
    /// empty meeting.
    #[error("nothing was heard in the recording")]
    NothingHeard,

    /// A live meeting on an engine whose two streams cannot share one
    /// instance could not open the second instance it needs. Returned before
    /// either stream opens, so nothing was heard and nothing is held.
    #[error("a second instance of the engine for the meeting's other stream could not open: {0}")]
    SecondEngine(#[source] macalla_core::Error),

    /// One of a live meeting's streams panicked: in its source, its engine,
    /// or the code between them. The meeting ended on both streams and kept
    /// what they had heard, with this as its `ended_early` reason; it is
    /// returned as an error only when nothing at all was heard. Carries the
    /// panic's own message, for the log, or nothing when it had none.
    #[error("a live meeting's stream panicked: {0}")]
    StreamPanicked(String),

    /// The call's audio changed format while a live meeting heard it: the
    /// output device switched to another rate, as a Bluetooth headset does
    /// when a call app opens its microphone. What followed would have been
    /// heard at the wrong speed, so the meeting ended on both streams and
    /// kept what they had heard, with this as its `ended_early` reason; it
    /// is returned as an error only when nothing at all was heard.
    #[error("the call audio's format changed during the live meeting")]
    CallAudioFormatChanged,

    /// The call's audio opened for a live meeting, and then would not start.
    /// Returned before either source is heard, so nothing is held: a meeting
    /// that cannot start its sources does not start.
    #[error("the call audio could not start: {0}")]
    CallAudioStart(#[source] macalla_platform::system_audio::OpenError),
}

pub type Result<T> = std::result::Result<T, Error>;

/// How a session is set up. Plain data, so a settings file, a command line and
/// a test all build it the same way.
#[derive(Debug, Clone)]
pub struct Config {
    /// An engine id from the registry, or `None` for the platform's default.
    /// With [`Config::cloud`] set, this is the on-device engine that shows
    /// the live preview while the person speaks.
    pub engine: Option<String>,
    /// A cloud provider to transcribe through. It gets the whole
    /// recording in one request at key release, and its text is what a
    /// dictation delivers; `engine` still runs during the recording for the
    /// preview. `None` dictates on `engine` alone, as before.
    pub cloud: Option<choice::CloudTranscription>,
    /// BCP-47, for example `en-GB`.
    pub language: String,
    /// A dictionary file to use. `None` looks in [`DICTIONARY_CANDIDATES`];
    /// `Some` of a missing file is an error.
    pub dictionary: Option<PathBuf>,
    /// An input device by name, from `macalla devices`. `None` is the default.
    /// A named device that is not connected when the microphone is armed
    /// gives way to the default until it is back; see [`Session::follow_device`].
    pub device: Option<String>,
    /// The cap on one dictation: a hold-mode key whose release never arrives
    /// must not record until the disk fills. Five minutes, for the CLI and the
    /// app alike. It was 30 s, sized for a terminal and a stuck key, and cut
    /// off a person dictating a long note mid-thought, first in the app and
    /// then at the terminal. A recording that reaches it says so through
    /// [`Progress::Capped`].
    pub max_duration: Duration,
    /// Run the voice-activity gate in front of the engine, so silence never
    /// reaches it and "no speech" is known locally rather than inferred from
    /// empty text. On by default since 2026-09-09, when it was measured on
    /// real dictations with the energy detector (docs/benchmarks.md):
    /// release-to-text moved by nothing outside the take-to-take spread, a
    /// sentence after two seconds of silence passed whole, and a silent hold
    /// was answered in 12 ms without the engine. The neural detector that
    /// replaced it costs 13 to 35 ms of a dictation where energy cost 1 to 7,
    /// all of it spent while recording.
    pub vad_gate: bool,
    /// Tier 1 cleanup to try, in order, as [`choice::cleanup_enhancers`]
    /// gives them. Every one that builds is kept. One is the session's
    /// provider: on this machine it cleans each segment while the person
    /// speaks; off it, it is sent the whole document once, at key release.
    /// Either way the paste waits for it up to [`ENHANCEMENT_WAIT`], and what
    /// has not come back by then is pasted as Tier 0 and reaches History
    /// later. More than one is a cloud pool,
    /// sent the document at key release, the first asked first and the next
    /// taking over when one fails, all inside the same wait. Empty is off,
    /// which it is until the person turns it on. None building does not stop
    /// the session; see [`Session::cleanup_error`].
    pub cleanup: Vec<choice::EnhancerChoice>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: None,
            cloud: None,
            language: "en-US".into(),
            dictionary: None,
            device: None,
            max_duration: Duration::from_secs(300),
            vad_gate: true,
            cleanup: Vec::new(),
        }
    }
}

/// The result of one dictation.
#[derive(Debug)]
pub struct Dictation {
    /// The finished text, corrected and structured.
    pub document: Document,
    /// What the engine produced, partials included and uncorrected. The raw
    /// form is what "add to dictionary" needs, since the user has to recognise
    /// what was misheard.
    pub transcripts: Vec<Transcript>,
    /// Where the time went. `paste_ms` is filled in by [`paste`].
    pub timing: latency::Dictation,
    /// The input device that was listened through, as the OS names it. The
    /// only place this was visible for a day was the log, and "dictation
    /// does not work" is how listening through the wrong microphone
    /// presents.
    pub device: String,
    /// The microphone stopped before the trigger said so, and this is what it
    /// said. Everything captured up to that point is in `document`, which is
    /// the whole point of reporting it here instead of failing: losing a
    /// half-finished sentence because a Bluetooth headset switched modes is
    /// worse than transcribing what arrived and saying what happened.
    pub device_fault: Option<String>,
    /// The engine failed before the trigger said stop, and this is what it
    /// said. Everything it had committed by then is in `document`, for the
    /// same reason a device fault keeps its audio: twenty seconds of speech
    /// are worth more than the error that ended them. Only when nothing was
    /// committed is the error returned instead, because then it is the whole
    /// story. What an engine's earlier results are worth after it fails is
    /// the engine's business; none says they are withdrawn.
    pub engine_fault: Option<macalla_core::Error>,
    /// The device's own complaints during the recording, as distinct from
    /// `timing.overruns`, which is this process falling behind. Non-zero means
    /// a gap in the audio -- a word the engine never had a chance to hear.
    pub stream_warnings: u64,
    /// Whether the gate heard any speech. `None` when the gate did not run,
    /// in which case an empty document is the only evidence and it cannot
    /// tell "nothing was said" from "the engine failed".
    pub heard_speech: Option<bool>,
    /// What became of the request to the cloud engine, when one was in use;
    /// `None` for an on-device session. Anything but
    /// [`CloudOutcome::Transcribed`] and [`paste`] refuses: a slow or failed
    /// cloud transcription is reported and the person decides, rather than
    /// the preview engine's text being pasted for them.
    pub cloud: Option<CloudResult>,
    /// What the on-device preview engine heard, whenever a cloud engine was
    /// in use and whatever became of its request. On
    /// [`CloudOutcome::Transcribed`], `document` and `transcripts` are the
    /// cloud engine's and this is the only copy of the local text; otherwise
    /// it is the same text as `document`. `None` for an on-device session.
    ///
    /// Nothing reads it yet. It is for the step that lets the person choose
    /// what to do with a slow or failed cloud request, where what this
    /// machine heard is one of the choices.
    pub local_document: Option<Document>,
    /// The app in front when the dictation started, by bundle identifier on
    /// macOS, from [`macalla_platform::frontmost_app`], whenever its text
    /// can be ready seconds after key release: with a cloud engine, or with
    /// Enhancement on. `None` where the platform could not say, and for a
    /// local dictation with Enhancement off, which is pasted at release and
    /// never asks. History keeps it as the row's `app_id`, and [`paste`]
    /// compares it with the app in front at paste time before it puts the
    /// text anywhere.
    pub front_app: Option<String>,
    /// Enhancement whose answer is not in `document`: what had not come back
    /// when [`ENHANCEMENT_WAIT`] ran out, or when `cancel` said to stop
    /// waiting ([`Waiting::DeliverNow`], [`Waiting::WithoutEnhancement`]).
    /// [`paste`] never waits for it and never uses it. From a provider on
    /// this machine it is the segments still out; from a remote provider it
    /// is the answer to the one request, sent at key release and not yet
    /// back.
    ///
    /// A trigger that keeps the dictation calls `wait` after pasting, from a
    /// thread that is not serving the next key press, and replaces the History
    /// row's text with what comes back. One that throws the dictation away,
    /// or keeps no History, drops it: a local worker starts no further
    /// segment, a pool asks no further member, and an answer still out goes
    /// nowhere. What was already sent stays sent. `None` with Enhancement
    /// off, when nothing was said, when everything came back in time, and
    /// when the dictation was discarded.
    pub late_cleanup: Option<LateCleanup>,
    /// What became of Enhancement for this dictation; `None` with it off,
    /// when there was nothing to enhance, and when the dictation was
    /// discarded. Typed, for a trigger to say when the paste went without it.
    pub enhancement: Option<EnhancementOutcome>,
    /// The trigger threw this dictation away: `stop` said
    /// [`StopSignal::Discard`], or `cancel` said [`Waiting::GiveUp`] once the
    /// key was up -- while the engine finished, during a cloud request or its
    /// answer, or during the wait for Enhancement. Nothing is to be pasted or
    /// kept. A remote Enhancement provider is sent nothing when the discard
    /// comes before the one ask at key release; a request already out is
    /// abandoned, and its answer goes nowhere. Said here so a trigger never
    /// has to work it out from the rest.
    pub discarded: bool,
}

/// What became of the one request a cloud engine was sent at key release.
#[derive(Debug, PartialEq)]
pub struct CloudResult {
    /// The provider id, as in [`choice::CloudTranscription::provider`].
    pub provider: String,
    pub outcome: CloudOutcome,
    /// From key release, when the request goes out, to this outcome.
    pub ms: f64,
    /// Cloud failures on this [`Session`] in a row, this one counted:
    /// [`CloudOutcome::Failed`] and [`CloudOutcome::TimedOut`] increment it,
    /// [`CloudOutcome::Transcribed`] and [`CloudOutcome::NoSpeech`] reset it
    /// to zero, and [`CloudOutcome::Abandoned`] leaves it as it was -- giving
    /// up on a request says nothing about whether it would have failed. See
    /// [`CloudResult::failing_repeatedly`].
    pub failures_in_a_row: u32,
}

/// How many failures in a row make a pattern worth naming, rather than one
/// request having a bad day.
pub const REPEATED_CLOUD_FAILURES: u32 = 3;

impl CloudResult {
    /// Whether this cloud engine has now failed enough times in a row that
    /// the caller should say so, instead of reporting each failure the same
    /// way it reports the first.
    pub fn failing_repeatedly(&self) -> bool {
        self.failures_in_a_row >= REPEATED_CLOUD_FAILURES
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CloudOutcome {
    /// The provider answered, and [`Dictation::document`] is its text.
    Transcribed,
    /// The request failed. The adapters take keys and credentials out of
    /// their own errors; nothing is added here that could carry one.
    Failed(CloudFailure),
    /// No answer within the session's cap. The request may still finish on
    /// its thread; its answer goes nowhere.
    TimedOut,
    /// The caller gave up, before the request was sent or while waiting.
    Abandoned,
    /// Nothing was heard, so nothing was sent.
    NoSpeech,
}

/// Why a cloud request produced no text: the engine's own typed error when
/// there is one, or free text when there never was one to type -- the worker
/// thread failed to start, the adapter panicked, or its answer channel closed
/// with nothing sent.
#[derive(Debug)]
pub enum CloudFailure {
    /// [`macalla_core::SttEngine::transcribe`]'s own error.
    Transcribe(macalla_core::Error),
    /// Nothing typed produced this: a panic's message, or a line this module
    /// wrote itself about the request never reaching the engine at all.
    Other(String),
}

impl std::fmt::Display for CloudFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transcribe(source) => write!(f, "{source}"),
            Self::Other(reason) => write!(f, "{reason}"),
        }
    }
}

// Manual, not derived: `macalla_core::Error` does not itself implement
// `PartialEq`, so `Transcribe` compares by its rendered text instead -- this
// exists so `CloudOutcome` and `CloudResult` stay comparable by `==` for
// their many payload-less variants (`Transcribed`, `TimedOut`, ...), not
// because any call site compares two `Failed` values against each other
// today.
impl PartialEq for CloudFailure {
    fn eq(&self, other: &Self) -> bool {
        self.to_string() == other.to_string()
    }
}

impl Eq for CloudFailure {}

/// What a caller hears about while a dictation is in progress.
#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    /// The recording went to a cloud engine in one request, and the wait for
    /// its answer begins. Sent once, and only when a request went out and
    /// `cancel` did not say stop as the wait was about to start.
    /// `heard` is the dictation as the on-device engine heard it -- its
    /// document, transcripts, timing and front app -- so a trigger can keep
    /// that text while the person waits, for them to take if the answer is
    /// slow. What becomes of the request is in the [`Dictation`] that
    /// [`Session::dictate`] returns.
    CloudSent {
        /// The provider id, as in [`choice::CloudTranscription::provider`].
        provider: &'a str,
        heard: &'a Dictation,
    },
    /// Which input device is being listened through, as the OS names it.
    /// Sent once, before anything else, so an overlay can say so from the
    /// first frame.
    Device(&'a str),
    /// The loudness of the audio just captured, once per poll. This is the
    /// waveform, and for the first seconds of every dictation it is the only
    /// feedback there is: Apple SpeechTranscriber emits nothing at all for
    /// roughly four seconds, then catches up.
    Level(Level),
    /// A transcript as the engine produced it, partial or final. Preview text
    /// is shown and never pasted; the paste comes from the document.
    Transcript(&'a Transcript),
    /// The recording reached [`Config::max_duration`] with the trigger still
    /// down. What was heard is kept and delivered; anything said after this is
    /// not. Sent once, at that moment, so a person still talking can be told.
    Capped(Duration),
    /// The recording is over, a cloud engine has it, and no answer has come
    /// yet. About once a second until one does, the wait reaches its cap, or
    /// the caller gives up, so nobody is left looking at nothing.
    CloudWaiting {
        /// The provider id, as in [`choice::CloudTranscription::provider`].
        provider: &'a str,
        elapsed: Duration,
    },
    /// The text is ready and the paste is waiting for Enhancement's answer,
    /// up to [`ENHANCEMENT_WAIT`]. About once a second, the first a second
    /// in, so a wait shorter than that shows nothing; until the answer comes,
    /// the wait runs out, or `cancel` stops it. `elapsed` counts from the
    /// final transcript, as the wait does.
    Enhancing {
        /// Who is being waited for; see [`Session::cleanup_enhancing_with`].
        with: &'a EnhancingWith,
        elapsed: Duration,
    },
}

/// What a trigger answers each time the record loop asks whether to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// Keep recording.
    Continue,
    /// The key came up, Enter was pressed, the tray was clicked: deliver what
    /// was said.
    Release,
    /// Esc twice: the recording is thrown away. Nothing more is committed or
    /// sent for cleanup, nothing is waited for, and a remote provider is sent
    /// nothing at all.
    Discard,
}

impl From<bool> for StopSignal {
    /// For a trigger that cannot throw a recording away, such as Enter at a
    /// terminal: `true` releases.
    fn from(stop: bool) -> Self {
        if stop {
            StopSignal::Release
        } else {
            StopSignal::Continue
        }
    }
}

/// What a trigger answers each time the session asks, once the key is up,
/// whether to stop waiting: for the engine to finish, for a cloud engine's
/// answer, or for Enhancement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    /// Keep waiting.
    Continue,
    /// Esc twice: the dictation is thrown away. Nothing is pasted or kept,
    /// nothing more is sent, and an answer still out goes nowhere.
    GiveUp,
    /// Stop waiting on Enhancement and deliver what is ready now, as a trigger
    /// whose next dictation is already asked for does. Nothing more is waited
    /// for: what came back already is used, and the rest is in
    /// [`Dictation::late_cleanup`] for the trigger to keep or drop. Asked as a
    /// remote provider's request is about to go, this still sends it, once,
    /// and hands its answer on the same way -- so a trigger answers this only
    /// when it has somewhere to keep that answer, and
    /// [`Waiting::WithoutEnhancement`] when it has not. The wait for a
    /// transcription is not Enhancement's, and is unaffected: for a cloud
    /// engine's request and its answer this is [`Waiting::Continue`].
    DeliverNow,
    /// Carry on without Enhancement: the person turned a remote provider off,
    /// or removed its key, during this dictation, or the
    /// trigger has nowhere to keep an answer that comes late. Meant
    /// for a remote provider -- a server elsewhere, a cloud model or a pool:
    /// a request not yet sent is never sent, a pool asks no further member,
    /// and an answer still out is not waited for. What came back already is
    /// used, and what is still out is in [`Dictation::late_cleanup`] for the
    /// trigger to keep or drop. It is not a stop for a provider on this
    /// machine: the wait ends, but its worker may still finish the segments
    /// committed by release, the last one included. A cloud engine's request
    /// and its answer are not Enhancement, and for them this is
    /// [`Waiting::Continue`].
    WithoutEnhancement,
}

impl From<bool> for Waiting {
    /// For a trigger whose only answer is whether to give up: `true` gives
    /// up.
    fn from(give_up: bool) -> Self {
        if give_up {
            Waiting::GiveUp
        } else {
            Waiting::Continue
        }
    }
}

impl Waiting {
    /// Whether this answer ends the wait for a cloud engine's request: only
    /// giving up does. The other answers are about Enhancement.
    fn gives_up_on_the_cloud(self) -> bool {
        self == Waiting::GiveUp
    }
}

/// Who a dictation's Enhancement waits for, for a trigger to name. Typed,
/// never words: each app says it in its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnhancingWith {
    /// Apple Intelligence, on this Mac.
    AppleIntelligence,
    /// A server at a loopback address.
    ServerOnThisComputer,
    /// A server anywhere else that is not a provider's own address.
    Server,
    /// A provider, by its id in [`providers::PROVIDERS`]. A pool is named by
    /// its first ticked member that built.
    Provider(String),
}

/// What became of Enhancement for a dictation that had text to enhance
/// ([`Dictation::enhancement`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnhancementOutcome {
    /// Its answer is in the document, including an answer that changed
    /// nothing.
    Applied,
    /// Not everything came back within [`ENHANCEMENT_WAIT`]: the document is
    /// Tier 0 where it had not, and the rest is in
    /// [`Dictation::late_cleanup`]. A dictation on this machine can be partly
    /// enhanced and still be this: the segments that came back in time are
    /// in the document.
    OutOfTime,
    /// It failed, and the document is Tier 0.
    Failed,
    /// It was not asked, or not waited for: it could not serve right now
    /// (Apple Intelligence switched off), or `cancel` said
    /// [`Waiting::DeliverNow`] or [`Waiting::WithoutEnhancement`].
    Skipped,
}

/// What [`paste`] did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Paste {
    /// The document was empty, so nothing was touched -- not the clipboard,
    /// not the focused app.
    Nothing,
    /// A cloud engine was in use and did not transcribe this dictation, so
    /// nothing was touched. Why is in [`Dictation::cloud`].
    Withheld,
    /// The text was ready after a wait -- a cloud engine's answer, or
    /// Enhancement's -- and the app in front now is not the one the person
    /// dictated into, so nothing was touched: text never lands in a window
    /// the person has moved to. The text is in [`Dictation::document`], for
    /// History. Only a dictation with a cloud engine or Enhancement on is
    /// held to this, and only when the app in front is known both when it
    /// started and now; where the platform cannot tell, text pastes as ever.
    MovedOn,
    /// The insertion was attempted, and `how` says what came of it -- pasted,
    /// typed, or [`Inserted::NotDelivered`], which is the platform reporting
    /// that nothing could have arrived and that it touched nothing. The text
    /// is in [`Dictation::document`] for History either way.
    Inserted { how: Inserted, ms: f64 },
}

/// An engine, a dictionary, a pipeline and an armed microphone, ready for the
/// next dictation. One per process; it lives as long as the trigger does.
pub struct Session {
    /// The on-device engine. With a cloud engine as well, it is the live
    /// preview while the person speaks. Shared with a meeting that borrowed
    /// it ([`Session::meeting_tools`]), which keeps it loaded until it ends.
    engine: Arc<dyn SttEngine>,
    /// Shared with a dictation's cleanup worker and its late cleanup, which
    /// can outlive the dictation and this pipeline being replaced.
    pipeline: Arc<Pipeline>,
    context: SttContext,
    /// The cloud engine a dictation's recording goes to at key release,
    /// with the pipeline and context fitted to it.
    cloud: Option<Cloud>,
    /// How long a dictation waits on `cloud`, and how often it says so.
    cloud_wait: Wait,
    /// How long the paste waits on Enhancement, and how often it says so:
    /// [`ENHANCEMENT_WAIT`] and a second, or shorter in a test.
    enhancement_wait: Wait,
    /// Why the on-device engine could not open, in a cloud session that
    /// opened without it. `engine` is then [`NoPreview`], and
    /// [`Session::dictate`] returns this before touching the microphone.
    preview_error: Option<macalla_core::Error>,
    /// Cloud failures in a row across this session's dictations, for
    /// [`CloudResult::failures_in_a_row`]. A `Cell` because `dictate_from`
    /// takes `&self`, not `&mut self`.
    cloud_failures_in_a_row: Cell<u32>,
    /// The recording the last dictation's cloud request carried, when that
    /// request failed or ran out of time, for [`Session::send_again`]. In
    /// memory only, never written anywhere; at most one, and no longer than
    /// [`Config::max_duration`], because it is one dictation's audio. Let go
    /// when the next dictation starts, after a transcription, and with the
    /// session.
    held: RefCell<Option<Arc<AudioSegment>>>,
    /// The cleanup provider that built from [`Config::cleanup`], with where it
    /// sends the text. Shared with each dictation's worker thread. `None`
    /// when cleanup is off or nothing built.
    cleanup: Option<cleanup::Cleanup>,
    /// Why cleanup is not cleaning, as found when the session opened, or
    /// last replaced.
    cleanup_error: Option<CleanupNotice>,
    /// The choices [`Config::cleanup`] named that did not build, when at
    /// least one other one did -- a cloud pool's own skipped members.
    /// [`Session::cleanup_error`] says nothing about these: it is `None`
    /// once anything builds. Empty when nothing was skipped, including
    /// when nothing in [`Config::cleanup`] built at all (that is
    /// [`Session::cleanup_error`]'s `Failed` instead, not a "skipped" pool
    /// member -- there is no pool to have skipped from). Rebuilt by every
    /// [`Session::replace_cleanup`] call, the same one path `open` and every
    /// later rebuild both go through.
    cleanup_skipped: Vec<Error>,
    dictionary_label: String,
    device: Option<String>,
    max_duration: Duration,
    vad_gate: bool,
    armed: Option<Capture>,
    /// Makes the detector each dictation or file is heard through. A maker
    /// rather than a detector, because every run starts with one that has
    /// heard nothing.
    detector: fn() -> Box<dyn SpeechDetector>,
    /// Asks which app is in front as a dictation starts, for
    /// [`Dictation::front_app`]. [`macalla_platform::frontmost_app`]; a test
    /// names an app of its own.
    front_app: fn() -> Option<String>,
}

/// A cloud transcription engine, ready for the next key release.
struct Cloud {
    /// The provider id, for [`CloudResult`] and [`Progress::CloudWaiting`].
    provider: String,
    /// Shared with the thread each request runs on, which can outlive a
    /// dictation that stopped waiting for it.
    engine: Arc<dyn SttEngine>,
    /// Fitted to this engine, not to the preview engine: what a provider
    /// does to punctuation and case is not what Apple Speech does.
    pipeline: Arc<Pipeline>,
    context: SttContext,
}

/// Stands in for the on-device engine in a cloud session that opened
/// without one. Never driven: a file goes to the cloud engine, and
/// [`Session::dictate`] refuses before recording.
struct NoPreview;

impl SttEngine for NoPreview {
    fn id(&self) -> &'static str {
        "none"
    }
    fn caps(&self) -> EngineCaps {
        EngineCaps::batch()
    }
    fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
        Err(macalla_core::Error::Unsupported {
            engine: "none",
            what: "a live preview",
        })
    }
    fn transcribe(
        &self,
        _audio: &AudioSegment,
        _ctx: &SttContext,
    ) -> macalla_core::Result<Transcript> {
        Err(macalla_core::Error::Unsupported {
            engine: "none",
            what: "transcription",
        })
    }
}

/// How long to wait for an answer -- a cloud engine's, or Enhancement's --
/// and how often to report.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Wait {
    pub(crate) tick: Duration,
    pub(crate) cap: Duration,
}

/// How often the paste says it is still waiting for Enhancement: once a
/// second, like [`CLOUD_TICK`], so "(3 s)" counts the way a cloud wait does.
const ENHANCEMENT_TICK: Duration = Duration::from_secs(1);

/// About once a second: often enough that "waiting (3 s)" visibly counts,
/// and each tick is also when the caller is asked whether to give up, so
/// Esc twice takes effect within a second.
const CLOUD_TICK: Duration = Duration::from_secs(1);

/// The cloud adapters in `macalla-engines` give up on a whole request after
/// 60 seconds (`WHOLE_REQUEST` in each), with an error that says why. Five
/// seconds more lets that error arrive first -- the worker still has to be
/// scheduled and the reply read after the transport stops -- so this cap
/// only decides for an adapter that did not keep to its own timeout. Raise
/// it with theirs. Counted from key release, which is when the request goes
/// out.
const CLOUD_CAP: Duration = Duration::from_secs(65);

impl Session {
    /// Open the platform's default engine and find the dictionary.
    ///
    /// Which engine, and whether it can run, are the registry's decisions --
    /// every trigger asks the same question and must get the same answer,
    /// including the same reason when the answer is no. When
    /// [`Config::cloud`] is set, that provider is opened as well, with its
    /// key from [`SystemSecrets`].
    pub fn open(config: Config) -> Result<Self> {
        Self::open_with_keys(config, &SystemSecrets::new())
    }

    /// Like [`Session::open`], but takes the key store explicitly: tests use
    /// [`macalla_platform::secrets::MemoryStore`], and the CLI's
    /// `--key-env` substitutes [`choice::EnvKey`] for one provider. `keys` is
    /// never touched when [`Config::cloud`] is `None` and no choice in
    /// [`Config::cleanup`] reads a stored key.
    ///
    /// A cleanup that cannot start does not stop the session: it opens
    /// without cleanup, and [`Session::cleanup_error`] says why.
    ///
    /// A cloud session opens even when its on-device engine cannot: only a
    /// dictation needs that engine, for the live preview, and a file goes
    /// straight to the cloud engine. [`Session::transcribe`] works, and
    /// [`Session::dictate`] returns [`Error::NoPreviewEngine`] with the
    /// reason. Without a cloud engine, no local engine is still
    /// [`Error::NoEngine`].
    pub fn open_with_keys(config: Config, keys: &dyn SecretStore) -> Result<Self> {
        // The cloud engine first. "Needs a key" is the answer a person can
        // act on whatever this machine's own engine can do, and building one
        // sends nothing.
        let cloud = match &config.cloud {
            Some(cloud) => Some((
                cloud.provider.clone(),
                choice::cloud_engine(cloud, &config.language, keys)?,
            )),
            None => None,
        };
        let id = config
            .engine
            .clone()
            .unwrap_or_else(|| registry::default_engine_id().to_string());
        let engine = match registry::open(&id, &config.language) {
            // No engine and no cloud is no session, said before the
            // dictionary is looked for, as it always was.
            Err(e) if cloud.is_none() => return Err(Error::NoEngine(e)),
            opened => opened,
        };
        let (dictionary, label) =
            load_dictionary(config.dictionary.as_deref(), DICTIONARY_CANDIDATES)?;
        let cleanup = config.cleanup.clone();
        let mut session = Self::from_opened(engine, cloud, dictionary, label, config)?;
        session.replace_cleanup(&cleanup, keys);
        Ok(session)
    }

    /// A session from an on-device engine that opened or failed to, and
    /// perhaps a cloud engine. The rule [`Session::open_with_keys`]
    /// documents, apart from the registry and the key store so it is tested
    /// with fakes.
    fn from_opened(
        local: macalla_core::Result<Box<dyn SttEngine>>,
        cloud: Option<(String, Box<dyn SttEngine>)>,
        dictionary: Dictionary,
        dictionary_label: impl Into<String>,
        config: Config,
    ) -> Result<Self> {
        match (local, cloud) {
            (Ok(engine), cloud) => Ok(Self::with_engines(
                engine,
                cloud,
                dictionary,
                dictionary_label,
                config,
            )),
            (Err(e), None) => Err(Error::NoEngine(e)),
            (Err(e), Some(cloud)) => {
                tracing::info!(
                    error = %e,
                    "no on-device engine for the preview; files still go to the cloud engine"
                );
                let mut session = Self::with_engines(
                    Box::new(NoPreview),
                    Some(cloud),
                    dictionary,
                    dictionary_label,
                    config,
                );
                session.preview_error = Some(e);
                Ok(session)
            }
        }
    }

    /// A session over an engine the caller chose. This is how tests run
    /// without a microphone or a speech model, and how a settings screen
    /// will open an engine other than the default.
    pub fn with_engine(
        engine: Box<dyn SttEngine>,
        dictionary: Dictionary,
        dictionary_label: impl Into<String>,
        config: Config,
    ) -> Self {
        Self::with_engines(engine, None, dictionary, dictionary_label, config)
    }

    /// A session over an on-device engine and, optionally, a cloud engine
    /// with its provider id, both chosen by the caller. [`Config::cloud`] is
    /// not read: `cloud` is the engine it would have built.
    pub fn with_engines(
        engine: Box<dyn SttEngine>,
        cloud: Option<(String, Box<dyn SttEngine>)>,
        dictionary: Dictionary,
        dictionary_label: impl Into<String>,
        config: Config,
    ) -> Self {
        let language = config.language;
        let cloud = cloud.map(|(provider, cloud_engine)| {
            let (pipeline, terms) = fit(cloud_engine.as_ref(), dictionary.clone(), &language);
            Cloud {
                provider,
                engine: Arc::from(cloud_engine),
                pipeline: Arc::new(pipeline),
                context: SttContext {
                    terms,
                    language: Some(language.clone()),
                },
            }
        });
        let (pipeline, terms) = fit(engine.as_ref(), dictionary, &language);
        Self {
            engine: Arc::from(engine),
            pipeline: Arc::new(pipeline),
            context: SttContext {
                terms,
                language: Some(language),
            },
            cloud,
            cloud_wait: Wait {
                tick: CLOUD_TICK,
                cap: CLOUD_CAP,
            },
            enhancement_wait: Wait {
                tick: ENHANCEMENT_TICK,
                cap: ENHANCEMENT_WAIT,
            },
            preview_error: None,
            cloud_failures_in_a_row: Cell::new(0),
            held: RefCell::new(None),
            cleanup: None,
            cleanup_error: None,
            cleanup_skipped: Vec::new(),
            dictionary_label: dictionary_label.into(),
            device: config.device,
            max_duration: config.max_duration,
            vad_gate: config.vad_gate,
            armed: None,
            detector: speech_detector,
            front_app: macalla_platform::frontmost_app,
        }
    }

    /// Take a new dictionary, as after an edit. The pipeline is rebuilt
    /// around it and the engine's vocabulary bias follows, for the cloud
    /// engine too; the engines and the armed microphone are untouched, so
    /// the next dictation is as cheap as it was.
    pub fn replace_dictionary(&mut self, dictionary: Dictionary, label: impl Into<String>) {
        let language = self.context.language.clone().unwrap_or_default();
        if let Some(cloud) = self.cloud.as_mut() {
            let (pipeline, terms) = fit(cloud.engine.as_ref(), dictionary.clone(), &language);
            cloud.pipeline = Arc::new(pipeline);
            cloud.context.terms = terms;
        }
        let (pipeline, terms) = fit(self.engine.as_ref(), dictionary, &language);
        self.pipeline = Arc::new(pipeline);
        self.context.terms = terms;
        self.dictionary_label = label.into();
    }

    /// What a meeting borrows to be heard on this session's on-device engine,
    /// sharing the loaded model rather than loading another
    /// ([`meeting::MeetingTools`]). A cloud session that opened without an
    /// on-device engine has none to lend.
    pub fn meeting_tools(&self) -> Result<meeting::MeetingTools> {
        if let Some(reason) = &self.preview_error {
            return Err(Error::NoPreviewEngine(reason.to_string()));
        }
        // Another instance of the same engine, in the same language, for a
        // live meeting whose engine cannot share one instance between two
        // streams. Nothing is opened here: only a meeting that needs it asks.
        let id = self.engine.id();
        let language = self.context.language.clone().unwrap_or_default();
        Ok(meeting::MeetingTools {
            engine: Arc::clone(&self.engine),
            pipeline: Arc::clone(&self.pipeline),
            context: self.context.clone(),
            detector: self.detector,
            cleanup: self.cleanup.clone(),
            open_another: Arc::new(move || registry::open(id, &language)),
        })
    }

    /// Whether the voice-activity gate runs in front of the engine.
    pub fn vad_gate(&self) -> bool {
        self.vad_gate
    }

    /// Whether [`Session::dictate`] can run. False only for a cloud session
    /// that opened without its on-device engine, which transcribes files and
    /// nothing else; a trigger should treat it as no session.
    pub fn can_dictate(&self) -> bool {
        self.preview_error.is_none()
    }

    /// Why the on-device engine could not open, when [`Session::can_dictate`]
    /// is false. `None` otherwise, including for an on-device session, where
    /// nothing ever sets it.
    pub fn preview_error(&self) -> Option<&macalla_core::Error> {
        self.preview_error.as_ref()
    }

    /// Build every one of `choices` that builds: one provider, or a cloud
    /// pool when more than one does. Nothing building is not an error:
    /// dictation is complete without cleanup, so the
    /// session carries on without it and keeps the reason.
    ///
    /// Replaces only Tier 1 cleanup: the engines, the cloud engine, the
    /// pipeline, the dictionary, the armed microphone and a held recording
    /// (for [`Session::send_again`]) are all untouched. Builds exactly as
    /// [`Session::open_with_keys`] does, through the same [`cleanup::build`],
    /// and building sends nothing.
    ///
    /// [`Session::cleanup_error`] is replaced with why the new choices could
    /// not start or cannot serve yet, if anything -- an earlier error does
    /// not linger once this returns. [`Session::cleanup_skipped`] is rebuilt
    /// the same way, and is empty again when nothing about the new choices
    /// was skipped.
    ///
    /// The old provider is retired ([`cleanup::Cleanup::retire`]): a pool's
    /// late pass asks no further member, and a local late worker still
    /// finishes what it was asked, against the old provider, because cutting
    /// it short would leave that dictation's History half cleaned.
    ///
    /// A provider on this machine is prewarmed now, always -- not only when
    /// the microphone is armed, because changing Enhancement while idle
    /// should not wait for the next arm to pay the cost. A remote one (a
    /// pool) is told nothing until a dictation's key release.
    ///
    /// `&mut self` is deliberate: nothing can call this while a dictation or
    /// [`Session::send_again`] holds `&self` or `&mut self`, so a swap mid-
    /// dictation is impossible by construction, not by a runtime check.
    /// Meeting tools already borrowed from this session ([`Session::meeting_tools`])
    /// keep the provider they were lent until they are re-lent.
    ///
    /// `keys` is read only for a choice that names a stored key -- a server
    /// choice and Apple Intelligence never touch it. Reading a key **can
    /// block on a keychain prompt**; callers on a UI or hotkey thread should
    /// expect that latency, the same as [`Session::open_with_keys`] always
    /// has.
    pub fn replace_cleanup(&mut self, choices: &[choice::EnhancerChoice], keys: &dyn SecretStore) {
        let (cleanup, reason, skipped) = cleanup::build(choices, keys);
        match (&cleanup, &reason) {
            (None, Some(reason)) => {
                tracing::warn!(reason = %reason, "cleanup could not start; dictating without it")
            }
            (Some(_), Some(reason)) => tracing::info!(
                reason = %reason,
                "cleanup cannot serve right now; it is asked again before each segment"
            ),
            _ => {}
        }
        if let Some(old) = self.cleanup.take() {
            old.retire();
        }
        if let Some(cleanup) = &cleanup {
            cleanup.prewarm();
        }
        self.cleanup = cleanup;
        self.cleanup_error = reason;
        self.cleanup_skipped = skipped;
    }

    /// Why cleanup is not cleaning, as found when the session opened, or last
    /// replaced ([`Session::replace_cleanup`]): nothing in
    /// [`Config::cleanup`] built, or what built could not serve yet -- Apple
    /// Intelligence switched off, which is kept anyway and asked again before
    /// every segment. The session dictates either way, and a trigger says so
    /// once. `None` when cleanup is off or ready.
    pub fn cleanup_error(&self) -> Option<&CleanupNotice> {
        self.cleanup_error.as_ref()
    }

    /// The choices [`Config::cleanup`] named that did not build, when at
    /// least one other one did -- a cloud pool's own skipped members, most
    /// often a member whose key has gone. Empty when nothing was skipped:
    /// either everything built, or nothing did, in which case
    /// [`Session::cleanup_error`] is the `Failed` notice instead. Rebuilt by
    /// every [`Session::replace_cleanup`] call, the same one path `open` and
    /// every later rebuild both go through.
    pub fn cleanup_skipped(&self) -> &[Error] {
        &self.cleanup_skipped
    }

    /// Where cleanup sends the text, for the label the privacy rule asks for
    /// while it is on (nothing leaves the machine unless the user turned it on): "Apple Intelligence", "Local" for a server at a
    /// loopback address, a provider's name at that provider's own chat
    /// address whatever the key, "Your server at" the host (never the
    /// address's user, password or port) for any key sent to another address,
    /// "Your server" for another address with no key or
    /// one not plain enough to name, or a cloud pool's providers in ticked
    /// order, at most two by name ("OpenAI, falling back to Anthropic and 1
    /// more"). `None` when cleanup is off or nothing built. Decided by
    /// [`choice::EnhancerChoice::destination`] for a single choice, or
    /// [`cleanup::pool_label`] for a pool.
    pub fn cleanup_destination(&self) -> Option<&str> {
        self.cleanup.as_ref().map(cleanup::Cleanup::destination)
    }

    /// The choices that actually built, in ticked order -- not
    /// [`Config::cleanup`] as it was given. Empty both when cleanup is off
    /// and when every choice failed to build (that case is
    /// [`Session::cleanup_error`]'s `Failed`, not something a caller can
    /// read a partial member list out of). Typed, so a caller that needs
    /// to name what cleanup is really using -- not what was chosen --
    /// reads data, not [`Session::cleanup_destination`]'s words.
    pub fn cleanup_built_choices(&self) -> &[choice::EnhancerChoice] {
        self.cleanup
            .as_ref()
            .map_or(&[], cleanup::Cleanup::built_choices)
    }

    /// Enhancement as a dictation starting right now would find it, typed,
    /// for a trigger that names where the words go. Whether what built can
    /// serve is read live, through the same `is_available()` the cleanup
    /// worker asks before each segment -- Apple Intelligence's system switch
    /// can move after the session opened. Everything else is the stored
    /// snapshot, which only [`Session::open_with_keys`] and
    /// [`Session::replace_cleanup`] change: [`CleanupNow::Failed`] is
    /// [`Session::cleanup_error`]'s `Failed`, and
    /// [`CleanupNow::Serving`]'s `skipped` is [`Session::cleanup_skipped`].
    /// Says nothing about speed: serving means the provider will be asked.
    pub fn cleanup_now(&self) -> CleanupNow<'_> {
        match (&self.cleanup, &self.cleanup_error) {
            (Some(cleanup), _) => {
                let built = cleanup.built_choices();
                if cleanup.is_available_now() {
                    CleanupNow::Serving {
                        built,
                        skipped: &self.cleanup_skipped,
                    }
                } else {
                    CleanupNow::NotReady(built)
                }
            }
            (None, Some(CleanupNotice::Failed(errors))) => CleanupNow::Failed(errors),
            // `NotReady` is only ever stored beside a built provider, so
            // with nothing built there is nothing ticked that could serve.
            (None, _) => CleanupNow::Off,
        }
    }

    /// Who Enhancement waits for, as [`Progress::Enhancing`] names it, for a
    /// trigger that says the paste went without it: the provider that built,
    /// or a pool's first ticked member that did. `None` when Enhancement is
    /// off or nothing built.
    pub fn cleanup_enhancing_with(&self) -> Option<&EnhancingWith> {
        self.cleanup.as_ref().map(cleanup::Cleanup::enhancing_with)
    }

    /// The cloud transcription provider this session sends recordings to,
    /// by id, as in [`choice::CloudTranscription::provider`]; `None` when it
    /// transcribes on this machine. What opened, not what was chosen: a
    /// choice whose key has gone opens a local session instead.
    pub fn cloud_provider(&self) -> Option<&str> {
        self.cloud.as_ref().map(|cloud| cloud.provider.as_str())
    }

    /// The engine a dictation's text comes from: the cloud engine when one is
    /// in use, otherwise the on-device one.
    pub fn engine_id(&self) -> &'static str {
        match &self.cloud {
            Some(cloud) => cloud.engine.id(),
            None => self.engine.id(),
        }
    }

    /// The on-device engine: the live preview's in a cloud session, and the
    /// whole dictation's otherwise. The engine a trigger names when it keeps
    /// [`Progress::CloudSent`]'s `heard` text.
    pub fn local_engine_id(&self) -> &'static str {
        self.engine.id()
    }

    /// The engine whose words are in this dictation's document, for History:
    /// the cloud engine's when it transcribed, and the on-device engine's
    /// otherwise, including when a cloud request failed and the local text
    /// was kept.
    pub fn engine_id_for(&self, dictation: &Dictation) -> &'static str {
        match (&self.cloud, &dictation.cloud) {
            (
                Some(cloud),
                Some(CloudResult {
                    outcome: CloudOutcome::Transcribed,
                    ..
                }),
            ) => cloud.engine.id(),
            _ => self.engine.id(),
        }
    }

    /// Where the dictionary came from and how big it is, for a banner.
    pub fn dictionary_label(&self) -> &str {
        &self.dictionary_label
    }

    /// What the pipeline will actually do for this engine.
    pub fn plan(&self) -> &PipelinePlan {
        self.pipeline.plan()
    }

    /// What the engine is told at the start of every dictation.
    pub fn context(&self) -> &SttContext {
        &self.context
    }

    /// Build the microphone stream now so the next [`Session::dictate`] does
    /// not pay for it.
    ///
    /// Building a cpal input stream is 95% of the cost of opening a
    /// microphone -- 2788.5 ms of 2928 on a cold Bluetooth headset, against
    /// 139.8 ms to start it -- and that cost does not have to sit on the
    /// keypress. Before this the first press on a Bluetooth headset ate the
    /// whole utterance.
    ///
    /// Allowed only because the recording indicator was checked: a stream that
    /// is built and not playing does not light the macOS recording dot
    /// (observed 2026-09-08 against a control that confirmed the dot does
    /// light once the stream plays). The dot must keep meaning "recording",
    /// never "ready"; if that ever stops being true this prewarm goes, not
    /// the indicator.
    ///
    /// Failure is logged, not returned. A device that is missing now may be
    /// back by the time a key is pressed, and refusing to wait for the key
    /// over it would be worse than paying the open cost once.
    pub fn arm(&mut self) {
        if self.armed.is_some() {
            return;
        }
        // The cleanup provider gets ready for the text as the microphone does
        // for the audio, off the keypress. Returns at once.
        if let Some(cleanup) = &self.cleanup {
            cleanup.prewarm();
        }
        match Capture::open(self.device_to_open(), RING_SECONDS) {
            Ok(capture) => self.armed = Some(capture),
            Err(e) => {
                tracing::debug!(error = %e, "could not arm the microphone; will open on demand")
            }
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// Listen through this device from now on, by name, or through the
    /// system default for `None`. The armed microphone is dropped and armed
    /// again, so the next dictation starts on it. Between dictations only: a
    /// dictation stays on the microphone it started with.
    pub fn set_device(&mut self, device: Option<String>) {
        if self.device != device {
            self.device = device;
            self.armed = None;
        }
        self.arm();
    }

    /// Follow the devices while idle. A headset taken off between dictations
    /// moves the default input, and a microphone armed on the old device
    /// would start the next dictation on something that is gone: the stream
    /// dies, the first seconds are lost, and only then does the fail-over
    /// land on the right device. Asking every couple of seconds and
    /// re-arming when the answer moves means the next dictation starts on a
    /// live microphone, and a screen that names the device follows within
    /// that time.
    ///
    /// A device chosen by name is kept while it is connected, whatever the
    /// default does. When it is not, the default stands in, and the chosen
    /// device is taken back as soon as it returns. Returns whether the
    /// microphone was re-armed.
    pub fn follow_device(&mut self) -> bool {
        let chosen = self.device.as_deref();
        let connected = match chosen {
            None => false,
            Some(name) => match macalla_platform::audio::input_device_names() {
                Ok(names) => names.iter().any(|n| n == name),
                // Nothing is known, so nothing moves.
                Err(_) => return false,
            },
        };
        let default_now = macalla_platform::audio::default_input_device_name().ok();
        let wanted = wanted_device(chosen, connected, default_now.as_deref());
        if !needs_rearm(self.device_name(), wanted) {
            return false;
        }
        tracing::info!(
            from = self.device_name().unwrap_or("none"),
            to = wanted.unwrap_or("none"),
            "the input device to use moved; re-arming"
        );
        self.armed = None;
        self.arm();
        true
    }

    /// The name to open the microphone by: the chosen device while it is
    /// connected, otherwise `None`, the system default.
    fn device_to_open(&self) -> Option<&str> {
        let chosen = self.device.as_deref()?;
        let connected = macalla_platform::audio::input_device_names()
            .map(|names| names.iter().any(|n| n == chosen))
            // Listing failed: try the name, and let opening say what is wrong.
            .unwrap_or(true);
        connected.then_some(chosen)
    }

    /// The input device the next dictation will listen through, once the
    /// microphone is armed. `None` before [`Session::arm`] has succeeded:
    /// which device the OS will hand over is not known until it is asked.
    pub fn device_name(&self) -> Option<&str> {
        self.armed.as_ref().map(Capture::device_name)
    }

    /// Record until `stop` says so or the cap is reached, then run the
    /// pipeline over what the engine committed.
    ///
    /// `on_progress` hears the audio level on every poll and every transcript
    /// as it arrives; an overlay draws them, a terminal prints the partials,
    /// a test counts them. The microphone closes when this returns, so the
    /// recording indicator tracks what is actually being listened to. Call
    /// [`Session::arm`] again afterwards, off the keypress, to keep the next
    /// start cheap.
    ///
    /// With a cloud engine, the recording is the same and the
    /// on-device engine still drives the preview. At release the audio the
    /// engine was handed -- only the speech when the gate is on, since
    /// silence is never sent -- goes to the cloud engine in one request on a
    /// worker thread before the preview engine finishes, and this thread
    /// waits, reporting
    /// [`Progress::CloudWaiting`] about once a second and asking `cancel`
    /// each time whether to give up. `cancel` is also asked once before the
    /// request, so a recording the person discarded is never sent, and once
    /// after the answer, so one discarded as it arrived is not cleaned. The
    /// outcome is in [`Dictation::cloud`]. In every session `cancel` is asked
    /// once the engine has finished, too: Esc twice can land while it does,
    /// after `stop` was last asked, and that recording is thrown away all
    /// the same. Whether it was is [`Dictation::discarded`].
    ///
    /// `stop` is asked on every poll. [`StopSignal::Release`] delivers;
    /// [`StopSignal::Discard`] ends the recording, commits and sends nothing
    /// more for cleanup, waits for nothing, and sends a remote provider
    /// nothing at all.
    ///
    /// With Enhancement on ([`Config::cleanup`]) and a provider on this
    /// machine ([`choice::CleanupReach::Local`]), each committed segment goes
    /// to one worker thread as it arrives, and the loop does not wait for it.
    /// A remote provider gets nothing during the recording, and the whole
    /// Tier 0 document once, at key release, unless `cancel` says otherwise
    /// as it is about to go. Either way the paste then waits until
    /// [`ENHANCEMENT_WAIT`] after release -- after the cloud engine's answer,
    /// in a cloud session, whose preview text is never sent -- reporting
    /// [`Progress::Enhancing`] about once a second and asking `cancel` every
    /// few tens of milliseconds, and the document has Tier 0 for anything
    /// still out. What is not in the document is in
    /// [`Dictation::late_cleanup`], what became of Enhancement is in
    /// [`Dictation::enhancement`], and [`Dictation::timing`]'s `tier1_ms` is
    /// how long the wait was.
    pub fn dictate<C: Into<Waiting>>(
        &mut self,
        stop: &mut dyn FnMut() -> StopSignal,
        cancel: &mut dyn FnMut() -> C,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> Result<Dictation> {
        // Before the microphone: a cloud session that opened without its
        // preview engine can transcribe files and nothing else.
        if let Some(reason) = &self.preview_error {
            return Err(Error::NoPreviewEngine(reason.to_string()));
        }
        let capture = match self.armed.take() {
            Some(capture) => capture,
            // Arming failed or was never done. Pay for it now.
            None => Capture::open(self.device_to_open(), RING_SECONDS)?,
        };
        self.dictate_from(capture, stop, cancel, on_progress)
    }

    /// [`Session::dictate`] from any source: the microphone, or a test's.
    fn dictate_from<S: AudioSource, T: Into<StopSignal>, C: Into<Waiting>>(
        &self,
        source: S,
        stop: &mut dyn FnMut() -> T,
        cancel: &mut dyn FnMut() -> C,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> Result<Dictation> {
        let mut asked = || -> Waiting { cancel().into() };
        let cancel: &mut dyn FnMut() -> Waiting = &mut asked;
        // A recording held from the last dictation's failed request goes as
        // this one starts: it is only ever the last dictation's to send again.
        self.held.borrow_mut().take();
        // A fresh gate per dictation, so the detector's adaptation to the
        // room starts over: the last dictation's floor is not this one's.
        let gate = self
            .vad_gate
            .then(|| Gate::new((self.detector)(), VadConfig::default()));
        let setup = Setup {
            engine: self.engine.as_ref(),
            pipeline: &self.pipeline,
            context: &self.context,
            max_duration: self.max_duration,
            detector: self.detector,
            // A cloud session's preview text is never pasted, so it is never
            // sent to be cleaned; the cloud engine's text is, once it answers.
            cleanup: if self.cloud.is_some() {
                None
            } else {
                self.cleanup.as_ref()
            },
            enhancement_wait: self.enhancement_wait,
        };
        // As the person starts: the app they are dictating into is the one in
        // front now. Asked only when the text can be ready seconds after key
        // release -- a cloud engine's answer, or Enhancement's -- so a local
        // dictation with Enhancement off, pasted at release, never asks.
        let front_app = (self.cloud.is_some() || self.cleanup.is_some())
            .then(self.front_app)
            .flatten();
        let Some(cloud) = &self.cloud else {
            let mut dictation = record(&setup, source, gate, None, stop, cancel, on_progress)?;
            dictation.front_app = front_app;
            return Ok(dictation);
        };
        // What the record loop keeps for the request is bounded by the
        // recording cap: 16 kHz of f32 is 3.84 MB a minute, 19.2 MB at the
        // five-minute default.
        let mut sent = None;
        let mut local = {
            let mut at_release = |released: Released| {
                sent = Some(send(cloud, released, &mut *cancel));
            };
            record(
                &setup,
                source,
                gate,
                Some(&mut at_release),
                stop,
                // Nothing is waited for here: the preview's text is never
                // cleaned, and `cancel` is the request's to ask.
                &mut || false,
                on_progress,
            )?
        };
        // The loop always reaches release before it returns a dictation, so
        // this stands in for something that does not happen.
        let sent = sent.unwrap_or_else(|| Sent {
            release: Instant::now(),
            request: Request::Unsent(CloudOutcome::Failed(CloudFailure::Other(
                "the recording ended before key release".into(),
            ))),
        });
        local.front_app = front_app;
        Ok(self.finish_in_the_cloud(cloud, local, sent, cancel, on_progress))
    }

    /// A dictation from audio that already exists -- a file, a fixture --
    /// down whichever path the engine's capabilities call for. Samples are
    /// 16 kHz mono, the core's contract.
    ///
    /// In a cloud session the file goes to the cloud engine, on this thread:
    /// a file has no preview to show, and `macalla transcribe --cloud` is how
    /// a provider is tried.
    pub fn transcribe(&self, samples: Vec<f32>) -> Result<Dictation> {
        let (engine, pipeline, context) = match &self.cloud {
            Some(cloud) => (cloud.engine.as_ref(), &cloud.pipeline, &cloud.context),
            None => (self.engine.as_ref(), &self.pipeline, &self.context),
        };
        let started = Instant::now();
        let audio = AudioSegment::new(samples, 0);
        // A batch engine gets the file the way it gets a dictation: cut at
        // the pauses, one utterance at a time, and no piece longer than the
        // VAD's segment cap however long someone talks without pausing.
        // Handed a minute of audio in one call, Parakeet dropped a sentence
        // and sherpa-onnx dropped the tail; handed a whole 13-minute reading,
        // Parakeet failed, and an hour crashed it (docs/benchmarks.md). An
        // engine that segments for itself takes the whole file.
        let transcripts = if engine.caps().owns_segmentation {
            stt::transcribe_segment(engine, &audio, context)?
        } else {
            // One segment at a time: each is cut as the gate reaches its end,
            // transcribed, and dropped before the next is cut, so a long file
            // is never held a second time as a list of segments.
            let mut vad = Vad::new((self.detector)(), VadConfig::default());
            let (mut segments, mut ms) = (0usize, 0u64);
            let mut all = Vec::new();
            for segment in vad.segments(&audio) {
                segments += 1;
                ms += segment.duration_ms();
                all.extend(stt::transcribe_segment(engine, &segment, context)?);
            }
            tracing::debug!(
                segments,
                ms,
                of_ms = audio.duration_ms(),
                "file cut at the pauses"
            );
            all
        };
        let stt_ms = millis_since(started);

        let assembling = Instant::now();
        let document = pipeline.assemble(&transcripts);
        let tier0_ms = millis_since(assembling);

        Ok(Dictation {
            document,
            transcripts,
            timing: latency::Dictation {
                audio_ms: audio.duration_ms(),
                open_ms: None,
                engine_ms: None,
                vad_ms: None,
                stt_ms: Some(stt_ms),
                // A file's request is its transcription, timed above; there
                // is no key release to send one at.
                cloud_ms: None,
                tier0_ms: Some(tier0_ms),
                tier1_ms: None,
                paste_ms: None,
                total_ms: millis_since(started),
                overruns: 0,
            },
            // Audio that already existed was not listened through anything.
            device: String::new(),
            device_fault: None,
            engine_fault: None,
            stream_warnings: 0,
            heard_speech: None,
            cloud: None,
            local_document: None,
            front_app: None,
            late_cleanup: None,
            enhancement: None,
            discarded: false,
        })
    }
}

/// The voice-activity gate as the record loop uses it: hands the engine the
/// pre-roll and the speech, and nothing else.
///
/// The gate decides per 32 ms frame and reports where speech began, which is
/// earlier than the moment it was sure, so a short ring of recent audio is
/// kept to reach back into. Once open, everything is forwarded until the
/// gate closes, hangover included: the hangover is the gate's patience with a
/// pause, and cutting it out of what the engine hears would cut the pause
/// out of the punctuation.
struct Gate {
    vad: Vad<Box<dyn SpeechDetector>>,
    recent: std::collections::VecDeque<f32>,
    /// Samples pushed so far; the clock the gate's events are read against.
    seen: u64,
    /// Everything before this sample has been handed on already. A start can
    /// name earlier audio than that: a pre-roll reaching back into a hangover
    /// the engine just heard, or a cut at the segment cap, which lands at the
    /// quietest moment behind it. Either way the engine hears a sample once.
    forwarded: u64,
    forwarding: bool,
    spent: Duration,
}

/// One second at 16 kHz: more than the pre-roll and the longest run the gate
/// needs before it is sure, with a poll or two to spare.
const RECENT_SAMPLES: usize = SAMPLE_RATE as usize;

impl Gate {
    fn new(detector: Box<dyn SpeechDetector>, config: VadConfig) -> Self {
        Self {
            vad: Vad::new(detector, config),
            recent: std::collections::VecDeque::with_capacity(RECENT_SAMPLES),
            seen: 0,
            forwarded: 0,
            forwarding: false,
            spent: Duration::ZERO,
        }
    }

    /// Feed a batch; `out` receives what the engine should hear from it.
    fn filter(&mut self, batch: &[f32], out: &mut Vec<f32>) {
        let started = Instant::now();
        out.clear();
        let batch_start = self.seen;
        self.recent.extend(batch.iter().copied());
        while self.recent.len() > RECENT_SAMPLES {
            self.recent.pop_front();
        }
        self.seen += batch.len() as u64;

        let mut from = if self.forwarding {
            Some(batch_start)
        } else {
            None
        };
        for event in self.vad.push(batch) {
            match event {
                VadEvent::SpeechStart { at_ms } => {
                    from = Some(samples_at(at_ms));
                    self.forwarding = true;
                }
                VadEvent::SpeechEnd { at_ms } => {
                    if let Some(start) = from.take() {
                        self.copy(start, samples_at(at_ms).max(batch_start), out);
                    }
                    self.forwarding = false;
                }
            }
        }
        if let Some(start) = from {
            self.copy(start, self.seen, out);
        }
        self.spent += started.elapsed();
    }

    /// Append samples `[from, to)` of everything seen, as far back as the
    /// ring still holds and never any already handed on.
    fn copy(&mut self, from: u64, to: u64, out: &mut Vec<f32>) {
        let base = self.seen - self.recent.len() as u64;
        let from = from.max(base).max(self.forwarded);
        if to <= from {
            return;
        }
        out.extend(
            self.recent
                .range((from - base) as usize..(to - base) as usize)
                .copied(),
        );
        self.forwarded = to;
    }

    fn heard_speech(&self) -> bool {
        self.vad.heard_speech()
    }
}

fn samples_at(ms: u64) -> u64 {
    ms * SAMPLE_RATE as u64 / 1_000
}

/// Put the text where the user was typing, and record how long it took.
///
/// An empty document touches nothing: not the clipboard, not the focused app.
/// A synthetic paste of nothing still costs the user their clipboard contents
/// for the restore wait, and still sends a chord into whatever has focus.
///
/// A dictation whose cloud engine did not transcribe it touches nothing
/// either, whatever the on-device engine heard. The person is told the
/// request was slow or failed and decides; pasting the preview text in its
/// place would decide for them. Refused here, so the CLI and the app cannot
/// disagree about it.
///
/// Text that was waited for -- a cloud engine's answer, or Enhancement's --
/// is pasted only into the app the person dictated into: the wait can take
/// seconds, and by then they may be typing somewhere else. When the app in
/// front now is not [`Dictation::front_app`], nothing is touched and this
/// says [`Paste::MovedOn`]. Where either answer is unknown -- Linux and
/// Windows for now -- it pastes as it always has. A local dictation with
/// Enhancement off is pasted at release, before anyone can move, and is not
/// asked.
pub fn paste(dictation: &mut Dictation) -> Result<Paste> {
    paste_with(dictation, macalla_platform::paste::system_inserter)
}

/// [`paste`], with the inserter's opening handed in.
///
/// Split out for the tests: opening the real one asks TCC and then holds the
/// clipboard and an input device, so a test that called it would either post
/// keystrokes into whichever window has focus or answer differently on every
/// machine. `open` fails in a test the way the OS fails in the app, and what
/// this does with each failure is the behaviour under test.
fn paste_with<F, C, K, T, G>(dictation: &mut Dictation, open: F) -> Result<Paste>
where
    F: FnOnce() -> macalla_platform::Result<macalla_platform::paste::Inserter<C, K, T, G>>,
    C: macalla_platform::paste::Clipboard,
    K: macalla_platform::paste::Keystrokes,
    T: macalla_platform::paste::Clock,
    G: macalla_platform::paste::DeliveryGates,
{
    if let Some(refused) = refused(dictation, macalla_platform::frontmost_app) {
        return Ok(refused);
    }
    let text = dictation.document.to_plain();
    // The attempt, which starts with a permission query: `system_inserter`
    // asks TCC before it opens anything, and a refusal is timed from here so
    // the log says how long finding out took.
    let attempt = Instant::now();
    let mut inserter = match open() {
        Ok(inserter) => inserter,
        Err(e) => match not_delivered(&e) {
            // The commonest way a paste does not happen, and the whole point
            // of the typed reason: without the grant the constructor refuses
            // before an inserter exists, so the gate inside `insert` never
            // gets a turn. Answered the same way either way, so both apps say
            // the one sentence and neither has to read an error's text.
            Some(reason) => {
                let how = Inserted::NotDelivered(reason);
                let ms = millis_since(attempt);
                dictation.timing.paste_ms = timed(how, ms);
                return Ok(Paste::Inserted { how, ms });
            }
            None => return Err(e.into()),
        },
    };
    // Timed from here and not from `attempt`: the TCC round trip inside the
    // constructor is a permission query, not part of putting the text in, and
    // `paste_ms` is the paste figure in the latency breakdown.
    let started = Instant::now();
    let how = inserter.insert(&text)?;
    let ms = millis_since(started);
    dictation.timing.paste_ms = timed(how, ms);
    Ok(Paste::Inserted { how, ms })
}

/// The typed reason behind a failure to open the inserter, or `None` when it
/// is a failure and not a refusal.
///
/// Only the Accessibility grant is a refusal: nothing was touched, the text
/// is still the caller's, and there is something the person can do. A
/// clipboard that would not open, or a missing input device, is a fault and
/// stays an error.
fn not_delivered(e: &macalla_platform::Error) -> Option<macalla_platform::paste::NotDelivered> {
    use macalla_platform::paste::NotDelivered;
    match e {
        macalla_platform::Error::Permission {
            permission: macalla_platform::Permission::Accessibility,
        } => Some(NotDelivered::NotTrustedForAccessibility),
        _ => None,
    }
}

/// What the latency breakdown keeps of a paste: how long it took, or `None`
/// when nothing was inserted.
///
/// A refusal touched nothing and returned in well under a millisecond. Kept,
/// that sample would pull the paste breakdown down, and the breakdown is how
/// the speed claim is proved. [`Paste::MovedOn`] leaves
/// it `None` for the same reason, by never reaching here.
fn timed(how: Inserted, ms: f64) -> Option<f64> {
    match how {
        Inserted::NotDelivered(_) | Inserted::Nothing => None,
        Inserted::PastedAndRestored | Inserted::PastedAndKept | Inserted::Typed => Some(ms),
    }
}

/// What [`paste`] answers without touching anything, or `None` to paste.
/// `front_app_now` is asked only when a front app was recorded -- a cloud
/// engine, or Enhancement on -- so a local dictation with Enhancement off
/// never pays for the question.
fn refused(dictation: &Dictation, front_app_now: impl FnOnce() -> Option<String>) -> Option<Paste> {
    if let Some(cloud) = &dictation.cloud {
        if cloud.outcome != CloudOutcome::Transcribed {
            return Some(Paste::Withheld);
        }
    }
    if dictation.document.to_plain().trim().is_empty() {
        return Some(Paste::Nothing);
    }
    let then = dictation.front_app.as_deref()?;
    match front_app_now() {
        Some(now) if now != then => Some(Paste::MovedOn),
        // Still in front, or it cannot be told now.
        _ => None,
    }
}

/// Find and parse the dictionary.
///
/// A named file that is missing is an error -- a typo must not silently
/// dictate with no vocabulary. A missing default is not: a first run has no
/// dictionary and must still work. Returns the dictionary and a label saying
/// where it came from.
pub fn load_dictionary(
    explicit: Option<&Path>,
    candidates: &[&str],
) -> Result<(Dictionary, String)> {
    let chosen = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => candidates.iter().map(PathBuf::from).find(|p| p.exists()),
    };
    let Some(path) = chosen else {
        return Ok((Dictionary::from_json("{}")?, "none".into()));
    };
    let json = std::fs::read_to_string(&path).map_err(|source| Error::DictionaryRead {
        path: path.clone(),
        source,
    })?;
    let dictionary = Dictionary::from_json(&json).map_err(|source| Error::DictionaryParse {
        path: path.clone(),
        source,
    })?;
    let label = format!("{} ({} terms)", path.display(), dictionary.len());
    Ok((dictionary, label))
}

/// What counts as speech, for the live gate, a batch engine's cutting and a
/// file alike: one place, so the three cannot disagree. A neural detector
/// rather than the core's energy one, which passed loud noise that a Whisper
/// engine then wrote words over (docs/benchmarks.md).
fn speech_detector() -> Box<dyn SpeechDetector> {
    Box::new(macalla_engines::vad::Earshot::new())
}

/// An audio file as 16 kHz mono, for a transcription. WAV and AIFF are read
/// natively; on macOS the system decoder takes mp3, m4a and the rest without
/// any tool; everywhere, `ffmpeg` on the PATH takes whatever it knows. The
/// order is a composition decision, which is why it lives here and not in
/// either adapter.
pub fn decode_file(path: &std::path::Path) -> Result<Vec<f32>> {
    match macalla_platform::decode::decode_file(path) {
        Ok(samples) => Ok(samples),
        Err(platform_err) => {
            #[cfg(target_os = "macos")]
            {
                if let Ok(samples) = macalla_engines::apple::decode_file(path) {
                    return Ok(samples);
                }
            }
            Err(Error::Platform(platform_err))
        }
    }
}

/// The microphone as the record loop sees it. `Capture` is the only real one;
/// the port exists so the loop's rules are tested without a device.
trait AudioSource {
    fn device_name(&self) -> &str;
    fn play(&mut self) -> macalla_platform::Result<()>;
    fn poll_into(&mut self, out: &mut Vec<f32>) -> macalla_platform::Result<usize>;
    fn finish_into(self, out: &mut Vec<f32>) -> macalla_platform::Result<usize>;
    fn overruns(&self) -> u64;
    fn stream_warnings(&self) -> u64;
}

impl AudioSource for Capture {
    fn device_name(&self) -> &str {
        Capture::device_name(self)
    }
    fn play(&mut self) -> macalla_platform::Result<()> {
        Capture::play(self)
    }
    fn poll_into(&mut self, out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
        Capture::poll_into(self, out)
    }
    fn finish_into(self, out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
        Capture::finish_into(self, out)
    }
    fn overruns(&self) -> u64 {
        Capture::overruns(self)
    }
    fn stream_warnings(&self) -> u64 {
        Capture::stream_warnings(self)
    }
}

/// What a recording runs against: the parts of a session that do not change
/// between dictations.
struct Setup<'a> {
    engine: &'a dyn SttEngine,
    pipeline: &'a Arc<Pipeline>,
    context: &'a SttContext,
    max_duration: Duration,
    /// Makes the detector a batch engine's audio is cut by.
    detector: fn() -> Box<dyn SpeechDetector>,
    /// Cleanup for the committed segments, when set.
    cleanup: Option<&'a cleanup::Cleanup>,
    /// How long the paste waits for it at release.
    enhancement_wait: Wait,
}

/// The record loop. Takes an ARMED source -- built, not yet playing -- so
/// `open_ms` measures what the trigger actually pays.
///
/// `at_release`, given in a cloud session, is handed at key release exactly
/// what the engine was handed, in order -- with a gate, the speech with its
/// pre-roll and hangovers; without one, everything captured -- before the
/// preview engine is asked to finish, so the cloud request is out while it
/// does. In a cloud session the engine here is only a preview:
/// when it fails, the recording carries on without it for the cloud
/// request, and the failure is reported beside the dictation, not returned.
///
/// `stop` saying [`StopSignal::Discard`] reaches the tail as well: nothing
/// the engine commits after it goes to cleanup, a cloud request is not sent,
/// and nothing is waited for. `cancel` is asked once the engine has
/// finished, and during the release wait for cleanup, so Esc twice after
/// `stop` was last asked is a discard too.
fn record<S: AudioSource, T: Into<StopSignal>, C: Into<Waiting>>(
    setup: &Setup<'_>,
    mut source: S,
    mut gate: Option<Gate>,
    mut at_release: Option<&mut dyn FnMut(Released)>,
    stop: &mut dyn FnMut() -> T,
    cancel: &mut dyn FnMut() -> C,
    on_progress: &mut dyn FnMut(Progress<'_>),
) -> Result<Dictation> {
    let device = source.device_name().to_string();
    on_progress(Progress::Device(&device));
    let opened = Instant::now();
    source.play()?;
    let open_ms = millis_since(opened);
    // Timed apart from the device. Both are paid on the keypress, but only
    // one is what the prewarm changes, and lumping them together once hid a
    // 31 ms device start inside a 268 ms figure labelled "device open".
    let engine_opened = Instant::now();
    let mut stream = stt::open_stream(setup.engine, setup.context, (setup.detector)())?;
    let engine_ms = millis_since(engine_opened);
    // Each final transcript becomes its segment here as it commits and, with
    // cleanup on, is handed to the worker without waiting.
    let mut assembly = cleanup::Assembly::new(Arc::clone(setup.pipeline), setup.cleanup.cloned());

    let deadline = Instant::now() + setup.max_duration;
    let mut transcripts: Vec<Transcript> = Vec::new();
    let mut samples = Vec::new();
    let mut heard = Vec::new();
    let mut total_samples = 0usize;
    let mut device_fault = None;
    let mut engine_fault: Option<macalla_core::Error> = None;
    // What a cloud request will carry, kept only when there is one.
    let mut kept: Option<Vec<f32>> = at_release.is_some().then(Vec::new);

    // Not `?` on the engine either. A push or a poll that fails ends the
    // recording and is reported beside what was committed, the way a dead
    // microphone is; the first failure is the one kept.
    let engine = |result: macalla_core::Result<()>, fault: &mut Option<macalla_core::Error>| {
        if let Err(e) = result {
            tracing::warn!(error = %e, "the engine failed mid-recording");
            fault.get_or_insert(e);
        }
    };

    // Whether the trigger threw the recording away.
    let mut discarded = false;
    loop {
        match stop().into() {
            StopSignal::Continue => {}
            StopSignal::Release => break,
            StopSignal::Discard => {
                discarded = true;
                break;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        // Whether the engine is still being fed. It stops at the engine's
        // first fault, which ends a local recording below; a cloud recording
        // goes on without it, for the cloud request.
        let feeding = engine_fault.is_none();
        samples.clear();
        // Not `?`. `poll_into` drains the ring BEFORE it reports a dead
        // device, so `samples` holds the last audio the microphone produced.
        let fault = source.poll_into(&mut samples).err();
        if !samples.is_empty() {
            total_samples += samples.len();
            // The level is what was captured, gate or no gate: the waveform
            // is feedback that the microphone hears, not that the engine does.
            on_progress(Progress::Level(level(&samples)));
            match gate.as_mut() {
                Some(gate) => {
                    gate.filter(&samples, &mut heard);
                    if !heard.is_empty() {
                        keep(&mut kept, &heard);
                        if feeding {
                            engine(stream.push(&heard), &mut engine_fault);
                        }
                    }
                }
                None => {
                    keep(&mut kept, &samples);
                    if feeding {
                        engine(stream.push(&samples), &mut engine_fault);
                    }
                }
            }
        }
        if feeding {
            match stream.poll() {
                Ok(polled) => {
                    for t in polled {
                        on_progress(Progress::Transcript(&t));
                        assembly.commit(&t);
                        transcripts.push(t);
                    }
                }
                Err(e) => engine(Err(e), &mut engine_fault),
            }
        }
        if let Some(e) = fault {
            tracing::warn!(error = %e, "the microphone stopped mid-recording");
            device_fault = Some(e.to_string());
            break;
        }
        // A local recording has nothing left to record for.
        if engine_fault.is_some() && kept.is_none() {
            break;
        }
        std::thread::sleep(POLL);
    }

    // === RELEASE. Everything after this is what the user waits for. ===
    let release = Instant::now();
    if release >= deadline {
        // The cap is for a key whose release never arrives. When a person
        // simply spoke that long, what was heard is kept and delivered; only
        // the tail after the cap is lost. The caller is told as well as the
        // log: the key is usually still down, and a log line is no use to
        // someone still talking.
        tracing::warn!(
            cap_s = setup.max_duration.as_secs(),
            "the dictation hit the time cap; keeping what was heard"
        );
        on_progress(Progress::Capped(setup.max_duration));
    }
    let overruns = source.overruns();
    let stream_warnings = source.stream_warnings();
    samples.clear();
    // Same reasoning as the poll above: keep the tail, then report.
    if let Err(e) = source.finish_into(&mut samples) {
        tracing::warn!(error = %e, "the microphone stopped while finishing");
        device_fault.get_or_insert_with(|| e.to_string());
    }
    if !samples.is_empty() && (engine_fault.is_none() || kept.is_some()) {
        total_samples += samples.len();
        let feeding = engine_fault.is_none();
        match gate.as_mut() {
            Some(gate) => {
                gate.filter(&samples, &mut heard);
                if !heard.is_empty() {
                    keep(&mut kept, &heard);
                    if feeding {
                        engine(stream.push(&heard), &mut engine_fault);
                    }
                }
            }
            None => {
                keep(&mut kept, &samples);
                if feeding {
                    engine(stream.push(&samples), &mut engine_fault);
                }
            }
        }
    }
    // The cloud request goes out now, before the preview engine finishes:
    // the person is already waiting.
    if let (Some(at_release), Some(kept_samples)) = (at_release.as_deref_mut(), kept.take()) {
        at_release(Released {
            samples: kept_samples,
            heard_speech: gate.as_ref().map(Gate::heard_speech),
            at: release,
            discarded,
        });
    }
    // Finish even after a failure: whatever the engine still holds is worth
    // asking for, and a second error adds nothing to the first.
    let finished = stream.finish();
    // Finishing can take a moment, and `stop` is not asked again. Esc twice
    // during it is heard here, before the tail goes to cleanup and before a
    // remote provider could be handed the document.
    if !discarded && cancel().into() == Waiting::GiveUp {
        discarded = true;
    }
    match finished {
        Ok(finished) => {
            for t in finished {
                on_progress(Progress::Transcript(&t));
                // The tail of a discarded recording is never sent.
                if !discarded {
                    assembly.commit(&t);
                }
                transcripts.push(t);
            }
        }
        Err(e) => engine(Err(e), &mut engine_fault),
    }
    // In a cloud session the recording went to the cloud engine whatever the
    // preview did, so its failure is reported beside the dictation instead.
    if engine_fault.is_some() && !transcripts.iter().any(|t| t.is_final) && at_release.is_none() {
        // Nothing was committed, so there is nothing to keep and the error
        // is the whole story.
        if let Some(e) = engine_fault.take() {
            return Err(Error::Core(e));
        }
    }
    let stt_ms = millis_since(release);

    // A discard sends and waits for nothing more. Otherwise this waits for
    // Enhancement, when it is on, until the wait after release and not a
    // moment longer.
    let assembled = if discarded {
        assembly.discard()
    } else {
        assembly.finish(
            release,
            setup.enhancement_wait,
            &mut || cancel().into(),
            on_progress,
        )
    };

    Ok(Dictation {
        document: assembled.document,
        transcripts,
        timing: latency::Dictation {
            audio_ms: (total_samples as u64 * 1_000) / SAMPLE_RATE as u64,
            open_ms: Some(open_ms),
            engine_ms: Some(engine_ms),
            vad_ms: gate.as_ref().map(|g| g.spent.as_secs_f64() * 1000.0),
            // The wait at release. None when nothing was waited for, rather
            // than a zero that would read as instant cleanup.
            tier1_ms: assembled.tier1_ms,
            stt_ms: Some(stt_ms),
            // Filled in after the recording, when a cloud engine has it.
            cloud_ms: None,
            // The per-segment steps during the recording and the finishing
            // step at release, together.
            tier0_ms: Some(assembled.tier0_ms),
            paste_ms: None,
            total_ms: millis_since(release),
            overruns,
        },
        device,
        device_fault,
        engine_fault,
        stream_warnings,
        heard_speech: gate.as_ref().map(Gate::heard_speech),
        cloud: None,
        local_document: None,
        front_app: None,
        late_cleanup: assembled.late,
        enhancement: assembled.enhancement,
        discarded: discarded || assembled.discarded,
    })
}

/// Add what the engine was just handed to what a cloud request will carry.
fn keep(kept: &mut Option<Vec<f32>>, samples: &[f32]) {
    if let Some(kept) = kept {
        kept.extend_from_slice(samples);
    }
}

/// The pipeline an engine calls for, and the dictionary terms it is sent.
///
/// The terms are the seam the dictionary crosses to reach the engine. An
/// engine that can bias towards vocabulary gets the written forms; one that
/// cannot gets nothing, and the phonetic layer in the pipeline catches its
/// mishearings afterwards. For a long time the field, the accessor and the
/// capability flag all existed and nothing joined them.
fn fit(engine: &dyn SttEngine, dictionary: Dictionary, language: &str) -> (Pipeline, Vec<String>) {
    let caps = engine.caps();
    let text = TextCaps::for_engine_id(engine.id());
    let terms = if caps.vocab_bias {
        dictionary
            .written_forms()
            .into_iter()
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    (
        Pipeline::new(&caps, &text, dictionary, Some(language)),
        terms,
    )
}

/// What the record loop hands a cloud request at key release.
struct Released {
    /// Exactly what the engine was handed during the recording.
    samples: Vec<f32>,
    heard_speech: Option<bool>,
    /// Key release, which every cloud figure is timed from.
    at: Instant,
    /// The trigger threw the recording away, so nothing is sent.
    discarded: bool,
}

/// A cloud request as it stood at key release.
struct Sent {
    release: Instant,
    request: Request,
}

enum Request {
    /// Never sent, and why.
    Unsent(CloudOutcome),
    /// Out on its worker thread. The answer, or the reason there is none,
    /// arrives on `answered`. `audio` is what was sent, shared with that
    /// thread rather than copied, for a failed request to be held.
    Out {
        answered: mpsc::Receiver<std::result::Result<Transcript, CloudFailure>>,
        audio: Arc<AudioSegment>,
    },
}

/// At key release in a cloud session: send what was kept to the cloud engine
/// in one request on a worker thread, unless the caller has given
/// up, so a recording discarded while held never leaves the machine, or
/// nothing was heard. Silence is never sent: the gate is a cost control as
/// well as a speed one.
fn send(cloud: &Cloud, released: Released, cancel: &mut dyn FnMut() -> Waiting) -> Sent {
    let request = if released.discarded || cancel().gives_up_on_the_cloud() {
        Request::Unsent(CloudOutcome::Abandoned)
    } else if released.heard_speech == Some(false) || released.samples.is_empty() {
        Request::Unsent(CloudOutcome::NoSpeech)
    } else {
        spawn_request(cloud, Arc::new(AudioSegment::new(released.samples, 0)))
    };
    Sent {
        release: released.at,
        request,
    }
}

/// Start the one request on a thread of its own. Its answer goes down the
/// channel, or nowhere if the wait has ended by then.
fn spawn_request(cloud: &Cloud, audio: Arc<AudioSegment>) -> Request {
    let (answer, answered) = mpsc::channel();
    let engine = Arc::clone(&cloud.engine);
    let context = cloud.context.clone();
    let sending = Arc::clone(&audio);
    let spawned = std::thread::Builder::new()
        .name("macalla-cloud".into())
        .spawn(move || {
            // An adapter that panics still says why, instead of leaving the
            // wait with nothing but a closed channel.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                engine.transcribe(&sending, &context)
            }));
            let _ = answer.send(match result {
                Ok(transcribed) => transcribed.map_err(CloudFailure::Transcribe),
                Err(payload) => Err(CloudFailure::Other(panic_reason(&*payload))),
            });
        });
    match spawned {
        Ok(_) => Request::Out { answered, audio },
        Err(e) => Request::Unsent(CloudOutcome::Failed(CloudFailure::Other(format!(
            "could not start the request: {e}"
        )))),
    }
}

/// What a panic said, when it said it in words.
fn panic_reason(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "the cloud engine stopped without saying why".to_string()
    }
}

/// Wait for the answer, up to the cap counted from key release. This thread
/// never runs the engine: it ticks, reports and asks whether to give up. A
/// request that outlives the wait is left to finish on its thread, where the
/// adapter's own timeout ends it, and its answer goes nowhere.
fn wait_for(
    cloud: &Cloud,
    wait: Wait,
    answered: mpsc::Receiver<std::result::Result<Transcript, CloudFailure>>,
    release: Instant,
    cancel: &mut dyn FnMut() -> Waiting,
    on_progress: &mut dyn FnMut(Progress<'_>),
) -> (CloudOutcome, Option<Transcript>) {
    loop {
        let left = wait.cap.saturating_sub(release.elapsed());
        match answered.recv_timeout(wait.tick.min(left)) {
            Ok(Ok(transcript)) => return (CloudOutcome::Transcribed, Some(transcript)),
            Ok(Err(reason)) => return (CloudOutcome::Failed(reason), None),
            // The thread ended without sending, which the catch above leaves
            // no ordinary way to do.
            Err(RecvTimeoutError::Disconnected) => {
                return (
                    CloudOutcome::Failed(CloudFailure::Other(
                        "the request stopped without an answer".into(),
                    )),
                    None,
                )
            }
            Err(RecvTimeoutError::Timeout) => {
                let elapsed = release.elapsed();
                if elapsed < wait.cap {
                    on_progress(Progress::CloudWaiting {
                        provider: &cloud.provider,
                        elapsed,
                    });
                }
                // Asked before the cap is: Esc twice on the last ask gives
                // the request up, rather than losing to running out of time.
                if cancel().gives_up_on_the_cloud() {
                    return (CloudOutcome::Abandoned, None);
                }
                if elapsed >= wait.cap {
                    return (CloudOutcome::TimedOut, None);
                }
            }
        }
    }
}

impl Session {
    /// After the recording, in a cloud session: wait for the request sent at
    /// key release, and settle which text the dictation delivers.
    ///
    /// `dictation` is the on-device engine's. On [`CloudOutcome::Transcribed`]
    /// its document and transcripts become the cloud engine's, through the same
    /// dictionary and Tier 0 stages and, when it is on, cleanup, with the paste
    /// budget counted from the answer; on anything else they stay, so History
    /// has what was heard, and [`paste`] refuses.
    fn finish_in_the_cloud(
        &self,
        cloud: &Cloud,
        mut dictation: Dictation,
        sent: Sent,
        cancel: &mut dyn FnMut() -> Waiting,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> Dictation {
        let (outcome, answer, audio) = match sent.request {
            Request::Unsent(outcome) => (outcome, None, None),
            // Esc twice while the recording was finishing, after `send` last
            // asked: given up before anything is handed over, so a trigger
            // never keeps the text of a dictation that is being thrown away.
            Request::Out { .. } if cancel().gives_up_on_the_cloud() => {
                dictation.timing.cloud_ms = Some(millis_since(sent.release));
                (CloudOutcome::Abandoned, None, None)
            }
            Request::Out { answered, audio } => {
                // Before the first tick, so the text is somewhere for the
                // person to take from the moment they start waiting.
                on_progress(Progress::CloudSent {
                    provider: &cloud.provider,
                    heard: &dictation,
                });
                let (outcome, answer) = wait_for(
                    cloud,
                    self.cloud_wait,
                    answered,
                    sent.release,
                    cancel,
                    on_progress,
                );
                dictation.timing.cloud_ms = Some(millis_since(sent.release));
                (outcome, answer, Some(audio))
            }
        };
        // Giving up is a discard. So is Esc twice in the moment the answer
        // came, after the wait last asked: asked once more, before anything
        // is cleaned or handed to a remote provider. The outcome stays what
        // the provider did.
        let mut discarded = dictation.discarded || outcome == CloudOutcome::Abandoned;
        if !discarded && audio.is_some() {
            discarded = cancel() == Waiting::GiveUp;
        }
        // A request that failed or ran out of time leaves its recording here,
        // in memory and nowhere else, for the person to send again
        // ([`Session::send_again`]) until the next dictation starts. Not after
        // a transcription, which needs nothing more, and not after giving up
        // or a discard, which the person did not want kept.
        if matches!(outcome, CloudOutcome::Failed(_) | CloudOutcome::TimedOut) && !discarded {
            *self.held.borrow_mut() = audio;
        }
        let ms = millis_since(sent.release);
        let failures_in_a_row = self.count_failures(&outcome);

        let local_document = match answer {
            Some(transcript) => {
                let assembled = self.assemble_answer(
                    cloud,
                    &transcript,
                    discarded,
                    self.enhancement_wait,
                    cancel,
                    on_progress,
                );
                discarded |= assembled.discarded;
                dictation.enhancement = assembled.enhancement;
                dictation.timing.tier0_ms =
                    Some(dictation.timing.tier0_ms.unwrap_or_default() + assembled.tier0_ms);
                dictation.timing.tier1_ms = assembled.tier1_ms;
                dictation.late_cleanup = assembled.late;
                dictation.transcripts = vec![transcript];
                std::mem::replace(&mut dictation.document, assembled.document)
            }
            None => dictation.document.clone(),
        };
        dictation.local_document = Some(local_document);
        dictation.discarded = discarded;
        dictation.cloud = Some(CloudResult {
            provider: cloud.provider.clone(),
            outcome,
            ms,
            failures_in_a_row,
        });
        // Key release to the text this dictation delivers, the wait included.
        dictation.timing.total_ms = millis_since(sent.release);
        dictation
    }

    /// The failure streak once `outcome` is counted, for
    /// [`CloudResult::failures_in_a_row`]. Failed and TimedOut build the
    /// streak a repeated failure warns about; Transcribed and NoSpeech are the
    /// provider working, so they clear it; Abandoned says nothing about
    /// whether the request would have failed, so it leaves the count as it
    /// was.
    fn count_failures(&self, outcome: &CloudOutcome) -> u32 {
        let streak = &self.cloud_failures_in_a_row;
        match outcome {
            CloudOutcome::Failed(_) | CloudOutcome::TimedOut => {
                streak.set(streak.get() + 1);
                streak.get()
            }
            CloudOutcome::Transcribed | CloudOutcome::NoSpeech => {
                streak.set(0);
                0
            }
            CloudOutcome::Abandoned => streak.get(),
        }
    }

    /// A cloud engine's answer as the text it delivers: the cloud pipeline's
    /// dictionary and Tier 0, then Enhancement when it is on, waited for up
    /// to `wait`. The wait counts from the answer, because that is when this
    /// text was ready, as key release is for a local dictation's. A discarded
    /// dictation's text is Tier 0 for a trigger that throws it away, and no
    /// Enhancement is asked for it.
    fn assemble_answer(
        &self,
        cloud: &Cloud,
        transcript: &Transcript,
        discarded: bool,
        wait: Wait,
        cancel: &mut dyn FnMut() -> Waiting,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> cleanup::Assembled {
        let answered = Instant::now();
        let cleanup = if discarded {
            None
        } else {
            self.cleanup.clone()
        };
        let mut assembly = cleanup::Assembly::new(Arc::clone(&cloud.pipeline), cleanup);
        assembly.commit(transcript);
        if discarded {
            assembly.discard()
        } else {
            assembly.finish(answered, wait, cancel, on_progress)
        }
    }

    /// Whether a recording is held for [`Session::send_again`]: the last
    /// dictation's cloud request failed or ran out of time, and no dictation
    /// has started since.
    pub fn can_send_again(&self) -> bool {
        self.held.borrow().is_some()
    }

    /// Let a held recording go now, rather than when the next dictation
    /// starts: the dictation was thrown away after the session returned it,
    /// or there is no History row to send it again from. Nothing to do when
    /// nothing is held.
    pub fn let_go(&self) {
        self.held.borrow_mut().take();
    }

    /// Take the held recording out of this session, for the session opened
    /// to replace it: a settings change is not a dictation, so what is held
    /// stays for [`Session::keep_held`]. `None` when nothing is held.
    pub fn release_held(&self) -> Option<HeldRecording> {
        let provider = self.cloud.as_ref()?.provider.clone();
        let audio = self.held.borrow_mut().take()?;
        Some(HeldRecording { audio, provider })
    }

    /// Hold `held`, taken from the session this one replaced, so
    /// [`Session::send_again`] sends it with this session's engine and key.
    /// Returns whether it is held. Only a session on the same provider keeps
    /// it -- a corrected key for that provider -- because the recording was
    /// only ever meant for the provider whose request failed. A session on
    /// another provider, or with no cloud engine, lets it go.
    pub fn keep_held(&self, held: HeldRecording) -> bool {
        match &self.cloud {
            Some(cloud) if cloud.provider == held.provider => {
                *self.held.borrow_mut() = Some(held.audio);
                true
            }
            _ => false,
        }
    }

    /// Send the held recording to the cloud engine again, and wait for the
    /// answer as a dictation does: [`Progress::CloudWaiting`] about once a
    /// second, `cancel` asked at each tick, the same cap. `None` when nothing
    /// is held.
    ///
    /// Nothing is pasted: the person asked from somewhere other than the app
    /// they dictated into. A transcription lets the recording go; any other
    /// outcome keeps it, because what failed is still there to try. With
    /// Enhancement on, the provider's text is sent to it as the answer
    /// arrives, and nothing waits for the reply, which is in
    /// [`SentAgain::late_cleanup`]. `cancel` is asked as that request is
    /// about to go: [`Waiting::GiveUp`] or [`Waiting::WithoutEnhancement`]
    /// there sends nothing, and [`Waiting::DeliverNow`] sends it without
    /// waiting. The streak in
    /// [`CloudResult::failures_in_a_row`] counts this request like any other.
    pub fn send_again<C: Into<Waiting>>(
        &self,
        cancel: &mut dyn FnMut() -> C,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> Option<SentAgain> {
        let mut asked = || -> Waiting { cancel().into() };
        let cancel: &mut dyn FnMut() -> Waiting = &mut asked;
        let cloud = self.cloud.as_ref()?;
        let audio = self.held.borrow().clone()?;
        let sent = Instant::now();
        let (outcome, answer) = match spawn_request(cloud, audio) {
            Request::Unsent(outcome) => (outcome, None),
            Request::Out { answered, .. } => {
                wait_for(cloud, self.cloud_wait, answered, sent, cancel, on_progress)
            }
        };
        let failures_in_a_row = self.count_failures(&outcome);
        let (document, late_cleanup, transcripts) = match answer {
            Some(transcript) => {
                // Nothing is pasted, so nothing waits: the request goes now,
                // and its answer is late, for History.
                let now = Wait {
                    cap: Duration::ZERO,
                    ..self.enhancement_wait
                };
                let assembled =
                    self.assemble_answer(cloud, &transcript, false, now, cancel, on_progress);
                (Some(assembled.document), assembled.late, vec![transcript])
            }
            None => (None, None, Vec::new()),
        };
        if outcome == CloudOutcome::Transcribed {
            self.held.borrow_mut().take();
        }
        Some(SentAgain {
            cloud: CloudResult {
                provider: cloud.provider.clone(),
                outcome,
                ms: millis_since(sent),
                failures_in_a_row,
            },
            engine: cloud.engine.id(),
            transcripts,
            document,
            late_cleanup,
        })
    }
}

/// A recording held after a failed cloud request, on its way from a session
/// to the one opened to replace it ([`Session::release_held`],
/// [`Session::keep_held`]). Opaque: it carries the audio, and the provider it
/// was recorded for, and nothing a trigger can read.
pub struct HeldRecording {
    audio: Arc<AudioSegment>,
    /// The only provider this recording may go to: the one whose request
    /// failed. Choosing another provider is not permission to send it there.
    provider: String,
}

impl std::fmt::Debug for HeldRecording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the audio: it is what the person said.
        f.debug_struct("HeldRecording").finish_non_exhaustive()
    }
}

/// What sending a held recording again came to: see [`Session::send_again`].
#[derive(Debug)]
pub struct SentAgain {
    /// What became of the request. `ms` is from sending to this outcome.
    pub cloud: CloudResult,
    /// The cloud engine the recording went to, as [`Session::engine_id_for`]
    /// names it for a dictation it transcribed: the engine a History row
    /// holding `document` names.
    pub engine: &'static str,
    /// What the cloud engine heard, before the dictionary and Tier 0, as in
    /// [`Dictation::transcripts`]. Empty unless it transcribed.
    pub transcripts: Vec<Transcript>,
    /// On [`CloudOutcome::Transcribed`], the provider's text through the
    /// dictionary, Tier 0 and cleanup, as a dictation's would be; `None`
    /// otherwise.
    pub document: Option<Document>,
    /// Enhancement whose answer is not in `document`, as in
    /// [`Dictation::late_cleanup`]: a trigger that keeps the text calls
    /// `wait` and replaces it again, and one that drops this lets the answer
    /// go nowhere.
    pub late_cleanup: Option<LateCleanup>,
}

/// A moment's loudness: linear (not dB) RMS and peak of a block of 16 kHz
/// mono samples. `rms` is clamped to 0..=1; `peak` is not -- the resampler
/// can ring slightly past full scale (`|s| <= 1.05`), and [`Level::loudness`]
/// reads that as "near clipping", so it must not be clamped away first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub rms: f32,
    pub peak: f32,
}

/// How loud a moment of audio was: the typed reason a caller turns into a
/// waveform colour. The words, if any, belong to the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loudness {
    Normal,
    Loud,
    NearClipping,
}

/// Loudness thresholds, in dBFS, on the 16 kHz level after resampling.
/// Measured on 2026-09-16 on a MacBook Pro (M1 Pro) with its built-in
/// microphone and with AirPods Pro; the figures are in docs/benchmarks.md,
/// "Overlay loudness colours".
///
/// The peak thresholds sit below full scale (0 dBFS) because resampling can
/// ring up to ~0.4 dB past a loud transient, so crossing one says "near
/// clipping", never "clipped".
const NEAR_CLIPPING_PEAK_DBFS: f32 = -1.0;
const NEAR_CLIPPING_RMS_DBFS: f32 = -6.0;
const LOUD_PEAK_DBFS: f32 = -6.0;
const LOUD_RMS_DBFS: f32 = -18.0;

impl Level {
    /// The RMS in dBFS, as [`Level::loudness`] reads it: full scale is 0,
    /// and silence is a very large negative number, never minus infinity.
    pub fn rms_dbfs(&self) -> f32 {
        dbfs(self.rms)
    }

    /// [`Loudness::NearClipping`] when the peak is at or above
    /// [`NEAR_CLIPPING_PEAK_DBFS`] or the RMS is at or above
    /// [`NEAR_CLIPPING_RMS_DBFS`]; otherwise [`Loudness::Loud`] when the RMS
    /// is at or above [`LOUD_RMS_DBFS`] or the peak is at or above
    /// [`LOUD_PEAK_DBFS`]; otherwise [`Loudness::Normal`].
    pub fn loudness(&self) -> Loudness {
        let rms_dbfs = dbfs(self.rms);
        let peak_dbfs = dbfs(self.peak);
        if peak_dbfs >= NEAR_CLIPPING_PEAK_DBFS || rms_dbfs >= NEAR_CLIPPING_RMS_DBFS {
            Loudness::NearClipping
        } else if rms_dbfs >= LOUD_RMS_DBFS || peak_dbfs >= LOUD_PEAK_DBFS {
            Loudness::Loud
        } else {
            Loudness::Normal
        }
    }
}

/// An amplitude (0.0 upward, full scale at 1.0) as dBFS.
fn dbfs(amplitude: f32) -> f32 {
    20.0 * amplitude.max(f32::EPSILON).log10()
}

/// One pass over a block of samples for both the RMS and the peak (max
/// `|s|`) [`Level`] needs. Empty input gives rms 0, peak 0.
fn level(samples: &[f32]) -> Level {
    if samples.is_empty() {
        return Level {
            rms: 0.0,
            peak: 0.0,
        };
    }
    let mut sum_sq = 0.0f32;
    let mut peak = 0.0f32;
    for &s in samples {
        sum_sq += s * s;
        peak = peak.max(s.abs());
    }
    Level {
        rms: (sum_sq / samples.len() as f32).sqrt().clamp(0.0, 1.0),
        peak,
    }
}

/// The device the microphone should be on: the one chosen by name while it
/// is connected, otherwise whatever the system would hand over. A headset
/// chosen yesterday and left in its case is no reason for dictation to fail,
/// and the Microphone card and the overlay name the device actually in use,
/// so standing in is visible rather than silent. Pure, so the rule is tested
/// without a device.
fn wanted_device<'a>(
    chosen: Option<&'a str>,
    chosen_connected: bool,
    default_now: Option<&'a str>,
) -> Option<&'a str> {
    match chosen {
        Some(name) if chosen_connected => Some(name),
        _ => default_now,
    }
}

/// Whether the armed microphone should be dropped and armed again on
/// `wanted`. `armed` is what is held now.
fn needs_rearm(armed: Option<&str>, wanted: Option<&str>) -> bool {
    match (armed, wanted) {
        // Nothing to move to; keep what there is, or keep waiting.
        (_, None) => false,
        // Arming failed earlier and a device is there now.
        (None, Some(_)) => true,
        (Some(held), Some(wanted)) => held != wanted,
    }
}

fn millis_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// A closed session's cleanup asks nothing more. A late pass already handed
/// to the trigger stops before its next segment or pool member: the app
/// closes a session to open one with new settings -- a removed key among
/// them -- and what the old one built must not be asked again.
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(cleanup) = &self.cleanup {
            cleanup.retire();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::enhance::Enhancer;
    use macalla_core::stt::{EngineCaps, SttStream};
    use std::sync::{Arc, Mutex};

    #[test]
    fn rms_dbfs_is_full_scale_at_zero_and_finite_for_silence() {
        let full = Level {
            rms: 1.0,
            peak: 1.0,
        };
        assert_eq!(full.rms_dbfs(), 0.0);
        let half = Level {
            rms: 0.5,
            peak: 0.5,
        };
        assert!(
            (half.rms_dbfs() + 6.0206).abs() < 1e-3,
            "{}",
            half.rms_dbfs()
        );
        let silence = level(&[]);
        assert!(silence.rms_dbfs().is_finite());
        // f32::EPSILON is about -138 dBFS: far below anything a display draws.
        assert!(silence.rms_dbfs() < -130.0, "{}", silence.rms_dbfs());
    }

    #[test]
    fn an_empty_block_of_samples_has_no_level() {
        let l = level(&[]);
        assert_eq!(
            l,
            Level {
                rms: 0.0,
                peak: 0.0
            }
        );
    }

    #[test]
    fn the_peak_is_the_largest_magnitude_either_side_of_zero() {
        let l = level(&[0.5, -0.9, 0.3]);
        assert!((l.peak - 0.9).abs() < 1e-6, "got {l:?}");
        let want_rms = ((0.25f32 + 0.81 + 0.09) / 3.0).sqrt();
        assert!((l.rms - want_rms).abs() < 1e-6, "got {l:?}");
    }

    #[test]
    fn loudness_classifies_from_rms_and_peak_at_the_dbfs_boundaries() {
        // Linear amplitude for a dBFS value, from the same formula
        // `Level::loudness` uses, so the boundary cases track the consts
        // instead of hand-computed literals that could drift from them.
        fn linear(dbfs: f32) -> f32 {
            10f32.powf(dbfs / 20.0)
        }
        let just_below = |dbfs: f32| linear(dbfs) * 0.999;
        let just_above = |dbfs: f32| linear(dbfs) * 1.001;
        // Well under every boundary, so it never contaminates the other axis.
        let quiet = 0.05;

        let cases: Vec<(f32, f32, Loudness, &str)> = vec![
            (0.0, 0.0, Loudness::Normal, "silence"),
            (quiet, quiet, Loudness::Normal, "quiet speech"),
            (
                just_below(LOUD_RMS_DBFS),
                quiet,
                Loudness::Normal,
                "just below the RMS Loud boundary",
            ),
            (
                just_above(LOUD_RMS_DBFS),
                quiet,
                Loudness::Loud,
                "just above the RMS Loud boundary",
            ),
            (
                quiet,
                just_below(LOUD_PEAK_DBFS),
                Loudness::Normal,
                "just below the peak Loud boundary",
            ),
            (
                quiet,
                just_above(LOUD_PEAK_DBFS),
                Loudness::Loud,
                "just above the peak Loud boundary",
            ),
            (
                just_below(NEAR_CLIPPING_RMS_DBFS),
                quiet,
                Loudness::Loud,
                "just below the RMS NearClipping boundary",
            ),
            (
                just_above(NEAR_CLIPPING_RMS_DBFS),
                quiet,
                Loudness::NearClipping,
                "just above the RMS NearClipping boundary",
            ),
            (
                quiet,
                just_below(NEAR_CLIPPING_PEAK_DBFS),
                Loudness::Loud,
                "just below the peak NearClipping boundary",
            ),
            (
                quiet,
                just_above(NEAR_CLIPPING_PEAK_DBFS),
                Loudness::NearClipping,
                "just above the peak NearClipping boundary",
            ),
            (
                quiet,
                1.05,
                Loudness::NearClipping,
                "a resampler-ring peak past full scale is not clamped away first",
            ),
            (
                0.02,
                0.95,
                Loudness::NearClipping,
                "a quiet RMS with a hot peak reads by the peak rule, not the RMS one",
            ),
        ];
        // The measured values themselves, as literals, so a changed const is
        // a deliberate edit here too: -18 / -6 dBFS for Loud, -6 / -1 for
        // NearClipping.
        let pinned: Vec<(f32, f32, Loudness, &str)> = vec![
            (0.1, 0.4, Loudness::Normal, "rms -20, peak -8"),
            (0.13, 0.4, Loudness::Loud, "rms -17.7 is loud"),
            (0.1, 0.51, Loudness::Loud, "peak -5.8 is loud"),
            (
                0.1,
                0.87,
                Loudness::Loud,
                "peak -1.2 is loud, not yet near clipping",
            ),
            (
                0.1,
                0.9,
                Loudness::NearClipping,
                "peak -0.9 is near clipping",
            ),
            (0.49, 0.4, Loudness::Loud, "rms -6.2 is loud"),
            (
                0.51,
                0.6,
                Loudness::NearClipping,
                "rms -5.8 is near clipping",
            ),
        ];
        for (rms, peak, expected, why) in cases.into_iter().chain(pinned) {
            let got = Level { rms, peak }.loudness();
            assert_eq!(got, expected, "{why}: rms={rms} peak={peak}");
        }
    }

    #[test]
    fn the_armed_microphone_follows_the_default_when_no_device_was_chosen() {
        let pods = Some("AirPods Pro");
        let mac = Some("MacBook Pro Microphone");
        assert_eq!(wanted_device(None, false, mac), mac);
        // The headset came off: the default moved, so re-arm.
        assert!(needs_rearm(pods, mac));
        // Still on the same device: leave the armed stream alone.
        assert!(!needs_rearm(pods, pods));
        // Arming failed at launch and a device is there now.
        assert!(needs_rearm(None, mac));
        // No default at all: nothing to move to.
        assert_eq!(wanted_device(None, false, None), None);
        assert!(!needs_rearm(pods, None));
        assert!(!needs_rearm(None, None));
    }

    #[test]
    fn a_chosen_microphone_is_kept_while_connected_and_the_default_stands_in_while_not() {
        let usb = Some("USB Microphone");
        let mac = Some("MacBook Pro Microphone");
        // Connected: chosen means chosen, whatever the default is.
        assert_eq!(wanted_device(usb, true, mac), usb);
        assert!(!needs_rearm(usb, wanted_device(usb, true, mac)));
        // Unplugged: dictation carries on through the default.
        assert_eq!(wanted_device(usb, false, mac), mac);
        assert!(needs_rearm(usb, wanted_device(usb, false, mac)));
        assert!(!needs_rearm(mac, wanted_device(usb, false, mac)));
        // Plugged back in: taken back from the default.
        assert!(needs_rearm(mac, wanted_device(usb, true, mac)));
        // Neither the chosen device nor any default: nothing to move to.
        assert_eq!(wanted_device(usb, false, None), None);
    }

    /// An engine that remembers what it was told and answers with one line.
    struct FakeEngine {
        caps: EngineCaps,
        seen_terms: Arc<Mutex<Vec<String>>>,
        pushed: Arc<Mutex<usize>>,
        fail_after_pushes: Option<usize>,
    }

    impl FakeEngine {
        fn new(caps: EngineCaps) -> Self {
            Self {
                caps,
                seen_terms: Arc::default(),
                pushed: Arc::default(),
                fail_after_pushes: None,
            }
        }

        fn failing_after(pushes: usize) -> Self {
            Self {
                fail_after_pushes: Some(pushes),
                ..Self::new(EngineCaps::stream_first())
            }
        }
    }

    struct FakeStream {
        pushed: Arc<Mutex<usize>>,
        /// Fail on the push after this many, if set. The polls before it
        /// commit one final each, so there is something to keep.
        fail_after_pushes: Option<usize>,
        pushes: usize,
        /// A push got through since the last poll, so there is a line to
        /// commit.
        uncommitted: bool,
    }

    fn engine_error() -> macalla_core::Error {
        macalla_core::Error::Stt {
            engine: "fake",
            message: "the recogniser went away".into(),
        }
    }

    impl SttStream for FakeStream {
        fn push(&mut self, samples: &[f32]) -> macalla_core::Result<()> {
            if self.fail_after_pushes == Some(self.pushes) {
                return Err(engine_error());
            }
            self.pushes += 1;
            self.uncommitted = true;
            *self.pushed.lock().unwrap() += samples.len();
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            if self.fail_after_pushes.is_some() {
                // Every push that got through commits a line, once.
                if !std::mem::take(&mut self.uncommitted) {
                    return Ok(Vec::new());
                }
                return Ok(vec![Transcript {
                    text: format!("line {}", self.pushes),
                    is_final: true,
                    start_ms: 0,
                    end_ms: 500,
                }]);
            }
            Ok(vec![Transcript {
                text: "partial".into(),
                is_final: false,
                start_ms: 0,
                end_ms: 0,
            }])
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            if self.fail_after_pushes.is_some() {
                return Err(engine_error());
            }
            Ok(vec![Transcript {
                text: "hello there".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 500,
            }])
        }
    }

    impl SttEngine for FakeEngine {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> EngineCaps {
            self.caps.clone()
        }
        fn stream(&self, ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            *self.seen_terms.lock().unwrap() = ctx.terms.clone();
            Ok(Box::new(FakeStream {
                pushed: Arc::clone(&self.pushed),
                fail_after_pushes: self.fail_after_pushes,
                pushes: 0,
                uncommitted: false,
            }))
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            *self.seen_terms.lock().unwrap() = ctx.terms.clone();
            Ok(Transcript {
                text: if audio.samples.is_empty() {
                    String::new()
                } else {
                    "hello there".into()
                },
                is_final: true,
                start_ms: 0,
                end_ms: audio.duration_ms(),
            })
        }
    }

    /// A microphone that produces a fixed number of polls, then either stops
    /// cleanly or dies.
    struct FakeSource {
        polls_left: usize,
        dies: bool,
    }

    impl AudioSource for FakeSource {
        fn device_name(&self) -> &str {
            "Fake Microphone"
        }
        fn play(&mut self) -> macalla_platform::Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
            out.extend(std::iter::repeat_n(0.1, 1_600));
            if self.polls_left == 0 {
                return if self.dies {
                    // Drained first, then the report -- the platform's
                    // contract, and the reason the loop must not use `?`.
                    Err(macalla_platform::Error::DeviceLost("fake".into()))
                } else {
                    Ok(1_600)
                };
            }
            self.polls_left -= 1;
            Ok(1_600)
        }
        fn finish_into(self, _out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
            Ok(0)
        }
        fn overruns(&self) -> u64 {
            0
        }
        fn stream_warnings(&self) -> u64 {
            0
        }
    }

    fn seed() -> Dictionary {
        let (d, _) = load_dictionary(None, &["../../fixtures/dictionary/seed.json"]).unwrap();
        assert!(!d.is_empty());
        d
    }

    /// A session that hears through the energy detector: the fakes here speak
    /// in tones and constants, which a speech model rightly does not hear.
    fn session(engine: FakeEngine) -> Session {
        let mut s = Session::with_engine(Box::new(engine), seed(), "seed", Config::default());
        s.detector = energy;
        s
    }

    fn energy() -> Box<dyn SpeechDetector> {
        Box::new(macalla_core::vad::energy::EnergyDetector::new())
    }

    #[test]
    fn the_detector_dictation_uses_hears_a_whole_reading_as_speech() {
        // The synthetic readings from fixtures/generate.sh, which runs on
        // macOS; skipped where they have not been generated.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/audio");
        for name in ["tech-terms.aiff", "three-sentences.aiff"] {
            let path = dir.join(name);
            if !path.exists() {
                eprintln!("{name}: not generated here, skipped");
                continue;
            }
            let audio = AudioSegment::new(decode_file(&path).unwrap(), 0);
            let segments = Vad::new(speech_detector(), VadConfig::default()).segment(&audio);
            let heard: u64 = segments.iter().map(AudioSegment::duration_ms).sum();
            assert!(
                heard * 10 >= audio.duration_ms() * 9,
                "{name}: {heard} ms of {} ms heard as speech, in {segments:?}",
                audio.duration_ms()
            );
        }
    }

    #[test]
    fn an_engine_that_can_bias_gets_the_dictionary_and_one_that_cannot_does_not() {
        // The wiring this test exists to keep. Both halves must hold: an
        // engine without vocab_bias must NOT be sent terms it will ignore or,
        // worse, misuse.
        let biased = session(FakeEngine::new(EngineCaps {
            vocab_bias: true,
            ..EngineCaps::batch()
        }));
        assert!(
            !biased.context().terms.is_empty(),
            "written forms reach a biasing engine"
        );

        let plain = session(FakeEngine::new(EngineCaps {
            vocab_bias: false,
            ..EngineCaps::batch()
        }));
        assert!(plain.context().terms.is_empty());
    }

    #[test]
    fn the_terms_reach_the_engine_at_the_start_of_a_dictation() {
        let engine = FakeEngine::new(EngineCaps {
            vocab_bias: true,
            ..EngineCaps::batch()
        });
        let seen = Arc::clone(&engine.seen_terms);
        let s = session(engine);
        s.transcribe(vec![0.1; 16_000]).unwrap();
        assert!(!seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_dictation_from_samples_assembles_a_document() {
        let s = session(FakeEngine::new(EngineCaps::batch()));
        let d = s.transcribe(vec![0.1; 16_000]).unwrap();
        assert_eq!(d.document.to_plain().trim(), "Hello there");
        assert_eq!(d.timing.audio_ms, 1_000);
        assert!(d.timing.stt_ms.is_some());
        assert!(d.device_fault.is_none());
    }

    #[test]
    fn a_stopped_recording_keeps_everything_that_was_captured() {
        let engine = FakeEngine::new(EngineCaps::stream_first());
        let pushed = Arc::clone(&engine.pushed);
        let s = session(engine);
        let mut polls = 0;
        let mut partials = 0;
        let mut levels = Vec::new();
        let mut device_first = Vec::new();
        let d = record(
            &Setup {
                engine: s.engine.as_ref(),
                pipeline: &s.pipeline,
                context: &s.context,
                max_duration: Duration::from_secs(5),
                detector: energy,
                cleanup: None,
                enhancement_wait: Wait {
                    tick: ENHANCEMENT_TICK,
                    cap: ENHANCEMENT_WAIT,
                },
            },
            FakeSource {
                polls_left: 100,
                dies: false,
            },
            None,
            None,
            &mut || {
                polls += 1;
                polls > 3
            },
            &mut || false,
            &mut |p| match p {
                Progress::Device(name) => device_first.push((levels.len(), name.to_string())),
                Progress::Level(l) => levels.push(l),
                Progress::Transcript(_) => partials += 1,
                Progress::Capped(_)
                | Progress::CloudWaiting { .. }
                | Progress::CloudSent { .. }
                | Progress::Enhancing { .. } => {}
            },
        )
        .unwrap();
        assert_eq!(*pushed.lock().unwrap(), 3 * 1_600);
        assert_eq!(d.timing.audio_ms, 300);
        assert!(partials >= 3, "partials were shown as they arrived");
        // The fake source is a constant 0.1, so the level's rms is 0.1 per
        // poll: the waveform has something to draw from the first poll.
        assert_eq!(levels.len(), 3);
        // The device is named once, before the first level, so an overlay
        // can show it from its first frame; and the result carries it.
        assert_eq!(device_first, vec![(0, "Fake Microphone".to_string())]);
        assert_eq!(d.device, "Fake Microphone");
        assert!((levels[0].rms - 0.1).abs() < 1e-4, "got {levels:?}");
        assert!((levels[0].peak - 0.1).abs() < 1e-4, "got {levels:?}");
        assert_eq!(d.document.to_plain().trim(), "Hello there");
        assert!(d.device_fault.is_none());
    }

    #[test]
    fn a_microphone_that_dies_mid_sentence_loses_nothing_it_captured() {
        // The bug of 2026-09-08: a Bluetooth headset switching into headset
        // mode invalidated the stream, and the half-sentence before it was
        // thrown away with the error. Now the fault is reported beside the
        // text, not instead of it.
        let engine = FakeEngine::new(EngineCaps::stream_first());
        let pushed = Arc::clone(&engine.pushed);
        let s = session(engine);
        let d = record(
            &Setup {
                engine: s.engine.as_ref(),
                pipeline: &s.pipeline,
                context: &s.context,
                max_duration: Duration::from_secs(5),
                detector: energy,
                cleanup: None,
                enhancement_wait: Wait {
                    tick: ENHANCEMENT_TICK,
                    cap: ENHANCEMENT_WAIT,
                },
            },
            FakeSource {
                polls_left: 2,
                dies: true,
            },
            None,
            None,
            &mut || false,
            &mut || false,
            &mut |_| {},
        )
        .unwrap();
        // Two clean polls, then the one that died -- whose samples were drained
        // before the report and must still be here.
        assert_eq!(*pushed.lock().unwrap(), 3 * 1_600);
        let fault = d.device_fault.as_deref().expect("the fault is reported");
        assert!(fault.contains("stopped mid-recording"), "got: {fault}");
        assert_eq!(d.document.to_plain().trim(), "Hello there");
    }

    #[test]
    fn an_engine_that_fails_mid_dictation_keeps_what_it_committed() {
        let s = session(FakeEngine::failing_after(2));
        let d = record(
            &Setup {
                engine: s.engine.as_ref(),
                pipeline: &s.pipeline,
                context: &s.context,
                max_duration: Duration::from_secs(5),
                detector: energy,
                cleanup: None,
                enhancement_wait: Wait {
                    tick: ENHANCEMENT_TICK,
                    cap: ENHANCEMENT_WAIT,
                },
            },
            FakeSource {
                polls_left: 10,
                dies: false,
            },
            None,
            None,
            &mut || false,
            &mut || false,
            &mut |_| {},
        )
        .expect("two lines were committed, so this is a dictation");
        let fault = d
            .engine_fault
            .as_ref()
            .expect("the fault is reported")
            .to_string();
        assert!(fault.contains("went away"), "got: {fault}");
        assert_eq!(d.document.to_plain(), "Line 1 line 2");
        assert!(d.device_fault.is_none());
    }

    #[test]
    fn an_engine_that_fails_before_committing_anything_is_an_error() {
        let s = session(FakeEngine::failing_after(0));
        let err = record(
            &Setup {
                engine: s.engine.as_ref(),
                pipeline: &s.pipeline,
                context: &s.context,
                max_duration: Duration::from_secs(5),
                detector: energy,
                cleanup: None,
                enhancement_wait: Wait {
                    tick: ENHANCEMENT_TICK,
                    cap: ENHANCEMENT_WAIT,
                },
            },
            FakeSource {
                polls_left: 10,
                dies: false,
            },
            None,
            None,
            &mut || false,
            &mut || false,
            &mut |_| {},
        )
        .expect_err("nothing to keep, so the error is the whole story");
        assert!(err.to_string().contains("went away"), "got: {err}");
    }

    #[test]
    fn the_device_and_the_engine_are_timed_apart() {
        let s = session(FakeEngine::new(EngineCaps::stream_first()));
        let d = record(
            &Setup {
                engine: s.engine.as_ref(),
                pipeline: &s.pipeline,
                context: &s.context,
                max_duration: Duration::from_secs(5),
                detector: energy,
                cleanup: None,
                enhancement_wait: Wait {
                    tick: ENHANCEMENT_TICK,
                    cap: ENHANCEMENT_WAIT,
                },
            },
            FakeSource {
                polls_left: 1,
                dies: false,
            },
            None,
            None,
            &mut || true,
            &mut || false,
            &mut |_| {},
        )
        .unwrap();
        assert!(d.timing.open_ms.is_some());
        assert!(d.timing.engine_ms.is_some());
        assert!(d.timing.vad_ms.is_none(), "no gate ran, and it must say so");
        assert_eq!(d.heard_speech, None);
    }

    /// Loud means speech. Deterministic, unlike the adaptive energy floor.
    struct Threshold;
    impl SpeechDetector for Threshold {
        fn is_speech(&mut self, frame: &[f32]) -> bool {
            level(frame).rms > 0.1
        }
        fn reset(&mut self) {}
    }

    /// Quiet for a while, then loud, then quiet again -- one utterance.
    struct Utterance {
        poll: usize,
        polls: usize,
        loud: std::ops::Range<usize>,
    }
    impl AudioSource for Utterance {
        fn device_name(&self) -> &str {
            "Fake Microphone"
        }
        fn play(&mut self) -> macalla_platform::Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
            let level = if self.loud.contains(&self.poll) {
                0.3
            } else {
                0.001
            };
            out.extend(std::iter::repeat_n(level, 800));
            self.poll += 1;
            Ok(800)
        }
        fn finish_into(self, _out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
            Ok(0)
        }
        fn overruns(&self) -> u64 {
            0
        }
        fn stream_warnings(&self) -> u64 {
            0
        }
    }

    #[test]
    fn a_recording_ended_by_the_cap_says_so_once_and_one_ended_by_the_key_does_not() {
        let run = |polls: usize, cap: Duration| {
            let s = session(FakeEngine::new(EngineCaps::stream_first()));
            let mut capped = Vec::new();
            let mut seen = 0;
            record(
                &Setup {
                    engine: s.engine.as_ref(),
                    pipeline: &s.pipeline,
                    context: &s.context,
                    max_duration: cap,
                    detector: energy,
                    cleanup: None,
                    enhancement_wait: Wait {
                        tick: ENHANCEMENT_TICK,
                        cap: ENHANCEMENT_WAIT,
                    },
                },
                FakeSource {
                    polls_left: 1_000,
                    dies: false,
                },
                None,
                None,
                &mut || {
                    seen += 1;
                    seen > polls
                },
                &mut || false,
                &mut |p| {
                    if let Progress::Capped(at) = p {
                        capped.push(at);
                    }
                },
            )
            .unwrap();
            capped
        };
        // Never released: the cap ends it, and says which cap.
        let cap = Duration::from_millis(120);
        assert_eq!(run(usize::MAX, cap), vec![cap]);
        // Released well inside the cap: nothing to say.
        assert!(run(2, Duration::from_secs(5)).is_empty());
    }

    fn gated_record(engine: &FakeEngine, source: Utterance) -> (Dictation, usize) {
        let pushed = Arc::clone(&engine.pushed);
        let polls = source.polls;
        let mut seen = 0;
        let s = Session::with_engine(
            Box::new(FakeEngine::new(EngineCaps::stream_first())),
            seed(),
            "seed",
            Config::default(),
        );
        let d = record(
            &Setup {
                engine,
                pipeline: &s.pipeline,
                context: &s.context,
                max_duration: Duration::from_secs(10),
                detector: energy,
                cleanup: None,
                enhancement_wait: Wait {
                    tick: ENHANCEMENT_TICK,
                    cap: ENHANCEMENT_WAIT,
                },
            },
            source,
            Some(Gate::new(Box::new(Threshold), VadConfig::default())),
            None,
            &mut || {
                seen += 1;
                seen > polls
            },
            &mut || false,
            &mut |_| {},
        )
        .unwrap();
        let n = *pushed.lock().unwrap();
        (d, n)
    }

    #[test]
    fn the_gate_hands_the_engine_the_speech_and_its_pre_roll_and_not_the_silence() {
        // 40 polls of 50 ms: quiet for 1 s, speech for 0.5 s, quiet for 0.5 s.
        let engine = FakeEngine::new(EngineCaps::stream_first());
        let (d, pushed) = gated_record(
            &engine,
            Utterance {
                poll: 0,
                polls: 40,
                loud: 20..30,
            },
        );
        let speech = 10 * 800;
        let pre_roll = 300 * 16;
        let hangover = VadConfig::default().hangover.as_millis() as usize * 16;
        assert!(
            pushed >= speech + pre_roll,
            "pre-roll and speech reach the engine: {pushed}"
        );
        assert!(
            pushed <= speech + pre_roll + hangover + 4 * 512,
            "the leading second of silence does not: {pushed}"
        );
        assert_eq!(d.heard_speech, Some(true));
        assert!(d.timing.vad_ms.is_some(), "the gate's cost is measured");
        assert_eq!(
            d.timing.audio_ms, 2_000,
            "what was captured is still counted"
        );
    }

    /// Polls of 50 ms in which every sample carries its own index in its
    /// value, so a sample handed on twice shows up in what the engine heard.
    fn numbered(loud: &[std::ops::Range<usize>], polls: usize) -> Vec<Vec<f32>> {
        (0..polls)
            .map(|poll| {
                let base = if loud.iter().any(|r| r.contains(&poll)) {
                    0.3
                } else {
                    0.001
                };
                (0..800)
                    .map(|i| base + (poll * 800 + i) as f32 * 1e-7)
                    .collect()
            })
            .collect()
    }

    fn heard_twice(heard: &[f32]) -> usize {
        let mut bits: Vec<u32> = heard.iter().map(|s| s.to_bits()).collect();
        bits.sort_unstable();
        let all = bits.len();
        bits.dedup();
        all - bits.len()
    }

    #[test]
    fn the_gate_never_hands_the_engine_a_sample_twice() {
        // Speech resumes 750 ms after it stopped, so a gate with a 600 ms
        // hangover has just closed and the new start's pre-roll reaches back
        // into the hangover the engine already heard. Set here rather than
        // taken from the default, which waits longer and would not close.
        let config = VadConfig {
            hangover: Duration::from_millis(600),
            ..VadConfig::default()
        };
        let mut gate = Gate::new(Box::new(Threshold), config);
        let (mut heard, mut out) = (Vec::new(), Vec::new());
        for batch in numbered(&[20..30, 45..55], 80) {
            gate.filter(&batch, &mut out);
            heard.extend_from_slice(&out);
        }
        assert_eq!(
            heard_twice(&heard),
            0,
            "of {} samples handed on",
            heard.len()
        );
    }

    #[test]
    fn a_cut_at_the_segment_cap_reaches_the_engine_as_unbroken_audio() {
        // Ten seconds with no pause under a two-second cap. The VAD cuts
        // looking back, at the quietest moment before the cap, and the engine
        // must still hear every sample exactly once and in order.
        let config = VadConfig {
            max_segment: Duration::from_secs(2),
            ..VadConfig::default()
        };
        let mut gate = Gate::new(Box::new(Threshold), config);
        let speech = 10..210;
        let polls = numbered(std::slice::from_ref(&speech), 240);
        let (mut heard, mut out) = (Vec::new(), Vec::new());
        for batch in &polls {
            gate.filter(batch, &mut out);
            heard.extend_from_slice(&out);
        }
        assert_eq!(heard_twice(&heard), 0);
        let captured = polls.concat();
        let first = captured
            .iter()
            .position(|s| s.to_bits() == heard[0].to_bits())
            .unwrap();
        assert_eq!(&captured[first..first + heard.len()], &heard[..]);
    }

    #[test]
    fn a_silent_recording_never_reaches_the_engine_and_says_so() {
        let engine = FakeEngine::new(EngineCaps::stream_first());
        let (d, pushed) = gated_record(
            &engine,
            Utterance {
                poll: 0,
                polls: 20,
                loud: 0..0,
            },
        );
        assert_eq!(pushed, 0, "silence stays local");
        assert_eq!(d.heard_speech, Some(false));
    }

    /// A cloud engine that remembers every call -- the samples it was sent,
    /// the terms, and the thread it ran on -- and answers, fails or panics as
    /// told. A gated one holds its answer until the test lets it go, so a
    /// test about waiting never races a clock.
    struct CloudFake {
        reply: Reply,
        /// Overrides `reply`, one dictation at a time, then leaves it to
        /// answer the same way again -- so one session's cloud engine can
        /// fail a few dictations in a row and then recover, the way a real
        /// provider's luck varies from request to request.
        then: Mutex<std::collections::VecDeque<Reply>>,
        calls: Arc<Mutex<Vec<CloudCall>>>,
        gate: Option<Mutex<std::sync::mpsc::Receiver<()>>>,
        done: Option<Mutex<std::sync::mpsc::Sender<()>>>,
    }

    #[derive(Clone, Copy)]
    enum Reply {
        Text(&'static str),
        Error(&'static str),
        Panic(&'static str),
    }

    struct CloudCall {
        samples: Vec<f32>,
        terms: Vec<String>,
        thread: std::thread::ThreadId,
    }

    impl CloudFake {
        fn answering(text: &'static str) -> Self {
            Self {
                reply: Reply::Text(text),
                then: Mutex::new(std::collections::VecDeque::new()),
                calls: Arc::default(),
                gate: None,
                done: None,
            }
        }

        /// Answer with `replies` in order, one per dictation, before falling
        /// back to whatever `self` already answers with.
        fn then(self, replies: impl IntoIterator<Item = Reply>) -> Self {
            Self {
                then: Mutex::new(replies.into_iter().collect()),
                ..self
            }
        }

        fn failing(message: &'static str) -> Self {
            Self {
                reply: Reply::Error(message),
                ..Self::answering("")
            }
        }

        fn panicking(message: &'static str) -> Self {
            Self {
                reply: Reply::Panic(message),
                ..Self::answering("")
            }
        }

        /// Held inside `transcribe` until the returned sender says go (or
        /// is dropped); the receiver hears once `transcribe` has answered.
        fn gated(
            self,
        ) -> (
            Self,
            std::sync::mpsc::Sender<()>,
            std::sync::mpsc::Receiver<()>,
        ) {
            let (go, gate) = std::sync::mpsc::channel();
            let (done, finished) = std::sync::mpsc::channel();
            (
                Self {
                    gate: Some(Mutex::new(gate)),
                    done: Some(Mutex::new(done)),
                    ..self
                },
                go,
                finished,
            )
        }
    }

    impl SttEngine for CloudFake {
        fn id(&self) -> &'static str {
            "cloud-fake"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps {
                owns_segmentation: true,
                vocab_bias: true,
                ..EngineCaps::batch()
            }
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Err(macalla_core::Error::Unsupported {
                engine: "cloud-fake",
                what: "streaming",
            })
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            self.calls.lock().unwrap().push(CloudCall {
                samples: audio.samples.clone(),
                terms: ctx.terms.clone(),
                thread: std::thread::current().id(),
            });
            if let Some(gate) = &self.gate {
                let _ = gate.lock().unwrap().recv();
            }
            let reply = self.then.lock().unwrap().pop_front().unwrap_or(self.reply);
            let reply = match reply {
                Reply::Text(text) => Ok(Transcript {
                    text: text.into(),
                    is_final: true,
                    start_ms: 0,
                    end_ms: audio.duration_ms(),
                }),
                Reply::Error(message) => Err(macalla_core::Error::Stt {
                    engine: "cloud-fake",
                    message: message.into(),
                }),
                Reply::Panic(message) => panic!("{message}"),
            };
            if let Some(done) = &self.done {
                let _ = done.lock().unwrap().send(());
            }
            reply
        }
    }

    fn threshold() -> Box<dyn SpeechDetector> {
        Box::new(Threshold)
    }

    /// A session with an on-device preview engine and a cloud engine, heard
    /// through the deterministic threshold detector, waiting on the cloud in
    /// short ticks so no test takes a minute.
    fn cloud_session(local: FakeEngine, cloud: CloudFake, vad_gate: bool) -> Session {
        cloud_session_with(Box::new(local), cloud, vad_gate)
    }

    fn cloud_session_with(local: Box<dyn SttEngine>, cloud: CloudFake, vad_gate: bool) -> Session {
        let mut s = Session::with_engines(
            local,
            Some(("fakecloud".into(), Box::new(cloud))),
            seed(),
            "seed",
            Config {
                vad_gate,
                ..Config::default()
            },
        );
        s.detector = threshold;
        s.cloud_wait = Wait {
            tick: Duration::from_millis(20),
            cap: Duration::from_secs(5),
        };
        s.enhancement_wait = Wait {
            tick: ENHANCEMENT_TICK,
            cap: PASTE_BUDGET,
        };
        s
    }

    /// Quiet for a second, speech for half a second, quiet for half a
    /// second: 40 polls of 800 samples, released after the last.
    fn one_utterance() -> Utterance {
        Utterance {
            poll: 0,
            polls: 40,
            loud: 20..30,
        }
    }

    /// Released after `n` polls.
    fn after(n: usize) -> impl FnMut() -> bool {
        let mut polls = 0;
        move || {
            polls += 1;
            polls > n
        }
    }

    fn outcome(d: &Dictation) -> &CloudOutcome {
        &d.cloud.as_ref().expect("a cloud engine was in use").outcome
    }

    #[test]
    fn a_session_without_a_cloud_engine_dictates_as_it_always_has() {
        let mut s = Session::with_engine(
            Box::new(FakeEngine::new(EngineCaps::stream_first())),
            seed(),
            "seed",
            Config {
                vad_gate: false,
                ..Config::default()
            },
        );
        s.detector = energy;
        let mut cancels = 0;
        let mut waiting = 0;
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || {
                    cancels += 1;
                    false
                },
                &mut |p| {
                    if let Progress::CloudWaiting { .. } = p {
                        waiting += 1;
                    }
                },
            )
            .unwrap();
        assert_eq!(d.document.to_plain().trim(), "Hello there");
        assert_eq!(stt::final_text(&d.transcripts), "hello there");
        assert!(d.cloud.is_none());
        assert!(d.local_document.is_none());
        assert_eq!(d.timing.cloud_ms, None);
        assert_eq!(
            (cancels, waiting),
            (1, 0),
            "asked once, after the engine finished, in case Esc landed then; \
             nothing to wait for"
        );
        assert!(!d.discarded);
        assert_eq!(s.engine_id_for(&d), "fake");
    }

    #[test]
    fn a_cloud_dictation_sends_what_the_gate_heard_once_at_release_and_delivers_its_text() {
        let local = FakeEngine::new(EngineCaps::stream_first());
        let pushed = Arc::clone(&local.pushed);
        let cloud = CloudFake::answering("the cloud heard this");
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(local, cloud, true);

        let mut release = after(40);
        let mut calls_while_recording = 0;
        let mut partials = 0;
        let d = s
            .dictate_from(
                one_utterance(),
                &mut || {
                    calls_while_recording += calls.lock().unwrap().len();
                    release()
                },
                &mut || false,
                &mut |p| {
                    if let Progress::Transcript(t) = p {
                        partials += usize::from(!t.is_final);
                    }
                },
            )
            .unwrap();

        assert_eq!(calls_while_recording, 0, "never inside the record loop");
        assert!(partials >= 40, "the preview engine still shows partials");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "one request for the whole recording");
        let sent = &calls[0];
        assert_eq!(
            sent.samples.len(),
            *pushed.lock().unwrap(),
            "exactly what the gate handed the preview engine"
        );
        let speech = 10 * 800;
        let pre_roll = 300 * 16;
        let hangover = VadConfig::default().hangover.as_millis() as usize * 16;
        assert!(
            sent.samples.len() <= speech + pre_roll + hangover + 4 * 512,
            "the leading second of silence is not sent: {}",
            sent.samples.len()
        );
        assert!(sent.samples.len() >= speech);
        assert_ne!(sent.thread, std::thread::current().id(), "a worker thread");
        assert!(!sent.terms.is_empty(), "the dictionary travels with it");

        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert_eq!(d.cloud.as_ref().unwrap().provider, "fakecloud");
        assert!(
            d.document
                .to_plain()
                .to_lowercase()
                .contains("the cloud heard this"),
            "got: {}",
            d.document.to_plain()
        );
        assert_eq!(stt::final_text(&d.transcripts), "the cloud heard this");
        assert_eq!(
            d.local_document.as_ref().unwrap().to_plain().trim(),
            "Hello there",
            "what the preview engine heard is kept"
        );
        assert!(d.timing.cloud_ms.is_some());
        assert_eq!(s.engine_id_for(&d), "cloud-fake");
    }

    /// A source whose release drains `tail` more samples after its polls, as
    /// a real ring does with what arrived since the last one.
    struct WithTail<S> {
        source: S,
        tail: Vec<f32>,
    }

    impl<S: AudioSource> AudioSource for WithTail<S> {
        fn device_name(&self) -> &str {
            self.source.device_name()
        }
        fn play(&mut self) -> macalla_platform::Result<()> {
            self.source.play()
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
            self.source.poll_into(out)
        }
        fn finish_into(self, out: &mut Vec<f32>) -> macalla_platform::Result<usize> {
            let drained = self.source.finish_into(out)?;
            out.extend_from_slice(&self.tail);
            Ok(drained + self.tail.len())
        }
        fn overruns(&self) -> u64 {
            self.source.overruns()
        }
        fn stream_warnings(&self) -> u64 {
            self.source.stream_warnings()
        }
    }

    #[test]
    fn with_the_gate_off_the_cloud_engine_gets_everything_captured_tail_included() {
        let cloud = CloudFake::answering("all of it");
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let d = s
            .dictate_from(
                WithTail {
                    source: FakeSource {
                        polls_left: 100,
                        dies: false,
                    },
                    tail: vec![0.1; 800],
                },
                &mut after(3),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert_eq!(
            calls.lock().unwrap()[0].samples.len(),
            3 * 1_600 + 800,
            "the polls, and the tail drained at release"
        );
    }

    #[test]
    fn with_the_gate_on_a_tail_still_in_speech_goes_to_the_cloud_as_the_preview_heard_it() {
        let local = FakeEngine::new(EngineCaps::stream_first());
        let pushed = Arc::clone(&local.pushed);
        let cloud = CloudFake::answering("to the last word");
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(local, cloud, true);
        let d = s
            .dictate_from(
                WithTail {
                    source: Utterance {
                        poll: 0,
                        polls: 40,
                        loud: 20..40,
                    },
                    tail: vec![0.35; 800],
                },
                &mut after(40),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        let calls = calls.lock().unwrap();
        let sent = &calls[0].samples;
        assert_eq!(
            sent.len(),
            *pushed.lock().unwrap(),
            "exactly what the preview engine was handed"
        );
        assert!(
            sent.ends_with(&[0.35_f32; 800]),
            "the tail, still speech, went too"
        );
    }

    #[test]
    fn a_failed_cloud_request_keeps_the_local_text_and_pastes_nothing() {
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("the provider said no"),
            false,
        );
        let mut d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        match outcome(&d) {
            CloudOutcome::Failed(reason) => {
                let reason = reason.to_string();
                assert!(reason.contains("the provider said no"), "got: {reason}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert_eq!(d.document.to_plain().trim(), "Hello there");
        assert_eq!(
            s.engine_id_for(&d),
            "fake",
            "History names the engine whose text it is"
        );
        // Refused before the clipboard: this would paste for real otherwise.
        assert_eq!(paste(&mut d).unwrap(), Paste::Withheld);
        assert_eq!(d.timing.paste_ms, None);
    }

    #[test]
    fn a_cloud_request_past_the_cap_times_out_and_its_late_answer_goes_nowhere() {
        let (cloud, go, finished) = CloudFake::answering("too late").gated();
        let mut s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let cap = Duration::from_millis(100);
        s.cloud_wait = Wait {
            tick: Duration::from_millis(20),
            cap,
        };
        let started = Instant::now();
        let mut d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(1),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        let took = started.elapsed();
        assert_eq!(outcome(&d), &CloudOutcome::TimedOut);
        assert!(
            took < cap + Duration::from_secs(2),
            "returned at the cap, not with the request: {took:?}"
        );
        assert!(
            finished.try_recv().is_err(),
            "the request is still out, held by the gate"
        );
        assert_eq!(d.document.to_plain().trim(), "Hello there");
        assert_eq!(paste(&mut d).unwrap(), Paste::Withheld);

        // Let the worker go: it finishes, and its answer goes nowhere.
        go.send(()).unwrap();
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker finishes once it is let go");
        assert_eq!(outcome(&d), &CloudOutcome::TimedOut);
        assert_eq!(d.document.to_plain().trim(), "Hello there");
    }

    #[test]
    fn giving_up_at_a_tick_returns_without_waiting_for_another() {
        let (cloud, go, finished) = CloudFake::answering("never seen").gated();
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let waits = std::cell::Cell::new(0);
        let mut waits_when_given_up = None;
        let mut d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(1),
                &mut || {
                    // No before sending; yes once a wait has been reported.
                    if waits.get() == 0 {
                        return false;
                    }
                    waits_when_given_up.get_or_insert(waits.get());
                    true
                },
                &mut |p| {
                    if let Progress::CloudWaiting { .. } = p {
                        waits.set(waits.get() + 1);
                    }
                },
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Abandoned);
        assert_eq!(waits_when_given_up, Some(1), "gave up at the first tick");
        assert_eq!(waits.get(), 1, "and did not wait for another");
        assert_eq!(paste(&mut d).unwrap(), Paste::Withheld);

        go.send(()).unwrap();
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker finishes once it is let go");
        assert_eq!(calls.lock().unwrap().len(), 1, "one request was sent");
    }

    #[test]
    fn cloud_failures_in_a_row_reach_the_threshold_and_a_success_resets_them() {
        let cloud = CloudFake::failing("network").then([
            Reply::Error("network"),
            Reply::Error("network"),
            Reply::Error("network"),
            Reply::Text("recovered"),
        ]);
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let dictate = || {
            s.dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || false,
                &mut |_| {},
            )
            .unwrap()
        };
        let d1 = dictate();
        assert_eq!(d1.cloud.as_ref().unwrap().failures_in_a_row, 1);
        assert!(!d1.cloud.as_ref().unwrap().failing_repeatedly());
        let d2 = dictate();
        assert_eq!(d2.cloud.as_ref().unwrap().failures_in_a_row, 2);
        assert!(!d2.cloud.as_ref().unwrap().failing_repeatedly());
        let d3 = dictate();
        assert_eq!(d3.cloud.as_ref().unwrap().failures_in_a_row, 3);
        assert!(
            d3.cloud.as_ref().unwrap().failing_repeatedly(),
            "three in a row is the threshold"
        );
        let d4 = dictate();
        assert_eq!(
            d4.cloud.as_ref().unwrap().failures_in_a_row,
            0,
            "a transcription resets the streak"
        );
        assert!(!d4.cloud.as_ref().unwrap().failing_repeatedly());
    }

    #[test]
    fn giving_up_leaves_the_failure_streak_unchanged() {
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("network"),
            false,
        );
        let failed = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(failed.cloud.as_ref().unwrap().failures_in_a_row, 1);

        // Cancel before release: the recording is discarded, never sent, and
        // whether it would have failed is never known -- the streak from the
        // dictation before it stands.
        let abandoned = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || true,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(outcome(&abandoned), &CloudOutcome::Abandoned);
        assert_eq!(
            abandoned.cloud.as_ref().unwrap().failures_in_a_row,
            1,
            "abandoning neither resets nor extends the streak"
        );
    }

    #[test]
    fn a_dictation_discarded_before_release_is_never_sent() {
        let cloud = CloudFake::answering("must not happen");
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || true,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Abandoned);
        assert_eq!(calls.lock().unwrap().len(), 0, "the audio never left");
        assert_eq!(d.timing.cloud_ms, None);
    }

    #[test]
    fn silence_is_never_sent_to_the_cloud() {
        let cloud = CloudFake::answering("must not happen");
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, true);
        let mut d = s
            .dictate_from(
                Utterance {
                    poll: 0,
                    polls: 20,
                    loud: 0..0,
                },
                &mut after(20),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::NoSpeech);
        assert_eq!(calls.lock().unwrap().len(), 0);
        assert_eq!(d.timing.cloud_ms, None, "no request, so no request time");
        assert_eq!(paste(&mut d).unwrap(), Paste::Withheld);
    }

    #[test]
    fn a_slow_cloud_request_says_it_is_waiting() {
        let (cloud, go, _finished) = CloudFake::answering("eventually").gated();
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let mut waits = Vec::new();
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(1),
                &mut || false,
                &mut |p| {
                    if let Progress::CloudWaiting { provider, elapsed } = p {
                        waits.push((provider.to_string(), elapsed));
                        // It answers only once the wait has been reported.
                        let _ = go.send(());
                    }
                },
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert!(!waits.is_empty(), "said at least once");
        assert!(waits.iter().all(|(provider, _)| provider == "fakecloud"));
        assert!(waits.windows(2).all(|w| w[0].1 <= w[1].1), "{waits:?}");
    }

    #[test]
    fn what_this_machine_heard_is_handed_over_as_the_request_goes_out() {
        let (cloud, go, _finished) = CloudFake::answering("the cloud heard this").gated();
        let mut s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        s.front_app = notes;
        let mut said = Vec::new();
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || false,
                &mut |p| match p {
                    Progress::CloudSent { provider, heard } => {
                        assert_eq!(provider, "fakecloud");
                        assert!(heard.cloud.is_none(), "no answer yet");
                        assert_eq!(heard.front_app, notes(), "with the app it was for");
                        said.push(format!("sent: {}", heard.document.to_plain().trim()));
                        // The provider answers only once the text is handed over.
                        let _ = go.send(());
                    }
                    Progress::CloudWaiting { .. } => said.push("waiting".into()),
                    _ => {}
                },
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert_eq!(said.first().map(String::as_str), Some("sent: Hello there"));
        assert_eq!(
            said.iter().filter(|s| s.starts_with("sent")).count(),
            1,
            "once, before any tick: {said:?}"
        );

        // No request, nothing handed over: discarded before release, nothing
        // heard, and a local session.
        let quiet = |s: &Session, cancel: bool, source: Utterance| {
            let mut sent = 0;
            s.dictate_from(source, &mut after(20), &mut || cancel, &mut |p| {
                if let Progress::CloudSent { .. } = p {
                    sent += 1;
                }
            })
            .unwrap();
            sent
        };
        let silent = || Utterance {
            poll: 0,
            polls: 20,
            loud: 0..0,
        };
        let gated = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("must not happen"),
            true,
        );
        assert_eq!(quiet(&gated, true, one_utterance()), 0, "discarded");
        assert_eq!(quiet(&gated, false, silent()), 0, "nothing heard");
        let mut local = session(FakeEngine::new(EngineCaps::stream_first()));
        local.detector = threshold;
        assert_eq!(quiet(&local, false, one_utterance()), 0, "on this machine");
    }

    #[test]
    fn esc_twice_after_the_request_goes_out_and_before_the_wait_hands_nothing_over() {
        // Not given up when the request went out, given up by the next time
        // anything is asked: the heard text must not be handed to a trigger
        // that keeps it, because this dictation is being thrown away.
        let (cloud, go, finished) = CloudFake::answering("the cloud heard this").gated();
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let mut asked = 0;
        let mut handed_over = 0;
        let d = s
            .dictate_from(
                fake_source(),
                &mut after(3),
                &mut || {
                    asked += 1;
                    asked > 1
                },
                &mut |p| {
                    if let Progress::CloudSent { .. } = p {
                        handed_over += 1;
                    }
                },
            )
            .unwrap();
        assert_eq!(handed_over, 0, "nothing handed over to be kept");
        assert_eq!(outcome(&d), &CloudOutcome::Abandoned);
        assert!(d.discarded);
        assert!(!s.can_send_again(), "a discard is not held");
        go.send(()).unwrap();
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker finishes once it is let go");
    }

    fn fake_source() -> FakeSource {
        FakeSource {
            polls_left: 100,
            dies: false,
        }
    }

    #[test]
    fn a_recording_is_held_after_a_failure_or_a_timeout_until_the_next_dictation_starts() {
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("the provider said no"),
            false,
        );
        assert!(!s.can_send_again(), "nothing yet");
        let d = s
            .dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        assert!(matches!(outcome(&d), CloudOutcome::Failed(_)));
        assert!(s.can_send_again(), "held after a failure");

        // The next dictation lets it go as it starts, before anything else.
        let mut held_while_recording = None;
        let mut release = after(3);
        s.dictate_from(
            fake_source(),
            &mut || {
                held_while_recording.get_or_insert(s.can_send_again());
                release()
            },
            &mut || false,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(held_while_recording, Some(false));

        let (cloud, go, finished) = CloudFake::answering("too late").gated();
        let mut s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        s.cloud_wait = Wait {
            tick: Duration::from_millis(20),
            cap: Duration::from_millis(100),
        };
        let d = s
            .dictate_from(fake_source(), &mut after(1), &mut || false, &mut |_| {})
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::TimedOut);
        assert!(s.can_send_again(), "held after a timeout");
        go.send(()).unwrap();
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker finishes once it is let go");
    }

    #[test]
    fn a_held_recording_carried_into_the_next_session_is_sent_again_from_there() {
        // A refused key holds the recording.
        let old_cloud = CloudFake::failing("HTTP 401");
        let old_calls = Arc::clone(&old_cloud.calls);
        let old = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            old_cloud,
            false,
        );
        old.dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        assert!(old.can_send_again(), "held after a failure");

        // The key is fixed, and the app opens a session to take it.
        let held = old.release_held().expect("a recording is held");
        assert!(!old.can_send_again(), "handed over, not copied");
        let new_cloud = CloudFake::answering("kubectl ran");
        let new_calls = Arc::clone(&new_cloud.calls);
        let new = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            new_cloud,
            false,
        );
        assert!(new.keep_held(held), "a cloud session keeps it");
        assert!(new.can_send_again());

        let again = new
            .send_again(&mut || false, &mut |_| {})
            .expect("held in the new session");
        assert_eq!(again.cloud.outcome, CloudOutcome::Transcribed);
        let new_calls = new_calls.lock().unwrap();
        assert_eq!(new_calls.len(), 1, "the new session's engine was asked");
        assert_eq!(
            new_calls[0].samples.len(),
            old_calls.lock().unwrap()[0].samples.len(),
            "the same recording"
        );
    }

    #[test]
    fn a_held_recording_is_not_kept_by_a_session_on_another_provider() {
        // Held after one provider refused the key...
        let old = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("HTTP 401"),
            false,
        );
        old.dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        let held = old.release_held().expect("a recording is held");

        // ...and the person then chooses a different provider. The
        // recording was only ever meant for the first one.
        let mut other = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        other.cloud.as_mut().expect("a cloud session").provider = "otherprovider".into();
        assert!(!other.keep_held(held), "not for this provider");
        assert!(!other.can_send_again());
        assert!(other.send_again(&mut || false, &mut |_| {}).is_none());
    }

    #[test]
    fn a_session_with_no_cloud_engine_does_not_keep_a_held_recording() {
        let old = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("HTTP 401"),
            false,
        );
        old.dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        let held = old.release_held().expect("a recording is held");
        let local = session(FakeEngine::new(EngineCaps::stream_first()));
        assert!(!local.keep_held(held), "nothing to send it with");
        assert!(!local.can_send_again());
    }

    #[test]
    fn letting_go_drops_a_held_recording() {
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("the provider said no"),
            false,
        );
        s.let_go(); // nothing held: nothing to do
        s.dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        assert!(s.can_send_again(), "held after a failure");
        s.let_go();
        assert!(!s.can_send_again(), "let go");
        assert!(
            s.send_again(&mut || false, &mut |_| {}).is_none(),
            "nothing left to send"
        );
    }

    #[test]
    fn a_recording_is_not_held_after_a_transcription_giving_up_or_a_discard() {
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        let d = s
            .dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert!(!s.can_send_again(), "transcribed: nothing to send again");

        // Given up before the request went out.
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("never sent"),
            false,
        );
        let d = s
            .dictate_from(fake_source(), &mut after(3), &mut || true, &mut |_| {})
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Abandoned);
        assert!(!s.can_send_again(), "given up before sending");

        // Given up while waiting.
        let (cloud, go, finished) = CloudFake::failing("slow").gated();
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let waited = Cell::new(false);
        let d = s
            .dictate_from(
                fake_source(),
                &mut after(1),
                &mut || waited.get(),
                &mut |p| {
                    if let Progress::CloudWaiting { .. } = p {
                        waited.set(true);
                    }
                },
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Abandoned);
        assert!(!s.can_send_again(), "given up while waiting");
        go.send(()).unwrap();
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the worker finishes once it is let go");

        // Esc twice as a failure lands: thrown away, so not kept either.
        // Asked as the request goes out, before the wait, and then as it
        // lands.
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("HTTP 500"),
            false,
        );
        let mut asked = 0;
        let d = s
            .dictate_from(
                fake_source(),
                &mut after(3),
                &mut || {
                    asked += 1;
                    asked > 2
                },
                &mut |_| {},
            )
            .unwrap();
        assert!(d.discarded);
        assert!(!s.can_send_again(), "discarded");
    }

    #[test]
    fn sending_again_returns_the_new_outcome_and_keeps_the_recording_until_it_transcribes() {
        let local = session(FakeEngine::new(EngineCaps::stream_first()));
        assert!(
            local.send_again(&mut || false, &mut |_| {}).is_none(),
            "no cloud engine"
        );

        let cloud = CloudFake::failing("HTTP 503").then([
            Reply::Error("HTTP 503"),
            Reply::Error("HTTP 500"),
            Reply::Text("kubectl ran"),
        ]);
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        assert!(
            s.send_again(&mut || false, &mut |_| {}).is_none(),
            "nothing held"
        );
        let d = s
            .dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        assert!(
            matches!(outcome(&d), CloudOutcome::Failed(r) if r.to_string().contains("HTTP 503"))
        );

        let again = s
            .send_again(&mut || false, &mut |_| {})
            .expect("a recording is held");
        match &again.cloud.outcome {
            CloudOutcome::Failed(reason) => {
                let reason = reason.to_string();
                assert!(reason.contains("HTTP 500"), "{reason}")
            }
            other => panic!("expected the second failure, got {other:?}"),
        }
        assert_eq!(again.document, None);
        assert!(again.transcripts.is_empty(), "nothing heard, nothing said");
        assert_eq!(again.cloud.provider, "fakecloud");
        assert_eq!(again.cloud.failures_in_a_row, 2, "counted in the streak");
        assert!(s.can_send_again(), "still held after failing again");

        let again = s.send_again(&mut || false, &mut |_| {}).unwrap();
        assert_eq!(again.cloud.outcome, CloudOutcome::Transcribed);
        // What History needs to name the row after the provider: its engine
        // and what that engine heard.
        assert_eq!(again.engine, "cloud-fake");
        assert_eq!(
            macalla_core::stt::final_text(&again.transcripts).trim(),
            "kubectl ran"
        );
        let text = again.document.expect("the provider's text").to_plain();
        assert!(text.to_lowercase().contains("kubectl ran"), "got: {text}");
        assert_eq!(again.cloud.failures_in_a_row, 0);
        assert!(!s.can_send_again(), "let go once it transcribed");
        assert!(s.send_again(&mut || false, &mut |_| {}).is_none());

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "the dictation, and two sends");
        assert!(
            calls.iter().all(|c| c.samples == calls[0].samples),
            "the same recording each time"
        );
        assert!(!calls[0].samples.is_empty());
    }

    #[test]
    fn a_preview_engine_that_fails_does_not_cost_the_cloud_its_recording() {
        // Failing before it has committed anything, and after two lines.
        for pushes in [0, 2] {
            let cloud = CloudFake::answering("the cloud heard all of it");
            let calls = Arc::clone(&cloud.calls);
            let s = cloud_session(FakeEngine::failing_after(pushes), cloud, false);
            let d = s
                .dictate_from(
                    WithTail {
                        source: FakeSource {
                            polls_left: 100,
                            dies: false,
                        },
                        tail: vec![0.1; 800],
                    },
                    &mut after(6),
                    &mut || false,
                    &mut |_| {},
                )
                .unwrap_or_else(|e| panic!("failing after {pushes} pushes: {e}"));
            assert_eq!(outcome(&d), &CloudOutcome::Transcribed, "after {pushes}");
            assert_eq!(
                calls.lock().unwrap()[0].samples.len(),
                6 * 1_600 + 800,
                "everything captured after the fault, the tail drained at release too \
                 (after {pushes})"
            );
            assert!(d.engine_fault.is_some(), "the fault is still reported");
            assert_eq!(
                d.timing.audio_ms, 650,
                "the tail is counted (after {pushes})"
            );
        }
    }

    #[test]
    fn only_a_session_with_its_on_device_engine_can_dictate() {
        assert!(session(FakeEngine::new(EngineCaps::stream_first())).can_dictate());
        assert!(cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering(""),
            false,
        )
        .can_dictate());
        let files_only = Session::from_opened(
            registry::open("no-such-engine", "en-US"),
            Some(("fakecloud".into(), Box::new(CloudFake::answering("")))),
            seed(),
            "seed",
            Config::default(),
        )
        .expect("a cloud session opens without its preview engine");
        assert!(!files_only.can_dictate());
    }

    /// A preview engine whose finish looks for the cloud request, for up to
    /// two seconds, and then takes a moment longer: the request has to be
    /// out while the preview finishes, not after.
    struct PreviewThatWatches {
        cloud_calls: Arc<Mutex<Vec<CloudCall>>>,
        saw_request: Arc<std::sync::atomic::AtomicBool>,
    }

    struct Watching {
        cloud_calls: Arc<Mutex<Vec<CloudCall>>>,
        saw_request: Arc<std::sync::atomic::AtomicBool>,
    }

    impl SttStream for Watching {
        fn push(&mut self, _samples: &[f32]) -> macalla_core::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            Ok(Vec::new())
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            let until = Instant::now() + Duration::from_secs(2);
            while self.cloud_calls.lock().unwrap().is_empty() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            let seen = !self.cloud_calls.lock().unwrap().is_empty();
            self.saw_request
                .store(seen, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            Ok(vec![Transcript {
                text: "hello there".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 500,
            }])
        }
    }

    impl SttEngine for PreviewThatWatches {
        fn id(&self) -> &'static str {
            "watching"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::stream_first()
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(Watching {
                cloud_calls: Arc::clone(&self.cloud_calls),
                saw_request: Arc::clone(&self.saw_request),
            }))
        }
        fn transcribe(
            &self,
            _audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            Err(macalla_core::Error::Unsupported {
                engine: "watching",
                what: "batch",
            })
        }
    }

    #[test]
    fn the_request_is_out_before_the_preview_engine_finishes() {
        let cloud = CloudFake::answering("sent at release");
        let saw_request = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let preview = PreviewThatWatches {
            cloud_calls: Arc::clone(&cloud.calls),
            saw_request: Arc::clone(&saw_request),
        };
        let s = cloud_session_with(Box::new(preview), cloud, false);
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(2),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert!(
            saw_request.load(std::sync::atomic::Ordering::SeqCst),
            "the preview engine finished with the request already out"
        );
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        let release_to_final = d.timing.stt_ms.unwrap();
        assert!(
            d.cloud.as_ref().unwrap().ms >= release_to_final,
            "timed from release"
        );
        assert!(d.timing.cloud_ms.unwrap() >= release_to_final);
    }

    #[test]
    fn a_cloud_engine_that_panics_fails_with_what_it_said() {
        let s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::panicking("the adapter fell over"),
            false,
        );
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(1),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        match outcome(&d) {
            CloudOutcome::Failed(reason) => {
                let reason = reason.to_string();
                assert!(reason.contains("the adapter fell over"), "got: {reason}")
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        assert_eq!(d.document.to_plain().trim(), "Hello there");
    }

    #[test]
    fn a_file_in_a_cloud_session_goes_to_the_cloud_engine() {
        // `macalla transcribe --cloud`: a file has no preview to show.
        let cloud = CloudFake::answering("from the file");
        let calls = Arc::clone(&cloud.calls);
        let s = cloud_session(FakeEngine::new(EngineCaps::batch()), cloud, false);
        let d = s.transcribe(vec![0.1; 16_000]).unwrap();
        assert_eq!(calls.lock().unwrap()[0].samples.len(), 16_000);
        assert_eq!(stt::final_text(&d.transcripts), "from the file");
    }

    #[test]
    fn a_cloud_session_opens_without_its_preview_engine_for_files_but_will_not_dictate() {
        // `macalla transcribe --cloud` on a machine with no local model: the
        // file goes to the cloud engine and never needed a preview.
        let cloud = CloudFake::answering("from the cloud");
        let calls = Arc::clone(&cloud.calls);
        let config = Config {
            engine: Some("no-such-engine".into()),
            ..Config::default()
        };
        let mut s = Session::from_opened(
            registry::open("no-such-engine", "en-US"),
            Some(("fakecloud".into(), Box::new(cloud))),
            seed(),
            "seed",
            config,
        )
        .expect("a file needs no preview engine");
        let d = s.transcribe(vec![0.1; 16_000]).unwrap();
        assert_eq!(stt::final_text(&d.transcripts), "from the cloud");
        assert_eq!(calls.lock().unwrap().len(), 1);

        // Refused before the microphone is touched, with the reason.
        match s.dictate(&mut || StopSignal::Release, &mut || false, &mut |_| {}) {
            Err(Error::NoPreviewEngine(reason)) => {
                assert!(reason.contains("no-such-engine"), "{reason}")
            }
            other => panic!("expected the preview engine's error, got {other:?}"),
        }
        assert_eq!(calls.lock().unwrap().len(), 1, "nothing more was sent");
    }

    #[test]
    fn a_local_session_whose_engine_cannot_open_is_still_no_session() {
        assert!(matches!(
            Session::from_opened(
                registry::open("no-such-engine", "en-US"),
                None,
                seed(),
                "seed",
                Config::default(),
            ),
            Err(Error::NoEngine(_))
        ));
    }

    #[test]
    fn a_new_dictionary_reaches_the_cloud_engine_too() {
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering(""),
            false,
        );
        assert!(
            s.context().terms.is_empty(),
            "the preview engine cannot bias"
        );
        let cloud_terms = |s: &Session| s.cloud.as_ref().unwrap().context.terms.clone();
        assert!(!cloud_terms(&s).is_empty());
        let (empty, label) = load_dictionary(None, &[]).unwrap();
        s.replace_dictionary(empty, label);
        assert!(cloud_terms(&s).is_empty());
    }

    // ------------------------------------------------------------- cleanup

    /// An engine that commits the lines it is given: the next of `during` on
    /// each poll after audio reached it, and all of `at_release` when it is
    /// finished, heard or not. Unmeasured, so Tier 0 runs in full.
    struct Lines {
        during: Vec<&'static str>,
        at_release: Vec<&'static str>,
    }

    struct LinesStream {
        during: std::collections::VecDeque<&'static str>,
        at_release: Vec<&'static str>,
        heard: bool,
    }

    fn final_line(text: &str) -> Transcript {
        Transcript {
            text: text.into(),
            is_final: true,
            start_ms: 0,
            end_ms: 500,
        }
    }

    impl SttStream for LinesStream {
        fn push(&mut self, _samples: &[f32]) -> macalla_core::Result<()> {
            self.heard = true;
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            if !std::mem::take(&mut self.heard) {
                return Ok(Vec::new());
            }
            Ok(self
                .during
                .pop_front()
                .map(final_line)
                .into_iter()
                .collect())
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            Ok(self
                .at_release
                .iter()
                .map(|line| final_line(line))
                .collect())
        }
    }

    impl SttEngine for Lines {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::stream_first()
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(LinesStream {
                during: self.during.iter().copied().collect(),
                at_release: self.at_release.clone(),
                heard: false,
            }))
        }
        fn transcribe(
            &self,
            _audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            Err(macalla_core::Error::Unsupported {
                engine: "fake",
                what: "batch",
            })
        }
    }

    /// One request a cleanup provider was sent: the blocks, and the thread it
    /// ran on.
    type CleanupRequest = (Vec<macalla_core::block::Block>, std::thread::ThreadId);

    /// Which of a fake provider's requests are held before answering, by
    /// number, and what lets each one go.
    type RequestGate = (fn(usize) -> bool, Mutex<std::sync::mpsc::Receiver<()>>);

    /// A cleanup provider that answers each block as `answer` says, after
    /// `delay`, both by the request's number, and applies the default
    /// prompt's guard the way every adapter does. It remembers every request.
    struct FakeCleanup {
        requests: Arc<Mutex<Vec<CleanupRequest>>>,
        answer: fn(usize, &str) -> macalla_core::Result<String>,
        delay: fn(usize) -> Duration,
        /// Told each request's number as it starts; see
        /// [`FakeCleanup::announcing`].
        started: Option<Mutex<std::sync::mpsc::Sender<usize>>>,
        /// Which requests are held before answering, and what lets each one
        /// go; see [`FakeCleanup::gated`].
        gate: Option<RequestGate>,
    }

    impl FakeCleanup {
        fn new(
            answer: fn(usize, &str) -> macalla_core::Result<String>,
            delay: fn(usize) -> Duration,
        ) -> (Self, Arc<Mutex<Vec<CleanupRequest>>>) {
            let requests = Arc::default();
            (
                Self {
                    requests: Arc::clone(&requests),
                    answer,
                    delay,
                    started: None,
                    gate: None,
                },
                requests,
            )
        }

        /// The receiver hears each request's number as it starts, so a test
        /// can wait until one is in flight instead of guessing how long a
        /// busy machine takes to get there.
        fn announcing(self) -> (Self, std::sync::mpsc::Receiver<usize>) {
            let (started, announced) = std::sync::mpsc::channel();
            (
                Self {
                    started: Some(Mutex::new(started)),
                    ..self
                },
                announced,
            )
        }

        /// Each request `which` names, by its number, waits after it starts
        /// for one go on the returned sender, or for the sender to be dropped,
        /// so a test decides when a provider answers instead of guessing how
        /// long one takes.
        fn gated(self, which: fn(usize) -> bool) -> (Self, std::sync::mpsc::Sender<()>) {
            let (go, gate) = std::sync::mpsc::channel();
            (
                Self {
                    gate: Some((which, Mutex::new(gate))),
                    ..self
                },
                go,
            )
        }
    }

    impl Enhancer for FakeCleanup {
        fn caps(&self) -> macalla_core::enhance::EnhancerCaps {
            macalla_core::enhance::EnhancerCaps {
                id: "fake-cleanup",
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            true
        }
        fn enhance(
            &self,
            blocks: &[macalla_core::block::Block],
            _terms: &[String],
        ) -> macalla_core::Result<Vec<macalla_core::block::Block>> {
            let call = {
                let mut requests = self.requests.lock().unwrap();
                requests.push((blocks.to_vec(), std::thread::current().id()));
                requests.len() - 1
            };
            if let Some(started) = &self.started {
                let _ = started.lock().unwrap().send(call);
            }
            if let Some((which, gate)) = &self.gate {
                if which(call) {
                    let _ = gate.lock().unwrap().recv();
                }
            }
            std::thread::sleep((self.delay)(call));
            macalla_core::enhance::rewrite_each(
                blocks,
                macalla_core::enhance::Guard::SameWords,
                |text| (self.answer)(call, text),
            )
        }
    }

    fn at_once(_: usize) -> Duration {
        Duration::ZERO
    }

    /// Well past the paste budget, and short enough for a test to wait out.
    const SLOW: Duration = Duration::from_millis(1_000);

    fn paragraph(text: &str) -> Vec<macalla_core::block::Block> {
        vec![macalla_core::block::Block::Paragraph { text: text.into() }]
    }

    /// Cleanup on this machine, as Apple Intelligence or a loopback server is.
    fn local(fake: FakeCleanup) -> cleanup::Cleanup {
        cleanup::Cleanup::new(Arc::new(fake), choice::CleanupReach::Local, "Local")
    }

    /// Cleanup off this machine, as any other server or a cloud model is.
    fn remote(fake: FakeCleanup) -> cleanup::Cleanup {
        cleanup::Cleanup::new(Arc::new(fake), choice::CleanupReach::Remote, "Your server")
    }

    /// A session on `engine`, gate off, cleaning up on this machine with
    /// `fake`.
    fn cleaning(engine: Lines, fake: FakeCleanup) -> Session {
        cleaning_with(engine, local(fake))
    }

    fn cleaning_with(engine: Lines, with: cleanup::Cleanup) -> Session {
        let mut s = Session::with_engine(
            Box::new(engine),
            seed(),
            "seed",
            Config {
                vad_gate: false,
                ..Config::default()
            },
        );
        s.detector = energy;
        s.cleanup = Some(with);
        // The old paste budget, which every test here was written against:
        // well short of `SLOW`, and a second tick never comes.
        s.enhancement_wait = Wait {
            tick: ENHANCEMENT_TICK,
            cap: PASTE_BUDGET,
        };
        s
    }

    /// Records for `polls` polls, then throws the recording away.
    fn discarded_after(polls: usize) -> impl FnMut() -> StopSignal {
        let mut seen = 0;
        move || {
            seen += 1;
            if seen > polls {
                StopSignal::Discard
            } else {
                StopSignal::Continue
            }
        }
    }

    /// `Lines` under another engine's id, so the pipeline is that engine's.
    struct Named(&'static str, Lines);

    impl SttEngine for Named {
        fn id(&self) -> &'static str {
            self.0
        }
        fn caps(&self) -> EngineCaps {
            self.1.caps()
        }
        fn stream(&self, ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            self.1.stream(ctx)
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            self.1.transcribe(audio, ctx)
        }
    }

    /// Released after `polls` polls of a microphone that always hears
    /// something.
    fn dictate_for(s: &Session, polls: usize) -> Dictation {
        s.dictate_from(
            FakeSource {
                polls_left: 100,
                dies: false,
            },
            &mut after(polls),
            &mut || false,
            &mut |_| {},
        )
        .unwrap()
    }

    /// What the same lines paste with cleanup off.
    fn tier0_of(s: &Session, lines: &[&str]) -> String {
        let transcripts: Vec<Transcript> = lines.iter().map(|line| final_line(line)).collect();
        s.pipeline.assemble(&transcripts).to_plain()
    }

    #[test]
    fn cleanup_that_answers_in_time_is_pasted_with_the_dictionary_having_the_last_word() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let s = cleaning(
            Lines {
                during: vec!["kubectl ran"],
                at_release: vec![],
            },
            fake,
        );
        let d = dictate_for(&s, 4);
        assert_eq!(
            tier0_of(&s, &["kubectl ran"]),
            "kubectl ran",
            "Tier 0 alone adds no full stop"
        );
        assert_eq!(d.document.to_plain(), "kubectl ran.");
        assert!(d.late_cleanup.is_none(), "nothing was left out");
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
        assert!(!d.discarded);
        assert!(d.timing.tier1_ms.is_some());

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].0,
            paragraph("Kubectl ran"),
            "Tier 0's blocks, never the raw transcript"
        );
        assert_ne!(
            requests[0].1,
            std::thread::current().id(),
            "never on the record loop"
        );
    }

    #[test]
    fn cleanup_past_the_budget_pastes_tier_0_and_the_cleaned_text_arrives_later() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), |_| SLOW);
        let s = cleaning(
            Lines {
                during: vec![],
                at_release: vec!["kubectl ran"],
            },
            fake,
        );
        let mut d = dictate_for(&s, 2);
        assert_eq!(d.document.to_plain(), "kubectl ran", "Tier 0 is pasted");
        let waited = d.timing.tier1_ms.expect("cleanup was on");
        assert!(
            waited <= 700.0,
            "waited about the budget and no longer: {waited} ms"
        );
        assert!(
            d.timing.total_ms >= PASTE_BUDGET.as_secs_f64() * 1000.0,
            "the paste waited out the budget from release: {} ms",
            d.timing.total_ms
        );

        assert_eq!(d.enhancement, Some(EnhancementOutcome::OutOfTime));
        let late = d.late_cleanup.take().expect("one segment was still out");
        assert_eq!(late.outstanding(), 1);
        let done = late.wait();
        assert_eq!(done.document.to_plain(), "kubectl ran.");
        assert!(!done.partial, "every segment came back");
        assert!(done.ms >= 900.0, "timed to the answer: {} ms", done.ms);
        assert_eq!(
            d.document.to_plain(),
            "kubectl ran",
            "what was pasted stays what was pasted"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_rejected_failed_or_panicking_segment_keeps_tier_0_and_the_rest_are_still_cleaned() {
        let (fake, requests) = FakeCleanup::new(
            |call, _| match call {
                // More words than were said: the guard throws it back.
                0 => Ok("The build passed, I think.".into()),
                1 => Err(macalla_core::Error::Enhance {
                    backend: "fake-cleanup",
                    message: "the server said no".into(),
                }),
                2 => panic!("the adapter fell over"),
                _ => Ok("We can ship!".into()),
            },
            at_once,
        );
        // Not "first ..., second ...": Tier 0 rightly hears that as a list.
        let lines = [
            "the build passed.",
            "the tests ran.",
            "the docs are done.",
            "we can ship",
        ];
        let s = cleaning(
            Lines {
                during: lines[..3].to_vec(),
                at_release: vec![lines[3]],
            },
            fake,
        );
        let d = dictate_for(&s, 6);
        assert_eq!(
            tier0_of(&s, &lines),
            "The build passed. The tests ran. The docs are done. We can ship"
        );
        assert_eq!(
            d.document.to_plain(),
            "The build passed. The tests ran. The docs are done. We can ship!"
        );
        assert!(d.late_cleanup.is_none());
        assert_eq!(
            d.enhancement,
            Some(EnhancementOutcome::Applied),
            "some segments came back cleaned, so the paste is not without it"
        );
        assert_eq!(requests.lock().unwrap().len(), 4, "one request a segment");
    }

    #[test]
    fn a_first_segment_back_in_time_and_a_last_one_not_are_pasted_mixed_and_seamed() {
        let (fake, _) = FakeCleanup::new(
            |call, _| {
                Ok(match call {
                    0 => "Kubectl ran,".into(),
                    _ => "And then it stopped.".into(),
                })
            },
            |call| if call == 0 { Duration::ZERO } else { SLOW },
        );
        let lines = ["cube control ran", "and then it stopped"];
        let s = cleaning(
            Lines {
                during: vec![lines[0]],
                at_release: vec![lines[1]],
            },
            fake,
        );
        let mut d = dictate_for(&s, 6);
        assert_eq!(tier0_of(&s, &lines), "kubectl ran and then it stopped");
        assert_eq!(
            d.document.to_plain(),
            "kubectl ran, and then it stopped",
            "the first cleaned, the second Tier 0, and one sentence across the seam"
        );
        let done = d.late_cleanup.take().expect("the second was out").wait();
        assert_eq!(
            done.document.to_plain(),
            "kubectl ran, and then it stopped."
        );
    }

    #[test]
    fn with_cleanup_off_there_is_no_wait_and_nothing_late() {
        let s = {
            let mut s = Session::with_engine(
                Box::new(Lines {
                    during: vec!["kubectl ran"],
                    at_release: vec!["and then it stopped"],
                }),
                seed(),
                "seed",
                Config {
                    vad_gate: false,
                    ..Config::default()
                },
            );
            s.detector = energy;
            s
        };
        let mut enhancing = 0;
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(4),
                &mut || false,
                &mut |p| {
                    if let Progress::Enhancing { .. } = p {
                        enhancing += 1;
                    }
                },
            )
            .unwrap();
        assert_eq!(
            d.document.to_plain(),
            tier0_of(&s, &["kubectl ran", "and then it stopped"])
        );
        assert_eq!(d.timing.tier1_ms, None);
        assert!(d.late_cleanup.is_none());
        assert_eq!(d.enhancement, None);
        assert_eq!(enhancing, 0, "nothing is waited for, so nothing says so");
    }

    #[test]
    fn nothing_said_sends_nothing_to_cleanup() {
        // Silence through the gate: the engine hears nothing and commits
        // nothing.
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let mut s = Session::with_engine(
            Box::new(Lines {
                during: vec!["never heard"],
                at_release: vec![],
            }),
            seed(),
            "seed",
            Config::default(),
        );
        s.detector = threshold;
        s.cleanup = Some(local(fake));
        let d = s
            .dictate_from(
                Utterance {
                    poll: 0,
                    polls: 20,
                    loud: 0..0,
                },
                &mut after(20),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(d.heard_speech, Some(false));
        assert_eq!(requests.lock().unwrap().len(), 0);
        assert_eq!(d.enhancement, None, "nothing to enhance");
        assert_eq!(d.timing.tier1_ms, None);
        assert!(d.late_cleanup.is_none());

        // An engine that commits only blank text.
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let s = cleaning(
            Lines {
                during: vec![],
                at_release: vec!["   "],
            },
            fake,
        );
        let d = dictate_for(&s, 2);
        assert!(d.document.to_plain().is_empty());
        assert_eq!(requests.lock().unwrap().len(), 0);
        assert_eq!(d.timing.tier1_ms, None);
    }

    #[test]
    fn a_cloud_dictation_cleans_the_cloud_engines_text_and_never_the_preview() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        s.cleanup = Some(local(fake));
        let mut d = dictate_for(&s, 3);
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert_eq!(d.document.to_plain(), "kubectl ran.");
        assert_eq!(
            d.local_document.as_ref().unwrap().to_plain(),
            "Hello there",
            "the preview's text is kept, uncleaned"
        );
        assert!(d.timing.tier1_ms.is_some());
        assert!(d.late_cleanup.take().is_none());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the cloud text only");
        assert_eq!(requests[0].0, paragraph("Kubectl ran"));
    }

    #[test]
    fn in_a_cloud_dictation_the_wait_counts_from_the_answer_not_from_release() {
        // The answer comes after release plus the whole wait. Enhancement is
        // still waited for, because the person was waiting for the answer,
        // not for the key.
        let (cloud, cloud_go, _finished) = CloudFake::answering("kubectl ran").gated();
        let (fake, _) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (fake, enhancement_go) = fake.gated(every_call);
        let mut s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        s.cleanup = Some(local(fake));
        // A wait no runner stalls past, and an answer a second after release:
        // counted from release, the first report would come after that
        // second; counted from the answer, it comes a tick after it.
        let answer_after = Duration::from_secs(1);
        s.enhancement_wait = Wait {
            tick: Duration::from_millis(10),
            cap: ROOMY,
        };
        let mut first_report = None;
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(1),
                &mut || false,
                &mut |p| match p {
                    // Answered a second after release.
                    Progress::CloudWaiting { elapsed, .. } if elapsed > answer_after => {
                        let _ = cloud_go.send(());
                    }
                    // Answered once the wait says it is waiting: it never
                    // would if it had counted from release.
                    Progress::Enhancing { elapsed, .. } => {
                        first_report.get_or_insert(elapsed);
                        let _ = enhancement_go.send(());
                    }
                    _ => {}
                },
            )
            .unwrap();
        let cloud = d.cloud.as_ref().unwrap();
        assert_eq!(cloud.outcome, CloudOutcome::Transcribed);
        assert!(
            cloud.ms > answer_after.as_secs_f64() * 1000.0,
            "the answer came a second after release: {} ms",
            cloud.ms
        );
        let first_report = first_report.expect("the wait was on after the answer");
        assert!(
            first_report.as_secs_f64() * 1000.0 < cloud.ms,
            "counted from the answer, not from release: {first_report:?} against {} ms",
            cloud.ms
        );
        assert_eq!(d.document.to_plain(), "kubectl ran.");
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
        assert!(d.late_cleanup.is_none());
    }

    #[test]
    fn a_failed_cloud_request_sends_nothing_to_cleanup() {
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("the provider said no"),
            false,
        );
        s.cleanup = Some(local(fake));
        let mut d = dictate_for(&s, 3);
        assert!(matches!(outcome(&d), CloudOutcome::Failed(_)));
        assert_eq!(requests.lock().unwrap().len(), 0);
        assert_eq!(d.timing.tier1_ms, None);
        assert_eq!(paste(&mut d).unwrap(), Paste::Withheld);
    }

    #[test]
    fn a_cleanup_that_cannot_start_leaves_a_session_that_dictates_and_says_why() {
        use macalla_platform::secrets::MemoryStore;
        let mut s = Session::with_engine(
            Box::new(FakeEngine::new(EngineCaps::stream_first())),
            seed(),
            "seed",
            Config {
                vad_gate: false,
                ..Config::default()
            },
        );
        s.detector = energy;
        // No key stored, so it cannot be built; nothing is sent to find out.
        s.replace_cleanup(
            &[choice::EnhancerChoice::Anthropic {
                model: "claude-haiku-4-5".into(),
                key: choice::KeySource::Stored("anthropic".into()),
                reasoning: None,
            }],
            &MemoryStore::new(),
        );
        assert!(s.cleanup.is_none());
        let reason = s.cleanup_error().expect("the reason is kept").to_string();
        assert!(reason.contains("anthropic"), "{reason}");

        let d = dictate_for(&s, 3);
        assert_eq!(d.document.to_plain(), "Hello there");
        assert_eq!(d.timing.tier1_ms, None);
    }

    #[test]
    fn cleanup_built_choices_names_only_what_actually_built() {
        // Three ticked, in order; Gemini's key is never stored, so it
        // never builds -- `cleanup_built_choices` must carry only OpenAI
        // and Anthropic, in that order, never all three, and never
        // Gemini standing in for the one that failed.
        use macalla_platform::secrets::MemoryStore;
        let store = MemoryStore::new();
        providers::store_key(&store, "openai", "sk-test").unwrap();
        providers::store_key(&store, "anthropic", "sk-test").unwrap();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Anthropic {
                    model: "claude-test".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Endpoint {
                    url: "https://generativelanguage.googleapis.com/v1beta/openai".into(),
                    model: "gemini-test".into(),
                    key: choice::KeySource::Stored("gemini".into()),
                    reasoning: None,
                },
            ],
            &store,
        );
        let built: Vec<String> = s
            .cleanup_built_choices()
            .iter()
            .map(|c| c.destination().into_owned())
            .collect();
        assert_eq!(built, vec!["OpenAI".to_string(), "Anthropic".to_string()]);

        // Nothing built at all: empty, not the ticked list.
        let store = MemoryStore::new();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[choice::EnhancerChoice::Anthropic {
                model: "claude-test".into(),
                key: choice::KeySource::Stored("anthropic".into()),
                reasoning: None,
            }],
            &store,
        );
        assert!(s.cleanup_built_choices().is_empty());
    }

    /// A cleanup provider whose `is_available` a test flips after it is
    /// built, so [`Session::cleanup_now`] is driven without a real Apple
    /// Intelligence switch, which differs from Mac to Mac.
    struct ToggleCleanup(Arc<std::sync::atomic::AtomicBool>);

    impl Enhancer for ToggleCleanup {
        fn caps(&self) -> macalla_core::enhance::EnhancerCaps {
            macalla_core::enhance::EnhancerCaps {
                id: "toggle-cleanup",
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn enhance(
            &self,
            blocks: &[macalla_core::block::Block],
            _terms: &[String],
        ) -> macalla_core::Result<Vec<macalla_core::block::Block>> {
            Ok(blocks.to_vec())
        }
    }

    #[test]
    fn cleanup_now_follows_availability_on_the_same_session_without_a_reopen() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let switch = Arc::new(AtomicBool::new(true));
        let picked = choice::EnhancerChoice::AppleIntelligence { guided: true };
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.cleanup = Some(
            cleanup::Cleanup::new(
                Arc::new(ToggleCleanup(Arc::clone(&switch))),
                choice::CleanupReach::Local,
                "Apple Intelligence",
            )
            .built_from(vec![picked]),
        );

        let serving = |now: CleanupNow<'_>| match now {
            CleanupNow::Serving { built, skipped } => {
                assert!(
                    matches!(
                        built,
                        [choice::EnhancerChoice::AppleIntelligence { guided: true }]
                    ),
                    "{built:?}"
                );
                assert!(skipped.is_empty());
            }
            other => panic!("expected Serving, got {other:?}"),
        };
        serving(s.cleanup_now());

        switch.store(false, Ordering::Relaxed);
        match s.cleanup_now() {
            CleanupNow::NotReady(built) => assert!(
                matches!(
                    built,
                    [choice::EnhancerChoice::AppleIntelligence { guided: true }]
                ),
                "{built:?}"
            ),
            other => panic!("expected NotReady, got {other:?}"),
        }

        switch.store(true, Ordering::Relaxed);
        serving(s.cleanup_now());
    }

    #[test]
    fn cleanup_now_is_failed_for_a_pool_with_no_keys_and_off_with_nothing_ticked() {
        use macalla_platform::secrets::MemoryStore;
        let store = MemoryStore::new();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        assert!(matches!(s.cleanup_now(), CleanupNow::Off));

        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Anthropic {
                    model: "claude-test".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
            ],
            &store,
        );
        match s.cleanup_now() {
            CleanupNow::Failed(errors) => {
                let providers: Vec<&str> = errors
                    .iter()
                    .map(|e| match e {
                        Error::NeedsKey { provider } => provider.as_str(),
                        other => panic!("expected NeedsKey, got {other:?}"),
                    })
                    .collect();
                assert_eq!(providers, vec!["openai", "anthropic"]);
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        s.replace_cleanup(&[], &store);
        assert!(matches!(s.cleanup_now(), CleanupNow::Off));
    }

    #[test]
    fn cleanup_now_serves_a_partly_built_pool_with_its_skipped_member() {
        use macalla_platform::secrets::MemoryStore;
        let store = MemoryStore::new();
        providers::store_key(&store, "anthropic", "sk-test").unwrap();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Anthropic {
                    model: "claude-test".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
            ],
            &store,
        );
        match s.cleanup_now() {
            CleanupNow::Serving { built, skipped } => {
                assert_eq!(built.len(), 1);
                assert_eq!(built[0].provider_id(), Some("anthropic"));
                assert!(
                    matches!(skipped, [Error::NeedsKey { provider }] if provider == "openai"),
                    "{skipped:?}"
                );
            }
            other => panic!("expected Serving, got {other:?}"),
        }
    }

    #[test]
    fn the_first_cleanup_choice_that_builds_is_the_one_used() {
        use macalla_platform::secrets::MemoryStore;
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
                // Built, never asked: nothing in this test dictates.
                choice::EnhancerChoice::Endpoint {
                    url: "http://127.0.0.1:9".into(),
                    model: "m".into(),
                    key: choice::KeySource::None,
                    reasoning: None,
                },
            ],
            &MemoryStore::new(),
        );
        assert_eq!(s.cleanup.as_ref().map(|c| c.caps().id), Some("endpoint"));
        assert_eq!(s.cleanup_error(), None);
        assert_eq!(s.cleanup_destination(), Some("Local"));

        s.replace_cleanup(&[], &MemoryStore::new());
        assert!(s.cleanup.is_none());
        assert_eq!(s.cleanup_error(), None, "off is not a failure");
    }

    #[test]
    fn apple_intelligence_is_kept_when_it_cannot_serve_yet_and_the_reason_is_too() {
        use macalla_platform::secrets::MemoryStore;
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        // Construction and a prewarm; nothing is asked.
        s.replace_cleanup(
            &[choice::EnhancerChoice::AppleIntelligence { guided: true }],
            &MemoryStore::new(),
        );
        let kept = s
            .cleanup
            .as_ref()
            .expect("built whatever the system reports now");
        assert_eq!(kept.reach(), choice::CleanupReach::Local);
        assert_eq!(s.cleanup_destination(), Some("Apple Intelligence"));
        assert_eq!(
            s.cleanup_error().map(ToString::to_string),
            choice::AppleIntelligenceAvailability::now()
                .explain()
                .map(str::to_string)
        );
    }

    #[test]
    fn a_full_pool_build_records_nothing_skipped() {
        use macalla_platform::secrets::MemoryStore;
        let keys = MemoryStore::new();
        keys.set("provider-key.openai", "test-key").unwrap();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
            ],
            &keys,
        );
        assert_eq!(s.cleanup_error(), None);
        assert!(s.cleanup_skipped().is_empty(), "{:?}", s.cleanup_skipped());
    }

    #[test]
    fn a_partial_pool_build_records_exactly_its_skipped_members_in_order() {
        use macalla_platform::secrets::MemoryStore;
        let keys = MemoryStore::new();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[
                // Builds.
                choice::EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
                // No key stored: `Error::NeedsKey`.
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
                // ElevenLabs offers no text model: `Error::WrongKind`,
                // whatever its key.
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.elevenlabs.io/v1".into(),
                    model: "eleven-text".into(),
                    key: choice::KeySource::Stored("elevenlabs".into()),
                    reasoning: None,
                },
            ],
            &keys,
        );
        // Anthropic built, so `cleanup_error` is `None` -- the two skipped
        // members are only on `cleanup_skipped`.
        assert_eq!(s.cleanup_error(), None);
        assert_eq!(s.cleanup_destination(), Some("Anthropic"));
        let skipped = s.cleanup_skipped();
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(
            matches!(&skipped[0], Error::NeedsKey { provider } if provider == "openai"),
            "{skipped:?}"
        );
        assert!(
            matches!(
                &skipped[1],
                Error::WrongKind { provider, kind: providers::Kind::Text }
                    if provider == "elevenlabs"
            ),
            "{skipped:?}"
        );
    }

    #[test]
    fn a_reply_that_ends_a_sentence_tier_0_left_open_does_not_split_it_at_the_seam() {
        let (fake, _) = FakeCleanup::new(
            |call, text| {
                Ok(if call == 0 {
                    "Kubectl ran.".into()
                } else {
                    text.into()
                })
            },
            at_once,
        );
        let lines = ["kubectl ran", "and then it stopped"];
        let s = cleaning(
            Lines {
                during: vec![lines[0]],
                at_release: vec![lines[1]],
            },
            fake,
        );
        let d = dictate_for(&s, 6);
        let pasted = d.document.to_plain();
        assert!(!pasted.contains(". And"), "{pasted}");
        assert_eq!(pasted, "kubectl ran and then it stopped");
        assert_eq!(pasted, tier0_of(&s, &lines));
    }

    #[test]
    fn chosen_cleanup_runs_where_the_plan_says_tier_1_has_no_work() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = Session::with_engine(
            Box::new(Named(
                "parakeet-tdt-v3",
                Lines {
                    during: vec!["kubectl ran"],
                    at_release: vec![],
                },
            )),
            seed(),
            "seed",
            Config {
                vad_gate: false,
                ..Config::default()
            },
        );
        s.detector = energy;
        s.cleanup = Some(local(fake));
        assert!(
            !s.plan().tier1,
            "Parakeet punctuates, so the plan sees no work"
        );
        let d = dictate_for(&s, 4);
        assert_eq!(requests.lock().unwrap().len(), 1, "the choice wins");
        assert_eq!(d.document.to_plain(), "kubectl ran.");
    }

    #[test]
    fn a_remote_provider_is_sent_nothing_while_recording_and_the_whole_document_once_at_release() {
        let (fake, requests) = FakeCleanup::new(
            |_, _| Ok("Kubectl ran, and then it stopped.".into()),
            at_once,
        );
        let lines = ["cube control ran", "and then it stopped"];
        let s = waiting_up_to(
            cleaning_with(
                Lines {
                    during: vec![lines[0]],
                    at_release: vec![lines[1]],
                },
                remote(fake),
            ),
            ROOMY,
        );
        let heard = Arc::clone(&requests);
        let mut polls = 0;
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut || {
                    assert_eq!(heard.lock().unwrap().len(), 0, "nothing while recording");
                    polls += 1;
                    polls > 6
                },
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        // The rule this test used to prove was that a remote provider is sent
        // nothing until a trigger keeps the dictation and asks. Enhancement
        // now holds the paste for up to ENHANCEMENT_WAIT, so the whole
        // document goes once at key release, and its answer is pasted.
        assert_eq!(
            d.document.to_plain(),
            "kubectl ran, and then it stopped.",
            "its answer is pasted"
        );
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
        assert!(d.timing.tier1_ms.is_some(), "the paste waited for it");
        assert!(d.late_cleanup.is_none(), "nothing is left to ask for");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "once, before dictate returned");
        assert_eq!(
            requests[0].0,
            paragraph("kubectl ran and then it stopped"),
            "the whole Tier 0 document"
        );
        assert_ne!(
            requests[0].1,
            std::thread::current().id(),
            "on a request thread, never the record loop"
        );
    }

    #[test]
    fn a_remote_provider_is_sent_the_text_at_release_even_when_nothing_keeps_it() {
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let s = waiting_up_to(
            cleaning_with(
                Lines {
                    during: vec!["kubectl ran"],
                    at_release: vec![],
                },
                remote(fake),
            ),
            ROOMY,
        );
        let d = dictate_for(&s, 4);
        // This test used to prove the opposite: a dictation nobody kept was
        // never sent. With Enhancement holding the paste, a remote provider
        // is sent the text at key release whenever Enhancement is on and Esc
        // twice was not pressed -- with History off, and from a plain
        // `macalla listen`, too.
        assert!(d.late_cleanup.is_none(), "nothing waits to be asked for");
        drop(d);
        assert_eq!(requests.lock().unwrap().len(), 1, "sent, whoever keeps it");
    }

    #[test]
    fn a_cloud_dictation_with_a_remote_provider_sends_the_cloud_text_as_it_answers_and_pastes_it() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        s.cleanup = Some(remote(fake));
        let s = waiting_up_to(s, ROOMY);
        let d = dictate_for(&s, 3);
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        // This test used to prove that Tier 0 was pasted and the cloud text
        // sent only when a trigger asked. Enhancement now holds the paste, so
        // the cloud engine's text goes as it answers, and the answer is
        // pasted.
        assert_eq!(d.document.to_plain(), "kubectl ran.");
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
        assert!(d.timing.tier1_ms.is_some());
        assert!(d.late_cleanup.is_none());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the cloud text, once");
        assert_eq!(
            requests[0].0,
            paragraph("kubectl ran"),
            "the whole Tier 0 document, the dictionary's last word included"
        );
    }

    #[test]
    fn a_discarded_dictation_sends_nothing_more_to_cleanup_on_this_machine_and_waits_for_nothing() {
        let (fake, requests) = FakeCleanup::new(
            |_, text| Ok(format!("{text},")),
            |_| Duration::from_millis(400),
        );
        let (fake, started) = fake.announcing();
        let s = cleaning(
            Lines {
                during: vec!["the red door", "the blue door", "the green door"],
                at_release: vec!["the last door"],
            },
            fake,
        );
        // The first segment is committed on the first poll. Esc twice once
        // it is in flight, however long a busy machine takes to start it.
        let mut polls = 0;
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut || {
                    polls += 1;
                    if polls == 1 {
                        return StopSignal::Continue;
                    }
                    started
                        .recv_timeout(Duration::from_secs(10))
                        .expect("the first segment went to cleanup");
                    StopSignal::Discard
                },
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert!(d.discarded);
        assert!(d.late_cleanup.is_none());
        assert_eq!(d.timing.tier1_ms, None, "nothing was waited for");
        // Long enough for the request in flight to finish and anything queued
        // behind it to have started.
        assert!(
            started.recv_timeout(Duration::from_millis(1_000)).is_err(),
            "nothing started after the discard"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "the one in flight at the discard, and nothing after it"
        );
        assert_eq!(requests[0].0, paragraph("The red door"));
    }

    #[test]
    fn a_discarded_dictation_sends_nothing_at_all_to_a_remote_provider() {
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let s = cleaning_with(
            Lines {
                during: vec!["the red door", "the blue door"],
                at_release: vec!["the last door"],
            },
            remote(fake),
        );
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut discarded_after(3),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert!(d.discarded);
        assert!(d.late_cleanup.is_none(), "nothing to ask for later either");
        drop(d);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(requests.lock().unwrap().len(), 0);
    }

    #[test]
    fn esc_twice_during_the_release_wait_ends_it_and_starts_nothing_queued() {
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(format!("{text},")), |_| SLOW);
        let (fake, started) = fake.announcing();
        let s = cleaning(
            Lines {
                during: vec!["the red door", "the blue door"],
                at_release: vec!["the last door"],
            },
            fake,
        );
        let mut polls = 0;
        let mut asked = 0;
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                // Released once the first segment is in flight, so the wait
                // has it to wait for however slow the machine is to start it.
                &mut || {
                    polls += 1;
                    if polls == 1 {
                        return false;
                    }
                    started
                        .recv_timeout(Duration::from_secs(10))
                        .expect("the first segment went to cleanup");
                    true
                },
                // Not yet when the engine has finished; Esc twice by the
                // time the wait asks.
                &mut || {
                    asked += 1;
                    asked > 1
                },
                &mut |_| {},
            )
            .unwrap();
        assert!(asked >= 2, "asked while waiting");
        assert!(d.discarded);
        let waited = d.timing.tier1_ms.expect("the wait began");
        assert!(
            waited < PASTE_BUDGET.as_secs_f64() * 1000.0,
            "ended before the budget: {waited} ms"
        );
        assert!(d.late_cleanup.is_none());
        assert!(
            started
                .recv_timeout(SLOW + Duration::from_millis(500))
                .is_err(),
            "nothing queued started"
        );
        assert_eq!(requests.lock().unwrap().len(), 1, "only the one in flight");
    }

    /// Three segments still out on this machine when the paste budget ends.
    fn three_local_segments_out() -> (Session, Arc<Mutex<Vec<CleanupRequest>>>) {
        let (fake, requests) = FakeCleanup::new(
            |_, text| Ok(format!("{text},")),
            |_| Duration::from_millis(400),
        );
        let s = cleaning(
            Lines {
                during: vec![],
                at_release: vec!["the red door", "the blue door", "the green door"],
            },
            fake,
        );
        (s, requests)
    }

    #[test]
    fn building_the_cleanup_again_lets_a_local_late_worker_finish() {
        let (mut s, requests) = three_local_segments_out();
        let mut d = dictate_for(&s, 2);
        let late = d.late_cleanup.take().expect("all three were out");
        assert_eq!(late.outstanding(), 3);
        // A setting changed -- an engine chosen, a key saved -- and the
        // session builds its cleanup again. Apple Intelligence or a loopback
        // server holds no key that could have gone, so its segments finish.
        s.replace_cleanup(&[], &macalla_platform::secrets::MemoryStore::new());

        let done = late.wait();
        assert!(!done.partial, "every segment came back");
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn replacing_a_local_cleanup_with_a_pool_names_the_pool_and_lets_the_old_worker_finish() {
        use macalla_platform::secrets::MemoryStore;
        let (mut s, requests) = three_local_segments_out();
        let mut d = dictate_for(&s, 2);
        let late = d.late_cleanup.take().expect("all three were out");
        assert_eq!(late.outstanding(), 3);

        let keys = MemoryStore::new();
        keys.set("provider-key.openai", "test-key").unwrap();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
            ],
            &keys,
        );
        assert_eq!(
            s.cleanup_destination(),
            Some("OpenAI, falling back to Anthropic")
        );

        let done = late.wait();
        assert!(
            !done.partial,
            "the old local worker finished against the old provider"
        );
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn replacing_a_pool_with_off_leaves_no_destination_and_no_error() {
        use macalla_platform::secrets::MemoryStore;
        let keys = MemoryStore::new();
        keys.set("provider-key.openai", "test-key").unwrap();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
            ],
            &keys,
        );
        assert!(s.cleanup_destination().is_some(), "the pool built");

        s.replace_cleanup(&[], &keys);
        assert_eq!(s.cleanup_destination(), None);
        assert_eq!(s.cleanup_error(), None);
    }

    #[test]
    fn replacing_off_with_apple_intelligence_names_it_and_keeps_its_own_reason() {
        use macalla_platform::secrets::MemoryStore;
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(&[], &MemoryStore::new());
        assert_eq!(s.cleanup_destination(), None);

        s.replace_cleanup(
            &[choice::EnhancerChoice::AppleIntelligence { guided: true }],
            &MemoryStore::new(),
        );
        assert_eq!(s.cleanup_destination(), Some("Apple Intelligence"));
        assert_eq!(
            s.cleanup_error().map(ToString::to_string),
            choice::AppleIntelligenceAvailability::now()
                .explain()
                .map(str::to_string)
        );
    }

    #[test]
    fn a_good_choice_clears_a_needs_key_error() {
        use macalla_platform::secrets::MemoryStore;
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[choice::EnhancerChoice::Anthropic {
                model: "claude-haiku-4-5".into(),
                key: choice::KeySource::Stored("anthropic".into()),
                reasoning: None,
            }],
            &MemoryStore::new(),
        );
        assert!(s.cleanup_error().is_some(), "no key stored");

        s.replace_cleanup(
            &[choice::EnhancerChoice::Endpoint {
                url: "http://127.0.0.1:9".into(),
                model: "m".into(),
                key: choice::KeySource::None,
                reasoning: None,
            }],
            &MemoryStore::new(),
        );
        assert_eq!(s.cleanup_error(), None);
        assert_eq!(s.cleanup_destination(), Some("Local"));
    }

    #[test]
    fn replacing_cleanup_leaves_the_chosen_engines_and_held_recording_alone() {
        use macalla_platform::secrets::MemoryStore;
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::failing("the provider said no"),
            false,
        );
        let mut d = dictate_for(&s, 3);
        assert!(matches!(outcome(&d), CloudOutcome::Failed(_)));
        let _ = paste(&mut d);
        assert!(s.can_send_again(), "held after a failure");

        let engine_before = s.engine_id();
        let cloud_before = s.cloud_provider().map(str::to_string);
        s.replace_cleanup(
            &[choice::EnhancerChoice::AppleIntelligence { guided: true }],
            &MemoryStore::new(),
        );

        assert_eq!(s.engine_id(), engine_before);
        assert_eq!(s.cloud_provider().map(str::to_string), cloud_before);
        assert!(s.can_send_again(), "the held recording survives a swap");
    }

    #[test]
    fn replacing_cleanup_refreshes_cleanup_skipped() {
        use macalla_platform::secrets::MemoryStore;
        let keys = MemoryStore::new();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
            ],
            &keys,
        );
        assert!(!s.cleanup_skipped().is_empty(), "openai has no key yet");

        keys.set("provider-key.openai", "test-key").unwrap();
        s.replace_cleanup(
            &[
                choice::EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: choice::KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
                choice::EnhancerChoice::Endpoint {
                    url: "https://api.openai.com/v1".into(),
                    model: "gpt-test".into(),
                    key: choice::KeySource::Stored("openai".into()),
                    reasoning: None,
                },
            ],
            &keys,
        );
        assert!(s.cleanup_skipped().is_empty(), "{:?}", s.cleanup_skipped());
    }

    #[test]
    fn closing_the_session_lets_a_local_late_worker_finish() {
        let (s, requests) = three_local_segments_out();
        let mut d = dictate_for(&s, 2);
        let late = d.late_cleanup.take().expect("all three were out");
        drop(s);

        let done = late.wait();
        assert!(!done.partial, "every segment came back");
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn dropping_late_cleanup_starts_none_of_its_queued_segments() {
        let (fake, requests) = FakeCleanup::new(
            |_, text| Ok(format!("{text},")),
            |_| Duration::from_millis(600),
        );
        let s = cleaning(
            Lines {
                during: vec![],
                at_release: vec!["the red door", "the blue door", "the green door"],
            },
            fake,
        );
        let mut d = dictate_for(&s, 2);
        let late = d.late_cleanup.take().expect("all three were out");
        assert_eq!(late.outstanding(), 3);
        drop(late);
        std::thread::sleep(Duration::from_millis(1_500));
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "the one in flight finished; the queued two never started"
        );
    }

    #[test]
    fn sending_again_sends_a_remote_provider_the_text_at_once_unless_told_to_go_without() {
        // Send again pastes nothing, so nothing waits for Enhancement: the
        // request goes as the cloud engine answers, and its answer is late,
        // for History. This test used to prove that dropping that late
        // cleanup sent the provider nothing; the request is now out before
        // there is anything to drop, and what keeps it from a provider the
        // person switched away from is `cancel` saying so as it is about to
        // go.
        let cloud = CloudFake::answering("kubectl ran").then([
            Reply::Error("HTTP 500"),
            Reply::Text("kubectl ran"),
            Reply::Error("HTTP 500"),
        ]);
        let mut s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        let d = s
            .dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        assert!(matches!(outcome(&d), CloudOutcome::Failed(_)));
        assert!(s.can_send_again(), "held after the failure");

        // Enhancement set only now, as it would be by the time a Send-again
        // wait's own `send_again` call runs, well after the dictation that
        // held the recording.
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(format!("{text}!")), at_once);
        let (fake, started) = fake.announcing();
        let (fake, go) = fake.gated(every_call);
        s.cleanup = Some(remote(fake));

        let again = s
            .send_again(&mut || false, &mut |_| {})
            .expect("a recording is held");
        assert_eq!(again.cloud.outcome, CloudOutcome::Transcribed);
        let late = again.late_cleanup.expect("the answer comes later");
        until_started(&started, 0);
        go.send(()).unwrap();
        assert!(
            late.wait().document.to_plain().ends_with('!'),
            "the provider's answer, for History"
        );

        // The next failure holds a recording again. This time Enhancement was
        // turned off while it was being sent again.
        let d = s
            .dictate_from(fake_source(), &mut after(3), &mut || false, &mut |_| {})
            .unwrap();
        assert!(matches!(outcome(&d), CloudOutcome::Failed(_)));
        let again = s
            .send_again(&mut || Waiting::WithoutEnhancement, &mut |_| {})
            .expect("a recording is held");
        assert_eq!(
            again.cloud.outcome,
            CloudOutcome::Transcribed,
            "a transcription is not Enhancement"
        );
        assert!(again.late_cleanup.is_none(), "nothing was sent to wait for");
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "only the first send again's"
        );
    }

    #[test]
    fn the_next_dictation_to_start_cleanup_supersedes_the_last_ones_queued_segments() {
        let (fake, requests) = FakeCleanup::new(
            |_, text| Ok(format!("{text},")),
            |_| Duration::from_millis(800),
        );
        let s = cleaning(
            Lines {
                during: vec![],
                at_release: vec!["the red door", "the blue door", "the green door"],
            },
            fake,
        );
        let mut first = dictate_for(&s, 2);
        let first_late = first.late_cleanup.take().expect("out at the budget");
        // Not waited for yet. The next dictation starts cleanup of its own.
        let mut second = dictate_for(&s, 2);
        drop(second.late_cleanup.take().expect("out at the budget too"));
        let done = first_late.wait();
        assert_eq!(
            done.document.to_plain(),
            "The red door, the blue door the green door",
            "only the segment in flight came back"
        );
        assert!(done.partial, "the queued two were never asked");
        std::thread::sleep(Duration::from_millis(1_500));
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "one in flight for each dictation, and nothing queued behind them"
        );
    }

    /// A cloud pool of `members`, in the order given, as a session holds one.
    fn pool(members: Vec<(&str, FakeCleanup)>) -> cleanup::Cleanup {
        let members = members
            .into_iter()
            .map(|(provider, fake)| failover::Member {
                enhancer: Box::new(fake),
                provider: provider.to_string(),
                model: format!("{provider}-test"),
            })
            .collect();
        cleanup::Cleanup::pool(
            failover::Failover::new(members),
            "OpenAI, falling back to Anthropic".to_string(),
        )
    }

    fn failing(_: usize, _: &str) -> macalla_core::Result<String> {
        Err(macalla_core::Error::Enhance {
            backend: "fake-cleanup",
            message: "the server answered 503".into(),
        })
    }

    #[test]
    fn a_pool_whose_first_model_fails_is_answered_by_the_next_inside_the_same_wait() {
        let (first, first_requests) = FakeCleanup::new(failing, at_once);
        let (second, second_requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let s = waiting_up_to(
            cleaning_with(
                Lines {
                    during: vec!["cube control ran"],
                    at_release: vec![],
                },
                pool(vec![("openai", first), ("anthropic", second)]),
            ),
            ROOMY,
        );
        assert_eq!(
            s.cleanup_destination(),
            Some("OpenAI, falling back to Anthropic")
        );

        let d = dictate_for(&s, 4);
        assert_eq!(d.document.to_plain(), "kubectl ran.", "pasted");
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
        assert!(d.late_cleanup.is_none());
        assert_eq!(
            (
                first_requests.lock().unwrap().len(),
                second_requests.lock().unwrap().len()
            ),
            (1, 1)
        );
    }

    #[test]
    fn the_next_dictation_stops_a_pool_still_working_on_the_last_one() {
        // The first model is still out on the last dictation when the next
        // one sends its own request, which it answers at once. Once it fails
        // the last one, that pass asks no further model for a superseded
        // document.
        let (first, _) = FakeCleanup::new(
            |call, text| {
                if call == 0 {
                    failing(call, text)
                } else {
                    Ok("Kubectl ran.".into())
                }
            },
            at_once,
        );
        let (first, started) = first.announcing();
        let (first, go) = first.gated(the_first_call);
        let (second, second_requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let s = waiting_up_to(
            cleaning_with(
                Lines {
                    during: vec!["cube control ran"],
                    at_release: vec![],
                },
                pool(vec![("openai", first), ("anthropic", second)]),
            ),
            BRIEF,
        );

        let mut last = dictate_for(&s, 4);
        let last_late = last
            .late_cleanup
            .take()
            .expect("its first model is still out");
        until_started(&started, 0);
        let _next = dictate_for(&s, 4);

        go.send(()).unwrap();
        let done = last_late.wait();
        assert_eq!(
            done.document.to_plain(),
            tier0_of(&s, &["cube control ran"]),
            "the first model failed, and the next was not asked for a superseded document"
        );
        assert_eq!(second_requests.lock().unwrap().len(), 0);
    }

    #[test]
    fn building_the_cleanup_again_stops_a_pool_still_working_for_the_old_one() {
        let (first, first_requests) = FakeCleanup::new(failing, at_once);
        let (first, started) = first.announcing();
        let (first, go) = first.gated(every_call);
        let (second, second_requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = waiting_up_to(
            cleaning_with(
                Lines {
                    during: vec!["cube control ran"],
                    at_release: vec![],
                },
                pool(vec![("openai", first), ("anthropic", second)]),
            ),
            BRIEF,
        );
        let mut d = dictate_for(&s, 4);
        let late = d.late_cleanup.take().expect("its first model is still out");
        until_started(&started, 0);
        // A setting changed -- a key removed -- and the session builds its
        // cleanup again. The old pool must not ask its next model.
        s.replace_cleanup(&[], &macalla_platform::secrets::MemoryStore::new());

        go.send(()).unwrap();
        let done = late.wait();
        assert_eq!(
            done.document.to_plain(),
            tier0_of(&s, &["cube control ran"])
        );
        assert_eq!(
            (
                first_requests.lock().unwrap().len(),
                second_requests.lock().unwrap().len()
            ),
            (1, 0)
        );
    }

    #[test]
    fn closing_the_session_stops_a_pool_still_working_for_it() {
        let (first, first_requests) = FakeCleanup::new(failing, at_once);
        let (first, started) = first.announcing();
        let (first, go) = first.gated(every_call);
        let (second, second_requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let s = waiting_up_to(
            cleaning_with(
                Lines {
                    during: vec!["cube control ran"],
                    at_release: vec![],
                },
                pool(vec![("openai", first), ("anthropic", second)]),
            ),
            BRIEF,
        );
        let mut d = dictate_for(&s, 4);
        let late = d.late_cleanup.take().expect("its first model is still out");
        until_started(&started, 0);
        // The app closes the session to open one with new settings.
        drop(s);

        go.send(()).unwrap();
        late.wait();
        assert_eq!(
            (
                first_requests.lock().unwrap().len(),
                second_requests.lock().unwrap().len()
            ),
            (1, 0)
        );
    }

    /// `Lines`, with Esc twice landing while the engine finishes: `pressed`
    /// is set inside `finish`, after the record loop last asked `stop`.
    struct EscWhileFinishing(Lines, Arc<std::sync::atomic::AtomicBool>);

    struct EscWhileFinishingStream(Box<dyn SttStream>, Arc<std::sync::atomic::AtomicBool>);

    impl SttStream for EscWhileFinishingStream {
        fn push(&mut self, samples: &[f32]) -> macalla_core::Result<()> {
            self.0.push(samples)
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            self.0.poll()
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            let Self(inner, pressed) = *self;
            pressed.store(true, std::sync::atomic::Ordering::Relaxed);
            inner.finish()
        }
    }

    impl SttEngine for EscWhileFinishing {
        fn id(&self) -> &'static str {
            self.0.id()
        }
        fn caps(&self) -> EngineCaps {
            self.0.caps()
        }
        fn stream(&self, ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(EscWhileFinishingStream(
                self.0.stream(ctx)?,
                Arc::clone(&self.1),
            )))
        }
        fn transcribe(
            &self,
            audio: &AudioSegment,
            ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            self.0.transcribe(audio, ctx)
        }
    }

    #[test]
    fn esc_twice_while_the_engine_finishes_sends_a_remote_provider_nothing() {
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let pressed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut s = Session::with_engine(
            Box::new(EscWhileFinishing(
                Lines {
                    during: vec!["the red door"],
                    at_release: vec!["the last door"],
                },
                Arc::clone(&pressed),
            )),
            seed(),
            "seed",
            Config {
                vad_gate: false,
                ..Config::default()
            },
        );
        s.detector = energy;
        s.cleanup = Some(remote(fake));
        let mut d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || pressed.load(std::sync::atomic::Ordering::Relaxed),
                &mut |_| {},
            )
            .unwrap();
        assert!(d.discarded, "the press after release was heard");
        // Only waiting on late cleanup sends a remote provider anything. Wait
        // on whatever there is, so the count below can fail.
        let late = d.late_cleanup.take();
        let left_for_later = late.is_some();
        if let Some(late) = late {
            late.wait();
        }
        assert!(!left_for_later, "nothing for a trigger to send later");
        assert_eq!(requests.lock().unwrap().len(), 0);
    }

    #[test]
    fn esc_twice_as_a_cloud_answer_lands_sends_a_remote_provider_nothing() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        // Ticks far longer than the fake takes to answer: the answer is in
        // before the wait would ask, so only the ask after it can hear Esc.
        s.cloud_wait = Wait {
            tick: Duration::from_secs(5),
            cap: Duration::from_secs(10),
        };
        s.cleanup = Some(remote(fake));
        let mut asked = 0;
        let mut d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                // Not yet when the request goes out, nor as the wait starts;
                // pressed by the answer.
                &mut || {
                    asked += 1;
                    asked > 2
                },
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            outcome(&d),
            &CloudOutcome::Transcribed,
            "the provider still answered"
        );
        assert!(d.discarded);
        // Waited on, if there is any, so the count below can fail.
        let late = d.late_cleanup.take();
        let left_for_later = late.is_some();
        if let Some(late) = late {
            late.wait();
        }
        assert!(!left_for_later);
        assert_eq!(requests.lock().unwrap().len(), 0);
    }

    // ------------------------------------------------ the Enhancement wait

    /// A wait that an answer given at once never runs out of, however busy
    /// the machine is.
    const ROOMY: Duration = Duration::from_secs(10);

    /// A wait that a held request always runs out of.
    const BRIEF: Duration = Duration::from_millis(50);

    /// `s`, waiting up to `cap` for Enhancement.
    fn waiting_up_to(mut s: Session, cap: Duration) -> Session {
        s.enhancement_wait = Wait {
            tick: ENHANCEMENT_TICK,
            cap,
        };
        s
    }

    fn every_call(_: usize) -> bool {
        true
    }

    fn the_first_call(call: usize) -> bool {
        call == 0
    }

    /// Blocks until `started` has heard request `call` start.
    fn until_started(started: &std::sync::mpsc::Receiver<usize>, call: usize) {
        while started
            .recv_timeout(Duration::from_secs(10))
            .expect("the request started")
            != call
        {}
    }

    /// Answers [`Waiting::Continue`] until `started` hears a request start,
    /// and `then` from that ask on.
    fn once_started(
        started: std::sync::mpsc::Receiver<usize>,
        then: Waiting,
    ) -> impl FnMut() -> Waiting {
        let mut seen = false;
        move || {
            seen = seen || started.try_recv().is_ok();
            if seen {
                then
            } else {
                Waiting::Continue
            }
        }
    }

    /// "kubectl ran", dictated once with Enhancement from `with`, waited for
    /// up to `cap`, and `cancel` answering once the key is up.
    fn kubectl_ran(
        with: cleanup::Cleanup,
        cap: Duration,
        cancel: &mut dyn FnMut() -> Waiting,
    ) -> (Session, Dictation) {
        let s = waiting_up_to(
            cleaning_with(
                Lines {
                    during: vec!["kubectl ran"],
                    at_release: vec![],
                },
                with,
            ),
            cap,
        );
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(4),
                cancel,
                &mut |_| {},
            )
            .unwrap();
        (s, d)
    }

    /// A provider that cannot serve right now, as Apple Intelligence
    /// switched off cannot.
    struct NotNow;

    impl Enhancer for NotNow {
        fn caps(&self) -> macalla_core::enhance::EnhancerCaps {
            macalla_core::enhance::EnhancerCaps {
                id: "not-now",
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            false
        }
        fn enhance(
            &self,
            _blocks: &[macalla_core::block::Block],
            _terms: &[String],
        ) -> macalla_core::Result<Vec<macalla_core::block::Block>> {
            unreachable!("a provider that cannot serve is never asked")
        }
    }

    #[test]
    fn a_yes_or_no_answer_to_whether_to_stop_waiting_gives_up_only_on_yes() {
        assert_eq!(Waiting::from(true), Waiting::GiveUp);
        assert_eq!(Waiting::from(false), Waiting::Continue);
    }

    #[test]
    fn only_giving_up_ends_the_wait_for_a_cloud_engine() {
        assert_eq!(
            [
                Waiting::Continue,
                Waiting::GiveUp,
                Waiting::DeliverNow,
                Waiting::WithoutEnhancement,
            ]
            .map(|answer| (answer, answer.gives_up_on_the_cloud())),
            [
                (Waiting::Continue, false),
                (Waiting::GiveUp, true),
                (Waiting::DeliverNow, false),
                (Waiting::WithoutEnhancement, false),
            ],
            "a transcription is not Enhancement: only Esc twice, or quitting, ends its wait"
        );
    }

    #[test]
    fn past_the_wait_tier_0_is_pasted_and_the_remote_answer_arrives_later() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (fake, started) = fake.announcing();
        let (fake, go) = fake.gated(every_call);
        let (s, mut d) = kubectl_ran(remote(fake), BRIEF, &mut || Waiting::Continue);
        let tier0 = tier0_of(&s, &["kubectl ran"]);
        assert_eq!(d.document.to_plain(), tier0, "Tier 0 is pasted");
        assert_eq!(d.enhancement, Some(EnhancementOutcome::OutOfTime));
        let waited = d.timing.tier1_ms.expect("the paste waited");
        assert!(
            waited < ROOMY.as_secs_f64() * 1000.0,
            "for the wait, not for the answer: {waited} ms"
        );
        until_started(&started, 0);

        let late = d.late_cleanup.take().expect("the answer is still out");
        assert_eq!(late.outstanding(), 1);
        go.send(()).unwrap();
        let done = late.wait();
        assert_eq!(done.document.to_plain(), "kubectl ran.");
        assert!(!done.partial);
        assert_eq!(
            d.document.to_plain(),
            tier0,
            "what was pasted stays what was pasted"
        );
        assert_eq!(requests.lock().unwrap().len(), 1, "sent once, at release");
    }

    #[test]
    fn a_remote_failure_says_failed_an_unchanged_answer_is_applied_and_one_not_serving_is_skipped()
    {
        let (fake, _) = FakeCleanup::new(failing, at_once);
        let (s, d) = kubectl_ran(remote(fake), ROOMY, &mut || Waiting::Continue);
        assert_eq!(d.document.to_plain(), tier0_of(&s, &["kubectl ran"]));
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Failed));
        assert!(d.late_cleanup.is_none(), "nothing more is coming");

        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let (s, d) = kubectl_ran(remote(fake), ROOMY, &mut || Waiting::Continue);
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(d.document.to_plain(), tier0_of(&s, &["kubectl ran"]));
        assert_eq!(
            d.enhancement,
            Some(EnhancementOutcome::Applied),
            "an answer that changed nothing is still an answer"
        );

        let not_now = cleanup::Cleanup::new(
            Arc::new(NotNow),
            choice::CleanupReach::Remote,
            "Your server",
        );
        let (_, d) = kubectl_ran(not_now, ROOMY, &mut || Waiting::Continue);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert!(d.late_cleanup.is_none());
    }

    #[test]
    fn on_this_machine_every_segment_failing_says_failed_but_unchanged_ones_do_not() {
        let (fake, _) = FakeCleanup::new(failing, at_once);
        let s = waiting_up_to(
            cleaning(
                Lines {
                    during: vec!["the build passed."],
                    at_release: vec!["we can ship"],
                },
                fake,
            ),
            ROOMY,
        );
        let d = dictate_for(&s, 4);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Failed));
        assert!(d.late_cleanup.is_none());

        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        let s = waiting_up_to(
            cleaning(
                Lines {
                    during: vec!["the build passed."],
                    at_release: vec!["we can ship"],
                },
                fake,
            ),
            ROOMY,
        );
        let d = dictate_for(&s, 4);
        assert_eq!(requests.lock().unwrap().len(), 2);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
    }

    #[test]
    fn a_first_pool_model_that_hangs_uses_up_the_whole_wait_and_the_next_answers_for_history() {
        let (first, first_requests) = FakeCleanup::new(failing, at_once);
        let (first, started) = first.announcing();
        let (first, go) = first.gated(every_call);
        let (second, second_requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (s, mut d) = kubectl_ran(
            pool(vec![("openai", first), ("anthropic", second)]),
            BRIEF,
            &mut || Waiting::Continue,
        );
        assert_eq!(d.document.to_plain(), tier0_of(&s, &["kubectl ran"]));
        assert_eq!(d.enhancement, Some(EnhancementOutcome::OutOfTime));
        until_started(&started, 0);
        assert_eq!(
            second_requests.lock().unwrap().len(),
            0,
            "the next model waits its turn"
        );

        let late = d.late_cleanup.take().expect("the pool is still working");
        go.send(()).unwrap();
        assert_eq!(late.wait().document.to_plain(), "kubectl ran.");
        assert_eq!(
            (
                first_requests.lock().unwrap().len(),
                second_requests.lock().unwrap().len()
            ),
            (1, 1)
        );
    }

    #[test]
    fn esc_twice_during_the_wait_pastes_and_keeps_nothing_and_asks_no_further_pool_model() {
        let (first, first_requests) = FakeCleanup::new(failing, at_once);
        let (first, first_started) = first.announcing();
        let (first, go) = first.gated(every_call);
        let (second, second_requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (second, second_started) = second.announcing();
        let (s, d) = kubectl_ran(
            pool(vec![("openai", first), ("anthropic", second)]),
            ROOMY,
            &mut once_started(first_started, Waiting::GiveUp),
        );
        assert!(d.discarded);
        assert!(d.late_cleanup.is_none(), "nothing for a trigger to keep");
        assert_eq!(d.enhancement, None);
        let waited = d.timing.tier1_ms.expect("the wait began");
        assert!(
            waited < ROOMY.as_secs_f64() * 1000.0,
            "ended at Esc twice: {waited} ms"
        );

        // The first model fails only now, with nobody waiting.
        drop(go);
        drop(d);
        drop(s);
        // The request thread holds the pool last, so the second model's
        // announcer goes when that pass ends -- and says so first if it was
        // asked.
        assert!(
            matches!(
                second_started.recv_timeout(Duration::from_secs(10)),
                Err(RecvTimeoutError::Disconnected)
            ),
            "the pass ended without asking the second model"
        );
        assert_eq!(
            (
                first_requests.lock().unwrap().len(),
                second_requests.lock().unwrap().len()
            ),
            (1, 0)
        );
    }

    #[test]
    fn esc_twice_as_the_remote_request_is_about_to_go_sends_nothing() {
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(text.into()), at_once);
        // Asked twice before anything is sent: once the engine has finished,
        // which hears nothing yet, then as the request is about to go, which
        // hears Esc twice. That second ask is what keeps a dictation thrown
        // away at the last moment on this machine.
        let mut asked = 0;
        let (_, d) = kubectl_ran(remote(fake), ROOMY, &mut || {
            asked += 1;
            Waiting::from(asked > 1)
        });
        assert_eq!(asked, 2, "the engine's finish, then the request");
        assert!(d.discarded);
        assert!(d.late_cleanup.is_none());
        assert_eq!(d.enhancement, None);
        assert_eq!(d.timing.tier1_ms, None, "nothing was waited for");
        assert_eq!(requests.lock().unwrap().len(), 0, "never sent");
    }

    #[test]
    fn delivering_now_at_release_sends_once_and_hands_the_answer_on() {
        // The next dictation is already asked for as the request is about to
        // go, and the trigger has a row for the answer: Tier 0 is pasted at
        // once, and the provider is sent the text once, not waited for.
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (s, mut d) = kubectl_ran(remote(fake), ROOMY, &mut || Waiting::DeliverNow);
        assert!(!d.discarded, "delivered, not thrown away");
        assert_eq!(d.document.to_plain(), tier0_of(&s, &["kubectl ran"]));
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert_eq!(d.timing.tier1_ms, None, "nothing was waited for");
        let late = d
            .late_cleanup
            .take()
            .expect("the answer is handed on, for History");
        assert_eq!(late.wait().document.to_plain(), "kubectl ran.");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_press_during_a_cloud_wait_sends_enhancement_once_and_hands_the_answer_on() {
        // What the desktop answers, with History on, for a press made while
        // the transcription is still out: DeliverNow on every ask from then
        // on. The transcription is waited for, its text pasted without
        // waiting on Enhancement, and a remote provider is sent it once.
        let (cloud, go, _finished) = CloudFake::answering("kubectl ran").gated();
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        s.cleanup = Some(remote(fake));
        let s = waiting_up_to(s, ROOMY);
        let pressed = std::cell::Cell::new(false);
        let mut d = s
            .dictate_from(
                fake_source(),
                &mut after(3),
                &mut || {
                    if pressed.get() {
                        Waiting::DeliverNow
                    } else {
                        Waiting::Continue
                    }
                },
                &mut |p| {
                    // Pressed while it says it is waiting; then it answers.
                    if let Progress::CloudWaiting { .. } = p {
                        pressed.set(true);
                        let _ = go.send(());
                    }
                },
            )
            .unwrap();
        assert!(
            pressed.get(),
            "the press came during the transcription wait"
        );
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert!(!d.discarded);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert_eq!(
            d.document.to_plain(),
            "kubectl ran",
            "as the engine heard it"
        );
        let late = d
            .late_cleanup
            .take()
            .expect("the answer is handed on, for History");
        assert_eq!(late.wait().document.to_plain(), "kubectl ran.");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn esc_twice_as_the_cloud_wait_runs_out_is_still_heard() {
        // A cloud wait already over when it first asks: Esc twice on that ask
        // gives the request up, rather than losing to the clock.
        let (cloud, go, _finished) = CloudFake::answering("too late").gated();
        let mut s = cloud_session(FakeEngine::new(EngineCaps::stream_first()), cloud, false);
        s.cloud_wait = Wait {
            tick: Duration::from_millis(20),
            cap: Duration::ZERO,
        };
        let mut asked = 0;
        let d = s
            .dictate_from(
                fake_source(),
                &mut after(3),
                // Not as the request goes, nor as the wait starts; then Esc
                // twice.
                &mut || {
                    asked += 1;
                    asked > 2
                },
                &mut |_| {},
            )
            .unwrap();
        drop(go);
        assert_eq!(outcome(&d), &CloudOutcome::Abandoned, "asked {asked} times");
        assert!(d.discarded);
    }

    #[test]
    fn delivering_now_during_the_wait_on_this_machine_uses_what_came_back_and_hands_on_the_rest() {
        let (fake, requests) = FakeCleanup::new(|_, text| Ok(format!("{text},")), at_once);
        let (fake, started) = fake.announcing();
        let (fake, go) = fake.gated(|call| call == 1);
        let s = waiting_up_to(
            cleaning(
                Lines {
                    during: vec!["the red door", "the blue door"],
                    at_release: vec![],
                },
                fake,
            ),
            ROOMY,
        );
        // Released once the second segment is in flight, so the first one's
        // answer is already back: the worker answers in order.
        let mut polls = 0;
        let mut d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut || {
                    polls += 1;
                    if polls <= 2 {
                        return false;
                    }
                    until_started(&started, 1);
                    true
                },
                &mut || Waiting::DeliverNow,
                &mut |_| {},
            )
            .unwrap();
        assert!(!d.discarded);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert_eq!(
            d.document.to_plain(),
            "The red door, the blue door",
            "the first as it came back, the second as heard"
        );
        let late = d.late_cleanup.take().expect("the second is still out");
        assert_eq!(late.outstanding(), 1);
        go.send(()).unwrap();
        assert_eq!(
            late.wait().document.to_plain(),
            "The red door, the blue door,"
        );
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn delivering_now_during_a_remote_wait_pastes_tier_0_and_hands_on_the_answer() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (fake, started) = fake.announcing();
        let (fake, go) = fake.gated(every_call);
        let (s, mut d) = kubectl_ran(
            remote(fake),
            ROOMY,
            &mut once_started(started, Waiting::DeliverNow),
        );
        assert!(!d.discarded);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert_eq!(d.document.to_plain(), tier0_of(&s, &["kubectl ran"]));
        let late = d.late_cleanup.take().expect("the answer is still out");
        go.send(()).unwrap();
        assert_eq!(late.wait().document.to_plain(), "kubectl ran.");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn enhancement_turned_off_before_release_sends_a_remote_provider_nothing() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (s, d) = kubectl_ran(remote(fake), ROOMY, &mut || Waiting::WithoutEnhancement);
        assert!(!d.discarded, "the dictation still pastes");
        assert_eq!(d.document.to_plain(), tier0_of(&s, &["kubectl ran"]));
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert!(d.late_cleanup.is_none());
        assert_eq!(requests.lock().unwrap().len(), 0);
    }

    #[test]
    fn enhancement_turned_off_during_the_wait_asks_no_further_pool_model() {
        let (first, first_requests) = FakeCleanup::new(failing, at_once);
        let (first, started) = first.announcing();
        let (first, go) = first.gated(every_call);
        let (second, second_requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (s, mut d) = kubectl_ran(
            pool(vec![("openai", first), ("anthropic", second)]),
            ROOMY,
            &mut once_started(started, Waiting::WithoutEnhancement),
        );
        assert!(!d.discarded);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert_eq!(d.document.to_plain(), tier0_of(&s, &["kubectl ran"]));
        let late = d
            .late_cleanup
            .take()
            .expect("handed on, for the trigger to keep or drop");
        go.send(()).unwrap();
        assert_eq!(
            late.wait().document.to_plain(),
            tier0_of(&s, &["kubectl ran"]),
            "the first model failed, and the next was not asked"
        );
        assert_eq!(
            (
                first_requests.lock().unwrap().len(),
                second_requests.lock().unwrap().len()
            ),
            (1, 0)
        );
    }

    #[test]
    fn both_leave_a_cloud_request_alone_and_only_delivering_now_sends_enhancement_for_later() {
        let (fake, requests) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        s.cleanup = Some(remote(fake));
        let s = waiting_up_to(s, ROOMY);
        let d = s
            .dictate_from(
                fake_source(),
                &mut after(3),
                &mut || Waiting::WithoutEnhancement,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            outcome(&d),
            &CloudOutcome::Transcribed,
            "a transcription is not Enhancement"
        );
        assert!(!d.discarded);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert_eq!(
            d.document.to_plain(),
            "kubectl ran",
            "as the engine heard it"
        );
        assert!(d.late_cleanup.is_none());
        assert_eq!(
            requests.lock().unwrap().len(),
            0,
            "going without Enhancement sends nothing"
        );

        let mut d = s
            .dictate_from(
                fake_source(),
                &mut after(3),
                &mut || Waiting::DeliverNow,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            outcome(&d),
            &CloudOutcome::Transcribed,
            "the transcription is waited for: only Enhancement is cut short"
        );
        assert!(!d.discarded, "delivered, not thrown away");
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Skipped));
        assert_eq!(
            d.document.to_plain(),
            "kubectl ran",
            "as the engine heard it"
        );
        let late = d
            .late_cleanup
            .take()
            .expect("sent, and handed on for History");
        assert_eq!(late.wait().document.to_plain(), "kubectl ran.");
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "sent once, and not waited for"
        );
    }

    /// What the wait said, and when, answered through `go` once it had said
    /// so `times` times.
    fn watched(
        s: &Session,
        go: std::sync::mpsc::Sender<()>,
        times: usize,
    ) -> (Dictation, Vec<(EnhancingWith, Duration)>) {
        let mut said = Vec::new();
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(4),
                &mut || false,
                &mut |p| {
                    if let Progress::Enhancing { with, elapsed } = p {
                        said.push((with.clone(), elapsed));
                        if said.len() == times {
                            let _ = go.send(());
                        }
                    }
                },
            )
            .unwrap();
        (d, said)
    }

    #[test]
    fn the_wait_is_reported_each_tick_counting_up_and_naming_who_it_waits_for() {
        let tick = Duration::from_millis(10);
        let counted = |said: &[(EnhancingWith, Duration)], who: EnhancingWith| {
            assert!(said.len() >= 3, "{said:?}");
            assert!(said.iter().all(|(with, _)| *with == who), "{said:?}");
            assert!(said[0].1 >= tick, "not before the first tick: {said:?}");
            assert!(
                said.windows(2).all(|pair| pair[0].1 < pair[1].1),
                "counting up: {said:?}"
            );
        };

        let (fake, _) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (fake, go) = fake.gated(every_call);
        let mut s = cleaning_with(
            Lines {
                during: vec!["kubectl ran"],
                at_release: vec![],
            },
            remote(fake),
        );
        s.enhancement_wait = Wait { tick, cap: ROOMY };
        let (d, said) = watched(&s, go, 3);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
        counted(&said, EnhancingWith::Server);

        // On this machine, for the segments still out, the same.
        let (fake, _) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (fake, go) = fake.gated(every_call);
        let mut s = cleaning(
            Lines {
                during: vec![],
                at_release: vec!["kubectl ran"],
            },
            fake,
        );
        s.enhancement_wait = Wait { tick, cap: ROOMY };
        let (d, said) = watched(&s, go, 3);
        assert_eq!(d.enhancement, Some(EnhancementOutcome::Applied));
        counted(&said, EnhancingWith::ServerOnThisComputer);
    }

    #[test]
    fn who_enhancement_waits_for_is_read_from_what_built_and_a_pool_by_its_first_ticked_model() {
        use choice::{EnhancerChoice, KeySource};
        use macalla_platform::secrets::MemoryStore;
        let endpoint = |url: &str, key: KeySource| EnhancerChoice::Endpoint {
            url: url.into(),
            model: "m".into(),
            key,
            reasoning: None,
        };
        let anthropic = || EnhancerChoice::Anthropic {
            model: "m".into(),
            key: KeySource::Stored("anthropic".into()),
            reasoning: None,
        };
        assert_eq!(
            EnhancerChoice::AppleIntelligence { guided: true }.enhancing_with(),
            EnhancingWith::AppleIntelligence
        );
        assert_eq!(
            endpoint("http://127.0.0.1:11434/v1", KeySource::None).enhancing_with(),
            EnhancingWith::ServerOnThisComputer
        );
        assert_eq!(
            endpoint(
                "https://api.openai.com/v1",
                KeySource::Stored("openai".into())
            )
            .enhancing_with(),
            EnhancingWith::Provider("openai".into())
        );
        assert_eq!(
            endpoint("https://api.openai.com/v1", KeySource::None).enhancing_with(),
            EnhancingWith::Provider("openai".into()),
            "at a provider's own address, whatever the key"
        );
        assert_eq!(
            endpoint(
                "https://llm.example.com/v1",
                KeySource::Stored("openai".into())
            )
            .enhancing_with(),
            EnhancingWith::Server
        );
        assert_eq!(
            anthropic().enhancing_with(),
            EnhancingWith::Provider("anthropic".into())
        );

        // OpenAI is ticked first and has no key, so the pool is Anthropic
        // falling back to a server here, and is named by Anthropic.
        let keys = MemoryStore::new();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        let mut s = session(FakeEngine::new(EngineCaps::stream_first()));
        assert_eq!(s.cleanup_enhancing_with(), None, "off");
        s.replace_cleanup(
            &[
                endpoint(
                    "https://api.openai.com/v1",
                    KeySource::Stored("openai".into()),
                ),
                anthropic(),
                endpoint("http://127.0.0.1:9", KeySource::None),
            ],
            &keys,
        );
        assert!(
            s.cleanup_destination()
                .is_some_and(|label| label.contains("falling back")),
            "a pool built: {:?}",
            s.cleanup_destination()
        );
        assert_eq!(
            s.cleanup_enhancing_with(),
            Some(&EnhancingWith::Provider("anthropic".into()))
        );
    }

    #[test]
    fn dropping_a_remote_late_cleanup_asks_no_further_pool_model() {
        let (first, first_requests) = FakeCleanup::new(failing, at_once);
        let (first, first_started) = first.announcing();
        let (first, go) = first.gated(every_call);
        let (second, _) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let (second, second_started) = second.announcing();
        let (s, mut d) = kubectl_ran(
            pool(vec![("openai", first), ("anthropic", second)]),
            BRIEF,
            &mut || Waiting::Continue,
        );
        let late = d.late_cleanup.take().expect("its first model is still out");
        until_started(&first_started, 0);
        // A trigger that keeps nothing -- History off -- drops it.
        drop(late);

        drop(go);
        drop(d);
        drop(s);
        assert!(
            matches!(
                second_started.recv_timeout(Duration::from_secs(10)),
                Err(RecvTimeoutError::Disconnected)
            ),
            "the pass ended without asking the second model"
        );
        assert_eq!(first_requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_cloud_provider_is_the_one_the_session_opened_with() {
        let local = session(FakeEngine::new(EngineCaps::stream_first()));
        assert_eq!(local.cloud_provider(), None);
        let cloud = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        assert_eq!(cloud.cloud_provider(), Some("fakecloud"));
    }

    #[test]
    fn pasting_an_empty_document_touches_nothing() {
        // No clipboard, no chord. This runs on CI with no Accessibility
        // grant, which is itself the proof: a real paste would fail here.
        let s = session(FakeEngine::new(EngineCaps::batch()));
        let mut d = s.transcribe(Vec::new()).unwrap();
        assert!(d.document.to_plain().trim().is_empty());
        assert_eq!(paste(&mut d).unwrap(), Paste::Nothing);
        assert_eq!(
            d.timing.paste_ms, None,
            "nothing was pasted, so nothing is timed"
        );
    }

    /// The inserter `system_inserter` would have returned. Named, never
    /// built: naming it satisfies `paste_with`'s generic without opening a
    /// clipboard, an input device or asking this Mac anything.
    type SystemInserter = macalla_platform::paste::Inserter<
        macalla_platform::paste::SystemClipboard,
        macalla_platform::paste::SystemKeys,
        macalla_platform::paste::RealClock,
    >;

    /// A dictation with words in it and no cloud engine, so `refused` has
    /// nothing to say and the paste is really attempted.
    fn dictated() -> Dictation {
        let s = session(FakeEngine::new(EngineCaps::batch()));
        let d = s.transcribe(vec![0.1; 16_000]).unwrap();
        assert!(!d.document.to_plain().trim().is_empty());
        assert_eq!(d.timing.paste_ms, None);
        d
    }

    #[test]
    fn a_paste_that_cannot_open_for_want_of_accessibility_is_a_refusal_and_is_not_timed() {
        // The whole path, not the helper: this is how the app meets a missing
        // grant, because `SystemKeys::new` refuses in the constructor and the
        // gate inside `insert` never gets a turn.
        use macalla_platform::paste::NotDelivered;
        let mut d = dictated();
        let answer = paste_with(&mut d, || -> macalla_platform::Result<SystemInserter> {
            Err(macalla_platform::Error::Permission {
                permission: macalla_platform::Permission::Accessibility,
            })
        })
        .expect("a missing grant is a refusal, not an error");
        match answer {
            Paste::Inserted { how, ms } => {
                assert_eq!(
                    how,
                    Inserted::NotDelivered(NotDelivered::NotTrustedForAccessibility)
                );
                assert!(ms >= 0.0, "how long finding out took");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(
            d.timing.paste_ms, None,
            "nothing was inserted, so the latency breakdown keeps no sample"
        );
    }

    #[test]
    fn a_paste_whose_clipboard_will_not_open_is_still_an_error() {
        // A fault, not a refusal: there is nothing for a person to grant, and
        // the apps report it as they always have.
        let mut d = dictated();
        let answer = paste_with(&mut d, || -> macalla_platform::Result<SystemInserter> {
            Err(macalla_platform::Error::Clipboard(
                "opening it: busy".into(),
            ))
        });
        assert!(answer.is_err(), "got {answer:?}");
        assert_eq!(d.timing.paste_ms, None);
    }

    #[test]
    fn an_inserter_that_cannot_open_for_want_of_accessibility_is_a_refusal_not_an_error() {
        // The path the app really takes: `SystemKeys::new` tests the grant
        // and refuses before an `Inserter` exists, so the gate inside
        // `insert` never gets a turn. Without this mapping the desktop shows
        // no sentence and the CLI loses the text.
        use macalla_platform::paste::NotDelivered;
        use macalla_platform::{Error, Permission};
        assert_eq!(
            not_delivered(&Error::Permission {
                permission: Permission::Accessibility
            }),
            Some(NotDelivered::NotTrustedForAccessibility)
        );
        // Every other failure is a fault, and stays an error.
        for e in [
            Error::Permission {
                permission: Permission::Microphone,
            },
            Error::Permission {
                permission: Permission::InputMonitoring,
            },
            Error::Clipboard("busy".into()),
            Error::Input("no device".into()),
        ] {
            assert_eq!(not_delivered(&e), None, "{e} is a fault, not a refusal");
        }
    }

    #[test]
    fn only_a_paste_that_happened_is_timed() {
        // The refusal mirrors `Paste::MovedOn` and the empty document above:
        // nothing was touched, so nothing is timed. Its sub-millisecond
        // sample would otherwise flatter the paste breakdown, which is how
        // the speed claim is proved.
        use macalla_platform::paste::NotDelivered;
        assert_eq!(
            timed(
                Inserted::NotDelivered(NotDelivered::NotTrustedForAccessibility),
                0.02
            ),
            None
        );
        assert_eq!(timed(Inserted::Nothing, 0.02), None);
        for how in [
            Inserted::PastedAndRestored,
            Inserted::PastedAndKept,
            Inserted::Typed,
        ] {
            assert_eq!(timed(how, 161.0), Some(161.0), "{how:?} inserted text");
        }
    }

    fn notes() -> Option<String> {
        Some("com.example.notes".into())
    }

    fn browser() -> Option<String> {
        Some("com.example.browser".into())
    }

    #[test]
    fn a_cloud_result_is_pasted_only_into_the_app_it_was_dictated_into() {
        let mut s = cloud_session(
            FakeEngine::new(EngineCaps::stream_first()),
            CloudFake::answering("kubectl ran"),
            false,
        );
        s.front_app = notes;
        let mut d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(outcome(&d), &CloudOutcome::Transcribed);
        assert_eq!(d.front_app, notes(), "the app in front as it started");
        assert_eq!(
            history::entry_from(&d, "cloud-fake", 1).app_id,
            notes(),
            "History keeps it"
        );

        // Asked, not pasted: a real paste would reach the focused app.
        assert_eq!(refused(&d, notes), None, "still in front: pasted");
        assert_eq!(refused(&d, browser), Some(Paste::MovedOn));
        assert_eq!(refused(&d, || None), None, "cannot tell now: pasted");
        d.front_app = None;
        assert_eq!(
            refused(&d, browser),
            None,
            "could not tell at the start: pasted"
        );

        // A request that did not transcribe is withheld whoever is in front.
        d.front_app = notes();
        d.cloud.as_mut().unwrap().outcome = CloudOutcome::TimedOut;
        assert_eq!(refused(&d, browser), Some(Paste::Withheld));
    }

    #[test]
    fn a_local_dictation_with_enhancement_on_is_pasted_only_into_the_app_it_was_dictated_into() {
        let (fake, _) = FakeCleanup::new(|_, _| Ok("Kubectl ran.".into()), at_once);
        let mut s = cleaning(
            Lines {
                during: vec!["kubectl ran"],
                at_release: vec![],
            },
            fake,
        );
        s.front_app = notes;
        let d = dictate_for(&s, 4);
        assert!(d.cloud.is_none(), "no cloud engine");
        assert_eq!(
            d.front_app,
            notes(),
            "Enhancement can hold the paste for seconds, so the app is asked for"
        );
        assert_eq!(refused(&d, browser), Some(Paste::MovedOn));
        assert_eq!(refused(&d, notes), None, "still in front: pasted");
    }

    #[test]
    fn a_local_dictation_with_enhancement_off_pastes_whichever_app_is_in_front_and_never_asks() {
        let mut s = Session::with_engine(
            Box::new(FakeEngine::new(EngineCaps::stream_first())),
            seed(),
            "seed",
            Config {
                vad_gate: false,
                ..Config::default()
            },
        );
        s.front_app =
            || -> Option<String> { panic!("a local dictation never asks which app is in front") };
        let d = s
            .dictate_from(
                FakeSource {
                    polls_left: 100,
                    dies: false,
                },
                &mut after(3),
                &mut || false,
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(
            d.front_app, None,
            "nothing holds the paste, so the app is not asked for"
        );
        assert_eq!(
            history::entry_from(&d, "fake", 1).app_id,
            None,
            "History keeps no app for it, as before"
        );
        assert_eq!(
            refused(&d, || -> Option<String> {
                panic!("a local dictation is pasted at release and never asks")
            }),
            None
        );
    }

    #[cfg(feature = "cloud-transcription")]
    #[test]
    fn opening_a_cloud_session_with_no_key_stored_fails_with_needs_key() {
        use macalla_platform::secrets::MemoryStore;
        let config = Config {
            // An engine no build has, so this answer cannot depend on which
            // local engine this machine happens to be able to open.
            engine: Some("no-such-engine".into()),
            cloud: Some(choice::CloudTranscription {
                provider: "openai".into(),
                model: None,
            }),
            ..Config::default()
        };
        // The cloud engine is built before the preview engine is opened, so
        // the answer a person can act on comes first, whatever this machine's
        // local engine can do.
        assert!(matches!(
            Session::open_with_keys(config, &MemoryStore::new()),
            Err(Error::NeedsKey { provider }) if provider == "openai"
        ));
    }

    #[cfg(feature = "cloud-transcription")]
    #[test]
    fn a_cloud_session_opens_without_its_preview_engine_and_only_dictation_refuses() {
        use macalla_platform::secrets::MemoryStore;
        let keys = MemoryStore::new();
        providers::store_key(&keys, "openai", "test-key").unwrap();
        let config = Config {
            engine: Some("no-such-engine".into()),
            cloud: Some(choice::CloudTranscription {
                provider: "openai".into(),
                model: None,
            }),
            ..Config::default()
        };
        // Opening builds the adapter and sends nothing; no file is
        // transcribed here, so nothing reaches the network.
        let mut s = Session::open_with_keys(config, &keys).expect("opens for files");
        assert!(matches!(
            s.dictate(&mut || StopSignal::Release, &mut || false, &mut |_| {}),
            Err(Error::NoPreviewEngine(reason)) if reason.contains("no-such-engine")
        ));
    }

    #[cfg(not(feature = "cloud-transcription"))]
    #[test]
    fn a_cloud_session_in_a_build_without_cloud_transcription_says_so() {
        use macalla_platform::secrets::MemoryStore;
        let config = Config {
            cloud: Some(choice::CloudTranscription {
                provider: "openai".into(),
                model: None,
            }),
            ..Config::default()
        };
        assert!(matches!(
            Session::open_with_keys(config, &MemoryStore::new()),
            Err(Error::CloudNotCompiledIn)
        ));
    }

    #[test]
    fn opening_a_local_session_never_touches_the_key_store() {
        struct Panicking;
        impl SecretStore for Panicking {
            fn get(&self, _: &str) -> macalla_platform::Result<Option<String>> {
                panic!("the key store must not be read when Config::cloud is None")
            }
            fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
        }
        // Whichever local engine this build has, it must open (or fail for
        // its own reason) without the panicking store ever being asked.
        let _ = Session::open_with_keys(Config::default(), &Panicking);
    }

    #[test]
    fn a_first_run_with_no_dictionary_anywhere_still_dictates() {
        let (dictionary, label) = load_dictionary(None, &[]).expect("no dictionary is fine");
        assert!(dictionary.is_empty());
        assert_eq!(label, "none");
    }

    #[test]
    fn the_repo_seed_is_found_without_being_asked_for() {
        let seed = "../../fixtures/dictionary/seed.json";
        assert!(Path::new(seed).exists(), "{seed} is missing");
        let (dictionary, label) = load_dictionary(None, &[seed]).expect("load the seed");
        assert!(!dictionary.is_empty());
        assert!(label.contains("terms"), "got: {label}");
    }

    #[test]
    fn naming_a_dictionary_that_does_not_exist_is_an_error_not_a_shrug() {
        match load_dictionary(Some(Path::new("/nonexistent/none.json")), &[]) {
            Err(Error::DictionaryRead { .. }) => {}
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("a typo in the path must not silently give an empty dictionary"),
        }
    }
}
