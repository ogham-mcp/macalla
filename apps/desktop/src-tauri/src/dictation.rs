// SPDX-License-Identifier: MIT
//! The trigger, wired to the session.
//!
//! One thread owns the [`Session`] for the life of the app -- it has to,
//! because the armed capture stream inside it is not `Send` -- and everything
//! that can start or stop a dictation talks to that thread through a channel:
//! the global shortcut, the tray item, and later an agent. That keeps the
//! rule that the core is callable without the UI: the tray is one more trigger for the same
//! state machine the CLI drives, not a second one.
//!
//! Nothing here decides how a dictation works. Arming, the record loop, the
//! fault handling and the paste rule are `macalla_session`'s.

use crate::history::HistoryHandle;
use macalla_platform::paste::{Inserted, NotDelivered};
use macalla_platform::secrets::{SecretStore, SystemSecrets};
use macalla_session::choice::CleanupReach;
use macalla_session::{
    CleanupNotice, CleanupNow, CloudOutcome, CloudResult, Config, Dictation, Paste, Progress,
    Session, StopSignal, Waiting,
};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, Wry};

/// The overlay window's size, matching `tauri.conf.json`. Positioned from
/// here because where "bottom-centre" is depends on the monitor at the time.
const OVERLAY_WIDTH: f64 = 360.0;
const OVERLAY_HEIGHT: f64 = 96.0;
const OVERLAY_BOTTOM_MARGIN: f64 = 72.0;

/// How often, while idle, to ask whether the system default input has moved.
/// Two seconds is under what a headset's own ear detection takes to hand the
/// route back, so it adds nothing a person would notice.
const IDLE_DEVICE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// What a trigger tells the dictation thread.
///
/// The whole protocol, on every platform. Only the shortcut sends Start and
/// Stop, and the shortcut is macOS-only until another backend lands, so
/// elsewhere those variants are part of the protocol and not yet
/// constructed. Discard also comes from a click ([`Dictator::cancel`]), and
/// Finish only from one ([`Dictator::finish`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Command {
    /// A key went down. Ignored while recording; once the recording has
    /// ended, the next dictation, started when this one is over.
    Start,
    /// A key came up. Ignored while idle.
    Stop,
    /// A click: start if idle, stop if recording, and once the recording has
    /// ended, the next dictation.
    Toggle,
    /// Esc twice, the tray's Cancel Dictation or the overlay's ×: stop and
    /// throw the recording away. Nothing is pasted.
    Discard,
    /// The overlay's ✓: stop and paste, as Toggle does while recording. Unlike
    /// Toggle it is never the next dictation: once the recording has ended,
    /// or while idle, it is passed over. A click cannot see a key-up already
    /// in the channel, so a Toggle sent then would start a dictation.
    Finish,
    /// The dictionary file changed: read it again before the next dictation.
    ReloadDictionary,
    /// A model landed: if there is no session yet, try to open one now.
    ReopenEngine,
    /// The person chose another engine or transcription provider: open a
    /// session on it. One model per session, so the old one goes; a
    /// dictation in progress finishes first.
    SwitchEngine,
    /// The person chose another Enhancement: swap the session's Tier 1
    /// cleanup provider in place, without reopening it or touching the
    /// speech engine. A dictation in progress finishes first, same as
    /// `SwitchEngine`.
    SwitchCleanup,
    /// The person chose another microphone: arm on it. A dictation in
    /// progress stays on the microphone it started with.
    SwitchDevice,
    /// Send the recording held for History row `row` to the cloud provider
    /// again, from the History page. Refused unless that row, by its id and
    /// its time, is the one whose recording the session holds.
    SendAgain { row: crate::history::Recorded },
    /// History row `row` was deleted. If its recording is the one held, it
    /// goes: there is nowhere left to send it again from.
    LetGo { row: crate::history::Recorded },
    /// The app is quitting. A dictation in progress stops at its next poll:
    /// `verdict`'s Shutdown arm sets `pending.closed`. What that means for
    /// the dictation depends on [`give_up_on`]: a local dictation stops
    /// waiting on Enhancement, sends it nothing more, and is pasted and kept in History with what is
    /// ready, inside the shared quit deadline (`RunEvent::Exit` in `lib.rs`) -- logged as
    /// "finished: the app is closing", then the usual "pasted". A cloud
    /// request is given up the way Esc twice would be -- nothing pastes, and
    /// an early History row, if there is one, stays rather than being
    /// forgotten (`history_step`'s closing arm) -- logged as "stopped: the
    /// app is closing", not "discarded: Esc twice". The idle loop leaves
    /// either way. See [`Dictator::shut_down`].
    Shutdown,
}

/// Commands that arrived during a dictation and are done once it is over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Pending {
    /// Send again was asked for during a dictation or another send. Answered
    /// once it is over: after a dictation the recording it named has gone.
    send_again: Option<crate::history::Recorded>,
    /// A History row was deleted during a dictation or a send. Done once it
    /// is over, if that row's recording is still the one held.
    let_go: Option<crate::history::Recorded>,
    reload: bool,
    switch: bool,
    /// [`Command::SwitchCleanup`] arrived: applied after this dictation the
    /// same way `switch` is, but in place -- see [`cleanup_step`].
    cleanup: bool,
    device: bool,
    /// [`Command::Shutdown`] arrived, or the channel closed: the app is
    /// going away. A cloud wait gives up on it, because nothing could ever
    /// say to stop waiting.
    closed: bool,
    /// The recording has ended: released, discarded, or into the wait after
    /// it. A press or a click from here on is the next dictation.
    released: bool,
    /// Triggers taken off the channel that belong after this dictation, in
    /// the order they came: a press or a click once the recording has ended,
    /// and everything behind it. A dictation starts with what the last one
    /// held back, and reads it before the channel.
    held: VecDeque<Command>,
}

/// What the commands waiting on the channel add up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    KeepGoing,
    Stop,
    Discard,
}

/// What `run`'s `give_up` closure answers [`Session::dictate`] each time it
/// asks, once the key is up, whether to stop waiting: for the engine to
/// finish, for a cloud request's answer, or for Enhancement. Read afresh from
/// `pending` on every ask, never from a command seen once: the session asks
/// again before a remote provider is sent anything, and only keeps
/// [`Waiting::GiveUp`] from one ask to the next. `discarded` is the person's
/// own Esc twice, seen on the channel; `pending.closed` is set by
/// [`Verdict`]'s Shutdown arm; `is_cloud` is whether this dictation's
/// transcription is a cloud request ([`Session::cloud_provider`]); `reach` is
/// where the Enhancement this dictation started with sends the text
/// ([`dictation_reach`]); `keeps_row` is whether History keeps this
/// dictation, read as it started ([`HistoryHandle::keeps_dictations`]).
///
/// The first of these that holds:
/// - Esc twice gives up.
/// - The app closing gives up a cloud request: it cannot be trusted to
///   answer before the shared quit deadline (`RunEvent::Exit` in `lib.rs`),
///   so nothing pastes, and its early History row, written before the
///   request went out, stays in place ([`history_step`]).
/// - The app closing takes a local dictation on without Enhancement: nothing
///   more is sent, and it is pasted and kept in History as on a normal Stop,
///   without spending that same deadline on Enhancement.
/// - A cleanup change or an engine switch goes without Enhancement when a
///   remote provider serves this dictation ([`enhancement_still_wanted`]):
///   nothing more is sent to a provider the person switched away from
///   (nothing leaves the machine unless turned on). A provider on this Mac finishes the dictation.
/// - A press or a click for the next dictation, held in `pending.held`, cuts
///   the Enhancement wait short, and the next dictation starts straight away.
///   With a row for a late answer (`keeps_row`) it delivers now: the text is
///   pasted without waiting, a remote provider not yet sent it is sent it
///   once, and the answer reaches History. With no row it goes without
///   Enhancement, so nothing leaves the machine to be thrown away.
///   Either way the session waits out a cloud transcription: only
///   Enhancement is cut short.
/// - Otherwise, keep waiting.
///
/// Pure, so a test drives every combination.
fn give_up_on(
    discarded: bool,
    is_cloud: bool,
    pending: &Pending,
    reach: Option<CleanupReach>,
    keeps_row: bool,
) -> Waiting {
    if discarded || (pending.closed && is_cloud) {
        Waiting::GiveUp
    } else if pending.closed || !enhancement_still_wanted(false, pending, reach) {
        Waiting::WithoutEnhancement
    } else if !pending.held.is_empty() {
        if keeps_row {
            Waiting::DeliverNow
        } else {
            Waiting::WithoutEnhancement
        }
    } else {
        Waiting::Continue
    }
}

/// The body of `run`'s `give_up` closure, pulled out so a test can drive it
/// against a real channel: takes the last look at what has arrived
/// ([`verdict`]), remembers a Discard the way [`Verdict::Discard`] always
/// does, and asks [`give_up_on`] what that adds up to for this dictation.
fn give_up_step(
    rx: &Receiver<Command>,
    pending: &mut Pending,
    discarded: &Cell<bool>,
    is_cloud: bool,
    reach: Option<CleanupReach>,
    keeps_row: bool,
) -> Waiting {
    pending.released = true;
    if verdict(rx, pending) == Verdict::Discard {
        discarded.set(true);
    }
    give_up_on(discarded.get(), is_cloud, pending, reach, keeps_row)
}

/// A handle to the dictation thread. Cheap to clone; lives in Tauri's
/// managed state so the tray can reach it.
#[derive(Clone)]
pub struct Dictator {
    tx: Sender<Command>,
    app: AppHandle<Wry>,
    device: DeviceInUse,
    held: HeldRow,
    lent: LentTools,
    shutdown: ShutdownHandle,
    status: OpenedStatusShared,
    cancel: CancelGate,
    microphone: MicrophoneOwner,
}

/// Who has the microphone: a dictation, a live meeting, or neither. One
/// owner at a time, so nothing dictated can land in a meeting's transcript
/// and a meeting never waits on a dictation's recording. Whoever asks
/// second is refused with a sentence, never queued. A claim lets go when it
/// is dropped, a panic's unwinding included, so dictation works again the
/// moment a meeting ends, with no restart.
#[derive(Clone, Default)]
pub(crate) struct MicrophoneOwner(Arc<Mutex<Option<MicrophoneUse>>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicrophoneUse {
    Dictation,
    LiveMeeting,
}

/// The microphone, held by a dictation or a live meeting until dropped.
#[must_use = "the microphone is let go as soon as its claim is dropped"]
pub(crate) struct MicrophoneClaim(MicrophoneOwner);

impl Drop for MicrophoneClaim {
    fn drop(&mut self) {
        *self.0 .0.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

impl MicrophoneOwner {
    /// Claim the microphone for `by`, or say who has it.
    fn claim(&self, by: MicrophoneUse) -> Result<MicrophoneClaim, MicrophoneUse> {
        let mut owner = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match *owner {
            Some(holder) => Err(holder),
            None => {
                *owner = Some(by);
                Ok(MicrophoneClaim(self.clone()))
            }
        }
    }

    /// For a dictation: refused, with the sentence to show, while a live
    /// meeting records.
    pub(crate) fn for_dictation(&self) -> Result<MicrophoneClaim, &'static str> {
        self.claim(MicrophoneUse::Dictation)
            .map_err(|holder| match holder {
                MicrophoneUse::LiveMeeting => crate::meeting_words::DICTATION_DURING_MEETING,
                // The dictation thread serves one dictation at a time, so this
                // is never reached; said plainly if it ever is.
                MicrophoneUse::Dictation => crate::meeting_words::MEETING_DURING_DICTATION,
            })
    }

    /// For a live meeting: refused, with the sentence to show, while a
    /// dictation runs or another meeting records.
    pub(crate) fn for_meeting(&self) -> Result<MicrophoneClaim, &'static str> {
        self.claim(MicrophoneUse::LiveMeeting)
            .map_err(|holder| match holder {
                MicrophoneUse::Dictation => crate::meeting_words::MEETING_DURING_DICTATION,
                MicrophoneUse::LiveMeeting => crate::meeting_words::LIVE_BUSY,
            })
    }
}

/// Whether a click may discard or finish: a dictation is running, told at the
/// same two moments the shortcut is ([`OnDictating`]), and kept by the same
/// rule the shortcut's recogniser keeps for Esc twice
/// (`macalla_platform::hotkey::Recogniser::set_dictating`). While one runs,
/// [`CancelGate::cancel`] sends one Discard and closes the gate; while none
/// does, it sends nothing, so no Discard is left in the channel for the next
/// dictation to read -- the idle loop passes over a stray one, but a press
/// held from the wait would be started ahead of it and then discarded.
///
/// The Discard is sent under the lock, and ending a dictation takes the same
/// lock before `last_look`, so a click that got in first is always in the
/// channel by the time that look is taken. Ending one also returns whether a
/// click discarded it, the way the recogniser reports Esc twice.
///
/// [`CancelGate::finish`] is kept by the same lock and the same two moments:
/// one Finish per dictation, none while idle, none after a click discarded.
/// It does not close the gate to a discard: the × stays through the wait
/// after a finish, and still throws the dictation away.
#[derive(Clone, Default)]
struct CancelGate(Arc<Mutex<GateState>>);

#[derive(Default)]
struct GateState {
    dictating: bool,
    /// A click discarded, and the dictation has not yet been said to be over.
    /// Defence in depth: the lock order already guarantees the Discard is in
    /// the channel before `end_dictation` looks, so `last_look` would find it
    /// anyway. Kept so the gate reports a click the way the recogniser
    /// reports Esc twice, whatever a later change does to that ordering.
    ending: bool,
    /// The last dictation to end was discarded by a click. Read only for
    /// the log line that names the cause ([`CancelGate::ended_by_click`]).
    ended_by_click: bool,
    /// A click finished this dictation: a second click sends nothing.
    finished: bool,
}

impl CancelGate {
    fn set_dictating(&self, dictating: bool) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.dictating = dictating;
        state.finished = false;
        let clicked = std::mem::take(&mut state.ending);
        state.ended_by_click = !dictating && clicked;
        clicked
    }

    /// Whether the dictation that last ended was discarded by a click (the
    /// overlay's × or the tray's Cancel Dictation) rather than Esc twice.
    fn ended_by_click(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ended_by_click
    }

    /// Send a Discard through `send` if a dictation is running. Returns
    /// whether one was sent.
    fn cancel(&self, send: impl FnOnce(Command)) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !state.dictating {
            return false;
        }
        state.dictating = false;
        state.ending = true;
        send(Command::Discard);
        true
    }

    /// Send a Finish through `send` if a dictation is running and no click
    /// has finished or discarded it yet. Returns whether one was sent. A
    /// Finish that lands once the recording has ended anyway -- a key-up got
    /// there first -- is passed over by [`verdict`], so this need not know.
    fn finish(&self, send: impl FnOnce(Command)) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !state.dictating || state.finished {
            return false;
        }
        state.finished = true;
        send(Command::Finish);
        true
    }
}

/// What [`Dictator::shut_down`] needs, pulled out of [`Dictator`] so a test
/// can drive it without an `AppHandle`: the thread's own handle, so quitting
/// can wait for it to end rather than let the process tear down over a live
/// ggml engine, and the lent tools, cleared here on a best-effort basis --
/// this only stops a *new* [`Dictator::meeting_tools`] call from handing out
/// what this clears; a Files job already holding it is bounded instead by
/// `Meetings::holding_model` refusing to keep holding once
/// `Meetings::stop_now` has run (files.rs). `quitting` is read by `run`
/// directly, not just through the channel: a command queued ahead of
/// `Command::Shutdown` -- `SwitchEngine`, `ReopenEngine`, a held `Start`,
/// `SendAgain` -- is still next out of the channel and would otherwise open
/// a model, start a dictation (which, only on the fallback path where the
/// overlay is not in its panel, reaches `show_overlay`'s `primary_monitor`,
/// which blocks the same way) or send a cloud request, after the app has
/// already begun quitting. A dictation already under way when quitting
/// begins is a different question, answered by `pending.closed` and
/// [`give_up_on`], not by this flag: a local one delivers what is ready and
/// is pasted, and only a cloud request is given up.
#[derive(Clone)]
struct ShutdownHandle {
    tx: Sender<Command>,
    lent: LentTools,
    quitting: Arc<AtomicBool>,
    deadline: QuitDeadline,
    handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl ShutdownHandle {
    /// Set the quitting flag and clear the lent tools before asking the
    /// thread to end, without waiting. The flag is set first: `run` polls it
    /// directly, so a command already queued ahead of `Command::Shutdown`
    /// still finds quitting under way when it is `run`'s turn to act on it.
    fn signal(&self) {
        self.quitting.store(true, Ordering::Relaxed);
        self.lent.set(None);
        // An error means the thread is already gone; nothing more to ask.
        let _ = self.tx.send(Command::Shutdown);
    }

    /// Wait until `deadline` for the thread [`ShutdownHandle::signal`] asked
    /// to end, polling every 10 ms, and join it once it has. Returns
    /// whether it finished in time.
    fn wait(&self, deadline: std::time::Instant) -> bool {
        loop {
            let finished = match self.handle.lock() {
                Ok(guard) => guard
                    .as_ref()
                    .is_none_or(std::thread::JoinHandle::is_finished),
                Err(_) => true, // a poisoned lock is not worth waiting on
            };
            if finished {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
        true
    }
}

/// How close to the shared quit deadline a paste is still allowed to start.
/// The clipboard write, the paste chord and the restore wait
/// (`macalla_platform::paste`'s `DEFAULT_RESTORE_WAIT`, 150 ms) all need to
/// land before the process exits under them; a paste that only just started
/// could be cut off mid-keystroke.
const PASTE_DEADLINE_MARGIN: std::time::Duration = std::time::Duration::from_millis(300);

/// The shared quit deadline, set once by [`Dictator::shut_down`] just before
/// it signals the thread to end, so a dictation already under way when the
/// app quits can tell how much of the deadline is left
/// ([`paste_before_deadline`]). `None` until the app has begun quitting.
#[derive(Clone, Default)]
struct QuitDeadline(Arc<Mutex<Option<std::time::Instant>>>);

impl QuitDeadline {
    fn set(&self, deadline: std::time::Instant) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = Some(deadline);
        }
    }

    fn get(&self) -> Option<std::time::Instant> {
        self.0.lock().ok().and_then(|guard| *guard)
    }
}

/// Whether there is still enough of the shared quit deadline left to start a
/// paste: always true when the app is not quitting, or in a test that never
/// set one (`deadline` is `None`), and otherwise only once at least
/// [`PASTE_DEADLINE_MARGIN`] remains.
fn paste_before_deadline(now: std::time::Instant, deadline: Option<std::time::Instant>) -> bool {
    deadline.is_none_or(|deadline| deadline.saturating_duration_since(now) >= PASTE_DEADLINE_MARGIN)
}

/// Whether a dictation reaching the paste step while the app is closing
/// should still be pasted. `frontmost_is_self` is
/// [`macalla_is_frontmost`], asked only while closing: pressing Cmd-Q or
/// choosing Quit from Macalla's own window means the person is looking at
/// Macalla, not at whatever app they were last dictating into, and pasting
/// there would land in the wrong place. `deadline_ok` is
/// [`paste_before_deadline`]. Not closing reads as `frontmost_is_self:
/// false, deadline_ok: true` at the call site, which this always lets
/// through, matching a normal Stop.
fn paste_on_close(frontmost_is_self: bool, deadline_ok: bool) -> bool {
    !frontmost_is_self && deadline_ok
}

/// Whether Macalla itself is the app in front right now: the frontmost
/// bundle identifier ([`macalla_platform::frontmost_app`], a synchronous
/// `NSWorkspace.frontmostApplication.bundleIdentifier` message send with no
/// dispatch to the main queue -- already called from this thread during a
/// live dictation, in `macalla_session`'s own use of the same function --
/// against this app's own (`tauri.conf.json`'s `identifier`, a plain field
/// on the `Config` `AppHandle::config` already holds, not a fresh read).
fn macalla_is_frontmost(app: &AppHandle<Wry>) -> bool {
    macalla_platform::frontmost_app().as_deref() == Some(app.config().identifier.as_str())
}

/// Whether a dictation's late Enhancement is still worth waiting on for
/// History: not while the app is closing, when the answer could only land
/// after the process has gone. One half of [`enhancement_still_wanted`],
/// which is where it is asked. While closing, [`give_up_on`] has already
/// delivered a local dictation without waiting on Enhancement, so the paste
/// carries whatever came back before that; what is still out is dropped with
/// the handle (`crates/macalla-session/src/cleanup.rs`'s `LateCleanup` doc
/// says what that ends).
fn keep_late_on_close(closing: bool) -> bool {
    !closing
}

/// The input device the dictation thread is armed on, as the OS names it,
/// shared with the Microphone card. `None` until a microphone has been
/// opened. Written by the thread, read by a command; the thread emits
/// `dictation-device` whenever it changes so the card and the tray's
/// Microphone submenu (`tray.rs`) follow without polling.
#[derive(Clone, Default)]
struct DeviceInUse(Arc<Mutex<Option<String>>>);

impl DeviceInUse {
    fn get(&self) -> Option<String> {
        self.0.lock().map(|d| d.clone()).unwrap_or_default()
    }

    /// Remember the name and, if it changed, say so.
    fn set(&self, app: &AppHandle<Wry>, name: Option<&str>) {
        let changed = match self.0.lock() {
            Ok(mut current) => {
                if current.as_deref() != name {
                    *current = name.map(str::to_string);
                    true
                } else {
                    false
                }
            }
            Err(_) => false,
        };
        if changed {
            tracing::info!(device = name.unwrap_or("none"), "input device in use");
            let _ = app.emit("dictation-device", name);
        }
    }
}

/// The History row whose recording the session holds after a failed cloud
/// request, shared with the History page's list so it offers Send again on
/// that row alone. Written by the dictation thread, which emits
/// `history-held` with the row's id, or null, whenever it changes.
#[derive(Clone, Default)]
struct HeldRow(Arc<Mutex<Option<crate::history::Recorded>>>);

impl HeldRow {
    fn get(&self) -> Option<crate::history::Recorded> {
        self.0.lock().map(|row| *row).unwrap_or_default()
    }

    /// Remember the row and, if it changed, say so.
    fn set(&self, app: &AppHandle<Wry>, row: Option<crate::history::Recorded>) {
        let changed = match self.0.lock() {
            Ok(mut current) if *current != row => {
                *current = row;
                true
            }
            _ => false,
        };
        if changed {
            let _ = app.emit("history-held", row.map(|row| row.id));
        }
    }
}

/// What a meeting borrows from the session the dictation thread holds, shared
/// with the Files page's commands so a file is heard on the model dictation
/// already loaded. Written by the thread before it waits for each command, so
/// an engine switch or a dictionary reload reaches the next file; `None` while
/// no session is open. A meeting already running keeps what it borrowed.
#[derive(Clone, Default)]
struct LentTools(Arc<Mutex<Option<macalla_session::meeting::MeetingTools>>>);

impl LentTools {
    fn get(&self) -> Option<macalla_session::meeting::MeetingTools> {
        self.0.lock().ok().and_then(|tools| tools.clone())
    }

    fn set(&self, session: Option<&Session>) {
        let tools = session.and_then(|session| session.meeting_tools().ok());
        if let Ok(mut current) = self.0.lock() {
            *current = tools;
        }
    }
}

impl Dictator {
    /// The input device the next dictation will listen through, if known.
    pub fn device(&self) -> Option<String> {
        self.device.get()
    }

    /// What dictation actually opened with, for the Models page's "in use"
    /// truth next to the stored choice. `None` before the first open.
    pub fn status(&self) -> Option<OpenedStatus> {
        self.status.get()
    }

    /// What a meeting from a file borrows from the open session: its engine,
    /// pipeline and cleanup. `None` while no session is open.
    pub fn meeting_tools(&self) -> Option<macalla_session::meeting::MeetingTools> {
        self.lent.get()
    }

    /// The History row whose recording can be sent again, if any.
    pub fn held_row(&self) -> Option<crate::history::Recorded> {
        self.held.get()
    }

    /// Who has the microphone, for a live meeting to claim until it ends
    /// (`files::start_live`). While the meeting holds it, every dictation
    /// trigger is refused and says why.
    pub(crate) fn microphone(&self) -> &MicrophoneOwner {
        &self.microphone
    }

    /// Throw the running dictation away, as Esc twice does: the tray's Cancel
    /// Dictation and the overlay's ×. Nothing happens if none is running
    /// ([`CancelGate`]).
    pub fn cancel(&self) {
        if !self.cancel.cancel(|command| self.send(command)) {
            tracing::debug!("cancel: no dictation to discard");
        }
    }

    /// Stop the running dictation and paste, as the tray's Stop Dictation
    /// does: the overlay's ✓. Nothing happens if none is running, or if a
    /// click already finished or discarded it ([`CancelGate::finish`]), so a
    /// late or double click never starts one.
    pub fn finish(&self) {
        if !self.cancel.finish(|command| self.send(command)) {
            tracing::debug!("finish: no dictation to finish");
        }
    }

    pub fn send(&self, command: Command) {
        if self.tx.send(command).is_err() {
            // The thread never started or has gone -- almost always because
            // no engine could be opened, which was logged once at startup.
            // A tray item that looks live and does nothing is worse than a
            // repeated message, so say it again on every attempt.
            tracing::warn!(?command, "no dictation thread to take the command");
            let _ = self.app.emit(
                "dictation-error",
                // Name the file rather than "the log": a Finder launch has no
                // terminal, so a log nobody can locate is no help at all.
                match crate::logging::log_path() {
                    Some(file) => format!(
                        "Dictation is not running. The reason is in {}.",
                        file.display()
                    ),
                    None => "Dictation is not running. The reason is in the app's log.".to_string(),
                },
            );
        }
    }

    /// Ask the dictation thread to end and wait up to `deadline` for it, so
    /// the Metal buffers behind its engine are freed before the process
    /// exits: records `deadline` where a dictation still finishing can read
    /// it ([`paste_before_deadline`]), clears the lent tools and sends
    /// [`Command::Shutdown`], then polls the thread every 10 ms and joins it
    /// once it has finished. Returns whether it finished in time. The
    /// deadline is set before `signal`, the same ordering `signal` itself
    /// uses for the quitting flag: so it is never missing by the time a
    /// dictation reads it.
    pub fn shut_down(&self, deadline: std::time::Instant) -> bool {
        self.shutdown.deadline.set(deadline);
        self.shutdown.signal();
        self.shutdown.wait(deadline)
    }
}

/// What the dictation thread tells the shortcut listener, so that Esc twice
/// discards a dictation whoever started it. Ending one returns whether the
/// listener saw Esc twice during it, which counts even when the Discard is
/// still crossing the shortcut thread. A no-op, and `false`, where there is
/// no listener.
type OnDictating = Box<dyn Fn(bool) -> bool + Send>;

/// `inner` -- the shortcut's hook, or a no-op where there is none -- with
/// `gate` told at the same moments, so a click can discard on every
/// platform. Both are told before `end_dictation` looks, so a Discard the
/// gate sent is already in the channel by then. Returns whether either
/// discarded; which one did is [`CancelGate::ended_by_click`]'s to say.
fn with_cancel_gate(inner: OnDictating, gate: CancelGate) -> OnDictating {
    Box::new(move |dictating| {
        let esc_fired = inner(dictating);
        let clicked = gate.set_dictating(dictating);
        esc_fired || clicked
    })
}

/// The event that tells the overlay, with a `bool`, whether the shortcut's
/// key is being held to dictate. The overlay shows no × while it is: the
/// default key is Option, and a click with Option down asks macOS to hide
/// the app in front.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const KEY_HELD: &str = "dictation-key-held";

/// The event that tells the overlay this dictation's recording has ended,
/// and with it the ✓: whatever ended it, told once by the dictation thread
/// ([`tell_recording_ended`]); and, on macOS, sooner by the shortcut's thread
/// as the key comes up, which in hold mode always ends a recording. Told
/// while idle too, where it changes nothing.
const RECORDING_ENDED: &str = "dictation-recording-ended";

/// Call `tell` the first time `released` is seen true for this dictation.
fn tell_recording_ended(released: bool, told: &Cell<bool>, tell: impl FnOnce()) {
    if released && !told.replace(true) {
        tell();
    }
}

/// What the overlay was last told on [`KEY_HELD`]. Told only on a change,
/// and read and told under one lock, so the shortcut listener's thread and
/// the dictation thread, which both tell it, cannot land answers out of order.
#[derive(Clone, Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct KeyHeld(Arc<Mutex<bool>>);

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl KeyHeld {
    /// Read `now` and pass it to `tell` if it differs from what was told
    /// last.
    fn update(&self, now: impl FnOnce() -> bool, tell: impl FnOnce(bool)) {
        let mut told = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let held = now();
        if held != *told {
            *told = held;
            tell(held);
        }
    }

    /// Read `now` and pass it to `tell` whatever was told last: an overlay
    /// that loaded again since has forgotten it.
    fn retell(&self, now: impl FnOnce() -> bool, tell: impl FnOnce(bool)) {
        let mut told = self.0.lock().unwrap_or_else(|p| p.into_inner());
        *told = now();
        tell(*told);
    }
}

/// `inner`, with the overlay told `is_held` again as each dictation starts,
/// before it says it is listening, so an overlay that loaded since the key
/// last moved still knows; and updated as each one ends. The key's own
/// moves are told by the shortcut's thread (`Listener::on_held`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn with_key_held(
    inner: OnDictating,
    held: KeyHeld,
    is_held: impl Fn() -> bool + Send + 'static,
    tell: impl Fn(bool) + Send + 'static,
) -> OnDictating {
    Box::new(move |dictating| {
        let discarded = inner(dictating);
        if dictating {
            held.retell(&is_held, &tell);
        } else {
            held.update(&is_held, &tell);
        }
        discarded
    })
}

/// Start the dictation thread and the shortcut that drives it.
pub fn start(app: AppHandle<Wry>, history: HistoryHandle) -> Dictator {
    let (tx, rx) = mpsc::channel();
    let device = DeviceInUse::default();
    let status = OpenedStatusShared::default();
    let held = HeldRow::default();

    // Whether the shortcut's key is held, as last told to the overlay: by
    // the listener's thread whenever the key goes down or up, and by the
    // dictation thread as each dictation starts and ends. A tray click
    // changes nothing.
    #[cfg(target_os = "macos")]
    let key_held = KeyHeld::default();
    #[cfg(target_os = "macos")]
    let (listener, on_dictating): (macalla_platform::hotkey::Listener, OnDictating) = {
        use macalla_platform::hotkey::{Listener, Mode, Shortcut};
        // Hold to dictate, on the platform default. Configurable later from
        // the Shortcut card; the key events become the same commands the
        // tray sends, which is the whole point of the channel.
        let listener = Listener::start(Shortcut::default_for_platform(), Mode::Hold);
        let watch_held = key_held.clone();
        let watch_app = app.clone();
        listener.on_held(move |now| {
            if !now {
                let _ = watch_app.emit(RECORDING_ENDED, ());
            }
            watch_held.update(
                || now,
                |held| {
                    let _ = watch_app.emit(KEY_HELD, held);
                },
            );
        });
        let state = listener.dictation_state();
        let held_state = listener.dictation_state();
        let tell_app = app.clone();
        let on_dictating = with_key_held(
            Box::new(move |dictating| state.set_dictating(dictating)),
            key_held.clone(),
            move || held_state.is_held(),
            move |held| {
                let _ = tell_app.emit(KEY_HELD, held);
            },
        );
        (listener, on_dictating)
    };
    #[cfg(not(target_os = "macos"))]
    let on_dictating: OnDictating = Box::new(|_| false);

    // The AirPods stem, the AirPods Max crown, a headset's mute button:
    // pressed during a dictation they mean "stop", and without a handler
    // macOS shows "Cannot Control Mic with AirPods" and carries on. The
    // handler is set when a dictation starts and cleared when it ends,
    // rather than once at launch: Apple's header says it "should be set by
    // the process doing the call's audio I/O", and a handler set before any
    // capture existed was never called (2026-09-09, the press still drew
    // the banner). Unmute is nothing to us.
    #[cfg(target_os = "macos")]
    let on_dictating: OnDictating = {
        let forward = tx.clone();
        Box::new(move |dictating: bool| {
            let discarded = on_dictating(dictating);
            if dictating {
                let forward = forward.clone();
                if let Err(e) = macalla_platform::mute::on_input_mute_request(move |muted| {
                    if muted {
                        tracing::info!("hardware mute gesture: stopping the dictation");
                        let _ = forward.send(Command::Stop);
                    }
                }) {
                    tracing::info!(error = %e, "the hardware mute gesture is not available");
                }
            } else if let Err(e) = macalla_platform::mute::clear_input_mute_handler() {
                tracing::info!(error = %e, "could not clear the hardware mute gesture");
            }
            discarded
        })
    };

    // Every platform: a click can discard where there is no shortcut.
    let cancel = CancelGate::default();
    let on_dictating = with_cancel_gate(on_dictating, cancel.clone());

    let thread_app = app.clone();
    let thread_device = device.clone();
    let thread_status = status.clone();
    let thread_held = held.clone();
    let thread_cancel = cancel.clone();
    let microphone = MicrophoneOwner::default();
    let thread_microphone = microphone.clone();
    let lent = LentTools::default();
    let thread_lent = lent.clone();
    let quitting = Arc::new(AtomicBool::new(false));
    let thread_quitting = Arc::clone(&quitting);
    let deadline = QuitDeadline::default();
    let thread_deadline = deadline.clone();
    let handle = std::thread::Builder::new()
        .name("macalla-dictation".into())
        .spawn(move || {
            run(
                thread_app,
                rx,
                history,
                on_dictating,
                Shared {
                    cancel: thread_cancel,
                    device: thread_device,
                    status: thread_status,
                    held_row: thread_held,
                    lent: thread_lent,
                    quitting: thread_quitting,
                    deadline: thread_deadline,
                    microphone: thread_microphone,
                },
            )
        })
        .expect("spawn the dictation thread");

    #[cfg(target_os = "macos")]
    {
        let shortcut = macalla_platform::hotkey::Shortcut::default_for_platform();
        let forward = tx.clone();
        let listener_app = app.clone();
        std::thread::Builder::new()
            .name("macalla-shortcut".into())
            .spawn(move || {
                while let Some(event) = listener.recv() {
                    match event {
                        Ok(action) => forward.send(command_for(action)).ok(),
                        Err(e) => {
                            // Usually Input Monitoring. Said on the settings
                            // screen and the checklist brought forward,
                            // because a shortcut that does nothing looks like
                            // a broken app and the log is not where anyone
                            // looks first.
                            tracing::error!(error = %e, "the shortcut listener stopped");
                            let _ = listener_app.emit(
                                "dictation-error",
                                "The shortcut cannot see your keys. Turn on Input Monitoring \
                                 for Macalla in System Settings, then open Macalla again.",
                            );
                            if let Some(window) = listener_app.get_webview_window("main") {
                                let _ = listener_app.emit_to("main", "navigate", "settings");
                                let _ = window.show();
                            }
                            break;
                        }
                    };
                }
            })
            .expect("spawn the shortcut thread");
        tracing::info!(shortcut = %shortcut, "hold the shortcut to dictate");
    }

    let shutdown = ShutdownHandle {
        tx: tx.clone(),
        lent: lent.clone(),
        quitting,
        deadline,
        handle: Arc::new(Mutex::new(Some(handle))),
    };
    Dictator {
        tx,
        app,
        device,
        status,
        held,
        lent,
        shutdown,
        cancel,
        microphone,
    }
}

/// The command a shortcut action becomes on its way to the dictation thread.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn command_for(action: macalla_platform::hotkey::Action) -> Command {
    use macalla_platform::hotkey::Action;
    match action {
        Action::Start => Command::Start,
        Action::Stop => Command::Stop,
        Action::Discard => Command::Discard,
    }
}

/// What the commands that have arrived since the last look say to do.
///
/// Drains everything waiting, because a Stop behind a Start must still win:
/// a key released before the loop looked is a release all the same. A second
/// Start while recording is not a second dictation. A Discard outranks a Stop
/// in the same batch: Esc twice and then letting go is still a discard. A
/// closed channel stops, since nothing can ever say so again.
///
/// Once the recording has ended, only a Discard is this dictation's. A
/// Start or a Toggle is the next dictation: it goes to [`Pending::held`]
/// with every trigger behind it, for [`next_command`] to hand over once this
/// one is over. A key coming up before one is this dictation's own release,
/// and goes. Dropping that press lost the whole of the next dictation.
///
/// A press made before Esc twice goes with the discarded dictation, and one
/// made after it starts the next: a Discard clears what is held, and a Start
/// behind it in the same batch is held again. The shortcut's `Recogniser`
/// keeps the same rule, so a press it will stop on release is one this
/// thread starts, and no other.
fn verdict(rx: &Receiver<Command>, pending: &mut Pending) -> Verdict {
    let mut verdict = Verdict::KeepGoing;
    loop {
        // While recording, what the last dictation held back comes first.
        // After release everything held is the next dictation's.
        let next = if pending.released {
            None
        } else {
            pending.held.pop_front()
        };
        match next.map_or_else(|| rx.try_recv(), Ok) {
            // Not a trigger. Remembered, and done once the dictation is over.
            Ok(Command::ReloadDictionary) => pending.reload = true,
            Ok(Command::SwitchEngine) => pending.switch = true,
            // Does not give up a Send-again wait: unlike `switch`, applying
            // this needs no reopen, so it never has to interrupt one.
            Ok(Command::SwitchCleanup) => pending.cleanup = true,
            Ok(Command::SwitchDevice) => pending.device = true,
            Ok(Command::SendAgain { row }) => pending.send_again = Some(row),
            Ok(Command::LetGo { row }) => pending.let_go = Some(row),
            // A `Some` session can always dictate (`PreviewMissing` opens
            // as `None`); with no session the Send-again arm never drains,
            // so the idle arm sees it. Nothing to do.
            Ok(Command::ReopenEngine) => {}
            // A click for this dictation only: never the next one.
            Ok(Command::Finish) if pending.released => {}
            Ok(Command::Discard) => {
                verdict = Verdict::Discard;
                pending.released = true;
                pending.held.clear();
            }
            Ok(trigger @ (Command::Start | Command::Stop | Command::Toggle))
                if pending.released =>
            {
                if trigger != Command::Stop || !pending.held.is_empty() {
                    pending.held.push_back(trigger);
                }
            }
            Ok(Command::Stop | Command::Toggle | Command::Finish) => {
                if verdict == Verdict::KeepGoing {
                    verdict = Verdict::Stop;
                }
                pending.released = true;
            }
            Ok(Command::Start) => {}
            // Treated like the channel closing (below): this only ends the
            // recording, the way Stop does above -- it always returns
            // `Verdict::Stop`, never `Discard`. What becomes of the
            // dictation from here is `give_up_on`'s call, downstream in
            // `run`'s `give_up`: a local one still finishes and pastes; a
            // cloud wait must not hold the app open for its answer.
            Ok(Command::Shutdown) => {
                pending.closed = true;
                return if verdict == Verdict::KeepGoing {
                    Verdict::Stop
                } else {
                    verdict
                };
            }
            Err(TryRecvError::Empty) => return verdict,
            Err(TryRecvError::Disconnected) => {
                pending.closed = true;
                return if verdict == Verdict::KeepGoing {
                    Verdict::Stop
                } else {
                    verdict
                };
            }
        }
    }
}

/// The last look at a dictation's commands, and whether it was discarded:
/// seen by this thread while the session asked (`seen`), in the channel
/// now, or reported by the shortcut as `on_dictating(false)` ended it
/// (`esc_fired`).
///
/// Called after `on_dictating(false)`, so the shortcut sends no Discard for
/// this dictation once the look is taken. One it sent before can still be
/// crossing the shortcut thread; without the shortcut's own answer that
/// dictation was kept, and a remote provider sent its text. The presses
/// held until then were all made before that Esc twice, and go with it. So
/// does a tray click read while that Discard is still crossing: the person
/// clicks again, and nothing is lost or sent.
fn last_look(rx: &Receiver<Command>, pending: &mut Pending, seen: bool, esc_fired: bool) -> bool {
    pending.released = true;
    let now = verdict(rx, pending) == Verdict::Discard;
    if esc_fired && !seen && !now {
        pending.held.clear();
    }
    seen || now || esc_fired
}

/// The end of a dictation, in the order it has to happen: the shortcut is
/// told first and says whether it heard Esc twice, and only then the last
/// look, so a Discard sent as it was told is in the channel to be read.
/// `seen` is whether this thread saw a Discard while the session asked.
/// Returns whether the dictation was discarded.
fn end_dictation(
    on_dictating: &OnDictating,
    rx: &Receiver<Command>,
    pending: &mut Pending,
    seen: bool,
) -> bool {
    let esc_fired = on_dictating(false);
    last_look(rx, pending, seen, esc_fired)
}

/// Whether a dictation should show "No speech detected." before the overlay
/// closes: it heard nothing, and it was not thrown away. `discarded` is
/// whatever is already known about a discard at this point -- the local
/// cell `should_stop` set, and/or the result's own [`Dictation::discarded`]
/// -- not what `end_dictation` finds afterwards from a Discard that lands
/// after the session last asked; that late case is not knowable this early
/// and still shows the notice.
fn says_nothing_said(result: &macalla_session::Result<Dictation>, discarded: bool) -> bool {
    !discarded
        && matches!(result, Ok(d)
            if d.heard_speech == Some(false) || d.document.to_plain().trim().is_empty())
}

/// The idle loop's next command: what the last dictation held back, in the
/// order it came, then the channel.
fn next_command(
    held: &mut VecDeque<Command>,
    rx: &Receiver<Command>,
    timeout: std::time::Duration,
) -> Result<Command, RecvTimeoutError> {
    match held.pop_front() {
        Some(command) => Ok(command),
        None => rx.recv_timeout(timeout),
    }
}

/// The cloud transcription config for the next session open, from the
/// stored choice ([`HistoryHandle::cloud_choice`]). `None` dictates on the
/// local engine alone.
fn cloud_config(history: &HistoryHandle) -> Option<macalla_session::choice::CloudTranscription> {
    history
        .cloud_choice()
        .map(|c| macalla_session::choice::CloudTranscription {
            provider: c.provider,
            model: c.model,
        })
}

/// The cleanup to try for the next session open, from the stored choice
/// ([`HistoryHandle::cleanup_choice`]). Empty is off.
pub(crate) fn cleanup_config(
    history: &HistoryHandle,
) -> Vec<macalla_session::choice::EnhancerChoice> {
    use crate::history::CleanupChoice as Stored;
    use macalla_session::choice::{cleanup_enhancers, CleanupChoice};
    let choice = match history.cleanup_choice() {
        Stored::Off => CleanupChoice::Off,
        Stored::AppleIntelligence => CleanupChoice::AppleIntelligence,
        Stored::Server { address, model } => CleanupChoice::Server { address, model },
        Stored::Pool { models } => CleanupChoice::Pool {
            models: models
                .into_iter()
                .map(|m| macalla_session::choice::PoolModel {
                    provider: m.provider,
                    model: m.model,
                })
                .collect(),
        },
    };
    cleanup_enhancers(&choice)
}

/// The notice for a session whose cleanup is not cleaning: nothing built, or
/// Apple Intelligence is switched off, which is kept and asked again before
/// every segment. Dictation works and pastes what was said, without the
/// tidying. Said once, when the session opens or a cleanup swap
/// ([`swap_cleanup`]) replaces it, rather than on every dictation. `notice`
/// is [`Session::cleanup_error`]; each choice that failed
/// to build gets its own sentence from [`say::say`](crate::say::say), shown
/// once even when more than one choice failed the same way.
fn cleanup_notice(notice: &CleanupNotice) -> String {
    let prefix = "Enhancement isn't working right now, so your words are pasted without it.";
    match notice {
        CleanupNotice::Failed(errors) => {
            let mut sentences: Vec<String> = Vec::new();
            for error in errors {
                let sentence = crate::say::say(error);
                if !sentences.contains(&sentence) {
                    sentences.push(sentence);
                }
            }
            format!("{prefix} {}", sentences.join(" "))
        }
        // Already a sentence fit to show, from
        // `macalla_enhancers::apple::Availability::explain`.
        CleanupNotice::NotReady(reason) => format!("{prefix} {reason}"),
    }
}

/// The overlay's destination line, the visible label for where words go: the
/// transcription provider when the recording goes to one, then where cleanup
/// sends the text, joined by a middle dot -- "ElevenLabs · Local". Either
/// alone when there is only one, and `None` when the words go to neither.
fn destination_label(transcription: Option<&str>, cleanup: Option<&str>) -> Option<String> {
    match (transcription, cleanup) {
        (Some(transcription), Some(cleanup)) => Some(format!("{transcription} \u{00b7} {cleanup}")),
        (one, other) => one.or(other).map(str::to_string),
    }
}

/// The overlay's own compact form for everyone a dictation's words go to,
/// in order, deduped by provider id -- two recipients naming the same
/// provider (two OpenAI models, or OpenAI transcribing and an OpenAI model
/// cleaning up) are one destination, not two. A recipient that is not a
/// provider (Apple Intelligence, a server) has no id and is deduped by its
/// display name instead. One distinct recipient gives the bare name
/// ("OpenAI"); more give the first, in order, plus how many others
/// ("OpenAI +1"). Never a model id (decided: the overlay names
/// providers, the Models page names models), and never the page's own
/// "…, falling back to …" wording
/// (`macalla_session::cleanup::pool_label`), which does not fit the bottom
/// row's right-hand side past two providers ([`OVERLAY_LINE_MAX_CHARS`]).
/// `None` when there is no recipient.
fn recipients_overlay_label<'a>(
    recipients: impl Iterator<Item = (Option<&'static str>, std::borrow::Cow<'a, str>)>,
) -> Option<String> {
    let mut seen: Vec<(Option<&'static str>, std::borrow::Cow<'a, str>)> = Vec::new();
    for (id, name) in recipients {
        let same =
            |(seen_id, seen_name): &(Option<&str>, std::borrow::Cow<str>)| match (id, seen_id) {
                (Some(id), Some(seen_id)) => id == *seen_id,
                (None, None) => name == *seen_name,
                _ => false,
            };
        if !seen.iter().any(same) {
            seen.push((id, name));
        }
    }
    match seen.as_slice() {
        [] => None,
        [(_, only)] => Some(only.to_string()),
        [(_, first), rest @ ..] => Some(format!("{first} +{}", rest.len())),
    }
}

/// [`recipients_overlay_label`] for a cleanup pool's built choices alone.
fn pool_overlay_label(choices: &[macalla_session::choice::EnhancerChoice]) -> Option<String> {
    recipients_overlay_label(choices.iter().map(|c| (c.provider_id(), c.destination())))
}

/// The overlay's destination line, bounded by [`OVERLAY_LINE_MAX_CHARS`] to
/// fit the bottom row beside "Enhancing…". `transcription` is the cloud
/// transcription provider's name, `cleanup` the compact label for where
/// Enhancement sends the text (which already includes the transcription
/// provider when both are the same provider, and then `transcription` is
/// `None`), `engine` the bare name of what transcribes, and `issue` the
/// short phrase for why the stored choices could not be fully used.
///
/// With no `issue`: [`destination_label`] of the two, or `None`.
///
/// With an `issue`, the first of these that applies:
///
/// 1. the label (or `engine` when there is no label), " · ", the issue --
///    when that fits;
/// 2. with a label (a cloud transcription name, a cleanup label, or both):
///    the whole label, unchanged, even past the limit (a "Your server at"
///    hostname long enough by itself). A label names where words are being
///    sent, and anything that leaves the Mac must stay visibly labelled
///    while it is in use, so a label never gives way to the issue, which the
///    Models page still shows;
/// 3. with no label (local transcription, nothing to clean up with, so
///    nothing leaves the Mac): the issue alone, then its first clause (the
///    issue's own "; "-joined phrases) alone, capitalised to open the line
///    -- the issue wins over the local engine's name;
/// 4. when even that first clause does not fit (three providers each
///    needing a key): `engine`, as if there were no issue.
///
/// So when a label and an issue exist and only one fits, the label is
/// shown; when only the local engine's name and an issue exist, the issue
/// is.
fn overlay_issue_line(
    engine: &str,
    transcription: Option<&str>,
    cleanup: Option<&str>,
    issue: Option<&str>,
) -> Option<String> {
    let label = destination_label(transcription, cleanup);
    let Some(issue) = issue else {
        return label;
    };
    let fits = |line: &str| line.chars().count() <= OVERLAY_LINE_MAX_CHARS;
    let base = label.as_deref().unwrap_or(engine);
    let full = format!("{base} \u{00b7} {issue}");
    if fits(&full) {
        return Some(full);
    }
    if label.is_some() {
        return label;
    }
    // The issue opens the line by itself here rather than sitting after
    // "· ": `capitalise_first` (`cloud_fallback_notice` uses it the same
    // way) makes that read right for a phrase such as
    // `short_session_error`'s "the saved provider is unknown", written
    // lower-case for the far commoner mid-sentence case. A no-op for a
    // phrase that already opens with a name.
    if fits(issue) {
        return Some(capitalise_first(issue));
    }
    let first_clause = issue.split("; ").next().unwrap_or(issue);
    if fits(first_clause) {
        return Some(capitalise_first(first_clause));
    }
    // Nothing short enough is left to say about the issue: the local engine
    // stands instead, as it would with no issue at all.
    Some(engine.to_string())
}

/// [`overlay_issue_line`]'s length limit for the whole right-hand line:
/// label or engine name, plus " · ", plus the issue. Measured with AppKit's
/// system font at 11px (`NSString.size(withAttributes:)`), not guessed:
///
/// - the overlay's content row is 332px; a 10px flex gap, an "…" (8.92px)
///   and " · Esc twice to discard" (115.01px) stood beside this line when it
///   was measured, leaving about 198px for it. That hint has since gone:
///   the left of the row now holds "Enhancing…" or nothing, which is no
///   wider (not measured), so the budget below is conservative. It was
///   left at 34 rather than re-derived, because re-deriving needs a new
///   AppKit measurement and the lines the tests pin were checked against
///   the old one;
/// - a pool can only ever hold OpenAI, Anthropic, Gemini or Mistral
///   (`choice::pool_route`'s own allow-list, not every
///   [`macalla_session::providers::PROVIDERS`] entry); "Anthropic", the
///   longest of the four bare, is 9 chars / 50.99px, 5.67px a character --
///   used as the seed for 198 / 5.67, a little under 35, rounded down to
///   34.
///
/// That seed is a heuristic, not the true widest character-for-character
/// (a bare local engine name like "Nemotron" measures higher, 6.50px a
/// character), so `overlay_line_never_exceeds_the_limit_across_pool_shapes`
/// checks every emitted line's length instead: nothing built, and 1- to
/// 4-member pools (a pool of all four `choice::pool_route` lets in, its
/// widest reachable shape) built with Anthropic first (the widest bare
/// pool-eligible name), every local engine's own card name and every cloud
/// transcription provider's, and no issue, a one-name, a two-name and a
/// mixed transcription-plus-pool needs-a-key issue. The pool's own model
/// ids are real (`pool_choice` builds a real `EnhancerChoice`) but never
/// read here, only checked absent from every emitted line. Lines measured
/// at 34 characters: "OpenRouter and Anthropic need keys" at 195.65px
/// and "OpenAI +3 · OpenRouter needs a key" at 193.76px. Also inside the
/// ~198px budget: "Anthropic +1"/"+2 · OpenAI needs a key" (33 chars /
/// 180.08px and 181.62px) and "Anthropic · OpenAI needs a key" (30 chars /
/// 164.76px). Longer combinations are shrunk by [`overlay_issue_line`]'s
/// steps before they are emitted. `.where` does not shrink, so a longer
/// line clips at the window edge rather than wrapping or ellipsising.
const OVERLAY_LINE_MAX_CHARS: usize = 34;

/// The overlay's whole destination line for a dictation starting now, from
/// typed facts only, rendered once through [`overlay_issue_line`]:
///
/// - recipients: `transcription` (a cloud transcription provider's id,
///   `None` on this machine) first, then Enhancement's built choices, but
///   only when `cleanup` is [`CleanupNow::Serving`] -- a choice that built
///   but cannot serve right now, or nothing built, is not where the words
///   go. Deduped by provider id: when transcription's provider is also in
///   the pool, the whole list is one [`recipients_overlay_label`] ("OpenAI
///   +1"); otherwise the transcription name and the pool's own label are
///   joined by [`destination_label`] ("ElevenLabs · OpenAI +1");
/// - the issue: `transcription_issue` merged with Enhancement's own
///   ([`cleanup_issue_parts`]), each provider needing a key named once.
///
/// `engine` is the name of what transcribes, and `ticked` the choices
/// Enhancement was built from, zipped with a `Failed` list for names.
fn overlay_line(
    engine: &str,
    transcription: Option<&str>,
    transcription_issue: &IssueParts,
    cleanup: CleanupNow<'_>,
    ticked: &[macalla_session::choice::EnhancerChoice],
) -> Option<String> {
    let built: &[macalla_session::choice::EnhancerChoice] = match cleanup {
        CleanupNow::Serving { built, .. } => built,
        CleanupNow::Off | CleanupNow::Failed(_) | CleanupNow::NotReady(_) => &[],
    };
    let shared = transcription.filter(|id| built.iter().any(|c| c.provider_id() == Some(*id)));
    let (transcription_name, cleanup_label) = match shared {
        Some(id) => (
            None,
            recipients_overlay_label(
                std::iter::once((
                    // The `'static` id from the provider table, not the
                    // borrowed one: the label outlives `transcription`.
                    macalla_session::providers::find(id).map(|p| p.id),
                    std::borrow::Cow::Borrowed(crate::say::provider_name(id)),
                ))
                .chain(built.iter().map(|c| (c.provider_id(), c.destination()))),
            ),
        ),
        None => (
            transcription.map(crate::say::provider_name),
            pool_overlay_label(built),
        ),
    };
    let issue = transcription_issue
        .clone()
        .merge(cleanup_issue_parts(cleanup, ticked))
        .phrase();
    overlay_issue_line(
        engine,
        transcription_name,
        cleanup_label.as_deref(),
        issue.as_deref(),
    )
}

/// [`overlay_line`] for a dictation starting now on `session`: what
/// transcribes and why the stored transcription choice could not be used
/// come from `status`, the open-time snapshot (nothing about transcription
/// changes between opens); Enhancement is read from `session` right now
/// ([`Session::cleanup_now`]), so Apple Intelligence switched on or off
/// since the open is reflected. `ticked` is what Enhancement was built
/// from.
fn overlay_line_now(
    status: &OpenedStatus,
    session: &Session,
    ticked: &[macalla_session::choice::EnhancerChoice],
) -> Option<String> {
    overlay_line(
        &status.transcription,
        session.cloud_provider(),
        &status.transcription_parts,
        session.cleanup_now(),
        ticked,
    )
}

/// What dictation actually opened with, kept where the Models page
/// ([`crate::commands::models_overview`]) can read it -- the stored choice is
/// History's, and does not change when a key goes missing, by design:
/// re-adding the key restores it. This is the truth next to it. `None`
/// anywhere in the app until the first `open_session` has run. Every field,
/// including a `CleanupNotice::NotReady` reason, stands as it is until the
/// next open -- nothing here is re-checked between dictations. The overlay
/// reads only the transcription side from here ([`overlay_line_now`]) and
/// reads Enhancement from the session at each dictation's start.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct OpenedStatus {
    /// What transcribes right now: the cloud provider's display name when
    /// one opened, otherwise the local engine's.
    pub transcription: String,
    /// What Enhancement really sends the text to. `None` when nothing
    /// built, or when it built but is not ready to serve yet (Apple
    /// Intelligence switched off) -- not "in use" either way.
    pub enhancement: Option<String>,
    /// A short phrase naming why the stored transcription choice could not
    /// be used, e.g. "OpenAI needs a key". `None` when it opened as chosen.
    pub transcription_issue: Option<String>,
    /// A short phrase naming why Enhancement is not doing what was chosen,
    /// e.g. "Anthropic needs a key", "Apple Intelligence is not ready" --
    /// read for the Enhancement pool panel's own "Saved" state
    /// (`commands::OpenedStatusView`).
    /// `None` when Enhancement is off, or opened exactly as chosen.
    pub enhancement_issue: Option<String>,
    /// InUseCard's own sentence: the [`Opened::LocalFallback`] notice,
    /// verbatim -- already a full sentence with its own next step. `None`
    /// when transcription opened as chosen.
    pub transcription_reason: Option<String>,
    /// The Enhancement card's own sentence, from [`cleanup_notice`] (a
    /// `CleanupNotice::Failed` or `NotReady`) or the partial-pool sentence
    /// built from [`crate::say::say`] -- the page's text is always one of
    /// these existing sentences, never a new one. `None` when Enhancement
    /// opened exactly as chosen, or is off.
    pub enhancement_reason: Option<String>,
    /// The raw material behind `transcription_issue`, kept so
    /// [`overlay_line_now`] can [`IssueParts::merge`] it with Enhancement's
    /// problem as the dictation starts, and [`publish_cleanup_swap`] can
    /// carry it over, rather than re-wrapping the already-rendered phrase.
    /// Never sent to the frontend.
    #[serde(skip)]
    transcription_parts: IssueParts,
    /// Whether Enhancement, as this open or swap left it, has nothing a
    /// rebuild could repair: off, or serving every ticked choice with no
    /// notice ([`cleanup_whole`]). `false` before any session opened, and
    /// for an open with no working transcription side. Never sent to the
    /// frontend.
    #[serde(skip)]
    cleanup_whole: bool,
}

impl OpenedStatus {
    /// See the `cleanup_whole` field. Read by the Models page's commands to
    /// skip a cleanup rebuild that would change nothing.
    pub fn cleanup_whole(&self) -> bool {
        self.cleanup_whole
    }
}

/// Whether Enhancement has nothing a rebuild could repair: off, or serving
/// with no ticked choice skipped and no notice stored beside it. Failed, not
/// ready, or a pool member left out are all worth building again, since a
/// key or a system switch may have changed since.
fn cleanup_whole(now: CleanupNow<'_>, notice: Option<&CleanupNotice>) -> bool {
    notice.is_none()
        && match now {
            CleanupNow::Off => true,
            CleanupNow::Serving { skipped, .. } => skipped.is_empty(),
            CleanupNow::Failed(_) | CleanupNow::NotReady(_) => false,
        }
}

/// A short phrase for `error`, naming `provider` where one is known, fit to
/// sit inside [`overlay_issue_line`] or [`IssueParts::phrase`] -- unlike
/// [`say::say`](crate::say::say), which writes a full sentence with its own
/// next step for the Models page. Never a raw error, a raw id, or a
/// pronoun standing in for a name this function could look up itself:
/// every arm names a display name, or a fixed phrase with none to give.
fn short_session_error(provider: Option<&str>, error: &macalla_session::Error) -> String {
    use macalla_session::Error;
    match error {
        Error::NeedsKey { provider } => {
            format!("{} needs a key", crate::say::provider_name(provider))
        }
        // A stored key's own account names its provider even when the
        // caller has no hint -- looked up the same way `say::platform`'s
        // `Secret` arm does, so this never falls back to a pronoun for the
        // common case.
        Error::Platform(macalla_platform::Error::Secret { account, .. }) => {
            let name = macalla_session::providers::PROVIDERS
                .iter()
                .find(|p| p.account == account.as_str())
                .map(|p| p.name)
                .or(provider);
            match name {
                Some(name) => format!("{name}'s key could not be read"),
                None => "a key could not be read".to_string(),
            }
        }
        // Only a secret-store failure reading a key is ever "{name}'s key
        // could not be read": every other `macalla_platform::Error` a
        // session can carry here (`SecretStoreUnavailable`, or one of the
        // capture/paste/shortcut variants surfacing through a path that
        // does not actually mean the key) is not about a key at all, so it
        // reads the same as anything else this function cannot name a
        // specific reason for -- the general fallback below, not a new one.
        Error::Platform(_) => match provider {
            Some(name) => format!("{name} could not be used"),
            None => "something could not be used".to_string(),
        },
        Error::WrongKind { provider, .. } => {
            format!(
                "{} does not offer this",
                crate::say::provider_name(provider)
            )
        }
        // Never the raw stored id: a provider this build does not know is
        // never named, only that one is not. Lower-case: like every other
        // fallback here with no name to give ("a key could not be read",
        // "something could not be used"), this reads mid-clause behind
        // `IssueParts`' "needs a key" phrase or the overlay's "· " far
        // more often than it opens a sentence.
        Error::UnknownProvider(_) => "the saved provider is unknown".to_string(),
        Error::CloudNotCompiledIn => match provider {
            Some(name) => format!("{name} is not in this build"),
            // Lower-case like this function's other no-name fallbacks:
            // it reads mid-clause far more often than it opens one.
            None => "no cloud transcription in this build".to_string(),
        },
        _ => match provider {
            Some(name) => format!("{name} could not be used"),
            None => "something could not be used".to_string(),
        },
    }
}

/// Join names for [`IssueParts::phrase`]: "OpenAI", "OpenAI and
/// Anthropic", "OpenAI, Anthropic and Gemini" -- the same shape
/// [`crate::say`] and `macalla_session::cleanup::pool_label` use elsewhere
/// for a short list of names, but joined with "and" rather than "falling
/// back to": these are the ones that did *not* build, so there is no order
/// to describe.
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [first, rest @ ..] => {
            let (last, middle) = rest.split_last().expect("checked non-empty above");
            let mut joined = first.clone();
            for name in middle {
                joined.push_str(", ");
                joined.push_str(name);
            }
            joined.push_str(" and ");
            joined.push_str(last);
            joined
        }
    }
}

/// The raw material for a short issue phrase -- names that need a key,
/// deduped, and every other short phrase, deduped -- kept apart from its
/// rendered [`IssueParts::phrase`] so [`overlay_line`] can
/// [`IssueParts::merge`] a transcription problem's names with an
/// Enhancement problem's before naming any of them, rather than rendering
/// each half first and joining two "needs a key" clauses into one line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct IssueParts {
    needs_key: Vec<String>,
    other: Vec<String>,
}

impl IssueParts {
    /// Add one error, with a display-name hint for the choice that
    /// produced it (`None` when the caller has none to give;
    /// [`short_session_error`] then falls back to what the error itself
    /// names).
    fn add(&mut self, hint: Option<&str>, error: &macalla_session::Error) {
        match error {
            macalla_session::Error::NeedsKey { provider } => {
                let name = crate::say::provider_name(provider).to_string();
                if !self.needs_key.contains(&name) {
                    self.needs_key.push(name);
                }
            }
            _ => {
                let phrase = short_session_error(hint, error);
                if !self.other.contains(&phrase) {
                    self.other.push(phrase);
                }
            }
        }
    }

    /// Every name and phrase from `other`, folded in and deduped against
    /// what is already here.
    fn merge(mut self, other: Self) -> Self {
        for name in other.needs_key {
            if !self.needs_key.contains(&name) {
                self.needs_key.push(name);
            }
        }
        for phrase in other.other {
            if !self.other.contains(&phrase) {
                self.other.push(phrase);
            }
        }
        self
    }

    /// One short phrase: every needs-a-key name folded into one clause
    /// ("OpenAI needs a key" / "OpenAI and Anthropic need keys"), then
    /// every other phrase, joined with "; ". `None` when nothing was added.
    fn phrase(&self) -> Option<String> {
        let mut phrases = Vec::new();
        if !self.needs_key.is_empty() {
            phrases.push(if self.needs_key.len() == 1 {
                format!("{} needs a key", self.needs_key[0])
            } else {
                format!("{} need keys", join_names(&self.needs_key))
            });
        }
        phrases.extend(self.other.iter().cloned());
        (!phrases.is_empty()).then(|| phrases.join("; "))
    }
}

/// Every distinct sentence [`crate::say::say`] gives `errors`, in order,
/// joined with a space -- the Models page's own reuse of the same
/// sentences [`cleanup_notice`] shows on the transient banner, for a
/// problem (some, but not every, cleanup choice built) that banner has no
/// arm for.
fn cleanup_say_sentences(errors: &[macalla_session::Error]) -> String {
    let mut sentences: Vec<String> = Vec::new();
    for error in errors {
        let sentence = crate::say::say(error);
        if !sentences.contains(&sentence) {
            sentences.push(sentence);
        }
    }
    sentences.join(" ")
}

/// [`OpenedStatus`]'s Enhancement fields.
struct CleanupStatus {
    enhancement: Option<String>,
    issue: IssueParts,
    /// The card sentence: [`cleanup_notice`]'s own text for `Failed` and
    /// `NotReady`, or the partial-pool sentence built from
    /// [`crate::say::say`] -- always an existing sentence, never a new one.
    reason: Option<String>,
}

/// The overlay's short issue parts for a `Failed` Enhancement: each ticked
/// choice zipped with its error, in order, the choice's display name as the
/// hint.
fn failed_issue_parts(
    errors: &[macalla_session::Error],
    ticked: &[macalla_session::choice::EnhancerChoice],
) -> IssueParts {
    let mut issue = IssueParts::default();
    for (choice, error) in ticked.iter().zip(errors) {
        issue.add(Some(&choice.destination()), error);
    }
    issue
}

/// The overlay's short issue parts for Enhancement that built but cannot
/// serve right now. One fixed phrase for every reason: there is no `Error`
/// behind it to build one from, and it claims nothing about when it will be.
fn not_ready_issue_parts() -> IssueParts {
    let mut issue = IssueParts::default();
    issue
        .other
        .push("Apple Intelligence is not ready".to_string());
    issue
}

/// The overlay's short issue parts for ticked choices skipped beside a pool
/// that built.
fn skipped_issue_parts(skipped: &[macalla_session::Error]) -> IssueParts {
    let mut issue = IssueParts::default();
    for error in skipped {
        issue.add(None, error);
    }
    issue
}

/// Enhancement's own issue parts for [`overlay_line`], from
/// [`Session::cleanup_now`]: the same parts [`cleanup_status`] gives the
/// Models page for the same facts.
fn cleanup_issue_parts(
    cleanup: CleanupNow<'_>,
    ticked: &[macalla_session::choice::EnhancerChoice],
) -> IssueParts {
    match cleanup {
        CleanupNow::Off => IssueParts::default(),
        CleanupNow::Failed(errors) => failed_issue_parts(errors, ticked),
        // The phrase names Apple Intelligence, so it is said only for it. Any
        // other provider whose live check fails (a saved choice with no
        // model, which saving refuses) gets no issue rather than a wrong one.
        CleanupNow::NotReady(built)
            if built.iter().all(|c| {
                matches!(
                    c,
                    macalla_session::choice::EnhancerChoice::AppleIntelligence { .. }
                )
            }) =>
        {
            not_ready_issue_parts()
        }
        CleanupNow::NotReady(_) => IssueParts::default(),
        CleanupNow::Serving { skipped, .. } => skipped_issue_parts(skipped),
    }
}

/// [`CleanupStatus`] from a session's own [`Session::cleanup_error`],
/// [`Session::cleanup_skipped`] and [`Session::cleanup_destination`] --
/// taken as plain values, not `&Session`, so a test drives every branch
/// with a canned [`CleanupNotice`] or error list and never needs a live
/// session or engine.
fn cleanup_status(
    cleanup_error: Option<&CleanupNotice>,
    cleanup_skipped: &[macalla_session::Error],
    cleanup_destination: Option<&str>,
    cleanup_choices: &[macalla_session::choice::EnhancerChoice],
) -> CleanupStatus {
    match cleanup_error {
        // `cleanup_notice` already gives NotReady its own sentence, straight
        // from `Availability::explain`, fit to show as written -- the page's
        // one wording table, reused rather than duplicated.
        Some(notice) => CleanupStatus {
            enhancement: None,
            issue: match notice {
                CleanupNotice::NotReady(_) => not_ready_issue_parts(),
                CleanupNotice::Failed(errors) => failed_issue_parts(errors, cleanup_choices),
            },
            reason: Some(cleanup_notice(notice)),
        },
        None => {
            let enhancement = cleanup_destination.map(str::to_string);
            if cleanup_skipped.is_empty() {
                CleanupStatus {
                    enhancement,
                    issue: IssueParts::default(),
                    reason: None,
                }
            } else {
                // Only the existing say.rs sentences for what was skipped:
                // no new sentence is added here (user-facing wording lives in one catalogue).
                // What is in use instead is shown by the pool line, from
                // `enhancement`.
                CleanupStatus {
                    enhancement,
                    issue: skipped_issue_parts(cleanup_skipped),
                    reason: Some(cleanup_say_sentences(cleanup_skipped)),
                }
            }
        }
    }
}

/// [`OpenedStatus`] for a session that has a working transcription side --
/// `Opened::Ready` and `Opened::LocalFallback` alike, and a cleanup-only
/// swap on an already-open session ([`publish_cleanup_swap`]) -- given what
/// already differs between those: what transcribes, and why it might not be
/// the stored choice. Everything about Enhancement comes from `session`
/// itself, through [`cleanup_status`], so a swap that only touches Tier 1
/// gets the same computation `open_session` always has.
fn status_for(
    session: &Session,
    cleanup_choices: &[macalla_session::choice::EnhancerChoice],
    transcription: String,
    transcription_issue: IssueParts,
    transcription_reason: Option<String>,
) -> OpenedStatus {
    let cleanup = cleanup_status(
        session.cleanup_error(),
        session.cleanup_skipped(),
        session.cleanup_destination(),
        cleanup_choices,
    );
    OpenedStatus {
        transcription,
        enhancement: cleanup.enhancement,
        transcription_issue: transcription_issue.phrase(),
        enhancement_issue: cleanup.issue.phrase(),
        transcription_reason,
        enhancement_reason: cleanup.reason,
        transcription_parts: transcription_issue,
        cleanup_whole: cleanup_whole(session.cleanup_now(), session.cleanup_error()),
    }
}

/// After `Session::replace_cleanup`, republish `status` with fresh
/// Enhancement fields and the transcription side carried over unchanged --
/// a cleanup swap never touches the engine, so whatever `status` already
/// said about it still holds. Reuses `previous.transcription_parts` rather
/// than the already-rendered phrase, so the next dictation's
/// [`overlay_line_now`] merges it exactly as it would after a fresh open.
fn publish_cleanup_swap(
    app: &AppHandle<Wry>,
    status: &OpenedStatusShared,
    session: &Session,
    cleanup_choices: &[macalla_session::choice::EnhancerChoice],
) {
    let previous = status.get();
    // A cleanup swap only ever runs on a session `open_session` already
    // opened -- `run`'s `Command::SwitchCleanup` arm reads `session.as_mut`
    // first, and the post-dictation and Send-again paths only reach here
    // through a live `session` too -- and every open sets `status` before
    // this thread can act on anything else, so `previous` is never the
    // `None` `unwrap_or_default` below silently stands in for. Debug-only:
    // release keeps the fallback rather than panic on a status a person is
    // mid-dictation with.
    debug_assert!(
        previous.is_some(),
        "a cleanup swap must follow an open that already published `status`"
    );
    let previous = previous.unwrap_or_default();
    let refreshed = status_for(
        session,
        cleanup_choices,
        previous.transcription,
        previous.transcription_parts.clone(),
        previous.transcription_reason,
    );
    status.set(app, refreshed);
}

/// Build `choices` into `session`'s cleanup, in place, then say what came of
/// it: the refreshed [`OpenedStatus`] ([`publish_cleanup_swap`]), then the one
/// `dictation-error` [`notices_for`]'s `Ready` arm would show for a fresh
/// open, if cleanup is not cleaning now -- status before the notice, matching
/// [`emit_and_open`]'s own order (`after_open` before `notices_for`), so the
/// frontend's `dictation-status` listener records this swap's own reason
/// before a `dictation-error` repeating it arrives, not the last open's. No
/// `dictation-state: preparing` -- unlike a reopen, this never touches the
/// speech engine, so there is nothing to say is loading.
fn swap_cleanup(
    app: &AppHandle<Wry>,
    status: &OpenedStatusShared,
    session: &mut Session,
    cleanup_choices: &[macalla_session::choice::EnhancerChoice],
    keys: &dyn SecretStore,
) {
    session.replace_cleanup(cleanup_choices, keys);
    publish_cleanup_swap(app, status, session, cleanup_choices);
    if let Some(reason) = session.cleanup_error() {
        let _ = app.emit("dictation-error", cleanup_notice(reason));
    }
}

/// Whether a pending `SwitchCleanup` should be applied now, in place: only
/// when nothing else is about to reopen the session anyway. A pending
/// `SwitchEngine` rebuilds cleanup along the way when it reopens next turn
/// (`switch_after`), so applying it here too would be redundant; closing
/// skips it the same way it skips `arm`. Pure, so a test can drive every
/// combination.
fn cleanup_step(cleanup: bool, switch: bool, closing: bool) -> bool {
    cleanup && !switch && !closing
}

/// Nothing more goes to a remote provider the person switched away from
/// (nothing leaves the machine unless turned on): false once a `SwitchCleanup` (`cleanup`) or a
/// `SwitchEngine` (`switch`) is pending, because the swap or the reopen that
/// follows retires the provider. The other half of
/// [`enhancement_still_wanted`], which applies it to a remote provider only:
/// one on this Mac finishes the dictation it started. Pure, so a test drives
/// every combination.
fn keep_late_cleanup_after(cleanup: bool, switch: bool) -> bool {
    !cleanup && !switch
}

/// The one rule for whether a dictation's Enhancement is still wanted: sent,
/// waited for, and a late answer kept for History. [`give_up_on`] asks it
/// while the session waits, and [`late_cleanup_to_keep`] once the dictation
/// or the Send again is over, so the two can never disagree.
///
/// Never while the app is closing ([`keep_late_on_close`]): `closing` as the
/// caller read it, or a Shutdown already drained into `pending.closed`. And
/// not once a cleanup change or a switch is pending
/// ([`keep_late_cleanup_after`]) -- but only when `reach` is
/// [`CleanupReach::Remote`]. A provider on this Mac, or none, finishes the
/// dictation it started: nothing leaves the machine, and its worker holds no
/// key that could have gone. `reach` is the provider this dictation is
/// using, the one being retired, read before any swap ([`dictation_reach`]).
/// Pure, so a test drives every combination.
fn enhancement_still_wanted(closing: bool, pending: &Pending, reach: Option<CleanupReach>) -> bool {
    keep_late_on_close(closing || pending.closed)
        && (reach != Some(CleanupReach::Remote)
            || keep_late_cleanup_after(pending.cleanup, pending.switch))
}

/// Where a dictation's Enhancement sends the text, from the choices its
/// session actually built ([`Session::cleanup_built_choices`]): remote when
/// any of them is, on this Mac when all are, `None` when Enhancement is off.
/// Read as a dictation or a Send again starts, before anything can swap the
/// cleanup, so the answer is about the provider in use. A pool only ever
/// holds cloud models (`choice::cleanup_enhancers`), so it is always remote.
fn dictation_reach(built: &[macalla_session::choice::EnhancerChoice]) -> Option<CleanupReach> {
    if built.is_empty() {
        None
    } else if built.iter().any(|c| c.reach() == CleanupReach::Remote) {
        Some(CleanupReach::Remote)
    } else {
        Some(CleanupReach::Local)
    }
}

/// What `run`'s Send-again closure answers [`Session::send_again`] each time
/// it asks, from `pending` after the closure's own drain. Asked from History,
/// so there is no overlay and no key to hold: a press or a click, an engine
/// switch, or `row` being deleted gives the wait up -- a dictation must not
/// queue behind a minute of it (a new dictation wins, and the row keeps what
/// this Mac heard), a reopen lets the recording go anyway, and there is
/// nowhere left for the answer. Otherwise [`give_up_on`] answers as
/// for a cloud dictation: the app closing gives up, and a cleanup change
/// while a remote provider enhances goes without Enhancement, so the provider
/// the person switched away from is sent nothing as the answer arrives.
fn send_again_answer(
    pending: &Pending,
    row: crate::history::Recorded,
    reach: Option<CleanupReach>,
) -> Waiting {
    if !pending.held.is_empty() || pending.switch || pending.let_go == Some(row) {
        Waiting::GiveUp
    } else {
        // A Send again always has its row: `sendable` checked it.
        give_up_on(false, true, pending, reach, true)
    }
}

/// The one decision both the Send-again end and the dictation end make about
/// a late cleanup: drain the channel once more, so a `SwitchCleanup`, a
/// `SwitchEngine` or a Shutdown sent in the last instant is not still queued
/// when `pending` is read; then hand `row` and `late` back together only when
/// [`enhancement_still_wanted`] holds, `None` otherwise -- dropping `late` on
/// the way out. `closing` is only what the caller already knew *before* this
/// drain; a Shutdown this drain finds is read off `pending.closed`. A caller
/// with nothing earlier to add passes `quitting.load(Ordering::Relaxed)`
/// alone. `reach` is what the dictation or the send started with
/// ([`dictation_reach`]). What dropping `late` ends is `LateCleanup`'s own
/// doc; what was sent while the session waited was [`give_up_on`]'s call.
fn late_cleanup_to_keep<R, T>(
    rx: &Receiver<Command>,
    pending: &mut Pending,
    closing: bool,
    reach: Option<CleanupReach>,
    row: Option<R>,
    late: Option<T>,
) -> Option<(R, T)> {
    verdict(rx, pending);
    if enhancement_still_wanted(closing, pending, reach) {
        row.zip(late)
    } else {
        None
    }
}

/// [`OpenedStatus`] for what [`try_open`] found, with the session's own
/// stored cleanup choices for [`cleanup_status`]'s `Failed` arm -- pure, so
/// a test drives it with a canned [`Opened`] and never needs a window.
fn opened_status(
    opened: &Opened,
    cleanup_choices: &[macalla_session::choice::EnhancerChoice],
) -> OpenedStatus {
    match opened {
        Opened::Ready(session) => {
            let transcription = match session.cloud_provider() {
                Some(id) => crate::say::provider_name(id).to_string(),
                None => crate::say::engine_name(session.engine_id()),
            };
            status_for(
                session,
                cleanup_choices,
                transcription,
                IssueParts::default(),
                None,
            )
        }
        Opened::LocalFallback {
            session,
            notice,
            issue,
        } => {
            let transcription = crate::say::engine_name(session.engine_id());
            status_for(
                session,
                cleanup_choices,
                transcription,
                issue.clone(),
                Some(notice.clone()),
            )
        }
        Opened::PreviewMissing(session) => OpenedStatus {
            transcription: crate::say::engine_name(session.engine_id()),
            // Never `session.cleanup_destination()`: with no on-device
            // engine to preview with, `Session::dictate` refuses outright,
            // so Enhancement is not in use whatever it built. App.tsx's pool
            // button reads `enhancement` unset as "Saved", not "In use".
            enhancement: None,
            transcription_issue: None,
            enhancement_issue: None,
            transcription_reason: Some(preview_missing_notice(session.preview_error())),
            enhancement_reason: None,
            transcription_parts: IssueParts::default(),
            cleanup_whole: false,
        },
        Opened::Failed(message) => OpenedStatus {
            transcription: String::new(),
            enhancement: None,
            transcription_issue: None,
            enhancement_issue: None,
            transcription_reason: Some(message.clone()),
            enhancement_reason: None,
            transcription_parts: IssueParts::default(),
            cleanup_whole: false,
        },
    }
}

/// Shares the latest [`OpenedStatus`] with [`crate::commands::models_overview`]
/// and the overlay's destination line, next to [`DeviceInUse`]. Written by
/// the dictation thread on every open, read by a command; the thread also
/// emits `dictation-status` on every open (not only a changed one -- see
/// [`OpenedStatusShared::set`]). The Models page does not read it to
/// refresh itself -- `dictation-state`'s own `idle`, which always follows,
/// already does that -- but does read its payload to tell a reopen's own
/// `dictation-error`, about to repeat what this status already carries in
/// `transcription_reason`/`enhancement_reason`, from one the page's error
/// banner is the only place showing.
#[derive(Clone, Default)]
struct OpenedStatusShared(Arc<Mutex<Option<OpenedStatus>>>);

impl OpenedStatusShared {
    fn get(&self) -> Option<OpenedStatus> {
        self.0.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Remember `status` and say so, on every open -- not only a changed
    /// one: the run loop's own next read of [`OpenedStatusShared::get`],
    /// for the overlay's destination line, and the Models page's read of
    /// this same emit, must never land on the last open's answer for one
    /// already under way, which a `changed` guard risked whenever two
    /// opens in a row landed on the same status (a key added back to the
    /// same choice, for one).
    fn set(&self, app: &AppHandle<Wry>, status: OpenedStatus) {
        if let Ok(mut current) = self.0.lock() {
            *current = Some(status);
        }
        let _ = app.emit("dictation-status", self.get());
    }
}

/// Wait for Enhancement that missed the paste and put what comes back in
/// place of the History row `id`, on a thread of its own: the next key press
/// must never wait on a provider. `tier0` is the document row already holds,
/// so [`late_to_history`] can log whether the answer changed it.
fn keep_late_cleanup(
    history: HistoryHandle,
    row: crate::history::Recorded,
    tier0: macalla_core::block::Document,
    late: macalla_session::LateCleanup,
) {
    let spawned = std::thread::Builder::new()
        .name("macalla-late-cleanup".into())
        .spawn(move || {
            late_to_history(&history, row, &tier0, &late.wait());
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not wait for late cleanup; history keeps the pasted text");
    }
}

/// What [`late_to_history`] should log for a finished wait, decided apart
/// from the write so a test can drive every case. `LateDocument` never says
/// outright whether Enhancement changed the pasted text or timed out or
/// failed -- `macalla_session::LateCleanup::wait` leaves Tier 0 standing for
/// both -- so this compares the answer against `tier0`, the document the row
/// already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LateOutcome {
    /// The worker or the request stopped before every segment came back; the
    /// segments that did are kept.
    Partial,
    /// The answer differs from what was pasted: Enhancement's text is now in
    /// History.
    Changed,
    /// The answer is the same as what was pasted: Enhancement timed out or
    /// failed, or (rarely) answered that nothing needed changing -- the
    /// session does not tell those two apart once the wait is over, so
    /// either way History keeps the pasted text.
    Unchanged,
}

fn late_outcome(
    done: &macalla_session::LateDocument,
    tier0: &macalla_core::block::Document,
) -> LateOutcome {
    if done.partial {
        LateOutcome::Partial
    } else if &done.document == tier0 {
        LateOutcome::Unchanged
    } else {
        LateOutcome::Changed
    }
}

/// Put late cleanup in History in place of the pasted text. A partial result
/// -- the worker stopped before every segment came back -- goes in as well:
/// a row has no mark that says "cleaned", so it claims nothing, and the
/// segments that did come back are worth keeping. `tier0` is the document
/// the row held before this call, for [`late_outcome`].
fn late_to_history(
    history: &HistoryHandle,
    row: crate::history::Recorded,
    tier0: &macalla_core::block::Document,
    done: &macalla_session::LateDocument,
) {
    match late_outcome(done, tier0) {
        LateOutcome::Partial => tracing::info!(
            ms = done.ms,
            "cleanup stopped before every segment came back; history has what did"
        ),
        LateOutcome::Changed => tracing::info!(
            ms = done.ms,
            "cleanup finished after the paste; history has it"
        ),
        LateOutcome::Unchanged => tracing::info!(
            ms = done.ms,
            "cleanup changed nothing after the paste (timed out, failed, or nothing to change); history keeps the pasted text"
        ),
    }
    history.replace_document(row, &done.document);
}

/// Keep what the on-device engine heard in History as a cloud request goes
/// out, so the person can take it from there while the answer is slow. The
/// row it went into, or `None`: History is off, or nothing was heard. Written
/// before any answer, so it is replaced or removed once the dictation is over
/// ([`to_history`], [`HistoryHandle::forget`]).
fn keep_heard(
    history: &HistoryHandle,
    heard: &macalla_session::Dictation,
    engine: &str,
) -> Option<crate::history::Recorded> {
    // A dictation being thrown away is never written, not even until the
    // discard arm could forget it: a crash in between would keep it.
    if heard.discarded || heard.document.to_plain().trim().is_empty() {
        return None;
    }
    history.record(heard, engine)
}

/// What becomes of a dictation's History row once it is over: see
/// [`history_step`]. Decided apart from the store, so every way a dictation
/// can end is tested against the rule the run loop follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// Remove the row written while the request was out.
    Forget(crate::history::Recorded),
    /// Put the finished dictation in that row's place.
    Replace(crate::history::Recorded),
    /// Leave that row as it is: it already holds what this Mac heard.
    Leave(crate::history::Recorded),
    /// Record the dictation in a row of its own.
    Record,
    /// No row, and none to write.
    Nothing,
}

/// The History decision for a dictation that ended. `early` is the row
/// [`keep_heard`] wrote while a cloud request was out; `finished` is the
/// dictation the session returned, `None` when it failed; `esc_twice` is what
/// [`end_dictation`] said, which includes Esc twice after the session last
/// asked; `closing` is whether the command channel closed because the app is
/// quitting.
///
/// Esc twice and a failed dictation keep nothing, so an early row goes. A
/// dictation the session stopped because the app is quitting, and not for
/// Esc twice, was not thrown away by the person, so an early row stays, as it
/// would for a slow request. Any other discard keeps nothing. Otherwise a
/// transcription takes the early row's place, a failure or a timeout leaves
/// it, and with no early row the dictation is recorded as it always was. A
/// dictation with no words is not worth a row, so an answer that came back
/// empty removes its early row.
fn history_step(
    early: Option<crate::history::Recorded>,
    finished: Option<&macalla_session::Dictation>,
    esc_twice: bool,
    closing: bool,
) -> Keep {
    let Some(dictation) = finished.filter(|_| !esc_twice) else {
        return early.map_or(Keep::Nothing, Keep::Forget);
    };
    if dictation.discarded {
        return match early {
            None => Keep::Nothing,
            Some(row) if closing => Keep::Leave(row),
            Some(row) => Keep::Forget(row),
        };
    }
    let said = !dictation.document.to_plain().trim().is_empty();
    let transcribed = matches!(
        &dictation.cloud,
        Some(CloudResult {
            outcome: CloudOutcome::Transcribed,
            ..
        })
    );
    match early {
        None if said => Keep::Record,
        None => Keep::Nothing,
        Some(row) if transcribed && !said => Keep::Forget(row),
        Some(row) if transcribed => Keep::Replace(row),
        Some(row) => Keep::Leave(row),
    }
}

/// Carry out `step` for a dictation that was kept, and say which row holds
/// it now: `None` when none does -- History off, nothing said, or a row that
/// went while the request was out.
fn to_history(
    history: &HistoryHandle,
    step: Keep,
    dictation: &macalla_session::Dictation,
    engine: &str,
) -> Option<crate::history::Recorded> {
    match step {
        Keep::Record => history.record(dictation, engine),
        // The provider's words, under its name and with what it heard. A row
        // deleted, or purged by History being switched off, during the wait
        // is not written back; the answer is kept as any dictation is, which
        // with History off is nowhere. A replace that failed on an error,
        // which is logged, leaves a row that is still there: it keeps what
        // this Mac heard, rather than the answer landing in a second row
        // beside it.
        Keep::Replace(row) => {
            if history.replace_entry(row, dictation, engine) || history.holds(row) {
                Some(row)
            } else {
                history.record(dictation, engine)
            }
        }
        // Only while it is there: a row deleted during the wait is not one
        // to offer Send again on.
        Keep::Leave(row) => history.holds(row).then_some(row),
        Keep::Forget(row) => {
            history.forget(row);
            None
        }
        Keep::Nothing => None,
    }
}

/// The held row Send again may go ahead for: the one asked for, by its id and
/// its time, because a row that took a deleted row's id is another dictation,
/// and still in History, because a page left open after History was switched
/// off must not send a recording History no longer keeps. `None` otherwise.
fn sendable(
    history: &HistoryHandle,
    held: Option<crate::history::Recorded>,
    asked: crate::history::Recorded,
) -> Option<crate::history::Recorded> {
    held.filter(|row| *row == asked && history.holds(*row))
}

/// The held row, when History no longer has it: deleted from the History
/// page, or erased by a retention rule, which "Not at all" always does. Its
/// recording is then let go ([`Command::LetGo`]): there is nowhere left to
/// send it again from. `None` when nothing is held or the row is still there.
pub(crate) fn held_row_gone(
    history: &HistoryHandle,
    held: Option<crate::history::Recorded>,
) -> Option<crate::history::Recorded> {
    held.filter(|row| !history.holds(*row))
}

/// Which dictation the overlay is for, counted up as each one starts, so a
/// notice's hide, which comes seconds later from another thread, never hides
/// a later dictation's overlay and the destination label it shows. A
/// mutex rather than an atomic: a start counts and shows the overlay while
/// holding it, and a hide checks and hides while holding it, so a check that
/// passed just before a count cannot hide the overlay just after its show.
#[derive(Clone, Default)]
struct OverlayShown(Arc<Mutex<u64>>);

impl OverlayShown {
    /// A dictation starts: count it, and hold the count until the caller
    /// drops the guard. Everything the caller emits and shows while it holds
    /// the guard belongs to this dictation, so a notice from the last one
    /// that comes due in between either hides before any of it -- harmless,
    /// the page fills again at once -- or finds the count moved and hides
    /// nothing. Emitting "listening" before taking it would let that
    /// notice's `dictation-overlay-hidden` land between "listening" and
    /// `dictation-destination`, and nothing re-emits the destination.
    ///
    /// Held across `emit` and `show_overlay` on purpose. Safe because this
    /// mutex is a local of [`run`]: it is never managed state, so no
    /// command, event handler or window callback can reach it, and the only
    /// other takers are the notice threads, which nothing joins or waits on.
    /// Nothing inside the held region touches this value either, so it
    /// cannot re-enter.
    fn starting(&self) -> std::sync::MutexGuard<'_, u64> {
        let mut count = self.0.lock().unwrap_or_else(|p| p.into_inner());
        *count += 1;
        count
    }

    /// The dictation the overlay is for now.
    fn current(&self) -> u64 {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Hide with `hide` if no dictation has started since `shown`, and say
    /// whether it did.
    fn hide_if_still(&self, shown: u64, hide: impl FnOnce()) -> bool {
        let count = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let still = *count == shown;
        if still {
            hide();
        }
        still
    }
}

/// Hide the overlay once a notice has been up for [`NOTICE`], from a thread of
/// its own so the dictation thread goes straight on, and only if no dictation
/// has started since.
fn hide_notice_later(app: &AppHandle<Wry>, shown: &OverlayShown) {
    let notice = shown.current();
    let (app, shown) = (app.clone(), shown.clone());
    let spawned = std::thread::Builder::new()
        .name("macalla-overlay-notice".into())
        .spawn(move || {
            std::thread::sleep(NOTICE);
            shown.hide_if_still(notice, || hide_overlay(&app));
        });
    if let Err(e) = spawned {
        tracing::warn!(
            error = %e,
            "could not time the notice; the overlay stays up until the next dictation"
        );
    }
}

/// Let the held recording go if it is `row`'s, by its id and its time, and
/// stop offering Send again. Any other row changes nothing.
fn let_go_if_held(
    session: &Session,
    held_row: &HeldRow,
    app: &AppHandle<Wry>,
    row: crate::history::Recorded,
) {
    if held_row.get() == Some(row) {
        session.let_go();
        held_row.set(app, None);
    }
}

/// How long the dictation thread keeps the overlay up itself once a
/// dictation is over, or `None` to go straight on. Only for the cap notice.
/// The moved-on notice is hidden after the same time by [`hide_notice_later`]:
/// the person is in another app by then, and a push-to-talk there must not
/// start late, or read as nothing said, because this thread was asleep.
fn overlay_hold(capped: bool, moved_on: bool) -> Option<std::time::Duration> {
    match (capped, moved_on) {
        (true, _) => Some(NOTICE),
        (false, _) => None,
    }
}

/// What History says when Send again names a recording the session no longer
/// holds: a dictation has started since, or the engine changed.
const NO_LONGER_HELD: &str = "That recording is no longer here to send again. Macalla lets it go \
                              when the next dictation starts.";

/// What History says when Send again transcribed, but the row it was for was
/// deleted, or History switched off, while it waited.
const GONE_FROM_HISTORY: &str =
    "That dictation is no longer in History, so the new text was not kept.";

/// `history-sent-again`: what sending a row's held recording again came to.
#[derive(Debug, Clone, serde::Serialize)]
struct SentAgain {
    id: i64,
    /// Why the row's text was not replaced, in plain English; `None` when it
    /// was.
    error: Option<String>,
}

/// Put what sending the held recording again came to in History row `row`,
/// and say why its text was not replaced when it was not. `None` when it was.
/// `sent` is `None` when the session held nothing after all. Nothing is
/// pasted: the person asked from the Macalla window, which is in front.
fn sent_again_to_history(
    history: &HistoryHandle,
    row: crate::history::Recorded,
    sent: Option<macalla_session::SentAgain>,
) -> Option<String> {
    let Some(mut sent) = sent else {
        return Some(NO_LONGER_HELD.to_string());
    };
    match &sent.document {
        Some(document) if !document.to_plain().trim().is_empty() => {
            // Under the cloud engine and with what it heard, as a dictation it
            // transcribed is kept, not under the on-device engine's hearing.
            if !history.replace_sent_again(row, sent.engine, &sent.transcripts, document) {
                return Some(GONE_FROM_HISTORY.to_string());
            }
            if let Some(late) = sent.late_cleanup.take() {
                keep_late_cleanup(history.clone(), row, document.clone(), late);
            }
            None
        }
        _ => {
            if let CloudOutcome::Failed(reason) = &sent.cloud.outcome {
                tracing::warn!(error = %reason, "sending again did not transcribe it either");
            }
            Some(send_again_notice(&sent.cloud))
        }
    }
}

/// Why sending again did not replace the text, naming the provider, with a
/// plain sentence for why when it failed.
fn send_again_notice(cloud: &CloudResult) -> String {
    let name = macalla_session::providers::find(&cloud.provider)
        .map_or(cloud.provider.as_str(), |provider| provider.name);
    match &cloud.outcome {
        CloudOutcome::Failed(reason) => {
            format!(
                "{name} could not transcribe it this time either. {}",
                crate::say::cloud_failure(reason)
            )
        }
        CloudOutcome::TimedOut => format!("{name} did not answer in time. You can send it again."),
        CloudOutcome::Abandoned => format!("Stopped waiting for {name}."),
        CloudOutcome::Transcribed | CloudOutcome::NoSpeech => {
            format!("{name} heard no words in it, so the text was left as it was.")
        }
    }
}

/// Whether opening a cloud session failed for a reason specific to the
/// chosen provider -- not the local engine -- so it is worth opening again
/// with [`Config::cloud`] cleared rather than leaving the person with no
/// session at all. Every one of these can only come from building the cloud
/// engine ([`macalla_session::choice::cloud_engine`]): the local engine's own
/// failures arrive as [`macalla_session::Error::NoEngine`], never these.
fn is_cloud_reason(error: &macalla_session::Error) -> bool {
    use macalla_session::Error;
    matches!(
        error,
        Error::UnknownProvider(_)
            | Error::WrongKind { .. }
            | Error::NeedsKey { .. }
            | Error::CloudNotCompiledIn
            | Error::Platform(_)
    )
}

/// The plain sentence for a cloud-reason fallback: names the provider and
/// the local engine that is dictating instead, so a heard-but-not-pasted
/// investigation is never sent chasing "no on-device engine" for a person
/// who removed a key on purpose.
///
/// "Add the key again under Providers on the Models page" is only right
/// advice for a missing or unreadable key ([`Error::NeedsKey`],
/// [`Error::Platform`]); every other reason a cloud provider stopped being
/// usable has no key to re-enter, so those arms say only to choose another
/// provider.
fn cloud_fallback_notice(
    provider_name: &str,
    engine_name: &str,
    error: &macalla_session::Error,
) -> String {
    use macalla_session::Error;
    const RE_ENTER: &str =
        "Add the key again under Providers on the Models page, or choose another provider.";
    const ELSEWHERE: &str = "Choose another provider.";
    // Without cloud transcription in the build no other provider can work
    // either, so "choose another provider" would send the person in a
    // circle: the fix is a local engine.
    const LOCAL: &str = "Choose a local engine under Transcription on the Models page.";
    if let Error::CloudNotCompiledIn = error {
        // This build has no cloud transcription at all, so the shared
        // "{reason}, so dictation is using {engine} on this Mac." template
        // would read as "does not include ElevenLabs, so dictation is..."
        // and hide that no cloud provider was ever an option here. Fold the
        // fallback into the same sentence instead: "This build of Macalla
        // does not include cloud transcription, so ElevenLabs is not
        // available and dictation is using Apple Speech on this Mac.
        // Choose a local engine under Transcription on the Models page."
        return format!(
            "This build of Macalla does not include cloud transcription, so \
             {provider_name} is not available and dictation is using {engine_name} on this \
             Mac. {LOCAL}"
        );
    }
    let (reason, advice) = match error {
        Error::NeedsKey { .. } => (format!("{provider_name} has no key saved"), RE_ENTER),
        Error::Platform(_) => (format!("{provider_name}'s key could not be read"), RE_ENTER),
        Error::WrongKind { .. } => (
            format!("{provider_name} does not offer transcription"),
            ELSEWHERE,
        ),
        Error::UnknownProvider(_) => (
            format!("{provider_name} is not a provider Macalla knows"),
            ELSEWHERE,
        ),
        // `is_cloud_reason` is what routes an error here at all, and it
        // lists exactly the four arms above plus `CloudNotCompiledIn`
        // (handled ahead of this match) -- unreachable today, kept so this
        // stays total if that list ever grows without this one.
        _ => (format!("{provider_name} could not be used"), ELSEWHERE),
    };
    // `provider_name` is lower-case ("the saved provider") when there is no
    // name, so it also reads right mid-sentence in the `CloudNotCompiledIn`
    // arm above. Every arm here opens its own sentence with it instead, so
    // that one case needs capitalising on the way out -- a real provider
    // name (a proper noun) is already capitalised and this is a no-op for it.
    let reason = capitalise_first(&reason);
    format!("{reason}, so dictation is using {engine_name} on this Mac. {advice}")
}

/// Capitalise only the first character, leaving the rest alone. Unlike
/// `say::sentence`, this never adds a trailing full stop: what it
/// capitalises here either opens a longer sentence (`cloud_fallback_notice`)
/// or is a short phrase standing on its own with no full stop of its own to
/// add (`overlay_issue_line`), never text that is a complete sentence
/// already.
fn capitalise_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The notice for [`Opened::PreviewMissing`]: the on-device engine did not
/// open, so there is nothing for the live preview even in a cloud session,
/// and [`Session::dictate`] refuses. "No speech engine is ready yet" used to
/// stand in here and read as "you haven't downloaded a model" -- true the
/// first time, wrong when Apple Speech itself refused to open. `reason` is
/// [`Session::preview_error`], which is always an *open* failure
/// (`registry::open`'s own `Error::Stt`, from `Session::from_opened`) --
/// never a mid-run one, so this never routes it through `say::core`'s `Stt`
/// arm, which reads as "it stopped while transcribing" and would claim a
/// cause this cannot back up. The advice is the same one [`say::say`] gives
/// for [`macalla_session::Error::NoEngine`], since that is what this is.
fn preview_missing_notice(reason: Option<&macalla_core::Error>) -> String {
    let notice = "The local engine could not open, so dictation cannot start.";
    match reason {
        Some(_) => format!(
            "{notice} Choose an engine under Transcription on the Models page, and download \
             its model there if it needs one."
        ),
        None => notice.to_string(),
    }
}

/// What opening the session found, before anything is emitted or armed --
/// the AppHandle-free core of [`open_session`], so a test can drive it with
/// [`macalla_platform::secrets::MemoryStore`] and never needs a Tauri app.
enum Opened {
    /// Dictates on the config as given.
    Ready(Session),
    /// A cloud session with no local engine to preview with: it can
    /// transcribe a file, but [`Session::dictate`] refuses.
    PreviewMissing(Session),
    /// The stored cloud choice could not open for a reason specific to the
    /// provider ([`is_cloud_reason`]); opened again with `cloud` cleared.
    LocalFallback {
        session: Session,
        notice: String,
        /// The same fact as `notice`, without the long-form advice -- what
        /// [`opened_status`] builds the overlay's short phrase from. Never
        /// shown as raw parts; only [`IssueParts::phrase`] and
        /// [`IssueParts::merge`] read it.
        issue: IssueParts,
    },
    /// Nothing could be opened at all.
    Failed(String),
}

/// Try `config`, falling back to the local engine alone on a cloud-specific
/// failure. A stored cloud choice whose key is gone, or unreadable, or asks
/// for something this build cannot do, must not take dictation down with
/// it: this opens again with `config.cloud` cleared and dictates on the
/// local engine, deliberately leaving the *stored* choice alone, so adding
/// the key back restores it with nothing to re-pick. A key removed on the
/// Models page, a key removed by hand from the keychain, a rebuild without
/// the feature, or a stored id that is no longer a real provider all land
/// here; `opened_status` says what opened instead.
fn try_open(config: &Config, keys: &dyn SecretStore) -> Opened {
    match Session::open_with_keys(config.clone(), keys) {
        Ok(session) if !session.can_dictate() => Opened::PreviewMissing(session),
        Ok(session) => Opened::Ready(session),
        Err(e) if config.cloud.is_some() && is_cloud_reason(&e) => {
            let cloud = config.cloud.as_ref().expect("checked by the guard above");
            // `found_name` is `None` exactly when the stored id is no
            // longer a real provider (`Error::UnknownProvider`'s own
            // case, or config drift more broadly): `provider_name` still
            // falls back to the pronoun-free "the saved provider" for
            // `cloud_fallback_notice`, lower-case since that function
            // uses it both mid-sentence (the `CloudNotCompiledIn` arm)
            // and to open a sentence (every other arm), and only it, not
            // a real provider name, needs capitalising for the second
            // case -- `capitalise_first` does that there. The `Err(e2)`
            // arm below reads `found_name` directly instead, since its
            // own sentence already names "the saved provider" once, and
            // filling `{provider_name}` into it too would repeat that
            // exact phrase when nothing more specific is known.
            let found_name = macalla_session::providers::find(&cloud.provider).map(|p| p.name);
            let provider_name = found_name.unwrap_or("the saved provider").to_string();
            let local_config = Config {
                cloud: None,
                ..config.clone()
            };
            match Session::open_with_keys(local_config, keys) {
                Ok(session) => {
                    tracing::warn!(
                        error = %e,
                        "the saved cloud provider could not open; using the local engine"
                    );
                    let engine_name = macalla_engines::registry::find(session.engine_id())
                        .map_or_else(
                            || session.engine_id().to_string(),
                            |info| info.name.to_string(),
                        );
                    let mut issue = IssueParts::default();
                    issue.add(Some(&provider_name), &e);
                    Opened::LocalFallback {
                        notice: cloud_fallback_notice(&provider_name, &engine_name, &e),
                        issue,
                        session,
                    }
                }
                // Both failed: say why the local engine could not open, and
                // that the saved provider was no way round it either, so the
                // person is not left thinking the cloud choice still stands.
                // `e2`, the local engine's error, is the real reason and is
                // the one shown, so it is logged at `error`, same as `e`; a
                // pure `both_failed_notice` builds the sentence, since `e2`
                // is already logged here and `report` would log it again.
                Err(e2) => {
                    tracing::error!(error = %e, "the saved cloud provider could not open");
                    tracing::error!(error = %e2, "the local engine could not open either");
                    Opened::Failed(both_failed_notice(&e2, found_name))
                }
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "the session could not open");
            Opened::Failed(crate::say::say(&e))
        }
    }
}

/// [`try_open`]'s [`Opened::Failed`] notice when the stored cloud choice
/// failed for a reason of its own and the local engine it fell back to
/// failed too: `local_failure`'s own sentence
/// ([`crate::say::say`]), plus that the saved provider was
/// no way round it either, named, when `provider_name` is known. With no
/// name to give it ([`macalla_session::Error::UnknownProvider`]'s case,
/// most often), that second sentence has nothing of its own left to say,
/// so it is dropped rather than shortened into one that never existed
/// before: `local_failure`'s own sentence stands alone -- it already says
/// the local engine could not open, the one reason either version gives.
/// "The saved provider, The saved provider, could not be used either."
/// was the bug this replaces.
fn both_failed_notice(
    local_failure: &macalla_session::Error,
    provider_name: Option<&str>,
) -> String {
    match provider_name {
        Some(name) => format!(
            "{} The saved provider, {name}, could not be used either.",
            crate::say::say(local_failure)
        ),
        // No second sentence to make of "the saved provider" with no name
        // to give it: `say`'s own sentence, unchanged, already says the
        // local engine could not open, which is the one reason given
        // either way.
        None => crate::say::say(local_failure),
    }
}

/// Where `open_session` sends its `dictation-state` and `dictation-error`
/// events. The only implementation besides a test recorder is
/// `AppHandle<Wry>`, below -- this exists so a test can substitute the
/// recorder and check the exact sequence [`emit_and_open`] sends, without a
/// live window.
trait Notify {
    fn send(&self, event: &'static str, payload: String);
}

impl Notify for AppHandle<Wry> {
    fn send(&self, event: &'static str, payload: String) {
        let _ = self.emit(event, payload);
    }
}

/// The `dictation-error` payloads `open_session` emits for `opened`, in the
/// order it emits them -- pulled out of `open_session` so a test can check
/// the sequence without an `AppHandle`. Every variant emits at most one
/// today; a `Vec` keeps this total if a future outcome ever needs two.
fn notices_for(opened: &Opened) -> Vec<String> {
    match opened {
        Opened::Ready(session) => session
            .cleanup_error()
            .map(cleanup_notice)
            .into_iter()
            .collect(),
        // The raw reason is already logged once, at `info`, where the
        // session first found it (`Session::from_opened`); this only adds
        // that no dictation is possible, so it does not repeat it.
        Opened::PreviewMissing(session) => {
            vec![preview_missing_notice(session.preview_error())]
        }
        Opened::LocalFallback {
            session, notice, ..
        } => {
            // One notice, so a second cannot land on top of the first. Both
            // halves are their own full sentence -- `cloud_fallback_notice`
            // ends in a full stop and `cleanup_notice` starts with a capital
            // -- so a single space between them reads as two sentences, not
            // one run together.
            let notice = match session.cleanup_error() {
                Some(reason) => format!("{notice} {}", cleanup_notice(reason)),
                None => notice.clone(),
            };
            vec![notice]
        }
        Opened::Failed(message) => vec![message.clone()],
    }
}

/// Emits `dictation-state: preparing`, runs `open` (the potentially slow
/// part -- the first Parakeet load on a machine compiles its shaders and
/// takes around ten seconds), then emits whatever [`notices_for`] finds for
/// its result as `dictation-error` and finally `dictation-state: idle`.
/// Split out of `open_session` so a test can hand it a canned [`Opened`] and
/// a recording [`Notify`] and check the exact sequence, without a real
/// engine, config or `AppHandle`.
///
/// This, and every other `app.emit` this thread calls (including from
/// `run`'s idle emit right after `idle_step` decides to leave), is safe to
/// call while the app is quitting -- even from this, the very thread
/// `RunEvent::Exit` is waiting on -- only because `emit` does not wait on
/// the main thread: it is a non-blocking proxy send
/// (`tauri-runtime-wry`'s `send_user_message`), since this build does not
/// enable tauri's `tracing` feature (`cargo tree -e features -i tauri`
/// confirms no crate here does). Turning that feature on would need this
/// rechecked. A Rust `listen` callback, in contrast, runs synchronously on
/// whichever thread calls `emit` (`Listeners::emit_filter`), so it must
/// never call a main-thread API itself -- which is why the tray's
/// `set_text` moved off its `listen` callback and onto its own animator
/// thread (tray.rs).
fn emit_and_open(
    notify: &dyn Notify,
    open: impl FnOnce() -> Opened,
    after_open: impl FnOnce(&Opened),
) -> Opened {
    notify.send("dictation-state", "preparing".to_string());
    let opened = open();
    // Before the notices and `idle`: a page or the overlay refreshing off
    // either of those must already see the new status, not the last
    // dictation's.
    after_open(&opened);
    for notice in notices_for(&opened) {
        notify.send("dictation-error", notice);
    }
    notify.send("dictation-state", "idle".to_string());
    opened
}

/// [`emit_and_open`], recording the status with `set_status` and lending
/// what opened to the Files page once the open is done, before the notices
/// and `idle`. The page asks again on `idle`, and the loop's own `lent.set`
/// comes only after the microphone is armed, which a page's question could
/// beat. A cloud session with no preview engine, or nothing at all, lends
/// nothing, as `open_session` returns no session for either.
fn open_and_lend(
    notify: &dyn Notify,
    open: impl FnOnce() -> Opened,
    set_status: impl FnOnce(&Opened),
    lent: &LentTools,
) -> Opened {
    emit_and_open(notify, open, |opened| {
        set_status(opened);
        lent.set(match opened {
            Opened::Ready(session) | Opened::LocalFallback { session, .. } => Some(session),
            Opened::PreviewMissing(_) | Opened::Failed(_) => None,
        });
    })
}

/// Open the session, arm it, and say which device it is on. Loading a model
/// can take seconds -- the first Parakeet load on a machine compiles its
/// shaders and takes around ten -- so the window is told it is preparing,
/// and that it is idle again, whichever way it went.
fn open_session(
    app: &AppHandle<Wry>,
    config: &Config,
    device: &DeviceInUse,
    status: &OpenedStatusShared,
    lent: &LentTools,
    keys: &dyn SecretStore,
) -> Option<Session> {
    let opened = open_and_lend(
        app,
        || try_open(config, keys),
        |opened| status.set(app, opened_status(opened, &config.cleanup)),
        lent,
    );
    match opened {
        Opened::Ready(mut session) => {
            tracing::info!(
                engine = session.engine_id(),
                dictionary = session.dictionary_label(),
                "session open"
            );
            session.arm();
            device.set(app, session.device_name());
            Some(session)
        }
        // A cloud session without its on-device engine transcribes files,
        // and this app has none to give it. No arm and no "session open": it
        // is no session, and a model landing reopens it.
        Opened::PreviewMissing(session) => {
            tracing::error!(
                engine = session.engine_id(),
                "no on-device engine for the live preview, so no dictation yet"
            );
            None
        }
        Opened::LocalFallback {
            mut session,
            notice,
            ..
        } => {
            tracing::warn!(
                engine = session.engine_id(),
                notice = %notice,
                "the chosen provider could not open; falling back to the local engine"
            );
            session.arm();
            device.set(app, session.device_name());
            Some(session)
        }
        Opened::Failed(message) => {
            tracing::error!(error = %message, "no session, so no dictation yet");
            None
        }
    }
}

/// How long the overlay keeps a notice up once the dictation is over: that it
/// reached its cap, held by the dictation thread, or that its text was not
/// pasted because another app is in front, hidden by [`hide_notice_later`].
const NOTICE: std::time::Duration = std::time::Duration::from_secs(3);

/// Put the overlay at the bottom-centre of the monitor it will appear on and
/// show it. Positioned every time, because monitors come and go. On macOS
/// the overlay is normally a panel of its own (`overlay_panel`), placed and
/// shown there; Tauri's window is used only if that panel could not be made.
fn show_overlay(app: &AppHandle<Wry>) {
    #[cfg(target_os = "macos")]
    if crate::overlay_panel::in_panel() {
        crate::overlay_panel::show(app, OVERLAY_WIDTH, OVERLAY_HEIGHT, OVERLAY_BOTTOM_MARGIN);
        return;
    }
    let Some(overlay) = app.get_webview_window("overlay") else {
        return;
    };
    if let Ok(Some(monitor)) = overlay.primary_monitor() {
        let scale = monitor.scale_factor();
        let size = monitor.size().to_logical::<f64>(scale);
        let origin = monitor.position().to_logical::<f64>(scale);
        let x = origin.x + (size.width - OVERLAY_WIDTH) / 2.0;
        let y = origin.y + size.height - OVERLAY_HEIGHT - OVERLAY_BOTTOM_MARGIN;
        let _ = overlay.set_position(tauri::LogicalPosition::new(x, y));
    }
    // Tauri's `show()` makes the window key on macOS; the overlay must not
    // become key.
    #[cfg(target_os = "macos")]
    crate::overlay_panel::show_window(
        &overlay,
        OVERLAY_WIDTH,
        OVERLAY_HEIGHT,
        OVERLAY_BOTTOM_MARGIN,
    );
    #[cfg(not(target_os = "macos"))]
    let _ = overlay.show();
}

/// Hide the overlay, and tell the page it is hidden.
///
/// `dictation-overlay-hidden` is what lets the overlay hold nothing between
/// dictations. The page cannot know this on its own -- hiding happens here,
/// and every notice the page keeps after a dictation ends (not pasted, moved
/// on, Enhancement missed) is still on screen when `dictation-state: idle`
/// follows, so `idle` cannot be the moment it throws them away. This can:
/// whatever was up has just gone. Emitted from the one hide path, before the
/// hide itself and whether or not there was a window to hide, so a new hide
/// path cannot forget it.
///
/// It is not one per dictation: a notice path hides here twice, once as the
/// recording ends and again when the notice's time is up, and the first of
/// those comes before the notice exists. That is why the page keeps the
/// destination label across this event and drops it only when the next
/// dictation starts -- see `keeps` in `apps/desktop/src/overlay.tsx`.
fn hide_overlay(app: &AppHandle<Wry>) {
    let _ = app.emit("dictation-overlay-hidden", ());
    #[cfg(target_os = "macos")]
    if crate::overlay_panel::in_panel() {
        crate::overlay_panel::hide(app);
        return;
    }
    if let Some(overlay) = app.get_webview_window("overlay") {
        let _ = overlay.hide();
    }
}

/// What `run`'s idle wait does with the next command, decided before any of
/// it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleStep {
    /// Leave the loop: nothing below `command` should run.
    Leave,
    /// Handle `command` as usual.
    Continue,
}

/// Whether the top of `run`'s loop should leave before it acts on
/// `switch_after` -- deferred from an earlier turn, so `idle_step` (which
/// only ever sees the command that set it) cannot stop it -- or anything
/// else read this turn. Pure, like `idle_step`, so a test can drive it
/// directly.
fn loop_top_step(quitting: bool) -> IdleStep {
    if quitting {
        IdleStep::Leave
    } else {
        IdleStep::Continue
    }
}

/// [`Command::Shutdown`] always leaves. Once `quitting` is already true --
/// set by [`ShutdownHandle::signal`], which can run before the Shutdown
/// message a command was queued ahead of is the next one out of the channel
/// -- so does anything that would otherwise start new work: opening a model
/// on `SwitchEngine` or `ReopenEngine`, a dictation on `Start`/`Toggle`
/// (which, only on the fallback path where the overlay is not in its panel,
/// reaches `show_overlay`'s `primary_monitor` and blocks the event loop's
/// own thread), a cloud request on `SendAgain`
/// (`Session::send_again` sends before it ever checks `cancel`, so a
/// recording would go out over the network, and an abandoned answer would
/// still be written to History), or `SwitchCleanup`
/// (`Session::replace_cleanup` can block on a keychain prompt for a stored
/// key, same as opening a session always could). Pure, so a test can drive
/// every combination without a channel or an engine.
fn idle_step(command: Command, quitting: bool) -> IdleStep {
    if command == Command::Shutdown {
        return IdleStep::Leave;
    }
    let starts_work = matches!(
        command,
        Command::SwitchEngine
            | Command::SwitchCleanup
            | Command::ReopenEngine
            | Command::Start
            | Command::Toggle
            | Command::SendAgain { .. }
    );
    if quitting && starts_work {
        return IdleStep::Leave;
    }
    IdleStep::Continue
}

/// What the connected input devices and the system default looked like at
/// the idle tick before this one, so [`device_snapshot_changed`] can tell
/// whether either moved. `names` is kept in the order
/// `macalla_platform::audio::input_device_names` enumerated it -- the same
/// order the tray's Microphone submenu shows, so a reorder with the same set
/// of names still counts as a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeviceSnapshot {
    names: Vec<String>,
    default: Option<String>,
}

/// Whether the devices moved since `previous`, and the snapshot to compare
/// against next time -- pure, so it is tested without a microphone or a
/// running app. `names` is `None` exactly when this tick's enumeration
/// itself failed: treated as unknown rather than as "no devices", since a
/// transient enumeration error must never look like every microphone
/// vanishing. A `default` lookup failure is not given the same treatment:
/// like everywhere else this codebase asks for the default device
/// (`commands::microphones`), it already collapses to "no default", which is
/// a real, comparable state rather than an unknown one.
fn device_snapshot_changed(
    previous: &DeviceSnapshot,
    names: Option<&[String]>,
    default: Option<&str>,
) -> (bool, DeviceSnapshot) {
    let Some(names) = names else {
        return (false, previous.clone());
    };
    let next = DeviceSnapshot {
        names: names.to_vec(),
        default: default.map(str::to_string),
    };
    let changed = next != *previous;
    (changed, next)
}

/// State the dictation thread shares with the rest of the app: written by
/// this thread, read by [`Dictator`] and its callers. Bundled so `run` takes
/// one argument for it instead of four.
struct Shared {
    /// Read only to name a discard's cause in the log.
    cancel: CancelGate,
    device: DeviceInUse,
    status: OpenedStatusShared,
    held_row: HeldRow,
    lent: LentTools,
    quitting: Arc<AtomicBool>,
    deadline: QuitDeadline,
    microphone: MicrophoneOwner,
}

/// Say, on the overlay and the page, why a dictation trigger did nothing:
/// `refused`, a whole sentence from a catalogue. The overlay goes once the
/// sentence has been up long enough, unless a dictation starts first; the
/// page clears it when the live meeting ends (`live-meeting-ended`). Not
/// `dictation-error`: the page keeps that until the next dictation, and the
/// Models page takes it for its own.
fn refuse_dictation(app: &AppHandle<Wry>, shown: &OverlayShown, refused: &str) {
    let _ = app.emit("dictation-refused", refused);
    show_overlay(app);
    hide_notice_later(app, shown);
}

/// How a dictation turn of `run`'s loop ends: on to the next command, or
/// out of the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Turn {
    Next,
    Leave,
}

/// Run `turn`, the dictation a Start or a Toggle asks for, holding the
/// microphone for all of it, and give the microphone back once it returns
/// or panics. While a live meeting has the microphone, `turn` does not run:
/// `refuse` is told why, and this is `None`. The claim lives in here, never
/// in the caller, so no caller can drop it before its turn is over.
fn with_microphone_for_dictation<R>(
    microphone: &MicrophoneOwner,
    refuse: impl FnOnce(&'static str),
    turn: impl FnOnce() -> R,
) -> Option<R> {
    match microphone.for_dictation() {
        Ok(claim) => {
            let done = turn();
            drop(claim);
            Some(done)
        }
        Err(refused) => {
            tracing::info!("dictation refused: a live meeting has the microphone");
            refuse(refused);
            None
        }
    }
}

/// Run `turn`, a Send again from History, holding the microphone for all of
/// it, as a dictation turn does. A resend never records, but it pastes into
/// whatever is in front and is heard on the engine a live meeting's
/// microphone stream shares, so while a live meeting records it does not
/// run: this is the sentence to show on the History row instead. Only a
/// live meeting can be holding the microphone here, because dictations run
/// on this same thread, which is why any refusal is said as the meeting's
/// sentence. Moving Send again off `run`'s thread would break that: a
/// dictation's claim would then be refused with the meeting's words. The
/// claim lives in here, never in the caller.
fn with_microphone_for_send_again<R>(
    microphone: &MicrophoneOwner,
    turn: impl FnOnce() -> R,
) -> Result<R, &'static str> {
    match microphone.claim(MicrophoneUse::Dictation) {
        Ok(claim) => {
            let done = turn();
            drop(claim);
            Ok(done)
        }
        Err(_) => {
            tracing::info!("send again refused: a live meeting has the microphone");
            Err(crate::meeting_words::LIVE_BUSY)
        }
    }
}

/// The thread. Opens the session, arms the microphone, and serves one
/// dictation per Start until the app goes away.
fn run(
    app: AppHandle<Wry>,
    rx: Receiver<Command>,
    history: HistoryHandle,
    on_dictating: OnDictating,
    shared: Shared,
) {
    let Shared {
        cancel,
        device,
        status,
        held_row,
        lent,
        quitting,
        deadline,
        microphone,
    } = shared;
    // The app's own dictionary, not the working directory's. Seeded on the
    // first run; a failure here is logged and the session runs without one
    // rather than not at all.
    let dictionary = match crate::dictionary::ensure(&app) {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::error!(error = %e, "could not set up the dictionary file");
            None
        }
    };
    let mut config = Config {
        // The chosen engine, or the platform's default. Read again on every
        // switch, so the setting is the one source of truth.
        engine: history.engine_choice(),
        // The chosen provider, or none for a local-only session. Read again
        // on every switch, like the engine; the local engine stays the live
        // preview alongside it.
        cloud: cloud_config(&history),
        // The chosen microphone, or the system default. Read again on every
        // switch, like the engine.
        device: history.device_choice(),
        dictionary: dictionary.clone(),
        // The chosen cleanup, or none. Read again on every switch, like the
        // engine.
        cleanup: cleanup_config(&history),
        ..Config::default()
    };
    // The real keychain, read once: every call in this loop reads a fresh
    // value, and only the account looked up under it changes.
    let keys = SystemSecrets::new();
    // None when the engine cannot open -- off macOS, that is a model not yet
    // downloaded. The thread stays, so a download can bring it up.
    let mut session = open_session(&app, &config, &device, &status, &lent, &keys);
    let mut switch_after = false;
    // Triggers the last dictation held back for after it: a press during its
    // wait, and whatever came behind it. Read before the channel.
    let mut held: VecDeque<Command> = VecDeque::new();
    // Which dictation the overlay is for, shared with a notice's later hide.
    let overlay_shown = OverlayShown::default();
    // What the idle tick saw of the input devices last time, so it only
    // tells the tray's Microphone submenu to rebuild when the list or the
    // default actually moved. See the idle tick below and
    // `device_snapshot_changed`.
    let mut devices = DeviceSnapshot::default();

    // Read the file again after an edit. Failure keeps the dictionary the
    // session already has, and says so.
    let reload = |session: &mut Session| {
        let Some(path) = dictionary.as_deref() else {
            return;
        };
        match macalla_session::load_dictionary(Some(path), &[]) {
            Ok((dictionary, label)) => {
                tracing::info!(dictionary = %label, "dictionary reloaded");
                session.replace_dictionary(dictionary, label);
            }
            Err(e) => {
                tracing::error!(error = %e, "the edited dictionary could not be read");
                let _ = app.emit("dictation-error", crate::say::say(&e));
            }
        }
    };

    loop {
        if loop_top_step(quitting.load(Ordering::Relaxed)) == IdleStep::Leave {
            break;
        }
        if switch_after {
            switch_after = false;
            config.engine = history.engine_choice();
            config.cloud = cloud_config(&history);
            config.cleanup = cleanup_config(&history);
            // A held recording stays until the next dictation, and a rebuild
            // is not one: it goes to the new session, so Send again after a
            // corrected key sends it with that key.
            let carried = session.as_ref().and_then(Session::release_held);
            // Take back the lent tools first, or they keep the old model
            // alive past the drop. A file already running keeps its own.
            lent.set(None);
            drop(session.take()); // the old model goes before the new one loads
            session = open_session(&app, &config, &device, &status, &lent, &keys);
            let kept = match (session.as_ref(), carried) {
                (Some(session), Some(held)) => session.keep_held(held),
                _ => false,
            };
            if !kept {
                held_row.set(&app, None);
            }
        }
        // Lend the session as it is now: every open, switch and dictionary
        // reload above has happened by this point, before the wait.
        lent.set(session.as_ref());
        // Wait for a trigger. A closed channel means the app is going away.
        // While waiting, follow the input devices, so the next dictation
        // starts on a microphone that is still there.
        let command = match next_command(&mut held, &rx, IDLE_DEVICE_POLL) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(session) = session.as_mut() {
                    if session.follow_device() {
                        device.set(&app, session.device_name());
                    }
                }
                // The one non-polling signal the tray's Microphone submenu
                // has: this tick already runs every IDLE_DEVICE_POLL and
                // only while idle, so riding on it adds no timer of its own.
                // `follow_device` above re-enumerates the same two things
                // when a session is armed; duplicating the call here rather
                // than threading its readings out keeps this a desktop-only
                // change; both are the "cheap enough to ask every couple of
                // seconds" calls `input_device_names`'s own doc promises.
                let names = macalla_platform::audio::input_device_names().ok();
                let default_now = macalla_platform::audio::default_input_device_name().ok();
                let (changed, next) =
                    device_snapshot_changed(&devices, names.as_deref(), default_now.as_deref());
                if changed {
                    devices = next;
                    let _ = app.emit("dictation-devices", ());
                }
                continue;
            }
            // Nothing can say anything more; leave exactly as Shutdown does.
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // Must run before every branch below that opens a model, starts a
        // dictation or sends a cloud request -- ReopenEngine, SwitchEngine
        // (by way of switch_after), SendAgain, and Start or Toggle -- so
        // none of them can begin once the app has started quitting.
        if idle_step(command, quitting.load(Ordering::Relaxed)) == IdleStep::Leave {
            break;
        }
        if command == Command::ReopenEngine {
            // No session, or one that cannot dictate: the model that just
            // landed may be what it was missing.
            if !session.as_ref().is_some_and(Session::can_dictate) {
                lent.set(None);
                drop(session.take());
                session = open_session(&app, &config, &device, &status, &lent, &keys);
            }
            continue;
        }
        if let Command::LetGo { row } = command {
            if let Some(session) = session.as_ref() {
                let_go_if_held(session, &held_row, &app, row);
            }
            continue;
        }
        if let Command::SendAgain { row: asked } = command {
            let mut pending = Pending {
                released: true,
                ..Pending::default()
            };
            let error = with_microphone_for_send_again(&microphone, || {
                match (session.as_ref(), sendable(&history, held_row.get(), asked)) {
                    (Some(session), Some(row)) if session.can_send_again() => {
                        // Where Enhancement sends the resend's text, read before
                        // anything can swap the cleanup: see `dictation_reach`.
                        let reach = dictation_reach(session.cleanup_built_choices());
                        // Asked from History, so there is no overlay and no key to
                        // hold: `send_again_answer` reads what is pending, afresh
                        // on every ask.
                        let mut give_up = || {
                            verdict(&rx, &mut pending);
                            send_again_answer(&pending, row, reach)
                        };
                        let mut sent = session.send_again(&mut give_up, &mut |_| {});
                        if !session.can_send_again() {
                            held_row.set(&app, None);
                        }
                        // `give_up` only drains the channel when the session
                        // asks, and an answer returns without a last ask, so a
                        // `SwitchCleanup` or a `SwitchEngine` sent in the final
                        // moment is still queued -- `late_cleanup_to_keep`'s own
                        // drain catches it before `pending` is read. Dropped
                        // before `sent_again_to_history` can spawn a wait on it,
                        // its answer goes nowhere (`LateCleanup`'s own doc). The
                        // resend's text still reaches History as Tier 0, from
                        // `sent.document` above. `Some(())` stands in for a row:
                        // unlike the dictation end, there is nothing else to gate
                        // on here. No earlier `closing` read to carry forward
                        // either, so `quitting` is read fresh, matching
                        // `cleanup_step`'s own `closing` a few lines below.
                        let kept = late_cleanup_to_keep(
                            &rx,
                            &mut pending,
                            quitting.load(Ordering::Relaxed),
                            reach,
                            Some(()),
                            sent.as_mut().and_then(|s| s.late_cleanup.take()),
                        );
                        if let Some(sent) = sent.as_mut() {
                            sent.late_cleanup = kept.map(|((), late)| late);
                        }
                        sent_again_to_history(&history, row, sent)
                    }
                    _ => Some(NO_LONGER_HELD.to_string()),
                }
            })
            .unwrap_or_else(|busy| Some(busy.to_string()));
            let _ = app.emit(
                "history-sent-again",
                SentAgain {
                    id: asked.id,
                    error,
                },
            );
            // What came in during the wait is done now, as after a dictation.
            if pending.reload {
                if let Some(session) = session.as_mut() {
                    reload(session);
                }
            }
            if pending.device {
                config.device = history.device_choice();
                if let Some(session) = session.as_mut() {
                    session.set_device(config.device.clone());
                    device.set(&app, session.device_name());
                }
            }
            // A pending engine switch reopens next turn and rebuilds
            // cleanup along the way; applying it here too would be
            // redundant, so this only runs when nothing else already will.
            // `pending.closed` is this wait's own Shutdown; `quitting` can
            // already be true a moment before that lands, so both are read,
            // matching the post-dictation path below.
            if cleanup_step(
                pending.cleanup,
                pending.switch,
                pending.closed || quitting.load(Ordering::Relaxed),
            ) {
                config.cleanup = cleanup_config(&history);
                if let Some(session) = session.as_mut() {
                    // As the reopen path does: the Files page must not lend
                    // the old cleanup tools while `replace_cleanup` builds.
                    lent.set(None);
                    swap_cleanup(&app, &status, session, &config.cleanup, &keys);
                    // No dictation follows this Send-again wait to send its
                    // own `idle` -- unlike the post-dictation cleanup swap
                    // below, this is the only thing this turn does, so the
                    // Models page's own refresh needs this to know a swap
                    // happened.
                    let _ = app.emit("dictation-state", "idle");
                }
            }
            switch_after |= pending.switch;
            if let (Some(session), Some(row)) = (session.as_ref(), pending.let_go) {
                let_go_if_held(session, &held_row, &app, row);
            }
            held.extend(pending.held);
            if let Some(row) = pending.send_again {
                held.push_back(Command::SendAgain { row });
            }
            if pending.closed {
                break;
            }
            continue;
        }
        if command == Command::SwitchEngine {
            switch_after = true;
            continue;
        }
        if command == Command::SwitchCleanup {
            // Applied in place, not through `switch_after`: no reopen, so
            // the speech engine and everything else the session holds stays
            // untouched (`Session::replace_cleanup`'s doc). `continue` sends
            // execution back to the loop top, which re-lends the session
            // (`lent.set`) so a Files job started right after sees it too.
            let Some(session) = session.as_mut() else {
                // Nothing to swap -- `Opened::Failed` or `PreviewMissing`.
                // Take the same path `SwitchEngine` does, so the next open
                // picks up the new cleanup choices and republishes status.
                switch_after = true;
                continue;
            };
            // Always swapped, even right after a `SwitchEngine` reopen this
            // same turn already rebuilt cleanup for: that reopen reads
            // `config.cleanup` before `open_session`, which can take
            // seconds, and Enhancement's own controls stay live the whole
            // time (only `busy`, cleared once `cleanup_choose` returns,
            // disables them) -- so the choice it read can already be stale
            // by the time this `SwitchCleanup` was queued. The cost of
            // swapping again on purpose is one cleanup build and a keychain
            // read, no model load.
            config.cleanup = cleanup_config(&history);
            // Take back the lent tools before the swap, as the reopen
            // path does, so the Files page cannot lend the old cleanup
            // tools while `replace_cleanup` builds the new ones. The
            // loop top re-lends once this `continue` returns there.
            lent.set(None);
            swap_cleanup(&app, &status, session, &config.cleanup, &keys);
            // No dictation follows to send its own `idle` -- unlike the
            // post-dictation cleanup swap, applying this in place is the
            // only thing this turn does, so the Models page's refresh,
            // which only reads `dictation-state: idle` (App.tsx), needs
            // this to know a swap happened.
            let _ = app.emit("dictation-state", "idle");
            continue;
        }
        if command == Command::SwitchDevice {
            // Kept in the config too, so a session opened later -- after a
            // download, or an engine switch -- starts on the choice.
            config.device = history.device_choice();
            if let Some(session) = session.as_mut() {
                session.set_device(config.device.clone());
                device.set(&app, session.device_name());
            }
            continue;
        }
        if command == Command::ReloadDictionary {
            if let Some(session) = session.as_mut() {
                reload(session);
            }
            continue;
        }
        if matches!(command, Command::Stop | Command::Discard | Command::Finish) {
            continue; // a release, a discard or a finish we did not start on
        }
        // Only Start or Toggle reach here -- idle_step above already left
        // on every other command, and on these two once quitting.
        //
        // One owner of the microphone: while a live meeting records, this
        // trigger -- the shortcut, the tray's Start Dictation or the overlay
        // -- is refused and says why. Held to the end of this turn, so a
        // meeting cannot start while this dictation runs or finishes.
        let turn = with_microphone_for_dictation(
            &microphone,
            |refused| refuse_dictation(&app, &overlay_shown, refused),
            || {
                let Some(session) = session.as_mut() else {
                    let _ = app.emit(
                        "dictation-error",
                        "No speech engine is ready yet. Choose one under Transcription on the Models \
                         page, and download its model there if it needs one.",
                    );
                    return Turn::Next;
                };
                // idle_step's read of `quitting`, above, can be stale by the time
                // execution reaches here -- signal() can run in between. Read again
                // right before anything that starts a dictation: the "listening"
                // emit, and the show_overlay below it, which, only on the
                // fallback path where the overlay is not in its panel, blocks the
                // main thread on primary_monitor during Exit.
                if quitting.load(Ordering::Relaxed) {
                    return Turn::Leave;
                }
                // Before "listening", which shows the overlay's ×: a click on it
                // must always find the cancel gate open. Nothing below reads the
                // channel before the session does, so the shortcut hearing Esc a
                // few emits sooner changes nothing.
                on_dictating(true);
                // Where this dictation's words are going, for the visible
                // destination label: one typed composition ([`overlay_line_now`]) from
                // the open-time transcription facts and Enhancement as the session
                // finds it right now, never the stored choice alone. Composed before
                // the count below, because it emits nothing and there is no reason
                // to hold the overlay's count across it.
                let opened_status = status.get().unwrap_or_default();
                let line = overlay_line_now(&opened_status, session, &config.cleanup);
                // Counted before the first of this dictation's events and held until
                // the overlay is up, so a notice from the last dictation that comes
                // due now either lands before all of it or hides nothing
                // ([`OverlayShown::starting`]).
                let counted = overlay_shown.starting();
                let _ = app.emit("dictation-state", "listening");
                // The session lets a held recording go as the dictation starts, so
                // History stops offering to send it again.
                held_row.set(&app, None);
                // The overlay may have loaded after the last change was announced.
                let _ = app.emit("dictation-device", device.get());
                let _ = app.emit("dictation-destination", line);
                show_overlay(&app);
                drop(counted);

                // Whether this dictation's transcription is a cloud request, decided
                // once here rather than read again inside the closure below: it does
                // not change mid-dictation, and `give_up_on` needs it before any
                // command has arrived.
                let is_cloud = session.cloud_provider().is_some();
                // Where this dictation's Enhancement sends the text, read before
                // anything can swap it: a cleanup change is applied only once the
                // dictation is over, and what it retires is the provider in use now.
                let reach = dictation_reach(session.cleanup_built_choices());
                // Who Enhancement is with, for a note that the paste went without
                // it, read at the same moment for the same reason.
                let enhancing_with = session.cleanup_enhancing_with().cloned();
                // Whether History keeps this dictation, read as it starts: a press
                // has a remote provider sent the text only when its answer has a row
                // to go to (`give_up_on`).
                let keeps_row = history.keeps_dictations();
                // Shared by two closures: the one that ends the recording and the one
                // that, once a cloud engine has it, gives up waiting.
                let discarded = Cell::new(false);
                let pending = RefCell::new(Pending {
                    held: std::mem::take(&mut held),
                    ..Pending::default()
                });
                // A discard is said to the session as a discard, so nothing more of
                // the recording goes to cleanup and nothing is waited for.
                // Whether the overlay has been told the recording ended
                // ([`RECORDING_ENDED`]): found out by the two closures below, and at
                // the cap by `on_progress`, which is told before the engine finishes.
                // A device or engine fault mid-recording is told only when the
                // session next asks either closure; a ✓ clicked before then is a
                // Finish for a recording that has stopped anyway, and harmless.
                let told_ended = Cell::new(false);
                let tell_ended_if = |ended: bool| {
                    tell_recording_ended(ended, &told_ended, || {
                        let _ = app.emit(RECORDING_ENDED, ());
                    });
                };
                let tell_ended = || tell_ended_if(pending.borrow().released);
                let mut should_stop = || {
                    let signal = match verdict(&rx, &mut pending.borrow_mut()) {
                        Verdict::KeepGoing => StopSignal::Continue,
                        Verdict::Stop => StopSignal::Release,
                        Verdict::Discard => {
                            discarded.set(true);
                            StopSignal::Discard
                        }
                    };
                    tell_ended();
                    signal
                };
                // Asked once the key is up: once the engine has finished, before a
                // request goes out, so a recording discarded while held is never
                // sent, and through every wait after it. What it answers is
                // `give_up_on`'s, read afresh from `pending` on every ask: Esc twice
                // gives up; the app closing gives up a cloud request and delivers a
                // local dictation without waiting on Enhancement, inside the shared
                // quit deadline; a cleanup change or a switch goes without a remote
                // provider; and a press or a click for the next dictation cuts the
                // Enhancement wait short and starts once this one is over -- with
                // History on (`keeps_row`) it delivers what comes back into that
                // row now, and with History off it goes without Enhancement,
                // pasting Tier 0 at once, because a late answer would have nowhere
                // to go.
                let mut give_up = || {
                    let waiting = give_up_step(
                        &rx,
                        &mut pending.borrow_mut(),
                        &discarded,
                        is_cloud,
                        reach,
                        keeps_row,
                    );
                    tell_ended();
                    waiting
                };
                // Levels on every poll and partials as they come: the overlay's
                // waveform and its text. For the first seconds there are only levels.
                let mut capped = false;
                // The History row what the on-device engine heard went into while a
                // cloud request was out.
                let mut early_row = None;
                let local_engine = session.local_engine_id();
                let mut on_progress = |p: Progress<'_>| match p {
                    Progress::CloudSent { heard, .. } => {
                        early_row = keep_heard(&history, heard, local_engine);
                    }
                    // Almost always what arming found, but not when arming failed
                    // and the device was opened on the keypress instead.
                    Progress::Device(name) => device.set(&app, Some(name)),
                    Progress::Level(level) => {
                        let _ = app.emit("dictation-level", DictationLevel::from(level));
                    }
                    Progress::Transcript(t) if !t.is_final => {
                        let _ = app.emit("dictation-partial", t.text.clone());
                    }
                    Progress::Transcript(_) => {}
                    // The key is usually still down: say so while the person is still
                    // talking, not in a log.
                    Progress::Capped(cap) => {
                        tell_ended_if(true);
                        capped = true;
                        let _ = app.emit("dictation-capped", cap.as_secs());
                    }
                    Progress::CloudWaiting { provider, elapsed } => {
                        let name =
                            macalla_session::providers::find(provider).map_or(provider, |p| p.name);
                        let _ = app.emit(
                            "dictation-cloud-waiting",
                            CloudWaiting {
                                provider: name.to_string(),
                                elapsed_ms: elapsed.as_millis() as u64,
                                in_history: early_row.is_some(),
                            },
                        );
                    }
                    // The paste is waiting on Enhancement: say who for, and for how
                    // long, about once a second. Emitted and not waited on.
                    Progress::Enhancing { with, elapsed } => {
                        let (with, provider) = enhancing_names(with);
                        let _ = app.emit(
                            "dictation-enhancing",
                            Enhancing {
                                with,
                                provider,
                                elapsed_ms: elapsed.as_millis() as u64,
                            },
                        );
                    }
                };
                let result = session.dictate(&mut should_stop, &mut give_up, &mut on_progress);
                let mut pending = pending.into_inner();
                // Nothing said: say so on the overlay for a moment before it goes,
                // instead of vanishing as if the key had never been pressed. Not an
                // error; pressing the key by accident is common. A discarded
                // dictation is not this: Esc twice closes silently even though its
                // document is empty like a real nothing-said. `discarded` (this
                // thread's own cell) and the result's own discard flag are both
                // knowable here; a Discard `end_dictation` finds afterwards, from
                // landing after the session last asked, is not, and still flashes
                // the notice -- see `says_nothing_said`.
                let discarded_so_far = discarded.get() || matches!(&result, Ok(d) if d.discarded);
                let nothing_said = says_nothing_said(&result, discarded_so_far);
                if nothing_said {
                    let _ = app.emit("dictation-empty", ());
                    // The app is quitting: no one is left to read the overlay, and
                    // the shared quit deadline should not pay for the wait. Checked
                    // both ways -- `pending.closed` is this dictation's own
                    // Shutdown, drained from the channel; `quitting` can be true a
                    // moment earlier, set by `ShutdownHandle::signal` before the
                    // channel message it sent is necessarily the next one this
                    // dictation's own polls saw.
                    if !pending.closed && !quitting.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(1_200));
                    }
                }
                // Ended by the cap, the overlay stays up through the paste, still saying
                // so. It never takes focus, and a notice gone in the few hundred
                // milliseconds the engine takes to finish is no notice.
                if !capped {
                    hide_overlay(&app);
                }
                // The shortcut stops listening for Esc, and says whether it heard Esc
                // twice; then the last look. A discard that landed after the session
                // last asked is still this dictation's, so it is not pasted and not
                // kept, and an Enhancement answer still out goes nowhere.
                let discarded = end_dictation(&on_dictating, &rx, &mut pending, discarded.get());
                // What becomes of the row kept while a cloud request was out.
                let step = history_step(early_row, result.as_ref().ok(), discarded, pending.closed);
                // A cloud result that came back after the person moved to another app.
                let mut moved_on = false;
                // The text really went into the app in front.
                let mut inserted = false;
                // The paste was refused before anything was posted, and why: nothing
                // was touched, not the clipboard and not the app in front. Said once
                // the History row below is known, like `moved_on`.
                let mut not_delivered: Option<NotDelivered> = None;
                match result {
                    // Esc twice, or a cloud request given up because the app is
                    // closing (`give_up_on`): a local dictation is never given up
                    // this way, so it never lands here on `pending.closed` alone.
                    Ok(d) if discarded || d.discarded => {
                        // A discarded dictation is never kept, and that includes what
                        // this Mac heard, written while its request was out -- Esc
                        // twice in the wait, as the answer landed, or after the session
                        // last asked. The app closing is not a discard, and leaves it.
                        if let Keep::Forget(row) = step {
                            history.forget(row);
                        }
                        // Nor is its recording: Esc twice after the session last
                        // asked finds one held.
                        session.let_go();
                        if discarded {
                            let cause = if cancel.ended_by_click() {
                                "a click on the overlay or the tray"
                            } else {
                                "Esc twice"
                            };
                            tracing::info!(cause, "discarded");
                        } else {
                            tracing::info!("stopped: the app is closing");
                        }
                        let _ = app.emit("dictation-discarded", ());
                    }
                    Ok(mut dictation) => {
                        // Read both ways, as everywhere else: `pending.closed` is
                        // this dictation's own Shutdown, drained from the channel;
                        // `quitting` can already be true a moment before that
                        // (`ShutdownHandle::signal` sets it first). Used below to
                        // skip a paste too close to the deadline, and to skip
                        // starting late cleanup.
                        let closing = pending.closed || quitting.load(Ordering::Relaxed);
                        // The app is closing and this dictation reached here anyway:
                        // `give_up_on` delivered a local one without waiting on
                        // Enhancement, so it is about to be pasted like a normal Stop,
                        // still inside the shared quit deadline.
                        if closing {
                            tracing::info!("finished: the app is closing");
                        }
                        if let Some(fault) = &dictation.device_fault {
                            tracing::warn!(
                                fault,
                                "the microphone stopped mid-recording; kept what it heard"
                            );
                        }
                        if let Some(fault) = &dictation.engine_fault {
                            // The raw fault is already logged once, at `warn`, by
                            // the record loop when it happened (`lib.rs`'s `engine`
                            // closure); this does not repeat it.
                            tracing::warn!(
                                "the engine failed mid-recording; kept what it committed"
                            );
                            let _ = app.emit(
                                "dictation-error",
                                engine_fault_notice(dictation.cloud.as_ref(), fault),
                            );
                        }
                        // A cloud engine that neither answered nor was given up on.
                        // Said before anything else goes out, and nothing is pasted:
                        // the person decides what to do with a slow or failed request.
                        if let Some(CloudResult {
                            outcome: CloudOutcome::Failed(reason),
                            ..
                        }) = dictation.cloud.as_ref()
                        {
                            tracing::warn!(error = %reason, "the cloud engine's request failed");
                        }
                        let cloud_failed = dictation.cloud.as_ref().and_then(cloud_failure);
                        // One notice per dictation. The onboarding checklist settles
                        // on the first `dictation-error` it hears, so a second, stronger
                        // one sent after it would never be shown there.
                        if let Some(notice) = dictation.cloud.as_ref().and_then(cloud_notice) {
                            tracing::warn!(notice = %notice, "the cloud engine did not transcribe");
                            let _ = app.emit("dictation-error", notice);
                        }
                        // The app is closing: `give_up_on` already kept a cloud
                        // request from reaching here, but a local dictation still has
                        // to decide whether to paste, for two reasons neither of
                        // which apply while not closing (`paste_on_close`):
                        // - Macalla itself is the frontmost app. That only happens
                        //   when the person quit from Macalla's own window, not from
                        //   the tray while another app was in front -- pasting would
                        //   land in whatever they were last dictating into before
                        //   they switched away to quit, not where they are now.
                        // - too little of the shared quit deadline is left to start
                        //   a paste safely: it could be cut off mid-keystroke by the
                        //   process exiting under it.
                        // Either way History still gets the row below, the same as a
                        // normal paste failure.
                        let frontmost_is_self = closing && macalla_is_frontmost(&app);
                        let deadline_ok =
                            paste_before_deadline(std::time::Instant::now(), deadline.get());
                        if paste_on_close(frontmost_is_self, deadline_ok) {
                            match macalla_session::paste(&mut dictation) {
                                Ok(Paste::Nothing) => tracing::info!("no speech was detected"),
                                Ok(Paste::Withheld) => {
                                    tracing::info!(
                                        "nothing pasted: the cloud engine did not transcribe"
                                    );
                                }
                                Ok(Paste::MovedOn) => {
                                    tracing::info!("not pasted: another app is in front now");
                                    moved_on = true;
                                }
                                // Nothing was posted, because nothing could have
                                // arrived: this process is not trusted for
                                // Accessibility. Not a success, and not an error
                                // either -- the platform touched nothing and said so,
                                // and the text still goes to History below.
                                Ok(Paste::Inserted {
                                    how: Inserted::NotDelivered(why),
                                    ms,
                                }) => {
                                    not_delivered = Some(why);
                                    tracing::warn!(
                                        ?why,
                                        // Not `paste_ms`: no paste was attempted, and
                                        // the latency breakdown keeps no sample for
                                        // this. It is how long the answer took.
                                        found_out_ms = ms,
                                        release_to_text_ms = dictation.timing.total_ms,
                                        device_ms = dictation.timing.open_ms,
                                        engine_ms = dictation.timing.engine_ms,
                                        "not pasted: it could not be delivered"
                                    );
                                }
                                Ok(Paste::Inserted { how, ms }) => {
                                    inserted = true;
                                    tracing::info!(
                                        ?how,
                                        paste_ms = ms,
                                        release_to_text_ms = dictation.timing.total_ms,
                                        device_ms = dictation.timing.open_ms,
                                        engine_ms = dictation.timing.engine_ms,
                                        "pasted"
                                    );
                                }
                                // A fault now, not a refusal: no clipboard, or no
                                // input device. A missing Accessibility grant is
                                // answered above, as `NotDelivered`, since the
                                // session started mapping it. The text is not lost:
                                // it goes out on the event below for the window to
                                // show.
                                Err(e) => tracing::error!(error = %e, "could not paste"),
                            }
                        } else if frontmost_is_self {
                            tracing::info!(
                                "not pasted: Macalla is the frontmost app while closing"
                            );
                        } else {
                            tracing::warn!("not pasted: the quit deadline was too close");
                        }
                        // After the paste, never before it. A dictation with no
                        // speech is not worth a row. A cloud dictation's row may exist
                        // already, written as its request went out; after a failed
                        // request that row is what the on-device engine heard, kept
                        // under that engine's name.
                        let row = to_history(
                            &history,
                            step,
                            &dictation,
                            session.engine_id_for(&dictation),
                        );
                        // A request that failed or ran out of time leaves its
                        // recording with the session; this row is the one History
                        // offers to send it again from. With no row -- History off,
                        // or nothing said -- there is nowhere to send it from, so it
                        // goes.
                        if row.is_none() {
                            session.let_go();
                        } else if session.can_send_again() {
                            held_row.set(&app, row);
                        }
                        // Enhancement that missed the paste reaches this row when it
                        // is done. With no row -- History off -- there is nowhere for
                        // it to go, and `late_cleanup_to_keep` drops it, as it does
                        // while closing, or once a cleanup change or a switch retires
                        // the remote provider this dictation used
                        // (`enhancement_still_wanted`). Dropped unwaited, its answer
                        // goes nowhere (`crates/macalla-session/src/cleanup.rs`'s
                        // `LateCleanup` doc). `closing` was read before the paste,
                        // above; a Shutdown landing during the paste (the clipboard
                        // restore wait) still reaches `late_cleanup_to_keep`'s own
                        // drain, and `quitting` can flip a moment before that message
                        // lands, so it is read fresh here too.
                        let late_kept = match late_cleanup_to_keep(
                            &rx,
                            &mut pending,
                            closing || quitting.load(Ordering::Relaxed),
                            reach,
                            row,
                            dictation.late_cleanup.take(),
                        ) {
                            Some((row, late)) => {
                                keep_late_cleanup(
                                    history.clone(),
                                    row,
                                    dictation.document.clone(),
                                    late,
                                );
                                true
                            }
                            None => false,
                        };
                        // Said on the overlay, where the person is looking, and with
                        // whether History kept the text, because it can be off. The
                        // wording is in `paste_words`; the platform sends a reason and
                        // never a sentence.
                        if let Some(why) = not_delivered {
                            let _ = app.emit(
                                "dictation-not-pasted",
                                crate::paste_words::could_not_paste(why, row.is_some()),
                            );
                            if !pending.closed && !quitting.load(Ordering::Relaxed) {
                                show_overlay(&app);
                                hide_notice_later(&app, &overlay_shown);
                            }
                        }
                        // Said on the overlay, because the person is looking at the
                        // app they moved to. Whether History kept the text: with
                        // History off it was kept nowhere, and the overlay says that
                        // too. It goes once it has been up long enough, unless a
                        // dictation starts first; this thread does not wait for it.
                        if moved_on {
                            let _ = app.emit("dictation-moved-on", row.is_some());
                            // The app is quitting: no window is left to show the
                            // overlay to, and no later thread is worth starting to
                            // time its notice.
                            if !pending.closed && !quitting.load(Ordering::Relaxed) {
                                show_overlay(&app);
                                hide_notice_later(&app, &overlay_shown);
                            }
                        }
                        // Pasted, but without the Enhancement it waited for: said on
                        // the overlay the way the note above is, with whether an
                        // enhanced text is still on its way to History (it may yet
                        // time out or fail). Not when the wait was cut
                        // short on purpose -- a press, or a provider switched away
                        // from -- and not while closing (`enhancement_missed`).
                        if let Some(missed) = enhancement_missed(
                            dictation.enhancement,
                            enhancing_with.as_ref(),
                            inserted,
                            pending.closed || quitting.load(Ordering::Relaxed),
                            late_kept,
                        ) {
                            let _ = app.emit("dictation-enhancement-missed", missed);
                            show_overlay(&app);
                            hide_notice_later(&app, &overlay_shown);
                        }
                        // Not after a cloud failure, and not when another app is in
                        // front. The window clears its notice on this event, and the
                        // checklist would read text that was not pasted as
                        // heard-but-not-pasted and blame Accessibility.
                        if cloud_failed.is_none() && !moved_on {
                            let _ = app.emit("dictation", dictation.document.to_plain());
                        }
                    }
                    Err(e) => {
                        // Nothing returns an error once a request is out, but a row
                        // for a dictation that did not finish would not be one to
                        // keep, nor would its recording.
                        if let Keep::Forget(row) = step {
                            history.forget(row);
                        }
                        session.let_go();
                        tracing::error!(error = %e, "the dictation failed");
                        let _ = app.emit("dictation-error", crate::say::say(&e));
                    }
                }
                // `pending.closed` is this dictation's own Shutdown, drained from
                // the channel; `quitting` can already be true a moment before that,
                // set by `ShutdownHandle::signal` ahead of the message it sends --
                // so every check below reads both.
                let closing = pending.closed || quitting.load(Ordering::Relaxed);
                // Before any wait below, so the next dictation starts on an armed,
                // up-to-date session however soon it is asked for.
                if pending.reload {
                    reload(session);
                }
                // Skipped while closing: `set_device` always calls `arm`, and a
                // dictation is never going to use the microphone it would open.
                if pending.device && !closing {
                    config.device = history.device_choice();
                    session.set_device(config.device.clone());
                }
                // A pending engine switch reopens next turn and rebuilds cleanup
                // along the way; applying it here too would be redundant, and
                // `cleanup_step` skips it while closing the same way `arm` is
                // skipped below.
                if cleanup_step(pending.cleanup, pending.switch, closing) {
                    config.cleanup = cleanup_config(&history);
                    // As the reopen path does: the Files page must not lend the old
                    // cleanup tools while `replace_cleanup` builds.
                    lent.set(None);
                    swap_cleanup(&app, &status, session, &config.cleanup, &keys);
                }
                if closing {
                    // The app is quitting: nothing rearms, and no notice or idle
                    // state is worth the wait for a window that is going away.
                } else {
                    // Off the keypress, so the next start is cheap.
                    session.arm();
                    device.set(&app, session.device_name());
                    if let Some(hold) = overlay_hold(capped, moved_on) {
                        std::thread::sleep(hold);
                        hide_overlay(&app);
                    }
                    let _ = app.emit("dictation-state", "idle");
                }
                // A switch that arrived mid-dictation is taken at the top of the next
                // turn, once this borrow of the session is over.
                switch_after = pending.switch;
                // A press during the wait starts the next dictation from there.
                held = pending.held;
                if let Some(row) = pending.let_go {
                    let_go_if_held(session, &held_row, &app, row);
                }
                // Send again asked for during this dictation is answered first: the
                // recording it named went as this dictation started.
                if let Some(row) = pending.send_again {
                    held.push_front(Command::SendAgain { row });
                }
                if closing {
                    Turn::Leave
                } else {
                    Turn::Next
                }
            },
        );
        match turn {
            // Refused: a live meeting has the microphone, and said so.
            None | Some(Turn::Next) => continue,
            Some(Turn::Leave) => break,
        }
    }
    leave(&lent, &mut session);
}

/// What every way out of the loop above ends with: forget what a Files job
/// might have borrowed and drop the session, so nothing keeps the ggml
/// engine's Metal buffers alive once the thread is done. The part of a
/// clean exit that needs no `AppHandle`, so a test can drive it directly on
/// a thread of its own.
fn leave(lent: &LentTools, session: &mut Option<Session>) {
    lent.set(None);
    drop(session.take());
}

/// [`macalla_session::Loudness`], typed for the wire: which colour the
/// overlay draws a waveform bar in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum LoudnessView {
    Normal,
    Loud,
    NearClipping,
}

impl From<macalla_session::Loudness> for LoudnessView {
    fn from(loudness: macalla_session::Loudness) -> Self {
        match loudness {
            macalla_session::Loudness::Normal => LoudnessView::Normal,
            macalla_session::Loudness::Loud => LoudnessView::Loud,
            macalla_session::Loudness::NearClipping => LoudnessView::NearClipping,
        }
    }
}

/// `dictation-level`: the loudness of the audio just captured, once per
/// poll. `rms_dbfs` is the RMS in dBFS (0 at full scale, a large negative
/// number for silence), which the overlay maps to a waveform bar's height;
/// `loudness` is the colour that bar is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
struct DictationLevel {
    rms_dbfs: f32,
    loudness: LoudnessView,
}

impl From<macalla_session::Level> for DictationLevel {
    fn from(level: macalla_session::Level) -> Self {
        DictationLevel {
            rms_dbfs: level.rms_dbfs(),
            loudness: level.loudness().into(),
        }
    }
}

/// `dictation-cloud-waiting`: the recording is with a cloud provider and no
/// answer has come yet.
#[derive(Debug, Clone, serde::Serialize)]
struct CloudWaiting {
    /// The provider's display name, not its id -- the overlay shows this
    /// directly.
    provider: String,
    elapsed_ms: u64,
    /// What the on-device engine heard is in a History row while the request
    /// is out, so the overlay can send the person there. False with History
    /// off, and when it heard nothing.
    in_history: bool,
}

/// Who Enhancement is with, for the overlay to choose its sentence
/// (`apps/desktop/src/overlayWords.ts`): [`macalla_session::EnhancingWith`]
/// with a provider's id turned into its display name beside it, by
/// [`enhancing_names`]. Typed, never words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum EnhancingKind {
    AppleIntelligence,
    ServerOnThisComputer,
    Server,
    Provider,
}

/// `dictation-enhancing`: the paste is waiting on Enhancement, sent about
/// once a second while it does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct Enhancing {
    with: EnhancingKind,
    /// The provider's display name when `with` is a provider, never a model
    /// id; `None` otherwise.
    provider: Option<String>,
    elapsed_ms: u64,
}

/// Why the paste went without the Enhancement it waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum MissedReason {
    OutOfTime,
    Failed,
}

/// `dictation-enhancement-missed`: the text was pasted without the
/// Enhancement it waited for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
struct EnhancementMissed {
    reason: MissedReason,
    with: EnhancingKind,
    /// As in [`Enhancing::provider`].
    provider: Option<String>,
    /// An enhanced text is still on its way: if that request finishes, it
    /// replaces the pasted text in History; it can still time out or fail.
    in_history: bool,
}

/// [`EnhancingKind`] for `with`, and the provider's display name when it is
/// one -- by its id, as the cloud wait names a provider this build no longer
/// lists.
fn enhancing_names(with: &macalla_session::EnhancingWith) -> (EnhancingKind, Option<String>) {
    use macalla_session::EnhancingWith;
    match with {
        EnhancingWith::AppleIntelligence => (EnhancingKind::AppleIntelligence, None),
        EnhancingWith::ServerOnThisComputer => (EnhancingKind::ServerOnThisComputer, None),
        EnhancingWith::Server => (EnhancingKind::Server, None),
        EnhancingWith::Provider(id) => (
            EnhancingKind::Provider,
            Some(
                macalla_session::providers::find(id)
                    .map_or(id.as_str(), |p| p.name)
                    .to_string(),
            ),
        ),
    }
}

/// The note that a dictation was pasted without the Enhancement it waited
/// for, or `None` for no note. Only after the text really went in
/// (`inserted`: not when another app was in front, or the paste was refused),
/// and never while `closing`. Only when Enhancement ran out of time or failed:
/// cut short on purpose -- a press, or a provider switched away from -- is
/// [`macalla_session::EnhancementOutcome::Skipped`], and says nothing.
/// `in_history` is for a late answer that is still coming and kept for the
/// row (`late_kept`), which a failure never has. `with` is who Enhancement
/// was with, read as the dictation started. Pure, so a test drives every
/// case.
fn enhancement_missed(
    outcome: Option<macalla_session::EnhancementOutcome>,
    with: Option<&macalla_session::EnhancingWith>,
    inserted: bool,
    closing: bool,
    late_kept: bool,
) -> Option<EnhancementMissed> {
    use macalla_session::EnhancementOutcome;
    if !inserted || closing {
        return None;
    }
    let reason = match outcome? {
        EnhancementOutcome::OutOfTime => MissedReason::OutOfTime,
        EnhancementOutcome::Failed => MissedReason::Failed,
        EnhancementOutcome::Applied | EnhancementOutcome::Skipped => return None,
    };
    let (with, provider) = enhancing_names(with?);
    Some(EnhancementMissed {
        reason,
        with,
        provider,
        in_history: reason == MissedReason::OutOfTime && late_kept,
    })
}

/// The sentence for a cloud request that neither answered nor was given up
/// on, naming the provider and saying nothing was pasted. `None` for every
/// other outcome: a transcription is delivered, giving up is a discard, and
/// no speech is what the overlay already says.
fn cloud_failure(cloud: &CloudResult) -> Option<String> {
    let name = macalla_session::providers::find(&cloud.provider)
        .map_or(cloud.provider.as_str(), |provider| provider.name);
    match &cloud.outcome {
        CloudOutcome::Failed(reason) => Some(format!(
            "{name} could not transcribe that, so nothing was pasted. {}",
            crate::say::cloud_failure(reason)
        )),
        CloudOutcome::TimedOut => Some(format!(
            "{name} did not answer in time, so nothing was pasted."
        )),
        CloudOutcome::Transcribed | CloudOutcome::Abandoned | CloudOutcome::NoSpeech => None,
    }
}

/// The stronger notice once a cloud provider has now failed
/// [`macalla_session::REPEATED_CLOUD_FAILURES`] times in a row: names it and
/// points at the fix, rather than reporting each failure in this streak the
/// same one-off way [`cloud_failure`] reports the first. `None` short of the
/// threshold.
fn repeated_cloud_failure_notice(cloud: &CloudResult) -> Option<String> {
    if !cloud.failing_repeatedly() {
        return None;
    }
    let name = macalla_session::providers::find(&cloud.provider)
        .map_or(cloud.provider.as_str(), |provider| provider.name);
    Some(format!(
        "{name} has failed {} times in a row, so nothing was pasted. Check its key under \
         Models and your connection, or choose Local.",
        cloud.failures_in_a_row
    ))
}

/// The one `dictation-error` a cloud outcome gets: the stronger notice once
/// failures have repeated, otherwise [`cloud_failure`]'s sentence. `None`
/// whenever `cloud_failure` has nothing to say, so giving up or hearing
/// nothing during a streak is not reported as another failure.
fn cloud_notice(cloud: &CloudResult) -> Option<String> {
    let sentence = cloud_failure(cloud)?;
    Some(repeated_cloud_failure_notice(cloud).unwrap_or(sentence))
}

/// The notice for an engine that stopped partway. In a cloud dictation that
/// engine was only the live preview, and the whole recording still went to
/// the provider -- unless nothing was sent at all.
fn engine_fault_notice(cloud: Option<&CloudResult>, fault: &macalla_core::Error) -> String {
    let sentence = crate::say::core(fault);
    match cloud {
        None => format!("The engine stopped partway. What it had heard was kept. {sentence}"),
        Some(CloudResult {
            outcome: CloudOutcome::NoSpeech | CloudOutcome::Abandoned,
            ..
        }) => format!("The live preview stopped partway. {sentence}"),
        Some(cloud) => {
            let name = macalla_session::providers::find(&cloud.provider)
                .map_or(cloud.provider.as_str(), |provider| provider.name);
            format!(
                "The live preview stopped partway. The whole recording still went to {name}. \
                 {sentence}"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_session::CloudFailure;

    /// A locally-ready engine from the real registry, for a test that must
    /// exercise `try_open`'s own `registry::open` call: `try_open` takes
    /// only a `Config`, with no seam for a `FakeEngine` in its place. Every
    /// caller carries `#[cfg_attr(not(target_os = "macos"), ignore = …)]`,
    /// so this only runs where `engine-apple` is always compiled in
    /// (macalla-session's Cargo.toml, unconditional on any feature, on
    /// macOS) and needs no download -- the `.expect()` below is a real
    /// assertion there, not a guess.
    fn a_ready_local_engine() -> macalla_engines::registry::EngineInfo {
        macalla_engines::registry::engines()
            .into_iter()
            .find(|e| e.availability.is_ready())
            .expect(
                "this test needs a locally ready engine (Apple Speech, or a downloaded \
                 Parakeet/Whisper model) to open a real session",
            )
    }

    /// Every id a notice must never contain, mirroring
    /// [`crate::say::tests::forbidden_ids`]: the raw reason behind a notice
    /// is logged, never shown, so nothing here should leak its provider,
    /// engine or internal tag.
    fn forbidden_ids() -> Vec<String> {
        let mut ids: Vec<String> = macalla_session::providers::PROVIDERS
            .iter()
            .flat_map(|p| [p.id.to_string(), p.account.to_string()])
            .collect();
        ids.extend(
            macalla_engines::registry::engines()
                .into_iter()
                .map(|e| e.id.to_string()),
        );
        ids.push("registry".to_string());
        ids.push("provider-key.".to_string());
        ids
    }

    fn snapshot(names: &[&str], default: Option<&str>) -> DeviceSnapshot {
        DeviceSnapshot {
            names: names.iter().map(|n| n.to_string()).collect(),
            default: default.map(str::to_string),
        }
    }

    #[test]
    fn a_first_reading_counts_as_a_change() {
        let (changed, next) = device_snapshot_changed(
            &DeviceSnapshot::default(),
            Some(&["Built-in".to_string()]),
            Some("Built-in"),
        );
        assert!(changed);
        assert_eq!(next, snapshot(&["Built-in"], Some("Built-in")));
    }

    #[test]
    fn nothing_changed_is_not_a_change() {
        let previous = snapshot(&["Built-in", "USB Mic"], Some("Built-in"));
        let names = previous.names.clone();
        let (changed, next) =
            device_snapshot_changed(&previous, Some(&names), previous.default.as_deref());
        assert!(!changed);
        assert_eq!(next, previous);
    }

    #[test]
    fn a_device_added_is_a_change() {
        let previous = snapshot(&["Built-in"], Some("Built-in"));
        let names = ["Built-in".to_string(), "USB Mic".to_string()];
        let (changed, next) = device_snapshot_changed(&previous, Some(&names), Some("Built-in"));
        assert!(changed);
        assert_eq!(next, snapshot(&["Built-in", "USB Mic"], Some("Built-in")));
    }

    #[test]
    fn a_device_removed_is_a_change() {
        let previous = snapshot(&["Built-in", "USB Mic"], Some("Built-in"));
        let names = ["Built-in".to_string()];
        let (changed, next) = device_snapshot_changed(&previous, Some(&names), Some("Built-in"));
        assert!(changed);
        assert_eq!(next, snapshot(&["Built-in"], Some("Built-in")));
    }

    #[test]
    fn the_default_moving_to_a_different_device_is_a_change() {
        let previous = snapshot(&["Built-in", "USB Mic"], Some("Built-in"));
        let names = previous.names.clone();
        let (changed, next) = device_snapshot_changed(&previous, Some(&names), Some("USB Mic"));
        assert!(changed);
        assert_eq!(next.default.as_deref(), Some("USB Mic"));
    }

    #[test]
    fn the_default_being_renamed_is_a_change_even_with_the_same_device_list() {
        // The OS can rename a device (a Bluetooth headset re-pairing under a
        // new name) without the connected count moving; `default` alone
        // carries that.
        let previous = snapshot(&["Old Name"], Some("Old Name"));
        let names = ["Old Name".to_string()];
        let (changed, _) = device_snapshot_changed(&previous, Some(&names), Some("New Name"));
        assert!(changed);
    }

    #[test]
    fn the_same_devices_in_a_different_order_is_a_change() {
        // cpal's order is what the Microphone submenu shows, so a reorder
        // with the same set of names is a real change to the menu, not a
        // no-op to be ignored.
        let previous = snapshot(&["Built-in", "USB Mic"], Some("Built-in"));
        let names = ["USB Mic".to_string(), "Built-in".to_string()];
        let (changed, _) = device_snapshot_changed(&previous, Some(&names), Some("Built-in"));
        assert!(changed);
    }

    #[test]
    fn an_enumeration_error_is_unknown_not_a_change_and_keeps_the_old_snapshot() {
        let previous = snapshot(&["Built-in"], Some("Built-in"));
        let (changed, next) = device_snapshot_changed(&previous, None, Some("Built-in"));
        assert!(!changed);
        assert_eq!(next, previous);
    }

    fn assert_clean(sentence: &str, forbidden: &[String]) {
        assert!(!sentence.contains('`'), "{sentence:?} has a backtick");
        assert!(
            !sentence.contains("macalla "),
            "{sentence:?} names a command"
        );
        for id in forbidden {
            assert!(
                !sentence.contains(id.as_str()),
                "{sentence:?} contains the raw id {id:?}"
            );
        }
        let first = sentence.chars().next().expect("a non-empty sentence");
        assert!(
            first.is_uppercase(),
            "{sentence:?} does not start with a capital"
        );
        assert!(
            sentence.ends_with('.'),
            "{sentence:?} does not end with '.'"
        );
    }

    #[test]
    fn the_four_notices_read_as_clean_sentences() {
        let forbidden = forbidden_ids();

        let cleanup = cleanup_notice(&CleanupNotice::Failed(vec![
            macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
        ]));
        assert_clean(&cleanup, &forbidden);

        let preview = preview_missing_notice(Some(&macalla_core::Error::Stt {
            engine: "registry",
            message: "parakeet-tdt-v3 needs its model first".into(),
        }));
        assert_clean(&preview, &forbidden);

        let cloud = cloud_failure(&CloudResult {
            provider: "elevenlabs".into(),
            outcome: CloudOutcome::Failed(CloudFailure::Transcribe(macalla_core::Error::Stt {
                engine: "elevenlabs",
                message: "401 Unauthorized".into(),
            })),
            ms: 3_000.0,
            failures_in_a_row: 1,
        })
        .expect("a failure is said");
        assert_clean(&cloud, &forbidden);

        let fault = engine_fault_notice(
            None,
            &macalla_core::Error::Stt {
                engine: "registry",
                message: "the recogniser went away".into(),
            },
        );
        assert_clean(&fault, &forbidden);
    }

    #[test]
    fn the_destination_names_the_transcription_provider_then_the_cleanup() {
        assert_eq!(
            destination_label(Some("ElevenLabs"), Some("Local")).as_deref(),
            Some("ElevenLabs \u{00b7} Local")
        );
        assert_eq!(
            destination_label(None, Some("Apple Intelligence")).as_deref(),
            Some("Apple Intelligence")
        );
        assert_eq!(
            destination_label(None, Some("Your server")).as_deref(),
            Some("Your server")
        );
        assert_eq!(
            destination_label(Some("ElevenLabs"), None).as_deref(),
            Some("ElevenLabs")
        );
        assert_eq!(destination_label(None, None), None);
    }

    #[test]
    fn cleanup_config_is_empty_until_chosen_and_a_server_is_tried_as_a_keyless_endpoint() {
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let history = HistoryHandle::in_memory_for_test();
        assert!(cleanup_config(&history).is_empty());
        history
            .set_cleanup_choice(&crate::history::CleanupChoice::Server {
                address: "http://localhost:11434".into(),
                model: "granite4.1:3b".into(),
            })
            .unwrap();
        match cleanup_config(&history).as_slice() {
            [EnhancerChoice::Endpoint {
                url,
                model,
                key: KeySource::None,
                reasoning: None,
            }] => {
                assert_eq!(url, "http://localhost:11434");
                assert_eq!(model, "granite4.1:3b");
            }
            other => panic!("expected one keyless endpoint, got {other:?}"),
        }
    }

    #[test]
    fn cleanup_config_tries_a_stored_pool_in_ticked_order_with_each_providers_stored_key() {
        use crate::history::PoolEntry;
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let history = HistoryHandle::in_memory_for_test();
        history
            .set_cleanup_choice(&crate::history::CleanupChoice::Pool {
                models: vec![
                    PoolEntry {
                        provider: "anthropic".into(),
                        model: "claude-test".into(),
                    },
                    PoolEntry {
                        provider: "openai".into(),
                        model: "gpt-test".into(),
                    },
                ],
            })
            .unwrap();
        match cleanup_config(&history).as_slice() {
            [EnhancerChoice::Anthropic {
                model: first,
                key: KeySource::Stored(first_key),
                ..
            }, EnhancerChoice::Endpoint {
                model: second,
                key: KeySource::Stored(second_key),
                ..
            }] => {
                assert_eq!(
                    (first.as_str(), first_key.as_str()),
                    ("claude-test", "anthropic")
                );
                assert_eq!(
                    (second.as_str(), second_key.as_str()),
                    ("gpt-test", "openai")
                );
            }
            other => panic!("expected Anthropic then OpenAI, got {other:?}"),
        }
    }

    #[test]
    fn the_cleanup_notice_says_dictation_carries_on_and_why_not_ready() {
        assert_eq!(
            cleanup_notice(&CleanupNotice::NotReady(
                "Apple Intelligence is turned off in System Settings."
            )),
            "Enhancement isn't working right now, so your words are pasted without it. Apple \
             Intelligence is turned off in System Settings."
        );
    }

    #[test]
    fn the_cleanup_notice_shows_each_distinct_failed_choice_once() {
        // Both are real producers of a cleanup choice's own build
        // (`choice::session_enhancer`, `choice::enhancer`): a pool member
        // with no key stored for it.
        let notice = cleanup_notice(&CleanupNotice::Failed(vec![
            macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
            // The same choice, refused for the same reason a second time --
            // as two pool members named the same provider would be.
            macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
            macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        ]));
        assert!(notice.starts_with(
            "Enhancement isn't working right now, so your words are pasted without it."
        ));
        let anthropic = "Anthropic needs a key before it can be used. Add one under Providers \
                          on the Models page.";
        let openai = "OpenAI needs a key before it can be used. Add one under Providers on the \
                       Models page.";
        assert_eq!(
            notice.matches(anthropic).count(),
            1,
            "the repeated sentence is shown once: {notice}"
        );
        assert_eq!(notice.matches(openai).count(), 1, "{notice}");
    }

    #[test]
    fn cloud_config_maps_the_stored_choice_and_none_maps_to_none() {
        let history = HistoryHandle::in_memory_for_test();
        assert!(cloud_config(&history).is_none());
        history.set_cloud_choice("elevenlabs", None).unwrap();
        let cloud = cloud_config(&history).expect("a cloud choice was stored");
        assert_eq!(cloud.provider, "elevenlabs");
        assert_eq!(cloud.model, None);
    }

    // ------------------------------------------------------- is_cloud_reason

    #[test]
    fn every_cloud_engine_error_is_a_cloud_reason() {
        use macalla_session::providers::Kind;
        use macalla_session::Error;
        assert!(is_cloud_reason(&Error::NeedsKey {
            provider: "elevenlabs".into()
        }));
        assert!(is_cloud_reason(&Error::CloudNotCompiledIn));
        assert!(is_cloud_reason(&Error::UnknownProvider("watson".into())));
        assert!(is_cloud_reason(&Error::WrongKind {
            provider: "anthropic".into(),
            kind: Kind::Transcription,
        }));
    }

    #[test]
    fn a_local_engine_error_is_never_a_cloud_reason() {
        let not_cloud = macalla_session::Error::DictionaryRead {
            path: std::path::PathBuf::from("/nowhere"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        };
        assert!(!is_cloud_reason(&not_cloud));
    }

    // -------------------------------------------------- cloud_fallback_notice

    #[test]
    fn the_fallback_notice_names_the_provider_and_the_local_engine() {
        let notice = cloud_fallback_notice(
            "ElevenLabs",
            "Apple Speech",
            &macalla_session::Error::NeedsKey {
                provider: "elevenlabs".into(),
            },
        );
        assert_eq!(
            notice,
            "ElevenLabs has no key saved, so dictation is using Apple Speech on this Mac. Add \
             the key again under Providers on the Models page, or choose another provider."
        );
    }

    #[test]
    fn a_key_that_could_not_be_read_also_gets_the_re_enter_advice() {
        let notice = cloud_fallback_notice(
            "ElevenLabs",
            "Apple Speech",
            &macalla_session::Error::Platform(macalla_platform::Error::Secret {
                account: "provider-key.elevenlabs".into(),
                reason: "the item could not be found".into(),
            }),
        );
        assert_eq!(
            notice,
            "ElevenLabs's key could not be read, so dictation is using Apple Speech on this Mac. \
             Add the key again under Providers on the Models page, or choose another provider."
        );
    }

    #[test]
    fn a_build_without_cloud_transcription_points_at_a_local_engine() {
        let notice = cloud_fallback_notice(
            "ElevenLabs",
            "Apple Speech",
            &macalla_session::Error::CloudNotCompiledIn,
        );
        assert_eq!(
            notice,
            "This build of Macalla does not include cloud transcription, so ElevenLabs is not \
             available and dictation is using Apple Speech on this Mac. Choose a local engine \
             under Transcription on the Models page."
        );
    }

    #[test]
    fn a_non_key_reason_only_says_to_choose_another_provider() {
        for error in [
            macalla_session::Error::WrongKind {
                provider: "anthropic".into(),
                kind: macalla_session::providers::Kind::Transcription,
            },
            macalla_session::Error::UnknownProvider("watson".into()),
        ] {
            let notice = cloud_fallback_notice("ElevenLabs", "Apple Speech", &error);
            assert!(
                notice.ends_with("on this Mac. Choose another provider."),
                "{error:?}: {notice}"
            );
            assert!(!notice.contains("Add the key again"), "{error:?}: {notice}");
        }
    }

    // ------------------------------------------------------------- try_open

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn a_stored_cloud_choice_with_no_key_falls_back_to_the_local_engine_and_says_so() {
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        let config = Config {
            engine: Some(engine.id.to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "elevenlabs".to_string(),
                model: None,
            }),
            ..Config::default()
        };
        match try_open(&config, &store) {
            Opened::LocalFallback {
                session, notice, ..
            } => {
                assert_eq!(session.engine_id(), engine.id);
                assert_eq!(
                    session.cloud_provider(),
                    None,
                    "the overlay names what opened, not the stored choice"
                );
                assert!(notice.contains("ElevenLabs"), "{notice}");
                assert!(notice.contains(engine.name), "{notice}");
                // Without the cloud-transcription feature, cloud_engine
                // refuses every provider with CloudNotCompiledIn before it
                // ever asks whether a key is stored (choice.rs's
                // `#[cfg(not(feature = "cloud-transcription"))] cloud_engine`),
                // so the notice differs from the default build's.
                #[cfg(not(feature = "cloud-transcription"))]
                assert_eq!(
                    notice,
                    format!(
                        "This build of Macalla does not include cloud transcription, so \
                         ElevenLabs is not available and dictation is using {} on this Mac. \
                         Choose a local engine under Transcription on the Models page.",
                        engine.name
                    )
                );
                #[cfg(feature = "cloud-transcription")]
                assert!(
                    notice.contains("Add the key again under Providers on the Models page"),
                    "{notice}"
                );
            }
            Opened::Ready(_) => panic!("expected a fallback, not a ready cloud session"),
            Opened::PreviewMissing(_) => panic!("expected a fallback, not a missing preview"),
            Opened::Failed(message) => panic!("expected a fallback, not a failure: {message}"),
        }
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn with_no_stored_cloud_choice_a_ready_local_engine_opens_as_usual() {
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        let config = Config {
            engine: Some(engine.id.to_string()),
            ..Config::default()
        };
        match try_open(&config, &store) {
            Opened::Ready(session) => assert_eq!(session.engine_id(), engine.id),
            Opened::LocalFallback { .. } => panic!("no cloud choice was stored"),
            Opened::PreviewMissing(_) => panic!("a local engine always previews itself"),
            Opened::Failed(message) => panic!("a ready engine should open: {message}"),
        }
    }

    #[test]
    fn both_failed_notice_adds_no_sentence_with_no_name_to_give_it() {
        let local_failure = macalla_session::Error::NoEngine(macalla_core::Error::Unsupported {
            engine: "fake",
            what: "opening",
        });
        // The stored provider id is not one Macalla knows
        // (`Error::UnknownProvider`'s case, most often): no real name to
        // name the saved provider by, so nothing is added -- `say`'s own
        // sentence for the local failure stands alone, word for word,
        // never a shortened second sentence that never existed before
        // ("The saved provider could not be used either.", with no name,
        // was the fix this replaces).
        let unknown = both_failed_notice(&local_failure, None);
        assert_eq!(unknown, crate::say::say(&local_failure));
        assert!(!unknown.contains("saved provider"), "{unknown}");

        // A real name still gets the appositive, unchanged.
        let named = both_failed_notice(&local_failure, Some("ElevenLabs"));
        assert!(
            named.ends_with("The saved provider, ElevenLabs, could not be used either."),
            "{named}"
        );
    }

    #[test]
    // Needs a real cloud engine to build; without the feature every
    // provider refuses with `CloudNotCompiledIn` before a key is ever
    // read (see `a_stored_cloud_choice_with_no_key_falls_back_…`).
    #[cfg(feature = "cloud-transcription")]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn a_removed_then_readded_key_lets_the_stored_cloud_choice_open_again() {
        // commands.rs's `provider_key_remove_with` never clears the stored
        // choice, by design: the choice on `config.cloud` here
        // stands for that -- unchanged across the round trip -- and this
        // shows what removing, then restoring, the key underneath it does
        // to the session `try_open` builds from it.
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        let config = Config {
            engine: Some(engine.id.to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "openai".to_string(),
                model: None,
            }),
            ..Config::default()
        };

        // No key: falls back, but the choice above is what `open_session`
        // still reads on the next open -- nothing here rewrites it.
        assert!(matches!(
            try_open(&config, &store),
            Opened::LocalFallback { .. }
        ));

        // The key comes back (`provider_key_save`'s own path): the same
        // stored choice opens as it was always meant to, no re-pick.
        macalla_session::providers::store_key(&store, "openai", "sk-test-again").unwrap();
        match try_open(&config, &store) {
            Opened::Ready(session) => {
                assert_eq!(session.cloud_provider(), Some("openai"));
            }
            Opened::LocalFallback { notice, .. } => {
                panic!("the restored key should open the stored cloud choice: {notice}")
            }
            Opened::PreviewMissing(_) => panic!("a cloud session previews on the local engine"),
            Opened::Failed(message) => panic!("the restored key should open: {message}"),
        }
    }

    // -------------------------------------------------------- opened_status

    // Every test below drives `short_session_error`, `IssueParts`,
    // `cleanup_status`, `overlay_issue_line` and `opened_status`'s
    // `Ready`/`PreviewMissing`/`Failed` arms with plain values or a
    // `FakeEngine`-backed session -- no registry lookup -- so none of them
    // needs a locally ready engine the way the `try_open`-driven tests
    // above do (`a_ready_local_engine`, asserting on macOS and marked
    // `ignore` elsewhere). A handful of `LocalFallback` scenarios still
    // need `try_open` against a real engine, guarded the same way, since
    // `Opened::LocalFallback` is only ever built there.

    #[test]
    fn join_names_reads_like_a_list() {
        assert_eq!(join_names(&[]), "");
        assert_eq!(join_names(&["OpenAI".to_string()]), "OpenAI");
        assert_eq!(
            join_names(&["OpenAI".to_string(), "Anthropic".to_string()]),
            "OpenAI and Anthropic"
        );
        assert_eq!(
            join_names(&[
                "OpenAI".to_string(),
                "Anthropic".to_string(),
                "Gemini".to_string()
            ]),
            "OpenAI, Anthropic and Gemini"
        );
    }

    #[test]
    fn short_session_error_names_the_provider_for_a_needed_key() {
        assert_eq!(
            short_session_error(
                Some("OpenAI"),
                &macalla_session::Error::NeedsKey {
                    provider: "openai".into()
                }
            ),
            "OpenAI needs a key"
        );
    }

    #[test]
    fn short_session_error_names_a_wrong_kind() {
        assert_eq!(
            short_session_error(
                None,
                &macalla_session::Error::WrongKind {
                    provider: "elevenlabs".into(),
                    kind: macalla_session::providers::Kind::Text,
                }
            ),
            "ElevenLabs does not offer this"
        );
    }

    #[test]
    fn short_session_error_never_names_an_unknown_provider_by_its_raw_id() {
        assert_eq!(
            short_session_error(
                None,
                &macalla_session::Error::UnknownProvider("watson".into())
            ),
            "the saved provider is unknown"
        );
    }

    #[test]
    fn the_unknown_provider_phrase_stays_lower_case_mid_sentence() {
        // `IssueParts::phrase` joins the needs-a-key clause first, so
        // "the saved provider is unknown" reads correctly lower-case
        // here, after "; " -- the far commoner case, and the one
        // `short_session_error` itself is written for.
        let mut issue = IssueParts::default();
        issue.add(
            Some("OpenAI"),
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        issue.add(
            None,
            &macalla_session::Error::UnknownProvider("watson".into()),
        );
        assert_eq!(
            issue.phrase().as_deref(),
            Some("OpenAI needs a key; the saved provider is unknown")
        );
    }

    #[test]
    fn short_session_error_only_reads_a_secret_error_as_a_key_problem() {
        // A `macalla_platform::Error` that is not about a key at all --
        // here, the store itself unreachable -- must not read "…'s key
        // could not be read", which would claim something this error never
        // said. It falls back to the same general phrase
        // `Error::UnknownProvider` and the wildcard arm already use.
        let unrelated =
            macalla_session::Error::Platform(macalla_platform::Error::SecretStoreUnavailable(
                "the keychain could not be reached".into(),
            ));
        assert_eq!(
            short_session_error(Some("OpenAI"), &unrelated),
            "OpenAI could not be used"
        );
        assert_eq!(
            short_session_error(None, &unrelated),
            "something could not be used"
        );
    }

    #[test]
    fn short_session_error_shortens_cloud_not_compiled_in() {
        assert_eq!(
            short_session_error(
                Some("ElevenLabs"),
                &macalla_session::Error::CloudNotCompiledIn
            ),
            "ElevenLabs is not in this build"
        );
        assert_eq!(
            short_session_error(None, &macalla_session::Error::CloudNotCompiledIn),
            "no cloud transcription in this build"
        );
    }

    #[test]
    fn short_session_error_names_the_provider_a_platform_secret_error_names_by_its_account() {
        // No hint from the caller: the account on the error itself is
        // enough, the same lookup `say::platform`'s `Secret` arm uses, so
        // this never falls back to a pronoun for the common case.
        assert_eq!(
            short_session_error(
                None,
                &macalla_session::Error::Platform(macalla_platform::Error::Secret {
                    account: "provider-key.openai".into(),
                    reason: "the item could not be found".into(),
                })
            ),
            "OpenAI's key could not be read"
        );
    }

    #[test]
    fn short_session_error_falls_back_without_a_pronoun_when_nothing_names_the_provider() {
        let unnamed = macalla_session::Error::Platform(macalla_platform::Error::Secret {
            account: "not-a-known-account".into(),
            reason: "the item could not be found".into(),
        });
        assert_eq!(
            short_session_error(None, &unnamed),
            "a key could not be read"
        );
        assert!(!short_session_error(None, &unnamed).starts_with("Its"));
        assert!(!short_session_error(None, &unnamed).starts_with("That"));
    }

    #[test]
    fn issue_parts_phrase_is_none_when_nothing_was_added() {
        assert_eq!(IssueParts::default().phrase(), None);
    }

    #[test]
    fn issue_parts_folds_needs_key_names_and_keeps_other_phrases_once_each() {
        let mut parts = IssueParts::default();
        parts.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        parts.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
        );
        // A provider ticked twice: the same `NeedsKey` twice, named once.
        parts.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        parts.add(
            None,
            &macalla_session::Error::WrongKind {
                provider: "elevenlabs".into(),
                kind: macalla_session::providers::Kind::Text,
            },
        );
        assert_eq!(
            parts.phrase().as_deref(),
            Some("OpenAI and Anthropic need keys; ElevenLabs does not offer this")
        );
    }

    #[test]
    fn issue_parts_merge_dedupes_a_name_common_to_both_sides() {
        let mut mine = IssueParts::default();
        mine.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        let mut theirs = IssueParts::default();
        theirs.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        theirs.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
        );
        assert_eq!(
            mine.merge(theirs).phrase().as_deref(),
            Some("OpenAI and Anthropic need keys")
        );
    }

    #[test]
    fn overlay_issue_line_with_no_issue_is_the_label_alone() {
        assert_eq!(overlay_issue_line("Apple Speech", None, None, None), None);
        assert_eq!(
            overlay_issue_line("Apple Speech", None, Some("Anthropic"), None).as_deref(),
            Some("Anthropic")
        );
        assert_eq!(
            overlay_issue_line("Apple Speech", Some("ElevenLabs"), Some("Anthropic"), None)
                .as_deref(),
            Some("ElevenLabs \u{00b7} Anthropic")
        );
    }

    #[test]
    fn overlay_issue_line_joins_the_bare_engine_name_with_the_issue_when_short_enough() {
        assert_eq!(
            overlay_issue_line("Apple Speech", None, None, Some("OpenAI needs a key")).as_deref(),
            Some("Apple Speech \u{00b7} OpenAI needs a key")
        );
    }

    #[test]
    fn overlay_issue_line_prefers_the_real_destination_label_over_the_bare_engine_name() {
        // The transcription key is gone, but Enhancement still receives
        // every dictation at Anthropic -- the line must name that, not
        // just the engine that is now transcribing instead.
        assert_eq!(
            overlay_issue_line(
                "Apple Speech",
                None,
                Some("Anthropic"),
                Some("OpenAI needs a key")
            )
            .as_deref(),
            Some("Anthropic \u{00b7} OpenAI needs a key")
        );
    }

    #[test]
    fn overlay_issue_line_never_drops_the_cleanup_label_even_past_the_limit() {
        let long_issue = "Your server at a very long hostname example dot com needs a key";
        assert!(
            format!("Anthropic \u{00b7} {long_issue}").chars().count() > OVERLAY_LINE_MAX_CHARS
        );
        assert_eq!(
            overlay_issue_line("Apple Speech", None, Some("Anthropic"), Some(long_issue))
                .as_deref(),
            Some("Anthropic")
        );
    }

    /// One cleanup pool member, built the way a real ticked model is: an
    /// `EnhancerChoice::Endpoint` at the provider's own address for the
    /// three that route through it, or `EnhancerChoice::Anthropic` --
    /// `choice::pool_route`'s own four, the only ones a pool can ever hold.
    #[test]
    fn only_apple_intelligence_is_called_not_ready() {
        let apple = [macalla_session::choice::EnhancerChoice::AppleIntelligence { guided: false }];
        assert_eq!(
            cleanup_issue_parts(CleanupNow::NotReady(&apple), &apple).phrase(),
            Some("Apple Intelligence is not ready".to_string())
        );
        // Any other provider whose live check fails gets no issue, never a
        // sentence that names Apple Intelligence.
        let server = [pool_choice("openai", "gpt-5.6")];
        assert_eq!(
            cleanup_issue_parts(CleanupNow::NotReady(&server), &server).phrase(),
            None
        );
    }

    fn pool_choice(provider: &str, model: &str) -> macalla_session::choice::EnhancerChoice {
        use macalla_session::choice::{EnhancerChoice, KeySource};
        if provider == "anthropic" {
            return EnhancerChoice::Anthropic {
                model: model.to_string(),
                key: KeySource::Stored("anthropic".into()),
                reasoning: None,
            };
        }
        let url = match provider {
            "openai" => "https://api.openai.com/v1",
            "gemini" => "https://generativelanguage.googleapis.com/v1beta/openai",
            "mistral" => "https://api.mistral.ai/v1",
            other => panic!("not a pool-eligible provider: {other}"),
        };
        EnhancerChoice::Endpoint {
            url: url.to_string(),
            model: model.to_string(),
            key: KeySource::Stored(provider.into()),
            reasoning: None,
        }
    }

    /// `IssueParts` with one needs-a-key name per provider id, as
    /// `IssueParts::add` gives it for a real `Error::NeedsKey`.
    fn needs_keys(providers: &[&str]) -> IssueParts {
        let mut parts = IssueParts::default();
        for provider in providers {
            parts.add(
                None,
                &macalla_session::Error::NeedsKey {
                    provider: (*provider).to_string(),
                },
            );
        }
        parts
    }

    /// `Error::NeedsKey` for each provider id, in order.
    fn needs_key_errors(providers: &[&str]) -> Vec<macalla_session::Error> {
        providers
            .iter()
            .map(|provider| macalla_session::Error::NeedsKey {
                provider: (*provider).to_string(),
            })
            .collect()
    }

    #[test]
    fn overlay_line_never_exceeds_the_limit_across_pool_shapes() {
        // Anthropic first: the longest bare name among the four
        // `choice::pool_route` lets into a pool at all (OpenAI, Anthropic,
        // Gemini, Mistral) -- 9 chars, 50.99px, the one that stresses the
        // first member's own width, which `pool_overlay_label` never
        // drops. Four members: a pool of all four is the widest count
        // ("+3") a pool can reach. Every member still carries its own real
        // model id (`pool_choice` builds a real `EnhancerChoice`), even
        // though the overlay's own form never shows one.
        let members = [
            pool_choice("anthropic", "claude-opus-5"),
            pool_choice("mistral", "mistral-medium-latest"),
            pool_choice("gemini", "gemini-3.5-flash-lite"),
            pool_choice("openai", "gpt-5.6-thinking"),
        ];
        let member_ids = ["anthropic", "mistral", "gemini", "openai"];
        // Every local engine's own card name (`registry::engines`) and
        // every cloud transcription provider -- `Kind::Transcription` in
        // `providers::PROVIDERS` -- as the two things that can open this
        // line.
        let local_engines: Vec<&'static str> = macalla_engines::registry::engines()
            .into_iter()
            .map(|e| e.name)
            .collect();
        let cloud_transcription_providers: Vec<&'static str> =
            macalla_session::providers::PROVIDERS
                .iter()
                .filter(|p| p.offers(macalla_session::providers::Kind::Transcription))
                .map(|p| p.id)
                .collect();
        // Transcription as a session gives it: opened in the cloud with no
        // issue, or on this machine, either as chosen or because one cloud
        // provider needs a key (`Opened::LocalFallback`).
        let mut transcriptions: Vec<(Option<&str>, IssueParts)> = vec![(None, needs_keys(&[]))];
        for id in &cloud_transcription_providers {
            transcriptions.push((Some(*id), needs_keys(&[])));
            transcriptions.push((None, needs_keys(&[*id])));
        }
        for built_count in 0..=members.len() {
            let built = &members[..built_count];
            let models: Vec<&str> = members.iter().filter_map(choice_model).collect();
            // Enhancement as a session gives it for this many built: off
            // or failed with nothing built, else serving beside none, one
            // or two of the pool providers that did not build.
            let mut cleanups: Vec<(RowCleanup, &[macalla_session::choice::EnhancerChoice])> =
                Vec::new();
            if built_count == 0 {
                cleanups.push((RowCleanup::Off, &[]));
                for failed in [1, 2, members.len()] {
                    cleanups.push((
                        RowCleanup::Failed(needs_key_errors(&member_ids[..failed])),
                        &members[..failed],
                    ));
                }
            } else {
                let rest = &member_ids[built_count..];
                for skipped in 0..=rest.len().min(2) {
                    cleanups.push((
                        RowCleanup::Serving(built.to_vec(), needs_key_errors(&rest[..skipped])),
                        &members[..],
                    ));
                }
            }
            for engine in local_engines.iter().copied() {
                for (transcription, transcription_issue) in &transcriptions {
                    for (cleanup, ticked) in &cleanups {
                        let line = overlay_line(
                            engine,
                            *transcription,
                            transcription_issue,
                            cleanup.now(),
                            ticked,
                        );
                        let Some(line) = line else { continue };
                        assert!(
                            line.chars().count() <= OVERLAY_LINE_MAX_CHARS,
                            "{built_count} built, engine={engine:?}, \
                             transcription={transcription:?}, {transcription_issue:?}: {line:?} \
                             is {} chars",
                            line.chars().count()
                        );
                        for model in &models {
                            assert!(
                                !line.contains(model),
                                "{line:?} names the model {model:?}, the overlay names providers \
                                 only"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A pool member's model id (`pool_overlay_label` never reads this --
    /// the overlay names providers only -- but the test above checks no
    /// emitted line leaks one anyway).
    fn choice_model(choice: &macalla_session::choice::EnhancerChoice) -> Option<&str> {
        use macalla_session::choice::EnhancerChoice;
        match choice {
            EnhancerChoice::Endpoint { model, .. } | EnhancerChoice::Anthropic { model, .. } => {
                Some(model)
            }
            EnhancerChoice::AppleIntelligence { .. } => None,
        }
    }

    #[test]
    fn a_mixed_pools_first_member_is_never_the_wrong_choices_provider() {
        // Anthropic first, Gemini second, each a real choice with its own
        // model id: `pool_overlay_label` must read the provider name off
        // the same member the count is built from, never the pool's other
        // member's, and never leak either model id.
        let choices = [
            pool_choice("anthropic", "claude-opus-5"),
            pool_choice("gemini", "gemini-3.5-flash-lite"),
        ];
        assert_eq!(
            pool_overlay_label(&choices).as_deref(),
            Some("Anthropic +1")
        );
        // Reversed: the first member's own name changes with it.
        let reversed = [choices[1].clone(), choices[0].clone()];
        assert_eq!(pool_overlay_label(&reversed).as_deref(), Some("Gemini +1"));
    }

    #[test]
    fn recipients_overlay_label_dedupes_by_provider_id_then_by_name() {
        use std::borrow::Cow;
        // Same id, different display names: one recipient.
        assert_eq!(
            recipients_overlay_label(
                [
                    (Some("openai"), Cow::Borrowed("OpenAI")),
                    (Some("openai"), Cow::Borrowed("Open AI")),
                ]
                .into_iter()
            )
            .as_deref(),
            Some("OpenAI")
        );
        // No id: deduped by name, and never merged with a provider.
        assert_eq!(
            recipients_overlay_label(
                [
                    (None, Cow::Borrowed("Your server")),
                    (None, Cow::Borrowed("Your server")),
                    (Some("openai"), Cow::Borrowed("OpenAI")),
                ]
                .into_iter()
            )
            .as_deref(),
            Some("Your server +1")
        );
        assert_eq!(recipients_overlay_label(std::iter::empty()), None);
    }

    /// Enhancement as a row of the table below holds it: owned, turned into
    /// the borrowed [`CleanupNow`] a session gives.
    enum RowCleanup {
        Off,
        Failed(Vec<macalla_session::Error>),
        NotReady(Vec<macalla_session::choice::EnhancerChoice>),
        Serving(
            Vec<macalla_session::choice::EnhancerChoice>,
            Vec<macalla_session::Error>,
        ),
    }

    impl RowCleanup {
        fn now(&self) -> CleanupNow<'_> {
            match self {
                RowCleanup::Off => CleanupNow::Off,
                RowCleanup::Failed(errors) => CleanupNow::Failed(errors),
                RowCleanup::NotReady(built) => CleanupNow::NotReady(built),
                RowCleanup::Serving(built, skipped) => CleanupNow::Serving { built, skipped },
            }
        }
    }

    /// The one rule the overlay's destination line follows: name only what
    /// will really be sent this dictation's words -- the cloud
    /// transcription provider, then Enhancement's built choices only while
    /// they serve, deduped by provider id -- then the short issue, merged
    /// from both halves and bounded by [`overlay_issue_line`]'s steps.
    /// Every row goes through [`overlay_line`], the function
    /// [`overlay_line_now`] (`run`'s one call) renders with, fed typed
    /// facts of the shapes a session gives; only the expected line is a
    /// literal. Real sessions are covered by the `try_open` tests further
    /// down.
    #[test]
    fn the_overlay_names_only_what_the_dictation_will_really_be_sent() {
        use macalla_session::choice::EnhancerChoice;

        struct Row {
            name: &'static str,
            /// A cloud transcription provider's id.
            transcription: Option<&'static str>,
            transcription_issue: IssueParts,
            cleanup: RowCleanup,
            ticked: Vec<EnhancerChoice>,
            expected: Option<&'static str>,
        }

        let openai = pool_choice("openai", "gpt-test");
        let openai_2 = pool_choice("openai", "gpt-test-2");
        let anthropic = pool_choice("anthropic", "claude-test");
        let gemini = pool_choice("gemini", "gemini-test");
        let mistral = pool_choice("mistral", "mistral-test");
        let apple_intelligence = EnhancerChoice::AppleIntelligence { guided: true };
        let row = |name, transcription, cleanup, ticked, expected| Row {
            name,
            transcription,
            transcription_issue: IssueParts::default(),
            cleanup,
            ticked,
            expected,
        };

        let rows = vec![
            row(
                "nothing ticked, local transcription",
                None,
                RowCleanup::Off,
                vec![],
                None,
            ),
            row(
                "nothing ticked, cloud transcription",
                Some("elevenlabs"),
                RowCleanup::Off,
                vec![],
                Some("ElevenLabs"),
            ),
            row(
                "serving: two different providers",
                None,
                RowCleanup::Serving(vec![openai.clone(), anthropic.clone()], vec![]),
                vec![openai.clone(), anthropic.clone()],
                Some("OpenAI +1"),
            ),
            row(
                "serving: two OpenAI models dedupe to one, no count",
                None,
                RowCleanup::Serving(vec![openai.clone(), openai_2.clone()], vec![]),
                vec![openai.clone(), openai_2.clone()],
                Some("OpenAI"),
            ),
            row(
                "serving: a partial pool beside its skipped member's issue",
                None,
                RowCleanup::Serving(
                    vec![anthropic.clone(), gemini.clone()],
                    needs_key_errors(&["openai"]),
                ),
                vec![openai.clone(), anthropic.clone(), gemini.clone()],
                Some("Anthropic +1 \u{00b7} OpenAI needs a key"),
            ),
            row(
                "every pool member needs a key, local transcription",
                None,
                RowCleanup::Failed(needs_key_errors(&["openai", "anthropic"])),
                vec![openai.clone(), anthropic.clone()],
                Some("OpenAI and Anthropic need keys"),
            ),
            row(
                "every pool member needs a key, cloud transcription: the cloud name stays",
                Some("elevenlabs"),
                RowCleanup::Failed(needs_key_errors(&["openai", "anthropic"])),
                vec![openai.clone(), anthropic.clone()],
                Some("ElevenLabs"),
            ),
            Row {
                name:
                    "transcription's missing key and a skipped pool member's are the same provider",
                transcription: None,
                transcription_issue: needs_keys(&["openai"]),
                cleanup: RowCleanup::Serving(
                    vec![anthropic.clone()],
                    needs_key_errors(&["openai"]),
                ),
                ticked: vec![openai.clone(), anthropic.clone()],
                expected: Some("Anthropic \u{00b7} OpenAI needs a key"),
            },
            row(
                "transcription and the pool are the same provider",
                Some("openai"),
                RowCleanup::Serving(vec![openai.clone()], vec![]),
                vec![openai.clone()],
                Some("OpenAI"),
            ),
            row(
                "transcription shares a provider with a two-provider pool",
                Some("openai"),
                RowCleanup::Serving(vec![openai.clone(), anthropic.clone()], vec![]),
                vec![openai.clone(), anthropic.clone()],
                Some("OpenAI +1"),
            ),
            row(
                "transcription shares a provider with two of its models",
                Some("openai"),
                RowCleanup::Serving(vec![openai.clone(), openai_2.clone()], vec![]),
                vec![openai.clone(), openai_2.clone()],
                Some("OpenAI"),
            ),
            row(
                "transcription shares a provider, beside a skipped member's issue",
                Some("openai"),
                RowCleanup::Serving(
                    vec![openai.clone(), anthropic.clone()],
                    needs_key_errors(&["gemini"]),
                ),
                vec![openai.clone(), anthropic.clone(), gemini.clone()],
                Some("OpenAI +1 \u{00b7} Gemini needs a key"),
            ),
            row(
                "a different transcription provider stays apart from the pool",
                Some("elevenlabs"),
                RowCleanup::Serving(vec![openai.clone(), anthropic.clone()], vec![]),
                vec![openai.clone(), anthropic.clone()],
                Some("ElevenLabs \u{00b7} OpenAI +1"),
            ),
            row(
                "Apple Intelligence serving",
                None,
                RowCleanup::Serving(vec![apple_intelligence.clone()], vec![]),
                vec![apple_intelligence.clone()],
                Some("Apple Intelligence"),
            ),
            row(
                "Apple Intelligence serving, cloud transcription",
                Some("elevenlabs"),
                RowCleanup::Serving(vec![apple_intelligence.clone()], vec![]),
                vec![apple_intelligence.clone()],
                Some("ElevenLabs \u{00b7} Apple Intelligence"),
            ),
            row(
                "Apple Intelligence not ready: named nowhere but the issue",
                None,
                RowCleanup::NotReady(vec![apple_intelligence.clone()]),
                vec![apple_intelligence.clone()],
                Some("Apple Intelligence is not ready"),
            ),
            row(
                "Apple Intelligence not ready, cloud transcription: the cloud name stays",
                Some("elevenlabs"),
                RowCleanup::NotReady(vec![apple_intelligence.clone()]),
                vec![apple_intelligence.clone()],
                Some("ElevenLabs"),
            ),
            row(
                "a pool of one with no key, local transcription",
                None,
                RowCleanup::Failed(needs_key_errors(&["openai"])),
                vec![openai.clone()],
                Some("Apple Speech \u{00b7} OpenAI needs a key"),
            ),
            // The width rule.
            row(
                "width: a transcription name and an issue both fit",
                Some("elevenlabs"),
                RowCleanup::Failed(needs_key_errors(&["openai"])),
                vec![openai.clone()],
                Some("ElevenLabs \u{00b7} OpenAI needs a key"),
            ),
            row(
                "width: a cloud name and a pool label never give way to the issue",
                Some("elevenlabs"),
                RowCleanup::Serving(
                    vec![anthropic.clone(), gemini.clone()],
                    needs_key_errors(&["mistral"]),
                ),
                vec![anthropic.clone(), gemini.clone(), mistral.clone()],
                Some("ElevenLabs \u{00b7} Anthropic +1"),
            ),
            row(
                "width: local transcription, the pool label and the issue fit",
                None,
                RowCleanup::Serving(
                    vec![anthropic.clone(), gemini.clone()],
                    needs_key_errors(&["mistral"]),
                ),
                vec![anthropic.clone(), gemini.clone(), mistral.clone()],
                Some("Anthropic +1 \u{00b7} Mistral needs a key"),
            ),
            row(
                "width: a pool label and an issue that never fit together keep the label",
                Some("elevenlabs"),
                RowCleanup::Serving(
                    vec![anthropic.clone(), gemini.clone()],
                    needs_key_errors(&["openai", "mistral"]),
                ),
                vec![
                    openai.clone(),
                    anthropic.clone(),
                    gemini.clone(),
                    mistral.clone(),
                ],
                Some("ElevenLabs \u{00b7} Anthropic +1"),
            ),
            row(
                "width: an issue too long even alone leaves the transcription name",
                Some("elevenlabs"),
                RowCleanup::Failed(needs_key_errors(&["openai", "anthropic", "gemini"])),
                vec![openai.clone(), anthropic.clone(), gemini.clone()],
                Some("ElevenLabs"),
            ),
            row(
                "width: an issue too long even alone leaves the engine name",
                None,
                RowCleanup::Failed(needs_key_errors(&["openai", "anthropic", "gemini"])),
                vec![openai.clone(), anthropic.clone(), gemini],
                Some("Apple Speech"),
            ),
        ];

        for row in rows {
            let line = overlay_line(
                "Apple Speech",
                row.transcription,
                &row.transcription_issue,
                row.cleanup.now(),
                &row.ticked,
            );
            assert_eq!(line.as_deref(), row.expected, "{}", row.name);
        }
    }

    #[test]
    fn the_overlay_pool_form_never_changes_the_pages_full_wording() {
        let choices = [
            pool_choice("openai", "gpt-5.6-mini"),
            pool_choice("anthropic", "claude-opus-5"),
        ];
        // What `opened_status` gives the Models page, straight from
        // `macalla_session::cleanup::pool_label` -- untouched by this
        // pool form, which only `overlay_line` ever builds.
        let full = "OpenAI, falling back to Anthropic";
        let status = cleanup_status(None, &[], Some(full), &choices);
        assert_eq!(status.enhancement.as_deref(), Some(full));

        let overlay = overlay_line(
            "Apple Speech",
            None,
            &IssueParts::default(),
            CleanupNow::Serving {
                built: &choices,
                skipped: &[],
            },
            &choices,
        );
        assert_eq!(overlay.as_deref(), Some("OpenAI +1"));
    }

    #[test]
    fn overlay_issue_line_falls_back_to_the_bare_engine_name_when_even_the_issue_alone_is_too_long()
    {
        // A real phrase `IssueParts::phrase` actually produces: three
        // providers each needing a key fold into one clause, no "; " to
        // split on, and it alone is still too long for the row (38 chars).
        // Emitting it anyway, over the limit, is the bug this replaces --
        // the bare engine name is what stood here before there was an
        // issue to try at all, so it stands again now nothing shorter is
        // left to say.
        let mut issue = IssueParts::default();
        for provider in ["OpenAI", "Anthropic", "Gemini"] {
            issue.add(
                Some(provider),
                &macalla_session::Error::NeedsKey {
                    provider: provider.to_lowercase(),
                },
            );
        }
        let phrase = issue.phrase().expect("three names were added");
        assert_eq!(phrase, "OpenAI, Anthropic and Gemini need keys");
        assert!(phrase.chars().count() > OVERLAY_LINE_MAX_CHARS);
        assert_eq!(
            overlay_issue_line("Apple Speech", None, None, Some(&phrase)).as_deref(),
            Some("Apple Speech")
        );
    }

    #[test]
    fn overlay_issue_line_capitalises_an_unknown_provider_phrase_standing_alone() {
        // With no label and the engine name dropped too ("Apple Speech ·
        // the saved provider is unknown" does not fit), the phrase opens
        // the line by itself and must read as a sentence start, not the
        // lower-case form `short_session_error` writes for the far
        // commoner mid-sentence case (pinned by
        // `the_unknown_provider_phrase_stays_lower_case_mid_sentence`).
        let issue = short_session_error(
            None,
            &macalla_session::Error::UnknownProvider("watson".into()),
        );
        assert_eq!(
            overlay_issue_line("Apple Speech", None, None, Some(&issue)).as_deref(),
            Some("The saved provider is unknown")
        );
    }

    #[test]
    fn overlay_issue_line_keeps_only_the_first_clause_when_it_alone_fits() {
        // A real two-clause `IssueParts::phrase`: OpenAI needs a key, and
        // Anthropic's own key could not be read -- two providers, two
        // different reasons, joined with "; ". Together they are too long
        // for the row, but the first clause alone is not, so that is what
        // stands, same as a `label` never being dropped for the issue.
        let mut issue = IssueParts::default();
        issue.add(
            Some("OpenAI"),
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        issue.add(
            None,
            &macalla_session::Error::Platform(macalla_platform::Error::Secret {
                account: "provider-key.anthropic".into(),
                reason: "the item could not be found".into(),
            }),
        );
        let phrase = issue.phrase().expect("two phrases were added");
        assert_eq!(
            phrase,
            "OpenAI needs a key; Anthropic's key could not be read"
        );
        assert!(phrase.chars().count() > OVERLAY_LINE_MAX_CHARS);
        assert_eq!(
            overlay_issue_line("Apple Speech", None, None, Some(&phrase)).as_deref(),
            Some("OpenAI needs a key")
        );
    }

    #[test]
    fn overlay_issue_line_falls_back_to_the_bare_engine_name_when_even_the_first_clause_is_too_long(
    ) {
        // The first clause itself -- three providers needing keys -- is
        // already too long alone; a second, shorter clause after it must
        // not change that: nothing here fits, so the bare engine name
        // stands, same as with a single too-long clause.
        let mut issue = IssueParts::default();
        for provider in ["OpenAI", "Anthropic", "Gemini"] {
            issue.add(
                Some(provider),
                &macalla_session::Error::NeedsKey {
                    provider: provider.to_lowercase(),
                },
            );
        }
        issue.add(
            None,
            &macalla_session::Error::Platform(macalla_platform::Error::Secret {
                account: "provider-key.mistral".into(),
                reason: "the item could not be found".into(),
            }),
        );
        let phrase = issue.phrase().expect("phrases were added");
        assert_eq!(
            phrase,
            "OpenAI, Anthropic and Gemini need keys; Mistral's key could not be read"
        );
        assert_eq!(
            overlay_issue_line("Apple Speech", None, None, Some(&phrase)).as_deref(),
            Some("Apple Speech")
        );
    }

    #[test]
    fn cleanup_status_is_untroubled_with_nothing_wrong() {
        let status = cleanup_status(None, &[], None, &[]);
        assert_eq!(status.enhancement, None);
        assert_eq!(status.issue.phrase(), None);
        assert_eq!(status.reason, None);
    }

    #[test]
    fn cleanup_status_is_untroubled_when_it_built_and_nothing_was_skipped() {
        let status = cleanup_status(None, &[], Some("Apple Intelligence"), &[]);
        assert_eq!(status.enhancement.as_deref(), Some("Apple Intelligence"));
        assert_eq!(status.issue.phrase(), None);
        assert_eq!(status.reason, None);
    }

    #[test]
    fn cleanup_status_names_apple_intelligence_not_ready_and_claims_nothing_in_use() {
        let notice =
            CleanupNotice::NotReady("Apple Intelligence is turned off in System Settings.");
        let status = cleanup_status(Some(&notice), &[], None, &[]);
        assert_eq!(status.enhancement, None, "not ready is not \"in use\"");
        assert_eq!(
            status.issue.phrase().as_deref(),
            Some("Apple Intelligence is not ready")
        );
        // The page sentence is `cleanup_notice`'s own, not a new one.
        assert_eq!(status.reason, Some(cleanup_notice(&notice)));
    }

    #[test]
    fn cleanup_status_names_every_choice_a_failed_pool_of_two_lost_including_platform() {
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let choices = vec![
            EnhancerChoice::Endpoint {
                url: "https://api.openai.com/v1".into(),
                model: "gpt-test".into(),
                key: KeySource::Stored("openai".into()),
                reasoning: None,
            },
            EnhancerChoice::Anthropic {
                model: "claude-test".into(),
                key: KeySource::Stored("anthropic".into()),
                reasoning: None,
            },
        ];
        let errors = vec![
            macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
            macalla_session::Error::Platform(macalla_platform::Error::Secret {
                account: "provider-key.anthropic".into(),
                reason: "denied".into(),
            }),
        ];
        let notice = CleanupNotice::Failed(errors);
        let status = cleanup_status(Some(&notice), &[], None, &choices);
        assert_eq!(status.enhancement, None);
        assert_eq!(
            status.issue.phrase().as_deref(),
            Some("OpenAI needs a key; Anthropic's key could not be read")
        );
        // The page reason reuses `cleanup_notice` -- no second wording table.
        assert_eq!(status.reason, Some(cleanup_notice(&notice)));
    }

    #[test]
    fn cleanup_status_folds_an_all_missing_pool_of_two_into_one_overlay_phrase() {
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let choices = vec![
            EnhancerChoice::Endpoint {
                url: "https://api.openai.com/v1".into(),
                model: "gpt-test".into(),
                key: KeySource::Stored("openai".into()),
                reasoning: None,
            },
            EnhancerChoice::Anthropic {
                model: "claude-test".into(),
                key: KeySource::Stored("anthropic".into()),
                reasoning: None,
            },
        ];
        let errors = vec![
            macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
            macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
        ];
        let notice = CleanupNotice::Failed(errors);
        let status = cleanup_status(Some(&notice), &[], None, &choices);
        assert_eq!(
            status.issue.phrase().as_deref(),
            Some("OpenAI and Anthropic need keys")
        );
    }

    #[test]
    fn cleanup_status_names_the_skipped_choice_and_what_built_for_a_partial_pool() {
        let skipped = [macalla_session::Error::NeedsKey {
            provider: "anthropic".into(),
        }];
        let status = cleanup_status(None, &skipped, Some("OpenAI"), &[]);
        assert_eq!(status.enhancement.as_deref(), Some("OpenAI"));
        assert_eq!(
            status.issue.phrase().as_deref(),
            Some("Anthropic needs a key")
        );
        let reason = status.reason.expect("something was skipped");
        assert!(reason.contains("Anthropic"), "{reason}");
        // No new sentence names what is in use; the pool line does that
        // from `enhancement`.
        assert!(!reason.contains("instead"), "{reason}");
    }

    #[test]
    fn cleanup_status_names_a_provider_ticked_twice_once_in_a_partial_pool() {
        // Two models from the same provider, both skipped: the provider is
        // named once, not twice.
        let skipped = [
            macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
            macalla_session::Error::NeedsKey {
                provider: "anthropic".into(),
            },
        ];
        let status = cleanup_status(None, &skipped, Some("OpenAI"), &[]);
        assert_eq!(
            status.issue.phrase().as_deref(),
            Some("Anthropic needs a key")
        );
    }

    /// A minimal [`macalla_core::stt::SttEngine`] for building a real
    /// [`Session`] without a compiled-in engine or a registry lookup, so
    /// `opened_status`'s `Ready`/`PreviewMissing`/`Failed` wiring can be
    /// tested on every build. Never asked to transcribe anything here.
    struct FakeEngine;

    impl macalla_core::stt::SttEngine for FakeEngine {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> macalla_core::stt::EngineCaps {
            macalla_core::stt::EngineCaps::stream_first()
        }
        fn stream(
            &self,
            _ctx: &macalla_core::stt::SttContext,
        ) -> macalla_core::Result<Box<dyn macalla_core::stt::SttStream>> {
            Err(macalla_core::Error::Unsupported {
                engine: "fake",
                what: "streaming",
            })
        }
        fn transcribe(
            &self,
            _audio: &macalla_core::audio::AudioSegment,
            _ctx: &macalla_core::stt::SttContext,
        ) -> macalla_core::Result<macalla_core::stt::Transcript> {
            Err(macalla_core::Error::Unsupported {
                engine: "fake",
                what: "batch transcription",
            })
        }
    }

    fn fake_session() -> Session {
        let dictionary = macalla_core::dictionary::Dictionary::new(
            &macalla_core::dictionary::TermFile::default(),
        );
        Session::with_engine(Box::new(FakeEngine), dictionary, "fake", Config::default())
    }

    #[test]
    fn cleanup_is_whole_only_when_off_or_serving_everything_ticked() {
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let keyless = |port: u16| EnhancerChoice::Endpoint {
            url: format!("http://127.0.0.1:{port}"),
            model: "m".into(),
            key: KeySource::None,
            reasoning: None,
        };
        let needs_key = EnhancerChoice::Endpoint {
            url: "http://127.0.0.1:29".into(),
            model: "m".into(),
            key: KeySource::Stored("openai".into()),
            reasoning: None,
        };
        let keys = macalla_platform::secrets::MemoryStore::new();

        let mut session = fake_session();
        let status = |s: &Session| status_for(s, &[], String::new(), IssueParts::default(), None);
        assert!(status(&session).cleanup_whole(), "off");

        session.replace_cleanup(&[keyless(9)], &keys);
        assert!(status(&session).cleanup_whole(), "serving");

        session.replace_cleanup(std::slice::from_ref(&needs_key), &keys);
        assert!(!status(&session).cleanup_whole(), "failed");

        session.replace_cleanup(&[needs_key, keyless(9)], &keys);
        assert!(
            matches!(session.cleanup_now(), CleanupNow::Serving { skipped, .. } if skipped.len() == 1)
        );
        assert!(!status(&session).cleanup_whole(), "a member skipped");

        assert!(
            !OpenedStatus::default().cleanup_whole(),
            "nothing opened yet"
        );
        let failed = opened_status(&Opened::Failed("no engine".into()), &[]);
        assert!(!failed.cleanup_whole());
    }

    #[test]
    fn cleanup_that_built_but_is_not_ready_is_not_whole() {
        let built: [macalla_session::choice::EnhancerChoice; 0] = [];
        assert!(!cleanup_whole(CleanupNow::NotReady(&built), None));
        assert!(!cleanup_whole(
            CleanupNow::Serving {
                built: &built,
                skipped: &[]
            },
            Some(&CleanupNotice::NotReady("not ready"))
        ));
        assert!(cleanup_whole(
            CleanupNow::Serving {
                built: &built,
                skipped: &[]
            },
            None
        ));
    }

    #[test]
    fn a_cleanup_swap_changes_what_a_borrowed_meeting_tools_would_tidy_with() {
        // Pinned so a stale borrow -- a Files page that read the fingerprint
        // before this swap -- is refused by `files::same_choice` rather than
        // silently sent to the provider that replaced it.
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let mut session = fake_session();
        session.replace_cleanup(
            &[EnhancerChoice::Endpoint {
                url: "http://127.0.0.1:9".into(),
                model: "m".into(),
                key: KeySource::None,
                reasoning: None,
            }],
            &macalla_platform::secrets::MemoryStore::new(),
        );
        let lent = LentTools::default();
        lent.set(Some(&session));
        let shown = lent
            .get()
            .expect("a session is open")
            .into_tidy()
            .expect("the endpoint choice built")
            .fingerprint()
            .to_string();

        // The person changes Enhancement -- `dictation::run` calls this the
        // same way on `Command::SwitchCleanup`, in place, no reopen.
        session.replace_cleanup(
            &[EnhancerChoice::Endpoint {
                url: "http://127.0.0.1:19".into(),
                model: "m2".into(),
                key: KeySource::None,
                reasoning: None,
            }],
            &macalla_platform::secrets::MemoryStore::new(),
        );
        lent.set(Some(&session));
        let now = lent
            .get()
            .expect("still open")
            .into_tidy()
            .expect("the new endpoint choice built")
            .fingerprint()
            .to_string();

        assert_ne!(
            shown, now,
            "a page still showing `shown` must read CHOICE_CHANGED, not send to the new choice"
        );
        assert!(
            crate::files::same_choice(&shown, &now).is_err(),
            "the stale fingerprint must be refused, not accepted"
        );
    }

    #[test]
    fn a_switch_cleanup_right_after_a_reopen_still_lands_on_the_choice_saved_during_it() {
        // Regression for the blocker the earlier `reopened_this_turn` skip
        // introduced: `run`'s `SwitchEngine` reopen reads `cleanup_config`
        // before `open_session`, which can take seconds while Enhancement's
        // own controls stay live the whole time -- so the choice it read
        // can already be stale by the time a queued `SwitchCleanup` runs.
        // Does not drive `run`'s `Command::SwitchCleanup` arm itself -- that
        // needs a channel and an `AppHandle` -- only the two functions it
        // calls, in the same order, on a fake session: `cleanup_config`
        // reads whatever `history` holds right now, not what an earlier
        // read saw, and `replace_cleanup` replaces the session's cleanup
        // with it, unconditionally. Proves those two calls land on the
        // choice saved during the reopen, not the stale one it read; the
        // arm's own control flow (that it always makes this call, never
        // skipped) is not under test here.
        let history = HistoryHandle::in_memory_for_test();
        let keys = macalla_platform::secrets::MemoryStore::new();
        history
            .set_cleanup_choice(&crate::history::CleanupChoice::Server {
                address: "http://127.0.0.1:9".into(),
                model: "m".into(),
            })
            .unwrap();

        // What the reopen already did, with the choice it read before it
        // started loading -- a loopback address, so its destination
        // ("Local") is deterministic, no network involved.
        let stale_choices = cleanup_config(&history);
        let mut session = fake_session();
        session.replace_cleanup(&stale_choices, &keys);
        assert_eq!(session.cleanup_destination(), Some("Local"));

        // The person turns Enhancement off while that reopen is still
        // loading.
        history
            .set_cleanup_choice(&crate::history::CleanupChoice::Off)
            .unwrap();

        // The `Command::SwitchCleanup` arm's own two steps: read fresh from
        // history, then swap -- unconditionally, not gated on the reopen
        // that just ran.
        let fresh_choices = cleanup_config(&history);
        assert!(fresh_choices.is_empty(), "Off maps to no choices");
        session.replace_cleanup(&fresh_choices, &keys);

        assert_eq!(
            session.cleanup_destination(),
            None,
            "the session must end on the NEW choice (Off), not the stale one the reopen read"
        );
        assert!(session.cleanup_built_choices().is_empty());
    }

    #[test]
    fn status_for_clears_a_stale_enhancement_reason_once_a_good_choice_builds() {
        // What `publish_cleanup_swap` republishes after `Command::SwitchCleanup`:
        // a NeedsKey reason from the last choice must not linger once a
        // choice that builds replaces it.
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let mut session = fake_session();
        let keys = macalla_platform::secrets::MemoryStore::new();
        let needs_key = [EnhancerChoice::Anthropic {
            model: "claude-haiku-4-5".into(),
            key: KeySource::Stored("anthropic".into()),
            reasoning: None,
        }];
        session.replace_cleanup(&needs_key, &keys);
        let stale = status_for(
            &session,
            &needs_key,
            "The speech engine".to_string(),
            IssueParts::default(),
            None,
        );
        assert!(stale.enhancement_reason.is_some(), "no key stored yet");
        assert!(stale.enhancement_issue.is_some());

        let local = [EnhancerChoice::Endpoint {
            url: "http://127.0.0.1:9".into(),
            model: "m".into(),
            key: KeySource::None,
            reasoning: None,
        }];
        session.replace_cleanup(&local, &keys);
        let refreshed = status_for(
            &session,
            &local,
            "The speech engine".to_string(),
            IssueParts::default(),
            None,
        );
        assert_eq!(refreshed.enhancement_reason, None);
        assert_eq!(refreshed.enhancement_issue, None);
        assert_eq!(
            overlay_line_now(&refreshed, &session, &local).as_deref(),
            Some("Local")
        );
        assert_eq!(refreshed.enhancement.as_deref(), Some("Local"));
    }

    /// Pool choices built the same way [`cleanup_config_tries_a_stored_pool_in_ticked_order_with_each_providers_stored_key`]
    /// checks the shape of: a keyless endpoint that always builds, then an
    /// endpoint whose key is not in `keys`, so it is skipped.
    fn pool_with_a_skipped_openai() -> Vec<macalla_session::choice::EnhancerChoice> {
        use macalla_session::choice::{EnhancerChoice, KeySource};
        vec![
            EnhancerChoice::Endpoint {
                url: "http://127.0.0.1:9".into(),
                model: "m".into(),
                key: KeySource::None,
                reasoning: None,
            },
            EnhancerChoice::Endpoint {
                url: "http://127.0.0.1:19".into(),
                model: "gpt-test".into(),
                key: KeySource::Stored("openai".into()),
                reasoning: None,
            },
        ]
    }

    #[test]
    fn publish_cleanup_swap_names_a_provider_once_when_transcription_and_the_pool_both_need_its_key(
    ) {
        // A transcription NeedsKey(openai) plus a pool skip of openai must
        // read "OpenAI needs a key", not "OpenAI needs a key; OpenAI needs a
        // key" -- and the swap path must read exactly what a fresh open
        // would for the same inputs.
        let keys = macalla_platform::secrets::MemoryStore::new();
        let choices = pool_with_a_skipped_openai();
        let opened_session = {
            let mut session = fake_session();
            session.replace_cleanup(&choices, &keys);
            session
        };

        let mut transcription_issue = IssueParts::default();
        transcription_issue.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        let fresh = opened_status(
            &Opened::LocalFallback {
                session: opened_session,
                notice: "Using the speech engine instead.".to_string(),
                issue: transcription_issue,
            },
            &choices,
        );
        assert_eq!(
            fresh.transcription_issue.as_deref(),
            Some("OpenAI needs a key")
        );

        // `publish_cleanup_swap`'s own computation, without an `AppHandle`:
        // `status_for` fed `previous.transcription_parts`, as it is there.
        // A second session, built from the same choices, stands in for the
        // one `swap_cleanup` would already have rebuilt in place.
        let mut swapped_session = fake_session();
        swapped_session.replace_cleanup(&choices, &keys);
        let refreshed = status_for(
            &swapped_session,
            &choices,
            fresh.transcription.clone(),
            fresh.transcription_parts.clone(),
            fresh.transcription_reason.clone(),
        );
        assert_eq!(
            refreshed, fresh,
            "a swap must read exactly as a fresh open would"
        );

        // The overlay's own destination line must read the same either
        // way too, naming OpenAI once although both halves need its key.
        let fresh_line = overlay_line_now(&fresh, &swapped_session, &choices);
        let swap_line = overlay_line_now(&refreshed, &swapped_session, &choices);
        assert_eq!(
            swap_line, fresh_line,
            "the overlay's destination line must read the same either way"
        );
        assert_eq!(
            fresh_line.as_deref(),
            Some("Local \u{00b7} OpenAI needs a key")
        );
    }

    #[test]
    fn publish_cleanup_swap_names_two_providers_the_same_way_a_fresh_open_would() {
        // Transcription needs one provider's key, the pool skips a
        // different one: the swap path must merge and order them exactly as
        // a fresh open does.
        let keys = macalla_platform::secrets::MemoryStore::new();
        let choices = pool_with_a_skipped_openai();
        let opened_session = {
            let mut session = fake_session();
            session.replace_cleanup(&choices, &keys);
            session
        };

        let mut transcription_issue = IssueParts::default();
        transcription_issue.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "elevenlabs".into(),
            },
        );
        let fresh = opened_status(
            &Opened::LocalFallback {
                session: opened_session,
                notice: "Using the speech engine instead.".to_string(),
                issue: transcription_issue,
            },
            &choices,
        );
        let mut swapped_session = fake_session();
        swapped_session.replace_cleanup(&choices, &keys);
        let refreshed = status_for(
            &swapped_session,
            &choices,
            fresh.transcription.clone(),
            fresh.transcription_parts.clone(),
            fresh.transcription_reason.clone(),
        );
        assert_eq!(
            refreshed, fresh,
            "a swap must read exactly as a fresh open would"
        );
        // "Local · ElevenLabs and OpenAI need keys" is too long, and a
        // cleanup label never gives way to the issue.
        let fresh_line = overlay_line_now(&fresh, &swapped_session, &choices);
        let swap_line = overlay_line_now(&refreshed, &swapped_session, &choices);
        assert_eq!(swap_line, fresh_line);
        assert_eq!(fresh_line.as_deref(), Some("Local"));
    }

    #[test]
    fn a_partly_built_pool_swap_reads_the_same_status_and_overlay_line_a_fresh_open_would() {
        // A partly built pool -- one member skipped for a missing key --
        // with nothing wrong on the transcription side: `Opened::Ready`'s
        // own status and a swap onto the same choices must read
        // identically, including the overlay's compact form, which only
        // built providers that serve ever appear in (`overlay_line`'s doc).
        let keys = macalla_platform::secrets::MemoryStore::new();
        let choices = pool_with_a_skipped_openai();
        let opened_session = {
            let mut session = fake_session();
            session.replace_cleanup(&choices, &keys);
            session
        };
        let fresh = opened_status(&Opened::Ready(opened_session), &choices);
        assert!(
            fresh.enhancement_reason.is_some(),
            "the skipped member still reports on the page"
        );

        let mut swapped_session = fake_session();
        swapped_session.replace_cleanup(&choices, &keys);
        let refreshed = status_for(
            &swapped_session,
            &choices,
            fresh.transcription.clone(),
            fresh.transcription_parts.clone(),
            fresh.transcription_reason.clone(),
        );
        assert_eq!(
            refreshed, fresh,
            "a swap must read exactly as a fresh open would"
        );

        let fresh_line = overlay_line_now(&fresh, &swapped_session, &choices);
        let swap_line = overlay_line_now(&refreshed, &swapped_session, &choices);
        assert_eq!(
            swap_line, fresh_line,
            "the overlay's destination line must read the same either way"
        );
    }

    #[test]
    fn a_pool_that_built_nothing_swap_reads_the_same_status_and_overlay_line_a_fresh_open_would() {
        // Every member needs a key `keys` does not have: nothing builds, so
        // there is no destination at all -- `Opened::Ready` still opens (a
        // cleanup pool never blocks transcription), and a swap onto the same
        // choices must read the same empty-pool status a fresh open would,
        // including an overlay line with nothing built to name.
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let keys = macalla_platform::secrets::MemoryStore::new(); // no keys stored
        let choices = vec![
            EnhancerChoice::Endpoint {
                url: "http://127.0.0.1:19".into(),
                model: "gpt-test".into(),
                key: KeySource::Stored("openai".into()),
                reasoning: None,
            },
            EnhancerChoice::Anthropic {
                model: "claude-haiku-4-5".into(),
                key: KeySource::Stored("anthropic".into()),
                reasoning: None,
            },
        ];
        let opened_session = {
            let mut session = fake_session();
            session.replace_cleanup(&choices, &keys);
            session
        };
        let fresh = opened_status(&Opened::Ready(opened_session), &choices);
        assert_eq!(
            fresh.enhancement, None,
            "nothing built, so nothing is in use"
        );
        assert!(
            fresh.enhancement_reason.is_some(),
            "why nothing built still reports on the page"
        );

        let mut swapped_session = fake_session();
        swapped_session.replace_cleanup(&choices, &keys);
        let refreshed = status_for(
            &swapped_session,
            &choices,
            fresh.transcription.clone(),
            fresh.transcription_parts.clone(),
            fresh.transcription_reason.clone(),
        );
        assert_eq!(
            refreshed, fresh,
            "a swap must read exactly as a fresh open would"
        );

        let fresh_line = overlay_line_now(&fresh, &swapped_session, &choices);
        let swap_line = overlay_line_now(&refreshed, &swapped_session, &choices);
        assert_eq!(
            swap_line, fresh_line,
            "the overlay's destination line must read the same either way"
        );
    }

    #[test]
    fn apple_intelligence_composes_the_same_status_and_overlay_line_through_opened_status_and_a_swap(
    ) {
        // Machine-independent, unlike an earlier version of this test:
        // Apple Intelligence's readiness only ever comes from
        // `Availability::now`, real hardware and System Settings state this
        // Mac's own toggle makes unreliable to assert
        // on and liable to flip a passing test red on someone else's Mac --
        // so this never asserts which way it reads here, only that
        // `opened_status`'s `Opened::Ready` arm and `publish_cleanup_swap`'s
        // own `status_for` call compose whatever it *does* read, ready or
        // not, the same way. The canned "Apple Intelligence is not ready"
        // phrase stays pinned on its own, in
        // `cleanup_status_names_apple_intelligence_not_ready_and_claims_nothing_in_use`
        // -- exercised only on a Mac where Apple Intelligence itself
        // actually reads not ready (off in System Settings, or this build
        // has none), never asserted here.
        use macalla_session::choice::EnhancerChoice;
        let keys = macalla_platform::secrets::MemoryStore::new();
        let choices = [EnhancerChoice::AppleIntelligence { guided: true }];

        let mut fresh_session = fake_session();
        fresh_session.replace_cleanup(&choices, &keys);
        let fresh = opened_status(&Opened::Ready(fresh_session), &choices);

        let mut swapped_session = fake_session();
        swapped_session.replace_cleanup(&choices, &keys);
        // `publish_cleanup_swap`'s own two carried-over fields, as it reads
        // them from the previous `OpenedStatus`, not a fresh `IssueParts`
        // and `None` -- a swap has a real transcription side to preserve,
        // and this must feed `status_for` the same way that call does.
        let swapped = status_for(
            &swapped_session,
            &choices,
            fresh.transcription.clone(),
            fresh.transcription_parts.clone(),
            fresh.transcription_reason.clone(),
        );
        assert_eq!(
            swapped, fresh,
            "a swap must read exactly as a fresh open would, whatever Apple Intelligence's real \
             state is"
        );

        let fresh_line = overlay_line_now(&fresh, &swapped_session, &choices);
        let swap_line = overlay_line_now(&swapped, &swapped_session, &choices);
        assert_eq!(
            swap_line, fresh_line,
            "the overlay's destination line must read the same either way"
        );
    }

    #[test]
    fn opened_status_never_serializes_its_internal_issue_parts() {
        // `transcription_parts` is read-side bookkeeping for
        // `publish_cleanup_swap`, never something the frontend's
        // `dictation-status` listener should see.
        let mut status = OpenedStatus::default();
        status.transcription_parts.add(
            None,
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        let json = serde_json::to_value(&status).expect("OpenedStatus serializes");
        assert!(
            json.as_object()
                .is_some_and(|o| !o.contains_key("transcription_parts")),
            "transcription_parts must stay out of the payload the frontend reads: {json}"
        );
    }

    #[test]
    fn opened_status_for_a_ready_session_with_nothing_configured_has_no_reason() {
        let opened = Opened::Ready(fake_session());
        let status = opened_status(&opened, &[]);
        assert_eq!(status.transcription, "The speech engine");
        assert_eq!(status.enhancement, None);
        assert_eq!(status.transcription_issue, None);
        assert_eq!(status.enhancement_issue, None);
        assert_eq!(status.transcription_reason, None);
        assert_eq!(status.enhancement_reason, None);
    }

    #[test]
    fn opened_status_for_a_local_fallback_carries_the_notice_and_the_short_issue() {
        let mut issue = IssueParts::default();
        issue.add(
            Some("OpenAI"),
            &macalla_session::Error::NeedsKey {
                provider: "openai".into(),
            },
        );
        let opened = Opened::LocalFallback {
            session: fake_session(),
            notice: "OpenAI has no key saved, so dictation is using The speech engine on this \
                     Mac. Add the key again under Providers on the Models page, or choose \
                     another provider."
                .to_string(),
            issue,
        };
        let status = opened_status(&opened, &[]);
        assert_eq!(
            status.transcription_issue.as_deref(),
            Some("OpenAI needs a key")
        );
        assert_eq!(status.enhancement_issue, None);
        assert!(status
            .transcription_reason
            .expect("a LocalFallback always carries the notice")
            .starts_with("OpenAI has no key saved"),);
        assert_eq!(status.enhancement_reason, None);
    }

    #[test]
    fn opened_status_for_preview_missing_names_the_engine_and_a_fixed_reason() {
        let opened = Opened::PreviewMissing(fake_session());
        let status = opened_status(&opened, &[]);
        assert_eq!(status.transcription, "The speech engine");
        assert_eq!(
            status.transcription_reason.as_deref(),
            Some("The local engine could not open, so dictation cannot start.")
        );
        assert_eq!(status.enhancement_reason, None);
    }

    #[test]
    fn opened_status_for_failed_carries_the_message_as_the_transcription_reason() {
        let opened = Opened::Failed("nothing could be opened".to_string());
        let status = opened_status(&opened, &[]);
        assert_eq!(status.transcription, "");
        assert_eq!(status.enhancement, None);
        assert_eq!(status.transcription_issue, None);
        assert_eq!(status.enhancement_issue, None);
        assert_eq!(
            status.transcription_reason.as_deref(),
            Some("nothing could be opened")
        );
        assert_eq!(status.enhancement_reason, None);
    }

    // -------------------------------------- opened_status, real try_open

    // Every test below opens a real session through `try_open` (a real
    // registry engine, a `MemoryStore`, never the network or the
    // keychain), so the fallback and the Enhancement pool are seen
    // building together, not just the pieces `opened_status`'s other
    // tests drive separately with a `FakeEngine` and canned values.

    /// OpenAI then Anthropic, each with its own stored key, as a real
    /// ticked pool holds them.
    #[cfg(feature = "cloud-transcription")]
    fn openai_then_anthropic() -> Vec<macalla_session::choice::EnhancerChoice> {
        use macalla_session::choice::{EnhancerChoice, KeySource};
        vec![
            EnhancerChoice::Endpoint {
                url: "https://api.openai.com/v1".into(),
                model: "gpt-test".into(),
                key: KeySource::Stored("openai".into()),
                reasoning: None,
            },
            EnhancerChoice::Anthropic {
                model: "claude-test".into(),
                key: KeySource::Stored("anthropic".into()),
                reasoning: None,
            },
        ]
    }

    #[test]
    // Needs a real cloud engine: without the feature every provider
    // refuses with `CloudNotCompiledIn` before a key is ever read.
    #[cfg(feature = "cloud-transcription")]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn opened_status_keeps_the_pools_provider_in_use_beside_the_transcription_issue() {
        // OpenAI's transcription key is gone, so transcription falls
        // back to the local engine, but the Enhancement pool still has
        // Anthropic's key and sends every dictation there -- `enhancement`
        // must still say so, and the overlay must not repeat OpenAI's name
        // twice just because both halves once pointed at it.
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        macalla_session::providers::store_key(&store, "anthropic", "sk-test").unwrap();
        let config = Config {
            engine: Some(engine.id.to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "openai".to_string(),
                model: None,
            }),
            cleanup: openai_then_anthropic(),
            ..Config::default()
        };
        let opened = try_open(&config, &store);
        let status = opened_status(&opened, &config.cleanup);
        assert_eq!(status.enhancement.as_deref(), Some("Anthropic"));
        let Opened::LocalFallback { session, .. } = &opened else {
            panic!("expected a local fallback");
        };
        assert_eq!(
            overlay_line_now(&status, session, &config.cleanup).as_deref(),
            Some("Anthropic \u{00b7} OpenAI needs a key")
        );
    }

    #[test]
    #[cfg(feature = "cloud-transcription")]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn opened_status_folds_every_needs_key_name_when_transcription_and_the_whole_pool_are_missing()
    {
        // OpenAI's transcription key is gone, and neither pool member
        // has a key either.
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        let config = Config {
            engine: Some(engine.id.to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "openai".to_string(),
                model: None,
            }),
            cleanup: openai_then_anthropic(),
            ..Config::default()
        };
        let opened = try_open(&config, &store);
        let status = opened_status(&opened, &config.cleanup);
        assert_eq!(status.enhancement, None);
        let Opened::LocalFallback { session, .. } = &opened else {
            panic!("expected a local fallback");
        };
        assert!(session.cleanup_built_choices().is_empty());
        assert!(matches!(session.cleanup_now(), CleanupNow::Failed(_)));
        assert_eq!(
            overlay_line_now(&status, session, &config.cleanup).as_deref(),
            Some("OpenAI and Anthropic need keys")
        );
    }

    #[test]
    #[cfg(feature = "cloud-transcription")]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn the_overlay_keeps_the_cloud_transcription_name_over_the_pools_missing_keys() {
        // ElevenLabs transcribing, its key stored, neither pool
        // member's is. "ElevenLabs · OpenAI and Anthropic need keys" does
        // not fit, and the cloud name stays: audio is going there, so the
        // line must say so. The Models page still gives the reason.
        // Building the cloud engine sends nothing.
        let store = macalla_platform::secrets::MemoryStore::new();
        macalla_session::providers::store_key(&store, "elevenlabs", "sk-test").unwrap();
        let config = Config {
            engine: Some(a_ready_local_engine().id.to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "elevenlabs".to_string(),
                model: None,
            }),
            cleanup: openai_then_anthropic(),
            ..Config::default()
        };
        let opened = try_open(&config, &store);
        let status = opened_status(&opened, &config.cleanup);
        let session = match &opened {
            Opened::Ready(session) | Opened::PreviewMissing(session) => session,
            Opened::LocalFallback { .. } | Opened::Failed(_) => {
                panic!("ElevenLabs has its key, so transcription opens as chosen")
            }
        };
        assert_eq!(session.cloud_provider(), Some("elevenlabs"));
        assert_eq!(
            overlay_line_now(&status, session, &config.cleanup).as_deref(),
            Some("ElevenLabs")
        );
    }

    #[test]
    #[cfg(feature = "cloud-transcription")]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn opened_status_names_both_providers_when_transcription_and_a_different_pool_member_are_missing(
    ) {
        // Transcription's OpenAI has no key; the pool's Anthropic member
        // builds, its Mistral member does not -- two different missing
        // providers, named together.
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        macalla_session::providers::store_key(&store, "anthropic", "sk-test").unwrap();
        let config = Config {
            engine: Some(engine.id.to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "openai".to_string(),
                model: None,
            }),
            cleanup: vec![
                EnhancerChoice::Anthropic {
                    model: "claude-test".into(),
                    key: KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
                EnhancerChoice::Endpoint {
                    url: "https://api.mistral.ai/v1".into(),
                    model: "voxtral".into(),
                    key: KeySource::Stored("mistral".into()),
                    reasoning: None,
                },
            ],
            ..Config::default()
        };
        let opened = try_open(&config, &store);
        let status = opened_status(&opened, &config.cleanup);
        assert_eq!(status.enhancement.as_deref(), Some("Anthropic"));
        assert_eq!(
            status.transcription_issue.as_deref(),
            Some("OpenAI needs a key")
        );
        assert_eq!(
            status.enhancement_issue.as_deref(),
            Some("Mistral needs a key")
        );
        // Only Anthropic built, so the overlay names a single provider, no
        // "+N" -- never Mistral, the ticked member that failed. "Anthropic ·
        // OpenAI and Mistral need keys" does not fit, and the cleanup label
        // never gives way to the issue.
        let Opened::LocalFallback { session, .. } = &opened else {
            panic!("expected a local fallback");
        };
        assert_eq!(
            overlay_line_now(&status, session, &config.cleanup).as_deref(),
            Some("Anthropic")
        );
    }

    #[test]
    #[cfg(feature = "cloud-transcription")]
    fn opened_status_for_preview_missing_never_claims_enhancement_is_in_use() {
        // A bogus local engine id always fails to open, while a real cloud
        // transcription choice and a real Enhancement choice both build
        // (building sends nothing): `Session::can_dictate` is still false,
        // so this is `Opened::PreviewMissing`, with Enhancement built
        // anyway.
        use macalla_session::choice::{EnhancerChoice, KeySource};
        let store = macalla_platform::secrets::MemoryStore::new();
        macalla_session::providers::store_key(&store, "elevenlabs", "sk-test").unwrap();
        macalla_session::providers::store_key(&store, "anthropic", "sk-test").unwrap();
        let config = Config {
            engine: Some("not-a-real-engine".to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "elevenlabs".to_string(),
                model: None,
            }),
            cleanup: vec![EnhancerChoice::Anthropic {
                model: "claude-test".into(),
                key: KeySource::Stored("anthropic".into()),
                reasoning: None,
            }],
            ..Config::default()
        };
        let opened = try_open(&config, &store);
        assert!(
            matches!(opened, Opened::PreviewMissing(_)),
            "expected PreviewMissing"
        );
        let status = opened_status(&opened, &config.cleanup);
        assert_eq!(status.enhancement, None);
    }

    // The `dictation-error` payloads `notices_for` picks out of each
    // `Opened` outcome. The full emitted sequence -- `dictation-state:
    // preparing`, those payloads, then `dictation-state: idle` -- is
    // checked further down, in the `emit_and_open` tests, with a recording
    // `Notify` in place of an `AppHandle`.
    #[test]
    fn a_failed_open_gives_one_notice_with_the_failure_message() {
        let opened = Opened::Failed("the engine could not be reached".to_string());
        assert_eq!(
            notices_for(&opened),
            vec!["the engine could not be reached".to_string()]
        );
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn a_ready_local_engine_with_nothing_to_clean_up_has_no_notice() {
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        let config = Config {
            engine: Some(engine.id.to_string()),
            ..Config::default()
        };
        let opened = try_open(&config, &store);
        assert!(matches!(opened, Opened::Ready(_)));
        assert_eq!(notices_for(&opened), Vec::<String>::new());
    }

    // -------------------------------------------------------- emit_and_open

    /// A `Notify` that keeps what was sent instead of reaching a window, so
    /// a test can check `open_session`'s emitted sequence without an
    /// `AppHandle`.
    #[derive(Default)]
    struct Recorder {
        events: std::cell::RefCell<Vec<(&'static str, String)>>,
    }

    impl Notify for Recorder {
        fn send(&self, event: &'static str, payload: String) {
            self.events.borrow_mut().push((event, payload));
        }
    }

    #[test]
    fn a_failed_open_emits_preparing_then_its_notice_then_idle() {
        let recorder = Recorder::default();
        let opened = emit_and_open(
            &recorder,
            || Opened::Failed("the engine could not be reached".to_string()),
            |_| {},
        );
        assert!(matches!(opened, Opened::Failed(_)));
        assert_eq!(
            recorder.events.into_inner(),
            vec![
                ("dictation-state", "preparing".to_string()),
                (
                    "dictation-error",
                    "the engine could not be reached".to_string()
                ),
                ("dictation-state", "idle".to_string()),
            ]
        );
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "needs a ready local speech engine; only macOS has one without a download"
    )]
    fn a_cloud_fallback_emits_preparing_then_its_notice_then_idle() {
        let engine = a_ready_local_engine();
        let store = macalla_platform::secrets::MemoryStore::new();
        let config = Config {
            engine: Some(engine.id.to_string()),
            cloud: Some(macalla_session::choice::CloudTranscription {
                provider: "elevenlabs".to_string(),
                model: None,
            }),
            ..Config::default()
        };
        let recorder = Recorder::default();
        let opened = emit_and_open(&recorder, || try_open(&config, &store), |_| {});
        assert!(matches!(opened, Opened::LocalFallback { .. }));
        let events = recorder.events.into_inner();
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(events[0], ("dictation-state", "preparing".to_string()));
        assert_eq!(events[1].0, "dictation-error");
        assert!(events[1].1.contains("ElevenLabs"), "{}", events[1].1);
        assert_eq!(events[2], ("dictation-state", "idle".to_string()));
    }

    #[test]
    fn emit_and_open_calls_after_open_before_the_idle_emit() {
        // A page or the overlay refreshing off `dictation-state: idle` must
        // already see the new status, not the last dictation's -- so
        // `after_open` has to land before that emit, not after it.
        let recorder = Recorder::default();
        let seen: std::cell::RefCell<Option<usize>> = std::cell::RefCell::new(None);
        emit_and_open(
            &recorder,
            || Opened::Failed("unreachable".to_string()),
            |_| *seen.borrow_mut() = Some(recorder.events.borrow().len()),
        );
        let events = recorder.events.into_inner();
        let at = seen.into_inner().expect("after_open was called");
        assert_eq!(
            events.get(at),
            Some(&("dictation-error", "unreachable".to_string())),
            "after_open must run before dictation-error and dictation-state: idle, {events:?}"
        );
    }

    /// Notes, at `dictation-state: idle`, whether a Files page asking then
    /// would find the session's tools lent.
    struct LentAtIdle {
        lent: LentTools,
        at_idle: std::cell::Cell<Option<bool>>,
    }

    impl Notify for LentAtIdle {
        fn send(&self, event: &'static str, payload: String) {
            if event == "dictation-state" && payload == "idle" {
                self.at_idle.set(Some(self.lent.get().is_some()));
            }
        }
    }

    /// The Files page asks `meeting_destinations` again on `idle`, so the
    /// engine a reopen brought up must already be lent by then: lent only
    /// after `open_session` returned (and armed the microphone), the page's
    /// question could land in between and be told there is no engine. A
    /// failed open must be answered "none" just the same.
    #[test]
    fn a_reopen_lends_the_new_engine_before_it_says_idle() {
        let probe = LentAtIdle {
            lent: LentTools::default(),
            at_idle: std::cell::Cell::new(None),
        };
        open_and_lend(
            &probe,
            || Opened::Ready(fake_session()),
            |_| {},
            &probe.lent,
        );
        assert_eq!(
            probe.at_idle.get(),
            Some(true),
            "ready, but not lent at idle"
        );

        open_and_lend(
            &probe,
            || Opened::Failed("no engine".to_string()),
            |_| {},
            &probe.lent,
        );
        assert_eq!(probe.at_idle.get(), Some(false), "failed, but still lent");
    }

    fn channel_with(commands: &[Command]) -> (Sender<Command>, Receiver<Command>) {
        let (tx, rx) = mpsc::channel();
        for c in commands {
            tx.send(*c).unwrap();
        }
        (tx, rx)
    }

    #[test]
    fn nothing_waiting_means_keep_recording() {
        let (_tx, rx) = channel_with(&[]);
        assert_eq!(verdict(&rx, &mut Pending::default()), Verdict::KeepGoing);
    }

    #[test]
    fn a_release_that_arrived_behind_a_press_still_stops() {
        // Key down and key up both landed before the loop looked. The
        // release is a release; draining only the first command would
        // record until the cap.
        let (_tx, rx) = channel_with(&[Command::Start, Command::Stop]);
        assert_eq!(verdict(&rx, &mut Pending::default()), Verdict::Stop);
    }

    #[test]
    fn a_second_press_while_recording_is_not_a_second_dictation() {
        let (_tx, rx) = channel_with(&[Command::Start, Command::Start]);
        assert_eq!(verdict(&rx, &mut Pending::default()), Verdict::KeepGoing);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)), "drained");
    }

    #[test]
    fn a_tray_click_while_recording_stops_it() {
        let (_tx, rx) = channel_with(&[Command::Toggle]);
        assert_eq!(verdict(&rx, &mut Pending::default()), Verdict::Stop);
    }

    #[test]
    fn a_dictionary_edit_mid_dictation_is_remembered_not_acted_on() {
        let (_tx, rx) = channel_with(&[Command::ReloadDictionary]);
        let mut pending = Pending::default();
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing);
        assert!(pending.reload, "done once the dictation is over");
        assert!(!pending.switch);
    }

    #[test]
    fn a_microphone_chosen_mid_dictation_waits_for_it_to_end() {
        // The dictation stays on the microphone it started with: switching
        // under the engine changes the sample rate for no gain.
        let (_tx, rx) = channel_with(&[Command::SwitchDevice]);
        let mut pending = Pending::default();
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing);
        assert!(pending.device, "done once the dictation is over");
        assert!(!pending.switch && !pending.reload);
    }

    #[test]
    fn an_enhancement_change_mid_dictation_is_remembered_not_a_switch() {
        // Unlike `SwitchEngine`, this never reopens the session -- so it
        // must never set `pending.switch`, which would give up a Send-again
        // wait and reopen next turn for nothing.
        let (_tx, rx) = channel_with(&[Command::SwitchCleanup]);
        let mut pending = Pending::default();
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing);
        assert!(pending.cleanup, "done once the dictation is over");
        assert!(!pending.switch);
    }

    #[test]
    fn late_cleanup_to_keep_drains_then_gates_on_a_change_by_reach_and_on_closing() {
        // What both call sites share, as they see it: `verdict`'s own drain,
        // so a `SwitchCleanup` or a `SwitchEngine` queued at the last instant
        // still counts, not a stale read from before it arrived. `R` and `T`
        // are dummies here -- the real ones are `crate::history::Recorded`
        // and `macalla_session::LateCleanup`. What going without Enhancement
        // does to a real remote provider (no request, no further pool model)
        // is proven with a counting fake in `crates/macalla-session/src/
        // lib.rs`'s `enhancement_turned_off_before_release_sends_a_remote_
        // provider_nothing` and `enhancement_turned_off_during_the_wait_asks_
        // no_further_pool_model`.
        use macalla_session::choice::CleanupReach::{Local, Remote};
        for (command, reach, expected, why) in [
            (
                Some(Command::SwitchCleanup),
                Some(Remote),
                None,
                "a cleanup change queued at the last instant drops a remote provider's answer",
            ),
            (
                Some(Command::SwitchEngine),
                Some(Remote),
                None,
                "so does a switch",
            ),
            (
                Some(Command::SwitchCleanup),
                Some(Local),
                Some(((), ())),
                "a provider on this Mac finishes the dictation it started",
            ),
            (
                Some(Command::SwitchEngine),
                Some(Local),
                Some(((), ())),
                "a switch leaves it to finish too",
            ),
            (None, Some(Remote), Some(((), ())), "nothing pending: kept"),
            (
                Some(Command::Shutdown),
                Some(Local),
                None,
                "a Shutdown queued at the last instant drops it, read after the drain",
            ),
        ] {
            let (_tx, rx) = channel_with(command.as_slice());
            let mut pending = Pending::default();
            assert_eq!(
                late_cleanup_to_keep(&rx, &mut pending, false, reach, Some(()), Some(())),
                expected,
                "{why}"
            );
        }

        for reach in [None, Some(Local), Some(Remote)] {
            let (_tx, rx) = channel_with(&[]);
            let mut pending = Pending::default();
            assert_eq!(
                late_cleanup_to_keep(&rx, &mut pending, true, reach, Some(()), Some(())),
                None,
                "closing drops it even with nothing pending: {reach:?}"
            );
        }
    }

    #[test]
    fn a_discard_outranks_a_release_in_the_same_batch() {
        // Esc, Esc, then the key comes up -- all before the loop looked.
        // Letting go after discarding does not turn it back into a paste.
        let (_tx, rx) = channel_with(&[Command::Discard, Command::Stop]);
        assert_eq!(verdict(&rx, &mut Pending::default()), Verdict::Discard);
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "drained, as the doc says: nothing waits for the next dictation"
        );
    }

    #[test]
    fn a_press_during_the_wait_after_release_is_started_once_this_dictation_is_over() {
        // Hold to dictate, let go, and press again while cleanup or a cloud
        // request is still out: the press and its release both land in the
        // wait. Dropping the press lost the whole next dictation.
        let (tx, rx) = channel_with(&[Command::Start, Command::Stop]);
        let mut pending = Pending {
            released: true,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing);
        assert_eq!(pending.held, [Command::Start, Command::Stop]);

        // This dictation is over. The idle loop starts the held press before
        // anything that came later, and the next dictation hears its release
        // and then the rest of the channel.
        tx.send(Command::ReloadDictionary).unwrap();
        let mut held = pending.held;
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Start)
        );
        let mut next = Pending {
            held,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut next), Verdict::Stop);
        assert!(next.reload);
        assert!(next.held.is_empty());
    }

    #[test]
    fn a_tray_click_during_the_wait_after_release_is_started_once_this_dictation_is_over() {
        let (_tx, rx) = channel_with(&[Command::Toggle]);
        let mut pending = Pending {
            released: true,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "not a stop");
        let mut held = pending.held;
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Toggle),
            "the idle loop starts on a Toggle"
        );
        // A second click during the next dictation stops it, as ever.
        let (_tx, rx) = channel_with(&[Command::Toggle]);
        let mut next = Pending {
            held,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut next), Verdict::Stop);
    }

    #[test]
    fn a_key_coming_up_after_release_with_nothing_held_is_this_dictations_own() {
        // A hold that reached the cap, let go during the wait: nothing to
        // start.
        let (_tx, rx) = channel_with(&[Command::Stop]);
        let mut pending = Pending {
            released: true,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing);
        assert!(pending.held.is_empty());
    }

    #[test]
    fn a_press_behind_a_release_in_the_same_batch_is_the_next_dictation() {
        let (_tx, rx) = channel_with(&[Command::Stop, Command::Start]);
        let mut pending = Pending::default();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        assert_eq!(pending.held, [Command::Start]);
    }

    #[test]
    fn esc_twice_during_the_wait_takes_a_press_made_before_it_and_holds_one_made_after() {
        let (_tx, rx) = channel_with(&[Command::Start, Command::Discard, Command::Start]);
        let mut pending = Pending {
            released: true,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut pending), Verdict::Discard);
        assert_eq!(
            pending.held,
            [Command::Start],
            "only the press after Esc twice"
        );
    }

    /// The shortcut as the app runs it: the real recogniser, each action it
    /// gives sent down the channel as `start`'s shortcut thread forwards it,
    /// and what the overlay is told on [`KEY_HELD`] kept in `told`.
    struct Keyboard {
        recogniser: Arc<Mutex<macalla_platform::hotkey::Recogniser>>,
        key: macalla_platform::hotkey::Key,
        tx: Sender<Command>,
        held: KeyHeld,
        told: Arc<Mutex<Vec<bool>>>,
    }

    impl Keyboard {
        fn new(tx: Sender<Command>) -> Self {
            use macalla_platform::hotkey::{Mode, Recogniser, Shortcut};
            let shortcut = Shortcut::default_for_platform();
            Self {
                key: shortcut.key,
                recogniser: Arc::new(Mutex::new(Recogniser::new(shortcut, Mode::Hold))),
                tx,
                held: KeyHeld::default(),
                told: Arc::default(),
            }
        }

        fn forward(&self, action: Option<macalla_platform::hotkey::Action>) {
            if let Some(action) = action {
                self.tx.send(command_for(action)).unwrap();
            }
        }

        /// A key down or up, as the listener takes it: a change in whether
        /// the trigger is held is told first (`Listener::on_held`), then the
        /// action forwarded.
        fn key_event(&self, key: macalla_platform::hotkey::Key, down: bool) {
            let (action, held) = self.recogniser.lock().unwrap().key(key, down);
            if let Some(now) = held {
                self.held
                    .update(|| now, |held| self.told.lock().unwrap().push(held));
            }
            self.forward(action);
        }

        /// Everything the overlay has been told on [`KEY_HELD`], in order.
        fn told(&self) -> Vec<bool> {
            self.told.lock().unwrap().clone()
        }

        /// Esc twice, as the listener takes it.
        fn esc_twice_forwarded(&mut self) {
            use macalla_platform::hotkey::Key;
            for _ in 0..2 {
                self.key_event(Key::Esc, true);
                self.key_event(Key::Esc, false);
            }
        }

        fn press(&mut self) {
            self.key_event(self.key, true);
        }

        fn release(&mut self) {
            self.key_event(self.key, false);
        }

        /// `on_dictating` as `start` builds it on macOS: the recogniser is
        /// told, and its answer returned, and the overlay told whether the
        /// key is still held.
        fn on_dictating(&self) -> OnDictating {
            let recogniser = Arc::clone(&self.recogniser);
            let held_recogniser = Arc::clone(&self.recogniser);
            let told = Arc::clone(&self.told);
            with_key_held(
                Box::new(move |dictating| recogniser.lock().unwrap().set_dictating(dictating)),
                self.held.clone(),
                move || held_recogniser.lock().unwrap().is_held(),
                move |held| told.lock().unwrap().push(held),
            )
        }

        /// The same, with Esc twice landing just as the dictation is said to
        /// be over: the tap thread takes the recogniser first, so its Discard
        /// is in the channel only once `on_dictating(false)` has returned.
        fn on_dictating_as_esc_twice_lands(&self) -> OnDictating {
            let recogniser = Arc::clone(&self.recogniser);
            let tx = self.tx.clone();
            Box::new(move |dictating| {
                use macalla_platform::hotkey::Key;
                let mut r = recogniser.lock().unwrap();
                if !dictating {
                    for _ in 0..2 {
                        for action in [r.press(Key::Esc), r.release(Key::Esc)]
                            .into_iter()
                            .flatten()
                        {
                            tx.send(command_for(action)).unwrap();
                        }
                    }
                }
                r.set_dictating(dictating)
            })
        }

        fn esc_twice(&mut self) {
            for command in self.esc_twice_in_transit() {
                self.tx.send(command).unwrap();
            }
        }

        /// Esc twice, with what it gives still on its way through the
        /// shortcut thread: returned, not yet sent.
        fn esc_twice_in_transit(&mut self) -> Vec<Command> {
            use macalla_platform::hotkey::Key;
            let mut r = self.recogniser.lock().unwrap();
            let mut in_transit = Vec::new();
            for _ in 0..2 {
                in_transit.extend(r.press(Key::Esc).map(command_for));
                in_transit.extend(r.release(Key::Esc).map(command_for));
            }
            in_transit
        }
    }

    #[test]
    fn esc_twice_landing_as_the_shortcut_is_told_takes_the_press_before_it() {
        // Read before the shortcut is told, the channel would not yet hold
        // the Discard: the press before it would be started, and the Discard
        // read by that dictation instead.
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        keys.release();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        keys.press();
        pending.released = true;
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "the wait");
        assert_eq!(pending.held, [Command::Start]);
        assert!(end_dictation(
            &keys.on_dictating_as_esc_twice_lands(),
            &rx,
            &mut pending,
            false
        ));
        keys.release();

        held = pending.held;
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Err(RecvTimeoutError::Timeout),
            "nothing is started, and no Discard is left for the next dictation"
        );
    }

    #[test]
    fn esc_twice_still_crossing_the_shortcut_thread_at_the_end_is_a_discard() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx.clone());
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        keys.release();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        keys.press();
        pending.released = true;
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "the wait");
        // Esc twice after the session last asked, its Discard not yet through.
        let in_transit = keys.esc_twice_in_transit();
        assert_eq!(in_transit, [Command::Discard]);
        assert!(
            end_dictation(&keys.on_dictating(), &rx, &mut pending, false),
            "not kept, so nothing is sent"
        );
        assert!(pending.held.is_empty(), "the press before it goes with it");

        // It arrives once the dictation is over, and starts nothing.
        for command in in_transit {
            tx.send(command).unwrap();
        }
        keys.release();
        held = pending.held;
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Discard),
            "which the idle loop passes over"
        );
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Err(RecvTimeoutError::Timeout)
        );
    }

    fn send_to(tx: &Sender<Command>) -> impl FnOnce(Command) + '_ {
        |command| tx.send(command).unwrap()
    }

    /// `run` has no seam without an `AppHandle`, so this reads its source:
    /// the gate opens before the "listening" emit that shows the ×.
    #[test]
    fn the_cancel_gate_opens_before_the_overlay_shows_the_cancel_button() {
        let source = include_str!("dictation.rs");
        let body = &source[source.find("\nfn run(").expect("run")..];
        let opened = body.find("on_dictating(true);").expect("the gate opens");
        let listening = body
            .find("app.emit(\"dictation-state\", \"listening\")")
            .expect("the listening emit");
        assert!(opened < listening);
    }

    #[test]
    fn an_esc_twice_discard_is_not_logged_as_a_click() {
        let gate = CancelGate::default();
        let on_dictating = with_cancel_gate(Box::new(|dictating| !dictating), gate.clone());
        on_dictating(true);
        assert!(on_dictating(false), "the shortcut's discard comes through");
        assert!(!gate.ended_by_click());
    }

    #[test]
    fn a_cancel_click_while_recording_discards_it() {
        let (tx, rx) = mpsc::channel();
        let gate = CancelGate::default();
        gate.set_dictating(true);
        let mut pending = Pending::default();
        assert!(gate.cancel(send_to(&tx)));
        assert_eq!(verdict(&rx, &mut pending), Verdict::Discard);
        assert!(
            gate.set_dictating(false),
            "ending it reports the click, as the shortcut reports Esc twice"
        );
        assert!(!gate.cancel(send_to(&tx)), "a second click finds it over");
    }

    #[test]
    fn a_cancel_click_while_idle_sends_nothing_for_the_next_dictation() {
        let (tx, rx) = mpsc::channel();
        let gate = CancelGate::default();
        assert!(!gate.cancel(send_to(&tx)), "before any dictation");
        gate.set_dictating(true);
        assert!(!gate.set_dictating(false));
        assert!(!gate.cancel(send_to(&tx)), "after one has ended");
        let mut held = VecDeque::new();
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Err(RecvTimeoutError::Timeout),
            "nothing waits in the channel"
        );
        gate.set_dictating(true);
        assert_eq!(
            verdict(&rx, &mut Pending::default()),
            Verdict::KeepGoing,
            "the next dictation records on"
        );
    }

    #[test]
    fn a_cancel_click_during_the_wait_takes_the_press_before_it_and_leaves_nothing_behind() {
        // A press held from the wait would be started ahead of a Discard left
        // in the channel, and that Discard would then end the new dictation.
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx.clone());
        let gate = CancelGate::default();
        let on_dictating = with_cancel_gate(keys.on_dictating(), gate.clone());
        let mut held = VecDeque::new();

        keys.press();
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Start)
        );
        on_dictating(true);
        let mut pending = Pending::default();
        keys.release();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        keys.press();
        pending.released = true;
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "the wait");
        assert_eq!(pending.held, [Command::Start]);
        // Clicked after the session last asked.
        assert!(gate.cancel(send_to(&tx)));
        assert!(end_dictation(&on_dictating, &rx, &mut pending, false));
        assert!(gate.ended_by_click(), "the log names the click");
        keys.release();
        assert!(!gate.cancel(send_to(&tx)), "a late click finds it over");

        held = pending.held;
        assert!(held.is_empty(), "the press before the click goes with it");
        // The shortcut never heard the click, so that key's release is a
        // Stop, which the idle loop passes over like any release it did not
        // start on.
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Stop)
        );
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Err(RecvTimeoutError::Timeout),
            "nothing is started, and no Discard is left for the next dictation"
        );

        // And the next hold works as ever.
        keys.press();
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Start)
        );
        on_dictating(true);
        let mut next = Pending::default();
        assert_eq!(verdict(&rx, &mut next), Verdict::KeepGoing);
        keys.release();
        assert_eq!(verdict(&rx, &mut next), Verdict::Stop);
    }

    #[test]
    fn a_finish_click_while_recording_stops_it_once() {
        let (tx, rx) = mpsc::channel();
        let gate = CancelGate::default();
        gate.set_dictating(true);
        let mut pending = Pending::default();
        assert!(gate.finish(send_to(&tx)));
        assert_eq!(
            verdict(&rx, &mut pending),
            Verdict::Stop,
            "stopped, to paste"
        );
        assert!(!gate.finish(send_to(&tx)), "a second click sends nothing");
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(
            !gate.set_dictating(false),
            "a finish is not reported as a discard"
        );
        assert!(!gate.ended_by_click());
        assert!(!gate.finish(send_to(&tx)), "a late click finds it over");
    }

    #[test]
    fn a_finish_click_while_idle_sends_nothing_for_the_next_dictation() {
        let (tx, rx) = mpsc::channel();
        let gate = CancelGate::default();
        assert!(!gate.finish(send_to(&tx)), "before any dictation");
        gate.set_dictating(true);
        gate.set_dictating(false);
        assert!(!gate.finish(send_to(&tx)), "after one has ended");
        let mut held = VecDeque::new();
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Err(RecvTimeoutError::Timeout),
            "nothing waits in the channel to start one"
        );
        gate.set_dictating(true);
        assert_eq!(
            verdict(&rx, &mut Pending::default()),
            Verdict::KeepGoing,
            "the next dictation records on"
        );
        assert!(gate.finish(send_to(&tx)), "and a click can finish it");
    }

    #[test]
    fn a_finish_click_after_a_cancel_click_sends_nothing() {
        let (tx, rx) = mpsc::channel();
        let gate = CancelGate::default();
        gate.set_dictating(true);
        let mut pending = Pending::default();
        assert!(gate.cancel(send_to(&tx)));
        assert!(!gate.finish(send_to(&tx)));
        assert_eq!(verdict(&rx, &mut pending), Verdict::Discard);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn a_cancel_click_after_a_finish_click_still_discards() {
        // The × stays through the wait after a finish.
        let (tx, rx) = mpsc::channel();
        let gate = CancelGate::default();
        let on_dictating = with_cancel_gate(Box::new(|_| false), gate.clone());
        on_dictating(true);
        let mut pending = Pending::default();
        assert!(gate.finish(send_to(&tx)));
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        pending.released = true;
        assert!(gate.cancel(send_to(&tx)), "clicked in the wait");
        assert!(end_dictation(&on_dictating, &rx, &mut pending, false));
        assert!(gate.ended_by_click());
    }

    #[test]
    fn a_finish_that_lands_after_the_key_came_up_starts_nothing() {
        // The gate cannot see a key-up already in the channel: a Toggle here
        // would be held as the next dictation.
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx.clone());
        let gate = CancelGate::default();
        let on_dictating = with_cancel_gate(keys.on_dictating(), gate.clone());
        let mut held = VecDeque::new();

        keys.press();
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Start)
        );
        on_dictating(true);
        let mut pending = Pending::default();
        keys.release();
        assert!(gate.finish(send_to(&tx)), "sent behind the key-up");
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        assert!(pending.held.is_empty(), "passed over, not held");
        assert!(!end_dictation(&on_dictating, &rx, &mut pending, false));
        held = pending.held;
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Err(RecvTimeoutError::Timeout),
            "nothing is started"
        );
    }

    #[test]
    fn a_finish_during_the_wait_is_passed_over_and_a_held_press_still_starts() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx.clone());
        let gate = CancelGate::default();
        let on_dictating = with_cancel_gate(keys.on_dictating(), gate.clone());
        let mut held = VecDeque::new();

        keys.press();
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Start)
        );
        on_dictating(true);
        let mut pending = Pending::default();
        keys.release();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        keys.press();
        pending.released = true;
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "the wait");
        assert_eq!(pending.held, [Command::Start]);
        // Clicked in the wait, where the overlay shows no ✓.
        assert!(gate.finish(send_to(&tx)));
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "not a stop");
        assert!(!end_dictation(&on_dictating, &rx, &mut pending, false));
        assert_eq!(pending.held, [Command::Start], "the press is still held");

        // The held press starts the next dictation, and no Finish ends it.
        held = pending.held;
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Start)
        );
        on_dictating(true);
        let mut next = Pending::default();
        assert_eq!(verdict(&rx, &mut next), Verdict::KeepGoing);
        assert!(gate.finish(send_to(&tx)), "its own ✓ works");
        assert_eq!(verdict(&rx, &mut next), Verdict::Stop);
        keys.release();
        assert_eq!(
            verdict(&rx, &mut next),
            Verdict::KeepGoing,
            "the key-up goes"
        );
        assert!(next.held.is_empty());
    }

    #[test]
    fn a_finish_in_a_send_again_wait_is_passed_over() {
        let (_tx, rx) = channel_with(&[Command::Finish]);
        let mut pending = Pending {
            released: true,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing);
        assert!(pending.held.is_empty());
    }

    #[test]
    fn the_overlay_is_told_the_recording_ended_once() {
        let told = Cell::new(false);
        let count = Cell::new(0);
        tell_recording_ended(false, &told, || count.set(count.get() + 1));
        assert_eq!(count.get(), 0, "still recording");
        tell_recording_ended(true, &told, || count.set(count.get() + 1));
        tell_recording_ended(true, &told, || count.set(count.get() + 1));
        assert_eq!(count.get(), 1);
    }

    /// The idle loop takes the next command and, on a press, the dictation
    /// starts: `on_dictating(true)`, and a fresh [`Pending`] with what was held.
    fn start_dictation(
        keys: &mut Keyboard,
        rx: &Receiver<Command>,
        held: &mut VecDeque<Command>,
    ) -> Pending {
        assert_eq!(
            next_command(held, rx, std::time::Duration::ZERO),
            Ok(Command::Start)
        );
        keys.on_dictating()(true);
        Pending {
            held: std::mem::take(held),
            ..Pending::default()
        }
    }

    #[test]
    fn a_held_press_is_told_before_its_dictation_starts_and_its_release_after() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        assert_eq!(keys.told(), [true], "before the Start is read");
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        assert_eq!(keys.told(), [true, true], "told again as it starts");
        keys.release();
        assert_eq!(keys.told(), [true, true, false]);
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        assert!(!end_dictation(
            &keys.on_dictating(),
            &rx,
            &mut pending,
            false
        ));
        assert_eq!(keys.told(), [true, true, false], "nothing more at the end");
    }

    #[test]
    fn a_tray_click_dictation_is_never_held() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx.clone());
        let mut held = VecDeque::new();

        // The key pressed and let go first, with no dictation to start.
        keys.press();
        keys.release();
        assert_eq!(keys.told(), [true, false]);
        // Idle, the idle loop would start on the one and pass over the other;
        // taken off here so only the tray's click is left.
        assert_eq!(rx.try_recv(), Ok(Command::Start));
        assert_eq!(rx.try_recv(), Ok(Command::Stop));

        tx.send(Command::Toggle).unwrap();
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Ok(Command::Toggle)
        );
        keys.on_dictating()(true);
        // The start tells the overlay again that the key is up, and only
        // that; the end has nothing new to tell.
        assert_eq!(keys.told(), [true, false, false]);
        let mut pending = Pending::default();
        tx.send(Command::Toggle).unwrap();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        assert!(!end_dictation(
            &keys.on_dictating(),
            &rx,
            &mut pending,
            false
        ));
        assert_eq!(keys.told(), [true, false, false]);
    }

    #[test]
    fn esc_twice_while_held_leaves_the_key_held_until_it_comes_up() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        keys.esc_twice_forwarded();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Discard);
        assert!(end_dictation(&keys.on_dictating(), &rx, &mut pending, true));
        assert_eq!(keys.told(), [true, true], "Option is still down");
        keys.release();
        assert_eq!(
            keys.told(),
            [true, true, false],
            "a key-up that stops nothing"
        );
    }

    #[test]
    fn a_dictation_ending_with_the_key_down_leaves_it_held_until_it_comes_up() {
        // At its cap: the recording ends and the dictation with it while the
        // key is still down, and its release, later, stops nothing.
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        assert!(!end_dictation(
            &keys.on_dictating(),
            &rx,
            &mut pending,
            false
        ));
        assert_eq!(keys.told(), [true, true], "still down");
        keys.release();
        assert_eq!(keys.told(), [true, true, false]);
        assert!(rx.try_recv().is_err(), "no Stop for a dictation now over");
    }

    #[test]
    fn a_press_during_the_wait_is_told_held_into_the_next_dictation() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        keys.release();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        keys.press();
        assert_eq!(keys.told(), [true, true, false, true], "held in the wait");
        assert!(!end_dictation(
            &keys.on_dictating(),
            &rx,
            &mut pending,
            false
        ));
        held = pending.held;
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        assert_eq!(keys.told(), [true, true, false, true, true], "still held");
        keys.release();
        assert_eq!(keys.told(), [true, true, false, true, true, false]);
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
    }

    #[test]
    fn a_press_during_the_wait_still_down_at_the_end_is_started_and_stopped_by_its_release() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        keys.release();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        keys.press();
        pending.released = true;
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "the wait");
        assert!(!end_dictation(
            &keys.on_dictating(),
            &rx,
            &mut pending,
            false
        ));

        held = pending.held;
        let mut next = start_dictation(&mut keys, &rx, &mut held);
        assert_eq!(verdict(&rx, &mut next), Verdict::KeepGoing);
        keys.release();
        assert_eq!(verdict(&rx, &mut next), Verdict::Stop);
    }

    #[test]
    fn a_press_after_esc_twice_starts_the_next_dictation_and_its_release_stops_it() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "recording");
        keys.esc_twice();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Discard);
        keys.release();
        // Pressed again while the engine finishes or the notice shows.
        keys.press();
        assert!(end_dictation(&keys.on_dictating(), &rx, &mut pending, true));

        held = pending.held;
        let mut next = start_dictation(&mut keys, &rx, &mut held);
        assert_eq!(verdict(&rx, &mut next), Verdict::KeepGoing);
        keys.release();
        assert_eq!(
            verdict(&rx, &mut next),
            Verdict::Stop,
            "stopped by its release, not the cap"
        );
    }

    #[test]
    fn a_press_before_esc_twice_goes_with_the_dictation_and_nothing_is_started() {
        let (tx, rx) = mpsc::channel();
        let mut keys = Keyboard::new(tx);
        let mut held = VecDeque::new();

        keys.press();
        let mut pending = start_dictation(&mut keys, &rx, &mut held);
        keys.release();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        keys.press();
        pending.released = true;
        assert_eq!(verdict(&rx, &mut pending), Verdict::KeepGoing, "the wait");
        assert_eq!(pending.held, [Command::Start]);
        // Esc twice with that key still down.
        keys.esc_twice();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Discard);
        keys.release();
        assert!(end_dictation(&keys.on_dictating(), &rx, &mut pending, true));

        held = pending.held;
        assert_eq!(
            next_command(&mut held, &rx, std::time::Duration::ZERO),
            Err(RecvTimeoutError::Timeout),
            "nothing is started"
        );
        // And the next hold works as ever.
        keys.press();
        let mut next = start_dictation(&mut keys, &rx, &mut held);
        keys.release();
        assert_eq!(verdict(&rx, &mut next), Verdict::Stop);
    }

    #[test]
    fn says_nothing_said_only_for_a_kept_empty_dictation() {
        use macalla_core::block::{Block, Document};
        let empty = || Document::new(Vec::new());
        let heard = || {
            Document::new(vec![Block::Paragraph {
                text: "kubectl ran".to_string(),
            }])
        };
        let dictation = |document: Document, heard_speech: Option<bool>, discarded: bool| {
            Ok(macalla_session::Dictation {
                document,
                transcripts: Vec::new(),
                timing: macalla_core::latency::Dictation::default(),
                device: String::new(),
                device_fault: None,
                engine_fault: None,
                stream_warnings: 0,
                heard_speech,
                cloud: None,
                local_document: None,
                front_app: None,
                late_cleanup: None,
                enhancement: None,
                discarded,
            })
        };

        // discarded + empty -> false: Esc twice closes silently even though
        // the document is empty like a real nothing-said.
        assert!(!says_nothing_said(&dictation(empty(), None, true), true));
        // not discarded + empty -> true: a real accidental press.
        assert!(says_nothing_said(&dictation(empty(), None, false), false));
        // not discarded + heard_speech false -> true, even with text in the
        // document: the gate outranks it.
        assert!(says_nothing_said(
            &dictation(heard(), Some(false), false),
            false
        ));
        // not discarded + text -> false.
        assert!(!says_nothing_said(
            &dictation(heard(), Some(true), false),
            false
        ));
        // discarded + text -> false.
        assert!(!says_nothing_said(
            &dictation(heard(), Some(true), true),
            true
        ));
    }

    #[test]
    fn late_cleanup_reaches_history_whole_or_partial() {
        use macalla_core::block::{Block, Document};
        let paragraph = |text: &str| {
            Document::new(vec![Block::Paragraph {
                text: text.to_string(),
            }])
        };
        let history = HistoryHandle::in_memory_for_test();
        let pasted = macalla_session::Dictation {
            document: paragraph("kubectl ran"),
            transcripts: Vec::new(),
            timing: macalla_core::latency::Dictation::default(),
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
        };
        let row = history
            .record(&pasted, "fake")
            .expect("history keeps 30 days until someone chooses");
        let text = || {
            history
                .text_of(row.id)
                .unwrap()
                .map(|t| t.trim().to_string())
        };
        let late = |text: &str, partial| macalla_session::LateDocument {
            document: paragraph(text),
            ms: 900.0,
            partial,
        };

        // Back-to-back dictations: the next one took the provider, and only
        // the first segment came back. What did come back is kept.
        late_to_history(
            &history,
            row,
            &pasted.document,
            &late("Kubectl ran, and then", true),
        );
        assert_eq!(text().as_deref(), Some("Kubectl ran, and then"));

        late_to_history(
            &history,
            row,
            &pasted.document,
            &late("kubectl ran.", false),
        );
        assert_eq!(text().as_deref(), Some("kubectl ran."));
    }

    #[test]
    fn late_outcome_tells_a_change_apart_from_no_change_and_a_partial_wait() {
        let paragraph = |text: &str| {
            macalla_core::block::Document::new(vec![macalla_core::block::Block::Paragraph {
                text: text.to_string(),
            }])
        };
        let tier0 = paragraph("kubectl ran");
        let done = |text: &str, partial| macalla_session::LateDocument {
            document: paragraph(text),
            ms: 900.0,
            partial,
        };

        assert_eq!(
            late_outcome(&done("kubectl ran, and then", true), &tier0),
            LateOutcome::Partial,
            "a partial wait is reported first, even though the text changed"
        );
        assert_eq!(
            late_outcome(&done("Kubectl ran.", false), &tier0),
            LateOutcome::Changed,
            "Enhancement's answer differs from what was pasted"
        );
        assert_eq!(
            late_outcome(&done("kubectl ran", false), &tier0),
            LateOutcome::Unchanged,
            "the answer matches Tier 0: a timeout, a failure, or nothing to change"
        );
    }

    /// A dictation saying `text`, with a cloud outcome when one is given.
    fn said(text: &str, outcome: Option<CloudOutcome>) -> macalla_session::Dictation {
        use macalla_core::block::{Block, Document};
        macalla_session::Dictation {
            document: Document::new(vec![Block::Paragraph {
                text: text.to_string(),
            }]),
            // What the engine heard is the text itself, so a row that names
            // the wrong engine's hearing shows up in [`texts`].
            transcripts: vec![macalla_core::stt::Transcript {
                text: text.to_string(),
                is_final: true,
                start_ms: 0,
                end_ms: 500,
            }],
            timing: macalla_core::latency::Dictation::default(),
            device: String::new(),
            device_fault: None,
            engine_fault: None,
            stream_warnings: 0,
            heard_speech: None,
            cloud: outcome.map(|outcome| CloudResult {
                provider: "openai".into(),
                outcome,
                ms: 4_000.0,
                failures_in_a_row: 0,
            }),
            local_document: None,
            front_app: Some("com.example.notes".into()),
            late_cleanup: None,
            enhancement: None,
            discarded: false,
        }
    }

    /// Each row as (text, engine, what the engine heard), newest first.
    fn texts(history: &HistoryHandle) -> Vec<(String, String, String)> {
        history
            .entries_for_test()
            .into_iter()
            .map(|entry| {
                (
                    entry.enhanced.trim().to_string(),
                    entry.engine,
                    entry.raw.trim().to_string(),
                )
            })
            .collect()
    }

    /// A row of [`texts`] for a [`said`] dictation kept under `engine`.
    fn kept(text: &str, engine: &str) -> (String, String, String) {
        (text.to_string(), engine.to_string(), text.to_string())
    }

    /// [`to_history`] for a dictation the person kept, as the run loop's
    /// kept arm calls it.
    fn kept_after(
        history: &HistoryHandle,
        early: Option<crate::history::Recorded>,
        dictation: &macalla_session::Dictation,
        engine: &str,
    ) -> Option<crate::history::Recorded> {
        to_history(
            history,
            history_step(early, Some(dictation), false, false),
            dictation,
            engine,
        )
    }

    #[test]
    fn what_this_mac_heard_is_in_history_while_the_request_is_out_and_the_answer_replaces_it() {
        let history = HistoryHandle::in_memory_for_test();
        let row = keep_heard(&history, &said("cube control ran", None), "apple")
            .expect("History is on, so the heard text has a row as the wait starts");
        assert_eq!(texts(&history), [kept("cube control ran", "apple")]);

        let answered = said("kubectl ran.", Some(CloudOutcome::Transcribed));
        assert_eq!(
            kept_after(&history, Some(row), &answered, "openai"),
            Some(row),
            "the same row"
        );
        assert_eq!(
            texts(&history),
            [kept("kubectl ran.", "openai")],
            "replaced, not added to, under the provider's name and with what it heard"
        );

        // A request that failed or timed out leaves what this Mac heard.
        let row = keep_heard(&history, &said("second one", None), "apple").unwrap();
        for outcome in [
            CloudOutcome::Failed(CloudFailure::Other("HTTP 500".into())),
            CloudOutcome::TimedOut,
        ] {
            let failed = said("second one", Some(outcome));
            assert_eq!(kept_after(&history, Some(row), &failed, "apple"), Some(row));
        }
        assert_eq!(
            texts(&history),
            [kept("second one", "apple"), kept("kubectl ran.", "openai")]
        );

        // No early row: recorded as it always was.
        let row = kept_after(&history, None, &said("a local one", None), "apple").unwrap();
        assert_eq!(
            history.text_of(row.id).unwrap().as_deref(),
            Some("a local one")
        );
    }

    #[test]
    fn a_dictation_discarded_during_the_wait_keeps_no_row_and_neither_does_an_empty_answer() {
        let history = HistoryHandle::in_memory_for_test();
        // Esc twice while the request was out: the run loop's discard arm.
        let row = keep_heard(&history, &said("never mind", None), "apple").unwrap();
        history.forget(row);
        assert!(texts(&history).is_empty());

        // The provider heard nothing: no text, so no row.
        let row = keep_heard(&history, &said("a cough", None), "apple").unwrap();
        let empty = said("", Some(CloudOutcome::Transcribed));
        assert_eq!(kept_after(&history, Some(row), &empty, "openai"), None);
        assert!(texts(&history).is_empty());

        // Nothing heard, or History off: there is no row to write.
        assert_eq!(keep_heard(&history, &said("  ", None), "apple"), None);
        history
            .set_retention(macalla_core::history::Retention::Off)
            .unwrap();
        assert_eq!(keep_heard(&history, &said("not kept", None), "apple"), None);
        assert!(texts(&history).is_empty());
    }

    #[test]
    fn a_discarded_dictation_is_never_written_as_its_request_goes_out() {
        let history = HistoryHandle::in_memory_for_test();
        let mut thrown_away = said("never mind", None);
        thrown_away.discarded = true;
        assert_eq!(keep_heard(&history, &thrown_away, "apple"), None);
        assert!(texts(&history).is_empty(), "not even for a moment");
    }

    #[test]
    fn an_answer_whose_early_row_has_gone_is_kept_in_a_row_of_its_own_or_nowhere_with_history_off()
    {
        let answered = said("kubectl ran.", Some(CloudOutcome::Transcribed));

        // Deleted from the History page while the request was out.
        let history = HistoryHandle::in_memory_for_test();
        let early = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        history.forget(early);
        let row = kept_after(&history, Some(early), &answered, "openai")
            .expect("kept, so the overlay may say it is in History");
        assert_eq!(
            history.text_of(row.id).unwrap().as_deref(),
            Some("kubectl ran.")
        );
        assert_eq!(texts(&history), [kept("kubectl ran.", "openai")]);

        // History switched off while the request was out: kept nowhere, and
        // no row to say it is in.
        let history = HistoryHandle::in_memory_for_test();
        let early = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        history
            .set_retention(macalla_core::history::Retention::Off)
            .unwrap();
        assert_eq!(kept_after(&history, Some(early), &answered, "openai"), None);
        assert!(texts(&history).is_empty());
    }

    #[test]
    fn sending_again_to_a_row_that_has_gone_says_the_text_was_not_kept() {
        let history = HistoryHandle::in_memory_for_test();
        let row = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        history.forget(row);
        let transcribed = macalla_session::SentAgain {
            cloud: CloudResult {
                provider: "openai".into(),
                outcome: CloudOutcome::Transcribed,
                ms: 2_000.0,
                failures_in_a_row: 0,
            },
            engine: "cloud-engine",
            transcripts: said("kubectl ran.", None).transcripts,
            document: Some(said("kubectl ran.", None).document),
            late_cleanup: None,
        };
        let notice = sent_again_to_history(&history, row, Some(transcribed))
            .expect("says the new text was kept nowhere");
        assert!(notice.contains("History"), "{notice}");
        assert!(texts(&history).is_empty(), "not written back");
    }

    /// The row kept while a request was out, for the History decision.
    const EARLY: crate::history::Recorded = crate::history::Recorded {
        id: 3,
        ts_ms: 1_000,
    };

    /// What a dictation's commands left pending, for the tables below:
    /// released, as it is by the time the session asks whether to stop
    /// waiting, with a press held for the next dictation when `start`.
    fn pending_with(closed: bool, cleanup: bool, switch: bool, start: bool) -> Pending {
        Pending {
            closed,
            cleanup,
            switch,
            released: true,
            held: if start {
                VecDeque::from([Command::Start])
            } else {
                VecDeque::new()
            },
            ..Pending::default()
        }
    }

    #[test]
    fn enhancement_still_wanted_stops_a_remote_provider_on_a_change_and_every_one_on_close() {
        use macalla_session::choice::CleanupReach::{Local, Remote};
        // (closing, closed, cleanup, switch, reach) -> wanted.
        let cases = [
            // Nothing pending: wanted, whoever serves it.
            (false, false, false, false, Some(Remote), true),
            (false, false, false, false, Some(Local), true),
            (false, false, false, false, None, true),
            // A cleanup change or a switch retires a remote provider at once.
            (false, false, true, false, Some(Remote), false),
            (false, false, false, true, Some(Remote), false),
            (false, false, true, true, Some(Remote), false),
            // A provider on this Mac finishes the dictation it started.
            (false, false, true, false, Some(Local), true),
            (false, false, false, true, Some(Local), true),
            (false, false, true, true, Some(Local), true),
            (false, false, true, true, None, true),
            // Closing, as the caller read it or from a drained Shutdown:
            // never, for any provider.
            (true, false, false, false, Some(Local), false),
            (true, false, false, false, Some(Remote), false),
            (true, false, false, false, None, false),
            (false, true, false, false, Some(Local), false),
            (false, true, false, false, Some(Remote), false),
            (true, true, true, true, Some(Local), false),
        ];
        for (closing, closed, cleanup, switch, reach, expected) in cases {
            let pending = pending_with(closed, cleanup, switch, false);
            assert_eq!(
                enhancement_still_wanted(closing, &pending, reach),
                expected,
                "closing={closing} closed={closed} cleanup={cleanup} switch={switch} reach={reach:?}"
            );
        }
    }

    #[test]
    fn a_dictation_reaches_as_far_as_its_furthest_provider_and_a_pool_is_always_remote() {
        use macalla_session::choice::{
            cleanup_enhancers, CleanupChoice, CleanupReach, EnhancerChoice, KeySource, PoolModel,
        };
        let server = |url: &str| EnhancerChoice::Endpoint {
            url: url.into(),
            model: "m".into(),
            key: KeySource::None,
            reasoning: None,
        };
        assert_eq!(dictation_reach(&[]), None, "Enhancement off");
        assert_eq!(
            dictation_reach(&[EnhancerChoice::AppleIntelligence { guided: true }]),
            Some(CleanupReach::Local)
        );
        assert_eq!(
            dictation_reach(&[server("http://127.0.0.1:11434")]),
            Some(CleanupReach::Local)
        );
        assert_eq!(
            dictation_reach(&[server("http://studio.example:11434")]),
            Some(CleanupReach::Remote)
        );
        assert_eq!(
            dictation_reach(&[
                server("http://127.0.0.1:11434"),
                server("http://studio.example:11434")
            ]),
            Some(CleanupReach::Remote),
            "any member that sends the text away makes the whole dictation remote"
        );
        // Every provider a pool can hold is a cloud one, so a pool with a
        // member on this Mac cannot be built.
        let pool = cleanup_enhancers(&CleanupChoice::Pool {
            models: ["openai", "anthropic", "gemini", "mistral"]
                .map(|provider| PoolModel {
                    provider: provider.into(),
                    model: "m".into(),
                })
                .to_vec(),
        });
        assert_eq!(pool.len(), 4);
        assert!(pool.iter().all(|c| c.reach() == CleanupReach::Remote));
        assert_eq!(dictation_reach(&pool), Some(CleanupReach::Remote));
    }

    #[test]
    fn give_up_on_answers_every_wait_from_what_is_pending() {
        use macalla_session::choice::CleanupReach::{Local, Remote};
        use Waiting::{Continue, DeliverNow, GiveUp, WithoutEnhancement};
        // (discarded, closing, cloud, cleanup, switch, start, reach) -> answer.
        let rows = [
            // Nothing: carry on.
            (
                false,
                false,
                false,
                false,
                false,
                false,
                Some(Remote),
                Continue,
            ),
            (
                false,
                false,
                true,
                false,
                false,
                false,
                Some(Local),
                Continue,
            ),
            // Esc twice gives up, whatever else is pending.
            (true, true, true, true, true, true, Some(Remote), GiveUp),
            (true, false, false, false, false, false, None, GiveUp),
            // Closing: a cloud request is given up, as it always was...
            (false, true, true, false, false, false, Some(Local), GiveUp),
            (false, true, true, false, false, true, None, GiveUp),
            // ...and a local dictation goes without Enhancement: nothing more
            // is sent, and the shared quit deadline is not spent on it.
            (
                false,
                true,
                false,
                false,
                false,
                false,
                Some(Remote),
                WithoutEnhancement,
            ),
            (
                false,
                true,
                false,
                false,
                false,
                false,
                Some(Local),
                WithoutEnhancement,
            ),
            (
                false,
                true,
                false,
                true,
                true,
                true,
                Some(Remote),
                WithoutEnhancement,
            ),
            // A cleanup change or a switch stops a remote provider at once,
            // and outranks a press, which would hand its answer on.
            (
                false,
                false,
                false,
                true,
                false,
                false,
                Some(Remote),
                WithoutEnhancement,
            ),
            (
                false,
                false,
                true,
                false,
                true,
                false,
                Some(Remote),
                WithoutEnhancement,
            ),
            (
                false,
                false,
                false,
                true,
                false,
                true,
                Some(Remote),
                WithoutEnhancement,
            ),
            // A provider on this Mac finishes the dictation.
            (
                false,
                false,
                false,
                true,
                true,
                false,
                Some(Local),
                Continue,
            ),
            (false, false, false, true, false, false, None, Continue),
            // A press for the next dictation, with History on, cuts the
            // Enhancement wait short.
            (
                false,
                false,
                false,
                false,
                false,
                true,
                Some(Remote),
                DeliverNow,
            ),
            (
                false,
                false,
                false,
                true,
                false,
                true,
                Some(Local),
                DeliverNow,
            ),
            // In a cloud session too: the session waits out the
            // transcription on it, and cuts only Enhancement short.
            (false, false, true, false, false, true, None, DeliverNow),
        ];
        for (discarded, closing, cloud, cleanup, switch, start, reach, expected) in rows {
            assert_eq!(
                give_up_on(
                    discarded,
                    cloud,
                    &pending_with(closing, cleanup, switch, start),
                    reach,
                    true
                ),
                expected,
                "discarded={discarded} closing={closing} cloud={cloud} cleanup={cleanup} \
                 switch={switch} start={start} reach={reach:?}"
            );
        }

        // With nowhere to keep a late answer -- History off -- a press goes
        // without Enhancement instead: a remote provider not yet sent the text
        // is never sent it, and in a cloud session the transcription is still
        // waited for.
        for (cloud, reach) in [
            (false, Some(Remote)),
            (false, Some(Local)),
            (true, Some(Remote)),
            (true, None),
        ] {
            assert_eq!(
                give_up_on(
                    false,
                    cloud,
                    &pending_with(false, false, false, true),
                    reach,
                    false
                ),
                WithoutEnhancement,
                "cloud={cloud} reach={reach:?}"
            );
        }

        // Every combination, against the rows in the order they rank.
        for bits in 0..128u8 {
            let [discarded, closing, cloud, cleanup, switch, start, keeps_row] =
                [0, 1, 2, 3, 4, 5, 6].map(|at| bits >> at & 1 == 1);
            for reach in [None, Some(Local), Some(Remote)] {
                let expected = if discarded || (closing && cloud) {
                    GiveUp
                } else if closing || (reach == Some(Remote) && (cleanup || switch)) {
                    WithoutEnhancement
                } else if start && keeps_row {
                    DeliverNow
                } else if start {
                    WithoutEnhancement
                } else {
                    Continue
                };
                assert_eq!(
                    give_up_on(
                        discarded,
                        cloud,
                        &pending_with(closing, cleanup, switch, start),
                        reach,
                        keeps_row
                    ),
                    expected,
                    "discarded={discarded} closing={closing} cloud={cloud} cleanup={cleanup} \
                     switch={switch} start={start} reach={reach:?} keeps_row={keeps_row}"
                );
            }
        }
    }

    #[test]
    fn quitting_skips_the_enhancement_wait_so_the_paste_still_fits_the_quit_deadline() {
        use macalla_session::choice::CleanupReach::{Local, Remote};
        // `RunEvent::Exit`'s shared deadline in `lib.rs`: five seconds from
        // quitting.
        let quit = std::time::Instant::now();
        let deadline = Some(quit + std::time::Duration::from_secs(5));
        assert!(
            !paste_before_deadline(quit + macalla_session::ENHANCEMENT_WAIT, deadline),
            "waiting Enhancement out would leave no room to paste"
        );
        // Closing answers a local dictation with WithoutEnhancement, whoever
        // enhances it and whether or not History keeps it: nothing more is
        // sent, nothing is waited for, and the paste can start at once.
        for reach in [None, Some(Local), Some(Remote)] {
            for keeps_row in [true, false] {
                assert_eq!(
                    give_up_on(
                        false,
                        false,
                        &pending_with(true, false, false, false),
                        reach,
                        keeps_row
                    ),
                    Waiting::WithoutEnhancement,
                    "reach={reach:?} keeps_row={keeps_row}"
                );
            }
        }
        assert!(paste_before_deadline(quit, deadline));
    }

    #[test]
    fn paste_on_close_refuses_a_self_frontmost_app_or_too_close_a_deadline() {
        // Not closing at the call site: `frontmost_is_self` reads `false`
        // and `deadline_ok` reads `true` (no deadline set), which is always
        // let through -- a normal Stop is unaffected.
        assert!(paste_on_close(false, true));
        // Closing, plenty of the deadline left, and another app in front:
        // pastes.
        assert!(paste_on_close(false, true));
        // Closing and Macalla itself is the frontmost app: refused, whatever
        // the deadline.
        assert!(!paste_on_close(true, true));
        assert!(!paste_on_close(true, false));
        // Closing, another app in front, but too little of the deadline
        // left: refused.
        assert!(!paste_on_close(false, false));
    }

    #[test]
    fn keep_late_on_close_only_while_not_closing() {
        assert!(keep_late_on_close(false));
        assert!(!keep_late_on_close(true));
    }

    #[test]
    fn paste_before_deadline_needs_the_full_margin_left() {
        let now = std::time::Instant::now();
        // Not quitting, or a test that never set a deadline: always room.
        assert!(paste_before_deadline(now, None));
        // Comfortably inside the margin.
        assert!(paste_before_deadline(
            now,
            Some(now + PASTE_DEADLINE_MARGIN + std::time::Duration::from_millis(1))
        ));
        // Exactly the margin: still enough to start.
        assert!(paste_before_deadline(
            now,
            Some(now + PASTE_DEADLINE_MARGIN)
        ));
        // Under the margin, and past the deadline entirely: too close either
        // way.
        assert!(!paste_before_deadline(
            now,
            Some(now + PASTE_DEADLINE_MARGIN - std::time::Duration::from_millis(1))
        ));
        assert!(!paste_before_deadline(now, Some(now)));
    }

    #[test]
    fn give_up_step_reads_shutdown_and_discard_off_a_real_channel() {
        use macalla_session::choice::CleanupReach::Remote;
        // After Shutdown: a local dictation is not given up -- it goes without
        // Enhancement, so nothing more is sent, and is pasted -- and
        // `pending.closed` is set, which is what the rest of `run` reads to
        // know the app is closing.
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Shutdown).unwrap();
        let mut pending = Pending::default();
        let discarded = Cell::new(false);
        assert_eq!(
            give_up_step(&rx, &mut pending, &discarded, false, Some(Remote), true),
            Waiting::WithoutEnhancement
        );
        assert!(pending.closed);

        // The same Shutdown gives up a cloud dictation: its answer cannot be
        // waited for.
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Shutdown).unwrap();
        let mut pending = Pending::default();
        let discarded = Cell::new(false);
        assert_eq!(
            give_up_step(&rx, &mut pending, &discarded, true, Some(Remote), true),
            Waiting::GiveUp
        );

        // Esc twice landing right behind Shutdown still gives up a local
        // dictation: the person's own discard always wins over closing.
        // `verdict`'s Shutdown arm returns as soon as it sees Shutdown, so
        // the queued Discard is only read on the next call -- the same shape
        // `give_up` is asked in more than once per dictation.
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Shutdown).unwrap();
        tx.send(Command::Discard).unwrap();
        let mut pending = Pending::default();
        let discarded = Cell::new(false);
        assert_eq!(
            give_up_step(&rx, &mut pending, &discarded, false, None, true),
            Waiting::WithoutEnhancement,
            "Shutdown alone"
        );
        assert_eq!(
            give_up_step(&rx, &mut pending, &discarded, false, None, true),
            Waiting::GiveUp,
            "then Discard"
        );
    }

    #[test]
    fn one_enhancement_change_is_answered_on_every_ask_not_only_the_one_that_drained_it() {
        use macalla_session::choice::CleanupReach::{Local, Remote};
        // The session drops an answer that is not GiveUp and asks again
        // before it sends, so the answer has to come from what is pending,
        // every time, and not from a command seen once.
        let (_tx, rx) = channel_with(&[Command::SwitchCleanup]);
        let mut pending = Pending::default();
        let discarded = Cell::new(false);
        assert_eq!(
            give_up_step(&rx, &mut pending, &discarded, false, Some(Remote), true),
            Waiting::WithoutEnhancement
        );
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)), "drained");
        assert_eq!(
            give_up_step(&rx, &mut pending, &discarded, false, Some(Remote), true),
            Waiting::WithoutEnhancement,
            "asked again, with nothing new on the channel"
        );

        // The same change leaves a provider on this Mac to finish, both times.
        let (_tx, rx) = channel_with(&[Command::SwitchCleanup]);
        let mut pending = Pending::default();
        for _ in 0..2 {
            assert_eq!(
                give_up_step(&rx, &mut pending, &discarded, false, Some(Local), true),
                Waiting::Continue
            );
        }
    }

    #[test]
    fn a_press_during_a_cloud_transcription_wait_is_held_and_discards_nothing() {
        // With History on the answer to a press is DeliverNow, and with it
        // off WithoutEnhancement. The session reads both as Continue while
        // the cloud engine has not answered (`crates/macalla-session/src/
        // lib.rs`'s `both_leave_a_cloud_request_alone_and_only_delivering_
        // now_sends_enhancement_for_later`), so the transcription is waited
        // for, and the press starts the next dictation once this one is over.
        for (keeps_row, expected) in [
            (true, Waiting::DeliverNow),
            (false, Waiting::WithoutEnhancement),
        ] {
            let (_tx, rx) = channel_with(&[Command::Start, Command::Stop]);
            let mut pending = Pending::default();
            let discarded = Cell::new(false);
            assert_eq!(
                give_up_step(&rx, &mut pending, &discarded, true, None, keeps_row),
                expected
            );
            assert!(!discarded.get(), "a press is never a discard");
            assert_eq!(pending.held, [Command::Start, Command::Stop]);
        }

        // Closing still gives up a cloud request, press or not.
        let (_tx, rx) = channel_with(&[Command::Start, Command::Shutdown]);
        let mut pending = Pending::default();
        let discarded = Cell::new(false);
        assert_eq!(
            give_up_step(&rx, &mut pending, &discarded, true, None, true),
            Waiting::GiveUp
        );
        assert!(!discarded.get(), "closing is not a discard either");
    }

    #[test]
    fn a_press_with_history_on_hands_the_late_answer_to_the_row_it_was_kept_in() {
        // The desktop's half of a press during a cloud wait with History on.
        // The session's half -- DeliverNow sends once and hands the answer on
        // -- is `crates/macalla-session/src/lib.rs`'s
        // `a_press_during_a_cloud_wait_sends_enhancement_once_and_hands_the_answer_on`.
        // A `LateDocument` stands in for what that answer's `wait` returns:
        // nothing outside the session can build one with a fake provider.
        use macalla_session::choice::CleanupReach::Remote;
        let history = HistoryHandle::in_memory_for_test();
        assert!(history.keeps_dictations(), "History is on by default");
        let (_tx, rx) = channel_with(&[Command::Start]);
        let mut pending = Pending::default();
        let discarded = Cell::new(false);
        assert_eq!(
            give_up_step(
                &rx,
                &mut pending,
                &discarded,
                true,
                Some(Remote),
                history.keeps_dictations()
            ),
            Waiting::DeliverNow
        );
        let tier0 = said("cube control ran", None).document;
        let row = keep_heard(&history, &said("cube control ran", None), "apple")
            .expect("kept while the request was out");
        let answer = macalla_session::LateDocument {
            document: said("Kubectl ran.", None).document,
            ms: 900.0,
            partial: false,
        };
        let (row, answer) = late_cleanup_to_keep(
            &rx,
            &mut pending,
            false,
            Some(Remote),
            Some(row),
            Some(answer),
        )
        .expect("kept for its row");
        late_to_history(&history, row, &tier0, &answer);
        assert_eq!(texts(&history)[0].0, "Kubectl ran.");
    }

    #[test]
    fn a_press_with_history_off_goes_without_enhancement_so_a_remote_provider_is_sent_nothing() {
        // What going without Enhancement sends a remote provider at the last
        // ask before sending -- nothing -- is `crates/macalla-session/src/
        // lib.rs`'s `enhancement_turned_off_before_release_sends_a_remote_
        // provider_nothing`.
        use macalla_session::choice::CleanupReach::Remote;
        let history = HistoryHandle::in_memory_for_test();
        history
            .set_retention(macalla_core::history::Retention::Off)
            .unwrap();
        assert!(!history.keeps_dictations());
        let (_tx, rx) = channel_with(&[Command::Start]);
        let mut pending = Pending::default();
        let discarded = Cell::new(false);
        assert_eq!(
            give_up_step(
                &rx,
                &mut pending,
                &discarded,
                true,
                Some(Remote),
                history.keeps_dictations()
            ),
            Waiting::WithoutEnhancement
        );
    }

    #[test]
    fn send_again_gives_up_for_the_next_dictation_and_goes_without_a_retired_remote_provider() {
        use macalla_session::choice::CleanupReach::{Local, Remote};
        use Waiting::{Continue, GiveUp, WithoutEnhancement};
        const OTHER: crate::history::Recorded = crate::history::Recorded {
            id: 4,
            ts_ms: 2_000,
        };
        // (start, switch, closed, let_go, cleanup, reach) -> answer.
        let rows = [
            (false, false, false, None, false, Some(Remote), Continue),
            // A press or a click: a dictation must not queue behind the wait.
            (true, false, false, None, false, Some(Local), GiveUp),
            // A switch reopens the session, which lets the recording go.
            (false, true, false, None, false, Some(Local), GiveUp),
            (false, true, false, None, false, None, GiveUp),
            // The app is closing.
            (false, false, true, None, false, Some(Local), GiveUp),
            // Its row was deleted; another row's deletion changes nothing.
            (false, false, false, Some(EARLY), false, None, GiveUp),
            (false, false, false, Some(OTHER), false, None, Continue),
            // A cleanup change: the transcription is still waited for, and a
            // remote provider is sent nothing as it answers.
            (
                false,
                false,
                false,
                None,
                true,
                Some(Remote),
                WithoutEnhancement,
            ),
            (false, false, false, None, true, Some(Local), Continue),
            (false, false, false, None, true, None, Continue),
        ];
        for (start, switch, closed, let_go, cleanup, reach, expected) in rows {
            let pending = Pending {
                let_go,
                ..pending_with(closed, cleanup, switch, start)
            };
            assert_eq!(
                send_again_answer(&pending, EARLY, reach),
                expected,
                "start={start} switch={switch} closed={closed} let_go={let_go:?} \
                 cleanup={cleanup} reach={reach:?}"
            );
        }
    }

    #[test]
    fn a_discard_late_esc_or_failure_forgets_the_early_row_and_quitting_leaves_it() {
        let answered = said("kubectl ran.", Some(CloudOutcome::Transcribed));
        let mut given_up = said("cube control ran", Some(CloudOutcome::Abandoned));
        given_up.discarded = true;

        // Esc twice while the session was still asking: it says discarded.
        assert_eq!(
            history_step(Some(EARLY), Some(&given_up), true, false),
            Keep::Forget(EARLY)
        );
        // Esc twice after the session last asked: only the last look saw it,
        // and the dictation it returned says nothing of it.
        assert_eq!(
            history_step(Some(EARLY), Some(&answered), true, false),
            Keep::Forget(EARLY),
            "late Esc"
        );
        // Esc twice as the app quits is still Esc twice.
        assert_eq!(
            history_step(Some(EARLY), Some(&given_up), true, true),
            Keep::Forget(EARLY)
        );
        // The dictation failed.
        assert_eq!(
            history_step(Some(EARLY), None, false, false),
            Keep::Forget(EARLY)
        );
        // Stopped because the channel closed, as the app quits: not the
        // person's discard, so what this Mac heard stays.
        assert_eq!(
            history_step(Some(EARLY), Some(&given_up), false, true),
            Keep::Leave(EARLY)
        );
        // Discarded, said to be neither Esc twice nor quitting: nothing is
        // inferred from that, and a discard keeps nothing.
        assert_eq!(
            history_step(Some(EARLY), Some(&given_up), false, false),
            Keep::Forget(EARLY)
        );
        // No early row: nothing to forget or leave, and nothing recorded.
        for (finished, esc_twice, closing) in [
            (Some(&given_up), true, false),
            (Some(&answered), true, false),
            (None, false, false),
            (Some(&given_up), false, true),
            (Some(&given_up), false, false),
        ] {
            assert_eq!(
                history_step(None, finished, esc_twice, closing),
                Keep::Nothing
            );
        }
    }

    #[test]
    fn a_finished_dictation_replaces_leaves_or_records_its_row() {
        let answered = said("kubectl ran.", Some(CloudOutcome::Transcribed));
        assert_eq!(
            history_step(Some(EARLY), Some(&answered), false, false),
            Keep::Replace(EARLY)
        );
        for outcome in [
            CloudOutcome::Failed(CloudFailure::Other("HTTP 500".into())),
            CloudOutcome::TimedOut,
        ] {
            let failed = said("cube control ran", Some(outcome));
            assert_eq!(
                history_step(Some(EARLY), Some(&failed), false, false),
                Keep::Leave(EARLY)
            );
        }
        let no_words = said("", Some(CloudOutcome::Transcribed));
        assert_eq!(
            history_step(Some(EARLY), Some(&no_words), false, false),
            Keep::Forget(EARLY),
            "an answer with no words is not worth a row"
        );
        assert_eq!(
            history_step(None, Some(&said("a local one", None)), false, false),
            Keep::Record
        );
        assert_eq!(
            history_step(None, Some(&said("  ", None)), false, false),
            Keep::Nothing
        );
    }

    #[test]
    fn a_failed_request_whose_early_row_was_deleted_leaves_no_row_to_send_again_from() {
        let history = HistoryHandle::in_memory_for_test();
        let early = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        history.forget(early); // Delete, on the History page, during the wait
        let failed = said("cube control ran", Some(CloudOutcome::TimedOut));
        let step = history_step(Some(early), Some(&failed), false, false);
        assert_eq!(to_history(&history, step, &failed, "apple"), None);
    }

    #[test]
    fn a_held_row_that_history_no_longer_has_is_the_one_to_let_go() {
        let history = HistoryHandle::in_memory_for_test();
        let row = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        assert_eq!(held_row_gone(&history, None), None, "nothing held");
        assert_eq!(
            held_row_gone(&history, Some(row)),
            None,
            "still in History, so still there to send again"
        );
        history
            .set_retention(macalla_core::history::Retention::Days(7))
            .unwrap();
        assert_eq!(
            held_row_gone(&history, Some(row)),
            None,
            "a rule that still keeps it"
        );
        history
            .set_retention(macalla_core::history::Retention::Off)
            .unwrap();
        assert_eq!(
            held_row_gone(&history, Some(row)),
            Some(row),
            "History switched to Not at all"
        );

        // Deleted from the History page.
        let history = HistoryHandle::in_memory_for_test();
        let row = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        history.delete(row.id).unwrap();
        assert_eq!(held_row_gone(&history, Some(row)), Some(row));
    }

    #[test]
    fn send_again_and_letting_go_during_a_dictation_are_remembered_for_when_it_is_over() {
        let row = crate::history::Recorded {
            id: 7,
            ts_ms: 1_000,
        };
        let (_tx, rx) = channel_with(&[
            Command::SendAgain { row },
            Command::LetGo { row },
            Command::Stop,
        ]);
        let mut pending = Pending::default();
        assert_eq!(
            verdict(&rx, &mut pending),
            Verdict::Stop,
            "neither is a trigger"
        );
        assert_eq!(pending.send_again, Some(row), "the row, id and time");
        assert_eq!(pending.let_go, Some(row));
        assert!(pending.held.is_empty());
    }

    #[test]
    fn only_the_cap_notice_keeps_the_dictation_thread_waiting() {
        // The overlay hides the moved-on notice itself: a push-to-talk in the
        // app the person moved to must start at once, not 3 s late.
        assert_eq!(overlay_hold(false, true), None);
        assert_eq!(overlay_hold(false, false), None);
        assert_eq!(overlay_hold(true, false), Some(NOTICE));
        assert_eq!(overlay_hold(true, true), Some(NOTICE));
    }

    #[test]
    fn a_notice_hides_the_overlay_only_if_no_dictation_has_started_since() {
        let shown = OverlayShown::default();
        let notice = shown.current();
        let mut hid = 0;
        assert!(
            shown.hide_if_still(notice, || hid += 1),
            "nothing started since"
        );
        assert_eq!(hid, 1);

        let notice = shown.current();
        drop(shown.starting());
        assert!(
            !shown.hide_if_still(notice, || panic!("hid the next dictation's overlay")),
            "a dictation started since"
        );
    }

    /// The source of one function, with the test module cut off first so a
    /// string written in a test can never stand in for the code it is
    /// checking. `from` is the function's signature, and the slice ends at
    /// the first column-zero `}`, which is that function's own end.
    fn body_of(from: &str) -> &'static str {
        let source = include_str!("dictation.rs");
        let code = &source[..source.find("\n#[cfg(test)]").expect("the test module")];
        let body = &code[code.find(from).expect(from)..];
        &body[..body.find("\n}\n").expect("the end of the function")]
    }

    /// `hide_overlay` has no seam without an `AppHandle`, so this reads its
    /// source, the way `the_cancel_gate_opens_before_the_overlay_shows_the_cancel_button`
    /// does -- but bounded to the function, which that one is not: the page
    /// is told before the overlay goes, so it can throw the dictation's
    /// words and notices away and hold nothing until the next one. Only the
    /// emit is pinned here; what the page then clears is in
    /// `apps/desktop/src/overlay.tsx` (`keeps`), which no test runner reaches.
    #[test]
    fn hiding_the_overlay_tells_the_page_it_is_hidden() {
        let body = body_of("\nfn hide_overlay(");
        let told = body
            .find("app.emit(\"dictation-overlay-hidden\"")
            .expect("the page is told");
        let panel = body
            .find("overlay_panel::hide(app)")
            .expect("the panel hide");
        let window = body.find("overlay.hide()").expect("the window hide");
        assert!(told < panel, "told before the panel goes");
        assert!(told < window, "told before the window goes");
    }

    /// `run` has no seam without an `AppHandle` either, so this reads its
    /// source too: the dictation is counted before the first event it emits,
    /// and the overlay is shown before the count is dropped. A notice from
    /// the last dictation coming due in that window would otherwise pass the
    /// guard and land its `dictation-overlay-hidden` between "listening" and
    /// `dictation-destination`, and nothing re-emits the destination -- the
    /// label would be missing for the whole run.
    #[test]
    fn a_dictation_is_counted_before_the_first_event_it_emits() {
        let body = body_of("\nfn run(");
        let counted = body.find("overlay_shown.starting()").expect("the count");
        let listening = body
            .find("app.emit(\"dictation-state\", \"listening\")")
            .expect("the listening emit");
        let destination = body
            .find("app.emit(\"dictation-destination\"")
            .expect("the destination emit");
        let shown = body.find("show_overlay(&app)").expect("the show");
        let dropped = body.find("drop(counted)").expect("the count is let go");
        assert!(counted < listening, "counted before \"listening\"");
        assert!(listening < destination, "\"listening\" before the label");
        assert!(destination < shown, "the label before the overlay is up");
        assert!(shown < dropped, "the count is held until it is up");
    }

    #[test]
    fn a_notice_that_comes_due_while_a_dictation_is_starting_hides_nothing() {
        // The guard holds the count from before it counts until the overlay
        // is up, so a hide on another thread lands before all of the
        // dictation's events or after them, never between. The source-
        // reading test above pins which events that covers.
        let shown = OverlayShown::default();
        let notice = shown.current();
        let events = Arc::new(Mutex::new(Vec::new()));
        let (showing, started) = mpsc::channel();
        let (go_on, go) = mpsc::channel::<()>();
        let starting = {
            let (shown, events) = (shown.clone(), Arc::clone(&events));
            std::thread::spawn(move || {
                let _counted = shown.starting();
                showing.send(()).unwrap();
                go.recv().unwrap();
                events.lock().unwrap().push("shown");
            })
        };
        started.recv().unwrap();
        let hiding = {
            let (shown, events) = (shown.clone(), Arc::clone(&events));
            std::thread::spawn(move || {
                shown.hide_if_still(notice, || events.lock().unwrap().push("hidden"))
            })
        };
        go_on.send(()).unwrap();
        starting.join().unwrap();
        assert!(!hiding.join().unwrap(), "the dictation started first");
        assert_eq!(*events.lock().unwrap(), ["shown"]);
    }

    #[test]
    fn sending_again_replaces_the_rows_text_or_says_why_not_in_the_providers_words() {
        use macalla_session::SentAgain as Sent;
        let history = HistoryHandle::in_memory_for_test();
        let row = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        let result = |outcome| CloudResult {
            provider: "openai".into(),
            outcome,
            ms: 2_000.0,
            failures_in_a_row: 1,
        };

        // The real producer's shape: every cloud adapter wraps every failure
        // as `Stt`, so this is what an OpenAI rate limit actually looks like.
        let failed = Sent {
            cloud: result(CloudOutcome::Failed(CloudFailure::Transcribe(
                macalla_core::Error::Stt {
                    engine: "openai",
                    message: "HTTP 429: rate limited".into(),
                },
            ))),
            engine: "cloud-engine",
            transcripts: Vec::new(),
            document: None,
            late_cleanup: None,
        };
        let notice = sent_again_to_history(&history, row, Some(failed)).expect("said why");
        // The raw reason ("HTTP 429: rate limited") is logged, never shown.
        assert_eq!(
            notice,
            "OpenAI could not transcribe it this time either. Check its key under Providers on \
             the Models page, and your connection."
        );
        assert_eq!(
            texts(&history),
            [kept("cube control ran", "apple")],
            "left as it was"
        );

        let slow = Sent {
            cloud: result(CloudOutcome::TimedOut),
            engine: "cloud-engine",
            transcripts: Vec::new(),
            document: None,
            late_cleanup: None,
        };
        let notice = sent_again_to_history(&history, row, Some(slow)).unwrap();
        assert!(
            notice.contains("OpenAI") && notice.contains("in time"),
            "{notice}"
        );

        assert_eq!(
            sent_again_to_history(&history, row, None).as_deref(),
            Some(NO_LONGER_HELD),
            "nothing held after all"
        );

        let answer = said("kubectl ran.", None);
        let transcribed = Sent {
            cloud: result(CloudOutcome::Transcribed),
            engine: "cloud-engine",
            transcripts: answer.transcripts,
            document: Some(answer.document),
            late_cleanup: None,
        };
        assert_eq!(
            sent_again_to_history(&history, row, Some(transcribed)),
            None
        );
        // The provider's words under its engine and with what it heard, not
        // the on-device engine's hearing the row was first kept with.
        assert_eq!(
            texts(&history),
            [kept("kubectl ran.", "cloud-engine")],
            "the same row, replaced"
        );
    }

    #[test]
    fn send_again_goes_ahead_only_for_the_held_row_while_history_still_has_it() {
        let history = HistoryHandle::in_memory_for_test();
        let row = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        assert_eq!(sendable(&history, Some(row), row), Some(row));
        let other = crate::history::Recorded {
            ts_ms: row.ts_ms.wrapping_sub(1),
            ..row
        };
        assert_eq!(
            sendable(&history, Some(row), other),
            None,
            "the same id at another time is another dictation"
        );
        assert_eq!(sendable(&history, None, row), None, "nothing held");
        // History switched to Not at all, with the page still showing the row.
        history
            .set_retention(macalla_core::history::Retention::Off)
            .unwrap();
        assert_eq!(
            sendable(&history, Some(row), row),
            None,
            "gone from History, so not sent"
        );
    }

    #[test]
    fn an_answer_whose_replace_fails_with_an_error_is_not_kept_twice() {
        let history = HistoryHandle::in_memory_for_test();
        let early = keep_heard(&history, &said("cube control ran", None), "apple").unwrap();
        let answered = said("kubectl ran.", Some(CloudOutcome::Transcribed));
        HistoryHandle::fail_replaces_for_test(true);
        let row = kept_after(&history, Some(early), &answered, "openai");
        HistoryHandle::fail_replaces_for_test(false);
        assert_eq!(
            texts(&history),
            [kept("cube control ran", "apple")],
            "one row, not the heard text and the answer beside it"
        );
        assert_eq!(row, Some(early), "the row that is still there");
    }

    #[test]
    fn a_failed_or_slow_cloud_request_names_the_provider_and_says_nothing_was_pasted() {
        let result = |outcome| CloudResult {
            provider: "elevenlabs".into(),
            outcome,
            ms: 65_000.0,
            failures_in_a_row: 1,
        };
        let failed = cloud_failure(&result(CloudOutcome::Failed(CloudFailure::Transcribe(
            macalla_core::Error::Stt {
                engine: "elevenlabs",
                message: "HTTP 401: invalid key".into(),
            },
        ))))
        .expect("a failure is said");
        // The raw reason ("HTTP 401: invalid key") is logged, never shown.
        assert_eq!(
            failed,
            "ElevenLabs could not transcribe that, so nothing was pasted. Check its key under \
             Providers on the Models page, and your connection."
        );

        let slow = cloud_failure(&result(CloudOutcome::TimedOut)).expect("a timeout is said");
        assert_eq!(
            slow,
            "ElevenLabs did not answer in time, so nothing was pasted."
        );

        for quiet in [
            CloudOutcome::Transcribed,
            CloudOutcome::Abandoned,
            CloudOutcome::NoSpeech,
        ] {
            assert_eq!(cloud_failure(&result(quiet)), None);
        }
    }

    #[test]
    fn repeated_cloud_failure_notice_only_fires_at_the_threshold() {
        let result = |failures_in_a_row| CloudResult {
            provider: "elevenlabs".into(),
            outcome: CloudOutcome::Failed(CloudFailure::Other("HTTP 401: invalid key".into())),
            ms: 65_000.0,
            failures_in_a_row,
        };
        assert_eq!(
            repeated_cloud_failure_notice(&result(macalla_session::REPEATED_CLOUD_FAILURES - 1)),
            None,
            "short of the threshold, cloud_failure's sentence is enough"
        );
        let notice =
            repeated_cloud_failure_notice(&result(macalla_session::REPEATED_CLOUD_FAILURES))
                .expect("at the threshold, a stronger notice is emitted");
        assert!(
            notice.contains("ElevenLabs") && notice.contains("Models") && notice.contains("Local"),
            "{notice}"
        );
    }

    #[test]
    fn a_cloud_outcome_gets_one_notice_and_the_stronger_one_wins() {
        let result = |outcome, failures_in_a_row| CloudResult {
            provider: "elevenlabs".into(),
            outcome,
            ms: 65_000.0,
            failures_in_a_row,
        };
        let threshold = macalla_session::REPEATED_CLOUD_FAILURES;
        let first = result(
            CloudOutcome::Failed(CloudFailure::Other("HTTP 500".into())),
            1,
        );
        assert_eq!(cloud_notice(&first), cloud_failure(&first));

        let streak = result(CloudOutcome::TimedOut, threshold);
        assert_eq!(
            cloud_notice(&streak),
            repeated_cloud_failure_notice(&streak),
            "at the threshold the stronger notice is the only one"
        );

        for outcome in [
            CloudOutcome::Abandoned,
            CloudOutcome::NoSpeech,
            CloudOutcome::Transcribed,
        ] {
            let debug = format!("{outcome:?}");
            assert_eq!(
                cloud_notice(&result(outcome, threshold)),
                None,
                "{debug} during a streak is not another failure"
            );
        }
    }

    #[test]
    fn preview_missing_notice_gives_the_reason_when_there_is_one() {
        // The real producer's shape (`registry::open`): always this variant,
        // tagged `registry` -- never a mid-run failure, so the raw message
        // is never shown, whatever it says.
        let reason = macalla_core::Error::Stt {
            engine: "registry",
            message: "speech recognition is not authorized".into(),
        };
        assert_eq!(
            preview_missing_notice(Some(&reason)),
            "The local engine could not open, so dictation cannot start. Choose an engine \
             under Transcription on the Models page, and download its model there if it needs \
             one."
        );
        assert_eq!(
            preview_missing_notice(None),
            "The local engine could not open, so dictation cannot start."
        );
    }

    #[test]
    fn a_preview_fault_in_a_cloud_dictation_is_not_called_the_engine_stopping() {
        // A real producer's shape: the Apple engine's own stream errors
        // (macalla-engines::apple) are tagged with its own id, not
        // `registry` -- a mid-run failure, unlike `preview_error`'s.
        const STOPPED: &str = "Apple Speech stopped before it finished transcribing. If it \
                                happens again, choose a different engine under Transcription on \
                                the Models page.";
        let fault = || macalla_core::Error::Stt {
            engine: "apple-speech",
            message: "went away".into(),
        };
        assert_eq!(
            engine_fault_notice(None, &fault()),
            format!("The engine stopped partway. What it had heard was kept. {STOPPED}")
        );

        let sent = CloudResult {
            provider: "elevenlabs".into(),
            outcome: CloudOutcome::Transcribed,
            ms: 1.0,
            failures_in_a_row: 0,
        };
        assert_eq!(
            engine_fault_notice(Some(&sent), &fault()),
            format!(
                "The live preview stopped partway. The whole recording still went to \
                 ElevenLabs. {STOPPED}"
            )
        );

        // Nothing was sent, so nothing "still went" anywhere.
        let unsent = CloudResult {
            outcome: CloudOutcome::NoSpeech,
            ..sent
        };
        let notice = engine_fault_notice(Some(&unsent), &fault());
        assert_eq!(
            notice,
            format!("The live preview stopped partway. {STOPPED}")
        );
    }

    #[test]
    fn a_closed_channel_is_remembered_so_a_cloud_wait_gives_up_too() {
        // At shutdown the channel closes while a cloud request may be out.
        // Waiting out the cap would hold the app open for a minute.
        let (_tx, rx) = channel_with(&[]);
        let mut pending = Pending::default();
        verdict(&rx, &mut pending);
        assert!(!pending.closed, "an empty channel is not a closed one");

        let (tx, rx) = channel_with(&[Command::Stop]);
        drop(tx);
        let mut pending = Pending::default();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        assert!(pending.closed);
    }

    #[test]
    fn a_closed_channel_stops_because_nothing_can_say_so_later() {
        let (tx, rx) = channel_with(&[]);
        drop(tx);
        assert_eq!(verdict(&rx, &mut Pending::default()), Verdict::Stop);
    }
}

/// The crash-on-quit fix: releasing the speech engine before the process
/// exits. See lib.rs's `RunEvent::Exit` handler for how [`Dictator::shut_down`]
/// fits into the wider quit sequence.
#[cfg(test)]
mod shutdown {
    use super::*;
    use macalla_core::audio::AudioSegment;
    use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
    use std::time::{Duration, Instant};

    /// An engine that does nothing but say, on a channel, that it was
    /// dropped -- so a test can tell whether the Metal buffers behind a real
    /// one would have been freed. Never asked to stream or transcribe: these
    /// tests never call [`Session::dictate`].
    struct FakeEngine {
        dropped: Sender<()>,
    }

    impl SttEngine for FakeEngine {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> EngineCaps {
            EngineCaps::batch()
        }
        fn stream(&self, _ctx: &SttContext) -> macalla_core::Result<Box<dyn SttStream>> {
            Err(macalla_core::Error::Unsupported {
                engine: "fake",
                what: "streaming",
            })
        }
        fn transcribe(
            &self,
            _audio: &AudioSegment,
            _ctx: &SttContext,
        ) -> macalla_core::Result<Transcript> {
            Err(macalla_core::Error::Unsupported {
                engine: "fake",
                what: "batch transcription",
            })
        }
    }

    impl Drop for FakeEngine {
        fn drop(&mut self) {
            let _ = self.dropped.send(());
        }
    }

    fn fake_session() -> (Session, Receiver<()>) {
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let (dictionary, label) = macalla_session::load_dictionary(None, &[]).unwrap();
        let session = Session::with_engine(
            Box::new(FakeEngine {
                dropped: dropped_tx,
            }),
            dictionary,
            label,
            Config::default(),
        );
        (session, dropped_rx)
    }

    #[test]
    fn shutdown_sets_closed_and_stops_even_after_release() {
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Shutdown).unwrap();
        let mut pending = Pending::default();
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        assert!(pending.closed);

        let (tx, rx) = mpsc::channel();
        tx.send(Command::Shutdown).unwrap();
        let mut pending = Pending {
            released: true,
            ..Pending::default()
        };
        assert_eq!(verdict(&rx, &mut pending), Verdict::Stop);
        assert!(pending.closed);
    }

    #[test]
    fn cleanup_step_applies_only_when_nothing_else_already_reopens_or_closes() {
        // (cleanup, switch, closing) -> apply now.
        let cases = [
            (false, false, false, false),
            (true, false, false, true),
            (true, true, false, false),
            (true, false, true, false),
            (true, true, true, false),
            (false, true, false, false),
            (false, false, true, false),
        ];
        for (cleanup, switch, closing, expected) in cases {
            assert_eq!(
                cleanup_step(cleanup, switch, closing),
                expected,
                "cleanup={cleanup} switch={switch} closing={closing}"
            );
        }
    }

    #[test]
    fn keep_late_cleanup_after_drops_once_a_cleanup_change_or_a_switch_is_pending() {
        // Nothing goes to a provider the person switched away from: a
        // `SwitchCleanup` or a `SwitchEngine` seen first means the swap or
        // the reopen that follows retires the provider a kept late cleanup
        // would otherwise hand the text to. Every combination, the one rule
        // both the Send-again and the dictation-end call sites share.
        // (cleanup, switch) -> keep.
        let cases = [
            (false, false, true),
            (true, false, false),
            (false, true, false),
            (true, true, false),
        ];
        for (cleanup, switch, expected) in cases {
            assert_eq!(
                keep_late_cleanup_after(cleanup, switch),
                expected,
                "cleanup={cleanup} switch={switch}"
            );
        }
    }

    #[test]
    fn enhancing_names_a_provider_by_its_display_name_and_the_rest_by_kind() {
        use macalla_session::EnhancingWith;
        let anthropic = macalla_session::providers::find("anthropic")
            .expect("a provider a pool can hold")
            .name;
        for (with, expected) in [
            (
                EnhancingWith::AppleIntelligence,
                (EnhancingKind::AppleIntelligence, None),
            ),
            (
                EnhancingWith::ServerOnThisComputer,
                (EnhancingKind::ServerOnThisComputer, None),
            ),
            (EnhancingWith::Server, (EnhancingKind::Server, None)),
            (
                EnhancingWith::Provider("anthropic".into()),
                (EnhancingKind::Provider, Some(anthropic.to_string())),
            ),
            // A provider this build no longer lists is named by its id, as
            // the cloud wait's is: a provider, never a model.
            (
                EnhancingWith::Provider("gone".into()),
                (EnhancingKind::Provider, Some("gone".to_string())),
            ),
        ] {
            assert_eq!(enhancing_names(&with), expected, "{with:?}");
        }
    }

    #[test]
    fn the_enhancement_payloads_have_the_shape_commands_ts_mirrors() {
        // `apps/desktop/src/commands.ts`'s `Enhancing` and `EnhancementMissed`.
        assert_eq!(
            serde_json::to_value(Enhancing {
                with: EnhancingKind::ServerOnThisComputer,
                provider: None,
                elapsed_ms: 2_000,
            })
            .unwrap(),
            serde_json::json!({
                "with": "server-on-this-computer",
                "provider": null,
                "elapsed_ms": 2_000,
            })
        );
        assert_eq!(
            serde_json::to_value(EnhancementMissed {
                reason: MissedReason::OutOfTime,
                with: EnhancingKind::Provider,
                provider: Some("OpenAI".into()),
                in_history: true,
            })
            .unwrap(),
            serde_json::json!({
                "reason": "out-of-time",
                "with": "provider",
                "provider": "OpenAI",
                "in_history": true,
            })
        );
        assert_eq!(
            serde_json::to_value([EnhancingKind::AppleIntelligence, EnhancingKind::Server])
                .unwrap(),
            serde_json::json!(["apple-intelligence", "server"])
        );
        assert_eq!(
            serde_json::to_value(MissedReason::Failed).unwrap(),
            serde_json::json!("failed")
        );
    }

    #[test]
    fn the_level_payload_has_the_shape_commands_ts_mirrors() {
        // `apps/desktop/src/commands.ts`'s `DictationLevel`, on the
        // `dictation-level` event.
        assert_eq!(
            serde_json::to_value(DictationLevel {
                // -32.5 round-trips exactly through f32 -> f64 -> JSON; an
                // arbitrary decimal like -32.8 would not, and the comparison
                // below would fail on a float that serialized correctly.
                rms_dbfs: -32.5,
                loudness: LoudnessView::NearClipping,
            })
            .unwrap(),
            serde_json::json!({
                "rms_dbfs": -32.5,
                "loudness": "near-clipping",
            })
        );
        assert_eq!(
            serde_json::to_value([
                LoudnessView::Normal,
                LoudnessView::Loud,
                LoudnessView::NearClipping,
            ])
            .unwrap(),
            serde_json::json!(["normal", "loud", "near-clipping"])
        );
    }

    #[test]
    fn a_session_level_becomes_the_wire_payload() {
        for (loudness, expected) in [
            (macalla_session::Loudness::Normal, LoudnessView::Normal),
            (macalla_session::Loudness::Loud, LoudnessView::Loud),
            (
                macalla_session::Loudness::NearClipping,
                LoudnessView::NearClipping,
            ),
        ] {
            assert_eq!(LoudnessView::from(loudness), expected, "{loudness:?}");
        }
        let level = macalla_session::Level {
            rms: 0.5,
            peak: 0.9,
        };
        let wire = DictationLevel::from(level);
        // An RMS of 0.5 is about -6.02 dBFS.
        assert!((wire.rms_dbfs + 6.0206).abs() < 1e-3, "{wire:?}");
        // A 0.9 peak is about -0.9 dBFS: near clipping.
        assert_eq!(wire.loudness, LoudnessView::NearClipping);
        // Silence is a finite number, so it serialises as one, not null.
        let silence = DictationLevel::from(macalla_session::Level {
            rms: 0.0,
            peak: 0.0,
        });
        assert!(serde_json::to_value(silence).unwrap()["rms_dbfs"].is_number());
    }

    #[test]
    fn a_missed_enhancement_is_said_only_after_a_paste_and_in_history_only_for_a_late_answer_kept()
    {
        use macalla_session::{EnhancementOutcome, EnhancingWith};
        let missed = |reason: MissedReason, in_history: bool| {
            Some(EnhancementMissed {
                reason,
                with: EnhancingKind::Server,
                provider: None,
                in_history,
            })
        };
        // (outcome, inserted, closing, late_kept) -> note.
        let rows = [
            // Out of time, with its answer kept for the row: it will be in
            // History.
            (
                Some(EnhancementOutcome::OutOfTime),
                true,
                false,
                true,
                missed(MissedReason::OutOfTime, true),
            ),
            // Out of time with nowhere to keep it: History off, or a remote
            // provider switched away from.
            (
                Some(EnhancementOutcome::OutOfTime),
                true,
                false,
                false,
                missed(MissedReason::OutOfTime, false),
            ),
            // Failed: nothing more is coming, whatever was kept.
            (
                Some(EnhancementOutcome::Failed),
                true,
                false,
                true,
                missed(MissedReason::Failed, false),
            ),
            (
                Some(EnhancementOutcome::Failed),
                true,
                false,
                false,
                missed(MissedReason::Failed, false),
            ),
            // Not pasted -- another app in front, or the paste refused --
            // says nothing about Enhancement.
            (
                Some(EnhancementOutcome::OutOfTime),
                false,
                false,
                true,
                None,
            ),
            (Some(EnhancementOutcome::Failed), false, false, false, None),
            // Closing: no one is left to read it.
            (Some(EnhancementOutcome::OutOfTime), true, true, true, None),
            (Some(EnhancementOutcome::Failed), true, true, false, None),
            // Cut short by a press or a provider switched away from, applied,
            // or Enhancement off.
            (Some(EnhancementOutcome::Skipped), true, false, true, None),
            (Some(EnhancementOutcome::Applied), true, false, false, None),
            (None, true, false, false, None),
        ];
        for (outcome, inserted, closing, late_kept, expected) in rows {
            assert_eq!(
                enhancement_missed(
                    outcome,
                    Some(&EnhancingWith::Server),
                    inserted,
                    closing,
                    late_kept
                ),
                expected,
                "{outcome:?} inserted={inserted} closing={closing} late_kept={late_kept}"
            );
        }
        assert_eq!(
            enhancement_missed(Some(EnhancementOutcome::Failed), None, true, false, false),
            None,
            "with Enhancement off there is nothing to have missed"
        );
    }

    #[test]
    fn dictation_is_refused_with_a_sentence_while_a_live_meeting_has_the_microphone() {
        let microphone = MicrophoneOwner::default();
        let meeting = microphone.for_meeting().expect("a free microphone");

        assert_eq!(
            microphone.for_dictation().err(),
            Some(crate::meeting_words::DICTATION_DURING_MEETING)
        );
        assert_eq!(
            microphone.for_meeting().err(),
            Some(crate::meeting_words::LIVE_BUSY),
            "and a second meeting with it"
        );

        drop(meeting);
        assert!(
            microphone.for_dictation().is_ok(),
            "dictation works again once the meeting ends"
        );
    }

    #[test]
    fn a_live_meeting_is_refused_with_a_sentence_while_a_dictation_has_the_microphone() {
        let microphone = MicrophoneOwner::default();
        let dictation = microphone.for_dictation().expect("a free microphone");

        assert_eq!(
            microphone.for_meeting().err(),
            Some(crate::meeting_words::MEETING_DURING_DICTATION)
        );

        drop(dictation);
        assert!(microphone.for_meeting().is_ok());
    }

    #[test]
    fn send_again_is_refused_while_a_meeting_records_and_holds_the_microphone_otherwise() {
        let microphone = MicrophoneOwner::default();
        let meeting = microphone.for_meeting().unwrap();
        let refused = with_microphone_for_send_again(&microphone, || {
            unreachable!("no resend runs while a meeting records")
        });
        assert_eq!(refused, Err::<(), _>(crate::meeting_words::LIVE_BUSY));

        drop(meeting);
        let inside = with_microphone_for_send_again(&microphone, || microphone.for_meeting().err());
        assert_eq!(
            inside,
            Ok(Some(crate::meeting_words::MEETING_DURING_DICTATION)),
            "a meeting cannot start while the resend runs"
        );
        assert!(
            microphone.for_meeting().is_ok(),
            "given back once the resend is over"
        );
    }

    #[test]
    fn a_dictation_turn_holds_the_microphone_throughout_and_is_refused_while_a_meeting_records() {
        let microphone = MicrophoneOwner::default();
        let meeting = microphone.for_meeting().unwrap();
        let mut told = Vec::new();

        let refused = with_microphone_for_dictation(
            &microphone,
            |refused| told.push(refused),
            || unreachable!("no dictation runs while a meeting records"),
        );
        assert_eq!(refused, None::<()>);
        assert_eq!(told, [crate::meeting_words::DICTATION_DURING_MEETING]);

        drop(meeting);
        let inside = with_microphone_for_dictation(
            &microphone,
            |refused| told.push(refused),
            || microphone.for_meeting().err(),
        );
        assert_eq!(
            inside,
            Some(Some(crate::meeting_words::MEETING_DURING_DICTATION)),
            "a meeting cannot start while the dictation's turn runs"
        );
        assert_eq!(told.len(), 1, "nothing refused");
        assert!(
            microphone.for_meeting().is_ok(),
            "given back once the turn is over"
        );
    }

    #[test]
    fn a_dictation_turn_that_panics_still_gives_the_microphone_back() {
        let microphone = MicrophoneOwner::default();
        let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_microphone_for_dictation(&microphone, |_| {}, || panic!("the turn fell over"))
        }));
        assert!(turn.is_err());
        assert!(microphone.for_meeting().is_ok());
    }

    #[test]
    fn a_live_meeting_that_panics_still_gives_the_microphone_back() {
        let microphone = MicrophoneOwner::default();
        let claim = microphone.for_meeting().unwrap();
        let meeting = std::thread::spawn(move || {
            let _claim = claim;
            panic!("the meeting fell over");
        });
        assert!(meeting.join().is_err());

        assert!(microphone.for_dictation().is_ok(), "no restart needed");
    }

    #[test]
    fn loop_top_step_leaves_only_once_quitting() {
        assert_eq!(loop_top_step(false), IdleStep::Continue);
        assert_eq!(loop_top_step(true), IdleStep::Leave);
    }

    #[test]
    fn idle_step_leaves_on_shutdown_whichever_way_quitting_reads() {
        assert_eq!(idle_step(Command::Shutdown, false), IdleStep::Leave);
        assert_eq!(idle_step(Command::Shutdown, true), IdleStep::Leave);
    }

    #[test]
    fn idle_step_leaves_before_switch_reopen_or_start_once_quitting() {
        let row = crate::history::Recorded { id: 1, ts_ms: 0 };
        for command in [
            Command::SwitchEngine,
            Command::SwitchCleanup,
            Command::ReopenEngine,
            Command::Start,
            Command::Toggle,
            Command::SendAgain { row },
        ] {
            assert_eq!(
                idle_step(command, true),
                IdleStep::Leave,
                "{command:?} should leave once quitting"
            );
            assert_eq!(
                idle_step(command, false),
                IdleStep::Continue,
                "{command:?} should run as usual while not quitting"
            );
        }
    }

    #[test]
    fn idle_step_leaves_normal_commands_alone() {
        let row = crate::history::Recorded { id: 1, ts_ms: 0 };
        for command in [
            Command::Stop,
            Command::Discard,
            Command::Finish,
            Command::ReloadDictionary,
            Command::SwitchDevice,
            Command::LetGo { row },
        ] {
            assert_eq!(idle_step(command, false), IdleStep::Continue);
            assert_eq!(
                idle_step(command, true),
                IdleStep::Continue,
                "{command:?} touches no model and sends nothing, so quitting leaves it alone"
            );
        }
    }

    /// A thread that owns a session and leaves -- by way of [`leave`] --
    /// once told to. Mirrors the shape of `run`'s idle wait and its exit,
    /// without an `AppHandle`.
    fn spawn_idle_thread(
        rx: Receiver<Command>,
        lent: LentTools,
        mut session: Option<Session>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            loop {
                match rx.recv() {
                    Ok(Command::Shutdown) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
            leave(&lent, &mut session);
        })
    }

    #[test]
    fn shut_down_frees_the_engine_and_clears_what_was_lent() {
        let (session, dropped_rx) = fake_session();
        let lent = LentTools::default();
        // Lent before the thread starts, the way `run` lends the session's
        // tools ahead of every wait -- so there is no race with the thread
        // over when the lending happens.
        lent.set(Some(&session));
        assert!(lent.get().is_some(), "the tools were lent before the wait");
        let (tx, rx) = mpsc::channel();
        let handle = spawn_idle_thread(rx, lent.clone(), Some(session));

        let quitting = Arc::new(AtomicBool::new(false));
        let shutdown = ShutdownHandle {
            tx,
            lent: lent.clone(),
            quitting: Arc::clone(&quitting),
            deadline: QuitDeadline::default(),
            handle: Arc::new(Mutex::new(Some(handle))),
        };
        // signal() then wait(): what `Dictator::shut_down` does, driven
        // directly since building a real `AppHandle` needs a live app.
        shutdown.signal();
        assert!(quitting.load(Ordering::Relaxed), "signal() sets the flag");
        let finished = shutdown.wait(Instant::now() + Duration::from_secs(1));

        assert!(finished, "the idle thread had nothing to wait out");
        assert!(dropped_rx.try_recv().is_ok(), "the engine was not dropped");
        assert!(lent.get().is_none(), "the lent tools were not cleared");
    }

    #[test]
    fn shut_down_gives_up_at_the_deadline_on_a_thread_that_ignores_it() {
        let (tx, rx) = mpsc::channel();
        // Never reads the channel, so Shutdown is never seen. Sleeps far
        // past every deadline used below, rather than joining promptly, so
        // the test stays meaningful under a busy shared machine (a shorter
        // sleep could itself outlast the 100 ms deadline and the test would
        // pass for the wrong reason).
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            drop(rx);
        });
        let shutdown = ShutdownHandle {
            tx,
            lent: LentTools::default(),
            quitting: Arc::new(AtomicBool::new(false)),
            deadline: QuitDeadline::default(),
            handle: Arc::new(Mutex::new(Some(handle))),
        };

        let started = Instant::now();
        shutdown.signal();
        let finished = shutdown.wait(started + Duration::from_millis(100));
        let elapsed = started.elapsed();

        assert!(
            !finished,
            "a thread that ignores Shutdown must not report finished"
        );
        // Loose enough to hold under load (this Mac shares builders and a
        // CI runner): the deadline is 100 ms, so 2 s is still evidence
        // `wait` gave up rather than actually joining the 5 s sleep.
        assert!(
            elapsed < Duration::from_secs(2),
            "gave up long after the 100 ms deadline: {elapsed:?}"
        );
        // The thread is not joined: it ends by itself in a few seconds,
        // well after this test has returned.
    }
}
