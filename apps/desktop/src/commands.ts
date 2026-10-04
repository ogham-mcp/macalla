// SPDX-License-Identifier: MIT
//
// Hand-written mirrors of the response models in
// `src-tauri/src/commands.rs`. Tauri commands are the only bridge between
// the UI and the Rust core; no `specta` here, so this file and
// the Rust structs it mirrors have to be kept in sync by hand.

import { invoke } from "@tauri-apps/api/core";

/** Mirrors `src-tauri::commands::StepSummary`. */
export interface StepSummary {
  id: string;
  title: string;
  detail: string;
  state: "done" | "required" | "optional";
  action: string | null;
  /** A permission this platform asks for, rather than the rest of the list. */
  permission: boolean;
  /** The permission has an ask the page can offer (`requestPermission`). */
  ask: boolean;
  /** The permission has a settings pane the page can open (`openSettingsPane`). */
  settings: boolean;
}

/** Mirrors `src-tauri::models::ModelView`. */
export interface ModelView {
  id: string;
  name: string;
  megabytes: number;
  licence: string;
  attribution: string;
  state: "installed" | "partial" | "absent" | "downloading";
  bytes: number;
}

/** Mirrors `src-tauri::models::ProgressEvent`, on the `model-progress` event. */
export interface ModelProgress {
  id: string;
  bytes: number;
  total: number;
  phase: "downloading" | "reconnecting" | "verifying" | "preparing";
}

/** Mirrors `src-tauri::models::ModelError`, on the `model-error` event. */
export interface ModelError {
  id: string;
  message: string;
}

/**
 * Mirrors `src-tauri::dictation::LoudnessView`: which colour the overlay
 * draws a waveform bar in.
 */
export type LoudnessView = "normal" | "loud" | "near-clipping";

/**
 * Mirrors `src-tauri::dictation::DictationLevel`, on the `dictation-level`
 * event, once per poll: the loudness of the audio just captured.
 * `rms_dbfs` is the RMS in dBFS (0 at full scale, a large negative number
 * for silence), mapped to a bar's height by the overlay; `loudness` is the
 * colour that bar is drawn in.
 */
export interface DictationLevel {
  rms_dbfs: number;
  loudness: LoudnessView;
}

/**
 * Mirrors `src-tauri::dictation::CloudWaiting`, on the `dictation-cloud-waiting`
 * event: the recording is with a cloud provider and no answer has come yet.
 * `provider` is already the provider's display name, not its id.
 */
export interface CloudWaiting {
  provider: string;
  elapsed_ms: number;
  /** What this Mac heard is in History while the request is out. */
  in_history: boolean;
}

/**
 * Mirrors `src-tauri::dictation::EnhancingKind`: who Enhancement is with, on
 * the `Enhancing` and `EnhancementMissed` payloads.
 */
export type EnhancingKind = "apple-intelligence" | "server-on-this-computer" | "server" | "provider";

/**
 * Mirrors `src-tauri::dictation::Enhancing`, on the `dictation-enhancing`
 * event, about once a second: the paste is waiting on Enhancement.
 * `provider` is the provider's display name when `with` is "provider", and
 * null otherwise.
 */
export interface Enhancing {
  with: EnhancingKind;
  provider: string | null;
  elapsed_ms: number;
}

/**
 * Mirrors `src-tauri::dictation::EnhancementMissed`, on the
 * `dictation-enhancement-missed` event: the text was pasted without the
 * Enhancement it waited for. `in_history` is whether an enhanced text is
 * still on its way: if that request finishes, it replaces the text in History;
 * it can still time out or fail.
 */
export interface EnhancementMissed {
  reason: "out-of-time" | "failed";
  with: EnhancingKind;
  provider: string | null;
  in_history: boolean;
}

/** Every speech model this build can fetch, and whether it is here. */
export function modelsList(): Promise<ModelView[]> {
  return invoke("models_list");
}

/** Where the model files are kept. */
export function modelsDir(): Promise<string> {
  return invoke("models_dir");
}

/** Start a download; progress and the end arrive as events. */
export function modelDownload(id: string): Promise<void> {
  return invoke("model_download", { id });
}

/** Stop a download. The partial file stays and the next download resumes it. */
export function modelCancel(id: string): Promise<void> {
  return invoke("model_cancel", { id });
}

export function modelRemove(id: string): Promise<void> {
  return invoke("model_remove", { id });
}

/**
 * The overlay's ×: throw the running dictation away, as Esc twice does.
 * Does nothing once it has ended. Only the overlay window may call it.
 */
export function dictationCancel(): Promise<void> {
  return invoke("dictation_cancel");
}

/**
 * The overlay's ✓: stop the running dictation and paste, as the tray's Stop
 * Dictation does. Does nothing once its recording has ended. Only the overlay
 * window may call it.
 */
export function dictationFinish(): Promise<void> {
  return invoke("dictation_finish");
}

/** The onboarding checklist: what still stands between the user and dictating. */
export function setupSteps(): Promise<StepSummary[]> {
  return invoke("setup_steps");
}

