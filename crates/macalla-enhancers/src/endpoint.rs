// SPDX-License-Identifier: MIT
//! An OpenAI-compatible chat-completions endpoint: Ollama, llama-server,
//! LM Studio or vLLM on a machine the person runs, or a cloud provider that
//! answers the same request, with their key.
//!
//! Each block goes out as its own request and comes back as plain text. No
//! grammar or JSON schema is honoured by every server, and Tier 1 returns text
//! only, so the core's guard ([`macalla_core::enhance::rewrite_each`]) is what
//! stops a reply that answers, summarises or wraps the text.
//!
//! Reasoning models are asked not to reason for the default prompt, which only
//! punctuates: on 2026-09-11, granite4.2:8b on Ollama took 105 to 112 s per
//! sentence with its default reasoning and 1.4 to 1.6 s without it.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use macalla_core::block::Block;
use macalla_core::enhance::{self, Enhancer, EnhancerCaps, RecentTimes};
use macalla_core::text_model::TextModel;
use macalla_core::{Error, Result};
use serde_json::{json, Value};

use crate::provider_error::{self, parse_openai_detail};

pub const BACKEND_ID: &str = "endpoint";

/// Long enough for a server on the local network to accept, short enough that
/// one that is switched off gives up on the segment quickly. Not measured.
const CONNECT: Duration = Duration::from_secs(3);

/// The most one request may take. By then the text has long been pasted from
/// Tier 0, and this only decides whether history gets the enhanced text; a
/// small model on a CPU can take several seconds. Not measured.
const WHOLE_REQUEST: Duration = Duration::from_secs(30);

/// Same reasoning as [`CONNECT`], for [`list_models`]'s own requests.
const LIST_CONNECT: Duration = Duration::from_secs(3);

/// A model list or one model's metadata is a small JSON reply with no
/// generation involved, so a healthy server answers in well under a second;
/// 5 s catches a stalled connection without a settings screen waiting
/// anywhere near [`WHOLE_REQUEST`]. Not measured.
const LIST_REQUEST: Duration = Duration::from_secs(5);

/// A reply bigger than this is not a model list, so refusing it early avoids
/// reading an unbounded body into memory from a server that misbehaves.
/// 1 MiB comfortably holds a few thousand model entries. Checked on the
/// *decoded* body, by [`macalla_core::bounded_read::read_capped`] -- `ureq`'s
/// own `.limit()` sits beneath its gzip/brotli decoder and would only bound
/// the compressed wire bytes, not what the `String` this becomes actually
/// holds.
const LIST_BODY_LIMIT: u64 = 1024 * 1024;

/// No `max_tokens` goes out on this request (unlike
/// [`crate::anthropic::Anthropic`], which sends one and can size its own
/// cap from it), so [`reply_limit`] is a heuristic guard against an
/// unbounded reply rather than a tight bound on a well-behaved one:
/// `enhance::rewrite_each` only ever asks Tier 1 to punctuate or lightly
/// rewrite what it is given, but a reasoning model may also emit a
/// `<think>` block ([`reply_text`]) ahead of the real answer, of a length
/// this module has no way to bound in the request. 20x the input's byte
/// length gives generous room for both without ever letting a decoded
/// reply grow unbounded; this floor keeps a short block's cap from being
/// tiny too. Derived, not measured.
const CHAT_REPLY_FLOOR: u64 = 1024 * 1024;

/// See [`CHAT_REPLY_FLOOR`]'s doc for the derivation. Checked on the
/// *decoded* body, by [`macalla_core::bounded_read::read_capped`] --
/// `ureq`'s own `.limit()` sits beneath its gzip/brotli decoder and would
/// only bound the compressed wire bytes, not what the `String` this becomes
/// actually holds.
fn reply_limit(text: &str) -> u64 {
    (text.len() as u64 * 20).max(CHAT_REPLY_FLOOR)
}

/// However many models a server names, a settings screen only ever shows a
/// short list; capping avoids an unbounded number of per-model `/api/show`
/// requests if a server claims to have thousands. Shared with
/// [`crate::openai`] and [`crate::mistral`] for the same reason, against
/// their own unpaginated `/v1/models`.
pub(crate) const MAX_MODELS: usize = 200;

/// How many `/api/show` lookups run at once. One at a time, at up to
/// [`LIST_REQUEST`] each, would take minutes against a server with a
/// realistic model count; 8 keeps the whole capability phase inside
/// [`LIST_TOTAL`] without opening enough sockets at once to look like a scan.
const SHOW_CONCURRENCY: usize = 8;

/// The whole capability phase -- every `/api/show` call together, however
/// many run at once -- gets this long before whatever hasn't answered yet is
/// left with `capabilities: None` rather than delaying the list further.
/// At [`SHOW_CONCURRENCY`]-way concurrency and [`LIST_REQUEST`] per call,
/// 15 s comfortably covers several dozen models on a healthy server, and
/// bounds a "check my server" action to that even against one that hangs.
const LIST_TOTAL: Duration = Duration::from_secs(15);

/// How much a reasoning model may think before it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reasoning {
    /// As little as the server allows. Asked for none; a server that refuses
    /// that is asked for minimal, and one that refuses minimal too is asked
    /// again without the field, and then reasons at its own default, which
    /// can be higher (OpenAI's chat request schema gives `medium`).
    Least,
    /// Nothing is said about reasoning, so the server and model decide.
    ServerDefault,
}

impl Reasoning {
    /// Least for the default prompt, which only punctuates; the server's own
    /// setting for a prompt the person wrote, which may ask for more.
    pub fn for_prompt(prompt: &str) -> Self {
        if matches!(
            enhance::Guard::for_prompt(prompt),
            enhance::Guard::SameWords
        ) {
            Self::Least
        } else {
            Self::ServerDefault
        }
    }
}

/// No `Debug`: it holds the key.
pub struct Endpoint {
    url: String,
    model: String,
    key: Option<String>,
    prompt: String,
    /// Set by [`Endpoint::with_reasoning`]; otherwise it follows the prompt.
    reasoning: Option<Reasoning>,
    /// What this server and model have refused so far, shared with every
    /// other `Endpoint` for the same address and model in this process, so a
    /// rebuilt one does not pay the refused round trips again. See
    /// [`LearnedStore`].
    learned: Arc<Learned>,
    agent: ureq::Agent,
    times: Mutex<RecentTimes>,
}

impl Endpoint {
    /// `address` is what the person typed: a server
    /// (`http://localhost:11434`), an API root with a path (`.../v1`), or the
    /// full chat-completions URL. `key` is sent as a bearer token when set.
    pub fn new(address: &str, model: &str, key: Option<String>) -> Self {
        Self::new_in(&GLOBAL, address, model, key)
    }

    /// [`Endpoint::new`], sharing what it learns through `store` rather than
    /// the process-wide one.
    fn new_in(store: &LearnedStore, address: &str, model: &str, key: Option<String>) -> Self {
        let url = chat_url(address);
        let model = model.trim().to_string();
        Self {
            learned: store.learned_for(&url, &model),
            url,
            model,
            key,
            prompt: enhance::DEFAULT_PROMPT.to_string(),
            reasoning: None,
            agent: ureq::Agent::config_builder()
                .timeout_connect(Some(CONNECT))
                .timeout_global(Some(WHOLE_REQUEST))
                // A 4xx/5xx is read like any other reply, not turned into
                // `Err(ureq::Error::StatusCode(n))`: that variant's `Display`
                // is just "http status: n" and ureq drops the body building
                // it, which is exactly where OpenAI's error envelope
                // (`{"error": {"message", "param", "code"}}`) lives -- see
                // `complete_with` and `provider_detail`.
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

    /// Choose how much a reasoning model thinks, instead of following the
    /// prompt. For comparing the two.
    pub fn with_reasoning(mut self, reasoning: Reasoning) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// Where requests go.
    pub fn url(&self) -> &str {
        &self.url
    }

    fn reasoning(&self) -> Reasoning {
        self.reasoning
            .unwrap_or_else(|| Reasoning::for_prompt(&self.prompt))
    }

    /// [`Self::complete`], with the reasoning setting [`Self::reasoning`]
    /// picks: [`Self::reasoning`] if [`Endpoint::with_reasoning`] set one,
    /// [`Reasoning::for_prompt`] on the Enhancer prompt otherwise. Used by
    /// [`Enhancer::enhance`].
    pub(crate) fn complete(&self, instructions: &str, text: &str) -> Result<String> {
        self.complete_with(instructions, text, self.reasoning())
    }

    /// [`Endpoint::complete`], with the reasoning setting passed in rather
    /// than taken from [`Self::reasoning`], so a caller can ask for a
    /// setting the Enhancer prompt would not have chosen -- see
    /// [`TextModel::complete`]'s use of [`Reasoning::Least`] always.
    fn complete_with(
        &self,
        instructions: &str,
        text: &str,
        reasoning: Reasoning,
    ) -> Result<String> {
        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": instructions},
                {"role": "user", "content": text},
            ],
            "temperature": 0,
            "stream": false,
        });
        // Whether *this* request still carries the reasoning field --
        // `Endpoint` is shared across the worker threads `cleanup.rs` spawns
        // per dictation, so two `complete_with` calls can be in flight at
        // once. The shared floor and `AtomicBool` below are memory for
        // *later* calls only (start from what a model is already known to
        // accept); they must never decide whether *this* response's refusal
        // is this request's own reasoning field or its `temperature`, or one
        // call's retry could read another call's state and blame the wrong
        // parameter -- see the loop's own comment.
        if reasoning == Reasoning::Least {
            if let (Some(fields), Some(effort)) = (
                body.as_object_mut(),
                self.learned.reasoning_floor.get().effort(),
            ) {
                if let Value::Object(extra) = least_reasoning(&self.url, effort) {
                    fields.extend(extra);
                }
            }
        }
        if self.learned.temperature_refused.load(Ordering::Relaxed) {
            if let Some(fields) = body.as_object_mut() {
                fields.remove("temperature");
            }
        }

