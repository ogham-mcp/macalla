// SPDX-License-Identifier: MIT
//! Locking an engine's native session so that one panic does not break the
//! engine for good.
//!
//! A panic while a `std::sync::Mutex` is held poisons it, and every later
//! `lock()` then fails. For a transcribe.cpp session that is shared by
//! dictation and a live meeting, that turned one bad piece into an engine
//! that refused everything until the app restarted. The lock is recovered
//! instead, and the recovery is said at warn.

use std::sync::{Mutex, MutexGuard};

/// Lock `session`, recovering it if an earlier holder panicked.
///
/// Why recovering is safe for a transcribe.cpp session, which is the first
/// question `into_inner` on an FFI session raises:
///
/// - The only Rust code transcribe.cpp calls back into during a run is
///   `transcribe-cpp`'s log and abort trampolines, both `extern "C" fn`. On
///   this crate's minimum Rust (1.88) a panic cannot unwind out of an
///   `extern "C" fn`: the process aborts instead. So no panic ever unwinds
///   through the C++ frames, and a poisoned lock never sits over a
///   half-finished native run.
/// - A panic under the lock can therefore only come from Rust before the
///   native call, when the session has not been touched, or after it
///   returned, when the run is complete and its results are only being read.
/// - Each run stands alone: transcribe.cpp's header says "Calling
///   transcribe_run() again replaces the previous result on the same
///   session" (transcribe-cpp-sys 0.2.3, include/transcribe.h).
///
/// transcribe-cpp recovers its own per-model compute lock the same way.
#[cfg_attr(not(feature = "engine-transcribe"), allow(dead_code))]
pub(crate) fn lock_recovering<'a, T>(
    session: &'a Mutex<T>,
    engine: &'static str,
) -> MutexGuard<'a, T> {
    session.lock().unwrap_or_else(|poisoned| {
        tracing::warn!(
            engine,
            "an earlier transcription panicked while it held the engine; carrying on with it"
        );
        let guard = poisoned.into_inner();
        // Said once, not on every run after: the flag outlives `into_inner`.
        session.clear_poison();
        guard
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Stands in for a native session: counts the runs it has served.
    #[derive(Default)]
    struct FakeSession {
        runs: u32,
    }

    impl FakeSession {
        fn run(&mut self, panics: bool) -> u32 {
            self.runs += 1;
            if panics {
                panic!("a transcription fell over mid-run");
            }
            self.runs
        }
    }

    #[test]
    fn a_run_that_panics_does_not_stop_the_next_one() {
        let session = Arc::new(Mutex::new(FakeSession::default()));

        let shared = Arc::clone(&session);
        let panicked = std::thread::spawn(move || {
            lock_recovering(&shared, "fake").run(true);
        })
        .join();
        assert!(panicked.is_err(), "the first run panicked");
        assert!(session.is_poisoned(), "and left the lock poisoned");

        // The next dictation on the same engine.
        assert_eq!(lock_recovering(&session, "fake").run(false), 2);
        assert!(
            !session.is_poisoned(),
            "recovered once, so later runs are not warned about it again"
        );
        assert_eq!(lock_recovering(&session, "fake").run(false), 3);
    }
}
