// SPDX-License-Identifier: MIT
//! OpenRouter's model listings, turned into offers.
//!
//! `macalla-enhancers` and `macalla-engines` each know how to fetch and parse
//! their own listing -- a text one and a transcription one -- and hand back
//! plain data with no policy attached. This module owns everything on top:
//! which models are worth offering for cleanup (the filter is tested against
//! the 2026-09-12 snapshot for exact counts, see the tests below), Macalla's
//! own measured figures for a handful of exact ids, and a small on-disk cache
//! so a stale network never means an empty page.

use crate::choice::MeasuredCleanup;
use crate::{Error, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ------------------------------------------------------------------ caching

/// How a cached listing may be refreshed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Always fetch: used when a key is saved, and by the page's own Refresh
    /// action.
    Now,
    /// Fetch only when there is no cached copy, or it is a week old or more:
    /// used when the page opens.
    IfStale,
}

/// [`Refresh::IfStale`]'s threshold: a week, so a page opened
/// often does not refetch every time, but one left untouched for a while
/// gets a current answer without anyone asking for it.
const STALE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// One kind of offer, with when its underlying listing was fetched.
#[derive(Debug, Clone, PartialEq)]
pub struct Offers<T> {
    pub offers: Vec<T>,
    pub fetched_at_ms: u64,
    /// Set when the network fetch failed and this is the last good cached
    /// copy instead -- the reason, never an error, so a page can show both
    /// the cached list and why it might be out of date.
    pub stale_reason: Option<String>,
}

/// What the cache file holds on read: the raw listing, before any policy is
/// applied, and when it was fetched.
#[derive(Deserialize)]
struct StoredListing<T> {
    fetched_at_ms: u64,
    models: Vec<T>,
}

/// The same shape, borrowed, for writing without cloning the list just
/// fetched.
#[derive(Serialize)]
struct StoredListingRef<'a, T> {
    fetched_at_ms: u64,
    models: &'a [T],
}

#[derive(Debug)]
pub(crate) struct RawResult<T> {
    pub(crate) models: Vec<T>,
    pub(crate) fetched_at_ms: u64,
    pub(crate) stale_reason: Option<String>,
}

/// Load, refresh and cache one raw listing under `cache_dir/file_name`.
/// `fetch` is called at most once, only when a fetch is needed, so a test can
/// hand it a closure that never touches the network and assert it was (or
/// was not) called. `now_ms` is the caller's own clock reading -- nothing
/// here calls [`std::time::SystemTime::now`] -- so every rule about
/// freshness is exercised with a chosen value instead of real time.
///
/// A read or parse failure on the cache file (missing, corrupt, or from an
/// old shape) is silently treated as no cache: the network is the source of
/// truth, and a bad cache file must never be a harder failure than no cache
/// file at all.
fn cached_listing<T>(
    cache_dir: &Path,
    file_name: &str,
    refresh: Refresh,
    now_ms: u64,
    fetch: impl FnOnce() -> std::result::Result<Vec<T>, String>,
) -> Result<RawResult<T>>
where
    T: Serialize + DeserializeOwned,
{
    cached_listing_or(
        cache_dir,
        file_name,
        refresh,
        now_ms,
        fetch,
        Error::OpenRouterFetch,
    )
}

