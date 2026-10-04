// SPDX-License-Identifier: MIT
//! STT engine implementations, each behind a feature flag.
//!
//! The app must build and run with the default engine for its platform and
//! nothing else compiled in (the app is complete with the default local engine
//! for the platform).

// Always compiled, feature or not. The FFI inside it is gated; the capability
// queries are not, so the settings screen and the onboarding checklist can ask
// about an engine that is not compiled in and get "unavailable" rather than a
// build error.
pub mod apple;
pub mod models;
pub mod registry;
// Every platform listens with it, so it is not an engine feature.
pub mod vad;
// Always compiled so its test runs in every build; only the transcribe.cpp
// engines lock a native session with it.
mod session_lock;

#[cfg(feature = "engine-cloud")]
pub mod cloud;
#[cfg(feature = "engine-cloud")]
pub mod elevenlabs;
#[cfg(feature = "engine-cloud")]
pub mod gemini;
#[cfg(feature = "engine-transcribe")]
pub mod nemotron;
#[cfg(feature = "engine-transcribe")]
pub mod parakeet;
#[cfg(feature = "engine-transcribe")]
mod transcribe;
#[cfg(feature = "engine-transcribe")]
pub mod whisper;

/// Shared skip logic for tests that read a generated audio fixture.
///
/// `fixtures/audio/` is git-ignored (audio never enters git) and
/// `fixtures/generate.sh` only reproduces most of it, so a fresh checkout can
/// be missing a file even when a model is installed. Every engine test that
/// needs a fixture should skip the same way it already skips for a missing
/// model, rather than unwrap a decode that was never going to succeed.
// Gated like its callers: every fixture-audio test decodes through the Apple
// decoder and only builds on macOS, so elsewhere this module would be dead code.
#[cfg(all(test, target_os = "macos"))]
pub(crate) mod test_fixtures {
    use std::path::PathBuf;

    /// Path to `name` under `fixtures/audio/`, or `None` (with a message on
    /// stderr) if it is not present. Callers should return early on `None`,
    /// the same way they already do when `models::Store::installed` is
    /// `None`.
    pub(crate) fn audio(name: &str) -> Option<PathBuf> {
        let path =
            PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/audio")).join(name);
        if path.exists() {
            Some(path)
        } else {
            eprintln!(
                "skipped: {} is missing -- run ./fixtures/generate.sh",
                path.display()
            );
            None
        }
    }
}

/// The engine id that should be preferred on this platform when nothing has
/// been configured. macOS gets Apple SpeechTranscriber -- no model download,
/// no model RAM, native streaming.
pub const fn default_engine_id() -> &'static str {
    if cfg!(target_os = "macos") {
        "apple-speech"
    } else {
        "parakeet-tdt-v3"
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_platform_default_is_apple_on_macos_and_parakeet_elsewhere() {
        let id = super::default_engine_id();
        if cfg!(target_os = "macos") {
            assert_eq!(id, "apple-speech");
        } else {
            assert_eq!(id, "parakeet-tdt-v3");
        }
    }
}
