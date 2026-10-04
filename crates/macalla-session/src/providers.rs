// SPDX-License-Identifier: MIT
//! The cloud providers that take a person's own key: which ones, what kind
//! of model each offers, and moving a key in and out of the OS keychain.
//!
//! The design names the transcription providers and, for Tier 1 step 3, the
//! text ones; a provider offering both kinds is listed once, so a key entered
//! for it covers every kind it offers ("a person adds a provider's key once
//! and its models appear under each kind").
//! Local Ollama is not here -- it takes no key, so there is nothing to
//! store.
//!
//! Keys live in the OS keychain, behind `macalla-platform`; an
//! adapter takes the key as a value and never reads the keychain itself.
//! This module is that read, through [`macalla_platform::secrets`] -- not
//! which provider is chosen, not which models it offers, and not a
//! registry. Those come later, once an adapter exists to choose one for.

use macalla_platform::secrets::SecretStore;

/// A kind of model a provider's key can be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Enhancement, Tier 1 step 3: a chat/completion endpoint.
    Text,
    /// Cloud transcription: a batch speech-to-text
    /// endpoint.
    Transcription,
}

/// A cloud provider that takes the person's own key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    /// Stable, never renamed once shipped: settings and history key on it.
    pub id: &'static str,
    /// Card copy.
    pub name: &'static str,
    /// What this provider's key is good for here.
    pub kinds: &'static [Kind],
    /// The keychain account name its key is stored under, within the one
    /// service [`macalla_platform::secrets::SERVICE`] the CLI and the
    /// desktop app share.
    pub account: &'static str,
}

impl Provider {
    /// Whether this provider offers a model of `kind`.
    pub fn offers(&self, kind: Kind) -> bool {
        self.kinds.contains(&kind)
    }
}

/// Every provider that takes a key. OpenAI, Gemini, Mistral and OpenRouter
/// offer both kinds; Anthropic and Ollama Cloud are text only; ElevenLabs
/// and Cohere are transcription only.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "openai",
        name: "OpenAI",
        kinds: &[Kind::Text, Kind::Transcription],
        account: "provider-key.openai",
    },
    Provider {
        id: "anthropic",
        name: "Anthropic",
        kinds: &[Kind::Text],
        account: "provider-key.anthropic",
    },
    Provider {
        id: "gemini",
        name: "Gemini",
        kinds: &[Kind::Text, Kind::Transcription],
        account: "provider-key.gemini",
    },
    Provider {
        id: "mistral",
        name: "Mistral",
        kinds: &[Kind::Text, Kind::Transcription],
        account: "provider-key.mistral",
    },
    Provider {
        id: "ollama-cloud",
        name: "Ollama Cloud",
        kinds: &[Kind::Text],
        account: "provider-key.ollama-cloud",
    },
    Provider {
        id: "openrouter",
        name: "OpenRouter",
        kinds: &[Kind::Text, Kind::Transcription],
        account: "provider-key.openrouter",
    },
    Provider {
        id: "elevenlabs",
        name: "ElevenLabs",
        kinds: &[Kind::Transcription],
        account: "provider-key.elevenlabs",
    },
    Provider {
        id: "cohere",
        name: "Cohere",
        kinds: &[Kind::Transcription],
        account: "provider-key.cohere",
    },
];

/// The provider by id, if it is one of the ones above.
pub fn find(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

fn lookup(id: &str) -> crate::Result<&'static Provider> {
    find(id).ok_or_else(|| crate::Error::UnknownProvider(id.to_string()))
}

/// Store `key` for provider `id` in `store`, replacing whatever was there.
///
/// Surrounding whitespace is trimmed first. What is left is refused, and
/// nothing is stored, if it is empty or spans more than one line -- the one
/// validation rule the CLI and the desktop app share, so a bad paste is
/// caught here rather than twice, or not at all in whichever caller forgot.
pub fn store_key(store: &dyn SecretStore, id: &str, key: &str) -> crate::Result<()> {
    let provider = lookup(id)?;
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::InvalidKey {
            provider: provider.id.to_string(),
            reason: "was empty",
        });
    }
    if trimmed.contains(['\n', '\r']) {
        return Err(crate::Error::InvalidKey {
            provider: provider.id.to_string(),
            reason: "must be a single line",
        });
    }
    Ok(store.set(provider.account, trimmed)?)
}

/// The key stored for provider `id`, or `None` if none has been entered.
pub fn read_key(store: &dyn SecretStore, id: &str) -> crate::Result<Option<String>> {
    let provider = lookup(id)?;
    Ok(store.get(provider.account)?)
}

