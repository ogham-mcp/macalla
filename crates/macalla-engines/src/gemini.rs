// SPDX-License-Identifier: MIT
//! Gemini 3.5 Transcribe: upload through the Files API, then transcribe by
//! URI through the Interactions API. Batch only, like [`crate::cloud`], and
//! for the same reason: the whole recording goes up in one
//! request when the key is released.
//!
//! Not the OpenAI-shaped multipart request [`crate::cloud`] sends: there is
//! no single-shot transcription endpoint for this model, so this module owns
//! its own three-request flow (upload start, upload, interact) and its own
//! auth header (`x-goog-api-key`, not `Authorization: Bearer`). It reuses
//! [`crate::cloud`]'s WAV encoder rather than duplicating it -- see
//! `encode_wav`'s doc comment there.
//!
//! Every fact this module encodes came from Google's own docs, read on
//! 2026-09-11:
//!
//! - The transcription guide -- the request shape: upload first, then
//!   `POST /v1beta/interactions` with the file's `uri`; `language_codes`
//!   takes full BCP-47 tags and an empty list means detect; `custom_vocabulary`
//!   up to 1,000 terms ("best results ... with up to 100"); `verbatim` is the
//!   default mode and is never overridden to `smart`, which changes the words.
//! - The Files API guide -- the resumable upload's headers and the
//!   `x-goog-upload-url` response header; files are deleted after use here
//!   rather than left for Google's 48-hour default retention.
//! - The model page for Gemini 3.5 Transcribe -- the 1,000-term vocabulary cap
//!   and the audio duration limits (unused directly here; the caller owns
//!   segmentation).
//! - Our own live runs also showed that `output_text`, which the
//!   transcription guide documents as the reply's transcript field, was
//!   absent from every real reply measured in the transcribe-summarize
//!   skill's live runs on 2026-09-06, and the text is in
//!   `steps[].content[].text` instead. [`reply_text`] reads that shape first
//!   and falls back to `output_text` only if it is ever populated.

use crate::cloud::encode_wav;
use macalla_core::audio::AudioSegment;
use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
use macalla_core::{Error, Result};
use std::time::Duration;

const ENGINE_ID: &str = "cloud-gemini";
const DEFAULT_MODEL: &str = "gemini-3.5-transcribe";
const API_ROOT: &str = "https://generativelanguage.googleapis.com";

/// Macalla's own cap, not Google's: `transcribe-guide.md`'s Custom vocabulary
/// section documents up to 1,000 terms but says "best results are typically
/// achieved with up to 100 terms", so that is what goes out.
const VOCAB_CAP: usize = 100;

/// Long enough for a slow connection to accept the request; short enough
/// that a provider that is unreachable gives up in a dictation, not a
/// coffee break. Not measured. Same figures as [`crate::cloud`].
const CONNECT: Duration = Duration::from_secs(10);
const WHOLE_REQUEST: Duration = Duration::from_secs(60);

/// A Files API upload acknowledgement (`{"file": {"uri": ..., "name": ...}}`)
/// or an error body (`{"error": {"code": ..., "message": ...}}`): both are
/// small, flat JSON documents with a handful of string fields, nothing
/// shaped like a transcript. Shared between the two rather than given a
/// constant each, since the reasoning -- "small JSON, generous margin" -- is
/// the same for both; the actual transcription reply is a different shape
/// and gets its own cap, [`crate::cloud::TRANSCRIPTION_REPLY_LIMIT`]. Not
/// measured, but several orders of magnitude over any real reply of this
/// shape. Checked on the *decoded* body, by
/// [`macalla_core::bounded_read::read_capped`] -- `ureq`'s own `.limit()`
/// sits beneath its gzip/brotli decoder and would only bound the compressed
/// wire bytes, not what the `String` this becomes actually holds.
const SMALL_JSON_REPLY_LIMIT: u64 = 64 * 1024;

/// Gemini 3.5 Transcribe over the Files and Interactions APIs. No `Debug`:
/// it holds the key.
pub struct GeminiEngine {
    model: Option<String>,
    key: String,
    agent: ureq::Agent,
    upload_start_url: String,
    interactions_url: String,
    api_root: String,
    /// Only ever set by [`Self::for_test`], to the fake server's own
    /// loopback origin. `new` and `with_model` never touch it, so
    /// [`Self::checked_upload_url`] runs the real googleapis.com rule
    /// ([`validate_upload_url`]) in every build but the test one.
    #[cfg(test)]
    trusted_upload_origin: Option<String>,
}