/// [`cached_listing`], with the error for "no cached copy and the fetch
/// failed" chosen by the caller. Shared with [`crate::cloud_pool`], whose
/// providers each name their own; the cache itself behaves identically.
pub(crate) fn cached_listing_or<T>(
    cache_dir: &Path,
    file_name: &str,
    refresh: Refresh,
    now_ms: u64,
    fetch: impl FnOnce() -> std::result::Result<Vec<T>, String>,
    no_cache: impl FnOnce(String) -> Error,
) -> Result<RawResult<T>>
where
    T: Serialize + DeserializeOwned,
{
    let path = cache_dir.join(file_name);
    let cached = read_cache::<T>(&path);

    let need_fetch = match refresh {
        Refresh::Now => true,
        Refresh::IfStale => match &cached {
            None => true,
            // `checked_sub` rather than `saturating_sub`: if the clock ever
            // moved backwards (or a corrupt/malicious cache claims a future
            // fetch time), `now_ms - fetched_at_ms` underflows, and treating
            // that as "just fetched" would mean this cache never goes stale
            // until the real clock caught up to the bogus timestamp. Treat
            // it as stale instead -- a needless refetch is cheap; a cache
            // that can never expire is not.
            Some(c) => match now_ms.checked_sub(c.fetched_at_ms) {
                Some(age) => age >= STALE_AFTER_MS,
                None => true,
            },
        },
    };

    if !need_fetch {
        let cached = cached.expect("need_fetch is only false once a cache was found");
        return Ok(RawResult {
            models: cached.models,
            fetched_at_ms: cached.fetched_at_ms,
            stale_reason: None,
        });
    }

    match fetch() {
        Ok(models) => {
            write_cache(&path, now_ms, &models);
            Ok(RawResult {
                models,
                fetched_at_ms: now_ms,
                stale_reason: None,
            })
        }
        Err(reason) => match cached {
            Some(cached) => Ok(RawResult {
                models: cached.models,
                fetched_at_ms: cached.fetched_at_ms,
                stale_reason: Some(reason),
            }),
            None => Err(no_cache(reason)),
        },
    }
}

fn read_cache<T: DeserializeOwned>(path: &Path) -> Option<StoredListing<T>> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Best-effort: a cache directory that can't be written to loses nothing but
/// the next `IfStale` skip -- the models just fetched are returned to the
/// caller either way, so a read-only disk must not turn a successful fetch
/// into an error. Written beside and renamed into place, the same pattern
/// [`crate::dictionary::write`] uses, so a crash mid-write leaves the old
/// cache rather than a half-written one.
///
/// The tmp path is unique per call (process id plus a process-wide counter),
/// not a fixed `.json.tmp`: the page opening (`IfStale`) and a person hitting
/// Refresh (`Now`) can race, and two writers sharing one tmp path could each
/// write, then rename, over the other mid-write. A unique tmp path per write
/// means each rename is still atomic and lands one writer's whole content --
/// whichever finishes last -- never a splice of both.
fn write_cache<T: Serialize>(path: &Path, fetched_at_ms: u64, models: &[T]) {
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let Ok(json) = serde_json::to_string(&StoredListingRef {
        fetched_at_ms,
        models,
    }) else {
        return;
    };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let unique = TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_name = format!(
        "{}.tmp.{}.{unique}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("cache"),
        std::process::id(),
    );
    let tmp = path.with_file_name(tmp_name);
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// -------------------------------------------------------------- cleanup

/// How much a cleanup model reasons, once models with no way to turn it down
/// far enough are dropped (see [`cleanup_offers_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningClass {
    /// No `reasoning` object and `"reasoning"` is not in
    /// `supported_parameters`: this model does not reason at all.
    NoReasoning,
    /// A `reasoning` object exists but `mandatory` is not `true`: reasoning
    /// can be turned off.
    CanTurnOff,
    /// Reasoning is mandatory, but the lowest supported effort is `"none"`
    /// or `"minimal"`.
    Minimal,
}

/// One cleanup model OpenRouter offers, filtered and classified.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanupOffer {
    pub id: String,
    pub name: String,
    pub reasoning: ReasoningClass,
    pub measured: Option<MeasuredCleanup>,
}

fn is_text_only(model: &macalla_enhancers::openrouter::TextModel) -> bool {
    model.output_modalities == ["text"] && model.input_modalities.iter().any(|m| m == "text")
}

