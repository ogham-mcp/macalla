// SPDX-License-Identifier: MIT
//! Tier 1 cleanup -- Enhancement -- as a dictation runs it.
//!
//! Where the text goes decides how, and [`CleanupReach`] says where:
//!
//! - **On this machine** -- Apple Intelligence, or a server at a loopback
//!   address -- each segment is cleaned while the person is still speaking,
//!   as the design draws it, so at release only the last one is in flight.
//! - **Off this machine** -- any other server, Anthropic, a cloud model --
//!   nothing is sent while the person speaks. The whole Tier 0 document is
//!   sent once, at release, on a thread of its own, after one last ask of the
//!   trigger: a recording the person threw away by then never leaves the
//!   machine, and neither does text for a provider they turned off.
//!
//! Either way the paste waits for what is out, for [`crate::ENHANCEMENT_WAIT`]
//! from release at most, and a pool's members
//! share that one wait. What has not come back by then is pasted as Tier 0,
//! and [`LateCleanup`] brings it to History.
//!
//! Nothing here runs a model on the record loop or on the thread that waits.
//! For cleanup on this machine the loop hands each segment's blocks to a
//! channel and carries on; one worker thread, owned by the dictation, asks
//! the provider for them one at a time, in the order they were spoken. A
//! remote provider's request has a thread of its own. Every channel is
//! unbounded, so no side ever waits to send, and the one wait -- at release
//! -- is a `recv_timeout` that cannot run past [`crate::ENHANCEMENT_WAIT`],
//! hears the trigger within [`DISCARD_POLL`], and says it is waiting once a
//! tick.

use macalla_core::block::{Block, Document};
use macalla_core::dictionary::Dictionary;
use macalla_core::enhance::{Enhancer, EnhancerCaps, Segment};
use macalla_core::meeting_parts::{self, Labels};
use macalla_core::pipeline::{Pipeline, SegmentState};
use macalla_core::stt::Transcript;
use macalla_platform::secrets::SecretStore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::choice::{self, CleanupReach, EnhancerChoice, KeySource};
use crate::failover::{Failover, Member};
use crate::{EnhancementOutcome, Progress, Wait, Waiting};

/// How often the release wait asks the trigger whether to stop waiting. Esc
/// twice takes effect within this, and asking cannot stretch a wait that is
/// never longer than [`crate::ENHANCEMENT_WAIT`].
const DISCARD_POLL: Duration = Duration::from_millis(25);

/// Why cleanup is not cleaning, as [`build`] found it.
#[derive(Debug)]
pub enum CleanupNotice {
    /// Nothing in [`crate::Config::cleanup`] built: one error per choice that
    /// failed, in the order it was tried.
    Failed(Vec<crate::Error>),
    /// One provider built, but cannot serve right now -- Apple Intelligence
    /// switched off, or its model not ready. Already a sentence fit to show,
    /// from [`macalla_enhancers::apple::Availability::explain`], which only
    /// ever returns a fixed string, never one built at runtime.
    NotReady(&'static str),
}

impl std::fmt::Display for CleanupNotice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(errors) => {
                let joined = errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ");
                write!(f, "{joined}")
            }
            Self::NotReady(reason) => write!(f, "{reason}"),
        }
    }
}

// Manual, not derived: `crate::Error` is not itself `PartialEq`, so `Failed`
// compares by its rendered text instead -- this exists so
// `Session::cleanup_error`, which returns `Option<&CleanupNotice>`, stays
// comparable with `assert_eq!(..., None)`, not because any call site
// compares two `Failed` values against each other today.
impl PartialEq for CleanupNotice {
    fn eq(&self, other: &Self) -> bool {
        self.to_string() == other.to_string()
    }
}

impl Eq for CleanupNotice {}

/// A session's cleanup provider, and where it sends the text.
#[derive(Clone)]
pub(crate) struct Cleanup {
    pub(crate) provider: Provider,
    reach: CleanupReach,
    destination: String,
    /// The choices that built, as [`choice::fingerprint`] names them. Empty
    /// when built without choices, as tests build it.
    fingerprint: String,
    /// The same choices [`fingerprint`](Self::fingerprint) names, kept
    /// whole: what actually built, in ticked order, not what was chosen
    /// ([`build`]'s own `choices` argument) -- a member `choice::
    /// session_enhancer` refused (most often a missing key) is not one of
    /// these. Empty when built without choices, as tests build it. Read by
    /// [`Session::cleanup_built_choices`] for a caller that needs to name
    /// what is really in use, typed, rather than parse
    /// [`Cleanup::destination`]'s words.
    built: Vec<EnhancerChoice>,
    /// The abandon flag of the last dictation that started a worker, shared
    /// by every clone, so a new worker can stop the last one's queued late
    /// segments: a slow server that serves one request at a time must not
    /// keep the next dictation waiting behind the last one's leftovers.
    last_worker: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// Who a dictation's Enhancement waits for: the first of
    /// [`built`](Self::built), or from the reach alone when built without
    /// choices, as tests build it.
    with: crate::EnhancingWith,
}

/// What cleans the text: one provider, or a pool of ticked cloud models that
/// fails over ([`Failover`]).
///
/// Every request carries the dictation's stop flag, so the call sites never
/// ask which of the two they have. One provider ignores the flag and is asked
/// exactly as it always was; a pool checks it between members.
#[derive(Clone)]
pub(crate) enum Provider {
    One(Arc<dyn Enhancer>),
    Pool(Arc<Failover<Member>>),
}

impl Provider {
    fn caps(&self) -> EnhancerCaps {
        match self {
            Provider::One(enhancer) => enhancer.caps(),
            Provider::Pool(pool) => pool.caps(),
        }
    }

    fn is_available(&self) -> bool {
        match self {
            Provider::One(enhancer) => enhancer.is_available(),
            Provider::Pool(pool) => pool.is_available(),
        }
    }

    /// A pool is remote, so there is nothing on this machine to get ready.
    fn prewarm(&self) {
        if let Provider::One(enhancer) = self {
            enhancer.prewarm();
        }
    }

    fn enhance(
        &self,
        blocks: &[Block],
        terms: &[String],
        stop: &AtomicBool,
    ) -> macalla_core::Result<Vec<Block>> {
        match self {
            Provider::One(enhancer) => enhancer.enhance(blocks, terms),
            Provider::Pool(pool) => pool.enhance(blocks, terms, stop),
        }
    }
}

/// Said when a meeting would be tidied with Apple Intelligence, which is not
/// offered.
///
/// Measured on a Mac mini (Macmini9,1, Apple M1) on 2026-09-14, one tidy
/// request through the same guided call cleanup makes, on an invented meeting
/// with its speakers' names taken off:
///
/// - A part of 876 characters (6 lines) took 7.36 to 12.88 s over seven runs,
///   five of them past cleanup's 10 s limit, which the session's cleanup
///   provider shares with dictation.
/// - A full part of 5,652 characters (40 lines) took 31.06 to 45.69 s over six
///   runs, and every reply read in full came back cut short, 38 lines and 1,014
///   of its 1,067 words, so no part could be matched back to its turns.
///
/// Smaller parts with a limit of their own might work, but would need a
/// provider built for tidying; that is not built.
pub(crate) const APPLE_CANNOT_TIDY: &str = "Apple Intelligence can't tidy a file yet. To tidy one, choose a server or a cloud model on the Enhancement card.";

impl Cleanup {
    pub(crate) fn new(
        enhancer: Arc<dyn Enhancer>,
        reach: CleanupReach,
        destination: impl Into<String>,
    ) -> Self {
        Self::with(Provider::One(enhancer), reach, destination.into())
    }

    /// A pool of ticked cloud models. Always remote.
    pub(crate) fn pool(failover: Failover<Member>, destination: String) -> Self {
        Self::with(
            Provider::Pool(Arc::new(failover)),
            CleanupReach::Remote,
            destination,
        )
    }

