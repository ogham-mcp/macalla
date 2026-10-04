// SPDX-License-Identifier: MIT
//! OpenAI's own model listing: `GET https://api.openai.com/v1/models`, key as
//! a bearer token. Plain data -- no policy: which of these are text models
//! worth offering for cleanup is `macalla-session`'s job, not this crate's
//! (the same split [`crate::openrouter`] and
//! [`crate::anthropic::list_models`] follow). OpenAI's reply carries no type
//! or capability field at all (per the provider's OpenAPI description),
//! so the session tells models apart by id naming; this module only turns
//! the wire shape into plain data, reusing [`crate::endpoint`]'s bounded
//! `GET` and list-sized timeouts since the shape is the same OpenAI-compatible
//! `/v1/models` that module already fetches for a person's own server.

use macalla_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::endpoint::{get, list_agent, redacted, MAX_MODELS};

/// <https://platform.openai.com/docs/api-reference/models/list>
pub const LISTING_URL: &str = "https://api.openai.com/v1/models";

const SOURCE: &str = "openai";

/// One model from OpenAI's listing. Plain data -- no policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    /// Missing only if a future reply omits it; today's schema requires it.
    #[serde(default)]
    pub owned_by: Option<String>,
    /// Unix timestamp, seconds. Missing only if a future reply omits it.
    #[serde(default)]
    pub created: Option<i64>,
    /// "The date when the model will shut down, or null if not announced"
    /// (per the provider's OpenAPI description). `#[serde(default)]` so a listing cached before this field
    /// was read still parses.
    #[serde(default)]
    pub shutdown_date: Option<String>,
}

/// [`list_models`]'s result: the models, and how many entries were skipped
/// for carrying no usable `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub models: Vec<Model>,
    pub skipped: usize,
}

/// Fetch and parse OpenAI's model listing. `key` is sent as a bearer token.
pub fn list_models(key: &str) -> Result<Listing> {
    list_models_at(LISTING_URL, key)
}

/// [`list_models`], with the URL as a parameter so a test can point it at a
/// 127.0.0.1 fake server instead of the real API.
fn list_models_at(url: &str, key: &str) -> Result<Listing> {
    let raw = get(&list_agent(), url, Some(key))
        .map_err(|e| error(format!("the request to {} failed: {e}", redacted(url))))?;
    parse_listing(&raw)
}

fn error(message: String) -> Error {
    Error::Enhance {
        backend: SOURCE,
        message,
    }
}

#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    owned_by: Option<String>,
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    shutdown_date: Option<String>,
}

impl From<WireModel> for Model {
    fn from(w: WireModel) -> Self {
        Model {
            id: w.id,
            owned_by: w.owned_by,
            created: w.created,
            shutdown_date: w.shutdown_date,
        }
    }
}

