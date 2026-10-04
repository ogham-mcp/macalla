// SPDX-License-Identifier: MIT
//! Turning a library error into a sentence a person reading the app can act
//! on.
//!
//! `macalla_session::Error`, `macalla_core::Error` and `macalla_platform::Error`
//! are written for `macalla doctor` and a terminal: their `Display` text
//! carries backticks, CLI commands (`macalla key set …`) and raw provider,
//! engine or account ids. None of that belongs on a page. [`say`], [`core`]
//! and [`platform`] match every variant of their enum with no wildcard arm,
//! so a new one cannot compile without a sentence for it. A next step, where
//! one exists, names the settings page or card that fixes it, never a
//! command.
//!
//! A variant's own free-text payload (a network failure's reason, a refused
//! choice's explanation, an OS error and the path it names) is never shown
//! as written: nothing here has checked every place that builds one, so
//! nothing here trusts it to be fit for a person, and several producers are
//! known to put a raw path or `std::io::Error` in it. It is logged instead,
//! so the detail is not lost, only kept out of the page. A file path is
//! different: the dictionary and transcript sentences name one on purpose
//! (`path.display()`), because a path is something a person put there and
//! can go and look at -- it is OS error text specifically that never
//! reaches the page.
//!
//! [`say`], [`core`] and [`platform`] are pure: they never log. Logging is
//! [`report`], [`report_core`] and [`report_platform`]'s job -- each logs
//! `error`'s own `Display` exactly once, at a level chosen per variant by an
//! exhaustive match with no wildcard ([`session_level`], [`core_level`],
//! [`platform_level`]: `debug` for an expected outcome such as `Stopped` or
//! a permanently unavailable secret store, `warn` for a real failure) --
//! then returns the same sentence the pure function would. A call site that
//! has not already logged `error` itself calls `report`/`report_core`/
//! `report_platform`; a site that logs immediately before building the
//! sentence (so the raw error is already in the log) calls the pure
//! function instead, or the error would be logged twice.

use macalla_session::providers;
use tracing::Level;

/// The provider's card name for `id`, or a safe fallback when `id` is not one
/// [`providers::PROVIDERS`] lists -- which a stored choice that predates a
/// provider's removal can still name.
pub(crate) fn provider_name(id: &str) -> &'static str {
    providers::find(id).map_or("that provider", |p| p.name)
}

/// The engine's card name for `id` ("Apple Speech"): the local engine
/// registry first, then [`providers::PROVIDERS`] (a cloud engine's `engine`
/// field on [`macalla_core::Error::Stt`] is a provider id, not a registry
/// one), then a grammatical fallback that names no id at all -- some
/// producers (the local registry's own open failures) use the internal tag
/// `"registry"`, which is not a name for anyone.
pub(crate) fn engine_name(id: &str) -> String {
    if let Some(info) = macalla_engines::registry::find(id) {
        return info.name.to_string();
    }
    if let Some(provider) = providers::find(id) {
        return provider.name.to_string();
    }
    "The speech engine".to_string()
}

/// Capitalise the first character and add a trailing full stop if one is
/// missing, for a message a crate's own doc comment already calls fit to
/// show a person, but which was not written with a page's punctuation rules
/// in mind. Only used for a payload checked to hold nothing but that --
/// never for one that can carry an OS error or a path (`core`'s `Model`
/// stopped using this; see its match arm).
fn sentence(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 1);
    let mut chars = text.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// Log `error` at `level`, the one way every `report*` function does it, so
/// there is one place that decides which tracing macro a level maps to.
fn log(level: Level, context: &'static str, error: &dyn std::fmt::Display) {
    match level {
        Level::DEBUG => tracing::debug!(%error, context),
        _ => tracing::warn!(%error, context),
    }
}

