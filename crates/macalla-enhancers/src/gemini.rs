// SPDX-License-Identifier: MIT
//! Gemini's native Models API: `GET
//! https://generativelanguage.googleapis.com/v1beta/models`
//! (per the provider's documentation), key as the
//! `x-goog-api-key` header -- never the `?key=` query-string form the same
//! docs mention, so a key never ends up in a URL that gets logged. This is
//! Gemini's own protocol, not the OpenAI-compatible layer
//! [`crate::endpoint`] already speaks for chat, so it gets its own module
//! rather than reusing that one's helpers; chat itself is unchanged.
//!
//! `Model.name` comes back as `models/{id}` (e.g. `models/gemini-3.5-flash-lite`);
//! [`Model::id`] here has that prefix stripped, since the OpenAI-compatible
//! chat layer Gemini enhancement goes through takes the bare id, not the
//! `models/`-prefixed resource name. Plain data -- no filtering on
//! `supportedGenerationMethods` here: which models are worth offering is
//! `macalla-session`'s policy, not this crate's.

use std::collections::HashSet;
use std::time::Duration;

use macalla_core::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::endpoint::MAX_MODELS;

/// <https://ai.google.dev/api/models#method:-models.list>
pub const LISTING_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

const SOURCE: &str = "gemini";

/// Same reasoning as [`crate::endpoint`]'s `LIST_CONNECT`: a model list is a
/// small JSON reply with no generation involved. Not measured.
const LIST_CONNECT: Duration = Duration::from_secs(3);

/// Same reasoning as [`crate::endpoint`]'s `LIST_REQUEST`. Not measured.
const LIST_REQUEST: Duration = Duration::from_secs(5);

/// Same reasoning as [`crate::endpoint`]'s `LIST_BODY_LIMIT`: a model list
/// reply is small JSON, so refusing an oversize one early avoids reading an
/// unbounded body from a server that misbehaves. Checked on the *decoded*
/// body, by [`macalla_core::bounded_read::read_capped`].
const BODY_LIMIT: u64 = 1024 * 1024;

/// However many models a page names, capping keeps one page's parse work
/// bounded even against a server that names far more than expected. Same
/// value as [`crate::endpoint`]'s `MAX_MODELS`.
const MAX_MODELS_PER_PAGE: usize = 200;

/// Gemini pages at up to 1000 per `pageSize`
/// (per the provider's documentation); this many pages
/// comfortably covers Google's actual catalogue while still bounding the
/// loop against a server that always sets `nextPageToken`, however large its
/// claimed catalogue.
const MAX_PAGES: usize = 20;

/// One model from Gemini's native listing. Plain data -- no policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// [`Model.name`] with the `models/` prefix stripped -- see the module
    /// doc for why.
    pub id: String,
    pub display_name: String,
    pub input_token_limit: Option<u64>,
    pub supported_generation_methods: Vec<String>,
}

/// [`list_models`]'s result: the models, and how many entries were skipped
/// for carrying no usable `name`.
#[derive(Debug, Clone, PartialEq)]
pub struct Listing {
    pub models: Vec<Model>,
    pub skipped: usize,
}

/// Fetch and parse every page of Gemini's model listing. `key` goes in the
/// `x-goog-api-key` header, never the URL.
pub fn list_models(key: &str) -> Result<Listing> {
    list_models_at(LISTING_URL, key, &agent())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(LIST_CONNECT))
        .timeout_global(Some(LIST_REQUEST))
        // Follow no redirects: unlike Authorization, ureq does not strip
        // x-goog-api-key on a redirect (only Authorization, Cookie and
        // Content-Length -- ureq-proto 0.6.2's
        // src/client/redirect.rs:86-90), so a 3xx reply could otherwise
        // carry the key to whatever host Location names. https_only is not
        // set alongside this: ureq 3.4.1's src/run.rs:149 rejects any
        // non-https URL outright when it is, which would break every test
        // here that points this agent at a plain http://127.0.0.1 fake.
        .max_redirects(0)
        .build()
        .into()
}

fn error(message: String) -> Error {
    Error::Enhance {
        backend: SOURCE,
        message,
    }
}

