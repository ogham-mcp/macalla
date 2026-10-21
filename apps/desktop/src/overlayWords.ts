// SPDX-License-Identifier: MIT
//
// The overlay's words, each entry whole: about Enhancement, a model loading,
// and its buttons' labels. The dictation thread sends typed events (that the
// paste is waiting, whether an enhanced text is still on its way to History,
// that a model is loading); this is the one place they become words. A
// placeholder is named in braces; what fills it is data, such as a name.
// Never glue an entry together from pieces, because other languages order
// words differently.

import type { EngineLoading, EnhancementMissed } from "./commands";

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
  // A dictation is waiting on an on-device model. `{engine}` is the
  // engine's name. Nothing is heard until the overlay says "Listening…".
  loadingOpen: "Loading {engine}. Start speaking when this says Listening.",
  // The same, after the model was unloaded for being idle.
  loadingReload: "Loading {engine} again. Start speaking when this says Listening.",
} as const;

/** The top row once the text was pasted without the Enhancement it waited for. */
export function missedSentence(missed: EnhancementMissed): string {
  return missed.in_history
    ? overlayWords.pastedWithoutEnhancementStillComing
    : overlayWords.pastedWithoutEnhancement;
}

/** The top row while a dictation waits on a model loading. */
export function loadingSentence(loading: EngineLoading): string {
  const sentence = loading.reason === "reload" ? overlayWords.loadingReload : overlayWords.loadingOpen;
  return sentence.replace("{engine}", loading.engine);
}
