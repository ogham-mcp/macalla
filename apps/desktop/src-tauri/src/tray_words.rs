// SPDX-License-Identifier: MIT
//! Tray menu wording added since the app's wording rule (user-facing
//! wording lives in one place per app). The menu's older labels ("Start
//! Dictation", "Open Macalla…", …) are still inline in `tray.rs`'s `build`
//! and `animate`, and move here in the wording refactor after v1, not before;
//! the Microphone submenu's words are in `microphone_words.rs`, because the
//! settings card shares them.

/// The item shown below Start/Stop Dictation while a dictation runs
/// (`tray.rs`'s `cancel_change`).
pub(crate) const CANCEL_DICTATION: &str = "Cancel Dictation";
