// SPDX-License-Identifier: MIT
//! The Shortcut card: which key is held to dictate, kept in History's
//! settings beside the microphone choice and applied to the running listener
//! without a relaunch. The rules for what is allowed live in
//! `macalla_platform::hotkey`; the words in `shortcut_words`.

use macalla_platform::hotkey::{Key, Shortcut, PRESETS};
use serde::Serialize;

use crate::command_error::CommandError;
use crate::dictation::Dictator;
use crate::history::HistoryHandle;
use crate::shortcut_words as words;

/// The shortcut to use: the saved one, or the platform default when nothing
/// is saved, or what is saved cannot be read or is one `check` refuses now.
pub(crate) fn chosen(history: &HistoryHandle) -> Shortcut {
    resolve(history.shortcut_choice().as_deref())
}

fn resolve(saved: Option<&str>) -> Shortcut {
    let Some(text) = saved else {
        return Shortcut::default_for_platform();
    };
    match Shortcut::parse(text) {
        Ok(shortcut) => match shortcut.check() {
            Ok(()) => shortcut,
            Err(reason) => {
                tracing::warn!(
                    saved = text,
                    ?reason,
                    "the saved shortcut is refused; using the default"
                );
                Shortcut::default_for_platform()
            }
        },
        Err(error) => {
            tracing::warn!(saved = text, %error, "the saved shortcut cannot be read; using the default");
            Shortcut::default_for_platform()
        }
    }
}

/// One entry in the card's list.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShortcutRow {
    pub label: String,
    /// What `shortcut_choose` takes for this row.
    pub value: String,
    /// A plain sentence shown with the row, if it has one.
    pub note: Option<&'static str>,
}

/// Mirrors `commands.ts`'s `ShortcutView`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShortcutView {
    pub heading: &'static str,
    pub about: String,
    pub rows: Vec<ShortcutRow>,
    pub chosen: String,
    pub record_option: &'static str,
    pub press_the_keys: &'static str,
    pub cancel: &'static str,
}

fn view(shortcut: &Shortcut) -> ShortcutView {
    let value = shortcut.to_string();
    let mut rows: Vec<ShortcutRow> = PRESETS
        .iter()
        .filter_map(|label| {
            let parsed = Shortcut::parse(label).ok()?;
            Some(ShortcutRow {
                label: (*label).to_string(),
                value: parsed.to_string(),
                note: (parsed.key == Key::Fn).then_some(words::FN_NOTE),
            })
        })
        .collect();
    if !rows.iter().any(|row| row.value == value) {
        // A recorded combination, shown as its own row so the list can say
        // which one is in use.
        rows.push(ShortcutRow {
            label: value.clone(),
            value: value.clone(),
            note: None,
        });
    }
    ShortcutView {
        heading: words::HEADING,
        about: words::ABOUT.replace("{shortcut}", &value),
        rows,
        chosen: value,
        record_option: words::RECORD_OPTION,
        press_the_keys: words::PRESS_THE_KEYS,
        cancel: words::CANCEL,
    }
}

/// Parse, check and save `text`. Nothing is saved unless it is allowed.
fn accept(history: &HistoryHandle, text: &str) -> Result<Shortcut, CommandError> {
    let shortcut = Shortcut::parse(text).map_err(|error| {
        tracing::debug!(%error, text, "not a shortcut");
        CommandError::from(words::NOT_USABLE)
    })?;
    shortcut
        .check()
        .map_err(|reason| CommandError::from(words::refusal(reason)))?;
    history.set_shortcut_choice(&shortcut.to_string())?;
    Ok(shortcut)
}

/// The shortcut to hold, the way the key caps read.
#[tauri::command]
pub fn shortcut(history: tauri::State<'_, HistoryHandle>) -> String {
    chosen(&history).to_string()
}

/// The Shortcut card: the list, the one in use, and every sentence it shows.
#[tauri::command]
pub fn shortcut_view(history: tauri::State<'_, HistoryHandle>) -> ShortcutView {
    view(&chosen(&history))
}

/// Use `text` from now on: parsed, checked, saved, and applied to the
/// listener at once. A refusal comes back as a sentence and changes nothing.
#[tauri::command]
pub fn shortcut_choose(
    history: tauri::State<'_, HistoryHandle>,
    dictator: tauri::State<'_, Dictator>,
    text: String,
) -> Result<(), CommandError> {
    let shortcut = accept(&history, &text)?;
    tracing::info!(shortcut = %shortcut, "shortcut changed");
    dictator.apply_shortcut(shortcut);
    Ok(())
}

/// The recorder opened (`true`) or closed (`false`): while it is open the
/// current shortcut starts nothing.
#[tauri::command]
pub fn shortcut_record(dictator: tauri::State<'_, Dictator>, on: bool) {
    dictator.pause_shortcut(on);
}

/// A key going down or up in one of Macalla's own windows, as the page's
/// `KeyboardEvent.code` names it. On Windows the keyboard hook hears nothing
/// while a Macalla window has focus, so the page passes its keys here and they
/// reach the shortcut the way the hook's would; elsewhere this does nothing.
/// A key the shortcut cannot use is ignored.
#[tauri::command]
pub fn shortcut_key(dictator: tauri::State<'_, Dictator>, code: String, down: bool, repeat: bool) {
    use macalla_platform::vkey::HookEvent;
    let Some(code) = virtual_key(&code) else {
        return;
    };
    dictator.deliver_key(if down {
        HookEvent::Down { code, repeat }
    } else {
        HookEvent::Up(code)
    });
}

