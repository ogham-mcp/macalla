// SPDX-License-Identifier: MIT
//
// The overlay: small, bottom-centre, a waveform and the live
// partial text, a × that throws the dictation away, the same as the tray's
// Cancel Dictation, and, while recording, a ✓ that stops and pastes, the same
// as the tray's Stop Dictation. It is shown and hidden by the Rust side;
// this page only draws what the dictation thread emits. That includes the
// notice that a cloud result was not pasted because another app is in front:
// Rust hides it, and only if no dictation has started since, so a timer here
// can never hide the next dictation's overlay. Rust says when it hid it
// (`dictation-overlay-hidden`), which is the moment this page throws the
// dictation away: see `keeps`.
//
// The waveform is not decoration. Apple SpeechTranscriber emits no text at
// all for roughly the first four seconds of a dictation, then catches up, so
// for those seconds the level bars are the only sign that anything is being
// heard.

import React, { useEffect, useRef, useState } from "react";
import ReactDOM from "react-dom/client";
import { listen } from "@tauri-apps/api/event";
import { dictationCancel, dictationFinish } from "./commands";
import type { CloudWaiting, DictationLevel, EnhancementMissed, Enhancing, LoudnessView } from "./commands";
import { missedSentence, overlayWords } from "./overlayWords";
import "./overlay.css";

const BARS = 48;
const SILENT: DictationLevel = { rms_dbfs: -Infinity, loudness: "normal" };

// The bar's height scale, display only: -55 dBFS RMS draws no bar above the
// floor, -10 dBFS draws a full one, linear in dB between. Chosen from the
// 2026-09-16 recordings so that a quiet built-in microphone's normal speech
// (about -33 dBFS) reaches about half height, not a sliver. The colour
// thresholds are separate and live in the session.
const FLOOR_DBFS = -55;
const CEIL_DBFS = -10;

/** A bar's height as a fraction of the waveform, 0 to 1. */
function barHeight(rmsDbfs: number): number {
  const fraction = (rmsDbfs - FLOOR_DBFS) / (CEIL_DBFS - FLOOR_DBFS);
  return Math.min(1, Math.max(0, fraction));
}

// Which CSS custom property colours a bar of each loudness -- the tokens
// declared beside `--overlay-wave` in overlay.css -- and the colour to use
// if a token ever resolves empty, since a canvas silently ignores an invalid
// fill and would draw the bar in the previous bar's colour. Bar height stays
// driven by RMS alone; this only picks the fill.
const WAVE: Record<LoudnessView, { token: string; fallback: string }> = {
  normal: { token: "--overlay-wave", fallback: "#2bb5a0" },
  loud: { token: "--overlay-wave-loud", fallback: "#ff9f0a" },
  "near-clipping": { token: "--overlay-wave-clip", fallback: "#ff6961" },
};

function waveColours(el: HTMLElement): Record<LoudnessView, string> {
  const style = getComputedStyle(el);
  const colour = (loudness: LoudnessView) =>
    style.getPropertyValue(WAVE[loudness].token).trim() || WAVE[loudness].fallback;
  return {
    normal: colour("normal"),
    loud: colour("loud"),
    "near-clipping": colour("near-clipping"),
  };
}

function Waveform({ levels }: { levels: DictationLevel[] }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  // The overlay has one fixed theme, so the colours are read once, not on
  // every level event.
  const colours = useRef<Record<LoudnessView, string> | null>(null);

  useEffect(() => {
    const el = canvas.current;
    if (!el) {
      return;
    }
    const ctx = el.getContext("2d");
    if (!ctx) {
      return;
    }
    const scale = window.devicePixelRatio || 1;
    const width = el.clientWidth;
    const height = el.clientHeight;
    el.width = width * scale;
    el.height = height * scale;
    ctx.scale(scale, scale);
    ctx.clearRect(0, 0, width, height);
    const gap = 2;
    const bar = (width - gap * (BARS - 1)) / BARS;
    colours.current ??= waveColours(el);
    const colors = colours.current;
    levels.forEach(({ rms_dbfs, loudness }, i) => {
      // Keep a 2px floor so silence still reads as "listening".
      const h = Math.max(2, barHeight(rms_dbfs) * height);
      const x = i * (bar + gap);
      ctx.fillStyle = colors[loudness];
      ctx.fillRect(x, (height - h) / 2, bar, h);
    });
  }, [levels]);

  return <canvas ref={canvas} className="waveform" />;
}

