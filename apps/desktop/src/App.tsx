// SPDX-License-Identifier: MIT
import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  about,
  licenses,
  cleanupChoose,
  poolModels,
  dictionaryAdd,
  dictionaryExport,
  dictionaryImport,
  dictionaryInfo,
  dictionaryRemove,
  dictionaryTerms,
  dictionaryUpdate,
  dictionaryWords,
  type DictionaryWords,
  finishSetup,
  inputDevice,
  microphoneChoose,
  launchAtLogin,
  openLink,
  launchAtLoginSet,
  type LaunchAtLogin,
  microphones,
  type MicrophoneView,
  modelCancel,
  modelsDir,
  modelDownload,
  modelRemove,
  modelsList,
  modelsOverview,
  openrouterModels,
  providerKeyRemove,
  providerKeySave,
  serverModels,
  transcriptionChoose,
  historyCopy,
  historyDelete,
  historyList,
  historyRetention,
  historySendAgain,
  historySetRetention,
  unloadChoice,
  unloadChoose,
  openSettingsPane,
  requestPermission,
  revealPath,
  setupSteps,
  shortcut,
  shortcutChoose,
  shortcutRecord,
  shortcutView,
  type ShortcutView,
  type About,
  type LicenseFamily,
  type CleanupChoiceView,
  type PoolModelView,
  type PoolModelsView,
  type PoolProviderView,
  type CleanupOptionView,
  type DictionaryInfo,
  type MeasuredCleanupView,
  type ModelsOverview,
  type OpenedStatusView,
  type OpenRouterTranscriptionOfferView,
  type ProviderKeyView,
  type ProviderKeysView,
  type ServerModelsView,
  type TranscriptionOptionView,
  type TranscriptionStatus,
  type ModelError,
  type ModelProgress,
  type ModelView,
  type HistoryItem,
  type SentAgain,
  type RetentionView,
  type UnloadView,
  type StepSummary,
  type TermView,
  meetingCancel,
  meetingChooseFile,
  meetingClear,
  meetingCopy,
  meetingCurrent,
  meetingEditTurn,
  meetingCorrectionOffer,
  type CorrectionOffer,
  meetingRunningSummary,
  meetingDestinations,
  meetingBatch,
  meetingBatchClear,
  meetingBatchStart,
  meetingOpenFile,
  meetingSave,
  meetingSaveTranslation,
  meetingCopyTranslation,
  meetingSummarise,
  meetingTranslate,
  meetingTranslation,
  meetingTranslationCheck,
  meetingTranslationOptions,
  meetingRunningTranslation,
  meetingLiveStart,
  meetingLiveStop,
  meetingLiveWords,
  meetingRunningLive,
  type BatchChanged,
  type BatchView,
  type LiveProgress,
  type LiveWords,
  type RunningLive,
  type MeetingDestinations,
  type MeetingError,
  type MeetingItem,
  type MeetingTurn,
  type MeetingProgress,
  type SummaryProgress,
  type MeetingView,
  type TranscriptWords,
  type RunningTranslation,
  type TranslationFailed,
  type TranslationOptions,
  type TranslationProgress,
  type TranslationSaid,
  type TranslationView,
} from "./commands";

/** What the one window is showing. The tray picks; the page follows. */
type View =
  | "settings"
  | "history"
  | "files"
  | "dictionary"
  | "about"
  | "licenses"
  | "models";

/** Which card the Models page should land on, set by the Engine and
 * Enhancement cards' buttons and consumed once by {@link ModelsView}. */
type ModelsTarget = "transcription" | "enhancement" | null;

interface PlaceholderCard {
  title: string;
  detail: string;
  /** A button under the sentence -- the Enhancement card's own way onto the
   * Models page, beside the Engine card's. */
  button?: { label: string; onClick: () => void };
}

function PlaceholderSection({ title, detail, button }: PlaceholderCard) {
  return (
    <section className="card">
      <h2>{title}</h2>
      <p>{detail}</p>
      {button && (
        <button type="button" className="button" onClick={button.onClick}>
          {button.label}
        </button>
      )}
    </section>
  );
}

function transcriptionStatusLabel(status: TranscriptionStatus, detail: string | null): string {
  switch (status) {
    case "ready":
      return "Ready";
    case "not-compiled-in":
      return "Not compiled in";
    case "needs-download":
      return "Needs a download";
    case "needs-key":
      return "Add a key under Providers to use this.";
    case "key-store-unavailable":
      // Same plain sentence the Providers card's banner uses -- the raw OS
      // reason alone reads like a crash, not a fact about this machine.
      return `Macalla can't read the Keychain right now, so this can't be used.${detail ? ` (${detail})` : ""}`;
    case "key-unreadable":
    case "unavailable":
      return detail ?? "Not available";
  }
}

/** Words wrong across the everyday, technical and clinical clips, before the
 * dictionary. Up to 4%
 * five dots, 8% four, 12% three, more than that two; nobody measured gets
 * none. The one mapping function every accuracy mark on this page shares. */
function sttDots(wrong: readonly number[] | null, clipWords: number): number {
  if (!wrong) {
    return 0;
  }
  const p = wrong.reduce((a, b) => a + b, 0) / clipWords;
  return p <= 0.04 ? 5 : p <= 0.08 ? 4 : p <= 0.12 ? 3 : 2;
}

/** The everyday, technical and clinical clips a local engine or a named
 * provider is measured on (docs/benchmarks.md). OpenRouter's own offers
 * carry their own clip word count instead, since a model's `measured` there
 * can be another maker's own run. */
const CLIP_WORDS = 174;

function Pips({ wrong, clipWords = CLIP_WORDS }: { wrong: readonly number[] | null; clipWords?: number }) {
  const n = sttDots(wrong, clipWords);
  if (!n) {
    return <span className="pips-na">not measured</span>;
  }
  const sum = (wrong ?? []).reduce((a, b) => a + b, 0);
  const title = `Measured: ${sum} words wrong of ${clipWords}, before the dictionary`;
  return (
    <span className="pips" role="img" aria-label={`Accuracy ${n} of 5`} title={title}>
      {[1, 2, 3, 4, 5].map((i) => (
        <i key={i} className={i <= n ? "on" : undefined} />
      ))}
    </span>
  );
}

/** Right out of 20 fixtures, cleanup accuracy: mirrors the mockup's
 * `cleanDots` -- 18 or more of 20 five dots, 16-17 four, 13-15 three, fewer
 * two. A different scale from `sttDots`: 20 cleanup fixtures, not 174
 * transcription words. */
function cleanDots(right: number): number {
  return right >= 18 ? 5 : right >= 16 ? 4 : right >= 13 ? 3 : 2;
}

function CleanupPips({ measured }: { measured: MeasuredCleanupView | null }) {
  if (!measured) {
    return <span className="pips-na">not measured</span>;
  }
  const n = cleanDots(measured.right);
  const title = `Measured on ${measured.machine}: right ${measured.right} of ${measured.of}`;
  return (
    <span className="pips" role="img" aria-label={`Accuracy ${n} of 5`} title={title}>
      {[1, 2, 3, 4, 5].map((i) => (
        <i key={i} className={i <= n ? "on" : undefined} />
      ))}
    </span>
  );
}

/** The brand mark's geometry, cropped to the trail. Rendered once per page that uses
 * `RouteMark`; `<use href="#trail">` finds it by id wherever it sits in the
 * document. */
function RouteSymbol() {
  return (
    <svg width="0" height="0" style={{ position: "absolute" }} aria-hidden="true">
      <defs>
        <symbol id="trail" viewBox="0 0 820 512">
          <circle cx="196" cy="256" r="196" className="route-src" />
          <circle cx="484" cy="256" r="104" className="route-echo" style={{ opacity: "var(--m-e1, 1)" }} />
          <circle cx="658" cy="256" r="62" className="route-echo" style={{ opacity: "calc(0.6 * var(--m-e2, 1))" }} />
          <circle cx="772" cy="256" r="34" className="route-echo" style={{ opacity: "calc(0.32 * var(--m-e3, 1))" }} />
        </symbol>
      </defs>
    </svg>
  );
}

/** Where words go: the dot alone for `"mac"`, the whole trail for
 * `"provider"`. */
function RouteMark({ to, label }: { to: "mac" | "provider"; label: string }) {
  return (
    <span className="route" data-to={to}>
      <svg aria-hidden="true">
        <use href="#trail" />
      </svg>
      {label}
    </span>
  );
}

/**
 * One local engine on the Transcription card: its name, the line on what it
 * is good and weak at, its accuracy dots, where it stands, and the actions
 * that fit. Moved here from the old Engine card verbatim -- every behaviour
 * (download progress, reconnecting, preparing, resume, cancel, remove) is
 * unchanged, only reading from `modelsOverview` and choosing through
 * `transcriptionChoose` rather than the two engine-only commands this page
 * replaced.
 */
function LocalEngineRow({
  option,
  isPreview,
  model,
  progress,
  preparing,
  busy,
  runChoice,
  onChange,
}: {
  option: TranscriptionOptionView;
  /** This is the on-device engine showing the live preview while a cloud
   * provider transcribes the whole recording -- not "in use" for the
   * dictation's own text, but not idle either. */
  isPreview: boolean;
  model: ModelView | null;
  progress: ModelProgress | null;
  preparing: boolean;
  /** A choose is already in flight somewhere on the page -- disables every
   * Use button, not just this row's, so two overlapping chooses can never
   * race each other's write to the stored choice. */
  busy: boolean;
  runChoice: (run: () => Promise<void>) => Promise<void>;
  onChange: () => void;
}) {
  const [error, setError] = useState<string | null>(null);
  // Preparing can take a while for a large model, and a bare "Preparing…"
  // stops meaning anything after a few seconds. Say what it is doing once
  // it has run long enough to need saying.
  const isPreparing = progress?.phase === "preparing" || (preparing && option.chosen);
  const [preparingLong, setPreparingLong] = useState(false);
  useEffect(() => {
    if (!isPreparing) {
      setPreparingLong(false);
      return;
    }
    const timer = window.setTimeout(() => setPreparingLong(true), 3000);
    return () => window.clearTimeout(timer);
  }, [isPreparing]);
  const act = (run: () => Promise<void>) => {
    setError(null);
    runChoice(run)
      .then(onChange)
      .catch((e: unknown) => setError(String(e)));
  };
  const button = (label: string, run: () => Promise<void>, disabled = false) => (
    <button
      key={label}
      type="button"
      className="button"
      disabled={disabled}
      onClick={() => act(run)}
    >
      {label}
    </button>
  );
  let state: string | null = null;
  const actions: React.ReactNode[] = [];
  if (progress?.phase === "verifying") {
    state = "Checking the file…";
  } else if (isPreparing) {
    state = preparingLong
      ? `Loading ${option.name} into memory; this can take a moment.`
      : "Preparing the model for this computer…";
  } else if (model && progress?.phase === "reconnecting") {
    state = `The connection stalled. Reconnecting, ${Math.floor(progress.bytes / 1_000_000)} of ${model.megabytes} MB…`;
    actions.push(button("Cancel", () => modelCancel(model.id)));
  } else if (model && (progress || model.state === "downloading")) {
    const bytes = progress?.bytes ?? model.bytes;
    const total = progress?.total ?? model.megabytes * 1_000_000;
    const pct = total > 0 ? Math.floor((bytes * 100) / total) : 0;
    state = `Downloading, ${Math.floor(bytes / 1_000_000)} of ${model.megabytes} MB (${pct}%)`;
    actions.push(button("Cancel", () => modelCancel(model.id)));
  } else if (model && model.state === "partial") {
    state = `Partly downloaded, ${Math.floor(model.bytes / 1_000_000)} of ${model.megabytes} MB.`;
    actions.push(button("Resume", () => modelDownload(model.id)));
  } else if (model && model.state === "absent" && option.status === "needs-download") {
    actions.push(button("Download", () => modelDownload(model.id)));
  } else if (option.status === "ready") {
    if (!option.chosen && !isPreview) {
      actions.push(
        button("Use", () => transcriptionChoose({ source: "local", id: option.id }), busy),
      );
      if (model) {
        actions.push(button("Remove", () => modelRemove(model.id)));
      }
    }
  } else {
    state = transcriptionStatusLabel(option.status, option.detail);
  }
  return (
    <li className="engine-row">
      <div className="item-text">
        <span className="item-name">
          {option.name}
          {option.chosen && <span className="engine-in-use"> · in use</span>}
          {!option.chosen && isPreview && <span className="engine-in-use"> · live preview</span>}
        </span>
        <span className="item-status">{option.blurb}</span>
        <Pips wrong={option.measured_words_wrong} />
        {state && <span className="item-status">{state}</span>}
        {error && <span className="card-error">{error}</span>}
      </div>
      {actions.length > 0 && <div className="row-actions">{actions}</div>}
    </li>
  );
}

/** Options under their maker's heading, in the order the registry lists them. */
function byGroup<T extends { group: string }>(items: T[]): [string, T[]][] {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    groups.set(item.group, [...(groups.get(item.group) ?? []), item]);
  }
  return Array.from(groups.entries());
}

/** The Engine card, kept as a settings section but no longer holding the
 * download machinery: it is one line, from the same `modelsOverview` call
 * the Models page makes, so the two can never disagree. */
