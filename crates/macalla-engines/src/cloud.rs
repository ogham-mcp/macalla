// SPDX-License-Identifier: MIT
//! Cloud STT over an OpenAI-shaped multipart `/audio/transcriptions` endpoint:
//! OpenAI, Mistral, Cohere and OpenRouter today.
//!
//! Batch only. The whole recording is sent in one request when the key
//! is released, on a worker thread -- but that session wiring, and choosing a
//! key from the OS keychain, are not part of this module. [`CloudEngine`]
//! takes its key as a plain value and never reads the keychain itself
//! (`macalla-platform` does that); this crate only knows how to shape and
//! send the request once it has one.
//!
//! [`ProviderSpec`] is a small table, one row per provider, built only from
//! facts a fact-verifier confirmed against each provider's own documentation.
//! The caller picks the model with [`CloudEngine::with_model`];
//! [`ProviderSpec::default_model`] is only the fallback when it does not, and
//! a provider with neither (OpenRouter today) refuses cleanly rather than
//! guessing one.
//!
//! OpenAI's language and dictionary fields both depend on the chosen model,
//! not just the provider, so [`ProviderSpec::language_field`] and
//! [`ProviderSpec::dictionary`] are functions of the model rather than fixed
//! strings -- see [`OPENAI`] for the one provider where that matters today.
//!
//! Cohere's file-size limit and every provider's error-body JSON shape are
//! unconfirmed; neither blocks this module, since the size limit is not
//! enforced here and errors are surfaced as plain text, not parsed.

use macalla_core::audio::{AudioSegment, SAMPLE_RATE};
use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
use macalla_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How `SttContext::terms` are sent for a model that accepts a dictionary.
#[derive(Debug, Clone, Copy)]
pub struct Dictionary {
    /// The multipart field name, e.g. `keywords[]` or `context_bias`.
    pub field: &'static str,
    /// Terms beyond this are cut, never sent over it.
    pub limit: usize,
    /// A term containing any of these characters is skipped entirely (not
    /// truncated) rather than sent. Separate from the CR/LF stripping every
    /// multipart text value gets: this is a provider rule about the term
    /// itself, not form-safety.
    pub reject_chars: &'static [char],
}

/// One provider's row: base URL, default model, and how language and the
/// dictionary go out. Every field comes from a verified fact.
#[derive(Clone, Copy)]
pub struct ProviderSpec {
    /// Used as the engine id, e.g. `cloud-openai`.
    pub id: &'static str,
    pub url: &'static str,
    /// `None` when no default is documented; [`CloudEngine::transcribe`]
    /// refuses cleanly rather than guessing one unless the caller picked one
    /// with [`CloudEngine::with_model`].
    pub default_model: Option<&'static str>,
    /// The multipart field the language goes out on, for the given model.
    pub language_field: fn(model: &str) -> &'static str,
    pub language_required: bool,
    /// The dictionary field for the given model, or `None` if that model
    /// takes no dictionary at all.
    pub dictionary: fn(model: &str) -> Option<Dictionary>,
}

/// <https://developers.openai.com/api/docs/guides/speech-to-text>
///
/// `gpt-transcribe` takes `languages[]` (repeated) and `keywords[]`; the
/// other models (`gpt-4o-transcribe`, `gpt-4o-mini-transcribe`, `whisper-1`)
/// take the singular `language` and no dictionary field at all. The API
/// reference documents `keywords[]` with no count or length limit beyond
/// "must not contain `<`, `>`, a carriage return or a line feed" -- the
/// 100-term cap below is Macalla's own choice, not OpenAI's, made so a huge
/// dictionary can't be sent unbounded into a field nobody has measured a
/// real limit for.
pub const OPENAI: ProviderSpec = ProviderSpec {
    id: "cloud-openai",
    url: "https://api.openai.com/v1/audio/transcriptions",
    default_model: Some("gpt-transcribe"),
    language_field: openai_language_field,
    language_required: false,
    dictionary: openai_dictionary,
};

fn openai_language_field(model: &str) -> &'static str {
    if model == "gpt-transcribe" {
        "languages[]"
    } else {
        "language"
    }
}

fn openai_dictionary(model: &str) -> Option<Dictionary> {
    if model == "gpt-transcribe" {
        Some(Dictionary {
            field: "keywords[]",
            // Macalla's own cap; see the OPENAI doc comment.
            limit: 100,
            reject_chars: &['<', '>', '\r', '\n'],
        })
    } else {
        None
    }
}