/**
 * Whether the × shows: from the moment a dictation starts listening until it
 * is over -- pasted, discarded, failed, not pasted because another app came
 * in front, or nothing heard. That covers the recording, the wait for the
 * engine or a cloud provider, and Enhancing…, all of which a Discard still
 * reaches. A click after the end does nothing (Rust's `Dictator::cancel`), so
 * the few milliseconds this lags an event are harmless.
 *
 * Not while the shortcut's key is held to dictate (`keyHeld`, told by Rust on
 * `dictation-key-held`): the default key is Option, and a click with Option
 * down asks macOS to hide the app in front. A dictation started from the tray
 * is held only while that key happens to be down. Untested: the app has no
 * frontend test runner.
 */
function showsCancel({ running, keyHeld }: { running: boolean; keyHeld: boolean }): boolean {
  return running && !keyHeld;
}

/**
 * A button whose slot is kept while it does not show: `shown` false leaves it
 * laid out but invisible, unclickable and out of the tab order and the
 * accessibility tree, so the other button never slides under a pointer
 * mid-dictation. A double click on ✓ would otherwise land its second click on
 * the ×, moved into the ✓'s place, and discard.
 */
function slot(shown: boolean) {
  return shown
    ? {}
    : { disabled: true, tabIndex: -1, "aria-hidden": true as const, "data-slot-only": "" };
}

/**
 * Whether the two button slots are laid out: while at least one button
 * shows. When neither does (no dictation, or the shortcut's key is held) the
 * waveform takes the whole row rather than stopping short of an empty box.
 * While the × shows, the ✓'s slot stays laid out even when the ✓ hides, so
 * the × never moves under a pointer; the row's width changes only as the ×
 * itself appears or goes (key release, dictation end), never under a click.
 * The ✓ never shows without the ×, so this is the ×'s own rule in practice.
 * Untested: the app has no frontend test runner.
 */
function keepsButtonSlots({ cancel, finish }: { cancel: boolean; finish: boolean }): boolean {
  return cancel || finish;
}

function CancelButton({ shown }: { shown: boolean }) {
  return (
    <button
      type="button"
      className="overlay-button cancel"
      aria-label={overlayWords.cancelDictation}
      title={shown ? overlayWords.cancelDictation : undefined}
      {...slot(shown)}
      onClick={() => {
        // Nothing to show on a failure: the dictation runs on, and the
        // shortcut or the tray still ends it.
        dictationCancel().catch(() => {});
      }}
    >
      <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
        <path d="M2 2 L8 8 M8 2 L2 8" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
      </svg>
    </button>
  );
}

/**
 * Whether the ✓ shows: only while this dictation is recording, from
 * "listening" until Rust says the recording ended (`dictation-recording-ended`,
 * sent whatever ended it, and sooner as the held key comes up) or the
 * dictation is over. Not in the wait for the engine, a cloud provider or
 * Enhancing…, where the × stays. Held-key rule as for `showsCancel`. A click
 * that arrives late does nothing (Rust's `Dictator::finish`). Untested: the
 * app has no frontend test runner.
 */
function showsFinish({ recording, keyHeld }: { recording: boolean; keyHeld: boolean }): boolean {
  return recording && !keyHeld;
}

function FinishButton({ shown }: { shown: boolean }) {
  return (
    <button
      type="button"
      className="overlay-button finish"
      aria-label={overlayWords.finishDictation}
      title={shown ? overlayWords.finishDictation : undefined}
      {...slot(shown)}
      onClick={() => {
        // Nothing to show on a failure: the dictation runs on, and the
        // shortcut or the tray still ends it.
        dictationFinish().catch(() => {});
      }}
    >
      <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
        <path
          d="M2.5 6.3 L5 8.6 L9.5 3.6"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.8"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      </svg>
    </button>
  );
}

