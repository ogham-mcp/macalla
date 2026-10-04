// SPDX-License-Identifier: MIT
//! Turning a plain choice and a key store into a running engine or enhancer.
//!
//! [`crate::providers`] knows which providers exist and how to move a key in
//! and out of the store. This module is the one place that maps a provider id
//! to the adapter that speaks for it, so the CLI and the desktop app each make
//! one call instead of carrying their own copy of the mapping (the same reasoning as
//! for the engine registry, applied to the cloud providers).
//!
//! "Needs a key" is decided here too: it combines the local engine registry
//! with the cloud provider table and checks the key store, so
//! `macalla-engines` never has to learn about keys or the keychain.

use crate::providers::{self, Kind};
use crate::{Error, Result};
use macalla_core::enhance::Enhancer;
use macalla_core::stt::SttEngine;
use macalla_platform::secrets::SecretStore;

pub use macalla_enhancers::endpoint::Reasoning;

/// Re-exported so the desktop app can name Apple Intelligence's availability
/// without depending on `macalla-enhancers` itself -- the app calls the
/// session, not the crates beneath it.
pub use macalla_enhancers::apple::Availability as AppleIntelligenceAvailability;

/// Re-exported so the CLI and the desktop app build
/// [`EnhancerChoice::AppleIntelligence`]'s default from the same constant the
/// library uses, rather than each naming `false` on its own.
pub use macalla_enhancers::apple::DEFAULT_GUIDED;

/// A cloud transcription engine to open, in [`crate::Config::cloud`].
#[derive(Debug, Clone)]
pub struct CloudTranscription {
    /// A [`providers::Provider::id`] that offers [`Kind::Transcription`].
    pub provider: String,
    /// The provider's own model id. `None` falls back to whatever the
    /// provider's adapter defaults to, if anything.
    pub model: Option<String>,
}

/// Build the cloud engine `choice` names, with its key from `keys`.
///
/// Checks, in order: the provider exists, it offers [`Kind::Transcription`],
/// and a key is stored for it. `language` is threaded through for the same
/// reason [`macalla_engines::registry::open`] takes one -- every engine is
/// opened the same way -- though no cloud provider here restricts by
/// language today.
#[cfg(feature = "cloud-transcription")]
pub fn cloud_engine(
    choice: &CloudTranscription,
    _language: &str,
    keys: &dyn SecretStore,
) -> Result<Box<dyn SttEngine>> {
    use macalla_engines::cloud::{CloudEngine, ProviderSpec, COHERE, MISTRAL, OPENAI, OPENROUTER};

    let provider = providers::find(&choice.provider)
        .ok_or_else(|| Error::UnknownProvider(choice.provider.clone()))?;
    if !provider.offers(Kind::Transcription) {
        return Err(Error::WrongKind {
            provider: provider.id.to_string(),
            kind: Kind::Transcription,
        });
    }
    let key = providers::read_key(keys, provider.id)?.ok_or_else(|| Error::NeedsKey {
        provider: provider.id.to_string(),
    })?;

    fn openai_shaped(
        spec: &'static ProviderSpec,
        key: String,
        model: Option<&str>,
    ) -> Box<dyn SttEngine> {
        let mut engine = CloudEngine::new(spec, key);
        if let Some(model) = model {
            engine = engine.with_model(model);
        }
        Box::new(engine)
    }

    let engine: Box<dyn SttEngine> = match provider.id {
        "openai" => openai_shaped(&OPENAI, key, choice.model.as_deref()),
        "mistral" => openai_shaped(&MISTRAL, key, choice.model.as_deref()),
        "cohere" => openai_shaped(&COHERE, key, choice.model.as_deref()),
        "openrouter" => openai_shaped(&OPENROUTER, key, choice.model.as_deref()),
        "gemini" => {
            let mut engine = macalla_engines::gemini::GeminiEngine::new(key);
            if let Some(model) = &choice.model {
                engine = engine.with_model(model.clone());
            }
            Box::new(engine)
        }
        "elevenlabs" => {
            let mut engine = macalla_engines::elevenlabs::ElevenLabsEngine::new(key);
            if let Some(model) = &choice.model {
                engine = engine.with_model(model.clone());
            }
            Box::new(engine)
        }
        // providers::PROVIDERS lists exactly these six as Kind::Transcription
        // (see providers::tests::transcription_providers_match_spec_section_4);
        // the offers() check above already refused anything else.
        other => unreachable!("{other} offers transcription but has no cloud constructor here"),
    };
    Ok(engine)
}

/// Without the `cloud` feature, no cloud engine can be built at all
/// (the app must ship and work with none compiled in).
#[cfg(not(feature = "cloud-transcription"))]
pub fn cloud_engine(
    _choice: &CloudTranscription,
    _language: &str,
    _keys: &dyn SecretStore,
) -> Result<Box<dyn SttEngine>> {
    Err(Error::CloudNotCompiledIn)
}

/// Where an enhancer's key comes from. One shape for `Endpoint` and
/// `Anthropic` alike, so `choice::enhancer` is the only place that resolves
/// one -- the CLI used to build `Endpoint`/`Anthropic` directly for its
/// `--key-env` path, duplicating this.
#[derive(Debug, Clone)]
pub enum KeySource {
    /// No key, as for a local Ollama.
    None,
    /// A [`providers::Provider::id`] offering [`Kind::Text`], whose stored
    /// key to read. Checked against the kind first, same as
    /// [`crate::providers::read_key`] callers everywhere else.
    Stored(String),
    /// An environment variable, read when the enhancer is built. No
    /// provider-table check -- this is how an arbitrary self-hosted
    /// endpoint's key, unrelated to anything in [`providers::PROVIDERS`],
    /// keeps working.
    Env(String),
}

impl KeySource {
    /// The key this source names, or `None` for [`KeySource::None`].
    /// [`KeySource::Env`] never touches `keys`.
    fn resolve(&self, keys: &dyn SecretStore) -> Result<Option<String>> {
        match self {
            KeySource::None => Ok(None),
            KeySource::Stored(id) => Ok(Some(key_for(keys, id, Kind::Text)?)),
            KeySource::Env(name) => match EnvKey::named(name.clone()).get("")? {
                Some(value) => Ok(Some(value)),
                None => Err(Error::EnvVarMissing(name.clone())),
            },
        }
    }
}

/// Which model `choice::enhancer` should build.
#[derive(Debug, Clone)]
pub enum EnhancerChoice {
    /// FoundationModels on this Mac, refused with the system's own reason
    /// when it is not available.
    AppleIntelligence { guided: bool },
    /// An OpenAI-compatible endpoint, local or cloud.
    Endpoint {
        url: String,
        model: String,
        key: KeySource,
        reasoning: Option<Reasoning>,
    },
    /// Anthropic's own Messages API. A caller normally passes
    /// `KeySource::Stored("anthropic".into())`; `KeySource::None` is refused
    /// with [`Error::NeedsKey`], since Anthropic has no keyless mode.
    Anthropic {
        model: String,
        key: KeySource,
        reasoning: Option<Reasoning>,
    },
}

/// Build the enhancer `choice` names, with a key from `keys` where one is
/// needed. The only place any of the three adapters is constructed: a
/// caller never needs `macalla-enhancers` itself to build one.
pub fn enhancer(choice: &EnhancerChoice, keys: &dyn SecretStore) -> Result<Box<dyn Enhancer>> {
    use macalla_enhancers::anthropic::Anthropic;
    use macalla_enhancers::apple::{AppleIntelligence, Availability};
    use macalla_enhancers::endpoint::Endpoint;

    match choice {
        EnhancerChoice::AppleIntelligence { guided } => {
            if let Some(reason) = Availability::now().explain() {
                return Err(Error::Core(macalla_core::Error::Enhance {
                    backend: macalla_enhancers::apple::BACKEND_ID,
                    message: reason.to_string(),
                }));
            }
            let apple = AppleIntelligence::new();
            Ok(Box::new(if *guided { apple.guided() } else { apple }))
        }
        EnhancerChoice::Endpoint {
            url,
            model,
            key,
            reasoning,
        } => {
            let key = key.resolve(keys)?;
            let endpoint = Endpoint::new(url, model, key);
            Ok(Box::new(match reasoning {
                Some(r) => endpoint.with_reasoning(*r),
                None => endpoint,
            }))
        }
        EnhancerChoice::Anthropic {
            model,
            key,
            reasoning,
        } => {
            let key = key.resolve(keys)?.ok_or_else(|| Error::NeedsKey {
                provider: "anthropic".to_string(),
            })?;
            let anthropic = Anthropic::new(model, key);
            Ok(Box::new(match reasoning {
                Some(r) => anthropic.with_reasoning(*r),
                None => anthropic,
            }))
        }
    }
}

/// Where cleanup sends the text it tidies.
///
/// Decided here, from the choice alone, so the session's scheduling and the
/// label a person sees cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupReach {
    /// The text stays on this machine: Apple Intelligence, or a server at a
    /// loopback address ([`is_loopback`]). Each segment is cleaned while the
    /// person speaks, and the paste waits for it up to
    /// [`crate::ENHANCEMENT_WAIT`].
    Local,
    /// The text leaves this machine: a server at any other address, Anthropic,
    /// or any cloud model added later. It gets nothing while the person
    /// speaks, and the whole Tier 0 document once, at key release, which the
    /// paste waits for up to [`crate::ENHANCEMENT_WAIT`]. A recording thrown
    /// away before then is never sent.
    Remote,
}

/// An opaque name for a list of cleanup choices, the same for the same choices
/// in the same order: addresses, models, key sources and reasoning, never a
/// key. A page that showed where text would go passes it back, so that a
/// changed choice is refused even when its label reads the same: two servers
/// both read "Your server", a pool's label is cut short, and a changed model
/// keeps its label.
///
/// The hash is keyed at random once per run of the app: stable while it runs,
/// so a page can pass back what it was shown, and not something anyone can
/// reproduce outside it. An endpoint's address is read without its userinfo,
/// so a user or password written into the address never goes into the hash.
pub fn fingerprint(choices: &[EnhancerChoice]) -> String {
    use std::hash::BuildHasher;
    static KEYS: std::sync::OnceLock<std::collections::hash_map::RandomState> =
        std::sync::OnceLock::new();
    let described: Vec<String> = choices.iter().map(described).collect();
    format!(
        "{:016x}",
        KEYS.get_or_init(Default::default).hash_one(described)
    )
}