/// Whether a key is stored for provider `id`, without handing it to the
/// caller. The Models page shows presence, never the value.
pub fn has_key(store: &dyn SecretStore, id: &str) -> crate::Result<bool> {
    Ok(read_key(store, id)?.is_some())
}

/// The state a provider's key is in, one read of the store away from what a
/// settings screen or the CLI's `key list` shows.
///
/// Distinguishes [`KeyState::Missing`] (nothing entered) from
/// [`KeyState::Unreadable`] (an entry exists but reading it failed -- the OS
/// access prompt was denied, or the item does not decode) so a caller never
/// offers "add a key" over one that already exists, which would overwrite
/// it. [`KeyState::StoreUnavailable`] is the store itself failing to open at
/// all (no keychain on this platform yet, or the OS refused it), reported
/// once rather than per provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    Saved,
    Missing,
    StoreUnavailable(String),
    Unreadable(String),
}

/// The state of provider `id`'s key, folding every error [`SecretStore::get`]
/// can return into a [`KeyState`] rather than a caller matching on
/// [`macalla_platform::Error`] itself. The `Result` here only ever fails with
/// [`crate::Error::UnknownProvider`] -- every other error `has_key` can
/// return is a `KeyState` on its own.
///
/// The desktop's `provider_keys_view`, the CLI's `key_status` and
/// [`crate::choice::cloud_availability`] used to each carry this mapping
/// themselves; this is the one place it is made.
pub fn key_state(store: &dyn SecretStore, id: &str) -> crate::Result<KeyState> {
    match has_key(store, id) {
        Ok(true) => Ok(KeyState::Saved),
        Ok(false) => Ok(KeyState::Missing),
        Err(crate::Error::Platform(macalla_platform::Error::SecretStoreUnavailable(reason))) => {
            Ok(KeyState::StoreUnavailable(reason))
        }
        Err(crate::Error::Platform(macalla_platform::Error::Secret { reason, .. })) => {
            Ok(KeyState::Unreadable(reason))
        }
        Err(e) => Err(e),
    }
}

