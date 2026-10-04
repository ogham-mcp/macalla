// SPDX-License-Identifier: MIT
//! Anthropic's own Messages API, not the OpenAI-compatible layer
//! [`crate::endpoint`] already speaks: that layer ignores `reasoning_effort`
//! and is documented as "not considered a long-term or production-ready
//! solution for most use cases"
//! (<https://platform.claude.com/docs/en/cli-sdks-libraries/libraries/openai-sdk>),
//! so it cannot turn a reasoning model's thinking down for the default
//! prompt. The native `thinking` field can.
//!
//! Cloud transcription and cloud enhancement share one shape:
//! the key is a value handed in from the OS keychain by `macalla-platform`,
//! never read here, and a bearer-style header plus its error mapping are
//! repeated across crates rather than pulled into a shared cloud crate that
//! does not exist.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use macalla_core::block::Block;
use macalla_core::enhance::{self, Enhancer, EnhancerCaps, RecentTimes};
use macalla_core::text_model::TextModel;
use macalla_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::endpoint::{redacted, Reasoning, MAX_MODELS};
use crate::provider_error::{self, parse_anthropic_detail};

pub const BACKEND_ID: &str = "anthropic";

/// <https://platform.claude.com/docs/en/api/overview>
const URL: &str = "https://api.anthropic.com/v1/messages";

/// <https://platform.claude.com/docs/en/api/http/models>
const LISTING_URL: &str = "https://api.anthropic.com/v1/models";

/// The only two versions ever published; this is the current, recommended
/// one. <https://platform.claude.com/docs/en/api/versioning>
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Long enough for the API to accept a connection, short enough that a
/// network problem gives up quickly. Matches [`crate::endpoint::Endpoint`];
/// not measured.
const CONNECT: Duration = Duration::from_secs(3);

/// The most one request may take. By then the text has long been pasted from
/// Tier 0, and this only decides whether history gets the enhanced text.
/// Matches [`crate::endpoint::Endpoint`]; not measured.
const WHOLE_REQUEST: Duration = Duration::from_secs(30);

/// Same reasoning as [`crate::endpoint`]'s `LIST_CONNECT`: a model list is a
/// small JSON reply with no generation involved. Not measured.
const LIST_CONNECT: Duration = Duration::from_secs(3);

/// Same reasoning as [`crate::endpoint`]'s `LIST_REQUEST`. Not measured.
const LIST_REQUEST: Duration = Duration::from_secs(5);

/// Same reasoning as [`crate::endpoint`]'s `LIST_BODY_LIMIT`: a model list
/// reply is small JSON, so refusing an oversize one early avoids reading an
/// unbounded body from a server that misbehaves. Checked on the *decoded*
/// body, by [`macalla_core::bounded_read::read_capped`].
const LIST_BODY_LIMIT: u64 = 1024 * 1024;

/// The largest page size Anthropic's `limit` query parameter accepts
/// (per the provider's documentation), so the whole catalogue
/// fits in as few pages as possible.
const PAGE_LIMIT: u32 = 1000;

/// Anthropic's own catalogue is a few dozen models, comfortably one page at
/// [`PAGE_LIMIT`]; this many pages bounds the loop against a server that
/// always answers `has_more: true` without ever making a healthy multi-page
/// reply take long to finish.
const MAX_PAGES: usize = 20;

/// No `Debug`: it holds the key.
pub struct Anthropic {
    url: String,
    model: String,
    key: String,
    prompt: String,
    /// Set by [`Anthropic::with_reasoning`]; otherwise it follows the prompt.
    reasoning: Option<Reasoning>,
    /// The highest ladder stage reached for this Anthropic value so far.
    /// [`Reasoning::Least`] tries stage 0 first: `thinking: {"type":
    /// "disabled"}` and `temperature: 0`. Stage 1 drops `temperature` --
    /// verified: Opus 5, Sonnet 5, Opus 4.8 and Opus 4.7 return 400 for any
    /// non-default temperature whatever the thinking setting. Stage 2 drops
    /// `thinking` too -- verified: Fable 5[.1] and Mythos 5[.1]/Preview also
    /// return 400 for thinking disabled. A 400 at a stage below 2 moves one
    /// stage down the ladder (fewer fields) and is retried immediately; the
    /// stage reached is kept here with `fetch_max` and reused by every later
    /// request for this value, so a refusal costs one extra request per
    /// process, not one per block.
    ///
    /// [`Anthropic::new`]'s agent reads a refused request's body instead of
    /// letting ureq turn it into an opaque status error (see its own doc),
    /// so the retry now logs the provider's own detail. That does not make
    /// this decision exact, though: Anthropic's error envelope carries no `param`
    /// to say which field was refused (see [`parse_anthropic_detail`]), so a
    /// 400 for an unrelated reason -- a mistyped model id -- is still
    /// indistinguishable from a refused field here and still moves down the
    /// ladder the same way, costing up to two extra requests once before
    /// settling at stage 2, same as before this change.
    ladder_stage: AtomicU8,
    agent: ureq::Agent,
    times: Mutex<RecentTimes>,
}

