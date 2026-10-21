// SPDX-License-Identifier: MIT
//! The global shortcut listener on Windows: the keyboard hook (`hook.rs`)
//! feeding the recogniser.
//!
//! Deliberately the same shape and the same public methods as the macOS
//! listener in `hotkey.rs`, written out again rather than shared with it, so
//! a change here cannot alter what macOS does. Re-exported from
//! `crate::hotkey`, which is where callers find it on either platform.

use crate::hotkey::{Action, Mode, Recogniser, Shortcut};
use crate::vkey::{apply, HookEvent};
use std::sync::{mpsc, Arc, Mutex};

/// A running global listener. Dropping this leaks the listener thread, which
/// is deliberate: the hook's message loop is not stopped from outside, so the
/// thread lives for the life of the process. One per process.
pub struct Listener {
    events: mpsc::Receiver<crate::Result<Action>>,
    recogniser: Arc<Mutex<Recogniser>>,
    held: HeldWatch,
    actions: mpsc::Sender<crate::Result<Action>>,
}

/// What [`Listener::on_held`] set, called from the hook's thread.
type HeldWatch = Arc<Mutex<Option<Box<dyn Fn(bool) + Send>>>>;

/// A way to tell the listener whether a dictation is running, whatever
/// started it -- the shortcut, the tray, an agent -- for its whole length,
/// including a wait on a cloud engine after the key is up, so that Esc twice
/// discards it. Cheap to clone; the dictation thread holds one.
#[derive(Clone)]
pub struct DictationState(Arc<Mutex<Recogniser>>);

impl DictationState {
    /// See [`Recogniser::set_dictating`], including what it returns.
    pub fn set_dictating(&self, dictating: bool) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .set_dictating(dictating)
    }

    /// See [`Recogniser::is_held`].
    pub fn is_held(&self) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).is_held()
    }
}

/// What a running [`Listener`] lets the app change. Cheap to clone. The same
/// shape as the macOS `Controls` in `hotkey.rs`, written out again on purpose.
#[derive(Clone)]
pub struct Controls {
    recogniser: Arc<Mutex<Recogniser>>,
    /// Told when the trigger is pressed or let go, as the hook tells it.
    held: HeldWatch,
    /// To send the `Stop` a swap or a pause can owe a running dictation.
    actions: mpsc::Sender<crate::Result<Action>>,
}

impl Controls {
    /// Change the shortcut while running. The hook cannot be restarted (one
    /// message loop per process), so this swaps the shortcut inside the shared
    /// recogniser and forgets its key state.
    pub fn set_shortcut(&self, shortcut: Shortcut) {
        let stopped = self.lock().set_shortcut(shortcut);
        self.send(stopped);
    }

    /// While `paused`, key events produce no action. The Shortcut card pauses
    /// the listener while it records a new shortcut.
    pub fn pause(&self, paused: bool) {
        let stopped = self.lock().set_paused(paused);
        self.send(stopped);
    }

    /// A key event the hook did not see, delivered exactly as the hook
    /// delivers its own.
    ///
    /// While one of the app's own WebView2 windows has focus, Windows calls no
    /// low-level keyboard hook in the app's process at all, for any key,
    /// typed or injected (seen on Macalla, and reported against wry:
    /// https://github.com/tauri-apps/wry/issues/1761). The key still reaches
    /// the focused page, so the page passes it here. Each event comes from one
    /// path or the other, never both.
    pub fn deliver(&self, event: HookEvent) {
        // The window cannot tell Macalla's own paste from a person's keys,
        // so a key going down around one is dropped (`own_keys`). A key
        // going up is always let through: dropping it could leave the
        // trigger held, and letting go of a key that is not down does nothing.
        if matches!(event, HookEvent::Down { .. }) && crate::paste::own_keys::in_flight() {
            return;
        }
        deliver(&self.recogniser, &self.held, &self.actions, event);
    }