impl GeminiEngine {
    pub fn new(key: String) -> Self {
        Self {
            model: None,
            key,
            agent: build_agent(),
            upload_start_url: format!("{API_ROOT}/upload/v1beta/files"),
            interactions_url: format!("{API_ROOT}/v1beta/interactions"),
            api_root: API_ROOT.to_string(),
            #[cfg(test)]
            trusted_upload_origin: None,
        }
    }

    /// Choose the model explicitly. Defaults to `gemini-3.5-transcribe`.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[cfg(test)]
    fn for_test(key: String, base: &str) -> Self {
        Self {
            model: None,
            key,
            agent: build_agent(),
            upload_start_url: format!("{base}/upload/v1beta/files"),
            interactions_url: format!("{base}/v1beta/interactions"),
            api_root: base.to_string(),
            trusted_upload_origin: Some(base.to_string()),
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

    /// The upload URL the Files API handed back is the one value in this
    /// whole flow that comes from the network rather than from this file
    /// (verified fact 3), so it is the one place a redirect could send the
    /// audio, and the key, somewhere else. Refuses anything that is not
    /// `https` on a `googleapis.com` host with no userinfo -- except in a
    /// test build, where a loopback origin set by [`Self::for_test`] is
    /// trusted, and only that exact origin.
    fn checked_upload_url(&self, url: &str) -> Result<String> {
        #[cfg(test)]
        if let Some(origin) = &self.trusted_upload_origin {
            if url.starts_with(origin.as_str()) {
                return Ok(url.to_string());
            }
        }
        validate_upload_url(url)
            .map(|()| url.to_string())
            .map_err(|reason| self.error(reason))
    }

    /// Delete the uploaded file once it has done its job, whether the
    /// transcription that followed succeeded or failed (verified fact 8).
    /// A failed delete is logged and never fails the transcript: the file
    /// is still cleaned up by Google after 48 hours either way.
    fn delete_file(&self, name: &str) {
        let result = self
            .agent
            .delete(format!("{}/v1beta/{name}", self.api_root))
            .header("x-goog-api-key", &self.key)
            .call();
        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(mut resp) => {
                let status = resp.status();
                // `.reader()` decodes gzip/brotli before read_capped ever
                // sees a byte -- see SMALL_JSON_REPLY_LIMIT's doc.
                let reader = resp.body_mut().with_config().reader();
                let raw = macalla_core::bounded_read::read_capped(reader, SMALL_JSON_REPLY_LIMIT)
                    .unwrap_or_default();
                tracing::warn!(
                    "deleting the uploaded Gemini file failed: {}",
                    error_message(status.as_u16(), &raw)
                );
            }
            Err(e) => tracing::warn!("deleting the uploaded Gemini file failed: {e}"),
        }
    }

    fn interact(&self, model: &str, uri: &str, ctx: &SttContext) -> Result<String> {
        let body = interactions_body(model, uri, ctx).to_string();
        let mut resp = self
            .agent
            .post(&self.interactions_url)
            .header("x-goog-api-key", &self.key)
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(|e| self.error(format!("transcribing: {e}")))?;
        check_status(&mut resp, "transcribing").map_err(|reason| self.error(reason))?;
        // `.reader()` decodes gzip/brotli before read_capped ever sees a
        // byte -- see crate::cloud::TRANSCRIPTION_REPLY_LIMIT's doc.
        let reader = resp.body_mut().with_config().reader();
        let raw = macalla_core::bounded_read::read_capped(
            reader,
            crate::cloud::TRANSCRIPTION_REPLY_LIMIT,
        )
        .map_err(|e| self.error(format!("reading the transcription reply: {e}")))?;
        reply_text(&raw).ok_or_else(|| self.error("the reply had no text in it".into()))
    }
}

fn build_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT))
        .timeout_global(Some(WHOLE_REQUEST))
        // Read the body ourselves on a 4xx/5xx: Google's error message is in
        // it, and it is unreadable once ureq has turned the status into an
        // Err (verified fact 9).
        .http_status_as_error(false)
        // Follow no redirects. ureq drops Authorization and Cookie when it
        // follows one but keeps every other header, and the key rides in
        // x-goog-api-key, so a redirect would carry it to wherever Location
        // pointed. A 3xx is reported as a failed request instead.
        .max_redirects(0)
        .build()
        .into()
}