/// A choice as [`fingerprint`] reads it: its debug form, with an endpoint's
/// address cut down to what it connects to.
fn described(choice: &EnhancerChoice) -> String {
    match choice {
        EnhancerChoice::Endpoint {
            url,
            model,
            key,
            reasoning,
        } => format!(
            "Endpoint {{ url: {:?}, model: {model:?}, key: {key:?}, reasoning: {reasoning:?} }}",
            without_userinfo(url)
        ),
        other => format!("{other:?}"),
    }
}

/// `address` without the userinfo in its authority, reading the authority as
/// [`loopback_host`] does: it ends at the first `/`, `?`, `#` or `\`, and the
/// host follows its last `@`.
fn without_userinfo(address: &str) -> String {
    let trimmed = address.trim();
    let (scheme, after_scheme) = trimmed
        .split_once("://")
        .map_or(("", trimmed), |(scheme, rest)| (scheme, rest));
    let end = after_scheme
        .find(['/', '?', '#', '\\'])
        .unwrap_or(after_scheme.len());
    let (authority, rest) = after_scheme.split_at(end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if scheme.is_empty() {
        format!("{host}{rest}")
    } else {
        format!("{scheme}://{host}{rest}")
    }
}

impl EnhancerChoice {
    /// Whether this choice keeps the text on this machine. Anything not
    /// known to run here is [`CleanupReach::Remote`]: the cost of that
    /// mistake is a slower paste, where the other mistake sends a
    /// recording the person threw away. A hostname that resolves to this
    /// machine is not looked up, for the same reason [`is_loopback`] gives.
    pub fn reach(&self) -> CleanupReach {
        match self {
            EnhancerChoice::AppleIntelligence { .. } => CleanupReach::Local,
            EnhancerChoice::Endpoint { url, .. } if is_loopback(url) => CleanupReach::Local,
            EnhancerChoice::Endpoint { .. } | EnhancerChoice::Anthropic { .. } => {
                CleanupReach::Remote
            }
        }
    }

    /// The name a person sees for where cleanup sends the text, for the label
    /// the rule that nothing leaves the machine unless the user turned it on
    /// asks for while it is on:
    ///
    /// - "Apple Intelligence";
    /// - "Local" for a server at a loopback address;
    /// - the provider's name for a server at that provider's own chat address,
    ///   the one a pool sends to, whatever the key: stored, from an
    ///   environment variable, or none, since the words go to that provider;
    /// - "Your server at" the host, for a key of any kind sent to any other
    ///   address, so the label names where the words and the key actually go.
    ///   Never the address's user, password or port;
    /// - "Your server" for any other address with no key, or for an address
    ///   not plain enough to be sure which part is the host.
    pub fn destination(&self) -> std::borrow::Cow<'static, str> {
        use std::borrow::Cow;
        match self {
            EnhancerChoice::AppleIntelligence { .. } => Cow::Borrowed("Apple Intelligence"),
            EnhancerChoice::Endpoint { url, .. } if is_loopback(url) => Cow::Borrowed("Local"),
            EnhancerChoice::Endpoint { url, key, .. } => match (providers_own_address(url), key) {
                (Some(provider), _) => Cow::Borrowed(provider),
                (None, KeySource::None) => Cow::Borrowed("Your server"),
                (None, KeySource::Stored(_) | KeySource::Env(_)) => match labelled_host(url) {
                    Some(host) => Cow::Owned(format!("Your server at {host}")),
                    None => Cow::Borrowed("Your server"),
                },
            },
            EnhancerChoice::Anthropic { .. } => {
                Cow::Borrowed(providers::find("anthropic").map_or("Anthropic", |p| p.name))
            }
        }
    }

    /// Who a dictation's Enhancement waits for when this choice serves it,
    /// typed, and read the way [`EnhancerChoice::destination`] reads the
    /// choice: a provider by id at its own address whatever the key, a server
    /// at a loopback address, or any other server.
    pub(crate) fn enhancing_with(&self) -> crate::EnhancingWith {
        use crate::EnhancingWith;
        match self {
            EnhancerChoice::AppleIntelligence { .. } => EnhancingWith::AppleIntelligence,
            EnhancerChoice::Endpoint { url, .. } if is_loopback(url) => {
                EnhancingWith::ServerOnThisComputer
            }
            EnhancerChoice::Endpoint { .. } | EnhancerChoice::Anthropic { .. } => {
                match self.provider_id() {
                    Some(id) => EnhancingWith::Provider(id.to_string()),
                    None => EnhancingWith::Server,
                }
            }
        }
    }

    /// The provider this choice sends the text to, by id as in
    /// [`providers::PROVIDERS`], read the way [`EnhancerChoice::destination`]
    /// reads the choice: a provider at its own chat address whatever the
    /// key, and Anthropic. `None` for Apple Intelligence, a server at a
    /// loopback address, and any other server, none of which is a provider.
    pub fn provider_id(&self) -> Option<&'static str> {
        match self {
            EnhancerChoice::AppleIntelligence { .. } => None,
            EnhancerChoice::Endpoint { url, .. } if is_loopback(url) => None,
            EnhancerChoice::Endpoint { url, .. } => providers_own(url).map(|p| p.id),
            EnhancerChoice::Anthropic { .. } => Some("anthropic"),
        }
    }
}

/// The host a label may name for `url`, or `None` when the address is not
/// plain enough to be sure which part is the host.
///
/// An authority holding a `?`, `#` or `\` can put part of a user name or
/// password where [`loopback_host`] reads the host (`reader:pa?ss@host`), so
/// such an address is never named. Otherwise the host is named only when it
/// is a plain hostname or IP address: ASCII letters, digits, `-` and `.`, or
/// an IPv6 address, shown in brackets. The loopback decision is not made
/// here and does not change.
fn labelled_host(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let after_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    let authority = after_scheme.split('/').next().unwrap_or("");
    if authority.contains(['?', '#', '\\']) {
        return None;
    }
    let host = loopback_host(url);
    if host.is_empty() {
        return None;
    }
    if host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
    {
        return Some(host);
    }
    let ipv6 = host.contains(':')
        && host
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'));
    ipv6.then(|| format!("[{host}]"))
}

/// The name of the provider whose own chat address `url` is, as [`pool_route`]
/// names it, or `None`. Hosts are compared through [`labelled_host`], so an
/// address not plain enough to be sure which part is the host names no
/// provider, whatever the host reader would make of it.
fn providers_own_address(url: &str) -> Option<&'static str> {
    providers_own(url).map(|provider| provider.name)
}

/// The provider whose own chat address `url` is, as
/// [`providers_own_address`] reads it.
fn providers_own(url: &str) -> Option<&'static providers::Provider> {
    let host = labelled_host(url)?;
    providers::PROVIDERS.iter().find(|provider| {
        matches!(
            pool_route(provider.id),
            Some(PoolRoute::Compatible(own)) if labelled_host(own).as_deref() == Some(host.as_str())
        )
    })
}

/// Like [`enhancer`], for a session that keeps its provider for its whole
/// life. Apple Intelligence is built whatever it reports right now, with the
/// system's reason beside it: it is a setting the person can switch on at any
/// moment, and the worker asks it again before every segment. Every other
/// choice is exactly [`enhancer`], with no reason.
pub(crate) fn session_enhancer(
    choice: &EnhancerChoice,
    keys: &dyn SecretStore,
) -> Result<(Box<dyn Enhancer>, Option<&'static str>)> {
    use macalla_enhancers::apple::{AppleIntelligence, Availability};
    match choice {
        EnhancerChoice::AppleIntelligence { guided } => {
            let not_now = Availability::now().explain();
            let apple = AppleIntelligence::new();
            let built: Box<dyn Enhancer> = Box::new(if *guided { apple.guided() } else { apple });
            Ok((built, not_now))
        }
        other => Ok((enhancer(other, keys)?, None)),
    }
}

/// The free-text model a choice names, before it is boxed. Apple Intelligence
/// stays its own type, so a test can see the time limit a summary gives it.
pub(crate) enum TextModelKind {
    Apple(macalla_enhancers::apple::AppleIntelligence),
    Other(Box<dyn macalla_core::text_model::TextModel>),
}

impl TextModelKind {
    pub(crate) fn into_box(self) -> Box<dyn macalla_core::text_model::TextModel> {
        match self {
            Self::Apple(model) => Box::new(model),
            Self::Other(model) => model,
        }
    }
}

/// The free-text model `choice` names, for a caller that wants a reply rather
/// than refined blocks (a meeting summary), with its key from `keys`. Building
/// sends nothing.
///
/// - Apple Intelligence waits up to [`crate::summary::PART_TIME_LIMIT`] for
///   each part's reply, not cleanup's limit for one segment. It is built
///   whatever it reports right now: whether it can serve is asked before each
///   request, through
///   [`TextModel::ready`](macalla_core::text_model::TextModel::ready).
/// - The choice's reasoning setting is not used, because every
///   [`TextModel`](macalla_core::text_model::TextModel) asks for the least
///   reasoning, always.
pub(crate) fn text_model_for(
    choice: &EnhancerChoice,
    keys: &dyn SecretStore,
) -> Result<TextModelKind> {
    use macalla_enhancers::anthropic::Anthropic;
    use macalla_enhancers::apple::AppleIntelligence;
    use macalla_enhancers::endpoint::Endpoint;
    match choice {
        EnhancerChoice::AppleIntelligence { .. } => Ok(TextModelKind::Apple(
            AppleIntelligence::new().answering_within(crate::summary::PART_TIME_LIMIT),
        )),
        EnhancerChoice::Endpoint {
            url, model, key, ..
        } => Ok(TextModelKind::Other(Box::new(Endpoint::new(
            url,
            model,
            key.resolve(keys)?,
        )))),
        EnhancerChoice::Anthropic { model, key, .. } => {
            let key = key.resolve(keys)?.ok_or_else(|| Error::NeedsKey {
                provider: "anthropic".to_string(),
            })?;
            Ok(TextModelKind::Other(Box::new(Anthropic::new(model, key))))
        }
    }
}

/// The key for `provider_id`, checked against `kind` first. Shared by every
/// arm of [`enhancer`] that takes a keychain-backed key.
fn key_for(keys: &dyn SecretStore, provider_id: &str, kind: Kind) -> Result<String> {
    let provider = providers::find(provider_id)
        .ok_or_else(|| Error::UnknownProvider(provider_id.to_string()))?;
    if !provider.offers(kind) {
        return Err(Error::WrongKind {
            provider: provider.id.to_string(),
            kind,
        });
    }
    providers::read_key(keys, provider.id)?.ok_or_else(|| Error::NeedsKey {
        provider: provider.id.to_string(),
    })
}