/// Not a batch-only variant (`:batch`), a moving alias (`~...`), or a model
/// with an expiry date.
///
/// The blanket "any dated model is dropped" rule is deliberate for now, even
/// though some dates look like sentinels rather than a real retirement: the
/// 2026-09-12 snapshot gives `z-ai/glm-5-turbo` and `z-ai/glm-5v-turbo` both
/// `expiration_date: "2098-12-31"` -- 72 years out, plainly not a real
/// deprecation date. Excluding them anyway keeps the filter simple and the
/// published counts (430/333/276) exact; if a far-future date turns out to
/// hide models people actually want, revisit with a horizon (for example,
/// keep anything dated more than N years out) rather than special-casing ids.
fn is_current(model: &macalla_enhancers::openrouter::TextModel) -> bool {
    !model.id.ends_with(":batch") && !model.id.starts_with('~') && model.expiration_date.is_none()
}

/// `None` means "too much reasoning": mandatory, with no low-enough effort
/// -- dropped rather than offered.
fn classify(model: &macalla_enhancers::openrouter::TextModel) -> Option<ReasoningClass> {
    let names_reasoning_parameter = model.supported_parameters.iter().any(|p| p == "reasoning");
    match &model.reasoning {
        None if !names_reasoning_parameter => Some(ReasoningClass::NoReasoning),
        reasoning => {
            let mandatory = reasoning
                .as_ref()
                .and_then(|r| r.mandatory)
                .unwrap_or(false);
            if !mandatory {
                return Some(ReasoningClass::CanTurnOff);
            }
            let can_go_low = reasoning.as_ref().is_some_and(|r| {
                r.supported_efforts
                    .iter()
                    .any(|e| e == "none" || e == "minimal")
            });
            can_go_low.then_some(ReasoningClass::Minimal)
        }
    }
}

/// Macalla's own cleanup accuracy for these exact OpenRouter ids, measured
/// direct against each maker's own endpoint rather than through OpenRouter
/// (`docs/benchmarks.md`): the lower of two runs, right out of 20.
///
/// This is the OpenRouter-id counterpart of [`crate::choice::MEASURED_CLEANUP`],
/// which is keyed by a person's own server's (Ollama) model ids instead.
/// Two tables, one owner (this module and `choice` are both `macalla-session`),
/// kept apart because the id spaces don't overlap.
const MEASURED_CLEANUP: &[(&str, u8)] = &[
    ("openai/gpt-5.6-luna", 19),
    ("openai/gpt-4.1-nano", 16),
    ("openai/gpt-5.4-nano", 14),
    ("anthropic/claude-haiku-4.5", 18),
    ("anthropic/claude-sonnet-5", 16),
    ("google/gemini-3.5-flash-lite", 17),
    ("google/gemini-3.1-flash-lite", 17),
    ("google/gemini-2.5-flash-lite", 15),
];

fn measured_cleanup(id: &str) -> Option<MeasuredCleanup> {
    MEASURED_CLEANUP
        .iter()
        .find(|(model, _)| *model == id)
        .map(|(_, right)| MeasuredCleanup {
            right: *right,
            of: 20,
            machine: "measured direct from the maker, not through OpenRouter",
        })
}

/// Every cleanup model worth offering, from the raw text listing: text-only,
/// current, and either unable to reason, able to turn reasoning off, or
/// willing to go as low as "none" or "minimal" when it is mandatory.
/// Anything left mandatory at "low" or higher is dropped.
fn cleanup_offers_from(models: &[macalla_enhancers::openrouter::TextModel]) -> Vec<CleanupOffer> {
    models
        .iter()
        .filter(|m| is_text_only(m) && is_current(m))
        .filter_map(|m| {
            let reasoning = classify(m)?;
            Some(CleanupOffer {
                id: m.id.clone(),
                name: m.name.clone(),
                measured: measured_cleanup(&m.id),
                reasoning,
            })
        })
        .collect()
}

