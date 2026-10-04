// SPDX-License-Identifier: MIT
//! Model downloads for the Engine card: list, download with progress and
//! cancel, remove. Humble: the store and the download are the engines
//! crate's; this maps them to commands and events.

use crate::command_error::{CommandError, OrSay};
use crate::dictation::{Command, Dictator};
use macalla_engines::models::{self, ModelStatus, Progress, Store};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter, State, Wry};

/// The downloads in flight, by model id, each with its cancel flag, and the
/// gate around warm-up ([`WarmUpGate`]) so quitting can wait for one already
/// running rather than race it for the Metal buffers it opens.
#[derive(Clone, Default)]
pub struct Downloads {
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    warm_up: Arc<WarmUpGate>,
}

/// Keeps a warm-up ([`models::warm`], which opens a model and drops it) from
/// starting once the app is quitting, and lets quitting wait for one already
/// under way: `models::warm` opens the same ggml engine dictation does, so a
/// live one at exit trips the same Metal assert
/// (`crates/macalla-engines/src/transcribe.rs`, `Worker`) freeing the
/// dictation thread's own model guards against.
#[derive(Default)]
struct WarmUpGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    quitting: bool,
    warming: usize,
}

impl WarmUpGate {
    /// Count a warm-up starting, unless quitting has begun. Returns whether
    /// it may go ahead.
    fn enter(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.quitting {
            return false;
        }
        state.warming += 1;
        true
    }

    /// A warm-up has ended, whichever way -- called only by
    /// [`WarmUpGuard`]'s drop. Wakes anything in [`WarmUpGate::wait_until`].
    fn leave(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.warming = state.warming.saturating_sub(1);
        self.changed.notify_all();
    }

    /// Stop any further warm-up from starting, without waiting. The "set
    /// stop" half of the app's quit sequence, pulled apart from
    /// [`WarmUpGate::wait_until`] so every stop -- Files, the dictation
    /// thread, this gate -- goes out before anything waits on one.
    fn begin_quitting(&self) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .quitting = true;
    }

    /// Wait until `deadline` for a warm-up already running -- as of
    /// whenever quitting began -- to finish. Returns whether it did.
    fn wait_until(&self, deadline: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.warming > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, timeout) = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|p| p.into_inner());
            state = guard;
            if timeout.timed_out() && state.warming > 0 {
                return false;
            }
        }
        true
    }
}

/// A warm-up counted in ([`WarmUpGate::enter`]). Its drop counts it back out
/// ([`WarmUpGate::leave`]) on every path out of scope, including a panic
/// unwinding out of `models::warm` -- so a warm-up that panics cannot leave
/// the gate counting one that no longer exists, which would hang every
/// later quit until its deadline.
struct WarmUpGuard(Arc<WarmUpGate>);

impl Drop for WarmUpGuard {
    fn drop(&mut self) {
        self.0.leave();
    }
}

/// One model as the Engine card shows it.
#[derive(Debug, Clone, Serialize)]
pub struct ModelView {
    pub id: String,
    pub name: String,
    pub megabytes: u64,
    pub licence: String,
    pub attribution: String,
    /// `installed`, `partial`, `absent`, or `downloading`.
    pub state: String,
    /// Bytes on disk, for a partial or a download in progress.
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: String,
    pub bytes: u64,
    pub total: u64,
    /// `downloading`, `reconnecting`, `verifying` or `preparing`.
    pub phase: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelError {
    pub id: String,
    pub message: String,
}

/// What a download's `model-error` says: cancelled, from the flag the
/// thread already holds, or a real failure. Cancelling really does resume
/// next time -- `macalla_engines::models::download` appends to the partial
/// file and its own cancelled-download error says so ("it will resume from
/// N MB next time"), which this does not repeat, only the fact of it.
fn download_outcome_message(cancelled: bool) -> &'static str {
    if cancelled {
        "Download stopped. It will pick up where it left off next time."
    } else {
        "That model could not be downloaded. Try again on the Models page."
    }
}

impl Downloads {
    /// Count a warm-up starting, unless quitting has begun. `None` if it may
    /// not go ahead; otherwise a guard that counts it back out when dropped.
    fn enter_warm_up(&self) -> Option<WarmUpGuard> {
        self.warm_up
            .enter()
            .then(|| WarmUpGuard(Arc::clone(&self.warm_up)))
    }

    /// Stop any further warm-up from starting, without waiting. Called
    /// alongside `Meetings::stop_now`, before the app's quit sequence
    /// computes its shared deadline and asks the dictation thread to end
    /// (`Dictator::shut_down`).
    pub fn begin_quitting(&self) {
        self.warm_up.begin_quitting();
    }

    /// Wait until `deadline` for a warm-up already running to finish.
    /// Returns whether it did.
    pub fn wait_until(&self, deadline: Instant) -> bool {
        self.warm_up.wait_until(deadline)
    }
}

