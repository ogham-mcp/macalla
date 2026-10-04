// SPDX-License-Identifier: MIT
//! Wording for the General settings card (`login.rs`'s
//! `launch_at_login`, drawn by `apps/desktop/src/App.tsx`'s `GeneralCard`):
//! the app's wording rule, one entry per sentence or label.

/// The card's heading.
pub(crate) const HEADING: &str = "General";

/// The card's one plain sentence.
pub(crate) const ABOUT: &str = "How Macalla starts.";

/// The switch's label.
pub(crate) const OPEN_AT_LOGIN: &str = "Open Macalla when you log in";

/// Shown under the switch when the system refused the change.
pub(crate) const COULD_NOT_CHANGE: &str =
    "Macalla could not change whether it opens when you log in.";