/// Text (cleanup) offers: fetched from
/// [`macalla_enhancers::openrouter::list_text_models`], cached under
/// `cache_dir`, and filtered by [`cleanup_offers_from`]. Always compiled --
/// the enhancers crate's OpenRouter listing carries no key and needs no
/// feature.
pub fn cleanup_offers(
    cache_dir: &Path,
    refresh: Refresh,
    now_ms: u64,
) -> Result<Offers<CleanupOffer>> {
    let raw = cached_listing(cache_dir, "openrouter-text.json", refresh, now_ms, || {
        macalla_enhancers::openrouter::list_text_models()
            .map(|listing| listing.models)
            .map_err(|e| e.to_string())
    })?;
    Ok(Offers {
        offers: cleanup_offers_from(&raw.models),
        fetched_at_ms: raw.fetched_at_ms,
        stale_reason: raw.stale_reason,
    })
}

// --------------------------------------------------------- transcription

/// Macalla's own measured accuracy for one exact OpenRouter transcription
/// model id, measured direct against the maker's own endpoint
/// (`docs/benchmarks.md`): words wrong on the everyday, technical and
/// clinical clips, with no dictionary sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasuredTranscription {
    /// Words wrong, in everyday/technical/clinical order.
    pub words_wrong: [u16; 3],
    /// Each clip's own word count, in the same order (80/30/64), so a
    /// percentage can be shown without hard-coding the sizes again.
    pub clip_words: [u16; 3],
}

#[cfg(feature = "cloud-transcription")]
const CLIP_WORDS: [u16; 3] = [80, 30, 64];

/// `docs/benchmarks.md`: words wrong on the everyday/technical/clinical
/// clips, no dictionary sent.
#[cfg(feature = "cloud-transcription")]
const MEASURED_TRANSCRIPTION: &[(&str, [u16; 3])] = &[
    ("openai/gpt-transcribe", [0, 3, 8]),
    ("openai/gpt-4o-transcribe", [0, 1, 9]),
    ("openai/gpt-4o-mini-transcribe", [1, 1, 9]),
];

#[cfg(feature = "cloud-transcription")]
fn measured_transcription(id: &str) -> Option<MeasuredTranscription> {
    MEASURED_TRANSCRIPTION
        .iter()
        .find(|(model, _)| *model == id)
        .map(|(_, words_wrong)| MeasuredTranscription {
            words_wrong: *words_wrong,
            clip_words: CLIP_WORDS,
        })
}

/// Ids that also run entirely on this machine, with no cloud request at all
/// -- tagged so the page can say so even though they are listed here as an
/// OpenRouter option too.
#[cfg(feature = "cloud-transcription")]
const ALSO_RUNS_LOCALLY: &[&str] = &[
    "nvidia/parakeet-tdt-0.6b-v3",
    "nvidia/nemotron-3.5-asr-streaming-multilingual-0.6b",
];

/// One transcription model OpenRouter offers.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionOffer {
    pub id: String,
    pub name: String,
    pub measured: Option<MeasuredTranscription>,
    pub also_runs_locally: bool,
}

#[cfg(feature = "cloud-transcription")]
fn transcription_offers_from(
    models: &[macalla_engines::cloud::TranscriptionModel],
) -> Vec<TranscriptionOffer> {
    models
        .iter()
        .map(|m| TranscriptionOffer {
            id: m.id.clone(),
            name: m.name.clone(),
            measured: measured_transcription(&m.id),
            also_runs_locally: ALSO_RUNS_LOCALLY.contains(&m.id.as_str()),
        })
        .collect()
}

