// SPDX-License-Identifier: MIT
//! Windows virtual-key codes, and what one key event from the Windows
//! keyboard hook does to the recogniser.
//!
//! Everything here is pure and builds on every platform, so the table and the
//! event handling are tested on whatever machine runs the tests. The hook
//! itself, which needs Windows, is in `hook.rs`.

use crate::hotkey::{is_droppable_repeat, Action, Key, Modifier, Recogniser, Side};
use std::collections::HashSet;

/// Windows virtual-key codes for the keys a shortcut can be made of, from
/// `winuser.h`. A low-level keyboard hook reports the sided code for a
/// modifier (`VK_RCONTROL`, not `VK_CONTROL`), which is what a bare-modifier
/// shortcut needs.
pub mod vk {
    pub const ESCAPE: u32 = 0x1B;
    pub const SPACE: u32 = 0x20;
    pub const WIN_LEFT: u32 = 0x5B;
    pub const WIN_RIGHT: u32 = 0x5C;
    pub const SHIFT_LEFT: u32 = 0xA0;
    pub const SHIFT_RIGHT: u32 = 0xA1;
    pub const CONTROL_LEFT: u32 = 0xA2;
    pub const CONTROL_RIGHT: u32 = 0xA3;
    pub const ALT_LEFT: u32 = 0xA4;
    pub const ALT_RIGHT: u32 = 0xA5;
}

/// Translate a Windows virtual-key code into ours, or `None` for keys we do
/// not care about.
///
/// Letters and digits are `0x41..=0x5A` and `0x30..=0x39`, the ASCII codes of
/// the capital letter and the digit. Unlike a macOS keycode, a virtual-key
/// code follows the keyboard layout: "d" is wherever the layout puts D.
pub fn from_vkey(code: u32) -> Option<Key> {
    Some(match code {
        vk::WIN_LEFT => Key::Mod(Modifier::Command, Side::Left),
        vk::WIN_RIGHT => Key::Mod(Modifier::Command, Side::Right),
        vk::SHIFT_LEFT => Key::Mod(Modifier::Shift, Side::Left),
        vk::SHIFT_RIGHT => Key::Mod(Modifier::Shift, Side::Right),
        vk::CONTROL_LEFT => Key::Mod(Modifier::Control, Side::Left),
        vk::CONTROL_RIGHT => Key::Mod(Modifier::Control, Side::Right),
        vk::ALT_LEFT => Key::Mod(Modifier::Alt, Side::Left),
        vk::ALT_RIGHT => Key::Mod(Modifier::Alt, Side::Right),
        vk::ESCAPE => Key::Esc,
        vk::SPACE => Key::Char(' '),
        0x41..=0x5A | 0x30..=0x39 => Key::Char(char::from_u32(code)?.to_ascii_lowercase()),
        _ => return None,
    })
}

/// Whether a Left Ctrl event is the one Windows makes up for AltGr.
///
/// On a layout with AltGr (UK, Irish, most of Europe), Right Alt arrives as a
/// Left Ctrl press followed by Right Alt, and the made-up Left Ctrl carries
/// `0x200` in its scan code where a real one does not. Without this check,
/// AltGr would start a dictation whose shortcut is Left Ctrl. The same test is
/// how QEMU's Windows keyboard hook tells the two apart
/// (https://gitlab.com/qemu-project/qemu/-/blob/master/ui/win32-kbd-hook.c).
pub fn is_altgr_control(code: u32, scan_code: u32) -> bool {
    code == vk::CONTROL_LEFT && scan_code & 0x200 != 0
}

/// What the hook delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    /// `repeat` is true when the key was already down: Windows re-sends
    /// key-down on a timer for every held key, modifiers included, and the
    /// hook has no flag for it, so the hook works it out from the keys it
    /// has seen go down (see [`reconcile`] for how that memory is kept
    /// honest).
    Down {
        code: u32,
        repeat: bool,
    },
    Up(u32),
}

/// One event from the hook, applied to the recogniser: the action it causes
/// and the new [`Recogniser::is_held`] if it changed.
///
/// A repeat is dropped for Esc, for the trigger, and for a modifier. Esc and
/// the trigger for the reasons the macOS listener gives. A modifier because
/// Windows repeats a held one where macOS does not: its repeat would reach
/// [`Recogniser::press`] as "another key", and disarm Esc for as long as
/// someone rests a finger on Shift.
pub fn apply(r: &mut Recogniser, event: HookEvent) -> (Option<Action>, Option<bool>) {
    match event {
        HookEvent::Down { code, repeat } => {
            tracing::debug!(code, repeat, mapped = ?from_vkey(code), "key down");
            match from_vkey(code) {
                Some(k)
                    if is_droppable_repeat(k, repeat)
                        || (repeat && (r.is_trigger(k) || matches!(k, Key::Mod(..)))) =>
                {
                    (None, None)
                }
                Some(k) => r.key(k, true),
                None => (None, None),
            }
        }
        HookEvent::Up(code) => {
            tracing::debug!(code, mapped = ?from_vkey(code), "key up");
            from_vkey(code).map_or((None, None), |k| r.key(k, false))
        }
    }
}