/// A `macalla_core::Error`, matched exhaustively so a new variant cannot ship
/// with no sentence. Pure: never logs. See the module doc.
pub fn core(error: &macalla_core::Error) -> String {
    use macalla_core::Error;
    match error {
        // Shared by real audio trouble (resampling) and by a dictionary file
        // that failed to parse as JSON -- the raw text can be either, so
        // nothing here can say which without repeating it.
        Error::Audio(_) => "Something could not be read or converted correctly.".to_string(),
        // `registry::open`'s own failures (a missing model, a feature not
        // compiled in, an unsupported language, the platform reporting the
        // engine unavailable) never reach here -- `Session::open_with_keys`
        // wraps every one of them in `Error::NoEngine` first, which has its
        // own sentence and never nests this one. What does reach here is a
        // mid-run failure: the worker thread dying partway
        // (macalla-engines::transcribe), a Swift callback's own error
        // (macalla-engines::apple), or a cloud request failing
        // (macalla-engines::cloud).
        Error::Stt { engine, .. } => format!(
            "{} stopped before it finished transcribing. If it happens again, choose a \
             different engine under Transcription on the Models page.",
            engine_name(engine)
        ),
        // Every producer -- macalla-session's summary.rs, failover.rs and
        // choice.rs (an Apple Intelligence availability check), macalla-core's
        // meeting_summary.rs, and every macalla-enhancers adapter (openai.rs,
        // anthropic.rs, gemini.rs, mistral.rs, openrouter.rs, apple.rs,
        // endpoint.rs) -- is "no result came back", for one of several
        // reasons this cannot tell apart from the type alone: not ready yet,
        // an empty reply, a reply not shaped as asked, a request that failed,
        // or nothing in a pool answering. Claiming any one of those would be
        // a fact the code cannot back up.
        Error::Enhance { .. } => {
            "Enhancement could not produce a result. Try again, or check the Enhancement choice \
             on the Models page."
                .to_string()
        }
        // `engine` and `what` are internal notes (an id, a capability name,
        // sometimes a design note) meant for the log, not a page.
        Error::Unsupported { .. } => {
            "Macalla could not do that with the speech engine in use.".to_string()
        }
        // The only producer is `Document::validate`, refusing a document
        // written by a schema newer than this build understands -- a
        // dictionary export or an imported file as much as a history entry,
        // so nothing here names history specifically.
        Error::SchemaVersion { .. } => {
            "This was saved by a newer version of Macalla. Update Macalla to open it.".to_string()
        }
        Error::History(_) => "History could not be read or saved just now. Try again.".to_string(),
        // core dictionary parsing and macalla-engines' model install/remove
        // both build this from raw OS text and a full path -- never fit to
        // show, whatever macalla-core's own doc comment on the variant
        // says. Kept in the log; the page gets one fixed sentence.
        Error::Model(_) => {
            "That model could not be prepared. Try again on the Models page.".to_string()
        }
    }
}

/// A `macalla_platform::Error`, matched exhaustively so a new variant cannot
/// ship with no sentence. Pure: never logs. See the module doc.
pub fn platform(error: &macalla_platform::Error) -> String {
    use macalla_platform::{Error, Permission};
    match error {
        Error::Permission { permission } => {
            let what = match permission {
                Permission::Microphone => "Microphone access",
                Permission::InputMonitoring => "Input Monitoring access",
                Permission::Accessibility => "Accessibility access",
            };
            // Where the grant is made differs by OS: macOS has System Settings
            // panes; on Windows the only permission that reaches here is the
            // microphone, changed under Windows Settings. Gate on Os::HERE so a
            // Windows build never tells the user to open a Mac pane.
            match macalla_platform::Os::HERE {
                macalla_platform::Os::MacOs => format!(
                    "{what} has not been granted. Open System Settings > {} to allow it.",
                    permission.settings_pane()
                ),
                macalla_platform::Os::Windows => format!(
                    "{what} has not been granted. Turn Macalla on under Settings > \
                     Privacy & security > Microphone to allow it."
                ),
                macalla_platform::Os::Linux => {
                    format!("{what} has not been granted.")
                }
            }
        }
        Error::Audio(_) => "The microphone could not be used.".to_string(),
        // Already a sentence fit to show a person, and its one producer
        // (macalla-platform's audio module) puts only a device name in it,
        // never OS or network text.
        Error::DeviceLost(_) => sentence(&error.to_string()),
        Error::Clipboard(_) => "The clipboard could not be used. Try again.".to_string(),
        Error::Input(_) => "This text could not be pasted. Try again.".to_string(),
        Error::Shortcut(_) => "That is not a shortcut Macalla can use.".to_string(),
        // The usual cause (dictation.rs's own hotkey-listener notice says so
        // directly) is Input Monitoring being revoked; named here too rather
        // than a vaguer "try again", which would not fix it.
        Error::Listener(_) => {
            "The shortcut stopped listening. Check Input Monitoring for Macalla in System \
             Settings, then open Macalla again."
                .to_string()
        }
        Error::Secret { account, .. } => {
            match providers::PROVIDERS.iter().find(|p| p.account == account.as_str()) {
                Some(provider) => format!(
                    "The key for {} could not be used. Enter it again under Providers on the Models page.",
                    provider.name
                ),
                None => "A stored key could not be used. Enter it again under Providers on the Models page."
                    .to_string(),
            }
        }
        // On macOS and Windows this is a real failure to open the store
        // (cached for the process's life in a `LazyLock`, `secrets.rs`) or
        // a per-call failure classified the same way; on Linux, which has no
        // store at all yet and never will until one is built in, it is the
        // one and only, permanent, expected answer. "Try again" would be
        // false for Linux, so neither sentence promises it -- see
        // `platform_level` for which of those two this logs as.
        Error::SecretStoreUnavailable(_) => {
            "This device's secure key storage is not available, so no key can be checked or saved."
                .to_string()
        }
    }
}

