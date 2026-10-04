// SPDX-License-Identifier: MIT
//! `macalla` -- the command-line binary.
//!
//! Same core as the desktop app with a different trigger. This is the test
//! harness, the headless answer, and the base for the agent interface. If a
//! feature only works from the tray, it is in the wrong crate (the core is callable
//! without the hotkey or the UI), so everything below is a thin shell over `macalla_core::pipeline`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use macalla_core::block::Document;
use macalla_core::enhance::Enhancer;
use macalla_core::latency::Latencies;
use macalla_core::stt::Transcript;
use macalla_platform::audio::input_device_names;
use macalla_platform::secrets::{SecretStore, SystemSecrets};
use macalla_session::history::SqliteHistory;
use macalla_session::{
    choice, providers, CloudOutcome, Config, Dictation, EnhancingWith, Progress, Session,
    StopSignal,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod stop_signals;
mod translate;
mod watch;
mod words;

/// The summary command's name, and the other spelling it answers to and
/// lists in `--help`. Kept here so a rename touches one place.
const SUMMARISE_COMMAND: &str = "summarise";
const SUMMARISE_ALIAS: &str = "summarize";

#[derive(Parser)]
#[command(name = "macalla", version, about = "Local-first voice dictation.")]
struct Cli {
    /// Raise the log level and print a latency breakdown per dictation.
    #[arg(long, global = true)]
    debug: bool,

    /// Dictionary JSON. Defaults to macalla-dictionary.json, then the repo seed.
    #[arg(long, global = true)]
    dictionary: Option<PathBuf>,

    /// BCP-47 language tag, for example en-GB. Defaults to en-US.
    #[arg(long, global = true)]
    language: Option<String>,

    /// Engine id from `macalla doctor`. Defaults to the platform's engine.
    #[arg(long, global = true)]
    engine: Option<String>,

    /// Where model files live. Defaults to $MACALLA_MODELS_DIR, then the OS
    /// data directory (~/Library/Application Support/Macalla/models on macOS).
    #[arg(long, global = true)]
    models_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum TriggerMode {
    /// Push to talk.
    Hold,
    /// Press to start, press again to stop.
    Toggle,
}

#[derive(Subcommand)]
enum Command {
    /// Listen on the microphone until Enter or the timeout, then print the text.
    Listen {
        /// Seconds to listen for: 30 unless given. With --hotkey, the cap on
        /// one dictation, the same as the app's unless given.
        #[arg(long)]
        timeout: Option<u64>,
        /// Wait for the global shortcut instead of Enter, and keep waiting
        /// after each dictation. Ctrl-C to stop. macOS needs Input Monitoring.
        #[arg(long)]
        hotkey: bool,
        /// The shortcut, for example "right option" (the default), "option",
        /// "fn" or "ctrl+alt+space". A chord with a letter in it stops working
        /// while any app has Secure Keyboard Entry on; a bare modifier does not.
        #[arg(long, requires = "hotkey")]
        shortcut: Option<String>,
        /// hold = dictate while the key is down. toggle = press to start and
        /// again to stop.
        #[arg(long, value_enum, default_value = "hold", requires = "hotkey")]
        mode: TriggerMode,
        /// Emit JSON rather than text.
        #[arg(long)]
        json: bool,
        /// Input device name, from `macalla devices`.
        #[arg(long)]
        device: Option<String>,
        /// Insert the text where you were typing instead of printing it.
        #[arg(long)]
        paste: bool,
        /// Show volatile results as they arrive.
        #[arg(long)]
        preview: bool,
        /// Record each dictation into this SQLite file, after it is delivered.
        #[arg(long)]
        history: Option<PathBuf>,
        /// Send the engine everything, silence included, instead of gating
        /// it on speech. For comparing --debug timings; the gate is on
        /// otherwise.
        #[arg(long)]
        no_vad: bool,
    },
    /// Clean up text the way dictation does with enhancement on: Tier 0 first,
    /// then a model, then the guard. Without --endpoint or --anthropic, Apple
    /// Intelligence on this Mac. With --endpoint, an OpenAI-compatible server --
    /// Ollama, llama-server, LM Studio, or a cloud provider; with --anthropic,
    /// Anthropic's own API. Either way the text goes to that address and
    /// nowhere else. Takes the text given, or one piece per line on stdin.
    Enhance {
        /// The server, for example http://localhost:11434, or its API root.
        #[arg(long, requires = "model", conflicts_with = "anthropic")]
        endpoint: Option<String>,
        /// Anthropic's Messages API instead of an endpoint. Needs --model.
        /// Without --key-env, reads the stored `anthropic` key.
        #[arg(long, requires = "model")]
        anthropic: bool,
        /// The model as the server or Anthropic names it, for example
        /// qwen2.5:0.5b or claude-haiku-4-5.
        #[arg(long)]
        model: Option<String>,
        /// The environment variable holding an API key, instead of the
        /// keychain. Read from the environment so the key never lands in
        /// shell history.
        #[arg(long, conflicts_with = "key_provider")]
        key_env: Option<String>,
        /// --endpoint only: a provider id from `macalla key list` whose
        /// stored key to send.
        #[arg(long, requires = "endpoint")]
        key_provider: Option<String>,
        /// Endpoint or Anthropic: how much a reasoning model thinks. `least`
        /// asks for none, `server` leaves it to the server. Without it, the
        /// default prompt gets `least`.
        #[arg(long, value_enum)]
        reasoning: Option<ReasoningChoice>,
        /// Apple Intelligence only: ask for a one-field schema first instead
        /// of plain text, the default. For comparing the two.
        #[arg(long, conflicts_with_all = ["endpoint", "anthropic"])]
        guided: bool,
        /// Deprecated, does nothing: plain text is now the default. Kept so
        /// `bench/enhance_ab.sh` still gets plain text from a `macalla` built
        /// before this change.
        #[arg(long, hide = true, conflicts_with_all = ["endpoint", "anthropic", "guided"])]
        unguided: bool,
        /// Ask the model to load first and wait two seconds, as if a dictation
        /// had just started. For measuring what prewarming buys.
        #[arg(long)]
        prewarm: bool,
        /// Leave the dictionary out: no corrections before Tier 0 and no terms
        /// in the prompt. For comparing with and without it.
        #[arg(long)]
        no_dictionary: bool,
        /// Emit one JSON object per piece.
        #[arg(long)]
        json: bool,
        /// The text. Without it, one piece per line on stdin.
        text: Vec<String>,
    },
    /// See and edit the dictionary: what should always come out right, and
    /// how it gets heard. Edits go to --dictionary, or to
    /// macalla-dictionary.json in the working directory.
    Dictionary {
        #[command(subcommand)]
        action: DictionaryAction,
    },
    /// List past dictations from a history file, newest first.
    History {
        /// The SQLite file that `listen --history` wrote.
        #[arg(long)]
        history: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Set how long the file keeps dictations: off, until-quit, 1d, 7d
        /// or 30d. off keeps nothing and erases what was kept. Thirty days
        /// is the most. Stored in the file, so the app and the CLI agree.
        #[arg(long)]
        keep: Option<String>,
    },
    /// Summarise a meeting transcript: key points, decisions and action items,
    /// every one quoting the transcript.
    ///
    /// Takes a WebVTT file, which keeps its speakers, or plain text, which has
    /// none. The transcript goes to one provider: Apple Intelligence on this
    /// Mac when no flag names another, a server with --endpoint, Anthropic
    /// with --anthropic, or a pool of cloud models with --pool. Where it goes
    /// is printed before anything is sent.
    #[command(name = SUMMARISE_COMMAND, visible_alias = SUMMARISE_ALIAS)]
    Summarise {
        /// The transcript file.
        path: PathBuf,
        /// A server, for example http://localhost:11434, or its API root.
        #[arg(long, requires = "model", conflicts_with_all = ["anthropic", "pool"])]
        endpoint: Option<String>,
        /// Anthropic's Messages API. Needs --model. Without --key-env, reads
        /// the stored `anthropic` key.
        #[arg(long, requires = "model", conflicts_with = "pool")]
        anthropic: bool,
        /// The model as the server or Anthropic names it.
        #[arg(long, conflicts_with = "pool")]
        model: Option<String>,
        /// The environment variable holding the API key, instead of the
        /// keychain.
        #[arg(long, conflicts_with_all = ["key_provider", "pool"])]
        key_env: Option<String>,
        /// --endpoint only: a provider id from `macalla key list` whose stored
        /// key to send.
        #[arg(long, requires = "endpoint")]
        key_provider: Option<String>,
        /// A cloud model in a pool, as provider:model, for example
        /// openai:gpt-5.6-luna. Repeat it; the first is asked first and the
        /// next takes over when one fails. Keys come from `macalla key set`.
        #[arg(long, value_name = "PROVIDER:MODEL")]
        pool: Vec<String>,
        /// Print the summary as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Record a meeting from the microphone until Enter, Ctrl-C or SIGTERM,
    /// then print its turns. The microphone only: macOS gives a terminal's
    /// request for the call's sound to the terminal, not to macalla, and it
    /// arrives as silence, so the desktop app is where a call is recorded
    /// whole. An agent stops the meeting and keeps it by sending SIGINT, as
    /// Ctrl-C does; a second Ctrl-C throws it away. On Windows, Ctrl-C ends
    /// the process and keeps nothing. Nothing is kept anywhere but what is
    /// printed.
    LiveMeeting {
        /// Print the turns as JSON.
        #[arg(long)]
        json: bool,
        /// Input device name, from `macalla devices`.
        #[arg(long)]
        device: Option<String>,
    },
    /// Transcribe one audio file, or several.
    ///
    /// wav and aiff need nothing. mp3, m4a, mp4, mov, opus, webm and the
    /// rest use ffmpeg from the PATH; on macOS the system decoder covers the
    /// common ones without it.
    ///
    /// Every input is read as audio. A .vtt or .txt transcript is not read as
    /// a transcript here, as the desktop app's Files page reads one.
    ///
    /// One file is printed. Several need --out-dir: each is transcribed in
    /// turn on the one engine and written there as it finishes, one file per
    /// input, and nothing is overwritten. For each file written, stdout gets
    /// a line "<input>\t<output>"; with --json, stdout gets one JSON object
    /// per input instead: {"input", "output", "error", "no_speech"}, where
    /// "error" is null or one of missing, no_ffmpeg, undecodable, engine or
    /// not_written. A path with a tab or a newline in it makes the
    /// tab-separated lines ambiguous; --json is the unambiguous form. A file
    /// that fails is said on stderr and the rest carry on; the exit status
    /// says whether any failed. A file with no speech in it writes nothing
    /// and is not a failure. Ctrl-C stops at once; files already written are
    /// kept.
    Transcribe {
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
        /// Write each transcript into this folder, as <name>.txt (or .json
        /// with --json, or .md with --format md), instead of printing it.
        /// Needed for several files.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = TranscribeFormat::Txt, help = words::TRANSCRIBE_FORMAT_HELP)]
        format: TranscribeFormat,
        #[arg(long)]
        json: bool,
        /// Print what the engine produced before the dictionary touched it.
        #[arg(long)]
        raw: bool,
        /// Send the file to a cloud provider in one request instead of
        /// transcribing it on this machine. The key comes from `macalla key
        /// set`, or --key-env; the audio goes to that provider and nowhere
        /// else, and nothing is sent without this flag.
        #[arg(long, value_enum)]
        cloud: Option<CloudProvider>,
        /// The provider's model, for example gpt-transcribe or
        /// voxtral-mini-latest. OpenRouter has no default, so it needs one.
        #[arg(long, requires = "cloud")]
        model: Option<String>,
        /// Read the provider's key from this environment variable instead of
        /// the keychain.
        #[arg(long, requires = "cloud")]
        key_env: Option<String>,
    },
    #[command(about = words::WATCH_ABOUT, long_about = words::WATCH_LONG_ABOUT)]
    Watch {
        #[arg(help = words::WATCH_DIR_HELP)]
        dir: PathBuf,
        #[arg(long, help = words::WATCH_OUT_DIR_HELP)]
        out_dir: PathBuf,
        #[arg(long, value_enum, default_value_t = WatchFormat::Txt, help = words::WATCH_FORMAT_HELP)]
        format: WatchFormat,
        #[arg(long, help = words::WATCH_JSON_HELP)]
        json: bool,
    },
    #[command(about = words::TRANSLATE_ABOUT, long_about = words::TRANSLATE_LONG_ABOUT)]
    Translate {
        #[arg(help = words::TRANSLATE_PATH_HELP)]
        path: PathBuf,
        #[arg(long, help = words::TRANSLATE_TO_HELP)]
        to: String,
        #[arg(long, value_enum, default_value_t = WatchFormat::Txt, help = words::TRANSLATE_FORMAT_HELP)]
        format: WatchFormat,
    },
    /// Walk through what is still needed before dictation works.
    Setup {
        /// Ask the OS for anything missing, instead of only reporting it.
        #[arg(long)]
        fix: bool,
    },
    /// The speech models this build can fetch: list, download, remove, import.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Store, remove or list a cloud provider's API key in the OS keychain.
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// List the audio input devices this machine can see.
    Devices,
    /// Report the engines, locales and permissions available on this machine.
    Doctor,
    /// Run the benchmark suites that back docs/benchmarks.md.
    Bench,
}

#[derive(Subcommand, Debug)]
enum DictionaryAction {
    /// Every term, with its spoken forms.
    List,
    /// Add spoken forms to a written form, creating the term if it is new.
    Add {
        /// The form to produce, for example kubectl.
        written: String,
        /// How it gets heard, for example "cube control". Repeatable.
        #[arg(long = "spoken", short = 's')]
        spoken: Vec<String>,
    },
    /// Forget a term.
    Remove { written: String },
    /// Merge another dictionary in: a JSON export, a plain word list (one
    /// term per line), or heard,written pairs as CSV or TSV. Adds and
    /// extends; never removes.
    Import { path: PathBuf },
    /// Print the dictionary as JSON, for sharing.
    Export,
}

#[derive(Subcommand)]
enum KeyAction {
    /// Store a provider's key, read from a pipe or an environment variable.
    /// Never typed with echo: `echo -n "$KEY" | macalla key set openai`.
    Set {
        /// A provider id, from `macalla key list`.
        provider: String,
        /// Read the key from this environment variable instead of stdin.
        #[arg(long)]
        from_env: Option<String>,
    },
    /// Remove a provider's stored key.
    Remove { provider: String },
    /// Every provider: what it offers and whether a key is stored for it.
    /// Never prints the key itself.
    List,
}

#[derive(Subcommand)]
enum ModelsAction {
    /// Every model this build knows, with size, licence and whether it is here.
    List,
    /// Fetch a model into the store. Resumes a partial download; Ctrl-C keeps
    /// the partial file for next time.
    Download {
        /// A model id from `models list`.
        id: String,
        /// Replace the scheme and host of the published URL, for a machine
        /// that cannot reach Hugging Face.
        #[arg(long)]
        mirror: Option<String>,
    },
    /// Delete a model from the store.
    Remove { id: String },
    /// Install a model from a file already on this machine. The checksum is
    /// checked; a wrong file is refused.
    Import { id: String, path: PathBuf },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(dir) = &cli.models_dir {
        // The store reads this once per look; set before anything asks.
        std::env::set_var(macalla_engines::models::DIR_ENV, dir);
    }

    // Logs on stderr, never stdout: stdout is the transcript, the JSON, the
    // device list. `macalla transcribe --engine parakeet-tdt-v3 f > out.txt`
    // wrote "parakeet loaded" into out.txt until this said so.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                if cli.debug {
                    "debug".into()
                } else {
                    "info".into()
                }
            }),
        )
        .init();

    let language = cli.language.clone().unwrap_or_else(|| "en-US".into());

    match &cli.command {
        Command::Setup { fix } => setup(&language, *fix),
        Command::Models { action } => models(action),
        Command::Key { action } => key_command(action),
        Command::Devices => devices(),
        Command::Doctor => doctor(&cli, &language),
        Command::Bench => bench(),
        Command::Transcribe {
            paths,
            out_dir,
            format,
            json,
            raw,
            cloud,
            model,
            key_env,
        } => {
            let cloud = cloud.map(|provider| CloudChoice {
                provider,
                model: model.as_deref(),
                key_env: key_env.as_deref(),
            });
            transcribe(
                &cli,
                &language,
                paths,
                out_dir.as_deref(),
                Rendering {
                    json: *json,
                    raw: *raw,
                    format: *format,
                },
                cloud,
            )
        }
        Command::Watch {
            dir,
            out_dir,
            format,
            json,
        } => watch(&cli, &language, dir, out_dir, *format, *json),
        Command::Translate { path, to, format } => {
            translate::translate(path, &language, to, *format)
        }
        Command::LiveMeeting { json, device } => {
            if let Some(name) = device.as_deref() {
                ensure_device_connected(name)?;
            }
            live_meeting(&cli, &language, device.as_deref(), *json)
        }
        Command::Summarise {
            path,
            endpoint,
            anthropic,
            model,
            key_env,
            key_provider,
            pool,
            json,
        } => {
            let choices = summary_choices(SummaryFlags {
                endpoint: endpoint.as_deref(),
                anthropic: *anthropic,
                model: model.as_deref(),
                key_env: key_env.as_deref(),
                key_provider: key_provider.as_deref(),
                pool,
            })?;
            summarise_file(path, &choices, *json)
        }
        Command::History {
            history,
            limit,
            keep,
        } => list_history(history, *limit, keep.as_deref()),
        Command::Dictionary { action } => dictionary(&cli, action),
        Command::Enhance {
            endpoint,
            anthropic,
            model,
            key_env,
            key_provider,
            reasoning,
            guided,
            unguided: _unguided,
            prewarm,
            no_dictionary,
            json,
            text,
        } => {
            let provider = match (*anthropic, endpoint, model) {
                (true, _, Some(model)) => Provider::Anthropic {
                    model,
                    // Anthropic's key is always the `anthropic` provider's,
                    // stored or overridden with --key-env.
                    key: match key_env {
                        Some(name) => choice::KeySource::Env(name.clone()),
                        None => choice::KeySource::Stored("anthropic".to_string()),
                    },
                    reasoning: *reasoning,
                },
                (false, Some(endpoint), Some(model)) => Provider::Endpoint {
                    endpoint,
                    model,
                    key: match (key_env, key_provider) {
                        (Some(name), _) => choice::KeySource::Env(name.clone()),
                        (None, Some(id)) => choice::KeySource::Stored(id.clone()),
                        (None, None) => choice::KeySource::None,
                    },
                    reasoning: *reasoning,
                },
                _ if model.is_some()
                    || key_env.is_some()
                    || key_provider.is_some()
                    || reasoning.is_some() =>
                {
                    anyhow::bail!(
                        "--model, --key-env, --key-provider and --reasoning go with \
                         --endpoint or --anthropic"
                    )
                }
                _ => Provider::AppleIntelligence { guided: *guided },
            };
            let dictionary = if *no_dictionary {
                None
            } else {
                Some(macalla_session::load_dictionary(
                    cli.dictionary.as_deref(),
                    macalla_session::DICTIONARY_CANDIDATES,
                )?)
            };
            enhance(&language, provider, dictionary, *prewarm, *json, text)
        }
        Command::Listen {
            timeout,
            json,
            device,
            paste,
            preview,
            history,
            no_vad,
            hotkey,
            shortcut,
            mode,
        } => {
            if let Some(name) = device.as_deref() {
                ensure_device_connected(name)?;
            }
            let opts = ListenOptions {
                timeout: if *hotkey {
                    *timeout
                } else {
                    Some(timeout.unwrap_or(ENTER_SECONDS))
                },
                json: *json,
                history: history.as_deref(),
                vad: !*no_vad,
                device: device.as_deref(),
                paste: *paste,
                preview: *preview,
            };
            if *hotkey {
                listen_on_hotkey(&cli, &language, opts, shortcut.as_deref(), *mode)
            } else {
                listen(&cli, &language, opts)
            }
        }
    }
}

// ---------------------------------------------------------------- session

/// How long `listen` waits for Enter when --timeout is not given. A listening
/// time, not the cap on a held key, which is the session's.
const ENTER_SECONDS: u64 = 30;

/// What `listen` and the hotkey loop both need from the command line.
#[derive(Clone, Copy)]
struct ListenOptions<'a> {
    /// Seconds. `None` is the session's cap on a dictation.
    timeout: Option<u64>,
    json: bool,
    device: Option<&'a str>,
    paste: bool,
    preview: bool,
    history: Option<&'a Path>,
    vad: bool,
}

/// The history store, opened once per session when `--history` names a file.
fn open_history(opts: ListenOptions<'_>) -> Result<Option<SqliteHistory>> {
    let Some(path) = opts.history else {
        return Ok(None);
    };
    let mut store = SqliteHistory::open(path)
        .with_context(|| format!("opening the history at {}", path.display()))?;
    purge_history(&mut store, macalla_core::history::Moment::Launch);
    Ok(Some(store))
}