    /// The page that was passing its keys to [`Controls::deliver`] lost
    /// focus with `code` still down by its count. Let go of it only if the key
    /// really is up: a window can lose focus for a moment (as a dictation
    /// starts, say) while the key is still held, and a key still down will
    /// be let go where focus went -- the hook in another app, the page if it
    /// comes back.
    pub fn focus_lost_with(&self, code: u32) {
        if !crate::hook::physically_down(code) {
            self.deliver(HookEvent::Up(code));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Recogniser> {
        self.recogniser.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn send(&self, action: Option<Action>) {
        if let Some(action) = action {
            let _ = self.actions.send(Ok(action));
        }
    }
}

/// One key event, from the hook or from [`Controls::deliver`], applied to the
/// recogniser: whether the trigger is held is told first, with the
/// recogniser let go, then the action goes out, so a held press is heard
/// before what it starts.
fn deliver(
    recogniser: &Mutex<Recogniser>,
    watch: &HeldWatch,
    actions: &mpsc::Sender<crate::Result<Action>>,
    event: HookEvent,
) {
    let (action, held) = {
        let mut recogniser = recogniser.lock().unwrap_or_else(|p| p.into_inner());
        apply(&mut recogniser, event)
    };
    if let Some(held) = held {
        if let Some(watch) = watch.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            watch(held);
        }
    }
    if let Some(action) = action {
        // A closed channel means the app is going away.
        let _ = actions.send(Ok(action));
    }
}

impl Listener {
    /// Start watching for `shortcut`.
    ///
    /// Windows asks for no permission. If the hook cannot be installed the
    /// listener reports the failure through the channel rather than silently
    /// never firing.
    pub fn start(shortcut: Shortcut, mode: Mode) -> Self {
        let (tx, events) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Recogniser::new(shortcut, mode)));
        let recogniser = shared.clone();
        let held = HeldWatch::default();
        let watch = held.clone();
        let actions = tx.clone();
        std::thread::Builder::new()
            .name("macalla-hotkey".into())
            .spawn(move || {
                let failures = tx.clone();
                // The hook's message loop blocks this thread for as long as
                // the process runs. Everything in this closure runs inside
                // the hook callback, so it stays short: Windows drops a hook
                // whose callback is slow.
                let result = crate::hook::listen(move |event| {
                    deliver(&recogniser, &watch, &tx, event);
                });
                if let Err(e) = result {
                    let _ = failures.send(Err(crate::Error::Listener(e)));
                }
            })
            .expect("spawn the hotkey thread");
        Self {
            events,
            recogniser: shared,
            held,
            actions,
        }
    }

    /// Call `watch` from the listener's thread whenever
    /// [`Recogniser::is_held`] changes, key-ups that start or stop nothing
    /// included. Replaces any earlier watch.
    pub fn on_held(&self, watch: impl Fn(bool) + Send + 'static) {
        *self.held.lock().unwrap_or_else(|p| p.into_inner()) = Some(Box::new(watch));
    }

    /// A handle for telling the listener about dictations it did not start.
    pub fn dictation_state(&self) -> DictationState {
        DictationState(self.recogniser.clone())
    }

    /// Change the shortcut while running. See [`Controls::set_shortcut`].
    pub fn set_shortcut(&self, shortcut: Shortcut) {
        self.controls().set_shortcut(shortcut);
    }

    /// While `paused`, key events produce no action. See [`Controls::pause`].
    pub fn pause(&self, paused: bool) {
        self.controls().pause(paused);
    }

    /// A handle for [`Listener::set_shortcut`] and [`Listener::pause`] from
    /// elsewhere, once the listener itself has gone to the thread that waits
    /// on it.
    pub fn controls(&self) -> Controls {
        Controls {
            recogniser: self.recogniser.clone(),
            held: self.held.clone(),
            actions: self.actions.clone(),
        }
    }

    /// Wait for the next action. `None` once the listener has gone.
    pub fn recv(&self) -> Option<crate::Result<Action>> {
        self.events.recv().ok()
    }

    /// The next action if one is waiting.
    pub fn try_recv(&self) -> Option<crate::Result<Action>> {
        self.events.try_recv().ok()
    }
}
