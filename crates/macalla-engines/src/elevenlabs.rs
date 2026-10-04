// SPDX-License-Identifier: MIT
//! ElevenLabs Scribe v2: batch speech-to-text over a single multipart
//! request. Batch only, like [`crate::cloud`] and [`crate::gemini`], and for
//! the same reason: the whole recording goes up in one request
//! when the key is released.
//!
//! Not the OpenAI-shaped request [`crate::cloud`] sends -- the field names,
//! the audio format and the auth header (`xi-api-key`, not
//! `Authorization: Bearer`) are all ElevenLabs' own -- but it reuses
//! [`crate::cloud`]'s [`crate::cloud::Multipart`] and
//! [`crate::cloud::primary_subtag`] rather than duplicating either.
//!
//! Every fact this module encodes came from ElevenLabs' own docs
//! (<https://elevenlabs.io/docs/api-reference/speech-to-text/convert>,
//! <https://elevenlabs.io/docs/overview/capabilities/speech-to-text>,
//! <https://elevenlabs.io/docs/eleven-api/guides/how-to/speech-to-text/batch/keyterm-prompting>,
//! <https://elevenlabs.io/docs/overview/models>):
//!
//! - `POST https://api.elevenlabs.io/v1/speech-to-text`, `multipart/form-data`,
//!   the key in the `xi-api-key` header. `model_id` is required; this module
//!   defaults to `scribe_v2`, the batch model (`scribe_v2_realtime` is the
//!   live one and out of scope here).
//! - The audio goes out as `file_format=pcm_s16le_16` -- raw 16-bit
//!   little-endian PCM at 16 kHz mono, no container -- because the docs say
//!   its latency is lower than an encoded file's, and
//!   `macalla_core::audio::SAMPLE_RATE` is already 16 kHz. `tag_audio_events`
//!   is sent `false` (the default is `true`, which puts sound tags like
//!   "(laughter)" in the text) and `timestamps_granularity=none` (the
//!   default is `word`): neither is used by this adapter, which only reads
//!   `text`.
//! - `language_code` is the context language's primary subtag
//!   ([`crate::cloud::primary_subtag`]), omitted when there is none or it is
//!   blank, so the model detects it.
//! - `keyterms`: one repeated text part per term. The docs cap it at 1,000
//!   and note more than 100 sets a 20-second minimum billable duration per
//!   request, so this module caps at 100 -- Macalla's own choice, made so a
//!   short dictation with a big dictionary is never billed as if it were 20
//!   seconds long. A term is skipped, not truncated, when it is empty after
//!   trimming, 50 or more characters, more than 5 whitespace-separated
//!   words, or contains any of `< > { } [ ] \`, which the docs say are not
//!   supported. A term goes out trimmed; a line break inside one is
//!   stripped, as every multipart value's is. No `keyterms` part goes out at
//!   all when none survive, since sending any adds a 20% surcharge.
//! - A 200 reply is JSON with a required `text`; a 200 without it, or a 202
//!   (webhook accepted -- this module never sets `webhook`), is an error.
//! - Error bodies are not documented beyond a 422's
//!   `{"detail": [{"msg": ...}, ...]}`; [`error_message`] also reads
//!   `detail` as a plain string or as an object with `message`, and falls
//!   back to the HTTP status when none of those match.

use crate::cloud::{encode_pcm_s16le, primary_subtag, Multipart};
use macalla_core::audio::{AudioSegment, SAMPLE_RATE};
use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
use macalla_core::{Error, Result};
use std::time::Duration;

const ENGINE_ID: &str = "cloud-elevenlabs";
const DEFAULT_MODEL: &str = "scribe_v2";
const API_URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";

/// Macalla's own cap, not ElevenLabs' documented 1,000: the docs say more
/// than 100 keyterms sets a 20-second minimum billable duration per request,
/// and a dictation is usually shorter than that.
const KEYTERM_CAP: usize = 100;