/// A `macalla_session::CloudFailure`, matched exhaustively so a new variant
/// cannot ship with no sentence. Pure: never logs -- every call site logs the
/// raw reason itself, before or beside building this sentence, so nothing
/// here repeats it.
pub fn cloud_failure(failure: &macalla_session::CloudFailure) -> String {
    use macalla_session::CloudFailure;
    match failure {
        // Every cloud adapter's `error` helper (macalla-engines' cloud.rs,
        // elevenlabs.rs, gemini.rs and the rest) wraps every failure it can
        // have -- a bad key, a quota, a DNS lookup, a malformed reply -- in
        // this one variant, tagged with the provider's id as `engine`. None
        // of that is "it stopped mid-transcription", so this never nests
        // `core`'s Stt sentence, which claims that cause and points at
        // switching engines -- not what a bad key or a dead connection
        // needs, and there is no local "engine" to switch to for a cloud
        // provider anyway. The caller already names the provider once.
        CloudFailure::Transcribe(macalla_core::Error::Stt { .. }) => {
            "Check its key under Providers on the Models page, and your connection.".to_string()
        }
        // A cloud adapter's error that is not `Stt` would be a producer this
        // crate does not know about yet; nothing here assumes there is none.
        CloudFailure::Transcribe(other) => core(other),
        // Nothing typed produced this -- a panic in the adapter, or the
        // request never reaching the engine at all -- so, like `Model`'s raw
        // OS text, it is never shown as written. No claim about *when*: the
        // one real producer (the worker thread failing to start) never got
        // as far as sending anything.
        CloudFailure::Other(_) => "No answer came back.".to_string(),
    }
}