impl Anthropic {
    /// `key` is sent as `x-api-key` on every request.
    pub fn new(model: &str, key: String) -> Self {
        Self {
            url: URL.to_string(),
            model: model.trim().to_string(),
            key,
            prompt: enhance::DEFAULT_PROMPT.to_string(),
            reasoning: None,
            ladder_stage: AtomicU8::new(0),
            agent: ureq::Agent::config_builder()
                .timeout_connect(Some(CONNECT))
                .timeout_global(Some(WHOLE_REQUEST))
                // Follow no redirects: ureq drops Authorization on a redirect
                // but keeps x-api-key, which would carry the key to wherever
                // Location pointed.
                .max_redirects(0)
                // A 4xx/5xx is read like any other reply, not turned into
                // `Err(ureq::Error::StatusCode(n))`: that variant's `Display`
                // is just "http status: n" and ureq drops the body building
                // it, which is exactly where Anthropic's error envelope
                // (`{"type": "error", "error": {"type", "message"}}`) lives
                // -- see `complete_with` and `provider_error::provider_detail`.
                // Matches [`crate::endpoint::Endpoint::new`]'s same setting,
                // for the same reason.
                .http_status_as_error(false)
                .build()
                .into(),
            times: Mutex::default(),
        }
    }

    /// Use the person's own instructions instead of the default.
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Choose how much thinking the model does, instead of following the
    /// prompt. For comparing the two.
    pub fn with_reasoning(mut self, reasoning: Reasoning) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Points at a fake server instead of the real API. Production code
    /// always talks to Anthropic's one fixed endpoint.
    #[cfg(test)]
    fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    fn reasoning(&self) -> Reasoning {
        self.reasoning
            .unwrap_or_else(|| Reasoning::for_prompt(&self.prompt))
    }

    /// [`Anthropic::complete`], with the reasoning setting passed in rather
    /// than taken from [`Self::reasoning`], so a caller can ask for a
    /// setting the Enhancer prompt would not have chosen -- see
    /// [`TextModel::complete`]'s use of [`Reasoning::Least`] always.
    ///
    /// With [`Reasoning::Least`], starts at the ladder stage already reached
    /// for this value (see [`Self::ladder_stage`]) and, on a 400 below stage
    /// 2, retries at the next stage down -- fewer optional fields --
    /// remembering the new stage so later requests for this value start
    /// there instead of failing again. Every other status, and a 400 at
    /// stage 2, is returned as-is. [`Reasoning::ServerDefault`] never sets
    /// `thinking` or `temperature` and never retries.
    fn complete_with(
        &self,
        instructions: &str,
        text: &str,
        reasoning: Reasoning,
    ) -> Result<String> {
        let mut body = json!({
            "model": self.model,
            "max_tokens": max_tokens(text),
            "system": instructions,
            "messages": [{"role": "user", "content": text}],
        });
        let mut response = if reasoning == Reasoning::Least {
            let mut stage = self.ladder_stage.load(Ordering::Relaxed);
            loop {
                apply_ladder_stage(&mut body, stage);
                let mut resp = self.send(&body)?;
                // Any 400 moves down the ladder, not only one caused by the
                // field just dropped: Anthropic's error envelope carries no
                // `param` to say which field was refused (see
                // `ladder_stage`'s doc), so a 400 for an unrelated reason (a
                // mistyped model id) is still indistinguishable here and
                // still costs up to two extra requests once, the same as a
                // genuine refusal.
                if stage < 2 && resp.status().as_u16() == 400 {
                    let status = resp.status().as_u16();
                    let shown = redacted(&self.url);
                    let detail = provider_error::provider_detail(&mut resp, parse_anthropic_detail);
                    stage += 1;
                    self.ladder_stage.fetch_max(stage, Ordering::Relaxed);
                    tracing::info!(
                        stage,
                        status,
                        url = %shown,
                        model = %self.model,
                        code = detail.code.as_deref(),
                        "a 400 moved the thinking/temperature ladder down a stage; asking again"
                    );
                    provider_error::debug_provider_message(&shown, &self.model, status, &detail);
                    continue;
                }
                break resp;
            }
        } else {
            self.send(&body)?
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let shown = redacted(&self.url);
            let detail = provider_error::provider_detail(&mut response, parse_anthropic_detail);
            provider_error::debug_provider_message(&shown, &self.model, status, &detail);
            return Err(self.error(provider_error::status_message(
                &shown,
                &self.model,
                status,
                &detail,
            )));
        }
        // `.reader()` decodes gzip/brotli before read_capped ever sees a
        // byte, so the cap below is on the decoded body -- see
        // reply_limit's doc.
        let reader = response.body_mut().with_config().reader();
        let raw = macalla_core::bounded_read::read_capped(reader, reply_limit(text))
            .map_err(|e| self.error(format!("reading the reply from {}: {e}", self.url)))?;
        reply_text(&raw)
            .ok_or_else(|| self.error(format!("the reply from {} had no text in it", self.url)))
    }

    /// [`Self::complete_with`], with the reasoning setting [`Self::reasoning`]
    /// picks: [`Self::reasoning`] if [`Anthropic::with_reasoning`] set one,
    /// [`Reasoning::for_prompt`] on the Enhancer prompt otherwise. Used by
    /// [`Enhancer::enhance`].
    pub(crate) fn complete(&self, instructions: &str, text: &str) -> Result<String> {
        self.complete_with(instructions, text, self.reasoning())
    }