/// The file's own retention rule, applied. Never fatal: the next moment
/// tries again.
fn purge_history(store: &mut SqliteHistory, moment: macalla_core::history::Moment) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    match macalla_core::history::purge(store, moment, now_ms) {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "history purged"),
        Err(e) => tracing::warn!(error = %e, "could not purge history"),
    }
}

/// Keep a delivered dictation. After the delivery and never before it: the
/// paste is what the user is waiting for. A dictation with no speech is not
/// worth a row.
fn remember(store: &mut Option<SqliteHistory>, dictation: &Dictation, engine: &str) -> Result<()> {
    let Some(store) = store.as_mut() else {
        return Ok(());
    };
    if dictation.document.to_plain().trim().is_empty() {
        return Ok(());
    }
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let entry = macalla_session::history::entry_from(dictation, engine, ts_ms);
    // The core's gate, so a file set to "off" is never written to.
    macalla_core::history::record_if_kept(store, entry).context("recording the dictation")?;
    purge_history(store, macalla_core::history::Moment::AfterDictation);
    Ok(())
}

/// The dictionary subcommand. The rules of an edit are the core's and the
/// file handling is the session crate's -- the same two the desktop app's
/// Dictionary view uses -- so this is only the terminal's shape of them.
fn dictionary(cli: &Cli, action: &DictionaryAction) -> Result<()> {
    use macalla_session::dictionary as file;
    let path = cli
        .dictionary
        .clone()
        .unwrap_or_else(|| PathBuf::from(macalla_session::DICTIONARY_CANDIDATES[0]));
    let mut terms = file::read(&path)?;
    match action {
        DictionaryAction::List => {
            if terms.terms.is_empty() {
                println!("no terms in {}", path.display());
            }
            for t in &terms.terms {
                if t.spoken.is_empty() {
                    println!("{}", t.written);
                } else {
                    println!("{}  <- {}", t.written, t.spoken.join(", "));
                }
            }
            Ok(())
        }
        DictionaryAction::Add { written, spoken } => {
            let created = terms.add(written, spoken)?;
            file::write(&path, &terms)?;
            let term = terms.get(written).expect("just added");
            println!(
                "{} {}  <- {}",
                if created { "added" } else { "updated" },
                term.written,
                term.spoken.join(", ")
            );
            Ok(())
        }
        DictionaryAction::Remove { written } => {
            anyhow::ensure!(terms.remove(written), "there is no term for {written:?}");
            file::write(&path, &terms)?;
            println!("forgot {written}");
            Ok(())
        }
        DictionaryAction::Import { path: other } => {
            let json = std::fs::read_to_string(other)
                .with_context(|| format!("reading {}", other.display()))?;
            let theirs = file::parse(&json)?;
            let changed = terms.merge(&theirs);
            file::write(&path, &terms)?;
            println!(
                "{changed} {} added or updated from {}",
                if changed == 1 { "term" } else { "terms" },
                other.display()
            );
            Ok(())
        }
        DictionaryAction::Export => {
            print!("{}", file::to_json(&terms));
            Ok(())
        }
    }
}

/// Which model `macalla enhance` asks. `key` is already the session's own
/// [`choice::KeySource`] -- `main` is the one place that reads `--key-env`
/// and `--key-provider` and decides which shape they make, so nothing below
/// this needs either flag by name.
enum Provider<'a> {
    AppleIntelligence {
        guided: bool,
    },
    Endpoint {
        endpoint: &'a str,
        model: &'a str,
        key: choice::KeySource,
        reasoning: Option<ReasoningChoice>,
    },
    Anthropic {
        model: &'a str,
        key: choice::KeySource,
        reasoning: Option<ReasoningChoice>,
    },
}

/// `--reasoning` for `macalla enhance`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ReasoningChoice {
    /// Ask a reasoning model not to reason.
    Least,
    /// Say nothing, and let the server decide.
    Server,
}

/// `--reasoning` translated into the session's own type.
fn reasoning_of(choice: Option<ReasoningChoice>) -> Option<choice::Reasoning> {
    choice.map(|r| match r {
        ReasoningChoice::Least => choice::Reasoning::Least,
        ReasoningChoice::Server => choice::Reasoning::ServerDefault,
    })
}

/// The enhancer `macalla enhance` asks, built entirely by
/// `macalla_session::choice::enhancer` -- this crate never imports a
/// `macalla-enhancers` adapter type to construct one, so Apple Intelligence's
/// availability check, `--key-provider`'s keychain lookup, `--key-env`'s
/// environment read and the stored `anthropic` key are all exactly the code
/// path the desktop app will use too.
fn build_enhancer(provider: Provider<'_>) -> Result<Box<dyn Enhancer>> {
    let store = SystemSecrets::new();
    let picked = match provider {
        Provider::AppleIntelligence { guided } => choice::EnhancerChoice::AppleIntelligence {
            guided: if guided { true } else { choice::DEFAULT_GUIDED },
        },
        Provider::Endpoint {
            endpoint,
            model,
            key,
            reasoning,
        } => choice::EnhancerChoice::Endpoint {
            url: endpoint.to_string(),
            model: model.to_string(),
            key,
            reasoning: reasoning_of(reasoning),
        },
        Provider::Anthropic {
            model,
            key,
            reasoning,
        } => choice::EnhancerChoice::Anthropic {
            model: model.to_string(),
            key,
            reasoning: reasoning_of(reasoning),
        },
    };
    Ok(choice::enhancer(&picked, &store)?)
}

/// `macalla enhance`: what dictation does with enhancement switched on, minus
/// the microphone: the dictionary's corrections, Tier 0, then the model with
/// the dictionary's written forms as known terms. An endpoint's times include
/// the network.
fn enhance(
    language: &str,
    provider: Provider<'_>,
    dictionary: Option<(macalla_core::dictionary::Dictionary, String)>,
    prewarm: bool,
    json: bool,
    text: &[String],
) -> Result<()> {
    use std::io::BufRead;

    let enhancer = build_enhancer(provider)?;
    let tier0 = macalla_core::tier0::Tier0Rules::new(language.split(['-', '_']).next());
    let pieces: Vec<String> = if text.is_empty() {
        std::io::stdin()
            .lock()
            .lines()
            .collect::<std::io::Result<_>>()?
    } else {
        vec![text.join(" ")]
    };
    let pieces: Vec<&String> = pieces.iter().filter(|p| !p.trim().is_empty()).collect();
    let terms: Vec<String> = match &dictionary {
        Some((dictionary, label)) => {
            eprintln!("dictionary: {label}");
            dictionary
                .written_forms()
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
        None => Vec::new(),
    };
    if prewarm {
        enhancer.prewarm();
        std::thread::sleep(Duration::from_secs(2));
    }

    let mut failed = 0;
    for piece in &pieces {
        let blocks = tier0_blocks(&tier0, dictionary.as_ref().map(|(d, _)| d), piece);
        let tier0_text = Document::new(blocks.clone()).to_plain();
        let started = Instant::now();
        let result = enhancer.enhance(&blocks, &terms);
        let ms = started.elapsed().as_millis();
        match result {
            Ok(enhanced) => {
                // Nothing else applies the dictionary after Tier 1: a
                // free-form reply may respell a term, or Tier 1's own case
                // rules may re-case one, so the dictionary gets the last
                // word before this is printed.
                let enhanced = match &dictionary {
                    Some((dictionary, _)) => dictionary.settle(&enhanced),
                    None => enhanced,
                };
                let enhanced = Document::new(enhanced).to_plain();
                if json {
                    let row = serde_json::json!({
                        "tier0": tier0_text,
                        "enhanced": enhanced,
                        "unchanged": enhanced == tier0_text,
                        "ms": ms,
                    });
                    println!("{row}");
                } else {
                    println!("{ms:>6} ms  {enhanced}");
                }
            }
            Err(error) => {
                failed += 1;
                if json {
                    let row = serde_json::json!({"tier0": tier0_text, "error": error.to_string(), "ms": ms});
                    println!("{row}");
                } else {
                    eprintln!("{ms:>6} ms  {error}");
                }
            }
        }
    }
    let caps = enhancer.caps();
    if let Some(median) = caps.typical_ms {
        eprintln!("{}: median of the recent requests {median} ms", caps.id);
    }
    if failed > 0 {
        anyhow::bail!("{failed} of {} pieces failed", pieces.len());
    }
    Ok(())
}

/// One piece in dictation's order: the dictionary's corrections, then Tier 0.
fn tier0_blocks(
    tier0: &macalla_core::tier0::Tier0Rules,
    dictionary: Option<&macalla_core::dictionary::Dictionary>,
    piece: &str,
) -> Vec<macalla_core::block::Block> {
    use macalla_core::enhance::Tier0;
    match dictionary {
        Some(dictionary) => tier0.process(&dictionary.correct(piece), None),
        None => tier0.process(piece, None),
    }
}

fn list_history(path: &Path, limit: usize, keep: Option<&str>) -> Result<()> {
    use macalla_core::history::{self, HistoryStore, Retention};
    anyhow::ensure!(path.exists(), "{} does not exist", path.display());
    let mut store = SqliteHistory::open(path)
        .with_context(|| format!("opening the history at {}", path.display()))?;
    if let Some(keep) = keep {
        let retention = Retention::parse(keep)
            .with_context(|| format!("{keep:?} is not a retention; 30 days is the most"))?;
        store.set_setting(Retention::SETTING, &retention.to_string())?;
        purge_history(&mut store, history::Moment::AfterDictation);
    }
    println!("kept for: {}", history::retention_of(&store)?.label());
    let entries = store.list(limit)?;
    if entries.is_empty() {
        println!("nothing yet");
    }
    for e in entries {
        println!("{}  {}  {}", e.id, e.ts_ms, e.enhanced.trim());
    }
    Ok(())
}

/// A session's configuration, from the flags. Everything else about a
/// dictation -- which engine, where the dictionary is, the microphone, the
/// loop -- is `macalla_session`'s, and the desktop app builds the same thing
/// from its settings.
fn config(cli: &Cli, language: &str, device: Option<&str>, timeout: Option<u64>) -> Config {
    Config {
        engine: cli.engine.clone(),
        cloud: None,
        language: language.to_string(),
        dictionary: cli.dictionary.clone(),
        device: device.map(str::to_string),
        max_duration: timeout.map_or(Config::default().max_duration, Duration::from_secs),
        vad_gate: false,
        // No flag chooses cleanup for a dictation; `macalla enhance` is where
        // a provider is tried.
        cleanup: Vec::new(),
    }
}

fn listen_config(cli: &Cli, language: &str, opts: ListenOptions<'_>) -> Config {
    Config {
        vad_gate: opts.vad,
        ..config(cli, language, opts.device, opts.timeout)
    }
}

/// Say which microphone is being listened through, and show transcripts as
/// they arrive when asked to: partials with dots, and each committed
/// utterance with a mark, which is the only live feedback a batch engine
/// gives. The level is the overlay's business, not a terminal's.
fn preview(progress: Progress<'_>, opts: ListenOptions<'_>) {
    match progress {
        Progress::Device(name) => eprintln!("  listening through {name}"),
        Progress::Transcript(t) if opts.preview => {
            if t.is_final {
                eprintln!("  > {}", t.text);
            } else {
                eprintln!("  ... {}", t.text);
            }
        }
        Progress::Capped(cap) => eprintln!(
            "  stopped at the {} limit; anything said after it was not heard",
            limit(cap)
        ),
        Progress::CloudWaiting { provider, elapsed } => {
            eprintln!("  waiting for {provider} ({} s)", elapsed.as_secs())
        }
        Progress::Enhancing { with, elapsed } => eprintln!("{}", words::enhancing(with, elapsed)),
        // The terminal keeps nothing while it waits; the wait itself is said.
        Progress::Transcript(_) | Progress::Level(_) | Progress::CloudSent { .. } => {}
    }
}

/// How a dictation's cap reads in a sentence: "5-minute", "30-second".
fn limit(cap: Duration) -> String {
    let secs = cap.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}-minute", secs / 60)
    } else {
        format!("{secs}-second")
    }
}

/// The per-dictation breakdown under `--debug`. This is how the speed claim
/// is proved, so every stage is named and the ones not yet wired print as
/// absent rather than as zero.
fn print_timing(dictation: &Dictation) {
    let t = &dictation.timing;
    let ms = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.0} ms"));
    // Only for a dictation that sent one, so a local breakdown reads as it
    // always has.
    let cloud = t
        .cloud_ms
        .map_or(String::new(), |v| format!(" | cloud {v:.0} ms"));
    // The same for a dictation that waited on cleanup.
    let cleanup = t
        .tier1_ms
        .map_or(String::new(), |v| format!(" | cleanup wait {v:.0} ms"));
    eprintln!(
        "  audio {:.1}s | overruns {} | device warnings {} | device {} | engine {} | \
         vad {} | release->final {}{cloud} | pipeline {}{cleanup} | release->text {:.0} ms",
        t.audio_ms as f64 / 1000.0,
        t.overruns,
        dictation.stream_warnings,
        ms(t.open_ms),
        ms(t.engine_ms),
        ms(t.vad_ms),
        ms(t.stt_ms),
        ms(t.tier0_ms),
        t.total_ms,
    );
}

/// The line `--debug` prints about where a dictation's words went, for
/// The visible label for "nothing leaves the machine unless turned on":
/// transcription on this machine ("Local") or a provider's
/// name, then cleanup's destination -- "Local", "Your server", a provider's
/// name -- or "off". Both are what the session opened, not what was asked
/// for.
fn where_line(transcription: Option<&str>, cleanup: Option<&str>) -> String {
    let transcription = match transcription {
        Some(id) => providers::find(id).map_or(id, |p| p.name),
        None => "Local",
    };
    format!(
        "  transcription: {transcription} | cleanup: {}",
        cleanup.unwrap_or("off")
    )
}

/// `macalla doctor`'s Tier 1 line. The plan is advice: cleanup a person has
/// chosen runs whatever it says, so "no work" must not read as "will not
/// run".
fn tier1_line(plan: &macalla_core::pipeline::PipelinePlan, engine_id: &str) -> String {
    use macalla_core::pipeline::{PipelinePlan, TextCaps};
    match PipelinePlan::tier1_disabled_reason(&TextCaps::for_engine_id(engine_id)) {
        Some(why) => format!("no work for this engine -- {why}; chosen cleanup runs anyway"),
        None if plan.tier1 => "has work for this engine; runs when cleanup is chosen".into(),
        None => "no work for this engine; chosen cleanup runs anyway".into(),
    }
}

/// The one line `--debug` prints when cleanup finished after the text was
/// delivered without it. Nothing else is said about it: what was delivered
/// stays the answer.
fn late_cleanup_line(late: &macalla_session::LateDocument) -> String {
    if late.partial {
        return format!(
            "  cleanup stopped before every segment came back, after {:.0} ms; the text above \
             is without it",
            late.ms
        );
    }
    format!(
        "  cleanup finished late, in {:.0} ms against a {} ms budget; the text above is without it",
        late.ms,
        macalla_session::ENHANCEMENT_WAIT.as_millis()
    )
}

/// Why a cloud engine's text is not being delivered, as a sentence, or
/// `None` when it is, when no cloud engine was used, or when nothing was
/// said -- silence is said the way a local session says it, in [`deliver`].
/// The session refuses the paste itself; this is only what the terminal
/// says about it.
fn cloud_withheld(dictation: &Dictation) -> Option<String> {
    let cloud = dictation.cloud.as_ref()?;
    let provider = &cloud.provider;
    match &cloud.outcome {
        CloudOutcome::Transcribed | CloudOutcome::NoSpeech => None,
        CloudOutcome::Failed(reason) => Some(format!(
            "{provider} could not transcribe it, so nothing was printed or pasted: {reason}"
        )),
        CloudOutcome::TimedOut => Some(format!(
            "{provider} did not answer within {:.0} s, so nothing was printed or pasted",
            cloud.ms / 1000.0
        )),
        CloudOutcome::Abandoned => Some(format!(
            "stopped waiting for {provider}, so nothing was printed or pasted"
        )),
    }
}

/// A stronger line for `--hotkey`, printed under `cloud_withheld`'s reason
/// once the provider has failed [`macalla_session::REPEATED_CLOUD_FAILURES`]
/// times in a row: one bad request is not evidence of a broken key or
/// connection, but a streak is, and a push-to-talk session that runs all day
/// must say so instead of repeating the same one-off-sounding reason every
/// time. `None` short of the threshold.
///
/// Only `listen_on_hotkey` calls this, and that is macOS-only for now; gated
/// the same way so the other platforms have no unused-function warning under
/// `-D warnings`.
#[cfg(target_os = "macos")]
fn repeated_cloud_failure_line(cloud: &macalla_session::CloudResult) -> Option<String> {
    if !cloud.failing_repeatedly() {
        return None;
    }
    let name = providers::find(&cloud.provider).map_or(cloud.provider.as_str(), |p| p.name);
    Some(format!(
        "{name} has failed {} times in a row. Check its key with `macalla key set {}` and your \
         connection, or choose a local engine.",
        cloud.failures_in_a_row, cloud.provider
    ))
}

/// The microphone or the engine stopping partway, which is worth saying
/// whatever happens to the text.
fn report_faults(dictation: &Dictation) {
    if let Some(fault) = &dictation.device_fault {
        eprintln!("  ! {fault}");
    }
    if let Some(fault) = &dictation.engine_fault {
        eprintln!("  ! {}", engine_fault_line(dictation, fault));
    }
}

/// How an engine stopping partway reads. In a cloud dictation that engine was
/// only the live preview, and the whole recording still went to the provider
/// -- unless nothing was sent at all.
fn engine_fault_line(dictation: &Dictation, fault: &macalla_core::Error) -> String {
    match &dictation.cloud {
        None => format!("the engine stopped partway; kept what it had: {fault}"),
        Some(cloud)
            if matches!(
                cloud.outcome,
                CloudOutcome::NoSpeech | CloudOutcome::Abandoned
            ) =>
        {
            format!("the live preview stopped partway: {fault}")
        }
        Some(cloud) => format!(
            "the live preview stopped partway; the whole recording still went to {}: {fault}",
            cloud.provider
        ),
    }
}

/// Print the document, or put it where the user was typing.
fn deliver(
    dictation: &mut Dictation,
    opts: ListenOptions<'_>,
    enhancing_with: Option<&EnhancingWith>,
) -> Result<()> {
    report_faults(dictation);
    // The gate's word, when it ran, or the cloud session's: nothing was said,
    // so nothing was sent and there is nothing to print or paste. Not an
    // error -- pressing the key by accident is common.
    let silent_cloud = matches!(
        &dictation.cloud,
        Some(cloud) if cloud.outcome == CloudOutcome::NoSpeech
    );
    if dictation.heard_speech == Some(false) || silent_cloud {
        eprintln!("no speech was detected");
        return Ok(());
    }
    // The session refuses to paste a cloud dictation it did not transcribe.
    // Printing it would deliver the same text by another door, so a caller
    // that skipped `cloud_withheld` still gets nothing on stdout.
    if let Some(reason) = cloud_withheld(dictation) {
        anyhow::bail!(reason);
    }
    if !opts.paste {
        return emit(&dictation.document, &dictation.transcripts, opts.json);
    }
    let pasted = macalla_session::paste(dictation).context("pasting")?;
    eprintln!("{}", paste_line(pasted));
    // Said only of text that went in: a paste that did not happen has no
    // Enhancement to explain.
    if let (
        macalla_session::Paste::Inserted {
            how:
                macalla_platform::paste::Inserted::PastedAndRestored
                | macalla_platform::paste::Inserted::PastedAndKept
                | macalla_platform::paste::Inserted::Typed,
            ..
        },
        Some(outcome),
        Some(with),
    ) = (pasted, dictation.enhancement, enhancing_with)
    {
        if let Some(line) = words::pasted_without_enhancement(outcome, with) {
            eprintln!("{line}");
        }
    }
    if printed_after_paste(pasted) {
        return emit(&dictation.document, &dictation.transcripts, opts.json);
    }
    Ok(())
}

/// Whether `--paste` still prints the document.
///
/// Only when nothing was delivered: the platform guarantees it touched
/// nothing to find that out -- no clipboard, no keystroke -- so stdout is
/// where the text goes instead. That is what the pre-flight note promises,
/// and without it a run with `--paste` and without `--history` loses the
/// text altogether. Text that went in is not printed as well; that would
/// deliver the same dictation twice.
fn printed_after_paste(pasted: macalla_session::Paste) -> bool {
    matches!(
        pasted,
        macalla_session::Paste::Inserted {
            how: macalla_platform::paste::Inserted::NotDelivered(_),
            ..
        }
    )
}

/// What the terminal says a paste came to.
fn paste_line(pasted: macalla_session::Paste) -> String {
    use macalla_session::Paste;
    match pasted {
        Paste::Nothing => "nothing to paste -- no speech was detected".into(),
        // The callers say why before they get here; this is the session
        // keeping to its rule if one ever does not.
        Paste::Withheld => "nothing pasted -- the cloud engine did not transcribe it".into(),
        // Printed, not an error: the text was heard and is kept with
        // --history, and a push-to-talk session carries on.
        Paste::MovedOn => {
            "not pasted, because another app is in front now -- the text did not follow you".into()
        }
        // The one outcome that is not a paste: nothing was posted, because
        // nothing could have arrived. A sentence, not a debug shape, because
        // there is something the person can do about it.
        Paste::Inserted {
            how: macalla_platform::paste::Inserted::NotDelivered(why),
            ..
        } => words::could_not_paste(why).to_string(),
        Paste::Inserted { how, .. } => format!("{how:?}"),
    }
}

