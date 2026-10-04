// SPDX-License-Identifier: MIT
//! OpenRouter's public text-model listing: `GET
//! https://openrouter.ai/api/v1/models`, no key. This module only
//! parses the wire shape into plain data -- which of these models are worth
//! offering for cleanup, Macalla's measured figures for them, and the
//! on-disk cache are `macalla-session::openrouter`'s policy, not this
//! crate's (the same split made for the two engines/enhancers crates each
//! parsing an OpenAI-shaped `/models` reply).

use macalla_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// The default listing: every model whose only output is text (transcription
/// and other non-text models need `?output_modalities=...` and are not here;
/// see the provider's documentation).
pub const LISTING_URL: &str = "https://openrouter.ai/api/v1/models";

const SOURCE: &str = "openrouter";

/// Long enough for a public HTTPS endpoint to accept the connection over the
/// open internet, not just a machine on the local network -- see
/// [`crate::endpoint`]'s `CONNECT`, which this is longer than for that
/// reason. Not measured.
const CONNECT: Duration = Duration::from_secs(5);

/// The untrimmed default listing was 735 KB on 2026-09-12
/// (per the provider's documentation); 15 s comfortably covers
/// reading that much JSON over a slow connection without waiting anywhere
/// near a whole-recording budget. Not measured.
const REQUEST: Duration = Duration::from_secs(15);

/// The untrimmed default listing was 735 KB on 2026-09-12; 4 MiB gives
/// headroom for OpenRouter's catalogue to grow well past five times that
/// size before this needs to move again, while still refusing an unbounded
/// reply from a misbehaving proxy. Checked on the *decoded* body, by
/// [`macalla_core::bounded_read::read_capped`] -- `ureq`'s own `.limit()`
/// sits beneath its gzip/brotli decoder and would only bound the compressed
/// wire bytes, not what the `String` this becomes actually holds.
const BODY_LIMIT: u64 = 4 * 1024 * 1024;

/// One model's reasoning support, from its `reasoning` object. `None` means
/// the field itself was missing or `null` in the reply -- not the same as a
/// present object saying reasoning is optional, which is
/// [`Reasoning::mandatory`] being `Some(false)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    pub mandatory: Option<bool>,
    #[serde(default)]
    pub supported_efforts: Vec<String>,
}

/// One model from the text listing. Plain data -- no policy: which models
/// are worth offering, and Macalla's measured figures, are
/// `macalla-session::openrouter`'s job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
    pub reasoning: Option<Reasoning>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    pub expiration_date: Option<String>,
}

/// [`list_text_models`]'s result: the models, and how many entries were
/// skipped for carrying no usable `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub models: Vec<TextModel>,
    pub skipped: usize,
}

/// Fetch and parse the text listing. No key: this endpoint is public.
pub fn list_text_models() -> Result<Listing> {
    let raw = get(LISTING_URL, &agent())?;
    parse_listing(&raw)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT))
        .timeout_global(Some(REQUEST))
        .build()
        .into()
}

fn error(message: String) -> Error {
    Error::Enhance {
        backend: SOURCE,
        message,
    }
}

fn get(url: &str, agent: &ureq::Agent) -> Result<String> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| error(format!("the request to {url} failed: {e}")))?;
    // `.reader()` decodes gzip/brotli before read_capped ever sees a byte,
    // so the cap below is on the decoded body -- see BODY_LIMIT's doc.
    let reader = response.body_mut().with_config().reader();
    macalla_core::bounded_read::read_capped(reader, BODY_LIMIT)
        .map_err(|e| error(format!("reading the reply from {url}: {e}")))
}

/// The shape actually on the wire: `architecture.{input,output}_modalities`
/// nested, everything else flat. Converted into the flat [`TextModel`] once
/// parsed, so the rest of the codebase (and the on-disk cache, which
/// round-trips [`TextModel`] itself) never has to know OpenRouter nests
/// modalities under `architecture`.
#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    architecture: WireArchitecture,
    #[serde(default)]
    reasoning: Option<Reasoning>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    #[serde(default)]
    expiration_date: Option<String>,
}

#[derive(Default, Deserialize)]
struct WireArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
}

impl From<WireModel> for TextModel {
    fn from(w: WireModel) -> Self {
        TextModel {
            id: w.id,
            name: w.name,
            input_modalities: w.architecture.input_modalities,
            output_modalities: w.architecture.output_modalities,
            reasoning: w.reasoning,
            supported_parameters: w.supported_parameters,
            expiration_date: w.expiration_date,
        }
    }
}

