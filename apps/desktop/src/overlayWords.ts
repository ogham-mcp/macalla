// SPDX-License-Identifier: MIT
//
// The overlay's words, each entry whole: about Enhancement, and its button's
// label. The dictation thread sends typed events (that the paste is waiting,
// and whether an enhanced text is still on its way to History); this is the
// one place they become words.
// Never glue an entry together from pieces, because other languages order
// words differently.

import type { EnhancementMissed } from "./commands";

export const overlayWords = {
  // The × button's accessible name and tooltip.
  cancelDictation: "Cancel dictation",
  // The ✓ button's accessible name and tooltip.
  finishDictation: "Finish dictation",
  enhancing: "Enhancing…",
  pastedWithoutEnhancement: "Pasted without Enhancement.",
  // Only a promise that History gets the enhanced text if Enhancement
  // finishes: a late request can still time out or fail. Two lines at the
  // overlay's 332 px, 13 px (476 px on one line).
  pastedWithoutEnhancementStillComing:
    "Pasted without Enhancement. If it finishes, History will have the enhanced text.",
} as const;

/** The top row once the text was pasted without the Enhancement it waited for. */
export function missedSentence(missed: EnhancementMissed): string {
  return missed.in_history
    ? overlayWords.pastedWithoutEnhancementStillComing
    : overlayWords.pastedWithoutEnhancement;
}