impl SttEngine for GeminiEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn caps(&self) -> EngineCaps {
        EngineCaps {
            streaming: false,
            // Sends the whole recording in one request; nothing
            // should cut it into pieces first.
            owns_segmentation: true,
            // custom_vocabulary (verified fact 6).
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
        let model = self.model().to_string();
        let wav = encode_wav(&audio.samples);

        // 1. Start the resumable upload (verified fact 2).
        let start_body =
            serde_json::json!({ "file": { "display_name": "macalla-dictation.wav" } }).to_string();
        let mut start_resp = self
            .agent
            .post(&self.upload_start_url)
            .header("x-goog-api-key", &self.key)
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header("X-Goog-Upload-Header-Content-Length", wav.len().to_string())
            .header("X-Goog-Upload-Header-Content-Type", "audio/wav")
            .header("Content-Type", "application/json")
            .send(start_body)
            .map_err(|e| self.error(format!("starting the upload: {e}")))?;
        check_status(&mut start_resp, "starting the upload")
            .map_err(|reason| self.error(reason))?;
        // Header names are compared case-insensitively: `http::HeaderMap`
        // does this for every lookup, which is what verified fact 2 asks for.
        let upload_url = start_resp
            .headers()
            .get("x-goog-upload-url")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| {
                self.error("the Files API did not return an upload URL; nothing was sent".into())
            })?;
        let upload_url = self.checked_upload_url(&upload_url)?;

        // 2. Upload the bytes. No API key on this leg (verified fact 3).
        let mut upload_resp = self
            .agent
            .post(&upload_url)
            .header("X-Goog-Upload-Offset", "0")
            .header("X-Goog-Upload-Command", "upload, finalize")
            .send(wav)
            .map_err(|e| self.error(format!("uploading the audio: {e}")))?;
        check_status(&mut upload_resp, "uploading the audio")
            .map_err(|reason| self.error(reason))?;
        // `.reader()` decodes gzip/brotli before read_capped ever sees a
        // byte -- see SMALL_JSON_REPLY_LIMIT's doc.
        let reader = upload_resp.body_mut().with_config().reader();
        let upload_raw = macalla_core::bounded_read::read_capped(reader, SMALL_JSON_REPLY_LIMIT)
            .map_err(|e| self.error(format!("reading the upload reply: {e}")))?;
        let file: serde_json::Value = serde_json::from_str(&upload_raw)
            .map_err(|e| self.error(format!("the upload reply was not JSON: {e}")))?;
        let file_uri = file
            .get("file")
            .and_then(|f| f.get("uri"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| self.error("the upload reply had no file.uri".into()))?;
        let file_name = file
            .get("file")
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| self.error("the upload reply had no file.name".into()))?;

        // 3. Transcribe by URI. The file is deleted whether this succeeds or
        // fails (verified fact 8).
        let result = self.interact(&model, &file_uri, ctx);
        self.delete_file(&file_name);
        Ok(Transcript {
            text: result?,
            is_final: true,
            start_ms: audio.start_ms,
            end_ms: audio.start_ms + audio.duration_ms(),
        })
    }
}

/// `generation_config.transcription_config` for one request.
fn transcription_config(ctx: &SttContext) -> serde_json::Value {
    let mut config = serde_json::json!({
        // The full BCP-47 tag, e.g. "en-GB" -- NOT trimmed to its primary
        // subtag the way crate::cloud trims for the OpenAI-shaped providers
        // (verified fact 5). [] (not omitting the field) means "detect the
        // language, and handle code-switching within one recording".
        "language_codes": ctx.language.as_deref().map(|l| vec![l]).unwrap_or_default(),
        // No `mode` block: verbatim is the default, and `smart` mode changes
        // the words -- disfluency removal, self-correction, reflowing into
        // paragraphs -- so it is never sent (verified fact 4).
    });
    let terms: Vec<&str> = ctx
        .terms
        .iter()
        .map(String::as_str)
        .filter(|term| !term.is_empty())
        // "best results are typically achieved with up to 100 terms"
        // (transcribe-guide.md, Custom vocabulary / Limitations) -- verified
        // fact 6. Omitted entirely when there are no terms, not sent empty.
        .take(VOCAB_CAP)
        .collect();
    if !terms.is_empty() {
        config["custom_vocabulary"] = serde_json::json!(terms);
    }
    config
}

