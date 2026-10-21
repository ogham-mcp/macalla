// SPDX-License-Identifier: MIT
//! The Files page: a meeting from a file, heard or read, tidied and summarised
//! when the person asks. Humble: the rules are `macalla_session::meeting` and
//! `macalla_session::summary`. This maps them to commands and events, runs the
//! work on a thread of its own, and keeps the one current meeting in memory.
//!
//! - **One job at a time.** A second file, or a summary, while one runs is
//!   refused with a sentence, not queued.
//! - **Kept in memory only**, until it is cleared or the app quits. Nothing is
//!   written to History.
//! - **The engine is borrowed** from the dictation thread
//!   ([`Dictator::meeting_tools`]), so a file shares the model dictation
//!   already loaded. A transcript file needs no engine at all.
//! - **Nothing is sent anywhere the page did not say.** Tidying and a summary
//!   carry the destination the page showed, and are refused, with nothing
//!   sent, when the person's choice has changed since.
//! - **A live meeting is a job too**, heard by `macalla_session::live_meeting`
//!   from the microphone dictation uses, as You, and the call's sound, as
//!   Others, until the person stops or discards it. It holds the microphone
//!   for its whole length ([`Dictator::microphone_for_meeting`]), so no
//!   dictation runs meanwhile, and what it heard becomes the current meeting
//!   exactly as a file's does. Its words are in `meeting_words`.
//! - **A turn can be corrected by hand** ([`meeting_edit_turn`]): its words
//!   change, its time and speaker do not, and Copy and Save read the corrected
//!   words. A summary written before the correction is kept and
//!   marked out of date, never shown as current. A meeting held from the
//!   last quit has its file written again with the correction, so a second
//!   quit before the person decides on it loses nothing.
//! - **Several files at once are a batch**, one job: each is heard, or read,
//!   in turn with [`run_job`] on the same borrowed engine, and its transcript
//!   written to Downloads as it finishes, then dropped before the next
//!   ([`run_batch`]). A batch never becomes the current meeting, never tidies
//!   and never summarises: those stay one file's actions. One file failing
//!   does not stop the rest; Stop stops the file being heard and every one
//!   still waiting, and what was saved stays saved.

pub mod translation;

use crate::command_error::{CommandError, OrSay};
use crate::dictation::{Dictator, MicrophoneClaim, MicrophoneOwner};
use crate::history::HistoryHandle;
use macalla_platform::secrets::SystemSecrets;
use macalla_session::live_meeting::{
    self, CallAudio, Committed, LiveMeeting, LiveProgress, Source,
};
use macalla_session::meeting::{self, MeetingFile, MeetingTools, MeetingTranscript};
use macalla_session::summary::Summariser;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;
use tauri::{AppHandle, Emitter, State, Wry};
use tauri_plugin_dialog::DialogExt;

/// Said when a recording is dropped and no engine is open to hear it.
const NO_ENGINE: &str = "No speech engine is ready to hear this recording. Choose one under \
                          Transcription on the Models page.";

/// Said when tidying is asked for and no session is open to lend the cleanup
/// provider, which it lends with the engine.
const NO_SESSION: &str =
    "Tidying needs a speech engine to be open. Choose one under Transcription on the Models page.";

/// Said when tidying is asked for with cleanup off.
const CLEANUP_OFF: &str =
    "Enhancement is off, so there is nothing to tidy with. Choose an Enhancement provider on the Models page.";

/// Said when a second job is started while one runs.
const BUSY: &str = "A file is already being worked on. Stop it, or wait for it to finish.";

/// Said when a job's thread panicked.
const WENT_WRONG: &str = "Something went wrong working on this file, and nothing was kept.";

/// The Files page's state: the job running, if any, and the meeting it made.
#[derive(Clone, Default)]
pub struct Meetings(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    next_job: u64,
    running: Option<Running>,
    current: Option<MeetingView>,
    /// The live meeting that was recording when the app began to quit, by
    /// its job: what [`Meetings::save_on_quit`] holds for the next launch.
    quit_live: Option<u64>,
    /// The meeting held from the last quit, while it is the current one:
    /// its job, and the file it was read from, which goes once the person
    /// copies, saves or clears it.
    held: Option<(u64, PathBuf)>,
    /// The last batch, running or ended, until the next one starts or the
    /// person clears its list: what a page that opens later shows.
    batch: Option<Batch>,
    /// The current meeting translated, kept beside it until another meeting
    /// or a Clear (`translation`).
    translation: Option<translation::Copy>,
}

struct Running {
    job: u64,
    stop: Arc<AtomicBool>,
    /// Set only while the job hears on a borrowed engine: not while it
    /// decodes, tidies or summarises.
    holds_model: bool,
    thread: Option<JoinHandle<()>>,
    /// Set only for a summary: what a page that opens while it runs needs.
    summary: Option<SummaryRun>,
    /// Set only for a live meeting: how it is asked to end, and what a page
    /// that opens while it runs needs.
    live: Option<LiveRun>,
    /// Set only for a translation: what a page that opens while it runs
    /// needs.
    translation: Option<translation::TranslationRun>,
}

/// A live meeting running: how to end it, whether it was asked to, and the
/// last progress it sent.
struct LiveRun {
    ending: Arc<live_meeting::Ending>,
    stopping: bool,
    progress: Option<LiveProgressEvent>,
    /// What the meeting has committed so far, once it is ready to record:
    /// what a quit can still keep if the meeting cannot finish in time.
    committed: Option<Committed>,
}

/// A summary running: when it started, where it sends the transcript and how
/// long a part can take once its summariser is built, and the last progress
/// it sent.
struct SummaryRun {
    started: Instant,
    destination: Option<String>,
    pace: Option<&'static str>,
    progress: Option<SummaryProgressEvent>,
}

/// What a job's thread tells the page as the job ends.
enum Ending {
    Transcript(MeetingView),
    Summary(MeetingView),
    Error(ErrorEvent),
}

fn emit_ending(app: &AppHandle<Wry>, ending: Ending) {
    let _ = match ending {
        Ending::Transcript(view) => app.emit("meeting-transcript", &view),
        Ending::Summary(view) => app.emit("meeting-summary", &view),
        Ending::Error(event) => app.emit("meeting-error", event),
    };
}

impl Meetings {
    /// Start a job if none is running: its number and its stop flag.
    fn begin(&self) -> Result<(u64, Arc<AtomicBool>), CommandError> {
        let mut inner = self
            .0
            .lock()
            .map_err(|_| crate::meeting_words::STATE_UNREADABLE)?;
        Ok(Self::begin_in(&mut inner)?)
    }

    fn begin_in(inner: &mut Inner) -> Result<(u64, Arc<AtomicBool>), String> {
        if let Some(running) = &inner.running {
            return Err(match running.live {
                Some(_) => crate::meeting_words::LIVE_BUSY,
                None => BUSY,
            }
            .to_string());
        }
        inner.next_job += 1;
        let job = inner.next_job;
        let stop = Arc::new(AtomicBool::new(false));
        inner.running = Some(Running {
            job,
            stop: Arc::clone(&stop),
            holds_model: false,
            thread: None,
            summary: None,
            live: None,
            translation: None,
        });
        Ok((job, stop))
    }

    /// Start a live meeting if no job is running: its number, and how it is
    /// asked to end.
    fn begin_live(&self) -> Result<(u64, Arc<live_meeting::Ending>), CommandError> {
        let mut inner = self
            .0
            .lock()
            .map_err(|_| crate::meeting_words::STATE_UNREADABLE)?;
        let (job, _) = Self::begin_in(&mut inner)?;
        let ending = Arc::new(live_meeting::Ending::new());
        if let Some(running) = inner.running.as_mut() {
            running.live = Some(LiveRun {
                ending: Arc::clone(&ending),
                stopping: false,
                progress: None,
                committed: None,
            });
        }
        Ok((job, ending))
    }

    /// Keep live meeting `job`'s committed words within reach, for
    /// [`Meetings::save_on_quit`].
    fn live_ready(&self, job: u64, committed: Committed) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(live) = inner
                .running
                .as_mut()
                .filter(|r| r.job == job)
                .and_then(|r| r.live.as_mut())
            {
                live.committed = Some(committed);
            }
        }
    }

    /// Keep live meeting `job`'s latest progress, for [`Meetings::running_live`].
    fn live_progressed(&self, job: u64, progress: LiveProgressEvent) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(live) = inner
                .running
                .as_mut()
                .filter(|r| r.job == job)
                .and_then(|r| r.live.as_mut())
            {
                live.progress = Some(progress);
            }
        }
    }

    /// The live meeting running, if any, for a page that opens while it
    /// records.
    fn running_live(&self) -> Option<RunningLiveView> {
        let inner = self.0.lock().ok()?;
        let running = inner.running.as_ref()?;
        let live = running.live.as_ref()?;
        Some(RunningLiveView {
            job: running.job,
            stopping: live.stopping,
            progress: live.progress.clone(),
        })
    }

    /// End the live meeting running, if any, and keep what it heard.
    fn stop_live(&self) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(live) = inner.running.as_mut().and_then(|r| r.live.as_mut()) {
                live.stopping = true;
                live.ending.stop();
            }
        }
    }

    /// Start a summary of the current meeting, taken under the same lock, so
    /// a file finishing in between cannot change which meeting it is.
    fn begin_summary(&self) -> Result<(u64, Arc<AtomicBool>, MeetingView), CommandError> {
        let mut inner = self
            .0
            .lock()
            .map_err(|_| crate::meeting_words::STATE_UNREADABLE)?;
        let current = inner
            .current
            .clone()
            .ok_or_else(|| "There is no file on the Files page to summarise.".to_string())?;
        let (job, stop) = Self::begin_in(&mut inner)?;
        if let Some(running) = inner.running.as_mut() {
            running.summary = Some(SummaryRun {
                started: Instant::now(),
                destination: None,
                pace: None,
                progress: None,
            });
        }
        Ok((job, stop, current))
    }

    /// Keep summary `job`'s latest progress, for [`Meetings::running_summary`].
    fn summary_progressed(&self, job: u64, progress: SummaryProgressEvent) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(summary) = inner
                .running
                .as_mut()
                .filter(|r| r.job == job)
                .and_then(|r| r.summary.as_mut())
            {
                summary.progress = Some(progress);
            }
        }
    }

    /// Keep where summary `job` sends the transcript, and how long a part can
    /// take, once its summariser is built. A page that opens later names what
    /// is running, not what the settings say by then.
    fn summary_built(&self, job: u64, destination: &str, pace: Option<&'static str>) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(summary) = inner
                .running
                .as_mut()
                .filter(|r| r.job == job)
                .and_then(|r| r.summary.as_mut())
            {
                summary.destination = Some(destination.to_string());
                summary.pace = pace;
            }
        }
    }

    /// The summary running, if any, for a page that opens while it works.
    fn running_summary(&self) -> Option<RunningSummaryView> {
        let inner = self.0.lock().ok()?;
        let running = inner.running.as_ref()?;
        let summary = running.summary.as_ref()?;
        Some(RunningSummaryView {
            job: running.job,
            elapsed_ms: u64::try_from(summary.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            stopping: running.stop.load(Ordering::Relaxed),
            destination: summary.destination.clone(),
            pace: summary.pace.map(str::to_string),
            progress: summary.progress.clone(),
        })
    }

    /// Keep the thread doing job `job`, so quitting can wait for it.
    fn attach(&self, job: u64, thread: JoinHandle<()>) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(running) = inner.running.as_mut().filter(|r| r.job == job) {
                running.thread = Some(thread);
            }
        }
    }

    /// Mark job `job` as holding a borrowed model, or as no longer holding
    /// one. Refused, with `false`, once the job has been told to stop. Quitting
    /// and this take the same lock, so either quitting sees the job holding a
    /// model and waits for it, or the job sees the stop and borrows nothing.
    fn holding_model(&self, job: u64, holds: bool) -> bool {
        let Ok(mut inner) = self.0.lock() else {
            return false;
        };
        match inner.running.as_mut().filter(|r| r.job == job) {
            Some(running) if holds && running.stop.load(Ordering::Relaxed) => false,
            Some(running) => {
                running.holds_model = holds;
                true
            }
            None => false,
        }
    }

    /// A file job has ended: forget it, and keep the meeting it made, if any.
    fn end(&self, job: u64, made: Option<MeetingView>) {
        if let Ok(mut inner) = self.0.lock() {
            if inner.running.as_ref().is_some_and(|r| r.job == job) {
                inner.running = None;
            }
            if let Some(view) = made {
                inner.current = Some(view);
                inner.translation = None;
            }
        }
    }

    /// A summary job has ended. Its summary is kept only if the meeting it
    /// summarised is still the current one: not after a Clear, and not after
    /// another file. It goes onto that meeting as it is now, so a turn
    /// corrected while the summary ran keeps its correction, and the summary
    /// is marked out of date when the turns it read are not the turns kept.
    /// Returns the meeting as kept, when it was.
    fn end_summary(&self, job: u64, summarised: Option<MeetingView>) -> Option<MeetingView> {
        let mut inner = self.0.lock().ok()?;
        if inner.running.as_ref().is_some_and(|r| r.job == job) {
            inner.running = None;
        }
        let summarised = summarised?;
        let current = inner.current.as_mut().filter(|c| c.job == summarised.job)?;
        current.summary_out_of_date = current.turns != summarised.turns;
        current.summary = summarised.summary;
        current.summary_by = summarised.summary_by;
        current.revision += 1;
        Some(current.clone())
    }

    /// Correct turn `index` of meeting `job` by hand, and return the meeting
    /// as kept. The turn's words become `text`, its runs of whitespace, line
    /// breaks included, made single spaces as a WebVTT cue has them; its
    /// time and speaker stay. A summary already written is marked out of
    /// date. The same words again change nothing.
    ///
    /// Refused, with nothing changed: words that are all whitespace (an
    /// empty turn would drop out of a written WebVTT file and move every
    /// summary quote after it onto the wrong turn), a `job` that is not the
    /// current meeting, and an `index` it has no turn for. When the meeting
    /// is the one held from the last quit, its file is written again first,
    /// and a write that fails refuses the correction too.
    fn edit_turn(&self, job: u64, index: usize, text: &str) -> Result<MeetingView, &'static str> {
        use crate::meeting_words as words;
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return Err(words::EDIT_EMPTY);
        }
        let mut inner = self.0.lock().map_err(|_| words::STATE_UNREADABLE)?;
        let held = inner
            .held
            .as_ref()
            .filter(|(held, _)| *held == job)
            .map(|(_, path)| path.clone());
        let current = inner
            .current
            .as_mut()
            .filter(|c| c.job == job)
            .ok_or(words::EDIT_STALE)?;
        let turn = current.turns.get(index).ok_or(words::EDIT_NO_SUCH_TURN)?;
        if turn.text == text {
            return Ok(current.clone());
        }
        let mut edited = current.clone();
        edited.turns[index].text = text;
        edited.summary_out_of_date |= edited.summary.is_some();
        edited.revision += 1;
        if let Some(path) = held {
            rewrite_held(&path, &edited).map_err(|error| {
                tracing::warn!(%error, "could not write a correction into the held live meeting");
                words::EDIT_HELD_NOT_WRITTEN
            })?;
        }
        *current = edited.clone();
        Ok(edited)
    }

    /// A file job's thread has its outcome: end the job, then tell the page.
    /// In that order, so a job the page starts on hearing it is not refused as
    /// busy.
    fn finish(&self, job: u64, outcome: Result<MeetingView, String>, tell: impl FnOnce(Ending)) {
        match outcome {
            Ok(view) => {
                self.end(job, Some(view.clone()));
                tell(Ending::Transcript(view));
            }
            Err(message) => {
                self.end(job, None);
                tell(Ending::Error(ErrorEvent { job, message }));
            }
        }
    }

    /// A summary job's thread has its outcome: end the job, then tell the
    /// page, as [`Meetings::finish`] does.
    fn finish_summary(
        &self,
        job: u64,
        outcome: Result<MeetingView, String>,
        tell: impl FnOnce(Ending),
    ) {
        match outcome {
            Ok(view) => {
                let kept = self.end_summary(job, Some(view.clone()));
                tell(Ending::Summary(kept.unwrap_or(view)));
            }
            Err(message) => {
                self.end_summary(job, None);
                tell(Ending::Error(ErrorEvent { job, message }));
            }
        }
    }

    /// Stop the job running, if any. A live meeting keeps nothing: this is
    /// its Discard, and what quitting does to it.
    fn stop(&self) {
        if let Ok(mut inner) = self.0.lock() {
            let Inner { running, batch, .. } = &mut *inner;
            if let Some(running) = running.as_mut() {
                running.stop.store(true, Ordering::Relaxed);
                if let Some(live) = running.live.as_mut() {
                    live.stopping = true;
                    live.ending.cancel();
                }
                if let Some(batch) = batch.as_mut().filter(|b| b.job == running.job) {
                    batch.stopping = true;
                }
            }
        }
    }

    /// Refuse, with the sentence [`Meetings::begin_in`] would give, while a
    /// job runs: asked before a batch takes its paths from [`ChosenFiles`], so
    /// a refusal does not use up a drop's trust. A job that starts in between
    /// is still refused by [`Meetings::begin_batch`].
    fn refuse_if_busy(&self) -> Result<(), CommandError> {
        let inner = self
            .0
            .lock()
            .map_err(|_| crate::meeting_words::STATE_UNREADABLE)?;
        match &inner.running {
            Some(running) if running.live.is_some() => Err(crate::meeting_words::LIVE_BUSY.into()),
            Some(_) => Err(BUSY.into()),
            None => Ok(()),
        }
    }

    /// Start a batch of `names` if no job is running: its number and its stop
    /// flag. The batch replaces the last one's list.
    fn begin_batch(&self, names: Vec<String>) -> Result<(u64, Arc<AtomicBool>), CommandError> {
        let mut inner = self
            .0
            .lock()
            .map_err(|_| crate::meeting_words::STATE_UNREADABLE)?;
        let (job, stop) = Self::begin_in(&mut inner)?;
        inner.batch = Some(Batch::new(job, names));
        Ok((job, stop))
    }

    /// Keep the batch's rows as they are now, for [`Meetings::batch`], and
    /// return it as the page shows it. Its rows are kept whole, as the
    /// batch's thread has them; a Stop pressed since is kept too.
    fn batch_changed(&self, changed: &Batch) -> Option<BatchView> {
        let mut inner = self.0.lock().ok()?;
        let batch = inner.batch.as_mut().filter(|b| b.job == changed.job)?;
        batch.rows.clone_from(&changed.rows);
        Some(batch.view())
    }

    /// Batch `job` has ended: forget the job, and keep its rows, ended.
    fn end_batch(&self, job: u64) -> Option<BatchView> {
        let mut inner = self.0.lock().ok()?;
        if inner.running.as_ref().is_some_and(|r| r.job == job) {
            inner.running = None;
        }
        let batch = inner.batch.as_mut().filter(|b| b.job == job)?;
        batch.running = false;
        // Only a batch whose thread went wrong ends with a file unfinished.
        for row in &mut batch.rows {
            if matches!(
                row.state,
                RowState::Waiting | RowState::Opening | RowState::Hearing { .. }
            ) {
                row.state = RowState::Failed;
                row.why = Some(crate::meeting_words::BATCH_WENT_WRONG.to_string());
            }
        }
        Some(batch.view())
    }

    /// The last batch, running or ended, for a page that opens later.
    fn batch(&self) -> Option<BatchView> {
        let inner = self.0.lock().ok()?;
        inner.batch.as_ref().map(Batch::view)
    }

    /// Take an ended batch's list off the page. A running one stays.
    fn clear_batch(&self) {
        if let Ok(mut inner) = self.0.lock() {
            if inner.batch.as_ref().is_some_and(|b| !b.running) {
                inner.batch = None;
            }
        }
    }

    fn current(&self) -> Option<MeetingView> {
        self.0.lock().ok().and_then(|inner| inner.current.clone())
    }

    fn clear(&self) {
        if let Ok(mut inner) = self.0.lock() {
            inner.current = None;
            inner.translation = None;
        }
    }

    /// Stop the job running, if any, without waiting. Called first, as the
    /// app exits, before every wait the quit sequence bounds by one shared
    /// deadline ([`Meetings::wait_until`]), so a job starts winding down
    /// before anything waits for it.
    ///
    /// A live meeting is stopped, not discarded: it finishes and keeps what
    /// it heard, for [`Meetings::save_on_quit`] to hold for the next launch.
    pub fn stop_now(&self) {
        let Ok(mut inner) = self.0.lock() else {
            return;
        };
        let Some(running) = inner.running.as_mut() else {
            return;
        };
        running.stop.store(true, Ordering::Relaxed);
        let Some(live) = running.live.as_mut() else {
            return;
        };
        live.stopping = true;
        live.ending.stop();
        let job = running.job;
        inner.quit_live = Some(job);
    }

    /// After quitting has waited for the jobs ([`Meetings::wait_until`]),
    /// write the live meeting that was recording, if there was one, into
    /// `dir` for the next launch to offer back ([`Meetings::offer_held`]).
    /// A meeting that finished in time is saved whole. One that did not is
    /// saved as far as it had committed, marked as missing its last part.
    /// Returns where it went.
    ///
    /// Only text, never audio. And it is not kept in the sense History
    /// keeps things: it is held until the person, at the next launch,
    /// copies it, saves it or clears it, and then it goes. A quit is not
    /// the person deciding to throw the meeting away.
    fn save_on_quit(&self, dir: &Path) -> Option<PathBuf> {
        let (turns, last_part_missing, ended_early) = {
            let mut inner = self.0.lock().ok()?;
            let job = inner.quit_live.take()?;
            match inner.running.as_ref().filter(|r| r.job == job) {
                Some(running) => {
                    let committed = running.live.as_ref()?.committed.as_ref()?;
                    (committed.meeting()?.turns, true, None)
                }
                None => {
                    let view = inner.current.as_ref().filter(|v| v.job == job)?;
                    (vtt_turns(view), false, view.ended_early.clone())
                }
            }
        };
        let held = Held {
            turns: &turns,
            last_part_missing,
            ended_early: ended_early.as_deref(),
        };
        match hold_for_next_launch(dir, &held, unix_ms()) {
            Ok(path) => {
                tracing::info!(
                    last_part_missing,
                    "held the live meeting for the next launch"
                );
                Some(path)
            }
            Err(error) => {
                tracing::warn!(%error, "could not hold the live meeting for the next launch");
                None
            }
        }
    }

    /// The Files page's part of quitting, after [`Meetings::stop_now`]:
    /// wait until `deadline` for a job holding a model
    /// ([`Meetings::wait_until`]), then hold a live meeting that was
    /// recording for the next launch, in `dir` when the app has one
    /// ([`Meetings::save_on_quit`]). Logs how long a live meeting's finish
    /// took, and whether it made the deadline: one that did not can still
    /// be freeing its model as the process exits. Returns whether the wait
    /// finished in time.
    pub fn finish_for_quit(&self, deadline: Instant, dir: Option<&Path>) -> bool {
        let started = Instant::now();
        let finished = self.wait_until(deadline);
        let finish_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let live = self.0.lock().is_ok_and(|inner| inner.quit_live.is_some());
        let held = dir.and_then(|dir| self.save_on_quit(dir));
        if live {
            tracing::info!(
                finish_ms,
                finished,
                held = held.is_some(),
                "quit: a live meeting was recording"
            );
        }
        finished
    }

    /// At launch, offer back the oldest live meeting a quit held
    /// ([`Meetings::save_on_quit`]) as the current meeting, with a sentence
    /// saying why it is there, and why it stopped early if it did. Any
    /// others wait, each for a later launch, one at a time. A write a quit
    /// left unfinished (`*.partial`) is removed. Nothing to offer, or a file
    /// that will not read, leaves the page as it was; an unreadable one is
    /// logged and left where it is.
    pub fn offer_held(&self, dir: &Path) {
        remove_unfinished_writes(dir);
        let Some(HeldFile {
            path,
            last_part_missing,
            ..
        }) = held_files(dir).into_iter().next()
        else {
            return;
        };
        let ended_early = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| ended_early_note(&text));
        let meeting = match meeting::open_file(&path) {
            Ok(MeetingFile::Transcript(meeting)) => meeting,
            Ok(MeetingFile::Recording) => return,
            Err(error) => {
                tracing::warn!(%error, "the live meeting held at the last quit could not be read");
                return;
            }
        };
        let Ok(mut inner) = self.0.lock() else {
            return;
        };
        if inner.current.is_some() {
            return;
        }
        inner.next_job += 1;
        let job = inner.next_job;
        let mut view = live_view(job, meeting);
        view.held = Some(crate::meeting_words::HELD_FROM_QUIT.to_string());
        view.ended_early = if last_part_missing {
            Some(crate::meeting_words::LAST_PART_MISSING.to_string())
        } else {
            ended_early
        };
        inner.current = Some(view);
        inner.held = Some((job, path));
    }

    /// The person copied, saved or cleared the current meeting: if it is the
    /// one held from the last quit, its file goes now.
    fn let_go_of_held(&self) {
        let Ok(mut inner) = self.0.lock() else {
            return;
        };
        let current = inner.current.as_ref().map(|v| v.job);
        if let Some((job, path)) = inner.held.take() {
            if Some(job) == current {
                if let Err(error) = std::fs::remove_file(&path) {
                    tracing::warn!(%error, "could not remove the held live meeting");
                }
            } else {
                inner.held = Some((job, path));
            }
        }
    }

    /// Wait until `deadline` for the job that was told to stop
    /// ([`Meetings::stop_now`]) to finish, if it holds a borrowed model.
    /// Called as the app exits: a borrowed model freed on another thread
    /// while the process exits runs into ggml's own teardown and aborts
    /// (`macalla-engines/src/transcribe.rs`, `Worker`). A summary holds no
    /// model and can be waiting on a request, so this never waits for one --
    /// `stop_now` already told it to give up. Returns whether the
    /// model-holding job, if there was one, finished in time.
    pub fn wait_until(&self, deadline: Instant) -> bool {
        let thread = match self.0.lock() {
            Ok(mut inner) => inner
                .running
                .as_mut()
                .and_then(|running| running.holds_model.then(|| running.thread.take()).flatten()),
            Err(_) => None,
        };
        let Some(thread) = thread else {
            return true;
        };
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = thread.join();
        true
    }
}

