// SPDX-License-Identifier: MIT
//! The global shortcut.
//!
//! Push-to-talk (hold) and toggle, any combo or single
//! modifier. A listen-only event tap of our own (see `tap`) rather than a
//! shortcut plugin, because push-to-talk needs the key-RELEASE event, which
//! is also why macOS asks for Input Monitoring on top of Microphone and
//! Accessibility.
//!
//! The default is a bare modifier on one side of the keyboard -- Right Option
//! on macOS -- and not a chord, for a reason that is not ergonomics. macOS
//! Secure Event Input, which any terminal or password field can switch on and
//! leave on, hides every ordinary key from the event tap and leaves
//! modifier-class events alone. A default with a letter in it therefore works
//! until the day it silently does not, and looks like a broken app when it
//! stops. A commercial dictation app defaults to Fn for this reason; two
//! open-source dictation apps default to Option+Space, which has an ordinary
//! key in it, and one of them ships a secure-input detector beside it because
//! of that. Right Option needs no
//! detector and no change in System Settings, where a Mac binds Fn to the
//! emoji picker or to Apple's own dictation out of the box.
//!
//! [`Recogniser`] is the whole state machine and knows nothing about rdev, so
//! it is tested from fixtures with no keyboard. [`listen`] is the thin shell
//! that feeds it real events.

use std::collections::BTreeSet;

/// How the shortcut behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Push-to-talk: dictate while the key is down.
    Hold,
    /// Press to start, press again to stop.
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modifier {
    /// Command on macOS, Super elsewhere.
    Command,
    Control,
    /// Option on macOS.
    Alt,
    Shift,
}

/// Which physical key, for a modifier that exists on both sides of the
/// keyboard. Only a bare-modifier shortcut cares: "hold Right Option" must
/// not fire on the Left Option someone is chording with. In a chord the side
/// is ignored, because nobody configures "the left Shift".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Whichever side; also what a fixture that does not care uses.
    Either,
    Left,
    Right,
}

/// The key that actually triggers, once the modifiers are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// A modifier used as the trigger in its own right, for a bare-modifier
    /// shortcut like "hold Right Option". The side is `Either` when the
    /// shortcut does not mind, and always concrete on a key the OS delivered.
    Mod(Modifier, Side),
    /// The macOS fn key, which several dictation tools use on its own.
    Fn,
    /// Escape. Not a shortcut; pressed twice during a dictation it discards
    /// it.
    Esc,
}

/// What the pipeline is told to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    /// Stop, and throw the recording away: Esc was pressed twice in a row
    /// while dictating. Nothing is pasted.
    Discard,
}

/// A parsed shortcut: zero or more modifiers plus a trigger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    pub modifiers: BTreeSet<Modifier>,
    pub key: Key,
}

/// Render a shortcut the way the platform writes it, so what the app prints
/// matches what the user is looking for on their keyboard. This is the inverse
/// of [`Shortcut::parse`] for every shortcut parse accepts, and the round trip
/// is tested below.
impl std::fmt::Display for Shortcut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for m in &self.modifiers {
            write!(f, "{m}+")?;
        }
        write!(f, "{}", self.key)
    }
}

impl std::fmt::Display for Modifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            // Named as the key cap reads, not as the enum spells it.
            Modifier::Command if cfg!(target_os = "macos") => "Cmd",
            Modifier::Command => "Super",
            Modifier::Control => "Ctrl",
            Modifier::Alt if cfg!(target_os = "macos") => "Option",
            Modifier::Alt => "Alt",
            Modifier::Shift => "Shift",
        };
        f.write_str(name)
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Char(' ') => f.write_str("Space"),
            Key::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            Key::Mod(m, Side::Either) => write!(f, "{m}"),
            Key::Mod(m, Side::Left) => write!(f, "Left {m}"),
            Key::Mod(m, Side::Right) => write!(f, "Right {m}"),
            Key::Fn => f.write_str("Fn"),
            Key::Esc => f.write_str("Esc"),
        }
    }
}

impl Shortcut {
    /// The default, as amended on 2026-09-09: Right Option
    /// on macOS, Right Ctrl elsewhere. A bare modifier, one side, no chord --
    /// see the module docs for why. The original Cmd+Shift+D was found bound
    /// in both iTerm2 and Terminal.app, the two places a developer first tries
    /// a dictation tool, and it is also another dictation app's default, so the collision was
    /// nobody's carelessness; it just reads well and is taken.
    pub fn default_for_platform() -> Self {
        let modifier = if cfg!(target_os = "macos") {
            Modifier::Alt
        } else {
            Modifier::Control
        };
        Self {
            modifiers: BTreeSet::new(),
            key: Key::Mod(modifier, Side::Right),
        }
    }

    /// Whether this shortcut keeps working while macOS Secure Event Input is
    /// on. Modifier-class events reach the event tap regardless (observed
    /// 2026-09-08: Shift, Cmd, Option and Ctrl all arrived while not one
    /// letter did); ordinary keys do not. Fn is delivered as a flags-changed
    /// event like a modifier, which is the class secure input leaves alone,
    /// so it is counted with them -- inferred from the event class, not
    /// separately observed.
    pub fn survives_secure_input(&self) -> bool {
        matches!(self.key, Key::Mod(..) | Key::Fn)
    }

    /// Parse "cmd+shift+d", "ctrl+alt+space", "fn", "right option" and so on.
    /// A side ("right", "left") is accepted only on a bare modifier, since a
    /// chord ignores it.
    pub fn parse(text: &str) -> crate::Result<Self> {
        let mut modifiers = BTreeSet::new();
        let mut trigger: Option<Key> = None;

        let parts: Vec<&str> = text
            .split('+')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if parts.is_empty() {
            return Err(crate::Error::Shortcut(
                "a shortcut needs at least one key".into(),
            ));
        }

        for part in &parts {
            if let Some(key) = parse_sided_modifier(part) {
                if parts.len() == 1 {
                    return Ok(Self {
                        modifiers: BTreeSet::new(),
                        key,
                    });
                }
                return Err(crate::Error::Shortcut(format!(
                    "{text:?} names a side inside a chord, and a chord fires on \
                     either side. Use {part:?} on its own, or drop the side."
                )));
            }
            let lowered = part.to_lowercase();
            if let Some(m) = parse_modifier(&lowered) {
                // The LAST modifier named becomes the trigger if nothing else
                // does, so "option" alone is a shortcut and "cmd+shift+d" is
                // not three triggers.
                modifiers.insert(m);
                continue;
            }
            let key = match lowered.as_str() {
                "fn" | "function" => Key::Fn,
                "esc" | "escape" => Key::Esc,
                "space" | "spacebar" => Key::Char(' '),
                other => {
                    let mut chars = other.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => Key::Char(c),
                        _ => {
                            return Err(crate::Error::Shortcut(format!(
                                "{part:?} is not a key I recognise"
                            )))
                        }
                    }
                }
            };
            if trigger.is_some() {
                return Err(crate::Error::Shortcut(format!(
                    "{text:?} names more than one trigger key"
                )));
            }
            trigger = Some(key);
        }

        match trigger {
            Some(Key::Esc) => Err(crate::Error::Shortcut(format!(
                "{text:?}: Esc is the key that discards a dictation, so it \
                 cannot also be the one that starts it"
            ))),
            Some(key) => Ok(Self { modifiers, key }),
            // Modifiers only. One is unambiguous -- "hold Option to dictate".
            // Two or more is not: in "cmd+shift" either could be the trigger
            // and the other the gate, and guessing would give the user a
            // shortcut that fires when they did not expect it.
            None if modifiers.len() == 1 => {
                let only = *modifiers.iter().next().expect("just checked");
                Ok(Self {
                    modifiers: BTreeSet::new(),
                    key: Key::Mod(only, Side::Either),
                })
            }
            None => Err(crate::Error::Shortcut(format!(
                "{text:?} is only modifiers, so there is no key to press. \
                 Add one, or use a single modifier on its own."
            ))),
        }
    }
}

