// SPDX-License-Identifier: MIT
//
// Dev-only fixture data for the browser preview (see ./README.md). Every
// export is typed with `satisfies` against the interfaces in `../commands.ts`
// so a change to a payload's shape fails `tsc`, not a silent runtime
// mismatch. Nothing here is real: no real path, key, or person's name.
// Model names and sizes are shaped like the real catalogue
// (crates/macalla-engines/src/models.rs) but the licence wording and blurbs
// are written fresh for this fixture, not copied from it.

import type {
  BatchView,
  CleanupChoiceView,
  CleanupOptionView,
  DictionaryInfo,
  DictionaryWords,
  HistoryItem,
  LiveWords,
  MeetingDestinations,
  MeetingView,
  MicrophoneView,
  ModelsOverview,
  ModelView,
  PoolProviderView,
  ProviderKeysView,
  RetentionView,
  RunningLive,
  RunningSummary,
  StepSummary,
  TermView,
  TranslationOptions,
  TranslationSaid,
  TranslationView,
  TranslationWords,
  TranscriptionOptionView,
} from "../commands";

const FIXTURE_MODELS_DIR = "~/Library/Application Support/dev.macalla.desktop/models";
const FIXTURE_DICTIONARY_PATH =
  "~/Library/Application Support/dev.macalla.desktop/dictionary.json";

/** The onboarding checklist: everything granted, so the settings page shows
 * "You're all set" and the Try It box instead of a checklist mid-way through.
 * The flags are macOS's own answer (`commands.rs`'s `summary_of`). */
export const FIXTURE_SETUP_STEPS = [
  {
    id: "microphone",
    title: "Microphone",
    detail: "To hear you.",
    state: "done",
    action: null,
    permission: true,
    ask: true,
    settings: true,
  },
  {
    id: "input-monitoring",
    title: "Input Monitoring",
    detail: "To notice when you hold the shortcut.",
    state: "done",
    action: null,
    permission: true,
    ask: false,
    settings: true,
  },
  {
    id: "accessibility",
    title: "Accessibility",
    detail: "To paste into other apps.",
    state: "done",
    action: null,
    permission: true,
    ask: true,
    settings: true,
  },
  {
    id: "secure-input",
    title: "Secure Keyboard Entry",
    detail: "Off, so your shortcut can see the keys.",
    state: "done",
    action: null,
    permission: false,
    ask: false,
    settings: false,
  },
  {
    id: "engine",
    title: "Engine",
    detail: "Apple Speech is built in and ready.",
    state: "done",
    action: null,
    permission: false,
    ask: false,
    settings: false,
  },
] satisfies StepSummary[];

/** macOS mid-way through: the microphone never asked, Input Monitoring
 * refused, Accessibility granted. The copy is `macalla-platform`'s
 * `onboarding.rs`, with its pane names filled in. */
export const FIXTURE_SETUP_STEPS_MACOS_PENDING = [
  {
    id: "microphone",
    title: "Microphone",
    detail: "To hear you.",
    state: "required",
    action: "Press Ask and allow it. Macalla only listens while you hold the shortcut.",
    permission: true,
    ask: true,
    settings: true,
  },
  {
    id: "input-monitoring",
    title: "Input Monitoring",
    detail: "To notice when you hold the shortcut.",
    state: "required",
    action:
      "Turn it on in System Settings > Privacy & Security > Input Monitoring. macOS will not ask again on its own.",
    permission: true,
    ask: false,
    settings: true,
  },
  {
    id: "accessibility",
    title: "Accessibility",
    detail: "To paste into other apps.",
    state: "done",
    action: null,
    permission: true,
    ask: true,
    settings: true,
  },
  {
    id: "secure-input",
    title: "Secure Keyboard Entry",
    detail: "Off, so your shortcut can see the keys.",
    state: "done",
    action: null,
    permission: false,
    ask: false,
    settings: false,
  },
  {
    id: "engine",
    title: "Engine",
    detail: "Apple Speech is built in and ready.",
    state: "done",
    action: null,
    permission: false,
    ask: false,
    settings: false,
  },
] satisfies StepSummary[];