    fn request(
        &self,
        body: &Value,
    ) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        self.agent
            .post(&self.url)
            .header("x-api-key", &self.key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .send(body.to_string())
    }

    /// [`Self::request`], with a transport failure (connect, TLS, timeout --
    /// the agent no longer turns a 4xx/5xx into one of these, see
    /// [`Anthropic::new`]) turned into the same [`Error::Enhance`] shape
    /// every other failure in this module returns. Never includes the key:
    /// [`ureq::Error`]'s `Display` never echoes a request header.
    fn send(&self, body: &Value) -> Result<ureq::http::Response<ureq::Body>> {
        self.request(body).map_err(|e| {
            self.error(format!(
                "the request to {} failed: {e}",
                redacted(&self.url)
            ))
        })
    }

    fn error(&self, message: String) -> Error {
        Error::Enhance {
            backend: BACKEND_ID,
            message,
        }
    }
}

impl Enhancer for Anthropic {
    fn caps(&self) -> EnhancerCaps {
        EnhancerCaps {
            id: BACKEND_ID,
            typical_ms: self.times.lock().ok().and_then(|t| t.median_ms()),
            constrained_output: false,
        }
    }

    /// Whether one is set up. Whether it answers is found by the request
    /// itself, which gives up within [`CONNECT`] when nothing answers.
    fn is_available(&self) -> bool {
        !self.model.is_empty() && !self.key.is_empty()
    }

    fn enhance(&self, blocks: &[Block], terms: &[String]) -> Result<Vec<Block>> {
        let instructions = enhance::instructions(&self.prompt, terms);
        let started = Instant::now();
        let guard = enhance::Guard::for_prompt(&self.prompt);
        let out = enhance::rewrite_each(blocks, guard, |text| self.complete(&instructions, text))?;
        if let Ok(mut times) = self.times.lock() {
            times.record(started.elapsed());
        }
        Ok(out)
    }
}

impl TextModel for Anthropic {
    /// Same check as [`Enhancer::is_available`].
    fn ready(&self) -> bool {
        !self.model.is_empty() && !self.key.is_empty()
    }

    /// Always asks for the least reasoning, whatever [`Anthropic::with_prompt`]
    /// or [`Anthropic::with_reasoning`] set for `enhance()` -- see
    /// [`macalla_core::text_model::TextModel`]'s doc.
    fn complete(&self, instructions: &str, text: &str) -> Result<String> {
        self.complete_with(instructions, text, Reasoning::Least)
    }
}

/// Sets `thinking` and `temperature` on `body` for one rung of the ladder
/// [`Anthropic::ladder_stage`] describes: 0 sends both, 1 sends `thinking`
/// only, 2 sends neither. Any fields from a previous stage are cleared
/// first, so the same `body` can be reused across retries.
fn apply_ladder_stage(body: &mut Value, stage: u8) {
    let Some(fields) = body.as_object_mut() else {
        return;
    };
    fields.remove("thinking");
    fields.remove("temperature");
    if stage == 0 {
        fields.insert("temperature".to_string(), json!(0));
    }
    if stage <= 1 {
        fields.insert("thinking".to_string(), json!({"type": "disabled"}));
    }
}

/// `max_tokens` is required on every request. Tier 1 only punctuates or
/// lightly rewrites the text it is handed, so the reply is
/// never much longer than the input: this estimates roughly one token per
/// four characters of English text -- a common rule of thumb, not an
/// Anthropic-documented ratio -- doubles it for headroom against added
/// punctuation or a slightly longer rewrite, and floors it so a short block
/// still gets enough room for its own stop token.
fn max_tokens(text: &str) -> u32 {
    let estimate = (text.chars().count() as u32 / 4).max(1);
    (estimate * 2).max(64)
}

/// Bytes of English text per token used to translate `max_tokens` into a
/// byte cap on the reply: the same "roughly one token per four characters"
/// rule of thumb [`max_tokens`] already uses, doubled again here for
/// multi-byte UTF-8 and the surrounding JSON. Derived, not measured.
const REPLY_BYTES_PER_TOKEN: u64 = 8;

/// A fixed allowance for everything in the reply besides the text itself --
/// the `content` block wrapper, `id`, `model`, `usage`, `stop_reason` and
/// the like. Generous, since none of those fields are large. Derived, not
/// measured.
const REPLY_ENVELOPE: u64 = 4096;

/// The reply cannot legitimately hold more text than `max_tokens` tokens'
/// worth -- Anthropic enforces that on its side -- so the cap on the wire is
/// that many tokens, translated to bytes by [`REPLY_BYTES_PER_TOKEN`], plus
/// [`REPLY_ENVELOPE`] for everything else in the reply. Checked on the
/// *decoded* body, by [`macalla_core::bounded_read::read_capped`] --
/// `ureq`'s own `.limit()` sits beneath its gzip/brotli decoder and would
/// only bound the compressed wire bytes, not what the `String` this becomes
/// actually holds.
fn reply_limit(text: &str) -> u64 {
    u64::from(max_tokens(text)) * REPLY_BYTES_PER_TOKEN + REPLY_ENVELOPE
}