fn views(downloads: &Downloads) -> Vec<ModelView> {
    let store = Store::shared();
    let active = downloads
        .active
        .lock()
        .map(|d| d.clone())
        .unwrap_or_default();
    models::catalogue()
        .iter()
        .map(|m| {
            let (state, bytes) = if active.contains_key(m.id) {
                ("downloading", 0)
            } else {
                match store.status(m) {
                    ModelStatus::Installed(_) => ("installed", m.bytes),
                    ModelStatus::Partial { bytes } => ("partial", bytes),
                    ModelStatus::Absent => ("absent", 0),
                }
            };
            ModelView {
                id: m.id.to_string(),
                name: m.name.to_string(),
                megabytes: m.megabytes(),
                licence: m.licence.to_string(),
                attribution: m.attribution.to_string(),
                state: state.to_string(),
                bytes,
            }
        })
        .collect()
}

#[tauri::command]
pub fn models_list(downloads: State<'_, Downloads>) -> Vec<ModelView> {
    views(&downloads)
}

/// Start a download on its own thread. Progress arrives as `model-progress`
/// events; the end as `model-ready` or `model-error`. When it lands, the
/// dictation thread is told to open its engine again, which is what turns a
/// fresh install into a working one without a relaunch.
#[tauri::command]
pub fn model_download(
    app: AppHandle<Wry>,
    downloads: State<'_, Downloads>,
    dictator: State<'_, Dictator>,
    id: String,
) -> Result<(), CommandError> {
    let spec = models::find(&id)
        .ok_or_else(|| CommandError::from("That is not a model Macalla knows about."))?;
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut active = downloads.active.lock().map_err(|_| {
            CommandError::from("Could not start the download. Restart Macalla, then try again.")
        })?;
        if active.contains_key(&id) {
            return Ok(()); // already on its way
        }
        active.insert(id.clone(), cancel.clone());
    }
    let active = downloads.active.clone();
    let downloads = downloads.inner().clone();
    let dictator = dictator.inner().clone();
    std::thread::Builder::new()
        .name(format!("macalla-download-{id}"))
        .spawn(move || {
            let store = Store::shared();
            let mut last_reported = 0u64;
            let mut on_progress = |p: Progress| {
                let event = match p {
                    Progress::Received { bytes, total } => {
                        // At most one event per megabyte: the card cannot
                        // draw faster than that and the bridge should not try.
                        if bytes / 1_000_000 == last_reported / 1_000_000 && bytes != total {
                            return;
                        }
                        last_reported = bytes;
                        ProgressEvent {
                            id: spec.id.to_string(),
                            bytes,
                            total,
                            phase: "downloading".into(),
                        }
                    }
                    // Always sent: a stall is the moment the card most needs to
                    // say something.
                    Progress::Reconnecting { bytes, total } => ProgressEvent {
                        id: spec.id.to_string(),
                        bytes,
                        total,
                        phase: "reconnecting".into(),
                    },
                    Progress::Verifying => ProgressEvent {
                        id: spec.id.to_string(),
                        bytes: spec.bytes,
                        total: spec.bytes,
                        phase: "verifying".into(),
                    },
                };
                let _ = app.emit("model-progress", event);
            };
            let result = models::download(spec, &store, None, &mut on_progress, &cancel);
            if let Ok(mut active) = active.lock() {
                active.remove(spec.id);
            }
            match result {
                Ok(path) => {
                    tracing::info!(model = spec.id, path = %path.display(), "model installed");
                    // The first load on this build compiles shaders for
                    // seconds. Under the card's "preparing" line now, not
                    // on the first keypress.
                    let _ = app.emit(
                        "model-progress",
                        ProgressEvent {
                            id: spec.id.to_string(),
                            bytes: spec.bytes,
                            total: spec.bytes,
                            phase: "preparing".into(),
                        },
                    );
                    // Skipped once the app is quitting: opening a model this
                    // late would only be one more thing the quit sequence
                    // has to wait on, or race, before the process exits.
                    // Still counted in, on a guard, while it runs, so a
                    // quit that starts mid-warm-up waits for it.
                    if let Some(_guard) = downloads.enter_warm_up() {
                        if let Err(e) = models::warm(spec) {
                            tracing::warn!(model = spec.id, error = %e, "could not warm the model");
                        }
                    }
                    let _ = app.emit("model-ready", spec.id);
                    dictator.send(Command::ReopenEngine);
                }
                Err(e) => {
                    tracing::warn!(model = spec.id, error = %e, "model download did not finish");
                    let _ = app.emit(
                        "model-error",
                        ModelError {
                            id: spec.id.to_string(),
                            message: download_outcome_message(cancel.load(Ordering::Relaxed))
                                .to_string(),
                        },
                    );
                }
            }
        })
        .or_say("Could not start the download. Try again.")?;
    Ok(())
}