/// <https://docs.mistral.ai/api/endpoint/audio/transcriptions>
///
/// `context_bias` is an array field; Mistral's own Python SDK
/// (github.com/mistralai/client-python,
/// `src/mistralai/client/utils/forms.py::serialize_multipart_form`) sends a
/// list field as repeated multipart values under the field's plain name, no
/// brackets. The capabilities page gives the 100-term cap.
pub const MISTRAL: ProviderSpec = ProviderSpec {
    id: "cloud-mistral",
    url: "https://api.mistral.ai/v1/audio/transcriptions",
    default_model: Some("voxtral-mini-latest"),
    language_field: |_model| "language",
    language_required: false,
    dictionary: |_model| {
        Some(Dictionary {
            field: "context_bias",
            limit: 100,
            reject_chars: &[],
        })
    },
};

/// <https://docs.cohere.com/reference/create-audio-transcription>
pub const COHERE: ProviderSpec = ProviderSpec {
    id: "cloud-cohere",
    url: "https://api.cohere.com/v2/audio/transcriptions",
    default_model: Some("cohere-transcribe-03-2026"),
    language_field: |_model| "language",
    language_required: true,
    // No vocabulary/biasing field is documented at all.
    dictionary: |_model| None,
};

/// <https://openrouter.ai/docs/guides/overview/multimodal/stt>
pub const OPENROUTER: ProviderSpec = ProviderSpec {
    id: "cloud-openrouter",
    url: "https://openrouter.ai/api/v1/audio/transcriptions",
    // OpenRouter documents no default model for this endpoint.
    default_model: None,
    language_field: |_model| "language",
    language_required: false,
    // Only `prompt`, and it is documented as accepted but ignored.
    dictionary: |_model| None,
};

/// Long enough for a slow connection to accept the request; short enough
/// that a provider that is unreachable gives up in a dictation, not a
/// coffee break. Not measured.
const CONNECT: Duration = Duration::from_secs(10);
/// A whole recording upload plus transcription. Not measured.
const WHOLE_REQUEST: Duration = Duration::from_secs(60);

/// Every provider here, [`crate::elevenlabs`] and [`crate::gemini`] send the
/// whole recording in one request and get back a transcript
/// wrapped in a small JSON envelope -- never a listing, never anything
/// meant to be large. Derived, not measured, from macalla-session's
/// `Config::max_duration`, a five-minute cap on the longest recording any
/// of these adapters is ever asked to transcribe: a fast speaker manages
/// perhaps 180 words a minute, and an English word averages around six
/// bytes with its trailing space, so five minutes of speech is roughly 900
/// words, about 5.4 KB of transcript text. 1 MiB is close to 200x that
/// estimate, so a real reply is never cut, while a reply anywhere near this
/// size could not be a transcript of a bounded recording. Checked on the
/// *decoded* body, by [`macalla_core::bounded_read::read_capped`] --
/// `ureq`'s own `.limit()` sits beneath its gzip/brotli decoder and would
/// only bound the compressed wire bytes, not what the `String` this becomes
/// actually holds.
pub(crate) const TRANSCRIPTION_REPLY_LIMIT: u64 = 1024 * 1024;

/// One provider, ready to transcribe. No `Debug`: it holds the key.
pub struct CloudEngine {
    provider: &'static ProviderSpec,
    model: Option<String>,
    key: String,
    agent: ureq::Agent,
}

impl CloudEngine {
    pub fn new(provider: &'static ProviderSpec, key: String) -> Self {
        Self {
            provider,
            model: None,
            key,
            agent: ureq::Agent::config_builder()
                .timeout_connect(Some(CONNECT))
                .timeout_global(Some(WHOLE_REQUEST))
                .build()
                .into(),
        }
    }

    /// Choose the model explicitly. Overrides `provider.default_model`; a
    /// provider with no default (OpenRouter today) needs this call, or
    /// [`Self::transcribe`] refuses rather than guessing one.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// The model to send: the caller's choice, falling back to the
    /// provider's default. `None` when neither is set.
    fn model(&self) -> Option<&str> {
        self.model.as_deref().or(self.provider.default_model)
    }

    fn error(&self, message: String) -> Error {
        Error::Stt {
            engine: self.provider.id,
            message,
        }
    }
}