/// The first `text` content block in the reply, skipping any `thinking` or
/// `redacted_thinking` block that precedes it.
fn reply_text(raw: &str) -> Option<String> {
    let reply: Value = serde_json::from_str(raw).ok()?;
    let blocks = reply.get("content")?.as_array()?;
    blocks.iter().find_map(|block| {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            block
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_string)
        } else {
            None
        }
    })
}

/// One model from Anthropic's listing. Plain data -- no policy: which of
/// these are worth offering for cleanup is `macalla-session`'s job. `capabilities` is not carried here: it comes back as a
/// deeply nested per-feature object (per the provider's documentation),
/// and nothing downstream needs it yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub display_name: String,
    pub max_input_tokens: Option<u64>,
}

/// [`list_models`]'s result: the models, and how many entries were skipped
/// for carrying no usable `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub models: Vec<Model>,
    pub skipped: usize,
}

/// Fetch and parse every page of Anthropic's model listing. `key` is sent as
/// `x-api-key`, the same header [`Anthropic::request`] sends.
pub fn list_models(key: &str) -> Result<Listing> {
    list_models_at(LISTING_URL, key, &list_agent())
}

fn list_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(LIST_CONNECT))
        .timeout_global(Some(LIST_REQUEST))
        .max_redirects(0)
        .build()
        .into()
}

fn list_error(message: String) -> Error {
    Error::Enhance {
        backend: BACKEND_ID,
        message,
    }
}

/// [`list_models`], with the base URL and agent as parameters so a test can
/// point it at a 127.0.0.1 fake server with short timeouts instead of the
/// real API. Pages with `after_id` set to the previous page's `last_id`
/// while `has_more` is true, bounded to [`MAX_PAGES`]: a page loop that goes
/// past that bound stops and returns what it has rather than looping
/// forever against a server that never stops claiming more.
fn list_models_at(url: &str, key: &str, agent: &ureq::Agent) -> Result<Listing> {
    let mut models = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut skipped = 0usize;
    let mut after_id: Option<String> = None;
    let mut seen_cursors = HashSet::new();

    for _ in 0..MAX_PAGES {
        // A cursor already used means the server is repeating itself --
        // trusting it further would loop forever against one that always
        // claims more, so this page is not fetched at all.
        if let Some(id) = &after_id {
            if !seen_cursors.insert(id.clone()) {
                break;
            }
        }

        let page_url = page_url(url, after_id.as_deref());
        let raw = list_get(agent, &page_url, key)
            .map_err(|e| list_error(format!("the request to {url} failed: {e}")))?;
        let page = parse_page(&raw)?;
        skipped += page.skipped;
        for model in page.models {
            // A repeated id -- from a repeated page, or a server that names
            // one model twice -- keeps only the first copy.
            if seen_ids.insert(model.id.clone()) {
                models.push(model);
            }
        }
        if models.len() > MAX_MODELS {
            models.truncate(MAX_MODELS);
        }
        if models.len() >= MAX_MODELS {
            break;
        }

        match (page.has_more, page.last_id) {
            (true, Some(id)) => after_id = Some(id),
            _ => break,
        }
    }

    Ok(Listing { models, skipped })
}

/// One page's parse result: its models, how many entries were skipped, and
/// the cursor fields [`list_models_at`] uses to decide whether to keep
/// paging.
#[derive(Debug)]
struct Page {
    models: Vec<Model>,
    skipped: usize,
    has_more: bool,
    last_id: Option<String>,
}

/// Parse one page's reply, from the network or a saved snapshot alike. A
/// reply with no `data` array is a clear error, since that is the documented
/// envelope; an entry with no usable `id` is skipped and counted, the same
/// rule [`crate::openai::parse_listing`] uses.
fn parse_page(raw: &str) -> Result<Page> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| list_error("the reply was not valid JSON".into()))?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| list_error("the reply had no model list in it".into()))?;

    let mut models = Vec::new();
    let mut skipped = 0usize;
    for entry in entries {
        match serde_json::from_value::<WireModel>(entry.clone()) {
            Ok(wire) => models.push(Model::from(wire)),
            Err(_) => skipped += 1,
        }
    }

    let has_more = value
        .get("has_more")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let last_id = value
        .get("last_id")
        .and_then(Value::as_str)
        .map(str::to_string);

    Ok(Page {
        models,
        skipped,
        has_more,
        last_id,
    })
}

fn page_url(base: &str, after_id: Option<&str>) -> String {
    match after_id {
        Some(id) => format!("{base}?limit={PAGE_LIMIT}&after_id={}", percent_encode(id)),
        None => format!("{base}?limit={PAGE_LIMIT}"),
    }
}