/// Remove the key stored for provider `id`. Removing one that was never
/// stored is not an error -- see [`SecretStore::delete`].
pub fn remove_key(store: &dyn SecretStore, id: &str) -> crate::Result<()> {
    let provider = lookup(id)?;
    Ok(store.delete(provider.account)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_platform::secrets::MemoryStore;

    #[test]
    fn a_key_stored_is_read_back() {
        let store = MemoryStore::new();
        store_key(&store, "openai", "sk-test-key").unwrap();
        assert_eq!(
            read_key(&store, "openai").unwrap().as_deref(),
            Some("sk-test-key")
        );
    }

    #[test]
    fn no_key_stored_reads_as_none() {
        let store = MemoryStore::new();
        assert_eq!(read_key(&store, "anthropic").unwrap(), None);
    }

    #[test]
    fn removing_a_key_makes_it_unreadable() {
        let store = MemoryStore::new();
        store_key(&store, "mistral", "sk-test-key").unwrap();
        remove_key(&store, "mistral").unwrap();
        assert_eq!(read_key(&store, "mistral").unwrap(), None);
    }

    #[test]
    fn removing_a_key_that_was_never_stored_is_ok() {
        let store = MemoryStore::new();
        assert!(remove_key(&store, "cohere").is_ok());
    }

    #[test]
    fn an_unknown_provider_id_is_an_error_not_a_shrug() {
        let store = MemoryStore::new();
        assert!(matches!(
            store_key(&store, "watson", "x"),
            Err(crate::Error::UnknownProvider(id)) if id == "watson"
        ));
        assert!(read_key(&store, "watson").is_err());
        assert!(remove_key(&store, "watson").is_err());
    }

    #[test]
    fn different_providers_keys_do_not_collide() {
        let store = MemoryStore::new();
        store_key(&store, "openai", "openai-key").unwrap();
        store_key(&store, "gemini", "gemini-key").unwrap();
        assert_eq!(
            read_key(&store, "openai").unwrap().as_deref(),
            Some("openai-key")
        );
        assert_eq!(
            read_key(&store, "gemini").unwrap().as_deref(),
            Some("gemini-key")
        );
    }

    #[test]
    fn local_ollama_takes_no_key_and_is_not_listed() {
        assert!(find("ollama").is_none());
        assert!(find("ollama-cloud").is_some());
    }

    #[test]
    fn text_providers_match_spec_section_6() {
        let text: Vec<&str> = PROVIDERS
            .iter()
            .filter(|p| p.offers(Kind::Text))
            .map(|p| p.id)
            .collect();
        assert_eq!(
            text,
            vec![
                "openai",
                "anthropic",
                "gemini",
                "mistral",
                "ollama-cloud",
                "openrouter"
            ]
        );
    }

    #[test]
    fn transcription_providers_match_spec_section_4() {
        let transcription: Vec<&str> = PROVIDERS
            .iter()
            .filter(|p| p.offers(Kind::Transcription))
            .map(|p| p.id)
            .collect();
        assert_eq!(
            transcription,
            vec![
                "openai",
                "gemini",
                "mistral",
                "openrouter",
                "elevenlabs",
                "cohere"
            ]
        );
    }

    #[test]
    fn has_key_is_false_before_true_after_and_false_again_after_removal() {
        let store = MemoryStore::new();
        assert!(!has_key(&store, "openai").unwrap());
        store_key(&store, "openai", "sk-test-key").unwrap();
        assert!(has_key(&store, "openai").unwrap());
        remove_key(&store, "openai").unwrap();
        assert!(!has_key(&store, "openai").unwrap());
    }

    #[test]
    fn has_key_for_an_unknown_provider_id_is_an_error() {
        let store = MemoryStore::new();
        assert!(matches!(
            has_key(&store, "watson"),
            Err(crate::Error::UnknownProvider(id)) if id == "watson"
        ));
    }

    #[test]
    fn an_empty_key_is_refused_and_nothing_is_stored() {
        let store = MemoryStore::new();
        assert!(matches!(
            store_key(&store, "openai", ""),
            Err(crate::Error::InvalidKey { provider, .. }) if provider == "openai"
        ));
        assert!(!has_key(&store, "openai").unwrap());
    }

    #[test]
    fn a_whitespace_only_key_is_refused_and_nothing_is_stored() {
        let store = MemoryStore::new();
        assert!(matches!(
            store_key(&store, "openai", "   \t  "),
            Err(crate::Error::InvalidKey { provider, .. }) if provider == "openai"
        ));
        assert!(!has_key(&store, "openai").unwrap());
    }

    #[test]
    fn a_multi_line_key_is_refused_and_nothing_is_stored() {
        let store = MemoryStore::new();
        assert!(matches!(
            store_key(&store, "openai", "sk-one\nsk-two"),
            Err(crate::Error::InvalidKey { provider, .. }) if provider == "openai"
        ));
        assert!(!has_key(&store, "openai").unwrap());
    }

    #[test]
    fn a_key_with_surrounding_whitespace_is_trimmed_before_storing() {
        let store = MemoryStore::new();
        store_key(&store, "openai", "  sk-test-key  \r\n").unwrap();
        assert_eq!(
            read_key(&store, "openai").unwrap().as_deref(),
            Some("sk-test-key")
        );
    }

    // ------------------------------------------------------------ key_state

    #[test]
    fn key_state_is_missing_before_saved_and_saved_after() {
        let store = MemoryStore::new();
        assert_eq!(key_state(&store, "openai").unwrap(), KeyState::Missing);
        store_key(&store, "openai", "sk-test-key").unwrap();
        assert_eq!(key_state(&store, "openai").unwrap(), KeyState::Saved);
    }

    #[test]
    fn key_state_for_an_unknown_provider_id_is_an_error() {
        let store = MemoryStore::new();
        assert!(matches!(
            key_state(&store, "watson"),
            Err(crate::Error::UnknownProvider(id)) if id == "watson"
        ));
    }

    /// A store that cannot be reached at all -- no keychain on this
    /// platform, or the OS refused it outright.
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
    fn key_state_reports_an_unreachable_store_as_store_unavailable() {
        let state = key_state(&Unavailable, "openai").unwrap();
        assert_eq!(
            state,
            KeyState::StoreUnavailable("no store on this platform".to_string())
        );
    }

    /// A store that is reachable but fails to read this one item -- the OS
    /// access prompt was denied, or the entry does not decode.
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
    fn key_state_reports_a_key_that_fails_to_read_as_unreadable_not_missing() {
        let state = key_state(&Unreadable, "openai").unwrap();
        assert_eq!(
            state,
            KeyState::Unreadable("the item could not be decoded".to_string())
        );
    }

    #[test]
    fn every_provider_has_a_stable_id_a_name_and_at_least_one_kind() {
        for p in PROVIDERS {
            assert!(!p.id.is_empty());
            assert!(!p.name.is_empty());
            assert!(!p.kinds.is_empty(), "{}: offers nothing", p.id);
            assert!(!p.account.is_empty());
        }
    }
}
