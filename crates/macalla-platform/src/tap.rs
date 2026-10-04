// SPDX-License-Identifier: MIT
//! A listen-only keyboard event tap for macOS: keycodes and flags, nothing
//! else.
//!
//! This replaced `rdev` on 2026-09-09 after the app crashed on its first
//! keystroke. rdev's tap callback translates every key into a character
//! through HIToolbox (`TISGetInputSourceProperty`), and macOS 26 asserts that
//! call is made on the main dispatch queue; a tap on its own thread trips
//! the assertion and the process is trapped. Macalla never needed the
//! character -- the recogniser works on which key, not what it types -- so
//! the fix is not to ask. The CLI had not hit it only because a process
//! with no AppKit run loop takes a different path through HIToolbox.
//!
//! Left and right modifiers arrive as `FlagsChanged`, with the keycode of the
//! key that changed and the flags as they now are. Whether that is a press or
//! a release is worked out here, per keycode, so two Shifts held together are
//! still two keys.

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CallbackResult, EventField,
};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

// core-graphics 0.25 wraps `CGEventTapEnable` only as a method on the tap it
// returns, and the callback that must call it cannot hold that tap; it does
// not wrap `CGEventSourceKeyState` or `CGEventSourceFlagsState` at all. All
// three are public CoreGraphics functions, in the framework the crate
// already links. From the SDK's CGEventSource.h:
//   bool CGEventSourceKeyState(CGEventSourceStateID stateID, CGKeyCode key)
//   CGEventFlags CGEventSourceFlagsState(CGEventSourceStateID stateID)
// where CGEventSourceStateID is an `int32_t` enum and CGEventFlags a
// `uint64_t` options type (CGEventTypes.h), and CGKeyCode is `uint16_t`.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventSourceKeyState(state_id: i32, key: u16) -> bool;
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
}

/// `kCGEventSourceStateHIDSystemState` (CGEventTypes.h): the keyboard as the
/// hardware reports it, whichever app has focus.
const HID_SYSTEM_STATE: i32 = 1;

/// Every macOS virtual keycode; the highest defined is 0x7E.
const KEYCODES: std::ops::RangeInclusive<u16> = 0..=0x7F;

/// Whether an ordinary (non-modifier) key is physically down right now.
///
/// Not asked about modifiers: on macOS 26.7 it reported a held Right Option
/// as up, in both the HID and the combined session state, while the flags
/// state had Option set. Modifiers go by [`modifier_flags_now`] instead.
fn physically_down(code: u16) -> bool {
    // SAFETY: takes a state id and a keycode by value, returns a bool, and
    // reads only; any keycode is valid input.
    unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, code) }
}

/// The modifier flags as the hardware reports them right now, including the
/// device-dependent left/right bits (see [`modifier_still_down`]).
fn modifier_flags_now() -> u64 {
    // SAFETY: takes a state id by value, returns an integer, reads only.
    unsafe { CGEventSourceFlagsState(HID_SYSTEM_STATE) }
}

/// The device-dependent bit of the flags that says one side's modifier is
/// down, from IOKit's IOLLEvent.h (`NX_DEVICELCTLKEYMASK` and its siblings).
/// Fn and Caps Lock have no side, so no bit.
fn device_bit(code: u16) -> Option<u64> {
    use crate::hotkey::keycode as k;
    Some(match code {
        k::CONTROL_LEFT => 0x0000_0001,
        k::SHIFT_LEFT => 0x0000_0002,
        k::SHIFT_RIGHT => 0x0000_0004,
        k::COMMAND_LEFT => 0x0000_0008,
        k::COMMAND_RIGHT => 0x0000_0010,
        k::OPTION_LEFT => 0x0000_0020,
        k::OPTION_RIGHT => 0x0000_0040,
        k::CONTROL_RIGHT => 0x0000_2000,
        _ => return None,
    })
}

/// Whether a modifier the tap saw go down is still down, from a flags value
/// read after the gap. Fn and Caps Lock, with no side bit to tell, are taken
/// to be still down: a wrong "up" would end a hold dictation whose key is
/// still held, while a wrong "down" only waits for the key's next change,
/// which is then delivered as its release.
fn modifier_still_down(code: u16, flags: u64) -> bool {
    device_bit(code).is_none_or(|bit| flags & bit != 0)
}