        let mut response = self.send(&body)?;
        let shown = redacted(&self.url);
        // A refused 400/422 here may be `temperature` or the reasoning
        // field -- decided from the provider's own `param`, never the
        // message text (see `ProviderDetail`'s doc). The temperature step
        // fires only for `param == "temperature"`; the reasoning step only
        // for a `param` [`refuses_reasoning`] accepts -- absent, or one of
        // the reasoning parameter names this module itself sends. Anything
        // else (a context-length error, an unrelated field) takes neither
        // step and fails through `refusal_failure` untouched, rather than
        // spending a reasoning retry on a refusal reasoning had nothing to
        // do with. Each step below also fires only while *this call's own*
        // `body` still carries the value it would change
        // (`body.get("temperature")`, the reasoning effort `lower_reasoning`
        // reads), and changes it the moment it fires -- never gated on the
        // shared state, so two overlapping calls each decide from their own
        // request only. Reasoning steps down none -> minimal -> no field,
        // one step per value, because a model that is sent no field reasons
        // at its default, and OpenAI's chat request schema gives `medium` as
        // the default. That bounds every call to at most 4 sends: the
        // first, one retry for temperature and two for reasoning -- fewer
        // whenever a refusal's `param` rules a step out. The steps taken are
        // remembered for later calls only once a stepped request succeeds:
        // a refusal that no step cured says nothing about what the server
        // accepts.
        let mut dropped_temperature = false;
        let mut lowered_to = None;
        loop {
            let status = response.status().as_u16();
            if response.status().is_success() || !matches!(status, 400 | 422) {
                break;
            }
            let detail = provider_error::provider_detail(&mut response, parse_openai_detail);

            if detail.param.as_deref() == Some("temperature") && body.get("temperature").is_some() {
                tracing::info!(
                    status,
                    url = %shown,
                    model = %self.model,
                    param = detail.param.as_deref(),
                    code = detail.code.as_deref(),
                    "the server refused the request's temperature; asking again without it"
                );
                provider_error::debug_provider_message(&shown, &self.model, status, &detail);
                dropped_temperature = true;
                if let Some(fields) = body.as_object_mut() {
                    fields.remove("temperature");
                }
                response = self.send(&body)?;
                continue;
            }

            if refuses_reasoning(detail.param.as_deref()) {
                if let Some(level) = lower_reasoning(&mut body) {
                    match level {
                        ReasoningLevel::Minimal => tracing::info!(
                            status,
                            url = %shown,
                            model = %self.model,
                            param = detail.param.as_deref(),
                            code = detail.code.as_deref(),
                            "the server refused the request for no reasoning; \
                             asking again with minimal reasoning"
                        ),
                        _ => tracing::info!(
                            status,
                            url = %shown,
                            model = %self.model,
                            param = detail.param.as_deref(),
                            code = detail.code.as_deref(),
                            "the server refused the request for minimal reasoning; \
                             asking again without it"
                        ),
                    }
                    provider_error::debug_provider_message(&shown, &self.model, status, &detail);
                    lowered_to = Some(level);
                    response = self.send(&body)?;
                    continue;
                }
            }

            return Err(self.refusal_failure(&shown, status, &detail));
        }
        if response.status().is_success() {
            if dropped_temperature {
                self.learned
                    .temperature_refused
                    .store(true, Ordering::Relaxed);
            }
            if let Some(level) = lowered_to {
                self.learned.reasoning_floor.record(level);
            }
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let detail = provider_error::provider_detail(&mut response, parse_openai_detail);
            return Err(self.refusal_failure(&shown, status, &detail));
        }
        // `.reader()` decodes gzip/brotli before read_capped ever sees a
        // byte, so the cap below is on the decoded body -- see
        // CHAT_REPLY_FLOOR's doc.
        let reader = response.body_mut().with_config().reader();
        let raw =
            macalla_core::bounded_read::read_capped(reader, reply_limit(text)).map_err(|e| {
                self.error(format!(
                    "reading the reply from {}: {e}",
                    redacted(&self.url)
                ))
            })?;
        reply_text(&raw).ok_or_else(|| {
            self.error(format!(
                "the reply from {} had no text in it",
                redacted(&self.url)
            ))
        })
    }

    fn request(
        &self,
        body: &Value,
    ) -> std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let mut request = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json");
        if let Some(key) = &self.key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        request.send(body.to_string())
    }

    /// [`Self::request`], with a transport failure (connect, TLS, timeout --
    /// the agent no longer turns a 4xx/5xx into one of these, see
    /// [`Endpoint::new`]) turned into the same [`Error::Enhance`] shape
    /// every other failure in this module returns.
    fn send(&self, body: &Value) -> Result<ureq::http::Response<ureq::Body>> {
        self.request(body).map_err(|e| {
            self.error(format!(
                "the request to {} for model {} failed: {e}",
                redacted(&self.url),
                self.model
            ))
        })
    }

    fn error(&self, message: String) -> Error {
        Error::Enhance {
            backend: BACKEND_ID,
            message,
        }
    }

    /// The [`Error`] for a refused request [`Self::complete_with`]'s retry
    /// steps gave up on: logs the provider's own message at debug (see
    /// [`provider_error::debug_provider_message`]) and returns the info-safe
    /// status/param/code text, never the provider's free text.
    fn refusal_failure(
        &self,
        url: &str,
        status: u16,
        detail: &provider_error::ProviderDetail,
    ) -> Error {
        provider_error::debug_provider_message(url, &self.model, status, detail);
        self.error(provider_error::status_message(
            url,
            &self.model,
            status,
            detail,
        ))
    }
}

impl Enhancer for Endpoint {
    fn caps(&self) -> EnhancerCaps {
        EnhancerCaps {
            id: BACKEND_ID,
            typical_ms: self.times.lock().ok().and_then(|t| t.median_ms()),
            constrained_output: false,
        }
    }