/// A read-only [`SecretStore`] that answers every account with the value of
/// one environment variable, read at call time. How `--key-env` keeps working
/// on a machine with no keychain (Linux, today): the CLI substitutes this for
/// [`macalla_platform::secrets::SystemSecrets`] rather than reading the
/// variable itself and handing the raw value around.
pub struct EnvKey(String);

impl EnvKey {
    /// Reads `name` on every [`SecretStore::get`], not now: a key named here
    /// but never set is only a problem once something asks for it.
    pub fn named(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl SecretStore for EnvKey {
    fn get(&self, _account: &str) -> macalla_platform::Result<Option<String>> {
        Ok(std::env::var(&self.0).ok())
    }

    fn set(&self, account: &str, _secret: &str) -> macalla_platform::Result<()> {
        Err(macalla_platform::Error::Secret {
            account: account.to_string(),
            reason: format!(
                "${} is read-only: a key from the environment cannot be stored",
                self.0
            ),
        })
    }

    fn delete(&self, account: &str) -> macalla_platform::Result<()> {
        Err(macalla_platform::Error::Secret {
            account: account.to_string(),
            reason: format!(
                "${} is read-only: a key from the environment cannot be removed",
                self.0
            ),
        })
    }
}

/// One choice `macalla transcribe` (or a settings screen) could offer for
/// transcription: a local engine from the registry, or a cloud provider with
/// whether it is ready to use.
#[derive(Debug, Clone)]
pub enum TranscriptionOption {
    Local(macalla_engines::registry::EngineInfo),
    Cloud {
        provider: &'static providers::Provider,
        availability: CloudAvailability,
    },
}

/// Whether a cloud transcription provider can be used right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudAvailability {
    /// A key is stored.
    Ready,
    /// The store works but has no key for this provider yet.
    NeedsKey,
    /// The key store itself could not be reached, with the reason.
    KeyStoreUnavailable(String),
    /// The store works and has an entry for this provider, but reading it
    /// failed -- the OS access prompt was denied, or the item does not
    /// decode. Distinct from [`CloudAvailability::NeedsKey`]: there IS a key
    /// here, so a settings screen must not offer "add a key" over it, which
    /// would overwrite it.
    KeyUnreadable(String),
    /// This build has no cloud transcription at all.
    NotCompiledIn,
}

/// Every local engine this build knows about, straight from
/// [`macalla_engines::registry::engines`] and nothing else. Never touches a
/// key store -- a caller that only wants the local list (the Engine card)
/// must not pay for a keychain read on every provider just to throw the
/// answer away.
pub fn local_transcription_options() -> Vec<macalla_engines::registry::EngineInfo> {
    macalla_engines::registry::engines()
}

/// Every transcription choice this build knows about: every local engine
/// from [`local_transcription_options`], and every provider offering
/// [`Kind::Transcription`] with whether it is ready. Checking presence may
/// read the key store; it never logs or returns the key itself.
pub fn transcription_options(keys: &dyn SecretStore) -> Vec<TranscriptionOption> {
    let mut options: Vec<TranscriptionOption> = local_transcription_options()
        .into_iter()
        .map(TranscriptionOption::Local)
        .collect();
    for provider in providers::PROVIDERS
        .iter()
        .filter(|p| p.offers(Kind::Transcription))
    {
        options.push(TranscriptionOption::Cloud {
            provider,
            availability: cloud_availability(provider, keys),
        });
    }
    options
}

fn cloud_availability(provider: &providers::Provider, keys: &dyn SecretStore) -> CloudAvailability {
    if !cfg!(feature = "cloud-transcription") {
        return CloudAvailability::NotCompiledIn;
    }
    match providers::key_state(keys, provider.id) {
        Ok(providers::KeyState::Saved) => CloudAvailability::Ready,
        Ok(providers::KeyState::Missing) => CloudAvailability::NeedsKey,
        Ok(providers::KeyState::StoreUnavailable(reason)) => {
            CloudAvailability::KeyStoreUnavailable(reason)
        }
        Ok(providers::KeyState::Unreadable(reason)) => CloudAvailability::KeyUnreadable(reason),
        // `provider` came from providers::PROVIDERS itself, so `key_state`
        // cannot fail with UnknownProvider; this is unreached in practice
        // and treated the same as "no key yet" rather than panicking a
        // settings screen.
        Err(_) => CloudAvailability::NeedsKey,
    }
}

/// Whether this build can open any cloud transcription engine at all
/// (the app is complete without one). A caller checks this once instead of noticing that
/// every provider in [`transcription_options`] came back
/// [`CloudAvailability::NotCompiledIn`].
pub fn cloud_transcription_compiled_in() -> bool {
    cfg!(feature = "cloud-transcription")
}

/// One id's measured words wrong on transcription: the everyday, technical
/// and clinical clips of 80, 30 and 64 words (174 total), before the
/// dictionary. From `docs/benchmarks.md`: the local engines are measured in
/// "Canary 180M Flash and Whisper, the small models" and "Nemotron 3.5 ASR
/// streaming 0.6b, against Parakeet and Apple"; the providers are measured
/// in "Cloud transcription (2026-09-11)", with no dictionary sent. One
/// request per clip, so a difference of one word is noise.
///
/// Keyed by [`macalla_engines::registry::EngineInfo::id`] for a local engine
/// or [`providers::Provider::id`] for a provider. OpenRouter has none of its
/// own here -- its transcription models are measured, where they are at
/// all, in [`crate::openrouter`] instead, keyed by OpenRouter's own model
/// ids.
const MEASURED_TRANSCRIPTION: &[(&str, [u16; 3])] = &[
    ("apple-speech", [2, 6, 18]),
    ("parakeet-tdt-v3", [0, 4, 15]),
    ("nemotron-3.5-streaming", [0, 4, 14]),
    ("whisper-small.en", [0, 6, 16]),
    ("whisper-medium.en", [0, 2, 15]),
    ("whisper-medium", [0, 4, 14]),
    ("openai", [0, 3, 8]),
    ("gemini", [0, 4, 9]),
    ("mistral", [0, 4, 14]),
    ("elevenlabs", [0, 0, 5]),
    ("cohere", [0, 4, 13]),
];

/// The measured words-wrong triple for `id`, an engine or provider id, or
/// `None` when nobody has measured it here (OpenRouter, or a future engine).
pub fn measured_transcription(id: &str) -> Option<[u16; 3]> {
    MEASURED_TRANSCRIPTION
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .map(|(_, measured)| *measured)
}

/// One choice a settings screen could offer for Tier 1 cleanup, the
/// counterpart of [`TranscriptionOption`]: switched off, Apple Intelligence,
/// a person's own OpenAI-compatible server, or a cloud provider offering
/// [`Kind::Text`] with its key's state.
#[derive(Debug, Clone)]
pub enum CleanupOption {
    Off,
    /// FoundationModels on this Mac. Carries [`AppleIntelligenceAvailability`]
    /// rather than a bool so the caller can show Apple's own reason when it
    /// is not available.
    AppleIntelligence(AppleIntelligenceAvailability),
    /// An OpenAI-compatible server the person names. Takes no key, so there
    /// is no state to carry yet; the address and model settings are a later
    /// step.
    LocalServer,
    Provider {
        provider: &'static providers::Provider,
        key: providers::KeyState,
    },
}

/// Every cleanup choice this build knows about, in the order a settings
/// screen lists them: off, Apple Intelligence, the local server, then every
/// provider offering [`Kind::Text`] in [`providers::PROVIDERS`] order.
///
/// Unlike [`transcription_options`], none of this is behind the `cloud`
/// feature -- the cleanup adapters (`endpoint`, `anthropic`) are always
/// compiled into this crate, so a cleanup provider never reports
/// not-compiled-in.
pub fn cleanup_options(keys: &dyn SecretStore) -> Vec<CleanupOption> {
    let mut options = vec![
        CleanupOption::Off,
        CleanupOption::AppleIntelligence(AppleIntelligenceAvailability::now()),
        CleanupOption::LocalServer,
    ];
    for provider in providers::PROVIDERS.iter().filter(|p| p.offers(Kind::Text)) {
        let key = match providers::key_state(keys, provider.id) {
            Ok(key) => key,
            // `provider` came from providers::PROVIDERS itself, so this
            // cannot fail with UnknownProvider in practice.
            Err(_) => providers::KeyState::Missing,
        };
        options.push(CleanupOption::Provider { provider, key });
    }
    options
}

/// A plain cleanup choice a person made -- what the app saves and reads
/// back, distinct from [`CleanupOption`], which is the menu a settings page
/// lists. Turned into the [`EnhancerChoice`]s to try, in order, by
/// [`cleanup_enhancers`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupChoice {
    /// A cloud pool: models from providers with a saved key, ticked in any
    /// order. The first ticked model is asked first and the next takes over
    /// when one fails, in the session's failover. Every model runs at the
    /// least reasoning its provider allows, and the text
    /// leaves this machine, so a pool is [`CleanupReach::Remote`].
    Pool {
        models: Vec<PoolModel>,
    },
    Off,
    /// FoundationModels on this Mac, at [`DEFAULT_GUIDED`].
    AppleIntelligence,
    /// A person's own OpenAI-compatible server -- Ollama or otherwise --
    /// reached with no key ([`KeySource::None`]).
    ///
    /// `address` is kept exactly as the person typed it: a bare
    /// `host:port` (Ollama's own default) or an address with a path
    /// (`.../v1`) both work unchanged for both listing and running,
    /// because [`Endpoint::new`] (through [`enhancer`]) and
    /// [`server_models`] resolve the same address the same way -- a bare
    /// host gets `/v1` appended, a path is kept as-is -- through
    /// `macalla_enhancers::endpoint`'s own `api_root` helper. Checked on
    /// 2026-09-12: neither this module nor its caller needs to normalise
    /// the address itself.
    ///
    /// [`Endpoint::new`]: macalla_enhancers::endpoint::Endpoint::new
    Server {
        address: String,
        model: String,
    },
}

/// One ticked model in a [`CleanupChoice::Pool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolModel {
    /// A [`providers::Provider::id`] a pool can hold: OpenAI, Anthropic,
    /// Gemini or Mistral.
    pub provider: String,
    /// The model id, as the provider's own model list names it.
    pub model: String,
}