/** Windows or Linux on a first run: no permission steps at all
 * (`Permission::required_on`), and Parakeet, the default engine there, not
 * downloaded yet (`commands.rs`'s `engine_step`). */
export const FIXTURE_SETUP_STEPS_ELSEWHERE_PENDING = [
  {
    id: "engine",
    title: "Engine",
    detail: "Parakeet turns your voice into text.",
    state: "required",
    action: "needs a download: Parakeet TDT 0.6B v3, 739 MB",
    permission: false,
    ask: false,
    settings: false,
  },
] satisfies StepSummary[];

/** Windows or Linux once Parakeet is downloaded: the engine alone, done. */
export const FIXTURE_SETUP_STEPS_ELSEWHERE_DONE = [
  {
    id: "engine",
    title: "Engine",
    detail: "Parakeet is ready.",
    state: "done",
    action: null,
    permission: false,
    ask: false,
    settings: false,
  },
] satisfies StepSummary[];

/** "Right Option" is the default hold-to-talk key on macOS. */
export const FIXTURE_SHORTCUT = "Right Option";

export const FIXTURE_DICTIONARY_INFO = {
  path: FIXTURE_DICTIONARY_PATH,
  terms: 10,
} satisfies DictionaryInfo;

/** The dictionary's own catalogue words, as `dictionary_words.rs` has them:
 * copied here for the preview only, which has no Rust to ask. */
export const FIXTURE_DICTIONARY_WORDS = {
  add_to_dictionary: "Add to dictionary",
  added: "Added {written} to the dictionary.",
  updated: "Updated {written} in the dictionary.",
} satisfies DictionaryWords;

/** Ten invented terms for the Dictionary page: a mix of tech and medical
 * words, each with the spoken forms that trip an engine up. The last has no
 * spoken forms, to exercise the "Heard as itself, near enough" line. */
export const FIXTURE_DICTIONARY_TERMS = [
  { written: "PostgreSQL", spoken: ["postgres", "post gres queue el"] },
  { written: "Kubernetes", spoken: ["koo-ber-net-eez", "k eight s"] },
  { written: "GraphQL", spoken: ["graphical", "graph queue el"] },
  { written: "OAuth", spoken: ["oh auth", "o auth"] },
  { written: "webhook", spoken: ["web hook"] },
  { written: "Nginx", spoken: ["engine x", "en jinx"] },
  { written: "tachycardia", spoken: ["tacky cardia"] },
  { written: "amoxicillin", spoken: ["a moxy silin"] },
  { written: "ibuprofen", spoken: ["eye buprofen"] },
  { written: "arrhythmia", spoken: [] },
] satisfies TermView[];

export const FIXTURE_HISTORY_RETENTION = {
  current: "30d",
  choices: [
    { value: "off", label: "Not at all" },
    { value: "until-quit", label: "Until Macalla quits" },
    { value: "1d", label: "1 day" },
    { value: "7d", label: "7 days" },
    { value: "30d", label: "30 days" },
  ],
} satisfies RetentionView;

export const FIXTURE_INPUT_DEVICE = "MacBook Pro Microphone";

export const FIXTURE_LAUNCH_AT_LOGIN = {
  heading: "General",
  about: "How Macalla starts.",
  label: "Open Macalla when you log in",
  on: false,
};

export const FIXTURE_MICROPHONES = {
  names: ["MacBook Pro Microphone", "External USB Mic"],
  chosen: null,
  rows: [
    { value: "", label: "System default (MacBook Pro Microphone)" },
    { value: "MacBook Pro Microphone", label: "MacBook Pro Microphone" },
    { value: "External USB Mic", label: "External USB Mic" },
  ],
} satisfies MicrophoneView;

