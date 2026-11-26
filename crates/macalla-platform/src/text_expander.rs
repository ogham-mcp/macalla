// SPDX-License-Identifier: MIT
//! Pausing Espanso around Macalla's own paste.
//!
//! With Espanso 2.4.1 running, a dictation pasted with one Cmd+V could arrive
//! twice: once short of its last characters, then in full, typed. With
//! Espanso switched off it never did, and with it paused for the paste it
//! never did either. So while Macalla pastes, Espanso is told to stand aside,
//! and once the pastes stop it is told to carry on.
//!
//! Only ever through Espanso's own command line, `espanso cmd disable` and
//! `espanso cmd enable`, about 15 ms each. Macalla never starts Espanso,
//! installs it or registers it to run at login: if it is not installed,
//! nothing is run, and if it is not running, `disable` fails and nothing is
//! resumed.
//!
//! Espanso has no command that says whether it is enabled, so someone who
//! had switched it off finds it switched back on after a dictation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

/// How long after the last paste Espanso stays paused. Long enough to cover
/// the clipboard restore and an app reading the pasteboard late, and a quick
/// second dictation keeps it paused rather than racing a resume.
const RESUME_AFTER: Duration = Duration::from_secs(1);

/// What the pause asks of the expander. The real one is Espanso's command
/// line; the tests count calls.
trait Expander {
    /// Ask it to stop expanding. `true` when it was running and now is not.
    fn disable(&self) -> bool;
    /// Ask it to expand again. `true` when it says it did.
    fn enable(&self) -> bool;
}

/// Whether the expander is paused by us, and which paste the pending resume
/// belongs to.
struct Pause {
    paused: bool,
    /// Bumped by every hold, so a resume scheduled for an earlier paste
    /// finds it changed and leaves the pause to the later one.
    generation: u64,
}

/// A resume that may only run if no paste has started since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ticket(u64);

impl Pause {
    const fn new() -> Self {
        Self {
            paused: false,
            generation: 0,
        }
    }

    /// Before a paste: pause, unless already paused.
    fn hold(&mut self, expander: &impl Expander) {
        self.generation = self.generation.wrapping_add(1);
        if !self.paused && expander.disable() {
            self.paused = true;
            tracing::debug!("text expander paused for the paste");
        }
    }

    /// After a paste: what a later resume needs, or `None` when nothing was
    /// paused.
    fn release(&self) -> Option<Ticket> {
        self.paused.then_some(Ticket(self.generation))
    }

    /// Resume, if `ticket` is still the latest paste, or unconditionally
    /// with `None`.
    fn resume(&mut self, ticket: Option<Ticket>, expander: &impl Expander) {
        if !self.paused || ticket.is_some_and(|t| t.0 != self.generation) {
            return;
        }
        // Once more after a short wait: a resume that never lands leaves the
        // person's snippets off until something else switches them on.
        let resumed = expander.enable() || {
            std::thread::sleep(Duration::from_millis(200));
            expander.enable()
        };
        if resumed {
            self.paused = false;
            tracing::debug!("text expander resumed");
        } else {
            tracing::warn!("Espanso did not resume after the paste; run `espanso cmd enable`");
        }
    }
}

/// Espanso's command line, wherever it is installed.
struct Espanso(PathBuf);

impl Espanso {
    /// The installed binary, or `None` when there is no Espanso.
    fn find() -> Option<Self> {
        let app = Path::new("Espanso.app/Contents/MacOS/espanso");
        let mut places = vec![Path::new("/Applications").join(app)];
        if let Some(home) = std::env::var_os("HOME") {
            places.push(Path::new(&home).join("Applications").join(app));
        }
        places.into_iter().find(|p| p.is_file()).map(Self)
    }