/// [`list_models`], with the base URL and agent as parameters so a test can
/// point it at a 127.0.0.1 fake server with short timeouts instead of the
/// real API.
fn list_models_at(url: &str, key: &str, agent: &ureq::Agent) -> Result<Listing> {
    let mut models = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut skipped = 0usize;
    let mut page_token: Option<String> = None;
    let mut seen_tokens = HashSet::new();

    for _ in 0..MAX_PAGES {
        // A cursor already used means the server is repeating itself --
        // trusting it further would loop forever against one that always
        // claims another page, so this page is not fetched at all.
        if let Some(token) = &page_token {
            if !seen_tokens.insert(token.clone()) {
                break;
            }
        }

        let page_url = page_url(url, page_token.as_deref());
        let raw = get(agent, &page_url, key)
            .map_err(|e| error(format!("the request to {} failed: {e}", redacted(url))))?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| error("the reply was not valid JSON".into()))?;
        let entries = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| error("the reply had no model list in it".into()))?;

        for entry in entries.iter().take(MAX_MODELS_PER_PAGE) {
            match serde_json::from_value::<WireModel>(entry.clone()) {
                Ok(wire) => match Model::try_from(wire) {
                    // A repeated id -- from a repeated page, or a server
                    // that names one model twice -- keeps only the first
                    // copy.
                    Some(model) if seen_ids.insert(model.id.clone()) => models.push(model),
                    Some(_) => {}
                    None => skipped += 1,
                },
                Err(_) => skipped += 1,
            }
        }
        if models.len() > MAX_MODELS {
            models.truncate(MAX_MODELS);
        }
        if models.len() >= MAX_MODELS {
            break;
        }

        page_token = value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_string);
        if page_token.is_none() {
            break;
        }
    }

    Ok(Listing { models, skipped })
}

fn page_url(base: &str, page_token: Option<&str>) -> String {
    match page_token {
        Some(token) => format!("{base}?pageSize=1000&pageToken={}", percent_encode(token)),
        None => format!("{base}?pageSize=1000"),
    }
}

/// A minimal percent-encoder for a query-parameter value: Gemini's page
/// tokens are opaque strings, not documented as URL-safe, so anything
/// outside the unreserved set (RFC 3986) is escaped rather than trusted.
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

/// `url` with any userinfo removed, for an error message. Gemini's own URL
/// never carries any, but every provider in this crate redacts before
/// putting a URL in an error -- see [`crate::endpoint::redacted`], which this
/// mirrors rather than imports, since pulling in an OpenAI-compatible module
/// for one string helper would misstate why this module exists on its own.
fn redacted(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.split_once('/') {
        Some((authority, tail)) => {
            let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            format!("{scheme}://{host}/{tail}")
        }
        None => {
            let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
            format!("{scheme}://{host}")
        }
    }
}