// How a dictation's cap reads in a sentence: "5-minute", "30-second".
function limit(seconds: number): string {
  return seconds >= 60 && seconds % 60 === 0 ? `${seconds / 60}-minute` : `${seconds}-second`;
}

/**
 * Everything the overlay draws from one dictation, in one object rather than
 * one `useState` each, so that what a dictation leaves behind is decided in
 * one place ([`keeps`]) and a piece added here cannot be cleared when a
 * dictation starts and forgotten when one ends.
 *
 * Not in here: whether a dictation is running or recording, and whether the
 * shortcut's key is held. Those are the overlay's own lifecycle, told by
 * Rust, and `keyHeld` in particular is never cleared by a dictation.
 */
export interface Shown {
  levels: DictationLevel[];
  text: string;
  empty: boolean;
  capped: number | null;
  /** The recording is with a cloud provider and no answer has come yet. */
  cloudWaiting: CloudWaiting | null;
  /**
   * The paste is waiting on Enhancement. The waveform and live words stay on
   * screen while this is true; only the hint line changes.
   */
  enhancing: boolean;
  /**
   * Where this dictation's words are going (the visible destination
   * label): the transcription provider, then where cleanup sends the text,
   * as in "ElevenLabs · Local"; either alone, or null when neither applies
   * -- or, when the stored choice could not be fully used, what is in use
   * and the short issue ("Apple Speech · OpenAI needs a key"). The
   * dictation thread composes the line.
   */
  destination: string | null;
  /**
   * A cloud result came back after the person moved to another app, so it
   * was not pasted: whether History kept it, or null for no such notice.
   */
  movedOn: boolean | null;
  /**
   * The paste could not be delivered -- Accessibility is not granted -- and
   * the whole sentence to show for it, or null for no such notice. Worded in
   * Rust (`paste_words.rs`), never here: the app's wording rule forbids the
   * same sentence living in both.
   */
  notPasted: string | null;
  /**
   * The text was pasted without the Enhancement it waited for, or null for
   * no such notice. Kept until its notice has been up long enough, like
   * `movedOn`.
   */
  missed: EnhancementMissed | null;
  /**
   * A dictation trigger was refused -- a live meeting has the microphone --
   * and the whole sentence to show for it, or null for no such notice.
   * Worded in Rust (`meeting_words.rs`), like `notPasted`.
   */
  refused: string | null;
}

/**
 * Nothing of any dictation: what the overlay holds while it is hidden, and
 * what it starts from. A fresh object every call, because the bars are an
 * array the waveform redraws on identity.
 */
export function nothing(): Shown {
  return {
    levels: Array(BARS).fill(SILENT),
    text: "",
    empty: false,
    capped: null,
    cloudWaiting: null,
    enhancing: false,
    destination: null,
    movedOn: null,
    notPasted: null,
    missed: null,
    refused: null,
  };
}

/** The three moments a dictation's state is thrown away. */
export type Moment = "started" | "ended" | "hidden";

/**
 * What the overlay keeps of the dictation that just ran, at `moment`. The
 * one place that decides it: a piece added to `Shown` goes at every moment
 * unless it is argued into surviving here, instead of being cleared when a
 * dictation starts and forgotten when one ends.
 *
 * - "started": a new dictation is listening, so nothing of the last one
 *   survives. This is the only moment `destination` goes. Belt and braces
 *   for the rest: the overlay is shown at about the same moment, and a frame
 *   painted before this arrives would show the last dictation's words, which
 *   is the bug this is all for.
 * - "hidden": Rust has hidden the overlay (`dictation-overlay-hidden`), so
 *   whatever was up has gone. Note that this is not once per dictation:
 *   `hide_overlay` runs when the recording ends, before any notice exists,
 *   and again from `hide_notice_later` when a notice's few seconds are up.
 *   Only the second of those clears a notice; the first is why `destination`
 *   cannot go here. It arrives before the notice does, and nothing re-emits
 *   the label, so clearing it would take the label off the moved-on notice,
 *   which today shows it -- that path emits no `dictation` event, which is
 *   what clears the label everywhere else.
 * - "ended": `dictation-state` has left "listening". The dictation is over,
 *   but the overlay can still be *up*, showing a notice that outlives it.
 *   The dictation thread emits that state last, after
 *   `dictation-not-pasted`, `dictation-moved-on` and
 *   `dictation-enhancement-missed` have each shown the overlay again for a
 *   few seconds (`dictation.rs`, `hide_notice_later`). Clearing those three
 *   here would blank the sentence the person is meant to read, milliseconds
 *   after it appeared.
 *
 * "ended" is belt and braces on every path traced today -- a hide always
 * comes first and has cleared the live state already. It is kept because it
 * is the one moment that does not depend on hearing a hide: a dictation that
 * ends without one still drops its waveform and words, while any notice on
 * screen stays readable.
 *
 * Everything else goes at "ended" as well as at "hidden": the waveform, the
 * live words, "No speech detected.", the cap notice and the waiting lines
 * are all either already hidden by then or held on screen by the dictation
 * thread's own wait, which finishes before it emits the state this handles.
 *
 * Pure, and the whole rule in one expression, because the app has no
 * frontend test runner: this is NOT covered by an automated test.
 */