    fn with(provider: Provider, reach: CleanupReach, destination: String) -> Self {
        Self {
            provider,
            reach,
            destination,
            fingerprint: String::new(),
            built: Vec::new(),
            last_worker: Arc::default(),
            with: match reach {
                CleanupReach::Local => crate::EnhancingWith::ServerOnThisComputer,
                CleanupReach::Remote => crate::EnhancingWith::Server,
            },
        }
    }

    pub(crate) fn built_from(mut self, choices: Vec<EnhancerChoice>) -> Self {
        self.fingerprint = choice::fingerprint(&choices);
        // A pool is named by its first ticked member that built.
        if let Some(first) = choices.first() {
            self.with = first.enhancing_with();
        }
        self.built = choices;
        self
    }

    pub(crate) fn reach(&self) -> CleanupReach {
        self.reach
    }

    pub(crate) fn destination(&self) -> &str {
        &self.destination
    }

    /// The choices that built, as [`choice::fingerprint`] names them.
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The choices that built, whole, in ticked order -- not what was
    /// chosen, only what `choice::session_enhancer` actually managed.
    pub(crate) fn built_choices(&self) -> &[EnhancerChoice] {
        &self.built
    }

    /// Whether the provider can serve right now: the same `is_available()`
    /// [`ask`] makes before each segment, read fresh. Apple Intelligence's
    /// answer is one synchronous system call whose cost has not been
    /// measured, so the read is timed at debug level.
    pub(crate) fn is_available_now(&self) -> bool {
        let started = Instant::now();
        let available = self.provider.is_available();
        tracing::debug!(
            backend = self.provider.caps().id,
            available,
            elapsed = ?started.elapsed(),
            "cleanup availability read"
        );
        available
    }

    /// Who a dictation's Enhancement waits for: the provider that built, or
    /// a pool's first ticked member that did.
    pub(crate) fn enhancing_with(&self) -> &crate::EnhancingWith {
        &self.with
    }