/// A `macalla_session::Error`, matched exhaustively so a new variant cannot
/// ship with no sentence. Pure: never logs. See the module doc.
pub fn say(error: &macalla_session::Error) -> String {
    use macalla_session::Error;
    match error {
        // Whatever `registry::open` returns: `Stt` (every producer tagged
        // `engine: "registry"` -- a missing model, a feature not compiled
        // in, a language the model does not speak, the platform reporting
        // the engine unavailable) or `Model` (the registry's own catalogue
        // pointing at a model that is not actually in the store). One fixed
        // sentence true for all of them, not `core`'s -- nesting it read as
        // "registry ran into a problem".
        Error::NoEngine(_) => {
            "No speech engine is ready. Choose one under Transcription on the Models page, and \
             download its model there if it needs one."
                .to_string()
        }
        Error::NoPreviewEngine(_) => {
            "No speech engine is available for the live preview. Choose one under Transcription \
             on the Models page."
                .to_string()
        }
        Error::Core(source) => core(source),
        Error::Platform(source) => platform(source),
        Error::DictionaryRead { path, .. } => format!(
            "Could not read the dictionary at {}. Check it on the Dictionary card.",
            path.display()
        ),
        // Its `source` is always `macalla_core::Error::Audio("dictionary:
        // …")` (core's dictionary module), so nesting `core(source)` here
        // would only ever say the same one sentence anyway -- said directly.
        Error::DictionaryParse { path, .. } => format!(
            "The dictionary at {} is not in a form Macalla can read. Check it on the Dictionary card.",
            path.display()
        ),
        Error::DictionaryWrite { path, .. } => format!(
            "Could not save the dictionary at {}. Check it on the Dictionary card.",
            path.display()
        ),
        Error::UnknownProvider(_) => {
            "That provider is not one Macalla knows about. Check your choice on the Models page."
                .to_string()
        }
        Error::WrongKind { provider, kind } => {
            let what = match kind {
                providers::Kind::Text => "enhancement",
                providers::Kind::Transcription => "transcription",
            };
            format!(
                "{} does not offer {what}. Choose a different provider on the Models page.",
                provider_name(provider)
            )
        }
        Error::NeedsKey { provider } => format!(
            "{} needs a key before it can be used. Add one under Providers on the Models page.",
            provider_name(provider)
        ),
        Error::CloudNotCompiledIn => "Cloud transcription is not included in this build.".to_string(),
        // CLI-only: [`choice::KeySource::Env`], read from an environment
        // variable the terminal sets. No desktop settings surface names a
        // variable, so there is nowhere to send anyone.
        Error::EnvVarMissing(_) => {
            "A key this needs was expected in an environment variable that is not set."
                .to_string()
        }
        Error::Stopped => "Stopped. Nothing was kept.".to_string(),
        // Only a live meeting has these, and their words are in its own
        // catalogue. A live meeting whose stream panicked, or whose call
        // audio changed format, and that kept words is worded by
        // `meeting_words::ended_early` and never comes here; these are the
        // same reasons with nothing heard. The second instance's failure is
        // not the no-engine sentence: the engine is loaded, and the likely
        // cause is memory.
        Error::SecondEngine(_) => crate::meeting_words::SECOND_ENGINE.to_string(),
        Error::StreamPanicked(_) => crate::meeting_words::WENT_WRONG_BEFORE_ANY_WORDS.to_string(),
        Error::CallAudioFormatChanged => {
            crate::meeting_words::CALL_CHANGED_BEFORE_ANY_WORDS.to_string()
        }
        Error::CallAudioStart(_) => crate::meeting_words::CALL_WOULD_NOT_START.to_string(),
        Error::InvalidKey { provider, reason } => format!(
            "The key for {} {reason}. Enter it again under Providers on the Models page.",
            provider_name(provider)
        ),
        Error::OpenRouterFetch(_) => {
            "OpenRouter's model list could not be fetched. Check your connection, then try again on the Models page."
                .to_string()
        }
        Error::ModelListFetch { provider, .. } => format!(
            "{provider}'s model list could not be fetched. Check your connection, then try again on the Models page."
        ),
        Error::InvalidCleanupChoice(_) => {
            "That Enhancement choice cannot be used as it is. Check it on the Models page."
                .to_string()
        }
        // `reason` is a fixed, person-facing sentence from
        // `Availability::explain` -- shown as written, unlike every other
        // payload this module logs instead of showing (see the module
        // doc), because this one is known, checked, and names no id.
        Error::CleanupChoiceUnavailable(reason) => {
            format!("{reason} Choose another Enhancement option on the Models page.")
        }
        Error::SummaryNeedsProvider => {
            "A summary needs an Enhancement provider, and none is chosen. Choose one on the \
             Models page."
                .to_string()
        }
        // Nothing is contacted: every reason `summary::Summariser::build`
        // joins here (`summary.rs`) is a build-time refusal -- a provider
        // needing a key it does not have, one offering the wrong kind,
        // Apple Intelligence not available, or the key store itself
        // unreachable -- not a request that went out and failed.
        Error::SummaryProviderUnavailable(_) => {
            "No Enhancement provider for the summary could start. Check its key under \
             Providers, and the Enhancement choice, on the Models page."
                .to_string()
        }
        Error::TranscriptRead { path, .. } => format!(
            "Could not read the transcript at {}. Check the file, then try again.",
            path.display()
        ),
        Error::EmptyTranscript { path } => format!(
            "The transcript at {} has no words in it, so there is nothing to summarise.",
            path.display()
        ),
        Error::NotWebVtt { path } => format!(
            "The file at {} does not look like a WebVTT file.",
            path.display()
        ),
        Error::SrtNotSupported { path } => format!(
            "The file at {} is an SRT file, and Macalla does not read those yet.",
            path.display()
        ),
        Error::MeetingFileRead { path, .. } => {
            format!("Could not read the file at {}.", path.display())
        }
        Error::NothingHeard => "Nothing was heard in this recording.".to_string(),
    }
}