/** The shortcut to hold, the way the key caps read. */
export function shortcut(): Promise<string> {
  return invoke("shortcut");
}

/**
 * The input device the next dictation listens through, as the OS names it,
 * or null until a microphone has been opened. Changes arrive on the
 * `dictation-device` event with the same payload.
 */
export function inputDevice(): Promise<string | null> {
  return invoke("input_device");
}

/** Mirrors `src-tauri::commands::MicrophoneRow`: one `<option>` for the
 * Microphone card's `<select>`, its label already composed in Rust
 * (user-facing wording lives in one catalogue) so the card and the tray's Microphone submenu
 * never say the same row differently. `value` is `""` for the system
 * default row, matching the `<select>`'s own empty-value convention. */
export interface MicrophoneRow {
  value: string;
  label: string;
}

/** Mirrors `src-tauri::commands::MicrophoneView`. */
export interface MicrophoneView {
  names: string[];
  chosen: string | null;
  rows: MicrophoneRow[];
}

/** The microphones connected now, and which one is chosen. */
export function microphones(): Promise<MicrophoneView> {
  return invoke("microphones");
}

/** Listen through this microphone from now on; null follows the system default. */
export function microphoneChoose(name: string | null): Promise<void> {
  return invoke("microphone_choose", { name });
}

/** Mirrors `src-tauri::login::LaunchAtLoginView`: the General card. */
export interface LaunchAtLogin {
  heading: string;
  about: string;
  label: string;
  on: boolean;
}

export function launchAtLogin(): Promise<LaunchAtLogin> {
  return invoke("launch_at_login");
}

export function launchAtLoginSet(on: boolean): Promise<void> {
  return invoke("launch_at_login_set", { on });
}

/** Mirrors `src-tauri::commands::Attribution`. */
export interface Attribution {
  name: string;
  licence: string;
  notice: string;
  status: string;
}

/** Mirrors `src-tauri::commands::About`. */
export interface About {
  version: string;
  licence: string;
  attributions: Attribution[];
  links: Link[];
}

/** Mirrors `src-tauri::links::Link`: a page on the website. */
export interface Link {
  id: string;
  label: string;
  url: string;
}

/** Open one of the About screen's links, by id, in the default browser. */
export function openLink(id: string): Promise<void> {
  return invoke("open_link", { id });
}

/** Version, licence and model attributions for the About screen. */
export function about(): Promise<About> {
  return invoke("about");
}

/** Mirrors `src-tauri::commands::CrateNotice`. */
export interface CrateNotice {
  name: string;
  version: string;
  repository: string;
  copyright: string[];
}

/** Mirrors `src-tauri::commands::LicenseFamily`. */
export interface LicenseFamily {
  spdx: string;
  name: string;
  text: string;
  crates: CrateNotice[];
}

/** Third-party crate licences, grouped by SPDX family, for the About screen's
 * Open-source software page. */
export function licenses(): Promise<LicenseFamily[]> {
  return invoke("licenses");
}

/** Open the System Settings pane where a step's grant is made. */
export function openSettingsPane(step: string): Promise<void> {
  return invoke("open_settings_pane", { step });
}

/** Ask the OS to prompt for a step's permission, where a prompt exists. */
export function requestPermission(step: string): Promise<string> {
  return invoke("request_permission", { step });
}

/** Setup is done: put the window away. */
export function finishSetup(): Promise<void> {
  return invoke("finish_setup");
}

/** Mirrors `src-tauri::history::HistoryItem`. */
export interface HistoryItem {
  id: number;
  ts_ms: number;
  text: string;
  raw: string;
  /** Its recording is held after a failed cloud request, so it can be sent again. */
  send_again: boolean;
}

/** The most recent dictations, newest first. */
export function historyList(limit: number): Promise<HistoryItem[]> {
  return invoke("history_list", { limit });
}

/** Put a past dictation's text on the clipboard. */
export function historyCopy(id: number): Promise<void> {
  return invoke("history_copy", { id });
}

/** Forget a past dictation. */
export function historyDelete(id: number): Promise<void> {
  return invoke("history_delete", { id });
}

/**
 * Send a failed cloud dictation's recording to the provider again. The row is
 * named by its id and its time, as History lists it. The answer arrives on the
 * `history-sent-again` event.
 */
export function historySendAgain(id: number, tsMs: number): Promise<void> {
  return invoke("history_send_again", { id, tsMs });
}

/**
 * Mirrors `src-tauri::dictation::SentAgain`, on the `history-sent-again`
 * event: what sending a row's recording again came to.
 */
export interface SentAgain {
  id: number;
  /** Why the row's text was not replaced, in plain English; null when it was. */
  error: string | null;
}

/** Mirrors `src-tauri::commands::RetentionView`. */
export interface RetentionView {
  current: string;
  choices: { value: string; label: string }[];
}

/** How long history is kept, and the choices. */
export function historyRetention(): Promise<RetentionView> {
  return invoke("history_retention");
}

