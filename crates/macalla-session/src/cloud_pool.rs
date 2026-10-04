// SPDX-License-Identifier: MIT
//! The cloud pool's model lists, split the way OpenRouter's are.
//! Each provider's own adapter in `macalla-enhancers` fetches and parses its
//! listing with the saved key. This module owns the rest: which providers a
//! pool can hold, which of their models can clean up text, and a cached copy
//! of each raw listing, so a stale network never means an empty page.
//!
//! What counts as a text model differs by provider, because their lists do:
//!
//! - OpenAI's list has no type or capability field, so text models are told
//!   apart by their ids: a model is kept only when its id is in an
//!   allow-listed chat family, is not a `-pro` id (gpt-5.2-pro's saved page
//!   says "available in the Responses API only"), is not a codex variant,
//!   is not a dated snapshot of a base id also listed, has not had a
//!   shutdown date announced, and names none of audio, search, realtime,
//!   speech, transcription, images or security research. A new kind of
//!   model stays out until it is added.
//! - Every Anthropic model is a text model.
//! - A Gemini model is kept when it supports `generateContent`. It is listed
//!   natively and cleans up through Gemini's OpenAI-compatible layer.
//! - A Mistral model is kept when it can chat and is neither archived nor
//!   marked for deprecation.
//!
//! Nothing is suggested here: a model is suggested only once it has been
//! measured on the cleanup fixtures.
//!
//! The key is read once for each fetch and handed to the adapter. It is never
//! cached, logged or returned, and a provider with no saved key is not listed.

use crate::openrouter::{cached_listing_or, Offers, Refresh};
use crate::providers::{self, KeyState, Provider};
use crate::{choice, Error, Result};
use macalla_enhancers::{anthropic, gemini, mistral, openai};
use macalla_platform::secrets::SecretStore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

/// One model a pool can hold, as a settings page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolOffer {
    /// The id to save and to send.
    pub id: String,
    /// The provider's display name for the model, or its id when the list
    /// gives none.
    pub name: String,
    /// The most input tokens the model takes, when the provider's list says.
    /// Anthropic, Gemini and Mistral say; OpenAI's list does not.
    pub input_token_limit: Option<u64>,
}

/// The providers a pool can hold that have a saved key, in
/// [`providers::PROVIDERS`] order. A key that cannot be read, or a key store
/// that cannot be reached, does not count as saved here.
pub fn pool_providers(keys: &dyn SecretStore) -> Vec<&'static Provider> {
    providers::PROVIDERS
        .iter()
        .filter(|provider| choice::pool_can_hold(provider.id))
        .filter(|provider| matches!(providers::key_state(keys, provider.id), Ok(KeyState::Saved)))
        .collect()
}

/// The models `provider` offers a pool: fetched with its saved key through its
/// own adapter, cached under `cache_dir`, and filtered as the module
/// documentation describes.
///
/// Refused before any fetch when a pool cannot hold `provider`, or when no key
/// is saved for it. A failed fetch with a cached copy returns that copy and
/// the reason; with none, [`Error::ModelListFetch`].
pub fn pool_offers(
    provider: &str,
    keys: &dyn SecretStore,
    cache_dir: &Path,
    refresh: Refresh,
    now_ms: u64,
) -> Result<Offers<PoolOffer>> {
    if !choice::pool_can_hold(provider) {
        return Err(Error::InvalidCleanupChoice(format!(
            "{provider} cannot be in the cleanup pool"
        )));
    }
    let key = providers::read_key(keys, provider)?.ok_or_else(|| Error::NeedsKey {
        provider: provider.to_string(),
    })?;
    let key = key.as_str();
    let at = Where {
        provider,
        cache_dir,
        refresh,
        now_ms,
    };
    match provider {
        "openai" => at.listed(
            || {
                openai::list_models(key)
                    .map(|listing| listing.models)
                    .map_err(|e| e.to_string())
            },
            openai_offers,
        ),
        "anthropic" => at.listed(
            || {
                anthropic::list_models(key)
                    .map(|listing| listing.models)
                    .map_err(|e| e.to_string())
            },
            anthropic_offers,
        ),
        "gemini" => at.listed(
            || {
                gemini::list_models(key)
                    .map(|listing| listing.models)
                    .map_err(|e| e.to_string())
            },
            gemini_offers,
        ),
        "mistral" => at.listed(
            || {
                mistral::list_models(key)
                    .map(|listing| listing.models)
                    .map_err(|e| e.to_string())
            },
            mistral_offers,
        ),
        other => Err(Error::InvalidCleanupChoice(format!(
            "{other} cannot be in the cleanup pool"
        ))),
    }
}