/// Stop a download. The partial file stays, and the next download resumes it.
#[tauri::command]
pub fn model_cancel(downloads: State<'_, Downloads>, id: String) {
    if let Ok(active) = downloads.active.lock() {
        if let Some(flag) = active.get(&id) {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Where the model files are kept, for the card and for a person who wants
/// to clean up by hand.
#[tauri::command]
pub fn models_dir() -> String {
    Store::shared().dir().display().to_string()
}

/// Delete a model, unless dictation runs on it: removing that would leave the
/// next dictation with no engine, so the person chooses another first.
#[tauri::command]
pub fn model_remove(
    history: State<'_, crate::history::HistoryHandle>,
    id: String,
) -> Result<(), CommandError> {
    let spec = models::find(&id)
        .ok_or_else(|| CommandError::from("That is not a model Macalla knows about."))?;
    if let Some(refusal) = in_use(spec, history.engine_choice().as_deref()) {
        return Err(refusal.into());
    }
    // Not `or_say`: the log line keeps which model it was.
    Store::shared().remove(spec).map_err(|error| {
        tracing::warn!(model = spec.id, %error, "could not remove a model file");
        CommandError::from("Could not remove the model file. Try again on the Models page.")
    })
}

/// Why a model cannot be removed now, or `None` when it can. With nothing
/// chosen, the platform's default is the engine in use.
fn in_use(spec: &models::ModelSpec, chosen: Option<&str>) -> Option<String> {
    let in_use = chosen.unwrap_or(macalla_engines::default_engine_id());
    (spec.engine == in_use).then(|| {
        format!(
            "{} is in use. Choose another engine first, then remove it.",
            spec.engine_name
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_engines::models::{PARAKEET_TDT_V3, WHISPER_SMALL_EN};

    #[test]
    fn a_cancelled_download_says_it_will_resume_and_a_real_failure_says_try_again() {
        assert_eq!(
            download_outcome_message(true),
            "Download stopped. It will pick up where it left off next time."
        );
        assert_eq!(
            download_outcome_message(false),
            "That model could not be downloaded. Try again on the Models page."
        );
    }

    #[test]
    fn the_model_dictation_runs_on_cannot_be_removed_from_under_it() {
        assert!(in_use(&PARAKEET_TDT_V3, Some(PARAKEET_TDT_V3.engine)).is_some());
        assert!(in_use(&PARAKEET_TDT_V3, Some(WHISPER_SMALL_EN.engine)).is_none());
        // Nothing chosen: whatever the platform defaults to is in use.
        let default_is_parakeet = macalla_engines::default_engine_id() == PARAKEET_TDT_V3.engine;
        assert_eq!(
            in_use(&PARAKEET_TDT_V3, None).is_some(),
            default_is_parakeet
        );
        assert!(in_use(&WHISPER_SMALL_EN, None).is_none());
    }

    #[test]
    fn quitting_waits_for_a_warm_up_in_progress() {
        let downloads = Downloads::default();
        let guard = downloads.enter_warm_up().expect("nothing is quitting yet");
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                // Stored before the guard drops: `wait_until` waking because
                // the drop counted the warm-up out must always see this
                // already true, never race it.
                done.store(true, Ordering::Relaxed);
                drop(guard);
            })
        };

        downloads.begin_quitting();
        let started = Instant::now();
        let finished = downloads.wait_until(started + std::time::Duration::from_secs(1));
        let elapsed = started.elapsed();

        assert!(finished);
        assert!(done.load(Ordering::Relaxed));
        // A `wait_until` that (wrongly) returned immediately could still
        // find `done` already true, and this test would pass for the wrong
        // reason, if the assertions above it happened to run more than
        // 20 ms after the thread was spawned. This is the check that
        // `wait_until` actually waited for the warm-up rather than racing
        // it: 15 ms gives margin under the real 20 ms sleep without making
        // the test flake on its own.
        assert!(
            elapsed >= std::time::Duration::from_millis(15),
            "wait_until returned too fast to have actually waited: {elapsed:?}"
        );
        thread.join().unwrap();
    }

    #[test]
    fn a_warm_up_starting_after_quitting_is_skipped() {
        let downloads = Downloads::default();

        downloads.begin_quitting();
        assert!(downloads.wait_until(Instant::now() + std::time::Duration::from_secs(1)));
        assert!(downloads.enter_warm_up().is_none());
    }

    #[test]
    fn quitting_gives_up_at_the_deadline_on_a_warm_up_that_never_ends() {
        let downloads = Downloads::default();
        let guard = downloads.enter_warm_up().expect("nothing is quitting yet");

        downloads.begin_quitting();
        let finished = downloads.wait_until(Instant::now() + std::time::Duration::from_millis(50));

        assert!(!finished);
        drop(guard); // let the gate go so nothing else hangs
    }
}