/// "right option", "Left Cmd" and so on: a side and a modifier, nothing else.
fn parse_sided_modifier(part: &str) -> Option<Key> {
    let mut words = part.split_whitespace().map(str::to_lowercase);
    let side = match words.next()?.as_str() {
        "right" => Side::Right,
        "left" => Side::Left,
        _ => return None,
    };
    let modifier = parse_modifier(&words.next()?)?;
    if words.next().is_some() {
        return None;
    }
    Some(Key::Mod(modifier, side))
}

fn parse_modifier(lowered: &str) -> Option<Modifier> {
    match lowered {
        "cmd" | "command" | "meta" | "super" | "win" => Some(Modifier::Command),
        "ctrl" | "control" => Some(Modifier::Control),
        "alt" | "option" | "opt" => Some(Modifier::Alt),
        "shift" => Some(Modifier::Shift),
        _ => None,
    }
}

/// macOS virtual keycodes, the same on every keyboard layout. The ones a
/// shortcut can be made of; the rest map to nothing.
pub mod keycode {
    pub const COMMAND_LEFT: u16 = 55;
    pub const COMMAND_RIGHT: u16 = 54;
    pub const SHIFT_LEFT: u16 = 56;
    pub const SHIFT_RIGHT: u16 = 60;
    pub const CONTROL_LEFT: u16 = 59;
    pub const CONTROL_RIGHT: u16 = 62;
    pub const OPTION_LEFT: u16 = 58;
    pub const OPTION_RIGHT: u16 = 61;
    pub const FN: u16 = 63;
    pub const CAPS_LOCK: u16 = 57;
    pub const ESCAPE: u16 = 53;
    pub const SPACE: u16 = 49;
    pub const UP_ARROW: u16 = 126;
}

/// Translate a macOS virtual keycode into ours, or `None` for keys we do not
/// care about.
///
/// The side of a modifier is kept, because a bare-modifier shortcut is one
/// side or the other; a chord ignores it. Pure, so the table is checked
/// without a keyboard.
pub fn from_keycode(code: u16) -> Option<Key> {
    use keycode as k;
    Some(match code {
        k::COMMAND_LEFT => Key::Mod(Modifier::Command, Side::Left),
        k::COMMAND_RIGHT => Key::Mod(Modifier::Command, Side::Right),
        k::SHIFT_LEFT => Key::Mod(Modifier::Shift, Side::Left),
        k::SHIFT_RIGHT => Key::Mod(Modifier::Shift, Side::Right),
        k::CONTROL_LEFT => Key::Mod(Modifier::Control, Side::Left),
        k::CONTROL_RIGHT => Key::Mod(Modifier::Control, Side::Right),
        k::OPTION_LEFT => Key::Mod(Modifier::Alt, Side::Left),
        k::OPTION_RIGHT => Key::Mod(Modifier::Alt, Side::Right),
        k::FN => Key::Fn,
        k::ESCAPE => Key::Esc,
        k::SPACE => Key::Char(' '),
        // ANSI letters and digits, by position. The keycode is the physical
        // key, so on a non-QWERTY layout "d" is where QWERTY's D is.
        0 => Key::Char('a'),
        11 => Key::Char('b'),
        8 => Key::Char('c'),
        2 => Key::Char('d'),
        14 => Key::Char('e'),
        3 => Key::Char('f'),
        5 => Key::Char('g'),
        4 => Key::Char('h'),
        34 => Key::Char('i'),
        38 => Key::Char('j'),
        40 => Key::Char('k'),
        37 => Key::Char('l'),
        46 => Key::Char('m'),
        45 => Key::Char('n'),
        31 => Key::Char('o'),
        35 => Key::Char('p'),
        12 => Key::Char('q'),
        15 => Key::Char('r'),
        1 => Key::Char('s'),
        17 => Key::Char('t'),
        32 => Key::Char('u'),
        9 => Key::Char('v'),
        13 => Key::Char('w'),
        7 => Key::Char('x'),
        16 => Key::Char('y'),
        6 => Key::Char('z'),
        29 => Key::Char('0'),
        18 => Key::Char('1'),
        19 => Key::Char('2'),
        20 => Key::Char('3'),
        21 => Key::Char('4'),
        23 => Key::Char('5'),
        22 => Key::Char('6'),
        26 => Key::Char('7'),
        28 => Key::Char('8'),
        25 => Key::Char('9'),
        _ => return None,
    })
}

/// Whether a key-down the tap flagged as an OS auto-repeat should never
/// reach [`Recogniser::press`] at all.
///
/// Only Esc needs this here. A held trigger's repeats are dropped separately
/// by `handle` (macOS), because after a gap in the tap `trigger_held` may have been
/// forgotten while the key is still repeating. A held modifier's repeat
/// (which macOS never actually sends -- `FlagsChanged` fires once per state
/// change) would just re-insert into `held_modifiers`, a no-op. Esc has no
/// such protection: [`Recogniser::press`] treats every Esc it is handed as a
/// real press (see the comment there for why the old `esc_held` latch that
/// used to do this job is gone), so a held Esc auto-repeating for as long as
/// someone pauses on it would otherwise count as several presses and
/// mis-fire the double-press discard.
///
/// Dropping repeats of every key here, not just Esc, would also change
/// behaviour: a repeat of an ordinary key disarms `esc_armed` on its way
/// through `press` (`press`'s "any other key in between" rule), and
/// silently swallowing it would keep Esc armed for longer. The trigger's
/// repeats are the exception, dropped by the listener for a different
/// reason (see `handle`), so a held trigger no longer disarms Esc.
///
/// `pub` like [`from_keycode`]: its only caller is the listener, behind
/// the `hotkey` feature, so a build with that feature off (the plain `cargo
/// test -p macalla-platform` most of this crate is checked with, and every
/// non-macOS target) has no in-crate caller at all, and a private fn would
/// warn `dead_code` there.
pub fn is_droppable_repeat(key: Key, repeat: bool) -> bool {
    repeat && key == Key::Esc
}

/// One event from the tap, applied to the recogniser: the action it causes
/// and the new [`Recogniser::is_held`] if it changed. What the listener does
/// with every event, apart from telling the app. Pure, so it is checked
/// without a tap.
///
/// A key-down macOS flagged as an auto-repeat is dropped for Esc (see
/// [`is_droppable_repeat`]) and for the trigger. A held trigger is normally
/// caught by the recogniser's own `trigger_held`, but after
/// [`Recogniser::forget_keys`] has let go of it, its next repeat would look
/// like a fresh press and start or toggle a dictation.
#[cfg(all(feature = "hotkey", target_os = "macos"))]
fn handle(r: &mut Recogniser, event: crate::tap::TapEvent) -> (Option<Action>, Option<bool>) {
    use crate::tap::{RawKey, TapEvent};
    match event {
        // Trace every key the tap delivers, at debug. Without this, "the
        // shortcut does nothing" has two indistinguishable causes -- the tap
        // is not receiving anything because the OS has not granted Input
        // Monitoring, or it is receiving keys and the recogniser is not
        // matching them. One line here splits those, and they need opposite
        // fixes.
        TapEvent::Key(RawKey::Down { code, repeat }) => {
            tracing::debug!(code, repeat, mapped = ?from_keycode(code), "key down");
            match from_keycode(code) {
                Some(k) if is_droppable_repeat(k, repeat) || (repeat && r.is_trigger(k)) => {
                    (None, None)
                }
                Some(k) => r.key(k, true),
                None => (None, None),
            }
        }
        TapEvent::Key(RawKey::Up(code)) => {
            tracing::debug!(code, mapped = ?from_keycode(code), "key up");
            from_keycode(code).map_or((None, None), |k| r.key(k, false))
        }
        TapEvent::KeysLost { still_down } => {
            let down: Vec<Key> = still_down.into_iter().filter_map(from_keycode).collect();
            (None, r.forget_keys(&down))
        }
    }
}