export function keeps(shown: Shown, moment: Moment): Shown {
  if (moment === "started") {
    return nothing();
  }
  const kept: Shown = { ...nothing(), destination: shown.destination };
  if (moment === "hidden") {
    return kept;
  }
  return {
    ...kept,
    notPasted: shown.notPasted,
    movedOn: shown.movedOn,
    missed: shown.missed,
    refused: shown.refused,
  };
}

function Overlay() {
  const [shown, setShown] = useState<Shown>(nothing);
  // A dictation is under way and a Discard still reaches it (`showsCancel`).
  const [running, setRunning] = useState(false);
  // This dictation is still recording, so a Finish still stops it
  // (`showsFinish`).
  const [recording, setRecording] = useState(false);
  // The shortcut's key is held to dictate. Only Rust says so; nothing here
  // clears it, not even a new dictation (`showsCancel`).
  const [keyHeld, setKeyHeld] = useState(false);

  useEffect(() => {
    // Cleared together: a stale "waiting for X" or a stale destination from
    // the dictation that just ended must never bleed into the next one --
    // in particular, `keeps` must clear these at "started" before the fresh
    // `dictation-destination` for the new dictation arrives.
    const clearDestinationAndWaiting = () => {
      setShown((s) => ({ ...s, cloudWaiting: null, enhancing: false, destination: null }));
    };
    // The whole of this dictation, thrown away as one (`keeps`).
    const clear = (moment: Moment) => setShown((s) => keeps(s, moment));
    const subscriptions = [
      listen<DictationLevel>("dictation-level", (e) => {
        setShown((s) => ({ ...s, levels: [...s.levels.slice(1), e.payload] }));
      }),
      listen<string>("dictation-partial", (e) => setShown((s) => ({ ...s, text: e.payload }))),
      listen<boolean>("dictation-key-held", (e) => setKeyHeld(e.payload)),
      listen<null>("dictation-recording-ended", () => setRecording(false)),
      listen<null>("dictation-empty", () => {
        setShown((s) => ({ ...s, empty: true }));
        setRunning(false);
      }),
      listen<number>("dictation-capped", (e) => setShown((s) => ({ ...s, capped: e.payload }))),
      listen<string | null>("dictation-destination", (e) =>
        setShown((s) => ({ ...s, destination: e.payload })),
      ),
      listen<CloudWaiting>("dictation-cloud-waiting", (e) =>
        setShown((s) => ({ ...s, cloudWaiting: e.payload })),
      ),
      // No dictation started, so nothing else is on the overlay: only the
      // sentence, until Rust hides it.
      listen<string>("dictation-refused", (e) => {
        setShown(() => ({ ...nothing(), refused: e.payload }));
      }),
      listen<string>("dictation-not-pasted", (e) => {
        setShown((s) => ({ ...s, notPasted: e.payload }));
        setRunning(false);
      }),
      listen<boolean>("dictation-moved-on", (e) => {
        setShown((s) => ({ ...s, movedOn: e.payload }));
        setRunning(false);
      }),
      // The cloud answer is in by the time Enhancement is waited on. The
      // payload's contents no longer drive the overlay; only its arrival
      // does, so the event stays typed for the listener without being read.
      listen<Enhancing>("dictation-enhancing", () => {
        setShown((s) => ({ ...s, cloudWaiting: null, enhancing: true }));
      }),
      listen<EnhancementMissed>("dictation-enhancement-missed", (e) => {
        // Pasted without it: nothing left to discard.
        setRunning(false);
        setShown((s) => ({ ...s, enhancing: false, missed: e.payload }));
      }),
      // Pasted or thrown away: the dictation is over.
      listen<string>("dictation", () => {
        setRunning(false);
        clearDestinationAndWaiting();
      }),
      // Not an end on its own: "the shortcut cannot see your keys" can
      // arrive mid-dictation, which is when the × matters most. A failed
      // dictation still ends with `dictation-state` leaving "listening".
      listen<string>("dictation-error", clearDestinationAndWaiting),
      listen<null>("dictation-discarded", () => {
        setRunning(false);
        clearDestinationAndWaiting();
      }),
      // The overlay has gone, so it holds nothing to flash: the last
      // dictation's words, and any notice whose time is up, go now rather
      // than when the next dictation starts, which is too late to stop a
      // stale frame. Not the destination label -- see `keeps`.
      listen<null>("dictation-overlay-hidden", () => clear("hidden")),
      listen<string>("dictation-state", (e) => {
        if (e.payload !== "listening") {
          setRunning(false);
          setRecording(false);
          clear("ended");
        } else {
          setRunning(true);
          setRecording(true);
          clear("started");
        }
      }),
    ];
    return () => {
      subscriptions.forEach((s) => s.then((unlisten) => unlisten()));
    };
  }, []);

  const { levels, text, empty, capped, cloudWaiting, enhancing } = shown;
  const { destination, movedOn, notPasted, missed, refused } = shown;
  const partial =
    refused !== null
      ? refused
      : notPasted !== null
      ? notPasted
      : movedOn !== null
        ? movedOn
          ? "Not pasted, because another app is in front. It is in History."
          : "Not pasted, because another app is in front. History is off, so it was not kept."
        : cloudWaiting
          ? `Waiting for ${cloudWaiting.provider} (${Math.round(cloudWaiting.elapsed_ms / 1000)} s)` +
            (cloudWaiting.in_history ? ". What this Mac heard is in History." : "")
          : capped !== null
            ? `Stopped at the ${limit(capped)} limit. Anything after it was not heard.`
            : enhancing
              ? text
              : missed
                ? missedSentence(missed)
                : empty
                  ? "No speech detected."
                  : text || "Listening…";
  // A sentence about what happened to the text, too long for one line
  // beside the waveform, which has nothing left to show once the key is up.
  // Enhancement running is not one of these: the waveform stays, and the row
  // above it holds the live words if there are any, blank otherwise -- never
  // "Listening…", since the key is already up by then. Only the hint line
  // below changes.
  const notice =
    refused !== null ||
    notPasted !== null ||
    movedOn !== null ||
    cloudWaiting !== null ||
    missed !== null;
  const cancelShown = showsCancel({ running, keyHeld });
  const finishShown = showsFinish({ recording: running && recording, keyHeld });

  return (
    <div className="overlay">
      <div className="top">
        {!notice && <Waveform levels={levels} />}
        {keepsButtonSlots({ cancel: cancelShown, finish: finishShown }) && (
          <div className="overlay-buttons">
            <CancelButton shown={cancelShown} />
            <FinishButton shown={finishShown} />
          </div>
        )}
      </div>
      <p
        className={[
          "partial",
          notice ? "notice" : "",
          notice || cloudWaiting || capped !== null || enhancing || (text && !empty)
            ? ""
            : "placeholder",
        ]
          .filter(Boolean)
          .join(" ")}
      >
        {partial}
      </p>
      <p className="hint">
        <span className="left">
          {/* Not beside a notice: a moved-on paste ends the wait with no
              `dictation` event to clear `enhancing`. */}
          {enhancing && !notice ? overlayWords.enhancing : null}
        </span>
        {destination && <span className="where">{destination}</span>}
      </p>
    </div>
  );
}

const root = document.getElementById("root");
if (!root) {
  throw new Error("missing #root element");
}

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <Overlay />
  </React.StrictMode>,
);