/// What the tap delivers. Keycodes are macOS virtual keycodes, the same on
/// every keyboard layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKey {
    /// `repeat` is macOS's own `KEYBOARD_EVENT_AUTOREPEAT` field: true when
    /// this key-down is the OS re-firing an already-held key, not a fresh
    /// press. A modifier transition (see `modifier_transition`) is never a
    /// repeat -- `FlagsChanged` fires once per state change, not on a timer
    /// -- so it always carries `false`.
    Down {
        code: u16,
        repeat: bool,
    },
    Up(u16),
}

/// What the tap hands the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapEvent {
    Key(RawKey),
    /// The system had switched the tap off and it has just been switched
    /// back on. Keys went by unseen in between, so whatever is remembered as
    /// held may no longer be. Modifiers seen going down that have since come
    /// up were already delivered as [`RawKey::Up`] just before this.
    KeysLost {
        /// The keycodes still physically down that the tap will deliver a
        /// key-up for: modifiers it saw go down, and any plain key. Anything
        /// remembered as held and not in here is no longer down.
        still_down: Vec<u16>,
    },
}

/// Run the tap on the current thread until the run loop stops, which it does
/// not on its own -- this is the listener thread's whole life.
///
/// Fails when the tap cannot be created, which on macOS means Input
/// Monitoring has not been granted to this process.
pub fn listen(on_event: impl Fn(TapEvent) + Send + 'static) -> Result<(), String> {
    // The tap's own port, for switching it back on from inside its callback,
    // which cannot hold the tap it belongs to: the tap is only returned once
    // the callback has been handed over. Set right after creation, before
    // the run loop starts, so every callback that can need it finds it.
    // Stored as an address: the tap below outlives every call of the
    // callback, which only ever runs on this thread's run loop.
    let port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
    let tap_port = port.clone();
    let held: Mutex<HashSet<u16>> = Mutex::new(HashSet::new());
    let callback = move |_proxy: CGEventTapProxy, kind: CGEventType, event: &CGEvent| {
        match kind {
            CGEventType::KeyDown | CGEventType::KeyUp => {
                let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                let autorepeat =
                    event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT);
                if let Some(raw) = key_event(kind, code, autorepeat) {
                    on_event(TapEvent::Key(raw));
                }
            }
            CGEventType::FlagsChanged => {
                let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                let raw = {
                    let mut held = held.lock().unwrap_or_else(|p| p.into_inner());
                    modifier_transition(&mut held, code, event.get_flags())
                };
                if let Some(raw) = raw {
                    on_event(TapEvent::Key(raw));
                }
            }
            // The system switches a tap off when its callback is too slow,
            // or when the user's input asks it to (secure input, for one),
            // and it stays off until the app switches it back on: nothing
            // else does, and no key reaches us until then. Keys went by
            // unseen meanwhile, so what we think is held is stale.
            kind if was_disabled(kind) => {
                if let Some(&address) = tap_port.get() {
                    // SAFETY: the port belongs to the tap `listen` keeps
                    // alive for as long as this callback can run.
                    unsafe { CGEventTapEnable(address as CFMachPortRef, true) };
                }
                let (ups, still_down) = {
                    let mut held = held.lock().unwrap_or_else(|p| p.into_inner());
                    after_gap(&mut held, modifier_flags_now(), physically_down)
                };
                tracing::warn!(
                    by_timeout = matches!(kind, CGEventType::TapDisabledByTimeout),
                    "the system switched the keyboard tap off; switched it back on"
                );
                // A release missed in the gap is a release: a held trigger
                // that came up still stops its dictation.
                for code in ups {
                    on_event(TapEvent::Key(RawKey::Up(code)));
                }
                on_event(TapEvent::KeysLost { still_down });
            }
            _ => {}
        }
        CallbackResult::Keep
    };
    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ],
        callback,
    )
    .map_err(|()| {
        "the event tap could not be created; on macOS this is the Input Monitoring permission"
            .to_string()
    })?;
    let _ = port.set(tap.mach_port().as_concrete_TypeRef() as usize);
    let source = tap
        .mach_port()
        .create_runloop_source(0)
        .map_err(|()| "the event tap's run loop source could not be created".to_string())?;
    // SAFETY: an immutable CoreFoundation constant.
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    // The first key-state or flags lookup in a process may ask the window
    // server; pay that here rather than inside the callback after a gap.
    let _ = physically_down(0);
    let _ = modifier_flags_now();
    CFRunLoop::run_current();
    drop(tap);
    Ok(())
}