/// The keys remembered as down that no longer are, removed from `held` and
/// returned so each can be delivered as a key-up.
///
/// The hook misses a key-up when the key is let go where it cannot see:
/// while a User Account Control prompt or the lock screen is up, both of
/// which run on another desktop. Without this, a trigger released there
/// would stay "held" and the dictation would run on; and its next press
/// would look like a repeat. The hook's thread asks a few times a second
/// while anything is held, so the memory is never wrong for long.
pub fn reconcile(held: &mut HashSet<u32>, is_down: impl Fn(u32) -> bool) -> Vec<u32> {
    let mut ups: Vec<u32> = held.iter().copied().filter(|&c| !is_down(c)).collect();
    ups.sort_unstable();
    for code in &ups {
        held.remove(code);
    }
    ups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hotkey::{Mode, Shortcut};

    fn right_ctrl() -> Recogniser {
        Recogniser::new(Shortcut::parse("Right Ctrl").expect("parses"), Mode::Hold)
    }

    fn down(code: u32) -> HookEvent {
        HookEvent::Down {
            code,
            repeat: false,
        }
    }

    fn repeat(code: u32) -> HookEvent {
        HookEvent::Down { code, repeat: true }
    }

    #[test]
    fn a_modifier_keeps_its_side() {
        assert_eq!(
            from_vkey(vk::CONTROL_RIGHT),
            Some(Key::Mod(Modifier::Control, Side::Right))
        );
        assert_eq!(
            from_vkey(vk::CONTROL_LEFT),
            Some(Key::Mod(Modifier::Control, Side::Left))
        );
        assert_eq!(
            from_vkey(vk::ALT_RIGHT),
            Some(Key::Mod(Modifier::Alt, Side::Right))
        );
        assert_eq!(
            from_vkey(vk::WIN_LEFT),
            Some(Key::Mod(Modifier::Command, Side::Left))
        );
    }

    #[test]
    fn the_left_ctrl_altgr_makes_up_is_told_from_a_real_one() {
        // A real Left Ctrl has scan code 0x1D; AltGr's has 0x21D.
        assert!(is_altgr_control(vk::CONTROL_LEFT, 0x21D));
        assert!(!is_altgr_control(vk::CONTROL_LEFT, 0x1D));
        // Only Left Ctrl is ever made up, whatever the scan code says.
        assert!(!is_altgr_control(vk::CONTROL_RIGHT, 0x21D));
        assert!(!is_altgr_control(vk::ALT_RIGHT, 0x238));
    }

    #[test]
    fn letters_digits_space_and_escape_map() {
        assert_eq!(from_vkey(0x44), Some(Key::Char('d')));
        assert_eq!(from_vkey(0x41), Some(Key::Char('a')));
        assert_eq!(from_vkey(0x5A), Some(Key::Char('z')));
        assert_eq!(from_vkey(0x30), Some(Key::Char('0')));
        assert_eq!(from_vkey(0x39), Some(Key::Char('9')));
        assert_eq!(from_vkey(vk::SPACE), Some(Key::Char(' ')));
        assert_eq!(from_vkey(vk::ESCAPE), Some(Key::Esc));
    }

    #[test]
    fn keys_we_do_not_care_about_map_to_nothing() {
        // The unsided VK_SHIFT, VK_CONTROL and VK_MENU are never what a
        // low-level hook reports; F9 and the arrow keys are not shortcut keys.
        for code in [0x10, 0x11, 0x12, 0x78, 0x26, 0x00, 0xFF] {
            assert_eq!(from_vkey(code), None, "{code:#x}");
        }
    }

    #[test]
    fn the_default_shortcut_off_macos_is_a_key_the_table_delivers() {
        if cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(
            from_vkey(vk::CONTROL_RIGHT),
            Some(Shortcut::default_for_platform().key)
        );
    }

    #[test]
    fn holding_right_ctrl_starts_and_letting_go_stops() {
        let mut r = right_ctrl();
        assert_eq!(
            apply(&mut r, down(vk::CONTROL_RIGHT)),
            (Some(Action::Start), Some(true))
        );
        assert_eq!(
            apply(&mut r, HookEvent::Up(vk::CONTROL_RIGHT)),
            (Some(Action::Stop), Some(false))
        );
    }

    #[test]
    fn left_ctrl_is_not_the_shortcut() {
        let mut r = right_ctrl();
        assert_eq!(apply(&mut r, down(vk::CONTROL_LEFT)).0, None);
        assert_eq!(apply(&mut r, HookEvent::Up(vk::CONTROL_LEFT)).0, None);
    }

    #[test]
    fn a_held_trigger_repeating_starts_nothing_more() {
        let mut r = right_ctrl();
        assert_eq!(
            apply(&mut r, down(vk::CONTROL_RIGHT)).0,
            Some(Action::Start)
        );
        for _ in 0..30 {
            assert_eq!(apply(&mut r, repeat(vk::CONTROL_RIGHT)), (None, None));
        }
        assert_eq!(
            apply(&mut r, HookEvent::Up(vk::CONTROL_RIGHT)).0,
            Some(Action::Stop)
        );
    }

    #[test]
    fn a_trigger_repeat_after_its_dictation_ended_does_not_start_another() {
        // The dictation ends while the key is still down (the 5-minute cap,
        // a discard). The key keeps repeating; none of those is a new press.
        let mut r = right_ctrl();
        apply(&mut r, down(vk::CONTROL_RIGHT));
        r.set_dictating(true);
        r.set_dictating(false);
        assert_eq!(apply(&mut r, repeat(vk::CONTROL_RIGHT)).0, None);
    }

    #[test]
    fn esc_twice_discards_and_a_held_esc_is_one_press() {
        let mut r = right_ctrl();
        apply(&mut r, down(vk::CONTROL_RIGHT));
        r.set_dictating(true);
        assert_eq!(apply(&mut r, down(vk::ESCAPE)).0, None);
        for _ in 0..10 {
            assert_eq!(apply(&mut r, repeat(vk::ESCAPE)).0, None, "a repeat");
        }
        apply(&mut r, HookEvent::Up(vk::ESCAPE));
        assert_eq!(apply(&mut r, down(vk::ESCAPE)).0, Some(Action::Discard));
    }

    #[test]
    fn a_held_modifier_repeating_does_not_disarm_esc() {
        // Windows repeats a held Shift. Those repeats must not count as
        // "another key" between the two Escapes.
        let mut r = right_ctrl();
        apply(&mut r, down(vk::CONTROL_RIGHT));
        r.set_dictating(true);
        apply(&mut r, down(vk::SHIFT_LEFT));
        apply(&mut r, down(vk::ESCAPE));
        apply(&mut r, HookEvent::Up(vk::ESCAPE));
        for _ in 0..10 {
            assert_eq!(apply(&mut r, repeat(vk::SHIFT_LEFT)), (None, None));
        }
        assert_eq!(apply(&mut r, down(vk::ESCAPE)).0, Some(Action::Discard));
    }

    #[test]
    fn reconcile_releases_only_what_came_up() {
        let mut held: HashSet<u32> = [vk::CONTROL_RIGHT, vk::SHIFT_LEFT, 0x44].into();
        let ups = reconcile(&mut held, |c| c == vk::SHIFT_LEFT);
        assert_eq!(ups, vec![0x44, vk::CONTROL_RIGHT]);
        assert_eq!(held, [vk::SHIFT_LEFT].into());
    }

    #[test]
    fn reconcile_with_everything_still_down_changes_nothing() {
        let mut held: HashSet<u32> = [vk::CONTROL_RIGHT].into();
        assert!(reconcile(&mut held, |_| true).is_empty());
        assert_eq!(held.len(), 1);
    }

    #[test]
    fn a_key_released_out_of_sight_stops_the_dictation_once_reconciled() {
        let mut r = right_ctrl();
        let mut held: HashSet<u32> = HashSet::new();
        held.insert(vk::CONTROL_RIGHT);
        assert_eq!(
            apply(&mut r, down(vk::CONTROL_RIGHT)).0,
            Some(Action::Start)
        );
        // No key-up arrives. The next look finds the key up.
        let ups = reconcile(&mut held, |_| false);
        let actions: Vec<_> = ups
            .into_iter()
            .map(|c| apply(&mut r, HookEvent::Up(c)))
            .collect();
        assert_eq!(actions, vec![(Some(Action::Stop), Some(false))]);
        // And the next press is a press, not a repeat of the lost one.
        assert_eq!(
            apply(&mut r, down(vk::CONTROL_RIGHT)).0,
            Some(Action::Start)
        );
    }
}