/// A keyterm longer than this many characters is skipped (documented limit).
const KEYTERM_MAX_CHARS: usize = 50;
/// A keyterm with more than this many whitespace-separated words is skipped
/// (documented limit).
const KEYTERM_MAX_WORDS: usize = 5;
/// Characters the docs say are not supported in a keyterm.
const KEYTERM_REJECT_CHARS: &[char] = &['<', '>', '{', '}', '[', ']', '\\'];

/// At least 100 ms of audio (documented minimum), i.e. at least this many
/// 16 kHz samples.
const MIN_SAMPLES: usize = SAMPLE_RATE as usize / 10;

/// Long enough for a slow connection to accept the request; short enough
/// that a provider that is unreachable gives up in a dictation, not a
/// coffee break. Not measured. Same figures as [`crate::cloud`] and
/// [`crate::gemini`].
const CONNECT: Duration = Duration::from_secs(10);
const WHOLE_REQUEST: Duration = Duration::from_secs(60);

/// ElevenLabs Scribe v2, over the single-shot `/v1/speech-to-text` endpoint.
/// No `Debug`: it holds the key.
pub struct ElevenLabsEngine {
    model: Option<String>,
    key: String,
    agent: ureq::Agent,
    url: String,
}

impl ElevenLabsEngine {
    pub fn new(key: String) -> Self {
        Self {
            model: None,
            key,
            agent: build_agent(),
            url: API_URL.to_string(),
        }
    }

    /// Choose the model explicitly. Defaults to `scribe_v2`.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[cfg(test)]
    fn for_test(key: String, base_url: &str) -> Self {
        Self {
            model: None,
            key,
            agent: build_agent(),
            url: base_url.to_string(),
        }
    }

    fn model(&self) -> &str {
        self.model.as_deref().unwrap_or(DEFAULT_MODEL)
    }

    fn error(&self, message: String) -> Error {
        Error::Stt {
            engine: ENGINE_ID,
            message,
        }
    }
}

fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT))
        .timeout_global(Some(WHOLE_REQUEST))
        // Read the body ourselves on a 4xx/5xx: ElevenLabs' error message is
        // in it, and it is unreadable once ureq has turned the status into
        // an Err.
        .http_status_as_error(false)
        // Follow no redirects. ureq-proto 0.6.2 strips only Authorization
        // and Cookie when it follows a redirect and keeps every other
        // header, and the key rides in xi-api-key, so a redirect would carry
        // it to wherever Location pointed. A 3xx is reported as a failed
        // request instead.
        .max_redirects(0)
        .build()
        .into()
}

/// A keyterm worth sending: not empty after trimming, under the character
/// and word caps, and free of the characters ElevenLabs does not support.
fn keyterm_allowed(term: &str) -> bool {
    let trimmed = term.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.chars().count() >= KEYTERM_MAX_CHARS {
        return false;
    }
    if trimmed.split_whitespace().count() > KEYTERM_MAX_WORDS {
        return false;
    }
    !trimmed.contains(KEYTERM_REJECT_CHARS)
}