/// How cleanup reaches a pool provider's models.
enum PoolRoute {
    /// The provider's OpenAI-compatible chat, at the base address the
    /// endpoint adapter appends `/chat/completions` to.
    Compatible(&'static str),
    /// Anthropic's own Messages API.
    Anthropic,
}

/// The providers a pool can hold, and how each is reached. The addresses are
/// the chat endpoints recorded in each provider's documentation; Gemini's
/// is its OpenAI-compatible layer, where its models are also listed natively.
fn pool_route(provider: &str) -> Option<PoolRoute> {
    match provider {
        "openai" => Some(PoolRoute::Compatible("https://api.openai.com/v1")),
        "anthropic" => Some(PoolRoute::Anthropic),
        "gemini" => Some(PoolRoute::Compatible(
            "https://generativelanguage.googleapis.com/v1beta/openai",
        )),
        "mistral" => Some(PoolRoute::Compatible("https://api.mistral.ai/v1")),
        _ => None,
    }
}

/// Whether a pool can hold `provider`'s models. Shared with the cloud pool's
/// model lists, so a page offers exactly the providers a saved pool accepts.
pub(crate) fn pool_can_hold(provider: &str) -> bool {
    pool_route(provider).is_some()
}

/// The [`EnhancerChoice`]s to try, in order, for `choice` -- empty for
/// [`CleanupChoice::Off`]. Never reads a key: a server choice always uses
/// [`KeySource::None`], Apple Intelligence needs none either, and a pool's
/// models name their provider's stored key, read only when one is built.
///
/// A pool gives one choice per ticked model, in ticked order, each at
/// [`Reasoning::Least`]. A model from a provider a pool cannot hold is left
/// out; [`validate_cleanup_choice`] refuses such a pool before it is saved.
pub fn cleanup_enhancers(choice: &CleanupChoice) -> Vec<EnhancerChoice> {
    match choice {
        CleanupChoice::Off => Vec::new(),
        CleanupChoice::AppleIntelligence => {
            vec![EnhancerChoice::AppleIntelligence {
                guided: DEFAULT_GUIDED,
            }]
        }
        CleanupChoice::Server { address, model } => vec![EnhancerChoice::Endpoint {
            url: address.clone(),
            model: model.clone(),
            key: KeySource::None,
            reasoning: None,
        }],
        CleanupChoice::Pool { models } => models
            .iter()
            .filter_map(|ticked| {
                Some(match pool_route(&ticked.provider)? {
                    PoolRoute::Compatible(url) => EnhancerChoice::Endpoint {
                        url: url.to_string(),
                        model: ticked.model.clone(),
                        key: KeySource::Stored(ticked.provider.clone()),
                        reasoning: Some(Reasoning::Least),
                    },
                    PoolRoute::Anthropic => EnhancerChoice::Anthropic {
                        model: ticked.model.clone(),
                        key: KeySource::Stored(ticked.provider.clone()),
                        reasoning: Some(Reasoning::Least),
                    },
                })
            })
            .collect(),
    }
}

/// Refuse a [`CleanupChoice`] before it is saved: [`CleanupChoice::Server`]
/// needs a non-empty address and model; [`CleanupChoice::AppleIntelligence`]
/// is refused with the system's own reason when
/// [`AppleIntelligenceAvailability::now`] is not `Available`.
/// [`CleanupChoice::Off`] is always valid.
///
/// Kept in the session, not the desktop command that calls it:
/// the reason string is a plain fact, not styled UI copy, so the command
/// stays a translator and never re-derives what "needs an address" means.
///
/// By decision, this never makes a network call: a `Server` choice's model
/// id is checked for non-emptiness only, never against what the server at
/// `address` actually offers. The page is the guard against a mismatched
/// pair -- it only ever offers a model from a list it fetched for that same
/// address, through [`server_models`], and never saves one from a stale or
/// superseded list.
pub fn validate_cleanup_choice(choice: &CleanupChoice) -> Result<()> {
    match choice {
        CleanupChoice::Off => Ok(()),
        CleanupChoice::AppleIntelligence => match AppleIntelligenceAvailability::now().explain() {
            Some(reason) => Err(Error::CleanupChoiceUnavailable(reason)),
            None => Ok(()),
        },
        CleanupChoice::Server { address, model } => {
            if address.trim().is_empty() {
                return Err(Error::InvalidCleanupChoice(
                    "a server needs an address".to_string(),
                ));
            }
            if model.trim().is_empty() {
                return Err(Error::InvalidCleanupChoice(
                    "a server needs a model".to_string(),
                ));
            }
            Ok(())
        }
        // Checked for shape only, like a server's model: no key is read and
        // no provider's list is fetched. The page offers only models from a
        // list it fetched with that provider's saved key.
        CleanupChoice::Pool { models } => {
            if models.is_empty() {
                return Err(Error::InvalidCleanupChoice(
                    "an Enhancement pool needs at least one model".to_string(),
                ));
            }
            for (at, ticked) in models.iter().enumerate() {
                if pool_route(&ticked.provider).is_none() {
                    return Err(Error::InvalidCleanupChoice(format!(
                        "{} cannot be in the Enhancement pool",
                        ticked.provider
                    )));
                }
                if ticked.model.trim().is_empty() {
                    return Err(Error::InvalidCleanupChoice(format!(
                        "a {} model in the Enhancement pool needs its id",
                        ticked.provider
                    )));
                }
                if models[..at].contains(ticked) {
                    return Err(Error::InvalidCleanupChoice(format!(
                        "{} from {} is ticked twice",
                        ticked.model, ticked.provider
                    )));
                }
            }
            Ok(())
        }
    }
}

/// One exact model id's measured accuracy on cleanup, from
/// `docs/benchmarks.md`'s "Cloud models for cleanup" comparison table
/// (2026-09-11): the reference run's score out of the 20 fixtures, and the
/// machine it ran on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasuredCleanup {
    pub right: u8,
    pub of: u8,
    pub machine: &'static str,
}

/// `docs/benchmarks.md`, "Cloud models for cleanup" (2026-09-11), the
/// comparison table below the cloud providers: only its two Ollama rows are
/// measured, keyed by the exact model id Ollama reports.
///
/// This is the counterpart of [`crate::openrouter::MEASURED_CLEANUP`], which
/// is keyed by OpenRouter ids instead of a person's own (Ollama) server's.
/// Two tables, one owner (`choice` and `openrouter` are both
/// `macalla-session`), kept apart because the id spaces don't overlap.
const MEASURED_CLEANUP: &[(&str, MeasuredCleanup)] = &[
    (
        "granite4.1:3b",
        MeasuredCleanup {
            right: 17,
            of: 20,
            machine: "M1 Mac mini, 16 GB",
        },
    ),
    (
        "qwen2.5:0.5b",
        MeasuredCleanup {
            right: 13,
            of: 20,
            machine: "M1 Mac mini, 16 GB",
        },
    ),
];

fn measured_cleanup(id: &str) -> Option<MeasuredCleanup> {
    MEASURED_CLEANUP
        .iter()
        .find(|(model, _)| *model == id)
        .map(|(_, measured)| *measured)
}

/// One model a person's own server offers, for the local-server cleanup
/// option: [`macalla_enhancers::endpoint::ServerModel`]'s own fields, plus
/// Macalla's measured accuracy where the model id is one of the two this
/// build has measured.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerModelOffer {
    pub id: String,
    pub size_bytes: Option<u64>,
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub measured: Option<MeasuredCleanup>,
}

/// List the models a person's own OpenAI-compatible server offers, through
/// [`macalla_enhancers::endpoint::list_models`], with Macalla's own measured
/// table attached. The address and the chosen model are not persisted here;
/// that is the page-wiring step's job.
pub fn server_models(address: &str) -> Result<Vec<ServerModelOffer>> {
    let models = macalla_enhancers::endpoint::list_models(address, None)?;
    Ok(models
        .into_iter()
        .map(|m| ServerModelOffer {
            measured: measured_cleanup(&m.id),
            id: m.id,
            size_bytes: m.size_bytes,
            parameter_size: m.parameter_size,
            quantization: m.quantization,
            capabilities: m.capabilities,
        })
        .collect())
}

/// Whether `address` names this machine: `localhost` (with or without the
/// trailing dot a fully-qualified name allows), `127.0.0.0/8`, `[::1]`, or
/// an IPv4-mapped IPv6 loopback (`[::ffff:127.0.0.1]`), with or without a
/// scheme or port. No DNS lookup -- a hostname that happens to resolve to a
/// loopback address is not recognised, since that would need a network round
/// trip just to label a server "Local".
///
/// `0.0.0.0` is deliberately **not** loopback: it is the wildcard bind
/// address ("listen on every interface"), not a specific address a client
/// would connect to, and a server bound to it is not necessarily this
/// machine's own.
///
/// `127.1` (a short form some C libraries accept for `127.0.0.1`) is also
/// deliberately **not** recognised: Rust's own `Ipv4Addr` parser already
/// refuses it, so this falls out of [`loopback_host`] rather than needing a
/// check, and the failure direction is safe -- a local server just gets
/// labelled "Your server" instead of "Local", never the reverse.
///
/// A URL's userinfo (`user:pass@host`) never changes the answer: the host
/// actually connected to is whatever follows the *last* `@`, so
/// `localhost@evil.com` is not loopback (it connects to `evil.com`) and
/// `evil.com@127.0.0.1` is (it connects to `127.0.0.1`), whichever way the
/// trick is written. The authority ends at the first `/`, `?`, `#` or `\`,
/// so an `@` after one of those is not userinfo at all:
/// `http://evil.example?@127.0.0.1` connects to `evil.example`.
pub fn is_loopback(address: &str) -> bool {
    let host = loopback_host(address);
    if host == "localhost" || host == "::1" {
        return true;
    }
    if let Some(mapped) = host.strip_prefix("::ffff:") {
        return mapped
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.octets()[0] == 127);
    }
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.octets()[0] == 127)
}

