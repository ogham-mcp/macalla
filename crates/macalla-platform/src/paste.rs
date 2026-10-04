// SPDX-License-Identifier: MIT
//! Putting the text where the user was typing.
//!
//! The rule: **save and restore the clipboard
//! around paste, and wait before restoring.** Chromium-based apps read the
//! pasteboard asynchronously and more than once, so an immediate restore is a
//! race the user loses -- they get their old clipboard pasted, or nothing.
//!
//! The wait is the reason this module exists, so the ordering is behind traits
//! and tested rather than left to a comment. The real clipboard and the real
//! key events are the only parts that need a desktop.
//!
//! Paste rather than typing because it is instant and does not depend on the
//! target app's key handling. Typing is the fallback for the apps that refuse a
//! paste, and it is slow enough that it must stay a fallback.

use std::time::Duration;

/// How long to leave the text on the clipboard before putting the user's own
/// content back. Chromium-based apps read the pasteboard asynchronously and
/// more than once, so a single frame is not enough.
pub const DEFAULT_RESTORE_WAIT: Duration = Duration::from_millis(150);

/// The system clipboard.
pub trait Clipboard {
    /// The current clipboard text, or `None` if it holds something else --
    /// an image, a file, nothing at all.
    fn read_text(&mut self) -> Option<String>;
    fn write_text(&mut self, text: &str) -> crate::Result<()>;
}

/// Synthetic key events.
pub trait Keystrokes {
    /// The platform's paste chord: Cmd+V on macOS, Ctrl+V elsewhere.
    fn paste_chord(&mut self) -> crate::Result<()>;
    /// Type the text character by character. Slow, and the fallback.
    fn type_text(&mut self, text: &str) -> crate::Result<()>;
}

/// Waiting. Injected so the ordering can be tested without really sleeping.
pub trait Clock {
    fn sleep(&mut self, d: Duration);
}

/// The real one.
pub struct RealClock;

impl Clock for RealClock {
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Why a synthetic paste could not reach the app in front. A typed reason,
/// not a sentence: each app words it in its own catalogue (user-facing
/// wording lives in one place per app).
///
/// One variant, and an enum rather than a bool because the shape is right and
/// a second reason may yet be found. Secure Event Input is NOT one: measured
/// on macOS 27 on 2026-09-18, pastes posted while secure input read on
/// arrived intact in a terminal that had *Secure Keyboard Entry* switched on.
/// Secure input hides ordinary keys FROM event taps, which is why the
/// shortcut is a bare modifier; it does not stop this process
/// posting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotDelivered {
    /// This process is not trusted for Accessibility, so the OS drops the
    /// events it posts -- the chord and the typing fallback alike. "Not
    /// trusted" covers denied and never-asked alike: the API cannot tell them
    /// apart, and for a paste they are one answer. A person can fix it.
    NotTrustedForAccessibility,
}

/// What is asked before a paste is posted, behind a trait for the same reason
/// the clipboard and the keys are: so the ordering is tested headless and no
/// test reads this machine's real permission state.
pub trait DeliveryGates {
    /// Whether this process may post synthetic key events at all.
    fn accessibility_trusted(&mut self) -> bool;
}

/// The real one, asked of the OS. Off macOS the question does not exist and
/// the answer is "not blocking", so Linux and Windows paste as they always
/// did.
pub struct SystemGates;

impl DeliveryGates for SystemGates {
    fn accessibility_trusted(&mut self) -> bool {
        // Reports, never prompts: on macOS this is `tcc::accessibility`, and
        // off it `NotRequired`. Anything that is not an outright grant is
        // treated as not trusted, because the API cannot distinguish denied
        // from never-asked.
        trusts(crate::Permission::Accessibility.granted())
    }
}

