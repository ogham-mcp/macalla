// SPDX-License-Identifier: MIT
//! Ctrl-C and SIGTERM during `macalla live-meeting`: the first Ctrl-C, or a
//! SIGTERM, stops the meeting and keeps it, as Enter does; a second Ctrl-C
//! throws it away. So an agent, or a process manager, that stops the
//! command with SIGINT or SIGTERM gets what was heard.
//!
//! The handlers only record what arrived, in lock-free atomics: a load and
//! a store, nothing else, which is async-signal-safe. The meeting reads
//! them from an ordinary thread ([`asked`]) and acts there. SIGTERM never
//! discards, however often it comes: a process manager may send it more
//! than once, and a meeting it stops should still be kept.
//!
//! The second Ctrl-C also hands SIGINT back to the system, so a third ends
//! the process at once: a meeting whose last piece is slow to finish can
//! always be killed.
//!
//! `macalla watch` installs the same handlers and treats any signal as a
//! stop: a file being heard is dropped, and the transcripts already written
//! stay.
//!
//! Unix only. Elsewhere nothing is installed and Ctrl-C ends the process
//! as it always has, keeping nothing.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// How many times Ctrl-C (SIGINT) has arrived, up to 255.
static INTERRUPTS: AtomicU8 = AtomicU8::new(0);

/// Whether SIGTERM has arrived.
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// What the signals that have arrived ask of the meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Asked {
    Nothing,
    /// Stop and keep what was heard.
    Stop,
    /// Throw the meeting away.
    Discard,
}

/// The one rule: a second Ctrl-C discards; the first, or any SIGTERM,
/// stops and keeps.
pub fn decide(interrupts: u8, terminated: bool) -> Asked {
    match (interrupts, terminated) {
        (2.., _) => Asked::Discard,
        (1, _) | (_, true) => Asked::Stop,
        (0, false) => Asked::Nothing,
    }
}

/// What the signals caught since [`install`] ask for.
pub fn asked() -> Asked {
    decide(
        INTERRUPTS.load(Ordering::Relaxed),
        TERMINATED.load(Ordering::Relaxed),
    )
}

#[cfg(unix)]
extern "C" fn on_interrupt(_signal: libc::c_int) {
    // A load and a store, not an atomic add: both are plainly
    // async-signal-safe on a lock-free atomic. Two Ctrl-Cs landing inside
    // one another's handler could count as one; a person presses again.
    let seen = INTERRUPTS.load(Ordering::Relaxed).saturating_add(1);
    INTERRUPTS.store(seen, Ordering::Relaxed);
    if hands_back(seen) {
        // SAFETY: `signal` is async-signal-safe (POSIX lists it), and
        // SIG_DFL is the system's own handler.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
/// Whether the Ctrl-C numbered `seen` hands SIGINT back to the system, so
/// the next one ends the process: from the second on, the one that
/// discards.
fn hands_back(seen: u8) -> bool {
    decide(seen, false) == Asked::Discard
}

#[cfg(unix)]
extern "C" fn on_terminate(_signal: libc::c_int) {
    TERMINATED.store(true, Ordering::Relaxed);
}

/// Catch Ctrl-C and SIGTERM for the rest of the process, instead of letting
/// them end it. Called once, as the meeting starts recording.
#[cfg(unix)]
pub fn install() {
    let interrupt: extern "C" fn(libc::c_int) = on_interrupt;
    let terminate: extern "C" fn(libc::c_int) = on_terminate;
    // SAFETY: both handlers do nothing but touch lock-free atomics, which is
    // async-signal-safe, and `signal` is given a valid function pointer of
    // the type it expects.
    unsafe {
        libc::signal(libc::SIGINT, interrupt as libc::sighandler_t);
        libc::signal(libc::SIGTERM, terminate as libc::sighandler_t);
    }
}

/// Nothing to install here: Ctrl-C ends the process, keeping nothing.
#[cfg(not(unix))]
pub fn install() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_ctrl_c_stops_and_keeps_and_a_second_discards() {
        assert_eq!(decide(0, false), Asked::Nothing);
        assert_eq!(decide(1, false), Asked::Stop);
        assert_eq!(decide(2, false), Asked::Discard);
        assert_eq!(decide(u8::MAX, false), Asked::Discard);
    }

    #[test]
    fn the_ctrl_c_that_discards_hands_sigint_back_so_the_next_ends_the_process() {
        assert!(!hands_back(1), "the first only stops");
        assert!(hands_back(2));
        assert!(hands_back(3));
    }

    #[test]
    fn sigterm_stops_and_keeps_and_never_discards_on_its_own() {
        assert_eq!(decide(0, true), Asked::Stop);
        assert_eq!(decide(1, true), Asked::Stop);
        assert_eq!(
            decide(2, true),
            Asked::Discard,
            "a second Ctrl-C still discards"
        );
    }
}