/** Change how long history is kept. Takes effect at once. */
export function historySetRetention(value: string): Promise<void> {
  return invoke("history_set_retention", { value });
}

/** Mirrors `src-tauri::dictionary::DictionaryInfo`. */
export interface DictionaryInfo {
  path: string;
  terms: number;
}

/** Where the dictionary is and how many terms it has. */
export function dictionaryInfo(): Promise<DictionaryInfo> {
  return invoke("dictionary_info");
}

/** Mirrors `src-tauri::dictionary::TermView`. */
export interface TermView {
  written: string;
  spoken: string[];
}

/** Every term, in file order. */
export function dictionaryTerms(): Promise<TermView[]> {
  return invoke("dictionary_terms");
}

/** Add spoken forms to a written form, creating the term if it is new. */
export function dictionaryAdd(written: string, spoken: string[]): Promise<boolean> {
  return invoke("dictionary_add", { written, spoken });
}

/** Replace one term, possibly renaming it. */
export function dictionaryUpdate(
  previous: string,
  written: string,
  spoken: string[],
): Promise<void> {
  return invoke("dictionary_update", { previous, written, spoken });
}

/** Forget a term. */
export function dictionaryRemove(written: string): Promise<void> {
  return invoke("dictionary_remove", { written });
}

/** Merge a shared JSON file in. Resolves to how many terms were added or changed. */
export function dictionaryImport(json: string): Promise<number> {
  return invoke("dictionary_import", { json });
}

/** Write a copy to Downloads. Resolves to its path. */
export function dictionaryExport(): Promise<string> {
  return invoke("dictionary_export");
}

/**
 * Mirrors `src-tauri::dictionary::DictionaryWordsView`, worded in Rust
 * (`dictionary_words.rs`). `added` and `updated` carry the named
 * placeholder `{written}`, for `fill`.
 */
export interface DictionaryWords {
  add_to_dictionary: string;
  added: string;
  updated: string;
}

/** The dictionary's own catalogue words: History's "Add to dictionary" button and its two notices. */
export function dictionaryWords(): Promise<DictionaryWords> {
  return invoke("dictionary_words");
}

/** Show a file in the file manager. */
export function revealPath(path: string): Promise<void> {
  return invoke("reveal_path", { path });
}

/** Mirrors `src-tauri::commands::ProviderKeyView`. */
export interface ProviderKeyView {
  id: string;
  name: string;
  kinds: string[];
  saved: boolean;
  /** Set when there IS a stored key but reading it failed (the OS access
   * prompt was denied, or the item does not decode) -- `saved` is false
   * alongside it, but this is not "no key", so do not offer "add a key"
   * here without saying it would overwrite one. */
  error: string | null;
}

/** Mirrors `src-tauri::commands::ProviderKeysView`. */
export interface ProviderKeysView {
  providers: ProviderKeyView[];
  /** Set, in place of every provider's own answer, when the key store itself
   * could not be reached. */
  store_unavailable: string | null;
}

/** Every provider this build knows about, and whether a key is stored for it. */
export function providerKeys(): Promise<ProviderKeysView> {
  return invoke("provider_keys");
}

/** Store a key for a provider, replacing whatever was there. */
export function providerKeySave(id: string, key: string): Promise<void> {
  return invoke("provider_key_save", { id, key });
}

/** Remove the stored key for a provider. */
export function providerKeyRemove(id: string): Promise<void> {
  return invoke("provider_key_remove", { id });
}

/** Mirrors `src-tauri::commands::TranscriptionStatus`. */
export type TranscriptionStatus =
  | "ready"
  | "needs-download"
  | "not-compiled-in"
  | "unavailable"
  | "needs-key"
  | "key-store-unavailable"
  | "key-unreadable";

/** Mirrors `src-tauri::commands::TranscriptionOptionView`. */
export interface TranscriptionOptionView {
  /** `"local"` or `"provider"`. */
  source: "local" | "provider";
  id: string;
  name: string;
  /** The maker, for a local engine; empty for a provider. */
  group: string;
  /** One sentence on what it is good for. Empty for a provider. */
  blurb: string;
  status: TranscriptionStatus;
  /** The download description, or the unavailable, needs-key or key-error reason. */
  detail: string | null;
  chosen: boolean;
  /** The catalogue id of the model a local engine runs on, or, for the chosen
   * provider, its own model id when it has one (OpenRouter). Null otherwise. */
  model: string | null;
  /**
   * Words wrong across the everyday, technical and clinical clips (174
   * words total, before the dictionary), or null when nobody has measured
   * this id.
   */
  measured_words_wrong: [number, number, number] | null;
}

/** Mirrors `src-tauri::commands::TranscriptionChoice`. */
export interface TranscriptionChoiceInput {
  source: "local" | "provider";
  id: string;
  /** An OpenRouter model id. Ignored for every other provider. */
  model?: string | null;
}

/**
 * Choose what every dictation transcribes on: a local engine, or a
 * provider's whole-recording request at key release. Refused unless the
 * choice is ready to use -- an engine that is not ready, a provider with no
 * saved key, or OpenRouter with no model chosen.
 */