impl SttEngine for ElevenLabsEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn caps(&self) -> EngineCaps {
        EngineCaps {
            streaming: false,
            // Sends the whole recording in one request; nothing
            // should cut it into pieces first.
            owns_segmentation: true,
            // keyterms.
            vocab_bias: true,
            cpu_ok: true,
            languages: Vec::new(),
            // Not offered for a live meeting: a meeting borrows only the
            // on-device engine.
            concurrent_streams: false,
        }
    }

    fn stream(&self, _ctx: &SttContext) -> Result<Box<dyn SttStream>> {
        Err(Error::Unsupported {
            engine: ENGINE_ID,
            what: "streaming; the whole recording goes in one request",
        })
    }

    fn transcribe(&self, audio: &AudioSegment, ctx: &SttContext) -> Result<Transcript> {
        if audio.samples.len() < MIN_SAMPLES {
            return Err(self.error(format!(
                "the recording is shorter than the 100 ms ElevenLabs requires ({} samples, need {MIN_SAMPLES})",
                audio.samples.len()
            )));
        }

        let mut form = Multipart::new();
        form.text("model_id", self.model());
        form.text("file_format", "pcm_s16le_16");
        form.text("tag_audio_events", "false");
        form.text("timestamps_granularity", "none");
        // A blank language would go out as `language_code=` rather than
        // being left out, and only leaving it out is documented to detect.
        if let Some(language) = ctx.language.as_deref().filter(|l| !l.trim().is_empty()) {
            form.text("language_code", &primary_subtag(language.trim()));
        }
        for term in ctx
            .terms
            .iter()
            .map(|t| t.trim())
            .filter(|t| keyterm_allowed(t))
            .take(KEYTERM_CAP)
        {
            form.text("keyterms", term);
        }
        form.file(
            "file",
            "audio.pcm",
            "application/octet-stream",
            &encode_pcm_s16le(&audio.samples),
        );
        let (content_type, body) = form.finish();

        let mut resp = self
            .agent
            .post(&self.url)
            .header("xi-api-key", &self.key)
            .header("Content-Type", &content_type)
            .send(body)
            .map_err(|e| self.error(format!("the request to {} failed: {e}", self.url)))?;

        let status = resp.status();
        // `.reader()` decodes gzip/brotli before read_capped ever sees a
        // byte, so the cap below is on the decoded body -- see
        // crate::cloud::TRANSCRIPTION_REPLY_LIMIT's doc.
        let reader = resp.body_mut().with_config().reader();
        let raw = macalla_core::bounded_read::read_capped(
            reader,
            crate::cloud::TRANSCRIPTION_REPLY_LIMIT,
        )
        .map_err(|e| self.error(format!("reading the reply from {}: {e}", self.url)))?;

        if !status.is_success() {
            return Err(self.error(error_message(status.as_u16(), &raw)));
        }
        if status.as_u16() == 202 {
            return Err(self.error(
                "the reply was 202 (webhook accepted), but no webhook was configured".into(),
            ));
        }

        let text = reply_text(&raw)
            .ok_or_else(|| self.error(format!("the reply from {} had no text in it", self.url)))?;

        Ok(Transcript {
            text,
            is_final: true,
            start_ms: audio.start_ms,
            end_ms: audio.start_ms + audio.duration_ms(),
        })
    }
}