/** A handful of invented past dictations, newest first, spanning what the
 * History page's own rendering branches on: plain ones, ones Enhancement
 * visibly cleaned up (raw differs from text), and the newest, whose cloud
 * request failed, so it holds the text as the on-device engine heard it and
 * offers Send again. Only the held row, which is always the newest, can. */
export const FIXTURE_HISTORY: HistoryItem[] = [
  {
    id: 5,
    ts_ms: Date.UTC(2026, 8, 16, 9, 14, 0),
    text: "Send the invoice to the client before Friday.",
    raw: "Send the invoice to the client before Friday.",
    send_again: true,
  },
  {
    id: 4,
    ts_ms: Date.UTC(2026, 8, 16, 8, 2, 0),
    text: "Book a table for four at seven on Thursday.",
    raw: "book a table for four at seven on thursday",
    send_again: false,
  },
  {
    id: 3,
    ts_ms: Date.UTC(2026, 8, 15, 17, 42, 0),
    text: "Ship the release candidate on Tuesday, then check the changelog and message the team.",
    raw: "ship release candidate tuesday check changelog message team",
    send_again: false,
  },
  {
    id: 2,
    ts_ms: Date.UTC(2026, 8, 15, 13, 20, 0),
    text: "Draft the quarterly summary for the board and circulate it Monday.",
    raw: "Draft the quarterly summary for the board and circulate it Monday.",
    send_again: false,
  },
  {
    id: 1,
    ts_ms: Date.UTC(2026, 8, 15, 8, 3, 0),
    text: "Call Aoife back about the weekend plans.",
    raw: "call eefa back about the weekend plans",
    send_again: false,
  },
];

/** Every provider this build knows about. Only OpenAI has a saved key, so
 * the cloud rows under Transcription render for it and the rest collapse
 * into the "also transcribe" providers note. */
export const FIXTURE_PROVIDER_KEYS = {
  providers: [
    { id: "openai", name: "OpenAI", kinds: ["transcription", "text"], saved: true, error: null },
    { id: "anthropic", name: "Anthropic", kinds: ["text"], saved: false, error: null },
    { id: "gemini", name: "Gemini", kinds: ["transcription", "text"], saved: false, error: null },
    { id: "mistral", name: "Mistral", kinds: ["transcription", "text"], saved: false, error: null },
    { id: "ollama-cloud", name: "Ollama Cloud", kinds: ["text"], saved: false, error: null },
    {
      id: "openrouter",
      name: "OpenRouter",
      kinds: ["transcription", "text"],
      saved: false,
      error: null,
    },
    { id: "elevenlabs", name: "ElevenLabs", kinds: ["transcription"], saved: false, error: null },
    { id: "cohere", name: "Cohere", kinds: ["transcription"], saved: false, error: null },
  ],
  store_unavailable: null,
} satisfies ProviderKeysView;