export function transcriptionChoose(choice: TranscriptionChoiceInput): Promise<void> {
  return invoke("transcription_choose", { choice });
}

/** Mirrors `src-tauri::commands::KeyStateView`. */
export type KeyStateView = "saved" | "missing" | "store-unavailable" | "unreadable";

/** Mirrors `src-tauri::commands::CleanupOptionView`. */
export interface CleanupOptionView {
  /** `"off"`, `"apple-intelligence"`, `"local-server"` or `"provider"`. */
  kind: "off" | "apple-intelligence" | "local-server" | "provider";
  id: string;
  name: string;
  available: boolean;
  /** Apple's own explanation when unavailable, or a provider's key reason. */
  reason: string | null;
  /** The stored key's state, for a provider option only. */
  key: KeyStateView | null;
}

/** Mirrors `src-tauri::commands::PoolModelView`: one ticked model in a pool. */
export interface PoolModelView {
  provider: string;
  model: string;
}

/**
 * Mirrors `src-tauri::commands::CleanupChoiceView`: what the Enhancement section
 * saves and reads back -- off, Apple Intelligence, a person's own
 * OpenAI-compatible server reached with no key, or a pool of cloud models in
 * ticked order.
 */
export type CleanupChoiceView =
  | { kind: "off" }
  | { kind: "apple-intelligence" }
  | { kind: "server"; address: string; model: string }
  | { kind: "pool"; models: PoolModelView[] };

/** Mirrors `src-tauri::commands::PoolProviderView`. */
export interface PoolProviderView {
  id: string;
  name: string;
}

/**
 * Mirrors `src-tauri::commands::OpenedStatusView`: what dictation actually
 * opened with, next to the stored choice above -- which stays as it is when
 * a key goes missing, by design, so this is what says the truth.
 */
export interface OpenedStatusView {
  /** What transcribes right now: the cloud provider's name, or the local
   * engine's. */
  transcription: string;
  /** What Enhancement really sends the text to; `null` when nothing built,
   * or when it built but is not ready to serve yet. */
  enhancement: string | null;
  /** A short phrase naming why Enhancement is not doing what was chosen,
   * e.g. "Anthropic needs a key", "Apple Intelligence is not ready" -- the
   * pool panel's own "Saved" state reads this; its own reason line does
   * not repeat it (the Enhancement card's `enhancement_reason` does).
   * `null` when Enhancement is off, or opened exactly as chosen. */
  enhancement_issue: string | null;
  /** InUseCard's own sentence: the full fallback notice, when the stored
   * transcription choice could not be used. `null` when it opened as
   * chosen. */
  transcription_reason: string | null;
  /** The Enhancement card's own sentence: an existing wording -- the
   * cleanup notice's text, or the partial-pool sentence. `null` when
   * Enhancement opened exactly as chosen, or is off. */
  enhancement_reason: string | null;
}

/**
 * Tidy what you say with `choice` from now on. Refused when Apple
 * Intelligence is not available, a server choice is missing its address or
 * model, or a pool has no models, a provider it cannot hold, or a model
 * ticked twice.
 */
export function cleanupChoose(choice: CleanupChoiceView): Promise<void> {
  return invoke("cleanup_choose", { choice });
}

/** Mirrors `src-tauri::commands::ModelsOverview`. */
export interface ModelsOverview {
  transcription: TranscriptionOptionView[];
  cleanup: CleanupOptionView[];
  keys: ProviderKeysView;
  cloud_transcription_compiled_in: boolean;
  /** The local engine that shows the live preview while the person
   * speaks -- the stored engine choice, or the platform default -- so the
   * page never needs a second call to find it. */
  preview_engine_id: string;
  /** The saved cleanup choice, shown selected when the page opens. */
  cleanup_chosen: CleanupChoiceView;
  /** Providers a cleanup pool can take models from: those with a saved key. */
  pool_providers: PoolProviderView[];
  /** What dictation actually opened with, the truth next to `chosen` and
   * `cleanup_chosen` when a stored choice could not be fully used. `null`
   * before the dictation thread's first open. */
  in_use: OpenedStatusView | null;
}

/**
 * Everything the Models page needs in one call: transcription options,
 * cleanup options and every provider's key state, so the page and the
 * Engine card can never disagree.
 */
export function modelsOverview(): Promise<ModelsOverview> {
  return invoke("models_overview");
}

/** Mirrors `src-tauri::commands::MeasuredCleanupView`. */
export interface MeasuredCleanupView {
  right: number;
  of: number;
  machine: string;
}

/** Mirrors `src-tauri::commands::ServerModelView`. */
export interface ServerModelView {
  id: string;
  size_bytes: number | null;
  parameter_size: string | null;
  quantization: string | null;
  capabilities: string[] | null;
  measured: MeasuredCleanupView | null;
}