/// The reply's top-level `text` field.
fn reply_text(raw: &str) -> Option<String> {
    let reply: serde_json::Value = serde_json::from_str(raw).ok()?;
    reply
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// ElevenLabs' error body is not fully documented beyond the 422 shape
/// (`{"detail": [{"msg": ...}, ...]}`); this also reads `detail` as a plain
/// string or as an object with `message`, and falls back to the raw body (or
/// just the status) when none of those match. Never includes the key: the
/// key is a request header, never part of the body this reads.
fn error_message(status: u16, raw: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return format!("HTTP {status}: {raw}");
    };
    let detail = value.get("detail");
    let message = match detail {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Object(_)) => detail
            .and_then(|d| d.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_string),
        Some(serde_json::Value::Array(items)) => items
            .first()
            .and_then(|item| item.get("msg"))
            .and_then(|m| m.as_str())
            .map(str::to_string),
        _ => None,
    };
    match message {
        Some(message) => format!("HTTP {status}: {message}"),
        None => format!("HTTP {status}: {raw}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    /// Answer one request on `listener` with `status` and `reply_body`, and
    /// hand back the request head (lower-cased) and its raw multipart body.
    fn answer(listener: &TcpListener, status: &str, reply_body: &str) -> (String, Vec<u8>) {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let (mut head, mut length) = (String::new(), 0);
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            head.push_str(&lower);
        }
        let mut sent = vec![0; length];
        reader.read_exact(&mut sent).unwrap();
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
            reply_body.len()
        )
        .unwrap();
        (head, sent)
    }

    fn serve_once(status: &str, reply_body: String) -> (String, JoinHandle<(String, Vec<u8>)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_string();
        let server = thread::spawn(move || answer(&listener, &status, &reply_body));
        (address, server)
    }

    fn reply(text: &str) -> String {
        serde_json::json!({
            "text": text,
            "language_code": "en",
            "language_probability": 0.99,
            "words": []
        })
        .to_string()
    }

    fn engine(url: String, key: &str) -> ElevenLabsEngine {
        ElevenLabsEngine::for_test(key.to_string(), &url)
    }

    /// A quarter-second ramp: well over the 100 ms minimum, and enough
    /// samples to check the PCM body's size and byte layout without a
    /// fixture.
    fn segment() -> AudioSegment {
        let samples: Vec<f32> = (0..4_000)
            .map(|i| (i as f32 / 4_000.0) * 2.0 - 1.0)
            .collect();
        AudioSegment::new(samples, 250)
    }

    fn body_text(sent: &[u8]) -> String {
        String::from_utf8_lossy(sent).into_owned()
    }

    #[test]
    fn every_part_goes_out_with_the_right_values_and_no_authorization_header() {
        let (address, server) = serve_once("200 OK", reply("hello there"));
        let e = engine(address, "xi-test-key");
        let ctx = SttContext {
            terms: vec![],
            language: None,
        };
        let out = e.transcribe(&segment(), &ctx).unwrap();
        assert_eq!(out.text, "hello there");
        assert!(out.is_final);
        assert_eq!(out.start_ms, 250);

        let (head, sent) = server.join().unwrap();
        assert!(head.starts_with("post /"), "{head}");
        assert!(head.contains("xi-api-key: xi-test-key"), "{head}");
        assert!(!head.contains("authorization:"), "{head}");
        let body = body_text(&sent);
        assert!(
            body.contains("name=\"model_id\"\r\n\r\nscribe_v2"),
            "{body}"
        );
        assert!(
            body.contains("name=\"file_format\"\r\n\r\npcm_s16le_16"),
            "{body}"
        );
        assert!(
            body.contains("name=\"tag_audio_events\"\r\n\r\nfalse"),
            "{body}"
        );
        assert!(
            body.contains("name=\"timestamps_granularity\"\r\n\r\nnone"),
            "{body}"
        );
        assert!(
            body.contains("name=\"file\"; filename=\"audio.pcm\""),
            "{body}"
        );
        assert!(
            body.contains("Content-Type: application/octet-stream"),
            "{body}"
        );
    }

    #[test]
    fn a_chosen_model_is_sent_instead_of_the_default() {
        let (address, server) = serve_once("200 OK", reply("ok"));
        let e = engine(address, "key").with_model("scribe_v2_experimental");
        e.transcribe(&segment(), &SttContext::default()).unwrap();
        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(
            body.contains("name=\"model_id\"\r\n\r\nscribe_v2_experimental"),
            "{body}"
        );
        assert!(!body.contains("scribe_v2\r\n"), "{body}");
    }

    #[test]
    fn language_code_is_the_primary_subtag_and_absent_without_a_language() {
        let (address, server) = serve_once("200 OK", reply("bonjour"));
        let e = engine(address, "key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("fr-FR".into()),
        };
        e.transcribe(&segment(), &ctx).unwrap();
        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(
            body.contains("name=\"language_code\"\r\n\r\nfr\r\n"),
            "{body}"
        );
        assert!(!body.contains("fr-FR"), "{body}");

        let (address, server) = serve_once("200 OK", reply("hi"));
        let e = engine(address, "key");
        e.transcribe(&segment(), &SttContext::default()).unwrap();
        let (_, sent) = server.join().unwrap();
        assert!(!body_text(&sent).contains("name=\"language_code\""));
    }

    #[test]
    fn keyterm_allowed_applies_the_documented_limits_at_their_edges() {
        assert!(keyterm_allowed("kubectl"));
        assert!(keyterm_allowed(&"a".repeat(49)));
        assert!(!keyterm_allowed(&"a".repeat(50)));
        // Characters, not bytes: 49 two-byte characters still pass.
        assert!(keyterm_allowed(&"é".repeat(49)));
        assert!(keyterm_allowed("one two three four five"));
        assert!(!keyterm_allowed("one two three four five six"));
        assert!(!keyterm_allowed("   "));
        for bad in ["a<b", "a>b", "a{b", "a}b", "a[b", "a]b", "a\\b"] {
            assert!(!keyterm_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn rejected_keyterms_are_skipped_and_the_rest_go_out_trimmed_in_order() {
        let (address, server) = serve_once("200 OK", reply("ok"));
        let e = engine(address, "key");
        // The rejected terms come first, so the filter has to run on them.
        let ctx = SttContext {
            terms: vec![
                "  ".into(),
                "Terragrunt".into(),
                "a".repeat(50),
                "one two three four five six".into(),
                "bad<term>".into(),
                "  kubectl  ".into(),
            ],
            language: None,
        };
        e.transcribe(&segment(), &ctx).unwrap();

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert_eq!(body.matches("name=\"keyterms\"").count(), 2, "{body}");
        let first = body
            .find("name=\"keyterms\"\r\n\r\nTerragrunt\r\n")
            .expect("Terragrunt sent");
        let second = body
            .find("name=\"keyterms\"\r\n\r\nkubectl\r\n")
            .expect("kubectl sent trimmed");
        assert!(first < second, "{body}");
        assert!(!body.contains("bad<term>"), "{body}");
        assert!(!body.contains(&"a".repeat(50)), "{body}");
    }

    #[test]
    fn the_cap_of_100_counts_only_terms_that_pass_the_filter() {
        let (address, server) = serve_once("200 OK", reply("ok"));
        let e = engine(address, "key");
        let mut terms: Vec<String> = vec!["bad<one>".into(), "  ".into()];
        terms.extend((0..105).map(|i| format!("term{i}")));
        let ctx = SttContext {
            terms,
            language: None,
        };
        e.transcribe(&segment(), &ctx).unwrap();

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert_eq!(body.matches("name=\"keyterms\"").count(), 100, "{body}");
        assert!(body.contains("\r\n\r\nterm99\r\n"), "{body}");
        assert!(!body.contains("\r\n\r\nterm100\r\n"), "{body}");
        let first = body.find("\r\n\r\nterm0\r\n").unwrap();
        let last = body.find("\r\n\r\nterm99\r\n").unwrap();
        assert!(first < last, "keyterms must go out in order");
    }

    #[test]
    fn a_line_break_inside_a_keyterm_is_stripped_not_rejected() {
        let (address, server) = serve_once("200 OK", reply("ok"));
        let e = engine(address, "key");
        let ctx = SttContext {
            terms: vec!["pg\r\nvector".into()],
            language: None,
        };
        e.transcribe(&segment(), &ctx).unwrap();
        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert_eq!(body.matches("name=\"keyterms\"").count(), 1, "{body}");
        assert!(
            body.contains("name=\"keyterms\"\r\n\r\npgvector\r\n"),
            "{body}"
        );
    }

    #[test]
    fn a_blank_language_is_left_out_so_elevenlabs_detects_it() {
        let (address, server) = serve_once("200 OK", reply("hi"));
        let e = engine(address, "key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("  ".into()),
        };
        e.transcribe(&segment(), &ctx).unwrap();
        let (_, sent) = server.join().unwrap();
        assert!(!body_text(&sent).contains("name=\"language_code\""));
    }

    #[test]
    fn no_keyterms_part_at_all_without_terms() {
        let (address, server) = serve_once("200 OK", reply("ok"));
        let e = engine(address, "key");
        e.transcribe(&segment(), &SttContext::default()).unwrap();
        let (_, sent) = server.join().unwrap();
        assert!(!body_text(&sent).contains("name=\"keyterms\""));
    }

    #[test]
    fn an_oversize_reply_is_rejected_not_read_whole() {
        let huge = "a".repeat(crate::cloud::TRANSCRIPTION_REPLY_LIMIT as usize + 1);
        let (address, server) = serve_once("200 OK", reply(&huge));
        let e = engine(address, "key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn a_200_without_text_fails() {
        let (address, server) = serve_once("200 OK", serde_json::json!({}).to_string());
        let e = engine(address, "key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("no text"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn a_202_fails() {
        let (address, server) = serve_once("202 Accepted", "{}".into());
        let e = engine(address, "key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("202"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn a_detail_string_error_is_surfaced_without_the_key() {
        let body = serde_json::json!({ "detail": "invalid model_id" }).to_string();
        let (address, server) = serve_once("400 Bad Request", body);
        let e = engine(address, "xi-super-secret-key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("invalid model_id"), "{err}");
        assert!(!err.to_string().contains("xi-super-secret-key"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn a_detail_object_error_is_surfaced_without_the_key() {
        let body = serde_json::json!({ "detail": { "message": "quota exceeded" } }).to_string();
        let (address, server) = serve_once("429 Too Many Requests", body);
        let e = engine(address, "xi-super-secret-key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("quota exceeded"), "{err}");
        assert!(!err.to_string().contains("xi-super-secret-key"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn a_detail_list_error_is_surfaced_without_the_key() {
        let body = serde_json::json!({
            "detail": [{ "loc": ["body", "model_id"], "msg": "field required", "type": "missing" }]
        })
        .to_string();
        let (address, server) = serve_once("422 Unprocessable Entity", body);
        let e = engine(address, "xi-super-secret-key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("field required"), "{err}");
        assert!(!err.to_string().contains("xi-super-secret-key"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn short_audio_fails_before_any_connection_is_made() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let e = engine(address, "key");
        // 1,000 samples at 16 kHz is 62.5 ms, under the 100 ms minimum.
        let short = AudioSegment::new(vec![0.0; 1_000], 0);
        let err = e.transcribe(&short, &SttContext::default()).unwrap_err();
        assert!(err.to_string().contains("100 ms"), "{err}");
        // A connection, had one been made, would be waiting to be accepted.
        match listener.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("a connection was made: {other:?}"),
        }
    }

    #[test]
    fn a_redirect_is_not_followed_and_no_key_reaches_the_target() {
        let (address, server) = serve_once("302 Found", String::new());
        let e = engine(address, "xi-super-secret-key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(!err.to_string().contains("xi-super-secret-key"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn stream_is_unsupported() {
        let e = ElevenLabsEngine::new("key".into());
        match e.stream(&SttContext::default()) {
            Err(Error::Unsupported { what, .. }) => assert!(what.contains("streaming")),
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("a cloud engine must not return a stream"),
        }
    }

    #[test]
    fn caps_are_batch_only_owned_segmentation_vocab_bias_and_cpu_ok() {
        let caps = ElevenLabsEngine::new("key".into()).caps();
        assert!(!caps.streaming);
        assert!(caps.owns_segmentation);
        assert!(caps.vocab_bias);
        assert!(caps.cpu_ok);
        assert!(caps.languages.is_empty());
    }

    #[test]
    fn id_is_cloud_elevenlabs() {
        assert_eq!(ElevenLabsEngine::new("key".into()).id(), "cloud-elevenlabs");
    }

    #[test]
    fn the_pcm_body_is_two_bytes_per_sample_little_endian() {
        let pcm = encode_pcm_s16le(&[0.0, 0.5, -1.0, 1.5]);
        assert_eq!(pcm.len(), 4 * 2);
        assert_eq!(i16::from_le_bytes([pcm[0], pcm[1]]), 0);
        let half = i16::from_le_bytes([pcm[2], pcm[3]]);
        assert_eq!(half, (0.5 * i16::MAX as f32) as i16);
        // Clamped: -1.0 and the out-of-range 1.5 both land at the extremes.
        assert_eq!(i16::from_le_bytes([pcm[4], pcm[5]]), -i16::MAX);
        assert_eq!(i16::from_le_bytes([pcm[6], pcm[7]]), i16::MAX);
    }
}

#[cfg(test)]
mod redirects {
    #[test]
    fn the_agent_follows_no_redirects_so_the_key_header_cannot_travel() {
        assert_eq!(super::build_agent().config().max_redirects(), 0);
    }
}
