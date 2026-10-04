// SPDX-License-Identifier: MIT
//! What the app says when a paste could not be delivered -- the app is not
//! trusted for Accessibility, so the OS drops the keystrokes it posts --
//! worded here and nowhere else (user-facing wording lives in one
//! place per app). `macalla-platform` hands over a typed
//! [`macalla_platform::paste::NotDelivered`] and no sentence at all.
//!
//! Each whole sentence is one entry, including the half that says where the
//! text went: History can be off, and a sentence promising History when it is
//! off would be a lie, so the pair is spelled out rather than glued together
//! from a stem and a tail.
//!
//! No app is named. The bundle identifier `macalla_platform::frontmost_app`
//! returns is data for a log, not a name for a person to read, and there is
//! no display name to be had here -- an invented one is worse than none.

use macalla_platform::paste::NotDelivered;

/// The overlay's notice, said to someone who is looking at the app they
/// dictated into and did not get their text.
pub(crate) fn could_not_paste(reason: NotDelivered, in_history: bool) -> &'static str {
    match (reason, in_history) {
        (NotDelivered::NotTrustedForAccessibility, true) => {
            "Not pasted, because Macalla does not have Accessibility permission. It is in History."
        }
        (NotDelivered::NotTrustedForAccessibility, false) => {
            "Not pasted, because Macalla does not have Accessibility permission. History is off, so it was not kept."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_says_whether_the_text_was_kept() {
        let reason = NotDelivered::NotTrustedForAccessibility;
        assert!(could_not_paste(reason, true).contains("in History"));
        assert!(could_not_paste(reason, false).contains("History is off"));
    }

    #[test]
    fn the_permission_is_named_because_the_person_can_fix_it() {
        // The condition someone can act on, so the sentence says which
        // permission to go and grant.
        assert!(
            could_not_paste(NotDelivered::NotTrustedForAccessibility, true)
                .contains("Accessibility")
        );
    }

    #[test]
    fn no_two_entries_are_the_same_sentence() {
        let all = [
            could_not_paste(NotDelivered::NotTrustedForAccessibility, true),
            could_not_paste(NotDelivered::NotTrustedForAccessibility, false),
        ];
        let mut seen = std::collections::HashSet::new();
        for sentence in all {
            assert!(seen.insert(sentence), "{sentence:?} is listed twice");
        }
    }
}
