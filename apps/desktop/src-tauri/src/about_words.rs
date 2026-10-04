// SPDX-License-Identifier: MIT
//! Wording for the About screen's links (`links.rs`, drawn by
//! `apps/desktop/src/App.tsx`'s `AboutView`): the app's wording rule, one
//! entry per label or sentence.

/// The link to the website's home page.
pub(crate) const WEBSITE: &str = "Macalla's website";

/// The link to the website's guide.
pub(crate) const HELP: &str = "Help";

/// The macOS menu bar's first item, which opens the About page.
#[cfg(target_os = "macos")]
pub(crate) const ABOUT_MACALLA: &str = "About Macalla";

/// The macOS menu bar's Help item that opens the guide.
#[cfg(target_os = "macos")]
pub(crate) const MACALLA_HELP: &str = "Macalla Help";

/// Shown when the system would not open the browser.
pub(crate) const COULD_NOT_OPEN: &str = "Macalla could not open your browser.";