function EngineCard({ onOpenModels }: { onOpenModels: () => void }) {
  const [overview, setOverview] = useState<ModelsOverview | null>(null);
  const [unload, setUnload] = useState<UnloadView | null>(null);
  const [unloadError, setUnloadError] = useState<string | null>(null);

  useEffect(() => {
    const refresh = () => modelsOverview().then(setOverview).catch(() => undefined);
    refresh();
    unloadChoice().then(setUnload).catch(() => undefined);
    const subscription = listen<string>("dictation-state", (e) => {
      if (e.payload === "idle") {
        refresh();
      }
    });
    return () => {
      subscription.then((unlisten) => unlisten());
    };
  }, []);

  const chooseUnload = (value: string) => {
    unloadChoose(value)
      .then(() => unloadChoice())
      .then((view) => {
        setUnload(view);
        setUnloadError(null);
      })
      .catch((e: unknown) => setUnloadError(String(e)));
  };

  const chosen = overview?.transcription.find((t) => t.chosen) ?? null;
  const summary = !chosen
    ? "Choose the engine that turns your voice into text."
    : chosen.source === "local"
      ? `${chosen.name}, running locally.`
      : `${chosen.name} writes the text from your recording, sent when you stop dictating.`;

  return (
    <section className="card">
      <h2>Engine</h2>
      <p>{summary}</p>
      <button type="button" className="button" onClick={onOpenModels}>
        Open Transcription on Models
      </button>
      <p>
        Free the model's memory when you haven't dictated for a while. It loads
        again on your next dictation.
      </p>
      {unload && (
        <label className="setting-row">
          <span>Unload model</span>
          <select
            className="select"
            value={unload.current}
            onChange={(e) => chooseUnload(e.target.value)}
          >
            {unload.choices.map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </select>
        </label>
      )}
      <p className="card-meta">
        Only downloaded engines are unloaded. Apple Speech is built into macOS,
        with nothing to free.
      </p>
      {unloadError && <p className="card-error">{unloadError}</p>}
    </section>
  );
}

/** Price lines, plain copy from each provider's published pricing. */
const PROVIDER_PRICES: Record<string, string> = {
  openai: "Transcription $0.0045 a minute; Enhancement priced per token",
  anthropic: "Enhancement priced per token",
  gemini: "Transcription about $0.005 a minute; Enhancement priced per token",
  mistral: "Transcription $0.003 a minute; Enhancement priced per token",
  "ollama-cloud": "Priced per token, with a surcharge 12:00 to 18:00 UTC on weekdays",
  openrouter: "Priced per model; each reply reports its cost",
  elevenlabs: "Billed by the hour of audio",
  cohere: "Trial keys are free, five requests a minute",
};

/** A provider's default transcription model and blurb, from the mockup's
 * CLOUD_STT table. OpenRouter is not here: it has no single default model,
 * which is the whole point of its own "Choose model" row. */
const CLOUD_STT: Record<string, { model: string; blurb: string }> = {
  openai: {
    model: "gpt-transcribe",
    blurb: "Uses your Dictionary. Your text arrives shortly after you stop dictating.",
  },
  gemini: {
    model: "gemini-3.5-transcribe",
    blurb: "Uses your Dictionary. Your text takes several seconds to arrive after you stop dictating.",
  },
  mistral: {
    model: "voxtral-mini-latest",
    blurb: "Uses your Dictionary. Your text arrives shortly after you stop dictating.",
  },
  elevenlabs: {
    model: "scribe_v2",
    blurb:
      "Uses your Dictionary, which adds 20% to the cost. Your text arrives shortly after you stop dictating.",
  },
  cohere: {
    model: "cohere-transcribe-03-2026",
    blurb:
      "Needs the language set, and doesn't use your Dictionary. Your text arrives shortly after you stop dictating.",
  },
};

function providerNames(names: string[]): string {
  return names.length < 2 ? names.join("") : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/** The wire kind ("text" / "transcription") as a person reads it. */
function kindLabel(kind: string): string {
  switch (kind) {
    case "text":
      return "Enhancement";
    case "transcription":
      return "Transcription";
    default:
      return kind;
  }
}

/** One line collapsing every provider that offers transcription but has no
 * saved key yet, with a link back to the Providers card. */
function CollapsedProvidersLine({ names, onJump }: { names: string[]; onJump: () => void }) {
  if (names.length === 0) {
    return null;
  }
  const verb = names.length === 1 ? "also transcribes" : "also transcribe";
  return (
    <p className="more-line">
      {providerNames(names)} {verb}.{" "}
      <button type="button" className="link" onClick={onJump}>
        Add a key under Providers
      </button>
      .
    </p>
  );
}

/** One provider with a saved key, under Transcription's "Your providers"
 * group: name, default model, blurb, dots and Use. */
function ProviderTranscriptionRow({
  option,
  busy,
  runChoice,
  onChange,
}: {
  option: TranscriptionOptionView;
  busy: boolean;
  runChoice: (run: () => Promise<void>) => Promise<void>;
  onChange: () => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const info = CLOUD_STT[option.id];
  const use = () => {
    setError(null);
    runChoice(() => transcriptionChoose({ source: "provider", id: option.id }))
      .then(onChange)
      .catch((e: unknown) => setError(String(e)));
  };
  return (
    <li className="engine-row">
      <div className="item-text">
        <span className="item-name">
          {option.name}
          {info && info.model !== option.name && ` · ${info.model}`}
          {option.chosen && <span className="engine-in-use"> · in use</span>}
        </span>
        <span className="item-status">{info?.blurb ?? ""}</span>
        <Pips wrong={option.measured_words_wrong} />
        {error && <span className="card-error">{error}</span>}
      </div>
      {!option.chosen && (
        <div className="row-actions">
          <button type="button" className="button" disabled={busy} onClick={use}>
            Use
          </button>
        </div>
      )}
    </li>
  );
}

/** OpenRouter's transcription row: collapsed it just says how many models
 * are behind the key; "Choose model" expands the list, fetched from
 * OpenRouter's own listing and grouped by maker. */
function OpenRouterTranscriptionRow({
  option,
  busy,
  runChoice,
  onChange,
}: {
  option: TranscriptionOptionView;
  busy: boolean;
  runChoice: (run: () => Promise<void>) => Promise<void>;
  onChange: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [offers, setOffers] = useState<OpenRouterTranscriptionOfferView[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  // Seeded from the stored choice's own model id, when this row is the one
  // chosen -- reopening the page (or re-rendering after a Use elsewhere)
  // must not lose track of which OpenRouter model is actually running.
  const [model, setModel] = useState<string | null>(option.model);
  const [useError, setUseError] = useState<string | null>(null);

  const load = (refresh: boolean) => {
    setLoading(true);
    setLoadError(null);
    openrouterModels("transcription", refresh)
      .then((view) => {
        if (view.kind === "transcription") {
          setOffers(view.offers);
        }
      })
      .catch((e: unknown) => setLoadError(String(e)))
      .finally(() => setLoading(false));
  };

  const toggle = () => {
    const next = !open;
    setOpen(next);
    if (next && offers === null) {
      load(false);
    }
  };

  const use = () => {
    if (!model) {
      return;
    }
    setUseError(null);
    runChoice(() => transcriptionChoose({ source: "provider", id: "openrouter", model }))
      .then(onChange)
      .catch((e: unknown) => setUseError(String(e)));
  };

  const groups = byGroup(
    (offers ?? []).map((offer) => ({ ...offer, group: offer.name.split(":")[0] })),
  );

  return (
    <li className="engine-row or-row">
      <div className="item-text">
        <span className="item-name">
          OpenRouter
          {model && ` · ${model}`}
          {option.chosen && <span className="engine-in-use"> · in use</span>}
        </span>
        <span className="item-status">
          Many transcription models with one key; choose one. Marks show only for models Macalla
          measured.
        </span>
        {useError && <span className="card-error">{useError}</span>}
      </div>
      <div className="row-actions">
        {model && !option.chosen && (
          <button type="button" className="button" disabled={busy} onClick={use}>
            Use
          </button>
        )}
        <button type="button" className="button" onClick={toggle}>
          {open ? "Hide list" : "Choose model"}
        </button>
      </div>
      {open && (
        <div className="or-picker">
          {loading && <p className="card-meta">Loading…</p>}
          {loadError && <p className="card-error">{loadError}</p>}
          {offers && (
            <>
              <div className="or-meta">
                <span>From OpenRouter's own listing.</span>
                <button type="button" className="link" onClick={() => load(true)}>
                  Refresh
                </button>
              </div>
              {groups.map(([maker, models]) => (
                <div key={maker}>
                  <div className="pool-provider">{maker}</div>
                  {models.map((offer) => (
                    <label key={offer.id} className="or-option">
                      <input
                        type="radio"
                        name="openrouter-transcription-model"
                        value={offer.id}
                        checked={model === offer.id}
                        onChange={() => setModel(offer.id)}
                      />
                      <span>
                        {offer.name.split(": ").slice(1).join(": ")}
                        {offer.also_runs_locally && <span className="kind">Also runs locally</span>}
                        {offer.name.split(": ").slice(1).join(": ") !== offer.id && (
                          <span className="or-sub">{offer.id}</span>
                        )}
                      </span>
                      <Pips
                        wrong={offer.measured?.words_wrong ?? null}
                        clipWords={
                          offer.measured
                            ? offer.measured.clip_words.reduce((a, b) => a + b, 0)
                            : CLIP_WORDS
                        }
                      />
                    </label>
                  ))}
                </div>
              ))}
            </>
          )}
        </div>
      )}
    </li>
  );
}

/**
 * Provider keys, in one place. Only providers with a saved key appear under
 * Transcription; add one here and it appears there.
 */
function ProvidersCard({
  keys,
  onChange,
  cardRef,
}: {
  keys: ProviderKeysView;
  onChange: () => void;
  cardRef: React.RefObject<HTMLElement | null>;
}) {
  const [adding, setAdding] = useState("");
  const [addKey, setAddKey] = useState("");
  const [addBusy, setAddBusy] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);
  const [replacing, setReplacing] = useState<string | null>(null);
  const [replaceKey, setReplaceKey] = useState("");
  const [replaceBusy, setReplaceBusy] = useState(false);
  const [replaceError, setReplaceError] = useState<string | null>(null);
  const [removeError, setRemoveError] = useState<string | null>(null);

  const keyed = keys.providers.filter((p) => p.saved);
  const unreadable = keys.providers.filter((p) => !p.saved && p.error);
  const missing = keys.providers.filter((p) => !p.saved && !p.error);

  useEffect(() => {
    if (missing.length > 0 && !missing.some((p) => p.id === adding)) {
      setAdding(missing[0].id);
    }
  }, [keys, adding, missing]);

  const saveNew = () => {
    setAddBusy(true);
    setAddError(null);
    providerKeySave(adding, addKey)
      .then(() => {
        setAddKey("");
        onChange();
      })
      .catch((e: unknown) => setAddError(String(e)))
      .finally(() => setAddBusy(false));
  };

  const saveReplace = (id: string) => {
    setReplaceBusy(true);
    setReplaceError(null);
    providerKeySave(id, replaceKey)
      .then(() => {
        setReplacing(null);
        setReplaceKey("");
        onChange();
      })
      .catch((e: unknown) => setReplaceError(String(e)))
      .finally(() => setReplaceBusy(false));
  };

  const remove = (id: string) => {
    setRemoveError(null);
    providerKeyRemove(id).then(onChange).catch((e: unknown) => setRemoveError(String(e)));
  };

  return (
    <section className="card" ref={cardRef}>
      <h2>Providers</h2>
      <p>
        Add a provider's key once, and it appears under Transcription. Keys stay in this machine's
        Keychain and are never shown again.
      </p>
      {keys.store_unavailable && (
        <p className="banner">
          Macalla can't read the Keychain right now, so no key can be added or used. (
          {keys.store_unavailable})
        </p>
      )}
      {removeError && <p className="card-error">{removeError}</p>}
      {keyed.length > 0 && (
        <ul className="engine-list">
          {keyed.map((p) => (
            <li className="engine-row" key={p.id}>
              <div className="item-text">
                <span className="item-name">
                  {p.name}
                  {p.kinds.map((k) => (
                    <span key={k} className="kind">
                      {kindLabel(k)}
                    </span>
                  ))}
                  <span className="chip">In Keychain</span>
                </span>
                <span className="item-status">{PROVIDER_PRICES[p.id] ?? ""}</span>
                {replacing === p.id && (
                  <div className="row-actions">
                    <input
                      className="field"
                      type="password"
                      autoComplete="off"
                      value={replaceKey}
                      onChange={(e) => setReplaceKey(e.target.value)}
                      placeholder={`Paste a new ${p.name} key`}
                    />
                    <button
                      type="button"
                      className="button"
                      disabled={replaceBusy || replaceKey.trim() === ""}
                      onClick={() => saveReplace(p.id)}
                    >
                      Save to Keychain
                    </button>
                    <button
                      type="button"
                      className="button"
                      onClick={() => {
                        setReplacing(null);
                        setReplaceKey("");
                      }}
                    >
                      Cancel
                    </button>
                  </div>
                )}
                {replacing === p.id && replaceError && <span className="card-error">{replaceError}</span>}
              </div>
              {replacing !== p.id && (
                <div className="row-actions">
                  <button
                    type="button"
                    className="button"
                    onClick={() => {
                      setReplacing(p.id);
                      setReplaceKey("");
                      setReplaceError(null);
                    }}
                  >
                    Replace
                  </button>
                  <button type="button" className="button" onClick={() => remove(p.id)}>
                    Remove
                  </button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
      {unreadable.length > 0 && (
        <ul className="engine-list">
          {unreadable.map((p) => (
            <li className="engine-row" key={p.id}>
              <div className="item-text">
                <span className="item-name">{p.name}</span>
                <span className="card-error">{p.error}</span>
              </div>
            </li>
          ))}
        </ul>
      )}
      {!keys.store_unavailable && missing.length > 0 && (
        <div className="add-provider">
          {keyed.length === 0 && (
            <p className="card-meta">No provider keys yet. Everything below runs on this machine.</p>
          )}
          <label className="setting-row">
            <span>Provider</span>
            <select className="select" value={adding} onChange={(e) => setAdding(e.target.value)}>
              {missing.map((p) => (
                <option key={p.id} value={p.id}>
                  {`${p.name} · ${p.kinds.map(kindLabel).join(" and ")}`}
                </option>
              ))}
            </select>
          </label>
          <label className="setting-row">
            <span>Key</span>
            <input
              className="field"
              type="password"
              autoComplete="off"
              value={addKey}
              onChange={(e) => setAddKey(e.target.value)}
              placeholder="Paste the key"
            />
            <button
              type="button"
              className="button"
              disabled={addBusy || addKey.trim() === ""}
              onClick={saveNew}
            >
              Save to Keychain
            </button>
          </label>
          {addError && <p className="card-error">{addError}</p>}
          <p className="card-meta">{PROVIDER_PRICES[adding] ?? ""}</p>
        </div>
      )}
    </section>
  );
}

/** What is transcribing right now, and, when it is a provider, what shows
 * the words while the person is still speaking. */
function InUseCard({
  chosen,
  previewName,
  inUse,
  preparing,
}: {
  chosen: TranscriptionOptionView | null;
  previewName: string | null;
  /** What dictation actually opened with; `null` before the first open, in
   * which case `chosen` (the stored choice) is shown with no caveat. */
  inUse: OpenedStatusView | null;
  /** A dictation just asked to open is still loading, so `inUse` may
   * already be stale for it. */
  preparing: boolean;
}) {
  return (
    <section className="card">
      <h2>In use</h2>
      {chosen ? (
        <dl className="in-use">
          <dt>Transcription</dt>
          <dd>
            <b>
              {chosen.name}
              {chosen.model && ` · ${chosen.model}`}
            </b>
            <RouteMark
              to={chosen.source === "provider" ? "provider" : "mac"}
              label={chosen.source === "provider" ? `Sent to ${chosen.name}` : "Local"}
            />
            {chosen.source === "provider" && (
              <span className="card-meta">
                {previewName
                  ? `${previewName} shows the words as you speak.`
                  : "Apple Speech shows the words as you speak."}
              </span>
            )}
          </dd>
        </dl>
      ) : (
        <p className="card-meta">Loading…</p>
      )}
      {/* Outside the `chosen` branch: the stored transcription id can
       * match no option at all (a provider or a local engine not in
       * this build), leaving `chosen` `null` and the card showing
       * "Loading…" forever -- the fallback reason still belongs on the
       * page then, since it is the only place a person would see it. */}
      {!preparing && inUse?.transcription_reason && (
        <p className="card-error">{inUse.transcription_reason}</p>
      )}
    </section>
  );
}

/** The models directory path, able to wrap only after a separator ("/",
 * or "\\" on Windows). A plain string either overflows the card or (with
 * `overflow-wrap: anywhere`) breaks mid-word inside a folder name; a `<wbr>`
 * after each separator gives the browser a wrap point there and nowhere
 * else. */
function PathWithBreaks({ path }: { path: string }) {
  // Each piece keeps its trailing separator, if it has one. Spaces become
  // no-break spaces, so "Application Support" does not wrap in the middle
  // either; the text is for reading, and "Show in Finder" opens the folder.
  const pieces = (path.match(/[^/\\]*[/\\]|[^/\\]+$/g) ?? [path]).map((piece) =>
    piece.replace(/ /g, "\u00a0"),
  );
  return (
    <>
      {pieces.map((piece, i) => (
        <Fragment key={i}>
          {piece}
          {i < pieces.length - 1 && <wbr />}
        </Fragment>
      ))}
    </>
  );
}

/**
 * Transcription: the local engines (moved here from the old Engine card),
 * every provider with a saved key, and OpenRouter's own row.
 */
function TranscriptionCard({
  local,
  providers,
  openrouter,
  compiledIn,
  previewId,
  models,
  dir,
  progress,
  preparing,
  busy,
  runChoice,
  onChange,
  onJumpToProviders,
  cardRef,
}: {
  local: TranscriptionOptionView[];
  providers: TranscriptionOptionView[];
  openrouter: TranscriptionOptionView | null;
  compiledIn: boolean;
  previewId: string;
  models: ModelView[];
  dir: string | null;
  progress: Record<string, ModelProgress>;
  preparing: boolean;
  busy: boolean;
  runChoice: (run: () => Promise<void>) => Promise<void>;
  onChange: () => void;
  onJumpToProviders: () => void;
  cardRef: React.RefObject<HTMLElement | null>;
}) {
  const ready = providers.filter((p) => p.status === "ready");
  const keyless = providers.filter((p) => p.status !== "ready");
  const openrouterReady = openrouter?.status === "ready";
  const missingNames = [
    ...keyless.map((p) => p.name),
    ...(openrouter && !openrouterReady ? [openrouter.name] : []),
  ];
  const previewName = local.find((e) => e.id === previewId)?.name ?? "Apple Speech";

  return (
    <section className="card" ref={cardRef}>
      <h2>Transcription</h2>
      <p>One engine turns your voice into text. Choosing another replaces it.</p>
      <h3 className="engine-group-name">Local</h3>
      <ul className="engine-list">
        {local.map((option) => (
          <LocalEngineRow
            key={option.id}
            option={option}
            isPreview={previewId === option.id && !option.chosen}
            model={models.find((m) => m.id === option.model) ?? null}
            progress={option.model ? (progress[option.model] ?? null) : null}
            preparing={preparing}
            busy={busy}
            runChoice={runChoice}
            onChange={onChange}
          />
        ))}
      </ul>
      {dir && (
        <p className="model-dir">
          Models are kept in{" "}
          <code>
            <PathWithBreaks path={dir} />
          </code>
          .{" "}
          <button type="button" className="link" onClick={() => revealPath(dir)}>
            Show in Finder
          </button>
        </p>
      )}
      {!compiledIn ? (
        <p className="more-line">
          This copy of Macalla was built without cloud transcription, so only local engines are
          available.
        </p>
      ) : (
        <>
          {ready.length > 0 && (
            <>
              <h3 className="engine-group-name">Your providers</h3>
              <p className="card-meta">
                The whole recording goes to the provider when you stop dictating. {previewName}{" "}
                still shows your words while you speak.
              </p>
              <ul className="engine-list">
                {ready.map((option) => (
                  <ProviderTranscriptionRow
                    key={option.id}
                    option={option}
                    busy={busy}
                    runChoice={runChoice}
                    onChange={onChange}
                  />
                ))}
              </ul>
            </>
          )}
          {openrouterReady && openrouter && (
            <ul className="engine-list">
              <OpenRouterTranscriptionRow
                option={openrouter}
                busy={busy}
                runChoice={runChoice}
                onChange={onChange}
              />
            </ul>
          )}
          <CollapsedProvidersLine names={missingNames} onJump={onJumpToProviders} />
        </>
      )}
    </section>
  );
}

/** A model's size and quantisation, when Ollama's own routes gave them
 * (another OpenAI-compatible
 * server lists names only). */
function serverModelDetail(model: { parameter_size: string | null; quantization: string | null; size_bytes: number | null }): string | null {
  const parts: string[] = [];
  if (model.parameter_size) {
    parts.push(model.parameter_size);
  }
  if (model.quantization) {
    parts.push(model.quantization);
  }
  if (model.size_bytes) {
    parts.push(`${(model.size_bytes / 1_000_000_000).toFixed(1)} GB`);
  }
  return parts.length > 0 ? parts.join(" · ") : null;
}

/** The server cleanup option's own panel: the address, Check, and once
 * checked, the model list. Choosing a model is what actually saves the
 * choice -- the radio above only opens this panel. */
function ServerCleanupPanel({
  saved,
  busy,
  runChoice,
  onChange,
}: {
  /** The saved choice's own address and model, when it names a server. */
  saved: { address: string; model: string } | null;
  busy: boolean;
  runChoice: (run: () => Promise<void>) => Promise<void>;
  onChange: () => void;
}) {
  const [address, setAddress] = useState(saved?.address ?? "");
  const [checking, setChecking] = useState(false);
  const [checkError, setCheckError] = useState<string | null>(null);
  const [result, setResult] = useState<ServerModelsView | null>(null);
  // The address `result` (or `checkError`) actually answers, trimmed the
  // same way `check` trims before sending. `null` until a check completes,
  // or once the address changes out from under it -- the guard against the
  // MAJOR review finding: showing or choosing from a list fetched for an
  // address that is no longer what is typed.
  const [checkedAddress, setCheckedAddress] = useState<string | null>(null);
  const [useError, setUseError] = useState<string | null>(null);
  // Bumped on every `check()`; a response is applied only while it is still
  // the most recent one. Guards the other race the review named: a second
  // Check (same or different address) started before the first's response
  // arrived, where comparing addresses alone would not catch a same-address
  // reorder.
  const requestSeq = useRef(0);

  const changeAddress = (value: string) => {
    setAddress(value);
    // The previous check no longer answers for what is now typed; clear it
    // rather than let a stale list or error sit under a changed address.
    setResult(null);
    setCheckError(null);
    setCheckedAddress(null);
  };

  const check = () => {
    const trimmed = address.trim();
    const id = ++requestSeq.current;
    setChecking(true);
    setCheckError(null);
    setResult(null);
    setCheckedAddress(null);
    serverModels(trimmed)
      .then((view) => {
        if (id !== requestSeq.current) {
          return; // superseded by a later Check before this one answered
        }
        setResult(view);
        setCheckedAddress(trimmed);
      })
      .catch((e: unknown) => {
        if (id !== requestSeq.current) {
          return;
        }
        setCheckError(String(e));
      })
      .finally(() => {
        if (id === requestSeq.current) {
          setChecking(false);
        }
      });
  };

  const choose = (model: string) => {
    setUseError(null);
    // Trimmed, as `check` sent it: a model is only offered while the list
    // answers this trimmed address, so that is the address to save.
    runChoice(() => cleanupChoose({ kind: "server", address: address.trim(), model }))
      .then(onChange)
      .catch((e: unknown) => setUseError(String(e)));
  };

  // Belt and braces alongside clearing on every edit above: even if a stale
  // response ever slipped past `requestSeq`, nothing renders unless the
  // address it answered for still matches what is typed now.
  const listCurrent = checkedAddress !== null && checkedAddress === address.trim();

  return (
    <div className="choice-more">
      <div className="inline-field">
        <span>Server address</span>
        <input
          className="field mono"
          placeholder="http://localhost:11434"
          aria-label="Server address"
          value={address}
          onChange={(e) => changeAddress(e.target.value)}
        />
        <button
          type="button"
          className="button"
          disabled={checking || address.trim().length === 0}
          onClick={check}
        >
          Check
        </button>
      </div>
      <span className="card-meta">
        Ollama on this computer answers here unless you changed it. On another computer, Mac,
        Windows or Linux, use its name or address with the same port, 11434.
      </span>
      {saved && (
        <span className="card-meta">
          In use: <span className="mono">{saved.model}</span> at <span className="mono">{saved.address}</span>
        </span>
      )}
      {!checking && checkedAddress === null && !checkError && (
        <p className="card-meta">Check this address to see its models.</p>
      )}
      {checkError && <p className="card-error">{checkError}</p>}
      {listCurrent && result && result.models.length === 0 && (
        <p className="card-meta">Your server has no text models.</p>
      )}
      {listCurrent && result && result.models.length > 0 && (
        <>
          <div className="or-meta">
            <span>{result.models.length} models on your server</span>
            {result.loopback && <span className="chip">Local</span>}
          </div>
          <ul className="pool">
            {result.models.map((model) => (
              <li key={model.id}>
                <label>
                  <input
                    type="radio"
                    name="cleanup-server-model"
                    disabled={busy}
                    checked={saved?.address === address && saved?.model === model.id}
                    onChange={() => choose(model.id)}
                  />
                  <span>
                    <span className="mono">{model.id}</span>
                    {serverModelDetail(model) && (
                      <span className="or-sub">{serverModelDetail(model)}</span>
                    )}
                  </span>
                  <CleanupPips measured={model.measured} />
                </label>
              </li>
            ))}
          </ul>
          {!result.loopback && (
            <p className="card-meta">
              Your words are sent to this server once they are ready.
            </p>
          )}
        </>
      )}
      {useError && <p className="card-error">{useError}</p>}
    </div>
  );
}

/** Whether two ticked lists name the same models in the same order. */
function samePool(a: PoolModelView[], b: PoolModelView[]): boolean {
  return (
    a.length === b.length &&
    a.every((m, i) => m.provider === b[i].provider && m.model === b[i].model)
  );
}

/** The name a person sees for one pool model: the provider's own name, plus
 * the model's display name when its list has loaded and offers one -- never
 * a raw id. `providers` is the keyed-only list a pool can pick new models
 * from; a provider whose key was just removed drops out of it, so
 * `allProviders` (every provider this build knows, keyed or not) is the
 * fallback that still finds its name, and "a provider" only when neither
 * does. */
function poolModelName(
  providers: PoolProviderView[],
  allProviders: { id: string; name: string }[],
  lists: Record<string, PoolModelsView>,
  model: PoolModelView,
): string {
  const provider =
    providers.find((p) => p.id === model.provider)?.name ??
    allProviders.find((p) => p.id === model.provider)?.name ??
    "a provider";
  const offer = lists[model.provider]?.offers.find((o) => o.id === model.model);
  // A saved model the fetched list no longer offers (a dated snapshot the
  // list hides) is still named by its id: settings pages show the model,
  // not only the provider.
  return `${provider} ${offer?.name ?? model.model}`;
}

/** What dictation uses now: unset while the ticks differ from what is saved
 * (including when nothing is saved as a pool yet), otherwise the saved
 * models in order, mirroring the "first, falling back to second[ and N
 * more]" shape `macalla-session`'s `pool_label` gives the dictation
 * destination label. Only a guess at what is really running -- it assumes
 * every ticked model built; [`poolInUseStatus`] is the authoritative
 * version, used once dictation has actually opened. */
function poolInUseLine(
  ticked: PoolModelView[],
  saved: PoolModelView[] | null,
  name: (model: PoolModelView) => string,
): string {
  if (saved === null || !samePool(ticked, saved)) {
    return "Not in use yet: press Use these models";
  }
  // A name that repeats (two models from one provider whose list has not
  // loaded) is said once, as `pool_label` does.
  const names = saved.map(name).filter((n, at, all) => all.indexOf(n) === at);
  if (names.length === 1) {
    return `In use: ${names[0]}`;
  }
  if (names.length === 2) {
    return `In use: ${names[0]}, falling back to ${names[1]}`;
  }
  return `In use: ${names[0]}, falling back to ${names[1]} and ${names.length - 2} more`;
}

/** The authoritative version of [`poolInUseLine`]: once dictation has
 * actually opened with this saved pool, `status` (`OpenedStatusView`) says
 * what really built. `poolInUseLine`'s own guess assumes every ticked
 * model built, model names and all; that stays true, and reads better,
 * whenever nothing was actually skipped (`enhancement_issue` unset), so
 * this only overrides it once something was -- `status.enhancement` is
 * only ever a provider's name, from `macalla_session::cleanup::pool_label`,
 * not a model's, so the line falls back to naming providers alone there.
 * Falls back to the guess before the first open, while the ticks differ
 * from what is saved, or while a dictation is still opening (`preparing`)
 * and `status` may already be stale. The not-built case says only "Not in
 * use": the Enhancement card's own line already gives the reason
 * (`enhancement_reason`), so the pool line does not repeat it beside a
 * button that already reads "Saved" for the same fact. */
function poolInUseStatus(
  ticked: PoolModelView[],
  saved: PoolModelView[] | null,
  name: (model: PoolModelView) => string,
  status: OpenedStatusView | null,
  preparing: boolean,
): string {
  if (preparing || saved === null || !samePool(ticked, saved) || !status) {
    return poolInUseLine(ticked, saved, name);
  }
  // Nothing is running for the saved pool (nothing built, or nothing can
  // dictate at all): the button already reads "Saved", so this line must
  // not claim "In use" beside it.
  if (!status.enhancement) {
    return "Not in use";
  }
  if (!status.enhancement_issue) {
    return poolInUseLine(ticked, saved, name);
  }
  return `In use: ${status.enhancement}`;
}

/** The cloud pool's own panel: every model the providers with a saved key
 * offer, listed from each provider's own model list. Ticking adds a model to
 * the end of the order; Save keeps that order. The first model is asked
 * first, and the next takes over when one fails. No model is suggested:
 * none of these has been measured on Macalla's cleanup fixtures yet. */
function PoolCleanupPanel({
  providers,
  allProviders,
  saved,
  busy,
  preparing,
  runChoice,
  onChange,
  inUse,
}: {
  providers: PoolProviderView[];
  allProviders: { id: string; name: string }[];
  /** The saved pool's models, when the saved choice is a pool. */
  saved: PoolModelView[] | null;
  busy: boolean;
  /** A dictation just asked to open is still loading, so `inUse` may
   * already be stale for it. */
  preparing: boolean;
  runChoice: (run: () => Promise<void>) => Promise<void>;
  onChange: () => void;
  /** What dictation actually opened with, when the saved choice is a pool;
   * `null` before the first open or when a pool is not the saved choice. */
  inUse: OpenedStatusView | null;
}) {
  const [ticked, setTicked] = useState<PoolModelView[]>(saved ?? []);
  const [lists, setLists] = useState<Record<string, PoolModelsView>>({});
  const [loadErrors, setLoadErrors] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  // Bumped on every load; only the latest load's answers are shown, the
  // same guard the server panel uses against an earlier reply landing last.
  const requestSeq = useRef(0);

  const load = (refresh: boolean) => {
    const id = ++requestSeq.current;
    setLoading(true);
    Promise.allSettled(providers.map((p) => poolModels(p.id, refresh))).then((results) => {
      if (id !== requestSeq.current) {
        return;
      }
      const nextLists: Record<string, PoolModelsView> = {};
      const nextErrors: Record<string, string> = {};
      results.forEach((result, i) => {
        const provider = providers[i].id;
        if (result.status === "fulfilled") {
          nextLists[provider] = result.value;
        } else {
          nextErrors[provider] = String(result.reason);
        }
      });
      setLists(nextLists);
      setLoadErrors(nextErrors);
      setLoading(false);
    });
  };

  const providerIds = providers.map((p) => p.id).join(",");
  useEffect(() => {
    load(false);
    // Loads again only when the set of providers with a key changes.
  }, [providerIds]);

  const providerName = (id: string) =>
    providers.find((p) => p.id === id)?.name ??
    allProviders.find((p) => p.id === id)?.name ??
    "a provider";
  const position = (provider: string, model: string) =>
    ticked.findIndex((t) => t.provider === provider && t.model === model);

  const toggle = (provider: string, model: string) => {
    setSaveError(null);
    setTicked((current) =>
      current.some((t) => t.provider === provider && t.model === model)
        ? current.filter((t) => !(t.provider === provider && t.model === model))
        : [...current, { provider, model }],
    );
  };

  const save = () => {
    setSaveError(null);
    runChoice(() => cleanupChoose({ kind: "pool", models: ticked }))
      .then(onChange)
      .catch((e: unknown) => setSaveError(String(e)));
  };

  const unsaved = saved === null || !samePool(ticked, saved);
  const name = (model: PoolModelView) => poolModelName(providers, allProviders, lists, model);
  const inUseLine = poolInUseStatus(ticked, saved, name, inUse, preparing);
  // "Saved" when the pool matches what was saved but `enhancement` is not
  // set: nothing built (a removed key, most often), or nothing can dictate
  // at all (no on-device engine to preview with). Both are true of what was
  // asked for, not of what is running, which "In use" would claim.
  const buttonLabel = unsaved
    ? "Use these models"
    : inUse && !preparing && !inUse.enhancement
      ? "Saved"
      : "In use";

  const order = ticked.length > 0 && (
    <ol className="pool pool-order">
      {ticked.map((t) => (
        <li key={`${t.provider}/${t.model}`}>
          <span>
            <span className="mono">{t.model}</span>
            <span className="or-sub">{providerName(t.provider)}</span>
          </span>
          <button
            type="button"
            className="link"
            disabled={busy}
            onClick={() => toggle(t.provider, t.model)}
          >
            Remove
          </button>
        </li>
      ))}
    </ol>
  );

  if (providers.length === 0) {
    // A saved pool whose providers have all lost their keys still shows its
    // order, so a model can be removed without saving a key first.
    return (
      <div className="choice-more">
        {order}
        {order && (
          <div className="inline-field">
            <button
              type="button"
              className="button"
              disabled={busy || ticked.length === 0 || !unsaved}
              onClick={save}
            >
              {buttonLabel}
            </button>
          </div>
        )}
        {order && <span className="card-meta">{inUseLine}</span>}
        {saveError && <p className="card-error">{saveError}</p>}
        <p className="card-meta">
          Save a key for OpenAI, Anthropic, Gemini or Mistral on this page to choose their models.
        </p>
      </div>
    );
  }

  return (
    <div className="choice-more">
      <span className="card-meta">
        Your words are sent over the internet to the provider of each model you tick. The first
        is asked first; if it fails, the next takes over.
      </span>
      {order}
      <div className="inline-field">
        <button
          type="button"
          className="button"
          disabled={busy || ticked.length === 0 || !unsaved}
          onClick={save}
        >
          {buttonLabel}
        </button>
        <button type="button" className="link" disabled={loading} onClick={() => load(true)}>
          Refresh lists
        </button>
      </div>
      <span className="card-meta">{inUseLine}</span>
      {saveError && <p className="card-error">{saveError}</p>}
      <div className="or-picker">
        {loading && <p className="card-meta">Loading…</p>}
        {providers.map((provider) => {
          const list = lists[provider.id];
          const error = loadErrors[provider.id];
          return (
            <div key={provider.id}>
              <div className="pool-provider">{provider.name}</div>
              {error && <p className="card-error">{error}</p>}
              {list?.stale_reason && (
                <p className="card-meta">
                  Showing the last list saved, because fetching a new one failed: {list.stale_reason}
                </p>
              )}
              {list && list.offers.length === 0 && (
                <p className="card-meta">No text models in this provider's list.</p>
              )}
              {list?.offers.map((offer) => {
                const at = position(provider.id, offer.id);
                return (
                  <label key={offer.id} className="or-option">
                    <input
                      type="checkbox"
                      disabled={busy}
                      checked={at >= 0}
                      onChange={() => toggle(provider.id, offer.id)}
                    />
                    <span>
                      {offer.name}
                      {offer.name !== offer.id && <span className="or-sub">{offer.id}</span>}
                    </span>
                    {at >= 0 && <span className="kind">{at + 1}</span>}
                  </label>
                );
              })}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/**
 * Enhancement: off, Apple Intelligence, a person's own OpenAI-compatible server,
 * or a pool of cloud models. Radio rows, one open at a time; the server and
 * pool rows expand their own panels rather than saving on their own click,
 * since each needs more before the choice means anything. A server that is
 * not this machine is sent a dictation's text when the key is let go; the
 * panel says so where that applies.
 */
function CleanupCard({
  options,
  chosen,
  poolProviders,
  allProviders,
  busy,
  preparing,
  runChoice,
  onChange,
  cardRef,
  inUse,
}: {
  options: CleanupOptionView[];
  chosen: CleanupChoiceView;
  poolProviders: PoolProviderView[];
  /** Every provider this build knows about, for naming a pool model whose
   * provider has lost its key and so dropped out of `poolProviders`. */
  allProviders: ProviderKeyView[];
  busy: boolean;
  /** A dictation just asked to open is still loading, so the last open's
   * `inUse` may already be stale -- the pool panel falls back to its own
   * ticked/saved guess while this is true. */
  preparing: boolean;
  runChoice: (run: () => Promise<void>) => Promise<void>;
  onChange: () => void;
  cardRef: React.RefObject<HTMLElement | null>;
  /** What dictation actually opened with; `null` before the first open. */
  inUse: OpenedStatusView | null;
}) {
  const apple = options.find((o) => o.kind === "apple-intelligence") ?? null;
  const [open, setOpen] = useState<"off" | "apple-intelligence" | "server" | "pool">(chosen.kind);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setOpen(chosen.kind);
  }, [chosen.kind]);

  const chooseSimple = (kind: "off" | "apple-intelligence") => {
    setOpen(kind);
    setError(null);
    runChoice(() => cleanupChoose({ kind }))
      .then(onChange)
      .catch((e: unknown) => setError(String(e)));
  };

  return (
    <section className="card" ref={cardRef}>
      <h2>Enhancement</h2>
      <p>
        Optional. Tidies punctuation and capitals after the dictionary has fixed your words. The
        paste waits up to 5 seconds for the tidied text.
      </p>
      {!preparing && inUse?.enhancement_reason && (
        <p className="card-error">{inUse.enhancement_reason}</p>
      )}
      <div className="choices" role="radiogroup" aria-label="Enhancement">
        <label className="choice-head">
          <input
            type="radio"
            name="cleanup"
            checked={open === "off"}
            disabled={busy}
            onChange={() => chooseSimple("off")}
          />
          <span className="choice-body">
            <span className="radio" />
            <span>
              <span className="choice-title">
                <b>Off</b>
              </span>
              <span className="card-meta">
                Only the dictionary and Macalla's own tidying. Nothing is sent anywhere.
              </span>
            </span>
          </span>
        </label>
        <label className="choice-head">
          <input
            type="radio"
            name="cleanup"
            checked={open === "apple-intelligence"}
            disabled={busy || !apple?.available}
            onChange={() => chooseSimple("apple-intelligence")}
          />
          <span className="choice-body">
            <span className="radio" />
            <span>
              <span className="choice-title">
                <b>Apple Intelligence</b>
              </span>
              <span className="card-meta">
                {apple?.available
                  ? "Tidies what you say, locally."
                  : (apple?.reason ?? "Not available.")}
              </span>
            </span>
          </span>
        </label>
        <div className="choice">
          <label className="choice-head">
            <input
              type="radio"
              name="cleanup"
              checked={open === "server"}
              disabled={busy}
              onChange={() => setOpen("server")}
            />
            <span className="choice-body">
              <span className="radio" />
              <span>
                <span className="choice-title">
                  <b>Ollama or another server</b>
                </span>
                <span className="card-meta">
                  A machine on your own network can tidy what you say.
                </span>
              </span>
            </span>
          </label>
          {open === "server" && (
            <ServerCleanupPanel
              saved={chosen.kind === "server" ? { address: chosen.address, model: chosen.model } : null}
              busy={busy}
              runChoice={runChoice}
              onChange={onChange}
            />
          )}
        </div>
        <div className="choice">
          <label className="choice-head">
            <input
              type="radio"
              name="cleanup"
              checked={open === "pool"}
              disabled={busy}
              onChange={() => setOpen("pool")}
            />
            <span className="choice-body">
              <span className="radio" />
              <span>
                <span className="choice-title">
                  <b>Cloud models</b>
                </span>
                <span className="card-meta">
                  Models from the providers you have saved a key for. Your words are sent to them.
                </span>
              </span>
            </span>
          </label>
          {open === "pool" && (
            <PoolCleanupPanel
              providers={poolProviders}
              allProviders={allProviders}
              saved={chosen.kind === "pool" ? chosen.models : null}
              busy={busy}
              preparing={preparing}
              runChoice={runChoice}
              onChange={onChange}
              inUse={chosen.kind === "pool" ? inUse : null}
            />
          )}
        </div>
      </div>
      {error && <p className="card-error">{error}</p>}
    </section>
  );
}

/**
 * The Models page: what is in use, provider keys, and every transcription
 * and cleanup choice. Reached from the Engine card's "Open Transcription on
 * Models" button and the Enhancement card's "Open Enhancement on Models"
 * button, each scrolling to its own card on arrival.
 */
function ModelsView({
  onBack,
  target,
  onTargetReached,
}: {
  onBack: () => void;
  target: ModelsTarget;
  onTargetReached: () => void;
}) {
  const [overview, setOverview] = useState<ModelsOverview | null>(null);
  const [models, setModels] = useState<ModelView[]>([]);
  const [dir, setDir] = useState<string | null>(null);
  const [progress, setProgress] = useState<Record<string, ModelProgress>>({});
  const [preparing, setPreparing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // One flag for the whole page: two overlapping "Use" clicks racing each
  // other's write to the stored choice is the problem, not any one button.
  const [busy, setBusy] = useState(false);
  const providersCard = useRef<HTMLElement | null>(null);
  const transcriptionCard = useRef<HTMLElement | null>(null);
  const enhancementCard = useRef<HTMLElement | null>(null);
  // Set from `dictation-status`'s own payload, just before any
  // `dictation-error` a reopen or an in-place Enhancement swap sends for
  // the same fact: InUseCard and
  // CleanupCard already show `transcription_reason`/`enhancement_reason`
  // once `refresh` (on the `idle` right after) picks them up, so this
  // page's own top-of-page `error` does not need to repeat one. Kept as
  // the reasons themselves, not a plain flag: a `Ready` session with a
  // partial pool sets `enhancement_reason` but sends no `dictation-error`
  // at all (dictation.rs's `notices_for`, `Ready` arm reads
  // `cleanup_error`, `None` for a partial pool that built), so an
  // unrelated error arriving before the next `idle` must still show --
  // matching only the exact notice a reopen would have sent for these
  // reasons (see the `dictation-error` listener below) tells the two
  // apart; a bare boolean could not.
  const expectedDuplicateReasons = useRef<{
    transcription: string | null;
    enhancement: string | null;
  }>({ transcription: null, enhancement: null });

  const runChoice = (run: () => Promise<void>): Promise<void> => {
    setError(null);
    setBusy(true);
    return run().finally(() => setBusy(false));
  };

  const refresh = () => {
    modelsOverview()
      .then(setOverview)
      .catch((e: unknown) => setError(String(e)));
    modelsList()
      .then(setModels)
      .catch((e: unknown) => setError(String(e)));
    modelsDir()
      .then(setDir)
      .catch(() => setDir(null));
  };

  useEffect(() => {
    refresh();
    const subscriptions = [
      listen<ModelProgress>("model-progress", (e) => {
        setProgress((prev) => ({ ...prev, [e.payload.id]: e.payload }));
      }),
      listen<string>("model-ready", (e) => {
        setProgress((prev) => {
          const next = { ...prev };
          delete next[e.payload];
          return next;
        });
        refresh();
      }),
      listen<ModelError>("model-error", (e) => {
        setProgress((prev) => {
          const next = { ...prev };
          delete next[e.payload.id];
          return next;
        });
        setError(e.payload.message);
        refresh();
      }),
      // `idle` follows every reopen (a key saved or removed for the
      // transcription choice sends `SwitchEngine`, so the "in use" truth on
      // this page never lags behind the keychain), every in-place
      // Enhancement swap (`SwitchCleanup` -- taken while idle, at the end
      // of a Send-again wait, or applied after a dictation that had one
      // pending; a pool-only key save or removal sends this, not
      // `SwitchEngine`) and every finished dictation -- every place a
      // keychain-backed `refresh` is worth its cost; `dictation-status`
      // (`OpenedStatusShared::set`) only ever fires on the first two of those,
      // just before this same `idle` every time, reopen or swap, so
      // reading it here for a second `refresh` would only run it twice for
      // the one case, not cover a case this does not already -- it is read
      // once below instead, for `expectedDuplicateReasons`.
      listen<string>("dictation-state", (e) => {
        setPreparing(e.payload === "preparing");
        if (e.payload === "idle") {
          // Reset regardless of whether a `dictation-error` actually
          // followed the status this dictation opened with (a `Ready`
          // session with nothing to report sends none): reasons left
          // armed would wrongly swallow the next unrelated error.
          expectedDuplicateReasons.current = { transcription: null, enhancement: null };
          refresh();
        }
      }),
      // What dictation actually opened with just changed: a reopen's own
      // `dictation-error`, if it sends one, is about to repeat
      // `transcription_reason`/`enhancement_reason` in this same payload,
      // which the cards below already show once `refresh` (on the `idle`
      // that always follows) has them.
      listen<{ transcription_reason: string | null; enhancement_reason: string | null } | null>(
        "dictation-status",
        (e) => {
          expectedDuplicateReasons.current = {
            transcription: e.payload?.transcription_reason ?? null,
            enhancement: e.payload?.enhancement_reason ?? null,
          };
        },
      ),
      // What open_session could not do, said on the page that just asked it
      // to open something. Cleared on the next choose or download, same as
      // the model-error line above. Skipped only when the payload is
      // exactly the notice dictation.rs's `notices_for` builds for the
      // reasons above -- `enhancement_reason` alone (the `Ready` arm), or
      // starting with `transcription_reason` (`LocalFallback`, which
      // appends the enhancement one after a space when both apply,
      // `PreviewMissing` and `Failed`, which are `transcription_reason`
      // alone) -- so a genuinely different error occurring at the same
      // moment is never hidden.
      listen<string>("dictation-error", (e) => {
        const { transcription, enhancement } = expectedDuplicateReasons.current;
        const isDuplicate =
          (enhancement !== null && e.payload === enhancement) ||
          (transcription !== null && e.payload.startsWith(transcription));
        if (isDuplicate) {
          return;
        }
        setError(e.payload);
      }),
    ];
    return () => {
      subscriptions.forEach((s) => s.then((unlisten) => unlisten()));
    };
  }, []);

  // Land on the card whose button opened this page, once its content has
  // loaded, then forget the target -- Back and a later return should not
  // re-scroll on their own.
  useEffect(() => {
    if (!overview || !target) {
      return;
    }
    const card = target === "transcription" ? transcriptionCard : enhancementCard;
    card.current?.scrollIntoView({ behavior: "smooth", block: "start" });
    onTargetReached();
  }, [overview, target, onTargetReached]);

  const jumpToProviders = () => {
    providersCard.current?.scrollIntoView({ behavior: "smooth", block: "start" });
  };

  const local = overview?.transcription.filter((t) => t.source === "local") ?? [];
  const providerOptions =
    overview?.transcription.filter((t) => t.source === "provider" && t.id !== "openrouter") ?? [];
  const openrouter = overview?.transcription.find((t) => t.id === "openrouter") ?? null;
  const chosen = overview?.transcription.find((t) => t.chosen) ?? null;

  return (
    <main className="cards">
      <RouteSymbol />
      <button type="button" className="button" onClick={onBack}>
        Back
      </button>
      <div className="page-head">
        <h1>Models</h1>
        <p>
          What turns your voice into text. Nothing leaves this machine until you choose something that
          does, and the mark beside it says where it goes.
        </p>
      </div>
      {error && <p className="card-error">{error}</p>}
      <InUseCard
        chosen={chosen}
        previewName={
          overview ? (local.find((e) => e.id === overview.preview_engine_id)?.name ?? null) : null
        }
        inUse={overview?.in_use ?? null}
        preparing={preparing}
      />
      {overview && (
        <ProvidersCard keys={overview.keys} onChange={refresh} cardRef={providersCard} />
      )}
      {overview && (
        <TranscriptionCard
          local={local}
          providers={providerOptions}
          openrouter={openrouter}
          compiledIn={overview.cloud_transcription_compiled_in}
          previewId={overview.preview_engine_id}
          models={models}
          dir={dir}
          progress={progress}
          preparing={preparing}
          busy={busy}
          runChoice={runChoice}
          onChange={refresh}
          onJumpToProviders={jumpToProviders}
          cardRef={transcriptionCard}
        />
      )}
      {overview && (
        <CleanupCard
          options={overview.cleanup}
          chosen={overview.cleanup_chosen}
          poolProviders={overview.pool_providers}
          allProviders={overview.keys.providers}
          busy={busy}
          preparing={preparing}
          runChoice={runChoice}
          onChange={refresh}
          cardRef={enhancementCard}
          inUse={overview.in_use ?? null}
        />
      )}
    </main>
  );
}

function stepMark(state: StepSummary["state"]): string {
  switch (state) {
    case "done":
      return "\u2713";
    case "required":
      return "\u25cb";
    case "optional":
      return "\u2013";
  }
}

/**
 * How long the words get to appear in the box after Macalla announces them.
 * The paste keystroke is sent before that announcement, and the clipboard is
 * restored 150 ms after the keystroke, so this only has to cover the window
 * handling a paste it has already been given.
 */
const TRY_IT_GRACE_MS = 1500;

type TryIt =
  | { kind: "waiting" }
  | { kind: "listening" }
  | { kind: "worked" }
  | { kind: "not-pasted" }
  | { kind: "wrong-paste" }
  | { kind: "nothing-heard" }
  | { kind: "failed"; message: string };

const squash = (s: string) => s.replace(/\s+/g, " ").trim();

function tryItDetail(result: TryIt, hold: string, historyOn: boolean): string {
  switch (result.kind) {
    case "waiting":
      return `Click the box, ${hold}, and say something.`;
    case "listening":
      return "Listening…";
    case "worked":
      return "It works. Your words will appear like this in any app you type in.";
    case "not-pasted":
      return "Macalla heard you but could not paste into that app. Try again.";
    case "wrong-paste":
      return `Macalla heard you, but something else pasted instead of your words. That is a fault in Macalla, not a setting.${historyOn ? " Your words are in History." : ""}`;
    case "nothing-heard":
      return "Nothing was heard. Check the microphone named below, then try again.";
    case "failed":
      return result.message;
  }
}

function termsLine(terms: number): string {
  if (terms === 0) {
    return "Your dictionary is empty. You can add words Macalla gets wrong from History.";
  }
  return `Your dictionary has ${terms} ${terms === 1 ? "term" : "terms"}.`;
}

/**
 * The checklist's last row: prove it works without leaving the window. Click
 * into the box, hold the shortcut, speak, and the words paste here, which
 * exercises the microphone, the shortcut, the engine and paste in one go.
 *
 * The verdict joins two separate signals, and keeping them apart is the
 * point. The "dictation" event says what Macalla heard; the box says what
 * landed. Heard but nothing landed points at Accessibility. Nothing heard
 * points at the microphone or the engine. Something landed that is not the
 * words means the clipboard was read after it was restored, which is our bug
 * and must not be blamed on a setting. Only a dictation that starts while the
 * box has focus counts, so dictating into another app with this window open
 * does not tick it.
 */
function TryItStep({
  shortcutName,
  onOpenAccessibility,
}: {
  shortcutName: string | null;
  /** Opens Accessibility's pane, where this platform has one; `null` where
   * it does not, and then there is no button. */
  onOpenAccessibility: (() => void) | null;
}) {
  const box = useRef<HTMLTextAreaElement | null>(null);
  // Refs rather than state: the listeners are registered once and must read
  // the current values, not the ones from the render that registered them.
  const armed = useRef(false);
  const landing = useRef(false);
  const heard = useRef("");
  const before = useRef("");
  const graceTimer = useRef<number | undefined>(undefined);
  const [result, setResult] = useState<TryIt>({ kind: "waiting" });
  const [terms, setTerms] = useState<number | null>(null);
  // Off until known: the message must not point at History that is not kept.
  const [historyOn, setHistoryOn] = useState(false);

  const landed = () => heard.current !== "" && squash(box.current?.value ?? "").includes(heard.current);

  useEffect(() => {
    dictionaryInfo()
      .then((info) => setTerms(info.terms))
      .catch(() => setTerms(null));
    historyRetention()
      .then((r) => setHistoryOn(r.current !== "off"))
      .catch(() => setHistoryOn(false));
    const settle = (next: TryIt) => {
      armed.current = false;
      landing.current = false;
      window.clearTimeout(graceTimer.current);
      setResult(next);
    };
    const subscriptions = [
      listen<string>("dictation-state", (e) => {
        if (e.payload === "listening" && box.current && document.activeElement === box.current) {
          armed.current = true;
          landing.current = false;
          heard.current = "";
          before.current = squash(box.current.value);
          window.clearTimeout(graceTimer.current);
          setResult({ kind: "listening" });
        }
      }),
      listen<string>("dictation", (e) => {
        if (!armed.current || squash(e.payload) === "") {
          return;
        }
        heard.current = squash(e.payload);
        if (landed()) {
          settle({ kind: "worked" });
          return;
        }
        armed.current = false;
        landing.current = true;
        graceTimer.current = window.setTimeout(() => {
          if (!landing.current) {
            return;
          }
          const changed = squash(box.current?.value ?? "") !== before.current;
          settle({ kind: changed ? "wrong-paste" : "not-pasted" });
        }, TRY_IT_GRACE_MS);
      }),
      listen<null>("dictation-empty", () => {
        if (armed.current) {
          settle({ kind: "nothing-heard" });
        }
      }),
      listen<string>("dictation-error", (e) => {
        if (armed.current) {
          settle({ kind: "failed", message: e.payload });
        }
      }),
      listen<null>("dictation-discarded", () => {
        if (armed.current) {
          settle({ kind: "waiting" });
        }
      }),
    ];
    return () => {
      window.clearTimeout(graceTimer.current);
      subscriptions.forEach((s) => s.then((unlisten) => unlisten()));
    };
  }, []);

  const onInput = () => {
    if (landing.current && landed()) {
      landing.current = false;
      window.clearTimeout(graceTimer.current);
      setResult({ kind: "worked" });
    }
  };

  const worked = result.kind === "worked";
  const trouble =
    result.kind === "not-pasted" ||
    result.kind === "wrong-paste" ||
    result.kind === "nothing-heard" ||
    result.kind === "failed";
  const hold = shortcutName ? `hold ${shortcutName}` : "hold the shortcut";
  return (
    <li className={`step ${worked ? "step-done" : "step-required"}`}>
      <span className="step-mark">{worked ? "✓" : "○"}</span>
      <div className="step-body">
        <span className="step-title">Try it</span>{" "}
        <span className={trouble ? "card-error" : "step-detail"}>{tryItDetail(result, hold, historyOn)}</span>
        <textarea
          ref={box}
          className="try-box"
          aria-label="Try dictating here"
          rows={2}
          onInput={onInput}
        />
        {result.kind === "not-pasted" && onOpenAccessibility && (
          <div className="button-row">
            <button type="button" className="button" onClick={onOpenAccessibility}>
              Open System Settings
            </button>
          </div>
        )}
        {terms !== null && <p className="step-action">{termsLine(terms)}</p>}
      </div>
    </li>
  );
}

// The first-launch checklist: a checklist, plain English, no modals, one Continue. The
// list re-reads itself every two seconds while something is not done, so a
// tick appears as soon as a grant lands in System Settings, without a
// relaunch. Once everything is done it says so and offers to get out of the
// way; after that it is not worth a card. "After that" outlasts the page: a
// setup already complete on the first read, at launch, was finished in an
// earlier run, so "You're all set" is not shown again every launch. A step
// that stops being done -- a permission revoked -- brings the card back.
function SetupCard({ shortcutName }: { shortcutName: string | null }) {
  const [steps, setSteps] = useState<StepSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const firstRead = useRef(true);

  const refresh = () => {
    setupSteps()
      .then((read) => {
        if (firstRead.current) {
          firstRead.current = false;
          if (read.every((s) => s.state !== "required")) setDismissed(true);
        }
        setSteps(read);
      })
      .catch((e: unknown) => setError(String(e)));
  };

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, 2000);
    return () => window.clearInterval(timer);
  }, []);

  const ask = (id: string) => {
    requestPermission(id)
      .then(refresh)
      .catch((e: unknown) => setError(String(e)));
  };

  const open = (id: string) => {
    openSettingsPane(id).catch((e: unknown) => setError(String(e)));
  };

  const done = () => {
    setDismissed(true);
    finishSetup().catch((e: unknown) => setError(String(e)));
  };

  if (!steps || (dismissed && steps.every((s) => s.state !== "required"))) {
    return null;
  }
  // Which steps are this platform's permissions, and which of them can be
  // asked for or have a pane, is the platform's answer (`StepSummary`'s
  // flags): the page never decides which OS it is on.
  const needs = steps.filter((s) => s.permission);
  const also = steps.filter((s) => !s.permission);
  const granted = needs.filter((s) => s.state === "done").length;
  const missing = needs.filter((s) => s.state !== "done").map((s) => s.title);
  // Setup is done when nothing required is left, as the core's
  // `Checklist::is_ready` says: an optional step's state is "optional", never
  // "done", so waiting for every step to be "done" never let Continue show.
  const allDone = steps.every((s) => s.state !== "required");
  return (
    <section className="card">
      <h2>
        {allDone ? "You're all set" : "Getting started"}
        {/* Only a platform with permissions to grant counts them; Windows
            and Linux send none. */}
        {needs.length > 0 && (
          <span className="progress">
            {granted} of {needs.length} permissions
          </span>
        )}
      </h2>
      {allDone ? (
        <p>{shortcutName ? `Hold ${shortcutName} to dictate.` : "Hold the shortcut to dictate."}</p>
      ) : (
        needs.length > 0 && (
          <p>
            {missing.length > 0
              ? `Still needed from macOS: ${missing.join(", ")}.`
              : "Macalla needs a few things from macOS before it can hear you and type for you."}
          </p>
        )
      )}
      {error && <p className="card-error">{error}</p>}
      {needs.length > 0 && (
      <ul className="steps">
        {needs.map((s) => (
          <li key={s.id} className={`step step-${s.state}`}>
            <span className="step-mark">{stepMark(s.state)}</span>
            <div className="step-body">
              <span className="step-title">{s.title}</span>{" "}
              <span className="step-detail">{s.detail}</span>
              {s.action && s.state !== "done" && (
                <p className="step-action">{s.action}</p>
              )}
              {s.state !== "done" && (
                <div className="button-row">
                  {s.ask && (
                    <button type="button" className="button" onClick={() => ask(s.id)}>
                      Ask
                    </button>
                  )}
                  {s.settings && (
                    <button type="button" className="button" onClick={() => open(s.id)}>
                      Open System Settings
                    </button>
                  )}
                </div>
              )}
            </div>
          </li>
        ))}
      </ul>
      )}
      {also.length > 0 && (
        <ul className="steps steps-also">
          {also.map((s) => (
            <li key={s.id} className={`step step-${s.state}`}>
              <span className="step-mark">{stepMark(s.state)}</span>
              <div className="step-body">
                <span className="step-title">{s.title}</span>{" "}
                <span className="step-detail">{s.detail}</span>
                {s.action && s.state !== "done" && (
                  <p className="step-action">{s.action}</p>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
      {/* Hearing a dictation needs the microphone and the key, not
          Accessibility. Without Accessibility the words still arrive, and
          saying they could not be pasted is the most useful thing this row
          can do. Waiting for all three made that message unreachable: the
          two-second re-read took the row away as soon as the grant went.
          It waits for an engine too: where there are no permissions to
          grant, the check above passes on the first launch, before the
          default model has downloaded. */}
      {needs.filter((s) => s.id !== "accessibility").every((s) => s.state === "done") &&
        steps.every((s) => s.id !== "engine" || s.state !== "required") && (
        <ul className="steps steps-try">
          <TryItStep
            shortcutName={shortcutName}
            onOpenAccessibility={
              needs.some((s) => s.id === "accessibility" && s.settings)
                ? () => open("accessibility")
                : null
            }
          />
        </ul>
      )}
      {allDone && (
        <button type="button" className="button" onClick={done}>
          Continue
        </button>
      )}
    </section>
  );
}

function DictionaryCard({ onOpen }: { onOpen: () => void }) {
  const [info, setInfo] = useState<DictionaryInfo | null>(null);

  useEffect(() => {
    dictionaryInfo()
      .then(setInfo)
      .catch(() => setInfo(null));
  }, []);

  return (
    <section className="card">
      <h2>Dictionary</h2>
      <p>Words and phrases that should always come out right.</p>
      {info && (
        <p className="card-meta" title={info.path}>
          {info.terms} {info.terms === 1 ? "term" : "terms"}, kept on this machine
        </p>
      )}
      <button type="button" className="button" onClick={onOpen}>
        Open Dictionary
      </button>
    </section>
  );
}

/** "neeve, neev" as the form takes it, into the list the file keeps. */
function splitSpoken(text: string): string[] {
  return text
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

interface TermFormProps {
  /** What the term is now; empty for a new one. */
  initial: TermView;
  /** The button's label. */
  submit: string;
  onSubmit: (written: string, spoken: string[]) => Promise<void>;
  onCancel?: () => void;
}

/**
 * One form for adding and editing: the written form, and how it gets heard.
 * The spoken forms are what makes a correction fire, so they are never hidden.
 */
function TermForm({ initial, submit, onSubmit, onCancel }: TermFormProps) {
  const [written, setWritten] = useState(initial.written);
  const [spoken, setSpoken] = useState(initial.spoken.join(", "));
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const save = () => {
    setBusy(true);
    onSubmit(written.trim(), splitSpoken(spoken))
      .then(() => {
        setError(null);
        if (!onCancel) {
          setWritten("");
          setSpoken("");
        }
      })
      .catch((e: unknown) => setError(String(e)))
      .finally(() => setBusy(false));
  };

  return (
    <div className="term-form">
      <label className="term-field">
        <span>Comes out as</span>
        <input
          value={written}
          onChange={(e) => setWritten(e.target.value)}
          placeholder="Niamh"
          autoFocus={initial.written === ""}
        />
      </label>
      <label className="term-field">
        <span>Heard as</span>
        <input
          value={spoken}
          onChange={(e) => setSpoken(e.target.value)}
          placeholder="neeve, neev"
        />
      </label>
      {error && <p className="card-error">{error}</p>}
      <div className="button-row">
        <button type="button" className="button" onClick={save} disabled={busy || written.trim() === ""}>
          {submit}
        </button>
        {onCancel && (
          <button type="button" className="button" onClick={onCancel}>
            Cancel
          </button>
        )}
      </div>
    </div>
  );
}

function DictionaryView({ onBack }: { onBack: () => void }) {
  const [terms, setTerms] = useState<TermView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [exported, setExported] = useState<string | null>(null);
  const fileInput = useRef<HTMLInputElement | null>(null);

  const refresh = () => {
    dictionaryTerms()
      .then((list) => {
        setTerms(list);
        setError(null);
      })
      .catch((e: unknown) => setError(String(e)));
  };

  useEffect(refresh, []);

  const add = (written: string, spoken: string[]) =>
    dictionaryAdd(written, spoken).then((created) => {
      setNotice(created ? `Added ${written}.` : `Updated ${written}.`);
      refresh();
    });

  const update = (previous: string) => (written: string, spoken: string[]) =>
    dictionaryUpdate(previous, written, spoken).then(() => {
      setEditing(null);
      setNotice(null);
      refresh();
    });

  const remove = (written: string) => {
    dictionaryRemove(written)
      .then(() => {
        setNotice(`Deleted ${written}.`);
        refresh();
      })
      .catch((e: unknown) => setError(String(e)));
  };

  const importFile = (file: File | undefined) => {
    if (!file) {
      return;
    }
    file
      .text()
      .then((json) => dictionaryImport(json))
      .then((changed) => {
        setNotice(
          changed === 0
            ? "Nothing new in that file."
            : `${changed} ${changed === 1 ? "term" : "terms"} added or updated from ${file.name}.`,
        );
        refresh();
      })
      .catch((e: unknown) => setError(String(e)));
    if (fileInput.current) {
      fileInput.current.value = "";
    }
  };

  const exportFile = () => {
    dictionaryExport()
      .then((path) => {
        setExported(path);
        setNotice(null);
      })
      .catch((e: unknown) => setError(String(e)));
  };

  return (
    <main className="cards">
      <button type="button" className="button" onClick={onBack}>
        Back
      </button>
      <section className="card">
        <h2>Dictionary</h2>
        <p>
          Words and phrases that should always come out right, and how they sound when spoken.
          When a dictation gets one wrong, add it here. The same list works with every engine.
        </p>
        {error && <p className="card-error">{error}</p>}
        {notice && <p className="card-meta">{notice}</p>}
        {terms && terms.length === 0 && <p className="card-meta">No terms yet.</p>}
      </section>
      <section className="card">
        <h2>Add a term</h2>
        <TermForm initial={{ written: "", spoken: [] }} submit="Add" onSubmit={add} />
      </section>
      {terms &&
        terms.map((term) =>
          editing === term.written ? (
            <section className="card" key={term.written}>
              <TermForm
                initial={term}
                submit="Save"
                onSubmit={update(term.written)}
                onCancel={() => setEditing(null)}
              />
            </section>
          ) : (
            <section className="card term" key={term.written}>
              <div className="term-body">
                <span className="term-written">{term.written}</span>
                <p className="term-spoken">
                  {term.spoken.length > 0
                    ? `Heard as ${term.spoken.join(", ")}`
                    : "Heard as itself, near enough"}
                </p>
              </div>
              <span className="row-actions">
                <button type="button" className="button" onClick={() => setEditing(term.written)}>
                  Edit
                </button>
                <button type="button" className="button" onClick={() => remove(term.written)}>
                  Delete
                </button>
              </span>
            </section>
          ),
        )}
      <section className="card">
        <h2>Share</h2>
        <p>
          Carry the list to another machine, or share it with a team. Import a JSON export, a
          plain list of words, or a CSV or TSV file with two columns: how it sounds, then how to
          write it. Importing only adds terms. Exports are JSON.
        </p>
        <div className="button-row">
          <button type="button" className="button" onClick={() => fileInput.current?.click()}>
            Import…
          </button>
          <button type="button" className="button" onClick={exportFile}>
            Export to Downloads
          </button>
          <input
            ref={fileInput}
            type="file"
            accept=".json,.txt,.csv,.tsv,application/json,text/plain,text/csv,text/tab-separated-values"
            hidden
            onChange={(e) => importFile(e.target.files?.[0])}
          />
        </div>
        {exported && (
          <p className="card-meta">
            Saved {exported.split("/").pop()} to Downloads.{" "}
            <button type="button" className="link" onClick={() => revealPath(exported).catch(() => undefined)}>
              Show in Finder
            </button>
          </p>
        )}
      </section>
    </main>
  );
}

/**
 * Which microphone is being listened through, and which to use. It changes
 * underneath the person -- a headset connects, a dock is unplugged -- and
 * "dictation does not work" is how the wrong one presents, so the card
 * follows the dictation thread's announcements rather than asking once, and
 * lists the devices again whenever the one in use changes or the window
 * comes back.
 */
function MicrophoneCard() {
  const [device, setDevice] = useState<string | null>(null);
  const [view, setView] = useState<MicrophoneView | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = () => {
    microphones()
      .then(setView)
      .catch((e: unknown) => setError(String(e)));
  };

  useEffect(() => {
    inputDevice()
      .then(setDevice)
      .catch(() => setDevice(null));
    refresh();
    const subscription = listen<string | null>("dictation-device", (e) => {
      setDevice(e.payload);
      refresh();
    });
    // A microphone plugged in while the window was away.
    window.addEventListener("focus", refresh);
    return () => {
      subscription.then((unlisten) => unlisten());
      window.removeEventListener("focus", refresh);
    };
  }, []);

  const choose = (value: string) => {
    setError(null);
    microphoneChoose(value === "" ? null : value)
      .then(refresh)
      .catch((e: unknown) => setError(String(e)));
  };

  const chosen = view?.chosen ?? null;
  const connected = chosen !== null && view !== null && view.names.includes(chosen);

  let status: string;
  if (chosen !== null && view !== null && !connected) {
    status = device
      ? `${chosen} is not connected, so Macalla is listening through ${device}.`
      : `${chosen} is not connected.`;
  } else if (!device) {
    status = "No microphone has been opened yet.";
  } else if (chosen !== null && device !== chosen) {
    status = `Switching to ${chosen}.`;
  } else {
    status = `Listening through ${device}.`;
  }

  return (
    <section className="card">
      <h2>Microphone</h2>
      <p>{status}</p>
      {view && (
        <label className="setting-row">
          <span>Use</span>
          <select className="select" value={chosen ?? ""} onChange={(e) => choose(e.target.value)}>
            {view.rows.map((row) => (
              <option key={row.value} value={row.value}>
                {row.label}
              </option>
            ))}
          </select>
        </label>
      )}
      {error && <p className="card-error">{error}</p>}
    </section>
  );
}

const RECORD_VALUE = "\u0000record";

/**
 * The Parse syntax for one non-modifier keydown, or null when the key pressed
 * is itself a modifier (those are captured on release, see the recorder).
 * Letters and digits by their physical key, so Option+D is "d" and not the
 * "∂" it types. Such a key is held with a modifier only to show the person it
 * is refused: the keyboard tap does not swallow it, so it would type.
 */
function chordFrom(e: KeyboardEvent): string | null {
  if (["Meta", "Control", "Alt", "Shift", "Fn", "CapsLock"].includes(e.key)) return null;
  let key = e.code.toLowerCase();
  if (e.code.startsWith("Key")) key = e.code.slice(3).toLowerCase();
  else if (e.code.startsWith("Digit")) key = e.code.slice(5);
  const parts: string[] = [];
  if (e.metaKey) parts.push("cmd");
  if (e.ctrlKey) parts.push("ctrl");
  if (e.altKey) parts.push("option");
  if (e.shiftKey) parts.push("shift");
  parts.push(key);
  return parts.join("+");
}

/**
 * The Parse syntax for a modifier key by its physical code, with its side, or
 * null for any other key. A single modifier held on its own is the only custom
 * shortcut the recorder can set: a printing key would type, a two-modifier
 * chord is ambiguous for a hold, and the webview never sees the Fn key (it is
 * offered as a preset on macOS instead).
 */
function sidedModifier(code: string): string | null {
  const m = /^(Shift|Control|Alt|Meta)(Left|Right)$/.exec(code);
  if (!m) return null;
  const word = { Shift: "shift", Control: "control", Alt: "alt", Meta: "meta" }[m[1]];
  return `${m[2].toLowerCase()} ${word}`;
}

function ShortcutCard({ onChanged }: { onChanged: () => void }) {
  const [view, setView] = useState<ShortcutView | null>(null);
  const [recording, setRecording] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = () => {
    shortcutView()
      .then(setView)
      .catch((e: unknown) => setError(String(e)));
  };

  useEffect(refresh, []);

  // The recorder pauses the listener for exactly as long as it is open, so
  // every way out -- a key, Cancel, Esc, leaving the page -- unpauses it. A
  // single modifier held on its own is captured on release; a printing key is
  // sent at once so the Rust side can say it would type.
  useEffect(() => {
    if (!recording) return;
    shortcutRecord(true).catch((e: unknown) => setError(String(e)));

    const held = new Set<string>();
    const seen: string[] = [];
    let sawPrintingKey = false;
    let done = false;

    const commit = (text: string) => {
      // One choice per recording: a second keydown before the first reply
      // lands must not fire shortcutChoose again.
      if (done) return;
      done = true;
      shortcutChoose(text)
        .then(() => {
          setError(null);
          refresh();
          onChanged();
        })
        .catch((err: unknown) => setError(String(err)))
        .finally(() => setRecording(false));
    };

    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      if (e.repeat) return;
      if (e.key === "Escape") {
        setRecording(false);
        return;
      }
      const mod = sidedModifier(e.code);
      if (mod) {
        held.add(e.code);
        seen.push(mod);
        return;
      }
      // A non-modifier key: send it so a refusal (it would type) is shown.
      const chord = chordFrom(e);
      if (chord === null) return;
      sawPrintingKey = true;
      commit(chord);
    };

    const onKeyUp = (e: KeyboardEvent) => {
      if (!sidedModifier(e.code)) return;
      e.preventDefault();
      held.delete(e.code);
      if (held.size > 0 || sawPrintingKey) return;
      // Everything is released and no printing key was pressed. One modifier
      // held on its own is the shortcut; two or more is sent so the Rust side
      // refuses it rather than guessing which one is the trigger.
      const unique = Array.from(new Set(seen));
      commit(unique.length === 1 ? unique[0] : unique.join("+"));
    };

    // If focus leaves mid-press (Cmd+Tab, say) the key-ups never arrive, so
    // forget what was held rather than wait on a release that cannot come.
    const onBlur = () => {
      held.clear();
      seen.length = 0;
    };

    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("keyup", onKeyUp, true);
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("keyup", onKeyUp, true);
      window.removeEventListener("blur", onBlur);
      shortcutRecord(false).catch(() => undefined);
    };
  }, [recording]);

  const choose = (value: string) => {
    setError(null);
    if (value === RECORD_VALUE) {
      setRecording(true);
      return;
    }
    shortcutChoose(value)
      .then(() => {
        refresh();
        onChanged();
      })
      .catch((e: unknown) => setError(String(e)));
  };

  if (!view) return null;
  const note = view.rows.find((row) => row.value === view.chosen)?.note ?? null;
  return (
    <section className="card">
      <h2>{view.heading}</h2>
      <p>{view.about}</p>
      <label className="setting-row">
        <select
          className="select"
          value={view.chosen}
          disabled={recording}
          onChange={(e) => choose(e.target.value)}
        >
          {view.rows.map((row) => (
            <option key={row.value} value={row.value}>
              {row.label}
            </option>
          ))}
          <option value={RECORD_VALUE}>{view.record_option}</option>
        </select>
      </label>
      {recording && (
        <div className="setting-row">
          <span>{view.press_the_keys}</span>
          <button type="button" className="button" onClick={() => setRecording(false)}>
            {view.cancel}
          </button>
        </div>
      )}
      {note && <p className="card-meta">{note}</p>}
      {error && <p className="card-error">{error}</p>}
    </section>
  );
}

/**
 * How Macalla starts: for now only whether it opens at login. The system
 * keeps that record, so the card reads it back after every change rather
 * than trusting the switch.
 */
function GeneralCard() {
  const [view, setView] = useState<LaunchAtLogin | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = () => {
    launchAtLogin()
      .then(setView)
      .catch((e: unknown) => setError(String(e)));
  };

  useEffect(refresh, []);

  const change = (on: boolean) => {
    setError(null);
    launchAtLoginSet(on)
      .catch((e: unknown) => setError(String(e)))
      .finally(refresh);
  };

  if (!view) return null;
  return (
    <section className="card">
      <h2>{view.heading}</h2>
      <p>{view.about}</p>
      <label className="setting-row">
        <input type="checkbox" checked={view.on} onChange={(e) => change(e.target.checked)} />
        <span>{view.label}</span>
      </label>
      {error && <p className="card-error">{error}</p>}
    </section>
  );
}

function AboutView({
  onBack,
  onOpenLicences,
}: {
  onBack: () => void;
  onOpenLicences: () => void;
}) {
  const [info, setInfo] = useState<About | null>(null);
  const [linkError, setLinkError] = useState<string | null>(null);

  useEffect(() => {
    about()
      .then(setInfo)
      .catch(() => setInfo(null));
  }, []);

  return (
    <main className="cards">
      <button type="button" className="button" onClick={onBack}>
        Back
      </button>
      <section className="card">
        <h2>Macalla</h2>
        <p>{info ? `Version ${info.version}. ${info.licence} licence.` : "Loading…"}</p>
        <p>Local-first dictation. Nothing leaves this machine unless you turn it on.</p>
        {info?.links.map((link) => (
          <p key={link.id}>
            <button
              type="button"
              className="link"
              onClick={() => {
                setLinkError(null);
                openLink(link.id).catch((e: unknown) => setLinkError(String(e)));
              }}
            >
              {link.label}
            </button>{" "}
            <span className="card-meta">{link.url}</span>
          </p>
        ))}
        {linkError && <p className="card-error">{linkError}</p>}
      </section>
      {info && info.attributions.length > 0 && (
        <section className="card">
          <h2>Models</h2>
          <ul className="attributions">
            {info.attributions.map((a) => (
              <li key={a.name}>
                <span className="item-name">{a.name}</span>{" "}
                <span className="item-status">{a.licence}</span>
                <p className="card-meta">{a.notice}</p>
              </li>
            ))}
          </ul>
        </section>
      )}
      <section className="card">
        <h2>Open-source software</h2>
        <p>The open-source libraries Macalla is built on, and their licences.</p>
        <p>
          Parakeet and the other downloadable speech models run on
          transcribe.cpp (MIT).
        </p>
        <button type="button" className="button" onClick={onOpenLicences}>
          View licences
        </button>
      </section>
    </main>
  );
}

function LicensesView({ onBack }: { onBack: () => void }) {
  const [families, setFamilies] = useState<LicenseFamily[] | null>(null);

  useEffect(() => {
    licenses()
      .then(setFamilies)
      .catch(() => setFamilies([]));
  }, []);

  const crateCount = families
    ? families.reduce((n, f) => n + f.crates.length, 0)
    : 0;

  return (
    <main className="cards">
      <button type="button" className="button" onClick={onBack}>
        Back
      </button>
      <section className="card">
        <h2>Open-source software</h2>
        <p>
          Macalla is MIT-licensed and built on the open-source libraries below,
          grouped by licence. Model licences are on the About screen.
        </p>
        {families && (
          <p className="card-meta">
            {families.length} licences · {crateCount} libraries
          </p>
        )}
      </section>
      {!families && <p>Loading…</p>}
      {families?.map((fam) => (
        <section className="card" key={fam.spdx}>
          <h2>
            {fam.name} <span className="item-status">{fam.spdx}</span>
          </h2>
          <ul className="attributions">
            {fam.crates.map((c) => (
              <li key={`${c.name} ${c.version}`}>
                <span className="item-name">
                  {c.name} {c.version}
                </span>
                {c.copyright.map((line) => (
                  <p className="card-meta" key={line}>
                    {line}
                  </p>
                ))}
                {c.repository && <p className="card-meta">{c.repository}</p>}
              </li>
            ))}
          </ul>
          <details>
            <summary>Licence text</summary>
            <pre className="licence-text">{fam.text}</pre>
          </details>
        </section>
      ))}
    </main>
  );
}

function when(tsMs: number): string {
  return new Date(tsMs).toLocaleString();
}

/** Which history item the current selection sits in, and what it says. */
function selectedInHistory(): { id: number; heard: string } | null {
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0 || selection.isCollapsed) {
    return null;
  }
  const node = selection.getRangeAt(0).commonAncestorContainer;
  const element = node instanceof Element ? node : node.parentElement;
  const item = element?.closest<HTMLElement>("[data-history-id]");
  const id = Number(item?.dataset.historyId);
  const heard = selection.toString().trim();
  if (!item || !Number.isFinite(id) || heard === "") {
    return null;
  }
  return { id, heard };
}

/**
 * Said in place of the "Add to dictionary" form when `dictionaryWords`
 * itself could not be loaded. Deliberately lives only here, not in
 * `dictionary_words.rs` (which explains why in a comment where the
 * constant would otherwise be): it is shown exactly when the catalogue
 * that sentence would come from could not be fetched, so asking
 * `dictionaryWords` again would be the exact call that just failed. This
 * is the one sentence in the app with nowhere else to come from.
 */
const WORDS_NOT_LOADED =
  "Could not load the dictionary's own words, so Add to dictionary isn't available right now. Try again.";

/**
 * "Add to dictionary" from a dictation, prefilled with what the engine heard.
 * Select the misheard words in the item first and they arrive as the spoken
 * form; the written form is the one thing only the person knows. `words` is
 * the dictionary's own catalogue (`dictionaryWords`), shared with the Files
 * page's correction offer, so the button and the two notices read the same
 * everywhere.
 */
function AddFromHistory({
  heard,
  words,
  onDone,
}: {
  heard: string;
  words: DictionaryWords;
  onDone: (notice: string) => void;
}) {
  return (
    <TermForm
      initial={{ written: "", spoken: heard ? [heard] : [] }}
      submit={words.add_to_dictionary}
      onSubmit={(written, spoken) =>
        dictionaryAdd(written, spoken).then((created) =>
          onDone(fill(created ? words.added : words.updated, { written })),
        )
      }
      onCancel={() => onDone("")}
    />
  );
}

function HistoryView({ onBack }: { onBack: () => void }) {
  const [items, setItems] = useState<HistoryItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState<number | null>(null);
  const [adding, setAdding] = useState<{ id: number; heard: string } | null>(null);
  const [selected, setSelected] = useState<{ id: number; heard: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [off, setOff] = useState(false);
  const [words, setWords] = useState<DictionaryWords | null>(null);
  const [wordsError, setWordsError] = useState<string | null>(null);

  useEffect(() => {
    historyRetention()
      .then((r) => setOff(r.current === "off"))
      .catch(() => setOff(false));
    dictionaryWords()
      .then((w) => {
        setWords(w);
        setWordsError(null);
      })
      .catch(() => {
        setWords(null);
        setWordsError(WORDS_NOT_LOADED);
      });
  }, []);

  // The "add" button only exists while words are selected inside an item:
  // a whole dictation is never a dictionary term, and a button on every
  // paragraph read as if it were.
  useEffect(() => {
    const onChange = () => setSelected(selectedInHistory());
    document.addEventListener("selectionchange", onChange);
    return () => document.removeEventListener("selectionchange", onChange);
  }, []);

  const refresh = () => {
    historyList(100)
      .then((list) => {
        setItems(list);
        setError(null);
      })
      .catch((e: unknown) => setError(String(e)));
  };

  useEffect(refresh, []);

  // The row whose recording is on its way to the provider again, and what
  // came of a send that did not replace the text.
  const [sending, setSending] = useState<number | null>(null);
  const [sendError, setSendError] = useState<{ id: number; message: string } | null>(null);

  // A recording held or let go, and a send that came back, change what the
  // page offers and what a row says.
  useEffect(() => {
    const subscriptions = [
      listen<number | null>("history-held", () => refresh()),
      listen<SentAgain>("history-sent-again", (e) => {
        setSending(null);
        setSendError(e.payload.error === null ? null : { id: e.payload.id, message: e.payload.error });
        refresh();
      }),
    ];
    return () => {
      subscriptions.forEach((s) => s.then((unlisten) => unlisten()));
    };
  }, []);

  const sendAgain = (id: number, tsMs: number) => {
    setSending(id);
    setSendError(null);
    historySendAgain(id, tsMs).catch((e: unknown) => {
      setSending(null);
      setSendError({ id, message: String(e) });
    });
  };

  const copy = (id: number) => {
    historyCopy(id)
      .then(() => setCopied(id))
      .catch((e: unknown) => setError(String(e)));
  };

  const remove = (id: number) => {
    historyDelete(id)
      .then(refresh)
      .catch((e: unknown) => setError(String(e)));
  };

  return (
    <main className="cards">
      <button type="button" className="button" onClick={onBack}>
        Back
      </button>
      <section className="card">
        <h2>History</h2>
        <p>
          What you dictated, newest first. Nothing here leaves this machine. Select a
          misheard word to add it to the dictionary.
        </p>
        {error && <p className="card-error">{error}</p>}
        {notice && <p className="card-meta">{notice}</p>}
        {items && items.length === 0 && (
          <p className="card-meta">
            {off
              ? "History is off, so nothing is kept. To turn it back on, go Back and change Keep for on the History card."
              : "Nothing yet."}
          </p>
        )}
      </section>
      {items &&
        items.map((item) => (
          <section className="card history-item" key={item.id} data-history-id={item.id}>
            <p className="history-text">{item.text}</p>
            {item.raw.trim() !== item.text.trim() && (
              <p className="history-raw">Heard: {item.raw}</p>
            )}
            {item.send_again && (
              <p className="card-meta">
                The provider did not transcribe this, so this is what this machine heard.
              </p>
            )}
            <div className="history-row">
              <span className="card-meta">{when(item.ts_ms)}</span>
              <span className="row-actions">
                {item.send_again && (
                  <button
                    type="button"
                    className="button"
                    disabled={sending === item.id}
                    onClick={() => sendAgain(item.id, item.ts_ms)}
                  >
                    {sending === item.id ? "Sending…" : "Send again"}
                  </button>
                )}
                {selected?.id === item.id && adding?.id !== item.id && (
                  <button
                    type="button"
                    className="button"
                    // Pressing the button must not collapse the selection
                    // it exists for.
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => setAdding({ id: item.id, heard: selected.heard })}
                  >
                    Add selection to dictionary
                  </button>
                )}
                <button type="button" className="button" onClick={() => copy(item.id)}>
                  {copied === item.id ? "Copied" : "Copy"}
                </button>
                <button type="button" className="button" onClick={() => remove(item.id)}>
                  Delete
                </button>
              </span>
            </div>
            {sendError?.id === item.id && <p className="card-error">{sendError.message}</p>}
            {adding?.id === item.id &&
              (words ? (
                <AddFromHistory
                  heard={adding.heard}
                  words={words}
                  onDone={(text) => {
                    setAdding(null);
                    setNotice(text === "" ? null : text);
                  }}
                />
              ) : (
                wordsError && <p className="card-error">{wordsError}</p>
              ))}
          </section>
        ))}
    </main>
  );
}

function HistoryCard({ onOpen }: { onOpen: () => void }) {
  const [retention, setRetention] = useState<RetentionView | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    historyRetention()
      .then(setRetention)
      .catch((e: unknown) => setError(String(e)));
  }, []);

  const choose = (value: string) => {
    historySetRetention(value)
      .then(() => historyRetention())
      .then((view) => {
        setRetention(view);
        setError(null);
      })
      .catch((e: unknown) => setError(String(e)));
  };

  return (
    <section className="card">
      <h2>History</h2>
      <p>
        {retention?.current === "off"
          ? "History is off. Nothing you dictate is written to this machine, and what was kept before has been erased. Audio is never saved."
          : "What you dictated, kept in a file on this machine and never sent anywhere, for thirty days at most. Audio is never saved."}
      </p>
      {retention && (
        <label className="setting-row">
          <span>Keep for</span>
          <select
            className="select"
            value={retention.current}
            onChange={(e) => choose(e.target.value)}
          >
            {retention.choices.map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </select>
        </label>
      )}
      {retention && retention.current !== "off" && (
        <p className="card-meta">
          A shorter setting applies at once. Choosing "Not at all" erases everything kept so far.
        </p>
      )}
      {error && <p className="card-error">{error}</p>}
      <button type="button" className="button" onClick={onOpen}>
        Open History
      </button>
    </section>
  );
}

/** A length of audio as minutes and seconds. */
function minutes(ms: number): string {
  const total = Math.round(ms / 1000);
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
}

/**
 * A job's bar across the card: `done` of `of` fills it, and `busy` slides a
 * band along the part not yet filled while a step with no count of its own
 * runs, such as one part of a summary. `label` names the bar and `status`,
 * the line shown beside it, is what a screen reader says for its value.
 */
function WorkBar({
  label,
  status,
  done,
  of,
  busy,
}: {
  label: string;
  status: string;
  done: number;
  of: number;
  busy: boolean;
}) {
  const percent = of > 0 ? Math.min(100, (done / of) * 100) : 0;
  return (
    <div
      className="work-bar"
      role="progressbar"
      aria-label={label}
      aria-valuetext={status}
      aria-valuemin={0}
      aria-valuemax={of > 0 ? of : undefined}
      aria-valuenow={busy ? undefined : done}
    >
      <div className="work-bar-fill" style={{ width: `${percent}%` }} />
      {busy && <div className="work-bar-rest" style={{ left: `${percent}%` }} />}
    </div>
  );
}

/** How far a summary's progress has got, to tell the newer of two apart. */
function progressRank(progress: SummaryProgress): number {
  return progress.part * 2 + (progress.stage === "finished" ? 1 : 0);
}

function FilesCard({ onOpen }: { onOpen: () => void }) {
  return (
    <section className="card">
      <h2>Files</h2>
      <p>
        Turn a recording, or a transcript you received, into text you can copy. The file stays
        on this machine.
      </p>
      <button type="button" className="button" onClick={onOpen}>
        Open Files
      </button>
    </section>
  );
}

/**
 * One list of a summary. Each quote is followed by who said it and when, a
 * button that goes to that turn in the transcript (`onGoTo`).
 */
function SummaryList({
  heading,
  items,
  onGoTo,
}: {
  heading: string;
  items: MeetingItem[];
  onGoTo: (turn: number) => void;
}) {
  return (
    <>
      <p className="item-name">{heading}</p>
      {items.length === 0 ? (
        <p className="card-meta">None.</p>
      ) : (
        <ul>
          {items.map((item, i) => (
            <li key={i}>
              {item.owner ? `${item.owner}: ` : ""}
              {item.text} “{item.quote}”{" "}
              <button type="button" className="link quote-said" onClick={() => onGoTo(item.turn)}>
                {item.said}
              </button>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}

/**
 * The live meeting card: Start while nothing runs, and while one records,
 * how much was heard, each source's line, and Stop and keep or Discard.
 * Every word on it comes from Rust (`meeting_words.rs`), through `words` and
 * the meeting's progress.
 */
function LiveMeetingCard({
  words,
  live,
  blocked,
  onStart,
  onStop,
  onDiscard,
}: {
  words: LiveWords;
  live: RunningLive | null;
  blocked: boolean;
  onStart: () => void;
  onStop: () => void;
  onDiscard: () => void;
}) {
  const progress = live?.progress ?? null;
  return (
    <section className="card">
      <h2>{words.heading}</h2>
      <p>{words.about}</p>
      {live ? (
        <>
          <div className="status-row">
            <p className="card-meta">
              {live.stopping ? words.stopping : progress ? progress.heard : words.starting}
            </p>
            <div className="button-row">
              <button type="button" className="button" disabled={live.stopping} onClick={onStop}>
                {words.stop}
              </button>
              <button type="button" className="button" disabled={live.stopping} onClick={onDiscard}>
                {words.discard}
              </button>
            </div>
          </div>
          {progress?.microphone_only && <p className="card-meta">{progress.microphone_only}</p>}
          {progress && <p className="card-meta">{progress.you}</p>}
          {progress?.others && (
            <p className={progress.others_notice ? "card-error" : "card-meta"}>{progress.others}</p>
          )}
        </>
      ) : (
        <div className="button-row">
          <button type="button" className="button" disabled={blocked} onClick={onStart}>
            {words.start}
          </button>
        </div>
      )}
    </section>
  );
}

/**
 * A catalogue template (`meeting_words.rs`) with its named placeholders,
 * `{name}`, filled with data. A placeholder with no value is left as it is.
 */
function fill(template: string, values: Record<string, string | number>): string {
  return template.replace(/\{(\w+)\}/g, (whole, name: string) =>
    name in values ? String(values[name]) : whole,
  );
}

/**
 * Of the meeting on the page and a view of a meeting just heard of, the one
 * to show: the view, when it is of the same meeting and at least as recent.
 * A correction's reply and a summary's event can arrive in either order; the
 * app raises `revision` under its lock with each change, so the later change
 * wins whichever arrives last.
 */
function newer(shown: MeetingView | null, view: MeetingView): MeetingView | null {
  return shown && shown.job === view.job && view.revision >= shown.revision ? view : shown;
}

/**
 * The meeting to show once the page's first `meetingCurrent()` reply comes
 * back, given what an event may already have put on the page. The reply was
 * built when it was asked for, so it can be older than an event that
 * overtook it: it wins only when it is of a later meeting, or of the same
 * meeting and at least as recent.
 */
function firstLoad(shown: MeetingView | null, reply: MeetingView | null): MeetingView | null {
  if (shown === null) return reply;
  if (reply === null) return shown;
  if (reply.job !== shown.job) return reply.job > shown.job ? reply : shown;
  return reply.revision >= shown.revision ? reply : shown;
}

/** One match of a find in the transcript: its turn, and where in that turn's words. */
interface FindMatch {
  turn: number;
  start: number;
  end: number;
}

/**
 * Every match of `query` in the turns' words, in reading order, ignoring
 * case. The query is matched as typed, not as a pattern. A query that is
 * only whitespace matches nothing. Speakers' names are not searched: a match
 * is shown in the words it was found in.
 */
function findMatches(turns: MeetingTurn[], query: string): FindMatch[] {
  if (query.trim() === "") {
    return [];
  }
  const pattern = new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "giu");
  const found: FindMatch[] = [];
  turns.forEach((turn, i) => {
    for (const match of turn.text.matchAll(pattern)) {
      const start = match.index ?? 0;
      found.push({ turn: i, start, end: start + match[0].length });
    }
  });
  return found;
}

/**
 * A turn's words with its find matches marked, the current one distinct.
 * `first` is the index, among all matches, of this turn's first; `markRef`
 * receives the current match's element, for the page to scroll to.
 */
function TurnWords({
  text,
  matches,
  first,
  current,
  markRef,
}: {
  text: string;
  matches: FindMatch[];
  first: number;
  current: number;
  markRef: (element: HTMLElement | null) => void;
}) {
  if (matches.length === 0) {
    return <>{text}</>;
  }
  const pieces: React.ReactNode[] = [];
  let at = 0;
  matches.forEach((match, i) => {
    if (match.start > at) {
      pieces.push(text.slice(at, match.start));
    }
    const isCurrent = first + i === current;
    pieces.push(
      <mark
        key={i}
        className={isCurrent ? "find-match find-current" : "find-match"}
        ref={isCurrent ? markRef : undefined}
      >
        {text.slice(match.start, match.end)}
      </mark>,
    );
    at = match.end;
  });
  if (at < text.length) {
    pieces.push(text.slice(at));
  }
  return <>{pieces}</>;
}

/**
 * The transcript's turns, with find above them and a way to correct each
 * turn by hand. Find is the page's own: every match highlighted, the current
 * one distinct, Next and Previous (Enter and Shift-Enter in the box), and a
 * count. A correction goes to the app (`meetingEditTurn`), which keeps it and
 * returns the meeting, passed to `onEdited`. In the text area, Enter saves
 * and Escape cancels. Every word here comes from Rust, through `words`.
 * `turnRefs` and `shownTurn` are the page's, for a summary quote to go to a
 * turn.
 *
 * A correction that looks like one mishearing is offered as one
 * quiet line under the turn just saved (`meetingCorrectionOffer`), with a
 * button that opens History's own form, pre-filled, and ends in History's
 * own notices. The offer goes when another turn is edited, this meeting is
 * left for another, or the button is used.
 */
function Transcript({
  meeting,
  words,
  turnRefs,
  shownTurn,
  onEdited,
}: {
  meeting: MeetingView;
  words: TranscriptWords;
  turnRefs: React.RefObject<(HTMLElement | null)[]>;
  shownTurn: number | null;
  onEdited: (view: MeetingView) => void;
}) {
  const [query, setQuery] = useState("");
  const [current, setCurrent] = useState(0);
  // `before` is the turn's text as it stood the moment Edit was pressed,
  // not whatever `meeting.turns[turn].text` says by the time Save runs --
  // the two usually agree, but capturing it at the click makes that not
  // something `save` has to get right by coincidence.
  const [editing, setEditing] = useState<{ turn: number; text: string; before: string } | null>(
    null,
  );
  const [saving, setSaving] = useState(false);
  const [editError, setEditError] = useState<string | null>(null);
  // The turn whose Edit button takes focus back once its editing ends.
  const [returnTo, setReturnTo] = useState<number | null>(null);
  const currentMark = useRef<HTMLElement | null>(null);
  const editButtons = useRef<(HTMLButtonElement | null)[]>([]);

  // The offer to add a hand correction to the dictionary, shown under the
  // turn just saved: the run Macalla heard and the run it was
  // corrected to. `addingOffer` is the same offer once its button opened
  // History's form; `addNotice` is what that form's submit said. All three
  // go away when another turn is edited, a new meeting opens, or the page
  // is left -- see the effect below and the Edit button's own click.
  const [offer, setOffer] = useState<({ turn: number } & CorrectionOffer) | null>(null);
  const [addingOffer, setAddingOffer] = useState<({ turn: number } & CorrectionOffer) | null>(
    null,
  );
  const [addNotice, setAddNotice] = useState<{ turn: number; text: string } | null>(null);
  // Bumped by anything that makes an in-flight `meetingCorrectionOffer`
  // reply stale -- starting a new edit, or a new meeting -- so a late
  // reply for an edit already superseded, or for a meeting already left,
  // is dropped instead of resurrecting an offer for the wrong turn.
  const offerRequest = useRef(0);

  const matches = useMemo(() => findMatches(meeting.turns, query), [meeting.turns, query]);
  const shown = matches.length === 0 ? -1 : Math.min(current, matches.length - 1);
  const byTurn = useMemo(() => {
    const grouped = new Map<number, { first: number; matches: FindMatch[] }>();
    matches.forEach((match, i) => {
      const group = grouped.get(match.turn);
      if (group) {
        group.matches.push(match);
      } else {
        grouped.set(match.turn, { first: i, matches: [match] });
      }
    });
    return grouped;
  }, [matches]);

  // Another meeting: whatever was being corrected, or offered, belonged to
  // the last one.
  useEffect(() => {
    setEditing(null);
    setEditError(null);
    setOffer(null);
    setAddingOffer(null);
    setAddNotice(null);
    offerRequest.current += 1;
  }, [meeting.job]);

  // The current match is brought into view as it moves, and as the query
  // changes, as a browser's find does. A match in the turn being corrected
  // has no mark while its text area is open, so that turn's row is brought
  // into view instead.
  const shownTurnOfMatch = shown < 0 ? null : matches[shown].turn;
  const inEdited = editing !== null && shownTurnOfMatch === editing.turn;
  useEffect(() => {
    if (shown < 0) {
      return;
    }
    const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const target = inEdited && shownTurnOfMatch !== null ? turnRefs.current[shownTurnOfMatch] : currentMark.current;
    target?.scrollIntoView({ behavior: still ? "auto" : "smooth", block: "nearest" });
  }, [shown, query, inEdited]);

  useEffect(() => {
    if (returnTo === null || editing !== null) {
      return;
    }
    editButtons.current[returnTo]?.focus();
    setReturnTo(null);
  }, [returnTo, editing]);

  const go = (step: number) => {
    if (matches.length === 0) {
      return;
    }
    setCurrent((((shown < 0 ? 0 : shown) + step) % matches.length + matches.length) % matches.length);
  };
  const cancel = () => {
    if (editing) {
      setReturnTo(editing.turn);
    }
    setEditing(null);
    setEditError(null);
  };
  const save = () => {
    if (!editing || saving) {
      return;
    }
    const { turn, text, before } = editing;
    // Captured before `meetingEditTurn` is even sent, so a meeting change
    // that lands while it is still in flight is caught the moment its
    // reply arrives, not only once the correction-offer request after it
    // is sent.
    const requestId = ++offerRequest.current;
    setSaving(true);
    meetingEditTurn(meeting.job, turn, text)
      .then((view) => {
        if (offerRequest.current !== requestId) {
          return; // a later edit, or another meeting, already superseded this save
        }
        onEdited(view);
        setEditing(null);
        setEditError(null);
        setReturnTo(turn);
        setAddingOffer(null);
        setAddNotice(null);
        const after = view.turns[turn]?.text ?? "";
        // `before` is this turn's text as it was when Edit was pressed for
        // THIS correction. Correcting the same turn a second time in a row
        // means `before` is the person's own first correction, not what
        // the engine originally heard -- so the offer's "Macalla heard"
        // names that first correction. Accepted: `TermForm` shows the
        // exact words before anything is saved, so it costs a look, not a
        // wrong entry.
        meetingCorrectionOffer(before, after)
          .then((found) => {
            if (offerRequest.current !== requestId) {
              return; // superseded by a later edit, or another meeting
            }
            setOffer(found ? { turn, heard: found.heard, written: found.written } : null);
          })
          .catch(() => {
            if (offerRequest.current === requestId) {
              setOffer(null);
            }
          });
      })
      .catch((e: unknown) => setEditError(String(e)))
      .finally(() => setSaving(false));
  };

  return (
    <>
      {meeting.turns.length > 0 && (
        <div className="find-row">
          <input
            type="search"
            className="field"
            placeholder={words.find}
            aria-label={words.find}
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setCurrent(0);
            }}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.nativeEvent.isComposing) {
                e.preventDefault();
                go(e.shiftKey ? -1 : 1);
              }
            }}
          />
          {/* Always there, and empty with no query, so a screen reader
              announces the first count as well as later ones. */}
          <span className="card-meta find-count" role="status">
            {query.trim() === ""
              ? ""
              : matches.length === 0
                ? words.none
                : fill(words.count, { current: shown + 1, total: matches.length })}
          </span>
          <button
            type="button"
            className="button"
            disabled={matches.length === 0}
            onClick={() => go(-1)}
          >
            {words.previous}
          </button>
          <button
            type="button"
            className="button"
            disabled={matches.length === 0}
            onClick={() => go(1)}
          >
            {words.next}
          </button>
        </div>
      )}
      <div className="history-text">
        {meeting.turns.map((turn, i) => {
          const setRef = (element: HTMLElement | null) => {
            turnRefs.current[i] = element;
          };
          const className = shownTurn === i ? "turn-shown" : undefined;
          const label = turn.label ? <strong>{turn.label}: </strong> : null;
          if (editing?.turn === i) {
            return (
              <div key={i} ref={setRef} tabIndex={-1} className={`turn-editing ${className ?? ""}`}>
                {label}
                <textarea
                  className="field turn-field"
                  aria-label={words.field}
                  value={editing.text}
                  rows={Math.max(2, Math.ceil(editing.text.length / 70))}
                  autoFocus
                  disabled={saving}
                  onChange={(e) => setEditing({ turn: i, text: e.target.value, before: editing.before })}
                  onKeyDown={(e) => {
                    if (e.key === "Escape") {
                      e.preventDefault();
                      cancel();
                    } else if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                      e.preventDefault();
                      save();
                    }
                  }}
                />
                <div className="button-row">
                  <button type="button" className="button" disabled={saving} onClick={save}>
                    {words.save}
                  </button>
                  <button type="button" className="button" disabled={saving} onClick={cancel}>
                    {words.cancel}
                  </button>
                </div>
                {editError && <p className="card-error">{editError}</p>}
              </div>
            );
          }
          const found = byTurn.get(i);
          return (
            <Fragment key={i}>
              <p ref={setRef} tabIndex={-1} className={className}>
                {label}
                <TurnWords
                  text={turn.text}
                  matches={found?.matches ?? []}
                  first={found?.first ?? 0}
                  current={shown}
                  markRef={(element) => {
                    currentMark.current = element;
                  }}
                />{" "}
                <button
                  type="button"
                  className="link turn-edit"
                  ref={(element) => {
                    editButtons.current[i] = element;
                  }}
                  disabled={editing !== null}
                  aria-label={turn.edit_label}
                  onClick={() => {
                    setEditError(null);
                    // Another turn is edited: whatever this turn was
                    // offering goes away too, and any correction-offer
                    // reply still in flight for a previous edit is now
                    // stale.
                    setOffer(null);
                    setAddingOffer(null);
                    setAddNotice(null);
                    offerRequest.current += 1;
                    setEditing({ turn: i, text: turn.text, before: turn.text });
                  }}
                >
                  {words.edit}
                </button>
              </p>
              {offer?.turn === i && (
                <p className="card-meta correction-offer">
                  {fill(words.correction_offer, { heard: offer.heard, written: offer.written })}{" "}
                  <button
                    type="button"
                    className="link"
                    onClick={() => {
                      setAddingOffer(offer);
                      setOffer(null);
                    }}
                  >
                    {words.add_to_dictionary}
                  </button>
                </p>
              )}
              {addingOffer?.turn === i && (
                <TermForm
                  initial={{ written: addingOffer.written, spoken: [addingOffer.heard] }}
                  submit={words.add_to_dictionary}
                  onSubmit={(written, spoken) =>
                    dictionaryAdd(written, spoken).then((created) => {
                      setAddNotice({
                        turn: i,
                        text: fill(created ? words.added : words.updated, { written }),
                      });
                      setAddingOffer(null);
                    })
                  }
                  onCancel={() => setAddingOffer(null)}
                />
              )}
              {addNotice?.turn === i && <p className="card-meta">{addNotice.text}</p>}
            </Fragment>
          );
        })}
      </div>
    </>
  );
}

/**
 * The batch card: several files heard in turn, one row each, with where the
 * file has got, where its transcript was saved, or why not; Stop while it
 * runs, and Clear list once it has ended. Every word on it comes from Rust
 * (`meeting_words.rs`), through `words` and the batch itself.
 */
function BatchCard({
  words,
  batch,
  onStop,
  onClear,
}: {
  words: LiveWords;
  batch: BatchView;
  onStop: () => void;
  onClear: () => void;
}) {
  return (
    <section className="card">
      <h2>{words.batch_heading}</h2>
      <p>{words.batch_about}</p>
      <div className="status-row">
        <p className="card-meta" role="status">
          {batch.summary}
        </p>
        {batch.running ? (
          <button type="button" className="button" disabled={batch.stopping} onClick={onStop}>
            {words.batch_stop}
          </button>
        ) : (
          <button type="button" className="button" onClick={onClear}>
            {words.batch_clear}
          </button>
        )}
      </div>
      <ul className="batch-list">
        {batch.rows.map((row, i) => (
          <li key={i} className={`batch-row batch-${row.state}`}>
            <div className="item-text">
              <span className="item-name">{row.name}</span>
              <span className={row.state === "failed" ? "card-error" : "item-status"}>
                {row.status}
              </span>
            </div>
            {row.state === "hearing" && (
              <WorkBar
                label={row.name}
                status={row.status}
                done={row.done}
                of={row.of}
                busy={row.of === 0}
              />
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** A job the page is waiting on: which file, and how far it has got. */
interface Working {
  job: number;
  name: string;
  phase: MeetingProgress["phase"];
  done: number;
  of: number;
}

/** Said when the Enhancement choice changed after the page showed where text goes. */
const CHOICE_CHANGED =
  "Your Enhancement choice changed after this page showed where the text would go, so nothing was sent. Check the page, then try again.";

/**
 * What a drop, or a pick in the open panel, opens: one file opens as the
 * current meeting, several start a batch, and every path is used. Both give
 * only the paths the app recorded (`files-dropped`, from `files.rs`'s
 * `ChosenFiles::record_dropped`, and `meetingChooseFile`), so a folder is
 * never counted. `null` for none. Pure, so it is checked without a real OS
 * drop.
 */
const dropTargets = (
  paths: string[],
): { kind: "one"; path: string } | { kind: "batch"; paths: string[] } | null =>
  paths.length === 0
    ? null
    : paths.length === 1
      ? { kind: "one", path: paths[0] }
      : { kind: "batch", paths };

/**
 * A language's name in the person's own language, from its BCP-47 code: the
 * web view's `Intl.DisplayNames`, which follows the system's language. Names
 * are data; the sentences around them come from Rust. The code itself when
 * the web view has no name for it.
 */
function languageName(code: string): string {
  try {
    return new Intl.DisplayNames(undefined, { type: "language" }).of(code) ?? code;
  } catch {
    return code;
  }
}

/** A translation sentence from Rust, its languages named. */
function sayTranslation(said: TranslationSaid): string {
  return fill(said.text, { source: languageName(said.source), target: languageName(said.target) });
}

/**
 * The translation card, under the transcript: a copy of the current meeting
 * in another language, translated on this Mac, with its own Save. Every
 * sentence on it comes from Rust (`translate_words.rs`); only language names
 * are the page's own, from `languageName`. The choice of language lists what
 * the translator lists; where there is no translator at all, the card says
 * why and offers nothing. A pair that cannot be translated says why under
 * the choice, before anything is sent.
 *
 * `blocked` is another job running; `onRunning` tells the page whether a
 * translation is, so the page's other buttons wait for it too. `saveFormat`
 * is the Files card's one choice of format.
 */
function TranslationCard({
  meeting,
  blocked,
  saveFormat,
  onRunning,
}: {
  meeting: MeetingView;
  blocked: boolean;
  saveFormat: string | null;
  onRunning: (running: boolean) => void;
}) {
  const [options, setOptions] = useState<TranslationOptions | null>(null);
  const [target, setTarget] = useState<string | null>(null);
  // Why the chosen pair cannot be translated, or `null` when it can; and
  // which target that answer is for, so a late one is not shown for another.
  const [check, setCheck] = useState<{ target: string; said: TranslationSaid | null } | null>(null);
  const [running, setRunning] = useState<RunningTranslation | null>(null);
  const [copy, setCopy] = useState<TranslationView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const ended = useRef(new Set<number>());

  useEffect(() => {
    onRunning(running !== null);
  }, [running]);

  useEffect(() => {
    meetingTranslationOptions()
      .then(setOptions)
      .catch((e: unknown) => setError(String(e)));
  }, []);

  // The languages to offer, by name, without the one translated from.
  const languages = useMemo(() => {
    if (!options) {
      return [];
    }
    const from = options.source.split("-")[0];
    return options.languages
      .filter((code) => code !== options.source && code !== from)
      .map((code) => ({ code, name: languageName(code) }))
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [options]);
  const chosen = target ?? languages[0]?.code ?? null;

  // Asked again when the window comes back, as well as on a new choice: the
  // person may have just added the language in System Settings, which is
  // what the card tells them to do.
  const [returned, setReturned] = useState(0);
  useEffect(() => {
    const back = () => {
      if (document.visibilityState === "visible") {
        setReturned((n) => n + 1);
      }
    };
    window.addEventListener("focus", back);
    document.addEventListener("visibilitychange", back);
    return () => {
      window.removeEventListener("focus", back);
      document.removeEventListener("visibilitychange", back);
    };
  }, []);

  useEffect(() => {
    if (chosen === null) {
      return;
    }
    let open = true;
    meetingTranslationCheck(chosen)
      .then((said) => open && setCheck({ target: chosen, said }))
      .catch((e: unknown) => open && setError(String(e)));
    return () => {
      open = false;
    };
  }, [chosen, returned]);

  // The copy kept for this meeting, again whenever the meeting changes: a
  // new meeting drops it, and a correction marks it out of date.
  useEffect(() => {
    meetingTranslation()
      .then((found) => setCopy(found && found.job === meeting.job ? found : null))
      .catch((e: unknown) => setError(String(e)));
  }, [meeting.job, meeting.revision]);

  useEffect(() => {
    const subscriptions = [
      listen<TranslationProgress>("translation-progress", (e) => {
        setRunning((r) => (r && r.job === e.payload.job ? { ...r, progress: e.payload } : r));
      }),
      listen<TranslationView>("meeting-translation", (e) => {
        setRunning(null);
        setCopy(e.payload);
        setError(null);
      }),
      listen<TranslationFailed>("translation-error", (e) => {
        ended.current.add(e.payload.job);
        setRunning((r) => (r && r.job !== e.payload.job ? r : null));
        setError(sayTranslation(e.payload.said));
      }),
      listen<number>("translation-ended", (e) => {
        ended.current.add(e.payload);
        setRunning((r) => (r && r.job !== e.payload ? r : null));
      }),
    ];
    let open = true;
    Promise.all(subscriptions)
      .then(() => meetingRunningTranslation())
      .then((found) => {
        if (open && found && !ended.current.has(found.job)) {
          setRunning((r) => r ?? found);
        }
      })
      .catch((e: unknown) => open && setError(String(e)));
    return () => {
      open = false;
      subscriptions.forEach((sub) => sub.then((unlisten) => unlisten()));
    };
  }, []);

  if (!options) {
    return null;
  }
  const words = options.words;
  const pairSaid = check && check.target === chosen ? check.said : null;
  const ready = chosen !== null && check?.target === chosen && check.said === null;
  const translate = () => {
    if (chosen === null) {
      return;
    }
    setError(null);
    setNotice(null);
    meetingTranslate(chosen)
      .then((job) => {
        if (!ended.current.has(job)) {
          setRunning((r) => r ?? { job, target: chosen, stopping: false, progress: null });
        }
      })
      .catch((e: unknown) => setError(String(e)));
  };
  const progress = running?.progress ?? null;
  const status = running
    ? running.stopping
      ? words.stopping
      : fill(words.progress, {
          current: progress ? Math.min(progress.done + 1, progress.of) : 1,
          total: progress?.of ?? meeting.turns.length,
        })
    : "";

  return (
    <section className="card">
      <h2>{words.heading}</h2>
      <p>{words.about}</p>
      {options.unavailable ? (
        <p className="card-meta">{sayTranslation(options.unavailable)}</p>
      ) : running ? (
        <>
          <WorkBar
            label={words.heading}
            status={status}
            done={progress?.done ?? 0}
            of={progress?.of ?? 0}
            busy={!progress || progress.done < progress.of}
          />
          <div className="status-row">
            <p className="card-meta">{status}</p>
            <button
              type="button"
              className="button"
              disabled={running.stopping}
              onClick={() => {
                setRunning((r) => r && { ...r, stopping: true });
                meetingCancel().catch((e: unknown) => {
                  setRunning((r) => r && { ...r, stopping: false });
                  setError(String(e));
                });
              }}
            >
              {words.stop}
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="setting-row">
            <label htmlFor="translate-into">{words.into}</label>
            <select
              id="translate-into"
              className="select"
              value={chosen ?? ""}
              onChange={(e) => setTarget(e.target.value)}
            >
              {languages.map((l) => (
                <option key={l.code} value={l.code}>
                  {l.name}
                </option>
              ))}
            </select>
            <button type="button" className="button" disabled={!ready || blocked} onClick={translate}>
              {words.translate}
            </button>
          </div>
          {pairSaid && <p className="card-meta">{sayTranslation(pairSaid)}</p>}
        </>
      )}
      {error && <p className="card-error">{error}</p>}
      {copy && (
        <>
          <p className="card-meta">
            {fill(words.made, {
              source: languageName(copy.source),
              target: languageName(copy.target),
            })}
          </p>
          {copy.out_of_date && <p className="summary-stale">{words.out_of_date}</p>}
          <div className="history-text" lang={copy.target}>
            {copy.meeting.turns.map((turn, i) => (
              <p key={i}>
                {turn.label ? <strong>{turn.label}: </strong> : null}
                {turn.text}
              </p>
            ))}
          </div>
          <div className="button-row">
            <button
              type="button"
              className="button"
              onClick={() =>
                meetingCopyTranslation()
                  .then(() => setNotice(words.copied))
                  .catch((e: unknown) => setError(String(e)))
              }
            >
              {words.copy}
            </button>
            <button
              type="button"
              className="button"
              disabled={saveFormat === null}
              onClick={() => {
                if (saveFormat === null) {
                  return;
                }
                meetingSaveTranslation(saveFormat)
                  .then((path) => setNotice(fill(words.saved, { path })))
                  .catch((e: unknown) => setError(String(e)));
              }}
            >
              {words.save}
            </button>
          </div>
          {notice && <p className="card-meta">{notice}</p>}
        </>
      )}
    </section>
  );
}

function FilesView({ onBack }: { onBack: () => void }) {
  const [meeting, setMeeting] = useState<MeetingView | null>(null);
  const [working, setWorking] = useState<Working | null>(null);
  const [summarising, setSummarising] = useState(false);
  // While a summary runs: when it was asked for, the part it is on, and the
  // time now, ticked every second so one long part still visibly moves.
  const [summaryStarted, setSummaryStarted] = useState(0);
  const [summaryProgress, setSummaryProgress] = useState<SummaryProgress | null>(null);
  const [now, setNow] = useState(() => Date.now());
  // Stop was pressed: the part in hand still has to come back before the
  // summary ends.
  const [stopping, setStopping] = useState(false);
  // The summary's job, once the app has started it. Until then there is no
  // job for Stop to reach, so Stop waits for it.
  const [summaryJob, setSummaryJob] = useState<number | null>(null);
  // Where the running summary sends the transcript, and how long a part can
  // take: as the page showed them when Summarise was pressed, or as the
  // summary itself reports them when the page reopens while it runs.
  const [summaryBy, setSummaryBy] = useState<string | null>(null);
  const [summaryPace, setSummaryPace] = useState<string | null>(null);
  // Which press of Summarise is current, so a job number that comes back late
  // from an earlier one is not taken for this one's.
  const summaryRun = useRef(0);
  // The job of the summary this page started, so a summary found running
  // keeps this page's provider label only when it is that same summary.
  const pressedJob = useRef<number | null>(null);
  const [destinations, setDestinations] = useState<MeetingDestinations | null>(null);
  // The live meeting card's words, and the live meeting recording, if any.
  const [liveWords, setLiveWords] = useState<LiveWords | null>(null);
  const [live, setLive] = useState<RunningLive | null>(null);
  // The last batch, running or ended, and the Files card's one choice of
  // format, by its extension: what the current meeting's Save button writes
  // and what the next batch saves in. `null` means the first format Rust
  // offers.
  const [batch, setBatch] = useState<BatchView | null>(null);
  const [format, setFormat] = useState<string | null>(null);
  const [tidy, setTidy] = useState(false);
  // A translation of the current meeting is running (`TranslationCard`).
  const [translating, setTranslating] = useState(false);
  // The transcript's turns, for a summary quote to go to, and the turn gone
  // to last, highlighted briefly. `n` tells two presses on one turn apart.
  const turnRefs = useRef<(HTMLElement | null)[]>([]);
  const [shownTurn, setShownTurn] = useState<{ turn: number; n: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Jobs that have already sent their result or error. A fast job, such as a
  // transcript file, can end before its number comes back from the open call,
  // and must not then be shown as running.
  const ended = useRef(new Set<number>());
  // Jobs ended since the page opened, counted so a summary found running is
  // not shown as running after it has ended.
  const endings = useRef(0);

  // Whether the first `meetingCurrent()` reply has come back. Until it has,
  // the page has nothing to compare a summary's event with, and `newer`
  // would drop it.
  const loaded = useRef(false);

  useEffect(() => {
    meetingCurrent()
      .then((view) => {
        loaded.current = true;
        setMeeting((m) => firstLoad(m, view));
      })
      .catch((e: unknown) => {
        loaded.current = true;
        setError(String(e));
      });
    meetingDestinations()
      .then(setDestinations)
      .catch(() => setDestinations(null));
    meetingLiveWords()
      .then(setLiveWords)
      .catch(() => setLiveWords(null));
  }, []);

  useEffect(() => {
    const subscriptions = [
      // Every open of the speech engine ends in `idle`, after the engine it
      // opened is lent to this page, and so does every dictation. Asking
      // again then keeps the engine line true: an engine chosen, downloaded
      // or still loading when the page opened shows once it is ready, and
      // the not-ready line comes back if an open fails.
      listen<string>("dictation-state", (e) => {
        if (e.payload === "idle") {
          meetingDestinations()
            .then(setDestinations)
            .catch(() => setDestinations(null));
        }
      }),
      listen<MeetingProgress>("meeting-progress", (e) => {
        setWorking((w) =>
          w && w.job === e.payload.job
            ? { ...w, phase: e.payload.phase, done: e.payload.done, of: e.payload.of }
            : w,
        );
      }),
      listen<SummaryProgress>("summary-progress", (e) => {
        setSummaryProgress(e.payload);
      }),
      listen<LiveProgress>("live-meeting-progress", (e) => {
        setLive((l) => (l && l.job === e.payload.job ? { ...l, progress: e.payload } : l));
      }),
      listen<MeetingView>("meeting-transcript", (e) => {
        ended.current.add(e.payload.job);
        setWorking((w) => (w && w.job !== e.payload.job ? w : null));
        setLive((l) => (l && l.job !== e.payload.job ? l : null));
        setMeeting(e.payload);
        setError(null);
      }),
      listen<MeetingView>("meeting-summary", (e) => {
        endings.current += 1;
        setSummarising(false);
        // Only for the meeting still on the page: never one cleared, or
        // replaced, while its summary was being written. Nor over a
        // correction made after the summary was kept: `newer`. Before the
        // first load there is nothing on the page yet to be cleared, so the
        // event is kept, and `firstLoad` weighs the reply against it. Read
        // now, not in the updater: the reply can set it before this
        // updater runs.
        const early = !loaded.current;
        setMeeting((m) => (early && m === null ? e.payload : newer(m, e.payload)));
        setError(null);
      }),
      listen<BatchChanged>("batch-changed", (e) => {
        const changed = e.payload.batch;
        // An older batch's late event never replaces a newer batch.
        setBatch((b) => (b && b.job > changed.job ? b : changed));
      }),
      listen<BatchView>("batch-ended", (e) => {
        setBatch((b) => (b && b.job > e.payload.job ? b : e.payload));
      }),
      listen<MeetingError>("meeting-error", (e) => {
        endings.current += 1;
        ended.current.add(e.payload.job);
        setWorking((w) => (w && w.job !== e.payload.job ? w : null));
        setLive((l) => (l && l.job !== e.payload.job ? l : null));
        setSummarising(false);
        setError(e.payload.message);
      }),
    ];
    // A summary still running from before the page was left is shown again,
    // with Stop, once the events that end it are being heard. One that ends
    // while it is being asked about is not.
    let open = true;
    // A live meeting still recording from before the page was left is shown
    // again, with its buttons, once the events that end it are being heard.
    // The last batch, running or ended, once the events that change it are
    // being heard. One heard since the page opened is newer and stays.
    Promise.all(subscriptions)
      .then(() => meetingBatch())
      .then((found) => {
        if (open && found) {
          setBatch((b) => (b && b.job >= found.job ? b : found));
        }
      })
      .catch((e: unknown) => {
        if (open) {
          setError(String(e));
        }
      });
    Promise.all(subscriptions)
      .then(() => meetingRunningLive())
      .then((running) => {
        if (open && running && !ended.current.has(running.job)) {
          setLive((l) => l ?? running);
        }
      })
      .catch((e: unknown) => {
        if (open) {
          setError(String(e));
        }
      });
    Promise.all(subscriptions)
      .then(() => {
        const before = endings.current;
        return meetingRunningSummary().then((running) => {
          if (!open || !running || endings.current !== before) {
            return;
          }
          const at = Date.now();
          const found = running.progress;
          summaryRun.current += 1;
          setSummaryStarted(at - running.elapsed_ms);
          setNow(at);
          // A progress event heard since the page opened can be newer than
          // the reply, so the further on of the two is kept.
          setSummaryProgress((heard) =>
            heard &&
            (!found || (heard.job === found.job && progressRank(heard) >= progressRank(found)))
              ? heard
              : found,
          );
          // Before its summariser is built the summary has no name for where
          // it sends the transcript. The label this page showed is kept only
          // for the summary this page started; any other says no name rather
          // than one that may be wrong.
          const ours = running.job === pressedJob.current;
          setSummaryBy((shown) => running.destination ?? (ours ? shown : null));
          setSummaryPace((shown) => running.pace ?? (ours ? shown : null));
          setStopping(running.stopping);
          setSummaryJob(running.job);
          setSummarising(true);
          setError(null);
        });
      })
      .catch((e: unknown) => {
        if (open) {
          setError(String(e));
        }
      });
    return () => {
      open = false;
      subscriptions.forEach((s) => s.then((unlisten) => unlisten()));
    };
  }, []);

  useEffect(() => {
    if (!summarising) {
      return;
    }
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(tick);
  }, [summarising]);

  // A file dropped anywhere on the window, or chosen in the open panel. Each
  // carries the cleanup choice behind the tidy destination the person can
  // see, and the app refuses to tidy with any other.
  const tidyWith =
    tidy && destinations?.tidy_choice && !destinations.tidy_unavailable
      ? destinations.tidy_choice
      : null;
  // Read by the drop listener, which is not subscribed again on every
  // progress event: a drop while a file or a summary runs is ignored.
  const batchRunning = batch !== null && batch.running;
  const busy = useRef(false);
  busy.current = working !== null || summarising || live !== null || batchRunning || translating;
  const saveFormat = format ?? liveWords?.formats[0]?.extension ?? null;
  const openPath = (path: string) => {
    if (busy.current) {
      return;
    }
    const name = path.split(/[\\/]/).pop() ?? path;
    const shown = tidyWith;
    setError(null);
    setNotice(null);
    // Asked again first, so an engine that finished loading after the page
    // opened is seen, and a tidy choice that changed is refused here.
    meetingDestinations()
      .then((fresh) => {
        setDestinations(fresh);
        if (shown !== null && fresh.tidy_choice !== shown) {
          setError(CHOICE_CHANGED);
          return;
        }
        const sent = meetingOpenFile(path, shown);
        // Tidying is for this file only: the next one starts unticked.
        setTidy(false);
        return sent.then((job) => {
          if (!ended.current.has(job)) {
            setWorking({ job, name, phase: "hearing", done: 0, of: 0 });
          }
        });
      })
      .catch((err: unknown) => setError(String(err)));
  };
  // Several files at once: each heard in turn and saved to Downloads, never
  // tidied, and never the current meeting.
  const startBatch = (paths: string[]) => {
    if (busy.current || saveFormat === null) {
      return;
    }
    setError(null);
    setNotice(null);
    meetingBatchStart(paths, saveFormat)
      .then((started) =>
        // An event heard before the reply is newer than it.
        setBatch((b) => (b && b.job >= started.job ? b : started)),
      )
      .catch((e: unknown) => setError(String(e)));
  };
  const openTargets = (paths: string[]) => {
    const targets = dropTargets(paths);
    if (targets?.kind === "one") {
      openPath(targets.path);
    } else if (targets?.kind === "batch") {
      startBatch(targets.paths);
    }
  };
  const chooseFile = () => {
    meetingChooseFile()
      .then(openTargets)
      .catch((e: unknown) => setError(String(e)));
  };
  // Subscribed again when the tick or the destination changes, so a drop
  // never carries a stale choice.
  useEffect(() => {
    // The paths the app recorded from the drop, not the drop's own: a
    // folder, or a path that could not be recorded, is already left out.
    const unlisten = listen<string[]>("files-dropped", (e) => {
      openTargets(e.payload);
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, [tidyWith, saveFormat]);

  const startLive = () => {
    setError(null);
    setNotice(null);
    meetingLiveStart()
      .then((job) => {
        if (!ended.current.has(job)) {
          setLive((l) => l ?? { job, stopping: false, progress: null });
        }
      })
      .catch((e: unknown) => setError(String(e)));
  };
  const endLive = (end: () => Promise<void>) => {
    setLive((l) => l && { ...l, stopping: true });
    end().catch((e: unknown) => {
      setLive((l) => l && { ...l, stopping: false });
      setError(String(e));
    });
  };

  // A summary quote's button: scroll its turn into view, move focus there so
  // the keyboard carries on from it, and highlight it briefly.
  const goToTurn = (turn: number) => {
    const element = turnRefs.current[turn];
    if (!element) {
      return;
    }
    const still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    element.scrollIntoView({ behavior: still ? "auto" : "smooth", block: "center" });
    element.focus({ preventScroll: true });
    setShownTurn((shown) => ({ turn, n: (shown?.n ?? 0) + 1 }));
  };
  useEffect(() => {
    if (shownTurn === null) {
      return;
    }
    const timer = window.setTimeout(() => setShownTurn(null), 2000);
    return () => window.clearTimeout(timer);
  }, [shownTurn]);

  // Save the kept meeting in the Files card's one choice of format.
  const saveMeeting = () => {
    if (saveFormat === null) {
      return;
    }
    meetingSave(saveFormat)
      .then((path) => setNotice(`Saved to ${path}.`))
      .catch((e: unknown) => setError(String(e)));
  };

  const summarise = () => {
    const shown = destinations?.summary_choice;
    if (!shown) {
      return;
    }
    const run = ++summaryRun.current;
    const started = Date.now();
    setSummaryStarted(started);
    setNow(started);
    setSummaryProgress(null);
    setStopping(false);
    setSummaryJob(null);
    setSummaryBy(destinations?.summary ?? null);
    setSummaryPace(destinations?.summary_pace ?? null);
    setSummarising(true);
    setError(null);
    meetingDestinations()
      .then((fresh) => {
        setDestinations(fresh);
        if (fresh.summary_choice !== shown) {
          // As in the catch below: never hide a summary found running since.
          if (run === summaryRun.current) {
            setSummarising(false);
          }
          setError(CHOICE_CHANGED);
          return;
        }
        return meetingSummarise(shown).then((job) => {
          if (run === summaryRun.current) {
            pressedJob.current = job;
            setSummaryJob(job);
          }
        });
      })
      .catch((e: unknown) => {
        // A refusal for a press that a summary found running has since
        // replaced must not hide that summary.
        if (run === summaryRun.current) {
          setSummarising(false);
        }
        setError(String(e));
      });
  };

  // Progress for the meeting on the page only, matched on its job as
  // `meeting-summary` is.
  const shownProgress =
    summaryProgress && meeting && summaryProgress.job === meeting.job ? summaryProgress : null;
  // The status lines beside each bar. Tidying names the part in hand: the
  // first report comes as tidying starts, before any part is answered.
  const workingStatus = !working
    ? ""
    : working.phase === "tidying"
      ? `Tidying, part ${Math.min(working.done + 1, working.of)} of ${working.of}`
      : working.of > 0
        ? `${minutes(working.done)} of ${minutes(working.of)} heard`
        : "Opening…";
  const summaryStatus = stopping
    ? shownProgress
      ? "Stopping when this part comes back"
      : "Stopping"
    : `Summarising with ${summaryBy ?? "your Enhancement provider"}${
        shownProgress ? `, part ${shownProgress.part} of ${shownProgress.of}` : ""
      }`;

  return (
    <main className="cards">
      <button type="button" className="button" onClick={onBack}>
        Back
      </button>
      <section className="card">
        <h2>Files</h2>
        <p>
          Turn a recording, or a transcript you received, into text you can copy. The file stays
          on this machine.
        </p>
        {working ? (
          <>
            <p className="card-meta">{working.name}</p>
            <WorkBar
              label="File progress"
              status={workingStatus}
              done={working.done}
              of={working.of}
              busy={working.of === 0 || (working.phase === "tidying" && working.done < working.of)}
            />
            <div className="status-row">
              <p className="card-meta">{workingStatus}</p>
              <button
                type="button"
                className="button"
                onClick={() => meetingCancel().catch((e: unknown) => setError(String(e)))}
              >
                Stop
              </button>
            </div>
          </>
        ) : (
          <>
            <p className="card-meta">
              Drop an audio or video file here, or a transcript file you already have (.vtt or .txt).{" "}
              {destinations?.engine
                ? `Recordings are heard on this machine by ${destinations.engine}.`
                : "No speech engine is ready, so only transcripts can be opened."}
            </p>
            {destinations?.tidy_unavailable && (
              <p className="card-meta">{destinations.tidy_unavailable}</p>
            )}
            {destinations?.tidy && !destinations.tidy_unavailable && (
              <label className="setting-row">
                <input type="checkbox" checked={tidy} onChange={(e) => setTidy(e.target.checked)} />
                <span>
                  Tidy this file with {destinations.tidy}. It sends the transcript there in parts.
                </span>
              </label>
            )}
            {/* Open while a batch runs: a batch looks its format up once, when
                it starts (`meeting_batch_start` in files.rs), and keeps it, so
                a change here moves only the current meeting's Save and the
                next batch. */}
            {liveWords && liveWords.formats.length > 0 && (
              <label className="setting-row">
                <span>{liveWords.save_transcripts_as}</span>
                <select
                  className="select"
                  value={saveFormat ?? ""}
                  onChange={(e) => setFormat(e.target.value)}
                >
                  {liveWords.formats.map((f) => (
                    <option key={f.extension} value={f.extension}>
                      {f.label}
                    </option>
                  ))}
                </select>
              </label>
            )}
            <div className="button-row">
              <button
                type="button"
                className="button"
                disabled={summarising || live !== null || batchRunning || translating}
                onClick={chooseFile}
              >
                Choose a file…
              </button>
            </div>
          </>
        )}
        {error && <p className="card-error">{error}</p>}
        {notice && <p className="card-meta">{notice}</p>}
      </section>
      {liveWords && batch && (
        <BatchCard
          words={liveWords}
          batch={batch}
          onStop={() => {
            setBatch((b) => b && { ...b, stopping: true });
            meetingCancel().catch((e: unknown) => {
              setBatch((b) => b && { ...b, stopping: false });
              setError(String(e));
            });
          }}
          onClear={() =>
            meetingBatchClear()
              .then(() => setBatch(null))
              .catch((e: unknown) => setError(String(e)))
          }
        />
      )}
      {liveWords && (
        <LiveMeetingCard
          words={liveWords}
          live={live}
          blocked={working !== null || summarising || batchRunning || translating}
          onStart={startLive}
          onStop={() => endLive(meetingLiveStop)}
          onDiscard={() => endLive(meetingCancel)}
        />
      )}
      {/* A finished summary waits for the catalogue's words: without them an
          out of date one would look current. A running summary's progress
          and Stop need none of them. */}
      {meeting && (summarising || (meeting.summary && liveWords)) && (
        <section className="card">
          <h2>Summary</h2>
          {summarising ? (
            <>
              <WorkBar
                label="Summary progress"
                status={summaryStatus}
                done={
                  shownProgress
                    ? shownProgress.stage === "finished"
                      ? shownProgress.part
                      : shownProgress.part - 1
                    : 0
                }
                of={shownProgress?.of ?? 0}
                busy={!shownProgress || shownProgress.stage === "started"}
              />
              <div className="status-row">
                <div>
                  <p className="card-meta">{summaryStatus}</p>
                  <p className="card-meta">{minutes(Math.max(0, now - summaryStarted))} so far</p>
                </div>
                <button
                  type="button"
                  className="button"
                  disabled={stopping || summaryJob === null}
                  onClick={() => {
                    setStopping(true);
                    meetingCancel().catch((e: unknown) => {
                      setStopping(false);
                      setError(String(e));
                    });
                  }}
                >
                  Stop
                </button>
              </div>
              {summaryPace && <p className="card-meta">{summaryPace}</p>}
            </>
          ) : (
            meeting.summary &&
            liveWords && (
              <>
                {meeting.summary_out_of_date && (
                  <p className="summary-stale">{liveWords.summary_out_of_date}</p>
                )}
                <p>
                  {liveWords.summary_quotes} Written by{" "}
                  {meeting.summary_by ?? "your Enhancement provider"}.
                </p>
                {meeting.summary.map((part, i) => (
                  <div key={i}>
                    {meeting.summary && meeting.summary.length > 1 && (
                      <p className="card-meta">
                        Part {i + 1}, {minutes(part.start_ms)} to {minutes(part.end_ms)}
                      </p>
                    )}
                    {part.error ? (
                      <p className="card-error">{part.error}</p>
                    ) : (
                      <>
                        <SummaryList
                          heading={liveWords?.key_points ?? ""}
                          items={part.key_points}
                          onGoTo={goToTurn}
                        />
                        <SummaryList
                          heading={liveWords?.decisions ?? ""}
                          items={part.decisions}
                          onGoTo={goToTurn}
                        />
                        <SummaryList
                          heading={liveWords?.action_items ?? ""}
                          items={part.action_items}
                          onGoTo={goToTurn}
                        />
                        {part.dropped > 0 && (
                          <p className="card-meta">
                            {part.dropped === 1
                              ? "1 line was left out because it did not quote the file."
                              : `${part.dropped} lines were left out because they did not quote the file.`}
                          </p>
                        )}
                      </>
                    )}
                  </div>
                ))}
              </>
            )
          )}
        </section>
      )}
      {meeting && (
        <section className="card">
          <h2>Transcript</h2>
          <p className="card-meta">
            {meeting.name}
            {meeting.audio_ms !== null ? `, ${minutes(meeting.audio_ms)} of audio` : ""},{" "}
            {meeting.turns.length} {meeting.turns.length === 1 ? "turn" : "turns"}
            {meeting.tidied
              ? `. Tidied by ${meeting.tidied.destination}${
                  meeting.tidied.unchanged > 0
                    ? `; ${meeting.tidied.unchanged} of ${meeting.tidied.parts} parts kept as heard`
                    : ""
                }`
              : ""}
            .
          </p>
          {meeting.held && <p className="card-meta">{meeting.held}</p>}
          {meeting.ended_early && <p className="card-error">{meeting.ended_early}</p>}
          {liveWords ? (
            <Transcript
              meeting={meeting}
              words={liveWords.transcript}
              turnRefs={turnRefs}
              shownTurn={shownTurn?.turn ?? null}
              onEdited={(view) => setMeeting((m) => newer(m, view))}
            />
          ) : (
            <div className="history-text">
              {meeting.turns.map((turn, i) => (
                <p
                  key={i}
                  ref={(element) => {
                    turnRefs.current[i] = element;
                  }}
                  tabIndex={-1}
                  className={shownTurn?.turn === i ? "turn-shown" : undefined}
                >
                  {turn.label ? <strong>{turn.label}: </strong> : null}
                  {turn.text}
                </p>
              ))}
            </div>
          )}
          <div className="button-row">
            <button
              type="button"
              className="button"
              onClick={() =>
                meetingCopy()
                  .then(() => setNotice("Copied."))
                  .catch((e: unknown) => setError(String(e)))
              }
            >
              Copy
            </button>
            <button
              type="button"
              className="button"
              disabled={saveFormat === null}
              onClick={saveMeeting}
            >
              {liveWords?.save ?? ""}
            </button>
            <button
              type="button"
              className="button"
              disabled={
                !destinations?.summary ||
                summarising ||
                working !== null ||
                live !== null ||
                batchRunning ||
                translating
              }
              onClick={summarise}
            >
              {summarising
                ? "Summarising…"
                : destinations?.summary
                  ? `Summarise with ${destinations.summary}`
                  : "Summarise"}
            </button>
            <button
              type="button"
              className="button"
              disabled={
                working !== null || summarising || live !== null || batchRunning || translating
              }
              onClick={() =>
                meetingClear()
                  .then(() => setMeeting(null))
                  .catch((e: unknown) => setError(String(e)))
              }
            >
              Clear
            </button>
          </div>
          {/* The meeting, and its summary with it, is kept in memory only, not
              in History (src-tauri/src/files.rs, `Meetings`). Save writes the
              Files card's one choice of format. Text, SRT and WebVTT all
              write the transcript alone (`transcript_text` /
              `vtt_turns`, as Copy does); only Markdown also writes the
              summary (`meeting_markdown`). The with-a-summary line is new
              wording, so it is a catalogue entry
              (`summary_gone_unless_markdown`); the no-summary line predates
              the wording rule and stays inline, unchanged. A summary whose
              every part failed or came back empty writes nothing to Markdown
              (`meeting_markdown` leaves out an empty list and its heading),
              so it gets the no-summary line. */}
          <p className="card-meta">
            {meeting.summary?.some(
              (part) =>
                part.key_points.length + part.decisions.length + part.action_items.length > 0,
            )
              ? (liveWords?.summary_gone_unless_markdown ?? "")
              : "The transcript is gone when you quit, unless you save it."}
          </p>
          {!destinations?.summary && (
            <p className="card-meta">
              To summarise, choose an Enhancement provider on the Models page.
            </p>
          )}
        </section>
      )}
      {meeting && (
        <TranslationCard
          meeting={meeting}
          blocked={working !== null || summarising || live !== null || batchRunning}
          saveFormat={saveFormat}
          onRunning={setTranslating}
        />
      )}
    </main>
  );
}

function isView(value: unknown): value is View {
  return (
    value === "settings" ||
    value === "history" ||
    value === "files" ||
    value === "dictionary" ||
    value === "about" ||
    value === "licenses" ||
    value === "models"
  );
}

export default function App() {
  const [view, setView] = useState<View>("settings");
  const [shortcutName, setShortcutName] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // The last dictation refusal shown, so the live meeting's end clears that
  // notice and no other.
  const refused = useRef<string | null>(null);
  const [modelsTarget, setModelsTarget] = useState<ModelsTarget>(null);

  useEffect(() => {
    // What the dictation thread could not do, said where the person is
    // looking. Cleared on the next successful dictation.
    const subscriptions = [
      listen<string>("dictation-error", (e) => setNotice(e.payload)),
      listen<string>("dictation", () => setNotice(null)),
      // A dictation refused while a live meeting records: said until the
      // meeting ends, and then gone, since dictation works again.
      listen<string>("dictation-refused", (e) => {
        refused.current = e.payload;
        setNotice(e.payload);
      }),
      listen<number>("live-meeting-ended", () => {
        const said = refused.current;
        refused.current = null;
        setNotice((n) => (n !== null && n === said ? null : n));
      }),
    ];
    return () => {
      subscriptions.forEach((s) => s.then((unlisten) => unlisten()));
    };
  }, []);

  const refreshShortcutName = () => {
    shortcut()
      .then(setShortcutName)
      .catch(() => setShortcutName(null));
  };

  useEffect(refreshShortcutName, []);

  useEffect(() => {
    const subscription = listen<string>("navigate", (e) => {
      if (isView(e.payload)) {
        setView(e.payload);
      }
    });
    return () => {
      subscription.then((unlisten) => unlisten());
    };
  }, []);

  if (view === "about") {
    return (
      <AboutView
        onBack={() => setView("settings")}
        onOpenLicences={() => setView("licenses")}
      />
    );
  }
  if (view === "licenses") {
    return <LicensesView onBack={() => setView("about")} />;
  }
  if (view === "history") {
    return <HistoryView onBack={() => setView("settings")} />;
  }
  if (view === "files") {
    return <FilesView onBack={() => setView("settings")} />;
  }
  if (view === "dictionary") {
    return <DictionaryView onBack={() => setView("settings")} />;
  }
  if (view === "models") {
    return (
      <ModelsView
        onBack={() => setView("settings")}
        target={modelsTarget}
        onTargetReached={() => setModelsTarget(null)}
      />
    );
  }

  // Settings top-level sections, in the fixed order. Ogham is omitted: it
  // only appears once the adapter is compiled in, which it is not yet.
  return (
    <main className="cards">
      {notice && <p className="notice-bar">{notice}</p>}
      <SetupCard shortcutName={shortcutName} />
      <HistoryCard onOpen={() => setView("history")} />
      <FilesCard onOpen={() => setView("files")} />
      <DictionaryCard onOpen={() => setView("dictionary")} />
      <ShortcutCard onChanged={refreshShortcutName} />
      <MicrophoneCard />
      <EngineCard
        onOpenModels={() => {
          setModelsTarget("transcription");
          setView("models");
        }}
      />
      <PlaceholderSection
        title="Enhancement"
        detail="What Enhancement does with what you say."
        button={{
          label: "Open Enhancement on Models",
          onClick: () => {
            setModelsTarget("enhancement");
            setView("models");
          },
        }}
      />
      <GeneralCard />
    </main>
  );
}
