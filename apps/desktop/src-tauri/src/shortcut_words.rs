// SPDX-License-Identifier: MIT
//! Wording for the Shortcut settings card (`shortcut.rs`, drawn by
//! `apps/desktop/src/App.tsx`'s `ShortcutCard`): the app's wording rule, one
//! whole sentence or label per entry. A placeholder is named in braces and
//! filled in by `shortcut.rs`; a key's own name is data.
//!
//! The macOS sentence about Input Monitoring is still inline in
//! `dictation.rs`'s `start`, and moves here in the wording refactor after v1,
//! not before.

use macalla_platform::hotkey::Refusal;

/// The card's heading.
pub(crate) const HEADING: &str = "Shortcut";

/// The card's one plain sentence. `{shortcut}` is the key's name.
pub(crate) const ABOUT: &str = "Hold {shortcut} to dictate.";

/// The last choice in the list, which opens the recorder. The recorder takes a
/// single modifier (or Fn) held on its own, so it says "key", not "combination".
pub(crate) const RECORD_OPTION: &str = "Record a key…";

/// Shown while the recorder waits for the key.
pub(crate) const PRESS_THE_KEYS: &str = "Press a key…";

/// The recorder's button, which leaves the shortcut as it was.
pub(crate) const CANCEL: &str = "Cancel";

/// Shown next to the Fn choice.
pub(crate) const FN_NOTE: &str = "macOS may also use Fn (Globe) for its own dictation or the \
     emoji picker. You can change that in System Settings, under Keyboard.";

/// Shown when the text names a key or combination Macalla cannot read.
pub(crate) const NOT_USABLE: &str = "Macalla cannot use that key. Try another.";

/// One sentence for each reason a shortcut is refused.
pub(crate) fn refusal(reason: Refusal) -> &'static str {
    match reason {
        Refusal::WouldType => {
            "That key would type while you dictate, and stops working while you type a \
             password. Hold a modifier such as Shift or Control on its own instead."
        }
        Refusal::Esc => "Esc discards a dictation, so it cannot start one.",
        Refusal::MenuKey => {
            "Alt on its own opens the menu of the app you're in, so your text would not paste \
             there. Hold Ctrl or Shift instead."
        }
    }
}

/// The Windows keyboard hook could not be installed, so the shortcut does
/// nothing. Windows has no permission to send anyone to; the tray still
/// starts a dictation. `shortcut` is the key as its cap reads, such as
/// "Right Ctrl".
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn not_listening(shortcut: &str) -> String {
    "{shortcut} cannot start a dictation right now. Use Start Dictation from the Macalla \
     icon, or quit Macalla and open it again."
        .replace("{shortcut}", shortcut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_names_the_key_and_the_way_that_still_works() {
        let said = not_listening("Right Ctrl");
        assert!(said.starts_with("Right Ctrl cannot start"), "{said}");
        assert!(said.contains("Start Dictation"), "{said}");
        assert!(!said.contains('{'), "a placeholder was left in: {said}");
    }
}
