// SPDX-License-Identifier: MIT
//! Apple SpeechTranscriber (SpeechAnalyzer), the macOS default engine.
//!
//! Verified on the reference machine 2026-09-08 (docs/benchmarks.md, Spike A):
//! runs on-device, needs no model download for English or German, holds no
//! model RAM against the section 7 budget, and does **not** require Apple
//! Intelligence -- `isAvailable` is true either way.
//!
//! Three findings constrain the implementation:
//!
//! - It is **stream-first**: it does its own endpointing, so
//!   [`macalla_core::stt::EngineCaps::owns_segmentation`] is true and VAD gates
//!   rather than cuts.
//! - **30 locales are supported but only some are installed.** English and
//!   German are pre-installed on the reference machine; Spanish and Italian are
//!   supported but need `AssetInventory.assetInstallationRequest` first.
//!   Onboarding must surface the difference rather than offering a language
//!   that will fail.
//! - Locale assets are reserved against a cap of five
//!   (`AssetInventory.maximumReservedLocales`), so switching between more than
//!   five languages needs `release(reservedLocale:)` handling.
//!
//! The Swift half lives in `swift/MacallaApple.swift` and is compiled to a
//! static library by `build.rs`. Everything here is available on every
//! platform: off macOS, or without the `engine-apple` feature, the engine
//! reports itself unavailable rather than failing to build. The app is
//! complete with the default local engine for the platform, so it must ship
//! and work without any given engine compiled in.

pub const ENGINE_ID: &str = "apple-speech";

#[cfg(apple_speech)]
mod ffi {
    use std::ffi::{c_char, CStr};

    extern "C" {
        pub fn macalla_apple_is_available() -> i32;
        pub fn macalla_apple_stream_open(
            locale: *const c_char,
            error_out: *mut *mut c_char,
        ) -> *mut std::ffi::c_void;
        pub fn macalla_apple_stream_push(
            handle: *mut std::ffi::c_void,
            samples: *const f32,
            count: usize,
        ) -> *mut c_char;
        pub fn macalla_apple_stream_poll(handle: *mut std::ffi::c_void) -> *mut c_char;
        pub fn macalla_apple_stream_finish(handle: *mut std::ffi::c_void) -> *mut c_char;
        pub fn macalla_apple_stream_close(handle: *mut std::ffi::c_void);
        pub fn macalla_apple_decode_file(
            path: *const c_char,
            context: *mut std::ffi::c_void,
            on_chunk: extern "C" fn(*mut std::ffi::c_void, *const f32, usize, usize),
            error_out: *mut *mut c_char,
        ) -> bool;
        pub fn macalla_apple_supported_locales() -> *mut c_char;
        pub fn macalla_apple_installed_locales() -> *mut c_char;
        pub fn macalla_apple_string_free(p: *mut c_char);
    }

    /// Take ownership of a string Swift allocated, copy it, and free the
    /// original. Every string that crosses the boundary goes through here, so
    /// there is one place that can leak rather than several.
    ///
    /// # Safety
    /// `p` must be a pointer returned by the Swift side, or null.
    pub unsafe fn take_string(p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let owned = CStr::from_ptr(p).to_string_lossy().into_owned();
        macalla_apple_string_free(p);
        Some(owned)
    }

    /// Call a Swift function returning a JSON array of strings.
    pub fn string_list(f: unsafe extern "C" fn() -> *mut c_char) -> Vec<String> {
        // SAFETY: `f` is one of the declared entry points above, each of which
        // returns either null or a heap C string owned by the caller.
        let raw = unsafe { take_string(f()) };
        let Some(raw) = raw else {
            return Vec::new();
        };
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, payload = %raw, "unparsable locale list from the Apple engine");
            Vec::new()
        })
    }
}

/// Whether SpeechTranscriber can run here.
pub fn is_available() -> bool {
    #[cfg(apple_speech)]
    {
        // SAFETY: no arguments, returns a plain integer.
        unsafe { ffi::macalla_apple_is_available() == 1 }
    }
    #[cfg(not(apple_speech))]
    {
        false
    }
}

/// BCP-47 tags the engine supports at all.
pub fn supported_locales() -> Vec<String> {
    #[cfg(apple_speech)]
    {
        ffi::string_list(ffi::macalla_apple_supported_locales)
    }
    #[cfg(not(apple_speech))]
    {
        Vec::new()
    }
}