/// How many recent choices [`ChosenFiles`] remembers: a pick in the open
/// panel or a drop, each counted once however many files it brought, so a
/// batch of twenty never pushes its own first files out before they are
/// opened. Old enough that opening several files or drops in a row is never
/// refused, small enough that a page that only ever opened the panel and
/// never opened what it gave cannot grow the record without bound.
const MAX_CHOSEN: usize = 8;

/// The paths the open panel or a drop have given the page, and nothing else:
/// `meeting_open_file` and `meeting_batch_start` refuse any path not recorded
/// here. Canonicalised on the way in and on the way out, so a `..` segment, a
/// symlink or a case difference cannot be used to smuggle a different file
/// past a recorded one. Kept as choices, each the paths one pick or one drop
/// gave ([`MAX_CHOSEN`]). One-shot: [`ChosenFiles::take`] removes what it
/// finds, whether the open that follows succeeds or fails, so a stale record
/// from an old drop cannot be replayed.
#[derive(Clone, Default)]
pub struct ChosenFiles(Arc<Mutex<VecDeque<Vec<PathBuf>>>>);

impl ChosenFiles {
    /// Canonicalise `path` and remember it as a choice of its own. Fails,
    /// recording nothing, if `path` cannot be canonicalised (there is nothing
    /// there to open).
    #[cfg(test)]
    fn record(&self, path: &Path) -> std::io::Result<()> {
        let canonical = path.canonicalize()?;
        self.remember(vec![canonical]);
        Ok(())
    }

    /// Canonicalise each of `paths` and remember them as one choice, dropping
    /// the oldest choice once there are more than [`MAX_CHOSEN`]. A path that
    /// cannot be canonicalised is left out, and so is a folder: a folder
    /// dropped is not opened, and nothing in it is either. Returns the paths
    /// recorded, as given, in their order.
    fn record_all<'a>(&self, paths: &'a [PathBuf]) -> Vec<&'a Path> {
        let (kept, canonical): (Vec<&Path>, Vec<PathBuf>) = paths
            .iter()
            .filter(|path| !path.is_dir())
            .filter_map(|path| Some((path.as_path(), path.canonicalize().ok()?)))
            .unzip();
        if !canonical.is_empty() {
            self.remember(canonical);
        }
        kept
    }

    fn remember(&self, choice: Vec<PathBuf>) {
        if let Ok(mut choices) = self.0.lock() {
            choices.push_back(choice);
            while choices.len() > MAX_CHOSEN {
                choices.pop_front();
            }
        }
    }

    /// Canonicalise `path` and, if it matches a recorded one, remove and
    /// return it. `None` both when `path` cannot be canonicalised and when
    /// the canonical path was never recorded, so a caller cannot tell the two
    /// apart from the answer alone.
    fn take(&self, path: &Path) -> Option<PathBuf> {
        let canonical = path.canonicalize().ok()?;
        let mut choices = self.0.lock().ok()?;
        let (which, index) = choices.iter().enumerate().find_map(|(which, choice)| {
            choice
                .iter()
                .position(|p| *p == canonical)
                .map(|index| (which, index))
        })?;
        let taken = choices[which].remove(index);
        if choices[which].is_empty() {
            choices.remove(which);
        }
        Some(taken)
    }

    /// Forget every recorded path, from `meeting_clear` and from quitting.
    pub(crate) fn clear(&self) {
        if let Ok(mut choices) = self.0.lock() {
            choices.clear();
        }
    }

    /// Record every path of a drop on the main window, as one choice, and
    /// return the ones recorded, as given, for the page. Called from
    /// `lib.rs`'s `on_window_event`, with the window's own native drop event,
    /// which sends what this returns to the page as `files-dropped`. The page
    /// opens exactly those: one opens as the current meeting, several start a
    /// batch (`App.tsx`'s `dropTargets`). A folder, or a path that cannot be
    /// canonicalised, is left out ([`ChosenFiles::record_all`]), so it is
    /// neither opened nor counted.
    pub(crate) fn record_dropped(&self, paths: &[PathBuf]) -> Vec<String> {
        self.record_all(paths)
            .into_iter()
            .map(|path| path.display().to_string())
            .collect()
    }
}

/// One turn as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnView {
    /// The speaker as data: a file's own name for them, or a live meeting's
    /// "You" and "Others", in English. What Copy and Save as text give.
    pub speaker: Option<String>,
    /// The speaker as the page shows them: a live meeting's through the
    /// catalogue (`meeting_words::speaker`), a file's as they are.
    pub label: Option<String>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// The turn's Edit button as a screen reader names it, from the speaker
    /// as data and when the turn starts (`meeting_words::edit_turn`).
    pub edit_label: String,
}

/// What tidying did, for the page to say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TidiedView {
    /// Where the text went, in cleanup's own words.
    pub destination: String,
    pub parts: usize,
    /// Parts whose words came back as they were heard.
    pub unchanged: usize,
}

/// The current meeting, on the `meeting-transcript` and `meeting-summary`
/// events and from `meeting_current`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MeetingView {
    pub job: u64,
    /// Raised, under the state's lock, each time this meeting changes after
    /// it is made: a correction, or a summary kept onto it. Replies and
    /// events can reach the page in another order than the changes were
    /// made, so the page keeps, of two views of one job, the one with the
    /// higher revision.
    pub revision: u64,
    /// The file's name, without its folder.
    pub name: String,
    pub turns: Vec<TurnView>,
    pub named_speakers: bool,
    /// `None` for a transcript file, which has no audio.
    pub audio_ms: Option<u64>,
    pub tidied: Option<TidiedView>,
    pub summary: Option<Vec<PartView>>,
    /// Where the summary's transcript went, in cleanup's own words.
    pub summary_by: Option<String>,
    /// The transcript was corrected by hand after `summary` was written, so
    /// a quote may no longer be in the turn it points to. The page says so
    /// above the summary (`meeting_words::SUMMARY_OUT_OF_DATE`), and
    /// anything that writes the summary out must too, or leave it out.
    /// Cleared by the next summary of the corrected turns.
    pub summary_out_of_date: bool,
    /// Why a live meeting stopped early and kept what it heard, a sentence;
    /// `None` for a file, and for a meeting the person stopped.
    pub ended_early: Option<String>,
    /// Why this meeting is here at launch: the app quit while it recorded,
    /// and it was held for the person to keep or let go. A sentence.
    pub held: Option<String>,
}

/// On `meeting-progress`: `hearing` counts milliseconds of audio, `tidying`
/// counts parts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProgressEvent {
    pub job: u64,
    pub phase: &'static str,
    pub done: u64,
    pub of: u64,
}

/// On `summary-progress`: part `part` of `of` has `started` or `finished`.
/// `job` is the meeting's, as on `meeting-summary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SummaryProgressEvent {
    pub job: u64,
    pub stage: &'static str,
    pub part: u64,
    pub of: u64,
}

fn summary_progress(
    job: u64,
    progress: macalla_core::meeting_summary::PartProgress,
) -> SummaryProgressEvent {
    use macalla_core::meeting_summary::PartProgress;
    let (stage, part, of) = match progress {
        PartProgress::Started { part, of } => ("started", part, of),
        PartProgress::Finished { part, of } => ("finished", part, of),
    };
    SummaryProgressEvent {
        job,
        stage,
        part: part as u64,
        of: of as u64,
    }
}

/// A summary still running, from `meeting_running_summary`, for a page that
/// opens while it works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunningSummaryView {
    /// The summary's own job, as `meeting_summarise` returned it.
    pub job: u64,
    /// Since the summary started.
    pub elapsed_ms: u64,
    /// Whether Stop was pressed.
    pub stopping: bool,
    /// Where it sends the transcript, in cleanup's own words, once its
    /// summariser is built: what is running, whatever the settings say now.
    pub destination: Option<String>,
    /// How long a part can take, when that was measured for its provider.
    pub pace: Option<String>,
    /// The last `summary-progress` sent, once a part has started.
    pub progress: Option<SummaryProgressEvent>,
}

/// Said when a summary is stopped before its last reply came back.
const SUMMARY_STOPPED: &str = "The summary was stopped.";

/// On `meeting-error`: the job ended without a result, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorEvent {
    pub job: u64,
    pub message: String,
}

/// A summary item with its quote, the owner when one was kept, and where the
/// quote was said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemView {
    pub text: String,
    pub quote: String,
    pub owner: Option<String>,
    /// The turn the quote was said in, by its index in the meeting's turns,
    /// for the page to go to.
    pub turn: usize,
    /// Who said it and when, as the page shows it beside the quote.
    pub said: String,
}

/// One part of a summary: its lists, or why it has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartView {
    pub start_ms: u64,
    pub end_ms: u64,
    pub key_points: Vec<ItemView>,
    pub decisions: Vec<ItemView>,
    pub action_items: Vec<ItemView>,
    /// Reply lines the quote check dropped.
    pub dropped: usize,
    pub error: Option<String>,
}

/// Where the page's buttons would send the text, said before anything is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DestinationsView {
    /// The engine a recording is heard by, by the name the Engine card uses,
    /// or `None` with no session.
    pub engine: Option<String>,
    /// Where tidying sends the text, or `None` with cleanup off.
    pub tidy: Option<String>,
    /// The cleanup choice tidying uses, for the page to pass back when it
    /// opens a file to tidy.
    pub tidy_choice: Option<String>,
    /// Why tidying is not offered with the cleanup provider, a sentence for the
    /// page to show in place of the Tidy tick; `None` when it is offered.
    pub tidy_unavailable: Option<String>,
    /// Where a summary sends the transcript, or `None` when none can be built.
    pub summary: Option<String>,
    /// The cleanup choice a summary uses, for the page to pass back to
    /// Summarise.
    pub summary_choice: Option<String>,
    /// How long a part of a summary can take, a sentence for the page to show
    /// while it waits, or `None` when that was not measured for the provider.
    pub summary_pace: Option<String>,
}

fn view_of(job: u64, name: String, meeting: MeetingTranscript) -> MeetingView {
    MeetingView {
        job,
        revision: 0,
        name,
        turns: meeting
            .turns
            .into_iter()
            .map(|turn| TurnView {
                edit_label: crate::meeting_words::edit_turn(
                    turn.speaker.as_deref(),
                    &clock(turn.start_ms),
                ),
                label: turn.speaker.clone(),
                speaker: turn.speaker,
                start_ms: turn.start_ms,
                end_ms: turn.end_ms,
                text: turn.text,
            })
            .collect(),
        named_speakers: meeting.named_speakers,
        audio_ms: meeting.audio_ms,
        tidied: None,
        summary: None,
        summary_by: None,
        summary_out_of_date: false,
        ended_early: None,
        held: None,
    }
}

/// A held meeting's file: `live-meeting-held-<unix-ms>.vtt`, with
/// `-last-part-missing` before the extension when the quit's time ran out
/// on its last piece. The time orders them, oldest first, and keeps each
/// quit's meeting apart: a newer one never replaces an older one.
const HELD_PREFIX: &str = "live-meeting-held-";
const LAST_PART_MISSING_SUFFIX: &str = "-last-part-missing";
const HELD_EXTENSION: &str = ".vtt";
const UNFINISHED_EXTENSION: &str = ".partial";

/// The first line of the `NOTE` block that carries why a held meeting
/// stopped early, in the words the page showed it in. A reader of the file
/// skips `NOTE` blocks, so its turns are untouched.
const ENDED_EARLY_NOTE: &str = "NOTE ended early";

/// What a quit holds: the turns, and what the page said about how the
/// meeting ended.
pub(crate) struct Held<'a> {
    pub turns: &'a [macalla_core::vtt::Turn],
    pub last_part_missing: bool,
    /// The sentence the page showed for why it stopped early, if it did.
    pub ended_early: Option<&'a str>,
}

/// A held meeting found in the data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeldFile {
    at_ms: u64,
    path: PathBuf,
    last_part_missing: bool,
}

/// Milliseconds since the Unix epoch, for a held meeting's name.
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Write `held` as WebVTT into `dir`, under a name of its own for `at_ms`
/// (the next free millisecond if that one is taken), and say where.
/// Written to a temporary name, flushed to disk, then renamed, so a quit
/// cut short leaves either the whole file or an unfinished one that the
/// next launch removes, never half of one under the real name.
pub(crate) fn hold_for_next_launch(
    dir: &Path,
    held: &Held<'_>,
    at_ms: u64,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let suffix = if held.last_part_missing {
        LAST_PART_MISSING_SUFFIX
    } else {
        ""
    };
    let taken = |ms: u64| held_files(dir).iter().any(|file| file.at_ms == ms);
    let at_ms = (at_ms..).find(|&ms| !taken(ms)).unwrap_or(at_ms);
    let path = dir.join(format!("{HELD_PREFIX}{at_ms}{suffix}{HELD_EXTENSION}"));
    write_held(&path, held)?;
    Ok(path)
}

/// Write `held` as WebVTT to `path`, through a temporary name beside it,
/// flushed and then renamed, as [`hold_for_next_launch`] says: a new held
/// file, or one written again with a correction. A write, flush or rename
/// that fails removes the temporary file, so a failure leaves nothing
/// behind for the next launch to clear.
fn write_held(path: &Path, held: &Held<'_>) -> std::io::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("a held meeting's path has no file name"))?;
    let mut unfinished_name = name.to_os_string();
    unfinished_name.push(UNFINISHED_EXTENSION);
    let unfinished = path.with_file_name(unfinished_name);
    let mut text = macalla_core::vtt::write(held.turns);
    if let Some(sentence) = held.ended_early {
        let note = format!("\n{ENDED_EARLY_NOTE}\n{}\n", one_line(sentence));
        text.insert_str("WEBVTT\n".len(), &note);
    }
    let written = std::fs::File::create(&unfinished).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&unfinished, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&unfinished);
    }
    written
}

/// Write the held meeting at `path` again as `view` now has it, after a
/// correction. What the file said about how the meeting ended stays: a last
/// part missing is in its name, and is then the sentence the page shows,
/// never a note; any other reason is the note the page's sentence came from.
fn rewrite_held(path: &Path, view: &MeetingView) -> std::io::Result<()> {
    let last_part_missing = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(HELD_EXTENSION))
        .is_some_and(|stem| stem.ends_with(LAST_PART_MISSING_SUFFIX));
    let turns = vtt_turns(view);
    let held = Held {
        turns: &turns,
        last_part_missing,
        ended_early: if last_part_missing {
            None
        } else {
            view.ended_early.as_deref()
        },
    };
    write_held(path, &held)
}

/// `text` on one line, and never able to end a `NOTE` block or read as a
/// timing line.
fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("-->", "->")
}

/// Why a held meeting stopped early, from its `NOTE` block, if it has one.
fn ended_early_note(text: &str) -> Option<String> {
    let mut lines = text.lines();
    lines.find(|line| *line == ENDED_EARLY_NOTE)?;
    lines
        .next()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
}

/// Every held meeting in `dir`, oldest first.
fn held_files(dir: &Path) -> Vec<HeldFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<HeldFile> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            let stem = name
                .strip_prefix(HELD_PREFIX)?
                .strip_suffix(HELD_EXTENSION)?;
            let (at, last_part_missing) = match stem.strip_suffix(LAST_PART_MISSING_SUFFIX) {
                Some(at) => (at, true),
                None => (stem, false),
            };
            Some(HeldFile {
                at_ms: at.parse().ok()?,
                path,
                last_part_missing,
            })
        })
        .collect();
    found.sort_by(|a, b| (a.at_ms, &a.path).cmp(&(b.at_ms, &b.path)));
    found
}

/// Remove held files a quit began and never finished writing.
fn remove_unfinished_writes(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
        let unfinished = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(HELD_PREFIX) && name.ends_with(UNFINISHED_EXTENSION)
            });
        if unfinished {
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::warn!(%error, "could not remove an unfinished held meeting");
            }
        }
    }
}

/// A view's turns as the VTT file carries them: each speaker as data, the
/// English "You" and "Others", never as the page shows them.
fn vtt_turns(view: &MeetingView) -> Vec<macalla_core::vtt::Turn> {
    view.turns
        .iter()
        .map(|turn| macalla_core::vtt::Turn {
            speaker: turn.speaker.clone(),
            start_ms: turn.start_ms,
            end_ms: turn.end_ms,
            text: turn.text.clone(),
        })
        .collect()
}

/// A live meeting as the page shows it: named as one, with its speakers
/// through the catalogue.
fn live_view(job: u64, meeting: MeetingTranscript) -> MeetingView {
    let mut view = view_of(job, crate::meeting_words::NAME.to_string(), meeting);
    for turn in &mut view.turns {
        turn.label = turn.speaker.as_deref().map(crate::meeting_words::speaker);
    }
    view
}

/// A session error as the page says it: a sentence. `NothingHeard` and
/// `Stopped` keep their own wording, said in terms of a file rather than
/// `crate::say::report`'s more general ones. A decode failure
/// (`macalla_session::decode_file`) wraps `macalla_platform::decode`'s own
/// `Error::Audio`, whose sentence ("The microphone could not be used.") is
/// written for capture, not for a file with nothing microphone-shaped about
/// it, so that gets its own words here too, logged the same way. Every
/// other variant goes through `report`, logged there since nothing has
/// logged it yet.
fn say(error: macalla_session::Error) -> String {
    use macalla_platform::Error::Audio;
    use macalla_session::Error::{NothingHeard, Platform, Stopped};
    match &error {
        NothingHeard => "Nothing was heard in this recording.".to_string(),
        Stopped => "Stopped. Nothing from this file was kept.".to_string(),
        Platform(Audio(_)) => {
            crate::say::report(&error);
            "This recording could not be read as audio.".to_string()
        }
        _ => crate::say::report(&error),
    }
}

/// Said when the cleanup choice is no longer the one the page showed.
const CHOICE_CHANGED: &str = "Your Enhancement choice changed after this page showed where the text would go, so nothing was sent. Check the page, then try again.";

/// Refuse to send anything unless the cleanup choice is still the one the page
/// showed. Compared by fingerprint, not by label: two servers can both read
/// "Your server", a pool's label is cut short, and a changed model keeps it.
pub(crate) fn same_choice(shown: &str, now: &str) -> Result<(), String> {
    if shown == now {
        Ok(())
    } else {
        Err(CHOICE_CHANGED.to_string())
    }
}

/// A file to a meeting, with no Tauri in it: open the file, hear a recording
/// on the lent tools, and tidy it when `tidy_with` is the cleanup choice the
/// page showed ([`MeetingTools::tidy_choice`]). `lend` is asked only when an
/// engine or the cleanup provider is needed, and again for tidying after
/// hearing, so a choice changed while a long file was heard is not missed.
///
/// `hold` marks the job as holding a borrowed model, or no longer, and says
/// `false` once the job has been told to stop. Only hearing holds one: a
/// recording is decoded before the engine is borrowed, and tidying lets the
/// engine go before its first request, so quitting never waits on a decode or
/// on a request.
pub(crate) fn run_job(
    job: u64,
    path: &Path,
    tidy_with: Option<&str>,
    lend: &dyn Fn() -> Option<MeetingTools>,
    hold: &dyn Fn(bool) -> bool,
    on_progress: &mut dyn FnMut(ProgressEvent),
    stop: &AtomicBool,
) -> Result<MeetingView, String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let meeting = match meeting::open_file(path).map_err(say)? {
        MeetingFile::Transcript(meeting) => meeting,
        MeetingFile::Recording => {
            // Said before a long decode, not after it.
            if lend().is_none() {
                return Err(NO_ENGINE.to_string());
            }
            let samples = meeting::decode_recording(path).map_err(say)?;
            if stop.load(Ordering::Relaxed) || !hold(true) {
                return Err(say(macalla_session::Error::Stopped));
            }
            let heard = match lend() {
                Some(lent) => meeting::transcribe_samples(
                    &lent,
                    samples,
                    &mut |p| {
                        on_progress(ProgressEvent {
                            job,
                            phase: "hearing",
                            done: p.done_ms,
                            of: p.of_ms,
                        })
                    },
                    stop,
                )
                .map_err(say),
                None => Err(NO_ENGINE.to_string()),
            };
            hold(false);
            heard?
        }
    };
    let Some(shown) = tidy_with else {
        return Ok(view_of(job, name, meeting));
    };
    let lent = lend().ok_or_else(|| NO_SESSION.to_string())?;
    if let Some(reason) = lent.tidy_unavailable() {
        return Err(reason.to_string());
    }
    let tools = lent.into_tidy().ok_or_else(|| CLEANUP_OFF.to_string())?;
    same_choice(shown, tools.fingerprint())?;
    let destination = tools.destination().to_string();
    let tidied = meeting::tidy(
        &tools,
        &meeting,
        &mut |p| {
            on_progress(ProgressEvent {
                job,
                phase: "tidying",
                done: p.done as u64,
                of: p.of as u64,
            })
        },
        stop,
    )
    .map_err(say)?;
    let mut view = view_of(job, name, tidied.meeting);
    view.tidied = Some(TidiedView {
        destination,
        parts: tidied.parts,
        unchanged: tidied.unchanged,
    });
    Ok(view)
}

