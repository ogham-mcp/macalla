// SPDX-License-Identifier: MIT
//
// Dev-only: a browser stand-in for the Tauri backend, so the settings UI can
// be worked on with `npx vite` and no Tauri app running. Imported only from
// main.tsx, only when `import.meta.env.DEV` is true and the page URL has
// `?preview` -- see ./README.md. Never imported by a release build.

import { emit } from "@tauri-apps/api/event";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import {
  FIXTURE_DICTIONARY_INFO,
  FIXTURE_DICTIONARY_TERMS,
  FIXTURE_DICTIONARY_WORDS,
  FIXTURE_EDIT_REFUSALS,
  FIXTURE_HISTORY,
  FIXTURE_HISTORY_RETENTION,
  FIXTURE_UNLOAD,
  FIXTURE_INPUT_DEVICE,
  FIXTURE_MEETING_BATCH,
  FIXTURE_MEETING_CURRENT,
  FIXTURE_MEETING_DESTINATIONS,
  FIXTURE_MEETING_LIVE_WORDS,
  FIXTURE_MEETING_RUNNING_LIVE,
  FIXTURE_MEETING_RUNNING_SUMMARY,
  FIXTURE_LAUNCH_AT_LOGIN,
  FIXTURE_MICROPHONES,
  FIXTURE_MODELS_DIR_PATH,
  FIXTURE_MODELS_LIST,
  FIXTURE_MODELS_OVERVIEW,
  FIXTURE_SETUP_STEPS,
  FIXTURE_SETUP_STEPS_ELSEWHERE_DONE,
  FIXTURE_SETUP_STEPS_ELSEWHERE_PENDING,
  FIXTURE_SETUP_STEPS_MACOS_PENDING,
  FIXTURE_SHORTCUT,
  FIXTURE_SHORTCUT_VIEW,
  FIXTURE_TRANSLATION_COPY,
  FIXTURE_TRANSLATION_NOT_INSTALLED,
  FIXTURE_TRANSLATION_OPTIONS,
  FIXTURE_TRANSLATION_OPTIONS_UNAVAILABLE,
  FIXTURE_TRANSLATION_UNSUPPORTED,
} from "./fixtures";
import type { MeetingView, TermView } from "../commands";

