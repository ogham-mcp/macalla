// SPDX-License-Identifier: MIT
//! Mistral's own model listing: `GET https://api.mistral.ai/v1/models`, key
//! as a bearer token, same OpenAI-shaped `/v1/models` envelope
//! [`crate::openai`] and [`crate::endpoint`] speak -- this module reuses
//! [`crate::endpoint`]'s bounded `GET` and list-sized timeouts the same way
//! [`crate::openai`] does. Its own module because a Mistral model card
//! carries fields (`capabilities.completion_chat`, `max_context_length`,
//! `archived`, `deprecation`, `aliases`) OpenAI's does not
//! (per the provider's OpenAPI description). Plain data --
//! no policy: which of these are chat-capable, and worth offering, is
//! `macalla-session`'s job.

use macalla_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::endpoint::{get, list_agent, redacted, MAX_MODELS};

/// <https://docs.mistral.ai/api/#tag/models/operation/list_models_v1_models_get>
pub const LISTING_URL: &str = "https://api.mistral.ai/v1/models";

const SOURCE: &str = "mistral";

/// One model from Mistral's listing. Plain data -- no policy. `BaseModelCard`
/// and `FTModelCard` (fine-tuned) share every field kept here, so both parse
/// the same way; a fine-tuned card's extra fields (`job`, `root`) are
/// ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    /// `false` when `capabilities` (or its `completion_chat` field) is
    /// absent, not an error: the schema marks `capabilities` required, but a
    /// reply that omits it is still a listing worth returning.
    pub completion_chat: bool,
    pub max_context_length: Option<u64>,
    pub archived: bool,
    pub deprecation: Option<String>,
    pub aliases: Vec<String>,
}

/// [`list_models`]'s result: the models, and how many entries were skipped
/// for carrying no usable `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub models: Vec<Model>,
    pub skipped: usize,
}

/// Fetch and parse Mistral's model listing. `key` is sent as a bearer token.
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

#[derive(Default, Deserialize)]
struct WireCapabilities {
    #[serde(default)]
    completion_chat: bool,
}

#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    capabilities: Option<WireCapabilities>,
    #[serde(default)]
    max_context_length: Option<u64>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    deprecation: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

impl From<WireModel> for Model {
    fn from(w: WireModel) -> Self {
        Model {
            id: w.id,
            completion_chat: w.capabilities.unwrap_or_default().completion_chat,
            max_context_length: w.max_context_length,
            archived: w.archived,
            deprecation: w.deprecation,
            aliases: w.aliases,
        }
    }
}

/// Parse a listing reply, from the network or a saved snapshot alike. A
/// reply with no `data` array is a clear error, since Mistral's own schema
/// (`ModelList`) always wraps entries that way; an entry with no usable `id`
/// is skipped and counted, the same rule [`crate::openai::parse_listing`]
/// uses. Capped at [`MAX_MODELS`], the same bound
/// [`crate::endpoint::list_names`] applies.
pub fn parse_listing(raw: &str) -> Result<Listing> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| error("the reply was not valid JSON".into()))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| error("the reply had no model list in it".into()))?;

    let mut models = Vec::new();
    let mut skipped = 0usize;
    for entry in data.iter().take(MAX_MODELS) {
        match serde_json::from_value::<WireModel>(entry.clone()) {
            Ok(wire) => models.push(Model::from(wire)),
            Err(_) => skipped += 1,
        }
    }
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
                    "id": "mistral-medium-2510",
                    "object": "model",
                    "created": 1_756_746_619u64,
                    "owned_by": "mistralai",
                    "capabilities": {
                        "completion_chat": true,
                        "completion_fim": false,
                        "function_calling": true,
                        "vision": false,
                    },
                    "max_context_length": 131_072,
                    "aliases": ["mistral-medium-latest"],
                    "archived": false,
                    "type": "base",
                },
                {
                    "id": "ft:open-mistral-7b:acme:20260101:abcd1234",
                    "object": "model",
                    "created": 1_756_746_619u64,
                    "owned_by": "acme",
                    "capabilities": {"completion_chat": true, "completion_fim": false},
                    "job": "job-123",
                    "root": "open-mistral-7b",
                    "max_context_length": 32_768,
                    "aliases": [],
                    "archived": true,
                    "deprecation": "2026-12-01T00:00:00Z",
                    "type": "fine-tuned",
                },
                {"object": "model"},
            ],
        })
        .to_string()
    }

    #[test]
    fn a_reply_is_parsed_into_plain_models_and_an_entry_with_no_id_is_skipped_and_counted() {
        let listing = parse_listing(&sample_reply()).unwrap();
        assert_eq!(listing.skipped, 1);
        assert_eq!(listing.models.len(), 2);

        let medium = &listing.models[0];
        assert_eq!(medium.id, "mistral-medium-2510");
        assert!(medium.completion_chat);
        assert_eq!(medium.max_context_length, Some(131_072));
        assert!(!medium.archived);
        assert_eq!(medium.deprecation, None);
        assert_eq!(medium.aliases, vec!["mistral-medium-latest"]);

        let fine_tuned = &listing.models[1];
        assert!(fine_tuned.completion_chat);
        assert!(fine_tuned.archived);
        assert_eq!(
            fine_tuned.deprecation.as_deref(),
            Some("2026-12-01T00:00:00Z")
        );
        assert!(fine_tuned.aliases.is_empty());
    }

    #[test]
    fn a_model_with_no_capabilities_object_gets_completion_chat_false() {
        let reply = serde_json::json!({
            "object": "list",
            "data": [{"id": "bare-model"}],
        })
        .to_string();
        let listing = parse_listing(&reply).unwrap();
        assert_eq!(listing.models.len(), 1);
        assert!(!listing.models[0].completion_chat);
        assert_eq!(listing.models[0].max_context_length, None);
    }

    #[test]
    fn a_reply_with_no_data_array_is_an_error() {
        let error = parse_listing(&serde_json::json!({"object": "list"}).to_string()).unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error}");
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
}