const FIXTURE_TRANSCRIPTION_OPTIONS = [
  {
    source: "local",
    id: "apple-speech",
    name: "Apple Speech",
    group: "Apple",
    blurb: "Built into macOS. Nothing to download, and shows words as you speak.",
    status: "ready",
    detail: null,
    chosen: true,
    model: null,
    measured_words_wrong: [3, 2, 4],
  },
  {
    source: "local",
    id: "parakeet-tdt-v3",
    name: "Parakeet",
    group: "NVIDIA",
    blurb: "Strong with technical terms across two dozen European languages.",
    status: "ready",
    detail: null,
    chosen: false,
    model: "parakeet-tdt-0.6b-v3",
    measured_words_wrong: [4, 3, 5],
  },
  {
    source: "local",
    id: "nemotron-3.5-streaming",
    name: "Nemotron",
    group: "NVIDIA",
    blurb: "Streams text as you speak, about a second behind.",
    status: "needs-download",
    detail: "A 751 MB download.",
    chosen: false,
    model: "nemotron-3.5-asr-streaming-0.6b",
    measured_words_wrong: null,
  },
  {
    source: "provider",
    id: "openai",
    name: "OpenAI",
    group: "",
    blurb: "",
    status: "ready",
    detail: null,
    chosen: false,
    model: null,
    measured_words_wrong: [2, 1, 3],
  },
  {
    source: "provider",
    id: "gemini",
    name: "Gemini",
    group: "",
    blurb: "",
    status: "needs-key",
    detail: "Gemini needs a key.",
    chosen: false,
    model: null,
    measured_words_wrong: null,
  },
  {
    source: "provider",
    id: "mistral",
    name: "Mistral",
    group: "",
    blurb: "",
    status: "needs-key",
    detail: "Mistral needs a key.",
    chosen: false,
    model: null,
    measured_words_wrong: null,
  },
  {
    source: "provider",
    id: "elevenlabs",
    name: "ElevenLabs",
    group: "",
    blurb: "",
    status: "needs-key",
    detail: "ElevenLabs needs a key.",
    chosen: false,
    model: null,
    measured_words_wrong: null,
  },
  {
    source: "provider",
    id: "cohere",
    name: "Cohere",
    group: "",
    blurb: "",
    status: "needs-key",
    detail: "Cohere needs a key.",
    chosen: false,
    model: null,
    measured_words_wrong: null,
  },
  {
    source: "provider",
    id: "openrouter",
    name: "OpenRouter",
    group: "",
    blurb: "",
    status: "needs-key",
    detail: "OpenRouter needs a key.",
    chosen: false,
    model: null,
    measured_words_wrong: null,
  },
] satisfies TranscriptionOptionView[];

const FIXTURE_CLEANUP_OPTIONS = [
  { kind: "off", id: "off", name: "Off", available: true, reason: null, key: null },
  {
    kind: "apple-intelligence",
    id: "apple-intelligence",
    name: "Apple Intelligence",
    available: true,
    reason: null,
    key: null,
  },
  {
    kind: "local-server",
    id: "server",
    name: "Ollama or another server",
    available: true,
    reason: null,
    key: null,
  },
  {
    kind: "provider",
    id: "openai",
    name: "OpenAI",
    available: true,
    reason: null,
    key: "saved",
  },
  {
    kind: "provider",
    id: "anthropic",
    name: "Anthropic",
    available: false,
    reason: "Anthropic needs a key.",
    key: "missing",
  },
  {
    kind: "provider",
    id: "ollama-cloud",
    name: "Ollama Cloud",
    available: false,
    reason: "Ollama Cloud needs a key.",
    key: "missing",
  },
] satisfies CleanupOptionView[];

const FIXTURE_POOL_PROVIDERS = [{ id: "openai", name: "OpenAI" }] satisfies PoolProviderView[];

const FIXTURE_CLEANUP_CHOSEN = { kind: "apple-intelligence" } satisfies CleanupChoiceView;

/** Everything the Models page (and the Engine card, from the same call) need
 * in one shot. Opened exactly as chosen: Apple Speech transcribes, Apple
 * Intelligence tidies, no fallback in play. */
export const FIXTURE_MODELS_OVERVIEW = {
  transcription: FIXTURE_TRANSCRIPTION_OPTIONS,
  cleanup: FIXTURE_CLEANUP_OPTIONS,
  keys: FIXTURE_PROVIDER_KEYS,
  cloud_transcription_compiled_in: true,
  preview_engine_id: "apple-speech",
  cleanup_chosen: FIXTURE_CLEANUP_CHOSEN,
  pool_providers: FIXTURE_POOL_PROVIDERS,
  in_use: {
    transcription: "Apple Speech",
    enhancement: "Apple Intelligence",
    enhancement_issue: null,
    transcription_reason: null,
    enhancement_reason: null,
  },
} satisfies ModelsOverview;

/** Two local models: one installed and in use as a spare, one still to
 * download. Sizes and ids are shaped like the real catalogue in
 * crates/macalla-engines/src/models.rs; the licence text here is fixture
 * wording, not a copy of that file's attribution strings. */