/// Where a quote was said, as the page shows it: the turn's speaker as the
/// page shows them, from `turns`, and when the turn starts; the time alone for
/// a turn with no speaker.
fn said_view(said: &macalla_core::meeting_summary::Said, turns: &[TurnView]) -> String {
    let time = clock(said.start_ms);
    match turns.get(said.turn).and_then(|turn| turn.label.as_deref()) {
        Some(speaker) => crate::meeting_words::quote_said_by(speaker, &time),
        None => crate::meeting_words::quote_said_at(&time),
    }
}

/// A summary's parts as the page shows them. `turns` are the meeting's, the
/// ones that were summarised, which each item's turn indexes.
fn part_views(
    parts: Vec<macalla_core::meeting_summary::PartSummary>,
    turns: &[TurnView],
) -> Vec<PartView> {
    let points = |points: Vec<macalla_core::meeting_summary::Point>| {
        points
            .into_iter()
            .map(|point| ItemView {
                said: said_view(&point.said, turns),
                turn: point.said.turn,
                text: point.text,
                quote: point.quote,
                owner: None,
            })
            .collect()
    };
    parts
        .into_iter()
        .map(|part| match part.lists {
            Ok(lists) => PartView {
                start_ms: part.start_ms,
                end_ms: part.end_ms,
                key_points: points(lists.key_points),
                decisions: points(lists.decisions),
                action_items: lists
                    .action_items
                    .into_iter()
                    .map(|item| ItemView {
                        said: said_view(&item.said, turns),
                        turn: item.said.turn,
                        text: item.text,
                        quote: item.quote,
                        owner: item.owner,
                    })
                    .collect(),
                dropped: lists.dropped.len(),
                error: None,
            },
            Err(error) => PartView {
                start_ms: part.start_ms,
                end_ms: part.end_ms,
                key_points: Vec::new(),
                decisions: Vec::new(),
                action_items: Vec::new(),
                dropped: 0,
                error: Some(crate::say::report_core(&error)),
            },
        })
        .collect()
}

/// Said when `meeting_open_file` is asked for a path the panel or a drop did
/// not give it. Never repeats the path: see the module doc on
/// [`ChosenFiles`].
const NOT_CHOSEN: &str = "Choose the file again, or drop it on this page.";

/// Open the file at `path` on a thread of its own and hear it, or read it,
/// and tidy it when `tidy_with` is the cleanup choice the page showed.
/// Returns the job's number at once; the result arrives as
/// `meeting-transcript`, a failure or a stop as `meeting-error`, and progress
/// as `meeting-progress`.
///
/// Refused, before anything else, unless `path` is one the open panel
/// ([`meeting_choose_file`]) or a drop on the main window recorded: the page
/// only ever shows what it was given by one of those, so this is the one
/// place a path from the page is trusted. `ChosenFiles::take` is one-shot, so
/// the same path must be chosen or dropped again to be opened twice.
///
/// Not `async`, on purpose: a plain command runs on the main thread, and so
/// does `RunEvent::Exit`, so quitting cannot land between `begin` and
/// `attach` and miss a thread that holds a model.
#[tauri::command]
pub fn meeting_open_file(
    app: AppHandle<Wry>,
    meetings: State<'_, Meetings>,
    dictator: State<'_, Dictator>,
    chosen: State<'_, ChosenFiles>,
    path: String,
    tidy_with: Option<String>,
) -> Result<u64, CommandError> {
    let Some(path) = chosen.take(Path::new(&path)) else {
        tracing::debug!(%path, "refused a path not chosen through the panel or a drop");
        return Err(CommandError::from(NOT_CHOSEN));
    };
    let (job, stop) = meetings.begin()?;
    let meetings_for_thread = meetings.inner().clone();
    let dictator = dictator.inner().clone();
    let spawned = std::thread::Builder::new()
        .name("macalla-meeting".into())
        .spawn(move || {
            let lend = || dictator.meeting_tools_loaded();
            let hold = |holds: bool| meetings_for_thread.holding_model(job, holds);
            let mut progress = |event: ProgressEvent| {
                let _ = app.emit("meeting-progress", event);
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_job(
                    job,
                    &path,
                    tidy_with.as_deref(),
                    &lend,
                    &hold,
                    &mut progress,
                    &stop,
                )
            }))
            .unwrap_or_else(|_| Err(WENT_WRONG.to_string()));
            meetings_for_thread.finish(job, outcome, |ending| emit_ending(&app, ending));
        });
    match spawned {
        Ok(thread) => {
            meetings.attach(job, thread);
            Ok(job)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start a thread for a file job");
            meetings.end(job, None);
            Err("Could not start working on this file. Try again.".into())
        }
    }
}

/// The system's open panel, for one file or several. Empty if the person
/// cancelled; otherwise the chosen paths, recorded in `chosen` as one choice
/// so `meeting_open_file` or `meeting_batch_start` will accept them, and
/// returned to the page to show and to pass back: one opens as the current
/// meeting, several start a batch. No filter: anything but a `.vtt`, `.txt`
/// or `.srt` is tried as a recording, and a filter would hide one that works.
///
/// `async`, so the panel (`pick_files`'s callback runs on the platform's own
/// event loop) never blocks the main thread, which a plain command runs on
/// and which macOS can deadlock if a modal panel blocks it. The callback
/// replies over a channel this command awaits.
#[tauri::command(async)]
pub async fn meeting_choose_file(
    app: AppHandle<Wry>,
    chosen: State<'_, ChosenFiles>,
) -> Result<Vec<String>, CommandError> {
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    app.dialog()
        .file()
        .set_title(crate::meeting_words::CHOOSE_TITLE)
        .pick_files(move |picked| {
            let _ = tx.blocking_send(picked);
        });
    // `recv` gives `None` only if the sender was dropped without sending --
    // the panel's callback never ran -- which is not the same as the person
    // cancelling (`Some(None)`), and worth telling apart in the log.
    let picked = match rx.recv().await {
        Some(picked) => picked,
        None => {
            tracing::debug!("the file panel closed without an answer");
            None
        }
    };
    let Some(picked) = picked else {
        return Ok(Vec::new());
    };
    let paths: Vec<PathBuf> = picked
        .into_iter()
        .filter_map(|file| file.into_path().ok())
        .collect();
    let recorded = chosen.record_all(&paths);
    if recorded.is_empty() {
        return Err("Could not use the chosen file.".into());
    }
    Ok(recorded
        .into_iter()
        .map(|path| path.display().to_string())
        .collect())
}

/// Summarise the current meeting with the cleanup provider, on a thread of its
/// own, sending it only while the cleanup choice is still `choice`, the one
/// the page showed.
/// The result arrives as `meeting-summary` with the meeting, kept in memory
/// beside it; a failure as `meeting-error`, and progress, each part as it
/// starts and finishes, as `summary-progress`. A stop that lands while a part
/// is out ends it as stopped, even if that reply still arrives; only a stop
/// that lands after the last reply came back keeps the summary.
#[tauri::command]
pub fn meeting_summarise(
    app: AppHandle<Wry>,
    meetings: State<'_, Meetings>,
    history: State<'_, HistoryHandle>,
    choice: String,
) -> Result<u64, CommandError> {
    let choices = crate::dictation::cleanup_config(&history);
    let (job, stop, current) = meetings.begin_summary()?;
    let meetings_for_thread = meetings.inner().clone();
    let spawned = std::thread::Builder::new()
        .name("macalla-summary".into())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let summariser = Summariser::build(&choices, &SystemSecrets::new()).map_err(say)?;
                same_choice(&choice, summariser.fingerprint())?;
                meetings_for_thread.summary_built(job, summariser.destination(), summariser.pace());
                let turns: Vec<macalla_core::vtt::Turn> = current
                    .turns
                    .iter()
                    .map(|turn| macalla_core::vtt::Turn {
                        speaker: turn.speaker.clone(),
                        start_ms: turn.start_ms,
                        end_ms: turn.end_ms,
                        text: turn.text.clone(),
                    })
                    .collect();
                let mut progress = |p: macalla_core::meeting_summary::PartProgress| {
                    let event = summary_progress(current.job, p);
                    meetings_for_thread.summary_progressed(job, event.clone());
                    let _ = app.emit("summary-progress", event);
                };
                // A stop after the last reply came back keeps the summary: the
                // session says Stopped for a stop seen while any part was out.
                let parts = summariser.summarise(&turns, &mut progress, &stop).map_err(
                    |error| match error {
                        macalla_session::Error::Stopped => SUMMARY_STOPPED.to_string(),
                        other => say(other),
                    },
                )?;
                let mut view = current.clone();
                view.summary = Some(part_views(parts, &view.turns));
                view.summary_by = Some(summariser.destination().to_string());
                Ok(view)
            }))
            .unwrap_or_else(|_| Err(WENT_WRONG.to_string()));
            meetings_for_thread.finish_summary(job, outcome, |ending| emit_ending(&app, ending));
        });
    match spawned {
        Ok(thread) => {
            meetings.attach(job, thread);
            Ok(job)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start a thread for a summary job");
            meetings.end_summary(job, None);
            Err("Could not start the summary. Try again.".into())
        }
    }
}

/// Said when Copy, or any of the Save commands, is asked for with no
/// meeting kept.
const NOTHING_KEPT: &str = "There is no file on the Files page.";

/// The meeting as plain text: one turn per paragraph, with its speaker's name
/// in front when it has one. What Copy and Save as text give.
fn transcript_text(view: &MeetingView) -> String {
    view.turns
        .iter()
        .map(|turn| match &turn.speaker {
            Some(name) => format!("{name}: {}", turn.text),
            None => turn.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The meeting as WebVTT: each turn with its times and its speaker as data.
fn vtt_text(view: &MeetingView) -> String {
    macalla_core::vtt::write(&vtt_turns(view))
}

/// A format a transcript can be saved in: the file's extension, the choice
/// as the page names it, and how the meeting is written in it.
pub(crate) struct Format {
    pub extension: &'static str,
    pub label: &'static str,
    pub render: fn(&MeetingView) -> String,
}

/// The meeting as SRT: each turn with its times, from the same turns as
/// WebVTT.
fn srt_text(view: &MeetingView) -> String {
    macalla_core::srt::write(&vtt_turns(view))
}

/// Every format a transcript can be saved in, in the order the Files card
/// offers them, the first its default. The card's one choice drives both
/// the current meeting's Save button (`meeting_save`) and a batch
/// (`meeting_batch_start`), so both look a format up here and nowhere else.
pub(crate) const FORMATS: &[Format] = &[
    Format {
        extension: "txt",
        label: crate::meeting_words::FORMAT_TEXT,
        render: transcript_text,
    },
    Format {
        extension: "vtt",
        label: crate::meeting_words::FORMAT_VTT,
        render: vtt_text,
    },
    Format {
        extension: "srt",
        label: crate::meeting_words::FORMAT_SRT,
        render: srt_text,
    },
    Format {
        extension: "md",
        label: crate::meeting_words::FORMAT_MARKDOWN,
        render: meeting_markdown,
    },
];

/// A format by its extension, as the page passes it back.
fn format_by_extension(extension: &str) -> Option<&'static Format> {
    FORMATS.iter().find(|f| f.extension == extension)
}

/// Write `text` to a new `.{extension}` file in `dir`, named for the meeting
/// and the date, with a counter when the day already has one
/// ([`macalla_session::output::write_new`]): nothing is overwritten, and a
/// write that fails part of the way leaves nothing behind. A name starting
/// with a dot is not a hidden file here.
fn save_new(
    dir: &Path,
    name: &str,
    date: &str,
    extension: &str,
    text: &str,
) -> std::io::Result<PathBuf> {
    // "{name} transcript {date}", or just "transcript {date}" when the file
    // gives no usable name, so it never reads "transcript transcript".
    let base = Path::new(name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().trim_start_matches('.').to_string())
        .filter(|stem| !stem.trim().is_empty())
        .map_or_else(
            || format!("transcript {date}"),
            |stem| format!("{stem} transcript {date}"),
        );
    macalla_session::output::write_new(dir, &base, extension, text.as_bytes())
}

/// Correct turn `index` of meeting `job` by hand, and return the meeting as
/// kept, for the page to show. The turn keeps its time and speaker; Copy and
/// Save read the corrected words. A summary already written is kept,
/// marked out of date. Refused with a sentence, and nothing changed, for
/// words that are all whitespace, a meeting no longer on the page, a turn it
/// does not have, and a held meeting whose file cannot be written again. See
/// [`Meetings::edit_turn`]. Not the person deciding to keep a held meeting:
/// its file stays until they copy, save or clear it.
#[tauri::command]
pub fn meeting_edit_turn(
    meetings: State<'_, Meetings>,
    job: u64,
    index: usize,
    text: String,
) -> Result<MeetingView, CommandError> {
    Ok(meetings.edit_turn(job, index, &text)?)
}

/// Whether the hand correction just saved for one turn looks like a single
/// mishearing worth offering to the dictionary, and if so, what to offer:
/// the core's own rule plus the one check that needs the dictionary itself
/// (`crate::dictionary::correction_offer`). `old` is the turn's text before
/// the correction, as the page held it; `new` is what `meeting_edit_turn`
/// returned for it. Nothing here reads or writes the meeting; only the
/// dictionary file.
#[tauri::command]
pub fn meeting_correction_offer(
    app: AppHandle<Wry>,
    old: String,
    new: String,
) -> Result<Option<macalla_core::dictionary::Offer>, CommandError> {
    crate::dictionary::correction_offer(&crate::commands::dictionary_path(&app)?, &old, &new)
}

/// Put the kept meeting's text on the clipboard.
#[tauri::command]
pub fn meeting_copy(meetings: State<'_, Meetings>) -> Result<(), CommandError> {
    use macalla_platform::paste::{Clipboard, SystemClipboard};
    let view = meetings
        .current()
        .ok_or_else(|| CommandError::from(NOTHING_KEPT))?;
    SystemClipboard::new()
        .and_then(|mut clipboard| clipboard.write_text(&transcript_text(&view)))?;
    meetings.let_go_of_held();
    Ok(())
}

/// Save the kept meeting: no meeting kept refuses with [`NOTHING_KEPT`];
/// otherwise `format` renders it and [`save_new`] writes it to a new file
/// in Downloads named for the meeting, today's date and the format's
/// extension, and the meeting stops being held for a quit that offered it
/// back. Says where the file went.
fn save_kept(
    app: &AppHandle<Wry>,
    meetings: &State<'_, Meetings>,
    format: &Format,
) -> Result<String, CommandError> {
    use tauri::Manager;
    let view = meetings
        .current()
        .ok_or_else(|| CommandError::from(NOTHING_KEPT))?;
    let dir = app
        .path()
        .download_dir()
        .or_say(crate::meeting_words::DOWNLOADS_NOT_FOUND)?;
    let saved = save_new(
        &dir,
        &view.name,
        &crate::dictionary::today(),
        format.extension,
        &(format.render)(&view),
    )
    .or_say(crate::meeting_words::COULD_NOT_SAVE_FILE)?;
    meetings.let_go_of_held();
    Ok(saved.display().to_string())
}

/// Where one file of a batch has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowState {
    Waiting,
    /// Opened, or decoded, and not yet heard.
    Opening,
    /// Heard this far, in milliseconds of audio.
    Hearing {
        done: u64,
        of: u64,
    },
    Done,
    Failed,
    /// Stopped while heard, or still waiting when the batch was stopped.
    Stopped,
}

/// One file of a batch: its name, where it has got, where its transcript
/// went, or why it has none. Never the transcript itself.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    state: RowState,
    saved: Option<PathBuf>,
    /// Why it failed or was stopped, a sentence.
    why: Option<String>,
}

/// A batch: its job and its files, in the order given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Batch {
    job: u64,
    rows: Vec<Row>,
    running: bool,
    stopping: bool,
}

impl Batch {
    fn new(job: u64, names: Vec<String>) -> Self {
        Self {
            job,
            rows: names
                .into_iter()
                .map(|name| Row {
                    name,
                    state: RowState::Waiting,
                    saved: None,
                    why: None,
                })
                .collect(),
            running: true,
            stopping: false,
        }
    }

    fn set(&mut self, index: usize, state: RowState, why: Option<String>) {
        if let Some(row) = self.rows.get_mut(index) {
            row.state = state;
            row.why = why;
        }
    }

    fn view(&self) -> BatchView {
        use crate::meeting_words as words;
        let count = self.rows.len();
        let finished = self
            .rows
            .iter()
            .filter(|r| {
                matches!(
                    r.state,
                    RowState::Done | RowState::Failed | RowState::Stopped
                )
            })
            .count();
        let saved = self
            .rows
            .iter()
            .filter(|r| r.state == RowState::Done)
            .count();
        let summary = if !self.running {
            words::batch_ended(saved, count)
        } else if self.stopping {
            words::BATCH_STOPPING.to_string()
        } else {
            words::batch_working(finished, count)
        };
        BatchView {
            job: self.job,
            running: self.running,
            stopping: self.stopping,
            summary,
            rows: self.rows.iter().map(Row::view).collect(),
        }
    }
}

impl Row {
    fn view(&self) -> BatchRowView {
        use crate::meeting_words as words;
        let (state, status, done, of) = match self.state {
            RowState::Waiting => ("waiting", words::BATCH_WAITING.to_string(), 0, 0),
            RowState::Opening => ("hearing", words::BATCH_OPENING.to_string(), 0, 0),
            RowState::Hearing { done, of } => (
                "hearing",
                words::batch_hearing(&clock(done), &clock(of)),
                done,
                of,
            ),
            RowState::Done => (
                "done",
                words::batch_saved(
                    &self
                        .saved
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                ),
                0,
                0,
            ),
            RowState::Failed => ("failed", self.why.clone().unwrap_or_default(), 0, 0),
            RowState::Stopped => ("stopped", self.why.clone().unwrap_or_default(), 0, 0),
        };
        BatchRowView {
            name: self.name.clone(),
            state,
            status,
            done,
            of,
            saved: self.saved.as_ref().map(|p| p.display().to_string()),
        }
    }
}

/// A batch as the page shows it: from `meeting_batch_start`, `meeting_batch`
/// and on `batch-changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchView {
    pub job: u64,
    pub running: bool,
    /// Whether Stop was pressed.
    pub stopping: bool,
    /// How far the batch has got, or how it ended, a sentence.
    pub summary: String,
    pub rows: Vec<BatchRowView>,
}

/// One file of a batch as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchRowView {
    /// The file's name, without its folder.
    pub name: String,
    /// `waiting`, `hearing`, `done`, `failed` or `stopped`: data, for the
    /// page to style the row by.
    pub state: &'static str,
    /// Where it has got, where it was saved, or why not, in words.
    pub status: String,
    /// While heard: milliseconds of audio heard, of how many.
    pub done: u64,
    pub of: u64,
    /// Where its transcript was saved, once it was.
    pub saved: Option<String>,
}

/// A file's name, without its folder, as a row shows it.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A batch, with no Tauri in it: each of `paths` in turn, heard or read by
/// [`run_job`] on what `lend` lends -- the engine dictation already loaded --
/// never tidied, written by `format` and
/// `save`, and dropped before the next file is opened, so a batch holds one
/// transcript at a time. A path of `None` was never chosen and is refused in
/// its row. One file failing, even panicking, fails its own row and the next
/// carries on. Once `stop` is set the file being heard stops as a single
/// file's does, and every file still waiting is marked as not heard; what
/// was saved stays saved. `on_change` sees the batch after every change,
/// with the index of the row that changed.
///
/// One engine for the whole batch: the instance `lend` lends as the batch
/// starts is pinned ([`MeetingTools::pin`]), and a recording is heard only
/// on that one, or on another instance of the same engine in the same
/// language, which is pinned in its place. If the engine is changed part of
/// the way through, whether the new one has opened yet or not, each
/// recording left fails its row and says so, rather than being heard on
/// another engine without a word. The engine is still borrowed per file,
/// only while the file is heard, never while it is decoded or saved, so
/// quitting waits for a batch only as long as it waits for one file.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_batch(
    batch: &mut Batch,
    paths: &[Option<PathBuf>],
    format: &Format,
    save: &mut dyn FnMut(&str, &str, &str) -> std::io::Result<PathBuf>,
    lend: &dyn Fn() -> Option<MeetingTools>,
    hold: &dyn Fn(bool) -> bool,
    on_change: &mut dyn FnMut(&Batch, usize),
    stop: &AtomicBool,
) {
    use crate::meeting_words as words;
    let job = batch.job;
    let pinned = std::cell::RefCell::new(lend().map(|tools| tools.pin()));
    let changed = std::cell::Cell::new(false);
    let pinned_lend = || {
        let mut pinned = pinned.borrow_mut();
        let pin = pinned.as_ref()?;
        match lend() {
            Some(tools) if pin.lends(&tools) => Some(tools),
            // The session was reopened on the same engine, as saving a key or
            // choosing the same engine again does: carry on, on the new one.
            Some(tools) if pin.same_kind(&tools) => {
                *pinned = Some(tools.pin());
                Some(tools)
            }
            Some(_) => {
                changed.set(true);
                None
            }
            // Nothing to lend while the engine is being changed: the old
            // session is gone and the new one is still opening.
            None if pin.is_gone() => {
                changed.set(true);
                None
            }
            None => None,
        }
    };
    for (index, path) in paths.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            batch.set(
                index,
                RowState::Stopped,
                Some(words::BATCH_NOT_STARTED.to_string()),
            );
            on_change(batch, index);
            continue;
        }
        let Some(path) = path else {
            batch.set(index, RowState::Failed, Some(NOT_CHOSEN.to_string()));
            on_change(batch, index);
            continue;
        };
        batch.set(index, RowState::Opening, None);
        on_change(batch, index);
        changed.set(false);
        let outcome = {
            let mut progress = |p: ProgressEvent| {
                batch.set(
                    index,
                    RowState::Hearing {
                        done: p.done,
                        of: p.of,
                    },
                    None,
                );
                on_change(batch, index);
            };
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_job(job, path, None, &pinned_lend, hold, &mut progress, stop)
            }))
            .unwrap_or_else(|_| {
                // A panic while heard never reached the job's own release.
                hold(false);
                Err(words::BATCH_WENT_WRONG.to_string())
            })
        };
        match outcome {
            Ok(view) => {
                let text = (format.render)(&view);
                let name = view.name.clone();
                drop(view);
                match save(&name, format.extension, &text) {
                    Ok(saved) => {
                        if let Some(row) = batch.rows.get_mut(index) {
                            row.saved = Some(saved);
                        }
                        batch.set(index, RowState::Done, None);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not save a batch file's transcript");
                        batch.set(
                            index,
                            RowState::Failed,
                            Some(words::BATCH_NOT_SAVED.to_string()),
                        );
                    }
                }
            }
            Err(why) if stop.load(Ordering::Relaxed) => {
                batch.set(index, RowState::Stopped, Some(why));
            }
            Err(_) if changed.get() => batch.set(
                index,
                RowState::Failed,
                Some(words::BATCH_ENGINE_CHANGED.to_string()),
            ),
            Err(why) => batch.set(index, RowState::Failed, Some(why)),
        }
        on_change(batch, index);
    }
}