/// The window lost focus with these keys still down by its count. Each is let
/// go unless it is still physically held, so a moment's loss of focus does not
/// end a held dictation. Does nothing off Windows.
#[tauri::command]
pub fn shortcut_focus_lost(dictator: tauri::State<'_, Dictator>, codes: Vec<String>) {
    for code in codes.iter().filter_map(|c| virtual_key(c)) {
        dictator.key_focus_lost(code);
    }
}

/// The Windows virtual-key code for a `KeyboardEvent.code`, for the keys a
/// shortcut can be made of or that act on a dictation: the sided modifiers,
/// letters, digits, Space and Esc. `KeyboardEvent.code` names the physical
/// key, as a virtual-key code does for a modifier; for a letter it names the
/// US-layout position, which can differ from the layout's own letter, but no
/// letter can be a shortcut, so a letter only ever counts as
/// "another key".
fn virtual_key(code: &str) -> Option<u32> {
    use macalla_platform::vkey::vk;
    Some(match code {
        "ShiftLeft" => vk::SHIFT_LEFT,
        "ShiftRight" => vk::SHIFT_RIGHT,
        "ControlLeft" => vk::CONTROL_LEFT,
        "ControlRight" => vk::CONTROL_RIGHT,
        "AltLeft" => vk::ALT_LEFT,
        "AltRight" => vk::ALT_RIGHT,
        "MetaLeft" => vk::WIN_LEFT,
        "MetaRight" => vk::WIN_RIGHT,
        "Escape" => vk::ESCAPE,
        "Space" => vk::SPACE,
        _ => {
            let rest = code
                .strip_prefix("Key")
                .or_else(|| code.strip_prefix("Digit"))?;
            let mut chars = rest.chars();
            let c = chars.next()?;
            if chars.next().is_some() || !c.is_ascii_alphanumeric() {
                return None;
            }
            u32::from(c.to_ascii_uppercase())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_key_maps_to_the_code_the_hook_would_have_seen() {
        use macalla_platform::hotkey::{Key, Modifier, Side};
        use macalla_platform::vkey::from_vkey;
        let key = |code: &str| virtual_key(code).and_then(from_vkey);
        assert_eq!(
            key("ControlLeft"),
            Some(Key::Mod(Modifier::Control, Side::Left))
        );
        assert_eq!(
            key("ControlRight"),
            Some(Key::Mod(Modifier::Control, Side::Right))
        );
        assert_eq!(
            key("ShiftRight"),
            Some(Key::Mod(Modifier::Shift, Side::Right))
        );
        assert_eq!(key("AltLeft"), Some(Key::Mod(Modifier::Alt, Side::Left)));
        assert_eq!(
            key("MetaRight"),
            Some(Key::Mod(Modifier::Command, Side::Right))
        );
        assert_eq!(key("Escape"), Some(Key::Esc));
        assert_eq!(key("Space"), Some(Key::Char(' ')));
        assert_eq!(key("KeyD"), Some(Key::Char('d')));
        assert_eq!(key("Digit7"), Some(Key::Char('7')));
        for ignored in [
            "F9", "ArrowUp", "Tab", "Key", "KeyDD", "Digit", "Numpad1", "",
        ] {
            assert_eq!(virtual_key(ignored), None, "{ignored:?}");
        }
    }

    fn handle() -> HistoryHandle {
        HistoryHandle::in_memory_for_test()
    }

    #[test]
    fn a_saved_choice_survives_a_restart() {
        let h = handle();
        // A recorded Fn-plus-modifier combination: not a preset, so it exercises
        // the saved-and-reloaded path for a custom shortcut.
        let saved = accept(&h, "shift+fn").unwrap();
        assert_eq!(chosen(&h), saved);
    }

    #[test]
    fn every_preset_is_accepted_and_comes_back() {
        for label in PRESETS {
            let h = handle();
            let saved = accept(&h, label).unwrap();
            assert_eq!(chosen(&h), saved, "{label}");
        }
    }

    #[test]
    fn nothing_saved_or_something_bad_saved_is_the_default() {
        assert_eq!(resolve(None), Shortcut::default_for_platform());
        assert_eq!(resolve(Some("not a key")), Shortcut::default_for_platform());
        assert_eq!(resolve(Some("Esc")), Shortcut::default_for_platform());
        assert_eq!(resolve(Some("Cmd+C")), Shortcut::default_for_platform());
        let h = handle();
        h.set_shortcut_choice("cmd+q").unwrap();
        assert_eq!(chosen(&h), Shortcut::default_for_platform());
    }

    #[test]
    fn a_refused_choice_says_why_and_is_never_saved() {
        let h = handle();
        let refused = accept(&h, "cmd+space").unwrap_err();
        assert_eq!(
            refused.to_string(),
            words::refusal(macalla_platform::hotkey::Refusal::WouldType)
        );
        assert_eq!(h.shortcut_choice(), None);
        assert!(accept(&h, "banana").is_err());
        assert_eq!(h.shortcut_choice(), None);
    }

    #[test]
    fn the_view_lists_the_presets_and_adds_a_custom_row() {
        let v = view(&Shortcut::default_for_platform());
        assert_eq!(v.rows.len(), PRESETS.len());
        assert!(v.rows.iter().any(|r| r.value == v.chosen));
        // The Fn row (macOS) is the only preset with a note; off macOS there is
        // no Fn preset, so none has one.
        let notes = v.rows.iter().filter(|r| r.note.is_some()).count();
        assert_eq!(notes, usize::from(cfg!(target_os = "macos")));

        // A recorded combination that is not a preset gets its own row.
        let v = view(&Shortcut::parse("shift+fn").unwrap());
        assert_eq!(v.rows.len(), PRESETS.len() + 1);
        assert_eq!(v.rows.last().unwrap().value, v.chosen);
        assert!(v.about.contains(&v.chosen));
    }
}