/// After a gap in the tap: which held modifiers came up unseen, and which
/// keys are still down. Modifiers are judged by `flags`, read once after the
/// gap (see [`modifier_still_down`]); ordinary keys by `key_down`.
///
/// A modifier still down stays in `held`, so its release later is still a
/// release; one that came up is removed and returned, in keycode order, to
/// be delivered as a key-up. `still_down` lists what the tap will deliver a
/// key-up for later: the modifiers left in `held`, and plain keys that are
/// down (their key-up needs no memory here). A modifier pressed during the
/// gap is not in it, because its release would find nothing in `held`.
fn after_gap(
    held: &mut HashSet<u16>,
    flags: u64,
    key_down: impl Fn(u16) -> bool,
) -> (Vec<u16>, Vec<u16>) {
    let mut ups: Vec<u16> = held
        .iter()
        .copied()
        .filter(|&c| !modifier_still_down(c, flags))
        .collect();
    ups.sort_unstable();
    for code in &ups {
        held.remove(code);
    }
    let still_down = KEYCODES
        .filter(|&c| {
            if flag_for(c).is_some() {
                held.contains(&c)
            } else {
                key_down(c)
            }
        })
        .collect();
    (ups, still_down)
}

/// Whether the system is telling the callback it switched the tap off.
fn was_disabled(kind: CGEventType) -> bool {
    matches!(
        kind,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    )
}

/// A plain (non-modifier) key-down or key-up, mapped from the event kind,
/// the keycode and macOS's own autorepeat field. Pure, so it is checked
/// without an event tap, next to [`modifier_transition`].
fn key_event(kind: CGEventType, code: u16, autorepeat: i64) -> Option<RawKey> {
    match kind {
        CGEventType::KeyDown => Some(RawKey::Down {
            code,
            repeat: autorepeat != 0,
        }),
        CGEventType::KeyUp => Some(RawKey::Up(code)),
        _ => None,
    }
}

/// A modifier key changed. Press or release, from what we already hold and
/// what the flags say about that key's class.
///
/// Per keycode, not per flag: with both Shifts down the Shift bit stays set
/// when one comes up, and only the set of held keycodes can tell which.
fn modifier_transition(held: &mut HashSet<u16>, code: u16, flags: CGEventFlags) -> Option<RawKey> {
    if held.remove(&code) {
        return Some(RawKey::Up(code));
    }
    let class = flag_for(code)?;
    if flags.contains(class) {
        held.insert(code);
        // A modifier transition is never an autorepeat: FlagsChanged fires
        // once per state change, not on a timer.
        Some(RawKey::Down {
            code,
            repeat: false,
        })
    } else {
        // A release for a key we never saw go down -- a missed event, or a
        // key held before the tap started. Not a press.
        None
    }
}