/// Whether an Accessibility answer lets keystrokes through. `NotRequired`
/// does: it is what every platform but macOS reports, and reading it as
/// untrusted is how paste once failed on Linux and Windows with a
/// permission error for a permission those platforms do not have. Both the
/// gate and [`SystemKeys::new`] ask through [`SystemGates`], so this is the
/// one place the rule lives.
fn trusts(grant: crate::Grant) -> bool {
    matches!(grant, crate::Grant::Granted | crate::Grant::NotRequired)
}

/// What actually happened, for the log and the latency breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// Pasted, and the user's clipboard was put back.
    PastedAndRestored,
    /// Pasted, but the previous clipboard could not be read, so the dictated
    /// text was left on it.
    PastedAndKept,
    /// The paste chord failed and the text was typed instead.
    Typed,
    /// There was nothing to insert.
    Nothing,
    /// Nothing was posted, because nothing could have arrived. The clipboard
    /// was not touched and no key event was sent. The text is still the
    /// caller's to keep -- History gets it as usual.
    NotDelivered(NotDelivered),
}

/// Inserts text by clipboard paste, restoring what was there before.
pub struct Inserter<C, K, T, G = SystemGates> {
    clipboard: C,
    keys: K,
    clock: T,
    gates: G,
    restore_wait: Duration,
}

impl<C: Clipboard, K: Keystrokes, T: Clock> Inserter<C, K, T, SystemGates> {
    pub fn new(clipboard: C, keys: K, clock: T) -> Self {
        Self {
            clipboard,
            keys,
            clock,
            gates: SystemGates,
            restore_wait: DEFAULT_RESTORE_WAIT,
        }
    }
}

impl<C: Clipboard, K: Keystrokes, T: Clock, G: DeliveryGates> Inserter<C, K, T, G> {
    /// Override the wait before the clipboard is restored.
    pub fn with_restore_wait(mut self, wait: Duration) -> Self {
        self.restore_wait = wait;
        self
    }

    /// Ask something other than the OS whether a paste can be delivered.
    /// Tests only; the app keeps [`SystemGates`].
    pub fn with_gates<G2: DeliveryGates>(self, gates: G2) -> Inserter<C, K, T, G2> {
        Inserter {
            clipboard: self.clipboard,
            keys: self.keys,
            clock: self.clock,
            gates,
            restore_wait: self.restore_wait,
        }
    }

    /// Why a paste sent right now could not arrive, or `None` to go ahead.
    fn blocked(&mut self) -> Option<NotDelivered> {
        (!self.gates.accessibility_trusted()).then_some(NotDelivered::NotTrustedForAccessibility)
    }

    /// Put `text` where the user was typing.
    pub fn insert(&mut self, text: &str) -> crate::Result<Inserted> {
        if text.trim().is_empty() {
            return Ok(Inserted::Nothing);
        }

        // Before the clipboard, not after: without the grant the chord is
        // dropped and the typing fallback with it, so a save-and-restore here
        // would cost the user their clipboard for the restore wait and
        // deliver nothing. Untouched is the only honest outcome.
        if let Some(reason) = self.blocked() {
            return Ok(Inserted::NotDelivered(reason));
        }

        // Read first. Whatever happens next, this is the only chance to learn
        // what the user had.
        let previous = self.clipboard.read_text();

        if self.clipboard.write_text(text).is_err() {
            // No clipboard, no paste. Typing is slower but it is not nothing,
            // and nothing was disturbed, so there is nothing to restore.
            self.keys.type_text(text)?;
            return Ok(Inserted::Typed);
        }

        let pasted = self.keys.paste_chord().is_ok();
        let outcome = if pasted {
            Inserted::PastedAndRestored
        } else {
            self.keys.type_text(text)?;
            Inserted::Typed
        };

        let Some(previous) = previous else {
            // Nothing to put back: the clipboard held an image, a file, or was
            // empty. Writing "" over it would destroy it, so the dictated text
            // stays -- the user just said it.
            return Ok(if pasted {
                Inserted::PastedAndKept
            } else {
                outcome
            });
        };

        // The wait. Chromium-based apps read the pasteboard asynchronously and
        // more than once, so restoring straight after the chord is a race the
        // user loses. It applies to the failed-chord path too: the paste may
        // still be in flight even though the call reported an error.
        self.clock.sleep(self.restore_wait);
        self.clipboard.write_text(&previous)?;
        Ok(outcome)
    }
}