/// BCP-47 tags whose assets are installed and usable right now.
///
/// Always a subset of [`supported_locales`]. The difference is what onboarding
/// has to ask the user to download.
pub fn installed_locales() -> Vec<String> {
    #[cfg(apple_speech)]
    {
        ffi::string_list(ffi::macalla_apple_installed_locales)
    }
    #[cfg(not(apple_speech))]
    {
        Vec::new()
    }
}

/// One result as the Swift side writes it.
#[cfg(apple_speech)]
#[derive(serde::Deserialize)]
struct RawResult {
    text: String,
    is_final: bool,
    start_ms: i64,
    end_ms: i64,
    /// Present only when the collector caught an error mid-stream.
    #[serde(default)]
    error: Option<String>,
}

#[cfg(apple_speech)]
fn parse_results(raw: &str) -> Vec<macalla_core::stt::Transcript> {
    let parsed: Vec<RawResult> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "unparsable results from the Apple engine");
            return Vec::new();
        }
    };
    parsed
        .into_iter()
        .filter_map(|r| {
            if let Some(error) = r.error {
                // Surfaced in the log rather than swallowed, but it is not a
                // transcript and must never reach the paste.
                tracing::warn!(error = %error, "Apple engine reported an error mid-stream");
                return None;
            }
            Some(macalla_core::stt::Transcript {
                text: r.text,
                is_final: r.is_final,
                start_ms: r.start_ms.max(0) as u64,
                end_ms: r.end_ms.max(0) as u64,
            })
        })
        .collect()
}

/// An open dictation against SpeechAnalyzer.
#[cfg(apple_speech)]
struct AppleStream {
    handle: *mut std::ffi::c_void,
    finished: bool,
}

// SAFETY: the Swift `Session` guards its only mutable state with a lock, and
// every method here takes `&mut self`, so exactly one thread touches the handle
// at a time. Moving it between threads is what the pipeline needs -- the stream
// is opened on the pipeline thread and never on the audio callback.
#[cfg(apple_speech)]
unsafe impl Send for AppleStream {}

#[cfg(apple_speech)]
impl AppleStream {
    fn open(locale: &str) -> macalla_core::Result<Self> {
        let c_locale = std::ffi::CString::new(locale).map_err(|_| macalla_core::Error::Stt {
            engine: ENGINE_ID,
            message: format!("locale {locale:?} contains a null byte"),
        })?;
        let mut error: *mut std::ffi::c_char = std::ptr::null_mut();
        // SAFETY: `c_locale` outlives the call; `error` is a valid out pointer.
        let handle = unsafe { ffi::macalla_apple_stream_open(c_locale.as_ptr(), &mut error) };
        if handle.is_null() {
            // SAFETY: Swift wrote a heap string here, or left it null.
            let message = unsafe { ffi::take_string(error) }
                .unwrap_or_else(|| format!("could not open a stream for {locale}"));
            return Err(macalla_core::Error::Stt {
                engine: ENGINE_ID,
                message,
            });
        }
        Ok(Self {
            handle,
            finished: false,
        })
    }
}

#[cfg(apple_speech)]
impl macalla_core::stt::SttStream for AppleStream {
    fn push(&mut self, samples: &[f32]) -> macalla_core::Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        // Waits while 30 s of audio is already queued for the analyzer, so a
        // file is fed no faster than it is analysed; live audio never gets
        // that far ahead. See `Session.push` in the Swift.
        // SAFETY: the slice outlives the call, and Swift copies out of it.
        let err =
            unsafe { ffi::macalla_apple_stream_push(self.handle, samples.as_ptr(), samples.len()) };
        // SAFETY: null on success, else a heap string we own.
        match unsafe { ffi::take_string(err) } {
            None => Ok(()),
            Some(message) => Err(macalla_core::Error::Stt {
                engine: ENGINE_ID,
                message,
            }),
        }
    }

    fn poll(&mut self) -> macalla_core::Result<Vec<macalla_core::stt::Transcript>> {
        // SAFETY: the handle is live until `close`, which only Drop calls.
        let raw = unsafe { ffi::take_string(ffi::macalla_apple_stream_poll(self.handle)) };
        Ok(raw.map(|r| parse_results(&r)).unwrap_or_default())
    }

    fn finish(mut self: Box<Self>) -> macalla_core::Result<Vec<macalla_core::stt::Transcript>> {
        // SAFETY: as above. `finished` stops Drop finalising a second time.
        let raw = unsafe { ffi::take_string(ffi::macalla_apple_stream_finish(self.handle)) };
        self.finished = true;
        Ok(raw.map(|r| parse_results(&r)).unwrap_or_default())
    }
}

