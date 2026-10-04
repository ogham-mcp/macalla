// SPDX-License-Identifier: MIT
//! What the app says about the dictionary itself, worded here and nowhere
//! else (user-facing wording lives in one place per app).
//! Shared by History's own "Add to dictionary" button and its two notices,
//! and by the Files page's offer to add a hand correction under a turn just
//! corrected: one entry serves both, so the two surfaces never say it
//! differently.
//!
//! `ADDED`, `UPDATED` and [`crate::files::TranscriptWordsView`]'s
//! `correction_offer` are templates with named placeholders, filled by the
//! page (`fill` in `App.tsx`), the same as `meeting_words.rs`'s own.

/// The button that opens the form to add a term: History's own, and the
/// Files page's offer under a corrected turn.
pub(crate) const ADD_TO_DICTIONARY: &str = "Add to dictionary";

/// Said after adding a term that did not exist before. `{written}` is the
/// term's written form.
pub(crate) const ADDED: &str = "Added {written} to the dictionary.";

/// Said after adding spoken forms to a term that already existed.
/// `{written}` is the term's written form.
pub(crate) const UPDATED: &str = "Updated {written} in the dictionary.";

/// The Files page's offer to add a hand correction to the dictionary, shown
/// under the turn just corrected when the correction looks like one
/// mishearing (`macalla_core::dictionary::correction_offer`). `{heard}` is
/// the run of words Macalla heard; `{written}` is what the person corrected
/// it to.
pub(crate) const CORRECTION_OFFER: &str = "Macalla heard \"{heard}\". Add \"{written}\" to the \
     dictionary so every engine writes it that way?";

// `WORDS_NOT_LOADED` is not here: it is said when *this catalogue itself*
// could not be fetched, so it can never be one more field on the view this
// module hands back -- asking again would be the exact call that just
// failed. It lives as a constant in `App.tsx`, with a comment pointing
// back to this file.