// ---------------------------------------------------------------- commands

/// The first-launch checklist's screen, without a screen.
///
/// The same `Checklist` the tray will render. If this and the tray ever
/// disagree about whether someone is ready, one of them is reading the wrong
/// source -- which is the entire reason the model lives in the core.
fn setup(language: &str, fix: bool) -> Result<()> {
    use macalla_core::onboarding::{Checklist, StepState};

    let mut steps = macalla_platform::onboarding::permission_steps();
    steps.extend(macalla_engines::registry::engine_step(language));
    let checklist = Checklist::new(steps);

    println!("Macalla setup\n");
    for step in checklist.steps() {
        let tick = match step.state {
            StepState::Done => "[x]",
            StepState::Required => "[ ]",
            // Not a box. It is not on the critical path and a box invites
            // someone to think it is.
            StepState::Optional => " - ",
        };
        println!("  {tick} {}  {}", step.title, step.detail);
        if step.state != StepState::Done {
            if let Some(action) = &step.action {
                println!("        {action}");
            }
        }
    }

    println!(
        "\n{}",
        checklist.summary(&format!(
            "and hold {}",
            macalla_platform::hotkey::Shortcut::default_for_platform()
        ))
    );

    if fix {
        // Asking is a separate operation from checking, deliberately: drawing
        // this screen must never throw dialogs at someone.
        let asked = ask_for_missing();
        if asked == 0 {
            println!("\nNothing left to ask for.");
        } else {
            println!(
                "\nAsked for {asked}. Anything granted to your TERMINAL rather than \
                 to macalla is correct -- an unsigned CLI has no identity of its own. \
                 Re-run `macalla setup` to see the result."
            );
        }
    } else if !checklist.is_ready() {
        println!("\nRun `macalla setup --fix` to be asked for what is missing.");
    }
    Ok(())
}

/// Prompt for every permission that is not granted. Returns how many were asked.
fn ask_for_missing() -> usize {
    use macalla_platform::{Grant, Permission};
    let mut asked = 0;
    for permission in Permission::required_here() {
        if permission.granted() == Grant::Granted {
            continue;
        }
        if permission.request() == Grant::Granted {
            println!("  granted: {permission:?}");
        }
        asked += 1;
    }
    asked
}

fn models(action: &ModelsAction) -> Result<()> {
    use macalla_engines::models::{self, ModelStatus, Progress, Store};
    let store = Store::shared();
    let spec_for = |id: &str| {
        models::find(id).ok_or_else(|| {
            let known: Vec<&str> = models::catalogue().iter().map(|m| m.id).collect();
            anyhow::anyhow!(
                "no model called {id:?}. This build has: {}",
                known.join(", ")
            )
        })
    };
    match action {
        ModelsAction::List => {
            println!("store: {}", store.dir().display());
            for m in models::catalogue() {
                let state = match store.status(m) {
                    ModelStatus::Installed(_) => "installed".to_string(),
                    ModelStatus::Partial { bytes } => {
                        format!("partial, {} of {} MB", bytes / 1_000_000, m.megabytes())
                    }
                    ModelStatus::Absent => "not downloaded".to_string(),
                };
                println!("  {} -- {}: {state}", m.id, m.name);
                println!("      {} MB, {}", m.megabytes(), m.licence);
                if !m.attribution.is_empty() {
                    println!("      {}", m.attribution);
                }
            }
            Ok(())
        }
        ModelsAction::Download { id, mirror } => {
            let spec = spec_for(id)?;
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let mut last = 0u64;
            let mut on_progress = |p: Progress| match p {
                Progress::Received { bytes, total } => {
                    // One line, rewritten, every few MB.
                    if bytes / 5_000_000 != last / 5_000_000 || bytes == total {
                        eprint!(
                            "\r  {} of {} MB ({:.0}%)   ",
                            bytes / 1_000_000,
                            total / 1_000_000,
                            bytes as f64 * 100.0 / total.max(1) as f64
                        );
                    }
                    last = bytes;
                }
                Progress::Reconnecting { bytes, total } => {
                    eprint!(
                        "\r  {} of {} MB, the connection stalled; reconnecting   ",
                        bytes / 1_000_000,
                        total / 1_000_000
                    );
                }
                Progress::Verifying => eprintln!("\n  checking the file"),
            };
            eprintln!(
                "downloading {} ({} MB) into {}",
                spec.name,
                spec.megabytes(),
                store.dir().display()
            );
            let path =
                models::download(spec, &store, mirror.as_deref(), &mut on_progress, &cancel)?;
            eprintln!("  preparing the model for this computer");
            match models::warm(spec) {
                Ok(took) => eprintln!("  ready in {:.1} s", took.as_secs_f64()),
                Err(e) => eprintln!("  ! could not warm the model: {e}"),
            }
            println!("{}", path.display());
            Ok(())
        }
        ModelsAction::Remove { id } => {
            let spec = spec_for(id)?;
            store.remove(spec)?;
            println!("removed {}", spec.name);
            Ok(())
        }
        ModelsAction::Import { id, path } => {
            let spec = spec_for(id)?;
            let installed = store.import(spec, path)?;
            println!("{}", installed.display());
            Ok(())
        }
    }
}

/// `macalla key set|remove|list`: moving a provider's key in and out of the
/// OS keychain. Always the real store -- there is no `--dictionary`-style
/// override here, because a key typed at this command is meant to end up
/// wherever `Session::open` will look for it.
fn key_command(action: &KeyAction) -> Result<()> {
    let store = SystemSecrets::new();
    match action {
        KeyAction::Set { provider, from_env } => {
            let key = read_key_input(from_env.as_deref())?;
            providers::store_key(&store, provider, &key)?;
            println!("stored a key for {provider}");
            Ok(())
        }
        KeyAction::Remove { provider } => {
            providers::remove_key(&store, provider)?;
            println!("removed the key for {provider}");
            Ok(())
        }
        KeyAction::List => {
            for provider in providers::PROVIDERS {
                let kinds: Vec<&str> = provider
                    .kinds
                    .iter()
                    .map(|k| match k {
                        providers::Kind::Text => "text",
                        providers::Kind::Transcription => "transcription",
                    })
                    .collect();
                println!(
                    "{:<12} {:<12} {:<20} {}",
                    provider.id,
                    provider.name,
                    kinds.join(", "),
                    key_status(&store, provider.id)
                );
            }
            Ok(())
        }
    }
}

/// Whether a key is stored for `provider_id`, without ever showing it or
/// holding it just to answer the question.
fn key_status(store: &dyn SecretStore, provider_id: &str) -> String {
    match providers::key_state(store, provider_id) {
        Ok(providers::KeyState::Saved) => "stored".to_string(),
        Ok(providers::KeyState::Missing) => "missing".to_string(),
        Ok(providers::KeyState::StoreUnavailable(reason)) => {
            format!("store unavailable: {reason}")
        }
        // There IS an entry here; reading it just failed -- the OS access
        // prompt was denied, or the item does not decode. Different from
        // "missing", so it is worth its own line rather than folding into
        // the generic error arm below.
        Ok(providers::KeyState::Unreadable(reason)) => format!("stored, but unreadable: {reason}"),
        Err(e) => format!("error: {e}"),
    }
}

/// The key for `key set`: one line piped in, or the value of `--from-env`.
/// Refuses a terminal with neither, so a key is never typed where a shell
/// would echo it -- that is input handling and stays here. Whether what comes
/// out is usable at all (empty, only whitespace, or more than one line) is
/// [`providers::store_key`]'s call, so this command and the desktop app share
/// one rule; a variable set to an empty string used to skip that check
/// entirely and store an empty key.
fn read_key_input(from_env: Option<&str>) -> Result<String> {
    match from_env {
        Some(name) => Ok(std::env::var(name).with_context(|| format!("${name} is not set"))?),
        None => {
            use std::io::{IsTerminal, Read};
            anyhow::ensure!(
                !std::io::stdin().is_terminal(),
                "pipe the key in, for example `echo -n \"$KEY\" | macalla key set openai`, \
                 or pass --from-env NAME"
            );
            let mut all = String::new();
            std::io::stdin()
                .read_to_string(&mut all)
                .context("reading the key from stdin")?;
            one_line(&all)
        }
    }
}

/// Exactly one non-empty line of `stdin`, trimmed (which also drops a lone
/// trailing CR). Blank lines around it are ignored; more than one non-empty
/// line is refused outright, since silently taking the first would drop real
/// input without saying so -- and the error never repeats what was piped in.
/// Zero non-empty lines returns an empty string; [`providers::store_key`]
/// refuses that the same way it refuses any other empty key.
fn one_line(stdin: &str) -> Result<String> {
    let mut lines = stdin.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or_default().to_string();
    anyhow::ensure!(
        lines.next().is_none(),
        "expected exactly one key on stdin, but found more than one non-empty line"
    );
    Ok(first)
}

fn devices() -> Result<()> {
    let names = input_device_names().context("listing input devices")?;
    if names.is_empty() {
        println!("no input devices found");
        return Ok(());
    }
    for name in names {
        println!("{name}");
    }
    Ok(())
}

/// A `--device` that names nothing connected is an error here. The session
/// quietly stands the system default in for a chosen device that is not
/// there, which suits a choice saved in the app yesterday; a flag is typed
/// for this run, and a typo in it should say so.
fn ensure_device_connected(name: &str) -> Result<()> {
    let names = input_device_names().context("listing input devices")?;
    anyhow::ensure!(
        names.iter().any(|n| n == name),
        "no input device named {name:?}; `macalla devices` lists them"
    );
    Ok(())
}

/// How `macalla transcribe` writes a transcript: the finished text, the
/// JSON [`emit`] prints, or with `raw` what the engine produced. With
/// `format` Markdown, it is heard through the meeting path instead, and
/// written by [`markdown`]; `json` and `raw` are then refused.
#[derive(Clone, Copy)]
struct Rendering {
    json: bool,
    raw: bool,
    format: TranscribeFormat,
}

impl Rendering {
    /// The extension a transcript written to `--out-dir` takes.
    fn extension(self) -> &'static str {
        if self.format == TranscribeFormat::Md {
            "md"
        } else if self.json && !self.raw {
            "json"
        } else {
            "txt"
        }
    }

    /// The transcript as it is written, or `None` when nothing was heard.
    fn render(self, dictation: &Dictation) -> Result<Option<String>> {
        let text = if self.raw {
            macalla_core::stt::final_text(&dictation.transcripts)
        } else {
            dictation.document.to_plain()
        };
        if text.trim().is_empty() {
            return Ok(None);
        }
        if self.json && !self.raw {
            return Ok(Some(json_payload(
                &dictation.document,
                &dictation.transcripts,
            )?));
        }
        Ok(Some(text))
    }
}

fn transcribe(
    cli: &Cli,
    language: &str,
    paths: &[PathBuf],
    out_dir: Option<&Path>,
    rendering: Rendering,
    cloud: Option<CloudChoice<'_>>,
) -> Result<()> {
    // Said before any engine is opened.
    let markdown = rendering.format == TranscribeFormat::Md;
    anyhow::ensure!(
        !markdown || !(rendering.json || rendering.raw || cloud.is_some()),
        words::TRANSCRIBE_MARKDOWN_ALONE
    );
    let path = match (paths, out_dir) {
        ([path], None) => Some(path),
        (_, Some(_)) => None,
        (_, None) => anyhow::bail!(words::TRANSCRIBE_NEEDS_OUT_DIR),
    };
    if let Some(path) = path {
        anyhow::ensure!(path.exists(), "{} does not exist", path.display());
    }
    let session = match cloud {
        Some(choice) => {
            anyhow::ensure!(
                cli.engine.is_none(),
                "--engine and --cloud each name an engine; give one"
            );
            let session_config = Config {
                cloud: Some(choice::CloudTranscription {
                    provider: choice.provider.id().to_string(),
                    model: choice.model.map(str::to_string),
                }),
                ..config(cli, language, None, None)
            };
            match choice.key_env {
                Some(name) => {
                    Session::open_with_keys(session_config, &choice::EnvKey::named(name))?
                }
                None => Session::open(session_config)?,
            }
        }
        None => Session::open(config(cli, language, None, None))?,
    };

    let (Some(path), None) = (path, out_dir) else {
        let dir = out_dir.expect("several files need --out-dir, checked above");
        let failed = transcribe_into(
            &session,
            paths,
            dir,
            rendering,
            cli.debug,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )?;
        if failed > 0 {
            anyhow::bail!(words::transcribe_not_all(failed, paths.len()));
        }
        return Ok(());
    };

    if markdown {
        return transcribe_markdown(
            &session,
            path,
            cli.debug,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        );
    }

    let decoded = Instant::now();
    let samples = decode(path)?;
    let decode_ms = decoded.elapsed().as_secs_f64() * 1000.0;

    let dictation = session.transcribe(samples)?;
    if rendering.raw {
        println!("{}", macalla_core::stt::final_text(&dictation.transcripts));
        return Ok(());
    }
    emit(&dictation.document, &dictation.transcripts, rendering.json)?;

    if cli.debug {
        eprintln!(
            "{}",
            timing_line(
                &session,
                dictation.timing.audio_ms,
                decode_ms,
                dictation.timing.stt_ms.unwrap_or_default()
            )
        );
    }
    Ok(())
}

/// `macalla transcribe --debug`'s line after one file: the engine, the
/// dictionary, how long the audio was, and how long decoding and hearing it
/// took.
fn timing_line(session: &Session, audio_ms: u64, decode_ms: f64, stt_ms: f64) -> String {
    format!(
        "  engine {} | dictionary {} | audio {:.1}s | decode {decode_ms:.0} ms | stt {stt_ms:.0} ms",
        session.engine_id(),
        session.dictionary_label(),
        audio_ms as f64 / 1000.0,
    )
}

/// One file as Markdown on `out`: heard through the meeting path, as
/// `macalla watch` hears a file, so it has timed turns, and written by
/// [`markdown`]. A file with no speech in it writes nothing on `out` and
/// says so on `err`. With `debug`, [`timing_line`] follows on `err`, as for
/// plain text; its stt figure is the whole meeting path's time, measured
/// here, since that path reports no engine time of its own.
fn transcribe_markdown(
    session: &Session,
    path: &Path,
    debug: bool,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<()> {
    use macalla_session::{meeting, Error};
    let tools = session.meeting_tools()?;
    let decoded = Instant::now();
    let samples = meeting::decode_recording(path)?;
    let decode_ms = decoded.elapsed().as_secs_f64() * 1000.0;
    let audio_ms = samples.len() as u64 * 1000 / u64::from(macalla_core::audio::SAMPLE_RATE);
    let heard_at = Instant::now();
    let heard = meeting::transcribe_samples(&tools, samples, &mut |_| {}, &AtomicBool::new(false));
    let stt_ms = heard_at.elapsed().as_secs_f64() * 1000.0;
    match heard {
        Ok(heard) => write!(out, "{}", markdown(path, &heard))?,
        Err(Error::NothingHeard) => writeln!(
            err,
            "{}",
            words::transcribe_no_speech(&path.display().to_string())
        )?,
        Err(error) => return Err(error.into()),
    }
    if debug {
        writeln!(err, "{}", timing_line(session, audio_ms, decode_ms, stt_ms))?;
    }
    Ok(())
}

/// A meeting heard from the file at `path` as Markdown, by
/// [`macalla_core::markdown::write`], the writer the desktop app saves
/// Markdown with: the file's stem as its title, then its turns. The CLI has
/// no summary, so there is none in it.
fn markdown(path: &Path, meeting: &macalla_session::meeting::MeetingTranscript) -> String {
    use macalla_core::markdown;
    let doc = markdown::Document {
        title: path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default(),
        turns: meeting
            .turns
            .iter()
            .map(|turn| markdown::DocTurn {
                speaker: turn.speaker.clone(),
                start_ms: turn.start_ms,
                text: turn.text.clone(),
            })
            .collect(),
        summary: None,
        summary_out_of_date: false,
    };
    markdown::write(&doc, &words::MARKDOWN)
}

/// Why one of several files was not transcribed: a reason, which
/// `words::transcribe_failed` turns into a sentence and `--json` gives as
/// [`NotTranscribed::code`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotTranscribed {
    /// Nothing is at the path given.
    Missing,
    /// It is not a wav or aiff file, nothing else decoded it, and ffmpeg is
    /// not on the PATH.
    NoFfmpeg,
    /// It could not be read as audio.
    Undecodable,
    /// The engine failed on it.
    Engine,
    /// It was heard, and its transcript could not be written.
    NotWritten,
}

impl NotTranscribed {
    /// The reason as data, for `--json`.
    fn code(self) -> &'static str {
        match self {
            NotTranscribed::Missing => "missing",
            NotTranscribed::NoFfmpeg => "no_ffmpeg",
            NotTranscribed::Undecodable => "undecodable",
            NotTranscribed::Engine => "engine",
            NotTranscribed::NotWritten => "not_written",
        }
    }
}

/// What became of one of several files: written to a path, heard with no
/// speech in it, or not transcribed, with why and the error behind it.
enum Outcome {
    Written(PathBuf),
    NoSpeech,
    Failed(NotTranscribed, anyhow::Error),
}

/// One file of several: decode it, hear it on `session`, and write its
/// transcript into `dir` named for it (`<stem>.txt` or `.json`, never over a
/// file already there). `ffmpeg` says whether ffmpeg is on the PATH, asked
/// only when a file will not decode. With `tools`, for `--format md`, it is
/// heard through the meeting path instead, as `macalla watch` hears one
/// ([`hear_one`]), and written as `<stem>.md`.
fn transcribe_one(
    session: &Session,
    tools: Option<&macalla_session::meeting::MeetingTools>,
    path: &Path,
    dir: &Path,
    rendering: Rendering,
    ffmpeg: &mut dyn FnMut() -> bool,
) -> Outcome {
    if let Some(tools) = tools {
        return hear_one(
            tools,
            path,
            dir,
            WatchFormat::Md,
            &AtomicBool::new(false),
            ffmpeg,
        )
        .expect("nothing sets the stop, so every file is heard to the end");
    }
    if !path.exists() {
        return Outcome::Failed(
            NotTranscribed::Missing,
            anyhow::anyhow!("{} does not exist", path.display()),
        );
    }
    let samples = match decode(path) {
        Ok(samples) => samples,
        Err(error) => {
            let why = if ffmpeg() {
                NotTranscribed::Undecodable
            } else {
                NotTranscribed::NoFfmpeg
            };
            return Outcome::Failed(why, error);
        }
    };
    let dictation = match session.transcribe(samples) {
        Ok(dictation) => dictation,
        Err(error) => return Outcome::Failed(NotTranscribed::Engine, error.into()),
    };
    let text = match rendering.render(&dictation) {
        Ok(Some(text)) => text,
        Ok(None) => return Outcome::NoSpeech,
        Err(error) => return Outcome::Failed(NotTranscribed::NotWritten, error),
    };
    drop(dictation);
    let mut bytes = text.into_bytes();
    bytes.push(b'\n');
    write_transcript(path, dir, rendering.extension(), &bytes)
}

/// Write the transcript of the input at `path` into `dir`, named for it
/// (`<stem>.<extension>`), never over a file already there
/// ([`macalla_session::output::write_new`]).
fn write_transcript(path: &Path, dir: &Path, extension: &str, bytes: &[u8]) -> Outcome {
    let base = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().trim_start_matches('.').to_string())
        .filter(|stem| !stem.trim().is_empty())
        .unwrap_or_else(|| "transcript".to_string());
    match macalla_session::output::write_new(dir, &base, extension, bytes) {
        Ok(written) => Outcome::Written(written),
        Err(error) => Outcome::Failed(NotTranscribed::NotWritten, error.into()),
    }
}

/// Transcribe each of `paths` in turn on `session`, the one engine, and write
/// each transcript into `dir` as it finishes, before the next file is
/// decoded, so no more than one is held at a time ([`transcribe_one`]).
/// Nothing is overwritten, whether it was there before or written by this
/// run, and a write that fails leaves no file behind. For each file written,
/// `out` gets "<input>\t<output>", so a script can match what was made to what
/// was given; with `rendering.json`, `out` gets one JSON object per input
/// instead, failures and silent files included. A file that fails, or has no
/// speech, is said on `err` and the rest carry on; with `debug`, the error
/// behind a failure follows it. Returns how many failed.
fn transcribe_into(
    session: &Session,
    paths: &[PathBuf],
    dir: &Path,
    rendering: Rendering,
    debug: bool,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<usize> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tools = match rendering.format {
        TranscribeFormat::Md => Some(session.meeting_tools()?),
        TranscribeFormat::Txt => None,
    };
    let mut ffmpeg_known = None;
    let mut ffmpeg =
        || *ffmpeg_known.get_or_insert_with(macalla_platform::decode::ffmpeg_available);
    let mut failed = 0;
    for path in paths {
        let outcome = transcribe_one(session, tools.as_ref(), path, dir, rendering, &mut ffmpeg);
        if report(path, &outcome, rendering.json, debug, out, err)? {
            failed += 1;
        }
    }
    Ok(failed)
}