/// The OS clipboard, via `arboard`.
///
/// Held open for the life of the app rather than opened per paste: on X11 and
/// Wayland the clipboard contents belong to the process that owns them, and
/// dropping the handle hands them back.
pub struct SystemClipboard {
    inner: arboard::Clipboard,
}

impl SystemClipboard {
    pub fn new() -> crate::Result<Self> {
        arboard::Clipboard::new()
            .map(|inner| Self { inner })
            .map_err(|e| crate::Error::Clipboard(format!("opening it: {e}")))
    }
}

impl Clipboard for SystemClipboard {
    fn read_text(&mut self) -> Option<String> {
        self.inner.get_text().ok()
    }

    fn write_text(&mut self, text: &str) -> crate::Result<()> {
        self.inner
            .set_text(text)
            .map_err(|e| crate::Error::Clipboard(format!("writing: {e}")))
    }
}

/// Synthetic key events: CGEvents of our own on macOS (see `keys`), `enigo`
/// elsewhere.
///
/// CGEvent rather than System Events, so macOS asks for Accessibility but not
/// Automation. Accessibility cannot be granted programmatically and an unsigned
/// dev rebuild loses it, which is why released builds carry a stable Developer
/// ID.
pub struct SystemKeys {
    #[cfg(target_os = "macos")]
    inner: crate::keys::CgKeys,
    #[cfg(not(target_os = "macos"))]
    inner: enigo::Enigo,
}

impl SystemKeys {
    pub fn new() -> crate::Result<Self> {
        // Ask first. Without Accessibility, enigo fails with "the application
        // does not have the permission to simulate input" -- true, but typed as
        // a runtime failure it is indistinguishable from a broken device, and
        // onboarding cannot offer the one thing that would fix it.
        //
        // Not-trusted covers both denied and never-asked, and for this purpose
        // they are the same answer: no keystroke is going anywhere.
        //
        // Asked through [`SystemGates`] so this and the gate below agree on
        // what "trusted" means. They did not: off macOS the grant reports
        // `NotRequired`, which this read as untrusted, and the ask below
        // returns the same `NotRequired`, so every Linux and Windows paste
        // failed here with a permission error for a permission that platform
        // does not have. macOS is unaffected -- Accessibility never reports
        // `NotRequired` there.
        if !SystemGates.accessibility_trusted() {
            // ASK, do not merely check. Checking alone is how a first run
            // fails without ever showing the user the dialog that would fix
            // it -- which is exactly what the check added a moment ago did.
            // After a denial macOS shows nothing, so this is also a no-op in
            // the case where the error below is the only useful answer.
            if crate::Permission::Accessibility.request() != crate::Grant::Granted {
                return Err(crate::Error::Permission {
                    permission: crate::Permission::Accessibility,
                });
            }
        }
        #[cfg(target_os = "macos")]
        {
            crate::keys::CgKeys::new().map(|inner| Self { inner })
        }
        #[cfg(not(target_os = "macos"))]
        {
            enigo::Enigo::new(&enigo::Settings::default())
                .map(|inner| Self { inner })
                .map_err(|e| crate::Error::Input(format!("opening the device: {e}")))
        }
    }
}

#[cfg(target_os = "macos")]
impl Keystrokes for SystemKeys {
    fn paste_chord(&mut self) -> crate::Result<()> {
        self.inner.paste_chord()
    }

    fn type_text(&mut self, text: &str) -> crate::Result<()> {
        self.inner.type_text(text)
    }
}