// Punctuation stripped from a word's, or a run's, own two edges, mirroring
// the core's own `EDGE_PUNCTUATION` and `EDGE_PUNCTUATION_UNICODE`
// (`crates/macalla-core/src/dictionary/mod.rs`): the ASCII set, plus curly
// quotes, guillemets, the ellipsis character and the dash family.
const EDGE_PUNCTUATION =
  /^[.,;:!?"'()‘’“”‹›«»…–—―−]+|[.,;:!?"'()‘’“”‹›«»…–—―−]+$/g;
const stripped = (word: string) => word.replace(EDGE_PUNCTUATION, "");

// Mirrors the core's own `MAX_OFFER_WORDS` and `MAX_OFFER_CHARS`.
const MAX_OFFER_WORDS = 3;
const MAX_OFFER_CHARS = 40;

/**
 * Mirrors `macalla_core::dictionary::correction_offer` for the preview,
 * which has no Rust to ask: trim the common prefix and suffix by word, each
 * pair compared with its own edge punctuation stripped -- which is also
 * what refuses a punctuation-only difference, since step trims it away as
 * common -- then reject an empty run or a run over the word cap. What is
 * offered is each run joined and then stripped of its own edge punctuation
 * only -- "quarter." offers "quarter", but "Mr. Bob" keeps its internal
 * period -- and rejected if, after that stripping, it is over the
 * character cap or has nothing alphanumeric left in it. Deliberately does
 * not try to detect a turn edited in two places: the person confirms the
 * offer in TermForm before anything is saved.
 */
function correctionOffer(old: string, updated: string): { heard: string; written: string } | null {
  const a = old.split(/\s+/).filter(Boolean);
  const b = updated.split(/\s+/).filter(Boolean);
  let start = 0;
  while (start < a.length && start < b.length && stripped(a[start]) === stripped(b[start])) {
    start++;
  }
  let aEnd = a.length;
  let bEnd = b.length;
  while (aEnd > start && bEnd > start && stripped(a[aEnd - 1]) === stripped(b[bEnd - 1])) {
    aEnd--;
    bEnd--;
  }
  const heardWords = a.slice(start, aEnd);
  const writtenWords = b.slice(start, bEnd);
  if (heardWords.length === 0 || writtenWords.length === 0) {
    return null;
  }
  if (heardWords.length > MAX_OFFER_WORDS || writtenWords.length > MAX_OFFER_WORDS) {
    return null;
  }
  const heard = stripped(heardWords.join(" "));
  const written = stripped(writtenWords.join(" "));
  if ([...heard].length > MAX_OFFER_CHARS || [...written].length > MAX_OFFER_CHARS) {
    return null;
  }
  if (!/\p{L}|\p{N}/u.test(heard) || !/\p{L}|\p{N}/u.test(written)) {
    return null;
  }
  return { heard, written };
}

/**
 * Unique marker for the leak guard (scripts/check-no-preview.mjs): a release
 * build must never contain this string. Referenced in a log line below so it
 * is not tree-shaken out of this module.
 */
export const MACALLA_BROWSER_PREVIEW_MARKER_7f3c = "MACALLA_BROWSER_PREVIEW_MARKER_7f3c";

// A mocked main window, so any window call the pages make resolves outside
// Tauri. Drops arrive as the app's own `files-dropped` event.
mockWindows("main");

/**
 * Every command the main settings page, the Models page, the History page,
 * the Dictionary page and the Files page call when they load, with invented,
 * realistic data (see fixtures.ts). Everything else -- every command
 * reachable only from a click -- rejects with a message starting
 * "not in preview: <command>" so a card visibly says so rather than
 * silently doing nothing.
 */
// The Files page's meeting, as the preview keeps it: a copy of the fixture,
// so a correction (`meeting_edit_turn`) shows, marks the summary out of date
// as the app does, and is gone on reload.
let previewMeeting: MeetingView = structuredClone(FIXTURE_MEETING_CURRENT);

// The dictionary, as the preview keeps it: a copy of the fixture, so
// `dictionary_add` (from History, or from the correction offer) actually
// adds a term and `meeting_correction_offer` can see it, and is gone on
// reload.
let previewDictionary: TermView[] = structuredClone(FIXTURE_DICTIONARY_TERMS);

/** Whether the preview dictionary already maps `heard` to `written`, folding
 * case the way `dictionary.rs`'s `already_has_pair` does for a term that is
 * not case-sensitive -- every preview term is. */
function alreadyHasPair(written: string, heard: string): boolean {
  const term = previewDictionary.find(
    (t) => t.written.trim().toLowerCase() === written.trim().toLowerCase(),
  );
  if (!term) {
    return false;
  }
  const target = heard.trim().toLowerCase();
  return term.spoken.some((s) => s.trim().toLowerCase() === target);
}

// Which state the Files page's translation card shows, from the URL:
// `?preview&translation=` `available` (the default: every pair installed),
// `not-installed` (no pair is on this Mac yet), `unsupported` (Irish, the
// one language offered), `unavailable` (no translator at all), or `done` (a
// German copy is kept).
const translationState = new URLSearchParams(window.location.search).get("translation") ?? "available";

// Which platform's onboarding checklist `setup_steps` answers with, from the
// URL: `?preview&os=` `macos` (the default), `windows` or `linux`, and
// `&setup=` `done` (the default) or `pending`. Windows and Linux send the
// same list, since neither has a permission step.
const previewParams = new URLSearchParams(window.location.search);
const setupOs = previewParams.get("os") ?? "macos";
const setupPending = previewParams.get("setup") === "pending";
function setupSteps() {
  if (setupOs === "windows" || setupOs === "linux") {
    return setupPending ? FIXTURE_SETUP_STEPS_ELSEWHERE_PENDING : FIXTURE_SETUP_STEPS_ELSEWHERE_DONE;
  }
  return setupPending ? FIXTURE_SETUP_STEPS_MACOS_PENDING : FIXTURE_SETUP_STEPS;
}

// `?preview&engine=loading`: the speech engine is still opening when the
// Files page first asks, so `meeting_destinations` names none, and two
// seconds later it is open and the app says `dictation-state: idle`, as the
// dictation thread does after every open. The page's engine line should
// follow.
let engineOpen = previewParams.get("engine") !== "loading";
let engineOpening = false;
function destinations() {
  if (!engineOpen && !engineOpening) {
    engineOpening = true;
    window.setTimeout(() => {
      engineOpen = true;
      void emit("dictation-state", "idle");
    }, 2000);
  }
  return engineOpen ? FIXTURE_MEETING_DESTINATIONS : { ...FIXTURE_MEETING_DESTINATIONS, engine: null };
}

// `?preview&meeting=early-summary`: the Files page opens while a summary
// runs, and the summary's event overtakes the page's first
// `meeting_current` reply, which was built before the summary was kept. The
// page should show the summary, not the older reply without it.
const earlySummary = previewParams.get("meeting") === "early-summary";

mockIPC((cmd, payload) => {
  switch (cmd) {
    // App.tsx itself, on mount.
    case "shortcut":
      return FIXTURE_SHORTCUT;

    // ShortcutCard. A choice is not kept: the preview has no listener.
    case "shortcut_view":
      return FIXTURE_SHORTCUT_VIEW;
    case "shortcut_choose":
    case "shortcut_record":
      return null;

    // SetupCard, polled every two seconds.
    case "setup_steps":
      return setupSteps();

    // DictionaryCard (and TryItStep's own read of the same numbers).
    case "dictionary_info":
      return FIXTURE_DICTIONARY_INFO;

    // HistoryCard (and TryItStep's own read of the same setting).
    case "history_retention":
      return FIXTURE_HISTORY_RETENTION;

    // EngineCard's idle-unload setting.
    case "unload_choice":
      return FIXTURE_UNLOAD;
    case "unload_choose":
      FIXTURE_UNLOAD.current = (payload as { value: string }).value;
      return undefined;

    // MicrophoneCard.
    case "input_device":
      return FIXTURE_INPUT_DEVICE;
    case "microphones":
      return FIXTURE_MICROPHONES;

    // GeneralCard. The switch holds for the session, as the system would.
    case "launch_at_login":
      return FIXTURE_LAUNCH_AT_LOGIN;
    case "launch_at_login_set":
      FIXTURE_LAUNCH_AT_LOGIN.on = (payload as { on: boolean }).on;
      return null;

    // EngineCard on the settings page, and ModelsView on the Models page --
    // the same call, by design, so the two pages can never disagree.
    case "models_overview":
      return FIXTURE_MODELS_OVERVIEW;

    // ModelsView, on load.
    case "models_list":
      return FIXTURE_MODELS_LIST;
    case "models_dir":
      return FIXTURE_MODELS_DIR_PATH;

    // Not called on load by either page (History's own list lives on the
    // History page, reached by a click), but cheap and asked for by name in
    // the preview brief, so it is mocked too.
    case "history_list":
      return FIXTURE_HISTORY;

    // DictionaryView, on load, and re-read after an add or an edit.
    case "dictionary_terms":
      return previewDictionary;
    // History's "Add to dictionary" button, and the Files page's offer.
    case "dictionary_words":
      return FIXTURE_DICTIONARY_WORDS;
    // Adding a term: the same rule as `TermFile::add` -- new spoken forms on
    // an existing written form, or a new term -- and returns whether it was
    // new, the same as the real command.
    case "dictionary_add": {
      const { written, spoken } = payload as { written: string; spoken: string[] };
      const key = written.trim().toLowerCase();
      const existing = previewDictionary.find((t) => t.written.trim().toLowerCase() === key);
      const forms = spoken.map((s) => s.trim()).filter(Boolean);
      if (existing) {
        const have = new Set(existing.spoken.map((s) => s.toLowerCase()));
        for (const form of forms) {
          if (!have.has(form.toLowerCase())) {
            existing.spoken.push(form);
            have.add(form.toLowerCase());
          }
        }
        return false;
      }
      previewDictionary.push({ written: written.trim(), spoken: forms });
      return true;
    }

    // FilesView, on load: the meeting kept in memory (if any), where hearing,
    // tidying and summarising would send the words, and whether a summary is
    // still running from before the page was left.
    case "meeting_current":
      if (earlySummary) {
        const before: MeetingView = { ...previewMeeting, summary: null, summary_by: null };
        const after: MeetingView = { ...previewMeeting, revision: previewMeeting.revision + 1 };
        window.setTimeout(() => void emit("meeting-summary", after), 300);
        return new Promise((resolve) => window.setTimeout(() => resolve(before), 1200));
      }
      return previewMeeting;
    // A correction to a turn: the same rules as `Meetings::edit_turn`, in
    // short, with the catalogue's refusal for an empty turn.
    case "meeting_edit_turn": {
      const { job, index, text } = payload as { job: number; index: number; text: string };
      const words = text.split(/\s+/).filter(Boolean).join(" ");
      if (words === "") {
        throw FIXTURE_EDIT_REFUSALS.empty;
      }
      if (job !== previewMeeting.job) {
        throw FIXTURE_EDIT_REFUSALS.stale;
      }
      if (!previewMeeting.turns[index]) {
        throw FIXTURE_EDIT_REFUSALS.noSuchTurn;
      }
      if (previewMeeting.turns[index].text !== words) {
        const turns = previewMeeting.turns.map((t, i) => (i === index ? { ...t, text: words } : t));
        previewMeeting = {
          ...previewMeeting,
          turns,
          revision: previewMeeting.revision + 1,
          summary_out_of_date: previewMeeting.summary_out_of_date || previewMeeting.summary !== null,
        };
      }
      return previewMeeting;
    }
    // The offer to add a turn's hand correction to the dictionary: the
    // core's own rule, then whether the preview dictionary already has the
    // pair (`correctionOffer`, `alreadyHasPair`).
    case "meeting_correction_offer": {
      const { old, new: updated } = payload as { old: string; new: string };
      const found = correctionOffer(old, updated);
      if (!found || alreadyHasPair(found.written, found.heard)) {
        return null;
      }
      return found;
    }
    case "meeting_destinations":
      return destinations();
    case "meeting_running_summary":
      return FIXTURE_MEETING_RUNNING_SUMMARY;
    // The live meeting card: its words, and whether one is recording.
    case "meeting_live_words":
      return FIXTURE_MEETING_LIVE_WORDS;
    case "meeting_running_live":
      return FIXTURE_MEETING_RUNNING_LIVE;
    // The batch card: a batch part-way through, so each row state shows.
    case "meeting_batch":
      return FIXTURE_MEETING_BATCH;
    // The translation card: see `translationState` above.
    case "meeting_translation_options":
      if (translationState === "unavailable") {
        return FIXTURE_TRANSLATION_OPTIONS_UNAVAILABLE;
      }
      // Apple does not list Irish; the preview does, to show the sentence.
      if (translationState === "unsupported") {
        return { ...FIXTURE_TRANSLATION_OPTIONS, languages: ["en-US", "ga"] };
      }
      return FIXTURE_TRANSLATION_OPTIONS;
    case "meeting_translation_check": {
      const { target } = payload as { target: string };
      if (translationState === "not-installed") {
        return { ...FIXTURE_TRANSLATION_NOT_INSTALLED, target };
      }
      if (translationState === "unsupported") {
        return FIXTURE_TRANSLATION_UNSUPPORTED;
      }
      return null;
    }
    case "meeting_translation":
      return translationState === "done" ? FIXTURE_TRANSLATION_COPY : null;
    case "meeting_running_translation":
      return null;

    // AboutView's Open-source software page. A representative slice, not the
    // full 445-crate list the real command compiles in.
    case "licenses":
      return [
        {
          spdx: "BSD-2-Clause",
          name: 'BSD 2-Clause "Simplified" License',
          text: 'Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:\n\n1. Redistributions of source code must retain the above copyright notice…',
          crates: [
            {
              name: "comrak",
              version: "0.55.0",
              repository: "https://github.com/kivikakk/comrak",
              copyright: ["Copyright (c) 2017–2025, Comrak contributors"],
            },
          ],
        },
        {
          spdx: "MIT",
          name: "MIT License",
          text: "Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the \"Software\")…",
          crates: [
            {
              name: "serde",
              version: "1.0.219",
              repository: "https://github.com/serde-rs/serde",
              copyright: ["Copyright (c) 2014 The serde Developers"],
            },
            {
              name: "transcribe-cpp",
              version: "0.2.3",
              repository: "https://github.com/handy-computer/transcribe.cpp",
              copyright: ["Copyright (c) 2026 The transcribe.cpp authors"],
            },
            {
              name: "tauri",
              version: "2.11.6",
              repository: "https://github.com/tauri-apps/tauri",
              copyright: [],
            },
          ],
        },
      ];

    default:
      throw new Error(`not in preview: ${cmd}`);
  }
}, { shouldMockEvents: true });

console.info(
  `[macalla] browser preview active (${MACALLA_BROWSER_PREVIEW_MARKER_7f3c}) -- dev only, fake data`,
);