/// The flag bit a modifier keycode moves.
fn flag_for(code: u16) -> Option<CGEventFlags> {
    use crate::hotkey::keycode as k;
    Some(match code {
        k::SHIFT_LEFT | k::SHIFT_RIGHT => CGEventFlags::CGEventFlagShift,
        k::CONTROL_LEFT | k::CONTROL_RIGHT => CGEventFlags::CGEventFlagControl,
        k::OPTION_LEFT | k::OPTION_RIGHT => CGEventFlags::CGEventFlagAlternate,
        k::COMMAND_LEFT | k::COMMAND_RIGHT => CGEventFlags::CGEventFlagCommand,
        k::FN => CGEventFlags::CGEventFlagSecondaryFn,
        k::CAPS_LOCK => CGEventFlags::CGEventFlagAlphaShift,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::keycode as k;

    #[test]
    fn a_modifier_going_down_then_up_is_a_press_and_a_release() {
        let mut held = HashSet::new();
        let shift = CGEventFlags::CGEventFlagShift;
        assert_eq!(
            modifier_transition(&mut held, k::SHIFT_LEFT, shift),
            Some(RawKey::Down {
                code: k::SHIFT_LEFT,
                repeat: false
            })
        );
        assert_eq!(
            modifier_transition(&mut held, k::SHIFT_LEFT, CGEventFlags::CGEventFlagNull),
            Some(RawKey::Up(k::SHIFT_LEFT))
        );
    }

    #[test]
    fn two_shifts_are_two_keys_even_though_they_share_a_flag() {
        let mut held = HashSet::new();
        let shift = CGEventFlags::CGEventFlagShift;
        modifier_transition(&mut held, k::SHIFT_LEFT, shift);
        assert_eq!(
            modifier_transition(&mut held, k::SHIFT_RIGHT, shift),
            Some(RawKey::Down {
                code: k::SHIFT_RIGHT,
                repeat: false
            })
        );
        // Right comes up; the Shift bit is still set because Left is down.
        assert_eq!(
            modifier_transition(&mut held, k::SHIFT_RIGHT, shift),
            Some(RawKey::Up(k::SHIFT_RIGHT))
        );
        assert_eq!(
            modifier_transition(&mut held, k::SHIFT_LEFT, CGEventFlags::CGEventFlagNull),
            Some(RawKey::Up(k::SHIFT_LEFT))
        );
    }

    #[test]
    fn a_release_for_a_key_never_seen_down_is_not_a_press() {
        let mut held = HashSet::new();
        assert_eq!(
            modifier_transition(&mut held, k::OPTION_RIGHT, CGEventFlags::CGEventFlagNull),
            None
        );
    }

    #[test]
    fn the_fn_key_is_a_modifier_here() {
        let mut held = HashSet::new();
        assert_eq!(
            modifier_transition(&mut held, k::FN, CGEventFlags::CGEventFlagSecondaryFn),
            Some(RawKey::Down {
                code: k::FN,
                repeat: false
            })
        );
    }

    #[test]
    fn a_plain_key_down_carries_whatever_autorepeat_macos_reported() {
        assert_eq!(
            key_event(CGEventType::KeyDown, k::ESCAPE, 0),
            Some(RawKey::Down {
                code: k::ESCAPE,
                repeat: false
            })
        );
        assert_eq!(
            key_event(CGEventType::KeyDown, k::ESCAPE, 1),
            Some(RawKey::Down {
                code: k::ESCAPE,
                repeat: true
            })
        );
    }

    #[test]
    fn a_plain_key_up_has_no_repeat_field_to_carry() {
        assert_eq!(
            key_event(CGEventType::KeyUp, k::ESCAPE, 1),
            Some(RawKey::Up(k::ESCAPE)),
            "macOS does not flag key-up as a repeat, but even if it did, \
             releases are not deduplicated the way key-downs are"
        );
    }

    #[test]
    fn key_event_ignores_event_kinds_it_is_not_asked_about() {
        assert_eq!(
            key_event(CGEventType::FlagsChanged, k::SHIFT_LEFT, 0),
            None,
            "modifier transitions go through modifier_transition, not this"
        );
    }

    #[test]
    fn both_ways_the_system_switches_a_tap_off_are_recognised() {
        assert!(was_disabled(CGEventType::TapDisabledByTimeout));
        assert!(was_disabled(CGEventType::TapDisabledByUserInput));
        assert!(!was_disabled(CGEventType::KeyDown));
        assert!(!was_disabled(CGEventType::KeyUp));
        assert!(!was_disabled(CGEventType::FlagsChanged));
        assert!(!was_disabled(CGEventType::Null));
    }

    /// Flags read on a Mac with Right Option held, Left not.
    const RIGHT_OPTION_HELD: u64 = 0x80140;
    /// And with Left Option held, Right not.
    const LEFT_OPTION_HELD: u64 = 0x80120;

    #[test]
    fn the_option_side_comes_from_the_device_bits() {
        assert!(modifier_still_down(k::OPTION_RIGHT, RIGHT_OPTION_HELD));
        assert!(!modifier_still_down(k::OPTION_LEFT, RIGHT_OPTION_HELD));
        assert!(modifier_still_down(k::OPTION_LEFT, LEFT_OPTION_HELD));
        assert!(!modifier_still_down(k::OPTION_RIGHT, LEFT_OPTION_HELD));
    }

    #[test]
    fn every_sided_modifier_has_its_own_device_bit() {
        for (code, bit) in [
            (k::CONTROL_LEFT, 0x01),
            (k::SHIFT_LEFT, 0x02),
            (k::SHIFT_RIGHT, 0x04),
            (k::COMMAND_LEFT, 0x08),
            (k::COMMAND_RIGHT, 0x10),
            (k::OPTION_LEFT, 0x20),
            (k::OPTION_RIGHT, 0x40),
            (k::CONTROL_RIGHT, 0x2000),
        ] {
            assert!(modifier_still_down(code, bit), "{code} with its bit");
            assert!(
                !modifier_still_down(code, !bit & 0x3fff),
                "{code} with every other device bit"
            );
        }
    }

    #[test]
    fn fn_and_caps_lock_are_kept_held_with_no_side_bit_to_ask() {
        assert!(modifier_still_down(k::FN, 0));
        assert!(modifier_still_down(k::CAPS_LOCK, 0));
    }

    #[test]
    fn after_a_gap_a_modifier_that_came_up_is_released_and_one_still_down_is_kept() {
        let mut held: HashSet<u16> = [k::OPTION_RIGHT, k::SHIFT_LEFT].into_iter().collect();
        // Left Shift still down (device bit 0x02), Right Option not.
        let (ups, still_down) = after_gap(&mut held, 0x20102, |c| c == 2);
        assert_eq!(ups, vec![k::OPTION_RIGHT]);
        assert_eq!(held, [k::SHIFT_LEFT].into_iter().collect());
        assert_eq!(
            still_down,
            vec![2, k::SHIFT_LEFT],
            "a plain key and the kept modifier"
        );
    }

    #[test]
    fn after_a_gap_a_held_right_option_is_kept_by_its_flag() {
        let mut held: HashSet<u16> = [k::OPTION_RIGHT].into_iter().collect();
        // The key-state lookup says every key is up, as it did for a held
        // Right Option; the flags decide.
        let (ups, still_down) = after_gap(&mut held, RIGHT_OPTION_HELD, |_| false);
        assert!(ups.is_empty());
        assert_eq!(still_down, vec![k::OPTION_RIGHT]);
        // And its release after the gap is still a release.
        assert_eq!(
            modifier_transition(&mut held, k::OPTION_RIGHT, CGEventFlags::CGEventFlagNull),
            Some(RawKey::Up(k::OPTION_RIGHT))
        );
    }

    #[test]
    fn after_a_gap_the_other_option_held_does_not_keep_this_one() {
        let mut held: HashSet<u16> = [k::OPTION_RIGHT].into_iter().collect();
        let (ups, still_down) = after_gap(&mut held, LEFT_OPTION_HELD, |_| true);
        assert_eq!(ups, vec![k::OPTION_RIGHT]);
        assert!(held.is_empty());
        assert!(
            !still_down.contains(&k::OPTION_RIGHT) && !still_down.contains(&k::OPTION_LEFT),
            "modifiers never come from the key-state lookup"
        );
    }

    #[test]
    fn after_a_gap_fn_and_caps_lock_stay_held() {
        let mut held: HashSet<u16> = [k::FN, k::CAPS_LOCK].into_iter().collect();
        let (ups, still_down) = after_gap(&mut held, 0, |_| false);
        assert!(ups.is_empty());
        assert_eq!(still_down, vec![k::CAPS_LOCK, k::FN]);
    }

    #[test]
    fn after_a_gap_a_modifier_pressed_in_it_is_not_counted_as_down() {
        let mut held = HashSet::new();
        let (ups, still_down) = after_gap(&mut held, 0x100108, |c| c == k::COMMAND_LEFT);
        assert!(ups.is_empty());
        assert!(held.is_empty());
        assert!(
            still_down.is_empty(),
            "the tap never saw it go down, so its release will not be delivered"
        );
    }

    #[test]
    fn after_a_gap_ordinary_keys_come_from_the_key_state() {
        let mut held = HashSet::new();
        let (_, still_down) = after_gap(&mut held, u64::MAX, |c| c == 2 || c == k::ESCAPE);
        assert_eq!(still_down, vec![2, k::ESCAPE]);
    }

    #[test]
    fn a_held_modifier_whose_flag_goes_off_is_released() {
        let mut held: HashSet<u16> = [61].into_iter().collect();
        assert_eq!(
            modifier_transition(&mut held, 61, CGEventFlags::CGEventFlagNull),
            Some(RawKey::Up(61))
        );
    }
}