/// Which of the page's `paths` a batch may open: each one the open panel or a
/// drop recorded, taken from `chosen` so it is opened only once, and `None`
/// for any other, which its row refuses. Refused whole, with
/// [`NOT_CHOSEN`], when not one was recorded.
fn batch_paths(
    chosen: &ChosenFiles,
    paths: &[String],
) -> Result<Vec<Option<PathBuf>>, &'static str> {
    let taken: Vec<Option<PathBuf>> = paths.iter().map(|p| chosen.take(Path::new(p))).collect();
    if taken.iter().all(Option::is_none) {
        tracing::debug!("refused a batch with no path chosen through the panel or a drop");
        return Err(NOT_CHOSEN);
    }
    Ok(taken)
}

/// Run batch `job`'s thread body, `run`, and end the batch however `run`
/// ends, even by panicking, so the Files page is never left busy. A file
/// the panic left unfinished is marked as gone wrong. Returns the batch as
/// it ended, for the page.
fn finish_batch(meetings: &Meetings, job: u64, run: impl FnOnce()) -> Option<BatchView> {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_err() {
        tracing::warn!("a batch's thread panicked");
    }
    meetings.end_batch(job)
}

/// On `batch-changed`: the batch as it is now, and the row that changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BatchChangedEvent {
    pub index: usize,
    pub batch: BatchView,
}

/// Hear, or read, each of `paths` in turn on a thread of its own, and save
/// each transcript to Downloads in `format` (an extension from
/// `meeting_live_words`' `formats`) as it finishes. Returns the batch at once,
/// every file waiting; each change arrives as `batch-changed`, and the end as
/// `batch-ended`. Nothing becomes the current meeting.
///
/// Each path must be one the open panel or a drop recorded, as for
/// `meeting_open_file`: a path that was not is refused in its own row and
/// never opened, and a batch with no recorded path at all is refused whole.
/// Refused, with a sentence, while any other job runs.
///
/// Not `async`, for the reason `meeting_open_file` gives.
#[tauri::command]
pub fn meeting_batch_start(
    app: AppHandle<Wry>,
    meetings: State<'_, Meetings>,
    dictator: State<'_, Dictator>,
    chosen: State<'_, ChosenFiles>,
    paths: Vec<String>,
    format: String,
) -> Result<BatchView, CommandError> {
    use crate::meeting_words as words;
    use tauri::Manager;
    // Looked up once, here, and moved into the batch's thread: the Files
    // card's format choice stays open while a batch runs, and changing it
    // never reaches a batch already started.
    let format = format_by_extension(&format)
        .ok_or_else(|| CommandError::from(words::BATCH_COULD_NOT_START))?;
    let dir = app
        .path()
        .download_dir()
        .map_err(|_| CommandError::from(words::DOWNLOADS_NOT_FOUND))?;
    meetings.refuse_if_busy()?;
    let taken = batch_paths(&chosen, &paths)?;
    let names: Vec<String> = paths.iter().map(|p| file_name(Path::new(p))).collect();
    let (job, stop) = meetings.begin_batch(names.clone())?;
    let meetings_for_thread = meetings.inner().clone();
    let dictator = dictator.inner().clone();
    let date = crate::dictionary::today();
    let spawned = std::thread::Builder::new()
        .name("macalla-batch".into())
        .spawn(move || {
            let ended = finish_batch(&meetings_for_thread, job, || {
                let lend = || dictator.meeting_tools_loaded();
                let hold = |holds: bool| meetings_for_thread.holding_model(job, holds);
                let mut save = |name: &str, extension: &str, text: &str| {
                    save_new(&dir, name, &date, extension, text)
                };
                let mut on_change = |batch: &Batch, index: usize| {
                    if let Some(view) = meetings_for_thread.batch_changed(batch) {
                        let _ = app.emit("batch-changed", BatchChangedEvent { index, batch: view });
                    }
                };
                let mut batch = Batch::new(job, names);
                run_batch(
                    &mut batch,
                    &taken,
                    format,
                    &mut save,
                    &lend,
                    &hold,
                    &mut on_change,
                    &stop,
                );
            });
            if let Some(ended) = ended {
                let _ = app.emit("batch-ended", ended);
            }
        });
    match spawned {
        Ok(thread) => {
            meetings.attach(job, thread);
            meetings
                .batch()
                .ok_or_else(|| CommandError::from(words::STATE_UNREADABLE))
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start a thread for a batch");
            meetings.end_batch(job);
            meetings.clear_batch();
            Err(words::BATCH_COULD_NOT_START.into())
        }
    }
}

/// The last batch, running or ended, for the page to show again when it
/// opens.
#[tauri::command]
pub fn meeting_batch(meetings: State<'_, Meetings>) -> Option<BatchView> {
    meetings.batch()
}

/// Take an ended batch's list off the page. Its saved files stay.
#[tauri::command]
pub fn meeting_batch_clear(meetings: State<'_, Meetings>) {
    meetings.clear_batch();
}

/// Save the kept meeting to a new file in Downloads in `format`, an
/// extension from `meeting_live_words`' `formats`, and say where. WebVTT is
/// the same [`macalla_core::vtt::write`] a held meeting is written with, so
/// the file [`macalla_core::vtt::parse`] reads back opens here too.
#[tauri::command]
pub fn meeting_save(
    app: AppHandle<Wry>,
    meetings: State<'_, Meetings>,
    format: String,
) -> Result<String, CommandError> {
    let format = format_by_extension(&format).ok_or_else(|| {
        tracing::warn!(%format, "the page asked to save in a format not offered");
        CommandError::from(crate::meeting_words::COULD_NOT_SAVE_FILE)
    })?;
    save_kept(&app, &meetings, format)
}

/// The kept meeting as Markdown, written by [`macalla_core::markdown::write`]
/// with this app's catalogue words. What saving as Markdown gives.
///
/// The meeting is turned into the writer's plain
/// [`macalla_core::markdown::Document`] here. A summary's items are taken
/// from every part in order and flattened: the part split is how a long
/// meeting was sent to a summariser, not something worth showing again
/// here. A part's own error is not shown; the page already said it while
/// the summary ran. A summary written before the transcript was corrected
/// by hand (`MeetingView::summary_out_of_date`) has
/// [`crate::meeting_words::SUMMARY_OUT_OF_DATE`] above it, as the page
/// shows it.
///
/// A turn's name is `TurnView.label` falling back to `speaker`, the same
/// name a summary's `said` already shows beside a quote from that turn.
/// Copy, Save as text, SRT and WebVTT all keep using `speaker` alone, never
/// `label`.
fn meeting_markdown(view: &MeetingView) -> String {
    use crate::meeting_words as words;
    use macalla_core::markdown;
    let items = |pick: fn(&PartView) -> &Vec<ItemView>, parts: &[PartView]| {
        parts
            .iter()
            .flat_map(pick)
            .map(|item| markdown::DocItem {
                text: item.text.clone(),
                owner: item.owner.clone(),
                quote: item.quote.clone(),
                said: item.said.clone(),
            })
            .collect()
    };
    let doc = markdown::Document {
        title: view.name.clone(),
        turns: view
            .turns
            .iter()
            .map(|turn| markdown::DocTurn {
                speaker: turn.label.clone().or_else(|| turn.speaker.clone()),
                start_ms: turn.start_ms,
                text: turn.text.clone(),
            })
            .collect(),
        summary: view.summary.as_deref().map(|parts| markdown::DocSummary {
            key_points: items(|p| &p.key_points, parts),
            decisions: items(|p| &p.decisions, parts),
            action_items: items(|p| &p.action_items, parts),
        }),
        summary_out_of_date: view.summary_out_of_date,
    };
    markdown::write(
        &doc,
        &markdown::Words {
            key_points: words::KEY_POINTS,
            decisions: words::DECISIONS,
            action_items: words::ACTION_ITEMS,
            transcript: words::MARKDOWN_TRANSCRIPT,
            summary_out_of_date: words::SUMMARY_OUT_OF_DATE,
        },
    )
}

/// Stop the job running, if any. Work it stops ends with `meeting-error` and
/// keeps nothing, but a summary whose last reply had already come back still
/// ends with `meeting-summary`.
#[tauri::command]
pub fn meeting_cancel(meetings: State<'_, Meetings>) {
    meetings.stop();
}

/// The meeting kept in memory, for the page to show again when it opens.
#[tauri::command]
pub fn meeting_current(meetings: State<'_, Meetings>) -> Option<MeetingView> {
    meetings.current()
}

/// The summary running, if any, for the page to show again when it opens:
/// its job, how long it has run, whether Stop was pressed, and its last
/// progress.
#[tauri::command]
pub fn meeting_running_summary(meetings: State<'_, Meetings>) -> Option<RunningSummaryView> {
    meetings.running_summary()
}

/// Forget the meeting kept in memory, and any path the panel or a drop gave
/// that was never opened.
#[tauri::command]
pub fn meeting_clear(meetings: State<'_, Meetings>, chosen: State<'_, ChosenFiles>) {
    meetings.let_go_of_held();
    meetings.clear();
    chosen.clear();
}

/// After this much of a live meeting heard with no sound at all from the
/// call, the page says so and points at the permission
/// (`meeting_words::NO_CALL_SOUND_YET`). A refused system audio permission
/// gives exact silence and no error, the same as a call where nobody has
/// spoken, so the notice can only be a nudge, and when it comes is a
/// trade: a call normally makes some sound within seconds of joining (a
/// chime, a greeting, someone else talking), so fifteen seconds rarely
/// fires on a call that works, even one started just before it connects,
/// and still catches a refusal while little of the call has been missed.
/// Once any sound has come, later quiet is only quiet, and says nothing.
pub(crate) const NO_CALL_SOUND_AFTER_MS: u64 = 15_000;

/// On `live-meeting-progress`, about once a second while a live meeting
/// records: how much was heard, and each source's line, as sentences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveProgressEvent {
    pub job: u64,
    pub heard_ms: u64,
    /// How much was heard, in words.
    pub heard: String,
    /// The microphone's line.
    pub you: String,
    /// The call's line, or `None` in a microphone-only meeting.
    pub others: Option<String>,
    /// Whether `others` is the no-sound-from-the-call notice, for the page
    /// to show it as one.
    pub others_notice: bool,
    /// Why the meeting hears the microphone alone, when it does because the
    /// call's sound could not be opened.
    pub microphone_only: Option<String>,
}

/// A live meeting still running, from `meeting_running_live`, for a page
/// that opens while it records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunningLiveView {
    pub job: u64,
    /// Whether Stop and keep or Discard was pressed.
    pub stopping: bool,
    /// The last `live-meeting-progress` sent, once there has been one.
    pub progress: Option<LiveProgressEvent>,
}

/// The Files page's own words, from `meeting_live_words`: the live meeting
/// card's, the line above a summary, a summary's three list headings, and
/// the Save control's and the batch card's. The page shows these and writes
/// none of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LiveWordsView {
    pub heading: &'static str,
    pub about: &'static str,
    pub start: &'static str,
    pub stop: &'static str,
    pub discard: &'static str,
    pub starting: &'static str,
    pub stopping: &'static str,
    /// Above a summary: what each quote promises.
    pub summary_quotes: &'static str,
    /// Above a summary's key points list, on the page and in Markdown export.
    pub key_points: &'static str,
    /// Above a summary's decisions list, on the page and in Markdown export.
    pub decisions: &'static str,
    /// Above a summary's action items list, on the page and in Markdown
    /// export.
    pub action_items: &'static str,
    /// The label beside the Files card's one choice of format, which the
    /// current meeting's Save button and a batch both save in.
    pub save_transcripts_as: &'static str,
    /// The current meeting's Save button.
    pub save: &'static str,
    /// Below the current meeting, once it has a summary: which format keeps
    /// it too.
    pub summary_gone_unless_markdown: &'static str,
    /// Above a summary written before the transcript was corrected.
    pub summary_out_of_date: &'static str,
    /// Find in the transcript, and correcting a turn.
    pub transcript: TranscriptWordsView,
    /// The batch card's heading, sentence and buttons.
    pub batch_heading: &'static str,
    pub batch_about: &'static str,
    pub batch_stop: &'static str,
    pub batch_clear: &'static str,
    /// The formats a transcript can be saved in, each with its name, the
    /// first the default.
    pub formats: Vec<FormatView>,
}

/// The transcript card's words for find and for correcting a turn, from
/// `meeting_words`. The templates carry named placeholders the page fills
/// with data: `count` has `{current}` and `{total}`. Each turn's Edit label
/// comes with the turn (`TurnView::edit_label`).
///
/// `correction_offer`, `add_to_dictionary`, `added` and `updated` are the
/// dictionary's own catalogue words (`dictionary_words.rs`), read here too
/// so the offer under a corrected turn says the same thing History's own
/// "Add to dictionary" does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranscriptWordsView {
    pub find: &'static str,
    pub count: &'static str,
    pub none: &'static str,
    pub previous: &'static str,
    pub next: &'static str,
    pub edit: &'static str,
    pub field: &'static str,
    pub save: &'static str,
    pub cancel: &'static str,
    /// The offer under a turn just corrected by hand: `{heard}`, `{written}`.
    pub correction_offer: &'static str,
    pub add_to_dictionary: &'static str,
    /// After using the offer's button: `{written}`.
    pub added: &'static str,
    /// After using the offer's button: `{written}`.
    pub updated: &'static str,
}

/// A format a transcript can be saved in: its extension, which the page
/// passes back to `meeting_save` and `meeting_batch_start`, and its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormatView {
    pub extension: &'static str,
    pub label: &'static str,
}