impl SttEngine for CloudEngine {
    fn id(&self) -> &'static str {
        self.provider.id
    }

    fn caps(&self) -> EngineCaps {
        let vocab_bias = self
            .model()
            .is_some_and(|model| (self.provider.dictionary)(model).is_some());
        EngineCaps {
            streaming: false,
            // The provider cuts and endpoints the audio itself, and the session
            // sends it the whole recording in one request. Reporting false
            // would have the session cut a file at the pauses and send one
            // request per piece.
            owns_segmentation: true,
            vocab_bias,
            cpu_ok: true,
            languages: Vec::new(),
            // Not offered for a live meeting: a meeting borrows only the
            // on-device engine.
            concurrent_streams: false,
        }
    }

    fn stream(&self, _ctx: &SttContext) -> Result<Box<dyn SttStream>> {
        Err(Error::Unsupported {
            engine: self.provider.id,
            what: "streaming; the whole recording goes in one request",
        })
    }

    fn transcribe(&self, audio: &AudioSegment, ctx: &SttContext) -> Result<Transcript> {
        if self.provider.language_required && ctx.language.is_none() {
            return Err(self.error("requires a language and none was given".into()));
        }
        let model = self.model().ok_or_else(|| {
            self.error(
                "no model was chosen: this provider has no default, call \
                 CloudEngine::with_model to pick one"
                    .into(),
            )
        })?;

        let mut form = Multipart::new();
        form.text("model", model);
        if let Some(language) = ctx.language.as_deref() {
            // The context carries a BCP-47 tag such as en-GB; every provider
            // here documents an ISO-639-1 code such as en, so only the primary
            // subtag goes out.
            form.text(
                (self.provider.language_field)(model),
                &primary_subtag(language),
            );
        }
        if let Some(dictionary) = (self.provider.dictionary)(model) {
            let terms = ctx
                .terms
                .iter()
                .filter(|term| !term.contains(dictionary.reject_chars))
                .take(dictionary.limit);
            for term in terms {
                form.text(dictionary.field, term);
            }
        }
        form.file(
            "file",
            "audio.wav",
            "audio/wav",
            &encode_wav(&audio.samples),
        );
        let (content_type, body) = form.finish();

        let mut response = self
            .agent
            .post(self.provider.url)
            .header("Authorization", format!("Bearer {}", self.key))
            .header("Content-Type", &content_type)
            .send(body)
            .map_err(|e| self.error(format!("the request to {} failed: {e}", self.provider.url)))?;
        // `.reader()` decodes gzip/brotli before read_capped ever sees a
        // byte, so the cap below is on the decoded body -- see
        // TRANSCRIPTION_REPLY_LIMIT's doc.
        let reader = response.body_mut().with_config().reader();
        let raw = macalla_core::bounded_read::read_capped(reader, TRANSCRIPTION_REPLY_LIMIT)
            .map_err(|e| {
                self.error(format!("reading the reply from {}: {e}", self.provider.url))
            })?;

        let text = reply_text(&raw).ok_or_else(|| {
            self.error(format!(
                "the reply from {} had no text in it",
                self.provider.url
            ))
        })?;

        Ok(Transcript {
            text,
            is_final: true,
            start_ms: audio.start_ms,
            end_ms: audio.start_ms + audio.duration_ms(),
        })
    }
}

/// A BCP-47 tag's primary subtag, lowercased -- `en-GB` becomes `en`. Every
/// provider in this module, and ElevenLabs ([`crate::elevenlabs`]), documents
/// an ISO-639-1/639-3 code rather than a full BCP-47 tag.
///
/// `pub(crate)`: shared with [`crate::elevenlabs`] rather than duplicated.
pub(crate) fn primary_subtag(language: &str) -> String {
    language
        .split(['-', '_'])
        .next()
        .unwrap_or(language)
        .to_ascii_lowercase()
}

/// The reply's top-level `text` field: confirmed the same shape on every one
/// of the four providers.
fn reply_text(raw: &str) -> Option<String> {
    let reply: serde_json::Value = serde_json::from_str(raw).ok()?;
    reply
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// `samples` as 16-bit little-endian PCM, clamped into range. No audio
/// dependency: `macalla-engines` cannot reach `macalla-platform`'s reader
/// (by design), and this is the one direction -- encode, not decode -- that
/// crate does not need either.
///
/// `pub(crate)`: [`crate::elevenlabs`] sends this raw, with no WAV container,
/// as ElevenLabs' `pcm_s16le_16` format -- see that module's doc comment.
pub(crate) fn encode_pcm_s16le(samples: &[f32]) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        pcm.extend_from_slice(&((clamped * i16::MAX as f32) as i16).to_le_bytes());
    }
    pcm
}

