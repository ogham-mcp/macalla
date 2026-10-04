// SPDX-License-Identifier: MIT
//! What every Tier 1 provider whose agent reads a refused request's body
//! (`.http_status_as_error(false)`: [`crate::endpoint::Endpoint`],
//! [`crate::anthropic::Anthropic`]) does with that body: read it capped,
//! parse it leniently in the provider's own shape, sanitise it, and put only
//! the identifying parts -- never the provider's own free-text message --
//! anywhere a person or a log outliving the call can see it.
//!
//! One parse function per provider ([`parse_openai_detail`],
//! [`parse_anthropic_detail`]) turns that provider's error envelope into the
//! same [`ProviderDetail`] shape; everything else here (reading, sanitising,
//! the info-level [`status_message`] and the debug-only
//! [`debug_provider_message`]) is shared. A provider whose own agent never
//! reads a refused body -- [`crate::openrouter`], [`crate::gemini`],
//! [`crate::openai`], [`crate::mistral`] each build one only for their own
//! public/keyed model listing, not for a chat request that reaches
//! Enhancement -- has no parse function here, because nothing would call it.

use serde_json::Value;

/// Anthropic's own Messages API (`docs.claude.com/en/api/overview`, no saved
/// copy of the error page -- see [`parse_anthropic_detail`]) and every
/// OpenAI-compatible server [`crate::endpoint::Endpoint`] talks to answer a
/// 4xx/5xx with a small JSON body; this is generous room for a longer
/// message while still refusing to read an unbounded body from a server that
/// sends something else on failure. Checked on the *decoded* body, same
/// reasoning as [`crate::endpoint`]'s `LIST_BODY_LIMIT`.
pub(crate) const ERROR_BODY_LIMIT: u64 = 16 * 1024;

/// How much of a provider's own text goes into a log line or
/// [`macalla_core::Error::Enhance`]. `message` is free text a provider
/// writes; `param` and `code` are normally short tokens, but nothing in any
/// of these shapes stops a server sending something longer, so all three are
/// held to the same cap.
const PROVIDER_DETAIL_LIMIT: usize = 300;

/// The provider's own error detail, when present, already sanitised
/// ([`sanitized`]). Never the request body, the prompt or the dictated text
/// -- those are never in a reply to begin with, so there is nothing here to
/// leak.
///
/// Not every provider's envelope has all three: Anthropic's has no `param`
/// (see [`parse_anthropic_detail`]); a missing field is left `None` rather
/// than guessed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ProviderDetail {
    pub message: Option<String>,
    pub param: Option<String>,
    pub code: Option<String>,
}

/// [`ProviderDetail`] read off a non-2xx response with the capped reader
/// ([`ERROR_BODY_LIMIT`]) and `parse`, one of the per-provider functions
/// below. A read failure (oversize, not UTF-8) or a body that doesn't parse
/// just yields [`ProviderDetail::default`]: the status code is still
/// reported by the caller either way.
pub(crate) fn provider_detail(
    response: &mut ureq::http::Response<ureq::Body>,
    parse: fn(&str) -> ProviderDetail,
) -> ProviderDetail {
    let reader = response.body_mut().with_config().reader();
    match macalla_core::bounded_read::read_capped(reader, ERROR_BODY_LIMIT) {
        Ok(raw) => parse(&raw),
        Err(_) => ProviderDetail::default(),
    }
}

/// OpenAI's error envelope (`{"error": {"message", "type", "param",
/// "code"}}`), the shape every OpenAI-compatible server [`crate::endpoint`]
/// talks to answers a 4xx/5xx with -- leniently: not JSON, no `error`
/// object, or a field of the wrong shape all just leave that field `None`
/// rather than failing the whole read. Used by [`crate::endpoint::Endpoint`]
/// for every OpenAI-compatible server it talks to, Ollama, a person's own
/// endpoint, or a cloud provider behind that layer -- including OpenRouter
/// and Gemini's own OpenAI-compatible chat endpoint, whose completion
/// requests go through [`crate::endpoint::Endpoint`] rather than through
/// `crate::openrouter` or `crate::gemini`, which each hold only their
/// provider's own model-listing request (see the module doc).
pub(crate) fn parse_openai_detail(raw: &str) -> ProviderDetail {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return ProviderDetail::default();
    };
    let error = value.get("error");
    let field = |name: &str| {
        error
            .and_then(|e| e.get(name))
            .and_then(Value::as_str)
            .map(sanitized)
    };
    ProviderDetail {
        message: field("message"),
        param: field("param"),
        code: field("code"),
    }
}