/// Where one provider's listing is cached, and how fresh it must be.
struct Where<'a> {
    provider: &'a str,
    cache_dir: &'a Path,
    refresh: Refresh,
    now_ms: u64,
}

impl Where<'_> {
    /// The raw listing through the shared cache, as `<provider>-text.json`,
    /// then `filter` over it.
    fn listed<T, O>(
        &self,
        fetch: impl FnOnce() -> std::result::Result<Vec<T>, String>,
        filter: fn(&[T]) -> Vec<O>,
    ) -> Result<Offers<O>>
    where
        T: Serialize + DeserializeOwned,
    {
        let raw = cached_listing_or(
            self.cache_dir,
            &format!("{}-text.json", self.provider),
            self.refresh,
            self.now_ms,
            fetch,
            |reason| Error::ModelListFetch {
                provider: providers::find(self.provider)
                    .map_or(self.provider, |p| p.name)
                    .to_string(),
                reason,
            },
        )?;
        Ok(Offers {
            offers: filter(&raw.models),
            fetched_at_ms: raw.fetched_at_ms,
            stale_reason: raw.stale_reason,
        })
    }
}

/// The OpenAI model families offered for cleanup: an allow-list, so a new
/// family (a future `gpt-7`) stays out until someone adds it deliberately,
/// and an old family stays out once it is no longer worth building for.
/// `o3` and `o4-mini` are, in OpenAI's own words, "succeeded by" a GPT-5
/// model (OpenAI's `o3` and `o4-mini` model pages, each: "succeeded by
/// GPT-5[...]"); those two pages don't
/// mention `o1`, which instead sits on the shutdown list, not a succession
/// claim (OpenAI's deprecations page: `o1-2024-12-17` \| `o1`, shutdown
/// date October 23, 2026). The same table also lists `gpt-3.5-turbo`, `gpt-4`,
/// `gpt-4-turbo` and `o1-pro`; the page also documents the deprecation and
/// removal of the `chatgpt-4o-latest` and `codex-mini-latest` snapshots.
/// Bare `gpt-4o` carries no saved deprecation notice of its own -- it is
/// left out simply because it sits outside the allow-listed families below.
const OPENAI_CHAT_FAMILIES: &[&str] = &["gpt-6", "gpt-5", "gpt-4.1"];

/// Words that mark a model inside a chat family that is not for cleaning up
/// text: previews and specialised variants a `-pro` or dated-snapshot check
/// can't catch. `instruct` and `deep-research` are Chat-Completions-shaped
/// exclusions this module already carried; the effect is carried over, not
/// the mechanism -- what was a separate family deny-list is now a word match
/// alongside the others. `codex` is left out as a product choice, plainly:
/// code-focused variants such as `gpt-5.3-codex`
/// (OpenAI's deprecations page,
/// "Codex variants such as `gpt-5.3-codex`") write code, not prose, so a
/// cleanup pool has no use for one whether or not a Chat Completions
/// request would succeed against it -- the saved OpenAPI enum
/// `ModelIdsShared` in fact lists `gpt-5.1-codex` and `codex-mini-latest`
/// as accepted ids (OpenAI's OpenAPI description), which proves neither way; treating enum membership
/// as a capability claim either way was the mistake the `-pro` check below
/// avoids by citing a model page instead, and this exclusion does not repeat
/// it. The rest are the audio, search, realtime, transcription, speech,
/// image and vulnerability-research exclusions this module already carried.
const OPENAI_NOT_CLEANUP: &[&str] = &[
    "audio",
    "search",
    "realtime",
    "transcribe",
    "tts",
    "image",
    "cyber",
    "instruct",
    "deep-research",
    "codex",
];

/// Whether `id` names one of `families`: the family's name, then the end of
/// the id or a `-` or `.` -- so `gpt-4` covers `gpt-4-turbo` but not
/// `gpt-4ever`, and `o1` covers `o1-mini` but not `o10`.
fn in_family(id: &str, families: &[&str]) -> bool {
    families.iter().any(|family| {
        id.strip_prefix(family)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '.']))
    })
}