/// `samples` as 16 kHz mono 16-bit PCM in a WAV container.
///
/// `pub(crate)`: [`crate::gemini`] uploads the same WAV shape to the Files
/// API and reuses this rather than duplicating it.
pub(crate) fn encode_wav(samples: &[f32]) -> Vec<u8> {
    let data = encode_pcm_s16le(samples);
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

/// A hand-built `multipart/form-data` body: the crate's `ureq` dependency has
/// the wire-format-matching `multipart` Cargo feature available, but it is
/// off everywhere in this workspace (it pulls in `mime_guess` and
/// `getrandom`), so this reproduces the same boundary and header layout by
/// hand rather than turning that feature on for one caller.
///
/// `pub(crate)`: [`crate::elevenlabs`] builds its own multipart body with it
/// rather than duplicating this type.
pub(crate) struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    pub(crate) fn new() -> Self {
        // build-less: a process-id boundary can't collide with another
        // request's text fields, and audio bytes landing on this exact
        // ASCII token are astronomically unlikely. Ceiling: this crate never
        // makes two cloud requests concurrently on one process today: if
        // that changes, or a per-request random boundary becomes cheap to
        // add, switch to one (getrandom is already pulled in transitively
        // by earshot, so no new dependency is needed).
        Self {
            boundary: format!("macalla-cloud-{}", std::process::id()),
            body: Vec::new(),
        }
    }

    fn boundary_line(&mut self) {
        if !self.body.is_empty() {
            self.body.extend_from_slice(b"\r\n");
        }
        self.body
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
    }

    /// A field value. Carriage returns and line feeds are stripped first --
    /// unstripped, either could be read as a boundary or header line and
    /// break the form.
    pub(crate) fn text(&mut self, field: &str, value: &str) {
        self.boundary_line();
        self.body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{field}\"\r\n\r\n").as_bytes(),
        );
        let sanitized: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
        self.body.extend_from_slice(sanitized.as_bytes());
    }

    pub(crate) fn file(&mut self, field: &str, filename: &str, mime: &str, bytes: &[u8]) {
        self.boundary_line();
        self.body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n\
                 Content-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        self.body.extend_from_slice(bytes);
    }

    pub(crate) fn finish(mut self) -> (String, Vec<u8>) {
        self.body
            .extend_from_slice(format!("\r\n--{}--\r\n", self.boundary).as_bytes());
        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

// --------------------------------------------- OpenRouter's model listings

/// OpenRouter's transcription listing: the same `/models` endpoint
/// [`crate::cloud`]'s multipart requests go to, filtered server-side to
/// `output_modalities=transcription`. Public, no key. No pricing
/// field: `pricing.prompt` comes back in mixed units across these models
/// (per the provider's documentation), so nothing here should be
/// shown as a price without checking the model's own page.
pub const TRANSCRIPTION_LISTING_URL: &str =
    "https://openrouter.ai/api/v1/models?output_modalities=transcription";

/// Same reasoning as [`macalla_enhancers::openrouter`]'s `CONNECT`: a public
/// HTTPS endpoint over the open internet, not a local network segment. Not
/// measured.
const LISTING_CONNECT: Duration = Duration::from_secs(5);

/// The transcription listing is a small slice of the same catalogue (21
/// models on 2026-09-12, 13 KB); 15 s keeps the same headroom the larger
/// default listing gets rather than inventing a separate, unmeasured number
/// for a smaller reply. Not measured.
const LISTING_REQUEST: Duration = Duration::from_secs(15);

/// Same reasoning and the same number as the default listing's cap: refusing
/// an unbounded reply from a misbehaving proxy matters more here than the
/// exact size of today's listing. Checked on the *decoded* body, by
/// [`macalla_core::bounded_read::read_capped`] -- `ureq`'s own `.limit()`
/// sits beneath its gzip/brotli decoder and would only bound the compressed
/// wire bytes, not what the `String` this becomes actually holds.
const LISTING_BODY_LIMIT: u64 = 4 * 1024 * 1024;

/// One model from OpenRouter's transcription listing. Plain data -- no
/// policy: which models are worth offering, Macalla's measured figures for
/// them, and the on-disk cache are `macalla-session::openrouter`'s job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptionModel {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
    pub expiration_date: Option<String>,
}

/// [`list_transcription_models`]'s result: the models, and how many entries
/// were skipped for carrying no usable `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptionListing {
    pub models: Vec<TranscriptionModel>,
    pub skipped: usize,
}

/// Fetch and parse the transcription listing. No key: this endpoint is
/// public.
pub fn list_transcription_models() -> Result<TranscriptionListing> {
    let raw = listing_get(TRANSCRIPTION_LISTING_URL, &listing_agent())?;
    parse_transcription_listing(&raw)
}