/// Say what became of one input: on `out`, "<input>\t<output>" for a file
/// written, or with `json` one JSON object for every input; on `err`, a
/// sentence for a failure or a file with no speech, and with `debug` the
/// error behind a failure. Returns whether it failed.
fn report(
    path: &Path,
    outcome: &Outcome,
    json: bool,
    debug: bool,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<bool> {
    let input = path.display().to_string();
    let (output, error, no_speech) = match outcome {
        Outcome::Written(written) => (Some(written.display().to_string()), None, false),
        Outcome::NoSpeech => (None, None, true),
        Outcome::Failed(why, _) => (None, Some(*why), false),
    };
    match outcome {
        Outcome::Written(_) => {}
        Outcome::NoSpeech => writeln!(err, "{}", words::transcribe_no_speech(&input))?,
        Outcome::Failed(why, error) => {
            writeln!(err, "{}", words::transcribe_failed(&input, *why))?;
            if debug {
                writeln!(err, "  {error:#}")?;
            }
        }
    }
    if json {
        let line = serde_json::json!({
            "input": input,
            "output": output,
            "error": error.map(NotTranscribed::code),
            "no_speech": no_speech,
        });
        writeln!(out, "{line}")?;
    } else if let Some(output) = output {
        writeln!(out, "{input}\t{output}")?;
    }
    Ok(error.is_some())
}

/// `--cloud` for `macalla transcribe`. Providers offering transcription, from
/// `macalla_session::providers::PROVIDERS`; kept as its own enum so clap can
/// validate it and print the choices, and mapped to a provider id by
/// [`CloudProvider::id`].
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CloudProvider {
    Openai,
    Mistral,
    Cohere,
    Openrouter,
    Gemini,
    Elevenlabs,
}

impl CloudProvider {
    fn id(self) -> &'static str {
        match self {
            CloudProvider::Openai => "openai",
            CloudProvider::Mistral => "mistral",
            CloudProvider::Cohere => "cohere",
            CloudProvider::Openrouter => "openrouter",
            CloudProvider::Gemini => "gemini",
            CloudProvider::Elevenlabs => "elevenlabs",
        }
    }
}

/// A cloud provider for `macalla transcribe`, with its model and where its
/// key comes from. `key_env` is `None` unless the flag was given, in which
/// case it overrides the keychain with [`choice::EnvKey`].
struct CloudChoice<'a> {
    provider: CloudProvider,
    model: Option<&'a str>,
    key_env: Option<&'a str>,
}

/// What `macalla transcribe` writes a transcript as: the dictation path's
/// text, which `--json` and `--raw` change, or Markdown from the meeting
/// path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum TranscribeFormat {
    #[value(help = words::TRANSCRIBE_FORMAT_TXT)]
    Txt,
    #[value(help = words::TRANSCRIBE_FORMAT_MD)]
    Md,
}

/// What `macalla watch` writes each transcript as, and what `macalla
/// transcribe --format md` writes several files as ([`hear_one`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum WatchFormat {
    #[value(help = words::WATCH_FORMAT_TXT)]
    Txt,
    #[value(help = words::WATCH_FORMAT_VTT)]
    Vtt,
    #[value(help = words::WATCH_FORMAT_SRT)]
    Srt,
    #[value(help = words::WATCH_FORMAT_MD)]
    Md,
}

impl WatchFormat {
    fn extension(self) -> &'static str {
        match self {
            WatchFormat::Txt => "txt",
            WatchFormat::Vtt => "vtt",
            WatchFormat::Srt => "srt",
            WatchFormat::Md => "md",
        }
    }

    /// The meeting heard from the file at `path` as it is written: all four
    /// from the same turns. Only Markdown uses `path`, for its title.
    fn render(
        self,
        path: &Path,
        meeting: &macalla_session::meeting::MeetingTranscript,
    ) -> Result<String> {
        Ok(match self {
            WatchFormat::Txt => render_turns(meeting, None, false)?,
            WatchFormat::Vtt => macalla_core::vtt::write(&meeting.turns),
            WatchFormat::Srt => macalla_core::srt::write(&meeting.turns),
            WatchFormat::Md => markdown(path, meeting),
        })
    }
}

/// How `macalla watch` reports and writes: the format, `--json`, `--debug`.
#[derive(Clone, Copy)]
struct WatchOutput {
    format: WatchFormat,
    json: bool,
    debug: bool,
}

/// `macalla watch`: hear each audio file that settles in `dir` and write its
/// transcript into `out_dir`, until Ctrl-C or SIGTERM ([`watch`] the module
/// has the rules). Opens the session once, and every file is heard on its
/// one engine through the meeting path, so each has timed turns for WebVTT
/// and SRT.
fn watch(
    cli: &Cli,
    language: &str,
    dir: &Path,
    out_dir: &Path,
    format: WatchFormat,
    json: bool,
) -> Result<()> {
    let done = watch_preflight(dir, out_dir)?;

    let session = Session::open(config(cli, language, None, None))?;
    let tools = session.meeting_tools()?;
    eprintln!(
        "{}",
        words::watch_started(
            &dir.display().to_string(),
            &out_dir.display().to_string(),
            watch::POLL.as_secs()
        )
    );
    stop_signals::install();
    let stop = AtomicBool::new(false);
    let ended = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Any Ctrl-C or SIGTERM stops the watch. The signals are read here,
        // on a thread of their own, because a file being heard holds the
        // other one.
        scope.spawn(|| {
            while !ended.load(Ordering::Relaxed) {
                if stop_signals::asked() != stop_signals::Asked::Nothing {
                    stop.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let _ended = SetOnDrop(&ended);
        watch_into(
            &tools,
            dir,
            out_dir,
            watch::Tracker::with_done(done),
            WatchOutput {
                format,
                json,
                debug: cli.debug,
            },
            &mut std::io::stdout(),
            &mut std::io::stderr(),
            &stop,
            &mut |stop| wait_unless_stopped(watch::POLL, stop),
            &mut watch::now_ns,
        )
    })
}

/// Everything `macalla watch` checks before any engine is opened: `dir` is
/// a folder, `out_dir` is made if it is not there, the two are not one
/// folder once links are followed, and the ledger reads. Gives back what
/// the ledger says is done.
fn watch_preflight(dir: &Path, out_dir: &Path) -> Result<Vec<watch::Seen>> {
    let name = |path: &Path| path.display().to_string();
    anyhow::ensure!(dir.is_dir(), words::watch_not_a_folder(&name(dir)));
    std::fs::create_dir_all(out_dir)
        .with_context(|| words::watch_out_dir_not_made(&name(out_dir)))?;
    let resolved = |path: &Path| {
        std::fs::canonicalize(path).with_context(|| words::watch_folder_unresolved(&name(path)))
    };
    anyhow::ensure!(
        resolved(dir)? != resolved(out_dir)?,
        words::WATCH_SAME_FOLDER
    );
    let ledger = out_dir.join(watch::LEDGER);
    watch::read_ledger(&ledger).with_context(|| words::watch_ledger_unreadable(&name(&ledger)))
}

/// Wait `interval`, looking at `stop` every 50 ms. Returns whether to go on.
fn wait_unless_stopped(interval: Duration, stop: &AtomicBool) -> bool {
    let until = Instant::now() + interval;
    while Instant::now() < until {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !stop.load(Ordering::Relaxed)
}

/// The watch loop: list `dir`, hear each file [`watch::Tracker::take`] says
/// is finished, write it into `out_dir`, note it in the ledger, report it
/// as `macalla transcribe` does ([`report`]), then `wait` for the next list.
/// A file that fails is said and the rest carry on. Ends when `stop` is set,
/// before the next file or while one is heard (which then writes nothing),
/// or when `wait` says so. `clock` gives the time each list is made at, for
/// [`watch::Tracker::take`]. The first list says how many files are waiting;
/// a name that is not UTF-8 is said once. A list that fails is tried again
/// at the next poll; only [`watch::LIST_ATTEMPTS`] failures in a row, or a
/// report that cannot be printed, end it with an error.
#[allow(clippy::too_many_arguments)]
fn watch_into(
    tools: &macalla_session::meeting::MeetingTools,
    dir: &Path,
    out_dir: &Path,
    mut tracker: watch::Tracker,
    output: WatchOutput,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
    stop: &AtomicBool,
    wait: &mut dyn FnMut(&AtomicBool) -> bool,
    clock: &mut dyn FnMut() -> u64,
) -> Result<()> {
    let ledger = out_dir.join(watch::LEDGER);
    let mut ffmpeg_known = None;
    let mut ffmpeg =
        || *ffmpeg_known.get_or_insert_with(macalla_platform::decode::ffmpeg_available);
    let mut failed_lists = 0;
    let mut counted = false;
    let mut said_not_utf8 = std::collections::HashSet::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let listed_ns = clock();
        let listing = match watch::snapshot(dir) {
            Ok(listing) => {
                failed_lists = 0;
                listing
            }
            Err(error) => {
                failed_lists += 1;
                tracing::debug!(%error, failed_lists, "the watched folder could not be listed");
                anyhow::ensure!(
                    failed_lists < watch::LIST_ATTEMPTS,
                    words::watch_folder_gone(&dir.display().to_string(), failed_lists)
                );
                if stop.load(Ordering::Relaxed) || !wait(stop) {
                    break;
                }
                continue;
            }
        };
        for name in listing.not_utf8 {
            if said_not_utf8.insert(name.clone()) {
                let shown = dir.join(&name).display().to_string();
                writeln!(err, "{}", words::watch_name_not_utf8(&shown))?;
            }
        }
        if !std::mem::replace(&mut counted, true) {
            writeln!(
                err,
                "{}",
                words::watch_waiting(tracker.waiting(&listing.seen))
            )?;
        }
        for seen in tracker.take(listing.seen, listed_ns) {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let path = dir.join(&seen.name);
            let Some(outcome) = hear_one(tools, &path, out_dir, output.format, stop, &mut ffmpeg)
            else {
                writeln!(
                    err,
                    "{}",
                    words::watch_stopped_during(&path.display().to_string())
                )?;
                break;
            };
            let written = match &outcome {
                Outcome::Written(written) => Some(Some(written.as_path())),
                Outcome::NoSpeech => Some(None),
                Outcome::Failed(..) => None,
            };
            report(&path, &outcome, output.json, output.debug, out, err)?;
            if let Some(written) = written {
                if let Err(error) = watch::append_ledger(&ledger, &seen, written) {
                    writeln!(
                        err,
                        "{}",
                        words::watch_not_noted(&path.display().to_string())
                    )?;
                    if output.debug {
                        writeln!(err, "  {error:#}")?;
                    }
                }
            }
            out.flush()?;
        }
        if stop.load(Ordering::Relaxed) || !wait(stop) {
            break;
        }
    }
    writeln!(err, "{}", words::WATCH_STOPPED)?;
    Ok(())
}

/// One watched file: decode it, hear it through the meeting path on the
/// session's engine, and write its transcript into `dir` in `format`, never
/// over a file already there. `None` when `stop` was set before it was
/// heard to the end: nothing is written for it.
fn hear_one(
    tools: &macalla_session::meeting::MeetingTools,
    path: &Path,
    dir: &Path,
    format: WatchFormat,
    stop: &AtomicBool,
    ffmpeg: &mut dyn FnMut() -> bool,
) -> Option<Outcome> {
    use macalla_session::{meeting, Error};
    if !path.exists() {
        return Some(Outcome::Failed(
            NotTranscribed::Missing,
            anyhow::anyhow!("{} does not exist", path.display()),
        ));
    }
    let samples = match meeting::decode_recording(path) {
        Ok(samples) => samples,
        Err(error) => {
            let why = if ffmpeg() {
                NotTranscribed::Undecodable
            } else {
                NotTranscribed::NoFfmpeg
            };
            return Some(Outcome::Failed(why, error.into()));
        }
    };
    let meeting = match meeting::transcribe_samples(tools, samples, &mut |_| {}, stop) {
        Ok(meeting) => meeting,
        Err(Error::Stopped) => return None,
        Err(Error::NothingHeard) => return Some(Outcome::NoSpeech),
        Err(error) => return Some(Outcome::Failed(NotTranscribed::Engine, error.into())),
    };
    Some(match format.render(path, &meeting) {
        Ok(text) => write_transcript(path, dir, format.extension(), text.as_bytes()),
        Err(error) => Outcome::Failed(NotTranscribed::NotWritten, error),
    })
}

fn listen(cli: &Cli, language: &str, opts: ListenOptions<'_>) -> Result<()> {
    let mut session = Session::open(listen_config(cli, language, opts))?;
    let mut history = open_history(opts)?;
    eprintln!(
        "listening -- press Enter to stop, or wait {}s  [{}, {}]",
        opts.timeout.unwrap_or(ENTER_SECONDS),
        session.engine_id(),
        session.dictionary_label()
    );

    let stop = Arc::new(AtomicBool::new(false));
    {
        // A blocking read on its own thread, so the capture loop keeps running.
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut line = String::new();
            // EOF is not Enter. With stdin closed or redirected from /dev/null
            // the read returns 0 immediately, and treating that as "stop" would
            // end every non-interactive dictation before a word was said.
            if matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
                stop.store(true, Ordering::Relaxed);
            }
        });
    }

    let flag = Arc::clone(&stop);
    // Nothing here gives up on a cloud request: Ctrl-C ends the process, and
    // the session's cap ends the wait.
    let mut dictation = session.dictate(
        // Enter cannot throw a recording away.
        &mut || StopSignal::from(flag.load(Ordering::Relaxed)),
        &mut || false,
        &mut |p| preview(p, opts),
    )?;
    if cli.debug {
        print_timing(&dictation);
        eprintln!(
            "{}",
            where_line(session.cloud_provider(), session.cleanup_destination())
        );
    }
    if let Some(reason) = cloud_withheld(&dictation) {
        report_faults(&dictation);
        // What the on-device engine heard is kept, as the app keeps it.
        if let Err(e) = remember(&mut history, &dictation, session.engine_id_for(&dictation)) {
            eprintln!("  ! history: {e:#}");
        }
        anyhow::bail!(reason);
    }
    deliver(&mut dictation, opts, session.cleanup_enhancing_with())?;
    remember(&mut history, &dictation, session.engine_id_for(&dictation))?;
    // Waited for only to say so: the text is already out.
    if let Some(late) = dictation.late_cleanup.take() {
        if cli.debug {
            eprintln!("{}", late_cleanup_line(&late.wait()));
        }
    }
    if cli.debug {
        // One dictation is not a distribution, and the report says so.
        let mut all = Latencies::new();
        all.record(dictation.timing);
        eprint!("{}", all.report());
    }
    Ok(())
}

/// What the hotkey loop does with a key event that arrives once the
/// recording is over, and whether it discards this dictation. Esc twice is
/// this dictation's. A press is the next dictation: held, with everything
/// behind it, for when this one is over. A key coming up before one is this
/// dictation's own release, and goes. Dropping that press lost the whole of
/// the next dictation.
///
/// A press made before Esc twice goes with the discarded dictation, and one
/// made after it starts the next, as the shortcut's `Recogniser` has it: a
/// Discard clears what is held.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn after_release(
    action: macalla_platform::hotkey::Action,
    held: &mut std::collections::VecDeque<macalla_platform::hotkey::Action>,
) -> bool {
    use macalla_platform::hotkey::Action;
    match action {
        Action::Discard => {
            held.clear();
            true
        }
        Action::Start => {
            held.push_back(action);
            false
        }
        Action::Stop => {
            if !held.is_empty() {
                held.push_back(action);
            }
            false
        }
    }
}

/// The end of a hotkey dictation, in the order it has to happen: tell the
/// shortcut first, which says whether it heard Esc twice, then take the last
/// look at the listener. The tap sends while it holds the recogniser, so a
/// Discard sent as the shortcut was told is in the listener by the look.
/// Returns whether the dictation was discarded.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn end_dictation(
    stop_listening: impl FnOnce() -> bool,
    mut last_look: impl FnMut() -> bool,
) -> bool {
    let esc_fired = stop_listening();
    last_look() || esc_fired
}