fn interactions_body(model: &str, uri: &str, ctx: &SttContext) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "input": [{ "type": "audio", "uri": uri, "mime_type": "audio/wav" }],
        "generation_config": { "transcription_config": transcription_config(ctx) },
    })
}

/// The transcript text: `steps[].content[].text`, joined with a space
/// (verified fact 7, `README.md` above). `output_text`, which
/// `transcribe-guide.md` documents as the reply's top-level transcript
/// field, is tried only as a fallback -- it was absent from every real
/// reply measured on 2026-09-06.
fn reply_text(raw: &str) -> Option<String> {
    let reply: serde_json::Value = serde_json::from_str(raw).ok()?;
    let mut parts = Vec::new();
    for step in reply
        .get("steps")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        for block in step
            .get("content")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                let text = text.trim();
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    if !parts.is_empty() {
        return Some(parts.join(" "));
    }
    reply
        .get("output_text")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `{"error": {"code": ..., "message": ...}}` (verified fact 9), with a
/// plain fallback if the body is not that shape. Never includes the key: the
/// key is a request header, never part of the body this reads.
fn error_message(status: u16, raw: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_string));
    match message {
        Some(message) => format!("HTTP {status}: {message}"),
        None => format!("HTTP {status}: {raw}"),
    }
}

fn check_status(
    resp: &mut ureq::http::Response<ureq::Body>,
    doing: &str,
) -> std::result::Result<(), String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // `.reader()` decodes gzip/brotli before read_capped ever sees a byte --
    // see SMALL_JSON_REPLY_LIMIT's doc.
    let reader = resp.body_mut().with_config().reader();
    let raw =
        macalla_core::bounded_read::read_capped(reader, SMALL_JSON_REPLY_LIMIT).unwrap_or_default();
    Err(format!("{doing}: {}", error_message(status.as_u16(), &raw)))
}