/// Transcription offers: fetched from
/// [`macalla_engines::cloud::list_transcription_models`], cached the same
/// way as [`cleanup_offers`]. Behind the `cloud` feature (the app is complete without cloud engines):
/// this is a cloud listing, and a build with no cloud transcription compiled
/// in has nothing to list. Callers (the desktop command, the CLI) call this
/// unconditionally either way, the same as [`crate::choice::cloud_engine`] --
/// [`Error::CloudNotCompiledIn`] below is the answer for a build with the
/// feature off, so nothing above this module needs its own feature check.
#[cfg(feature = "cloud-transcription")]
pub fn transcription_offers(
    cache_dir: &Path,
    refresh: Refresh,
    now_ms: u64,
) -> Result<Offers<TranscriptionOffer>> {
    let raw = cached_listing(
        cache_dir,
        "openrouter-transcription.json",
        refresh,
        now_ms,
        || {
            macalla_engines::cloud::list_transcription_models()
                .map(|listing| listing.models)
                .map_err(|e| e.to_string())
        },
    )?;
    Ok(Offers {
        offers: transcription_offers_from(&raw.models),
        fetched_at_ms: raw.fetched_at_ms,
        stale_reason: raw.stale_reason,
    })
}

/// Without the `cloud` feature, no cloud listing can be fetched at all
/// (the app must ship and work with none compiled in).
#[cfg(not(feature = "cloud-transcription"))]
pub fn transcription_offers(
    _cache_dir: &Path,
    _refresh: Refresh,
    _now_ms: u64,
) -> Result<Offers<TranscriptionOffer>> {
    Err(Error::CloudNotCompiledIn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_enhancers::openrouter::{Reasoning, TextModel};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macalla-session-openrouter-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn text_model(id: &str) -> TextModel {
        TextModel {
            id: id.to_string(),
            name: String::new(),
            input_modalities: vec!["text".to_string()],
            output_modalities: vec!["text".to_string()],
            reasoning: None,
            supported_parameters: vec![],
            expiration_date: None,
        }
    }

    // -------------------------------------------------------------- filter

    #[test]
    fn text_only_requires_text_in_and_only_text_out() {
        let mut m = text_model("a");
        assert!(is_text_only(&m));
        m.output_modalities = vec!["text".to_string(), "image".to_string()];
        assert!(!is_text_only(&m));
        m.output_modalities = vec!["text".to_string()];
        m.input_modalities = vec!["image".to_string()];
        assert!(!is_text_only(&m));
    }

    #[test]
    fn current_drops_batch_alias_and_expiring_ids() {
        assert!(is_current(&text_model("openai/gpt-5")));
        assert!(!is_current(&text_model("openai/gpt-5:batch")));
        assert!(!is_current(&text_model("~openai/gpt-luna-latest")));
        let mut expiring = text_model("openai/old");
        expiring.expiration_date = Some("2026-12-31".to_string());
        assert!(!is_current(&expiring));
    }

    #[test]
    fn classify_covers_every_reasoning_shape() {
        let mut none_at_all = text_model("a");
        assert_eq!(classify(&none_at_all), Some(ReasoningClass::NoReasoning));

        none_at_all.supported_parameters = vec!["reasoning".to_string()];
        assert_eq!(
            classify(&none_at_all),
            Some(ReasoningClass::CanTurnOff),
            "reasoning is named as a parameter with no object: missing counts as not mandatory"
        );

        let mut can_turn_off = text_model("b");
        can_turn_off.reasoning = Some(Reasoning {
            mandatory: Some(false),
            supported_efforts: vec!["low".to_string(), "high".to_string()],
        });
        assert_eq!(classify(&can_turn_off), Some(ReasoningClass::CanTurnOff));

        let mut mandatory_null = text_model("b2");
        mandatory_null.reasoning = Some(Reasoning {
            mandatory: None,
            supported_efforts: vec!["low".to_string(), "high".to_string()],
        });
        assert_eq!(
            classify(&mandatory_null),
            Some(ReasoningClass::CanTurnOff),
            "a present reasoning object with mandatory: null counts as not mandatory"
        );

        let mut minimal = text_model("c");
        minimal.reasoning = Some(Reasoning {
            mandatory: Some(true),
            supported_efforts: vec!["minimal".to_string(), "low".to_string()],
        });
        assert_eq!(classify(&minimal), Some(ReasoningClass::Minimal));

        let mut none_effort = text_model("d");
        none_effort.reasoning = Some(Reasoning {
            mandatory: Some(true),
            supported_efforts: vec!["none".to_string()],
        });
        assert_eq!(classify(&none_effort), Some(ReasoningClass::Minimal));

        let mut too_much = text_model("e");
        too_much.reasoning = Some(Reasoning {
            mandatory: Some(true),
            supported_efforts: vec!["low".to_string(), "high".to_string()],
        });
        assert_eq!(
            classify(&too_much),
            None,
            "mandatory with no low-enough effort is dropped"
        );
    }

    /// A JSON `"mandatory": null` -- distinct from the field being absent --
    /// deserializes to `Reasoning::mandatory: None` and classifies the same
    /// way as an absent `mandatory` field would.
    #[test]
    fn a_json_mandatory_null_deserializes_to_none_and_classifies_as_can_turn_off() {
        let raw = serde_json::json!({
            "data": [{
                "id": "acme/mandatory-null",
                "architecture": {
                    "input_modalities": ["text"],
                    "output_modalities": ["text"],
                },
                "reasoning": {
                    "mandatory": null,
                    "supported_efforts": ["low", "high"],
                },
                "supported_parameters": ["reasoning"],
                "expiration_date": null,
            }],
        })
        .to_string();
        let listing = macalla_enhancers::openrouter::parse_listing(&raw).unwrap();
        let model = &listing.models[0];
        assert_eq!(
            model.reasoning.as_ref().unwrap().mandatory,
            None,
            "a JSON null is a present object with no mandatory value, not an absent object"
        );
        assert_eq!(classify(model), Some(ReasoningClass::CanTurnOff));
    }

    #[test]
    fn measured_cleanup_attaches_by_exact_id_only() {
        assert_eq!(
            measured_cleanup("openai/gpt-5.6-luna"),
            Some(MeasuredCleanup {
                right: 19,
                of: 20,
                machine: "measured direct from the maker, not through OpenRouter",
            })
        );
        assert_eq!(measured_cleanup("openai/gpt-5.6-luna-preview"), None);
    }

    /// The 2026-09-12 snapshot: 445 models, 430 text-only, 333 current, 276
    /// kept (113 no-reasoning, 151 can-turn-off, 12 minimal). Skipped with a
    /// message if the file is not present.
    #[test]
    fn the_filter_matches_the_exact_counts_on_the_default_snapshot() {
        let path = format!(
            "{}/../../fixtures/provider-snapshots/openrouter/models-default-2026-09-12.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let Ok(raw) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: {path} is not present");
            return;
        };
        let listing = macalla_enhancers::openrouter::parse_listing(&raw).unwrap();
        assert_eq!(listing.models.len(), 445);

        let text_only: Vec<_> = listing.models.iter().filter(|m| is_text_only(m)).collect();
        assert_eq!(text_only.len(), 430);

        let current: Vec<_> = text_only.into_iter().filter(|m| is_current(m)).collect();
        assert_eq!(current.len(), 333);

        let offers = cleanup_offers_from(&listing.models);
        assert_eq!(offers.len(), 276);
        let count = |class| offers.iter().filter(|o| o.reasoning == class).count();
        assert_eq!(count(ReasoningClass::NoReasoning), 113);
        assert_eq!(count(ReasoningClass::CanTurnOff), 151);
        assert_eq!(count(ReasoningClass::Minimal), 12);
    }

    // --------------------------------------------------------- transcription

    #[cfg(feature = "cloud-transcription")]
    #[test]
    fn transcription_offers_attach_measured_figures_and_the_local_tag_by_exact_id() {
        use macalla_engines::cloud::TranscriptionModel;
        let model = |id: &str| TranscriptionModel {
            id: id.to_string(),
            name: String::new(),
            input_modalities: vec!["audio".to_string()],
            output_modalities: vec!["transcription".to_string()],
            expiration_date: None,
        };
        let models = vec![
            model("openai/gpt-transcribe"),
            model("nvidia/parakeet-tdt-0.6b-v3"),
            model("some/other-model"),
        ];
        let offers = transcription_offers_from(&models);
        let by_id = |id: &str| offers.iter().find(|o| o.id == id).unwrap();

        assert_eq!(
            by_id("openai/gpt-transcribe").measured,
            Some(MeasuredTranscription {
                words_wrong: [0, 3, 8],
                clip_words: [80, 30, 64],
            })
        );
        assert!(!by_id("openai/gpt-transcribe").also_runs_locally);
        assert!(by_id("nvidia/parakeet-tdt-0.6b-v3").also_runs_locally);
        assert_eq!(by_id("nvidia/parakeet-tdt-0.6b-v3").measured, None);
        assert_eq!(by_id("some/other-model").measured, None);
        assert!(!by_id("some/other-model").also_runs_locally);
    }

    #[cfg(not(feature = "cloud-transcription"))]
    #[test]
    fn without_the_cloud_feature_transcription_offers_is_not_compiled_in() {
        let dir = temp_dir("not-compiled-in");
        let result = transcription_offers(&dir, Refresh::Now, 0);
        assert!(matches!(result, Err(Error::CloudNotCompiledIn)));
    }

    // --------------------------------------------------------------- cache

    fn write_raw_cache(dir: &Path, file_name: &str, fetched_at_ms: u64, models: &[TextModel]) {
        std::fs::create_dir_all(dir).unwrap();
        let json = serde_json::to_string(&StoredListingRef {
            fetched_at_ms,
            models,
        })
        .unwrap();
        std::fs::write(dir.join(file_name), json).unwrap();
    }

    const FILE: &str = "cache.json";
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    #[test]
    fn a_fresh_cache_is_not_refetched() {
        let dir = temp_dir("fresh");
        let now = 10 * DAY_MS;
        write_raw_cache(&dir, FILE, now - DAY_MS, &[text_model("cached")]);

        let result = cached_listing(
            &dir,
            FILE,
            Refresh::IfStale,
            now,
            || -> std::result::Result<Vec<TextModel>, String> {
                panic!("a fresh cache must not be refetched")
            },
        )
        .unwrap();

        assert_eq!(result.models, vec![text_model("cached")]);
        assert_eq!(result.fetched_at_ms, now - DAY_MS);
        assert_eq!(result.stale_reason, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_cache_is_refetched_and_the_cache_file_is_updated() {
        let dir = temp_dir("stale");
        let now = 10 * DAY_MS;
        write_raw_cache(&dir, FILE, now - 8 * DAY_MS, &[text_model("old")]);

        let result = cached_listing(&dir, FILE, Refresh::IfStale, now, || {
            Ok(vec![text_model("fresh")])
        })
        .unwrap();

        assert_eq!(result.models, vec![text_model("fresh")]);
        assert_eq!(result.fetched_at_ms, now);
        assert_eq!(result.stale_reason, None);

        // The cache file itself was updated, so the next call sees it fresh.
        let again = cached_listing(
            &dir,
            FILE,
            Refresh::IfStale,
            now,
            || -> std::result::Result<Vec<TextModel>, String> {
                panic!("must not refetch: the cache was just written fresh")
            },
        )
        .unwrap();
        assert_eq!(again.models, vec![text_model("fresh")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_now_always_fetches_even_with_a_fresh_cache() {
        let dir = temp_dir("now");
        let now = 10 * DAY_MS;
        write_raw_cache(&dir, FILE, now - 1, &[text_model("old")]);

        let result = cached_listing(&dir, FILE, Refresh::Now, now, || {
            Ok(vec![text_model("fresh")])
        })
        .unwrap();

        assert_eq!(result.models, vec![text_model("fresh")]);
        assert_eq!(result.fetched_at_ms, now);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_fetch_falls_back_to_the_cache_with_the_reason() {
        let dir = temp_dir("fallback");
        let now = 10 * DAY_MS;
        write_raw_cache(&dir, FILE, now - 8 * DAY_MS, &[text_model("cached")]);

        let result = cached_listing(&dir, FILE, Refresh::Now, now, || {
            Err::<Vec<TextModel>, _>("the network is down".to_string())
        })
        .unwrap();

        assert_eq!(result.models, vec![text_model("cached")]);
        assert_eq!(result.fetched_at_ms, now - 8 * DAY_MS);
        assert_eq!(result.stale_reason.as_deref(), Some("the network is down"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_fetch_with_no_cache_is_an_error() {
        let dir = temp_dir("no-cache-error");
        let now = 10 * DAY_MS;

        let error = cached_listing(&dir, FILE, Refresh::Now, now, || {
            Err::<Vec<TextModel>, _>("the network is down".to_string())
        })
        .unwrap_err();

        assert!(matches!(
            error,
            Error::OpenRouterFetch(reason) if reason == "the network is down"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_cache_file_is_treated_as_no_cache() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(FILE), b"not json at all").unwrap();
        let now = 10 * DAY_MS;

        let result = cached_listing(&dir, FILE, Refresh::IfStale, now, || {
            Ok(vec![text_model("fresh")])
        })
        .unwrap();

        assert_eq!(result.models, vec![text_model("fresh")]);
        assert_eq!(result.fetched_at_ms, now);
        assert_eq!(result.stale_reason, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The clock moving backwards (or a corrupt/malicious cache claiming a
    /// future fetch time) must not leave a cache that can never go stale.
    #[test]
    fn a_cache_dated_in_the_future_is_treated_as_stale() {
        let dir = temp_dir("future");
        let now = 10 * DAY_MS;
        write_raw_cache(&dir, FILE, now + DAY_MS, &[text_model("from-the-future")]);

        let result = cached_listing(&dir, FILE, Refresh::IfStale, now, || {
            Ok(vec![text_model("fresh")])
        })
        .unwrap();

        assert_eq!(result.models, vec![text_model("fresh")]);
        assert_eq!(result.fetched_at_ms, now);
        assert_eq!(result.stale_reason, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache directory that cannot be created (something else already
    /// occupies that path) must not turn a successful fetch into an error --
    /// only the next `IfStale` skip is lost.
    #[test]
    fn an_uncreatable_cache_directory_does_not_fail_a_successful_fetch() {
        let dir = temp_dir("uncreatable");
        // Put a plain file where the cache directory needs to be, so
        // `std::fs::create_dir_all(dir)` fails: `dir` exists and is not a
        // directory.
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&dir, b"not a directory").unwrap();
        let now = 10 * DAY_MS;

        let result = cached_listing(&dir, FILE, Refresh::Now, now, || {
            Ok(vec![text_model("fresh")])
        })
        .unwrap();

        assert_eq!(result.models, vec![text_model("fresh")]);
        assert_eq!(result.fetched_at_ms, now);
        assert_eq!(result.stale_reason, None);
        let _ = std::fs::remove_file(&dir);
    }

    /// Many threads writing the same cache file at once must never leave it
    /// part-written: each write goes to its own unique tmp path and only the
    /// rename is visible, so whichever writer's rename lands last, the file
    /// afterwards is always exactly one writer's whole content.
    #[test]
    fn concurrent_writes_never_corrupt_the_cache_file() {
        let dir = temp_dir("concurrent-writes");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);

        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..25u64 {
                        write_cache(&path, t * 1000 + i, &[text_model(&format!("m{t}-{i}"))]);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: StoredListing<TextModel> = serde_json::from_str(&text)
            .expect("the file is always one writer's complete content, never an interleave");
        assert_eq!(
            parsed.models.len(),
            1,
            "one write's whole content, not a splice of two"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