    fn cmd(&self, verb: &str) -> bool {
        Command::new(&self.0)
            .args(["cmd", verb])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

impl Expander for Espanso {
    fn disable(&self) -> bool {
        self.cmd("disable")
    }
    fn enable(&self) -> bool {
        self.cmd("enable")
    }
}

static PAUSE: Mutex<Pause> = Mutex::new(Pause::new());

fn pause() -> std::sync::MutexGuard<'static, Pause> {
    PAUSE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Before Macalla's own keystrokes: pause Espanso if it is running.
pub(crate) fn hold() {
    if let Some(espanso) = Espanso::find() {
        pause().hold(&espanso);
    }
}

/// After them: resume Espanso [`RESUME_AFTER`] from now, unless another
/// paste has started by then.
pub(crate) fn release() {
    let Some(ticket) = pause().release() else {
        return;
    };
    std::thread::spawn(move || {
        std::thread::sleep(RESUME_AFTER);
        if let Some(espanso) = Espanso::find() {
            pause().resume(Some(ticket), &espanso);
        }
    });
}

/// Resume Espanso now if Macalla paused it. For quitting: a resume still
/// waiting on its timer dies with the process.
pub(crate) fn resume_now() {
    if let Some(espanso) = Espanso::find() {
        pause().resume(None, &espanso);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct Fake {
        running: bool,
        enable_fails: Cell<u32>,
        disables: Cell<u32>,
        enables: Cell<u32>,
    }

    impl Fake {
        fn running() -> Self {
            Self {
                running: true,
                ..Self::default()
            }
        }
    }

    impl Expander for Fake {
        fn disable(&self) -> bool {
            self.disables.set(self.disables.get() + 1);
            self.running
        }
        fn enable(&self) -> bool {
            self.enables.set(self.enables.get() + 1);
            if self.enable_fails.get() > 0 {
                self.enable_fails.set(self.enable_fails.get() - 1);
                return false;
            }
            true
        }
    }

    #[test]
    fn one_paste_pauses_then_resumes() {
        let fake = Fake::running();
        let mut p = Pause::new();
        p.hold(&fake);
        let ticket = p.release();
        assert!(ticket.is_some());
        p.resume(ticket, &fake);
        assert_eq!((fake.disables.get(), fake.enables.get()), (1, 1));
        assert!(!p.paused);
    }

    #[test]
    fn not_running_means_nothing_to_resume() {
        let fake = Fake::default();
        let mut p = Pause::new();
        p.hold(&fake);
        assert_eq!(p.release(), None);
        p.resume(None, &fake);
        assert_eq!(fake.enables.get(), 0);
    }

    #[test]
    fn a_second_paste_keeps_the_first_resume_from_landing() {
        let fake = Fake::running();
        let mut p = Pause::new();
        p.hold(&fake);
        let first = p.release();
        p.hold(&fake);
        // The first paste's timer fires mid second paste: it must not resume.
        p.resume(first, &fake);
        assert_eq!(fake.enables.get(), 0);
        assert!(p.paused);
        // Still paused, so the second hold asked nothing of Espanso.
        assert_eq!(fake.disables.get(), 1);
        let second = p.release();
        p.resume(second, &fake);
        assert_eq!(fake.enables.get(), 1);
        assert!(!p.paused);
    }

    #[test]
    fn quitting_resumes_whatever_the_timer() {
        let fake = Fake::running();
        let mut p = Pause::new();
        p.hold(&fake);
        p.resume(None, &fake);
        assert!(!p.paused);
        assert_eq!(fake.enables.get(), 1);
    }

    #[test]
    fn a_failed_resume_is_tried_once_more() {
        let fake = Fake::running();
        fake.enable_fails.set(1);
        let mut p = Pause::new();
        p.hold(&fake);
        p.resume(p.release(), &fake);
        assert_eq!(fake.enables.get(), 2);
        assert!(!p.paused);
    }

    #[test]
    fn a_resume_that_never_lands_stays_paused_for_the_next_try() {
        let fake = Fake::running();
        fake.enable_fails.set(2);
        let mut p = Pause::new();
        p.hold(&fake);
        p.resume(p.release(), &fake);
        assert!(p.paused);
        // The next paste does not disable again, and its release resumes.
        p.hold(&fake);
        assert_eq!(fake.disables.get(), 1);
        p.resume(p.release(), &fake);
        assert!(!p.paused);
    }
}