/// A bounded `GET` with the key in `x-goog-api-key`, never the URL.
fn get(agent: &ureq::Agent, url: &str, key: &str) -> std::result::Result<String, String> {
    let mut response = agent
        .get(url)
        .header("x-goog-api-key", key)
        .call()
        .map_err(|e| e.to_string())?;
    // `.reader()` decodes gzip/brotli before read_capped ever sees a byte,
    // so the cap is on the decoded body -- see BODY_LIMIT's doc.
    let reader = response.body_mut().with_config().reader();
    macalla_core::bounded_read::read_capped(reader, BODY_LIMIT).map_err(|e| e.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireModel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

impl Model {
    /// `wire.name`, with the `models/` prefix stripped when present, kept
    /// as-is otherwise -- a future reply shaped differently should not be
    /// silently mangled.
    fn try_from(wire: WireModel) -> Option<Model> {
        let name = wire.name?;
        let id = name.strip_prefix("models/").unwrap_or(&name).to_string();
        Some(Model {
            id,
            display_name: wire.display_name,
            input_token_limit: wire.input_token_limit,
            supported_generation_methods: wire.supported_generation_methods,
        })
    }
}

/// Parse one page's reply, from the network or a saved snapshot alike. A
/// reply with no `models` array is a clear error, since that is the
/// documented envelope; an entry with no usable `name` is skipped and
/// counted, the same rule [`crate::openai::parse_listing`] uses.
pub fn parse_listing(raw: &str) -> Result<Listing> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| error("the reply was not valid JSON".into()))?;
    let entries = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| error("the reply had no model list in it".into()))?;

    let mut models = Vec::new();
    let mut skipped = 0usize;
    for entry in entries {
        match serde_json::from_value::<WireModel>(entry.clone()) {
            Ok(wire) => match Model::try_from(wire) {
                Some(model) => models.push(model),
                None => skipped += 1,
            },
            Err(_) => skipped += 1,
        }
    }
    Ok(Listing { models, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{serve, serve_once};

    fn page(models: Vec<Value>, next_page_token: Option<&str>) -> String {
        let mut body = serde_json::json!({"models": models});
        if let Some(token) = next_page_token {
            body["nextPageToken"] = Value::String(token.to_string());
        }
        body.to_string()
    }

    fn model_entry(id: &str) -> Value {
        serde_json::json!({
            "name": format!("models/{id}"),
            "baseModelId": id,
            "version": "3.5",
            "displayName": format!("Gemini {id}"),
            "description": "a model",
            "inputTokenLimit": 1_048_576,
            "outputTokenLimit": 65_536,
            "supportedGenerationMethods": ["generateContent"],
            "thinking": true,
        })
    }

    #[test]
    fn a_reply_is_parsed_with_the_models_prefix_stripped_and_an_entry_with_no_name_is_skipped() {
        let raw = page(
            vec![
                model_entry("gemini-3.5-flash-lite"),
                serde_json::json!({"displayName": "no name"}),
            ],
            None,
        );
        let listing = parse_listing(&raw).unwrap();
        assert_eq!(listing.skipped, 1);
        assert_eq!(listing.models.len(), 1);
        let model = &listing.models[0];
        assert_eq!(model.id, "gemini-3.5-flash-lite");
        assert_eq!(model.display_name, "Gemini gemini-3.5-flash-lite");
        assert_eq!(model.input_token_limit, Some(1_048_576));
        assert_eq!(model.supported_generation_methods, vec!["generateContent"]);
    }

    #[test]
    fn a_reply_with_no_models_array_is_an_error() {
        let error =
            parse_listing(&serde_json::json!({"nextPageToken": null}).to_string()).unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error}");
    }

    #[test]
    fn the_key_goes_in_the_header_never_the_request_line() {
        let (address, server) = serve_once(
            "200 OK",
            page(vec![model_entry("gemini-3.5-flash-lite")], None),
        );
        let listing = list_models_at(&address, "secret-key", &agent()).unwrap();
        assert_eq!(listing.models.len(), 1);
        let (head, _) = server.join().unwrap();
        assert!(head.contains("x-goog-api-key: secret-key"), "{head}");
        let request_line = head.lines().next().unwrap_or_default();
        assert!(!request_line.contains("secret-key"), "{request_line}");
    }

    #[test]
    fn two_pages_are_followed_via_pagetoken() {
        let (address, server) = serve(vec![
            (
                "200 OK",
                page(vec![model_entry("gemini-3.5-flash-lite")], Some("page-2")),
            ),
            (
                "200 OK",
                page(vec![model_entry("gemini-3.1-flash-lite")], None),
            ),
        ]);
        let listing = list_models_at(&address, "secret-key", &agent()).unwrap();
        assert_eq!(listing.models.len(), 2);
        assert_eq!(listing.models[0].id, "gemini-3.5-flash-lite");
        assert_eq!(listing.models[1].id, "gemini-3.1-flash-lite");
        let sent = server.join().unwrap();
        assert!(
            sent[0].0.starts_with("get /?pagesize=1000 "),
            "{}",
            sent[0].0
        );
        assert!(sent[1].0.contains("pagetoken=page-2"), "{}", sent[1].0);
    }

    #[test]
    fn a_401_is_an_error_whose_message_does_not_contain_the_key() {
        let (address, server) = serve_once("401 Unauthorized", "{}".to_string());
        let error = list_models_at(&address, "secret-key", &agent()).unwrap_err();
        assert!(!error.to_string().contains("secret-key"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_repeating_cursor_stops_paging_after_two_requests_with_one_copy_of_each_model() {
        let (address, server) = serve(vec![
            (
                "200 OK",
                page(
                    vec![model_entry("gemini-3.5-flash-lite")],
                    Some("same-token"),
                ),
            ),
            (
                "200 OK",
                page(
                    vec![model_entry("gemini-3.5-flash-lite")],
                    Some("same-token"),
                ),
            ),
        ]);
        let listing = list_models_at(&address, "secret-key", &agent()).unwrap();
        assert_eq!(listing.models.len(), 1);
        assert_eq!(listing.models[0].id, "gemini-3.5-flash-lite");
        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "a repeated cursor must not be followed a third time"
        );
    }

    #[test]
    fn the_total_is_capped_at_max_models_even_across_pages() {
        let page_a: Vec<Value> = (0..150)
            .map(|i| model_entry(&format!("gemini-a-{i}")))
            .collect();
        let page_b: Vec<Value> = (0..150)
            .map(|i| model_entry(&format!("gemini-b-{i}")))
            .collect();
        let (address, server) = serve(vec![
            ("200 OK", page(page_a, Some("page-2"))),
            ("200 OK", page(page_b, Some("page-3"))),
        ]);
        let listing = list_models_at(&address, "secret-key", &agent()).unwrap();
        assert_eq!(listing.models.len(), crate::endpoint::MAX_MODELS);
        let sent = server.join().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "paging must stop once the cap is reached, without a third request"
        );
    }

    #[test]
    fn a_redirect_is_not_followed_so_the_key_never_reaches_the_new_host() {
        let (second_address, watcher) = watch_for_no_request();
        let (address, redirecter) = redirect_to(&format!("{second_address}/models"));
        let error = list_models_at(&address, "secret-key", &agent()).unwrap_err();
        assert!(!error.to_string().contains("secret-key"), "{error}");
        redirecter.join().unwrap();
        assert!(
            !watcher.join().unwrap(),
            "the redirect target received a request"
        );
    }

    /// A fake that answers one request with a 302 to `location`.
    fn redirect_to(location: &str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let location = location.to_string();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            )
            .unwrap();
        });
        (address, handle)
    }

    /// A listener that reports whether anything connected to it within a
    /// short window, without ever answering -- so a caller can assert a
    /// redirect target was never reached.
    fn watch_for_no_request() -> (String, std::thread::JoinHandle<bool>) {
        use std::net::TcpListener;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                if listener.accept().is_ok() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        });
        (address, handle)
    }
}