/// A minimal percent-encoder for a query-parameter value: Anthropic's page
/// cursor is an opaque id, not documented as URL-safe, so anything outside
/// the unreserved set (RFC 3986) is escaped rather than trusted.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// A bounded `GET`, with the same two headers [`Anthropic::request`] sends.
fn list_get(agent: &ureq::Agent, url: &str, key: &str) -> std::result::Result<String, String> {
    let mut response = agent
        .get(url)
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .call()
        .map_err(|e| e.to_string())?;
    // `.reader()` decodes gzip/brotli before read_capped ever sees a byte,
    // so the cap is on the decoded body -- see LIST_BODY_LIMIT's doc.
    let reader = response.body_mut().with_config().reader();
    macalla_core::bounded_read::read_capped(reader, LIST_BODY_LIMIT).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    max_input_tokens: Option<u64>,
}

impl From<WireModel> for Model {
    fn from(w: WireModel) -> Self {
        Model {
            id: w.id,
            display_name: w.display_name,
            max_input_tokens: w.max_input_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{item, serve, serve_once, Capture};

    fn reply(text: &str) -> String {
        json!({"content": [{"type": "text", "text": text}]}).to_string()
    }

    fn thinking_then_reply(thinking: &str, text: &str) -> String {
        json!({"content": [
            {"type": "thinking", "thinking": thinking, "signature": "sig"},
            {"type": "text", "text": text},
        ]})
        .to_string()
    }

    #[test]
    fn a_block_goes_out_with_the_headers_prompt_terms_and_model_and_comes_back_punctuated() {
        let (address, server) =
            serve_once("200 OK", reply("We need to ship the parser by Friday."));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        assert_eq!(anthropic.caps().typical_ms, None);

        let out = anthropic
            .enhance(
                &item("we need to ship the parser by friday"),
                &["Macalla".into()],
            )
            .unwrap();

        assert_eq!(out, item("We need to ship the parser by Friday."));
        let (head, sent) = server.join().unwrap();
        assert!(head.starts_with("post /v1/messages "), "{head}");
        assert!(head.contains("x-api-key: test-key"), "{head}");
        assert!(
            head.contains(&format!("anthropic-version: {ANTHROPIC_VERSION}")),
            "{head}"
        );
        assert!(head.contains("content-type: application/json"), "{head}");
        assert_eq!(sent["model"], "claude-haiku-4-5");
        assert!(sent["max_tokens"].as_u64().unwrap() > 0);
        assert_eq!(sent["messages"][0]["role"], "user");
        assert_eq!(
            sent["messages"][0]["content"],
            "we need to ship the parser by friday"
        );
        let system = sent["system"].as_str().unwrap();
        assert!(system.starts_with(enhance::DEFAULT_PROMPT));
        assert!(system.contains("Macalla"));
        assert!(anthropic.caps().typical_ms.is_some());
    }

    #[test]
    fn with_the_default_prompt_thinking_is_sent_disabled_with_temperature_zero() {
        let (address, server) = serve_once("200 OK", reply("Review the spec."));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        anthropic.enhance(&item("review the spec"), &[]).unwrap();
        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["thinking"], json!({"type": "disabled"}));
        assert_eq!(sent["temperature"], 0);
    }

    #[test]
    fn a_prompt_the_person_wrote_sends_no_thinking_field_at_all() {
        let (address, server) = serve_once("200 OK", reply("Review the spec."));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"))
            .with_prompt("Fix product names.");
        anthropic.enhance(&item("review the spec"), &[]).unwrap();
        let (_, sent) = server.join().unwrap();
        assert!(sent.get("thinking").is_none(), "{sent}");
        assert!(sent.get("temperature").is_none(), "{sent}");
    }

    #[test]
    fn reasoning_can_be_chosen_whatever_the_prompt() {
        let (address, server) = serve_once("200 OK", reply("Review the spec."));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"))
            .with_reasoning(Reasoning::ServerDefault);
        anthropic.enhance(&item("review the spec"), &[]).unwrap();
        let (_, sent) = server.join().unwrap();
        assert!(sent.get("thinking").is_none(), "{sent}");
    }

    #[test]
    fn a_thinking_block_ahead_of_the_text_block_is_not_part_of_the_reply() {
        let (address, server) = serve_once(
            "200 OK",
            thinking_then_reply(
                "The user wants punctuation.",
                "We need to ship the parser by Friday.",
            ),
        );
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        assert_eq!(
            anthropic
                .enhance(&item("we need to ship the parser by friday"), &[])
                .unwrap(),
            item("We need to ship the parser by Friday.")
        );
        server.join().unwrap();
    }

    #[test]
    fn a_reply_that_answers_instead_of_punctuating_keeps_the_tier_0_text() {
        let (address, server) = serve_once(
            "200 OK",
            reply("The latency budget is three hundred milliseconds."),
        );
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        let asked = item("what is the latency budget for the parser");
        assert_eq!(anthropic.enhance(&asked, &[]).unwrap(), asked);
        server.join().unwrap();
    }

    #[test]
    fn with_the_default_prompt_a_swapped_word_keeps_the_tier_0_text() {
        let (address, server) =
            serve_once("200 OK", reply("The PostgreSQL database uses pgvector."));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        let asked = item("the postgres database uses pgvector");
        assert_eq!(anthropic.enhance(&asked, &[]).unwrap(), asked);
        server.join().unwrap();
    }

    fn bad_request(message: &str) -> String {
        json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message},
        })
        .to_string()
    }

    #[test]
    fn a_model_that_refuses_the_temperature_is_asked_again_and_not_asked_twice() {
        let (address, server) = serve(vec![
            (
                "400 Bad Request",
                bad_request("`temperature` does not support 0 with this model."),
            ),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        let anthropic = Anthropic::new("claude-sonnet-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        assert_eq!(
            anthropic.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );
        assert_eq!(
            anthropic.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );
        let sent = server.join().unwrap();
        assert_eq!(sent[0].1["thinking"], json!({"type": "disabled"}));
        assert_eq!(sent[0].1["temperature"], 0);
        assert_eq!(sent[1].1["thinking"], json!({"type": "disabled"}));
        assert!(sent[1].1.get("temperature").is_none(), "{}", sent[1].1);
        assert_eq!(sent[2].1["thinking"], json!({"type": "disabled"}));
        assert!(sent[2].1.get("temperature").is_none(), "{}", sent[2].1);
    }

    #[test]
    fn a_model_that_refuses_both_settles_at_neither_and_is_not_asked_twice() {
        let (address, server) = serve(vec![
            (
                "400 Bad Request",
                bad_request("`temperature` does not support 0 with this model."),
            ),
            (
                "400 Bad Request",
                bad_request("\"thinking.type.disabled\" is not supported for this model."),
            ),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        let anthropic = Anthropic::new("claude-mythos-5-1", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        assert_eq!(
            anthropic.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );
        assert_eq!(
            anthropic.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );
        let sent = server.join().unwrap();
        assert_eq!(sent[0].1["thinking"], json!({"type": "disabled"}));
        assert_eq!(sent[0].1["temperature"], 0);
        assert_eq!(sent[1].1["thinking"], json!({"type": "disabled"}));
        assert!(sent[1].1.get("temperature").is_none(), "{}", sent[1].1);
        assert!(sent[2].1.get("thinking").is_none(), "{}", sent[2].1);
        assert!(sent[2].1.get("temperature").is_none(), "{}", sent[2].1);
        assert!(sent[3].1.get("thinking").is_none(), "{}", sent[3].1);
        assert!(sent[3].1.get("temperature").is_none(), "{}", sent[3].1);
    }

    #[test]
    fn a_401_is_returned_as_an_error_with_no_retry() {
        let (address, server) = serve_once("401 Unauthorized", bad_request("invalid x-api-key"));
        let anthropic = Anthropic::new("claude-sonnet-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        let error = anthropic
            .enhance(&item("review the spec"), &[])
            .unwrap_err();
        assert!(!error.to_string().contains("test-key"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_server_error_fails_without_the_key_in_the_message() {
        let (address, server) = serve_once("500 Internal Server Error", "{}".into());
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        let error = anthropic
            .enhance(&item("review the spec"), &[])
            .unwrap_err();
        assert!(error.to_string().contains(&address), "{error}");
        assert!(!error.to_string().contains("test-key"), "{error}");
        server.join().unwrap();
    }

    // -------------------------------------------- provider error detail

    #[test]
    fn a_refused_request_reports_status_model_and_code_but_not_the_providers_own_message() {
        // 429, not 400: the thinking/temperature ladder only retries a 400,
        // so this exercises the terminal-error path directly rather than
        // through a retry.
        let marker = "do-not-leak-this-dictated-text";
        // A provider can quote the input back in its message.
        let (address, server) = serve_once(
            "429 Too Many Requests",
            bad_request(&format!(
                "Number of request tokens has exceeded your per-minute rate limit: '{marker}'"
            )),
        );
        let anthropic = Anthropic::new("claude-sonnet-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        let error = anthropic
            .enhance(&item(marker), &["do-not-leak-this-term".into()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("429"), "{error}");
        assert!(error.contains("claude-sonnet-5"), "{error}");
        assert!(error.contains("invalid_request_error"), "{error}");
        // The provider's own message is debug-only: it can quote input.
        assert!(!error.contains("per-minute rate limit"), "{error}");
        assert!(!error.contains(marker), "{error}");
        assert!(!error.contains("do-not-leak-this-term"), "{error}");
        assert!(!error.contains(enhance::DEFAULT_PROMPT), "{error}");
        assert!(!error.contains("test-key"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_non_json_error_body_still_reports_status_and_model_without_panicking() {
        let (address, server) = serve_once("400 Bad Request", "not json at all".into());
        let anthropic = Anthropic::new("claude-mythos-5-1", "test-key".into())
            .with_url(format!("{address}/v1/messages"))
            .with_reasoning(Reasoning::ServerDefault);
        let error = anthropic
            .enhance(&item("review the spec"), &[])
            .unwrap_err();
        assert!(error.to_string().contains("400"), "{error}");
        assert!(error.to_string().contains("claude-mythos-5-1"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn the_retry_log_line_carries_the_model_and_the_providers_detail_at_debug_only() {
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let capture = Capture {
            events: events.clone(),
        };
        let marker = "do-not-leak-this-dictated-text";
        // A provider can quote the input back in its message.
        let body = bad_request(&format!(
            "`temperature` does not support 0 with this model, in '{marker}'"
        ));
        let (address, server) = serve(vec![
            ("400 Bad Request", body),
            ("200 OK", reply("Review the spec.")),
        ]);
        let anthropic = Anthropic::new("claude-sonnet-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));

        crate::test_support::with_capture(capture, || {
            anthropic
                .enhance(&item(marker), &["do-not-leak-this-term".into()])
                .unwrap();
        });
        server.join().unwrap();

        let events = events.lock().unwrap();
        let retry = events
            .iter()
            .find(|e| e.message.contains("asking again"))
            .expect("the retry log line was emitted");
        assert_eq!(
            retry.fields.get("model").map(String::as_str),
            Some("claude-sonnet-5")
        );
        assert!(
            retry
                .fields
                .get("code")
                .is_some_and(|v| v.contains("invalid_request_error")),
            "{:?}",
            retry.fields
        );
        assert_eq!(retry.level, tracing::Level::INFO);
        assert!(
            !retry.fields.contains_key("provider_message"),
            "the provider's message is debug-only: {:?}",
            retry.fields
        );
        let debug = events
            .iter()
            .find(|e| e.fields.contains_key("provider_message"))
            .expect("the provider's message was logged at debug");
        assert_eq!(debug.level, tracing::Level::DEBUG);
        assert!(debug.fields["provider_message"].contains("does not support 0"));
        // The quoted input reaches the debug message and nothing else.
        assert!(debug.fields["provider_message"].contains(marker));
        assert_eq!(
            debug.fields.get("model").map(String::as_str),
            Some("claude-sonnet-5")
        );
        // Nothing sent is logged anywhere else, and neither is the key.
        for event in events.iter() {
            let all = format!("{} {:?}", event.message, event.fields);
            let outside_provider_message = format!(
                "{} {:?}",
                event.message,
                event
                    .fields
                    .iter()
                    .filter(|(name, _)| name.as_str() != "provider_message")
                    .collect::<Vec<_>>()
            );
            assert!(
                !outside_provider_message.contains(marker),
                "{outside_provider_message}"
            );
            assert!(!all.contains("do-not-leak-this-term"), "{all}");
            assert!(!all.contains(enhance::DEFAULT_PROMPT), "{all}");
            assert!(!all.contains("test-key"), "{all}");
        }
    }

    #[test]
    fn a_reply_with_no_text_in_it_fails() {
        let (address, server) = serve_once("200 OK", json!({"content": []}).to_string());
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        let error = anthropic
            .enhance(&item("review the spec"), &[])
            .unwrap_err();
        assert!(error.to_string().contains("no text"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn an_oversize_reply_is_rejected_not_read_whole() {
        let text = "review the spec";
        let huge = "a".repeat(reply_limit(text) as usize + 1);
        let (address, server) = serve_once("200 OK", reply(&huge));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));
        let error = anthropic.enhance(&item(text), &[]).unwrap_err();
        assert!(error.to_string().contains("more than"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn with_no_model_or_key_it_is_not_available() {
        assert!(!Anthropic::new(" ", "test-key".into()).is_available());
        assert!(!Anthropic::new("claude-haiku-4-5", String::new()).is_available());
        assert!(Anthropic::new("claude-haiku-4-5", "test-key".into()).is_available());
    }

    // ---------------------------------------------------------- TextModel

    #[test]
    fn text_model_complete_sends_the_instructions_and_text_and_returns_the_reply() {
        let (address, server) = serve_once("200 OK", reply("Three words: parser shipped Friday."));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));

        let out = TextModel::complete(
            &anthropic,
            "Summarise in three words.",
            "we shipped the parser on friday",
        )
        .unwrap();

        assert_eq!(out, "Three words: parser shipped Friday.");
        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["system"], "Summarise in three words.");
        assert_eq!(
            sent["messages"][0]["content"],
            "we shipped the parser on friday"
        );
    }

    #[test]
    fn text_model_complete_returns_an_error_reply_as_an_error() {
        let (address, server) = serve_once("500 Internal Server Error", "{}".into());
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"));

        let error = TextModel::complete(&anthropic, "Summarise in three words.", "we shipped it")
            .unwrap_err();

        assert!(error.to_string().contains(&address), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn text_model_complete_asks_for_the_least_reasoning_whatever_the_enhancer_prompt_says() {
        // A prompt the person wrote: Guard::for_prompt gives Guard::Ratio, so
        // Reasoning::for_prompt gives ServerDefault -- more than the least --
        // for enhance(). TextModel::complete must ignore that and still ask
        // for the least reasoning.
        let (address, server) = serve_once("200 OK", reply("Three words: parser shipped Friday."));
        let anthropic = Anthropic::new("claude-haiku-4-5", "test-key".into())
            .with_url(format!("{address}/v1/messages"))
            .with_prompt("Fix product names.");
        assert_eq!(
            Reasoning::for_prompt("Fix product names."),
            Reasoning::ServerDefault,
            "the prompt used by this test must not itself ask for the least reasoning"
        );

        TextModel::complete(&anthropic, "Summarise in three words.", "we shipped it").unwrap();

        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["thinking"], json!({"type": "disabled"}));
        assert_eq!(sent["temperature"], 0);
    }

    // ------------------------------------------------------------ list_models

    fn listing_page(ids: &[&str], last_id: Option<&str>, has_more: bool) -> String {
        json!({
            "data": ids.iter().map(|id| json!({
                "type": "model",
                "id": id,
                "display_name": format!("Claude {id}"),
                "created_at": "2026-07-24T00:00:00Z",
                "max_input_tokens": 200_000,
                "max_tokens": 64_000,
                "capabilities": {"thinking": {"supported": true}},
            })).collect::<Vec<_>>(),
            "first_id": ids.first(),
            "has_more": has_more,
            "last_id": last_id,
        })
        .to_string()
    }

    #[test]
    fn a_page_is_parsed_into_plain_models_and_an_entry_with_no_id_is_skipped_and_counted() {
        let raw = json!({
            "data": [
                {"type": "model", "id": "claude-opus-5", "display_name": "Claude Opus 5", "max_input_tokens": 200_000},
                {"type": "model", "display_name": "no id here"},
            ],
            "has_more": false,
        })
        .to_string();
        let page = parse_page(&raw).unwrap();
        assert_eq!(page.skipped, 1);
        assert_eq!(page.models.len(), 1);
        assert_eq!(page.models[0].id, "claude-opus-5");
        assert_eq!(page.models[0].display_name, "Claude Opus 5");
        assert_eq!(page.models[0].max_input_tokens, Some(200_000));
        assert!(!page.has_more);
    }

    #[test]
    fn a_reply_with_no_data_array_is_an_error() {
        let error = parse_page(&json!({"has_more": false}).to_string()).unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error}");
    }

    #[test]
    fn version_and_key_headers_are_sent_and_two_pages_are_followed() {
        let (address, server) = serve(vec![
            (
                "200 OK",
                listing_page(&["claude-opus-5"], Some("claude-opus-5"), true),
            ),
            (
                "200 OK",
                listing_page(&["claude-haiku-4-5"], Some("claude-haiku-4-5"), false),
            ),
        ]);
        let listing = list_models_at(&address, "test-key", &list_agent()).unwrap();
        assert_eq!(listing.skipped, 0);
        assert_eq!(listing.models.len(), 2);
        assert_eq!(listing.models[0].id, "claude-opus-5");
        assert_eq!(listing.models[0].display_name, "Claude claude-opus-5");
        assert_eq!(listing.models[0].max_input_tokens, Some(200_000));
        assert_eq!(listing.models[1].id, "claude-haiku-4-5");

        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].0.contains("x-api-key: test-key"), "{}", sent[0].0);
        assert!(
            sent[0]
                .0
                .contains(&format!("anthropic-version: {ANTHROPIC_VERSION}")),
            "{}",
            sent[0].0
        );
        assert!(sent[0].0.starts_with("get /?limit=1000 "), "{}", sent[0].0);
        assert!(
            sent[1].0.contains("after_id=claude-opus-5"),
            "{}",
            sent[1].0
        );
    }

    #[test]
    fn a_401_is_an_error_whose_message_does_not_contain_the_key() {
        let (address, server) = serve_once("401 Unauthorized", bad_request("invalid x-api-key"));
        let error = list_models_at(&address, "test-key", &list_agent()).unwrap_err();
        assert!(!error.to_string().contains("test-key"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_repeating_cursor_stops_paging_after_two_requests_with_one_copy_of_each_model() {
        let (address, server) = serve(vec![
            (
                "200 OK",
                listing_page(&["claude-opus-5"], Some("same-cursor"), true),
            ),
            (
                "200 OK",
                listing_page(&["claude-opus-5"], Some("same-cursor"), true),
            ),
        ]);
        let listing = list_models_at(&address, "test-key", &list_agent()).unwrap();
        assert_eq!(listing.models.len(), 1);
        assert_eq!(listing.models[0].id, "claude-opus-5");
        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "a repeated cursor must not be followed a third time"
        );
    }

    #[test]
    fn the_total_is_capped_at_max_models_even_across_pages() {
        fn page(prefix: &str, last_id: &str) -> String {
            let ids: Vec<String> = (0..150).map(|i| format!("{prefix}-{i}")).collect();
            json!({
                "data": ids.iter().map(|id| json!({
                    "type": "model", "id": id, "display_name": format!("Claude {id}"),
                })).collect::<Vec<_>>(),
                "has_more": true,
                "last_id": last_id,
            })
            .to_string()
        }
        let (address, server) = serve(vec![
            ("200 OK", page("claude-a", "claude-a-149")),
            ("200 OK", page("claude-b", "claude-b-149")),
        ]);
        let listing = list_models_at(&address, "test-key", &list_agent()).unwrap();
        assert_eq!(listing.models.len(), crate::endpoint::MAX_MODELS);
        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "paging must stop once the cap is reached, without a third request"
        );
    }
}

#[cfg(test)]
mod redirects {
    #[test]
    fn the_agent_follows_no_redirects_so_the_key_header_cannot_travel() {
        let anthropic = super::Anthropic::new("claude-haiku-4-5", "test-key".into());
        assert_eq!(anthropic.agent.config().max_redirects(), 0);
    }
}