/** Mirrors `src-tauri::commands::ServerModelsView`. */
export interface ServerModelsView {
  models: ServerModelView[];
  /** Whether `address` names this machine, so the page can label it "Local"
   * rather than "Your server". */
  loopback: boolean;
}

/**
 * Check a person's own OpenAI-compatible server: what it offers, and
 * whether it is this machine. A network call, so this can take a moment or
 * fail with the reason.
 */
export function serverModels(address: string): Promise<ServerModelsView> {
  return invoke("server_models", { address });
}

/** Mirrors `src-tauri::commands::ReasoningClassView`. */
export type ReasoningClassView = "no-reasoning" | "can-turn-off" | "minimal";

/** Mirrors `src-tauri::commands::OpenRouterCleanupOfferView`. */
export interface OpenRouterCleanupOfferView {
  id: string;
  name: string;
  reasoning: ReasoningClassView;
  measured: MeasuredCleanupView | null;
}

/** Mirrors `src-tauri::commands::MeasuredTranscriptionView`. */
export interface MeasuredTranscriptionView {
  /** Words wrong, in everyday/technical/clinical order. */
  words_wrong: [number, number, number];
  /** Each clip's own word count, in the same order (80/30/64). */
  clip_words: [number, number, number];
}

/** Mirrors `src-tauri::commands::OpenRouterTranscriptionOfferView`. */
export interface OpenRouterTranscriptionOfferView {
  id: string;
  name: string;
  measured: MeasuredTranscriptionView | null;
  also_runs_locally: boolean;
}

/**
 * Mirrors `src-tauri::commands::OpenRouterModelsView`: which kind was asked
 * for, its offers, when the underlying listing was fetched, and (when the
 * fetch failed and this is the last good cached copy instead) why.
 */
export type OpenRouterModelsView =
  | {
      kind: "text";
      offers: OpenRouterCleanupOfferView[];
      fetched_at_ms: number;
      stale_reason: string | null;
    }
  | {
      kind: "transcription";
      offers: OpenRouterTranscriptionOfferView[];
      fetched_at_ms: number;
      stale_reason: string | null;
    };

/**
 * List OpenRouter's models of one kind ("text" for cleanup, "transcription"
 * for STT), from the on-disk cache or fresh from OpenRouter.
 * `refresh: true` always fetches -- on Refresh and when a key is saved;
 * `false` fetches only when there is no cache or it is a week old or more --
 * when the page opens.
 */
export function openrouterModels(
  kind: "text" | "transcription",
  refresh: boolean,
): Promise<OpenRouterModelsView> {
  return invoke("openrouter_models", { kind, refresh });
}

/** Mirrors `src-tauri::commands::PoolOfferView`. */
export interface PoolOfferView {
  id: string;
  name: string;
  input_token_limit: number | null;
}

/** Mirrors `src-tauri::commands::PoolModelsView`. */
export interface PoolModelsView {
  provider: string;
  offers: PoolOfferView[];
  fetched_at_ms: number;
  stale_reason: string | null;
}

/**
 * List the models one provider offers a cleanup pool, with its saved key,
 * from the on-disk cache or fresh from the provider. `refresh` as for
 * `openrouterModels`.
 */
export function poolModels(provider: string, refresh: boolean): Promise<PoolModelsView> {
  return invoke("pool_models", { provider, refresh });
}

/** Mirrors `src-tauri::files::TurnView`. */
export interface MeetingTurn {
  /** The speaker as data, what Copy and Save as text give. */
  speaker: string | null;
  /** The speaker as the page shows them, worded in Rust. */
  label: string | null;
  start_ms: number;
  end_ms: number;
  text: string;
  /** The turn's Edit button as a screen reader names it, worded in Rust. */
  edit_label: string;
}

/** Mirrors `src-tauri::files::TidiedView`: what tidying did. */
export interface MeetingTidied {
  destination: string;
  parts: number;
  /** Parts whose words came back as they were heard. */
  unchanged: number;
}

/**
 * Mirrors `src-tauri::files::ItemView`: a summary item, its quote, and where
 * the quote was said.
 */
export interface MeetingItem {
  text: string;
  quote: string;
  owner: string | null;
  /** The turn the quote was said in, by its index in the meeting's turns. */
  turn: number;
  /** Who said it and when, worded in Rust (`meeting_words.rs`). */
  said: string;
}

/** Mirrors `src-tauri::files::PartView`: one part of a summary. */
export interface MeetingPart {
  start_ms: number;
  end_ms: number;
  key_points: MeetingItem[];
  decisions: MeetingItem[];
  action_items: MeetingItem[];
  /** Reply lines the quote check left out. */
  dropped: number;
  error: string | null;
}

/**
 * Mirrors `src-tauri::files::MeetingView`, on `meeting-transcript` and
 * `meeting-summary`. Kept in memory only, until cleared or the app quits.
 */