/// The production rule for a Files API upload URL (verified fact 3): `https`,
/// a host that is `generativelanguage.googleapis.com` or ends in
/// `.googleapis.com`, and no userinfo. Hand-rolled rather than pulling in a
/// URL-parsing crate: `ureq` does not expose one, and the shape checked here
/// is a handful of characters, not a general parser's job.
fn validate_upload_url(url: &str) -> std::result::Result<(), String> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!(
            "upload URL {url} is not https; refusing to send audio"
        ));
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Err(format!(
            "upload URL {url} carries userinfo; refusing to send audio"
        ));
    }
    let host = authority.split(':').next().unwrap_or(authority);
    if host == "generativelanguage.googleapis.com" || host.ends_with(".googleapis.com") {
        Ok(())
    } else {
        Err(format!(
            "upload URL {url} points at {host}, not a googleapis.com host; refusing to send audio"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};

    struct Recorded {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    struct Canned {
        status: &'static str,
        headers: Vec<(&'static str, String)>,
        body: String,
    }

    fn ok(body: String) -> Canned {
        Canned {
            status: "200 OK",
            headers: vec![],
            body,
        }
    }

    fn read_request(
        reader: &mut BufReader<TcpStream>,
    ) -> (String, String, HashMap<String, String>, Vec<u8>) {
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut headers = HashMap::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                if name == "content-length" {
                    length = value.parse().unwrap_or(0);
                }
                headers.insert(name, value);
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (method, path, headers, body)
    }

    /// Bind a listener and hand back its address before any request shows
    /// up, so a test can bake that address into a canned reply (the upload
    /// start response needs to point `x-goog-upload-url` back at this same
    /// server).
    fn bind() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        (listener, address)
    }

    /// Serve exactly `canned.len()` requests in order, one canned reply
    /// each, and hand back what was actually sent.
    fn serve(listener: TcpListener, canned: Vec<Canned>) -> JoinHandle<Vec<Recorded>> {
        thread::spawn(move || {
            let mut recorded = Vec::new();
            for reply in canned {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (method, path, headers, body) = read_request(&mut reader);
                let mut out = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in &reply.headers {
                    out.push_str(&format!("{name}: {value}\r\n"));
                }
                out.push_str("Connection: close\r\n\r\n");
                out.push_str(&reply.body);
                stream.write_all(out.as_bytes()).unwrap();
                recorded.push(Recorded {
                    method,
                    path,
                    headers,
                    body,
                });
            }
            recorded
        })
    }

    fn segment() -> AudioSegment {
        // A quarter-second ramp: enough samples to check the WAV body isn't
        // empty without a fixture.
        let samples: Vec<f32> = (0..4_000)
            .map(|i| (i as f32 / 4_000.0) * 2.0 - 1.0)
            .collect();
        AudioSegment::new(samples, 250)
    }

    fn upload_start_reply(upload_url: &str) -> Canned {
        Canned {
            status: "200 OK",
            headers: vec![("x-goog-upload-url", upload_url.to_string())],
            body: "{}".to_string(),
        }
    }

    fn upload_reply(uri: &str, name: &str) -> Canned {
        ok(serde_json::json!({ "file": { "uri": uri, "name": name } }).to_string())
    }

    fn interactions_reply(text: &str) -> Canned {
        ok(serde_json::json!({
            "steps": [{ "content": [{ "text": text }] }]
        })
        .to_string())
    }

    fn delete_reply() -> Canned {
        ok("{}".to_string())
    }

    #[test]
    fn sends_upload_transcribe_and_delete_in_order_with_the_full_language_tag_and_no_mode_block() {
        let (listener, address) = bind();
        let upload_url = format!("{address}/upload-bytes");
        let canned = vec![
            upload_start_reply(&upload_url),
            upload_reply("files/abc123", "files/abc123"),
            interactions_reply("hello there"),
            delete_reply(),
        ];
        let server = serve(listener, canned);

        let e = GeminiEngine::for_test("gk-test".into(), &address);
        let ctx = SttContext {
            terms: vec![],
            language: Some("en-GB".into()),
        };
        let out = e.transcribe(&segment(), &ctx).unwrap();
        assert_eq!(out.text, "hello there");
        assert!(out.is_final);
        assert_eq!(out.start_ms, 250);

        let recorded = server.join().unwrap();
        assert_eq!(
            recorded.len(),
            4,
            "expected upload start, upload, interact, delete"
        );

        let start = &recorded[0];
        assert_eq!(start.method, "POST");
        assert_eq!(start.path, "/upload/v1beta/files");
        assert_eq!(start.headers.get("x-goog-api-key").unwrap(), "gk-test");
        assert_eq!(
            start.headers.get("x-goog-upload-protocol").unwrap(),
            "resumable"
        );
        assert_eq!(start.headers.get("x-goog-upload-command").unwrap(), "start");
        assert_eq!(
            start
                .headers
                .get("x-goog-upload-header-content-type")
                .unwrap(),
            "audio/wav"
        );
        assert!(start
            .headers
            .contains_key("x-goog-upload-header-content-length"));
        let start_body: serde_json::Value = serde_json::from_slice(&start.body).unwrap();
        assert_eq!(start_body["file"]["display_name"], "macalla-dictation.wav");

        let upload = &recorded[1];
        assert_eq!(upload.method, "POST");
        assert_eq!(upload.path, "/upload-bytes");
        assert_eq!(upload.headers.get("x-goog-upload-offset").unwrap(), "0");
        assert_eq!(
            upload.headers.get("x-goog-upload-command").unwrap(),
            "upload, finalize"
        );
        assert!(
            !upload.headers.contains_key("x-goog-api-key"),
            "no key on the upload leg (verified fact 3)"
        );
        assert!(
            upload.body.starts_with(b"RIFF"),
            "the uploaded bytes are a WAV"
        );

        let interact = &recorded[2];
        assert_eq!(interact.method, "POST");
        assert_eq!(interact.path, "/v1beta/interactions");
        assert_eq!(interact.headers.get("x-goog-api-key").unwrap(), "gk-test");
        let interact_body: serde_json::Value = serde_json::from_slice(&interact.body).unwrap();
        assert_eq!(interact_body["model"], "gemini-3.5-transcribe");
        assert_eq!(interact_body["input"][0]["type"], "audio");
        assert_eq!(interact_body["input"][0]["uri"], "files/abc123");
        assert_eq!(interact_body["input"][0]["mime_type"], "audio/wav");
        let config = &interact_body["generation_config"]["transcription_config"];
        assert_eq!(config["language_codes"], serde_json::json!(["en-GB"]));
        assert!(config.get("mode").is_none(), "no mode block should be sent");
        assert!(config.get("custom_vocabulary").is_none());

        let delete = &recorded[3];
        assert_eq!(delete.method, "DELETE");
        assert_eq!(delete.path, "/v1beta/files/abc123");
        assert_eq!(delete.headers.get("x-goog-api-key").unwrap(), "gk-test");
    }

    #[test]
    fn with_no_language_the_language_codes_field_is_an_empty_list() {
        let (listener, address) = bind();
        let upload_url = format!("{address}/upload-bytes");
        let canned = vec![
            upload_start_reply(&upload_url),
            upload_reply("files/x", "files/x"),
            interactions_reply("bonjour"),
            delete_reply(),
        ];
        let server = serve(listener, canned);
        let e = GeminiEngine::for_test("key".into(), &address);
        e.transcribe(&segment(), &SttContext::default()).unwrap();

        let recorded = server.join().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&recorded[2].body).unwrap();
        assert_eq!(
            body["generation_config"]["transcription_config"]["language_codes"],
            serde_json::json!([])
        );
    }

    #[test]
    fn custom_vocabulary_is_cut_at_100_terms_and_skips_empty_ones() {
        let (listener, address) = bind();
        let upload_url = format!("{address}/upload-bytes");
        let canned = vec![
            upload_start_reply(&upload_url),
            upload_reply("files/x", "files/x"),
            interactions_reply("ok"),
            delete_reply(),
        ];
        let server = serve(listener, canned);
        let e = GeminiEngine::for_test("key".into(), &address);
        let mut terms: Vec<String> = (0..105).map(|i| format!("term{i}")).collect();
        terms.push(String::new());
        let ctx = SttContext {
            terms,
            language: None,
        };
        e.transcribe(&segment(), &ctx).unwrap();

        let recorded = server.join().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&recorded[2].body).unwrap();
        let vocabulary = body["generation_config"]["transcription_config"]["custom_vocabulary"]
            .as_array()
            .unwrap();
        assert_eq!(vocabulary.len(), 100);
        assert_eq!(vocabulary[0], "term0");
        assert_eq!(vocabulary[99], "term99");
    }

    #[test]
    fn custom_vocabulary_is_omitted_entirely_when_there_are_no_terms() {
        let (listener, address) = bind();
        let upload_url = format!("{address}/upload-bytes");
        let canned = vec![
            upload_start_reply(&upload_url),
            upload_reply("files/x", "files/x"),
            interactions_reply("ok"),
            delete_reply(),
        ];
        let server = serve(listener, canned);
        let e = GeminiEngine::for_test("key".into(), &address);
        e.transcribe(&segment(), &SttContext::default()).unwrap();

        let recorded = server.join().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&recorded[2].body).unwrap();
        assert!(body["generation_config"]["transcription_config"]
            .get("custom_vocabulary")
            .is_none());
    }

    #[test]
    fn reply_text_falls_back_to_output_text_when_steps_carry_none() {
        let raw = serde_json::json!({ "output_text": "fallback text" }).to_string();
        assert_eq!(reply_text(&raw).as_deref(), Some("fallback text"));
    }

    #[test]
    fn reply_text_prefers_steps_content_over_output_text() {
        let raw = serde_json::json!({
            "output_text": "should not be used",
            "steps": [{ "content": [{ "text": "hello" }, { "text": "world" }] }],
        })
        .to_string();
        assert_eq!(reply_text(&raw).as_deref(), Some("hello world"));
    }

    #[test]
    fn a_reply_with_no_text_anywhere_is_none() {
        let raw = serde_json::json!({}).to_string();
        assert_eq!(reply_text(&raw), None);
    }

    #[test]
    fn an_oversize_transcription_reply_is_rejected_not_read_whole() {
        let (listener, address) = bind();
        let upload_url = format!("{address}/upload-bytes");
        let huge = "a".repeat(crate::cloud::TRANSCRIPTION_REPLY_LIMIT as usize + 1);
        let canned = vec![
            upload_start_reply(&upload_url),
            upload_reply("files/x", "files/x"),
            interactions_reply(&huge),
            delete_reply(),
        ];
        let server = serve(listener, canned);
        let e = GeminiEngine::for_test("key".into(), &address);
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
        let recorded = server.join().unwrap();
        assert_eq!(
            recorded.len(),
            4,
            "the file is still deleted after an oversize reply"
        );
    }

    #[test]
    fn a_reply_with_no_text_fails_the_transcription() {
        let (listener, address) = bind();
        let upload_url = format!("{address}/upload-bytes");
        let canned = vec![
            upload_start_reply(&upload_url),
            upload_reply("files/x", "files/x"),
            ok(serde_json::json!({}).to_string()),
            delete_reply(),
        ];
        let server = serve(listener, canned);
        let e = GeminiEngine::for_test("key".into(), &address);
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("no text"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn the_file_is_deleted_after_a_failed_transcription_and_googles_message_is_surfaced() {
        let (listener, address) = bind();
        let upload_url = format!("{address}/upload-bytes");
        let canned = vec![
            upload_start_reply(&upload_url),
            upload_reply("files/bad", "files/bad"),
            Canned {
                status: "400 Bad Request",
                headers: vec![],
                body: serde_json::json!({
                    "error": { "code": 400, "message": "custom_vocabulary is incompatible with word-level timestamps" }
                })
                .to_string(),
            },
            delete_reply(),
        ];
        let server = serve(listener, canned);
        let e = GeminiEngine::for_test("gk-super-secret".into(), &address);
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("custom_vocabulary is incompatible with word-level timestamps"),
            "{err}"
        );
        assert!(!err.to_string().contains("gk-super-secret"), "{err}");

        let recorded = server.join().unwrap();
        assert_eq!(
            recorded.len(),
            4,
            "the delete still happens after a failure"
        );
        assert_eq!(recorded[3].method, "DELETE");
        assert_eq!(recorded[3].path, "/v1beta/files/bad");
    }

    #[test]
    fn a_bad_upload_url_means_no_audio_is_sent_and_no_key_goes_to_that_host() {
        let (listener, address) = bind();
        // Points somewhere other than this fake server and not googleapis.com:
        // the engine must refuse to follow it.
        let canned = vec![upload_start_reply("http://evil.example.com/upload")];
        let server = serve(listener, canned);
        let e = GeminiEngine::for_test("gk-super-secret".into(), &address);
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(!err.to_string().contains("gk-super-secret"), "{err}");

        let recorded = server.join().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "only the upload-start request was ever sent"
        );
    }

    #[test]
    fn validate_upload_url_accepts_https_googleapis_hosts() {
        assert!(validate_upload_url("https://generativelanguage.googleapis.com/upload/x").is_ok());
        assert!(validate_upload_url("https://storage.googleapis.com/upload/x").is_ok());
    }

    #[test]
    fn validate_upload_url_refuses_http() {
        let err =
            validate_upload_url("http://generativelanguage.googleapis.com/upload/x").unwrap_err();
        assert!(err.contains("not https"), "{err}");
    }

    #[test]
    fn validate_upload_url_refuses_a_non_googleapis_host() {
        let err = validate_upload_url("https://evil.example.com/upload/x").unwrap_err();
        assert!(err.contains("not a googleapis.com host"), "{err}");
    }

    #[test]
    fn validate_upload_url_refuses_userinfo() {
        let err = validate_upload_url("https://attacker@generativelanguage.googleapis.com/x")
            .unwrap_err();
        assert!(err.contains("userinfo"), "{err}");
    }

    #[test]
    fn stream_is_unsupported() {
        let e = GeminiEngine::new("key".into());
        match e.stream(&SttContext::default()) {
            Err(Error::Unsupported { what, .. }) => assert!(what.contains("streaming")),
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("a cloud engine must not return a stream"),
        }
    }

    #[test]
    fn caps_report_batch_only_owned_segmentation_and_vocab_bias() {
        let caps = GeminiEngine::new("key".into()).caps();
        assert!(!caps.streaming);
        assert!(caps.owns_segmentation);
        assert!(caps.vocab_bias);
        assert!(caps.cpu_ok);
        assert!(caps.languages.is_empty());
    }

    #[test]
    fn id_is_cloud_gemini() {
        assert_eq!(GeminiEngine::new("key".into()).id(), "cloud-gemini");
    }
}

#[cfg(test)]
mod redirects {
    #[test]
    fn the_agent_follows_no_redirects_so_the_key_header_cannot_travel() {
        assert_eq!(super::build_agent().config().max_redirects(), 0);
    }
}