export const FIXTURE_MODELS_LIST = [
  {
    id: "parakeet-tdt-0.6b-v3",
    name: "Parakeet TDT 0.6B v3",
    megabytes: 739,
    licence: "CC-BY-4.0",
    attribution: "Derived from a third-party GGUF conversion of NVIDIA's Parakeet TDT checkpoint.",
    state: "installed",
    bytes: 739_508_576,
  },
  {
    id: "nemotron-3.5-asr-streaming-0.6b",
    name: "Nemotron 3.5 ASR Streaming 0.6B",
    megabytes: 751,
    licence: "NVIDIA Open Model License",
    attribution: "Derived from a third-party GGUF conversion of NVIDIA's Nemotron ASR checkpoint.",
    state: "absent",
    bytes: 0,
  },
] satisfies ModelView[];

export const FIXTURE_MODELS_DIR_PATH = FIXTURE_MODELS_DIR;

/** Where the Files page would send words: hearing by the local engine,
 * summarising by Apple Intelligence. Apple Intelligence cannot tidy a file,
 * so tidying is unavailable, with the app's own sentence for why. */
export const FIXTURE_MEETING_DESTINATIONS = {
  engine: "Apple Speech",
  tidy: "Apple Intelligence",
  tidy_choice: "apple-intelligence",
  tidy_unavailable:
    "Apple Intelligence can't tidy a file yet. To tidy one, choose a server or a cloud model on the Enhancement card.",
  summary: "Apple Intelligence",
  summary_choice: "apple-intelligence",
  summary_pace:
    "On M1 Macs, Apple Intelligence can take 15 to 35 seconds for most parts of a file, and over a minute for a few.",
} satisfies MeetingDestinations;

/** A short invented meeting, already heard and summarised (not tidied:
 * Apple Intelligence cannot tidy a file), so the
 * Files page shows its transcript and summary cards rather than the empty
 * drop prompt. Two invented speakers, no real names. */
export const FIXTURE_MEETING_CURRENT = {
  job: 1,
  revision: 0,
  name: "Team Sync - Q3 Planning.m4a",
  turns: [
    {
      speaker: "Speaker 1",
      label: "Speaker 1",
      start_ms: 0,
      end_ms: 4200,
      text: "Let's start with the roadmap for next quarter.",
      edit_label: "Edit what Speaker 1 said at 0:00",
    },
    {
      speaker: "Speaker 2",
      label: "Speaker 2",
      start_ms: 4300,
      end_ms: 9100,
      text: "I think we should prioritise the dictionary sync work first.",
      edit_label: "Edit what Speaker 2 said at 0:04",
    },
    {
      speaker: "Speaker 1",
      label: "Speaker 1",
      start_ms: 9200,
      end_ms: 15000,
      text: "Agreed. Can you own that and report back on Friday?",
      edit_label: "Edit what Speaker 1 said at 0:09",
    },
  ],
  named_speakers: false,
  audio_ms: 15000,
  tidied: null,
  summary: [
    {
      start_ms: 0,
      end_ms: 15000,
      key_points: [
        {
          text: "Next quarter's roadmap should prioritise the dictionary sync work.",
          quote: "prioritise the dictionary sync work first",
          owner: null,
          turn: 1,
          said: "Speaker 2, 0:04",
        },
      ],
      decisions: [
        {
          text: "Dictionary sync work is the top priority for next quarter.",
          quote: "Agreed.",
          owner: null,
          turn: 2,
          said: "Speaker 1, 0:09",
        },
      ],
      action_items: [
        {
          text: "Report back on dictionary sync progress on Friday.",
          quote: "report back on Friday",
          owner: "Speaker 2",
          turn: 2,
          said: "Speaker 1, 0:09",
        },
      ],
      dropped: 0,
      error: null,
    },
  ],
  summary_by: "Apple Intelligence",
  summary_out_of_date: false,
  ended_early: null,
  held: null,
} satisfies MeetingView;