/// A running global listener. Dropping this leaks the listener thread, which
/// is deliberate: the tap's run loop cannot be stopped from outside, so the
/// thread lives for the life of the process. One per process.
#[cfg(all(feature = "hotkey", target_os = "macos"))]
pub struct Listener {
    events: std::sync::mpsc::Receiver<crate::Result<Action>>,
    recogniser: std::sync::Arc<std::sync::Mutex<Recogniser>>,
    held: HeldWatch,
}

/// What [`Listener::on_held`] set, called from the tap thread.
#[cfg(all(feature = "hotkey", target_os = "macos"))]
type HeldWatch = std::sync::Arc<std::sync::Mutex<Option<Box<dyn Fn(bool) + Send>>>>;

/// A way to tell the listener whether a dictation is running, whatever
/// started it -- the shortcut, the tray, an agent -- for its whole length,
/// including a wait on a cloud engine after the key is up, so that Esc twice
/// discards it. Cheap to clone; the dictation thread holds one.
#[cfg(all(feature = "hotkey", target_os = "macos"))]
#[derive(Clone)]
pub struct DictationState(std::sync::Arc<std::sync::Mutex<Recogniser>>);

#[cfg(all(feature = "hotkey", target_os = "macos"))]
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

#[cfg(all(feature = "hotkey", target_os = "macos"))]
impl Listener {
    /// Start watching for `shortcut`.
    ///
    /// On macOS this needs **Input Monitoring**, which is why the app asks for
    /// it on top of Microphone and Accessibility: a global key-RELEASE event is
    /// what push-to-talk is made of, and nothing less privileged delivers one.
    /// Without the grant the listener reports a failure through the channel
    /// rather than silently never firing.
    pub fn start(shortcut: Shortcut, mode: Mode) -> Self {
        let (tx, events) = std::sync::mpsc::channel();
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Recogniser::new(shortcut, mode)));
        let recogniser = shared.clone();
        let held = HeldWatch::default();
        let watch = held.clone();
        std::thread::Builder::new()
            .name("macalla-hotkey".into())
            .spawn(move || {
                let failures = tx.clone();
                // The tap blocks this thread for as long as the process runs.
                let result = crate::tap::listen(move |event| {
                    let (action, held) = {
                        let mut recogniser = recogniser.lock().unwrap_or_else(|p| p.into_inner());
                        handle(&mut recogniser, event)
                    };
                    // Told with the recogniser let go, and before the action
                    // goes out, so a held press is heard before what it starts.
                    if let Some(held) = held {
                        if let Some(watch) =
                            watch.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
                        {
                            watch(held);
                        }
                    }
                    if let Some(action) = action {
                        // A closed channel means the app is going away.
                        let _ = tx.send(Ok(action));
                    }
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

    /// Wait for the next action. `None` once the listener has gone.
    pub fn recv(&self) -> Option<crate::Result<Action>> {
        self.events.recv().ok()
    }

    /// The next action if one is waiting.
    pub fn try_recv(&self) -> Option<crate::Result<Action>> {
        self.events.try_recv().ok()
    }
}

/// Turns key events into [`Action`]s.
pub struct Recogniser {
    shortcut: Shortcut,
    mode: Mode,
    held_modifiers: BTreeSet<Modifier>,
    trigger_held: bool,
    /// The trigger's own state: started by a press, stopped by the release
    /// in hold mode or the next press in toggle mode, or ended by a discard,
    /// a reset or `set_dictating(false)`. The app never sets it.
    active: bool,
    /// The app says a dictation is running, whatever started it: set by
    /// [`Recogniser::set_dictating`], cleared by it or by a discard. It
    /// outlasts the trigger: with a cloud engine the key comes up, the
    /// recording goes to the provider, and the dictation runs on while it
    /// waits, so Esc twice must still reach it.
    dictating: bool,
    /// The trigger's gesture began while the app said a dictation was
    /// running. It is the next dictation's -- a press during the wait after
    /// release, which the app starts once this one is over -- so the end of
    /// this one does not end it: its release must still stop the next.
    next: bool,
    /// Esc twice fired, and the app has not yet said the dictation is over.
    /// A press from here on is the next dictation's, like one made during
    /// the wait, and its release must still stop what it starts. A press
    /// made before Esc twice was thrown away with the dictation. Ending the
    /// dictation reports it, however far the Discard has got.
    ending: bool,
    /// Escape was the last key pressed during this dictation. One more
    /// discards. Any other key in between resets it, so "Esc, type, Esc" is
    /// two ordinary escapes and not a discard.
    esc_armed: bool,
}

impl Recogniser {
    pub fn new(shortcut: Shortcut, mode: Mode) -> Self {
        Self {
            shortcut,
            mode,
            held_modifiers: BTreeSet::new(),
            trigger_held: false,
            active: false,
            dictating: false,
            next: false,
            ending: false,
            esc_armed: false,
        }
    }

    /// Whether exactly the required modifiers are down -- no more, no fewer.
    ///
    /// Exact, not "at least". Cmd+Shift+Alt+D belongs to whatever app defines
    /// it, and firing on a superset steals other applications' keystrokes.
    fn modifiers_match(&self) -> bool {
        self.held_modifiers == self.shortcut.modifiers
    }

    /// Whether a delivered key is this shortcut's trigger. A shortcut that
    /// does not name a side accepts both; one that does accepts only that one.
    fn is_trigger(&self, pressed: Key) -> bool {
        match (self.shortcut.key, pressed) {
            (Key::Mod(want, Side::Either), Key::Mod(got, _)) => want == got,
            (Key::Mod(want, want_side), Key::Mod(got, got_side)) => {
                want == got && want_side == got_side
            }
            (want, got) => want == got,
        }
    }

    /// Whether the trigger's own gesture is under way: a press started it,
    /// and neither its release (hold), the next press (toggle), a discard nor
    /// a reset has ended it. Not whether a dictation is running -- one the
    /// tray started, or one waiting on a cloud engine after the key is up, is
    /// the app's to say, through [`Recogniser::set_dictating`].
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Whether a dictation is running, whatever started it -- the shortcut,
    /// the tray, an agent -- for its whole length, including a wait on a
    /// cloud engine after the key is up. Esc twice discards whatever is
    /// running; the first version only armed Esc for dictations the shortcut
    /// had started, and a tray dictation could not be discarded at all.
    ///
    /// Call it with `true` when a dictation starts and `false` once it is
    /// over. `true` leaves the trigger's own state alone, so a quick tap the
    /// app hears only after the key is up stays released and the next tap
    /// still starts. `false` ends both, and disarms Esc -- except a gesture
    /// that began while the dictation was running, which is the next
    /// dictation's: a key pressed during the wait and still down when this
    /// one ends must stop the next one when it comes up. A press made after
    /// Esc twice counts as one of those; a press made before it went with the
    /// discarded dictation.
    ///
    /// Returns whether Esc twice fired since the app last said a dictation
    /// was running. A trigger ending one reads it as the last word: the
    /// Discard may still be on its way to it, and a dictation kept on a
    /// missed Discard is one a remote provider is sent. Asked after this,
    /// the recogniser sends no Discard for that dictation.
    pub fn set_dictating(&mut self, dictating: bool) -> bool {
        let discarded = std::mem::take(&mut self.ending);
        self.dictating = dictating;
        if !dictating {
            if !std::mem::take(&mut self.next) {
                self.active = false;
            }
            self.esc_armed = false;
        }
        discarded
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the trigger is physically down in hold mode, from its press
    /// to its key-up, whatever it started or stopped: still true after Esc
    /// twice or the end of its dictation, while the key stays down. Always
    /// false in toggle mode, where the key is pressed, not held. A key-up the
    /// tap never delivers leaves it true, as it leaves the next press
    /// ignored, until the key goes down and up again -- or until the tap
    /// reports that it was switched off, which
    /// [`Recogniser::forget_keys`] handles.
    pub fn is_held(&self) -> bool {
        self.mode == Mode::Hold && self.trigger_held
    }

    /// [`Recogniser::press`] (`down`) or [`Recogniser::release`], and the new
    /// [`Recogniser::is_held`] if the key changed it.
    pub fn key(&mut self, key: Key, down: bool) -> (Option<Action>, Option<bool>) {
        let before = self.is_held();
        let action = if down {
            self.press(key)
        } else {
            self.release(key)
        };
        let after = self.is_held();
        (action, (after != before).then_some(after))
    }

    /// A key went down.
    ///
    /// Every Esc this method is handed is taken to be a genuine new press,
    /// never an auto-repeat of one still held. That used to be this
    /// method's own job, through an `esc_held` latch set on every Esc down
    /// and cleared only by the matching key-up or by `reset()` -- and
    /// production never calls `reset()`. If one key-up never reached the
    /// tap (Secure Event Input switching on mid-press, the tap disabled for
    /// a moment), the latch stayed set and every later Esc looked like a
    /// repeat of a press that had, in reality, long since ended: Esc twice
    /// stopped discarding until the app restarted. The macOS tap now reads
    /// whether a key-down is an OS auto-repeat straight from the event
    /// (`RawKey::Down`'s `repeat` field, `tap.rs`) and the listener drops a
    /// repeat-flagged Esc before it ever reaches here (see
    /// `is_droppable_repeat`), so the latch's only job is done upstream and
    /// for a fact rather than an inference from a key-up that might not
    /// come. Nothing else feeds this method in production -- `Listener` is
    /// the only caller outside tests (`apps/cli`, `apps/desktop` and this
    /// module's own tests call `press`/`release` directly as fixtures, never
    /// simulating an unreleased repeat) -- so removing the latch entirely is
    /// safe rather than merely papering over the same gap.
    pub fn press(&mut self, key: Key) -> Option<Action> {
        // Esc twice in a row while dictating discards it.
        // Counted, not timed: the recogniser has no clock, and "the last key
        // was also Esc" is the rule a person can predict. Any other key --
        // a modifier too -- disarms it. Esc never fires as a shortcut key
        // through this path, so a shortcut set to Esc is refused below.
        if key == Key::Esc && self.shortcut.key != Key::Esc {
            // Running while the trigger holds it, or for as long as the app
            // says, which covers a wait after release.
            let running = self.active || self.dictating;
            if running && self.esc_armed {
                self.esc_armed = false;
                self.active = false;
                self.next = false;
                self.dictating = false;
                self.ending = true;
                return Some(Action::Discard);
            }
            self.esc_armed = running;
            return None;
        }
        self.esc_armed = false;
        // Track modifiers first: one may also be the trigger.
        let is_trigger = self.is_trigger(key);
        if let Key::Mod(m, _) = key {
            if !is_trigger {
                self.held_modifiers.insert(m);
                return None;
            }
        }
        if !is_trigger {
            return None;
        }
        // The trigger. Auto-repeat sends this over and over while the key is
        // down (X11 especially), so a second press with no release between is
        // not a new gesture -- it is the same one, still happening.
        if self.trigger_held {
            return None;
        }
        if !self.modifiers_match() {
            return None;
        }
        self.trigger_held = true;
        match self.mode {
            Mode::Hold => {
                if self.active {
                    return None;
                }
                self.active = true;
                self.next = self.dictating || self.ending;
                Some(Action::Start)
            }
            Mode::Toggle => {
                self.active = !self.active;
                self.next = self.active && (self.dictating || self.ending);
                Some(if self.active {
                    Action::Start
                } else {
                    Action::Stop
                })
            }
        }
    }

    /// A key came up.
    pub fn release(&mut self, key: Key) -> Option<Action> {
        if key == Key::Esc && self.shortcut.key != Key::Esc {
            return None;
        }
        let is_trigger = self.is_trigger(key);
        if let Key::Mod(m, _) = key {
            if !is_trigger {
                self.held_modifiers.remove(&m);
                // Deliberately not a stop. Rolling a finger off Shift
                // mid-sentence is common, and the trigger key is what holds
                // the recording open.
                return None;
            }
        }
        if !is_trigger {
            return None;
        }
        self.trigger_held = false;
        if let Key::Mod(m, _) = key {
            self.held_modifiers.remove(&m);
        }
        match self.mode {
            Mode::Hold if self.active => {
                self.active = false;
                self.next = false;
                Some(Action::Stop)
            }
            // Toggle waits for the next press.
            _ => None,
        }
    }

    /// The tap was switched off for a while and key events went by unseen:
    /// forget the keys that are no longer down, and disarm Esc. `still_down`
    /// is what is physically down now and will still deliver a key-up.
    /// Returns the new [`Recogniser::is_held`] if this changed it, as
    /// [`Recogniser::key`] does, so whoever watches the held state is told.
    ///
    /// Only forgets, never adds: a key pressed during the gap was never
    /// seen go down, and its key-up must not be what completes a gesture.
    /// A trigger still down stays held, so its release still stops the
    /// dictation it started.
    ///
    /// A dictation is left running, and one is never started: a short gap
    /// in the tap is no reason to end one. If the trigger came up in the gap
    /// and its release was not delivered, its next press and release stop
    /// it, as its release would have. Whether the app says one is running,
    /// and whether Esc twice already ended one, are the app's and the
    /// discard's, not the keys', so they stay too.
    ///
    /// Esc is disarmed because nobody knows what was typed in the gap; at
    /// worst a discard takes one more Esc.
    pub fn forget_keys(&mut self, still_down: &[Key]) -> Option<bool> {
        let before = self.is_held();
        self.held_modifiers.retain(|held| {
            still_down
                .iter()
                .any(|k| matches!(k, Key::Mod(m, _) if m == held))
        });
        self.trigger_held = self.trigger_held && still_down.iter().any(|&k| self.is_trigger(k));
        self.esc_armed = false;
        let after = self.is_held();
        (after != before).then_some(after)
    }

    /// Forget everything, stopping a dictation that was running.
    ///
    /// For the cases where key events go missing: the listener restarts, the
    /// screen locks, a permission is revoked mid-dictation. Without this a
    /// hold-mode dictation whose release event never arrived records for ever.
    ///
    /// `dictating` survives it. That state is the app's, set through
    /// [`Recogniser::set_dictating`], and a dictation the app says is running
    /// is still running after the keys are forgotten.
    pub fn reset(&mut self) -> Option<Action> {
        self.held_modifiers.clear();
        self.trigger_held = false;
        self.esc_armed = false;
        self.next = false;
        if self.active {
            self.active = false;
            return Some(Action::Stop);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_and_right_modifiers_are_the_same_key_in_a_chord() {
        // Nobody configures "the left Shift" in a chord, so the side is kept
        // on the key (a bare-modifier shortcut needs it) and ignored by the
        // chord: Cmd+Shift+D fires with either Shift.
        use keycode as k;
        assert_eq!(
            from_keycode(k::SHIFT_LEFT),
            Some(Key::Mod(Modifier::Shift, Side::Left))
        );
        assert_eq!(
            from_keycode(k::SHIFT_RIGHT),
            Some(Key::Mod(Modifier::Shift, Side::Right))
        );
        assert_eq!(
            from_keycode(k::COMMAND_LEFT),
            Some(Key::Mod(Modifier::Command, Side::Left))
        );
        assert_eq!(
            from_keycode(k::OPTION_RIGHT),
            Some(Key::Mod(Modifier::Alt, Side::Right))
        );
        assert_eq!(from_keycode(k::FN), Some(Key::Fn));

        let mut r = cmd_shift_d(Mode::Hold);
        r.press(from_keycode(k::COMMAND_RIGHT).unwrap());
        r.press(from_keycode(k::SHIFT_RIGHT).unwrap());
        assert_eq!(r.press(from_keycode(2).unwrap()), Some(Action::Start));
    }

    #[test]
    fn what_we_print_is_what_parse_accepts() {
        // The startup banner tells the user which keys to hold. If Display and
        // parse ever disagree, the app prints a shortcut that its own --shortcut
        // flag would reject, and the user has no way to tell which is right.
        for text in [
            "cmd+shift+d",
            "ctrl+alt+space",
            "shift+a",
            "fn",
            "option",
            "right option",
            "left ctrl",
        ] {
            let Ok(parsed) = Shortcut::parse(text) else {
                continue; // not every sample is valid on every platform
            };
            let printed = parsed.to_string();
            let reparsed = Shortcut::parse(&printed)
                .unwrap_or_else(|e| panic!("printed {printed:?} from {text:?}, unparseable: {e}"));
            assert_eq!(parsed, reparsed, "{text:?} printed as {printed:?}");
        }
    }

    #[test]
    fn the_default_shortcut_prints_the_way_the_key_caps_read() {
        let printed = Shortcut::default_for_platform().to_string();
        if cfg!(target_os = "macos") {
            assert_eq!(printed, "Right Option");
        } else {
            assert_eq!(printed, "Right Ctrl");
        }
    }

    #[test]
    fn the_default_is_a_bare_modifier_so_secure_input_cannot_eat_it() {
        // Secure Event Input hides every ordinary key from the event tap and
        // leaves modifier-class events alone. A default with a letter in it
        // is dead the moment a terminal or a password field turns it on,
        // which is how a whole evening went on 2026-09-08. So the default
        // must be a modifier on its own, and it must be one side, because
        // the other Option is what people chord with (Option+Backspace,
        // Option+arrows, Option+3 for # on a UK keyboard) and each chord
        // would start a dictation.
        let s = Shortcut::default_for_platform();
        assert!(s.modifiers.is_empty(), "no chord: {s}");
        assert!(
            matches!(s.key, Key::Mod(_, Side::Right)),
            "one side of a bare modifier: {s}"
        );
        assert!(s.survives_secure_input());
    }

    #[test]
    fn a_shortcut_with_an_ordinary_key_does_not_survive_secure_input() {
        assert!(!Shortcut::parse("cmd+shift+d")
            .unwrap()
            .survives_secure_input());
        assert!(!Shortcut::parse("ctrl+alt+space")
            .unwrap()
            .survives_secure_input());
        assert!(Shortcut::parse("option").unwrap().survives_secure_input());
        assert!(Shortcut::parse("fn").unwrap().survives_secure_input());
    }

    #[test]
    fn a_sided_modifier_only_fires_on_that_side() {
        let mut r = Recogniser::new(Shortcut::parse("right option").unwrap(), Mode::Hold);
        assert_eq!(r.press(Key::Mod(Modifier::Alt, Side::Left)), None);
        assert_eq!(r.release(Key::Mod(Modifier::Alt, Side::Left)), None);
        assert_eq!(
            r.press(Key::Mod(Modifier::Alt, Side::Right)),
            Some(Action::Start)
        );
        assert_eq!(
            r.release(Key::Mod(Modifier::Alt, Side::Right)),
            Some(Action::Stop)
        );
    }

    #[test]
    fn an_unsided_modifier_fires_on_either_side() {
        let mut r = Recogniser::new(Shortcut::parse("option").unwrap(), Mode::Hold);
        assert_eq!(
            r.press(Key::Mod(Modifier::Alt, Side::Left)),
            Some(Action::Start)
        );
        assert_eq!(
            r.release(Key::Mod(Modifier::Alt, Side::Left)),
            Some(Action::Stop)
        );
        assert_eq!(
            r.press(Key::Mod(Modifier::Alt, Side::Right)),
            Some(Action::Start)
        );
    }

    #[test]
    fn a_side_belongs_to_a_bare_modifier_only() {
        assert_eq!(
            Shortcut::parse("right option").unwrap().key,
            Key::Mod(Modifier::Alt, Side::Right)
        );
        assert_eq!(
            Shortcut::parse("Left Cmd").unwrap().key,
            Key::Mod(Modifier::Command, Side::Left)
        );
        // In a chord the side is meaningless -- nobody configures "the left
        // Shift" -- and accepting it would promise a distinction the chord
        // does not keep.
        let err = Shortcut::parse("right option+d").expect_err("side in a chord");
        assert!(err.to_string().contains("right option"), "got: {err}");
    }

    #[test]
    fn the_default_shortcut_can_be_built_from_real_key_events() {
        // The mapping and the recogniser have to agree, or the shortcut in the
        // settings file never fires however correct each half looks alone.
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        let trigger = if cfg!(target_os = "macos") {
            keycode::OPTION_RIGHT
        } else {
            keycode::CONTROL_RIGHT
        };
        assert_eq!(
            r.press(from_keycode(trigger).expect("the trigger maps")),
            Some(Action::Start)
        );
        assert_eq!(
            r.release(from_keycode(trigger).expect("the trigger maps")),
            Some(Action::Stop)
        );
    }

    #[test]
    fn keys_we_do_not_care_about_map_to_nothing_rather_than_a_wrong_key() {
        assert_eq!(from_keycode(keycode::ESCAPE), Some(Key::Esc));
        assert_eq!(from_keycode(keycode::UP_ARROW), None);
        assert_eq!(
            from_keycode(keycode::CAPS_LOCK),
            None,
            "caps lock is not a shortcut key"
        );
    }

    fn cmd_shift_d(mode: Mode) -> Recogniser {
        Recogniser::new(
            Shortcut {
                modifiers: [Modifier::Command, Modifier::Shift].into_iter().collect(),
                key: Key::Char('d'),
            },
            mode,
        )
    }

    /// Press the whole combo in the order a person would.
    fn press_combo(r: &mut Recogniser) -> Option<Action> {
        r.press(Key::Mod(Modifier::Command, Side::Either));
        r.press(Key::Mod(Modifier::Shift, Side::Either));
        r.press(Key::Char('d'))
    }

    #[test]
    fn holding_the_combo_starts_and_releasing_the_key_stops() {
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        assert!(r.is_active());
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        assert!(!r.is_active());
    }

    #[test]
    fn auto_repeat_does_not_start_a_second_dictation() {
        // X11 sends key-down over and over while a key is held. Without this the hold path restarts constantly.
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        for _ in 0..20 {
            assert_eq!(r.press(Key::Char('d')), None, "auto-repeat must be ignored");
        }
        assert!(r.is_active());
    }

    #[test]
    fn auto_repeat_does_not_flap_the_toggle() {
        // The same defect is far worse in toggle mode: it would start and stop
        // dictation as fast as the key repeats.
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        for _ in 0..20 {
            assert_eq!(r.press(Key::Char('d')), None);
        }
        assert!(r.is_active(), "still one dictation, not twenty");
    }

    #[test]
    fn toggle_starts_on_one_press_and_stops_on_the_next() {
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        assert_eq!(r.release(Key::Char('d')), None, "toggle ignores release");
        assert!(r.is_active());
        assert_eq!(press_combo(&mut r), Some(Action::Stop));
        assert!(!r.is_active());
    }

    #[test]
    fn the_trigger_alone_does_nothing_without_its_modifiers() {
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(
            r.press(Key::Char('d')),
            None,
            "typing 'd' is not a shortcut"
        );
        assert!(!r.is_active());
    }

    #[test]
    fn a_partial_combo_does_nothing() {
        let mut r = cmd_shift_d(Mode::Hold);
        r.press(Key::Mod(Modifier::Command, Side::Either));
        assert_eq!(r.press(Key::Char('d')), None, "shift was never held");
        assert!(!r.is_active());
    }

    #[test]
    fn an_extra_modifier_means_it_is_somebody_elses_shortcut() {
        // Cmd+Shift+Alt+D belongs to whatever app defines it. Firing on a
        // superset would steal keystrokes from other applications.
        let mut r = cmd_shift_d(Mode::Hold);
        r.press(Key::Mod(Modifier::Command, Side::Either));
        r.press(Key::Mod(Modifier::Shift, Side::Either));
        r.press(Key::Mod(Modifier::Alt, Side::Either));
        assert_eq!(r.press(Key::Char('d')), None);
        assert!(!r.is_active());
    }

    #[test]
    fn letting_go_of_a_modifier_does_not_cut_the_dictation_short() {
        // Rolling a finger off Shift mid-sentence is common and must not end
        // the recording. The trigger key is what holds it open.
        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        assert_eq!(r.release(Key::Mod(Modifier::Shift, Side::Either)), None);
        assert!(r.is_active(), "still talking");
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
    }

    #[test]
    fn releasing_the_key_when_nothing_is_running_is_not_a_stop() {
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(r.release(Key::Char('d')), None);
    }

    #[test]
    fn a_bare_modifier_can_be_the_whole_shortcut() {
        // "Hold Option to dictate", which is allowed.
        let mut r = Recogniser::new(
            Shortcut {
                modifiers: BTreeSet::new(),
                key: Key::Mod(Modifier::Alt, Side::Either),
            },
            Mode::Hold,
        );
        assert_eq!(
            r.press(Key::Mod(Modifier::Alt, Side::Either)),
            Some(Action::Start)
        );
        assert_eq!(
            r.press(Key::Mod(Modifier::Alt, Side::Either)),
            None,
            "auto-repeat"
        );
        assert_eq!(
            r.release(Key::Mod(Modifier::Alt, Side::Either)),
            Some(Action::Stop)
        );
    }

    #[test]
    fn the_fn_key_works_on_its_own_too() {
        let mut r = Recogniser::new(
            Shortcut {
                modifiers: BTreeSet::new(),
                key: Key::Fn,
            },
            Mode::Hold,
        );
        assert_eq!(r.press(Key::Fn), Some(Action::Start));
        assert_eq!(r.release(Key::Fn), Some(Action::Stop));
    }

    #[test]
    fn esc_twice_discards_a_dictation_the_tray_started() {
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert!(!r.is_active());
    }

    #[test]
    fn a_hold_press_is_held_until_its_release() {
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        let key = Shortcut::default_for_platform().key;
        assert!(!r.is_held());
        assert_eq!(r.press(key), Some(Action::Start));
        assert!(r.is_held());
        r.set_dictating(true);
        assert!(r.is_held(), "the dictation starting changes nothing");
        assert_eq!(r.release(key), Some(Action::Stop));
        assert!(!r.is_held());
    }

    #[test]
    fn a_key_still_down_after_esc_twice_is_held_until_its_key_up() {
        let key = Shortcut::default_for_platform().key;
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        assert_eq!(r.key(key, true), (Some(Action::Start), Some(true)));
        r.set_dictating(true);
        assert_eq!(r.key(Key::Esc, true), (None, None));
        assert_eq!(r.key(Key::Esc, false), (None, None));
        assert_eq!(r.key(Key::Esc, true), (Some(Action::Discard), None));
        assert!(r.is_held(), "Esc twice leaves the key down");
        assert_eq!(r.key(key, false), (None, Some(false)), "no Stop, but up");
        assert!(!r.is_held());
    }

    #[test]
    fn a_key_still_down_after_its_dictation_ended_is_held_until_its_key_up() {
        // At its cap: the dictation ends while the key is still down.
        let key = Shortcut::default_for_platform().key;
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        r.key(key, true);
        r.set_dictating(true);
        r.set_dictating(false);
        assert!(r.is_held(), "the dictation ended, the key did not come up");
        assert_eq!(r.key(key, false), (None, Some(false)));
        assert!(!r.is_held());
    }

    #[test]
    fn other_keys_change_nothing_held() {
        let key = Shortcut::default_for_platform().key;
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        assert_eq!(r.key(Key::Char('a'), true), (None, None));
        assert_eq!(r.key(key, true), (Some(Action::Start), Some(true)));
        assert_eq!(r.key(key, true), (None, None), "a repeat");
        assert_eq!(r.key(Key::Char('a'), false), (None, None));
    }

    #[test]
    fn a_press_during_the_wait_is_still_held_when_its_dictation_starts() {
        let key = Shortcut::default_for_platform().key;
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        r.press(key);
        r.set_dictating(true);
        r.release(key);
        assert!(!r.is_held());
        assert_eq!(r.press(key), Some(Action::Start), "the next dictation");
        assert!(r.is_held());
        r.set_dictating(false);
        assert!(r.is_held(), "the next dictation's press outlasts this one");
        r.set_dictating(true);
        assert_eq!(r.release(key), Some(Action::Stop));
        assert!(!r.is_held());
    }

    #[test]
    fn a_toggle_press_is_never_held() {
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        assert!(r.is_active());
        assert!(!r.is_held());
    }

    #[test]
    fn forgetting_keys_lets_go_of_a_trigger_that_came_up_and_keeps_the_dictation() {
        let key = Shortcut::default_for_platform().key;
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        assert_eq!(r.key(key, true), (Some(Action::Start), Some(true)));
        r.set_dictating(true);
        // The tap was off; the key-up went by unseen and nothing is down.
        assert_eq!(r.forget_keys(&[]), Some(false), "the watch hears it");
        assert!(!r.is_held());
        assert!(r.is_active(), "a gap in the tap does not end a dictation");
        assert_eq!(r.forget_keys(&[]), None, "nothing more to change");
        // The trigger's next press and release stop it, starting nothing.
        assert_eq!(r.key(key, true), (None, Some(true)));
        assert_eq!(r.key(key, false), (Some(Action::Stop), Some(false)));
        r.set_dictating(false);
        // And the one after that starts as ever.
        assert_eq!(r.key(key, true), (Some(Action::Start), Some(true)));
        assert_eq!(r.key(key, false), (Some(Action::Stop), Some(false)));
    }

    #[test]
    fn forgetting_keys_keeps_a_trigger_that_is_still_down() {
        let key = Shortcut::default_for_platform().key;
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        assert_eq!(r.key(key, true), (Some(Action::Start), Some(true)));
        r.set_dictating(true);
        assert_eq!(r.forget_keys(&[key]), None, "still held; nothing to tell");
        assert!(r.is_held());
        assert_eq!(r.key(key, false), (Some(Action::Stop), Some(false)));
    }

    #[test]
    fn forgetting_keys_never_adds_a_key_pressed_in_the_gap() {
        let key = Shortcut::default_for_platform().key;
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        assert_eq!(r.forget_keys(&[key]), None);
        assert!(!r.is_held());
        assert!(!r.is_active());
    }

    #[test]
    fn forgetting_keys_with_nothing_held_starts_and_ends_nothing() {
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        assert_eq!(r.forget_keys(&[]), None);
        assert!(!r.is_active());
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        assert_eq!(r.forget_keys(&[]), None, "a toggle press is never held");
        assert!(r.is_active(), "the toggled dictation keeps running");
        // The keys came up in the gap; only the modifiers' ups are seen.
        assert_eq!(r.release(Key::Mod(Modifier::Command, Side::Left)), None);
        assert_eq!(r.release(Key::Mod(Modifier::Shift, Side::Left)), None);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Stop),
            "the next press is a press, not a repeat of the forgotten one"
        );
        assert!(!r.is_active());
    }

    #[test]
    fn forgetting_keys_forgets_the_modifiers_that_came_up() {
        let mut r = cmd_shift_d(Mode::Toggle);
        r.press(Key::Mod(Modifier::Command, Side::Left));
        r.press(Key::Mod(Modifier::Shift, Side::Left));
        r.forget_keys(&[]);
        assert_eq!(
            r.press(Key::Char('d')),
            None,
            "their key-ups went by unseen"
        );
        r.release(Key::Char('d'));
        assert_eq!(press_combo(&mut r), Some(Action::Start));
    }

    #[test]
    fn forgetting_keys_keeps_the_modifiers_still_down() {
        let mut r = cmd_shift_d(Mode::Toggle);
        r.press(Key::Mod(Modifier::Command, Side::Left));
        r.press(Key::Mod(Modifier::Shift, Side::Left));
        r.forget_keys(&[
            Key::Mod(Modifier::Command, Side::Left),
            Key::Mod(Modifier::Shift, Side::Left),
        ]);
        assert_eq!(r.press(Key::Char('d')), Some(Action::Start));
    }

    #[test]
    fn forgetting_keys_disarms_esc_but_a_dictation_can_still_be_discarded() {
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None);
        r.forget_keys(&[]);
        assert_eq!(r.press(Key::Esc), None, "arms again; does not discard");
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
    }

    #[cfg(all(feature = "hotkey", target_os = "macos"))]
    mod listener_events {
        use super::super::*;
        use crate::tap::{RawKey, TapEvent};

        const D: u16 = 2;

        fn bare_d(mode: Mode) -> Recogniser {
            Recogniser::new(
                Shortcut {
                    modifiers: BTreeSet::new(),
                    key: Key::Char('d'),
                },
                mode,
            )
        }

        fn down(code: u16, repeat: bool) -> TapEvent {
            TapEvent::Key(RawKey::Down { code, repeat })
        }

        #[test]
        fn keys_lost_lets_go_of_a_trigger_that_is_no_longer_down() {
            let right_option = keycode::OPTION_RIGHT;
            let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
            assert_eq!(
                handle(&mut r, down(right_option, false)),
                (Some(Action::Start), Some(true))
            );
            assert_eq!(
                handle(
                    &mut r,
                    TapEvent::KeysLost {
                        still_down: vec![D]
                    }
                ),
                (None, Some(false))
            );
            assert!(r.is_active(), "the dictation is kept");
        }

        #[test]
        fn keys_lost_keeps_a_trigger_that_is_still_down() {
            let right_option = keycode::OPTION_RIGHT;
            let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
            handle(&mut r, down(right_option, false));
            assert_eq!(
                handle(
                    &mut r,
                    TapEvent::KeysLost {
                        still_down: vec![right_option]
                    }
                ),
                (None, None)
            );
            assert_eq!(
                handle(&mut r, TapEvent::Key(RawKey::Up(right_option))),
                (Some(Action::Stop), Some(false))
            );
        }

        #[test]
        fn a_repeated_trigger_after_keys_lost_does_not_toggle() {
            let mut r = bare_d(Mode::Toggle);
            assert_eq!(handle(&mut r, down(D, false)).0, Some(Action::Start));
            handle(&mut r, TapEvent::KeysLost { still_down: vec![] });
            assert_eq!(handle(&mut r, down(D, true)), (None, None), "no Stop");
            assert!(r.is_active());
        }

        #[test]
        fn a_repeated_trigger_after_esc_twice_and_keys_lost_starts_nothing() {
            let mut r = bare_d(Mode::Hold);
            assert_eq!(handle(&mut r, down(D, false)).0, Some(Action::Start));
            r.set_dictating(true);
            handle(&mut r, down(keycode::ESCAPE, false));
            handle(&mut r, TapEvent::Key(RawKey::Up(keycode::ESCAPE)));
            assert_eq!(
                handle(&mut r, down(keycode::ESCAPE, false)).0,
                Some(Action::Discard)
            );
            handle(&mut r, TapEvent::KeysLost { still_down: vec![] });
            assert_eq!(handle(&mut r, down(D, true)), (None, None), "no Start");
            assert!(!r.is_active());
        }

        #[test]
        fn a_fresh_trigger_press_is_not_dropped() {
            let mut r = bare_d(Mode::Hold);
            assert_eq!(
                handle(&mut r, down(D, false)),
                (Some(Action::Start), Some(true))
            );
            assert_eq!(handle(&mut r, down(D, true)), (None, None), "a repeat");
        }
    }

    #[test]
    fn a_dictation_that_ended_elsewhere_disarms_esc() {
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        r.set_dictating(false);
        assert_eq!(r.press(Key::Esc), None, "nothing to discard");
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), None);
    }

    #[test]
    fn esc_twice_after_a_hold_release_discards_a_dictation_still_running() {
        // With a cloud engine the key comes up, the recording goes to the
        // provider, and the dictation runs on while it waits. Esc twice must
        // still reach it.
        let mut r = cmd_shift_d(Mode::Hold);
        r.set_dictating(true);
        press_combo(&mut r);
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), None, "discarded once, not again");
    }

    #[test]
    fn esc_twice_after_a_toggle_stop_discards_a_dictation_still_running() {
        // The order the CLI sees in toggle mode: a press starts it, the app
        // says it is running, the next press stops the recording, and the
        // wait goes on.
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(press_combo(&mut r), Some(Action::Stop));
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
    }

    #[test]
    fn a_quick_tap_released_before_the_app_says_it_is_dictating_leaves_the_trigger_free() {
        // The CLI can hear the Start after the key is already up, and only
        // then say a dictation is running. A second tap while that dictation
        // finishes is a new gesture and must still start the next one.
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        r.set_dictating(true);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
    }

    #[test]
    fn a_hold_begun_during_the_wait_and_still_down_when_it_ends_stops_the_next_dictation() {
        // The apps start a press heard during the wait after release once
        // that dictation is over. The key is often still down then, and its
        // release is the only thing that can stop the dictation it started.
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        assert_eq!(press_combo(&mut r), Some(Action::Start), "during the wait");
        r.set_dictating(false);
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        r.set_dictating(false);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Start),
            "and the one after"
        );
    }

    #[test]
    fn a_toggle_begun_during_the_wait_is_stopped_by_the_next_press() {
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(press_combo(&mut r), Some(Action::Stop));
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(press_combo(&mut r), Some(Action::Start), "during the wait");
        assert_eq!(r.release(Key::Char('d')), None);
        r.set_dictating(false);
        r.set_dictating(true);
        assert_eq!(press_combo(&mut r), Some(Action::Stop));
    }

    #[test]
    fn a_dictation_that_ended_with_its_own_key_still_down_leaves_the_next_press_free() {
        // Ended by the cap, or by the tray, while the key that started it is
        // held: that gesture was this dictation's, so it ends with it.
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        r.set_dictating(false);
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
    }

    #[test]
    fn a_press_after_esc_twice_is_the_next_dictation_and_its_release_stops_it() {
        // Esc twice, let go, and press again before the app has finished
        // with the discarded dictation: that press starts the next one.
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.release(Key::Char('d')), None, "the discarded one's key");
        assert_eq!(press_combo(&mut r), Some(Action::Start), "after Esc twice");
        r.set_dictating(false);
        r.set_dictating(true);
        assert_eq!(
            r.release(Key::Char('d')),
            Some(Action::Stop),
            "not left recording until the cap"
        );
    }

    #[test]
    fn ending_a_dictation_says_whether_esc_twice_discarded_it() {
        let mut r = cmd_shift_d(Mode::Hold);
        r.set_dictating(true);
        assert!(!r.set_dictating(false), "kept");
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert!(
            r.set_dictating(false),
            "discarded, wherever the Discard has got to"
        );
        assert!(!r.set_dictating(false), "said once");
    }

    #[test]
    fn a_press_before_esc_twice_goes_with_the_dictation() {
        // Let go, press during the wait, then Esc twice with that key still
        // down: the press was made before Esc twice, so it is thrown away
        // too, and its release stops nothing.
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        assert_eq!(press_combo(&mut r), Some(Action::Start), "during the wait");
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.release(Key::Char('d')), None, "nothing to stop");
        r.set_dictating(false);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Start),
            "the next hold works"
        );
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
    }

    #[test]
    fn in_toggle_mode_a_press_after_esc_twice_is_the_next_dictation_and_the_next_press_stops_it() {
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(press_combo(&mut r), Some(Action::Start), "after Esc twice");
        assert_eq!(r.release(Key::Char('d')), None);
        r.set_dictating(false);
        r.set_dictating(true);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Stop),
            "the next press stops it, not starts another"
        );
    }

    #[test]
    fn in_toggle_mode_a_press_before_esc_twice_goes_with_the_dictation() {
        let mut r = cmd_shift_d(Mode::Toggle);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Stop),
            "the recording ends"
        );
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(press_combo(&mut r), Some(Action::Start), "during the wait");
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert_eq!(r.release(Key::Esc), None);
        r.set_dictating(false);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Start),
            "a press after the end starts a dictation, not stops one"
        );
    }

    #[test]
    fn a_dictation_the_tray_started_is_still_stopped_by_the_trigger() {
        // The app ignores a Start while it is recording; the release is what
        // stops the tray's dictation.
        let mut r = cmd_shift_d(Mode::Hold);
        r.set_dictating(true);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        r.set_dictating(false);
        assert!(!r.is_active());
    }

    #[test]
    fn a_push_to_talk_dictation_waiting_after_release_can_be_discarded_and_the_next_hold_works() {
        // The order the apps see: the press starts it, the app says it is
        // running, the release stops the recording, and the wait goes on.
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert_eq!(r.release(Key::Esc), None);
        r.set_dictating(false);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Start),
            "the next hold works"
        );
    }

    #[test]
    fn esc_twice_while_dictating_discards_it() {
        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        assert_eq!(r.press(Key::Esc), None, "one Esc is a pause for thought");
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert!(!r.is_active());
        // The trigger is still physically down; its release is not a Stop.
        assert_eq!(r.release(Key::Char('d')), None);
        assert_eq!(
            press_combo(&mut r),
            Some(Action::Start),
            "the next hold works"
        );
    }

    /// What `Listener::start` does before handing a key-down to `press`:
    /// drop it outright if `is_droppable_repeat` says so, otherwise deliver
    /// it unchanged. Used by the tests below to exercise the recogniser the
    /// way production actually feeds it, without a tap.
    fn feed_esc_down(repeat: bool, r: &mut Recogniser) -> Option<Action> {
        if is_droppable_repeat(Key::Esc, repeat) {
            None
        } else {
            r.press(Key::Esc)
        }
    }

    #[test]
    fn holding_esc_down_is_one_press_however_long_it_repeats() {
        // Auto-repeat delivers press after press while a key is held; an
        // anxious pause on Escape is one press, not several. The recogniser
        // no longer collapses this itself (see the comment on `press`) --
        // the listener drops every repeat-flagged Esc before `press` ever
        // sees it, which `feed_esc_down` stands in for here.
        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        assert_eq!(feed_esc_down(false, &mut r), None);
        assert_eq!(
            feed_esc_down(true, &mut r),
            None,
            "repeat, dropped before it reaches press"
        );
        assert_eq!(feed_esc_down(true, &mut r), None);
        assert!(r.is_active());
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(
            feed_esc_down(false, &mut r),
            Some(Action::Discard),
            "the real second press"
        );
    }

    #[test]
    fn a_repeat_flagged_esc_down_between_two_real_esc_downs_does_not_change_the_outcome() {
        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        assert_eq!(feed_esc_down(false, &mut r), None, "first real Esc arms it");
        assert_eq!(
            feed_esc_down(true, &mut r),
            None,
            "a repeat in between changes nothing"
        );
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(
            feed_esc_down(false, &mut r),
            Some(Action::Discard),
            "the real second press"
        );
        assert!(!r.is_active());
    }

    #[test]
    fn a_repeat_flagged_esc_alone_never_counts_as_a_press() {
        assert!(is_droppable_repeat(Key::Esc, true));
        assert!(!is_droppable_repeat(Key::Esc, false));
        assert!(
            !is_droppable_repeat(Key::Char('d'), true),
            "a repeat of the trigger is the recogniser's own job, via trigger_held"
        );

        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        assert_eq!(
            feed_esc_down(true, &mut r),
            None,
            "a repeat with nothing real before it is never delivered at all"
        );
        assert!(
            r.is_active(),
            "not disarmed and not discarded -- press never saw it"
        );
    }

    #[test]
    fn a_lost_esc_key_up_does_not_disable_esc_twice_for_the_next_tray_dictation() {
        // Esc down with no matching key-up -- Secure Event Input switching
        // on mid-press, or the tap disabled for a moment. The dictation
        // ends and a new one starts; Esc twice in the new one must still
        // discard, which the old `esc_held` latch could not guarantee: it
        // was cleared by a key-up or by `reset()`, neither of which
        // `set_dictating` calls.
        let mut r = Recogniser::new(Shortcut::default_for_platform(), Mode::Hold);
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None, "arms; its key-up never arrives");
        r.set_dictating(false);

        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None, "one Esc is a pause for thought");
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert!(!r.is_active());
    }

    #[test]
    fn a_lost_esc_key_up_does_not_disable_esc_twice_for_the_next_hotkey_dictation() {
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(press_combo(&mut r), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None, "arms; its key-up never arrives");
        assert_eq!(r.release(Key::Char('d')), Some(Action::Stop));
        r.set_dictating(false);

        assert_eq!(
            press_combo(&mut r),
            Some(Action::Start),
            "the next hold works"
        );
        r.set_dictating(true);
        assert_eq!(r.press(Key::Esc), None, "one Esc is a pause for thought");
        assert_eq!(r.press(Key::Esc), Some(Action::Discard));
        assert!(!r.is_active());
    }

    #[test]
    fn esc_with_another_key_between_is_two_ordinary_escapes() {
        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Mod(Modifier::Shift, Side::Either)), None);
        assert_eq!(r.press(Key::Esc), None, "disarmed by the key between");
        assert!(r.is_active());
    }

    #[test]
    fn esc_while_idle_does_nothing_and_does_not_arm() {
        let mut r = cmd_shift_d(Mode::Hold);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        assert_eq!(r.press(Key::Esc), None);
        assert_eq!(r.release(Key::Esc), None);
        press_combo(&mut r);
        assert_eq!(
            r.press(Key::Esc),
            None,
            "escapes before the start do not count"
        );
        assert!(r.is_active());
    }

    #[test]
    fn esc_cannot_be_the_shortcut() {
        assert!(Shortcut::parse("esc").is_err());
        assert!(Shortcut::parse("cmd+escape").is_err());
    }

    #[test]
    fn a_reset_stops_a_dictation_whose_release_never_arrived() {
        // The listener restarted, the screen locked, the permission was
        // revoked. Without this, hold mode records until the disk fills.
        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        assert_eq!(r.reset(), Some(Action::Stop));
        assert!(!r.is_active());
        assert_eq!(r.reset(), None, "nothing to stop the second time");
    }

    #[test]
    fn a_reset_clears_the_held_keys_so_the_next_press_still_works() {
        let mut r = cmd_shift_d(Mode::Hold);
        press_combo(&mut r);
        r.reset();
        assert_eq!(press_combo(&mut r), Some(Action::Start));
    }

    #[test]
    fn the_platform_default_is_right_option_on_macos_and_right_ctrl_elsewhere() {
        // As amended 2026-09-09: Right Option on macOS, Right
        // Ctrl elsewhere.
        let s = Shortcut::default_for_platform();
        if cfg!(target_os = "macos") {
            assert_eq!(s.key, Key::Mod(Modifier::Alt, Side::Right));
        } else {
            assert_eq!(s.key, Key::Mod(Modifier::Control, Side::Right));
        }
    }

    #[test]
    fn shortcuts_parse_from_the_form_a_settings_file_would_hold() {
        assert_eq!(
            Shortcut::parse("cmd+shift+d").expect("parse"),
            Shortcut {
                modifiers: [Modifier::Command, Modifier::Shift].into_iter().collect(),
                key: Key::Char('d'),
            }
        );
        // Spelling, order and spacing must not matter.
        assert_eq!(
            Shortcut::parse("Shift + Command + D").expect("parse"),
            Shortcut::parse("cmd+shift+d").expect("parse")
        );
        assert_eq!(
            Shortcut::parse("ctrl+alt+space").expect("parse").key,
            Key::Char(' ')
        );
        assert_eq!(Shortcut::parse("fn").expect("parse").key, Key::Fn);
        assert_eq!(
            Shortcut::parse("option").expect("parse").key,
            Key::Mod(Modifier::Alt, Side::Either),
            "a bare modifier is a legal shortcut"
        );
    }

    #[test]
    fn an_unparsable_shortcut_says_what_it_did_not_understand() {
        let err = Shortcut::parse("cmd+wibble").expect_err("wibble is not a key");
        assert!(err.to_string().contains("wibble"), "got: {err}");
        assert!(
            matches!(err, crate::Error::Shortcut(_)),
            "a bad shortcut is a Shortcut error, not a permission problem: {err:?}"
        );
        assert!(Shortcut::parse("").is_err(), "an empty shortcut is not one");
        assert!(
            Shortcut::parse("cmd+shift").is_err(),
            "modifiers with no trigger cannot fire"
        );
    }
}