/// Parse a text listing reply, from the network or a saved snapshot alike. A
/// reply with no `data` array is a clear error, since this route is the one
/// OpenRouter's own docs describe; an entry with no usable `id` is skipped
/// and counted rather than failing the whole listing, the same rule
/// [`crate::endpoint::list_models`] uses for a stricter API that promises
/// every entry has one.
pub fn parse_listing(raw: &str) -> Result<Listing> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| error("the reply was not valid JSON".into()))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| error("the reply had no model list in it".into()))?;

    let mut models = Vec::with_capacity(data.len());
    let mut skipped = 0usize;
    for entry in data {
        match serde_json::from_value::<WireModel>(entry.clone()) {
            Ok(wire) => models.push(TextModel::from(wire)),
            Err(_) => skipped += 1,
        }
    }
    Ok(Listing { models, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::serve_once;
    use serde_json::json;
    use std::net::TcpListener;
    use std::time::Instant;

    fn sample_reply() -> String {
        json!({
            "data": [
                {
                    "id": "openai/gpt-5.6-luna",
                    "name": "OpenAI: GPT-5.6 Luna",
                    "architecture": {
                        "input_modalities": ["text", "image"],
                        "output_modalities": ["text"],
                    },
                    "reasoning": {
                        "mandatory": true,
                        "default_enabled": true,
                        "supported_efforts": ["max", "high", "medium", "low"],
                        "default_effort": "medium",
                    },
                    "supported_parameters": ["temperature", "reasoning"],
                    "expiration_date": null,
                },
                {
                    "id": "acme/plain-text",
                    "architecture": {
                        "input_modalities": ["text"],
                        "output_modalities": ["text"],
                    },
                    "supported_parameters": ["temperature"],
                    "expiration_date": null,
                },
                {"name": "no id here"},
            ],
        })
        .to_string()
    }

    #[test]
    fn a_reply_is_parsed_into_plain_models_and_an_entry_with_no_id_is_skipped_and_counted() {
        let listing = parse_listing(&sample_reply()).unwrap();
        assert_eq!(listing.skipped, 1);
        assert_eq!(listing.models.len(), 2);

        let luna = &listing.models[0];
        assert_eq!(luna.id, "openai/gpt-5.6-luna");
        assert_eq!(luna.name, "OpenAI: GPT-5.6 Luna");
        assert_eq!(luna.input_modalities, vec!["text", "image"]);
        assert_eq!(luna.output_modalities, vec!["text"]);
        let reasoning = luna.reasoning.as_ref().unwrap();
        assert_eq!(reasoning.mandatory, Some(true));
        assert_eq!(
            reasoning.supported_efforts,
            vec!["max", "high", "medium", "low"]
        );
        assert_eq!(luna.expiration_date, None);

        let plain = &listing.models[1];
        assert_eq!(plain.name, "", "a missing name defaults to empty");
        assert!(plain.reasoning.is_none());
    }

    #[test]
    fn a_reply_with_no_data_array_is_an_error() {
        let error = parse_listing(&json!({"object": "list"}).to_string()).unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error}");
    }

    #[test]
    fn a_reply_that_is_not_json_is_an_error() {
        let error = parse_listing("not json").unwrap_err();
        assert!(error.to_string().contains("not valid JSON"), "{error}");
    }

    #[test]
    fn a_fake_server_reply_is_fetched_and_parsed() {
        let (address, server) = serve_once("200 OK", sample_reply());
        let raw = get(&address, &agent()).unwrap();
        let listing = parse_listing(&raw).unwrap();
        assert_eq!(listing.models.len(), 2);
        let (head, _) = server.join().unwrap();
        assert!(head.starts_with("get / "), "{head}");
    }

    #[test]
    fn an_unreachable_address_fails_bounded_and_names_the_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let started = Instant::now();
        let error = get(&address, &agent()).unwrap_err();
        assert!(
            started.elapsed() <= CONNECT * 2,
            "took {:?}",
            started.elapsed()
        );
        assert!(error.to_string().contains(&address), "{error}");
    }

    /// The 2026-09-12 snapshot of the provider's listing:
    /// 445 text models, none skipped. Skipped with a message if the file is
    /// not present -- it is trimmed for the repository's size limit but
    /// otherwise always checked in; a public checkout that dropped it must
    /// not fail the suite over a file it never had.
    #[test]
    fn the_saved_default_snapshot_parses_into_445_models() {
        let path = format!(
            "{}/../../fixtures/provider-snapshots/openrouter/models-default-2026-09-12.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let Ok(raw) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: {path} is not present");
            return;
        };
        let listing = parse_listing(&raw).unwrap();
        assert_eq!(listing.skipped, 0);
        assert_eq!(listing.models.len(), 445);
    }
}