/** No summary is running when the Files page opens in the preview. */
export const FIXTURE_MEETING_RUNNING_SUMMARY = null satisfies RunningSummary | null;

/** No live meeting is recording when the Files page opens in the preview. */
export const FIXTURE_MEETING_RUNNING_LIVE = null satisfies RunningLive | null;

/** The live meeting card's words, as `meeting_words.rs` has them: copied
 * here for the preview only, which has no Rust to ask. */
export const FIXTURE_MEETING_LIVE_WORDS = {
  heading: "Live meeting",
  about:
    "Turn a call, or a meeting in the room, into text while it happens. The sound is heard on this Mac and not kept.",
  start: "Start a live meeting",
  stop: "Stop and keep",
  discard: "Discard",
  starting: "Starting…",
  stopping: "Stopping…",
  summary_quotes:
    "Each point quotes what was really said, and who said it and when. Whether the quote backs the point is yours to judge.",
  key_points: "Key points",
  decisions: "Decisions",
  action_items: "Action items",
  save_transcripts_as: "Save transcripts as",
  save: "Save",
  summary_gone_unless_markdown:
    "The transcript and its summary are gone when you quit. Saving as Markdown keeps both; the other formats keep the transcript alone.",
  summary_out_of_date:
    "This summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.",
  transcript: {
    find: "Find in the transcript",
    count: "{current} of {total}",
    none: "No matches",
    previous: "Previous",
    next: "Next",
    edit: "Edit",
    field: "The turn's words",
    save: "Save",
    cancel: "Cancel",
    correction_offer: 'Macalla heard "{heard}". Add "{written}" to the dictionary so every engine writes it that way?',
    add_to_dictionary: "Add to dictionary",
    added: "Added {written} to the dictionary.",
    updated: "Updated {written} in the dictionary.",
  },
  batch_heading: "Several files",
  batch_about:
    "Each file is heard in turn on the same speech engine, and its transcript is saved to your Downloads folder as soon as it is done. Tidying and summaries work on one file at a time.",
  batch_stop: "Stop",
  batch_clear: "Clear list",
  formats: [
    { extension: "txt", label: "Text (.txt)" },
    { extension: "vtt", label: "WebVTT subtitles (.vtt)" },
    { extension: "srt", label: "SRT subtitles (.srt)" },
    { extension: "md", label: "Markdown (.md)" },
  ],
} satisfies LiveWords;

/** Why `meeting_edit_turn` refuses a correction, as `meeting_words.rs` has
 * them (`EDIT_EMPTY`, `EDIT_STALE`, `EDIT_NO_SUCH_TURN`): copied here for the
 * preview's stand-in for the command. */
export const FIXTURE_EDIT_REFUSALS = {
  empty: "A turn cannot be left empty. Type what was said, or press Cancel.",
  stale: "That transcript is no longer on the Files page, so the correction was not saved.",
  noSuchTurn: "That turn is not in the transcript, so the correction was not saved.",
};

/** A batch part-way through, worded as `meeting_words.rs` words it: two
 * files saved, one that would not read, one being heard, two waiting. Paths
 * and names invented. */
export const FIXTURE_MEETING_BATCH = {
  job: 2,
  running: true,
  stopping: false,
  summary: "3 of 6 files finished.",
  rows: [
    {
      name: "Standup Monday.m4a",
      state: "done",
      status: "Saved to ~/Downloads/Standup Monday transcript 2026-09-25.txt.",
      done: 0,
      of: 0,
      saved: "~/Downloads/Standup Monday transcript 2026-09-25.txt",
    },
    {
      name: "Design review.vtt",
      state: "done",
      status: "Saved to ~/Downloads/Design review transcript 2026-09-25.txt.",
      done: 0,
      of: 0,
      saved: "~/Downloads/Design review transcript 2026-09-25.txt",
    },
    {
      name: "Voice memo 14.m4a",
      state: "failed",
      status: "This recording could not be read as audio.",
      done: 0,
      of: 0,
      saved: null,
    },
    {
      name: "Customer call.mp4",
      state: "hearing",
      status: "12:40 of 31:05 heard",
      done: 760000,
      of: 1865000,
      saved: null,
    },
    {
      name: "Retro.m4a",
      state: "waiting",
      status: "Waiting",
      done: 0,
      of: 0,
      saved: null,
    },
    {
      name: "Planning.wav",
      state: "waiting",
      status: "Waiting",
      done: 0,
      of: 0,
      saved: null,
    },
  ],
} satisfies BatchView;