/// The host part of `address`, lower-cased, with any scheme, userinfo, port,
/// path, query, fragment and trailing dot stripped.
fn loopback_host(address: &str) -> String {
    let trimmed = address.trim();
    let after_scheme = trimmed.split_once("://").map_or(trimmed, |(_, rest)| rest);
    // The authority ends at the first '/', '?' or '#', or at '\', which URL
    // parsers read as '/'. Cut only at '/', an '@' in a query or fragment
    // passed for userinfo, and a remote host read as this machine.
    let host_port = after_scheme
        .split(['/', '?', '#', '\\'])
        .next()
        .unwrap_or("");
    // Authority is [userinfo@]host[:port]; the host is whatever follows the
    // LAST '@' -- see the userinfo note on [`is_loopback`].
    let host_port = host_port.rsplit_once('@').map_or(host_port, |(_, h)| h);
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.find(']').map_or(host_port, |end| &rest[..end])
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            _ => host_port,
        }
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_platform::secrets::MemoryStore;

    #[test]
    fn a_fingerprint_changes_with_the_choice_even_when_its_label_does_not() {
        let server = |url: &str, model: &str| EnhancerChoice::Endpoint {
            url: url.into(),
            model: model.into(),
            key: KeySource::None,
            reasoning: None,
        };
        let shown = [server("http://192.0.2.10:11434", "qwen3:4b")];
        let other_server = [server("http://192.0.2.11:11434", "qwen3:4b")];
        let other_model = [server("http://192.0.2.10:11434", "qwen3:8b")];

        // The label cannot tell these apart.
        assert_eq!(shown[0].destination(), other_server[0].destination());
        assert_eq!(shown[0].destination(), other_model[0].destination());
        assert_eq!(
            fingerprint(&shown),
            fingerprint(&[server("http://192.0.2.10:11434", "qwen3:4b")])
        );
        assert_ne!(fingerprint(&shown), fingerprint(&other_server));
        assert_ne!(fingerprint(&shown), fingerprint(&other_model));
        assert_ne!(fingerprint(&shown), fingerprint(&[]));
    }

    #[test]
    fn a_password_in_an_address_never_reaches_the_fingerprint() {
        let server = |url: &str| EnhancerChoice::Endpoint {
            url: url.into(),
            model: "qwen3:4b".into(),
            key: KeySource::None,
            reasoning: None,
        };

        assert_eq!(
            without_userinfo("http://reader:s3cret@192.0.2.10:11434/v1"),
            "http://192.0.2.10:11434/v1"
        );
        assert!(!described(&server("http://reader:s3cret@192.0.2.10:11434")).contains("s3cret"));
        assert_eq!(
            fingerprint(&[server("http://reader:s3cret@192.0.2.10:11434")]),
            fingerprint(&[server("http://reader:other@192.0.2.10:11434")])
        );
        // The host still counts, however the userinfo is written.
        assert_ne!(
            fingerprint(&[server("http://192.0.2.10@192.0.2.11:11434")]),
            fingerprint(&[server("http://192.0.2.10:11434")])
        );
    }

    #[test]
    fn a_summary_waits_longer_for_apple_intelligence_than_cleanup_does() {
        // Through text_model_for, which Summariser::build uses, so a return to
        // cleanup's limit on this arm fails here.
        let built = text_model_for(
            &EnhancerChoice::AppleIntelligence { guided: true },
            &MemoryStore::new(),
        )
        .unwrap();
        let TextModelKind::Apple(summary) = built else {
            panic!("Apple Intelligence was built as another kind of model");
        };

        assert_eq!(summary.time_limit(), crate::summary::PART_TIME_LIMIT);
        assert!(
            summary.time_limit() > macalla_enhancers::apple::AppleIntelligence::new().time_limit()
        );
    }

    // ------------------------------------------------------------ cloud_engine

    #[cfg(feature = "cloud-transcription")]
    #[test]
    fn every_transcription_provider_opens_the_right_engine_id() {
        let store = MemoryStore::new();
        for (id, engine_id) in [
            ("openai", "cloud-openai"),
            ("mistral", "cloud-mistral"),
            ("cohere", "cloud-cohere"),
            ("openrouter", "cloud-openrouter"),
            ("gemini", "cloud-gemini"),
            ("elevenlabs", "cloud-elevenlabs"),
        ] {
            store.set(&format!("provider-key.{id}"), "sk-test").unwrap();
            let choice = CloudTranscription {
                provider: id.to_string(),
                model: None,
            };
            let engine = cloud_engine(&choice, "en-US", &store).unwrap();
            assert_eq!(engine.id(), engine_id, "{id}");
        }
    }

    #[cfg(feature = "cloud-transcription")]
    #[test]
    fn with_no_key_stored_it_needs_one() {
        let store = MemoryStore::new();
        let choice = CloudTranscription {
            provider: "openai".to_string(),
            model: None,
        };
        assert!(matches!(
            cloud_engine(&choice, "en-US", &store),
            Err(Error::NeedsKey { provider }) if provider == "openai"
        ));
    }

    #[cfg(feature = "cloud-transcription")]
    #[test]
    fn an_unknown_provider_id_is_refused() {
        let store = MemoryStore::new();
        let choice = CloudTranscription {
            provider: "watson".to_string(),
            model: None,
        };
        assert!(matches!(
            cloud_engine(&choice, "en-US", &store),
            Err(Error::UnknownProvider(id)) if id == "watson"
        ));
    }

    #[cfg(feature = "cloud-transcription")]
    #[test]
    fn a_text_only_provider_is_the_wrong_kind_for_transcription() {
        let store = MemoryStore::new();
        store.set("provider-key.anthropic", "sk-test").unwrap();
        let choice = CloudTranscription {
            provider: "anthropic".to_string(),
            model: None,
        };
        assert!(matches!(
            cloud_engine(&choice, "en-US", &store),
            Err(Error::WrongKind { provider, kind: Kind::Transcription }) if provider == "anthropic"
        ));
    }

    #[cfg(not(feature = "cloud-transcription"))]
    #[test]
    fn without_the_cloud_feature_nothing_can_be_built() {
        let store = MemoryStore::new();
        let choice = CloudTranscription {
            provider: "openai".to_string(),
            model: None,
        };
        assert!(matches!(
            cloud_engine(&choice, "en-US", &store),
            Err(Error::CloudNotCompiledIn)
        ));
    }

    // -------------------------------------------------------------- enhancer

    #[test]
    fn apple_intelligence_is_refused_with_its_own_reason_when_unavailable() {
        // `cargo test -p macalla-session` alone never links FoundationModels
        // (Unsupported); `cargo test --workspace` turns macalla-enhancers'
        // apple-intelligence feature on through the apps' default features,
        // by way of this crate's own feature of that name, on macOS, where the
        // reference machine has it switched on. Ask the system
        // itself, rather than assuming either.
        let store = MemoryStore::new();
        let choice = EnhancerChoice::AppleIntelligence { guided: true };
        match macalla_enhancers::apple::Availability::now().explain() {
            Some(reason) => {
                let err = match enhancer(&choice, &store) {
                    Err(e) => e,
                    Ok(_) => panic!("the system said {reason:?} but one was built anyway"),
                };
                assert!(err.to_string().contains(reason), "{err}");
            }
            None => {
                enhancer(&choice, &store).expect("Apple Intelligence reports itself available");
            }
        }
    }

    #[test]
    fn an_endpoint_with_no_key_source_never_touches_the_store() {
        struct Panicking;
        impl SecretStore for Panicking {
            fn get(&self, _: &str) -> macalla_platform::Result<Option<String>> {
                panic!("the store must not be read when the key source is None")
            }
            fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
        }
        let choice = EnhancerChoice::Endpoint {
            url: "http://localhost:11434".to_string(),
            model: "qwen2.5:0.5b".to_string(),
            key: KeySource::None,
            reasoning: None,
        };
        enhancer(&choice, &Panicking).unwrap();
    }

    #[test]
    fn an_endpoint_with_a_stored_key_source_reads_that_providers_key() {
        let store = MemoryStore::new();
        store.set("provider-key.openai", "sk-test").unwrap();
        let choice = EnhancerChoice::Endpoint {
            url: "https://api.openai.com/v1".to_string(),
            model: "gpt-5".to_string(),
            key: KeySource::Stored("openai".to_string()),
            reasoning: None,
        };
        enhancer(&choice, &store).unwrap();
    }

    #[test]
    fn an_endpoint_stored_key_source_with_no_key_stored_needs_one() {
        let store = MemoryStore::new();
        let choice = EnhancerChoice::Endpoint {
            url: "https://api.openai.com/v1".to_string(),
            model: "gpt-5".to_string(),
            key: KeySource::Stored("openai".to_string()),
            reasoning: None,
        };
        assert!(matches!(
            enhancer(&choice, &store),
            Err(Error::NeedsKey { provider }) if provider == "openai"
        ));
    }

    #[test]
    fn an_endpoint_stored_key_source_that_offers_no_text_model_is_the_wrong_kind() {
        let store = MemoryStore::new();
        store.set("provider-key.elevenlabs", "sk-test").unwrap();
        let choice = EnhancerChoice::Endpoint {
            url: "https://example.com".to_string(),
            model: "m".to_string(),
            key: KeySource::Stored("elevenlabs".to_string()),
            reasoning: None,
        };
        assert!(matches!(
            enhancer(&choice, &store),
            Err(Error::WrongKind { provider, kind: Kind::Text }) if provider == "elevenlabs"
        ));
    }

    #[test]
    fn an_endpoint_env_key_source_reads_the_variable() {
        let name = "MACALLA_TEST_ENHANCER_ENDPOINT_ENV_KEY";
        std::env::set_var(name, "sk-from-env");
        // A store that would panic if touched: an Env source must never
        // consult the keychain, only the environment.
        struct Panicking;
        impl SecretStore for Panicking {
            fn get(&self, _: &str) -> macalla_platform::Result<Option<String>> {
                panic!("an Env key source must not read the store")
            }
            fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
        }
        let choice = EnhancerChoice::Endpoint {
            url: "https://example.com".to_string(),
            model: "m".to_string(),
            key: KeySource::Env(name.to_string()),
            reasoning: None,
        };
        enhancer(&choice, &Panicking).unwrap();
        std::env::remove_var(name);
    }

    #[test]
    fn an_endpoint_env_key_source_with_the_variable_unset_names_it_and_leaks_nothing() {
        let name = "MACALLA_TEST_ENHANCER_ENDPOINT_ENV_KEY_UNSET";
        std::env::remove_var(name);
        let store = MemoryStore::new();
        let choice = EnhancerChoice::Endpoint {
            url: "https://example.com".to_string(),
            model: "m".to_string(),
            key: KeySource::Env(name.to_string()),
            reasoning: None,
        };
        let err = match enhancer(&choice, &store) {
            Err(e) => e,
            Ok(_) => panic!("an unset variable must be refused"),
        };
        assert!(err.to_string().contains(name), "{err}");
    }

    #[test]
    fn anthropic_with_a_stored_key_source_reads_the_anthropic_key() {
        let store = MemoryStore::new();
        store.set("provider-key.anthropic", "sk-ant-test").unwrap();
        let choice = EnhancerChoice::Anthropic {
            model: "claude-haiku-4-5".to_string(),
            key: KeySource::Stored("anthropic".to_string()),
            reasoning: None,
        };
        enhancer(&choice, &store).unwrap();
    }

    #[test]
    fn anthropic_with_no_key_stored_needs_one() {
        let store = MemoryStore::new();
        let choice = EnhancerChoice::Anthropic {
            model: "claude-haiku-4-5".to_string(),
            key: KeySource::Stored("anthropic".to_string()),
            reasoning: None,
        };
        assert!(matches!(
            enhancer(&choice, &store),
            Err(Error::NeedsKey { provider }) if provider == "anthropic"
        ));
    }

    #[test]
    fn anthropic_with_an_env_key_source_reads_the_variable() {
        let name = "MACALLA_TEST_ENHANCER_ANTHROPIC_ENV_KEY";
        std::env::set_var(name, "sk-ant-from-env");
        let store = MemoryStore::new();
        let choice = EnhancerChoice::Anthropic {
            model: "claude-haiku-4-5".to_string(),
            key: KeySource::Env(name.to_string()),
            reasoning: None,
        };
        enhancer(&choice, &store).unwrap();
        std::env::remove_var(name);
    }

    #[test]
    fn anthropic_with_no_key_source_at_all_needs_one() {
        // KeySource::None is not a valid choice for Anthropic, which has no
        // keyless mode; the CLI never builds this, but enhancer() must still
        // refuse it rather than construct an adapter with an empty key.
        let store = MemoryStore::new();
        let choice = EnhancerChoice::Anthropic {
            model: "claude-haiku-4-5".to_string(),
            key: KeySource::None,
            reasoning: None,
        };
        assert!(matches!(
            enhancer(&choice, &store),
            Err(Error::NeedsKey { provider }) if provider == "anthropic"
        ));
    }

    // --------------------------------------------------------------- EnvKey

    #[test]
    fn env_key_reads_the_named_variable_for_any_account() {
        let name = "MACALLA_TEST_ENV_KEY_READS";
        // build-less: env vars are process-global, so a unique name per test
        // avoids the tests racing each other; ceiling is this crate's own
        // test binary, which is fine for a handful of tests.
        std::env::set_var(name, "sk-from-env");
        let store = EnvKey::named(name);
        assert_eq!(
            store.get("whatever-account").unwrap().as_deref(),
            Some("sk-from-env")
        );
        std::env::remove_var(name);
    }

    #[test]
    fn env_key_with_the_variable_unset_reads_as_none() {
        let name = "MACALLA_TEST_ENV_KEY_UNSET";
        std::env::remove_var(name);
        let store = EnvKey::named(name);
        assert_eq!(store.get("account").unwrap(), None);
    }

    #[test]
    fn env_key_refuses_to_set_or_delete() {
        let store = EnvKey::named("MACALLA_TEST_ENV_KEY_READ_ONLY");
        assert!(store.set("account", "x").is_err());
        assert!(store.delete("account").is_err());
    }

    // ------------------------------------------------------- transcription_options

    /// A store that always reports itself unreachable, the way [`SystemSecrets`]
    /// does on a platform with no keychain (Linux, today).
    struct Unavailable;
    impl SecretStore for Unavailable {
        fn get(&self, _: &str) -> macalla_platform::Result<Option<String>> {
            Err(macalla_platform::Error::SecretStoreUnavailable(
                "no store on this platform".to_string(),
            ))
        }
        fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
            unreachable!()
        }
        fn delete(&self, _: &str) -> macalla_platform::Result<()> {
            unreachable!()
        }
    }

    #[test]
    fn an_unreachable_store_reports_itself_unavailable_not_needs_key() {
        let availability = cloud_availability(&providers::PROVIDERS[0], &Unavailable);
        if cfg!(feature = "cloud-transcription") {
            assert!(
                matches!(availability, CloudAvailability::KeyStoreUnavailable(ref reason) if reason == "no store on this platform"),
                "{availability:?}"
            );
        } else {
            assert_eq!(availability, CloudAvailability::NotCompiledIn);
        }
    }

    /// A store that is reachable but fails on this one item -- the OS access
    /// prompt was denied, or the entry does not decode. Distinct from
    /// [`Unavailable`], which cannot be reached at all.
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

    #[test]
    fn a_key_that_fails_to_read_is_unreadable_not_needs_key() {
        let availability = cloud_availability(&providers::PROVIDERS[0], &Unreadable);
        if cfg!(feature = "cloud-transcription") {
            assert!(
                matches!(availability, CloudAvailability::KeyUnreadable(ref reason) if reason == "the item could not be decoded"),
                "{availability:?}"
            );
        } else {
            assert_eq!(availability, CloudAvailability::NotCompiledIn);
        }
    }

    #[test]
    fn every_local_engine_is_present() {
        let store = MemoryStore::new();
        let options = transcription_options(&store);
        let local: Vec<_> = options
            .iter()
            .filter_map(|o| match o {
                TranscriptionOption::Local(info) => Some(info.id),
                TranscriptionOption::Cloud { .. } => None,
            })
            .collect();
        let registry_ids: Vec<_> = macalla_engines::registry::engines()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(local, registry_ids);
    }

    #[test]
    fn local_transcription_options_matches_the_local_entries_of_transcription_options() {
        let store = MemoryStore::new();
        let local_ids: Vec<&str> = local_transcription_options().iter().map(|e| e.id).collect();
        let combined_local_ids: Vec<&str> = transcription_options(&store)
            .into_iter()
            .filter_map(|o| match o {
                TranscriptionOption::Local(info) => Some(info.id),
                TranscriptionOption::Cloud { .. } => None,
            })
            .collect();
        assert_eq!(local_ids, combined_local_ids);
    }

    // --------------------------------------------------- measured_transcription

    #[test]
    fn every_measured_transcription_id_is_a_real_engine_or_provider() {
        let engine_ids: Vec<&str> = local_transcription_options().iter().map(|e| e.id).collect();
        for (id, _) in MEASURED_TRANSCRIPTION {
            assert!(
                engine_ids.contains(id) || providers::find(id).is_some(),
                "{id} is not a registered engine or provider"
            );
        }
    }

    #[test]
    fn a_measured_id_reports_its_words_wrong_and_an_unmeasured_one_reports_none() {
        assert_eq!(measured_transcription("apple-speech"), Some([2, 6, 18]));
        assert_eq!(measured_transcription("elevenlabs"), Some([0, 0, 5]));
        assert_eq!(measured_transcription("openrouter"), None);
        assert_eq!(measured_transcription("watson"), None);
    }

    // --------------------------------------------------------- cleanup_options

    #[test]
    fn cleanup_options_start_with_off_apple_intelligence_and_the_local_server() {
        let store = MemoryStore::new();
        let options = cleanup_options(&store);
        assert!(matches!(options[0], CleanupOption::Off));
        assert!(matches!(options[1], CleanupOption::AppleIntelligence(_)));
        assert!(matches!(options[2], CleanupOption::LocalServer));
    }

    #[test]
    fn cleanup_options_apple_intelligence_reports_what_the_build_actually_says() {
        // A headless or non-macOS test build reports Unsupported; the
        // reference machine with Apple Intelligence on reports Available.
        // Ask the system rather than assuming either (as
        // apple_intelligence_is_refused_with_its_own_reason_when_unavailable
        // does above).
        let store = MemoryStore::new();
        let options = cleanup_options(&store);
        let CleanupOption::AppleIntelligence(availability) = options[1] else {
            panic!("expected AppleIntelligence at index 1: {options:?}");
        };
        assert_eq!(availability, AppleIntelligenceAvailability::now());
    }

    #[test]
    fn cleanup_options_providers_are_exactly_the_text_providers_in_table_order() {
        let store = MemoryStore::new();
        let options = cleanup_options(&store);
        let ids: Vec<&str> = options
            .iter()
            .skip(3)
            .map(|o| match o {
                CleanupOption::Provider { provider, .. } => provider.id,
                other => panic!("expected a provider after index 2, got {other:?}"),
            })
            .collect();
        let text_provider_ids: Vec<&str> = providers::PROVIDERS
            .iter()
            .filter(|p| p.offers(Kind::Text))
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, text_provider_ids);
    }

    #[test]
    fn cleanup_options_reports_each_providers_key_state() {
        let store = MemoryStore::new();
        store.set("provider-key.openai", "sk-test").unwrap();
        let options = cleanup_options(&store);
        let key_for = |id: &str| {
            options.iter().find_map(|o| match o {
                CleanupOption::Provider { provider, key } if provider.id == id => Some(key.clone()),
                _ => None,
            })
        };
        assert_eq!(
            key_for("openai").expect("openai is a text provider"),
            providers::KeyState::Saved
        );
        assert_eq!(
            key_for("anthropic").expect("anthropic is a text provider"),
            providers::KeyState::Missing
        );
    }

    #[test]
    fn cleanup_options_reports_an_unreachable_store_for_every_provider() {
        let options = cleanup_options(&Unavailable);
        for option in &options {
            if let CleanupOption::Provider { key, .. } = option {
                assert_eq!(
                    *key,
                    providers::KeyState::StoreUnavailable("no store on this platform".to_string())
                );
            }
        }
    }

    #[test]
    fn cleanup_options_reports_a_key_that_fails_to_read_as_unreadable() {
        let options = cleanup_options(&Unreadable);
        let openai_key = options.iter().find_map(|o| match o {
            CleanupOption::Provider { provider, key } if provider.id == "openai" => {
                Some(key.clone())
            }
            _ => None,
        });
        assert_eq!(
            openai_key.expect("openai is a text provider"),
            providers::KeyState::Unreadable("the item could not be decoded".to_string())
        );
    }

    #[test]
    fn a_provider_with_a_stored_key_is_ready_and_one_without_needs_one() {
        let store = MemoryStore::new();
        store.set("provider-key.openai", "sk-test").unwrap();
        let options = transcription_options(&store);
        let availability = |id: &str| {
            options.iter().find_map(|o| match o {
                TranscriptionOption::Cloud {
                    provider,
                    availability,
                } if provider.id == id => Some(availability.clone()),
                _ => None,
            })
        };
        let openai = availability("openai").expect("openai is a transcription provider");
        let cohere = availability("cohere").expect("cohere is a transcription provider");
        if cfg!(feature = "cloud-transcription") {
            assert_eq!(openai, CloudAvailability::Ready);
            assert_eq!(cohere, CloudAvailability::NeedsKey);
        } else {
            assert_eq!(openai, CloudAvailability::NotCompiledIn);
            assert_eq!(cohere, CloudAvailability::NotCompiledIn);
        }
    }

    // ---------------------------------------------------------- cleanup reach

    #[test]
    fn cleanup_that_stays_on_this_machine_is_local_and_anything_else_is_remote() {
        let endpoint = |url: &str, key: KeySource| EnhancerChoice::Endpoint {
            url: url.into(),
            model: "m".into(),
            key,
            reasoning: None,
        };
        for (picked, reach, destination) in [
            (
                EnhancerChoice::AppleIntelligence { guided: true },
                CleanupReach::Local,
                "Apple Intelligence",
            ),
            (
                endpoint("http://localhost:11434", KeySource::None),
                CleanupReach::Local,
                "Local",
            ),
            (
                endpoint("127.0.0.1:1234/v1", KeySource::None),
                CleanupReach::Local,
                "Local",
            ),
            (
                endpoint("http://10.0.0.5:11434", KeySource::None),
                CleanupReach::Remote,
                "Your server",
            ),
            (
                endpoint(
                    "https://api.openai.com/v1",
                    KeySource::Stored("openai".into()),
                ),
                CleanupReach::Remote,
                "OpenAI",
            ),
            (
                EnhancerChoice::Anthropic {
                    model: "claude-haiku-4-5".into(),
                    key: KeySource::Stored("anthropic".into()),
                    reasoning: None,
                },
                CleanupReach::Remote,
                "Anthropic",
            ),
        ] {
            assert_eq!(picked.reach(), reach, "{picked:?}");
            assert_eq!(picked.destination(), destination, "{picked:?}");
        }
    }

    #[test]
    fn a_stored_key_names_its_provider_only_at_that_providers_own_address() {
        let with_key = |url: &str, id: &str| EnhancerChoice::Endpoint {
            url: url.into(),
            model: "m".into(),
            key: KeySource::Stored(id.into()),
            reasoning: None,
        };
        for (url, id) in [
            ("https://api.openai.com/v1", "openai"),
            (
                "https://generativelanguage.googleapis.com/v1beta/openai",
                "gemini",
            ),
            ("https://api.mistral.ai/v1", "mistral"),
        ] {
            let name = providers::find(id).expect("a provider").name;
            assert_eq!(with_key(url, id).destination(), name, "{url}");
        }

        // OpenAI's key, but the words and the key go to another host: the label
        // names that host, and never the address's user, password or port.
        let elsewhere =
            with_key("https://reader:hunter2@llm.example.net:8443/v1", "openai").destination();
        assert_eq!(elsewhere, "Your server at llm.example.net");

        // A password holding a `?` fools the host reader into cutting the
        // address early. Whatever it would print is not a plain host, so the
        // label says no more than "Your server".
        for url in [
            "https://reader:pa?ss@llm.example.net/v1",
            "https://reader:1234?x@llm.example.net/v1",
        ] {
            let label = with_key(url, "openai").destination();
            for secret in ["reader", "pa", "1234"] {
                assert!(!label.contains(secret), "{url}: {label}");
            }
        }
    }

    #[test]
    fn provider_id_names_a_provider_only_where_the_destination_does() {
        let at = |url: &str, key: KeySource| EnhancerChoice::Endpoint {
            url: url.into(),
            model: "m".into(),
            key,
            reasoning: None,
        };
        assert_eq!(
            at("https://api.openai.com/v1", KeySource::None).provider_id(),
            Some("openai")
        );
        assert_eq!(
            at(
                "https://api.mistral.ai/v1",
                KeySource::Stored("anthropic".into())
            )
            .provider_id(),
            Some("mistral")
        );
        assert_eq!(
            EnhancerChoice::Anthropic {
                model: "m".into(),
                key: KeySource::None,
                reasoning: None,
            }
            .provider_id(),
            Some("anthropic")
        );
        assert_eq!(
            EnhancerChoice::AppleIntelligence { guided: true }.provider_id(),
            None
        );
        assert_eq!(
            at("http://127.0.0.1:11434/v1", KeySource::None).provider_id(),
            None
        );
        assert_eq!(
            at(
                "https://llm.example.net/v1",
                KeySource::Stored("openai".into())
            )
            .provider_id(),
            None
        );
        // Every provider id named is one the provider table knows.
        for url in [
            "https://api.openai.com/v1",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "https://api.mistral.ai/v1",
        ] {
            let id = at(url, KeySource::None).provider_id().expect("a provider");
            assert_eq!(
                providers::find(id).map(|p| p.name),
                Some(at(url, KeySource::None).destination().as_ref())
            );
        }
    }

    #[test]
    fn a_providers_own_address_is_named_whatever_the_key_source() {
        // Found running cloud summaries with `--key-env`: a key from an
        // environment variable at OpenAI's own address was labelled "Your
        // server", and the label is what says where the words go.
        let at = |url: &str, key: KeySource| EnhancerChoice::Endpoint {
            url: url.into(),
            model: "m".into(),
            key,
            reasoning: None,
        };
        for (url, id) in [
            ("https://api.openai.com/v1", "openai"),
            (
                "https://generativelanguage.googleapis.com/v1beta/openai",
                "gemini",
            ),
            ("https://api.mistral.ai/v1", "mistral"),
        ] {
            let name = providers::find(id).expect("a provider").name;
            for key in [
                KeySource::Env("MACALLA_TEST_LABEL_KEY".into()),
                KeySource::Stored(id.into()),
                // Another provider's stored key, sent to this provider: the
                // words still go to this provider.
                KeySource::Stored("anthropic".into()),
                KeySource::None,
            ] {
                assert_eq!(at(url, key.clone()).destination(), name, "{url} {key:?}");
            }
        }

        // Any key at another host names that host, and never the address's
        // user, password or port.
        let elsewhere = at(
            "https://reader:hunter2@llm.example.net:8443/v1",
            KeySource::Env("MACALLA_TEST_LABEL_KEY".into()),
        );
        assert_eq!(elsewhere.destination(), "Your server at llm.example.net");
        // No key at another host says no more than it did.
        assert_eq!(
            at("https://llm.example.net/v1", KeySource::None).destination(),
            "Your server"
        );

        // An authority holding `?`, `#` or `\` is never named, with any key,
        // even when what the host reader sees is a provider's own host.
        for url in [
            "https://reader:pa?ss@llm.example.net/v1",
            "https://reader:1234#x@llm.example.net/v1",
            "https://api.openai.com?x@llm.example.net/v1",
        ] {
            for key in [
                KeySource::Env("MACALLA_TEST_LABEL_KEY".into()),
                KeySource::Stored("openai".into()),
            ] {
                let label = at(url, key.clone()).destination();
                assert_eq!(label, "Your server", "{url} {key:?}");
            }
        }

        // Named by the exact host: a look-alike host, a provider's host in the
        // path, or a provider's host written as the user name is not that
        // provider. Checked with OpenAI's own stored key too, where naming
        // OpenAI would hide that its key goes to someone else's server.
        for (url, host) in [
            (
                "https://api.openai.com.evil.example/v1",
                "api.openai.com.evil.example",
            ),
            ("https://evil.example/api.openai.com/v1", "evil.example"),
            ("https://api.openai.com@evil.example/v1", "evil.example"),
        ] {
            for key in [
                KeySource::Env("MACALLA_TEST_LABEL_KEY".into()),
                KeySource::Stored("openai".into()),
            ] {
                assert_eq!(
                    at(url, key.clone()).destination(),
                    format!("Your server at {host}"),
                    "{url} {key:?}"
                );
            }
        }
        // The same host written with a user, a port, capitals or a trailing
        // dot is still that provider's, which is where the words go.
        assert_eq!(
            at("https://user@API.OPENAI.COM.:8443/v1", KeySource::None).destination(),
            "OpenAI"
        );
    }

    #[test]
    fn a_session_keeps_apple_intelligence_when_it_is_not_available_and_says_why() {
        // Construction only: nothing here asks the model anything.
        let (built, not_now) = session_enhancer(
            &EnhancerChoice::AppleIntelligence { guided: true },
            &MemoryStore::new(),
        )
        .unwrap_or_else(|e| panic!("never refused for availability: {e}"));
        assert_eq!(built.caps().id, "apple-intelligence");
        assert_eq!(not_now, AppleIntelligenceAvailability::now().explain());
    }

    // ------------------------------------------------------ cleanup_enhancers

    #[test]
    fn off_tries_nothing() {
        assert!(cleanup_enhancers(&CleanupChoice::Off).is_empty());
    }

    #[test]
    fn apple_intelligence_is_tried_as_plain_text() {
        let enhancers = cleanup_enhancers(&CleanupChoice::AppleIntelligence);
        assert!(matches!(
            enhancers.as_slice(),
            [EnhancerChoice::AppleIntelligence { guided: false }]
        ));
    }

    #[test]
    fn a_server_choice_is_tried_as_a_keyless_endpoint() {
        let choice = CleanupChoice::Server {
            address: "http://localhost:11434".to_string(),
            model: "granite4.1:3b".to_string(),
        };
        let enhancers = cleanup_enhancers(&choice);
        match enhancers.as_slice() {
            [EnhancerChoice::Endpoint {
                url,
                model,
                key,
                reasoning,
            }] => {
                assert_eq!(url, "http://localhost:11434");
                assert_eq!(model, "granite4.1:3b");
                assert!(matches!(key, KeySource::None));
                assert!(reasoning.is_none());
            }
            other => panic!("expected one keyless Endpoint choice, got {other:?}"),
        }
    }

    #[test]
    fn a_server_choices_enhancer_never_touches_the_key_store() {
        // The mapping promises no key is read for a server choice; building
        // the enhancer it produces must not touch the store at all, the same
        // guarantee `an_endpoint_with_no_key_source_never_touches_the_store`
        // checks directly against `KeySource::None`.
        struct Panicking;
        impl SecretStore for Panicking {
            fn get(&self, _: &str) -> macalla_platform::Result<Option<String>> {
                panic!("a server cleanup choice must not read the store")
            }
            fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
        }
        let choice = CleanupChoice::Server {
            address: "localhost:11434".to_string(),
            model: "qwen2.5:0.5b".to_string(),
        };
        let enhancers = cleanup_enhancers(&choice);
        enhancer(&enhancers[0], &Panicking).unwrap();
    }

    // -------------------------------------------------- validate_cleanup_choice

    #[test]
    fn off_is_always_valid() {
        validate_cleanup_choice(&CleanupChoice::Off).unwrap();
    }

    #[test]
    fn apple_intelligence_validates_against_what_the_system_actually_says() {
        // Same reasoning as apple_intelligence_is_refused_with_its_own_reason_when_unavailable:
        // ask the system rather than assuming either state.
        match AppleIntelligenceAvailability::now().explain() {
            Some(reason) => {
                let err = validate_cleanup_choice(&CleanupChoice::AppleIntelligence).unwrap_err();
                assert!(err.to_string().contains(reason), "{err}");
            }
            None => validate_cleanup_choice(&CleanupChoice::AppleIntelligence).unwrap(),
        }
    }

    #[test]
    fn a_server_choice_needs_a_non_empty_address() {
        let choice = CleanupChoice::Server {
            address: "  ".to_string(),
            model: "granite4.1:3b".to_string(),
        };
        let err = validate_cleanup_choice(&choice).unwrap_err();
        assert!(err.to_string().contains("address"), "{err}");
    }

    #[test]
    fn a_server_choice_needs_a_non_empty_model() {
        let choice = CleanupChoice::Server {
            address: "http://localhost:11434".to_string(),
            model: "".to_string(),
        };
        let err = validate_cleanup_choice(&choice).unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
    }

    #[test]
    fn a_server_choice_with_an_address_and_model_is_valid() {
        let choice = CleanupChoice::Server {
            address: "http://localhost:11434".to_string(),
            model: "granite4.1:3b".to_string(),
        };
        validate_cleanup_choice(&choice).unwrap();
    }

    // ---------------------------------------------------------- is_loopback

    #[test]
    fn localhost_with_or_without_a_scheme_and_port_is_loopback() {
        for address in ["localhost", "localhost:11434", "http://localhost:11434/v1"] {
            assert!(is_loopback(address), "{address}");
        }
    }

    #[test]
    fn any_127_address_with_or_without_a_scheme_and_port_is_loopback() {
        for address in [
            "127.0.0.1",
            "127.0.0.1:11434",
            "http://127.0.0.1:11434/v1",
            "127.5.6.7",
        ] {
            assert!(is_loopback(address), "{address}");
        }
    }

    #[test]
    fn ipv6_loopback_with_or_without_a_scheme_and_port_is_loopback() {
        for address in ["[::1]", "[::1]:11434", "http://[::1]:11434/v1"] {
            assert!(is_loopback(address), "{address}");
        }
    }

    #[test]
    fn the_wildcard_bind_address_is_not_loopback() {
        for address in ["0.0.0.0", "0.0.0.0:11434", "http://0.0.0.0:11434/v1"] {
            assert!(!is_loopback(address), "{address}");
        }
    }

    #[test]
    fn a_remote_host_is_not_loopback() {
        for address in [
            "10.0.0.5:11434",
            "http://my-box.local:11434/v1",
            "https://api.openai.com/v1",
        ] {
            assert!(!is_loopback(address), "{address}");
        }
    }

    #[test]
    fn localhost_with_a_trailing_dot_is_loopback() {
        for address in ["localhost.", "localhost.:11434", "http://localhost./v1"] {
            assert!(is_loopback(address), "{address}");
        }
    }

    #[test]
    fn an_ipv4_mapped_ipv6_loopback_address_is_loopback() {
        for address in [
            "[::ffff:127.0.0.1]",
            "[::ffff:127.0.0.1]:11434",
            "http://[::ffff:127.5.6.7]:11434/v1",
        ] {
            assert!(is_loopback(address), "{address}");
        }
    }

    #[test]
    fn the_short_127_1_form_is_deliberately_not_recognised() {
        // Documenting existing behaviour, not adding a check: Rust's own
        // Ipv4Addr parser already refuses this shorthand (unlike C's
        // inet_aton), so loopback_host's parse simply fails and is_loopback
        // falls through to false. Failing to recognise a loopback address
        // only mislabels a local server "Your server" instead of "Local" --
        // never the other way round.
        assert!(!is_loopback("127.1"));
    }

    #[test]
    fn userinfo_in_the_address_never_changes_which_host_is_checked() {
        // `localhost@evil.com` really connects to evil.com, using
        // "localhost" as a (nonsensical) username: not loopback.
        assert!(!is_loopback("http://localhost@evil.com"));
        // `evil.com@127.0.0.1` really connects to 127.0.0.1, using
        // "evil.com" as the username: this one genuinely is loopback, and
        // reading the trick as "contains evil.com, so remote" would be
        // backwards -- the address after the last '@' is what a client
        // actually dials.
        assert!(is_loopback("http://evil.com@127.0.0.1"));
    }

    #[test]
    fn an_at_sign_after_a_query_fragment_backslash_or_path_is_not_userinfo() {
        // Each of these dials evil.example: the authority ended before the
        // '@', and what follows it is a query, a fragment or a path.
        for address in [
            "http://evil.example?@127.0.0.1",
            "http://evil.example#@127.0.0.1",
            "http://evil.example\\@127.0.0.1",
            "http://evil.example/@127.0.0.1",
            "http://evil.example?@localhost",
            "evil.example#@[::1]",
        ] {
            assert!(!is_loopback(address), "{address}");
        }
    }

    #[test]
    fn a_loopback_host_followed_by_a_query_fragment_or_backslash_is_still_loopback() {
        for address in [
            "http://127.0.0.1:11434?probe=1",
            "http://localhost#models",
            "http://[::1]:11434\\v1",
            "localhost:11434?x",
            "http://user@127.0.0.1:11434?next=@evil.example",
        ] {
            assert!(is_loopback(address), "{address}");
        }
    }

    // -------------------------------------------------------- server_models

    /// A single-request fake server, so [`server_models`] is exercised
    /// against a real socket without ever leaving 127.0.0.1. Mirrors the
    /// shape `macalla-enhancers`' own fixed-response test server answers
    /// with -- see `crates/macalla-enhancers/src/test_support.rs` -- but
    /// this crate has no access to that private module, so this is a
    /// minimal one just for the pass-through below.
    fn respond_once(status: &str, body: String) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        address
    }

    #[test]
    fn server_models_attaches_the_measured_table_by_exact_model_id() {
        let address = respond_once(
            "200 OK",
            serde_json::json!({"object": "list", "data": [
                {"id": "granite4.1:3b", "object": "model", "created": 1, "owned_by": "library"},
                {"id": "some-other-model", "object": "model", "created": 1, "owned_by": "library"},
            ]})
            .to_string(),
        );
        let offers = server_models(&address).unwrap();
        let measured = |id: &str| offers.iter().find(|o| o.id == id).unwrap().measured;
        assert_eq!(
            measured("granite4.1:3b"),
            Some(MeasuredCleanup {
                right: 17,
                of: 20,
                machine: "M1 Mac mini, 16 GB",
            })
        );
        assert_eq!(measured("some-other-model"), None);
    }

    #[test]
    fn server_models_reports_an_unreachable_address_as_an_error() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert!(server_models(&address).is_err());
    }

    fn pool(models: &[(&str, &str)]) -> CleanupChoice {
        CleanupChoice::Pool {
            models: models
                .iter()
                .map(|(provider, model)| PoolModel {
                    provider: provider.to_string(),
                    model: model.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_pool_gives_one_choice_per_ticked_model_in_order_at_the_least_reasoning() {
        let choices = cleanup_enhancers(&pool(&[
            ("mistral", "mistral-test"),
            ("anthropic", "claude-test"),
            ("gemini", "gemini-test"),
            ("openai", "gpt-test"),
        ]));
        assert_eq!(choices.len(), 4, "{choices:?}");

        let compatible =
            |choice: &EnhancerChoice, want_url: &str, want_model: &str, want_id: &str| match choice
            {
                EnhancerChoice::Endpoint {
                    url,
                    model,
                    key: KeySource::Stored(id),
                    reasoning,
                } => {
                    assert_eq!(url, want_url);
                    assert_eq!(model, want_model);
                    assert_eq!(id, want_id);
                    assert_eq!(*reasoning, Some(Reasoning::Least));
                }
                other => panic!("expected {want_id} through its compatible chat, got {other:?}"),
            };
        compatible(
            &choices[0],
            "https://api.mistral.ai/v1",
            "mistral-test",
            "mistral",
        );
        match &choices[1] {
            EnhancerChoice::Anthropic {
                model,
                key: KeySource::Stored(id),
                reasoning,
            } => {
                assert_eq!(model, "claude-test");
                assert_eq!(id, "anthropic");
                assert_eq!(*reasoning, Some(Reasoning::Least));
            }
            other => panic!("expected Anthropic's own API, got {other:?}"),
        }
        compatible(
            &choices[2],
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "gemini-test",
            "gemini",
        );
        compatible(
            &choices[3],
            "https://api.openai.com/v1",
            "gpt-test",
            "openai",
        );

        for choice in &choices {
            assert_eq!(choice.reach(), CleanupReach::Remote, "{choice:?}");
        }
    }

    #[test]
    fn a_pool_is_refused_empty_or_with_an_unknown_provider_a_blank_model_or_a_model_twice() {
        let refused = |choice: CleanupChoice| match validate_cleanup_choice(&choice) {
            Err(Error::InvalidCleanupChoice(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(refused(pool(&[])).contains("at least one model"));
        assert!(
            refused(pool(&[("openai", "gpt-test"), ("elevenlabs", "scribe")]))
                .contains("elevenlabs cannot be in the Enhancement pool")
        );
        assert!(refused(pool(&[("openai", "  ")])).contains("needs its id"));
        assert!(refused(pool(&[
            ("openai", "gpt-test"),
            ("anthropic", "claude-test"),
            ("openai", "gpt-test"),
        ]))
        .contains("ticked twice"));

        assert!(validate_cleanup_choice(&pool(&[
            ("openai", "gpt-test"),
            ("anthropic", "claude-test")
        ]))
        .is_ok());
        assert!(
            cleanup_enhancers(&pool(&[("elevenlabs", "scribe")])).is_empty(),
            "a provider a pool cannot hold gives no choice"
        );
    }
}