    /// Why a meeting cannot be tidied with this provider, as a sentence, or
    /// `None` when it can. Apple Intelligence cannot: see
    /// [`APPLE_CANNOT_TIDY`]. A pool never holds Apple Intelligence.
    pub(crate) fn tidy_unavailable(&self) -> Option<&'static str> {
        (self.provider.caps().id == macalla_enhancers::apple::BACKEND_ID)
            .then_some(APPLE_CANNOT_TIDY)
    }

    /// The provider's capabilities; a pool's are its first ticked member's.
    #[cfg(test)]
    pub(crate) fn caps(&self) -> EnhancerCaps {
        self.provider.caps()
    }

    /// Get a provider on this machine ready for the text. A remote one is
    /// told nothing until a dictation's key release.
    pub(crate) fn prewarm(&self) {
        if self.reach == CleanupReach::Local {
            self.provider.prewarm();
        }
    }

    /// This cleanup is being replaced or closed. A pool's last late pass asks
    /// no further member: a model whose key was just removed must not be
    /// asked again once the session has moved on. Anything else carries on:
    /// a local worker (Apple Intelligence, a loopback server) holds no key
    /// that could have gone, and cutting it short would leave the last
    /// dictation half cleaned in History.
    pub(crate) fn retire(&self) {
        if !matches!(self.provider, Provider::Pool(_)) {
            return;
        }
        let last = self.last_worker.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(flag) = last.as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Record `current` as the last dictation's worker, and tell the one
    /// before it to start no further segment.
    fn supersede(&self, current: &Arc<AtomicBool>) {
        let mut last = self.last_worker.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(previous) = last.replace(Arc::clone(current)) {
            if !Arc::ptr_eq(&previous, current) {
                previous.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Every one of `choices` that builds, in order, with its key from `keys`,
/// through [`choice::session_enhancer`]. Building sends nothing.
///
/// - Nothing builds: `None`, with the reason why, or no reason because
///   `choices` is empty.
/// - One builds: that provider, exactly as a single choice always was. A
///   reason beside it means it built but cannot serve right now -- Apple
///   Intelligence switched off -- and the worker asks again before every
///   segment.
/// - More than one builds: a cloud pool ([`Failover`]). The first ticked
///   model is asked first and the next takes over when one fails; a pool is
///   remote. A ticked model that could not start is logged and left out.
pub(crate) fn build(
    choices: &[EnhancerChoice],
    keys: &dyn SecretStore,
) -> (Option<Cleanup>, Option<CleanupNotice>, Vec<crate::Error>) {
    let mut reasons = Vec::new();
    let mut built = Vec::new();
    for picked in choices {
        match choice::session_enhancer(picked, keys) {
            Ok((enhancer, not_now)) => built.push((picked, enhancer, not_now)),
            Err(e) => reasons.push(e),
        }
    }
    if built.is_empty() {
        // Nothing built at all: every reason above is the notice itself,
        // not a "skipped" pool member -- there is no pool for anything to
        // have been skipped from (`Session::cleanup_skipped`'s doc).
        let notice = (!reasons.is_empty())
            .then_some(reasons)
            .map(CleanupNotice::Failed);
        return (None, notice, Vec::new());
    }
    if !reasons.is_empty() {
        let skipped = reasons
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        tracing::info!(skipped = %skipped, "a cleanup choice could not start");
    }
    // Fingerprinted by what built, not by what was chosen: a pool member that
    // gains or loses its key changes where the text can go.
    let built_choices: Vec<EnhancerChoice> = built
        .iter()
        .map(|(picked, _, _)| (*picked).clone())
        .collect();
    if built.len() > 1 {
        let destination = pool_label(built.iter().map(|(picked, _, _)| picked.destination()));
        let members = built
            .into_iter()
            .map(|(picked, enhancer, _)| {
                let (provider, model) = provider_and_model(picked);
                Member {
                    enhancer,
                    provider,
                    model,
                }
            })
            .collect();
        return (
            Some(Cleanup::pool(Failover::new(members), destination).built_from(built_choices)),
            None,
            reasons,
        );
    }
    let (picked, enhancer, not_now) = built.pop().expect("checked non-empty above");
    (
        Some(
            Cleanup::new(Arc::from(enhancer), picked.reach(), picked.destination())
                .built_from(built_choices),
        ),
        not_now.map(CleanupNotice::NotReady),
        reasons,
    )
}

/// A pool member's provider id and model id, for its log lines.
pub(crate) fn provider_and_model(choice: &EnhancerChoice) -> (String, String) {
    match choice {
        EnhancerChoice::AppleIntelligence { .. } => (
            "apple-intelligence".to_string(),
            "foundation-models".to_string(),
        ),
        EnhancerChoice::Endpoint {
            key: KeySource::Stored(id),
            model,
            ..
        } => (id.clone(), model.clone()),
        EnhancerChoice::Endpoint { model, .. } => ("server".to_string(), model.clone()),
        EnhancerChoice::Anthropic { model, .. } => ("anthropic".to_string(), model.clone()),
    }
}

/// Where a pool's text may go, for the label the privacy rule asks for (nothing leaves the machine
/// unless the user turned it on): each
/// provider once, in ticked order, at most two by name and then how many
/// more. "OpenAI, falling back to Anthropic", or "OpenAI, falling back to
/// Anthropic and 1 more".
pub(crate) fn pool_label<S: AsRef<str>>(destinations: impl Iterator<Item = S>) -> String {
    let mut names: Vec<String> = Vec::new();
    for name in destinations {
        let name = name.as_ref();
        if !names.iter().any(|seen| seen == name) {
            names.push(name.to_string());
        }
    }
    match names.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first}, falling back to {second}"),
        [first, second, rest @ ..] => {
            format!("{first}, falling back to {second} and {} more", rest.len())
        }
    }
}

/// A dictation's document as it is built: each committed transcript becomes
/// its segment as it arrives and, with cleanup on this machine, that segment
/// goes to the worker. [`Assembly::finish`] at release is the only place
/// anything waits.
pub(crate) struct Assembly {
    pipeline: Arc<Pipeline>,
    state: SegmentState,
    segments: Vec<Segment>,
    /// Whether Tier 0 left each segment's sentence open, by index.
    open: Vec<bool>,
    /// `None` with cleanup off, and from the moment a worker could not start.
    cleanup: Option<Cleanup>,
    /// Started with the first segment worth sending, so a dictation with no
    /// speech never starts one and never sends a request.
    worker: Option<Worker>,
    /// Set when the dictation is thrown away, when its late cleanup is
    /// dropped, or when the next dictation's worker supersedes this one. The
    /// worker starts no further segment once it is set.
    abandoned: Arc<AtomicBool>,
    /// Time in the per-segment and finishing steps, for `tier0_ms`.
    spent: Duration,
}

/// What [`Assembly::finish`] or [`Assembly::discard`] made.
pub(crate) struct Assembled {
    pub document: Document,
    pub tier0_ms: f64,
    /// Time spent waiting for cleanup at release; `None` when nothing was
    /// waited for.
    pub tier1_ms: Option<f64>,
    pub late: Option<LateCleanup>,
    /// What became of Enhancement; see [`crate::Dictation::enhancement`].
    pub enhancement: Option<crate::EnhancementOutcome>,
    /// Thrown away: by [`Assembly::discard`], or by `cancel` during the
    /// release wait. `late` is `None` then.
    pub discarded: bool,
}

impl Assembly {
    pub fn new(pipeline: Arc<Pipeline>, cleanup: Option<Cleanup>) -> Self {
        Self {
            pipeline,
            state: SegmentState::default(),
            segments: Vec::new(),
            open: Vec::new(),
            cleanup,
            worker: None,
            abandoned: Arc::new(AtomicBool::new(false)),
            spent: Duration::ZERO,
        }
    }

    /// Take one transcript as the engine produced it. A volatile one, or one
    /// with nothing in it, is not a segment and is not sent.
    pub fn commit(&mut self, transcript: &Transcript) {
        let started = Instant::now();
        let segment = self.pipeline.segment(transcript, &mut self.state);
        self.spent += started.elapsed();
        let Some(segment) = segment else {
            return;
        };
        let index = self.segments.len();
        if segment.blocks.iter().any(|b| !b.text().trim().is_empty()) {
            self.send(index, segment.blocks.clone());
        }
        self.open.push(self.state.sentence_open());
        self.segments.push(segment);
    }

    /// Hand one segment to the worker. Only a provider on this machine hears
    /// a segment while the person speaks.
    fn send(&mut self, index: usize, blocks: Vec<Block>) {
        let Some(enhancer) = self
            .cleanup
            .as_ref()
            .filter(|c| c.reach() == CleanupReach::Local)
            .map(|c| c.provider.clone())
        else {
            return;
        };
        if self.worker.is_none() {
            let started = Worker::start(
                enhancer,
                Arc::clone(&self.pipeline),
                Arc::clone(&self.abandoned),
            );
            match started {
                Ok(worker) => {
                    self.worker = Some(worker);
                    if let Some(cleanup) = &self.cleanup {
                        cleanup.supersede(&self.abandoned);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not start cleanup; this dictation stays Tier 0");
                    self.cleanup = None;
                    return;
                }
            }
        }
        if let Some(worker) = self.worker.as_mut() {
            // The worker only stops early once the dictation has been given
            // up on, and then nothing more is committed.
            if worker.jobs.send(Job { index, blocks }).is_ok() {
                worker.sent += 1;
            }
        }
    }

    /// At release: wait for Enhancement, then run the finishing step over
    /// every segment with whatever came back in place of its Tier 0 blocks.
    /// `from` is key release, or a cloud engine's answer, and the wait lasts
    /// until `wait.cap` after it at most, saying [`Progress::Enhancing`] each
    /// `wait.tick` and asking `cancel` every [`DISCARD_POLL`] whether to stop.
    ///
    /// On this machine the wait is for the segments still out. For a remote
    /// provider, `cancel` is asked once more as the request is about to go:
    /// [`Waiting::GiveUp`] and [`Waiting::WithoutEnhancement`] there send
    /// nothing, and [`Waiting::DeliverNow`] sends without waiting and hands the
    /// answer on in [`Assembled::late`]. Otherwise the whole Tier 0 document is
    /// sent on a thread of its own and its
    /// answer waited for, a pool's members sharing the one wait. Whatever has
    /// not come back by the end is in [`Assembled::late`], and what became of
    /// Enhancement is in [`Assembled::enhancement`]. [`Waiting::GiveUp`]
    /// behaves as [`Assembly::discard`] from that moment, and abandons a
    /// request already out.
    pub fn finish(
        mut self,
        from: Instant,
        wait: Wait,
        cancel: &mut dyn FnMut() -> Waiting,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> Assembled {
        let (document, waited) = match self.worker.take() {
            Some(worker) => {
                let waited = self.wait_for_segments(worker, from, wait, cancel, on_progress);
                (self.document(), waited)
            }
            None => {
                let mut document = self.document();
                let remote = self
                    .cleanup
                    .take()
                    .filter(|c| c.reach() == CleanupReach::Remote);
                let waited = match remote {
                    Some(cleanup) if !document.to_plain().trim().is_empty() => {
                        self.ask_remote(&cleanup, &mut document, from, wait, cancel, on_progress)
                    }
                    _ => Waited::default(),
                };
                (document, waited)
            }
        };
        if let Some(enhancement) = waited.enhancement {
            tracing::debug!(
                enhancement = ?enhancement,
                tier1_ms = ?waited.tier1_ms,
                "the wait for Enhancement ended"
            );
        }
        Assembled {
            document,
            tier0_ms: self.spent.as_secs_f64() * 1000.0,
            tier1_ms: waited.tier1_ms,
            late: waited.late,
            enhancement: waited.enhancement,
            discarded: waited.discarded,
        }
    }

    /// The trigger threw the dictation away. Nothing more is sent, nothing is
    /// waited for, and the worker starts no further segment; the one in flight
    /// finishes and its answer goes nowhere. A remote provider is sent nothing
    /// at all. The document is Tier 0 of what was committed, for a trigger
    /// that throws it away.
    pub fn discard(mut self) -> Assembled {
        self.abandoned.store(true, Ordering::Relaxed);
        self.worker = None;
        let document = self.document();
        Assembled {
            document,
            tier0_ms: self.spent.as_secs_f64() * 1000.0,
            tier1_ms: None,
            late: None,
            enhancement: None,
            discarded: true,
        }
    }

    /// The wait for the segments still out on this machine.
    fn wait_for_segments(
        &mut self,
        worker: Worker,
        from: Instant,
        wait: Wait,
        cancel: &mut dyn FnMut() -> Waiting,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> Waited {
        let Worker {
            jobs,
            replies,
            sent,
        } = worker;
        // No more jobs: the worker ends once it has answered the ones it has.
        drop(jobs);
        let waiting = Instant::now();
        let with = self.cleanup.as_ref().map(|c| c.enhancing_with().clone());
        let mut outstanding = sent;
        let mut tally = Tally::default();
        let segments = &mut self.segments;
        let ended = if outstanding == 0 {
            Ended::Answered
        } else {
            wait_on(
                &replies,
                from,
                wait,
                cancel,
                &mut |elapsed| {
                    if let Some(with) = &with {
                        on_progress(Progress::Enhancing { with, elapsed });
                    }
                },
                |reply: Reply| {
                    tally.count(&reply.cleaned);
                    apply(segments, reply);
                    outstanding -= 1;
                    outstanding == 0
                },
            )
        };
        let tier1_ms = Some(crate::millis_since(waiting));
        let enhancement = match ended {
            Ended::GaveUp => {
                self.abandoned.store(true, Ordering::Relaxed);
                return Waited {
                    tier1_ms,
                    discarded: true,
                    ..Waited::default()
                };
            }
            // Everything came back, or the worker has gone without answering
            // and nothing more will come, now or later.
            Ended::Answered | Ended::Gone => {
                return Waited {
                    tier1_ms,
                    enhancement: Some(tally.outcome()),
                    ..Waited::default()
                };
            }
            Ended::OutOfTime => EnhancementOutcome::OutOfTime,
            Ended::Stopped(stopped) => {
                stop_on(stopped, &self.abandoned);
                EnhancementOutcome::Skipped
            }
        };
        Waited {
            tier1_ms,
            late: Some(LateCleanup {
                inner: Late::Local {
                    replies,
                    segments: self.segments.clone(),
                    open: self.open.clone(),
                    outstanding,
                    pipeline: Arc::clone(&self.pipeline),
                    from,
                    abandoned: Arc::clone(&self.abandoned),
                },
            }),
            enhancement: Some(enhancement),
            discarded: false,
        }
    }

    /// The one request for a remote provider, and the wait for its answer.
    /// `document` becomes the answer when it comes back in time and changed.
    fn ask_remote(
        &self,
        cleanup: &Cleanup,
        document: &mut Document,
        from: Instant,
        wait: Wait,
        cancel: &mut dyn FnMut() -> Waiting,
        on_progress: &mut dyn FnMut(Progress<'_>),
    ) -> Waited {
        // Asked once more as the request is about to go, so Esc twice, or the
        // provider turned off, since the engine finished sends nothing. A
        // trigger that delivers now -- its next dictation already asked for,
        // with somewhere to keep the answer -- has it sent, and not waited for.
        let deliver_now = match cancel() {
            Waiting::Continue => false,
            Waiting::GiveUp => {
                self.abandoned.store(true, Ordering::Relaxed);
                return Waited {
                    discarded: true,
                    ..Waited::default()
                };
            }
            Waiting::WithoutEnhancement => {
                return Waited {
                    enhancement: Some(EnhancementOutcome::Skipped),
                    ..Waited::default()
                };
            }
            Waiting::DeliverNow => true,
        };
        // The next dictation to send supersedes this one, so a pool still
        // working through its members for an older document stops at the
        // next member. One provider never looks at the flag.
        cleanup.supersede(&self.abandoned);
        let waiting = Instant::now();
        let sent = send_once(
            cleanup.provider.clone(),
            Arc::clone(&self.pipeline),
            document.blocks.clone(),
            Arc::clone(&self.abandoned),
            from,
        );
        let answers = match sent {
            Ok(answers) => answers,
            Err(e) => {
                tracing::warn!(error = %e, "could not send the text for Enhancement; it keeps its Tier 0 form");
                return Waited {
                    enhancement: Some(EnhancementOutcome::Failed),
                    ..Waited::default()
                };
            }
        };
        if deliver_now {
            return Waited {
                late: Some(LateCleanup {
                    inner: Late::Remote {
                        answers,
                        document: document.clone(),
                        from,
                        abandoned: Arc::clone(&self.abandoned),
                    },
                }),
                enhancement: Some(EnhancementOutcome::Skipped),
                ..Waited::default()
            };
        }
        let with = cleanup.enhancing_with();
        let mut answer = None;
        let ended = wait_on(
            &answers,
            from,
            wait,
            cancel,
            &mut |elapsed| on_progress(Progress::Enhancing { with, elapsed }),
            |answered| {
                answer = Some(answered);
                true
            },
        );
        let tier1_ms = Some(crate::millis_since(waiting));
        let enhancement = match ended {
            Ended::GaveUp => {
                // The answer, when it comes, goes nowhere, and a pool asks no
                // further member.
                self.abandoned.store(true, Ordering::Relaxed);
                return Waited {
                    tier1_ms,
                    discarded: true,
                    ..Waited::default()
                };
            }
            Ended::Answered | Ended::Gone => {
                let enhancement = match answer.map(|answered: Answered| answered.cleaned) {
                    Some(Cleaned::Changed(blocks)) => {
                        *document = Document::new(blocks);
                        EnhancementOutcome::Applied
                    }
                    Some(Cleaned::Unchanged) => EnhancementOutcome::Applied,
                    Some(Cleaned::Unavailable) => EnhancementOutcome::Skipped,
                    // Failed, or the thread ended without answering.
                    Some(Cleaned::Failed) | None => EnhancementOutcome::Failed,
                };
                return Waited {
                    tier1_ms,
                    enhancement: Some(enhancement),
                    ..Waited::default()
                };
            }
            Ended::OutOfTime => EnhancementOutcome::OutOfTime,
            Ended::Stopped(stopped) => {
                stop_on(stopped, &self.abandoned);
                EnhancementOutcome::Skipped
            }
        };
        Waited {
            tier1_ms,
            late: Some(LateCleanup {
                inner: Late::Remote {
                    answers,
                    document: document.clone(),
                    from,
                    abandoned: Arc::clone(&self.abandoned),
                },
            }),
            enhancement: Some(enhancement),
            discarded: false,
        }
    }

    fn document(&mut self) -> Document {
        let started = Instant::now();
        let terms = written_forms(self.pipeline.dictionary());
        let document = self
            .pipeline
            .finish(finishing(&self.segments, &self.open, &terms));
        self.spent += started.elapsed();
        document
    }
}

/// The dictation's end of the one worker thread.
struct Worker {
    jobs: mpsc::Sender<Job>,
    replies: mpsc::Receiver<Reply>,
    /// Jobs handed over, each of which gets exactly one reply.
    sent: usize,
}

struct Job {
    index: usize,
    blocks: Vec<Block>,
}

struct Reply {
    index: usize,
    cleaned: Cleaned,
}

/// What a provider made of some blocks.
enum Cleaned {
    /// Blocks that replace them.
    Changed(Vec<Block>),
    /// It answered, and changed nothing: an answer, never a failure.
    Unchanged,
    /// It failed or panicked, and they keep their Tier 0 form.
    Failed,
    /// It could not serve right now, and was not asked.
    Unavailable,
}

impl Cleaned {
    /// The blocks that replace the ones sent, when there are any.
    fn changed(self) -> Option<Vec<Block>> {
        match self {
            Cleaned::Changed(blocks) => Some(blocks),
            _ => None,
        }
    }
}

/// How many of a dictation's answers were which, for what became of its
/// Enhancement.
#[derive(Default)]
struct Tally {
    answered: usize,
    failed: usize,
    unavailable: usize,
}

impl Tally {
    fn count(&mut self, cleaned: &Cleaned) {
        match cleaned {
            Cleaned::Changed(_) | Cleaned::Unchanged => self.answered += 1,
            Cleaned::Failed => self.failed += 1,
            Cleaned::Unavailable => self.unavailable += 1,
        }
    }

    /// Once nothing more is coming: anything answered means the paste has
    /// Enhancement in it; nothing asked at all is skipped; otherwise it
    /// failed, including when nothing came back.
    fn outcome(&self) -> crate::EnhancementOutcome {
        use crate::EnhancementOutcome;
        if self.answered > 0 {
            EnhancementOutcome::Applied
        } else if self.failed == 0 && self.unavailable > 0 {
            EnhancementOutcome::Skipped
        } else {
            EnhancementOutcome::Failed
        }
    }
}

impl Worker {
    fn start(
        enhancer: Provider,
        pipeline: Arc<Pipeline>,
        abandoned: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        let (jobs, queue) = mpsc::channel::<Job>();
        let (answer, replies) = mpsc::channel();
        std::thread::Builder::new()
            .name("macalla-cleanup".into())
            .spawn(move || {
                let dictionary = pipeline.dictionary();
                let terms = written_forms(dictionary);
                for Job { index, blocks } in queue {
                    if abandoned.load(Ordering::Relaxed) {
                        break;
                    }
                    let cleaned = clean(&enhancer, dictionary, &blocks, &terms, &abandoned);
                    if answer.send(Reply { index, cleaned }).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            jobs,
            replies,
            sent: 0,
        })
    }
}

fn written_forms(dictionary: &Dictionary) -> Vec<String> {
    dictionary
        .written_forms()
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Blocks through the provider, which applies its own guard, then the
/// dictionary's last word on what it changed. Anything but
/// [`Cleaned::Changed`] keeps Tier 0. `stop` is the dictation's flag, which
/// only a pool looks at, between its members.
///
/// Only the blocks the reply changed are settled. A block the guard threw
/// back is Tier 0's exactly, and running the dictionary over it a second
/// time is not what Tier 0 without cleanup would have pasted.
fn clean(
    enhancer: &Provider,
    dictionary: &Dictionary,
    blocks: &[Block],
    terms: &[String],
    stop: &AtomicBool,
) -> Cleaned {
    match ask(enhancer, blocks, terms, stop) {
        Cleaned::Changed(enhanced) => {
            Cleaned::Changed(settle_changed(dictionary, blocks, enhanced))
        }
        other => other,
    }
}

/// Blocks through the provider, with its own guard, and nothing after it.
/// [`clean`] without the dictionary's last word, for a caller that must read
/// the reply before settling it.
fn ask(enhancer: &Provider, blocks: &[Block], terms: &[String], stop: &AtomicBool) -> Cleaned {
    let reply = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        enhancer
            .is_available()
            .then(|| enhancer.enhance(blocks, terms, stop))
    }));
    // Never the text in these: it is what the person said.
    let enhanced = match reply {
        Ok(Some(Ok(enhanced))) => enhanced,
        Ok(None) => {
            tracing::debug!(
                backend = enhancer.caps().id,
                "cleanup is not available right now; the text keeps its Tier 0 form"
            );
            return Cleaned::Unavailable;
        }
        Ok(Some(Err(e))) => {
            tracing::info!(error = %e, "cleanup failed; the text keeps its Tier 0 form");
            return Cleaned::Failed;
        }
        Err(payload) => {
            tracing::warn!(
                reason = %crate::panic_reason(&*payload),
                "cleanup panicked; the text keeps its Tier 0 form"
            );
            return Cleaned::Failed;
        }
    };
    if enhanced == blocks {
        Cleaned::Unchanged
    } else {
        Cleaned::Changed(enhanced)
    }
}

/// The dictionary's last word on what a reply changed. Only the blocks the
/// reply changed are settled; the rest are `blocks`' exactly.
fn settle_changed(dictionary: &Dictionary, blocks: &[Block], enhanced: Vec<Block>) -> Vec<Block> {
    if enhanced.len() != blocks.len() {
        return dictionary.settle(&enhanced);
    }
    enhanced
        .iter()
        .zip(blocks)
        .map(|(new, old)| {
            if new == old {
                old.clone()
            } else {
                dictionary
                    .settle(std::slice::from_ref(new))
                    .pop()
                    .unwrap_or_else(|| new.clone())
            }
        })
        .collect()
}

/// Each reply line that may replace its own source line, settled by the
/// dictionary; `None` for a line that did not change or did not keep to its
/// source line.
///
/// The provider's guard judged the whole part, and a run of words moved across
/// a line break keeps every word of the part, so it passes there. Checked line
/// by line it fails, and that line keeps its words as heard instead of landing
/// against another turn and speaker. Each line gets the strictness the part
/// got: the strict guard when the whole part kept its words, the ratio when it
/// did not, as for a prompt the person wrote.
fn kept_lines(
    dictionary: &Dictionary,
    sources: &[Block],
    replied: Vec<Block>,
) -> Vec<Option<String>> {
    use macalla_core::enhance::Guard;
    let guard = if Guard::SameWords.accepts(sources, &replied) {
        Guard::SameWords
    } else {
        Guard::Ratio
    };
    replied
        .into_iter()
        .zip(sources)
        .map(|(reply, source)| {
            let source = std::slice::from_ref(source);
            let keep = reply != source[0] && guard.accepts(source, std::slice::from_ref(&reply));
            keep.then(|| {
                settle_changed(dictionary, source, vec![reply])
                    .remove(0)
                    .text()
                    .to_string()
            })
        })
        .collect()
}

/// A tidied part's reply as its lines: one block holding exactly `expected`
/// non-empty lines, or `None` when it cannot be matched to the part's lines.
fn reply_lines(reply: &[Block], expected: usize) -> Option<Vec<String>> {
    let [block] = reply else {
        return None;
    };
    let lines: Vec<String> = block
        .text()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    (lines.len() == expected).then_some(lines)
}

fn apply(segments: &mut [Segment], reply: Reply) {
    if let (Cleaned::Changed(cleaned), Some(segment)) =
        (reply.cleaned, segments.get_mut(reply.index))
    {
        segment.blocks = cleaned;
    }
}

/// The segments as the finishing step gets them.
///
/// Each segment is cleaned on its own, and a model asked to punctuate half a
/// sentence tends to end it: "So I opened kubectl." before "and the pods were
/// running" would paste as two sentences where the person said one. So a
/// segment Tier 0 left mid-sentence loses the one '.', '?' or '!' at its end
/// when the next segment continues it at a plain seam, and the seam pass
/// joins them as it would have joined Tier 0's text. The last segment keeps
/// its punctuation, and Tier 0 never ends an open segment with one, so
/// without cleanup this changes nothing. `terms` are the dictionary's written
/// forms; see [`unended`] for what is never taken off.
fn finishing(segments: &[Segment], open: &[bool], terms: &[String]) -> Vec<Segment> {
    let mut out = segments.to_vec();
    for i in 0..out.len().saturating_sub(1) {
        let continued = open.get(i).copied().unwrap_or(false)
            && out[i + 1].opened_by.is_none()
            && !out[i + 1].blocks.is_empty();
        if !continued {
            continue;
        }
        if let Some(block) = out[i].blocks.last_mut() {
            if let Some(unended) = unended(block.text(), terms) {
                *block.text_mut() = unended;
            }
        }
    }
    out
}

/// `text` without the one closing mark a model put at its end, or `None` to
/// leave it alone: there is no mark, the mark follows an ellipsis ("..",
/// "..."), or the text ends in a dictionary term whose written form carries
/// the mark ("Acme Inc."). One mark only: "run?!" becomes "run?", because a
/// run of marks is not all the model's.
fn unended(text: &str, terms: &[String]) -> Option<String> {
    let text = text.trim_end();
    let body = text.strip_suffix(['.', '?', '!'])?;
    if text.ends_with("..") || body.ends_with("..") {
        return None;
    }
    let ends_in_term = terms.iter().any(|term| {
        term.ends_with(['.', '?', '!'])
            && text.ends_with(term.as_str())
            // The whole term, not the end of a longer word.
            && text[..text.len() - term.len()]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
    });
    (!ends_in_term).then(|| body.to_string())
}

/// The one request a remote provider gets for a dictation: the whole Tier 0
/// document's `blocks`, on a thread of its own, so the paste can wait for it
/// without running it. Called from [`Assembly::finish`] and nowhere else,
/// once per dictation, after the trigger's last say. The answer goes down the
/// returned channel, or nowhere once nobody is listening; the thread ends
/// when the provider answers or its own request timeout ends it. `stop` is
/// the dictation's flag, which a pool reads between its members.
fn send_once(
    provider: Provider,
    pipeline: Arc<Pipeline>,
    blocks: Vec<Block>,
    stop: Arc<AtomicBool>,
    from: Instant,
) -> std::io::Result<mpsc::Receiver<Answered>> {
    let (answer, answers) = mpsc::channel();
    std::thread::Builder::new()
        .name("macalla-enhance".into())
        .spawn(move || {
            let dictionary = pipeline.dictionary();
            let cleaned = clean(
                &provider,
                dictionary,
                &blocks,
                &written_forms(dictionary),
                &stop,
            );
            let _ = answer.send(Answered {
                cleaned,
                ms: crate::millis_since(from),
            });
        })?;
    Ok(answers)
}

/// A remote provider's answer, and when it came.
struct Answered {
    cleaned: Cleaned,
    /// From key release, or a cloud engine's answer, to this one.
    ms: f64,
}

/// What a wait at release came to.
#[derive(Default)]
struct Waited {
    /// How long it was; `None` when nothing was waited for.
    tier1_ms: Option<f64>,
    late: Option<LateCleanup>,
    enhancement: Option<EnhancementOutcome>,
    discarded: bool,
}

/// How a wait at release ended.
#[derive(Debug, Clone, Copy)]
enum Ended {
    /// Nothing more is out.
    Answered,
    /// What is out will never come: the thread that would answer has gone.
    Gone,
    /// The wait ran out with something still out.
    OutOfTime,
    /// `cancel` said to give up.
    GaveUp,
    /// `cancel` said to stop waiting without giving up.
    Stopped(Waiting),
}

/// Wait on `answers` until `take` says nothing more is out, the wait runs out
/// `wait.cap` after `from`, or `cancel` stops it. An answer already queued is
/// taken even with the wait gone, so everything that made it in time is used.
/// `report` hears the time since `from` once a tick, the first a whole tick
/// in, while the wait lasts. `cancel` is asked whenever nothing has arrived
/// for [`DISCARD_POLL`] or until the next tick, whichever is sooner, and
/// before the deadline is checked, so an answer on the last ask is kept.
fn wait_on<T>(
    answers: &mpsc::Receiver<T>,
    from: Instant,
    wait: Wait,
    cancel: &mut dyn FnMut() -> Waiting,
    report: &mut dyn FnMut(Duration),
    mut take: impl FnMut(T) -> bool,
) -> Ended {
    let deadline = from + wait.cap;
    // A tick of nothing reports nothing, rather than reporting without end.
    let ticking = !wait.tick.is_zero();
    let mut next_tick = from + wait.tick;
    loop {
        let now = Instant::now();
        if ticking && now >= next_tick && now < deadline {
            report(now - from);
            while next_tick <= now {
                next_tick += wait.tick;
            }
        }
        let mut patience = deadline.saturating_duration_since(now).min(DISCARD_POLL);
        if ticking {
            patience = patience.min(next_tick.saturating_duration_since(now));
        }
        match answers.recv_timeout(patience) {
            Ok(answer) => {
                if take(answer) {
                    return Ended::Answered;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Ended::Gone,
            Err(RecvTimeoutError::Timeout) => {
                // Asked before the deadline is read: what the trigger asks
                // for on the last ask -- Esc twice, a press -- wins over
                // running out of time.
                match cancel() {
                    Waiting::Continue => {}
                    Waiting::GiveUp => return Ended::GaveUp,
                    stopped => return Ended::Stopped(stopped),
                }
                if Instant::now() >= deadline {
                    return Ended::OutOfTime;
                }
            }
        }
    }
}

/// What stopping the wait without giving up does to what is still out.
/// [`Waiting::WithoutEnhancement`], meant for a remote provider, sets the
/// dictation's flag, so a pool asks no further member. A worker on this
/// machine may still finish the segments committed by release: it reads the
/// flag only between segments. [`Waiting::DeliverNow`] leaves all of that to
/// whoever keeps or drops the late cleanup.
fn stop_on(stopped: Waiting, abandoned: &AtomicBool) {
    if stopped == Waiting::WithoutEnhancement {
        abandoned.store(true, Ordering::Relaxed);
    }
}

/// What tidying a meeting's turns made.
pub(crate) struct TidiedTurns {
    /// The turns, in order, each with its words as the provider left them.
    pub(crate) turns: Vec<macalla_core::vtt::Turn>,
    /// Requests sent: one per part.
    pub(crate) parts: usize,
    /// Parts whose words came back as they were heard: the provider was not
    /// available, failed, changed nothing, or replied with a different number
    /// of lines than it was sent, which cannot be matched back to the turns.
    pub(crate) unchanged: usize,
}

/// A meeting's turns tidied part by part, in meeting order, for the Files
/// page. Nothing is pasted, so nothing waits on a budget of time, and a part
/// is one request whether the provider is on this machine or not. The parts
/// are cut by [`meeting_parts::split`], without speaker names, at `budget`
/// characters: a whole meeting in one request would overrun a model's
/// context, which is why a dictation's single request ([`send_once`]) does
/// not fit.
///
/// A part is one request: its lines as one block, a line break between each,
/// and the reply read back line by line. The provider applies its guard to the
/// whole part, and the dictionary has its last word on each line the reply
/// changed, as it does on a dictation's blocks. A reply with a different
/// number of lines cannot be matched to the turns, so the part keeps its words
/// as heard. A matching count is only the first check: each reply line is then
/// checked against its own source line ([`kept_lines`]), with the strictness
/// the whole part got, so a reply that merged two lines and split another, or
/// moved words across a line break, keeps those lines as heard rather than
/// putting words against the wrong turn and speaker. The lines are settled only
/// after the reply is split, because settling rejoins words with spaces and
/// would lose the line breaks.
///
/// `on_part` hears (0, of) as tidying starts, before the first part is sent
/// (not at all when there are no parts),
/// so a caller can say tidying has begun while that part is out, and then
/// each part answered, as (done, of). `stop` is asked before each part; once
/// set, nothing more is sent and the result is `None`.
pub(crate) fn tidy_turns(
    cleanup: &Cleanup,
    dictionary: &Dictionary,
    turns: &[macalla_core::vtt::Turn],
    budget: usize,
    on_part: &mut dyn FnMut(usize, usize),
    stop: &AtomicBool,
) -> Option<TidiedTurns> {
    let parts = meeting_parts::split(turns, budget, Labels::Bare);
    let terms = written_forms(dictionary);
    let mut pieces: Vec<Vec<String>> = vec![Vec::new(); turns.len()];
    // Whether any of a turn's lines came back from the provider. A turn none
    // of whose lines did keeps its text exactly, rather than as the cutting
    // rejoined it.
    let mut answered = vec![false; turns.len()];
    let mut unchanged = 0;
    if !parts.is_empty() {
        on_part(0, parts.len());
    }
    for (index, lines) in parts.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let blocks: Vec<Block> = lines
            .iter()
            .map(|line| Block::Paragraph {
                text: line.text.clone(),
            })
            .collect();
        let part = [Block::Paragraph {
            text: lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        }];
        let kept = ask(&cleanup.provider, &part, &terms, stop)
            .changed()
            .and_then(|reply| reply_lines(&reply, lines.len()))
            .map(|replied| {
                let replied = replied
                    .into_iter()
                    .map(|text| Block::Paragraph { text })
                    .collect();
                kept_lines(dictionary, &blocks, replied)
            })
            .unwrap_or_else(|| vec![None; lines.len()]);
        if kept.iter().all(Option::is_none) {
            unchanged += 1;
        }
        for (line, kept) in lines.iter().zip(&kept) {
            let text = match kept {
                Some(text) => {
                    answered[line.turn] = true;
                    text.as_str()
                }
                None => line.text.as_str(),
            };
            pieces[line.turn].push(text.to_string());
        }
        on_part(index + 1, parts.len());
    }
    // A stop that arrived while the last part was out still means stopped: a
    // pool gives up between its members and that part came back as heard.
    if stop.load(Ordering::Relaxed) {
        return None;
    }
    let turns = turns
        .iter()
        .zip(pieces)
        .zip(answered)
        .map(|((turn, pieces), answered)| macalla_core::vtt::Turn {
            text: if answered {
                pieces.join(" ")
            } else {
                turn.text.clone()
            },
            ..turn.clone()
        })
        .collect();
    Some(TidiedTurns {
        turns,
        parts: parts.len(),
        unchanged,
    })
}

/// Enhancement whose answer is not in the pasted document.
///
/// The paste went ahead without it once [`crate::ENHANCEMENT_WAIT`] ran out,
/// or once the trigger said to stop waiting.
/// This is how the enhanced document reaches History afterwards; it never
/// reaches the paste. It is one of two things:
///
/// - From a provider on this machine: the segments still out. Dropping it
///   tells the worker to start no further segment; the one in flight
///   finishes, and its answer goes nowhere.
/// - From a remote provider: the answer to the one request for the whole
///   Tier 0 document, sent at key release. Dropping it tells a pool to ask
///   no further member; the request already out finishes on its thread, and
///   its answer goes nowhere. What was sent stays sent.
///
/// `wait` blocks for as long as the provider takes, so a trigger that serves
/// the next key press calls it from another thread.
pub struct LateCleanup {
    inner: Late,
}

enum Late {
    Local {
        replies: mpsc::Receiver<Reply>,
        /// Every segment, with the answers that came back in time in place.
        segments: Vec<Segment>,
        open: Vec<bool>,
        outstanding: usize,
        pipeline: Arc<Pipeline>,
        from: Instant,
        abandoned: Arc<AtomicBool>,
    },
    Remote {
        /// Where the one request's answer arrives.
        answers: mpsc::Receiver<Answered>,
        /// The Tier 0 document that was pasted.
        document: Document,
        from: Instant,
        /// This dictation's flag, set when the next dictation to send
        /// supersedes it, or when this is dropped. A pool stops at its next
        /// member once it is set.
        abandoned: Arc<AtomicBool>,
    },
}

/// The whole document once late cleanup is done.
#[derive(Debug, Clone, PartialEq)]
pub struct LateDocument {
    /// What History should hold in place of the pasted text. Anything whose
    /// cleanup failed is Tier 0 here too.
    pub document: Document,
    /// From the moment the wait was counted from -- key release, or a cloud
    /// engine's answer -- to the last answer.
    pub ms: f64,
    /// Some segments never came back and are Tier 0 here: the worker stopped
    /// before it had answered them all -- the next dictation superseded this
    /// one's queue, or the worker thread ended some other way. Not the whole
    /// of the cleanup, so a trigger that reports on it says so. A segment
    /// the provider answered with a failure was answered, and does not make
    /// it partial.
    pub partial: bool,
}

impl LateCleanup {
    /// How much is still to come back: the segments a provider on this
    /// machine had not answered when the paste went ahead, or 1 for a remote
    /// provider's single request, still out.
    pub fn outstanding(&self) -> usize {
        match &self.inner {
            Late::Local { outstanding, .. } => *outstanding,
            Late::Remote { .. } => 1,
        }
    }

    /// Block until the Enhancement still out is back, and return the whole
    /// document.
    pub fn wait(mut self) -> LateDocument {
        match &mut self.inner {
            Late::Local {
                replies,
                segments,
                open,
                outstanding,
                pipeline,
                from,
                ..
            } => {
                while *outstanding > 0 {
                    match replies.recv() {
                        Ok(reply) => {
                            apply(segments, reply);
                            *outstanding -= 1;
                        }
                        // The worker stopped with segments unasked.
                        Err(_) => break,
                    }
                }
                let terms = written_forms(pipeline.dictionary());
                LateDocument {
                    document: pipeline.finish(finishing(segments, open, &terms)),
                    ms: crate::millis_since(*from),
                    partial: *outstanding > 0,
                }
            }
            Late::Remote {
                answers,
                document,
                from,
                ..
            } => match answers.recv() {
                Ok(Answered {
                    cleaned: Cleaned::Changed(blocks),
                    ms,
                }) => LateDocument {
                    document: Document::new(blocks),
                    ms,
                    partial: false,
                },
                // Answered with nothing to change, or failed: Tier 0 stands.
                Ok(Answered { ms, .. }) => LateDocument {
                    document: document.clone(),
                    ms,
                    partial: false,
                },
                // The request thread ended without answering.
                Err(_) => LateDocument {
                    document: document.clone(),
                    ms: crate::millis_since(*from),
                    partial: true,
                },
            },
        }
    }
}

impl Drop for LateCleanup {
    fn drop(&mut self) {
        let (Late::Local { abandoned, .. } | Late::Remote { abandoned, .. }) = &self.inner;
        abandoned.store(true, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for LateCleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not the text: it is what the person said.
        f.debug_struct("LateCleanup")
            .field("outstanding", &self.outstanding())
            .field("remote", &matches!(self.inner, Late::Remote { .. }))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_answer_to_whether_to_stop_waiting_ends_the_wait_its_own_way() {
        // No answer here depends on how busy the machine is: a stopping answer
        // is heard long before a minute could run out, and Continue starts
        // with its wait already over, so it runs out at once.
        let long = Wait {
            tick: Duration::ZERO,
            cap: Duration::from_secs(60),
        };
        let short = Wait {
            tick: Duration::ZERO,
            cap: Duration::from_secs(1),
        };
        for answer in [
            Waiting::Continue,
            Waiting::GiveUp,
            Waiting::DeliverNow,
            Waiting::WithoutEnhancement,
        ] {
            let (from, wait) = if answer == Waiting::Continue {
                (already_over(short), short)
            } else {
                (Instant::now(), long)
            };
            // Kept alive, so the wait never hears that nothing will come.
            let (_answering, answers) = mpsc::channel::<()>();
            let ended = wait_on(
                &answers,
                from,
                wait,
                &mut || answer,
                &mut |_| panic!("a tick of nothing reports nothing"),
                |()| true,
            );
            let as_it_should = match answer {
                Waiting::Continue => matches!(ended, Ended::OutOfTime),
                Waiting::GiveUp => matches!(ended, Ended::GaveUp),
                Waiting::DeliverNow | Waiting::WithoutEnhancement => {
                    matches!(ended, Ended::Stopped(stopped) if stopped == answer)
                }
            };
            assert!(as_it_should, "{answer:?} ended {ended:?}");

            let abandoned = AtomicBool::new(false);
            stop_on(answer, &abandoned);
            assert_eq!(
                abandoned.load(Ordering::Relaxed),
                answer == Waiting::WithoutEnhancement,
                "only going without Enhancement stops what is still out: {answer:?}"
            );
        }
    }

    /// A start for `wait` that is already more than its cap in the past.
    fn already_over(wait: Wait) -> Instant {
        Instant::now()
            .checked_sub(wait.cap * 2)
            .expect("the clock has run for longer than the wait")
    }

    #[test]
    fn an_answer_given_as_the_wait_runs_out_is_still_heard() {
        // The wait is over before it first asks. What `cancel` says on that
        // ask wins over running out of time: Esc twice, or a press, at the
        // very end of the wait is not turned into a paste of Tier 0.
        let wait = Wait {
            tick: Duration::ZERO,
            cap: Duration::from_secs(1),
        };
        for (answer, expected) in [
            (Waiting::Continue, Ended::OutOfTime),
            (Waiting::GiveUp, Ended::GaveUp),
            (Waiting::DeliverNow, Ended::Stopped(Waiting::DeliverNow)),
            (
                Waiting::WithoutEnhancement,
                Ended::Stopped(Waiting::WithoutEnhancement),
            ),
        ] {
            let (_answering, answers) = mpsc::channel::<()>();
            let ended = wait_on(
                &answers,
                already_over(wait),
                wait,
                &mut || answer,
                &mut |_| {},
                |()| true,
            );
            assert_eq!(format!("{ended:?}"), format!("{expected:?}"), "{answer:?}");
        }
    }

    /// What the first of two segments ends with once the second continues
    /// its sentence at a plain seam.
    fn at_the_seam(first: &str, terms: &[&str]) -> String {
        let terms: Vec<String> = terms.iter().map(|t| t.to_string()).collect();
        let segments = [
            Segment::paragraph(first.to_string()),
            Segment::paragraph("and then it stopped".to_string()),
        ];
        finishing(&segments, &[true, false], &terms)[0].blocks[0]
            .text()
            .to_string()
    }

    #[test]
    fn a_segment_left_open_loses_the_closing_mark_a_model_gave_it() {
        assert_eq!(
            at_the_seam("So I opened kubectl.", &[]),
            "So I opened kubectl"
        );
        assert_eq!(at_the_seam("Did it run?", &[]), "Did it run");
        assert_eq!(at_the_seam("It ran!", &[]), "It ran");
    }

    #[test]
    fn only_one_mark_is_taken_off() {
        assert_eq!(
            at_the_seam("Did it really run?!", &[]),
            "Did it really run?"
        );
        assert_eq!(at_the_seam("It ran!!", &[]), "It ran!");
    }

    #[test]
    fn an_ellipsis_keeps_every_dot_and_any_mark_after_it() {
        assert_eq!(at_the_seam("So I waited...", &[]), "So I waited...");
        assert_eq!(at_the_seam("So I waited..", &[]), "So I waited..");
        assert_eq!(at_the_seam("So I waited..?", &[]), "So I waited..?");
    }

    #[test]
    fn a_dictionary_term_that_ends_with_the_mark_keeps_it() {
        assert_eq!(
            at_the_seam("I emailed Acme Inc.", &["Acme Inc."]),
            "I emailed Acme Inc."
        );
        assert_eq!(
            at_the_seam("I emailed Acme Inc.", &["Inc."]),
            "I emailed Acme Inc."
        );
        assert_eq!(
            at_the_seam("We met at DeCo.", &["Co."]),
            "We met at DeCo",
            "the end of a longer word is not the term"
        );
    }

    #[test]
    fn a_segment_that_closed_its_sentence_or_comes_last_keeps_its_mark() {
        let segments = [
            Segment::paragraph("It ran.".to_string()),
            Segment::paragraph("It stopped.".to_string()),
        ];
        let out = finishing(&segments, &[false, true], &[]);
        assert_eq!(out[0].blocks[0].text(), "It ran.", "Tier 0 closed it");
        assert_eq!(out[1].blocks[0].text(), "It stopped.", "the last one");
    }

    #[test]
    fn a_pool_label_names_each_provider_once_in_ticked_order_and_counts_the_rest() {
        let label = |names: &[&str]| pool_label(names.iter().copied());
        assert_eq!(label(&[]), "");
        assert_eq!(label(&["OpenAI"]), "OpenAI");
        assert_eq!(
            label(&["OpenAI", "Anthropic"]),
            "OpenAI, falling back to Anthropic"
        );
        assert_eq!(
            label(&["OpenAI", "Anthropic", "Gemini"]),
            "OpenAI, falling back to Anthropic and 1 more"
        );
        assert_eq!(
            label(&["OpenAI", "Anthropic", "Gemini", "Mistral"]),
            "OpenAI, falling back to Anthropic and 2 more"
        );
        assert_eq!(
            label(&["OpenAI", "OpenAI", "Anthropic"]),
            "OpenAI, falling back to Anthropic",
            "two models from one provider are one provider"
        );
    }

    fn pool_of(models: &[(&str, &str)]) -> Vec<EnhancerChoice> {
        choice::cleanup_enhancers(&choice::CleanupChoice::Pool {
            models: models
                .iter()
                .map(|(provider, model)| choice::PoolModel {
                    provider: provider.to_string(),
                    model: model.to_string(),
                })
                .collect(),
        })
    }

    #[test]
    fn more_than_one_ticked_model_that_builds_is_a_remote_pool() {
        let keys = macalla_platform::secrets::MemoryStore::new();
        keys.set("provider-key.openai", "test-key").unwrap();
        keys.set("provider-key.anthropic", "test-key").unwrap();

        let (cleanup, reason, skipped) = build(
            &pool_of(&[("openai", "gpt-test"), ("anthropic", "claude-test")]),
            &keys,
        );

        let cleanup = cleanup.expect("the pool built");
        assert!(reason.is_none(), "{reason:?}");
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(matches!(cleanup.provider, Provider::Pool(_)));
        assert_eq!(cleanup.reach(), CleanupReach::Remote);
        assert_eq!(cleanup.destination(), "OpenAI, falling back to Anthropic");
    }

    #[test]
    fn a_pool_with_only_one_key_left_is_that_provider_alone() {
        let keys = macalla_platform::secrets::MemoryStore::new();
        keys.set("provider-key.anthropic", "test-key").unwrap();

        let (cleanup, reason, skipped) = build(
            &pool_of(&[("openai", "gpt-test"), ("anthropic", "claude-test")]),
            &keys,
        );

        let cleanup = cleanup.expect("the model with a key built");
        assert!(reason.is_none(), "{reason:?}");
        assert!(
            matches!(
                skipped.as_slice(),
                [crate::Error::NeedsKey { provider }] if provider == "openai"
            ),
            "{skipped:?}"
        );
        assert!(matches!(cleanup.provider, Provider::One(_)));
        assert_eq!(cleanup.reach(), CleanupReach::Remote);
        assert_eq!(cleanup.destination(), "Anthropic");
    }

    #[test]
    fn a_pool_with_no_key_saved_does_not_start_and_says_why() {
        let keys = macalla_platform::secrets::MemoryStore::new();
        let (cleanup, reason, skipped) = build(
            &pool_of(&[("openai", "gpt-test"), ("anthropic", "claude-test")]),
            &keys,
        );
        assert!(cleanup.is_none());
        assert!(reason.is_some_and(|reason| !reason.to_string().is_empty()));
        // Nothing built at all: there is no pool for anything to have been
        // skipped from -- the notice above is the whole story.
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    /// A provider that says only which backend it is.
    struct Named(&'static str);

    impl Enhancer for Named {
        fn caps(&self) -> EnhancerCaps {
            EnhancerCaps {
                id: self.0,
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            true
        }
        fn enhance(&self, blocks: &[Block], _terms: &[String]) -> macalla_core::Result<Vec<Block>> {
            Ok(blocks.to_vec())
        }
    }

    #[test]
    fn a_meeting_is_not_tidied_with_apple_intelligence_but_is_with_other_providers() {
        let apple = Cleanup::new(
            Arc::new(Named(macalla_enhancers::apple::BACKEND_ID)),
            CleanupReach::Local,
            "Apple Intelligence",
        );
        let server = Cleanup::new(
            Arc::new(Named("endpoint")),
            CleanupReach::Remote,
            "Your server",
        );

        assert_eq!(apple.tidy_unavailable(), Some(APPLE_CANNOT_TIDY));
        assert_eq!(server.tidy_unavailable(), None);
    }

    /// A provider that counts how many times it was told to get ready.
    struct Counting(std::sync::atomic::AtomicUsize);

    impl Enhancer for Counting {
        fn caps(&self) -> EnhancerCaps {
            EnhancerCaps {
                id: "counting",
                typical_ms: None,
                constrained_output: false,
            }
        }
        fn is_available(&self) -> bool {
            true
        }
        fn prewarm(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn enhance(&self, blocks: &[Block], _terms: &[String]) -> macalla_core::Result<Vec<Block>> {
            Ok(blocks.to_vec())
        }
    }

    #[test]
    fn prewarm_reaches_a_local_provider_but_never_a_remote_one() {
        let local_counter = Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0)));
        let local = Cleanup::new(
            Arc::clone(&local_counter) as Arc<dyn Enhancer>,
            CleanupReach::Local,
            "Apple Intelligence",
        );
        local.prewarm();
        assert_eq!(local_counter.0.load(Ordering::Relaxed), 1);

        let remote_counter = Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0)));
        let remote = Cleanup::new(
            Arc::clone(&remote_counter) as Arc<dyn Enhancer>,
            CleanupReach::Remote,
            "Your server",
        );
        remote.prewarm();
        assert_eq!(remote_counter.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_pool_member_gaining_its_key_changes_the_fingerprint() {
        let keys = macalla_platform::secrets::MemoryStore::new();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        let choices = pool_of(&[("openai", "gpt-test"), ("anthropic", "claude-test")]);

        let before = build(&choices, &keys).0.expect("Anthropic built");
        keys.set("provider-key.openai", "test-key").unwrap();
        let after = build(&choices, &keys).0.expect("the pool built");

        // The same stored choice, but the text can now reach OpenAI too.
        assert_ne!(before.fingerprint(), after.fingerprint());
        assert_eq!(
            after.fingerprint(),
            build(&choices, &keys)
                .0
                .expect("the pool built")
                .fingerprint(),
            "the same build gives the same fingerprint"
        );
    }
}