/// Anthropic's error envelope: `{"type": "error", "error": {"type",
/// "message"}}` -- no `param`, and no field literally named `code`, so
/// `error.type` (a short token, e.g. `invalid_request_error`) fills
/// [`ProviderDetail::code`] instead, matching what [`crate::anthropic`]'s own
/// test fixtures already send. This shape comes from the `bad_request`
/// helper in [`crate::anthropic`]'s tests, not from a fetched copy of
/// Anthropic's docs. Leniently: not JSON, no `error` object, or a field of
/// the wrong shape all just leave that field `None`.
pub(crate) fn parse_anthropic_detail(raw: &str) -> ProviderDetail {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return ProviderDetail::default();
    };
    let error = value.get("error");
    let field = |name: &str| {
        error
            .and_then(|e| e.get(name))
            .and_then(Value::as_str)
            .map(sanitized)
    };
    ProviderDetail {
        message: field("message"),
        param: None,
        code: field("type"),
    }
}

/// `s`, folded onto one line (a provider's message could in theory carry a
/// newline) and capped at [`PROVIDER_DETAIL_LIMIT`] characters, for anywhere
/// a provider's own text goes into a log line or an error a person may see.
fn sanitized(s: &str) -> String {
    let one_line: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if one_line.chars().count() > PROVIDER_DETAIL_LIMIT {
        one_line.chars().take(PROVIDER_DETAIL_LIMIT).collect()
    } else {
        one_line
    }
}

/// The provider's own error message, at debug level only. Some providers
/// quote the offending input in it, which here could be dictated text, so it
/// never goes into an info line or an [`macalla_core::Error::Enhance`]:
/// those can reach the app's error lines. `param` and `code` are identifiers
/// and are logged at info by the caller. `url` is taken already redacted --
/// this function does no redaction of its own, so every caller decides what
/// a person's address looks like once and reuses it for both this and
/// [`status_message`].
pub(crate) fn debug_provider_message(url: &str, model: &str, status: u16, detail: &ProviderDetail) {
    if let Some(message) = detail.message.as_deref() {
        tracing::debug!(
            status,
            url = %url,
            model = %model,
            // Named `provider_message`, not `message`: tracing uses the field
            // name "message" for this call's own string literal.
            provider_message = message,
            "the server's own error message"
        );
    }
}

/// The [`macalla_core::Error::Enhance`] message for a non-2xx reply: the
/// status and the model always, and `detail`'s `param`/`code` in parentheses
/// when the provider sent them. Never the provider's message (see
/// [`debug_provider_message`]). `url` is taken already redacted, same
/// reasoning as there. For example: "the request to <url> for model <model>
/// failed: http status 400 (param: temperature, code: unsupported_value)".
pub(crate) fn status_message(
    url: &str,
    model: &str,
    status: u16,
    detail: &ProviderDetail,
) -> String {
    let mut message =
        format!("the request to {url} for model {model} failed: http status {status}");
    let mut parens = Vec::new();
    if let Some(param) = &detail.param {
        parens.push(format!("param: {param}"));
    }
    if let Some(code) = &detail.code {
        parens.push(format!("code: {code}"));
    }
    if !parens.is_empty() {
        message.push_str(&format!(" ({})", parens.join(", ")));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_shaped_fields_are_read_and_sanitised() {
        let raw = serde_json::json!({
            "error": {
                "message": "line one\nline two",
                "type": "invalid_request_error",
                "param": "temperature",
                "code": "unsupported_value",
            }
        })
        .to_string();
        let detail = parse_openai_detail(&raw);
        assert_eq!(detail.message.as_deref(), Some("line one line two"));
        assert_eq!(detail.param.as_deref(), Some("temperature"));
        assert_eq!(detail.code.as_deref(), Some("unsupported_value"));
    }

    #[test]
    fn anthropic_shaped_fields_have_no_param_and_type_becomes_code() {
        let raw = serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "`temperature` does not support 0 with this model.",
            }
        })
        .to_string();
        let detail = parse_anthropic_detail(&raw);
        assert_eq!(
            detail.message.as_deref(),
            Some("`temperature` does not support 0 with this model.")
        );
        assert_eq!(detail.param, None);
        assert_eq!(detail.code.as_deref(), Some("invalid_request_error"));
    }

    #[test]
    fn non_json_bodies_parse_to_the_default_for_either_shape() {
        assert_eq!(parse_openai_detail("not json"), ProviderDetail::default());
        assert_eq!(
            parse_anthropic_detail("not json"),
            ProviderDetail::default()
        );
    }

    #[test]
    fn a_long_message_is_capped_and_folded_to_one_line() {
        let long = format!("line one\nline two\n{}", "x".repeat(500));
        let raw = serde_json::json!({"error": {"message": long}}).to_string();
        let detail = parse_openai_detail(&raw);
        let message = detail.message.unwrap();
        assert!(!message.contains('\n'), "{message}");
        assert!(message.len() < 500, "{message}");
    }

    #[test]
    fn status_message_never_carries_the_providers_own_message() {
        let detail = ProviderDetail {
            message: Some("do not leak this".into()),
            param: Some("temperature".into()),
            code: Some("unsupported_value".into()),
        };
        let message = status_message("https://api.example.com/v1", "model-x", 400, &detail);
        assert!(!message.contains("do not leak this"), "{message}");
        assert!(message.contains("400"));
        assert!(message.contains("model-x"));
        assert!(message.contains("temperature"));
        assert!(message.contains("unsupported_value"));
    }
}