/// Push-to-talk. Waits for the shortcut, dictates, delivers, waits again.
fn listen_on_hotkey(
    cli: &Cli,
    language: &str,
    opts: ListenOptions<'_>,
    shortcut: Option<&str>,
    mode: TriggerMode,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Only the macOS hotkey loop uses these; imported here so the Linux
        // and Windows builds have no unused import under -D warnings.
        use macalla_platform::hotkey::{Action, Listener, Mode, Shortcut};
        use std::cell::{Cell, RefCell};
        use std::collections::VecDeque;

        let shortcut = match shortcut {
            Some(text) => Shortcut::parse(text)?,
            None => Shortcut::default_for_platform(),
        };
        let mode = match mode {
            TriggerMode::Hold => Mode::Hold,
            TriggerMode::Toggle => Mode::Toggle,
        };

        let mut session = Session::open(listen_config(cli, language, opts))?;
        // Opened once for the session, like the microphone: not per
        // dictation. A bad path fails here, before anyone has spoken.
        let mut history = open_history(opts)?;
        // Check BEFORE starting the listener. rdev creates a ListenOnly event
        // tap, which macOS gates on Input Monitoring -- and after a denial it
        // never prompts again, so without this the app sits there looking
        // ready while every keypress goes nowhere.
        match macalla_platform::Permission::InputMonitoring.granted() {
            macalla_platform::Grant::Denied => anyhow::bail!(
                "Input Monitoring is denied, so the shortcut cannot see your keys. \
                 macOS will not ask again -- grant it in System Settings > {} \
                 (the entry is your TERMINAL, not macalla, because an unsigned \
                  CLI has no identity of its own), then restart the terminal.",
                macalla_platform::Permission::InputMonitoring.settings_pane()
            ),
            macalla_platform::Grant::Unknown => eprintln!(
                "note: Input Monitoring has not been granted yet. macOS should \
                 prompt on the first keypress; if no dialog appears, check \
                 System Settings > {}.",
                macalla_platform::Permission::InputMonitoring.settings_pane()
            ),
            _ => {}
        }
        if opts.paste
            && macalla_platform::Permission::Accessibility.granted()
                != macalla_platform::Grant::Granted
        {
            eprintln!(
                "note: --paste needs Accessibility and it is not granted. \
                 Nothing will be pasted and your clipboard will be left \
                 alone; the dictation is printed instead. System \
                 Settings > {}.",
                macalla_platform::Permission::Accessibility.settings_pane()
            );
        }

        // The failure that is indistinguishable from a denied permission and
        // has the opposite fix. Only a chord with an ordinary key is affected;
        // the default is a bare modifier and is immune, which is
        // why this is a warning and the app carries on.
        if macalla_platform::secure_input_enabled() == Some(true)
            && !shortcut.survives_secure_input()
        {
            eprintln!(
                "warning: something has Secure Keyboard Entry on -- a terminal, or a \
                 password field -- and while it is on, ordinary keys never reach \
                 Macalla. {shortcut} will not fire. Turn it off in that app, or use a \
                 bare modifier: --shortcut \"{}\"",
                Shortcut::default_for_platform().to_string().to_lowercase()
            );
        }

        // Accumulated across every dictation this session, because one
        // reading answers nothing: real dictations here have ranged 78 to
        // 267 ms against a 300 ms budget.
        let mut measured = Latencies::new();
        let listener = Listener::start(shortcut.clone(), mode);
        // Tells the recogniser a dictation is running for as long as it is,
        // including a cloud wait after the key is up, so Esc twice reaches it.
        let dictation_state = listener.dictation_state();

        // Name the shortcut. "hold the shortcut to dictate" without saying
        // WHICH leaves someone guessing at the one thing they have to get
        // right, and a wrong guess is indistinguishable from a broken listener.
        eprintln!(
            "{} {shortcut} to dictate  [{}, {}]",
            match mode {
                Mode::Hold => "hold",
                Mode::Toggle => "press",
            },
            session.engine_id(),
            session.dictionary_label()
        );
        eprintln!("Ctrl-C to stop waiting. macOS needs Input Monitoring for this.");

        // Arm the microphone before waiting for a key, and again after every
        // dictation, so the cost of building the stream never sits on the
        // keypress. See Session::arm for why this is allowed.
        session.arm();
        // Key events the last dictation held back for after it: a press
        // during its wait, and whatever came behind it. Read before the
        // listener.
        let held: RefCell<VecDeque<Action>> = RefCell::new(VecDeque::new());

        loop {
            // Wait for a press.
            let held_back = held.borrow_mut().pop_front();
            match held_back.map_or_else(|| listener.recv(), |action| Some(Ok(action))) {
                None => {
                    anyhow::bail!("the shortcut listener stopped");
                }
                Some(Err(e)) => return Err(e.into()),
                // A release, or a discard, we did not start on.
                Some(Ok(Action::Stop | Action::Discard)) => continue,
                Some(Ok(Action::Start)) => {}
            }
            eprintln!("listening...");

            // Recording. A Stop from the listener ends it; so does the cap.
            // Cells, because two closures share them: the one that ends the
            // recording and the one that gives up on a cloud request.
            let ended = Cell::new(false);
            let discarded = Cell::new(false);
            let failure: Cell<Option<macalla_platform::Error>> = Cell::new(None);
            // A discard is said to the session as a discard, so nothing more
            // of the recording goes to cleanup and nothing is waited for.
            let mut should_stop = || {
                if ended.get() {
                    return if discarded.get() {
                        StopSignal::Discard
                    } else {
                        StopSignal::Release
                    };
                }
                loop {
                    // What the last dictation held back comes first.
                    let held_back = held.borrow_mut().pop_front();
                    let Some(event) =
                        held_back.map_or_else(|| listener.try_recv(), |action| Some(Ok(action)))
                    else {
                        break;
                    };
                    match event {
                        Ok(Action::Stop) => {
                            ended.set(true);
                            return StopSignal::Release;
                        }
                        Ok(Action::Discard) => {
                            discarded.set(true);
                            ended.set(true);
                            return StopSignal::Discard;
                        }
                        // Already recording; a second press cannot start a
                        // second dictation.
                        Ok(Action::Start) => {}
                        Err(e) => {
                            failure.set(Some(e));
                            ended.set(true);
                            return StopSignal::Release;
                        }
                    }
                }
                StopSignal::Continue
            };
            // Once the key is up, Esc twice gives up: on a cloud request, the
            // way it discards a recording, and on the wait for cleanup. A
            // press is the next dictation, held until this one is over.
            let mut give_up = || {
                while let Some(event) = listener.try_recv() {
                    match event {
                        Ok(action) => {
                            if after_release(action, &mut held.borrow_mut()) {
                                discarded.set(true);
                            }
                        }
                        Err(e) => {
                            failure.set(Some(e));
                            return true;
                        }
                    }
                }
                discarded.get()
            };
            dictation_state.set_dictating(true);
            let dictated =
                session.dictate(&mut should_stop, &mut give_up, &mut |p| preview(p, opts));
            // Over, whichever way it ended, before anything can return. The
            // shortcut stops listening for Esc first and says whether it
            // heard Esc twice. The tap sends while it holds the recogniser,
            // so a Discard it sent is already in the listener, and the last
            // look below reads it: a discard that landed after the session
            // last asked is still this dictation's.
            if end_dictation(|| dictation_state.set_dictating(false), &mut give_up) {
                discarded.set(true);
            }
            let mut dictation = dictated?;
            if let Some(e) = failure.take() {
                return Err(e.into());
            }
            if cli.debug {
                print_timing(&dictation);
                eprintln!(
                    "{}",
                    where_line(session.cloud_provider(), session.cleanup_destination())
                );
            }
            if discarded.get() || dictation.discarded {
                // Esc twice. Nothing is pasted; the timing is still real.
                eprintln!("discarded");
            } else if let Some(reason) = cloud_withheld(&dictation) {
                // Reported, not fatal: one slow or failed request must not
                // end a push-to-talk session that may run all day. Without
                // --hotkey, `listen` exits non-zero instead. What the
                // on-device engine heard is kept, as the app keeps it.
                report_faults(&dictation);
                eprintln!("  ! {reason}");
                if let Some(line) = dictation
                    .cloud
                    .as_ref()
                    .and_then(repeated_cloud_failure_line)
                {
                    eprintln!("  ! {line}");
                }
                if let Err(e) =
                    remember(&mut history, &dictation, session.engine_id_for(&dictation))
                {
                    eprintln!("  ! history: {e:#}");
                }
            } else {
                deliver(&mut dictation, opts, session.cleanup_enhancing_with())?;
                // Reported, not fatal. The text is already in front of the
                // user, and a full disk must not end a session that may run
                // all day.
                if let Err(e) =
                    remember(&mut history, &dictation, session.engine_id_for(&dictation))
                {
                    eprintln!("  ! history: {e:#}");
                }
                // Kept, so the cleanup that missed the paste is worth waiting
                // for. Off the loop, so the next press never waits on a
                // provider.
                if cli.debug {
                    if let Some(late) = dictation.late_cleanup.take() {
                        std::thread::spawn(move || {
                            eprintln!("{}", late_cleanup_line(&late.wait()))
                        });
                    }
                }
            }
            // Late cleanup nobody asked for goes with the dictation. A remote
            // provider's request already went at key release: dropping it only
            // stops a pool before its next member, and the answer goes nowhere.
            measured.record(dictation.timing);

            // Re-arm for the next press. Doing it here rather than before the
            // next `recv` keeps the cost off the moment a key goes down even
            // when dictations come back to back.
            session.arm();
            if cli.debug {
                eprint!("{}", measured.report());
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (cli, language, opts, shortcut, mode);
        anyhow::bail!(
            "the global shortcut is macOS-only for now. It needs a desktop \
             session, and the CLI stays headless on {} so it runs on a server \
             with no display.",
            std::env::consts::OS
        )
    }
}

fn doctor(cli: &Cli, language: &str) -> Result<()> {
    println!("macalla {}", env!("CARGO_PKG_VERSION"));
    println!(
        "  platform:       {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("  default engine: {}", macalla_engines::default_engine_id());
    println!("  block schema:   v{}", macalla_core::block::SCHEMA_VERSION);

    println!("  engines:");
    for e in macalla_engines::registry::engines() {
        use macalla_engines::registry::Availability;
        let mark = if e.id == macalla_engines::default_engine_id() {
            " (default here)"
        } else {
            ""
        };
        match e.availability {
            Availability::Ready => {
                println!("    {} -- {}{mark}: ready", e.id, e.name);
                println!("      languages:    {}", join(&e.languages));
                if !e.downloadable.is_empty() {
                    println!("      downloadable: {}", join(&e.downloadable));
                }
            }
            Availability::NotCompiledIn => {
                println!("    {} -- {}{mark}: not in this build", e.id, e.name);
            }
            Availability::NeedsDownload(what) => {
                println!(
                    "    {} -- {}{mark}: needs a download ({what}); run `macalla models download {}`",
                    e.id, e.name, e.id
                );
            }
            Availability::Unavailable(reason) => {
                println!("    {} -- {}{mark}: unavailable, {reason}", e.id, e.name);
                if !e.downloadable.is_empty() {
                    println!("      downloadable: {}", join(&e.downloadable));
                }
            }
        }
    }

    println!(
        "  ffmpeg:         {}",
        if macalla_platform::decode::ffmpeg_available() {
            "found -- transcribe takes any format it knows"
        } else {
            "not on the PATH -- transcribe takes wav and aiff (and the system decoder on macOS)"
        }
    );

    match input_device_names() {
        Ok(names) if !names.is_empty() => println!("  input devices:  {}", join(&names)),
        Ok(_) => println!("  input devices:  none found"),
        Err(e) => println!("  input devices:  unavailable ({e})"),
    }
    if let Some(on) = macalla_platform::secure_input_enabled() {
        println!(
            "  secure input:   {}",
            if on {
                "ON -- only a bare-modifier shortcut can reach Macalla"
            } else {
                "off"
            }
        );
    }

    let (_, label) = macalla_session::load_dictionary(
        cli.dictionary.as_deref(),
        macalla_session::DICTIONARY_CANDIDATES,
    )?;
    println!("  dictionary:     {label}");

    // What the pipeline would actually do, rather than what the docs claim.
    match Session::open(config(cli, language, None, None)) {
        Ok(session) => {
            let plan = session.plan();
            println!("  pipeline for {}:", session.engine_id());
            println!("    vad segments: {}", plan.vad_segments);
            println!("    dictionary:   {}", plan.dictionary);
            println!(
                "    tier 0:       fillers {} structure {}",
                plan.tier0_fillers, plan.tier0_structure
            );
            println!(
                "    tier 1:       {}",
                tier1_line(plan, session.engine_id())
            );
        }
        Err(e) => println!("  pipeline:       unavailable ({e})"),
    }

    println!("  permissions:");
    for p in macalla_platform::Permission::required_here() {
        use macalla_platform::Grant;
        let state = match p.granted() {
            Grant::Granted => "granted".to_string(),
            Grant::Denied => format!("DENIED -- fix in System Settings > {}", p.settings_pane()),
            Grant::Unknown => "not asked yet".to_string(),
            Grant::NotRequired => "not needed here".to_string(),
        };
        println!("    - {p:?}: {} [{state}]", p.reason());
    }
    Ok(())
}

fn bench() -> Result<()> {
    println!("The benchmark suites live in bench/ and are driven by bench/run_all.sh.");
    println!("Results and their method are in docs/benchmarks.md.");
    println!();
    println!("Engine and pipeline timings are measured by the ignored tests:");
    println!("  cargo test -p macalla-engines --features engine-apple -- --ignored --nocapture");
    println!("  cargo test -p macalla-core --lib -- --ignored --nocapture");
    Ok(())
}

// ---------------------------------------------------------------- helpers

fn decode(path: &Path) -> Result<Vec<f32>> {
    Ok(macalla_session::decode_file(path)?)
}

/// Print the result.
///
/// In JSON, `text` and `markdown` are the finished document but `segments`
/// carry what the ENGINE produced, uncorrected. That is deliberate: the raw
/// form is what "add to dictionary" needs to prefill, since the user has to
/// recognise what was misheard.
fn emit(document: &Document, transcripts: &[Transcript], json: bool) -> Result<()> {
    if !json {
        let text = document.to_plain();
        if text.trim().is_empty() {
            eprintln!("no speech was detected");
        } else {
            println!("{text}");
        }
        return Ok(());
    }
    println!("{}", json_payload(document, transcripts)?);
    Ok(())
}

/// The JSON [`emit`] prints, and `transcribe --out-dir --json` writes.
fn json_payload(document: &Document, transcripts: &[Transcript]) -> Result<String> {
    let payload = serde_json::json!({
        "text": document.to_plain(),
        "markdown": document.to_markdown(),
        "schema_version": document.schema_version,
        "segments": transcripts
            .iter()
            .filter(|t| t.is_final)
            .map(|t| serde_json::json!({
                "text": t.text,
                "start_ms": t.start_ms,
                "end_ms": t.end_ms,
            }))
            .collect::<Vec<_>>(),
    });
    Ok(serde_json::to_string_pretty(&payload)?)
}

fn join(items: &[String]) -> String {
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

/// The provider flags of `macalla summarise`, as given.
struct SummaryFlags<'a> {
    endpoint: Option<&'a str>,
    anthropic: bool,
    model: Option<&'a str>,
    key_env: Option<&'a str>,
    key_provider: Option<&'a str>,
    pool: &'a [String],
}

/// The cleanup choices the summary flags name: the provider this summary
/// goes to, as the command line has no saved cleanup choice to read.
///
/// A pool is read through the same mapping the app uses for a saved pool, so
/// the two cannot drift apart. A `--pool` entry splits at its first colon, so
/// a fine-tuned model's id keeps its own.
fn summary_choices(flags: SummaryFlags<'_>) -> Result<Vec<choice::EnhancerChoice>> {
    if !flags.pool.is_empty() {
        let models = flags
            .pool
            .iter()
            .map(|entry| {
                let (provider, model) = entry.split_once(':').with_context(|| {
                    format!(
                        "--pool {entry:?} is not provider:model, for example openai:gpt-5.6-luna"
                    )
                })?;
                Ok(choice::PoolModel {
                    provider: provider.to_string(),
                    model: model.to_string(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let pool = choice::CleanupChoice::Pool { models };
        choice::validate_cleanup_choice(&pool)?;
        return Ok(choice::cleanup_enhancers(&pool));
    }
    match (flags.anthropic, flags.endpoint, flags.model) {
        (true, _, Some(model)) => Ok(vec![choice::EnhancerChoice::Anthropic {
            model: model.to_string(),
            // Anthropic's key is always the `anthropic` provider's, stored or
            // overridden with --key-env.
            key: match flags.key_env {
                Some(name) => choice::KeySource::Env(name.to_string()),
                None => choice::KeySource::Stored("anthropic".to_string()),
            },
            reasoning: None,
        }]),
        (false, Some(endpoint), Some(model)) => Ok(vec![choice::EnhancerChoice::Endpoint {
            url: endpoint.to_string(),
            model: model.to_string(),
            key: match (flags.key_env, flags.key_provider) {
                (Some(name), _) => choice::KeySource::Env(name.to_string()),
                (None, Some(id)) => choice::KeySource::Stored(id.to_string()),
                (None, None) => choice::KeySource::None,
            },
            reasoning: None,
        }]),
        _ if flags.model.is_some() || flags.key_env.is_some() || flags.key_provider.is_some() => {
            anyhow::bail!("--model, --key-env and --key-provider go with --endpoint or --anthropic")
        }
        _ if cfg!(target_os = "macos") => Ok(choice::cleanup_enhancers(
            &choice::CleanupChoice::AppleIntelligence,
        )),
        _ => anyhow::bail!(
            "say where the transcript goes: --endpoint and --model, --anthropic and --model, \
             or --pool provider:model"
        ),
    }
}

/// `macalla summarise`: read the file, say where it goes, summarise it.
///
/// Where the transcript goes is printed before anything is sent. The command
/// fails when no part could be summarised; parts that failed beside others
/// that did not are printed with their reason.
fn summarise_file(path: &Path, choices: &[choice::EnhancerChoice], json: bool) -> Result<()> {
    use macalla_session::summary::{read_transcript, Summariser};

    let transcript = read_transcript(path)?;
    eprintln!(
        "{}: {} turn(s), {}",
        path.display(),
        transcript.turns.len(),
        if transcript.named_speakers {
            "with speaker names"
        } else {
            "with no speaker names, so no action item gets an owner"
        }
    );
    if transcript.skipped_blocks > 0 {
        eprintln!(
            "{} block(s) of the file could not be read as a cue and were skipped",
            transcript.skipped_blocks
        );
    }
    let summariser = Summariser::build(choices, &SystemSecrets::new())?;
    eprintln!(
        "summary goes to: {} ({})",
        summariser.destination(),
        match summariser.reach() {
            choice::CleanupReach::Local => "on this machine",
            choice::CleanupReach::Remote => "off this machine",
        }
    );
    let parts = summariser.summarise(&transcript.turns, &mut |_| {}, &AtomicBool::new(false))?;
    for line in dropped_lines(&parts) {
        tracing::debug!("summary line dropped: {line}");
    }
    print!("{}", render_summary(&parts, json));
    let failed = parts.iter().filter(|part| part.lists.is_err()).count();
    if failed == parts.len() {
        anyhow::bail!("no part of the transcript could be summarised");
    }
    if failed > 0 {
        eprintln!("{failed} of {} parts could not be summarised", parts.len());
    }
    Ok(())
}

/// The summary as it is printed: each part with its times, its three lists,
/// the reply lines that were dropped (a count in text, each line with its list
/// and reason in JSON), or why the part has no lists.
fn render_summary(parts: &[macalla_core::meeting_summary::PartSummary], json: bool) -> String {
    use std::fmt::Write as _;

    if json {
        let points = |points: &[macalla_core::meeting_summary::Point]| {
            serde_json::Value::Array(
                points
                    .iter()
                    .map(|point| serde_json::json!({"text": point.text, "quote": point.quote}))
                    .collect(),
            )
        };
        let parts: Vec<serde_json::Value> = parts
            .iter()
            .map(|part| {
                let mut row = serde_json::json!({"start_ms": part.start_ms, "end_ms": part.end_ms});
                match &part.lists {
                    Ok(lists) => {
                        row["key_points"] = points(&lists.key_points);
                        row["decisions"] = points(&lists.decisions);
                        row["action_items"] = serde_json::Value::Array(
                            lists
                                .action_items
                                .iter()
                                .map(|item| {
                                    serde_json::json!({
                                        "text": item.text,
                                        "quote": item.quote,
                                        "owner": item.owner,
                                    })
                                })
                                .collect(),
                        );
                        row["dropped"] = serde_json::Value::Array(
                            lists
                                .dropped
                                .iter()
                                .map(|item| {
                                    serde_json::json!({
                                        "section": item.section.map(|section| section.name()),
                                        "line": item.line,
                                        "reason": item.cause.name(),
                                    })
                                })
                                .collect(),
                        );
                        row["assumed_empty"] = serde_json::json!(lists.assumed_empty);
                    }
                    Err(error) => row["error"] = serde_json::json!(error.to_string()),
                }
                row
            })
            .collect();
        return format!("{}\n", serde_json::json!({ "parts": parts }));
    }

    fn list(out: &mut String, heading: &str, items: Vec<String>) {
        let _ = writeln!(out, "{heading}");
        if items.is_empty() {
            let _ = writeln!(out, "- none");
        }
        for item in items {
            let _ = writeln!(out, "- {item}");
        }
    }

    let mut out = String::new();
    for (at, part) in parts.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "Part {}, {} to {}",
            at + 1,
            clock(part.start_ms),
            clock(part.end_ms)
        );
        match &part.lists {
            Ok(lists) => {
                let quoted = |text: &str, quote: &str| format!("{text} \"{quote}\"");
                list(
                    &mut out,
                    "Key points",
                    lists
                        .key_points
                        .iter()
                        .map(|p| quoted(&p.text, &p.quote))
                        .collect(),
                );
                list(
                    &mut out,
                    "Decisions",
                    lists
                        .decisions
                        .iter()
                        .map(|p| quoted(&p.text, &p.quote))
                        .collect(),
                );
                list(
                    &mut out,
                    "Action items",
                    lists
                        .action_items
                        .iter()
                        .map(|item| match &item.owner {
                            Some(owner) => format!("{owner}: {}", quoted(&item.text, &item.quote)),
                            None => quoted(&item.text, &item.quote),
                        })
                        .collect(),
                );
                match lists.dropped.len() {
                    0 => {}
                    1 => {
                        let _ = writeln!(
                            out,
                            "1 line dropped: it did not quote this part, or sat outside the lists"
                        );
                    }
                    n => {
                        let _ = writeln!(
                            out,
                            "{n} lines dropped: they did not quote this part, or sat outside the lists"
                        );
                    }
                }
            }
            Err(error) => {
                let _ = writeln!(out, "Could not summarise this part: {error}");
            }
        }
    }
    out
}

/// Every dropped line of every part, one sentence each, for `--debug`. The
/// lines are the model's reply about the person's own transcript, and stay on
/// this machine as the summary does.
fn dropped_lines(parts: &[macalla_core::meeting_summary::PartSummary]) -> Vec<String> {
    parts
        .iter()
        .enumerate()
        .filter_map(|(at, part)| part.lists.as_ref().ok().map(|lists| (at, lists)))
        .flat_map(|(at, lists)| {
            lists.dropped.iter().map(move |item| {
                let section = item
                    .section
                    .map_or("before the lists".to_string(), |section| {
                        section.name().replace('_', " ")
                    });
                format!(
                    "part {}, {section}: {}: {}",
                    at + 1,
                    item.cause.explain(),
                    item.line
                )
            })
        })
        .collect()
}

/// `macalla live-meeting`: the microphone as a live meeting, until Enter,
/// Ctrl-C or SIGTERM ([`stop_signals`]).
///
/// The session's own live meeting, microphone only: no Others source is
/// opened, so the turns carry no speaker, as a recording heard from a file.
/// A second Ctrl-C throws it away. The meeting is held in memory until it
/// is printed. Until it starts recording, Ctrl-C ends the process as it
/// always has: nothing has been heard yet.
fn live_meeting(cli: &Cli, language: &str, device: Option<&str>, json: bool) -> Result<()> {
    use macalla_session::live_meeting::{self, Ending, LiveMeeting};

    let session = Session::open(config(cli, language, device, None))?;
    let tools = session.meeting_tools()?;
    eprintln!("{}", words::LIVE_MICROPHONE_ONLY);
    let you = live_meeting::open_microphone(device)?;
    let prepared = live_meeting::prepare(&tools, you, None).map_err(live_failure)?;

    let ending = Arc::new(Ending::new());
    {
        // A blocking read on its own thread, as `listen`'s: EOF is not Enter,
        // so a closed stdin never ends the meeting by itself.
        let ending = Arc::clone(&ending);
        std::thread::spawn(move || {
            let mut line = String::new();
            if matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
                ending.stop();
            }
        });
    }
    stop_signals::install();
    let recorded = AtomicBool::new(false);
    eprintln!("{}", words::LIVE_RECORDING);
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !recorded.load(Ordering::Relaxed) {
                follow(stop_signals::asked(), &ending);
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        // Set as this ends, whether `record` returned or panicked, so the
        // poller above always stops and the scope can end.
        let _recorded = SetOnDrop(&recorded);
        prepared.record(&mut |_| {}, &ending)
    });
    let LiveMeeting {
        meeting,
        ended_early,
    } = result.map_err(live_failure)?;
    print!("{}", render_turns(&meeting, ended_early.as_ref(), json)?);
    if let Some(error) = &ended_early {
        tracing::debug!(%error, "the live meeting stopped early");
        eprintln!("{}", words::live_ended_early(error));
    }
    Ok(())
}

/// Sets its flag when dropped, a panic's unwinding included.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Do what the signals ask of the meeting. Asked again every poll; a stop
/// or a discard asked twice is the same request.
fn follow(asked: stop_signals::Asked, ending: &macalla_session::live_meeting::Ending) {
    match asked {
        stop_signals::Asked::Nothing => {}
        stop_signals::Asked::Stop => ending.stop(),
        stop_signals::Asked::Discard => ending.cancel(),
    }
}

/// A live meeting that kept nothing, in the CLI's own words where it has
/// them, and the error as it is otherwise.
fn live_failure(error: macalla_session::Error) -> anyhow::Error {
    match words::live_not_kept(&error) {
        Some(said) => {
            tracing::debug!(%error, "the live meeting kept nothing");
            anyhow::anyhow!(said)
        }
        None => error.into(),
    }
}

/// A meeting's turns as they are printed: one paragraph each, with the
/// speaker's name in front when it has one, or as JSON with their times.
/// `ended_early` is in the JSON as the error's own text, for a program; a
/// person reads the sentence on stderr.
fn render_turns(
    meeting: &macalla_session::meeting::MeetingTranscript,
    ended_early: Option<&macalla_session::Error>,
    json: bool,
) -> Result<String> {
    if json {
        let payload = serde_json::json!({
            "turns": meeting
                .turns
                .iter()
                .map(|turn| serde_json::json!({
                    "speaker": turn.speaker,
                    "start_ms": turn.start_ms,
                    "end_ms": turn.end_ms,
                    "text": turn.text,
                }))
                .collect::<Vec<_>>(),
            "named_speakers": meeting.named_speakers,
            "audio_ms": meeting.audio_ms,
            "ended_early": ended_early.map(ToString::to_string),
        });
        return Ok(format!("{}\n", serde_json::to_string_pretty(&payload)?));
    }
    let text = meeting
        .turns
        .iter()
        .map(|turn| match &turn.speaker {
            Some(name) => format!("{name}: {}", turn.text),
            None => turn.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(format!("{text}\n"))
}

/// A time in a meeting, as minutes and seconds, with hours once there are any.
fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use macalla_session::CloudFailure;

    /// An engine that hears "we ship on friday" in any recording of a second
    /// or more, and nothing in a shorter one, counting how many of it were
    /// made and how many files it heard.
    struct Counted {
        heard: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl macalla_core::stt::SttEngine for Counted {
        fn id(&self) -> &'static str {
            "counted"
        }
        fn caps(&self) -> macalla_core::stt::EngineCaps {
            macalla_core::stt::EngineCaps {
                owns_segmentation: true,
                ..macalla_core::stt::EngineCaps::batch()
            }
        }
        fn stream(
            &self,
            _ctx: &macalla_core::stt::SttContext,
        ) -> macalla_core::Result<Box<dyn macalla_core::stt::SttStream>> {
            Err(macalla_core::Error::Unsupported {
                engine: "counted",
                what: "streaming",
            })
        }
        fn transcribe(
            &self,
            audio: &macalla_core::audio::AudioSegment,
            _ctx: &macalla_core::stt::SttContext,
        ) -> macalla_core::Result<Transcript> {
            self.heard.fetch_add(1, Ordering::SeqCst);
            let text = if audio.duration_ms() >= 1_000 {
                "we ship on friday"
            } else {
                ""
            };
            Ok(Transcript {
                text: text.into(),
                is_final: true,
                start_ms: 0,
                end_ms: audio.duration_ms(),
            })
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("macalla-cli-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `ms` of 16 kHz mono 16-bit silence.
    fn write_wav(path: &Path, ms: u32) {
        let samples = 16 * ms;
        let data_len = samples * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&32_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(bytes.len() + data_len as usize, 0);
        std::fs::write(path, bytes).unwrap();
    }

    const PLAIN: Rendering = Rendering {
        json: false,
        raw: false,
        format: TranscribeFormat::Txt,
    };

    #[test]
    fn several_files_are_heard_on_one_session_and_written_one_per_input_never_overwriting() {
        let dir = temp_dir("several");
        let out = temp_dir("several-out");
        let (standup, retro, quiet) = (
            dir.join("standup.wav"),
            dir.join("retro.wav"),
            dir.join("quiet.wav"),
        );
        write_wav(&standup, 2_000);
        write_wav(&retro, 2_000);
        write_wav(&quiet, 500);
        std::fs::write(out.join("standup.txt"), "keep me").unwrap();
        let missing = dir.join("gone.wav");
        let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let session = Session::with_engine(
            Box::new(Counted {
                heard: Arc::clone(&heard),
            }),
            macalla_core::dictionary::Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );
        let paths = [
            standup.clone(),
            missing.clone(),
            quiet.clone(),
            retro.clone(),
            standup.clone(),
        ];
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

        let failed = transcribe_into(
            &session,
            &paths,
            &out,
            PLAIN,
            false,
            &mut stdout,
            &mut stderr,
        )
        .unwrap();

        assert_eq!(failed, 1, "the missing file, and only that one");
        assert_eq!(heard.load(Ordering::SeqCst), 4, "every file that decoded");
        let pairs: Vec<(PathBuf, PathBuf)> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(|line| {
                let (input, output) = line.split_once('\t').expect("input, a tab, output");
                (PathBuf::from(input), PathBuf::from(output))
            })
            .collect();
        assert_eq!(
            pairs,
            [
                (standup.clone(), out.join("standup-2.txt")),
                (retro, out.join("retro.txt")),
                (standup, out.join("standup-3.txt"))
            ],
            "each output beside the input it came from"
        );
        let written: Vec<PathBuf> = pairs.into_iter().map(|(_, output)| output).collect();
        assert_eq!(
            std::fs::read_to_string(out.join("standup.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            std::fs::read_to_string(&written[1]).unwrap(),
            "We ship on friday\n"
        );
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(
            stderr.contains(&words::transcribe_failed(
                &missing.display().to_string(),
                NotTranscribed::Missing
            )),
            "{stderr}"
        );
        assert!(
            !stderr.contains("does not exist"),
            "the error itself only with --debug"
        );
        assert!(
            stderr.contains(&words::transcribe_no_speech(&quiet.display().to_string())),
            "{stderr}"
        );
        assert!(!out.join("quiet.txt").exists());
    }

    #[test]
    fn several_files_as_json_are_written_as_json() {
        let dir = temp_dir("several-json");
        let out = temp_dir("several-json-out");
        let a = dir.join("a.wav");
        write_wav(&a, 2_000);
        let session = Session::with_engine(
            Box::new(Counted {
                heard: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            macalla_core::dictionary::Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );
        let json = Rendering {
            json: true,
            raw: false,
            format: TranscribeFormat::Txt,
        };
        let missing = dir.join("gone.wav");
        let (quiet, mut stdout, mut stderr) = (dir.join("quiet.wav"), Vec::new(), Vec::new());
        write_wav(&quiet, 500);

        let failed = transcribe_into(
            &session,
            &[a.clone(), missing.clone(), quiet.clone()],
            &out,
            json,
            true,
            &mut stdout,
            &mut stderr,
        )
        .unwrap();

        assert_eq!(failed, 1);
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("a.json")).unwrap()).unwrap();
        assert_eq!(written["text"], "We ship on friday");
        let lines: Vec<serde_json::Value> = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines,
            [
                serde_json::json!({
                    "input": a.display().to_string(),
                    "output": out.join("a.json").display().to_string(),
                    "error": null,
                    "no_speech": false,
                }),
                serde_json::json!({
                    "input": missing.display().to_string(),
                    "output": null,
                    "error": "missing",
                    "no_speech": false,
                }),
                serde_json::json!({
                    "input": quiet.display().to_string(),
                    "output": null,
                    "error": null,
                    "no_speech": true,
                }),
            ],
            "one line per input, failures and silence included"
        );
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("does not exist"),
            "--debug adds the error behind a failure"
        );
    }

    #[test]
    fn a_file_that_will_not_decode_says_whether_ffmpeg_is_the_reason() {
        let dir = temp_dir("undecodable");
        let out = temp_dir("undecodable-out");
        let clip = dir.join("clip.m4a");
        std::fs::write(&clip, b"not audio at all").unwrap();
        let session = Session::with_engine(
            Box::new(Counted {
                heard: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            macalla_core::dictionary::Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );
        let why =
            |ffmpeg: bool| match transcribe_one(&session, None, &clip, &out, PLAIN, &mut || ffmpeg)
            {
                Outcome::Failed(why, _) => Some(why),
                _ => None,
            };

        assert_eq!(why(false), Some(NotTranscribed::NoFfmpeg));
        assert_eq!(why(true), Some(NotTranscribed::Undecodable));
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    }

    #[test]
    fn several_files_need_somewhere_to_go_and_one_still_prints() {
        let parse =
            |args: &[&str]| Cli::try_parse_from([&["macalla", "transcribe"], args].concat());
        assert!(parse(&[]).is_err(), "a file is needed");
        let Ok(Cli {
            command: Command::Transcribe { paths, out_dir, .. },
            ..
        }) = parse(&["a.wav", "b.wav", "--out-dir", "out"])
        else {
            panic!("several files and --out-dir parse");
        };
        assert_eq!(paths, [PathBuf::from("a.wav"), PathBuf::from("b.wav")]);
        assert_eq!(out_dir, Some(PathBuf::from("out")));

        let cli = parse(&["a.wav", "b.wav"]).unwrap();
        let Command::Transcribe { paths, .. } = &cli.command else {
            unreachable!()
        };
        let refused = transcribe(&cli, "en-US", paths, None, PLAIN, None).unwrap_err();
        assert_eq!(refused.to_string(), words::TRANSCRIBE_NEEDS_OUT_DIR);
    }

    #[test]
    fn transcribe_takes_md_alone_and_plain_text_is_still_the_default() {
        let parse =
            |args: &[&str]| Cli::try_parse_from([&["macalla", "transcribe"], args].concat());
        let format_of = |args: &[&str]| match parse(args).unwrap().command {
            Command::Transcribe { format, .. } => format,
            _ => unreachable!(),
        };
        assert_eq!(format_of(&["a.wav"]), TranscribeFormat::Txt);
        assert_eq!(
            format_of(&["a.wav", "--format", "md"]),
            TranscribeFormat::Md
        );
        assert!(parse(&["a.wav", "--format", "vtt"]).is_err());

        let markdown = Rendering {
            format: TranscribeFormat::Md,
            ..PLAIN
        };
        let a = [PathBuf::from("a.wav")];
        for (json, raw) in [(true, false), (false, true)] {
            let cli = parse(&["a.wav"]).unwrap();
            let rendering = Rendering {
                json,
                raw,
                ..markdown
            };
            let refused = transcribe(&cli, "en-US", &a, None, rendering, None).unwrap_err();
            assert_eq!(refused.to_string(), words::TRANSCRIBE_MARKDOWN_ALONE);
        }
        let cli = parse(&["a.wav"]).unwrap();
        let cloud = CloudChoice {
            provider: CloudProvider::Openai,
            model: None,
            key_env: None,
        };
        let refused = transcribe(&cli, "en-US", &a, None, markdown, Some(cloud)).unwrap_err();
        assert_eq!(refused.to_string(), words::TRANSCRIBE_MARKDOWN_ALONE);
    }

    /// The CLI's Markdown is the file's stem as the title and its turns,
    /// with no summary: [`macalla_core::markdown::write`] with the CLI's
    /// words, and a turn with no speaker has no name in front.
    #[test]
    fn one_file_as_markdown_is_its_title_and_its_turns() {
        let dir = temp_dir("markdown-one");
        let (standup, quiet) = (dir.join("Team standup.wav"), dir.join("quiet.wav"));
        write_wav(&standup, 2_000);
        write_wav(&quiet, 500);
        let (session, _) = counted_session();

        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        transcribe_markdown(&session, &standup, false, &mut stdout, &mut stderr).unwrap();
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "# Team standup\n\n## Transcript\n\n(0:00) We ship on friday\n"
        );
        assert!(stderr.is_empty());

        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        transcribe_markdown(&session, &standup, true, &mut stdout, &mut stderr).unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(
            stderr.starts_with(&format!(
                "  engine {} | dictionary {} | audio 2.0s | decode ",
                session.engine_id(),
                session.dictionary_label()
            )) && stderr.contains(" ms | stt "),
            "--debug gives the plain-text path's line: {stderr}"
        );

        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        transcribe_markdown(&session, &quiet, false, &mut stdout, &mut stderr).unwrap();
        assert!(stdout.is_empty(), "no speech, no document");
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!(
                "{}\n",
                words::transcribe_no_speech(&quiet.display().to_string())
            )
        );
    }

    #[test]
    fn several_files_as_markdown_are_heard_as_meetings_and_written_as_md() {
        let dir = temp_dir("several-md");
        let out = temp_dir("several-md-out");
        let (standup, quiet) = (dir.join("standup.wav"), dir.join("quiet.wav"));
        write_wav(&standup, 2_000);
        write_wav(&quiet, 500);
        let (session, heard) = counted_session();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

        let failed = transcribe_into(
            &session,
            &[standup.clone(), quiet.clone()],
            &out,
            Rendering {
                format: TranscribeFormat::Md,
                ..PLAIN
            },
            false,
            &mut stdout,
            &mut stderr,
        )
        .unwrap();

        assert_eq!(failed, 0);
        assert_eq!(heard.load(Ordering::SeqCst), 2);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!(
                "{}\t{}\n",
                standup.display(),
                out.join("standup.md").display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(out.join("standup.md")).unwrap(),
            "# standup\n\n## Transcript\n\n(0:00) We ship on friday\n"
        );
        assert!(!out.join("quiet.md").exists());
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains(&words::transcribe_no_speech(&quiet.display().to_string())),);
    }

    #[test]
    fn a_watch_writes_markdown_named_for_each_file() {
        let dir = temp_dir("watch-md");
        let out = temp_dir("watch-md-out");
        write_wav(&dir.join("retro.wav"), 2_000);
        let (session, _) = counted_session();

        let (stdout, _) = run_watch(&session, &dir, &out, WatchFormat::Md, false, 2);

        assert!(stdout.ends_with(&format!("\t{}\n", out.join("retro.md").display())));
        assert_eq!(
            std::fs::read_to_string(out.join("retro.md")).unwrap(),
            "# retro\n\n## Transcript\n\n(0:00) We ship on friday\n"
        );
    }

    #[test]
    fn a_count_of_failures_is_a_sentence_for_one_and_another_for_more() {
        assert_eq!(
            words::transcribe_not_all(1, 3),
            "1 of 3 files was not transcribed"
        );
        assert_eq!(
            words::transcribe_not_all(2, 3),
            "2 of 3 files were not transcribed"
        );
    }

    /// A session over [`Counted`], with no dictionary, and the count of
    /// files it heard.
    fn counted_session() -> (Session, Arc<std::sync::atomic::AtomicUsize>) {
        let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let session = Session::with_engine(
            Box::new(Counted {
                heard: Arc::clone(&heard),
            }),
            macalla_core::dictionary::Dictionary::from_json("{}").unwrap(),
            "none",
            Config::default(),
        );
        (session, heard)
    }

    /// A list's clock an hour ahead, so files the test just wrote are past
    /// the settle age without waiting for it.
    fn an_hour_from_now() -> u64 {
        watch::now_ns() + 3_600 * 1_000_000_000
    }

    /// Run the watch over `dir` for `polls` lists, with no real waiting, and
    /// give back stdout and stderr.
    fn run_watch(
        session: &Session,
        dir: &Path,
        out: &Path,
        format: WatchFormat,
        json: bool,
        polls: usize,
    ) -> (String, String) {
        let tools = session.meeting_tools().unwrap();
        let tracker =
            watch::Tracker::with_done(watch::read_ledger(&out.join(watch::LEDGER)).unwrap());
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let mut left = polls;
        watch_into(
            &tools,
            dir,
            out,
            tracker,
            WatchOutput {
                format,
                json,
                debug: false,
            },
            &mut stdout,
            &mut stderr,
            &AtomicBool::new(false),
            &mut |_| {
                left -= 1;
                left > 0
            },
            &mut an_hour_from_now,
        )
        .unwrap();
        (
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn a_watch_hears_each_settled_file_once_and_one_failure_does_not_stop_the_rest() {
        let dir = temp_dir("watch");
        let out = temp_dir("watch-out");
        write_wav(&dir.join("a-standup.wav"), 2_000);
        std::fs::write(dir.join("b-broken.wav"), b"not audio at all").unwrap();
        write_wav(&dir.join("c-quiet.wav"), 500);
        write_wav(&dir.join("d-retro.wav"), 2_000);
        std::fs::write(dir.join("notes.txt"), b"not heard").unwrap();
        let (session, heard) = counted_session();

        let (stdout, stderr) = run_watch(&session, &dir, &out, WatchFormat::Vtt, false, 3);

        assert_eq!(
            heard.load(Ordering::SeqCst),
            3,
            "each file that decoded, once, however many lists saw it"
        );
        assert_eq!(
            stdout.lines().collect::<Vec<_>>(),
            [
                format!(
                    "{}\t{}",
                    dir.join("a-standup.wav").display(),
                    out.join("a-standup.vtt").display()
                ),
                format!(
                    "{}\t{}",
                    dir.join("d-retro.wav").display(),
                    out.join("d-retro.vtt").display()
                ),
            ]
        );
        let vtt = std::fs::read_to_string(out.join("a-standup.vtt")).unwrap();
        assert!(vtt.starts_with("WEBVTT\n"), "{vtt}");
        assert!(
            vtt.contains("00:00:00.000 --> 00:00:02.000"),
            "timed turns: {vtt}"
        );
        assert!(
            stderr.contains(&words::transcribe_failed(
                &dir.join("b-broken.wav").display().to_string(),
                NotTranscribed::Undecodable
            )) || stderr.contains(&words::transcribe_failed(
                &dir.join("b-broken.wav").display().to_string(),
                NotTranscribed::NoFfmpeg
            )),
            "{stderr}"
        );
        assert!(stderr.contains(&words::transcribe_no_speech(
            &dir.join("c-quiet.wav").display().to_string()
        )));
        assert!(stderr.ends_with(&format!("{}\n", words::WATCH_STOPPED)));
        assert!(
            stderr.starts_with(&format!("{}\n", words::watch_waiting(4))),
            "the four audio files, said once: {stderr}"
        );
        assert_eq!(stderr.matches("will be transcribed").count(), 1);
        let noted: Vec<String> = watch::read_ledger(&out.join(watch::LEDGER))
            .unwrap()
            .into_iter()
            .map(|seen| seen.name)
            .collect();
        assert_eq!(
            noted,
            ["a-standup.wav", "c-quiet.wav", "d-retro.wav"],
            "done means written, or heard with no speech; never failed"
        );
    }

    #[test]
    fn a_restarted_watch_skips_what_is_done_and_takes_a_changed_file_into_a_new_file() {
        let dir = temp_dir("watch-restart");
        let out = temp_dir("watch-restart-out");
        write_wav(&dir.join("standup.wav"), 2_000);
        write_wav(&dir.join("retro.wav"), 2_000);
        let (session, heard) = counted_session();
        run_watch(&session, &dir, &out, WatchFormat::Txt, false, 2);
        assert_eq!(heard.load(Ordering::SeqCst), 2);

        let (stdout, _) = run_watch(&session, &dir, &out, WatchFormat::Txt, false, 3);
        assert_eq!(heard.load(Ordering::SeqCst), 2, "nothing heard again");
        assert!(stdout.is_empty(), "{stdout}");

        write_wav(&dir.join("standup.wav"), 3_000);
        let (stdout, _) = run_watch(&session, &dir, &out, WatchFormat::Txt, true, 2);
        assert_eq!(heard.load(Ordering::SeqCst), 3, "only the changed file");
        let line: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(
            line,
            serde_json::json!({
                "input": dir.join("standup.wav").display().to_string(),
                "output": out.join("standup-2.txt").display().to_string(),
                "error": null,
                "no_speech": false,
            })
        );
        assert_eq!(
            std::fs::read_to_string(out.join("standup.txt")).unwrap(),
            "We ship on friday\n",
            "the first transcript is kept"
        );
    }

    #[test]
    fn a_stopped_watch_hears_nothing_more_and_says_so() {
        let dir = temp_dir("watch-stop");
        let out = temp_dir("watch-stop-out");
        write_wav(&dir.join("standup.wav"), 2_000);
        let (session, heard) = counted_session();
        let tools = session.meeting_tools().unwrap();
        let stop = AtomicBool::new(false);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

        watch_into(
            &tools,
            &dir,
            &out,
            watch::Tracker::default(),
            WatchOutput {
                format: WatchFormat::Txt,
                json: false,
                debug: false,
            },
            &mut stdout,
            &mut stderr,
            &stop,
            &mut |stop| {
                // Ctrl-C while waiting for the next list.
                stop.store(true, Ordering::Relaxed);
                wait_unless_stopped(Duration::from_secs(60), stop)
            },
            &mut an_hour_from_now,
        )
        .unwrap();

        assert_eq!(heard.load(Ordering::SeqCst), 0);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n{}\n", words::watch_waiting(1), words::WATCH_STOPPED)
        );
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    }

    #[test]
    fn a_file_stopped_while_heard_writes_nothing() {
        let dir = temp_dir("watch-stop-during");
        let out = temp_dir("watch-stop-during-out");
        let path = dir.join("standup.wav");
        write_wav(&path, 2_000);
        let (session, _) = counted_session();
        let tools = session.meeting_tools().unwrap();

        let outcome = hear_one(
            &tools,
            &path,
            &out,
            WatchFormat::Txt,
            &AtomicBool::new(true),
            &mut || true,
        );

        assert!(outcome.is_none());
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    }

    #[test]
    fn watch_needs_an_out_dir_of_its_own_and_takes_txt_vtt_srt_and_md() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["macalla", "watch"], args].concat());
        assert!(parse(&["in"]).is_err(), "--out-dir is needed");
        assert!(parse(&["in", "--out-dir", "out", "--format", "docx"]).is_err());
        let Ok(Cli {
            command: Command::Watch { format, json, .. },
            ..
        }) = parse(&["in", "--out-dir", "out"])
        else {
            panic!("a folder and --out-dir parse");
        };
        assert_eq!((format, json), (WatchFormat::Txt, false));
        for (flag, want) in [
            ("txt", WatchFormat::Txt),
            ("vtt", WatchFormat::Vtt),
            ("srt", WatchFormat::Srt),
            ("md", WatchFormat::Md),
        ] {
            let Ok(Cli {
                command: Command::Watch { format, .. },
                ..
            }) = parse(&["in", "--out-dir", "out", "--format", flag])
            else {
                panic!("{flag} parses");
            };
            assert_eq!(format, want);
        }
    }

    #[test]
    fn the_checks_before_any_engine_refuse_one_folder_and_a_folder_that_is_not_there() {
        let dir = temp_dir("watch-same");
        let refused = watch_preflight(&dir, &dir).unwrap_err();
        assert_eq!(refused.to_string(), words::WATCH_SAME_FOLDER);
        let through_a_dot = dir.join(".");
        let refused = watch_preflight(&dir, &through_a_dot).unwrap_err();
        assert_eq!(
            refused.to_string(),
            words::WATCH_SAME_FOLDER,
            "one folder however it is spelled"
        );

        let missing = dir.join("nowhere");
        let refused = watch_preflight(&missing, &dir).unwrap_err();
        assert_eq!(
            refused.to_string(),
            words::watch_not_a_folder(&missing.display().to_string())
        );

        let out = dir.join("made").join("here");
        assert!(watch_preflight(&dir, &out).unwrap().is_empty());
        assert!(out.is_dir(), "--out-dir is made");

        let a_file = dir.join("a-file");
        std::fs::write(&a_file, b"x").unwrap();
        let refused = watch_preflight(&dir, &a_file.join("out")).unwrap_err();
        assert_eq!(
            refused.to_string(),
            words::watch_out_dir_not_made(&a_file.join("out").display().to_string())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_that_is_not_utf8_is_said_once_however_many_lists_see_it() {
        use std::os::unix::ffi::OsStringExt;
        let dir = temp_dir("watch-not-utf8");
        let out = temp_dir("watch-not-utf8-out");
        let name = std::ffi::OsString::from_vec(b"call-\xff.m4a".to_vec());
        std::fs::write(dir.join(&name), b"x").unwrap();
        let (session, heard) = counted_session();

        let (stdout, stderr) = run_watch(&session, &dir, &out, WatchFormat::Txt, false, 3);

        let said = words::watch_name_not_utf8(&dir.join(&name).display().to_string());
        assert_eq!(stderr.matches(&said).count(), 1, "{stderr}");
        assert_eq!(heard.load(Ordering::SeqCst), 0);
        assert!(stdout.is_empty(), "{stdout}");
    }

    #[test]
    fn a_list_that_fails_now_and_then_does_not_end_the_watch_but_several_in_a_row_do() {
        let dir = temp_dir("watch-lists");
        let out = temp_dir("watch-lists-out");
        let (session, heard) = counted_session();
        let tools = session.meeting_tools().unwrap();
        let run = |wait: &mut dyn FnMut(&AtomicBool) -> bool| {
            let mut stderr = Vec::new();
            let result = watch_into(
                &tools,
                &dir,
                &out,
                watch::Tracker::default(),
                WatchOutput {
                    format: WatchFormat::Txt,
                    json: false,
                    debug: false,
                },
                &mut Vec::new(),
                &mut stderr,
                &AtomicBool::new(false),
                wait,
                &mut an_hour_from_now,
            );
            (result, String::from_utf8(stderr).unwrap())
        };

        // The folder is gone for fewer lists than the limit, then back with
        // a recording in it: the watch carries on and takes it.
        std::fs::remove_dir_all(&dir).unwrap();
        let mut waits = 0;
        let (result, _) = run(&mut |_| {
            waits += 1;
            if waits == watch::LIST_ATTEMPTS - 1 {
                std::fs::create_dir_all(&dir).unwrap();
                write_wav(&dir.join("standup.wav"), 2_000);
            }
            waits < watch::LIST_ATTEMPTS + 2
        });
        result.unwrap();
        assert_eq!(heard.load(Ordering::SeqCst), 1);

        // Gone for good: the watch stops once, with the sentence.
        std::fs::remove_dir_all(&dir).unwrap();
        let mut waits = 0;
        let (result, stderr) = run(&mut |_| {
            waits += 1;
            true
        });
        assert_eq!(
            result.unwrap_err().to_string(),
            words::watch_folder_gone(&dir.display().to_string(), watch::LIST_ATTEMPTS)
        );
        assert_eq!(waits, watch::LIST_ATTEMPTS - 1);
        assert!(!stderr.contains(words::WATCH_STOPPED), "{stderr}");
    }

    #[test]
    fn the_count_waiting_at_the_start_is_a_whole_sentence_for_none_one_and_more() {
        assert_eq!(words::watch_waiting(0), words::WATCH_WAITING_NONE);
        assert_eq!(words::watch_waiting(1), words::WATCH_WAITING_ONE);
        assert_eq!(
            words::watch_waiting(3),
            "3 files already in the folder will be transcribed once they are finished"
        );
    }

    #[test]
    fn the_watch_help_says_the_interval_the_note_the_extensions_and_ctrl_c() {
        let help = words::WATCH_LONG_ABOUT;
        assert!(
            help.contains(&format!("every {} seconds", watch::POLL.as_secs())),
            "{help}"
        );
        assert!(help.contains(watch::LEDGER));
        assert!(
            help.contains(&format!("at least {} seconds", watch::SETTLE.as_secs())),
            "{help}"
        );
        assert!(
            help.contains(&format!("{} times in a row", watch::LIST_ATTEMPTS)),
            "{help}"
        );
        for extension in watch::AUDIO_EXTENSIONS {
            assert!(help.contains(&format!(".{extension}")), "{extension}");
        }
        assert!(help.contains("Ctrl-C"));
        assert!(!help.contains("  "), "{help:?}");
    }

    #[test]
    fn the_argument_parser_is_well_formed() {
        // clap's own check: duplicate flags, bad defaults, conflicting names.
        Cli::command().debug_assert();
    }

    #[test]
    fn the_poller_is_told_the_meeting_ended_even_when_it_panicked() {
        let recorded = AtomicBool::new(false);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _recorded = SetOnDrop(&recorded);
            panic!("the meeting fell over");
        }));
        assert!(outcome.is_err());
        assert!(recorded.load(Ordering::Relaxed));
    }

    #[test]
    fn live_meeting_takes_json_and_a_device() {
        assert!(Cli::try_parse_from(["macalla", "live-meeting"]).is_ok());
        assert!(Cli::try_parse_from([
            "macalla",
            "live-meeting",
            "--json",
            "--device",
            "USB Microphone"
        ])
        .is_ok());
    }

    fn turn(speaker: Option<&str>, start_ms: u64, text: &str) -> macalla_core::vtt::Turn {
        macalla_core::vtt::Turn {
            speaker: speaker.map(str::to_string),
            start_ms,
            end_ms: start_ms + 1_000,
            text: text.to_string(),
        }
    }

    #[test]
    fn a_live_meetings_turns_print_as_paragraphs_with_any_speaker_in_front() {
        let alone = macalla_session::meeting::MeetingTranscript {
            turns: vec![
                turn(None, 0, "We ship on Friday."),
                turn(None, 2_000, "Agreed."),
            ],
            named_speakers: false,
            audio_ms: Some(3_000),
        };
        assert_eq!(
            render_turns(&alone, None, false).unwrap(),
            "We ship on Friday.\n\nAgreed.\n"
        );
        let named = macalla_session::meeting::MeetingTranscript {
            turns: vec![turn(Some("You"), 0, "We ship on Friday.")],
            named_speakers: true,
            audio_ms: Some(1_000),
        };
        assert_eq!(
            render_turns(&named, None, false).unwrap(),
            "You: We ship on Friday.\n"
        );
    }

    #[test]
    fn a_live_meetings_json_carries_each_turn_its_times_and_why_it_ended_early() {
        let meeting = macalla_session::meeting::MeetingTranscript {
            turns: vec![turn(None, 500, "We ship on Friday.")],
            named_speakers: false,
            audio_ms: Some(2_000),
        };
        let lost = macalla_session::Error::Platform(macalla_platform::Error::DeviceLost(
            "USB Microphone".into(),
        ));
        let printed: serde_json::Value =
            serde_json::from_str(&render_turns(&meeting, Some(&lost), true).unwrap()).unwrap();
        assert_eq!(printed["turns"][0]["start_ms"], 500);
        assert_eq!(printed["turns"][0]["speaker"], serde_json::Value::Null);
        assert_eq!(printed["audio_ms"], 2_000);
        assert_eq!(printed["named_speakers"], false);
        assert_eq!(printed["ended_early"], lost.to_string());
        let clean: serde_json::Value =
            serde_json::from_str(&render_turns(&meeting, None, true).unwrap()).unwrap();
        assert_eq!(clean["ended_early"], serde_json::Value::Null);
    }

    #[test]
    fn a_cloud_dictation_is_delivered_only_when_the_provider_transcribed_it() {
        let dictation = |cloud: Option<macalla_session::CloudResult>| Dictation {
            document: Document::new(Vec::new()),
            transcripts: Vec::new(),
            timing: macalla_core::latency::Dictation::default(),
            device: String::new(),
            device_fault: None,
            engine_fault: None,
            stream_warnings: 0,
            heard_speech: None,
            cloud,
            local_document: None,
            front_app: None,
            late_cleanup: None,
            enhancement: None,
            discarded: false,
        };
        let result = |outcome| {
            Some(macalla_session::CloudResult {
                provider: "elevenlabs".into(),
                outcome,
                ms: 3_000.0,
                failures_in_a_row: 1,
            })
        };
        assert_eq!(cloud_withheld(&dictation(None)), None, "no cloud engine");
        assert_eq!(
            cloud_withheld(&dictation(result(CloudOutcome::Transcribed))),
            None
        );
        // The real producer's shape: every cloud adapter wraps every failure
        // as `Stt`, tagged with the provider's own id.
        let failed = cloud_withheld(&dictation(result(CloudOutcome::Failed(
            CloudFailure::Transcribe(macalla_core::Error::Stt {
                engine: "elevenlabs",
                message: "HTTP 401: invalid key".into(),
            }),
        ))))
        .unwrap();
        assert_eq!(
            failed,
            "elevenlabs could not transcribe it, so nothing was printed or pasted: stt engine \
             elevenlabs: HTTP 401: invalid key"
        );
        // Never typed: the worker thread failing to start, or the answer
        // channel closing with nothing sent.
        let unanswered = cloud_withheld(&dictation(result(CloudOutcome::Failed(
            CloudFailure::Other("the request stopped without an answer".into()),
        ))))
        .unwrap();
        assert_eq!(
            unanswered,
            "elevenlabs could not transcribe it, so nothing was printed or pasted: the request \
             stopped without an answer"
        );
        for outcome in [CloudOutcome::TimedOut, CloudOutcome::Abandoned] {
            let reason = cloud_withheld(&dictation(result(outcome))).unwrap();
            assert!(reason.contains("elevenlabs"), "{reason}");
        }
        // Silence is not withheld: it is silence, said the way a local
        // session says it.
        assert_eq!(
            cloud_withheld(&dictation(result(CloudOutcome::NoSpeech))),
            None
        );
    }

    #[test]
    fn debug_says_where_transcription_and_cleanup_happen() {
        assert_eq!(
            where_line(None, None),
            "  transcription: Local | cleanup: off"
        );
        let cloud = where_line(Some("elevenlabs"), Some("Your server"));
        assert!(
            cloud.contains("transcription: ElevenLabs") && cloud.contains("cleanup: Your server"),
            "the provider's name, not its id: {cloud}"
        );
        assert!(where_line(None, Some("Local")).ends_with("cleanup: Local"));
    }

    #[test]
    fn a_late_cloud_result_for_an_app_the_person_left_says_it_was_not_pasted_and_why() {
        let line = paste_line(macalla_session::Paste::MovedOn);
        assert!(
            line.contains("not pasted") && line.contains("another app is in front"),
            "{line}"
        );
        assert_ne!(
            line,
            paste_line(macalla_session::Paste::Withheld),
            "not the same as a request that failed"
        );
    }

    #[test]
    fn a_paste_that_could_not_be_delivered_still_prints_the_dictation() {
        // The pre-flight note promises exactly this, and with --paste and
        // without --history stdout is the only place the text survives.
        // Asserted on the decision rather than by running `deliver`: that
        // builds a real inserter and would send keystrokes into whichever
        // window is focused.
        use macalla_platform::paste::{Inserted, NotDelivered};
        use macalla_session::Paste;
        let refused = Paste::Inserted {
            how: Inserted::NotDelivered(NotDelivered::NotTrustedForAccessibility),
            ms: 0.4,
        };
        assert!(printed_after_paste(refused));
        let line = paste_line(refused);
        assert!(
            line.contains("not pasted") && line.contains("Accessibility"),
            "{line}"
        );

        // Text that went in is delivered once, not twice.
        for how in [
            Inserted::PastedAndRestored,
            Inserted::PastedAndKept,
            Inserted::Typed,
            Inserted::Nothing,
        ] {
            assert!(
                !printed_after_paste(Paste::Inserted { how, ms: 161.0 }),
                "{how:?}"
            );
        }
        assert!(!printed_after_paste(Paste::MovedOn));
        assert!(!printed_after_paste(Paste::Withheld));
        assert!(!printed_after_paste(Paste::Nothing));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn repeated_cloud_failure_line_only_names_the_streak_at_the_threshold() {
        let result = |failures_in_a_row| macalla_session::CloudResult {
            provider: "elevenlabs".into(),
            outcome: CloudOutcome::Failed(CloudFailure::Other("HTTP 500".into())),
            ms: 3_000.0,
            failures_in_a_row,
        };
        assert_eq!(
            repeated_cloud_failure_line(&result(macalla_session::REPEATED_CLOUD_FAILURES - 1)),
            None,
            "short of the threshold, cloud_withheld's reason is enough"
        );
        let line = repeated_cloud_failure_line(&result(macalla_session::REPEATED_CLOUD_FAILURES))
            .expect("at the threshold, a stronger line is printed");
        assert!(
            line.contains("elevenlabs") && line.contains("macalla key set elevenlabs"),
            "{line}"
        );
    }

    #[test]
    fn the_tier1_line_never_reads_as_cleanup_being_switched_off() {
        use macalla_core::pipeline::{PipelinePlan, TextCaps};
        use macalla_core::stt::EngineCaps;
        let apple =
            PipelinePlan::for_engine(&EngineCaps::stream_first(), &TextCaps::apple_speech());
        assert_eq!(
            tier1_line(&apple, "apple-speech"),
            "no work for this engine -- the engine already punctuates and capitalises; chosen \
             cleanup runs anyway"
        );
        let unmeasured = PipelinePlan::for_engine(&EngineCaps::batch(), &TextCaps::unmeasured());
        assert_eq!(
            tier1_line(&unmeasured, "some-new-engine"),
            "has work for this engine; runs when cleanup is chosen"
        );
    }

    #[test]
    fn the_late_cleanup_line_says_how_late_against_the_budget() {
        let late = macalla_session::LateDocument {
            document: Document::new(Vec::new()),
            ms: 1_234.4,
            partial: false,
        };
        assert_eq!(
            late_cleanup_line(&late),
            "  cleanup finished late, in 1234 ms against a 5000 ms budget; the text above is \
             without it"
        );
        let cut_short = macalla_session::LateDocument {
            partial: true,
            ..late
        };
        assert_eq!(
            late_cleanup_line(&cut_short),
            "  cleanup stopped before every segment came back, after 1234 ms; the text above \
             is without it",
            "part of the cleanup is not called finished"
        );
    }

    #[test]
    fn a_press_after_release_is_held_for_the_next_dictation_with_its_release() {
        use macalla_platform::hotkey::Action;
        let mut held = std::collections::VecDeque::new();
        assert!(!after_release(Action::Stop, &mut held), "not a discard");
        assert!(held.is_empty(), "this dictation's own release goes");
        assert!(!after_release(Action::Start, &mut held));
        assert!(!after_release(Action::Stop, &mut held));
        assert_eq!(
            held,
            [Action::Start, Action::Stop],
            "the next one, in order"
        );
        assert!(
            after_release(Action::Discard, &mut held),
            "Esc twice is this dictation's"
        );
        assert!(held.is_empty(), "and takes the presses made before it");
        assert!(!after_release(Action::Start, &mut held));
        assert_eq!(held, [Action::Start], "a press after it is held");
    }

    #[test]
    fn a_hotkey_dictation_ends_by_telling_the_shortcut_before_the_last_look() {
        use macalla_platform::hotkey::{Action, Key, Mode, Recogniser, Shortcut};
        use std::cell::RefCell;
        use std::collections::VecDeque;
        let shortcut = Shortcut::default_for_platform();
        let key = shortcut.key;
        let r = RefCell::new(Recogniser::new(shortcut, Mode::Hold));
        // What the tap has sent and the loop has not read yet.
        let listener: RefCell<VecDeque<Action>> = RefCell::default();
        let mut held = VecDeque::new();

        // Released, and a press made during the wait, read and held.
        assert_eq!(r.borrow_mut().press(key), Some(Action::Start));
        r.borrow_mut().set_dictating(true);
        assert_eq!(r.borrow_mut().release(key), Some(Action::Stop));
        let press = r.borrow_mut().press(key).expect("a press during the wait");
        assert!(!after_release(press, &mut held));

        // Esc twice lands as the shortcut is told the dictation is over: the
        // tap takes the recogniser first, and sends while it holds it.
        let stop_listening = || {
            let mut r = r.borrow_mut();
            assert_eq!(r.press(Key::Esc), None);
            assert_eq!(r.release(Key::Esc), None);
            listener.borrow_mut().extend(r.press(Key::Esc));
            r.set_dictating(false)
        };
        let last_look = || {
            let mut discarded = false;
            loop {
                let Some(action) = listener.borrow_mut().pop_front() else {
                    break;
                };
                discarded |= after_release(action, &mut held);
            }
            discarded
        };
        assert!(end_dictation(stop_listening, last_look));
        assert!(
            held.is_empty(),
            "the press made before Esc twice goes with it"
        );
        assert_eq!(
            r.borrow_mut().release(key),
            None,
            "and its release starts nothing"
        );

        // The shortcut's own answer counts when the look finds nothing.
        assert!(end_dictation(|| true, || false));
        assert!(!end_dictation(|| false, || false));
    }

    #[test]
    fn the_shortcut_and_the_hotkey_loop_agree_on_presses_around_esc_twice() {
        use macalla_platform::hotkey::{Action, Key, Mode, Recogniser, Shortcut};
        let shortcut = Shortcut::default_for_platform();
        let key = shortcut.key;
        let mut r = Recogniser::new(shortcut, Mode::Hold);
        let mut held = std::collections::VecDeque::new();
        let esc_twice = |r: &mut Recogniser| {
            assert_eq!(r.press(Key::Esc), None);
            assert_eq!(r.release(Key::Esc), None);
            let discard = r.press(Key::Esc);
            assert_eq!(r.release(Key::Esc), None);
            discard.expect("Esc twice discards")
        };

        // A press during the wait, then Esc twice with it still down.
        assert_eq!(r.press(key), Some(Action::Start));
        r.set_dictating(true);
        assert_eq!(r.release(key), Some(Action::Stop));
        let press = r.press(key).expect("a press during the wait");
        assert!(!after_release(press, &mut held));
        let discard = esc_twice(&mut r);
        assert!(after_release(discard, &mut held));
        assert_eq!(r.release(key), None);
        r.set_dictating(false);
        assert!(held.is_empty(), "nothing is started");

        // Esc twice while recording, let go, and press again before the
        // loop has finished with it.
        assert_eq!(r.press(key), Some(Action::Start), "the next hold works");
        r.set_dictating(true);
        assert_eq!(esc_twice(&mut r), Action::Discard);
        assert_eq!(r.release(key), None);
        let press = r.press(key).expect("a press after Esc twice");
        assert!(!after_release(press, &mut held));
        r.set_dictating(false);
        assert_eq!(held.pop_front(), Some(Action::Start), "started next");
        r.set_dictating(true);
        assert_eq!(
            r.release(key),
            Some(Action::Stop),
            "and stopped by its release"
        );
    }

    #[test]
    fn deliver_says_no_speech_for_a_silent_cloud_dictation_and_never_prints_a_withheld_one() {
        let opts = ListenOptions {
            timeout: None,
            json: false,
            device: None,
            // Never true here: this must not reach the clipboard.
            paste: false,
            preview: false,
            history: None,
            vad: true,
        };
        let dictation = |outcome| Dictation {
            document: Document::new(Vec::new()),
            transcripts: Vec::new(),
            timing: macalla_core::latency::Dictation::default(),
            device: String::new(),
            device_fault: None,
            engine_fault: None,
            stream_warnings: 0,
            heard_speech: None,
            cloud: Some(macalla_session::CloudResult {
                provider: "elevenlabs".into(),
                outcome,
                ms: 3_000.0,
                failures_in_a_row: 1,
            }),
            local_document: None,
            front_app: None,
            late_cleanup: None,
            enhancement: None,
            discarded: false,
        };
        assert!(deliver(&mut dictation(CloudOutcome::NoSpeech), opts, None).is_ok());
        for outcome in [
            CloudOutcome::Failed(CloudFailure::Other("HTTP 500".into())),
            CloudOutcome::TimedOut,
            CloudOutcome::Abandoned,
        ] {
            let debug = format!("{outcome:?}");
            let refused = deliver(&mut dictation(outcome), opts, None)
                .expect_err("a withheld dictation is not printed either");
            assert!(
                refused.to_string().contains("elevenlabs"),
                "{debug}: {refused}"
            );
        }
    }

    #[test]
    fn an_engine_fault_in_a_cloud_dictation_is_worded_as_the_preview_stopping() {
        // The record loop's own shape (`lib.rs`'s `engine` closure): a typed
        // engine error, its `Display` embedded verbatim in the line below.
        let fault = || macalla_core::Error::Stt {
            engine: "apple-speech",
            message: "the recogniser went away".into(),
        };
        let dictation = |cloud: Option<macalla_session::CloudResult>| Dictation {
            document: Document::new(Vec::new()),
            transcripts: Vec::new(),
            timing: macalla_core::latency::Dictation::default(),
            device: String::new(),
            device_fault: None,
            engine_fault: Some(fault()),
            stream_warnings: 0,
            heard_speech: None,
            cloud,
            local_document: None,
            front_app: None,
            late_cleanup: None,
            enhancement: None,
            discarded: false,
        };
        assert_eq!(
            engine_fault_line(&dictation(None), &fault()),
            "the engine stopped partway; kept what it had: stt engine apple-speech: the \
             recogniser went away",
            "a local dictation reads as it always has"
        );
        let cloud = |outcome| {
            Some(macalla_session::CloudResult {
                provider: "elevenlabs".into(),
                outcome,
                ms: 3_000.0,
                failures_in_a_row: 1,
            })
        };
        let sent = engine_fault_line(&dictation(cloud(CloudOutcome::Transcribed)), &fault());
        assert_eq!(
            sent,
            "the live preview stopped partway; the whole recording still went to elevenlabs: stt \
             engine apple-speech: the recogniser went away"
        );
        // Nothing was sent, so nothing "still went" anywhere.
        let unsent = engine_fault_line(&dictation(cloud(CloudOutcome::NoSpeech)), &fault());
        assert_eq!(
            unsent,
            "the live preview stopped partway: stt engine apple-speech: the recogniser went away"
        );
    }

    #[test]
    fn enhance_corrects_with_the_dictionary_before_tier0_unless_it_is_left_out() {
        let dictionary = macalla_core::dictionary::Dictionary::from_json(
            r#"{"terms": [{"written": "Terragrunt", "spoken": ["terra grunt"], "case_sensitive": true, "threshold": 0.85}]}"#,
        )
        .expect("a dictionary");
        let tier0 = macalla_core::tier0::Tier0Rules::new(Some("en"));
        let plain = |blocks: Vec<macalla_core::block::Block>| Document::new(blocks).to_plain();
        let piece = "um we ran terra grunt today";
        let with = plain(tier0_blocks(&tier0, Some(&dictionary), piece));
        let without = plain(tier0_blocks(&tier0, None, piece));
        assert!(with.contains("Terragrunt"), "{with}");
        assert!(!with.contains("um"), "Tier 0 still ran: {with}");
        assert!(!without.contains("Terragrunt"), "{without}");
    }

    #[test]
    fn a_list_of_nothing_reads_as_none() {
        assert_eq!(join(&[]), "none");
        assert_eq!(
            join(&["en-US".to_string(), "de-DE".to_string()]),
            "en-US, de-DE"
        );
    }

    #[test]
    fn anthropic_needs_a_model_and_no_endpoint_but_not_a_key_variable() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["macalla", "enhance"], args].concat());
        assert!(parse(&["--anthropic", "hi"]).is_err());
        // No --key-env: the session reads the stored `anthropic` key.
        assert!(parse(&["--anthropic", "--model", "claude-haiku-4-5", "hi"]).is_ok());
        assert!(parse(&["--anthropic", "--model", "m", "--key-env", "K", "hi"]).is_ok());
        assert!(parse(&[
            "--anthropic",
            "--endpoint",
            "http://localhost:11434",
            "--model",
            "m",
            "--key-env",
            "K",
            "hi"
        ])
        .is_err());
    }

    #[test]
    fn an_endpoint_takes_either_a_key_env_or_a_key_provider_not_both() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["macalla", "enhance"], args].concat());
        assert!(parse(&[
            "--endpoint",
            "http://localhost:11434",
            "--model",
            "m",
            "--key-provider",
            "openai",
            "hi"
        ])
        .is_ok());
        assert!(parse(&[
            "--endpoint",
            "http://localhost:11434",
            "--model",
            "m",
            "--key-env",
            "K",
            "--key-provider",
            "openai",
            "hi"
        ])
        .is_err());
        // --key-provider without --endpoint makes no sense.
        assert!(parse(&["--key-provider", "openai", "hi"]).is_err());
    }

    #[test]
    fn enhance_with_no_flag_asks_for_plain_text_guided_asks_for_the_schema() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["macalla", "enhance"], args].concat());
        let guided_of = |cli: Cli| match cli.command {
            Command::Enhance { guided, .. } => guided,
            _ => panic!("expected Command::Enhance"),
        };

        // No flag: plain text, the default.
        assert!(!guided_of(parse(&["hi"]).unwrap()));
        // --guided: the schema-first shape, for comparison runs.
        assert!(guided_of(parse(&["--guided", "hi"]).unwrap()));
        // --unguided still parses, for bench/enhance_ab.sh against older
        // builds, and maps to plain text same as no flag: it is a no-op now
        // that plain text is the default.
        assert!(!guided_of(parse(&["--unguided", "hi"]).unwrap()));
        // Asking for both at once is refused rather than silently picking one.
        assert!(parse(&["--guided", "--unguided", "hi"]).is_err());
    }

    #[test]
    fn cloud_transcription_no_longer_needs_a_key_variable_but_the_model_needs_a_provider() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from([&["macalla", "transcribe", "f.wav"], args].concat())
        };
        // The key comes from the keychain through the session now.
        assert!(parse(&["--cloud", "openai"]).is_ok());
        assert!(parse(&["--model", "gpt-transcribe"]).is_err());
        assert!(parse(&[
            "--cloud",
            "openrouter",
            "--model",
            "openai/gpt-transcribe",
            "--key-env",
            "K"
        ])
        .is_ok());
    }

    #[test]
    fn elevenlabs_is_a_cloud_provider() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from([&["macalla", "transcribe", "f.wav"], args].concat())
        };
        assert!(parse(&["--cloud", "elevenlabs"]).is_ok());
        assert!(parse(&["--cloud", "elevenlabs", "--key-env", "K"]).is_ok());
        assert!(parse(&[
            "--cloud",
            "elevenlabs",
            "--model",
            "scribe_v2",
            "--key-env",
            "K"
        ])
        .is_ok());
    }

    #[test]
    fn summarise_takes_a_file_and_answers_to_both_spellings_listed_in_help() {
        assert!(Cli::try_parse_from(["macalla", "summarise", "meeting.vtt"]).is_ok());
        assert!(Cli::try_parse_from(["macalla", "summarize", "meeting.vtt"]).is_ok());
        assert!(
            Cli::try_parse_from(["macalla", "summarise"]).is_err(),
            "a file is required"
        );
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("summarise"), "{help}");
        assert!(
            help.contains("summarize"),
            "the other spelling is listed: {help}"
        );
    }

    #[test]
    fn summarise_takes_one_provider_or_a_pool_not_both() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from([&["macalla", "summarise", "meeting.vtt"], args].concat())
        };
        assert!(parse(&[
            "--pool",
            "openai:gpt-5.6-luna",
            "--pool",
            "anthropic:claude-test"
        ])
        .is_ok());
        assert!(parse(&["--anthropic", "--model", "claude-test"]).is_ok());
        assert!(parse(&[
            "--endpoint",
            "http://localhost:11434",
            "--model",
            "granite4.1:3b"
        ])
        .is_ok());
        assert!(parse(&[
            "--pool",
            "openai:gpt-5.6-luna",
            "--endpoint",
            "http://localhost:11434",
            "--model",
            "m"
        ])
        .is_err());
        assert!(parse(&[
            "--pool",
            "openai:gpt-5.6-luna",
            "--anthropic",
            "--model",
            "m"
        ])
        .is_err());
        assert!(parse(&["--anthropic"]).is_err(), "Anthropic needs a model");
        assert!(parse(&["--key-provider", "openai"]).is_err());
    }

    fn no_flags() -> SummaryFlags<'static> {
        SummaryFlags {
            endpoint: None,
            anthropic: false,
            model: None,
            key_env: None,
            key_provider: None,
            pool: &[],
        }
    }

    #[test]
    fn a_pool_flag_becomes_the_choices_a_stored_pool_does() {
        let pool = vec![
            "openai:gpt-5.6-luna".to_string(),
            "anthropic:claude-test".to_string(),
            // A fine-tuned model's id keeps its own colons.
            "openai:ft:gpt-4o-mini-2024-07-18:acme::abc123".to_string(),
        ];
        let choices = summary_choices(SummaryFlags {
            pool: &pool,
            ..no_flags()
        })
        .unwrap();
        let stored = choice::cleanup_enhancers(&choice::CleanupChoice::Pool {
            models: vec![
                choice::PoolModel {
                    provider: "openai".to_string(),
                    model: "gpt-5.6-luna".to_string(),
                },
                choice::PoolModel {
                    provider: "anthropic".to_string(),
                    model: "claude-test".to_string(),
                },
                choice::PoolModel {
                    provider: "openai".to_string(),
                    model: "ft:gpt-4o-mini-2024-07-18:acme::abc123".to_string(),
                },
            ],
        });
        assert_eq!(stored.len(), 3);
        assert_eq!(format!("{choices:?}"), format!("{stored:?}"));
    }

    #[test]
    fn a_pool_entry_needs_a_provider_a_pool_can_hold_and_a_model() {
        let refused = |entry: &str| {
            let pool = vec![entry.to_string()];
            summary_choices(SummaryFlags {
                pool: &pool,
                ..no_flags()
            })
            .unwrap_err()
            .to_string()
        };
        assert!(refused("gpt-5.6-luna").contains("provider:model"));
        assert!(refused("elevenlabs:scribe").contains("cannot be in the Enhancement pool"));
        assert!(refused("openai:").contains("needs its id"));
    }

    #[test]
    fn a_single_provider_takes_its_key_as_enhance_does() {
        let anthropic = summary_choices(SummaryFlags {
            anthropic: true,
            model: Some("claude-test"),
            ..no_flags()
        })
        .unwrap();
        assert!(matches!(
            anthropic.as_slice(),
            [choice::EnhancerChoice::Anthropic { model, key: choice::KeySource::Stored(id), .. }]
                if model == "claude-test" && id == "anthropic"
        ));

        let from_env = summary_choices(SummaryFlags {
            anthropic: true,
            model: Some("claude-test"),
            key_env: Some("ANTHROPIC_KEY"),
            ..no_flags()
        })
        .unwrap();
        assert!(matches!(
            from_env.as_slice(),
            [choice::EnhancerChoice::Anthropic { key: choice::KeySource::Env(name), .. }]
                if name == "ANTHROPIC_KEY"
        ));

        let server = summary_choices(SummaryFlags {
            endpoint: Some("http://localhost:11434"),
            model: Some("granite4.1:3b"),
            ..no_flags()
        })
        .unwrap();
        assert!(matches!(
            server.as_slice(),
            [choice::EnhancerChoice::Endpoint { url, key: choice::KeySource::None, .. }]
                if url == "http://localhost:11434"
        ));

        let keyed = summary_choices(SummaryFlags {
            endpoint: Some("https://api.mistral.ai/v1"),
            model: Some("mistral-test"),
            key_provider: Some("mistral"),
            ..no_flags()
        })
        .unwrap();
        assert!(matches!(
            keyed.as_slice(),
            [choice::EnhancerChoice::Endpoint { key: choice::KeySource::Stored(id), .. }]
                if id == "mistral"
        ));
    }

    #[test]
    fn with_no_provider_flag_it_is_apple_intelligence_on_a_mac_and_refused_elsewhere() {
        let result = summary_choices(no_flags());
        if cfg!(target_os = "macos") {
            assert!(matches!(
                result.unwrap().as_slice(),
                [choice::EnhancerChoice::AppleIntelligence { .. }]
            ));
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("--endpoint"), "{error}");
            assert!(error.contains("--pool"), "{error}");
        }

        let model_alone = summary_choices(SummaryFlags {
            model: Some("m"),
            ..no_flags()
        });
        assert!(
            model_alone.is_err(),
            "--model goes with --endpoint or --anthropic"
        );
    }

    fn summary_parts() -> Vec<macalla_core::meeting_summary::PartSummary> {
        use macalla_core::meeting_summary::{
            ActionItem, DropCause, DroppedItem, PartLists, PartSummary, Point, Said, Section,
        };
        use macalla_core::summary::DropReason;
        let said = |turn: usize| Said { turn, start_ms: 0 };
        vec![
            PartSummary {
                start_ms: 0,
                end_ms: 65_000,
                lists: Ok(PartLists {
                    key_points: vec![Point {
                        text: "The parser ships Friday".to_string(),
                        quote: "ship the parser by Friday".to_string(),
                        said: said(0),
                    }],
                    decisions: Vec::new(),
                    action_items: vec![ActionItem {
                        text: "Write the release notes".to_string(),
                        quote: "write the release notes".to_string(),
                        owner: Some("Brian".to_string()),
                        said: said(0),
                    }],
                    dropped: vec![
                        DroppedItem {
                            section: Some(Section::Decisions),
                            line: "- We agreed on everything".to_string(),
                            cause: DropCause::Guard(DropReason::NoQuote),
                        },
                        DroppedItem {
                            section: None,
                            line: "Here is the summary.".to_string(),
                            cause: DropCause::OutsideSections,
                        },
                    ],
                    assumed_empty: 1,
                }),
            },
            PartSummary {
                start_ms: 65_000,
                end_ms: 3_725_000,
                lists: Err(macalla_core::Error::Enhance {
                    backend: "summary",
                    message: "the model's reply was empty".to_string(),
                }),
            },
        ]
    }

    #[test]
    fn a_summary_prints_each_part_with_its_lists_owners_drops_and_errors() {
        let text = render_summary(&summary_parts(), false);
        assert!(text.contains("Part 1, 0:00 to 1:05"), "{text}");
        assert!(
            text.contains("- The parser ships Friday \"ship the parser by Friday\""),
            "{text}"
        );
        assert!(
            text.contains("- Brian: Write the release notes \"write the release notes\""),
            "{text}"
        );
        assert!(text.contains("2 lines dropped"), "{text}");
        assert!(text.contains("Part 2, 1:05 to 1:02:05"), "{text}");
        assert!(text.contains("the model's reply was empty"), "{text}");
    }

    #[test]
    fn a_summary_as_json_carries_the_lists_owners_drops_and_errors() {
        let json: serde_json::Value =
            serde_json::from_str(&render_summary(&summary_parts(), true)).unwrap();
        let parts = json["parts"].as_array().expect("a list of parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["start_ms"], 0);
        assert_eq!(
            parts[0]["key_points"][0]["quote"],
            "ship the parser by Friday"
        );
        assert_eq!(parts[0]["action_items"][0]["owner"], "Brian");
        // Every dropped line, with where it was and why: the transcript's
        // words stay on this machine, as the summary does.
        let dropped = parts[0]["dropped"]
            .as_array()
            .expect("the dropped lines, as a list");
        assert_eq!(dropped.len(), 2);
        assert_eq!(dropped[0]["section"], "decisions");
        assert_eq!(dropped[0]["line"], "- We agreed on everything");
        assert_eq!(dropped[0]["reason"], "no_quote");
        // A line before the first heading belongs to no list.
        assert!(dropped[1]["section"].is_null(), "{}", dropped[1]);
        assert_eq!(dropped[1]["line"], "Here is the summary.");
        assert_eq!(dropped[1]["reason"], "outside_sections");
        assert_eq!(parts[0]["assumed_empty"], 1);
        assert!(parts[1]["error"]
            .as_str()
            .is_some_and(|error| error.contains("reply was empty")));
    }

    #[test]
    fn debug_names_each_dropped_line_with_its_part_list_and_reason() {
        let lines = dropped_lines(&summary_parts());

        assert_eq!(
            lines,
            [
                "part 1, decisions: it has no complete quote: - We agreed on everything",
                "part 1, before the lists: it came before the first list heading: Here is the summary.",
            ]
        );
    }

    #[test]
    fn key_set_takes_a_provider_and_an_optional_from_env() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["macalla", "key"], args].concat());
        assert!(parse(&["set", "openai"]).is_ok());
        assert!(parse(&["set", "openai", "--from-env", "OPENAI_KEY"]).is_ok());
        assert!(parse(&["set"]).is_err(), "a provider id is required");
    }

    #[test]
    fn key_remove_takes_a_provider() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["macalla", "key"], args].concat());
        assert!(parse(&["remove", "openai"]).is_ok());
        assert!(parse(&["remove"]).is_err());
    }

    #[test]
    fn key_list_takes_nothing() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["macalla", "key"], args].concat());
        assert!(parse(&["list"]).is_ok());
        assert!(parse(&["list", "openai"]).is_err());
    }

    #[test]
    fn key_status_never_reads_the_key_just_to_report_it() {
        use macalla_platform::secrets::MemoryStore;

        let store = MemoryStore::new();
        assert_eq!(key_status(&store, "openai"), "missing");
        providers::store_key(&store, "openai", "sk-test-key").unwrap();
        let status = key_status(&store, "openai");
        assert_eq!(status, "stored");
        assert!(!status.contains("sk-test-key"));
    }

    #[test]
    fn key_status_reports_a_stored_key_that_fails_to_read() {
        struct Unreadable;
        impl SecretStore for Unreadable {
            fn get(&self, account: &str) -> macalla_platform::Result<Option<String>> {
                Err(macalla_platform::Error::Secret {
                    account: account.to_string(),
                    reason: "the item could not be decoded".to_string(),
                })
            }
            fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
        }
        let status = key_status(&Unreadable, "openai");
        assert!(status.contains("unreadable"), "{status}");
        assert!(status.contains("the item could not be decoded"), "{status}");
    }

    // ---------------------------------------------------------- key input

    #[test]
    fn one_line_takes_the_single_non_empty_line_and_strips_a_trailing_cr() {
        assert_eq!(one_line("sk-test\r\n").unwrap(), "sk-test");
        assert_eq!(one_line("sk-test").unwrap(), "sk-test");
        assert_eq!(one_line("sk-test\n").unwrap(), "sk-test");
    }

    #[test]
    fn one_line_ignores_blank_lines_around_the_key() {
        assert_eq!(one_line("\n\n  sk-test  \n\n").unwrap(), "sk-test");
    }

    #[test]
    fn a_whitespace_only_pipe_has_no_line() {
        // Emptiness itself is refused downstream, by
        // `macalla_session::providers::store_key`; this is only about what
        // `one_line` hands it.
        assert_eq!(one_line("   \n\t\n").unwrap(), "");
    }

    #[test]
    fn one_line_refuses_several_non_empty_lines_without_echoing_them() {
        let err = one_line("sk-one\nsk-two\n").unwrap_err();
        let message = err.to_string();
        assert!(!message.contains("sk-one"), "{message}");
        assert!(!message.contains("sk-two"), "{message}");
    }
}