#[cfg(apple_speech)]
impl Drop for AppleStream {
    fn drop(&mut self) {
        if !self.finished {
            // A stream dropped without `finish` -- an abandoned dictation, or
            // Escape twice. Close the input so the analyzer stops rather than
            // waiting on a stream nobody will feed.
            // SAFETY: the handle is still live.
            let raw = unsafe { ffi::macalla_apple_stream_finish(self.handle) };
            // SAFETY: whatever came back is ours to free; the text is discarded.
            unsafe { ffi::macalla_apple_string_free(raw) };
        }
        // SAFETY: releases the retained Swift object. Called exactly once.
        unsafe { ffi::macalla_apple_stream_close(self.handle) };
    }
}

/// Decode an audio file to 16 kHz mono float, the one format the pipeline uses.
///
/// Uses the OS decoder, so aiff, wav, caf, mp3 and m4a all work with no
/// decoding crate and no ffmpeg. Off macOS this is unsupported until Parakeet
/// lands with a decoder of its own in Phase 2.
pub fn decode_file(path: &std::path::Path) -> macalla_core::Result<Vec<f32>> {
    #[cfg(apple_speech)]
    {
        let raw = path.to_string_lossy();
        let c_path = std::ffi::CString::new(raw.as_ref()).map_err(|_| {
            macalla_core::Error::Audio(format!("{} contains a null byte", path.display()))
        })?;
        /// Where the decoder's chunks land. Each brings the length the file is
        /// expected to decode to, so the buffer is sized once at the first
        /// chunk rather than grown by doubling.
        extern "C" fn append(
            context: *mut std::ffi::c_void,
            samples: *const f32,
            count: usize,
            expected: usize,
        ) {
            // SAFETY: `context` is the `Vec` below, which outlives the decode
            // and is not touched by anything else while it runs.
            let out = unsafe { &mut *context.cast::<Vec<f32>>() };
            if out.capacity() == 0 {
                out.reserve_exact(expected);
            }
            // SAFETY: Swift passes `count` initialised floats, valid for the call.
            out.extend_from_slice(unsafe { std::slice::from_raw_parts(samples, count) });
        }

        let mut out: Vec<f32> = Vec::new();
        let mut error: *mut std::ffi::c_char = std::ptr::null_mut();
        // SAFETY: `c_path` and `out` outlive the call, `append` has the
        // signature Swift calls, and the error pointer is valid.
        let decoded = unsafe {
            ffi::macalla_apple_decode_file(
                c_path.as_ptr(),
                std::ptr::from_mut(&mut out).cast(),
                append,
                &mut error,
            )
        };
        if !decoded {
            // SAFETY: Swift wrote a heap string here, or left it null.
            let message = unsafe { ffi::take_string(error) }
                .unwrap_or_else(|| format!("could not decode {}", path.display()));
            return Err(macalla_core::Error::Audio(message));
        }
        Ok(out)
    }
    #[cfg(not(apple_speech))]
    {
        Err(macalla_core::Error::Audio(format!(
            "cannot decode {} -- audio decoding needs the engine-apple feature on macOS",
            path.display()
        )))
    }
}

/// The macOS default engine.
///
/// Stream-first: SpeechAnalyzer does its own endpointing, so VAD gates the
/// stream rather than cutting it, and the batch method below is implemented by
/// running a stream to completion -- Apple offers no discrete-blob API, and
/// pretending otherwise would put the lie in the trait rather than here.
pub struct AppleEngine {
    locale: String,
}

impl AppleEngine {
    /// The engine for a BCP-47 locale, for example `en-US`.
    pub fn new(locale: &str) -> Self {
        Self {
            locale: locale.to_string(),
        }
    }

    pub fn locale(&self) -> &str {
        &self.locale
    }
}