    /// Whether one is set up. Whether it answers is found by the request
    /// itself, which gives up within [`CONNECT`] when nothing is listening:
    /// asking first would add a round trip to every segment.
    fn is_available(&self) -> bool {
        !self.model.is_empty()
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

impl TextModel for Endpoint {
    /// Same check as [`Enhancer::is_available`]: whether one is set up, not
    /// whether it answers.
    fn ready(&self) -> bool {
        !self.model.is_empty()
    }

    /// Always asks for the least reasoning, whatever [`Endpoint::with_prompt`]
    /// or [`Endpoint::with_reasoning`] set for `enhance()` -- see
    /// [`macalla_core::text_model::TextModel`]'s doc.
    fn complete(&self, instructions: &str, text: &str) -> Result<String> {
        self.complete_with(instructions, text, Reasoning::Least)
    }
}

/// The OpenAI-compatible API root for what a person typed. A bare server
/// gets `/v1` appended; an address with a path is taken as the API root
/// as-is, since cloud providers put theirs in different places; a full
/// chat-completions URL has that last segment stripped back off.
fn api_root(address: &str) -> String {
    let base = address.trim().trim_end_matches('/');
    if let Some(root) = base.strip_suffix("/chat/completions") {
        return root.to_string();
    }
    let after_scheme = base.split_once("://").map_or(base, |(_, rest)| rest);
    if after_scheme.contains('/') {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// The chat-completions URL for what a person typed.
fn chat_url(address: &str) -> String {
    format!("{}/chat/completions", api_root(address))
}

/// The server root for Ollama's own routes (`/api/tags`, `/api/show`), or
/// `None` when `address`'s own path is not the shape those routes are ever
/// guessed at: a bare host, with or without a port, or a path of exactly
/// `/v1`. Anything else -- OpenRouter's `https://openrouter.ai/api/v1`, or a
/// proxy's `https://host/some-prefix/v1` -- keeps its own path prefix, and
/// Ollama's routes are never guessed there: they live at the bare server
/// root, and guessing would send those requests to an unrelated endpoint
/// that happens to share the last path segment.
fn ollama_root(address: &str) -> Option<String> {
    let base = address.trim().trim_end_matches('/');
    let base = base.strip_suffix("/chat/completions").unwrap_or(base);
    let after_scheme = base.split_once("://").map_or(base, |(_, rest)| rest);
    match after_scheme.split_once('/') {
        None => Some(base.to_string()),
        Some((_, "v1")) => Some(base.strip_suffix("/v1").unwrap_or(base).to_string()),
        Some(_) => None,
    }
}

/// `url` with any userinfo (`user:pass@`) removed, for anywhere a URL goes
/// into an error string or a log field. A typed address may carry
/// `http://user:pass@host:port/...` -- ureq sends that as basic auth, same
/// as a browser would, but leaving it in an error the page shows, or a log
/// that outlives the call, would put the password somewhere it doesn't
/// belong.
///
/// Everything up to the last `@` in the address is taken as userinfo, not
/// only what comes before the first `/`, `?` or `#`: a password typed with a
/// raw `/`, `?` or `#` in it would otherwise end the authority early and be
/// shown whole. The cost is that an address with an `@` in its path or query
/// loses the part before it when shown, which reveals less, never more.
pub(crate) fn redacted(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// One model a server offers, as much detail as could be had without asking
/// the model to run.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerModel {
    pub id: String,
    /// Ollama only.
    pub size_bytes: Option<u64>,
    /// Ollama only.
    pub parameter_size: Option<String>,
    /// Ollama only.
    pub quantization: Option<String>,
    /// `None` when the server never answered `/api/show` (not Ollama, or
    /// this one model's own lookup failed) -- never used to hide a model,
    /// only to mark that its capabilities are unknown.
    pub capabilities: Option<Vec<String>>,
}

/// List the models a server names: the OpenAI-compatible `GET /v1/models`
/// every such server answers, filled in with Ollama's own detail
/// (`/api/tags`, `/api/show`) when the address is plausibly one
/// ([`ollama_root`]) and the server actually answers those routes.
/// `address` is resolved the same way [`Endpoint::new`] resolves the chat
/// completions URL, so a person's address means the same server for a list
/// or a chat. `key` is sent as a bearer token on `/v1/models` only --
/// Ollama's own routes are probed without it, so a proxied Ollama that
/// demands auth for them fails open to names only rather than looking wrong.
///
/// A model whose `/api/show` capabilities do not include `"completion"` (an
/// embedding-only model) is left out. A model whose `/api/show` call fails,
/// or was never reached before [`LIST_TOTAL`] ran out, is kept with
/// `capabilities: None`: a failed or skipped lookup must never hide a model.
/// Not being Ollama at all is not an error -- most OpenAI-compatible servers
/// are not -- so [`list_models`] only fails when `/v1/models` itself does
/// not answer or does not parse.
pub fn list_models(address: &str, key: Option<&str>) -> Result<Vec<ServerModel>> {
    list_models_with(address, key, &list_agent(), SHOW_CONCURRENCY, LIST_TOTAL)
}

/// [`list_models`], with the agent, capability-phase concurrency and total
/// budget as parameters -- so a test can use a tiny budget and a short-timeout
/// agent instead of waiting out [`LIST_REQUEST`] or [`LIST_TOTAL`] for real.
fn list_models_with(
    address: &str,
    key: Option<&str>,
    agent: &ureq::Agent,
    concurrency: usize,
    budget: Duration,
) -> Result<Vec<ServerModel>> {
    let root = api_root(address);
    let names = list_names(agent, &format!("{root}/models"), key)?;

    let ollama = ollama_root(address);
    let tags = ollama
        .as_deref()
        .and_then(|root| list_tags(agent, &format!("{root}/api/tags")));

    let mut models: Vec<ServerModel> = names
        .into_iter()
        .map(|id| {
            let detail = tags.as_ref().and_then(|t| t.get(&id));
            ServerModel {
                size_bytes: detail.and_then(|d| d.size_bytes),
                parameter_size: detail.and_then(|d| d.parameter_size.clone()),
                quantization: detail.and_then(|d| d.quantization.clone()),
                capabilities: None,
                id,
            }
        })
        .collect();

    // Only when the address is plausibly Ollama's and it actually answered
    // /api/tags does a settings screen get asked to wait for capabilities
    // too; a plain OpenAI-compatible server has no such route to ask, and an
    // address shaped like OpenRouter's or a proxy's own path is never
    // guessed at it (see ollama_root).
    if let Some(ollama) = ollama.filter(|_| tags.is_some()) {
        let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
        let caps = show_capabilities_bounded(agent, &ollama, &ids, concurrency, budget);
        for (model, cap) in models.iter_mut().zip(caps) {
            model.capabilities = cap;
        }
        models.retain(|m| {
            m.capabilities
                .as_ref()
                .is_none_or(|caps| caps.iter().any(|c| c == "completion"))
        });
    }

    Ok(models)
}

/// Shared with [`crate::openai`] and [`crate::mistral`], whose own listing
/// requests have the same shape (a small JSON reply, no generation
/// involved) as this module's `/v1/models`.
pub(crate) fn list_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(LIST_CONNECT))
        .timeout_global(Some(LIST_REQUEST))
        .build()
        .into()
}

fn list_error(message: String) -> Error {
    Error::Enhance {
        backend: BACKEND_ID,
        message,
    }
}

/// A bounded `GET`, with the same bearer header the chat request sends. The
/// error is a plain message either way: a connection failure's `Display` or
/// [`macalla_core::bounded_read::read_capped`]'s, since every caller here
/// either formats it into its own message or discards it with `.ok()`.
/// Shared with [`crate::openai`] and [`crate::mistral`], which send the same
/// bearer-token `GET` against their own OpenAI-shaped `/v1/models`.
pub(crate) fn get(
    agent: &ureq::Agent,
    url: &str,
    key: Option<&str>,
) -> std::result::Result<String, String> {
    let mut request = agent.get(url);
    if let Some(key) = key {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let mut response = request.call().map_err(|e| e.to_string())?;
    // `.reader()` decodes gzip/brotli before read_capped ever sees a byte,
    // so the cap is on the decoded body -- see LIST_BODY_LIMIT's doc.
    let reader = response.body_mut().with_config().reader();
    macalla_core::bounded_read::read_capped(reader, LIST_BODY_LIMIT).map_err(|e| e.to_string())
}

/// The names from an OpenAI-compatible `GET /v1/models`, capped at
/// [`MAX_MODELS`]. A malformed reply -- not JSON, no `data` array, or an
/// entry with no `id` -- is a clear error, since this route is the one every
/// such server is expected to answer.
fn list_names(agent: &ureq::Agent, url: &str, key: Option<&str>) -> Result<Vec<String>> {
    let shown = redacted(url);
    let raw = get(agent, url, key)
        .map_err(|e| list_error(format!("the request to {shown} failed: {e}")))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|_| list_error(format!("the reply from {shown} was not valid JSON")))?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| list_error(format!("the reply from {shown} had no model list in it")))?;
    let mut names = Vec::new();
    for entry in data.iter().take(MAX_MODELS) {
        let id = entry.get("id").and_then(Value::as_str).ok_or_else(|| {
            list_error(format!("the reply from {shown} named a model with no id"))
        })?;
        names.push(id.to_string());
    }
    Ok(names)
}

/// One Ollama `/api/tags` entry's detail.
struct TagDetail {
    size_bytes: Option<u64>,
    parameter_size: Option<String>,
    quantization: Option<String>,
}

/// Ollama's own `GET /api/tags`, keyed by model name. Sent with no key, even
/// when one was given for `/v1/models`: this route is only ever guessed at,
/// never asked for by the person, so a proxied Ollama that demands auth for
/// it fails open to `None` (names only) rather than leaking a key to a route
/// that turns out not to be Ollama's at all. `None` for anything that goes
/// wrong -- connection error, non-2xx status, or a reply that doesn't parse
/// -- since a server simply not being Ollama is normal, not a failure to
/// report.
fn list_tags(agent: &ureq::Agent, url: &str) -> Option<HashMap<String, TagDetail>> {
    let raw = get(agent, url, None).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let models = value.get("models")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|entry| {
                let name = entry.get("name")?.as_str()?.to_string();
                let size_bytes = entry.get("size").and_then(Value::as_u64);
                let details = entry.get("details");
                let parameter_size = details
                    .and_then(|d| d.get("parameter_size"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let quantization = details
                    .and_then(|d| d.get("quantization_level"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some((
                    name,
                    TagDetail {
                        size_bytes,
                        parameter_size,
                        quantization,
                    },
                ))
            })
            .collect(),
    )
}

/// Ollama's own `POST /api/show`, for one model's capabilities. Sent with no
/// key, same reasoning as [`list_tags`]. `None` for anything that goes
/// wrong, same reasoning too: the caller keeps the model with unknown
/// capabilities rather than hiding it.
fn show_capabilities(agent: &ureq::Agent, ollama_root: &str, model: &str) -> Option<Vec<String>> {
    let url = format!("{ollama_root}/api/show");
    let mut response = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .send(json!({"model": model}).to_string())
        .ok()?;
    // `.reader()` decodes gzip/brotli before read_capped ever sees a byte,
    // so the cap is on the decoded body -- see LIST_BODY_LIMIT's doc.
    let reader = response.body_mut().with_config().reader();
    let raw = macalla_core::bounded_read::read_capped(reader, LIST_BODY_LIMIT).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let caps = value.get("capabilities")?.as_array()?;
    Some(
        caps.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// [`show_capabilities`] for every id in `ids`, `concurrency` lookups at a
/// time, for no longer than `budget` in total. When the budget runs out, no
/// new lookup is started; whatever hasn't been asked about yet, or hasn't
/// answered yet, is left `None` -- never hidden, never waited for past the
/// budget.
fn show_capabilities_bounded(
    agent: &ureq::Agent,
    ollama_root: &str,
    ids: &[String],
    concurrency: usize,
    budget: Duration,
) -> Vec<Option<Vec<String>>> {
    if ids.is_empty() {
        return Vec::new();
    }
    let deadline = Instant::now() + budget;
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<Vec<String>>>> = ids.iter().map(|_| Mutex::new(None)).collect();
    thread::scope(|scope| {
        for _ in 0..concurrency.min(ids.len()).max(1) {
            scope.spawn(|| loop {
                if Instant::now() >= deadline {
                    return;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= ids.len() {
                    return;
                }
                let caps = show_capabilities(agent, ollama_root, &ids[idx]);
                *results[idx].lock().unwrap() = caps;
            });
        }
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect()
}

/// How little reasoning a request asks for, least first. The order is what
/// [`ReasoningFloor`] relies on: it only ever moves to a larger value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ReasoningLevel {
    /// `effort: "none"`, the first thing asked for.
    None = 0,
    /// `effort: "minimal"`, after a server refused `"none"`.
    Minimal = 1,
    /// No reasoning field, after a server refused `"minimal"` too.
    Omitted = 2,
}

impl ReasoningLevel {
    /// The effort value to send, or `None` for no field at all.
    fn effort(self) -> Option<&'static str> {
        match self {
            Self::None => Some("none"),
            Self::Minimal => Some("minimal"),
            Self::Omitted => None,
        }
    }
}

/// What a server and model are known to refuse. Both parts only ever move
/// one way -- the floor down the ladder, temperature from accepted to
/// refused -- so any number of `Endpoint`s and overlapping calls can write
/// to one without a writer that learned less undoing one that learned more.
#[derive(Default)]
struct Learned {
    /// The least reasoning the server has accepted, so later requests start
    /// there rather than failing once each.
    reasoning_floor: ReasoningFloor,
    /// The server refused the request's `temperature`, so later requests go
    /// without it rather than failing once each. Independent of
    /// `reasoning_floor`: a model can refuse either, both, or neither.
    temperature_refused: AtomicBool,
}

/// Which [`Learned`] an endpoint shares: the chat URL with its userinfo,
/// query and fragment removed, and the model id. Nothing a person could
/// have typed as a secret is kept as a key, even in memory. A typed query is
/// dropped from the key, so two addresses that differ only in their query
/// share what they learned.
fn learned_key(url: &str, model: &str) -> (String, String) {
    let shown = redacted(url);
    let bare = shown.split(['?', '#']).next().unwrap_or_default();
    (bare.to_string(), model.to_string())
}

/// Every [`Learned`] in one place, by [`learned_key`]. In memory only, never
/// written anywhere. A `BTreeMap` because its constructor is `const`, so the
/// process-wide store is a plain `static`.
// build-less: one small entry per address and model ever built, never
// evicted; fine for the handful a person configures. Upgrade trigger: a
// caller that builds endpoints for arbitrary addresses in a loop.
#[derive(Default)]
struct LearnedStore(Mutex<BTreeMap<(String, String), Arc<Learned>>>);

/// The store [`Endpoint::new`] shares through.
static GLOBAL: LearnedStore = LearnedStore(Mutex::new(BTreeMap::new()));

impl LearnedStore {
    /// The [`Learned`] for `url` and `model`, made on first use.
    fn learned_for(&self, url: &str, model: &str) -> Arc<Learned> {
        let mut map = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(map.entry(learned_key(url, model)).or_default())
    }
}

/// The least reasoning a server has accepted, shared by overlapping calls.
/// It only moves down the ladder (none, minimal, omitted): a writer that
/// learned less never undoes one that learned more.
#[derive(Default)]
struct ReasoningFloor(AtomicU8);

impl ReasoningFloor {
    fn get(&self) -> ReasoningLevel {
        match self.0.load(Ordering::Relaxed) {
            0 => ReasoningLevel::None,
            1 => ReasoningLevel::Minimal,
            _ => ReasoningLevel::Omitted,
        }
    }

    fn record(&self, level: ReasoningLevel) {
        self.0.fetch_max(level as u8, Ordering::Relaxed);
    }
}

/// Whether a refusal's `param` names the reasoning field, so
/// [`Endpoint::complete_with`]'s loop takes a reasoning step rather than
/// failing through [`Endpoint::refusal_failure`] untouched. `None` counts:
/// a server may refuse the reasoning field without naming it (no `param`,
/// `param: null`, or an `error` that is only a string), and such a refusal
/// still needs the step.
/// Otherwise `param` must name exactly one of the reasoning parameters this
/// module itself sends ([`least_reasoning`]): OpenAI's flat
/// `"reasoning_effort"`, or OpenRouter's nested `"reasoning"` field, named
/// either as the field itself or as the `"reasoning.effort"` JSON-pointer
/// path some providers report. Anything else -- `"top_p"`, `"messages"`, a
/// context-length param -- is a refusal reasoning had nothing to do with.
const REASONING_PARAMS: [&str; 3] = ["reasoning_effort", "reasoning", "reasoning.effort"];

/// See [`REASONING_PARAMS`]'s doc.
fn refuses_reasoning(param: Option<&str>) -> bool {
    param.is_none_or(|p| REASONING_PARAMS.contains(&p))
}

/// Take the request `body` one step down the reasoning ladder, from what it
/// carries now: `"none"` becomes `"minimal"`, anything else drops the field.
/// `None`, and the body unchanged, if it carries no reasoning field. Works on
/// both shapes [`least_reasoning`] writes.
fn lower_reasoning(body: &mut Value) -> Option<ReasoningLevel> {
    let path = ["/reasoning_effort", "/reasoning/effort"]
        .into_iter()
        .find(|path| body.pointer(path).is_some())?;
    if let Some(effort) = body
        .pointer_mut(path)
        .filter(|e| e.as_str() == Some("none"))
    {
        *effort = json!("minimal");
        return Some(ReasoningLevel::Minimal);
    }
    let fields = body.as_object_mut()?;
    fields.remove("reasoning");
    fields.remove("reasoning_effort");
    Some(ReasoningLevel::Omitted)
}

/// The request fields that ask for `effort` reasoning (`"none"` or
/// `"minimal"`). OpenRouter takes one object and translates it for each
/// provider (<https://openrouter.ai/docs/use-cases/reasoning-tokens>), and
/// `exclude` keeps any reasoning out of the reply. Ollama and other servers
/// take the OpenAI-style field
/// (<https://docs.ollama.com/api/openai-compatibility>).
fn least_reasoning(url: &str, effort: &str) -> Value {
    let host = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', ':'])
        .next()
        .unwrap_or_default();
    if host == "openrouter.ai" || host.ends_with(".openrouter.ai") {
        json!({"reasoning": {"effort": effort, "exclude": true}})
    } else {
        json!({"reasoning_effort": effort})
    }
}

/// The reply's text, without a leading `<think>...</think>` block: some
/// servers leave a model's reasoning in the text, and some leave an empty block
/// even with reasoning off, which the guard would reject as added markup.
fn reply_text(raw: &str) -> Option<String> {
    let reply: Value = serde_json::from_str(raw).ok()?;
    let content = reply.pointer("/choices/0/message/content")?.as_str()?;
    let start = content.trim_start();
    let text = start
        .strip_prefix("<think>")
        .and_then(|rest| rest.split_once("</think>"))
        .map_or(content, |(_, after)| after.trim_start());
    Some(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{answer, item, serve, serve_once, Capture};
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    fn reply(text: &str) -> String {
        json!({"choices": [{"message": {"role": "assistant", "content": text}}]}).to_string()
    }

    #[test]
    fn a_block_goes_out_with_the_prompt_terms_and_model_and_comes_back_punctuated() {
        let (address, server) =
            serve_once("200 OK", reply("We need to ship the parser by Friday."));
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "qwen2.5:1.5b", None);
        assert_eq!(endpoint.caps().typical_ms, None);

        let out = endpoint
            .enhance(
                &item("we need to ship the parser by friday"),
                &["Macalla".into()],
            )
            .unwrap();

        assert_eq!(out, item("We need to ship the parser by Friday."));
        let (head, sent) = server.join().unwrap();
        assert!(head.starts_with("post /v1/chat/completions "), "{head}");
        assert!(!head.contains("authorization"));
        assert_eq!(sent["model"], "qwen2.5:1.5b");
        assert_eq!(sent["temperature"], 0);
        assert_eq!(sent["stream"], false);
        assert_eq!(sent["messages"][1]["role"], "user");
        assert_eq!(
            sent["messages"][1]["content"],
            "we need to ship the parser by friday"
        );
        let system = sent["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with(enhance::DEFAULT_PROMPT));
        assert!(system.contains("Macalla"));
        assert!(endpoint.caps().typical_ms.is_some());
    }

    #[test]
    fn with_the_default_prompt_a_reasoning_model_is_asked_not_to_reason() {
        let (address, server) = serve_once("200 OK", reply("Review the spec."));
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "granite4.2:8b", None);
        endpoint.enhance(&item("review the spec"), &[]).unwrap();
        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["reasoning_effort"], "none");
        assert!(sent.get("reasoning").is_none(), "{sent}");
    }

    #[test]
    fn openrouter_is_asked_in_its_own_shape_and_other_servers_in_openais() {
        assert_eq!(
            least_reasoning("https://openrouter.ai/api/v1/chat/completions", "none"),
            json!({"reasoning": {"effort": "none", "exclude": true}})
        );
        for url in [
            "http://localhost:11434/v1/chat/completions",
            "https://api.openai.com/v1/chat/completions",
            "https://notopenrouter.ai/v1/chat/completions",
        ] {
            assert_eq!(
                least_reasoning(url, "none"),
                json!({"reasoning_effort": "none"}),
                "{url}"
            );
        }
    }

    #[test]
    fn a_server_that_refuses_the_reasoning_field_is_asked_again_and_not_asked_twice() {
        let (address, server) = serve(vec![
            (
                "400 Bad Request",
                json!({"error": "unknown field"}).to_string(),
            ),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        assert_eq!(
            endpoint.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );
        assert_eq!(
            endpoint.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );
        let sent = server.join().unwrap();
        assert_eq!(sent[0].1["reasoning_effort"], "none");
        assert_eq!(sent[1].1["reasoning_effort"], "minimal", "{}", sent[1].1);
        assert_eq!(sent[2].1["reasoning_effort"], "minimal", "{}", sent[2].1);
    }

    #[test]
    fn a_leading_think_block_is_not_part_of_the_reply() {
        let (address, server) = serve_once(
            "200 OK",
            reply("<think>\n\n</think>\n\nWe need to ship the parser by Friday."),
        );
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        assert_eq!(
            endpoint
                .enhance(&item("we need to ship the parser by friday"), &[])
                .unwrap(),
            item("We need to ship the parser by Friday.")
        );
        server.join().unwrap();
    }

    #[test]
    fn a_key_goes_in_the_authorization_header() {
        let (address, server) = serve_once("200 OK", reply("Review the spec."));
        let endpoint = Endpoint::new_in(
            &LearnedStore::default(),
            &address,
            "model",
            Some("sk-test".into()),
        );
        endpoint.enhance(&item("review the spec"), &[]).unwrap();
        let (head, _) = server.join().unwrap();
        assert!(head.contains("authorization: bearer sk-test"), "{head}");
    }

    #[test]
    fn a_reply_that_answers_instead_of_punctuating_keeps_the_tier_0_text() {
        let (address, server) = serve_once(
            "200 OK",
            reply("The latency budget is three hundred milliseconds."),
        );
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        let asked = item("what is the latency budget for the parser");
        assert_eq!(endpoint.enhance(&asked, &[]).unwrap(), asked);
        server.join().unwrap();
    }

    #[test]
    fn with_the_default_prompt_a_swapped_word_keeps_the_tier_0_text() {
        let (address, server) =
            serve_once("200 OK", reply("The PostgreSQL database uses pgvector."));
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        let asked = item("the postgres database uses pgvector");
        assert_eq!(endpoint.enhance(&asked, &[]).unwrap(), asked);
        server.join().unwrap();
    }

    #[test]
    fn a_prompt_the_person_wrote_is_held_to_the_ratio_and_leaves_reasoning_to_the_server() {
        let (address, server) =
            serve_once("200 OK", reply("The PostgreSQL database uses pgvector."));
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None)
            .with_prompt("Fix product names.");
        let out = endpoint
            .enhance(&item("the postgres database uses pgvector"), &[])
            .unwrap();
        assert_eq!(out, item("The PostgreSQL database uses pgvector."));
        let (_, sent) = server.join().unwrap();
        assert!(sent.get("reasoning_effort").is_none(), "{sent}");
        assert!(sent.get("reasoning").is_none(), "{sent}");
    }

    #[test]
    fn reasoning_can_be_chosen_whatever_the_prompt() {
        let (address, server) = serve_once("200 OK", reply("Review the spec."));
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None)
            .with_reasoning(Reasoning::ServerDefault);
        endpoint.enhance(&item("review the spec"), &[]).unwrap();
        let (_, sent) = server.join().unwrap();
        assert!(sent.get("reasoning_effort").is_none(), "{sent}");
    }

    #[test]
    fn a_server_error_fails_and_names_the_address() {
        let (address, server) = serve_once("500 Internal Server Error", "{}".into());
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        let error = endpoint.enhance(&item("review the spec"), &[]).unwrap_err();
        assert!(error.to_string().contains(&address), "{error}");
        server.join().unwrap();
    }

    // -------------------------------------------- provider error detail

    fn openai_error(message: &str, param: &str, code: &str) -> String {
        json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": param,
                "code": code,
            }
        })
        .to_string()
    }

    #[test]
    fn a_model_that_refuses_reasoning_effort_and_then_temperature_is_enhanced_on_the_third_request()
    {
        let refuses_reasoning = openai_error(
            "Unsupported value: 'reasoning_effort' is not supported with this model.",
            "reasoning_effort",
            "unsupported_value",
        );
        let refuses_temperature = openai_error(
            "Unsupported value: 'temperature' does not support 0 with this model.",
            "temperature",
            "unsupported_value",
        );
        let (address, server) = serve(vec![
            ("400 Bad Request", refuses_reasoning),
            ("400 Bad Request", refuses_temperature),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-nano", None);

        assert_eq!(
            endpoint.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );
        assert_eq!(
            endpoint.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );

        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            4,
            "full, then minimal reasoning, then minimal and no temperature, \
             then one more the same"
        );
        assert_eq!(sent[0].1["reasoning_effort"], "none");
        assert_eq!(sent[0].1["temperature"], 0);
        assert_eq!(sent[1].1["reasoning_effort"], "minimal", "{}", sent[1].1);
        assert_eq!(sent[1].1["temperature"], 0);
        assert_eq!(sent[2].1["reasoning_effort"], "minimal", "{}", sent[2].1);
        assert!(sent[2].1.get("temperature").is_none(), "{}", sent[2].1);
        assert_eq!(sent[3].1["reasoning_effort"], "minimal", "{}", sent[3].1);
        assert!(sent[3].1.get("temperature").is_none(), "{}", sent[3].1);
    }

    #[test]
    fn a_model_that_refuses_only_temperature_keeps_reasoning_effort_and_is_enhanced_on_the_second_request(
    ) {
        let refuses_temperature = openai_error(
            "Unsupported value: 'temperature' does not support 0 with this model.",
            "temperature",
            "unsupported_value",
        );
        let (address, server) = serve(vec![
            ("400 Bad Request", refuses_temperature),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-nano", None);

        assert_eq!(
            endpoint.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );
        assert_eq!(
            endpoint.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );

        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 3, "full, then no temperature, then one more");
        assert_eq!(sent[0].1["reasoning_effort"], "none");
        assert_eq!(sent[0].1["temperature"], 0);
        assert_eq!(
            sent[1].1["reasoning_effort"], "none",
            "reasoning_effort is kept on the retry: only temperature was refused -- {}",
            sent[1].1
        );
        assert!(sent[1].1.get("temperature").is_none(), "{}", sent[1].1);
        assert_eq!(sent[2].1["reasoning_effort"], "none");
        assert!(sent[2].1.get("temperature").is_none(), "{}", sent[2].1);
    }

    #[test]
    fn an_unknown_param_is_not_retried_and_nothing_dictated_leaks_into_the_error() {
        let body = openai_error(
            "Unsupported value: 'top_p' is not supported with this model.",
            "top_p",
            "unsupported_value",
        );
        let (address, server) = serve_once("400 Bad Request", body);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-nano", None)
            .with_reasoning(Reasoning::ServerDefault);
        let marker = "do-not-leak-this-dictated-text";

        let error = endpoint
            .enhance(&item(marker), &["do-not-leak-this-term".into()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("400"), "{error}");
        assert!(error.contains("gpt-5-nano"), "{error}");
        assert!(error.contains("top_p"), "{error}");
        assert!(error.contains("unsupported_value"), "{error}");
        // The provider's own message is debug-only: it can quote input.
        assert!(
            !error.contains("is not supported with this model"),
            "{error}"
        );
        assert!(!error.contains(marker), "{error}");
        assert!(!error.contains("do-not-leak-this-term"), "{error}");
        assert!(!error.contains(enhance::DEFAULT_PROMPT), "{error}");

        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["temperature"], 0, "{sent}");
        assert!(sent.get("reasoning_effort").is_none(), "{sent}");
    }

    #[test]
    fn an_unknown_param_is_not_retried_as_reasoning_at_the_default_least_setting() {
        // With the default `Reasoning::Least`, the reasoning field is still
        // in this request's own body, but `param` names something the
        // reasoning step doesn't recognise, so it takes no step -- see
        // `refuses_reasoning` and the loop's own comment in `complete_with`.
        // `temperature` is untouched too, since its own step only ever fires
        // for `param == "temperature"`.
        let body = openai_error(
            "Unsupported value: 'top_p' is not supported with this model.",
            "top_p",
            "unsupported_value",
        );
        // A second reply is queued, so a reasoning retry would turn this
        // call into a success and fail the test outright; otherwise the next
        // enhance takes it.
        let (address, server) = serve(vec![
            ("400 Bad Request", body),
            ("200 OK", reply("Review the spec.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-nano", None);

        let error = endpoint
            .enhance(&item("review the spec"), &[])
            .unwrap_err()
            .to_string();

        assert!(error.contains("400"), "{error}");
        assert!(error.contains("gpt-5-nano"), "{error}");
        assert!(error.contains("top_p"), "{error}");
        assert!(
            !error.contains("is not supported with this model"),
            "the provider's own message is debug-only: {error}"
        );

        assert_eq!(
            endpoint.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );
        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 2, "no reasoning retry, then the next enhance");
        for (_, body) in &sent {
            assert_eq!(body["reasoning_effort"], "none", "{body}");
            assert_eq!(body["temperature"], 0, "{body}");
        }
    }

    #[test]
    fn a_context_length_refusal_takes_no_reasoning_step_and_leaves_the_floor_unchanged() {
        let marker = "do-not-leak-this-dictated-text";
        let body = openai_error(
            &format!("This model's maximum context length is 4096 tokens, in '{marker}'"),
            "messages",
            "context_length_exceeded",
        );
        let (address, server) = serve(vec![
            ("400 Bad Request", body),
            ("200 OK", reply("Ship it.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-nano", None);

        let error = endpoint
            .enhance(&item(marker), &["do-not-leak-this-term".into()])
            .unwrap_err()
            .to_string();

        assert!(error.contains("400"), "{error}");
        assert!(error.contains("gpt-5-nano"), "{error}");
        assert!(error.contains("messages"), "{error}");
        // The provider's own message is debug-only: it can quote input.
        assert!(!error.contains("maximum context length"), "{error}");
        assert!(!error.contains(marker), "{error}");
        assert!(!error.contains("do-not-leak-this-term"), "{error}");

        assert_eq!(
            endpoint.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );

        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "one send for the refused call, no reasoning retry; one for the next"
        );
        assert_eq!(
            sent[0].1["reasoning_effort"], "none",
            "the first call never gets to send a second request"
        );
        assert_eq!(
            sent[1].1["reasoning_effort"], "none",
            "an unrelated refusal never moves reasoning_floor: {}",
            sent[1].1
        );
    }

    #[test]
    fn a_400_with_no_param_still_runs_the_reasoning_ladder() {
        let body = json!({
            "error": {
                "message": "Unsupported value: 'reasoning_effort' is not supported with this model.",
                "type": "invalid_request_error",
                "code": "unsupported_value",
            }
        })
        .to_string();
        let (address, server) = serve(vec![
            ("400 Bad Request", body),
            ("200 OK", reply("Review the spec.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-mini", None);

        assert_eq!(
            endpoint.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );

        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 2, "none, then minimal, then accepted");
        assert_eq!(effort(&sent[0].1), Some("none"));
        assert_eq!(effort(&sent[1].1), Some("minimal"));
    }

    #[test]
    fn a_non_json_error_body_still_reports_status_and_model_without_panicking() {
        let (address, server) = serve_once("400 Bad Request", "not json at all".into());
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model-x", None)
            .with_reasoning(Reasoning::ServerDefault);
        let error = endpoint.enhance(&item("review the spec"), &[]).unwrap_err();
        assert!(error.to_string().contains("400"), "{error}");
        assert!(error.to_string().contains("model-x"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_server_error_without_reasoning_sends_one_request_and_reports_status_and_model() {
        let (address, server) = serve_once("500 Internal Server Error", "{}".into());
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model-y", None)
            .with_reasoning(Reasoning::ServerDefault);
        let error = endpoint.enhance(&item("review the spec"), &[]).unwrap_err();
        assert!(error.to_string().contains("500"), "{error}");
        assert!(error.to_string().contains("model-y"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_long_or_multiline_provider_message_is_logged_at_debug_on_one_line_and_capped() {
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let capture = Capture {
            events: events.clone(),
        };
        let long_message = format!("line one\nline two\n{}", "x".repeat(500));
        let body = openai_error(&long_message, "temperature", "unsupported_value");
        let (address, server) = serve_once("400 Bad Request", body);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model-z", None)
            .with_reasoning(Reasoning::ServerDefault);
        let error = crate::test_support::with_capture(capture, || {
            endpoint.enhance(&item("review the spec"), &[]).unwrap_err()
        })
        .to_string();
        server.join().unwrap();

        assert!(!error.contains("line one"), "{error}");
        let events = events.lock().unwrap();
        let debug = events
            .iter()
            .find(|e| e.fields.contains_key("provider_message"))
            .expect("the provider's message was logged");
        assert_eq!(debug.level, tracing::Level::DEBUG);
        let logged = &debug.fields["provider_message"];
        assert!(logged.contains("line one line two"), "{logged}");
        assert!(!logged.contains('\n'), "{logged}");
        assert!(logged.len() < long_message.len(), "{logged}");
    }

    #[test]
    fn the_retry_log_line_carries_the_model_and_the_providers_detail() {
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let capture = Capture {
            events: events.clone(),
        };
        let marker = "do-not-leak-this-dictated-text";
        // A provider can quote the input back in its message.
        let body = openai_error(
            &format!("'temperature' does not support 0, in '{marker}'"),
            "temperature",
            "unsupported_value",
        );
        let (address, server) = serve(vec![
            ("400 Bad Request", body),
            ("200 OK", reply("Review the spec.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-nano", None);

        crate::test_support::with_capture(capture, || {
            endpoint
                .enhance(&item(marker), &["do-not-leak-this-term".into()])
                .unwrap();
        });
        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[1].1.get("temperature").is_none(), "{}", sent[1].1);
        assert_eq!(sent[1].1["reasoning_effort"], "none", "{}", sent[1].1);

        let events = events.lock().unwrap();
        let retry = events
            .iter()
            .find(|e| e.message.contains("temperature; asking again"))
            .expect("the retry log line was emitted");
        assert_eq!(
            retry.fields.get("model").map(String::as_str),
            Some("gpt-5-nano")
        );
        assert!(
            retry
                .fields
                .get("param")
                .is_some_and(|v| v.contains("temperature")),
            "{:?}",
            retry.fields
        );
        assert!(
            retry
                .fields
                .get("code")
                .is_some_and(|v| v.contains("unsupported_value")),
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
            Some("gpt-5-nano")
        );
        // Nothing sent is logged anywhere but the debug provider_message.
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
        }
    }

    // ------------------------------------------- the reasoning ladder

    fn refuses_param(param: &str) -> String {
        openai_error(
            &format!("Unsupported value: '{param}' is not supported with this model."),
            param,
            "unsupported_value",
        )
    }

    /// The reasoning setting a request carried, in either shape.
    fn effort(sent: &Value) -> Option<&str> {
        sent.get("reasoning_effort")
            .or_else(|| sent.pointer("/reasoning/effort"))
            .and_then(Value::as_str)
    }

    fn two_enhances(endpoint: &Endpoint) {
        assert_eq!(
            endpoint.enhance(&item("review the spec"), &[]).unwrap(),
            item("Review the spec.")
        );
        assert_eq!(
            endpoint.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );
    }

    #[test]
    // Also covers a 400 whose `param` is exactly "reasoning_effort": every
    // `refuses_param("reasoning_effort")` reply below is that case, and
    // `refuses_reasoning` accepts it.
    fn a_model_that_refuses_no_reasoning_is_asked_for_minimal_and_keeps_it() {
        let (address, server) = serve(vec![
            ("400 Bad Request", refuses_param("reasoning_effort")),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        two_enhances(&Endpoint::new_in(
            &LearnedStore::default(),
            &address,
            "gpt-5-mini",
            None,
        ));
        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 3, "none, then minimal, then minimal again");
        assert_eq!(effort(&sent[0].1), Some("none"));
        assert_eq!(effort(&sent[1].1), Some("minimal"));
        assert_eq!(effort(&sent[2].1), Some("minimal"));
        for (_, body) in &sent {
            assert_eq!(body["temperature"], 0, "{body}");
        }
    }

    #[test]
    fn a_model_that_refuses_none_and_minimal_goes_without_the_field() {
        let (address, server) = serve(vec![
            ("400 Bad Request", refuses_param("reasoning_effort")),
            ("400 Bad Request", refuses_param("reasoning_effort")),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        two_enhances(&Endpoint::new_in(
            &LearnedStore::default(),
            &address,
            "gpt-4.1-nano",
            None,
        ));
        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 4, "none, minimal, no field, then no field");
        assert_eq!(effort(&sent[0].1), Some("none"));
        assert_eq!(effort(&sent[1].1), Some("minimal"));
        assert!(sent[2].1.get("reasoning_effort").is_none(), "{}", sent[2].1);
        assert!(sent[3].1.get("reasoning_effort").is_none(), "{}", sent[3].1);
    }

    #[test]
    fn a_model_that_refuses_none_then_temperature_keeps_minimal_without_temperature() {
        let (address, server) = serve(vec![
            ("400 Bad Request", refuses_param("reasoning_effort")),
            ("400 Bad Request", refuses_param("temperature")),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        two_enhances(&Endpoint::new_in(
            &LearnedStore::default(),
            &address,
            "gpt-5-mini",
            None,
        ));
        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            4,
            "none, minimal, minimal without temperature, then the same"
        );
        assert_eq!(effort(&sent[0].1), Some("none"));
        assert_eq!(sent[0].1["temperature"], 0);
        assert_eq!(effort(&sent[1].1), Some("minimal"));
        assert_eq!(sent[1].1["temperature"], 0);
        for (_, body) in &sent[2..] {
            assert_eq!(effort(body), Some("minimal"), "{body}");
            assert!(body.get("temperature").is_none(), "{body}");
        }
    }

    #[test]
    fn a_model_that_refuses_none_minimal_and_temperature_is_answered_by_the_fourth_send() {
        let (address, server) = serve(vec![
            ("400 Bad Request", refuses_param("reasoning_effort")),
            ("400 Bad Request", refuses_param("reasoning_effort")),
            ("400 Bad Request", refuses_param("temperature")),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        two_enhances(&Endpoint::new_in(
            &LearnedStore::default(),
            &address,
            "gpt-5-mini",
            None,
        ));
        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            5,
            "four sends for the first call, one for the next"
        );
        assert_eq!(effort(&sent[0].1), Some("none"));
        assert_eq!(effort(&sent[1].1), Some("minimal"));
        assert!(sent[2].1.get("reasoning_effort").is_none(), "{}", sent[2].1);
        assert_eq!(sent[2].1["temperature"], 0);
        for (_, body) in &sent[3..] {
            assert!(body.get("reasoning_effort").is_none(), "{body}");
            assert!(body.get("temperature").is_none(), "{body}");
        }
    }

    #[test]
    fn openrouter_is_asked_for_minimal_in_its_own_shape() {
        let mut body = json!({"reasoning": {"effort": "none", "exclude": true}});
        assert!(lower_reasoning(&mut body) == Some(ReasoningLevel::Minimal));
        assert_eq!(
            body,
            json!({"reasoning": {"effort": "minimal", "exclude": true}})
        );
        assert!(lower_reasoning(&mut body) == Some(ReasoningLevel::Omitted));
        assert_eq!(body, json!({}));
        assert!(lower_reasoning(&mut body).is_none());
    }

    #[test]
    fn the_remembered_reasoning_level_only_moves_down() {
        let floor = ReasoningFloor::default();
        assert!(floor.get() == ReasoningLevel::None);
        floor.record(ReasoningLevel::Omitted);
        floor.record(ReasoningLevel::Minimal);
        floor.record(ReasoningLevel::None);
        assert!(floor.get() == ReasoningLevel::Omitted);

        let floor = ReasoningFloor::default();
        floor.record(ReasoningLevel::Minimal);
        assert!(floor.get() == ReasoningLevel::Minimal);
    }

    #[test]
    fn each_reasoning_step_logs_its_own_line_and_nothing_dictated_outside_debug() {
        let events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let capture = Capture {
            events: events.clone(),
        };
        let marker = "do-not-leak-this-dictated-text";
        let body = openai_error(
            &format!("'reasoning_effort' is not supported, in '{marker}'"),
            "reasoning_effort",
            "unsupported_value",
        );
        let (address, server) = serve(vec![
            ("400 Bad Request", body.clone()),
            ("400 Bad Request", body),
            ("200 OK", reply("Review the spec.")),
        ]);
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "gpt-5-mini", None);
        crate::test_support::with_capture(capture, || {
            endpoint.enhance(&item(marker), &[]).unwrap();
        });
        assert_eq!(server.join().unwrap().len(), 3);

        let events = events.lock().unwrap();
        for line in [
            "the server refused the request for no reasoning; asking again with minimal reasoning",
            "the server refused the request for minimal reasoning; asking again without it",
        ] {
            let step = events
                .iter()
                .find(|e| e.message == line)
                .unwrap_or_else(|| panic!("no log line {line:?}"));
            assert_eq!(step.level, tracing::Level::INFO);
            assert_eq!(
                step.fields.get("model").map(String::as_str),
                Some("gpt-5-mini")
            );
            assert!(!step.fields.contains_key("provider_message"));
        }
        for event in events.iter() {
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
        }
        let debug: Vec<_> = events
            .iter()
            .filter(|e| e.fields.contains_key("provider_message"))
            .collect();
        assert_eq!(debug.len(), 2, "one debug line per refused request");
        assert!(debug
            .iter()
            .all(|e| e.level == tracing::Level::DEBUG
                && e.fields["provider_message"].contains(marker)));
    }

    #[test]
    fn a_reply_with_no_text_in_it_fails() {
        let (address, server) = serve_once("200 OK", json!({"choices": []}).to_string());
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        let error = endpoint.enhance(&item("review the spec"), &[]).unwrap_err();
        assert!(error.to_string().contains("no text"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn an_oversize_reply_is_rejected_not_read_whole() {
        let huge = "a".repeat(CHAT_REPLY_FLOOR as usize + 1);
        let (address, server) = serve_once("200 OK", reply(&huge));
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        let error = endpoint.enhance(&item("review the spec"), &[]).unwrap_err();
        assert!(error.to_string().contains("more than"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn nothing_listening_fails_rather_than_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let started = Instant::now();
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        assert!(endpoint.enhance(&item("review the spec"), &[]).is_err());
        assert!(started.elapsed() <= CONNECT, "took {:?}", started.elapsed());
    }

    #[test]
    fn the_address_a_person_types_becomes_the_chat_url() {
        for (typed, url) in [
            (
                "http://localhost:11434",
                "http://localhost:11434/v1/chat/completions",
            ),
            (
                "http://localhost:11434/",
                "http://localhost:11434/v1/chat/completions",
            ),
            (
                " http://box:8080/v1 ",
                "http://box:8080/v1/chat/completions",
            ),
            (
                "https://api.example.com/openai/v1",
                "https://api.example.com/openai/v1/chat/completions",
            ),
            (
                "http://box/v1/chat/completions",
                "http://box/v1/chat/completions",
            ),
            (
                "http://[::1]:11434",
                "http://[::1]:11434/v1/chat/completions",
            ),
            (
                "https://openrouter.ai/api/v1",
                "https://openrouter.ai/api/v1/chat/completions",
            ),
        ] {
            assert_eq!(chat_url(typed), url, "typed {typed:?}");
        }
    }

    #[test]
    fn with_no_model_named_it_is_not_available() {
        assert!(!Endpoint::new_in(
            &LearnedStore::default(),
            "http://localhost:11434",
            " ",
            None
        )
        .is_available());
        assert!(Endpoint::new_in(
            &LearnedStore::default(),
            "http://localhost:11434",
            "qwen2.5:0.5b",
            None
        )
        .is_available());
    }

    // ---------------------------------------------------------- TextModel

    #[test]
    fn text_model_complete_sends_the_instructions_and_text_and_returns_the_reply() {
        let (address, server) = serve_once("200 OK", reply("Three words: parser shipped Friday."));
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);

        let out = TextModel::complete(
            &endpoint,
            "Summarise in three words.",
            "we shipped the parser on friday",
        )
        .unwrap();

        assert_eq!(out, "Three words: parser shipped Friday.");
        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["messages"][0]["content"], "Summarise in three words.");
        assert_eq!(
            sent["messages"][1]["content"],
            "we shipped the parser on friday"
        );
    }

    #[test]
    fn text_model_complete_returns_an_error_reply_as_an_error() {
        let (address, server) = serve_once("500 Internal Server Error", "{}".into());
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);

        let error = TextModel::complete(&endpoint, "Summarise in three words.", "we shipped it")
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
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None)
            .with_prompt("Fix product names.");
        assert_eq!(
            Reasoning::for_prompt("Fix product names."),
            Reasoning::ServerDefault,
            "the prompt used by this test must not itself ask for the least reasoning"
        );

        TextModel::complete(&endpoint, "Summarise in three words.", "we shipped it").unwrap();

        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["reasoning_effort"], "none");
    }

    // ------------------------------------------------------------ list_models

    fn openai_models(ids: &[&str]) -> String {
        json!({
            "object": "list",
            "data": ids.iter().map(|id| json!({
                "id": id, "object": "model", "created": 1_789_129_412u64, "owned_by": "library",
            })).collect::<Vec<_>>(),
        })
        .to_string()
    }

    #[test]
    fn an_openai_only_server_gives_names_only() {
        let (address, server) = serve(vec![
            ("200 OK", openai_models(&["granite4.1:3b"])),
            ("404 Not Found", "{}".to_string()),
        ]);
        let models = list_models(&address, None).unwrap();
        assert_eq!(
            models,
            vec![ServerModel {
                id: "granite4.1:3b".to_string(),
                size_bytes: None,
                parameter_size: None,
                quantization: None,
                capabilities: None,
            }]
        );
        let sent = server.join().unwrap();
        assert!(sent[0].0.starts_with("get /v1/models "), "{}", sent[0].0);
        assert!(sent[1].0.starts_with("get /api/tags "), "{}", sent[1].0);
    }

    #[test]
    fn an_ollama_server_gives_details_and_filters_out_an_embedding_only_model() {
        let (address, server) = serve(vec![
            (
                "200 OK",
                openai_models(&["granite4.1:3b", "nomic-embed-text"]),
            ),
            (
                "200 OK",
                json!({"models": [
                    {
                        "name": "granite4.1:3b",
                        "size": 2_099_520_281u64,
                        "details": {
                            "format": "gguf", "family": "granite", "families": ["granite"],
                            "parameter_size": "3.4B", "quantization_level": "Q4_K_M",
                            "context_length": 131_072,
                        },
                    },
                    {
                        "name": "nomic-embed-text",
                        "size": 274_302_450u64,
                        "details": {
                            "format": "gguf", "family": "nomic-bert", "families": ["nomic-bert"],
                            "parameter_size": "137M", "quantization_level": "F16",
                            "context_length": 2048,
                        },
                    },
                ]})
                .to_string(),
            ),
            (
                "200 OK",
                json!({"capabilities": ["completion", "tools"]}).to_string(),
            ),
            ("200 OK", json!({"capabilities": ["embedding"]}).to_string()),
        ]);
        // Concurrency 1: the show calls below assume request order, which is
        // only guaranteed sequentially -- the concurrency itself has its own
        // test, a_stalled_show_phase_returns_within_the_budget_and_keeps_
        // every_model, below.
        let models = list_models_with(&address, None, &list_agent(), 1, LIST_TOTAL).unwrap();
        assert_eq!(
            models,
            vec![ServerModel {
                id: "granite4.1:3b".to_string(),
                size_bytes: Some(2_099_520_281),
                parameter_size: Some("3.4B".to_string()),
                quantization: Some("Q4_K_M".to_string()),
                capabilities: Some(vec!["completion".to_string(), "tools".to_string()]),
            }]
        );
        let sent = server.join().unwrap();
        assert!(sent[0].0.starts_with("get /v1/models "), "{}", sent[0].0);
        assert!(sent[1].0.starts_with("get /api/tags "), "{}", sent[1].0);
        assert!(sent[2].0.starts_with("post /api/show "), "{}", sent[2].0);
        assert_eq!(sent[2].1["model"], "granite4.1:3b");
        assert!(sent[3].0.starts_with("post /api/show "), "{}", sent[3].0);
        assert_eq!(sent[3].1["model"], "nomic-embed-text");
    }

    #[test]
    fn a_failing_show_call_keeps_the_model_with_unknown_capabilities() {
        let (address, server) = serve(vec![
            ("200 OK", openai_models(&["granite4.1:3b"])),
            (
                "200 OK",
                json!({"models": [{
                    "name": "granite4.1:3b",
                    "size": 2_099_520_281u64,
                    "details": {"parameter_size": "3.4B", "quantization_level": "Q4_K_M"},
                }]})
                .to_string(),
            ),
            ("500 Internal Server Error", "{}".to_string()),
        ]);
        let models = list_models(&address, None).unwrap();
        assert_eq!(
            models,
            vec![ServerModel {
                id: "granite4.1:3b".to_string(),
                size_bytes: Some(2_099_520_281),
                parameter_size: Some("3.4B".to_string()),
                quantization: Some("Q4_K_M".to_string()),
                capabilities: None,
            }]
        );
        server.join().unwrap();
    }

    #[test]
    fn a_malformed_models_reply_is_an_error() {
        let (address, server) = serve_once("200 OK", json!({"object": "list"}).to_string());
        let error = list_models(&address, None).unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_data_entry_with_no_id_gives_a_specific_error() {
        let (address, server) = serve_once(
            "200 OK",
            json!({"object": "list", "data": [{"object": "model"}]}).to_string(),
        );
        let error = list_models(&address, None).unwrap_err();
        assert!(error.to_string().contains("no id"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn an_unreachable_address_fails_bounded_and_names_the_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let started = Instant::now();
        let error = list_models(&address, None).unwrap_err();
        assert!(
            started.elapsed() <= LIST_CONNECT * 2,
            "took {:?}",
            started.elapsed()
        );
        assert!(error.to_string().contains(&address), "{error}");
    }

    #[test]
    fn a_key_reaches_models_only_when_given_and_never_tags_or_show() {
        let (address, server) = serve(vec![
            ("200 OK", openai_models(&["m"])),
            ("200 OK", json!({"models": [{"name": "m"}]}).to_string()),
            (
                "200 OK",
                json!({"capabilities": ["completion"]}).to_string(),
            ),
        ]);
        list_models(&address, Some("sk-test")).unwrap();
        let sent = server.join().unwrap();
        assert!(
            sent[0].0.contains("authorization: bearer sk-test"),
            "{}",
            sent[0].0
        );
        // A proxied Ollama that demands auth for its own routes must still
        // fail open to names only, so these two never carry the key at all,
        // whether or not the server would have accepted it.
        assert!(!sent[1].0.contains("authorization"), "{}", sent[1].0);
        assert!(!sent[2].0.contains("authorization"), "{}", sent[2].0);

        let (address, server) = serve(vec![
            ("200 OK", openai_models(&["m"])),
            ("404 Not Found", "{}".to_string()),
        ]);
        list_models(&address, None).unwrap();
        let sent = server.join().unwrap();
        assert!(!sent[0].0.contains("authorization"), "{}", sent[0].0);
    }

    // ------------------------------------------------- ollama_root gating

    #[test]
    fn an_openrouter_shaped_address_is_never_probed_for_ollama_routes() {
        let (root, server) = serve_once_and_watch("200 OK", openai_models(&["gpt-x"]));
        let address = format!("{root}/api/v1");
        let models = list_models(&address, None).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-x");
        assert!(
            !server.join().unwrap(),
            "a second request was made to an OpenRouter-shaped address"
        );
    }

    #[test]
    fn ollama_root_is_none_for_anything_but_a_bare_host_or_a_bare_v1_path() {
        for address in [
            "http://localhost:11434",
            "http://localhost:11434/v1",
            "http://localhost:11434/v1/chat/completions",
        ] {
            assert!(ollama_root(address).is_some(), "{address}");
        }
        for address in [
            "https://openrouter.ai/api/v1",
            "https://host/some-prefix/v1",
            "https://api.openai.com/v1/chat/completions/v1",
        ] {
            assert!(ollama_root(address).is_none(), "{address}");
        }
    }

    // ------------------------------------------------------- bounded concurrency

    /// A server that answers `/v1/models` and `/api/tags` normally, then
    /// never answers `/api/show` at all: every such connection is read and
    /// then left hanging, so it's the client's own timeout that ends the
    /// call, not the server closing it.
    fn stalling_show_server(v1_models: String, tags: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            answer(&listener, "200 OK", &v1_models);
            answer(&listener, "200 OK", &tags);
            loop {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                        line.clear();
                    }
                    thread::sleep(Duration::from_secs(3600));
                });
            }
        });
        address
    }

    fn tiny_timeout_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_millis(50)))
            .timeout_global(Some(Duration::from_millis(100)))
            .build()
            .into()
    }

    #[test]
    fn a_stalled_show_phase_returns_within_the_budget_and_keeps_every_model() {
        let ids: Vec<String> = (0..6).map(|i| format!("m{i}")).collect();
        let names: Vec<&str> = ids.iter().map(String::as_str).collect();
        let tags = json!({
            "models": ids.iter().map(|id| json!({"name": id})).collect::<Vec<_>>(),
        })
        .to_string();
        let address = stalling_show_server(openai_models(&names), tags);

        let budget = Duration::from_millis(150);
        let started = Instant::now();
        let models = list_models_with(&address, None, &tiny_timeout_agent(), 2, budget).unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < budget + Duration::from_millis(500),
            "took {elapsed:?}, budget was {budget:?}"
        );
        assert_eq!(models.len(), ids.len(), "no model is hidden by a timeout");
        assert!(
            models.iter().all(|m| m.capabilities.is_none()),
            "{models:?}"
        );
    }

    // ------------------------------------------------ refusals across rebuilds

    /// A server that refuses `none`, then `minimal`, then `temperature` for
    /// one model, then answers `extra` more requests.
    fn refusing_everything_then(extra: usize) -> (String, JoinHandle<Vec<(String, Value)>>) {
        let mut replies = vec![
            (
                "400 Bad Request",
                openai_error("no", "reasoning_effort", "unsupported_value"),
            ),
            (
                "400 Bad Request",
                openai_error("no", "reasoning_effort", "unsupported_value"),
            ),
            (
                "400 Bad Request",
                openai_error("no", "temperature", "unsupported_value"),
            ),
            ("200 OK", reply("Review the spec.")),
        ];
        replies.extend((0..extra).map(|_| ("200 OK", reply("Ship it."))));
        serve(replies)
    }

    #[test]
    fn a_rebuilt_endpoint_for_the_same_address_and_model_asks_in_the_learned_shape_at_once() {
        let store = LearnedStore::default();
        let (address, server) = refusing_everything_then(1);
        let first = Endpoint::new_in(&store, &address, "gpt-5-mini", None);
        first.enhance(&item("review the spec"), &[]).unwrap();
        drop(first);

        let rebuilt = Endpoint::new_in(&store, &format!("{address}/v1/"), " gpt-5-mini ", None);
        assert_eq!(
            rebuilt.enhance(&item("ship it"), &[]).unwrap(),
            item("Ship it.")
        );

        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 5, "four for the first, one for the rebuilt");
        let last = &sent[4].1;
        assert!(last.get("reasoning_effort").is_none(), "{last}");
        assert!(last.get("temperature").is_none(), "{last}");
    }

    #[test]
    fn a_rebuilt_endpoint_keeps_only_the_steps_that_were_taken() {
        let store = LearnedStore::default();
        let (address, server) = serve(vec![
            (
                "400 Bad Request",
                openai_error("no", "reasoning_effort", "unsupported_value"),
            ),
            ("200 OK", reply("Review the spec.")),
            ("200 OK", reply("Ship it.")),
        ]);
        Endpoint::new_in(&store, &address, "gpt-5-mini", None)
            .enhance(&item("review the spec"), &[])
            .unwrap();
        Endpoint::new_in(&store, &address, "gpt-5-mini", None)
            .enhance(&item("ship it"), &[])
            .unwrap();
        let sent = server.join().unwrap();
        assert_eq!(sent[2].1["reasoning_effort"], "minimal", "{}", sent[2].1);
        assert_eq!(sent[2].1["temperature"], 0, "{}", sent[2].1);
    }

    #[test]
    fn a_ladder_that_ends_in_failure_teaches_a_rebuilt_endpoint_nothing() {
        // An unrelated 400 with no param: every reasoning step is taken and
        // none cures it, so nothing about the server was learned.
        let store = LearnedStore::default();
        let unrelated = || json!({"error": {"message": "bad request"}}).to_string();
        let (address, server) = serve(vec![
            ("400 Bad Request", unrelated()),
            ("400 Bad Request", unrelated()),
            ("400 Bad Request", unrelated()),
            ("200 OK", reply("Ship it.")),
        ]);
        let first = Endpoint::new_in(&store, &address, "gpt-5-mini", None);
        assert!(first.enhance(&item("review the spec"), &[]).is_err());
        let rebuilt = Endpoint::new_in(&store, &address, "gpt-5-mini", None);
        rebuilt.enhance(&item("ship it"), &[]).unwrap();
        let sent = server.join().unwrap();
        assert_eq!(sent.len(), 4, "three for the failed ladder, one after");
        assert!(sent[2].1.get("reasoning_effort").is_none(), "{}", sent[2].1);
        assert_eq!(sent[3].1["reasoning_effort"], "none", "{}", sent[3].1);
        assert_eq!(sent[3].1["temperature"], 0, "{}", sent[3].1);
    }

    #[test]
    fn another_model_or_another_address_starts_fresh() {
        let store = LearnedStore::default();
        let (address, server) = refusing_everything_then(1);
        Endpoint::new_in(&store, &address, "gpt-5-mini", None)
            .enhance(&item("review the spec"), &[])
            .unwrap();
        Endpoint::new_in(&store, &address, "gpt-4.1-mini", None)
            .enhance(&item("ship it"), &[])
            .unwrap();
        let sent = server.join().unwrap();
        assert_eq!(sent[4].1["reasoning_effort"], "none", "{}", sent[4].1);
        assert_eq!(sent[4].1["temperature"], 0, "{}", sent[4].1);

        let (other, server) = serve_once("200 OK", reply("Ship it."));
        Endpoint::new_in(&store, &other, "gpt-5-mini", None)
            .enhance(&item("ship it"), &[])
            .unwrap();
        let (_, sent) = server.join().unwrap();
        assert_eq!(sent["reasoning_effort"], "none", "{sent}");
        assert_eq!(sent["temperature"], 0, "{sent}");
    }

    #[test]
    fn endpoint_new_shares_the_process_wide_store() {
        // No request: an address nothing listens on, a model no other test
        // names, so no other test's learning can reach it.
        let address = "http://127.0.0.1:9/process-wide-store-test";
        let first = Endpoint::new(address, "only-this-test", None);
        let second = Endpoint::new(address, "only-this-test", None);
        assert!(Arc::ptr_eq(&first.learned, &second.learned));
        assert!(!Arc::ptr_eq(
            &first.learned,
            &Endpoint::new(address, "only-this-test-2", None).learned
        ));
    }

    #[test]
    fn what_was_learned_never_moves_back_under_two_writers() {
        for _ in 0..50 {
            let learned = Arc::new(Learned::default());
            let writers: Vec<_> = [
                [ReasoningLevel::Omitted, ReasoningLevel::None],
                [ReasoningLevel::None, ReasoningLevel::Minimal],
            ]
            .into_iter()
            .map(|levels| {
                let learned = Arc::clone(&learned);
                thread::spawn(move || {
                    for level in levels {
                        learned.reasoning_floor.record(level);
                    }
                    learned.temperature_refused.store(true, Ordering::Relaxed);
                })
            })
            .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            assert!(learned.reasoning_floor.get() == ReasoningLevel::Omitted);
            assert!(learned.temperature_refused.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn the_stores_key_holds_no_userinfo_query_or_fragment() {
        let url = chat_url("https://user:hunter2@api.example.com/v1?api-key=sk-secret#frag");
        let (address, model) = learned_key(&url, "gpt-5-mini");
        assert!(!address.contains("hunter2"), "{address}");
        assert!(!address.contains("user"), "{address}");
        assert!(!address.contains("sk-secret"), "{address}");
        assert!(!address.contains("frag"), "{address}");
        assert_eq!(address, "https://api.example.com/v1");
        assert_eq!(model, "gpt-5-mini");

        let plain = chat_url("https://api.example.com/v1");
        assert_eq!(
            learned_key(&plain, "m").0,
            "https://api.example.com/v1/chat/completions"
        );
    }

    // ------------------------------------------------------- credential redaction

    #[test]
    fn userinfo_is_stripped_from_a_url_for_display() {
        assert_eq!(
            redacted("http://user:hunter2@host:11434/v1/models"),
            "http://host:11434/v1/models"
        );
        assert_eq!(
            redacted("http://user:hunter2@host:11434"),
            "http://host:11434"
        );
        assert_eq!(redacted("http://host:11434/v1"), "http://host:11434/v1");
    }

    #[test]
    fn a_password_with_a_slash_in_it_is_stripped_too() {
        assert_eq!(
            redacted("https://user:pa/ss@host:8443/v1/chat/completions"),
            "https://host:8443/v1/chat/completions"
        );
        assert_eq!(redacted("http://u:a/b/c@host"), "http://host");
        assert_eq!(
            redacted("http://host/v1?next=a@b"),
            "http://b",
            "an @ anywhere is taken as userinfo: less is shown, never more"
        );
        assert_eq!(redacted("https://u:p?s@host/v1"), "https://host/v1");
        assert_eq!(redacted("https://u:p#s@host/v1"), "https://host/v1");
        assert_eq!(redacted("https://u:p:w@host/v1"), "https://host/v1");
        let (address, _) = learned_key(&chat_url("http://u:pa/ss@host/v1"), "m");
        assert_eq!(address, "http://host/v1/chat/completions");
    }

    #[test]
    fn a_password_in_the_address_never_reaches_a_chat_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let address = format!("http://user:hunter2@127.0.0.1:{port}");
        let endpoint = Endpoint::new_in(&LearnedStore::default(), &address, "model", None);
        let error = endpoint.enhance(&item("review the spec"), &[]).unwrap_err();
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }

    #[test]
    fn a_password_in_the_address_never_reaches_a_list_models_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let address = format!("http://user:hunter2@127.0.0.1:{port}");
        let error = list_models(&address, None).unwrap_err();
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }

    // ------------------------------------------------- serve_once_and_watch

    /// Answers one request, then reports whether the client tried a second
    /// one within a short window. A guarded probe that is skipped and one
    /// that is attempted-but-fails both leave the return value unchanged, so
    /// asserting "no second request" needs a server that can tell the
    /// difference -- this one keeps listening afterwards instead of exiting.
    fn serve_once_and_watch(status: &str, body: String) -> (String, JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_string();
        let handle = thread::spawn(move || {
            answer(&listener, &status, &body);
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                if listener.accept().is_ok() {
                    return true;
                }
                thread::sleep(Duration::from_millis(5));
            }
            false
        });
        (address, handle)
    }
}