export interface MeetingView {
  job: number;
  /**
   * Raised each time this meeting changes after it is made. Of two views of
   * one job, the page keeps the one with the higher revision (`newer`).
   */
  revision: number;
  name: string;
  turns: MeetingTurn[];
  named_speakers: boolean;
  /** `null` for a transcript file, which has no audio. */
  audio_ms: number | null;
  tidied: MeetingTidied | null;
  summary: MeetingPart[] | null;
  /** Where the summary's transcript went, in cleanup's own words. */
  summary_by: string | null;
  /**
   * The transcript was corrected after the summary was written, so a quote
   * may no longer match its turn. The page says so above the summary.
   */
  summary_out_of_date: boolean;
  /** Why a live meeting stopped early and kept what it heard, a sentence. */
  ended_early: string | null;
  /** Why this meeting is here at launch: held from a quit, a sentence. */
  held: string | null;
}

/**
 * Mirrors `src-tauri::files::ProgressEvent`, on `meeting-progress`: `hearing`
 * counts milliseconds of audio, `tidying` counts parts.
 */
export interface MeetingProgress {
  job: number;
  phase: "hearing" | "tidying";
  done: number;
  of: number;
}

/**
 * Mirrors `src-tauri::files::SummaryProgressEvent`, on `summary-progress`: part
 * `part` of `of` has `started` or `finished`. `job` is the meeting's, as on
 * `meeting-summary`.
 */
export interface SummaryProgress {
  job: number;
  stage: "started" | "finished";
  part: number;
  of: number;
}

/**
 * Mirrors `src-tauri::files::RunningSummaryView`: a summary still running, for
 * a page that opens while it works.
 */
export interface RunningSummary {
  /** The summary's own job, as `meetingSummarise` returned it. */
  job: number;
  elapsed_ms: number;
  /** Whether Stop was pressed. */
  stopping: boolean;
  /** Where it sends the transcript once its summariser is built: what is running, whatever the settings say now. */
  destination: string | null;
  /** How long a part can take, when that was measured for its provider. */
  pace: string | null;
  /** The last `summary-progress` sent, once a part has started. */
  progress: SummaryProgress | null;
}

/** Mirrors `src-tauri::files::ErrorEvent`, on `meeting-error`. */
export interface MeetingError {
  job: number;
  message: string;
}

/** Mirrors `src-tauri::files::DestinationsView`: where the words would go. */
export interface MeetingDestinations {
  /** The engine's name, as the Engine card shows it; `null` with no session. */
  engine: string | null;
  tidy: string | null;
  /** The cleanup choice behind `tidy`, to pass back when opening a file. */
  tidy_choice: string | null;
  /** Why tidying is not offered with this provider, shown in place of the tick. */
  tidy_unavailable: string | null;
  summary: string | null;
  /** The cleanup choice behind `summary`, to pass back to Summarise. */
  summary_choice: string | null;
  /** How long a part of a summary can take, said while it runs; `null` when not measured. */
  summary_pace: string | null;
}

/**
 * Hear or read a file; returns the job, results arrive as events. Tidied when
 * `tidyWith` is the `tidy_choice` the page showed, and refused with nothing
 * sent if the cleanup choice has changed since.
 */
export function meetingOpenFile(path: string, tidyWith: string | null): Promise<number> {
  return invoke("meeting_open_file", { path, tidyWith });
}

/**
 * Summarise the meeting kept in memory, only while the cleanup choice is still
 * `choice`, the `summary_choice` the page showed; the result arrives as
 * `meeting-summary`.
 */
export function meetingSummarise(choice: string): Promise<number> {
  return invoke("meeting_summarise", { choice });
}

/**
 * The system's open panel, for one file or several; their paths, or none if
 * the person cancelled. Rust opens the panel and records the chosen paths, so
 * they are ones `meeting_open_file` or `meeting_batch_start` will accept.
 */
export function meetingChooseFile(): Promise<string[]> {
  return invoke("meeting_choose_file");
}

/** Stop the job running. */
export function meetingCancel(): Promise<void> {
  return invoke("meeting_cancel");
}

/** The meeting kept in memory, if any. */
export function meetingCurrent(): Promise<MeetingView | null> {
  return invoke("meeting_current");
}

/** The summary running, if any, for the page to show again when it opens. */
export function meetingRunningSummary(): Promise<RunningSummary | null> {
  return invoke("meeting_running_summary");
}

/** Forget the meeting kept in memory. */
export function meetingClear(): Promise<void> {
  return invoke("meeting_clear");
}

/** Where hearing, tidying and a summary would send the words. */
export function meetingDestinations(): Promise<MeetingDestinations> {
  return invoke("meeting_destinations");
}

/** Put the meeting's text on the clipboard. */
export function meetingCopy(): Promise<void> {
  return invoke("meeting_copy");
}

/**
 * Correct turn `index` of meeting `job` by hand; returns the meeting as kept.
 * The turn keeps its time and speaker. Refused, with a sentence, for words
 * that are all whitespace, a meeting no longer on the page, or a turn it does
 * not have.
 */
export function meetingEditTurn(job: number, index: number, text: string): Promise<MeetingView> {
  return invoke("meeting_edit_turn", { job, index, text });
}