#[cfg(not(target_os = "macos"))]
impl Keystrokes for SystemKeys {
    fn paste_chord(&mut self) -> crate::Result<()> {
        use enigo::{Direction, Key, Keyboard as _};
        let modifier = Key::Control;
        self.inner
            .key(modifier, Direction::Press)
            .map_err(|e| crate::Error::Input(format!("pressing the paste modifier: {e}")))?;
        let tapped = self
            .inner
            .key(Key::Unicode('v'), Direction::Click)
            .map_err(|e| crate::Error::Input(format!("tapping V: {e}")));
        // Release the modifier whatever happened to the tap. Leaving Cmd held
        // down turns the user's next keystroke into a shortcut.
        let released = self
            .inner
            .key(modifier, Direction::Release)
            .map_err(|e| crate::Error::Input(format!("releasing the paste modifier: {e}")));
        tapped.and(released)
    }

    fn type_text(&mut self, text: &str) -> crate::Result<()> {
        use enigo::Keyboard as _;
        self.inner
            .text(text)
            .map_err(|e| crate::Error::Input(format!("typing the text: {e}")))
    }
}

/// The inserter the app uses: real clipboard, real keys, real waiting.
pub fn system_inserter() -> crate::Result<Inserter<SystemClipboard, SystemKeys, RealClock>> {
    Ok(Inserter::new(
        SystemClipboard::new()?,
        SystemKeys::new()?,
        RealClock,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Everything the inserter did, in order. The order is the behaviour under
    /// test, not an implementation detail.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Step {
        Read,
        Write(String),
        Paste,
        Type(String),
        Slept(Duration),
    }

    type Log = Rc<RefCell<Vec<Step>>>;

    struct FakeClipboard {
        log: Log,
        holds: Option<String>,
        write_fails: bool,
    }

    impl Clipboard for FakeClipboard {
        fn read_text(&mut self) -> Option<String> {
            self.log.borrow_mut().push(Step::Read);
            self.holds.clone()
        }
        fn write_text(&mut self, text: &str) -> crate::Result<()> {
            self.log.borrow_mut().push(Step::Write(text.to_string()));
            if self.write_fails {
                return Err(crate::Error::Clipboard("busy".into()));
            }
            self.holds = Some(text.to_string());
            Ok(())
        }
    }

    struct FakeKeys {
        log: Log,
        paste_fails: bool,
    }

    impl Keystrokes for FakeKeys {
        fn paste_chord(&mut self) -> crate::Result<()> {
            self.log.borrow_mut().push(Step::Paste);
            if self.paste_fails {
                return Err(crate::Error::Permission {
                    permission: crate::Permission::Accessibility,
                });
            }
            Ok(())
        }
        fn type_text(&mut self, text: &str) -> crate::Result<()> {
            self.log.borrow_mut().push(Step::Type(text.to_string()));
            Ok(())
        }
    }

    struct FakeClock {
        log: Log,
    }

    impl Clock for FakeClock {
        fn sleep(&mut self, d: Duration) {
            self.log.borrow_mut().push(Step::Slept(d));
        }
    }

    /// The question answered without asking this Mac. No test in this file
    /// may read the real permission state: the answer would differ between
    /// CI, a signed build and a dev rebuild that has just lost its grant.
    struct FakeGates {
        trusted: bool,
    }

    impl FakeGates {
        /// Nothing in the way: what CI, Linux and Windows see. Off macOS
        /// Accessibility is `NotRequired`, which [`SystemGates`] turns into
        /// exactly this.
        fn clear() -> Self {
            Self { trusted: true }
        }
    }

    impl DeliveryGates for FakeGates {
        fn accessibility_trusted(&mut self) -> bool {
            self.trusted
        }
    }

    type Rig = Inserter<FakeClipboard, FakeKeys, FakeClock, FakeGates>;

    fn rig(holds: Option<&str>) -> (Log, Rig) {
        rig_with(holds, false, false)
    }

    fn rig_with(holds: Option<&str>, paste_fails: bool, write_fails: bool) -> (Log, Rig) {
        rig_gated(holds, paste_fails, write_fails, FakeGates::clear())
    }

    fn rig_gated(
        holds: Option<&str>,
        paste_fails: bool,
        write_fails: bool,
        gates: FakeGates,
    ) -> (Log, Rig) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let inserter = Inserter::new(
            FakeClipboard {
                log: Rc::clone(&log),
                holds: holds.map(str::to_string),
                write_fails,
            },
            FakeKeys {
                log: Rc::clone(&log),
                paste_fails,
            },
            FakeClock {
                log: Rc::clone(&log),
            },
        )
        .with_gates(gates);
        (log, inserter)
    }

    #[test]
    fn the_wait_happens_between_the_paste_and_the_restore() {
        // This is the whole rule. Chromium reads the pasteboard asynchronously
        // and more than once; restoring immediately after the chord is a race
        // the user loses.
        let (log, mut ins) = rig(Some("what the user had"));
        assert_eq!(
            ins.insert("dictated text").expect("insert"),
            Inserted::PastedAndRestored
        );
        assert_eq!(
            *log.borrow(),
            vec![
                Step::Read,
                Step::Write("dictated text".into()),
                Step::Paste,
                Step::Slept(DEFAULT_RESTORE_WAIT),
                Step::Write("what the user had".into()),
            ]
        );
    }

    #[test]
    fn the_wait_is_configurable() {
        let (log, ins) = rig(Some("x"));
        let mut ins = ins.with_restore_wait(Duration::from_millis(400));
        ins.insert("hello").expect("insert");
        assert!(log
            .borrow()
            .contains(&Step::Slept(Duration::from_millis(400))));
        assert!(
            !log.borrow().contains(&Step::Slept(DEFAULT_RESTORE_WAIT)),
            "the default must not also fire"
        );
    }

    #[test]
    fn an_unreadable_clipboard_is_left_holding_the_dictated_text() {
        // The clipboard held an image, or a file, or nothing. Writing an empty
        // string over it would destroy it, and there is nothing to put back.
        // Leaving the dictation there is the least surprising outcome: the user
        // just said it.
        let (log, mut ins) = rig(None);
        assert_eq!(
            ins.insert("dictated text").expect("insert"),
            Inserted::PastedAndKept
        );
        assert_eq!(
            *log.borrow(),
            vec![Step::Read, Step::Write("dictated text".into()), Step::Paste,],
            "no wait and no restore when there is nothing to restore"
        );
    }

    #[test]
    fn a_failed_paste_falls_back_to_typing() {
        let (log, mut ins) = rig_with(Some("old"), true, false);
        assert_eq!(ins.insert("hello").expect("insert"), Inserted::Typed);
        let steps = log.borrow().clone();
        assert!(steps.contains(&Step::Type("hello".into())));
        assert_eq!(
            steps.last(),
            Some(&Step::Write("old".into())),
            "the clipboard still gets put back after the fallback"
        );
    }

    #[test]
    fn a_failed_paste_still_waits_before_restoring() {
        // The chord reported an error, but a partial paste may still be in
        // flight. Restoring without the wait risks the same race.
        let (log, mut ins) = rig_with(Some("old"), true, false);
        ins.insert("hello").expect("insert");
        let steps = log.borrow().clone();
        let slept = steps.iter().position(|s| matches!(s, Step::Slept(_)));
        let restored = steps.iter().rposition(|s| *s == Step::Write("old".into()));
        assert!(
            slept < restored,
            "the wait must come before the restore, got {steps:?}"
        );
    }

    #[test]
    fn empty_text_touches_nothing_at_all() {
        let (log, mut ins) = rig(Some("precious"));
        assert_eq!(ins.insert("").expect("insert"), Inserted::Nothing);
        assert!(
            log.borrow().is_empty(),
            "an empty dictation must not clobber the clipboard, got {:?}",
            log.borrow()
        );
    }

    #[test]
    fn text_that_is_only_whitespace_is_still_nothing() {
        let (log, mut ins) = rig(Some("precious"));
        assert_eq!(ins.insert("  \n ").expect("insert"), Inserted::Nothing);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn a_clipboard_that_refuses_the_write_falls_back_to_typing() {
        // No clipboard, no paste. Typing is slower but it is not nothing.
        let (log, mut ins) = rig_with(Some("old"), false, true);
        assert_eq!(ins.insert("hello").expect("insert"), Inserted::Typed);
        let steps = log.borrow().clone();
        assert!(steps.contains(&Step::Type("hello".into())));
        assert!(
            !steps.contains(&Step::Paste),
            "there was nothing on the clipboard to paste"
        );
    }

    #[test]
    fn an_untrusted_process_stops_everything_before_the_clipboard_is_touched() {
        // Without the grant the chord is dropped by the OS with no error,
        // which is how this used to be logged as a success. Not just "no
        // restore ran": nothing happened at all, so there is no half-done
        // save-and-restore to lose a race with.
        let (log, mut ins) =
            rig_gated(Some("precious"), false, false, FakeGates { trusted: false });
        assert_eq!(
            ins.insert("dictated text").expect("insert"),
            Inserted::NotDelivered(NotDelivered::NotTrustedForAccessibility)
        );
        assert!(
            log.borrow().is_empty(),
            "nothing may be read, written, posted, typed or waited on, got {:?}",
            log.borrow()
        );
        assert_eq!(
            ins.clipboard.holds.as_deref(),
            Some("precious"),
            "the clipboard is exactly as it was"
        );
    }

    #[test]
    fn a_platform_that_does_not_gate_accessibility_is_trusted_and_only_a_grant_is_on_macos() {
        // The rule SystemKeys::new and the delivery gate share, as a pure
        // function of the answer, so it runs on every host.
        assert!(trusts(crate::Grant::NotRequired), "Linux and Windows");
        assert!(trusts(crate::Grant::Granted));
        assert!(!trusts(crate::Grant::Denied));
        assert!(!trusts(crate::Grant::Unknown), "never asked is not trusted");
    }

    /// Off macOS the real answer is `NotRequired`, so the real gate is open.
    /// Runs on the Linux CI job; compiled out on this Mac.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_the_system_gate_trusts_without_any_grant() {
        assert_eq!(
            crate::Permission::Accessibility.granted(),
            crate::Grant::NotRequired
        );
        assert!(SystemGates.accessibility_trusted());
    }

    #[test]
    fn clear_gates_are_what_linux_and_windows_see_and_change_nothing() {
        // Off macOS Accessibility is `NotRequired`, which `SystemGates` turns
        // into `FakeGates::clear()`, so this is that platform's path,
        // expressed through the trait rather than a `#[cfg]` that would
        // delete the test on this machine.
        let (log, mut ins) = rig_gated(Some("what the user had"), false, false, FakeGates::clear());
        assert_eq!(
            ins.insert("dictated text").expect("insert"),
            Inserted::PastedAndRestored
        );
        assert_eq!(
            *log.borrow(),
            vec![
                Step::Read,
                Step::Write("dictated text".into()),
                Step::Paste,
                Step::Slept(DEFAULT_RESTORE_WAIT),
                Step::Write("what the user had".into()),
            ]
        );
    }

    /// Needs a desktop session, so it never runs in CI.
    ///
    /// ```text
    /// cargo test -p macalla-platform --lib -- --ignored --nocapture round_trips
    /// ```
    ///
    /// Deliberately does not send any keystrokes: a test that pastes types into
    /// whatever window happens to be focused. The chord is exercised by the
    /// paste harness instead.
    #[test]
    #[ignore = "needs a desktop session"]
    fn the_real_clipboard_round_trips_text() {
        let mut clip = SystemClipboard::new().expect("open the clipboard");
        let before = clip.read_text();
        println!("clipboard held: {before:?}");

        clip.write_text("macalla clipboard check").expect("write");
        assert_eq!(clip.read_text().as_deref(), Some("macalla clipboard check"));

        match before {
            Some(text) => clip.write_text(&text).expect("restore"),
            None => println!("nothing text-shaped to restore"),
        }
    }
}