fn listing_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(LISTING_CONNECT))
        .timeout_global(Some(LISTING_REQUEST))
        .build()
        .into()
}

fn listing_error(message: String) -> Error {
    Error::Stt {
        engine: OPENROUTER.id,
        message,
    }
}

fn listing_get(url: &str, agent: &ureq::Agent) -> Result<String> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| listing_error(format!("the request to {url} failed: {e}")))?;
    // `.reader()` decodes gzip/brotli before read_capped ever sees a byte,
    // so the cap below is on the decoded body -- see LISTING_BODY_LIMIT's doc.
    let reader = response.body_mut().with_config().reader();
    macalla_core::bounded_read::read_capped(reader, LISTING_BODY_LIMIT)
        .map_err(|e| listing_error(format!("reading the reply from {url}: {e}")))
}

/// The wire shape: `architecture.{input,output}_modalities` nested,
/// everything else flat -- the same shape `macalla-enhancers`' text listing
/// parses, for the same reason (see that crate's `openrouter` module).
#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    architecture: WireArchitecture,
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

impl From<WireModel> for TranscriptionModel {
    fn from(w: WireModel) -> Self {
        TranscriptionModel {
            id: w.id,
            name: w.name,
            input_modalities: w.architecture.input_modalities,
            output_modalities: w.architecture.output_modalities,
            expiration_date: w.expiration_date,
        }
    }
}