/// A time in a meeting as minutes and seconds, with hours once there are
/// any: data for a sentence, not a sentence.
fn clock(ms: u64) -> String {
    let seconds = ms / 1_000;
    let (hours, minutes, seconds) = (seconds / 3_600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// A live meeting's progress as the page says it. `microphone_only` is why
/// the call's sound is not heard, when it is not.
pub(crate) fn live_progress(
    job: u64,
    progress: LiveProgress,
    microphone_only: Option<&'static str>,
) -> LiveProgressEvent {
    use crate::meeting_words;
    let silent_call = progress
        .others
        .is_some_and(|call| !call.signal && progress.heard_ms >= NO_CALL_SOUND_AFTER_MS);
    LiveProgressEvent {
        job,
        heard_ms: progress.heard_ms,
        heard: meeting_words::heard(&clock(progress.heard_ms)),
        you: meeting_words::microphone_health(progress.you).to_string(),
        others: progress.others.map(|call| {
            if silent_call {
                meeting_words::NO_CALL_SOUND_YET
            } else {
                meeting_words::call_health(call)
            }
            .to_string()
        }),
        others_notice: silent_call,
        microphone_only: microphone_only.map(str::to_string),
    }
}

/// A live meeting's failure as the page says it, logged once here: nothing
/// was kept, and why.
fn live_say(error: macalla_session::Error) -> String {
    crate::say::report(&error);
    crate::meeting_words::not_kept(&error)
}

/// A live meeting, with no Tauri in it: borrow the engine, open the
/// sources with `open`, get ready, and record until `ending` says to stop
/// or discard. The meeting that results is the page's current meeting, as a
/// file's is, named for a live meeting, its speakers shown through the
/// catalogue, and with why it stopped early when it did.
///
/// `hold` marks the job as holding a borrowed model, as a file's hearing
/// does, and says `false` once the job was told to stop: quitting waits for
/// a meeting that holds one. `on_ready` is given what the meeting commits
/// as it records, once it is ready to: what a quit can still keep. Everything this opens it lets go of before it
/// returns: the sources stop when dropped.
pub(crate) fn run_live(
    job: u64,
    lend: &dyn Fn() -> Option<MeetingTools>,
    open: &mut dyn FnMut() -> macalla_session::Result<(Box<dyn Source>, CallAudio)>,
    hold: &dyn Fn(bool) -> bool,
    on_ready: &mut dyn FnMut(Committed),
    on_progress: &mut dyn FnMut(LiveProgressEvent),
    ending: &live_meeting::Ending,
) -> Result<MeetingView, String> {
    use crate::meeting_words;
    let tools = lend().ok_or_else(|| meeting_words::NO_ENGINE.to_string())?;
    if !hold(true) {
        return Err(meeting_words::DISCARDED.to_string());
    }
    let heard = (|| {
        let (you, call) = open()?;
        let microphone_only = call
            .unavailable
            .as_ref()
            .map(meeting_words::microphone_only);
        let prepared = live_meeting::prepare(&tools, you, call.others)?;
        on_ready(prepared.committed());
        prepared.record(
            &mut |progress| on_progress(live_progress(job, progress, microphone_only)),
            ending,
        )
    })();
    hold(false);
    let LiveMeeting {
        meeting,
        ended_early,
    } = heard.map_err(live_say)?;
    let mut view = live_view(job, meeting);
    view.ended_early = ended_early.map(|error| {
        crate::say::report(&error);
        meeting_words::ended_early(&error)
    });
    Ok(view)
}

/// Begin a live meeting and claim the microphone for it, in that order, so
/// a meeting refused as busy never says dictation is off; then hand both to
/// `spawn`, which runs [`live_thread`] on a thread of its own and returns
/// its handle (`None` when it ran the meeting itself, as a test does).
/// Refused, with a sentence and nothing left behind, while another job runs
/// or a dictation has the microphone.
fn start_live(
    meetings: &Meetings,
    microphone: &MicrophoneOwner,
    spawn: impl FnOnce(
        u64,
        Arc<live_meeting::Ending>,
        MicrophoneClaim,
    ) -> std::io::Result<Option<JoinHandle<()>>>,
) -> Result<u64, CommandError> {
    let (job, ending) = meetings.begin_live()?;
    let claim = match microphone.for_meeting() {
        Ok(claim) => claim,
        Err(refused) => {
            meetings.end(job, None);
            return Err(refused.into());
        }
    };
    match spawn(job, ending, claim) {
        Ok(thread) => {
            if let Some(thread) = thread {
                meetings.attach(job, thread);
            }
            Ok(job)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start a thread for a live meeting");
            meetings.end(job, None);
            Err(crate::meeting_words::COULD_NOT_START.into())
        }
    }
}

/// A live meeting's thread: `run` the meeting while `microphone` is held,
/// then give the microphone back, whether `run` returned or panicked, and
/// only then end the job and `tell` the page, so a dictation started on
/// hearing it is not refused.
fn live_thread(
    meetings: &Meetings,
    job: u64,
    microphone: MicrophoneClaim,
    run: impl FnOnce() -> Result<MeetingView, String>,
    tell: impl FnOnce(Ending),
) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .unwrap_or_else(|_| Err(crate::meeting_words::WENT_WRONG.to_string()));
    drop(microphone);
    meetings.finish(job, outcome, tell);
}

/// Start a live meeting on a thread of its own: the microphone dictation
/// uses, as You, and the call's sound, as Others, or the microphone alone
/// when the call's sound cannot be opened. Returns the job's number at once;
/// progress arrives as `live-meeting-progress`, the meeting as
/// `meeting-transcript` once it is stopped, and a discard or a failure as
/// `meeting-error`.
///
/// Refused, with a sentence, while a dictation runs, and while any other
/// job runs. The meeting holds the microphone until it ends, and every
/// dictation trigger is refused meanwhile.
///
/// Not `async`, for the reason `meeting_open_file` gives: quitting cannot
/// land between `begin_live` and `attach`.
///
/// Sends `live-meeting-ended` once it is over, however it ended, after the
/// microphone is given back: the page clears a dictation refusal on it.
#[tauri::command]
pub fn meeting_live_start(
    app: AppHandle<Wry>,
    meetings: State<'_, Meetings>,
    dictator: State<'_, Dictator>,
) -> Result<u64, CommandError> {
    let meetings = meetings.inner();
    let dictator = dictator.inner();
    start_live(meetings, dictator.microphone(), |job, ending, claim| {
        let meetings = meetings.clone();
        let dictator = dictator.clone();
        std::thread::Builder::new()
            .name("macalla-live-meeting".into())
            .spawn(move || {
                // The microphone dictation would listen through now.
                let device = dictator.device();
                let lend = || dictator.meeting_tools_loaded();
                let hold = |holds: bool| meetings.holding_model(job, holds);
                let mut ready = |committed: Committed| meetings.live_ready(job, committed);
                let mut open = || -> macalla_session::Result<(Box<dyn Source>, CallAudio)> {
                    let you = live_meeting::open_microphone(device.as_deref())?;
                    Ok((you, live_meeting::open_call_audio()))
                };
                let mut progress = |event: LiveProgressEvent| {
                    meetings.live_progressed(job, event.clone());
                    let _ = app.emit("live-meeting-progress", event);
                };
                live_thread(
                    &meetings,
                    job,
                    claim,
                    || {
                        run_live(
                            job,
                            &lend,
                            &mut open,
                            &hold,
                            &mut ready,
                            &mut progress,
                            &ending,
                        )
                    },
                    |ending| emit_ending(&app, ending),
                );
                let _ = app.emit("live-meeting-ended", job);
            })
            .map(Some)
    })
}

/// End the live meeting running, if any, and keep what it heard: it
/// arrives as `meeting-transcript`. Discarding it is `meeting_cancel`.
#[tauri::command]
pub fn meeting_live_stop(meetings: State<'_, Meetings>) {
    meetings.stop_live();
}

/// The live meeting running, if any, for the page to show again when it
/// opens.
#[tauri::command]
pub fn meeting_running_live(meetings: State<'_, Meetings>) -> Option<RunningLiveView> {
    meetings.running_live()
}

/// The live meeting card's words, and the Files page's other words that
/// come from the catalogue: the line above a summary, and the transcript's
/// find and correcting.
#[tauri::command]
pub fn meeting_live_words() -> LiveWordsView {
    use crate::meeting_words;
    LiveWordsView {
        heading: meeting_words::HEADING,
        about: meeting_words::ABOUT,
        start: meeting_words::START,
        stop: meeting_words::STOP,
        discard: meeting_words::DISCARD,
        starting: meeting_words::STARTING,
        stopping: meeting_words::STOPPING,
        summary_quotes: meeting_words::SUMMARY_QUOTES,
        key_points: meeting_words::KEY_POINTS,
        decisions: meeting_words::DECISIONS,
        action_items: meeting_words::ACTION_ITEMS,
        save_transcripts_as: meeting_words::SAVE_TRANSCRIPTS_AS,
        save: meeting_words::SAVE,
        summary_gone_unless_markdown: meeting_words::TRANSCRIPT_AND_SUMMARY_GONE_UNLESS_MARKDOWN,
        summary_out_of_date: meeting_words::SUMMARY_OUT_OF_DATE,
        transcript: TranscriptWordsView {
            find: meeting_words::FIND,
            count: meeting_words::FIND_COUNT,
            none: meeting_words::FIND_NONE,
            previous: meeting_words::FIND_PREVIOUS,
            next: meeting_words::FIND_NEXT,
            edit: meeting_words::EDIT,
            field: meeting_words::EDIT_FIELD,
            save: meeting_words::SAVE,
            cancel: meeting_words::EDIT_CANCEL,
            correction_offer: crate::dictionary_words::CORRECTION_OFFER,
            add_to_dictionary: crate::dictionary_words::ADD_TO_DICTIONARY,
            added: crate::dictionary_words::ADDED,
            updated: crate::dictionary_words::UPDATED,
        },
        batch_heading: meeting_words::BATCH_HEADING,
        batch_about: meeting_words::BATCH_ABOUT,
        batch_stop: meeting_words::BATCH_STOP,
        batch_clear: meeting_words::BATCH_CLEAR,
        formats: FORMATS
            .iter()
            .map(|f| FormatView {
                extension: f.extension,
                label: f.label,
            })
            .collect(),
    }
}

/// Where hearing, tidying and a summary would send the words, for the page to
/// say before any button is pressed. Building a summariser sends nothing, but
/// can read a stored key, and a keychain read can wait on the OS access
/// prompt, so this runs off the main thread.
#[tauri::command(async)]
pub fn meeting_destinations(
    dictator: State<'_, Dictator>,
    history: State<'_, HistoryHandle>,
) -> DestinationsView {
    let tools = dictator.meeting_tools();
    let engine = tools
        .as_ref()
        .map(|t| crate::say::engine_name(t.engine_id()));
    let tidy = tools
        .as_ref()
        .and_then(|t| t.tidy_destination())
        .map(str::to_string);
    let tidy_choice = tools
        .as_ref()
        .and_then(|t| t.tidy_choice())
        .map(str::to_string);
    let tidy_unavailable = tools
        .as_ref()
        .and_then(|t| t.tidy_unavailable())
        .map(str::to_string);
    // Read out and dropped before `Summariser::build`, which can wait on the
    // OS keychain prompt: `tools` holds a clone of the engine's `Arc`, and
    // holding that open across an indefinite wait is one more thing that
    // could keep a ggml engine's Metal buffers alive past the quit
    // sequence's deadline if the dictation thread dropped its own session
    // while this command was waiting.
    drop(tools);
    let summariser = Summariser::build(
        &crate::dictation::cleanup_config(&history),
        &SystemSecrets::new(),
    )
    .ok();
    DestinationsView {
        engine,
        tidy,
        tidy_choice,
        tidy_unavailable,
        summary: summariser.as_ref().map(|s| s.destination().to_string()),
        summary_choice: summariser.as_ref().map(|s| s.fingerprint().to_string()),
        summary_pace: summariser
            .as_ref()
            .and_then(|s| s.pace())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Files page's `useEffect` blocks: the page has no seam without a
    /// web view, so this reads its source, as `dictation.rs` does for `run`.
    fn files_view_effects() -> &'static str {
        let source = include_str!("../../src/App.tsx");
        let start = source
            .find("\nfunction FilesView(")
            .expect("the Files page");
        let body = &source[start..];
        let end = body.find("\n  return (").expect("its render");
        &body[..end]
    }

    /// "No speech engine is ready" comes from `meeting_destinations`, which
    /// reads the tools the dictation thread lends. The page asked once, when
    /// it opened: an engine chosen or downloaded while it was open, or still
    /// loading when it opened, never reached it. It must ask again on every
    /// `dictation-state: idle`, which follows each open, successful or not,
    /// after the tools are lent (`open_and_lend`).
    #[test]
    fn the_files_page_asks_where_words_go_again_when_dictation_goes_idle() {
        let effects = files_view_effects();
        let listener = effects
            .find("listen<string>(\"dictation-state\"")
            .expect("the Files page listens for dictation-state");
        let after = &effects[listener..];
        let handler = &after[..after.find("}),").expect("the listener's end")];
        assert!(handler.contains("\"idle\""), "{handler}");
        assert!(handler.contains("meetingDestinations()"), "{handler}");
        assert!(handler.contains("setDestinations"), "{handler}");
    }
    use macalla_core::audio::AudioSegment;
    use macalla_core::dictionary::Dictionary;
    use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
    use macalla_session::{Config, Session};
    use std::sync::atomic::AtomicUsize;

    /// An engine that segments for itself and hears one line in any
    /// recording, or with `silent`, commits nothing.
    struct OneLine {
        silent: bool,
    }

    struct OneLineStream {
        silent: bool,
    }

    impl SttStream for OneLineStream {
        fn push(&mut self, _samples: &[f32]) -> macalla_core::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            Ok(Vec::new())
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            if self.silent {
                return Ok(Vec::new());
            }
            Ok(vec![Transcript {
                text: "we ship on friday".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 2_000,
            }])
        }
    }

    impl SttEngine for OneLine {
        fn id(&self) -> &'static str {
            "one-line"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps {
                streaming: true,
                owns_segmentation: true,
                concurrent_streams: true,
                ..EngineCaps::batch()
            }
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(OneLineStream {
                silent: self.silent,
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

    fn lent_on(silent: bool) -> Option<MeetingTools> {
        Session::with_engine(
            Box::new(OneLine { silent }),
            Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        )
        .meeting_tools()
        .ok()
    }

    fn lent() -> Option<MeetingTools> {
        lent_on(false)
    }

    /// One session, as the app has: every loan from it is the same engine
    /// instance, which a batch pins. [`lent`] makes a new one each time.
    fn one_session() -> Session {
        Session::with_engine(
            Box::new(OneLine { silent: false }),
            Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        )
    }

    fn nothing_lent() -> Option<MeetingTools> {
        None
    }

    fn quiet() -> AtomicBool {
        AtomicBool::new(false)
    }

    /// A job that may always hold a model.
    fn holds(_: bool) -> bool {
        true
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macalla-desktop-files-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two seconds of 16 kHz mono 16-bit silence.
    fn write_wav(path: &Path) {
        let samples = 32_000u32;
        let data_len = samples * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&32_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(bytes.len() + data_len as usize, 0);
        std::fs::write(path, bytes).unwrap();
    }

    fn vtt(name: &str) -> PathBuf {
        let path = temp_dir(name).join("meeting.vtt");
        std::fs::write(
            &path,
            "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\n<v Aoife Byrne>We ship on Friday.\n",
        )
        .unwrap();
        path
    }

    fn view(job: u64) -> MeetingView {
        MeetingView {
            job,
            revision: 0,
            name: "meeting.vtt".into(),
            turns: Vec::new(),
            named_speakers: false,
            audio_ms: None,
            tidied: None,
            summary: None,
            summary_by: None,
            summary_out_of_date: false,
            ended_early: None,
            held: None,
        }
    }

    #[test]
    fn a_transcript_file_opens_without_borrowing_the_engine() {
        let borrowed = AtomicUsize::new(0);
        let lend = || {
            borrowed.fetch_add(1, Ordering::SeqCst);
            lent()
        };

        let view = run_job(1, &vtt("vtt"), None, &lend, &holds, &mut |_| {}, &quiet()).unwrap();

        assert_eq!(borrowed.load(Ordering::SeqCst), 0);
        assert_eq!(view.name, "meeting.vtt");
        assert!(view.named_speakers);
        assert_eq!(view.turns[0].speaker.as_deref(), Some("Aoife Byrne"));
        assert_eq!(view.audio_ms, None);
        assert_eq!(view.tidied, None);
    }

    #[test]
    fn a_recording_with_no_engine_open_says_so() {
        let path = temp_dir("no-engine").join("meeting.wav");
        write_wav(&path);

        let result = run_job(2, &path, None, &nothing_lent, &holds, &mut |_| {}, &quiet());

        assert_eq!(result, Err(NO_ENGINE.to_string()));
    }

    #[test]
    fn a_recording_is_heard_on_the_lent_engine_with_progress() {
        let path = temp_dir("wav").join("meeting.wav");
        write_wav(&path);
        let mut seen = Vec::new();

        let view = run_job(
            3,
            &path,
            None,
            &lent,
            &holds,
            &mut |p| seen.push(p),
            &quiet(),
        )
        .unwrap();

        assert_eq!(view.job, 3);
        assert_eq!(view.turns.len(), 1);
        assert_eq!(view.audio_ms, Some(2_000));
        assert!(seen.iter().all(|p| p.phase == "hearing" && p.job == 3));
        assert_eq!(seen.last().map(|p| (p.done, p.of)), Some((2_000, 2_000)));
    }

    #[test]
    fn a_recording_with_nothing_in_it_says_so_in_a_sentence() {
        let path = temp_dir("silent").join("meeting.wav");
        write_wav(&path);

        let result = run_job(
            4,
            &path,
            None,
            &|| lent_on(true),
            &holds,
            &mut |_| {},
            &quiet(),
        );

        assert_eq!(
            result,
            Err("Nothing was heard in this recording.".to_string())
        );
    }

    #[test]
    fn tidying_with_cleanup_off_is_refused_with_a_sentence() {
        let path = temp_dir("tidy-off").join("meeting.wav");
        write_wav(&path);

        let result = run_job(
            5,
            &path,
            Some("Your server"),
            &lent,
            &holds,
            &mut |_| {},
            &quiet(),
        );

        assert_eq!(result, Err(CLEANUP_OFF.to_string()));
    }

    #[test]
    fn tidying_a_transcript_with_no_engine_open_names_what_is_missing() {
        let result = run_job(
            6,
            &vtt("tidy-no-session"),
            Some("Your server"),
            &nothing_lent,
            &holds,
            &mut |_| {},
            &quiet(),
        );

        assert_eq!(result, Err(NO_SESSION.to_string()));
    }

    #[test]
    fn nothing_is_sent_unless_the_cleanup_choice_is_still_the_one_shown() {
        assert_eq!(same_choice("a1b2", "a1b2"), Ok(()));
        assert_eq!(same_choice("a1b2", "c3d4"), Err(CHOICE_CHANGED.to_string()));
    }

    #[test]
    fn a_stopped_job_ends_with_a_sentence_and_keeps_nothing() {
        let path = temp_dir("stopped").join("meeting.wav");
        write_wav(&path);

        let result = run_job(
            7,
            &path,
            None,
            &lent,
            &holds,
            &mut |_| {},
            &AtomicBool::new(true),
        );

        assert_eq!(
            result,
            Err("Stopped. Nothing from this file was kept.".to_string())
        );
    }

    #[test]
    fn only_one_job_runs_at_a_time_and_the_next_may_start_after() {
        let meetings = Meetings::default();

        let (first, _) = meetings.begin().unwrap();
        assert_eq!(
            meetings.begin().map(|(job, _)| job),
            Err(CommandError::from(BUSY))
        );

        meetings.end(first, None);
        let (second, _) = meetings.begin().unwrap();
        assert_eq!(second, first + 1);
    }

    #[test]
    fn a_finished_job_keeps_its_meeting_and_a_failed_one_keeps_the_last() {
        let meetings = Meetings::default();

        let (first, _) = meetings.begin().unwrap();
        meetings.end(first, Some(view(first)));
        let (second, _) = meetings.begin().unwrap();
        meetings.end(second, None);

        assert_eq!(meetings.current().map(|v| v.job), Some(first));
    }

    #[test]
    fn a_summary_is_kept_only_while_its_meeting_is_still_the_current_one() {
        let meetings = Meetings::default();
        let (first, _) = meetings.begin().unwrap();
        meetings.end(first, Some(view(first)));

        // Summarised, then cleared before it finished: the clear stands.
        let (job, _, summarising) = meetings.begin_summary().unwrap();
        meetings.clear();
        meetings.end_summary(job, Some(summarising));
        assert_eq!(meetings.current(), None);

        // Summarised, then another file finished first: the newer file stays.
        let (third, _) = meetings.begin().unwrap();
        meetings.end(third, Some(view(third)));
        let (job, _, summarising) = meetings.begin_summary().unwrap();
        let mut summarised = summarising;
        summarised.summary_by = Some("Apple Intelligence".into());
        // A file job cannot start while the summary runs, so the change is
        // made directly, as a later file would leave it.
        if let Ok(mut inner) = meetings.0.lock() {
            inner.current = Some(view(99));
        }
        meetings.end_summary(job, Some(summarised));
        assert_eq!(meetings.current().map(|v| v.job), Some(99));
    }

    #[test]
    fn a_job_has_ended_before_the_page_hears_that_it_has() {
        let meetings = Meetings::default();
        let idle = || meetings.0.lock().unwrap().running.is_none();

        let (file, _) = meetings.begin().unwrap();
        let mut told = 0;
        meetings.finish(file, Ok(view(file)), |ending| {
            assert!(matches!(ending, Ending::Transcript(_)));
            assert!(idle(), "a file's transcript");
            assert_eq!(
                meetings.current().map(|v| v.job),
                Some(file),
                "the file's meeting is kept before the page hears of it"
            );
            told += 1;
        });
        let (failed, _) = meetings.begin().unwrap();
        meetings.finish(failed, Err("no".into()), |ending| {
            assert!(matches!(ending, Ending::Error(ErrorEvent { job, .. }) if job == failed));
            assert!(idle(), "a file's error");
            told += 1;
        });

        let (summary, _, current) = meetings.begin_summary().unwrap();
        let mut summarised = current;
        summarised.summary_by = Some("Local".into());
        meetings.finish_summary(summary, Ok(summarised), |ending| {
            assert!(matches!(ending, Ending::Summary(_)));
            assert!(idle(), "a summary");
            assert_eq!(
                meetings.current().and_then(|v| v.summary_by),
                Some("Local".to_string()),
                "the summary is kept before the page hears of it"
            );
            told += 1;
        });
        let (stopped, _, _) = meetings.begin_summary().unwrap();
        meetings.finish_summary(stopped, Err(SUMMARY_STOPPED.into()), |ending| {
            assert!(matches!(ending, Ending::Error(ErrorEvent { job, .. }) if job == stopped));
            assert!(idle(), "a summary's error");
            told += 1;
        });
        assert_eq!(told, 4);
    }

    #[test]
    fn a_running_summary_can_be_found_again_with_its_stop_and_last_progress() {
        use macalla_core::meeting_summary::PartProgress;
        let meetings = Meetings::default();
        let (file, _) = meetings.begin().unwrap();
        assert_eq!(
            meetings.running_summary(),
            None,
            "a file job is not a summary"
        );
        meetings.end(file, Some(view(file)));

        let (job, _, _) = meetings.begin_summary().unwrap();
        let found = meetings.running_summary().expect("the summary runs");
        assert_eq!(
            (found.job, found.stopping, found.destination, found.progress),
            (job, false, None, None)
        );

        meetings.summary_built(job, "Apple Intelligence", Some("A part takes a while."));
        let found = meetings.running_summary().expect("still running");
        assert_eq!(
            (found.destination.as_deref(), found.pace.as_deref()),
            (Some("Apple Intelligence"), Some("A part takes a while.")),
            "what is running, not what the settings say now"
        );

        let started = summary_progress(file, PartProgress::Started { part: 1, of: 2 });
        meetings.summary_progressed(job, started.clone());
        meetings.summary_progressed(
            job + 1,
            summary_progress(file, PartProgress::Finished { part: 2, of: 2 }),
        );
        meetings.stop();
        let found = meetings.running_summary().expect("still running");
        assert_eq!(
            (found.stopping, found.progress),
            (true, Some(started)),
            "another job's progress is not kept"
        );

        meetings.end_summary(job, None);
        assert_eq!(meetings.running_summary(), None);
    }

    #[test]
    fn summary_progress_names_the_meeting_the_stage_and_the_part() {
        use macalla_core::meeting_summary::PartProgress;
        assert_eq!(
            summary_progress(7, PartProgress::Started { part: 1, of: 3 }),
            SummaryProgressEvent {
                job: 7,
                stage: "started",
                part: 1,
                of: 3
            }
        );
        assert_eq!(
            summary_progress(7, PartProgress::Finished { part: 3, of: 3 }).stage,
            "finished"
        );
    }

    #[test]
    fn with_no_meeting_there_is_nothing_to_summarise() {
        let result = Meetings::default().begin_summary().map(|(job, _, _)| job);

        assert_eq!(
            result,
            Err(CommandError::from(
                "There is no file on the Files page to summarise."
            ))
        );
    }

    #[test]
    fn a_file_that_will_not_decode_as_audio_is_not_blamed_on_the_microphone() {
        let error = macalla_session::Error::Platform(macalla_platform::Error::Audio(
            "cannot open /tmp/meeting.wav: unexpected end of file".to_string(),
        ));
        assert_eq!(say(error), "This recording could not be read as audio.");
    }

    #[test]
    fn quitting_stops_a_job_holding_a_model_and_waits_for_it() {
        let meetings = Meetings::default();
        let (job, stop) = meetings.begin().unwrap();
        assert!(meetings.holding_model(job, true));
        let finished = Arc::new(AtomicBool::new(false));
        let thread = {
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                finished.store(true, Ordering::Relaxed);
            })
        };
        meetings.attach(job, thread);

        meetings.stop_now();
        let bounded = meetings.wait_until(Instant::now() + std::time::Duration::from_secs(1));

        assert!(bounded, "the job finished well inside the deadline");
        assert!(finished.load(Ordering::Relaxed));
    }

    #[test]
    fn quitting_gives_up_at_the_deadline_on_a_job_that_never_stops() {
        let meetings = Meetings::default();
        let (job, _stop) = meetings.begin().unwrap();
        assert!(meetings.holding_model(job, true));
        // Ignores the stop flag entirely, unlike a real job, but still ends
        // on its own so the test does not leak a thread that runs forever.
        // Sleeps far past the 50 ms deadline below, rather than something
        // closer to it, so a scheduling delay on a busy shared machine
        // between spawning this thread and calling `wait_until` cannot let
        // the sleep finish first and pass the test for the wrong reason.
        let thread = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
        });
        meetings.attach(job, thread);

        meetings.stop_now();
        let bounded = meetings.wait_until(Instant::now() + std::time::Duration::from_millis(50));

        assert!(
            !bounded,
            "a job that ignores stop must not hang the deadline"
        );
    }

    #[test]
    fn quitting_tells_a_summary_to_stop_without_waiting_for_it() {
        let meetings = Meetings::default();
        let (first, _) = meetings.begin().unwrap();
        meetings.end(first, Some(view(first)));
        let (job, stop, _) = meetings.begin_summary().unwrap();
        let gate = Arc::new(AtomicBool::new(false));
        let thread = {
            let gate = Arc::clone(&gate);
            // Held until the test lets it go, as a request still out would be.
            std::thread::spawn(move || {
                while !gate.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            })
        };
        meetings.attach(job, thread);

        meetings.stop_now();
        let bounded = meetings.wait_until(Instant::now() + std::time::Duration::from_secs(1));

        assert!(bounded, "a summary holds no model, so nothing waits for it");
        assert!(stop.load(Ordering::Relaxed));
        gate.store(true, Ordering::Relaxed);
    }

    #[test]
    fn the_text_to_copy_names_each_speaker_it_has() {
        let turn = |speaker: Option<&str>, text: &str| TurnView {
            speaker: speaker.map(str::to_string),
            label: speaker.map(str::to_string),
            start_ms: 0,
            end_ms: 1_000,
            text: text.into(),
            edit_label: String::new(),
        };
        let mut meeting = view(1);
        meeting.turns = vec![
            turn(Some("Aoife Byrne"), "We ship on Friday."),
            turn(None, "Agreed."),
        ];

        assert_eq!(
            transcript_text(&meeting),
            "Aoife Byrne: We ship on Friday.\n\nAgreed."
        );
    }

    /// The Markdown writer and its own tests live in
    /// `macalla_core::markdown`; this checks the wiring only: the
    /// catalogue's words reach the file, the out-of-date sentence is there
    /// exactly when the view says so, a summary's parts are flattened, and
    /// a turn is named by its label, falling back to its speaker.
    #[test]
    fn saving_as_markdown_uses_the_catalogue_and_the_views_names() {
        let turn =
            |speaker: Option<&str>, label: Option<&str>, start_ms: u64, text: &str| TurnView {
                speaker: speaker.map(str::to_string),
                label: label.map(str::to_string),
                start_ms,
                end_ms: start_ms + 1_000,
                text: text.into(),
                edit_label: String::new(),
            };
        let item = |text: &str, quote: &str, owner: Option<&str>, said: &str| ItemView {
            text: text.into(),
            quote: quote.into(),
            owner: owner.map(str::to_string),
            turn: 0,
            said: said.into(),
        };
        let part = |key_points, decisions, action_items| PartView {
            start_ms: 0,
            end_ms: 1_000,
            key_points,
            decisions,
            action_items,
            dropped: 0,
            error: Some("not shown".into()),
        };
        let mut meeting = view(1);
        meeting.name = "Team Sync.m4a".into();
        meeting.turns = vec![
            turn(
                Some("aoife-id"),
                Some("Aoife Byrne"),
                0,
                "We ship on Friday.",
            ),
            turn(Some("Others"), None, 65_000, "Agreed."),
            turn(None, None, 3_725_000, "Late words."),
        ];
        meeting.summary = Some(vec![
            part(
                vec![item(
                    "  Ship on Friday. ",
                    "We ship on Friday",
                    None,
                    "Aoife Byrne, 0:00",
                )],
                vec![item(
                    "Friday it is.",
                    "Agreed",
                    Some(" Aoife Byrne "),
                    "Others, 1:05",
                )],
                vec![],
            ),
            part(
                vec![],
                vec![],
                vec![item("Confirm.", "Late words", Some("Ciara"), "1:02:05")],
            ),
        ]);
        let summary_and_turns = "## Key points\n\n\
            - Ship on Friday. \u{201c}We ship on Friday\u{201d} (Aoife Byrne, 0:00)\n\n\
            ## Decisions\n\n\
            - Aoife Byrne: Friday it is. \u{201c}Agreed\u{201d} (Others, 1:05)\n\n\
            ## Action items\n\n\
            - Ciara: Confirm. \u{201c}Late words\u{201d} (1:02:05)\n\n\
            ## Transcript\n\n\
            **Aoife Byrne** (0:00) We ship on Friday.\n\n\
            **Others** (1:05) Agreed.\n\n\
            (1:02:05) Late words.\n";

        assert_eq!(
            meeting_markdown(&meeting),
            format!("# Team Sync.m4a\n\n{summary_and_turns}")
        );
        meeting.summary_out_of_date = true;
        assert_eq!(
            meeting_markdown(&meeting),
            format!(
                "# Team Sync.m4a\n\n{}\n\n{summary_and_turns}",
                crate::meeting_words::SUMMARY_OUT_OF_DATE
            )
        );
    }

    #[test]
    fn the_vtt_export_round_trips_through_the_reader() {
        let mut meeting = view(1);
        meeting.turns = vec![
            TurnView {
                speaker: Some("Aoife Byrne".into()),
                label: Some("Aoife Byrne".into()),
                start_ms: 1_200,
                end_ms: 3_450,
                text: "We ship on Friday.".into(),
                edit_label: String::new(),
            },
            TurnView {
                speaker: None,
                label: None,
                start_ms: 3_600,
                end_ms: 5_000,
                text: "Agreed.".into(),
                edit_label: String::new(),
            },
        ];

        let written = macalla_core::vtt::write(&vtt_turns(&meeting));
        let read = macalla_core::vtt::parse(&written).unwrap();

        assert_eq!(
            read.turns(),
            vec![
                macalla_core::vtt::Turn {
                    speaker: Some("Aoife Byrne".into()),
                    start_ms: 1_200,
                    end_ms: 3_450,
                    text: "We ship on Friday.".into(),
                },
                macalla_core::vtt::Turn {
                    speaker: None,
                    start_ms: 3_600,
                    end_ms: 5_000,
                    text: "Agreed.".into(),
                },
            ]
        );
    }

    #[test]
    fn saving_never_overwrites_a_file_already_there() {
        let dir = temp_dir("save");

        let first = save_new(&dir, "Weekly call.m4a", "2026-09-14", "txt", "first").unwrap();
        let second = save_new(&dir, "Weekly call.m4a", "2026-09-14", "txt", "second").unwrap();

        assert_eq!(
            first.file_name().unwrap(),
            "Weekly call transcript 2026-09-14.txt"
        );
        assert_eq!(
            second.file_name().unwrap(),
            "Weekly call transcript 2026-09-14-2.txt"
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
    }

    #[test]
    fn a_name_starting_with_a_dot_does_not_save_as_a_hidden_file() {
        let dir = temp_dir("dotfile");

        let saved = save_new(&dir, ".vtt", "2026-09-14", "txt", "text").unwrap();

        let name = saved.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.starts_with('.'), "{name}");
        assert_eq!(name, "vtt transcript 2026-09-14.txt");
    }

    #[test]
    fn a_file_with_no_usable_name_saves_as_just_transcript() {
        let dir = temp_dir("no-stem");

        let saved = save_new(&dir, "", "2026-09-14", "txt", "text").unwrap();

        let name = saved.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "transcript 2026-09-14.txt");
    }

    #[test]
    fn each_extension_gets_its_own_file_and_its_own_never_overwrite_counter() {
        let dir = temp_dir("save-extensions");

        for extension in ["txt", "vtt", "srt", "md"] {
            let first = save_new(&dir, "Weekly call.m4a", "2026-09-14", extension, "one").unwrap();
            let second = save_new(&dir, "Weekly call.m4a", "2026-09-14", extension, "two").unwrap();

            assert_eq!(
                first.file_name().unwrap().to_string_lossy(),
                format!("Weekly call transcript 2026-09-14.{extension}")
            );
            assert_eq!(
                second.file_name().unwrap().to_string_lossy(),
                format!("Weekly call transcript 2026-09-14-2.{extension}")
            );
            assert_eq!(std::fs::read_to_string(&first).unwrap(), "one");
            assert_eq!(std::fs::read_to_string(&second).unwrap(), "two");
        }
    }

    #[test]
    fn a_job_holds_a_model_only_when_marked_and_never_once_told_to_stop() {
        let meetings = Meetings::default();
        let (job, stop) = meetings.begin().unwrap();

        assert!(meetings.holding_model(job, true));
        assert!(meetings.holding_model(job, false));
        assert!(
            !meetings.holding_model(job + 1, true),
            "another job's number changes nothing"
        );
        // Quitting sets the stop first, so the job borrows nothing and there
        // is nothing to wait for.
        stop.store(true, Ordering::Relaxed);
        assert!(!meetings.holding_model(job, true));
    }

    #[test]
    fn a_recording_holds_a_model_only_while_heard_and_a_transcript_never_does() {
        let marks = std::sync::Mutex::new(Vec::new());
        let hold = |holds: bool| {
            marks.lock().unwrap().push(holds);
            true
        };
        let path = temp_dir("marks").join("meeting.wav");
        write_wav(&path);

        run_job(8, &path, None, &lent, &hold, &mut |_| {}, &quiet()).unwrap();
        assert_eq!(*marks.lock().unwrap(), [true, false]);

        marks.lock().unwrap().clear();
        run_job(
            9,
            &vtt("marks-vtt"),
            None,
            &lent,
            &hold,
            &mut |_| {},
            &quiet(),
        )
        .unwrap();
        assert!(marks.lock().unwrap().is_empty());
    }

    #[test]
    fn a_stop_that_lands_while_a_recording_is_decoded_hears_nothing() {
        let path = temp_dir("stop-decode").join("meeting.wav");
        write_wav(&path);
        // As quitting leaves it: the job may no longer hold a model.
        let refused = |_: bool| false;

        let result = run_job(10, &path, None, &lent, &refused, &mut |_| {}, &quiet());

        assert_eq!(
            result,
            Err("Stopped. Nothing from this file was kept.".to_string())
        );
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"x").unwrap();
        path
    }

    #[test]
    fn a_path_the_panel_or_a_drop_never_gave_is_refused() {
        let dir = temp_dir("chosen-unrecorded");
        let path = touch(&dir, "meeting.wav");

        assert_eq!(ChosenFiles::default().take(&path), None);
    }

    #[test]
    fn a_recorded_path_is_taken_once_then_refused() {
        let dir = temp_dir("chosen-one-shot");
        let path = touch(&dir, "meeting.wav");
        let chosen = ChosenFiles::default();
        chosen.record(&path).unwrap();

        assert_eq!(chosen.take(&path), Some(path.canonicalize().unwrap()));
        assert_eq!(chosen.take(&path), None, "a path is taken only once");
    }

    #[test]
    fn a_dotdot_spelling_of_the_same_recorded_file_is_accepted_by_design() {
        let dir = temp_dir("chosen-dotdot-same");
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let path = touch(&sub, "meeting.wav");
        let chosen = ChosenFiles::default();
        chosen.record(&path).unwrap();

        // Canonicalising both sides means a `..` spelling of the very same
        // file the panel or a drop gave still compares equal, on purpose.
        let respelled = sub.join("..").join("sub").join("meeting.wav");
        assert_eq!(chosen.take(&respelled), Some(path.canonicalize().unwrap()));
    }

    #[test]
    fn a_dotdot_path_resolving_to_a_different_file_is_refused() {
        let dir = temp_dir("chosen-dotdot-different");
        let a = dir.join("a");
        let b = dir.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let recorded = touch(&a, "meeting.wav");
        touch(&b, "meeting.wav");
        let chosen = ChosenFiles::default();
        chosen.record(&recorded).unwrap();

        // Spelled from `a`, but `..` walks it back out to the file in `b`,
        // which was never recorded.
        let other_file = a.join("..").join("b").join("meeting.wav");
        assert_eq!(chosen.take(&other_file), None);
    }

    #[test]
    fn a_path_that_cannot_be_canonicalised_is_refused() {
        let dir = temp_dir("chosen-missing");
        let missing = dir.join("nothing-here.wav");

        assert_eq!(ChosenFiles::default().take(&missing), None);
        assert!(ChosenFiles::default().record(&missing).is_err());
    }

    #[test]
    fn recording_keeps_only_the_most_recent_few_paths() {
        let dir = temp_dir("chosen-bounded");
        let chosen = ChosenFiles::default();
        let paths: Vec<PathBuf> = (0..MAX_CHOSEN + 2)
            .map(|n| touch(&dir, &format!("meeting-{n}.wav")))
            .collect();
        for path in &paths {
            chosen.record(path).unwrap();
        }

        assert_eq!(chosen.take(&paths[0]), None, "the oldest was dropped");
        assert!(chosen.take(paths.last().unwrap()).is_some());
    }

    #[test]
    fn clearing_forgets_a_recorded_path() {
        let dir = temp_dir("chosen-cleared");
        let path = touch(&dir, "meeting.wav");
        let chosen = ChosenFiles::default();
        chosen.record(&path).unwrap();

        chosen.clear();

        assert_eq!(chosen.take(&path), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_path_and_the_real_path_it_resolves_to_compare_equal() {
        let dir = temp_dir("chosen-symlink");
        let real_dir = dir.join("real");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real_file = touch(&real_dir, "meeting.wav");
        let link_dir = dir.join("link");
        std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();
        let via_link = link_dir.join("meeting.wav");

        // Recorded through the symlink, taken by the real path.
        let chosen = ChosenFiles::default();
        chosen.record(&via_link).unwrap();
        assert!(chosen.take(&real_file).is_some());

        // And the reverse: recorded by the real path, taken through the
        // symlink.
        chosen.record(&real_file).unwrap();
        assert!(chosen.take(&via_link).is_some());
    }

    #[test]
    fn a_drop_or_a_pick_of_several_is_one_choice_so_a_big_batch_keeps_its_first_files() {
        let dir = temp_dir("chosen-batch");
        let chosen = ChosenFiles::default();
        let batch: Vec<PathBuf> = (0..20)
            .map(|n| touch(&dir, &format!("part-{n}.wav")))
            .collect();
        chosen.record_dropped(&batch);
        // More choices after it, short of pushing the batch out.
        for n in 0..MAX_CHOSEN - 1 {
            chosen
                .record(&touch(&dir, &format!("later-{n}.wav")))
                .unwrap();
        }

        for path in &batch {
            assert!(chosen.take(path).is_some(), "{path:?} was recorded");
        }
        assert_eq!(chosen.take(&batch[0]), None, "and each is taken once");
    }

    #[test]
    fn a_drop_records_every_path_it_can_and_leaves_out_one_that_is_not_there() {
        let dir = temp_dir("chosen-drop-missing");
        let there = touch(&dir, "there.wav");
        let missing = dir.join("gone.wav");
        let chosen = ChosenFiles::default();

        let dropped = [missing.clone(), there.clone()];
        let recorded = chosen.record_all(&dropped);

        assert_eq!(recorded, [there.as_path()]);
        assert_eq!(chosen.take(&missing), None);
        assert!(chosen.take(&there).is_some());
    }

    #[test]
    fn a_path_in_a_batch_that_was_never_chosen_is_refused_while_the_chosen_ones_are_taken() {
        let dir = temp_dir("chosen-batch-mixed");
        let chosen_one = touch(&dir, "chosen.wav");
        let never = touch(&dir, "never.wav");
        let chosen = ChosenFiles::default();
        chosen.record_dropped(std::slice::from_ref(&chosen_one));

        assert_eq!(chosen.take(&never), None);
        assert!(chosen.take(&chosen_one).is_some());
    }

    /// An engine like [`OneLine`] that counts how many of it were made and
    /// how many streams it opened, to show a batch loads no second engine.
    struct Counted {
        streams: Arc<AtomicUsize>,
    }

    impl Counted {
        fn new(instances: &AtomicUsize, streams: &Arc<AtomicUsize>) -> Self {
            instances.fetch_add(1, Ordering::SeqCst);
            Self {
                streams: Arc::clone(streams),
            }
        }
    }

    impl SttEngine for Counted {
        fn id(&self) -> &'static str {
            "counted"
        }
        fn caps(&self) -> EngineCaps {
            OneLine { silent: false }.caps()
        }
        fn stream(&self, ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            self.streams.fetch_add(1, Ordering::SeqCst);
            OneLine { silent: false }.stream(ctx)
        }
        fn transcribe(
            &self,
            _audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            unreachable!("a file streams to an engine that segments for itself")
        }
    }

    /// Saves into `dir` as the app saves into Downloads.
    fn saving_into(dir: &Path) -> impl FnMut(&str, &str, &str) -> std::io::Result<PathBuf> + '_ {
        move |name, extension, text| save_new(dir, name, "2026-09-25", extension, text)
    }

    fn states(batch: &Batch) -> Vec<&'static str> {
        batch.view().rows.iter().map(|r| r.state).collect()
    }

    fn batch_of(paths: &[Option<PathBuf>]) -> Batch {
        Batch::new(
            1,
            paths
                .iter()
                .map(|p| p.as_deref().map(file_name).unwrap_or_default())
                .collect(),
        )
    }

    #[test]
    fn a_batch_hears_each_file_on_one_engine_and_saves_each_as_it_finishes() {
        let dir = temp_dir("batch-files");
        let out = temp_dir("batch-out");
        let first = dir.join("standup.wav");
        let second = dir.join("retro.wav");
        write_wav(&first);
        write_wav(&second);
        let paths = [Some(first), Some(vtt("batch-vtt")), Some(second)];
        let (instances, streams) = (AtomicUsize::new(0), Arc::new(AtomicUsize::new(0)));
        let session = Session::with_engine(
            Box::new(Counted::new(&instances, &streams)),
            Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );
        // Every loan, to show each file was heard on the one instance.
        let loans = std::sync::Mutex::new(Vec::new());
        let lend = || {
            let tools = session.meeting_tools().ok()?;
            loans.lock().unwrap().push(tools.clone());
            Some(tools)
        };
        let mut batch = batch_of(&paths);
        // What was on disk when each row finished: a file is saved before the
        // next one starts.
        let mut saved_by_then = Vec::new();

        run_batch(
            &mut batch,
            &paths,
            &FORMATS[0],
            &mut saving_into(&out),
            &lend,
            &holds,
            &mut |b, index| {
                if b.rows[index].state == RowState::Done {
                    saved_by_then.push(std::fs::read_dir(&out).unwrap().count());
                }
            },
            &quiet(),
        );

        assert_eq!(states(&batch), ["done", "done", "done"]);
        assert_eq!(saved_by_then, [1, 2, 3]);
        assert_eq!(instances.load(Ordering::SeqCst), 1);
        let loans = loans.into_inner().unwrap();
        assert!(loans.len() >= 3, "pinned, then lent for each recording");
        let pin = loans[0].pin();
        assert!(
            loans.iter().all(|tools| pin.lends(tools)),
            "every file saw the same engine instance"
        );
        assert_eq!(streams.load(Ordering::SeqCst), 2, "one per recording");
        let saved: Vec<String> = batch
            .rows
            .iter()
            .map(|r| std::fs::read_to_string(r.saved.as_ref().unwrap()).unwrap())
            .collect();
        assert_eq!(
            saved,
            [
                "We ship on friday",
                "Aoife Byrne: We ship on Friday.",
                "We ship on friday"
            ]
        );
        assert_eq!(
            batch.rows[0].saved.as_ref().unwrap().file_name().unwrap(),
            "standup transcript 2026-09-25.txt"
        );
        let view = batch.view();
        assert_eq!(
            view.rows[1].status,
            crate::meeting_words::batch_saved(&view.rows[1].saved.clone().unwrap())
        );
    }

    #[test]
    fn a_batch_saves_as_webvtt_that_reads_back_as_the_same_turns() {
        let out = temp_dir("batch-vtt-out");
        let paths = [Some(vtt("batch-vtt-in"))];
        let mut batch = batch_of(&paths);
        let vtt = format_by_extension("vtt").expect("offered");

        run_batch(
            &mut batch,
            &paths,
            vtt,
            &mut saving_into(&out),
            &nothing_lent,
            &holds,
            &mut |_, _| {},
            &quiet(),
        );

        let saved = batch.rows[0].saved.clone().expect("saved");
        assert_eq!(saved.extension().unwrap(), "vtt");
        let Ok(MeetingFile::Transcript(back)) = meeting::open_file(&saved) else {
            panic!("the saved file reads back as a transcript");
        };
        let turns: Vec<(Option<&str>, u64, u64, &str)> = back
            .turns
            .iter()
            .map(|t| (t.speaker.as_deref(), t.start_ms, t.end_ms, t.text.as_str()))
            .collect();
        assert_eq!(
            turns,
            [(Some("Aoife Byrne"), 1_000, 3_000, "We ship on Friday.")]
        );
    }

    #[test]
    fn one_list_offers_text_webvtt_srt_and_markdown_in_that_order() {
        let offered: Vec<(&str, &str)> = FORMATS.iter().map(|f| (f.extension, f.label)).collect();
        assert_eq!(
            offered,
            [
                ("txt", crate::meeting_words::FORMAT_TEXT),
                ("vtt", crate::meeting_words::FORMAT_VTT),
                ("srt", crate::meeting_words::FORMAT_SRT),
                ("md", crate::meeting_words::FORMAT_MARKDOWN),
            ]
        );
        assert!(format_by_extension("markdown").is_none());
        assert!(format_by_extension(".md").is_none());
        // The page's list is this one, labels and all.
        let words = meeting_live_words();
        let page: Vec<(&str, &str)> = words
            .formats
            .iter()
            .map(|f| (f.extension, f.label))
            .collect();
        assert_eq!(page, offered);
    }

    #[test]
    fn a_batch_can_save_in_every_format_the_card_offers() {
        for format in FORMATS {
            let out = temp_dir(&format!("batch-every-{}-out", format.extension));
            let paths = [Some(vtt(&format!("batch-every-{}-in", format.extension)))];
            let mut batch = batch_of(&paths);

            run_batch(
                &mut batch,
                &paths,
                format_by_extension(format.extension).expect("offered"),
                &mut saving_into(&out),
                &nothing_lent,
                &holds,
                &mut |_, _| {},
                &quiet(),
            );

            let saved = batch.rows[0].saved.clone().expect("saved");
            assert_eq!(saved.extension().unwrap(), format.extension);
            let written = std::fs::read_to_string(&saved).unwrap();
            assert!(
                written.contains("We ship on Friday."),
                "{}: {written:?}",
                format.extension
            );
        }
    }

    #[test]
    fn one_file_failing_fails_its_own_row_and_the_rest_carry_on() {
        let dir = temp_dir("batch-failing");
        let out = temp_dir("batch-failing-out");
        let broken = dir.join("broken.wav");
        std::fs::write(&broken, b"RIFF not really").unwrap();
        let paths = [Some(broken), None, Some(vtt("batch-after-failure"))];
        let mut batch = batch_of(&paths);

        run_batch(
            &mut batch,
            &paths,
            &FORMATS[0],
            &mut saving_into(&out),
            &lent,
            &holds,
            &mut |_, _| {},
            &quiet(),
        );

        assert_eq!(states(&batch), ["failed", "failed", "done"]);
        let view = batch.view();
        assert!(!view.rows[0].status.is_empty(), "says why in a sentence");
        assert_eq!(view.rows[1].status, NOT_CHOSEN);
        batch.running = false;
        assert_eq!(
            batch.view().summary,
            crate::meeting_words::batch_ended(1, 3)
        );
    }

    #[test]
    fn a_file_that_panics_fails_its_row_lets_go_of_the_model_and_the_rest_carry_on() {
        let dir = temp_dir("batch-panic");
        let out = temp_dir("batch-panic-out");
        let (first, second) = (dir.join("first.wav"), dir.join("second.wav"));
        write_wav(&first);
        write_wav(&second);
        let paths = [Some(first), Some(second)];
        let lent_count = AtomicUsize::new(0);
        let session = one_session();
        // The batch's first loan pins the engine; the first file's second
        // loan, the one it hears on, panics.
        let lend = || {
            if lent_count.fetch_add(1, Ordering::SeqCst) == 2 {
                panic!("the engine fell over");
            }
            session.meeting_tools().ok()
        };
        let marks = std::sync::Mutex::new(Vec::new());
        let hold = |holds: bool| {
            marks.lock().unwrap().push(holds);
            true
        };
        let mut batch = batch_of(&paths);

        run_batch(
            &mut batch,
            &paths,
            &FORMATS[0],
            &mut saving_into(&out),
            &lend,
            &hold,
            &mut |_, _| {},
            &quiet(),
        );

        assert_eq!(states(&batch), ["failed", "done"]);
        assert_eq!(
            batch.rows[0].why.as_deref(),
            Some(crate::meeting_words::BATCH_WENT_WRONG)
        );
        assert_eq!(*marks.lock().unwrap(), [true, false, true, false]);
    }

    #[test]
    fn stop_stops_the_file_being_heard_and_every_one_waiting_and_what_was_saved_stays() {
        let dir = temp_dir("batch-stop");
        let out = temp_dir("batch-stop-out");
        let (second, third) = (dir.join("second.wav"), dir.join("third.wav"));
        write_wav(&second);
        write_wav(&third);
        let paths = [Some(vtt("batch-stop-first")), Some(second), Some(third)];
        let session = one_session();
        let mut batch = batch_of(&paths);
        let stop = quiet();

        run_batch(
            &mut batch,
            &paths,
            &FORMATS[0],
            &mut saving_into(&out),
            &|| session.meeting_tools().ok(),
            &holds,
            // Stop is pressed as the second file opens.
            &mut |b, index| {
                if index == 1 && b.rows[1].state == RowState::Opening {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            &stop,
        );

        assert_eq!(states(&batch), ["done", "stopped", "stopped"]);
        assert!(batch.rows[0].saved.as_ref().unwrap().exists());
        assert_eq!(
            batch.rows[1].why.as_deref(),
            Some("Stopped. Nothing from this file was kept.")
        );
        assert_eq!(
            batch.rows[2].why.as_deref(),
            Some(crate::meeting_words::BATCH_NOT_STARTED)
        );
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);
    }

    #[test]
    fn a_batch_never_overwrites_a_file_already_there_nor_one_of_its_own() {
        let a = temp_dir("batch-same-name-a");
        let b = temp_dir("batch-same-name-b");
        let out = temp_dir("batch-same-name-out");
        let existing = out.join("meeting transcript 2026-09-25.txt");
        std::fs::write(&existing, "keep me").unwrap();
        let text = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHello.\n";
        let (first, second) = (a.join("meeting.vtt"), b.join("meeting.vtt"));
        std::fs::write(&first, text).unwrap();
        std::fs::write(&second, text).unwrap();
        let paths = [Some(first), Some(second)];
        let mut batch = batch_of(&paths);

        run_batch(
            &mut batch,
            &paths,
            &FORMATS[0],
            &mut saving_into(&out),
            &nothing_lent,
            &holds,
            &mut |_, _| {},
            &quiet(),
        );

        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "keep me");
        let names: Vec<String> = batch
            .rows
            .iter()
            .map(|r| {
                r.saved
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            [
                "meeting transcript 2026-09-25-2.txt",
                "meeting transcript 2026-09-25-3.txt"
            ]
        );
    }

    #[test]
    fn a_transcript_that_cannot_be_saved_fails_its_row_with_a_sentence() {
        let paths = [Some(vtt("batch-unsaved"))];
        let mut batch = batch_of(&paths);

        run_batch(
            &mut batch,
            &paths,
            &FORMATS[0],
            &mut |_, _, _| Err(std::io::Error::other("disk full")),
            &nothing_lent,
            &holds,
            &mut |_, _| {},
            &quiet(),
        );

        assert_eq!(states(&batch), ["failed"]);
        assert_eq!(
            batch.view().rows[0].status,
            crate::meeting_words::BATCH_NOT_SAVED
        );
    }

    #[test]
    fn a_batch_opens_only_the_paths_that_were_chosen_and_each_only_once() {
        let dir = temp_dir("batch-paths");
        let chosen_one = touch(&dir, "chosen.wav");
        let never = touch(&dir, "never.wav");
        let as_given = |path: &Path| path.display().to_string();
        let chosen = ChosenFiles::default();
        chosen.record_dropped(std::slice::from_ref(&chosen_one));

        let taken = batch_paths(&chosen, &[as_given(&chosen_one), as_given(&never)]).unwrap();
        assert_eq!(taken, [Some(chosen_one.canonicalize().unwrap()), None]);

        assert_eq!(
            batch_paths(&chosen, &[as_given(&never)]),
            Err(NOT_CHOSEN),
            "a batch with no chosen path is refused whole"
        );

        chosen.record_dropped(std::slice::from_ref(&chosen_one));
        let twice = batch_paths(&chosen, &[as_given(&chosen_one), as_given(&chosen_one)]).unwrap();
        assert_eq!(
            twice,
            [Some(chosen_one.canonicalize().unwrap()), None],
            "a path chosen once is opened once"
        );
    }

    #[test]
    fn a_dropped_folder_is_not_recorded_nor_handed_to_the_page() {
        let dir = temp_dir("dropped-folder");
        let folder = dir.join("recordings");
        std::fs::create_dir_all(&folder).unwrap();
        let file = touch(&dir, "meeting.wav");
        let chosen = ChosenFiles::default();

        // What the page hears of a drop of the folder and the file: the file
        // alone, so it opens as one file and no batch of two starts.
        let for_page = chosen.record_dropped(&[folder.clone(), file.clone()]);
        assert_eq!(for_page, [file.display().to_string()]);
        assert!(chosen.take(&file).is_some());

        let dropped = [folder.clone(), file.clone()];
        let recorded = chosen.record_all(&dropped);

        assert_eq!(recorded, [file.as_path()]);
        assert_eq!(chosen.take(&folder), None);
    }

    #[test]
    fn a_busy_page_refuses_a_batch_before_it_takes_anything() {
        let meetings = Meetings::default();
        assert!(meetings.refuse_if_busy().is_ok());
        let (job, _) = meetings.begin().unwrap();
        assert_eq!(meetings.refuse_if_busy(), Err(CommandError::from(BUSY)));
        meetings.end(job, None);
        let (live, _) = meetings.begin_live().unwrap();
        assert_eq!(
            meetings.refuse_if_busy(),
            Err(CommandError::from(crate::meeting_words::LIVE_BUSY))
        );
        meetings.end(live, None);
    }

    /// How the engine changes part of the way through a test batch, once
    /// its first file is saved.
    enum Switch {
        /// The old session is closed and the new one has not opened yet:
        /// nothing is lent, as while the app changes engine.
        Opening,
        /// Another engine is open.
        To(Box<Session>),
    }

    /// A batch of two recordings in which the engine changes as `switch`
    /// says once the first is saved.
    fn batch_across_a_switch(name: &str, switch: Switch) -> Batch {
        let dir = temp_dir(name);
        let out = temp_dir(&format!("{name}-out"));
        let (first, second) = (dir.join("first.wav"), dir.join("second.wav"));
        write_wav(&first);
        write_wav(&second);
        let paths = [Some(first), Some(second)];
        let open = std::sync::Mutex::new(Some(one_session()));
        let mut next = Some(switch);
        let lend = || open.lock().unwrap().as_ref()?.meeting_tools().ok();
        let mut batch = batch_of(&paths);

        run_batch(
            &mut batch,
            &paths,
            &FORMATS[0],
            &mut saving_into(&out),
            &lend,
            &holds,
            &mut |b, index| {
                if index == 0 && b.rows[0].state == RowState::Done {
                    // The old session goes, and its engine with it.
                    *open.lock().unwrap() = match next.take() {
                        Some(Switch::To(session)) => Some(*session),
                        _ => None,
                    };
                }
            },
            &quiet(),
        );
        batch
    }

    #[test]
    fn a_recording_left_while_the_new_engine_is_still_opening_says_the_engine_changed() {
        let batch = batch_across_a_switch("batch-engine-opening", Switch::Opening);

        assert_eq!(states(&batch), ["done", "failed"]);
        assert_eq!(
            batch.rows[1].why.as_deref(),
            Some(crate::meeting_words::BATCH_ENGINE_CHANGED),
            "not that no engine is ready"
        );
    }

    #[test]
    fn a_recording_left_when_the_engine_changes_is_not_heard_on_the_new_one() {
        let other = Session::with_engine(
            Box::new(Counted::new(
                &AtomicUsize::new(0),
                &Arc::new(AtomicUsize::new(0)),
            )),
            Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );

        let batch = batch_across_a_switch("batch-engine-changed", Switch::To(Box::new(other)));

        assert_eq!(states(&batch), ["done", "failed"]);
        assert_eq!(
            batch.rows[1].why.as_deref(),
            Some(crate::meeting_words::BATCH_ENGINE_CHANGED)
        );
    }

    #[test]
    fn a_session_reopened_on_the_same_engine_carries_the_batch_on() {
        let batch =
            batch_across_a_switch("batch-engine-reopened", Switch::To(Box::new(one_session())));

        assert_eq!(states(&batch), ["done", "done"]);
    }

    #[test]
    fn a_batch_whose_thread_panics_still_ends_and_the_page_is_not_left_busy() {
        let meetings = Meetings::default();
        let names = vec!["a.wav".to_string(), "b.wav".to_string()];
        let (job, _) = meetings.begin_batch(names.clone()).unwrap();
        let mut batch = Batch::new(job, names);
        batch.set(0, RowState::Done, None);
        batch.set(1, RowState::Hearing { done: 1, of: 2 }, None);
        meetings.batch_changed(&batch);

        let ended = finish_batch(&meetings, job, || panic!("saving fell over")).unwrap();

        assert!(!ended.running);
        let states: Vec<&str> = ended.rows.iter().map(|r| r.state).collect();
        assert_eq!(states, ["done", "failed"]);
        assert_eq!(ended.rows[1].status, crate::meeting_words::BATCH_WENT_WRONG);
        assert!(meetings.begin().is_ok(), "the page is not left busy");
    }

    #[test]
    fn a_batch_is_one_job_that_keeps_its_rows_after_it_ends_until_cleared() {
        let meetings = Meetings::default();
        let (first, _) = meetings.begin().unwrap();
        meetings.end(first, Some(view(first)));

        let (job, stop) = meetings
            .begin_batch(vec!["a.wav".into(), "b.wav".into()])
            .unwrap();
        assert_eq!(
            meetings.begin().map(|(job, _)| job),
            Err(CommandError::from(BUSY)),
            "a single file waits for the batch"
        );
        assert!(meetings.begin_summary().is_err(), "so does a summary");
        meetings.clear_batch();
        assert!(meetings.batch().is_some(), "a running batch is not cleared");

        let mut batch = Batch::new(job, vec!["a.wav".into(), "b.wav".into()]);
        batch.set(0, RowState::Done, None);
        let changed = meetings.batch_changed(&batch).unwrap();
        assert_eq!(changed.summary, crate::meeting_words::batch_working(1, 2));

        meetings.stop();
        assert!(stop.load(Ordering::Relaxed));
        let stopping = meetings.batch_changed(&batch).unwrap();
        assert!(stopping.stopping, "a Stop pressed is kept through changes");
        assert_eq!(stopping.summary, crate::meeting_words::BATCH_STOPPING);

        let ended = meetings.end_batch(job).unwrap();
        assert!(!ended.running);
        assert_eq!(ended.summary, crate::meeting_words::batch_ended(1, 2));
        assert_eq!(
            meetings.current().map(|v| v.job),
            Some(first),
            "the current meeting is left as it was"
        );
        assert!(meetings.begin().is_ok(), "and the job is over");
        meetings.clear_batch();
        assert_eq!(meetings.batch(), None);
    }

    /// A live source: nothing when the meeting's clock starts, then
    /// `samples` at once, then `after`: nothing more, or a failure, and
    /// `then` is called once as it runs dry.
    struct Canned {
        samples: Vec<f32>,
        polls: usize,
        after: After,
        then: Option<Box<dyn FnOnce() + Send>>,
    }

    enum After {
        Quiet,
        Lost,
        Panics,
    }

    impl Canned {
        fn new(seconds: usize, after: After) -> Self {
            Self {
                samples: vec![0.1; seconds * 16_000],
                polls: 0,
                after,
                then: None,
            }
        }
        fn then(mut self, then: impl FnOnce() + Send + 'static) -> Self {
            self.then = Some(Box::new(then));
            self
        }
    }

    impl Source for Canned {
        fn start(&mut self) -> macalla_session::Result<()> {
            Ok(())
        }
        fn poll_into(&mut self, out: &mut Vec<f32>) -> macalla_session::Result<usize> {
            self.polls += 1;
            match self.polls {
                1 => Ok(0),
                2 => {
                    let n = self.samples.len();
                    out.append(&mut self.samples);
                    if let Some(then) = self.then.take() {
                        then();
                    }
                    Ok(n)
                }
                _ => match self.after {
                    After::Quiet => Ok(0),
                    After::Lost => {
                        Err(macalla_platform::Error::DeviceLost("USB Microphone".into()).into())
                    }
                    After::Panics => panic!("the tap fell over"),
                },
            }
        }
        fn finish_into(self: Box<Self>, _out: &mut Vec<f32>) -> macalla_session::Result<usize> {
            Ok(0)
        }
    }

    /// Opens `you` and, when there is one, `others` as the call's sound, or
    /// with `unavailable` as why there is none.
    fn opening(
        you: Canned,
        others: Result<Canned, macalla_platform::system_audio::OpenError>,
    ) -> impl FnMut() -> macalla_session::Result<(Box<dyn Source>, CallAudio)> {
        let mut sources = Some((you, others));
        move || {
            let (you, others) = sources.take().expect("opened once");
            Ok((Box::new(you), live_meeting::call_audio(others)))
        }
    }

    #[test]
    fn a_live_meeting_becomes_the_current_meeting_with_its_speakers_shown_through_the_catalogue() {
        let ending = Arc::new(live_meeting::Ending::new());
        let stop = {
            let ending = Arc::clone(&ending);
            move || ending.stop()
        };
        let mut open = opening(
            Canned::new(2, After::Quiet),
            Ok(Canned::new(2, After::Quiet).then(stop)),
        );
        let mut seen = Vec::new();

        let view = run_live(
            11,
            &lent,
            &mut open,
            &holds,
            &mut |_| {},
            &mut |p| seen.push(p),
            &ending,
        )
        .unwrap();

        assert_eq!(view.job, 11);
        assert_eq!(view.name, crate::meeting_words::NAME);
        assert!(view.named_speakers);
        assert_eq!(view.ended_early, None, "stopped by the person");
        let speakers: Vec<(Option<&str>, Option<&str>)> = view
            .turns
            .iter()
            .map(|t| (t.speaker.as_deref(), t.label.as_deref()))
            .collect();
        assert_eq!(
            speakers,
            [
                (Some(live_meeting::YOU), Some("You")),
                (Some(live_meeting::OTHERS), Some("Others")),
            ]
        );
        assert!(seen
            .last()
            .is_some_and(|p| p.job == 11 && p.others.is_some()));
        // Copy and Save as text carry the speakers as data.
        assert!(transcript_text(&view).starts_with("You: "));
    }

    #[test]
    fn a_live_meeting_without_the_calls_sound_hears_the_microphone_alone_and_says_why() {
        let ending = Arc::new(live_meeting::Ending::new());
        let stop = {
            let ending = Arc::clone(&ending);
            move || ending.stop()
        };
        let mut open = opening(
            Canned::new(2, After::Quiet).then(stop),
            Err(macalla_platform::system_audio::OpenError::Unsupported),
        );
        let mut seen = Vec::new();

        let view = run_live(
            12,
            &lent,
            &mut open,
            &holds,
            &mut |_| {},
            &mut |p| seen.push(p),
            &ending,
        )
        .unwrap();

        assert!(!view.named_speakers);
        assert_eq!(view.turns.len(), 1);
        assert_eq!(view.turns[0].label, None);
        let last = seen.last().unwrap();
        assert_eq!(last.others, None);
        assert_eq!(
            last.microphone_only.as_deref(),
            Some(crate::meeting_words::microphone_only(
                &macalla_platform::system_audio::OpenError::Unsupported
            ))
        );
    }

    #[test]
    fn a_live_meeting_that_ended_early_keeps_its_words_and_says_why_in_its_own_sentence() {
        // The microphone is lost, in a meeting that hears it alone; and in
        // another, the call's stream panics. Each keeps what it heard, and
        // says why with a sentence of its own that is never the
        // stopped-nothing-kept one. The engine here commits its words as a
        // stream finishes, so the stream that panics has none: the words
        // kept are the microphone's.
        let stopped = crate::say::say(&macalla_session::Error::Stopped);
        for (you, others, expected) in [
            (
                After::Lost,
                None,
                crate::meeting_words::ended_early(&macalla_session::Error::Platform(
                    macalla_platform::Error::DeviceLost("USB Microphone".into()),
                )),
            ),
            (
                After::Quiet,
                Some(After::Panics),
                crate::meeting_words::ended_early(&macalla_session::Error::StreamPanicked(
                    String::new(),
                )),
            ),
        ] {
            let ending = live_meeting::Ending::new();
            let others = others
                .map(|after| Canned::new(2, after))
                .ok_or_else(no_call);
            let mut open = opening(Canned::new(2, you), others);

            let view = run_live(
                13,
                &lent,
                &mut open,
                &holds,
                &mut |_| {},
                &mut |_| {},
                &ending,
            )
            .unwrap();

            assert_eq!(view.turns.len(), 1, "the words heard are kept");
            let said = view.ended_early.expect("it ended early");
            assert_eq!(said, expected);
            assert_ne!(said, stopped);
            assert!(
                said.ends_with("What was heard until then is kept."),
                "{said}"
            );
        }
    }

    /// Why a test meeting has no call's sound: it hears the microphone
    /// alone.
    fn no_call() -> macalla_platform::system_audio::OpenError {
        macalla_platform::system_audio::OpenError::Unsupported
    }

    #[test]
    fn a_discarded_live_meeting_keeps_nothing_and_says_so() {
        let ending = Arc::new(live_meeting::Ending::new());
        let discard = {
            let ending = Arc::clone(&ending);
            move || ending.cancel()
        };
        let mut open = opening(Canned::new(2, After::Quiet).then(discard), Err(no_call()));

        let result = run_live(
            14,
            &lent,
            &mut open,
            &holds,
            &mut |_| {},
            &mut |_| {},
            &ending,
        );

        assert_eq!(result, Err(crate::meeting_words::DISCARDED.to_string()));
    }

    #[test]
    fn a_live_meeting_with_no_engine_open_says_so_and_opens_nothing() {
        let mut open = || -> macalla_session::Result<(Box<dyn Source>, CallAudio)> {
            unreachable!("nothing is opened without an engine")
        };

        let result = run_live(
            15,
            &nothing_lent,
            &mut open,
            &holds,
            &mut |_| {},
            &mut |_| {},
            &live_meeting::Ending::new(),
        );

        assert_eq!(result, Err(crate::meeting_words::NO_ENGINE.to_string()));
    }

    #[test]
    fn a_live_meeting_and_a_file_refuse_each_other_with_a_sentence() {
        let meetings = Meetings::default();

        let (live, _) = meetings.begin_live().unwrap();
        assert_eq!(
            meetings.begin().map(|(job, _)| job),
            Err(CommandError::from(crate::meeting_words::LIVE_BUSY))
        );
        meetings.end(live, Some(view(live)));
        assert_eq!(
            meetings.begin_summary().map(|(job, _, _)| job).err(),
            None,
            "a summary may start once the meeting has ended"
        );

        let meetings = Meetings::default();
        let (file, _) = meetings.begin().unwrap();
        assert_eq!(
            meetings.begin_live().map(|(job, _)| job),
            Err(CommandError::from(BUSY))
        );
        meetings.end(file, None);
        assert!(meetings.begin_live().is_ok());
    }

    #[test]
    fn stop_keeps_a_live_meeting_and_a_quit_before_it_records_keeps_nothing() {
        let meetings = Meetings::default();
        let (job, ending) = meetings.begin_live().unwrap();
        // A stop is a keep: pressed while the meeting records, it still
        // hears its words to the end.
        let stop = {
            let meetings = meetings.clone();
            move || meetings.stop_live()
        };
        let heard = run_live(
            job,
            &lent,
            &mut opening(Canned::new(2, After::Quiet).then(stop), Err(no_call())),
            &holds,
            &mut |_| {},
            &mut |_| {},
            &ending,
        );
        assert!(meetings
            .running_live()
            .is_some_and(|r| r.job == job && r.stopping));
        assert!(heard.is_ok(), "{heard:?}");
        meetings.end(job, heard.ok());

        // Pressed before the meeting started, it starts nothing and keeps
        // nothing, as a meeting with no words does.
        let (job, ending) = meetings.begin_live().unwrap();
        meetings.stop_live();
        let early = run_live(
            job,
            &lent,
            &mut opening(Canned::new(2, After::Quiet), Err(no_call())),
            &holds,
            &mut |_| {},
            &mut |_| {},
            &ending,
        );
        assert_eq!(early, Err(crate::meeting_words::NOTHING_HEARD.to_string()));
        meetings.end(job, None);

        let (job, ending) = meetings.begin_live().unwrap();
        meetings.stop_now();
        assert!(!meetings.holding_model(job, true), "quitting holds nothing");
        let discarded = run_live(
            job,
            &lent,
            &mut opening(Canned::new(2, After::Quiet), Err(no_call())),
            &|holds| meetings.holding_model(job, holds),
            &mut |_| {},
            &mut |_| {},
            &ending,
        );
        assert_eq!(discarded, Err(crate::meeting_words::DISCARDED.to_string()));
    }

    #[test]
    fn the_call_is_said_to_be_silent_only_after_a_while_and_only_before_any_sound() {
        use macalla_session::live_meeting::SignalHealth;
        let quiet = SignalHealth {
            signal: false,
            since_signal_ms: None,
        };
        let heard = SignalHealth {
            signal: true,
            since_signal_ms: Some(60_000),
        };
        let at = |heard_ms, call| {
            live_progress(
                1,
                LiveProgress {
                    heard_ms,
                    you: heard,
                    others: call,
                },
                None,
            )
        };

        let early = at(NO_CALL_SOUND_AFTER_MS - 1, Some(quiet));
        assert!(!early.others_notice);
        let late = at(NO_CALL_SOUND_AFTER_MS, Some(quiet));
        assert!(late.others_notice);
        assert_eq!(
            late.others.as_deref(),
            Some(crate::meeting_words::NO_CALL_SOUND_YET)
        );
        let sounded = at(10 * NO_CALL_SOUND_AFTER_MS, Some(heard));
        assert!(!sounded.others_notice, "quiet after sound is only quiet");
        assert!(!at(10 * NO_CALL_SOUND_AFTER_MS, None).others_notice);
        assert_eq!(late.heard, "0:15 heard");
    }

    fn turn_view(speaker: Option<&str>, label: Option<&str>, start_ms: u64) -> TurnView {
        TurnView {
            speaker: speaker.map(str::to_string),
            label: label.map(str::to_string),
            start_ms,
            end_ms: start_ms + 1_000,
            text: "We ship the parser on Friday.".to_string(),
            edit_label: String::new(),
        }
    }

    #[test]
    fn each_quote_says_who_said_it_and_when_and_names_its_turn() {
        use macalla_core::meeting_summary::{ActionItem, PartLists, PartSummary, Point, Said};
        let said = |turn: usize, start_ms: u64| Said { turn, start_ms };
        let turns = [
            turn_view(Some("Aoife Byrne"), Some("Aoife Byrne"), 1_000),
            // A live meeting's speaker, shown through the catalogue.
            turn_view(
                Some(macalla_session::live_meeting::YOU),
                Some(&crate::meeting_words::speaker(
                    macalla_session::live_meeting::YOU,
                )),
                158_000,
            ),
            // A recording heard here: no speaker.
            turn_view(None, None, 3_725_000),
        ];
        let parts = vec![PartSummary {
            start_ms: 0,
            end_ms: 3_726_000,
            lists: Ok(PartLists {
                key_points: vec![Point {
                    text: "Friday".to_string(),
                    quote: "We ship the parser on Friday".to_string(),
                    said: said(0, 1_000),
                }],
                decisions: vec![Point {
                    text: "Friday".to_string(),
                    quote: "We ship the parser on Friday".to_string(),
                    said: said(2, 3_725_000),
                }],
                action_items: vec![ActionItem {
                    text: "Ship it".to_string(),
                    quote: "We ship the parser on Friday".to_string(),
                    owner: Some("You".to_string()),
                    said: said(1, 158_000),
                }],
                dropped: Vec::new(),
                assumed_empty: 0,
            }),
        }];

        let views = part_views(parts, &turns);

        let part = &views[0];
        assert_eq!(
            (part.key_points[0].turn, part.key_points[0].said.as_str()),
            (
                0,
                crate::meeting_words::quote_said_by("Aoife Byrne", "0:01").as_str()
            )
        );
        assert_eq!(
            (
                part.action_items[0].turn,
                part.action_items[0].said.as_str()
            ),
            (
                1,
                crate::meeting_words::quote_said_by("You", "2:38").as_str()
            )
        );
        assert_eq!(
            (part.decisions[0].turn, part.decisions[0].said.as_str()),
            (2, crate::meeting_words::quote_said_at("1:02:05").as_str())
        );
    }

    #[test]
    fn a_meeting_clock_counts_hours_once_there_are_any() {
        assert_eq!(clock(59_999), "0:59");
        assert_eq!(clock(3_725_000), "1:02:05");
    }

    /// What a live meeting's thread saw of the microphone while it ran.
    fn refused_while_running(microphone: &MicrophoneOwner) -> bool {
        microphone.for_dictation().err() == Some(crate::meeting_words::DICTATION_DURING_MEETING)
    }

    #[test]
    fn a_live_meeting_holds_the_microphone_while_it_runs_and_gives_it_back_however_it_ends() {
        let outcomes: [fn() -> Result<MeetingView, String>; 3] = [
            || Ok(view(0)),
            || Err("no".to_string()),
            || panic!("the meeting fell over"),
        ];
        for outcome in outcomes {
            let meetings = Meetings::default();
            let microphone = MicrophoneOwner::default();
            let mut held = None;
            let mut told = None;

            let job = start_live(&meetings, &microphone, |job, _ending, claim| {
                let (meetings, microphone) = (meetings.clone(), microphone.clone());
                let held = &mut held;
                let told = &mut told;
                live_thread(
                    &meetings,
                    job,
                    claim,
                    || {
                        *held = Some(refused_while_running(&microphone));
                        outcome()
                    },
                    |ending| *told = Some(matches!(ending, Ending::Error(_))),
                );
                Ok(None)
            })
            .unwrap();

            assert_eq!(held, Some(true), "dictation is refused while it records");
            assert!(told.is_some(), "the page heard it end");
            assert!(
                microphone.for_dictation().is_ok(),
                "given back after job {job}"
            );
            assert!(meetings.begin().is_ok(), "and the job is over");
        }
    }

    #[test]
    fn a_live_meeting_that_cannot_begin_or_is_refused_takes_nothing_and_leaves_no_job() {
        // Another job runs: nothing is claimed, and nothing is spawned.
        let meetings = Meetings::default();
        let microphone = MicrophoneOwner::default();
        let (file, _) = meetings.begin().unwrap();
        let refused = start_live(&meetings, &microphone, |_, _, _| {
            unreachable!("nothing starts while a file is worked on")
        });
        assert_eq!(refused, Err(CommandError::from(BUSY)));
        assert!(
            microphone.for_dictation().is_ok(),
            "the microphone was never taken"
        );
        meetings.end(file, None);

        // A dictation has the microphone: the job it began is ended again.
        let dictation = microphone.for_dictation().unwrap();
        let refused = start_live(&meetings, &microphone, |_, _, _| {
            unreachable!("nothing starts under a dictation")
        });
        assert_eq!(
            refused,
            Err(CommandError::from(
                crate::meeting_words::MEETING_DURING_DICTATION
            ))
        );
        assert!(meetings.begin().is_ok(), "no job was left running");
        drop(dictation);
    }

    #[test]
    fn a_live_meeting_whose_thread_cannot_start_gives_everything_back() {
        let meetings = Meetings::default();
        let microphone = MicrophoneOwner::default();

        let result = start_live(&meetings, &microphone, |_, _, claim| {
            drop(claim);
            Err(std::io::Error::other("no threads left"))
        });

        assert_eq!(
            result,
            Err(CommandError::from(crate::meeting_words::COULD_NOT_START))
        );
        assert!(microphone.for_dictation().is_ok());
        assert!(meetings.begin().is_ok());
    }

    /// Streams that commit "we ship on friday" at their first poll, and
    /// "the last words" at their finish, which takes `finish_takes`: the
    /// last piece of a meeting, still being heard when the app quits.
    #[derive(Clone, Copy)]
    struct LastWords {
        finish_takes: std::time::Duration,
    }

    struct LastWordsStream {
        finish_takes: std::time::Duration,
        polled: bool,
    }

    impl SttStream for LastWordsStream {
        fn push(&mut self, _samples: &[f32]) -> macalla_core::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> macalla_core::Result<Vec<Transcript>> {
            if std::mem::replace(&mut self.polled, true) {
                return Ok(Vec::new());
            }
            Ok(vec![Transcript {
                text: "we ship on friday".into(),
                is_final: true,
                start_ms: 0,
                end_ms: 1_000,
            }])
        }
        fn finish(self: Box<Self>) -> macalla_core::Result<Vec<Transcript>> {
            std::thread::sleep(self.finish_takes);
            Ok(vec![Transcript {
                text: "the last words".into(),
                is_final: true,
                start_ms: 1_000,
                end_ms: 2_000,
            }])
        }
    }

    impl SttEngine for LastWords {
        fn id(&self) -> &'static str {
            "last-words"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps {
                streaming: true,
                owns_segmentation: true,
                concurrent_streams: true,
                ..EngineCaps::batch()
            }
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Ok(Box::new(LastWordsStream {
                finish_takes: self.finish_takes,
                polled: false,
            }))
        }
        fn transcribe(
            &self,
            _audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            unreachable!("a live meeting streams to an engine that segments for itself")
        }
    }

    /// A live meeting recording on its own thread, as the app runs one, on
    /// `engine`, from a microphone that never runs dry: only a stop ends
    /// it. Returns once it has recorded for a moment.
    fn recording(meetings: &Meetings, engine: LastWords) -> u64 {
        let microphone = MicrophoneOwner::default();
        let job = start_live(meetings, &microphone, |job, ending, claim| {
            let meetings = meetings.clone();
            std::thread::Builder::new()
                .spawn(move || {
                    let lend = || {
                        Session::with_engine(
                            Box::new(engine),
                            Dictionary::from_json("{}").unwrap(),
                            "none",
                            Config::default(),
                        )
                        .meeting_tools()
                        .ok()
                    };
                    let mut open = opening(Canned::new(2, After::Quiet), Err(no_call()));
                    live_thread(
                        &meetings,
                        job,
                        claim,
                        || {
                            run_live(
                                job,
                                &lend,
                                &mut open,
                                &|holds| meetings.holding_model(job, holds),
                                &mut |committed| meetings.live_ready(job, committed),
                                &mut |event| meetings.live_progressed(job, event),
                                &ending,
                            )
                        },
                        |_| {},
                    );
                })
                .map(Some)
        })
        .unwrap();
        // The first progress comes once the meeting has started and heard
        // for a second.
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while meetings.running_live().and_then(|r| r.progress).is_none() {
            assert!(Instant::now() < deadline, "the meeting never started");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        job
    }

    #[test]
    fn quitting_during_a_live_meeting_holds_its_words_and_offers_them_back_until_cleared() {
        let dir = temp_dir("quit-held");
        let meetings = Meetings::default();
        recording(
            &meetings,
            LastWords {
                finish_takes: std::time::Duration::ZERO,
            },
        );

        meetings.stop_now();
        assert!(meetings.finish_for_quit(
            Instant::now() + std::time::Duration::from_secs(5),
            Some(&dir)
        ));
        let held = held_files(&dir);
        assert_eq!(held.len(), 1, "{held:?}");
        let path = held[0].path.clone();
        assert!(!held[0].last_part_missing);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("WEBVTT"), "{written}");
        assert!(written.contains("We ship on friday") && written.contains("The last words"));

        // The next launch offers it back as the current meeting.
        let next = Meetings::default();
        next.offer_held(&dir);
        let offered = next.current().expect("offered back");
        assert_eq!(offered.name, crate::meeting_words::NAME);
        assert_eq!(
            offered.held.as_deref(),
            Some(crate::meeting_words::HELD_FROM_QUIT)
        );
        assert_eq!(offered.ended_early, None, "it finished in time");
        let texts: Vec<&str> = offered.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["We ship on friday The last words"]);

        // Cleared, as the page's Clear does: the file goes, and so does the
        // offer.
        next.let_go_of_held();
        next.clear();
        assert!(!path.exists(), "let go once the person decided");
        let after = Meetings::default();
        after.offer_held(&dir);
        assert_eq!(after.current(), None);
    }

    #[test]
    fn a_quit_whose_last_piece_does_not_finish_in_time_holds_what_was_committed_and_says_so() {
        let dir = temp_dir("quit-last-part");
        let meetings = Meetings::default();
        recording(
            &meetings,
            LastWords {
                finish_takes: std::time::Duration::from_secs(2),
            },
        );

        meetings.stop_now();
        assert!(
            !meetings.finish_for_quit(
                Instant::now() + std::time::Duration::from_millis(200),
                Some(&dir)
            ),
            "the last piece is still being heard"
        );
        let held = held_files(&dir);
        assert_eq!(held.len(), 1, "{held:?}");
        let path = held[0].path.clone();
        assert!(held[0].last_part_missing, "{path:?}");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("We ship on friday"), "{written}");
        assert!(!written.contains("The last words"), "{written}");

        let next = Meetings::default();
        next.offer_held(&dir);
        let offered = next.current().expect("offered back");
        assert_eq!(
            offered.ended_early.as_deref(),
            Some(crate::meeting_words::LAST_PART_MISSING)
        );
        // Copying lets it go too.
        next.let_go_of_held();
        assert!(!path.exists());
    }

    fn said(text: &str) -> macalla_core::vtt::Turn {
        macalla_core::vtt::Turn {
            speaker: Some(live_meeting::YOU.into()),
            start_ms: 0,
            end_ms: 1_000,
            text: text.into(),
        }
    }

    fn held<'a>(turns: &'a [macalla_core::vtt::Turn]) -> Held<'a> {
        Held {
            turns,
            last_part_missing: false,
            ended_early: None,
        }
    }

    fn offered_texts(meetings: &Meetings) -> Vec<String> {
        meetings
            .current()
            .map(|view| view.turns.into_iter().map(|t| t.text).collect())
            .unwrap_or_default()
    }

    #[test]
    fn two_held_meetings_are_both_offered_one_per_launch_oldest_first() {
        let dir = temp_dir("held-two");
        let (first, second) = ([said("First.")], [said("Second.")]);
        // The newer one is written first, to show order comes from the time
        // in the name, not the order they were written in.
        let newer = hold_for_next_launch(&dir, &held(&second), 2_000).unwrap();
        let older = hold_for_next_launch(&dir, &held(&first), 1_000).unwrap();
        assert!(
            older.exists() && newer.exists(),
            "neither replaced the other"
        );

        let launch = Meetings::default();
        launch.offer_held(&dir);
        assert_eq!(offered_texts(&launch), ["First."]);
        launch.let_go_of_held();
        launch.clear();
        assert!(!older.exists(), "the one it read is let go");
        assert!(newer.exists(), "and only that one");

        let next = Meetings::default();
        next.offer_held(&dir);
        assert_eq!(offered_texts(&next), ["Second."]);
        next.let_go_of_held();
        assert!(!newer.exists());

        let after = Meetings::default();
        after.offer_held(&dir);
        assert_eq!(after.current(), None);
    }

    #[test]
    fn two_quits_in_the_same_millisecond_still_hold_two_files() {
        let dir = temp_dir("held-same-ms");
        let a = hold_for_next_launch(&dir, &held(&[said("A.")]), 5_000).unwrap();
        let b = hold_for_next_launch(&dir, &held(&[said("B.")]), 5_000).unwrap();
        assert_ne!(a, b);
        assert_eq!(held_files(&dir).len(), 2);
    }

    #[test]
    fn a_held_meeting_that_stopped_early_says_why_again_when_offered() {
        let dir = temp_dir("held-ended-early");
        let why = crate::meeting_words::ended_early(&macalla_session::Error::Platform(
            macalla_platform::Error::DeviceLost("USB Microphone".into()),
        ));
        let turns = [said("We ship on Friday.")];
        hold_for_next_launch(
            &dir,
            &Held {
                turns: &turns,
                last_part_missing: false,
                ended_early: Some(&why),
            },
            1_000,
        )
        .unwrap();

        let launch = Meetings::default();
        launch.offer_held(&dir);
        let offered = launch.current().expect("offered");
        assert_eq!(offered.ended_early.as_deref(), Some(why.as_str()));
        assert_eq!(
            offered_texts(&launch),
            ["We ship on Friday."],
            "the note is not a turn"
        );
    }

    #[test]
    fn a_quit_that_finished_a_meeting_which_had_ended_early_carries_why_into_the_file() {
        let dir = temp_dir("quit-ended-early");
        let meetings = Meetings::default();
        let (job, _) = meetings.begin_live().unwrap();
        let mut ended = view(job);
        ended.turns = vec![TurnView {
            speaker: Some(live_meeting::YOU.into()),
            label: Some("You".into()),
            start_ms: 0,
            end_ms: 1_000,
            text: "We ship on Friday.".into(),
            edit_label: String::new(),
        }];
        ended.ended_early = Some("The meeting stopped early.".into());
        meetings.stop_now();
        meetings.end(job, Some(ended));

        assert!(meetings.finish_for_quit(Instant::now(), Some(&dir)));

        let launch = Meetings::default();
        launch.offer_held(&dir);
        assert_eq!(
            launch.current().and_then(|v| v.ended_early).as_deref(),
            Some("The meeting stopped early.")
        );
    }

    #[test]
    fn an_unfinished_write_is_removed_and_never_offered() {
        let dir = temp_dir("held-partial");
        let unfinished = dir.join(format!(
            "{HELD_PREFIX}1000{HELD_EXTENSION}{UNFINISHED_EXTENSION}"
        ));
        std::fs::write(&unfinished, "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nHalf").unwrap();

        let launch = Meetings::default();
        launch.offer_held(&dir);

        assert_eq!(launch.current(), None);
        assert!(!unfinished.exists());
    }

    #[test]
    fn a_held_meeting_stays_until_the_person_decides_on_it() {
        let dir = temp_dir("held-stays");
        let path = hold_for_next_launch(&dir, &held(&[said("First.")]), 1_000).unwrap();

        // Offered, but another meeting became the current one before the
        // person decided: the file stays for the next launch.
        let meetings = Meetings::default();
        meetings.offer_held(&dir);
        let (job, _) = meetings.begin().unwrap();
        meetings.end(job, Some(view(job)));
        meetings.let_go_of_held();
        assert!(path.exists());
        assert_eq!(
            meetings.current().map(|v| v.job),
            Some(job),
            "the held one is not current any more"
        );
    }

    /// A kept meeting of two turns, the first with a speaker, the second
    /// without, and a summary whose one point quotes the first.
    fn summarised(meetings: &Meetings) -> u64 {
        let (job, _) = meetings.begin().unwrap();
        let mut kept = view(job);
        kept.turns = vec![
            TurnView {
                speaker: Some(live_meeting::YOU.into()),
                label: Some("You".into()),
                start_ms: 1_000,
                end_ms: 3_000,
                text: "We ship on Friday.".into(),
                edit_label: String::new(),
            },
            TurnView {
                speaker: None,
                label: None,
                start_ms: 3_500,
                end_ms: 5_000,
                text: "Agreed.".into(),
                edit_label: String::new(),
            },
        ];
        kept.summary = Some(vec![PartView {
            start_ms: 0,
            end_ms: 5_000,
            key_points: vec![ItemView {
                text: "The release is on Friday.".into(),
                quote: "ship on Friday".into(),
                owner: None,
                turn: 0,
                said: "You, 0:01".into(),
            }],
            decisions: Vec::new(),
            action_items: Vec::new(),
            dropped: 0,
            error: None,
        }]);
        kept.summary_by = Some("Apple Intelligence".into());
        meetings.end(job, Some(kept));
        job
    }

    #[test]
    fn a_live_meetings_edit_buttons_are_named_for_you_and_the_other_side() {
        let turn = |speaker: &str, start_ms: u64| macalla_core::vtt::Turn {
            speaker: Some(speaker.into()),
            start_ms,
            end_ms: start_ms + 1_000,
            text: "Words.".into(),
        };
        let meeting = MeetingTranscript {
            turns: vec![
                turn(live_meeting::YOU, 1_000),
                turn(live_meeting::OTHERS, 5_000),
            ],
            named_speakers: true,
            audio_ms: None,
        };
        let labels: Vec<String> = live_view(1, meeting)
            .turns
            .into_iter()
            .map(|t| t.edit_label)
            .collect();
        assert_eq!(
            labels,
            [
                "Edit what you said at 0:01",
                "Edit what others said at 0:05"
            ]
        );
    }

    #[test]
    fn a_corrected_turn_keeps_its_time_and_speaker_and_marks_the_summary_out_of_date() {
        let meetings = Meetings::default();
        let job = summarised(&meetings);
        let before = meetings.current().unwrap();
        assert!(!before.summary_out_of_date);

        let after = meetings
            .edit_turn(job, 0, "  We ship on\nThursday.  ")
            .unwrap();

        assert_eq!(
            after,
            meetings.current().unwrap(),
            "what it returns is kept"
        );
        assert_eq!(
            after.turns[0].text, "We ship on Thursday.",
            "whitespace made single spaces"
        );
        assert_eq!(after.turns[0].speaker, before.turns[0].speaker);
        assert_eq!(after.turns[0].label, before.turns[0].label);
        assert_eq!(after.turns[0].start_ms, before.turns[0].start_ms);
        assert_eq!(after.turns[0].end_ms, before.turns[0].end_ms);
        assert_eq!(
            after.turns[1], before.turns[1],
            "the other turn is untouched"
        );
        assert!(
            after.summary_out_of_date,
            "the summary is marked, not hidden"
        );
        assert_eq!(
            after.summary, before.summary,
            "and kept, quotes and turns as they were"
        );
        assert_eq!(after.summary_by, before.summary_by);
    }

    #[test]
    fn copy_and_save_as_text_read_the_corrected_words() {
        let meetings = Meetings::default();
        let job = summarised(&meetings);
        meetings.edit_turn(job, 1, "Agreed, Thursday.").unwrap();
        let text = transcript_text(&meetings.current().unwrap());
        assert_eq!(text, "You: We ship on Friday.\n\nAgreed, Thursday.");

        let dir = temp_dir("edit-save");
        let saved = save_new(&dir, "meeting.vtt", "2026-09-25", "txt", &text).unwrap();
        assert_eq!(std::fs::read_to_string(saved).unwrap(), text);
    }

    #[test]
    fn a_correction_with_no_summary_marks_nothing_and_the_same_words_change_nothing() {
        let meetings = Meetings::default();
        let (job, _) = meetings.begin().unwrap();
        let mut kept = view(job);
        kept.turns = vec![TurnView {
            speaker: None,
            label: None,
            start_ms: 0,
            end_ms: 1_000,
            text: "Hello there.".into(),
            edit_label: String::new(),
        }];
        meetings.end(job, Some(kept));
        let after = meetings.edit_turn(job, 0, "Hello, there.").unwrap();
        assert!(
            !after.summary_out_of_date,
            "there is no summary to be out of date"
        );

        let with_summary = Meetings::default();
        let job = summarised(&with_summary);
        let same = with_summary
            .edit_turn(job, 0, "We ship on Friday.")
            .unwrap();
        assert!(!same.summary_out_of_date, "nothing was corrected");
    }

    #[test]
    fn a_correction_to_nothing_a_stale_meeting_or_a_missing_turn_is_refused_and_changes_nothing() {
        use crate::meeting_words as words;
        let meetings = Meetings::default();
        let job = summarised(&meetings);
        let before = meetings.current();

        assert_eq!(meetings.edit_turn(job, 0, " \n\t "), Err(words::EDIT_EMPTY));
        assert_eq!(
            meetings.edit_turn(job, 2, "Words."),
            Err(words::EDIT_NO_SUCH_TURN)
        );
        assert_eq!(
            meetings.edit_turn(job + 1, 0, "Words."),
            Err(words::EDIT_STALE)
        );
        assert_eq!(meetings.current(), before, "nothing changed");

        meetings.clear();
        assert_eq!(meetings.edit_turn(job, 0, "Words."), Err(words::EDIT_STALE));
    }

    #[test]
    fn a_summary_that_ends_after_a_correction_keeps_it_and_is_out_of_date() {
        let meetings = Meetings::default();
        let job = summarised(&meetings);
        let (summary, _, reading) = meetings.begin_summary().unwrap();
        // Corrected while the summary runs.
        meetings.edit_turn(job, 1, "Agreed, Thursday.").unwrap();
        let mut written = reading;
        written.summary_by = Some("Local".into());
        let mut told = None;
        meetings.finish_summary(summary, Ok(written), |ending| {
            if let Ending::Summary(view) = ending {
                told = Some(view);
            }
        });

        let kept = meetings.current().unwrap();
        assert_eq!(
            kept.turns[1].text, "Agreed, Thursday.",
            "the correction stands"
        );
        assert_eq!(
            kept.summary_by.as_deref(),
            Some("Local"),
            "the new summary is kept"
        );
        assert!(
            kept.summary_out_of_date,
            "it read the turns before the correction"
        );
        assert_eq!(told, Some(kept), "the page is told what is kept");
    }

    #[test]
    fn a_summary_of_the_corrected_turns_is_up_to_date_again() {
        let meetings = Meetings::default();
        let job = summarised(&meetings);
        meetings.edit_turn(job, 0, "We ship on Thursday.").unwrap();
        assert!(meetings.current().unwrap().summary_out_of_date);

        let (summary, _, reading) = meetings.begin_summary().unwrap();
        meetings.end_summary(summary, Some(reading));
        assert!(!meetings.current().unwrap().summary_out_of_date);
    }

    #[test]
    fn a_correction_to_a_held_meeting_is_written_into_its_file() {
        let dir = temp_dir("held-edit");
        let why = "The meeting stopped early because USB Microphone went away.";
        let others = macalla_core::vtt::Turn {
            speaker: Some(live_meeting::OTHERS.into()),
            start_ms: 1_000,
            end_ms: 2_000,
            text: "Agreed.".into(),
        };
        let turns = [said("We ship on friday."), others];
        let path = hold_for_next_launch(
            &dir,
            &Held {
                turns: &turns,
                last_part_missing: false,
                ended_early: Some(why),
            },
            1_000,
        )
        .unwrap();
        let launch = Meetings::default();
        launch.offer_held(&dir);
        let job = launch.current().unwrap().job;

        launch.edit_turn(job, 0, "We ship on Friday.").unwrap();

        // Quit again before deciding: the next launch offers the correction,
        // and still says why the meeting stopped early.
        assert!(path.exists(), "a correction is not deciding to keep it");
        assert_eq!(
            held_files(&dir).len(),
            1,
            "written again, not a second file"
        );
        let next = Meetings::default();
        next.offer_held(&dir);
        assert_eq!(offered_texts(&next), ["We ship on Friday.", "Agreed."]);
        assert_eq!(next.current().unwrap().ended_early.as_deref(), Some(why));
    }

    #[test]
    fn a_held_meeting_missing_its_last_part_keeps_that_in_its_name_when_corrected() {
        let dir = temp_dir("held-edit-last-part");
        let turns = [said("We ship on friday.")];
        let path = hold_for_next_launch(
            &dir,
            &Held {
                turns: &turns,
                last_part_missing: true,
                ended_early: None,
            },
            1_000,
        )
        .unwrap();
        let launch = Meetings::default();
        launch.offer_held(&dir);
        let job = launch.current().unwrap().job;
        launch.edit_turn(job, 0, "We ship on Friday.").unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains(ENDED_EARLY_NOTE), "{written}");
        let next = Meetings::default();
        next.offer_held(&dir);
        let offered = next.current().unwrap();
        assert_eq!(
            offered.ended_early.as_deref(),
            Some(crate::meeting_words::LAST_PART_MISSING)
        );
        assert_eq!(offered_texts(&next), ["We ship on Friday."]);
    }

    #[test]
    fn a_held_meeting_whose_file_cannot_be_written_again_refuses_the_correction() {
        let dir = temp_dir("held-edit-unwritable");
        hold_for_next_launch(&dir, &held(&[said("First.")]), 1_000).unwrap();
        let launch = Meetings::default();
        launch.offer_held(&dir);
        let job = launch.current().unwrap().job;
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            launch.edit_turn(job, 0, "Second."),
            Err(crate::meeting_words::EDIT_HELD_NOT_WRITTEN)
        );
        assert_eq!(
            offered_texts(&launch),
            ["First."],
            "not made on the page either"
        );
    }

    #[test]
    fn a_summary_told_after_a_correction_has_the_lower_revision() {
        let meetings = Meetings::default();
        let job = summarised(&meetings);
        let (summary, _, reading) = meetings.begin_summary().unwrap();
        let mut told = None;
        let mut corrected = None;
        // The correction lands between the summary being kept, under the
        // lock, and the page being told of it, after the lock is released.
        meetings.finish_summary(summary, Ok(reading), |ending| {
            corrected = Some(meetings.edit_turn(job, 0, "We ship on Thursday.").unwrap());
            if let Ending::Summary(view) = ending {
                told = Some(view);
            }
        });
        let (told, corrected) = (told.unwrap(), corrected.unwrap());
        assert!(
            told.revision < corrected.revision,
            "the page keeps the correction, whichever reaches it first"
        );
        assert_eq!(meetings.current().unwrap(), corrected);
        assert!(corrected.summary_out_of_date);
    }

    #[test]
    fn a_held_write_that_fails_leaves_no_unfinished_file() {
        let dir = temp_dir("held-write-fails");
        // A directory with something in it where the file would go: the
        // write succeeds and the rename onto it fails.
        let path = dir.join(format!("{HELD_PREFIX}1000{HELD_EXTENSION}"));
        std::fs::create_dir_all(path.join("in-the-way")).unwrap();
        assert!(write_held(&path, &held(&[said("First.")])).is_err());
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 1, "only what was in the way: {left:?}");
    }

    #[test]
    fn a_correction_after_the_held_meeting_is_let_go_or_replaced_never_writes_its_file() {
        // Let go (copied, as the page's Copy does): its file is gone, and a
        // correction afterwards does not bring it back.
        let dir = temp_dir("held-edit-let-go");
        let path = hold_for_next_launch(&dir, &held(&[said("First.")]), 1_000).unwrap();
        let launch = Meetings::default();
        launch.offer_held(&dir);
        let job = launch.current().unwrap().job;
        launch.let_go_of_held();
        assert!(!path.exists());
        launch.edit_turn(job, 0, "Second.").unwrap();
        assert!(!path.exists(), "not written back");
        assert!(held_files(&dir).is_empty());

        // Replaced by another meeting: the held file stays as it was, for
        // the next launch, whatever is corrected in the new one.
        let dir = temp_dir("held-edit-replaced");
        let path = hold_for_next_launch(&dir, &held(&[said("First.")]), 1_000).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let launch = Meetings::default();
        launch.offer_held(&dir);
        let (other, _) = launch.begin().unwrap();
        let mut replacing = view(other);
        replacing.turns = vec![TurnView {
            speaker: None,
            label: None,
            start_ms: 0,
            end_ms: 1_000,
            text: "Another meeting.".into(),
            edit_label: String::new(),
        }];
        launch.end(other, Some(replacing));
        launch
            .edit_turn(other, 0, "Another meeting, corrected.")
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn a_summary_kept_after_a_correction_has_the_higher_revision() {
        let meetings = Meetings::default();
        let job = summarised(&meetings);
        let (summary, _, reading) = meetings.begin_summary().unwrap();
        let corrected = meetings.edit_turn(job, 0, "We ship on Thursday.").unwrap();
        let mut written = reading;
        written.summary_by = Some("Local".into());
        let mut told = None;
        meetings.finish_summary(summary, Ok(written), |ending| {
            if let Ending::Summary(view) = ending {
                told = Some(view);
            }
        });
        let told = told.expect("the page is told of the summary");
        assert!(
            told.revision > corrected.revision,
            "the page takes the summary over the correction's reply"
        );
        assert_eq!(told.summary_by.as_deref(), Some("Local"), "the new summary");
        assert_eq!(told.turns[0].text, "We ship on Thursday.");
    }
}