/// Parse a listing reply, from the network or a saved snapshot alike. A
/// reply with no `data` array is a clear error, since `/v1/models` always
/// answers in that shape; an entry with no usable `id` is skipped and
/// counted rather than failing the whole listing, the same rule
/// [`crate::endpoint::list_names`] uses for a stricter API that promises
/// every entry has one. Sorted newest [`Model::created`] first (missing
/// last, then by id for a stable order) *before* the [`MAX_MODELS`] cap
/// [`crate::endpoint::list_names`] also applies, so the cap drops the
/// oldest models rather than an arbitrary slice of the wire order.
pub fn parse_listing(raw: &str) -> Result<Listing> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| error("the reply was not valid JSON".into()))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| error("the reply had no model list in it".into()))?;

    let mut models = Vec::new();
    let mut skipped = 0usize;
    for entry in data {
        match serde_json::from_value::<WireModel>(entry.clone()) {
            Ok(wire) => models.push(Model::from(wire)),
            Err(_) => skipped += 1,
        }
    }
    models.sort_by_key(|m| {
        (
            m.created.is_none(),
            std::cmp::Reverse(m.created),
            m.id.clone(),
        )
    });
    models.truncate(MAX_MODELS);
    Ok(Listing { models, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve_once;

    fn sample_reply() -> String {
        serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": "gpt-5.6-thinking",
                    "object": "model",
                    "created": 1_789_129_412u64,
                    "owned_by": "openai",
                    "shutdown_date": null,
                },
                {
                    "id": "gpt-5.6-mini",
                    "object": "model",
                    "created": 1_789_129_500u64,
                    "owned_by": "openai",
                },
                {"object": "model", "created": 1_789_129_412u64},
            ],
        })
        .to_string()
    }

    #[test]
    fn a_reply_is_parsed_into_plain_models_and_an_entry_with_no_id_is_skipped_and_counted() {
        let listing = parse_listing(&sample_reply()).unwrap();
        assert_eq!(listing.skipped, 1);
        assert_eq!(listing.models.len(), 2);
        // Newest (`created`) first: gpt-5.6-mini's timestamp is later.
        assert_eq!(listing.models[0].id, "gpt-5.6-mini");
        assert_eq!(listing.models[0].owned_by.as_deref(), Some("openai"));
        assert_eq!(listing.models[0].created, Some(1_789_129_500));
    }

    #[test]
    fn shutdown_date_is_read_when_present_and_defaults_to_none_when_the_field_is_missing() {
        // The network path: parse_listing, through WireModel. Written the
        // way a listing cached before this field was read would be: the
        // second entry has no `shutdown_date` key at all.
        let raw = serde_json::json!({
            "data": [
                {"id": "gpt-5", "created": 1u64, "shutdown_date": "2027-01-01"},
                {"id": "gpt-4.1", "created": 2u64},
            ],
        })
        .to_string();
        let listing = parse_listing(&raw).unwrap();
        let find = |id: &str| listing.models.iter().find(|m| m.id == id).unwrap();
        assert_eq!(find("gpt-5").shutdown_date.as_deref(), Some("2027-01-01"));
        assert_eq!(find("gpt-4.1").shutdown_date, None);

        // The cache path: `cloud_pool.rs`'s `cached_listing_or` deserialises
        // `Vec<Model>` directly, with no WireModel step. `Model`'s own
        // `#[serde(default)]` must cover a cached entry from before this
        // field existed the same way.
        let cached: Vec<Model> =
            serde_json::from_str(r#"[{"id": "gpt-4.1", "owned_by": "openai", "created": 2}]"#)
                .unwrap();
        assert_eq!(cached[0].shutdown_date, None);
    }

    #[test]
    fn a_reply_with_no_data_array_is_an_error() {
        let error = parse_listing(&serde_json::json!({"object": "list"}).to_string()).unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error}");
    }

    #[test]
    fn a_reply_that_is_not_json_is_an_error() {
        let error = parse_listing("not json").unwrap_err();
        assert!(error.to_string().contains("not valid JSON"), "{error}");
    }

    #[test]
    fn a_fake_server_reply_is_fetched_with_the_bearer_header() {
        let (address, server) = serve_once("200 OK", sample_reply());
        let listing = list_models_at(&address, "sk-test").unwrap();
        assert_eq!(listing.models.len(), 2);
        let (head, _) = server.join().unwrap();
        assert!(head.starts_with("get / "), "{head}");
        assert!(head.contains("authorization: bearer sk-test"), "{head}");
    }

    #[test]
    fn a_401_is_an_error_whose_message_does_not_contain_the_key() {
        let (address, server) = serve_once("401 Unauthorized", "{}".into());
        let error = list_models_at(&address, "sk-test").unwrap_err();
        assert!(!error.to_string().contains("sk-test"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn models_are_sorted_newest_first_with_none_last_then_by_id() {
        let raw = serde_json::json!({
            "data": [
                {"id": "b-no-date", "object": "model"},
                {"id": "gpt-old", "object": "model", "created": 100u64},
                {"id": "gpt-new", "object": "model", "created": 300u64},
                {"id": "a-no-date", "object": "model"},
                {"id": "gpt-mid", "object": "model", "created": 200u64},
            ],
        })
        .to_string();
        let listing = parse_listing(&raw).unwrap();
        let ids: Vec<&str> = listing.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["gpt-new", "gpt-mid", "gpt-old", "a-no-date", "b-no-date"]
        );
    }

    #[test]
    fn the_cap_drops_the_oldest_models_not_an_arbitrary_slice() {
        let total = MAX_MODELS + 10;
        let entries: Vec<Value> = (0..total)
            .map(|i| {
                // Ascending id/created order: the oldest models come first
                // on the wire, so a cap that simply took the first
                // MAX_MODELS entries (instead of sorting first) would keep
                // the oldest ones and drop the newest -- the opposite of
                // what's wanted.
                serde_json::json!({"id": format!("m{i:04}"), "created": i as u64})
            })
            .collect();
        let raw = serde_json::json!({"data": entries}).to_string();

        let listing = parse_listing(&raw).unwrap();

        assert_eq!(listing.models.len(), MAX_MODELS);
        // Newest-first: the highest `created` values survive the cap.
        assert_eq!(listing.models[0].id, format!("m{:04}", total - 1));
        assert_eq!(
            listing.models[MAX_MODELS - 1].id,
            format!("m{:04}", total - MAX_MODELS)
        );
        for pair in listing.models.windows(2) {
            assert!(pair[0].created >= pair[1].created, "{listing:?}");
        }
    }
}