/// Parse a transcription listing reply, from the network or a saved snapshot
/// alike. A reply with no `data` array is a clear error; an entry with no
/// usable `id` is skipped and counted rather than failing the whole listing.
pub fn parse_transcription_listing(raw: &str) -> Result<TranscriptionListing> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|_| listing_error("the reply was not valid JSON".into()))?;
    let data = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| listing_error("the reply had no model list in it".into()))?;

    let mut models = Vec::with_capacity(data.len());
    let mut skipped = 0usize;
    for entry in data {
        match serde_json::from_value::<WireModel>(entry.clone()) {
            Ok(wire) => models.push(TranscriptionModel::from(wire)),
            Err(_) => skipped += 1,
        }
    }
    Ok(TranscriptionListing { models, skipped })
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
        serde_json::json!({ "text": text }).to_string()
    }

    /// Points a copy of `provider`'s spec at the fake server: leaked for the
    /// engine's lifetime, which is fine -- tests are short-lived processes.
    fn engine(provider: &ProviderSpec, url: String, key: &str) -> CloudEngine {
        let spec = ProviderSpec {
            url: Box::leak(url.into_boxed_str()),
            ..*provider
        };
        let leaked: &'static ProviderSpec = Box::leak(Box::new(spec));
        CloudEngine::new(leaked, key.to_string())
    }

    fn segment() -> AudioSegment {
        // A quarter-second ramp: enough samples to check the WAV size and
        // its byte layout without a fixture.
        let samples: Vec<f32> = (0..4_000)
            .map(|i| (i as f32 / 4_000.0) * 2.0 - 1.0)
            .collect();
        AudioSegment::new(samples, 250)
    }

    fn body_text(sent: &[u8]) -> String {
        String::from_utf8_lossy(sent).into_owned()
    }

    #[test]
    fn openai_gpt_transcribe_sends_repeated_language_and_keywords_fields() {
        let (address, server) = serve_once("200 OK", reply("hello there"));
        let e = engine(&OPENAI, address, "sk-test");
        let ctx = SttContext {
            terms: vec!["Terragrunt".into(), "kubectl".into()],
            language: Some("en".into()),
        };
        let out = e.transcribe(&segment(), &ctx).unwrap();
        assert_eq!(out.text, "hello there");
        assert!(out.is_final);
        assert_eq!(out.start_ms, 250);

        let (head, sent) = server.join().unwrap();
        assert!(head.starts_with("post /"), "{head}");
        assert!(head.contains("authorization: bearer sk-test"), "{head}");
        let body = body_text(&sent);
        assert!(
            body.contains("name=\"model\"\r\n\r\ngpt-transcribe"),
            "{body}"
        );
        assert!(body.contains("name=\"languages[]\"\r\n\r\nen"), "{body}");
        assert_eq!(body.matches("name=\"keywords[]\"").count(), 2, "{body}");
        assert!(body.contains("Terragrunt"), "{body}");
        assert!(body.contains("kubectl"), "{body}");
        assert!(
            body.contains("name=\"file\"; filename=\"audio.wav\""),
            "{body}"
        );
        assert!(body.contains("Content-Type: audio/wav"), "{body}");
    }

    #[test]
    fn openai_whisper_model_sends_singular_language_and_no_keywords_field() {
        let (address, server) = serve_once("200 OK", reply("hello there"));
        let e = engine(&OPENAI, address, "sk-test").with_model("whisper-1");
        let ctx = SttContext {
            terms: vec!["Terragrunt".into()],
            language: Some("en".into()),
        };
        let out = e.transcribe(&segment(), &ctx).unwrap();
        assert_eq!(out.text, "hello there");

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(body.contains("name=\"model\"\r\n\r\nwhisper-1"), "{body}");
        assert!(body.contains("name=\"language\"\r\n\r\nen"), "{body}");
        assert!(!body.contains("name=\"languages[]\""), "{body}");
        assert!(!body.contains("name=\"keywords[]\""), "{body}");
    }

    #[test]
    fn openai_term_with_a_line_feed_is_skipped() {
        let (address, server) = serve_once("200 OK", reply("hello there"));
        let e = engine(&OPENAI, address, "sk-test");
        let ctx = SttContext {
            terms: vec!["bad\nterm".into(), "goodterm".into()],
            language: None,
        };
        e.transcribe(&segment(), &ctx).unwrap();

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert_eq!(body.matches("name=\"keywords[]\"").count(), 1, "{body}");
        assert!(body.contains("goodterm"), "{body}");
        assert!(!body.contains("bad\nterm"), "{body}");
        assert!(!body.contains("badterm"), "{body}");
    }

    #[test]
    fn mistral_omits_language_when_the_context_has_none() {
        let (address, server) = serve_once("200 OK", reply("bonjour"));
        let e = engine(&MISTRAL, address, "key");
        let out = e.transcribe(&segment(), &SttContext::default()).unwrap();
        assert_eq!(out.text, "bonjour");

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(
            body.contains("name=\"model\"\r\n\r\nvoxtral-mini-latest"),
            "{body}"
        );
        assert!(!body.contains("name=\"language\""), "{body}");
    }

    #[test]
    fn mistral_sends_context_bias_repeated_and_cut_at_100() {
        let (address, server) = serve_once("200 OK", reply("bonjour"));
        let e = engine(&MISTRAL, address, "key");
        let terms: Vec<String> = (0..105).map(|i| format!("term{i}")).collect();
        let ctx = SttContext {
            terms,
            language: None,
        };
        e.transcribe(&segment(), &ctx).unwrap();

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert_eq!(body.matches("name=\"context_bias\"").count(), 100, "{body}");
        assert!(body.contains("term0"), "{body}");
        assert!(body.contains("term99"), "{body}");
        assert!(!body.contains("term100"), "{body}");
    }

    #[test]
    fn mistral_strips_cr_and_lf_from_a_context_bias_term_rather_than_skipping_it() {
        // Mistral has no documented character-exclusion rule, so an
        // embedded CR/LF is stripped for form-safety (task 4), not skipped
        // the way an OpenAI keyword with the same characters would be.
        let (address, server) = serve_once("200 OK", reply("bonjour"));
        let e = engine(&MISTRAL, address, "key");
        let ctx = SttContext {
            terms: vec!["bad\r\nterm".into()],
            language: None,
        };
        e.transcribe(&segment(), &ctx).unwrap();

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(body.contains("badterm"), "{body}");
        assert!(!body.contains("bad\r\nterm"), "{body}");
    }

    #[test]
    fn cohere_without_a_language_fails_before_any_request_is_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let e = engine(&COHERE, address, "key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("requires a language"), "{err}");
        drop(listener);
    }

    #[test]
    fn cohere_with_a_language_sends_the_required_field() {
        let (address, server) = serve_once("200 OK", reply("hola"));
        let e = engine(&COHERE, address, "key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("es".into()),
        };
        let out = e.transcribe(&segment(), &ctx).unwrap();
        assert_eq!(out.text, "hola");
        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(
            body.contains("name=\"model\"\r\n\r\ncohere-transcribe-03-2026"),
            "{body}"
        );
        assert!(body.contains("name=\"language\"\r\n\r\nes"), "{body}");
    }

    #[test]
    fn a_bcp47_language_goes_out_as_its_primary_subtag() {
        let (address, server) = serve_once("200 OK", reply("hello"));
        let e = engine(&OPENAI, address, "key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("en-GB".into()),
        };
        e.transcribe(&segment(), &ctx).unwrap();
        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(
            body.contains("name=\"languages[]\"\r\n\r\nen\r\n"),
            "{body}"
        );
        assert!(!body.contains("en-GB"), "{body}");
    }

    #[test]
    fn openrouter_with_no_model_chosen_fails_before_any_request_is_sent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let e = engine(&OPENROUTER, address, "key");
        let err = e
            .transcribe(&segment(), &SttContext::default())
            .unwrap_err();
        assert!(err.to_string().contains("no model was chosen"), "{err}");
        drop(listener);
    }

    #[test]
    fn openrouter_with_a_chosen_model_sends_it() {
        let (address, server) = serve_once("200 OK", reply("ok"));
        let e = engine(&OPENROUTER, address, "key").with_model("some/model");
        let out = e.transcribe(&segment(), &SttContext::default()).unwrap();
        assert_eq!(out.text, "ok");

        let (_, sent) = server.join().unwrap();
        let body = body_text(&sent);
        assert!(body.contains("name=\"model\"\r\n\r\nsome/model"), "{body}");
    }

    #[test]
    fn stream_is_unsupported_for_every_provider() {
        for provider in [&OPENAI, &MISTRAL, &COHERE, &OPENROUTER] {
            let e = CloudEngine::new(provider, "key".into());
            match e.stream(&SttContext::default()) {
                Err(Error::Unsupported { what, .. }) => assert!(what.contains("streaming")),
                Err(other) => panic!("{}: wrong error: {other}", provider.id),
                Ok(_) => panic!("{}: a cloud engine must not return a stream", provider.id),
            }
        }
    }

    #[test]
    fn caps_report_batch_only_and_no_languages() {
        let e = CloudEngine::new(&OPENAI, "key".into());
        let caps = e.caps();
        assert!(!caps.streaming);
        // Takes the whole recording, so nothing cuts it into requests.
        assert!(caps.owns_segmentation);
        assert!(caps.languages.is_empty());
        assert!(cpu_ok_matches(&caps));
        // Never a live meeting's engine: that borrows the on-device one.
        assert!(!caps.concurrent_streams);
    }

    fn cpu_ok_matches(caps: &EngineCaps) -> bool {
        caps.cpu_ok
    }

    #[test]
    fn vocab_bias_is_true_for_openai_gpt_transcribe_and_mistral_false_otherwise() {
        // OpenAI's default model is gpt-transcribe, which takes keywords[].
        assert!(CloudEngine::new(&OPENAI, "key".into()).caps().vocab_bias);
        assert!(
            !CloudEngine::new(&OPENAI, "key".into())
                .with_model("whisper-1")
                .caps()
                .vocab_bias
        );
        assert!(CloudEngine::new(&MISTRAL, "key".into()).caps().vocab_bias);
        assert!(!CloudEngine::new(&COHERE, "key".into()).caps().vocab_bias);
        // No model chosen and no default: vocab_bias can't be true either.
        assert!(
            !CloudEngine::new(&OPENROUTER, "key".into())
                .caps()
                .vocab_bias
        );
        assert!(
            !CloudEngine::new(&OPENROUTER, "key".into())
                .with_model("some/model")
                .caps()
                .vocab_bias
        );
    }

    #[test]
    fn a_server_error_is_surfaced_without_the_key_in_the_message() {
        let (address, server) = serve_once("401 Unauthorized", "{}".into());
        let e = engine(&OPENAI, address, "sk-super-secret-key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("en".into()),
        };
        let err = e.transcribe(&segment(), &ctx).unwrap_err();
        assert!(!err.to_string().contains("sk-super-secret-key"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn a_reply_with_no_text_field_fails() {
        let (address, server) = serve_once("200 OK", serde_json::json!({}).to_string());
        let e = engine(&OPENAI, address, "key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("en".into()),
        };
        let err = e.transcribe(&segment(), &ctx).unwrap_err();
        assert!(err.to_string().contains("no text"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn an_oversize_transcription_reply_is_rejected_not_read_whole() {
        let huge = "a".repeat(TRANSCRIPTION_REPLY_LIMIT as usize + 1);
        let (address, server) = serve_once("200 OK", reply(&huge));
        let e = engine(&OPENAI, address, "key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("en".into()),
        };
        let err = e.transcribe(&segment(), &ctx).unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
        server.join().unwrap();
    }

    /// gzip an in-memory buffer -- the only way this test module builds a
    /// compressed body, since Macalla itself never sends one.
    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// Serves one request whose reply arrives gzip-encoded on the wire and
    /// decodes to `decompressed_body` -- `ureq`'s own gzip decoding (on by
    /// default) undoes this before `read_capped` ever sees a byte, which is
    /// exactly the case its cap has to hold against: a small compressed
    /// reply that inflates past the limit.
    fn serve_once_gzip(status: &str, decompressed_body: String) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_string();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut sent = vec![0; length];
            reader.read_exact(&mut sent).unwrap();

            let compressed = gzip(decompressed_body.as_bytes());
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                compressed.len()
            )
            .unwrap();
            stream.write_all(&compressed).unwrap();
        });
        (address, server)
    }

    #[test]
    fn a_gzip_encoded_oversize_reply_is_rejected_after_decoding_not_before() {
        // Highly compressible: this gzips to a few hundred bytes on the
        // wire, well under TRANSCRIPTION_REPLY_LIMIT, but decodes to well
        // over it -- the shape `ureq`'s own `.limit()` cannot catch, since
        // it sits beneath the gzip decoder (see the constant's doc).
        let huge = "a".repeat(TRANSCRIPTION_REPLY_LIMIT as usize + 1);
        let (address, server) = serve_once_gzip("200 OK", reply(&huge));
        let e = engine(&OPENAI, address, "key");
        let ctx = SttContext {
            terms: vec![],
            language: Some("en".into()),
        };
        let err = e.transcribe(&segment(), &ctx).unwrap_err();
        assert!(err.to_string().contains("more than"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn the_wav_body_has_the_right_size_and_a_pcm_header() {
        let wav = encode_wav(&[0.0, 0.5, -1.0, 1.5]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 4 * 2);
        // The clamp keeps the out-of-range samples inside i16.
        let last_two: [u8; 2] = wav[wav.len() - 2..].try_into().unwrap();
        assert_eq!(i16::from_le_bytes(last_two), i16::MAX);
    }

    // ------------------------------------------- OpenRouter model listings

    fn transcription_reply() -> String {
        serde_json::json!({
            "data": [
                {
                    "id": "openai/gpt-transcribe",
                    "name": "OpenAI: GPT Transcribe",
                    "architecture": {
                        "input_modalities": ["audio"],
                        "output_modalities": ["transcription"],
                    },
                    "expiration_date": null,
                },
                {"name": "no id here"},
            ],
        })
        .to_string()
    }

    #[test]
    fn a_transcription_reply_is_parsed_and_an_entry_with_no_id_is_skipped_and_counted() {
        let listing = parse_transcription_listing(&transcription_reply()).unwrap();
        assert_eq!(listing.skipped, 1);
        assert_eq!(listing.models.len(), 1);
        let model = &listing.models[0];
        assert_eq!(model.id, "openai/gpt-transcribe");
        assert_eq!(model.name, "OpenAI: GPT Transcribe");
        assert_eq!(model.input_modalities, vec!["audio"]);
        assert_eq!(model.output_modalities, vec!["transcription"]);
        assert_eq!(model.expiration_date, None);
    }

    #[test]
    fn a_transcription_reply_with_no_data_array_is_an_error() {
        let error = parse_transcription_listing(&serde_json::json!({"object": "list"}).to_string())
            .unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error}");
    }

    #[test]
    fn a_fake_server_transcription_reply_is_fetched_and_parsed() {
        let (address, server) = serve_once("200 OK", transcription_reply());
        let raw = listing_get(&address, &listing_agent()).unwrap();
        let listing = parse_transcription_listing(&raw).unwrap();
        assert_eq!(listing.models.len(), 1);
        let (head, _) = server.join().unwrap();
        assert!(head.starts_with("get / "), "{head}");
    }

    #[test]
    fn an_unreachable_listing_address_fails_bounded_and_names_the_address() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let started = std::time::Instant::now();
        let error = listing_get(&address, &listing_agent()).unwrap_err();
        assert!(
            started.elapsed() <= LISTING_CONNECT * 2,
            "took {:?}",
            started.elapsed()
        );
        assert!(error.to_string().contains(&address), "{error}");
    }

    /// The 2026-09-12 snapshot of the provider's listing:
    /// 21 transcription models, none skipped. Skipped with a message if the
    /// file is not present.
    #[test]
    fn the_saved_transcription_snapshot_parses_into_21_models() {
        let path = format!(
            "{}/../../fixtures/provider-snapshots/openrouter/models-transcription-2026-09-12.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let Ok(raw) = std::fs::read_to_string(&path) else {
            eprintln!("skipping: {path} is not present");
            return;
        };
        let listing = parse_transcription_listing(&raw).unwrap();
        assert_eq!(listing.skipped, 0);
        assert_eq!(listing.models.len(), 21);
    }
}
