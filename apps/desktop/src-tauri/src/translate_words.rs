// SPDX-License-Identifier: MIT
//! What the Files page says about translating a transcript, worded here and
//! nowhere else (user-facing wording lives in one place per
//! app). The core hands over typed reasons
//! (`macalla_core::translate::Unavailable`, `TranslateError`) and no
//! sentence; `files/translation.rs` picks an entry here and never builds one.
//!
//! Each entry is a whole sentence, or two, never glued from fragments. Its
//! placeholders are named and are data only: `{source}` and `{target}` are
//! languages, which reach the page as BCP-47 codes and are named there in the
//! person's own language by the web view's `Intl.DisplayNames`, which follows
//! the system's; `{current}` and `{total}` are counts.
//!
//! A first draft, for review.

/// The Files page's translation card: its heading.
pub(crate) const HEADING: &str = "Translation";

/// The translation card's one sentence.
pub(crate) const ABOUT: &str = "Make a copy of this transcript in another language, translated \
                                on this Mac. The original stays as it is.";

/// The label beside the choice of language.
pub(crate) const INTO: &str = "Translate into";

/// The button that starts a translation.
pub(crate) const TRANSLATE: &str = "Translate";

/// The button that stops a translation.
pub(crate) const STOP: &str = "Stop";

/// While a translation runs: `{current}` is the turn being translated, of
/// `{total}`.
pub(crate) const PROGRESS: &str = "Translating turn {current} of {total}…";

/// Once Stop was pressed, until the turn in hand comes back.
pub(crate) const STOPPING: &str = "Stopping after this turn…";

/// Above a finished translation: `{source}` and `{target}`.
pub(crate) const MADE: &str = "Translated from {source} into {target}.";

/// Above a translation made before the transcript was corrected by hand.
pub(crate) const OUT_OF_DATE: &str =
    "The transcript was corrected after this was translated, so the two may not match.";

/// The translated copy's Save button, which saves in the Files card's format.
pub(crate) const SAVE: &str = "Save translation";

/// The translated copy's Copy button, beside Save translation.
pub(crate) const COPY: &str = "Copy translation";

/// After Copy translation.
pub(crate) const COPIED: &str = "Copied.";

/// After Save translation: `{path}` is where the file went.
pub(crate) const SAVED: &str = "Saved to {path}.";

/// The pair can be translated, but the language is not on this Mac yet. The
/// path is Apple's own, from its "Translate text on Mac" support page; the
/// Translation framework cannot open the download prompt for an app with no
/// SwiftUI window.
pub(crate) const NOT_INSTALLED: &str = "{target} isn't on this Mac yet for translating from \
                                        {source}. Add it in System Settings > General > Language \
                                        & Region > Translation Languages, then try again.";

/// The pair cannot be translated here at all, whatever is installed. Irish is
/// one such target.
pub(crate) const UNSUPPORTED: &str = "This Mac can't translate from {source} into {target}.";

/// There is no translator, on a Mac: a macOS older than 26, or a build of
/// Macalla without it. `files/translation.rs` picks this or
/// [`UNAVAILABLE_ELSEWHERE`] by the platform the app was built for.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const UNAVAILABLE_MACOS: &str =
    "Translating isn't available here. It needs macOS 26 or later.";

/// There is no translator, on Windows or Linux, which have none yet.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) const UNAVAILABLE_ELSEWHERE: &str = "Translation isn't available on this platform yet.";

/// A translation stopped by the person.
pub(crate) const STOPPED: &str = "The translation was stopped, and nothing from it was kept.";

/// A translation that failed for a reason with no sentence of its own.
pub(crate) const FAILED: &str = "The translation didn't finish, and nothing from it was kept.";

/// Translate pressed with no transcript on the page.
pub(crate) const NOTHING_TO_TRANSLATE: &str =
    "There is no transcript on the Files page to translate.";

/// Save translation pressed with no translation kept.
pub(crate) const NOTHING_TO_SAVE: &str = "There is no translation to save.";

/// Copy translation pressed with no translation kept.
pub(crate) const NOTHING_TO_COPY: &str = "There is no translation to copy.";

/// The translation's thread could not be started.
pub(crate) const COULD_NOT_START: &str = "Could not start the translation. Try again.";