/// The level [`report_core`] logs a `macalla_core::Error` at: `debug` for an
/// outcome the code already expects and handles, `warn` for a real failure.
/// Exhaustive, no wildcard, so a new variant is silently `warn` by nobody's
/// decision until someone looks at it here.
fn core_level(error: &macalla_core::Error) -> Level {
    use macalla_core::Error;
    match error {
        Error::Audio(_) => Level::WARN,
        Error::Stt { .. } => Level::WARN,
        Error::Enhance { .. } => Level::WARN,
        Error::Unsupported { .. } => Level::WARN,
        Error::SchemaVersion { .. } => Level::WARN,
        Error::History(_) => Level::WARN,
        Error::Model(_) => Level::WARN,
    }
}

/// The level [`report_platform`] logs a `macalla_platform::Error` at. See
/// [`core_level`].
fn platform_level(error: &macalla_platform::Error) -> Level {
    use macalla_platform::Error;
    match error {
        // Not yet granted is the ordinary state on a fresh install; onboarding
        // asks about it routinely.
        Error::Permission { .. } => Level::DEBUG,
        Error::Audio(_) => Level::WARN,
        Error::DeviceLost(_) => Level::WARN,
        Error::Clipboard(_) => Level::WARN,
        Error::Input(_) => Level::WARN,
        // A typed shortcut string failing to parse is an expected rejection,
        // not a failure.
        Error::Shortcut(_) => Level::DEBUG,
        Error::Listener(_) => Level::WARN,
        Error::Secret { .. } => Level::WARN,
        // On macOS and Windows this always means the store itself is
        // broken -- either the one process-wide open failed (cached in a
        // `LazyLock`, so every call reports it until relaunch) or one call
        // was refused outright -- and that is worth a `warn` each time it is
        // hit, however often the Providers card re-renders. On every other
        // platform (Linux, for now) there is no store at all yet, by design,
        // so this is the one and only expected answer -- `debug`.
        Error::SecretStoreUnavailable(_) => {
            if cfg!(any(target_os = "macos", target_os = "windows")) {
                Level::WARN
            } else {
                Level::DEBUG
            }
        }
    }
}

/// The level [`report`] logs a `macalla_session::Error` at. See
/// [`core_level`].
fn session_level(error: &macalla_session::Error) -> Level {
    use macalla_session::Error;
    match error {
        Error::NoEngine(_) => Level::WARN,
        Error::NoPreviewEngine(_) => Level::WARN,
        Error::Core(source) => core_level(source),
        Error::Platform(source) => platform_level(source),
        Error::DictionaryRead { .. } => Level::WARN,
        Error::DictionaryParse { .. } => Level::WARN,
        Error::DictionaryWrite { .. } => Level::WARN,
        Error::UnknownProvider(_) => Level::WARN,
        Error::WrongKind { .. } => Level::WARN,
        // No key saved yet is routine, not a failure.
        Error::NeedsKey { .. } => Level::DEBUG,
        Error::CloudNotCompiledIn => Level::DEBUG,
        Error::EnvVarMissing(_) => Level::WARN,
        // The person asked for this.
        Error::Stopped => Level::DEBUG,
        // A bad paste is an expected, foreseen rejection.
        Error::InvalidKey { .. } => Level::DEBUG,
        Error::OpenRouterFetch(_) => Level::WARN,
        Error::ModelListFetch { .. } => Level::WARN,
        // Refused before it was ever saved -- validation working as meant.
        Error::InvalidCleanupChoice(_) => Level::DEBUG,
        Error::CleanupChoiceUnavailable(_) => Level::DEBUG,
        Error::SummaryNeedsProvider => Level::DEBUG,
        Error::SummaryProviderUnavailable(_) => Level::WARN,
        Error::TranscriptRead { .. } => Level::WARN,
        // A recording or file with nothing worth summarising is an outcome,
        // not a failure.
        Error::EmptyTranscript { .. } => Level::DEBUG,
        Error::NotWebVtt { .. } => Level::DEBUG,
        Error::SrtNotSupported { .. } => Level::DEBUG,
        Error::MeetingFileRead { .. } => Level::WARN,
        Error::NothingHeard => Level::DEBUG,
        Error::SecondEngine(_) => Level::WARN,
        // A bug, not an outcome.
        Error::StreamPanicked(_) => Level::WARN,
        Error::CallAudioFormatChanged => Level::WARN,
        Error::CallAudioStart(_) => Level::WARN,
    }
}

/// [`core`], plus logging `error`'s own `Display` once at the level
/// [`core_level`] chooses. For a call site that has not already logged
/// `error` itself.
pub fn report_core(error: &macalla_core::Error) -> String {
    log(core_level(error), "core error", error);
    core(error)
}