/// The model an OpenAI id is judged by. A fine-tuned model is used "in either
/// the Responses or Chat Completions API, just as you would an OpenAI base
/// model", and its id "should start with `ft:`"
/// (OpenAI's supervised fine-tuning guide), so it is judged by the base model named before the next colon;
/// the organisation and suffix after it are not the model. That layout past
/// `ft:` is not stated in the saved guide.
fn openai_base(id: &str) -> &str {
    match id.strip_prefix("ft:") {
        Some(rest) => rest.split(':').next().unwrap_or_default(),
        None => id,
    }
}

/// Whether `id` has a `-pro` part: a variant "available in the Responses
/// API only" (OpenAI's `gpt-5.2-pro` model page,
/// said of `gpt-5.2-pro`), which a Chat-Completions-style cleanup
/// request never reaches. Matched at a boundary, as [`in_family`] matches a
/// family: `-pro`, then the end of the id or another `-` or `.` -- so
/// `gpt-5-pro` and its dated snapshot `gpt-5.2-pro-2026-01-01` match, but
/// `gpt-5-professional` and `gpt-5-promo` don't.
fn is_pro(id: &str) -> bool {
    let mut rest = id;
    while let Some(idx) = rest.find("-pro") {
        let after = &rest[idx + "-pro".len()..];
        if after.is_empty() || after.starts_with(['-', '.']) {
            return true;
        }
        rest = &rest[idx + 1..];
    }
    false
}

/// The base id a dated snapshot names, if `id` ends in a `-YYYY-MM-DD` part
/// (three dash-separated numeric groups, four then two then two digits) --
/// the shape OpenAI's snapshot ids use, e.g. `gpt-5-2026-01-15` names
/// `gpt-5`. `None` when `id` doesn't end that way, including when it's a
/// bare date-shaped id with no base before it.
fn dated_snapshot_base(id: &str) -> Option<&str> {
    let parts: Vec<&str> = id.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    let tail = &parts[parts.len() - 3..];
    let is_digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    if !(is_digits(tail[0], 4) && is_digits(tail[1], 2) && is_digits(tail[2], 2)) {
        return None;
    }
    let date_len = tail[0].len() + tail[1].len() + tail[2].len() + 3; // three dashes
    Some(&id[..id.len() - date_len])
}

/// The OpenAI models a cleanup pool can hold.
///
/// A known limit: this reads ids, not capabilities. OpenAI's model list
/// carries no capability field, so which models answer a chat request is
/// decided from the id alone, against the family, word and shape checks
/// above, each taken from a saved OpenAI source. A model it wrongly keeps
/// fails on every pass, and the pool moves on to the next ticked model; one
/// it wrongly leaves out cannot be ticked. The words are matched anywhere in
/// the id, so `search` also leaves out a future chat model with `research`
/// in its id.
fn openai_offers(models: &[openai::Model]) -> Vec<PoolOffer> {
    // Two passes: the family, `-pro`, shutdown-announced and word checks
    // first, then the dated-snapshot check against the ids that SURVIVED
    // those -- not the raw listing, and never against a fine-tune (below). A
    // snapshot's base is "also listed" only if it would itself be offered;
    // a base that has a shutdown date announced, for instance, isn't, so
    // its still-live dated snapshot is kept, not hidden alongside it. A
    // fine-tune's base doesn't count as "listed" either, even though the
    // fine-tune itself survived: a user's own `ft:gpt-5:acme::x` is not the
    // bare `gpt-5` they never saved, so it must not stand in for one.
    let survivors: Vec<&openai::Model> = models
        .iter()
        .filter(|model| {
            // Present at all means a shutdown date has been announced for
            // this model -- not that the date has passed. No date parsing:
            // an announced shutdown is reason enough to leave it out.
            if model.shutdown_date.is_some() {
                return false;
            }
            let base = openai_base(&model.id);
            if !in_family(base, OPENAI_CHAT_FAMILIES) {
                return false;
            }
            if is_pro(base) {
                return false;
            }
            !OPENAI_NOT_CLEANUP.iter().any(|word| base.contains(word))
        })
        .collect();
    let listed_bases: std::collections::HashSet<&str> = survivors
        .iter()
        .filter(|m| !m.id.starts_with("ft:"))
        .map(|m| openai_base(&m.id))
        .collect();
    survivors
        .into_iter()
        .filter(|model| {
            // A fine-tune is never "also listed" as itself: its own id, not
            // its base, is what a user picked and saved. Hiding it because
            // its base names a dated snapshot that happens to have a bare
            // sibling would drop the user's own fine-tune, not a duplicate.
            if model.id.starts_with("ft:") {
                return true;
            }
            let base = openai_base(&model.id);
            match dated_snapshot_base(base) {
                Some(snapshot_base) => !listed_bases.contains(snapshot_base),
                None => true,
            }
        })
        .map(|model| offer(&model.id, "", None))
        .collect()
}