impl macalla_core::stt::SttEngine for AppleEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn caps(&self) -> macalla_core::stt::EngineCaps {
        macalla_core::stt::EngineCaps {
            // The analyzer endpoints for itself, so VAD gates rather than cuts.
            streaming: true,
            owns_segmentation: true,
            // No vocabulary biasing on this API. SpeechAnalyzer accepts
            // contextual strings, but SpeechTranscriber ignores them: with the
            // dictionary set as context, every transcript came out unchanged
            // (docs/benchmarks.md). The dictionary's phonetic pass is what
            // corrects terms here, which is the whole argument for
            // the dictionary.
            vocab_bias: false,
            cpu_ok: true,
            // Installed, not supported. Offering a locale whose assets are
            // absent is offering a failure.
            languages: installed_locales(),
            // Each stream gets its own SpeechAnalyzer, so a second stream
            // waits on nothing. Measured at real-time pace: a second stream
            // did not change when a final arrived (docs/benchmarks.md,
            // "At real-time pace").
            concurrent_streams: true,
        }
    }

    fn stream(
        &self,
        ctx: &macalla_core::stt::SttContext,
    ) -> macalla_core::Result<Box<dyn macalla_core::stt::SttStream>> {
        let locale = ctx.language.as_deref().unwrap_or(&self.locale);
        #[cfg(apple_speech)]
        {
            Ok(Box::new(AppleStream::open(locale)?))
        }
        #[cfg(not(apple_speech))]
        {
            let _ = locale;
            Err(macalla_core::Error::Unsupported {
                engine: ENGINE_ID,
                what: "streaming without the engine-apple feature on macOS",
            })
        }
    }

    fn transcribe(
        &self,
        audio: &macalla_core::audio::AudioSegment,
        ctx: &macalla_core::stt::SttContext,
    ) -> macalla_core::Result<macalla_core::stt::Transcript> {
        // Apple has no discrete-blob API, so batch is a stream run to
        // completion. Honest here rather than a lie in the trait.
        let mut stream = self.stream(ctx)?;
        let mut all = Vec::new();
        for chunk in audio.samples.chunks(macalla_core::stt::STREAM_CHUNK) {
            stream.push(chunk)?;
            all.extend(stream.poll()?);
        }
        all.extend(stream.finish()?);
        Ok(macalla_core::stt::Transcript {
            text: macalla_core::stt::final_text(&all),
            is_final: true,
            start_ms: audio.start_ms,
            end_ms: audio.start_ms + audio.duration_ms(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use macalla_core::stt::SttEngine;

    /// Whether this machine can actually run a dictation.
    ///
    /// Having the macOS 26 SDK is not the same as having the speech assets. A
    /// CI runner compiles the Swift happily and then reports `isAvailable`
    /// false, an EMPTY `supportedLocales`, and no audio format the analyzer
    /// will accept. That is also a real user state -- a fresh Mac before any
    /// locale is downloaded -- so the tests below skip loudly rather than
    /// failing, and the engine's own error path is what covers it.
    #[cfg(apple_speech)]
    fn live_engine() -> bool {
        let ready = is_available() && !installed_locales().is_empty();
        if !ready {
            println!(
                "skipped: no usable speech assets here (available {}, installed {:?})",
                is_available(),
                installed_locales()
            );
        }
        ready
    }
    // Only the tests that drive a real session need these, and those are gated.
    #[cfg(apple_speech)]
    use macalla_core::{
        audio::{AudioSegment, SAMPLE_RATE},
        stt::{final_text, SttContext},
    };

    #[test]
    fn the_engine_is_stream_first_because_the_analyzer_endpoints_for_itself() {
        let caps = AppleEngine::new("en-US").caps();
        assert!(caps.streaming);
        assert!(
            caps.owns_segmentation,
            "VAD gates this stream, it does not cut it"
        );
        assert!(caps.cpu_ok, "on-device, no accelerator required");
    }

    #[test]
    fn the_advertised_languages_are_the_installed_ones_not_the_supported_ones() {
        // Offering a locale whose assets are absent is offering a failure.
        assert_eq!(
            AppleEngine::new("en-US").caps().languages,
            installed_locales()
        );
    }

    #[cfg(apple_speech)]
    #[test]
    fn a_session_opens_for_english_and_closes_without_audio() {
        if !live_engine() {
            return;
        }
        let engine = AppleEngine::new("en-US");
        let stream = engine
            .stream(&SttContext::default())
            .expect("open a stream");
        let trailing = stream.finish().expect("finish");
        assert!(
            final_text(&trailing).is_empty(),
            "nothing was said, so nothing was transcribed: {trailing:?}"
        );
    }

    #[cfg(apple_speech)]
    #[test]
    fn a_second_of_silence_transcribes_to_nothing() {
        // The whole FFI path -- open, push, poll, finish -- without needing a
        // microphone or a recording.
        if !live_engine() {
            return;
        }
        let engine = AppleEngine::new("en-US");
        let mut stream = engine
            .stream(&SttContext::default())
            .expect("open a stream");
        for _ in 0..10 {
            stream
                .push(&vec![0.0; SAMPLE_RATE as usize / 10])
                .expect("push");
            let _ = stream.poll().expect("poll");
        }
        let trailing = stream.finish().expect("finish");
        assert!(
            final_text(&trailing).trim().is_empty(),
            "silence produced {trailing:?}"
        );
    }

    #[cfg(apple_speech)]
    #[test]
    fn the_batch_path_works_too_even_though_apple_has_no_batch_api() {
        if !live_engine() {
            return;
        }
        let engine = AppleEngine::new("en-US");
        let quiet = AudioSegment::new(vec![0.0; SAMPLE_RATE as usize / 2], 0);
        let t = engine
            .transcribe(&quiet, &SttContext::default())
            .expect("transcribe");
        assert!(t.is_final, "a batch result is committed by definition");
    }

    #[cfg(apple_speech)]
    #[test]
    fn an_unusable_locale_reports_why_rather_than_panicking() {
        if !live_engine() {
            return;
        }
        let engine = AppleEngine::new("zz-ZZ");
        match engine.stream(&SttContext::default()) {
            Err(e) => {
                let message = e.to_string();
                assert!(
                    message.contains("zz-ZZ") || message.contains("apple-speech"),
                    "the error should name the engine or the locale: {message}"
                );
            }
            Ok(_) => panic!("a locale with no assets must not open a working stream"),
        }
    }

    #[cfg(apple_speech)]
    #[test]
    fn locales_come_back_as_bcp_47_not_as_apple_identifiers() {
        // `Locale.identifier` on macOS is "en_US" with an underscore.
        // Everything on the Rust side -- SttContext::language, the Tier 0
        // language tables, the settings store -- is BCP-47, which is "en-US".
        // Normalise once, here, or the mismatch leaks into all three.
        for tag in supported_locales().iter().chain(installed_locales().iter()) {
            assert!(
                !tag.contains('_'),
                "{tag} is an Apple locale identifier, not a BCP-47 tag"
            );
        }
        // The separator is the invariant and holds vacuously on a machine with
        // no locales. Only check for a specific tag where there are any.
        let supported = supported_locales();
        if !supported.is_empty() {
            assert!(
                supported.contains(&"en-US".to_string()),
                "expected en-US in {supported:?}"
            );
        }
    }

    #[test]
    fn the_engine_id_is_stable_because_settings_are_stored_against_it() {
        assert_eq!(ENGINE_ID, "apple-speech");
    }

    #[test]
    fn every_platform_can_ask_whether_the_engine_is_available() {
        // The point of this test is that it COMPILES everywhere. An engine that
        // cannot be asked about without being compiled in is an engine the
        // settings screen cannot describe.
        let available = is_available();
        if cfg!(apple_speech) {
            // Whatever the answer, asking must not panic or hang. What it
            // ANSWERS is a property of the machine: true where the speech
            // assets are installed, false on a fresh Mac or a CI runner.
            println!("apple-speech available here: {available}");
        } else {
            assert!(!available, "not compiled in, so not available");
        }
    }

    #[test]
    fn installed_locales_are_a_subset_of_supported_ones() {
        let supported = supported_locales();
        let installed = installed_locales();
        for tag in &installed {
            assert!(
                supported.contains(tag),
                "{tag} is installed but not in the supported list"
            );
        }
        // Deliberately no assertion that anything IS installed. A fresh Mac
        // has none, and neither does a CI runner. The subset property above is
        // the invariant; the counts belong to the machine.
        println!(
            "supported {}, installed {}",
            supported.len(),
            installed.len()
        );
    }

    #[cfg(apple_speech)]
    #[test]
    fn report_what_this_machine_actually_has() {
        // Not an assertion, a readout. `--nocapture` to see it.
        println!("available: {}", is_available());
        println!("supported: {:?}", supported_locales());
        println!("installed: {:?}", installed_locales());
    }

    #[cfg(apple_speech)]
    #[test]
    fn english_is_installed_so_the_default_engine_works_out_of_the_box() {
        if !live_engine() {
            return;
        }
        let installed = installed_locales();
        assert!(
            installed.iter().any(|l| l.starts_with("en")),
            "no English locale installed, got {installed:?}"
        );
    }

    /// Real speech through the bridge, checked against what the standalone
    /// Swift probe measured on 2026-09-08 (fixtures/transcripts/tech-terms.json).
    ///
    /// Ignored because the fixture audio is generated, never committed
    /// (fixtures/generate.sh), and because it shells out to ffmpeg to decode.
    ///
    /// ```text
    /// ./fixtures/generate.sh
    /// cargo test -p macalla-engines --features engine-apple -- \
    ///     --ignored --nocapture reproduces_the_measured
    /// ```
    #[cfg(apple_speech)]
    #[test]
    #[ignore = "needs generated fixture audio and ffmpeg"]
    fn reproduces_the_measured_reading_of_the_technical_terms_fixture() {
        let Some(fixture) = crate::test_fixtures::audio("tech-terms.aiff") else {
            return;
        };

        // Headerless 16 kHz mono float, the format the pipeline works in.
        let decoded = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(fixture)
            .args(["-f", "f32le", "-ar", "16000", "-ac", "1", "-"])
            .output()
            .expect("run ffmpeg");
        assert!(
            decoded.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        let (frames, _) = decoded.stdout.as_chunks::<4>();
        let samples: Vec<f32> = frames.iter().copied().map(f32::from_le_bytes).collect();
        println!(
            "fed {} samples ({:.1} s)",
            samples.len(),
            samples.len() as f32 / SAMPLE_RATE as f32
        );

        let engine = AppleEngine::new("en-US");
        let mut stream = engine
            .stream(&SttContext::default())
            .expect("open a stream");
        let mut all = Vec::new();
        // FED AT REAL-TIME PACE, on purpose. Dumping the whole clip in as fast
        // as it will go leaves the engine a backlog at the moment of release,
        // and the finalisation figure then measures the backlog rather than the
        // trailing segment -- 270 to 402 ms instead of the ~127 ms Spike C
        // measured. Live capture delivers a chunk every 100 ms, so the test
        // does too, and the number means the same thing as the one in
        // docs/benchmarks.md.
        let chunk_time = std::time::Duration::from_millis(100);
        for chunk in samples.chunks(macalla_core::stt::STREAM_CHUNK) {
            let sent = std::time::Instant::now();
            stream.push(chunk).expect("push");
            all.extend(stream.poll().expect("poll"));
            if let Some(left) = chunk_time.checked_sub(sent.elapsed()) {
                std::thread::sleep(left);
            }
        }
        // The clock the user feels: everything is fed, and the engine has to
        // finalise the trailing segment. Spike C measured 127 ms on this
        // machine with a standalone Swift binary; this is the same thing
        // through the FFI, so a big gap here would be the bridge's fault.
        let release = std::time::Instant::now();
        all.extend(stream.finish().expect("finish"));
        let finalise_ms = release.elapsed().as_secs_f64() * 1000.0;

        let text = final_text(&all);
        let partials = all.iter().filter(|t| !t.is_final).count();
        println!("finalise: {finalise_ms:.0} ms after the last sample");
        println!("partials: {partials}");
        println!("final   : {text}");

        assert!(!text.is_empty(), "the bridge produced no committed text");
        assert!(
            partials > 0,
            "no volatile results -- the overlay would have nothing to show"
        );

        // From the 2026-09-08 reading. These are what the engine gets RIGHT
        // unaided, and the bridge must not be quietly losing them.
        for term in ["Terraform", "AWS", "HCL"] {
            assert!(
                text.contains(term),
                "expected {term:?} in the transcript, got: {text}"
            );
        }
        // And the known miss, which is the dictionary's job, not the engine's.
        // If this ever starts passing, the fixture's ground truth is stale.
        if !text.to_lowercase().contains("cube control") {
            println!(
                "NOTE: 'cube control' no longer appears -- the measured reading \
                 in fixtures/transcripts/tech-terms.json may be out of date"
            );
        }
    }

    #[cfg(apple_speech)]
    #[test]
    fn decoding_a_missing_file_says_which_one_rather_than_panicking() {
        let err = decode_file(std::path::Path::new("/nonexistent/nope.aiff"))
            .expect_err("a missing file must be an error");
        assert!(err.to_string().contains("nope.aiff"), "got: {err}");
    }

    #[cfg(apple_speech)]
    #[test]
    #[ignore = "needs generated fixture audio"]
    fn decoding_gives_the_same_audio_ffmpeg_does() {
        let Some(fixture) = crate::test_fixtures::audio("tech-terms.aiff") else {
            return;
        };
        let ours = decode_file(&fixture).expect("decode");
        println!(
            "decoded {} samples ({:.2} s)",
            ours.len(),
            ours.len() as f32 / 16_000.0
        );
        // The ignored streaming test fed 180848 samples via ffmpeg. Resamplers
        // differ by a frame or two; anything more means a real disagreement.
        assert!(
            (ours.len() as i64 - 180_848).abs() < 2_000,
            "expected about 180848 samples, got {}",
            ours.len()
        );
    }

    /// The VAD against REAL speech, not tones.
    ///
    /// The unit tests in `macalla_core::vad` drive the policy with sine waves,
    /// which an energy detector finds trivially easy and which prove nothing
    /// about a person in a room. This is the check that matters, and it lives
    /// here because it needs the decoder.
    ///
    /// ```text
    /// cargo test -p macalla-engines --features engine-apple -- \
    ///     --ignored --nocapture vad_against_real
    /// ```
    #[cfg(apple_speech)]
    #[test]
    #[ignore = "needs generated fixture audio"]
    fn vad_against_real_speech_finds_the_utterances() {
        use macalla_core::vad::{energy::EnergyDetector, Vad, VadConfig};

        for name in [
            "tech-terms.aiff",
            "three-sentences.aiff",
            "sampletext-kevin-technical.mp3",
        ] {
            let path = std::path::PathBuf::from("../../fixtures/audio").join(name);
            if !path.exists() {
                println!("{name}: missing, run ./fixtures/generate.sh");
                continue;
            }
            let samples = decode_file(&path).expect("decode");
            let total_s = samples.len() as f32 / SAMPLE_RATE as f32;

            let mut vad = Vad::new(EnergyDetector::new(), VadConfig::default());
            let segments = vad.segment(&AudioSegment::new(samples, 0));
            let voiced: u64 = segments.iter().map(|s| s.duration_ms()).sum();

            println!(
                "{name:34} {total_s:5.1}s -> {} segments, {:.1}s voiced ({:.0}% of the file)",
                segments.len(),
                voiced as f32 / 1000.0,
                voiced as f32 / 10.0 / total_s
            );
            for (i, s) in segments.iter().enumerate() {
                println!("    {i}: {:>6} ms at {:>6} ms", s.duration_ms(), s.start_ms);
            }

            assert!(
                vad.heard_speech(),
                "{name} is speech and the VAD heard none"
            );
            assert!(!segments.is_empty(), "{name} produced no segments at all");

            // SYNTHETIC AUDIO CANNOT TEST SEGMENTATION. Measured 2026-09-08:
            // the `say`-generated fixtures come back as ONE segment at 100% of
            // the file, because they have no room tone and no real pauses --
            // which makes the VAD look useless. The same code on 66 seconds of
            // a person speaking found 9 segments at 90%, tracking the pauses
            // it is supposed to track. Only assert the segmentation property on
            // real speech; on synthetic audio it is a false negative.
            if name.starts_with("sampletext") {
                assert!(
                    segments.len() > 1,
                    "{name} is a minute of speech with pauses and produced one segment"
                );
                let ratio = voiced as f32 / 1000.0 / total_s;
                assert!(
                    (0.5..0.98).contains(&ratio),
                    "{name} is {:.0}% voiced, which is either deaf or not gating at all",
                    ratio * 100.0
                );
            }
            assert!(
                voiced as f32 / 1000.0 <= total_s + 0.5,
                "{name} claims more voiced audio than exists"
            );
        }
    }
}