/** Mirrors `src-tauri::macalla_core::dictionary::Offer`: what a hand correction offers to teach the dictionary. */
export interface CorrectionOffer {
  heard: string;
  written: string;
}

/**
 * Whether the hand correction just saved for one turn looks like a single
 * mishearing worth offering to the dictionary, and if so, what to offer.
 * `old` is the turn's text as it was before the correction, held by the
 * page from before Save; `updated` is what `meetingEditTurn` returned for
 * that turn.
 */
export function meetingCorrectionOffer(old: string, updated: string): Promise<CorrectionOffer | null> {
  return invoke("meeting_correction_offer", { old, new: updated });
}

/**
 * Save the meeting to Downloads as `format` (a `SaveFormat.extension`);
 * returns where it went.
 */
export function meetingSave(format: string): Promise<string> {
  return invoke("meeting_save", { format });
}

/**
 * Mirrors `src-tauri::files::LiveProgressEvent`, on `live-meeting-progress`,
 * about once a second while a live meeting records. Every string is a
 * sentence worded in Rust (`meeting_words.rs`).
 */
export interface LiveProgress {
  job: number;
  heard_ms: number;
  heard: string;
  you: string;
  /** The call's line; `null` in a microphone-only meeting. */
  others: string | null;
  /** Whether `others` is the no-sound-from-the-call notice. */
  others_notice: boolean;
  /** Why the meeting hears the microphone alone, when it does. */
  microphone_only: string | null;
}

/** Mirrors `src-tauri::files::RunningLiveView`: a live meeting still running. */
export interface RunningLive {
  job: number;
  stopping: boolean;
  progress: LiveProgress | null;
}

/**
 * Mirrors `src-tauri::files::LiveWordsView`: the live meeting card's words,
 * a summary's three list headings, and saving's own words.
 */
export interface LiveWords {
  heading: string;
  about: string;
  start: string;
  stop: string;
  discard: string;
  starting: string;
  stopping: string;
  /** Above a summary: what each quote promises. */
  summary_quotes: string;
  key_points: string;
  decisions: string;
  action_items: string;
  /**
   * The label beside the Files card's one choice of format, which the
   * current meeting's Save button and a batch both save in.
   */
  save_transcripts_as: string;
  /** The current meeting's Save button. */
  save: string;
  /** Below the current meeting, once it has a summary: which format keeps it too. */
  summary_gone_unless_markdown: string;
  /** Above a summary written before the transcript was corrected. */
  summary_out_of_date: string;
  /** Find in the transcript, and correcting a turn. */
  transcript: TranscriptWords;
  /** The batch card's heading, sentence and buttons. */
  batch_heading: string;
  batch_about: string;
  batch_stop: string;
  batch_clear: string;
  /** The formats a transcript can be saved in, the first the default. */
  formats: SaveFormat[];
}

/**
 * Mirrors `src-tauri::files::TranscriptWordsView`, worded in Rust
 * (`meeting_words.rs`). `count` carries the named placeholders `{current}`
 * and `{total}`, for `fill`. Each turn's Edit label comes with the turn.
 *
 * `correction_offer`, `add_to_dictionary`, `added` and `updated` are the
 * dictionary's own catalogue words (`dictionary_words.rs`), the same ones
 * History's own "Add to dictionary" reads through `dictionaryWords`.
 */
export interface TranscriptWords {
  find: string;
  count: string;
  none: string;
  previous: string;
  next: string;
  edit: string;
  field: string;
  save: string;
  cancel: string;
  /** The offer under a turn just corrected by hand. `{heard}`, `{written}`. */
  correction_offer: string;
  add_to_dictionary: string;
  /** After using the offer's button. `{written}`. */
  added: string;
  /** After using the offer's button. `{written}`. */
  updated: string;
}

/** Mirrors `src-tauri::files::FormatView`. */
export interface SaveFormat {
  /** Passed back to `meetingSave` and `meetingBatchStart`. */
  extension: string;
  label: string;
}

/** Mirrors `src-tauri::files::BatchRowView`: one file of a batch. */
export interface BatchRow {
  name: string;
  state: "waiting" | "hearing" | "done" | "failed" | "stopped";
  /** Where it has got, where it was saved, or why not, worded in Rust. */
  status: string;
  /** While heard: milliseconds of audio heard, of how many. */
  done: number;
  of: number;
  saved: string | null;
}

/**
 * Mirrors `src-tauri::files::BatchView`: several files heard in turn, each
 * saved to Downloads as it finishes. Also on `batch-ended`.
 */
export interface BatchView {
  job: number;
  running: boolean;
  stopping: boolean;
  /** How far it has got, or how it ended, worded in Rust. */
  summary: string;
  rows: BatchRow[];
}

/** Mirrors `src-tauri::files::BatchChangedEvent`, on `batch-changed`. */
export interface BatchChanged {
  index: number;
  batch: BatchView;
}

/**
 * Hear, or read, each of `paths` in turn and save each transcript to
 * Downloads as `format` (a `SaveFormat.extension`). Returns the batch, every
 * file waiting; changes arrive as `batch-changed` and the end as
 * `batch-ended`. Stop is `meetingCancel`.
 */
