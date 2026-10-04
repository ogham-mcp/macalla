// SPDX-License-Identifier: MIT
//! Nemotron 3.5 ASR streaming 0.6B through transcribe.cpp.
//!
//! The streaming choice beside Parakeet, which stays the default off macOS:
//! text about a second after speech starts, at Parakeet's accuracy on the
//! reference fixtures (docs/benchmarks.md). It takes audio as it is captured,
//! and its session lives on a thread of its own (`crate::transcribe`).
//!
//! It streams but does not endpoint: there is one running text and no
//! segments, so [`EngineCaps::owns_segmentation`] is false. Live, that changes
//! nothing -- the record loop streams to any engine that streams, behind the
//! gate. For a file it means the VAD cuts the audio first and each piece is
//! streamed on its own, so no stream runs for an hour.
//!
//! It takes locales, not languages: `en-US` works and a bare `en` is refused.
//! A language setting is mapped to one of its catalogue entry's locales before
//! it reaches the model.
//!
//! Licence: OpenMDW-1.1; see MODELS.md and the catalogue entry.

use crate::models::NEMOTRON_STREAMING;
use crate::transcribe::{self, Worker};
use macalla_core::audio::AudioSegment;
use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
use macalla_core::{Error, Result};
use std::path::Path;

pub const ENGINE_ID: &str = NEMOTRON_STREAMING.engine;

pub struct NemotronEngine {
    worker: Worker,
    backend: String,
}

impl NemotronEngine {
    /// Load the model from a file in the store; see
    /// [`crate::transcribe::load`] for what a first load costs.
    pub fn load(path: &Path) -> Result<Self> {
        let loaded = transcribe::load(path, ENGINE_ID)?;
        let backend = loaded.model.backend().to_string();
        Ok(Self {
            worker: Worker::spawn(loaded, ENGINE_ID)?,
            backend,
        })
    }

    /// "MTL0" on Apple Silicon, "CPU" elsewhere. For the log and the card.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    fn options(ctx: &SttContext) -> Result<transcribe_cpp::RunOptions> {
        let language = match ctx.language.as_deref() {
            None => None,
            Some(tag) => Some(
                NEMOTRON_STREAMING
                    .language_for(tag)
                    .ok_or_else(|| Error::Stt {
                        engine: ENGINE_ID,
                        message: format!("it does not speak {tag}"),
                    })?
                    .to_string(),
            ),
        };
        Ok(transcribe_cpp::RunOptions {
            language,
            ..Default::default()
        })
    }
}

impl SttEngine for NemotronEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn caps(&self) -> EngineCaps {
        EngineCaps {
            streaming: true,
            owns_segmentation: false,
            // transcribe.cpp takes no vocabulary for this family; the
            // dictionary corrects terms afterwards, as it does for Parakeet.
            vocab_bias: false,
            cpu_ok: true,
            languages: NEMOTRON_STREAMING
                .languages
                .iter()
                .map(|l| l.to_string())
                .collect(),
            // Measured on the path a live meeting uses: it cuts this engine's
            // audio into pieces, as a file meeting does, and each piece holds
            // the `Worker` only while `transcribe` runs it. Paced at real time
            // through a live meeting, one shared instance finished 0.88 to
            // 1.96 s after the audio against 0.89 to 0.94 s for two, with the
            // same words, at a peak of 974 to 975 MiB against 1,791 to 1,792 MiB
            // (docs/benchmarks.md, "A live meeting on Nemotron, in pieces").
            // The 143,182 ms block in "At real-time pace" belongs to the
            // other path, `stream`, where the `Worker` serves one stream for
            // that stream's whole life; a live meeting does not use it.
            concurrent_streams: true,
        }
    }

    fn stream(&self, ctx: &SttContext) -> Result<Box<dyn SttStream>> {
        self.worker.stream(Self::options(ctx)?)
    }

    fn transcribe(&self, audio: &AudioSegment, ctx: &SttContext) -> Result<Transcript> {
        let text = self.worker.run(audio, Self::options(ctx)?)?;
        Ok(Transcript {
            text,
            is_final: true,
            start_ms: audio.start_ms,
            end_ms: audio.start_ms + audio.duration_ms(),
        })
    }
}

// The only test here needs the model and the Apple decoder, so the module is
// macOS-only; on Linux an empty module left `use super::*` unused, and
// clippy's -D warnings failed the Linux job.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Needs the model in the store: `MACALLA_MODELS_DIR` pointing at a
    /// directory holding the Q8_0 file. Skips itself otherwise. macOS only
    /// until the fixture decoder leaves the Apple module.
    #[test]
    fn streams_the_control_fixture_when_the_model_is_here() {
        let store = crate::models::Store::shared();
        let Some(path) = store.installed(&NEMOTRON_STREAMING) else {
            eprintln!("skipped: no Nemotron model in {}", store.dir().display());
            return;
        };
        let Some(wav) = crate::test_fixtures::audio("control-harvard.aiff") else {
            return;
        };
        let engine = NemotronEngine::load(&path).unwrap();
        let samples = crate::apple::decode_file(&wav).unwrap();
        let ctx = SttContext {
            language: Some("en".into()),
            ..Default::default()
        };

        // A tenth of real time: slow enough for text to come back while the
        // audio is still going in, which is the point of a streaming engine.
        let mut stream = engine.stream(&ctx).unwrap();
        let mut partials = Vec::new();
        for chunk in samples.chunks(macalla_core::stt::STREAM_CHUNK) {
            stream.push(chunk).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            partials.extend(stream.poll().unwrap());
        }
        let finals = stream.finish().unwrap();
        assert!(!partials.is_empty(), "no text before the end");
        assert!(partials.iter().all(|t| !t.is_final));
        assert_eq!(finals.len(), 1, "{finals:?}");
        assert!(finals[0].is_final);
        let text = finals[0].text.to_lowercase();
        assert!(text.contains("birch canoe"), "{text}");
        assert!(text.contains("stockings"), "{text}");

        // The stream reset the session behind it, so the next call works.
        let again = engine
            .transcribe(&AudioSegment::new(samples, 0), &ctx)
            .unwrap();
        assert!(
            again.text.to_lowercase().contains("birch canoe"),
            "{}",
            again.text
        );
    }
}