fn anthropic_offers(models: &[anthropic::Model]) -> Vec<PoolOffer> {
    models
        .iter()
        .map(|model| offer(&model.id, &model.display_name, model.max_input_tokens))
        .collect()
}

fn gemini_offers(models: &[gemini::Model]) -> Vec<PoolOffer> {
    models
        .iter()
        .filter(|model| {
            model
                .supported_generation_methods
                .iter()
                .any(|method| method == "generateContent")
        })
        .map(|model| offer(&model.id, &model.display_name, model.input_token_limit))
        .collect()
}

fn mistral_offers(models: &[mistral::Model]) -> Vec<PoolOffer> {
    models
        .iter()
        .filter(|model| model.completion_chat && !model.archived && model.deprecation.is_none())
        .map(|model| offer(&model.id, "", model.max_context_length))
        .collect()
}

fn offer(id: &str, name: &str, input_token_limit: Option<u64>) -> PoolOffer {
    let name = name.trim();
    PoolOffer {
        id: id.to_string(),
        name: if name.is_empty() { id } else { name }.to_string(),
        input_token_limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_platform::secrets::MemoryStore;

    fn ids(offers: &[PoolOffer]) -> Vec<&str> {
        offers.iter().map(|offer| offer.id.as_str()).collect()
    }

    /// Builds an `openai::Model` with no `shutdown_date`, for tests that
    /// aren't about that rule.
    fn model(id: &str) -> openai::Model {
        openai::Model {
            id: id.to_string(),
            owned_by: Some("openai".to_string()),
            created: None,
            shutdown_date: None,
        }
    }

    #[test]
    fn openai_keeps_only_the_allow_listed_families_and_nothing_else() {
        let offers = openai_offers(&[
            // Allowed families, including a dotted minor version.
            model("gpt-5"),
            model("gpt-5.2"),
            model("gpt-4.1"),
            model("gpt-6"),
            // A family outside the allow-list.
            model("gpt-4o"),
            model("o3"),
            model("chatgpt-4o-latest"),
            // A family's name only counts up to a separator.
            model("gpt-4ever"),
            model("gpt-50"),
            model("gpt-4.10"),
        ]);
        assert_eq!(ids(&offers), vec!["gpt-5", "gpt-5.2", "gpt-4.1", "gpt-6"]);
        assert_eq!(
            offers[0],
            PoolOffer {
                id: "gpt-5".to_string(),
                name: "gpt-5".to_string(),
                input_token_limit: None,
            }
        );
    }

    #[test]
    fn openai_drops_pro_ids_and_their_dated_snapshots() {
        // "available in the Responses API only":
        // OpenAI's `gpt-5.2-pro` model page.
        let offers = openai_offers(&[
            model("gpt-5.2-pro"),
            model("gpt-5.2-pro-2026-01-15"),
            model("gpt-5.2"),
        ]);
        assert_eq!(ids(&offers), vec!["gpt-5.2"]);
    }

    #[test]
    fn openai_matches_pro_at_a_boundary_not_as_a_bare_substring() {
        let offers = openai_offers(&[
            model("gpt-5-pro"),
            model("gpt-5.2-pro-2025-12-11"),
            // "pro" is only a prefix of these words, not the whole part
            // between dashes, so none of these three is a -pro id.
            model("gpt-5-professional"),
            model("gpt-5-promo"),
            model("gpt-5-prompt-helper"),
        ]);
        assert_eq!(
            ids(&offers),
            vec!["gpt-5-professional", "gpt-5-promo", "gpt-5-prompt-helper"]
        );
    }

    #[test]
    fn openai_drops_codex_variants() {
        // "Codex variants such as `gpt-5.3-codex`":
        // OpenAI's deprecations page.
        let offers = openai_offers(&[model("gpt-5.3-codex"), model("gpt-5")]);
        assert_eq!(ids(&offers), vec!["gpt-5"]);
    }

    #[test]
    fn openai_hides_a_dated_snapshot_only_when_its_base_is_also_listed() {
        let offers = openai_offers(&[
            model("gpt-5"),
            model("gpt-5-2026-01-15"),
            // Its base, gpt-5-mini, isn't in this listing, so it stays.
            model("gpt-5-mini-2026-01-01"),
        ]);
        assert_eq!(ids(&offers), vec!["gpt-5", "gpt-5-mini-2026-01-01"]);
    }

    #[test]
    fn openai_keeps_a_dated_snapshot_whose_base_was_itself_dropped_by_another_rule() {
        // gpt-5 has a shutdown date announced and is dropped for it, but
        // gpt-5-2026-01-15 has none and should stay: its base isn't "also
        // listed" once gpt-5 didn't survive the other filters, so the
        // snapshot rule must not hide it too, or the model becomes
        // unofferable outright.
        let with_shutdown = openai::Model {
            shutdown_date: Some("2027-01-01".to_string()),
            ..model("gpt-5")
        };
        let offers = openai_offers(&[with_shutdown, model("gpt-5-2026-01-15")]);
        assert_eq!(ids(&offers), vec!["gpt-5-2026-01-15"]);
    }

    #[test]
    fn openai_drops_a_model_that_has_a_shutdown_date_announced() {
        // The field being set is what matters, not whether the date has
        // arrived: this hides gpt-5 as soon as a shutdown is announced for
        // it, with no parsing of "2027-01-01" to check whether that day has
        // come.
        let with_shutdown = openai::Model {
            shutdown_date: Some("2027-01-01".to_string()),
            ..model("gpt-5")
        };
        let offers = openai_offers(&[with_shutdown, model("gpt-4.1")]);
        assert_eq!(ids(&offers), vec!["gpt-4.1"]);
    }

    #[test]
    fn openai_leaves_out_models_its_pages_send_elsewhere() {
        let offers = openai_offers(&[
            model("gpt-5.6-cyber"),
            model("gpt-5.6-audio-preview"),
            model("gpt-5.6-mini-search-preview"),
            model("gpt-5.6-mini-tts"),
            model("gpt-5.6-transcribe"),
            model("gpt-5.6-instruct"),
            model("gpt-5.6-deep-research"),
            // Kept: none of the excluded words or shapes.
            model("gpt-5.6-luna"),
        ]);
        assert_eq!(ids(&offers), vec!["gpt-5.6-luna"]);
    }

    #[test]
    fn openai_keeps_a_fine_tuned_model_whose_base_is_a_chat_model() {
        // A fine-tuned model's id "should start with `ft:`", and is used "in
        // either the Responses or Chat Completions API, just as you would an
        // OpenAI base model": guides/supervised-fine-tuning.md:116 and 119.
        let offers = openai_offers(&[
            // A word in the organisation or suffix is not the model.
            model("ft:gpt-4.1:acme:search-team:abc123"),
            // The base decides, as it does for any other id.
            model("ft:gpt-4o-mini:acme::abc123"),
            model("ft:"),
        ]);
        assert_eq!(ids(&offers), vec!["ft:gpt-4.1:acme:search-team:abc123"]);
    }

    #[test]
    fn openai_keeps_a_fine_tuned_model_on_a_dated_base_even_when_the_bare_snapshot_is_hidden() {
        // ft:gpt-5-2026-01-15:acme::abc123's base is gpt-5-2026-01-15, whose
        // own dated-snapshot base is gpt-5 -- but the fine-tune is not the
        // bare snapshot, and a user's own fine-tune must survive even though
        // gpt-5 is also offered.
        let offers = openai_offers(&[
            model("gpt-5"),
            model("gpt-5-2026-01-15"),
            model("ft:gpt-5-2026-01-15:acme::abc123"),
        ]);
        assert_eq!(
            ids(&offers),
            vec!["gpt-5", "ft:gpt-5-2026-01-15:acme::abc123"],
            "the bare dated snapshot is still hidden; the fine-tune on it is not"
        );
    }

    #[test]
    fn openai_does_not_count_a_fine_tunes_base_as_a_listed_base() {
        // gpt-5 itself has a shutdown date announced, so it never survives
        // to be "listed". A fine-tune on gpt-5 must not stand in for it:
        // gpt-5-2026-01-15's own dated-snapshot base is gpt-5, and that must
        // still read as NOT listed, so the bare snapshot stays offered too.
        let with_shutdown = openai::Model {
            shutdown_date: Some("2027-01-01".to_string()),
            ..model("gpt-5")
        };
        let offers = openai_offers(&[
            with_shutdown,
            model("ft:gpt-5:acme::x"),
            model("gpt-5-2026-01-15"),
        ]);
        assert_eq!(
            ids(&offers),
            vec!["ft:gpt-5:acme::x", "gpt-5-2026-01-15"],
            "a fine-tune's base must not count as gpt-5 being listed"
        );
    }

    #[test]
    fn a_failed_fetch_with_nothing_cached_names_the_provider_as_people_see_it() {
        let cache = std::env::temp_dir().join(format!(
            "macalla-cloud-pool-fetch-test-{}",
            std::process::id()
        ));
        let at = Where {
            provider: "openai",
            cache_dir: &cache,
            refresh: Refresh::Now,
            now_ms: 0,
        };
        let error = at
            .listed(
                || Err::<Vec<openai::Model>, String>("the network is down".to_string()),
                openai_offers,
            )
            .unwrap_err();
        assert!(
            matches!(
                &error,
                Error::ModelListFetch { provider, reason }
                    if provider == "OpenAI" && reason == "the network is down"
            ),
            "{error}"
        );
    }

    #[test]
    fn anthropic_keeps_every_model_with_its_name_and_input_limit() {
        let offers = anthropic_offers(&[anthropic::Model {
            id: "claude-test".to_string(),
            display_name: "Claude Test".to_string(),
            max_input_tokens: Some(200_000),
        }]);
        assert_eq!(
            offers,
            vec![PoolOffer {
                id: "claude-test".to_string(),
                name: "Claude Test".to_string(),
                input_token_limit: Some(200_000),
            }]
        );
    }

    #[test]
    fn gemini_keeps_models_that_generate_content() {
        let model = |id: &str, name: &str, methods: &[&str]| gemini::Model {
            id: id.to_string(),
            display_name: name.to_string(),
            input_token_limit: Some(1_048_576),
            supported_generation_methods: methods.iter().map(|m| m.to_string()).collect(),
        };
        let offers = gemini_offers(&[
            model(
                "gemini-test",
                "Gemini Test",
                &["generateContent", "countTokens"],
            ),
            model("embedding-test", "Embedding", &["embedContent"]),
            model("gemini-unnamed", " ", &["generateContent"]),
        ]);
        assert_eq!(ids(&offers), vec!["gemini-test", "gemini-unnamed"]);
        assert_eq!(offers[0].name, "Gemini Test");
        assert_eq!(offers[1].name, "gemini-unnamed", "no name, so the id");
    }

    #[test]
    fn mistral_keeps_chat_models_that_are_current() {
        let model =
            |id: &str, chat: bool, archived: bool, deprecation: Option<&str>| mistral::Model {
                id: id.to_string(),
                completion_chat: chat,
                max_context_length: Some(128_000),
                archived,
                deprecation: deprecation.map(str::to_string),
                aliases: Vec::new(),
            };
        let offers = mistral_offers(&[
            model("mistral-test", true, false, None),
            model("mistral-embed-test", false, false, None),
            model("mistral-old", true, true, None),
            model("mistral-leaving", true, false, Some("2026-12-31")),
        ]);
        assert_eq!(ids(&offers), vec!["mistral-test"]);
        assert_eq!(offers[0].input_token_limit, Some(128_000));
    }

    #[test]
    fn only_providers_a_pool_can_hold_with_a_saved_key_are_listed() {
        let keys = MemoryStore::new();
        for provider in ["mistral", "openai", "elevenlabs"] {
            keys.set(&format!("provider-key.{provider}"), "test-key")
                .unwrap();
        }
        let listed: Vec<&str> = pool_providers(&keys).iter().map(|p| p.id).collect();
        assert_eq!(listed, vec!["openai", "mistral"]);
    }

    #[test]
    fn a_provider_a_pool_cannot_hold_or_one_without_a_key_is_refused_before_any_fetch() {
        let keys = MemoryStore::new();
        let cache =
            std::env::temp_dir().join(format!("macalla-cloud-pool-test-{}", std::process::id()));

        let error = pool_offers("elevenlabs", &keys, &cache, Refresh::Now, 0).unwrap_err();
        assert!(
            matches!(&error, Error::InvalidCleanupChoice(reason) if reason.contains("elevenlabs")),
            "{error}"
        );

        let error = pool_offers("anthropic", &keys, &cache, Refresh::Now, 0).unwrap_err();
        assert!(
            matches!(&error, Error::NeedsKey { provider } if provider == "anthropic"),
            "{error}"
        );
        assert!(
            !cache.join("anthropic-text.json").exists(),
            "nothing was fetched or cached"
        );
    }
}