/** The translation card's words, as `translate_words.rs` has them: copied
 * here for the preview only, which has no Rust to ask. */
export const FIXTURE_TRANSLATION_WORDS = {
  heading: "Translation",
  about:
    "Make a copy of this transcript in another language, translated on this Mac. The original stays as it is.",
  into: "Translate into",
  translate: "Translate",
  stop: "Stop",
  progress: "Translating turn {current} of {total}…",
  stopping: "Stopping after this turn…",
  made: "Translated from {source} into {target}.",
  out_of_date:
    "The transcript was corrected after this was translated, so the two may not match.",
  save: "Save translation",
  saved: "Saved to {path}.",
  copy: "Copy translation",
  copied: "Copied.",
} satisfies TranslationWords;

/** A few of the languages Apple's translator lists, as codes. */
export const FIXTURE_TRANSLATION_OPTIONS = {
  source: "en-US",
  languages: ["en-US", "en-GB", "de", "fr", "es", "it", "ja", "nl", "pt-BR", "pl"],
  unavailable: null,
  words: FIXTURE_TRANSLATION_WORDS,
} satisfies TranslationOptions;

/** No translator at all, as a Mac says it: older than macOS 26, or a build
 * without it. Windows and Linux say "Translation isn't available on this
 * platform yet." instead. */
export const FIXTURE_TRANSLATION_OPTIONS_UNAVAILABLE = {
  source: "en-US",
  languages: [],
  unavailable: {
    text: "Translating isn't available here. It needs macOS 26 or later.",
    source: "",
    target: "",
  },
  words: FIXTURE_TRANSLATION_WORDS,
} satisfies TranslationOptions;

/** The chosen pair can be translated but is not on this Mac yet. */
export const FIXTURE_TRANSLATION_NOT_INSTALLED = {
  text: "{target} isn't on this Mac yet for translating from {source}. Add it in System Settings > General > Language & Region > Translation Languages, then try again.",
  source: "en-US",
  target: "de",
} satisfies TranslationSaid;

/** The chosen pair cannot be translated at all. */
export const FIXTURE_TRANSLATION_UNSUPPORTED = {
  text: "This Mac can't translate from {source} into {target}.",
  source: "en-US",
  target: "ga",
} satisfies TranslationSaid;

/** The fixture meeting translated into German, turn by turn. Invented
 * German, written for the preview. */
export const FIXTURE_TRANSLATION_COPY = {
  job: 1,
  source: "en-US",
  target: "de",
  out_of_date: false,
  meeting: {
    ...FIXTURE_MEETING_CURRENT,
    name: "Team Sync - Q3 Planning (de).m4a",
    turns: [
      { ...FIXTURE_MEETING_CURRENT.turns[0], text: "Beginnen wir mit der Roadmap für das nächste Quartal." },
      {
        ...FIXTURE_MEETING_CURRENT.turns[1],
        text: "Ich finde, wir sollten zuerst die Synchronisierung des Wörterbuchs angehen.",
      },
      {
        ...FIXTURE_MEETING_CURRENT.turns[2],
        text: "Einverstanden. Kannst du das übernehmen und am Freitag berichten?",
      },
    ],
    summary: null,
    summary_by: null,
  },
} satisfies TranslationView;
