// SPDX-License-Identifier: MIT
//! Wording shared between the Microphone settings card
//! (`commands::microphones`, drawn on by `apps/desktop/src/App.tsx`'s
//! `MicrophoneCard`) and the tray's Microphone submenu (`tray.rs`'s
//! `microphone_menu_items`): the app's wording rule, one entry per row, so
//! the two surfaces can never say the same row differently. A device name
//! itself is data, shown verbatim as the OS gives it -- only the words
//! around it live here. The tray alone doubles a `&` in the composed label
//! afterwards, for muda's mnemonic convention (`tray.rs`'s
//! `escape_menu_text`); that is a menu-only concern and has no business
//! here, since the card shows the same text in an HTML `<option>`, which
//! reads a `&` as nothing but itself.

/// The submenu's own title, next to the tray's other inline labels
/// ("Start Dictation", "Open Macalla…", …, in `tray.rs`'s `build`).
pub(crate) const MICROPHONE: &str = "Microphone";

/// The system default row's label: the resolved name in parentheses once
/// known, or the bare phrase before it is, or when the OS could not say.
pub(crate) fn system_default_label(resolved: Option<&str>) -> String {
    match resolved {
        Some(name) => format!("System default ({name})"),
        None => "System default".to_string(),
    }
}

/// The saved choice's row label once it is no longer among the connected
/// devices.
pub(crate) fn gone_label(name: &str) -> String {
    format!("{name} (not connected)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_label_names_the_resolved_device() {
        assert_eq!(
            system_default_label(Some("Built-in")),
            "System default (Built-in)"
        );
    }

    #[test]
    fn the_default_label_falls_back_when_nothing_resolved() {
        assert_eq!(system_default_label(None), "System default");
    }

    #[test]
    fn the_gone_label_names_the_device_and_says_so() {
        assert_eq!(gone_label("USB Mic"), "USB Mic (not connected)");
    }
}