/// [`platform`], plus logging `error`'s own `Display` once at the level
/// [`platform_level`] chooses. For a call site that has not already logged
/// `error` itself.
pub fn report_platform(error: &macalla_platform::Error) -> String {
    log(platform_level(error), "platform error", error);
    platform(error)
}

/// [`say`], plus logging `error`'s own `Display` once at the level
/// [`session_level`] chooses. For a call site that has not already logged
/// `error` itself.
pub fn report(error: &macalla_session::Error) -> String {
    log(session_level(error), "session error", error);
    say(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Provider ids used to build test payloads, matching the real catalogue
    /// so a lookup finding a display name is exercised, not only the
    /// fallback.
    const OPENAI: &str = "openai";
    const ANTHROPIC: &str = "anthropic";

    /// Every id a sentence must never contain, case-sensitively: every real
    /// provider's id and keychain account, every real engine's registry id,
    /// and the two internal tags no producer's `engine` or `account` field
    /// is ever meant for a person ("registry", the `provider-key.` prefix).
    fn forbidden_ids() -> Vec<String> {
        let mut ids: Vec<String> = providers::PROVIDERS
            .iter()
            .flat_map(|p| [p.id.to_string(), p.account.to_string()])
            .collect();
        ids.extend(
            macalla_engines::registry::engines()
                .into_iter()
                .map(|e| e.id.to_string()),
        );
        ids.push("registry".to_string());
        ids.push("provider-key.".to_string());
        ids
    }

    fn assert_says(sentence: &str, forbidden_ids: &[String]) {
        assert!(!sentence.contains('`'), "{sentence:?} has a backtick");
        assert!(
            !sentence.contains("macalla "),
            "{sentence:?} names a command"
        );
        for id in forbidden_ids {
            assert!(
                !sentence.contains(id.as_str()),
                "{sentence:?} contains the raw id {id:?}"
            );
        }
        let first = sentence.chars().next().expect("a non-empty sentence");
        assert!(
            first.is_uppercase(),
            "{sentence:?} does not start with a capital"
        );
        assert!(
            sentence.ends_with('.'),
            "{sentence:?} does not end with '.'"
        );
    }

    /// `core`, `platform` and `say` match every variant with no wildcard, so
    /// a new one fails to compile there until it has a sentence -- add a
    /// sample of it here too, or its sentence goes untested.
    fn core_variants() -> Vec<macalla_core::Error> {
        use macalla_core::Error;
        vec![
            Error::Audio("device reported a sample rate of 0".to_string()),
            // A real producer: macalla-engines::registry::open's `Unsupported`
            // engine fields, every one tagged "registry".
            Error::Stt {
                engine: "registry",
                message: "parakeet-tdt-v3 needs its model first".to_string(),
            },
            // A cloud engine's own producer (macalla-engines::cloud): `engine`
            // is a provider id, not a registry one.
            Error::Stt {
                engine: "openai",
                message: "401 Unauthorized".to_string(),
            },
            Error::Enhance {
                backend: "summary",
                message: "the summary was stopped before this part was sent".to_string(),
            },
            // A real producer (macalla-engines::cloud): a note for the log
            // inside `what`, never fit to show.
            Error::Unsupported {
                engine: "openai",
                what: "streaming; the whole recording goes in one request (design note)",
            },
            Error::SchemaVersion {
                found: 3,
                expected: 2,
            },
            Error::History("the file is locked".to_string()),
            // A real producer (macalla-engines::models::Store::remove):
            // an OS error and a path, neither fit to show.
            Error::Model(
                "removing /tmp/macalla-test/models/x.bin: Permission denied (os error 13)"
                    .to_string(),
            ),
        ]
    }

    /// `core`, `platform` and `say` match every variant with no wildcard, so
    /// a new one fails to compile there until it has a sentence -- add a
    /// sample of it here too, or its sentence goes untested.
    fn platform_variants() -> Vec<macalla_platform::Error> {
        use macalla_platform::{Error, Permission};
        vec![
            Error::Permission {
                permission: Permission::Microphone,
            },
            Error::Permission {
                permission: Permission::InputMonitoring,
            },
            Error::Permission {
                permission: Permission::Accessibility,
            },
            Error::Audio("no default input device".to_string()),
            Error::DeviceLost("USB Microphone".to_string()),
            Error::Clipboard("busy".to_string()),
            Error::Input("no device".to_string()),
            Error::Shortcut("not a shortcut".to_string()),
            Error::Listener("the event tap died".to_string()),
            Error::Secret {
                account: "provider-key.openai".to_string(),
                reason: "the item could not be found".to_string(),
            },
            // Linux's permanent producer (macalla-platform::secrets::NO_STORE).
            Error::SecretStoreUnavailable(
                "Macalla keeps no keys in a keychain on this platform yet; give the key in an \
                 environment variable"
                    .to_string(),
            ),
        ]
    }

    /// `core`, `platform` and `say` match every variant with no wildcard, so
    /// a new one fails to compile there until it has a sentence -- add a
    /// sample of it here too, or its sentence goes untested.
    fn session_variants() -> Vec<macalla_session::Error> {
        use macalla_session::providers::Kind;
        use macalla_session::Error;
        vec![
            // Whatever `registry::open` returns: `Stt` or `Model`.
            Error::NoEngine(macalla_core::Error::Stt {
                engine: "registry",
                message: "parakeet-tdt-v3 needs its model first".to_string(),
            }),
            // A real producer's shape: `Session::preview_error`, set from
            // the local engine's own open error's `Display`.
            Error::NoPreviewEngine(
                "stt engine registry: apple-speech cannot run here: speech assets are not \
                 installed"
                    .to_string(),
            ),
            Error::Core(macalla_core::Error::History(
                "the file is locked".to_string(),
            )),
            Error::Platform(macalla_platform::Error::Clipboard("busy".to_string())),
            Error::DictionaryRead {
                path: PathBuf::from("/tmp/macalla-dictionary.json"),
                source: std::io::Error::other("permission denied"),
            },
            Error::DictionaryParse {
                path: PathBuf::from("/tmp/macalla-dictionary.json"),
                source: macalla_core::Error::Audio("dictionary: expected value".to_string()),
            },
            Error::DictionaryWrite {
                path: PathBuf::from("/tmp/macalla-dictionary.json"),
                source: std::io::Error::other("disk full"),
            },
            Error::UnknownProvider("watson".to_string()),
            Error::WrongKind {
                provider: ANTHROPIC.to_string(),
                kind: Kind::Transcription,
            },
            Error::NeedsKey {
                provider: OPENAI.to_string(),
            },
            Error::CloudNotCompiledIn,
            Error::EnvVarMissing("MACALLA_OPENAI_KEY".to_string()),
            Error::Stopped,
            Error::InvalidKey {
                provider: OPENAI.to_string(),
                reason: "was empty",
            },
            Error::OpenRouterFetch("the network is down".to_string()),
            Error::ModelListFetch {
                provider: "OpenAI".to_string(),
                reason: "429 Too Many Requests".to_string(),
            },
            Error::InvalidCleanupChoice(format!(
                "a {OPENAI} model in the cleanup pool needs its id"
            )),
            Error::CleanupChoiceUnavailable("Apple Intelligence is turned off in System Settings."),
            Error::SummaryNeedsProvider,
            Error::SummaryProviderUnavailable(format!("{ANTHROPIC} needs a key")),
            Error::TranscriptRead {
                path: PathBuf::from("/tmp/meeting.vtt"),
                source: std::io::Error::other("permission denied"),
            },
            Error::EmptyTranscript {
                path: PathBuf::from("/tmp/meeting.vtt"),
            },
            Error::NotWebVtt {
                path: PathBuf::from("/tmp/meeting.vtt"),
            },
            Error::SrtNotSupported {
                path: PathBuf::from("/tmp/meeting.srt"),
            },
            Error::MeetingFileRead {
                path: PathBuf::from("/tmp/meeting.wav"),
                source: std::io::Error::other("permission denied"),
            },
            Error::NothingHeard,
            Error::SecondEngine(macalla_core::Error::Stt {
                engine: "registry",
                message: "out of memory".to_string(),
            }),
            Error::StreamPanicked("the tap fell over".to_string()),
            Error::CallAudioFormatChanged,
            Error::CallAudioStart(macalla_platform::system_audio::OpenError::CoreAudio {
                step: macalla_platform::system_audio::Step::Start,
                status: -1,
            }),
        ]
    }

    #[test]
    fn every_core_error_variant_says_a_clean_sentence() {
        let forbidden = forbidden_ids();
        for error in core_variants() {
            assert_says(&core(&error), &forbidden);
        }
    }

    #[test]
    fn every_platform_error_variant_says_a_clean_sentence() {
        let forbidden = forbidden_ids();
        for error in platform_variants() {
            assert_says(&platform(&error), &forbidden);
        }
    }

    #[test]
    fn every_session_error_variant_says_a_clean_sentence() {
        let forbidden = forbidden_ids();
        for error in session_variants() {
            assert_says(&say(&error), &forbidden);
        }
    }

    #[test]
    fn every_cloud_failure_variant_says_a_clean_sentence() {
        use macalla_session::CloudFailure;
        let forbidden = forbidden_ids();
        let variants = [
            CloudFailure::Transcribe(macalla_core::Error::Stt {
                engine: "openai",
                message: "401 Unauthorized".to_string(),
            }),
            CloudFailure::Transcribe(macalla_core::Error::Enhance {
                backend: "summary",
                message: "never happens for a real cloud adapter; exhaustiveness only".to_string(),
            }),
            // Never typed: a panic in the adapter, or the request never
            // reaching the engine at all.
            CloudFailure::Other("the adapter fell over".to_string()),
        ];
        for failure in variants {
            assert_says(&cloud_failure(&failure), &forbidden);
        }
    }

    #[test]
    fn every_cloud_adapter_wraps_its_failure_as_stt_so_none_claims_it_stopped_mid_run() {
        use macalla_session::CloudFailure;
        // Every real producer (macalla-engines' cloud.rs, elevenlabs.rs,
        // gemini.rs): a bad key, a quota, a DNS failure and a malformed
        // reply are all this one variant, so nothing here can tell them
        // apart -- and none of them is the engine stopping partway.
        assert_eq!(
            cloud_failure(&CloudFailure::Transcribe(macalla_core::Error::Stt {
                engine: "elevenlabs",
                message: "HTTP 401: invalid key".to_string(),
            })),
            "Check its key under Providers on the Models page, and your connection."
        );
        assert_eq!(
            cloud_failure(&CloudFailure::Other(
                "the request stopped without an answer".to_string()
            )),
            "No answer came back."
        );
    }

    #[test]
    fn a_model_error_with_an_os_error_and_a_path_leaks_neither() {
        let sentence = core(&macalla_core::Error::Model(
            "removing /tmp/macalla-test/models/x.bin: Permission denied (os error 13)".to_string(),
        ));
        assert!(!sentence.contains("os error"), "{sentence:?}");
        assert!(!sentence.contains('/'), "{sentence:?}");
    }

    #[test]
    fn an_unsupported_error_does_not_repeat_its_log_note() {
        let sentence = core(&macalla_core::Error::Unsupported {
            engine: "openai",
            what: "streaming; the whole recording goes in one request (design note)",
        });
        assert!(!sentence.contains("design note"), "{sentence:?}");
    }

    #[test]
    fn report_returns_exactly_says_text_for_every_session_variant() {
        for error in session_variants() {
            assert_eq!(report(&error), say(&error));
        }
    }

    #[test]
    fn report_core_returns_exactly_cores_text_for_every_variant() {
        for error in core_variants() {
            assert_eq!(report_core(&error), core(&error));
        }
    }

    #[test]
    fn report_platform_returns_exactly_platforms_text_for_every_variant() {
        for error in platform_variants() {
            assert_eq!(report_platform(&error), platform(&error));
        }
    }

    #[test]
    fn a_known_provider_id_is_shown_by_its_card_name() {
        assert_eq!(provider_name("openai"), "OpenAI");
    }

    #[test]
    fn an_id_no_provider_lists_falls_back_to_a_safe_phrase() {
        assert_eq!(provider_name("watson"), "that provider");
    }

    #[test]
    fn a_known_engine_id_is_shown_by_its_card_name() {
        assert_eq!(engine_name("apple-speech"), "Apple Speech");
    }

    #[test]
    fn a_cloud_provider_id_is_shown_by_its_card_name_too() {
        assert_eq!(engine_name("openai"), "OpenAI");
    }

    #[test]
    fn the_internal_registry_tag_names_no_id_at_all() {
        assert_eq!(engine_name("registry"), "The speech engine");
    }

    #[test]
    fn an_id_nothing_knows_falls_back_to_a_safe_phrase() {
        assert_eq!(engine_name("no-such-engine"), "The speech engine");
    }

    #[test]
    fn a_message_already_fit_to_show_gets_a_capital_and_a_full_stop() {
        assert_eq!(sentence("was empty"), "Was empty.");
        assert_eq!(
            sentence("already ends with a full stop."),
            "Already ends with a full stop."
        );
    }
}