export function meetingBatchStart(paths: string[], format: string): Promise<BatchView> {
  return invoke("meeting_batch_start", { paths, format });
}

/** The last batch, running or ended, for the page to show again when it opens. */
export function meetingBatch(): Promise<BatchView | null> {
  return invoke("meeting_batch");
}

/** Take an ended batch's list off the page; its saved files stay. */
export function meetingBatchClear(): Promise<void> {
  return invoke("meeting_batch_clear");
}

/**
 * Start a live meeting; returns the job. Progress arrives as
 * `live-meeting-progress`, the meeting as `meeting-transcript`, and a discard
 * or a failure as `meeting-error`. Refused, with a sentence, while a
 * dictation or another job runs.
 */
export function meetingLiveStart(): Promise<number> {
  return invoke("meeting_live_start");
}

/** End the live meeting and keep what it heard. Discarding is `meetingCancel`. */
export function meetingLiveStop(): Promise<void> {
  return invoke("meeting_live_stop");
}

/** The live meeting running, if any, for the page to show again when it opens. */
export function meetingRunningLive(): Promise<RunningLive | null> {
  return invoke("meeting_running_live");
}

/** The live meeting card's words. */
export function meetingLiveWords(): Promise<LiveWords> {
  return invoke("meeting_live_words");
}

/**
 * Mirrors `src-tauri::files::translation::Said`: a catalogue sentence from
 * `translate_words.rs`, with the languages its `{source}` and `{target}` name,
 * as BCP-47 codes. The page names them (`languageName`) and fills the
 * sentence; a sentence with neither ignores them.
 */
export interface TranslationSaid {
  text: string;
  source: string;
  target: string;
}

/** Mirrors `src-tauri::files::translation::TranslationWordsView`. */
export interface TranslationWords {
  heading: string;
  about: string;
  into: string;
  translate: string;
  stop: string;
  /** `{current}` and `{total}`. */
  progress: string;
  stopping: string;
  /** `{source}` and `{target}`. */
  made: string;
  out_of_date: string;
  save: string;
  /** `{path}`. */
  saved: string;
  copy: string;
  copied: string;
}

/** Mirrors `src-tauri::files::translation::TranslationOptionsView`. */
export interface TranslationOptions {
  /** The language a translation translates from, a BCP-47 code. */
  source: string;
  /** The languages the translator lists, as codes; empty with no translator. */
  languages: string[];
  /** Why there is no translator at all, when there is not. */
  unavailable: TranslationSaid | null;
  words: TranslationWords;
}

/** Mirrors `src-tauri::files::translation::TranslationView`: the translated copy. */
export interface TranslationView {
  /** The meeting it translates, as `MeetingView.job`. */
  job: number;
  source: string;
  target: string;
  meeting: MeetingView;
  /** A turn of the original was corrected after this was translated. */
  out_of_date: boolean;
}

/** Mirrors `src-tauri::files::translation::TranslationProgressEvent`, on `translation-progress`. */
export interface TranslationProgress {
  /** The translation's own job, as `meetingTranslate` returned it. */
  job: number;
  done: number;
  of: number;
}

/** Mirrors `src-tauri::files::translation::TranslationFailedEvent`, on `translation-error`. */
export interface TranslationFailed {
  job: number;
  said: TranslationSaid;
}

/** Mirrors `src-tauri::files::translation::RunningTranslationView`. */
export interface RunningTranslation {
  job: number;
  target: string;
  stopping: boolean;
  progress: TranslationProgress | null;
}

/** What the page needs to offer a translation of the current meeting. */
export function meetingTranslationOptions(): Promise<TranslationOptions> {
  return invoke("meeting_translation_options");
}

/** Whether the current meeting can be translated into `target`: `null` when it can. */
export function meetingTranslationCheck(target: string): Promise<TranslationSaid | null> {
  return invoke("meeting_translation_check", { target });
}

/**
 * Translate the current meeting into `target`; returns the job. The copy
 * arrives as `meeting-translation`, a failure or a stop as
 * `translation-error`, progress as `translation-progress`. Stopped by
 * `meetingCancel`.
 */
export function meetingTranslate(target: string): Promise<number> {
  return invoke("meeting_translate", { target });
}

/** The translated copy kept beside the current meeting, if any. */
export function meetingTranslation(): Promise<TranslationView | null> {
  return invoke("meeting_translation");
}

/** The translation running, if any, for the page to show again when it opens. */
export function meetingRunningTranslation(): Promise<RunningTranslation | null> {
  return invoke("meeting_running_translation");
}

/** Put the translated copy's plain text on the clipboard. */
export function meetingCopyTranslation(): Promise<void> {
  return invoke("meeting_copy_translation");
}

/** Save the translated copy to Downloads as `format`; returns where it went. */
export function meetingSaveTranslation(format: string): Promise<string> {
  return invoke("meeting_save_translation", { format });
}
