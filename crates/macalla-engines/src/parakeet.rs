// SPDX-License-Identifier: MIT
//! Parakeet TDT 0.6B v3 through transcribe.cpp.
//!
//! The default engine off macOS and the macOS fallback: the engine that
//! unlocks Linux and Windows. Batch, not streaming -- the model is offline by
//! construction, so the core's segmenter cuts the live stream into utterances
//! and each one lands here whole (`macalla_core::stt::BatchStream`). Its
//! languages and card copy live in its catalogue entry.
//!
//! Licence: CC-BY-4.0. **Attribution is required** in the About screen and in
//! MODELS.md -- this is a licence condition, not a courtesy.

use crate::models::PARAKEET_TDT_V3;
use crate::transcribe::Batch;
use macalla_core::audio::AudioSegment;
use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
use macalla_core::{Error, Result};
use std::path::Path;

pub const ENGINE_ID: &str = PARAKEET_TDT_V3.engine;

pub struct ParakeetEngine {
    batch: Batch,
    backend: String,
}

impl ParakeetEngine {
    /// Load the model from a file in the store; see
    /// [`crate::transcribe::load`] for what a first load costs.
    pub fn load(path: &Path) -> Result<Self> {
        let loaded = crate::transcribe::load(path, ENGINE_ID)?;
        let backend = loaded.model.backend().to_string();
        Ok(Self {
            batch: Batch::new(loaded, ENGINE_ID),
            backend,
        })
    }

    /// "MTL0" on Apple Silicon, "CPU" elsewhere. For the log and the card.
    pub fn backend(&self) -> &str {
        &self.backend
    }
}

impl SttEngine for ParakeetEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn caps(&self) -> EngineCaps {
        EngineCaps {
            streaming: false,
            owns_segmentation: false,
            // No vocabulary biasing on this path. The dictionary's phonetic
            // pass corrects terms afterwards, which is the whole
            // argument for the dictionary.
            vocab_bias: false,
            cpu_ok: true,
            languages: PARAKEET_TDT_V3
                .languages
                .iter()
                .map(|l| l.to_string())
                .collect(),
            // One load shared by both streams: they take turns on the model a
            // piece at a time (`transcribe::Batch`), never for a stream's
            // whole life. Measured at real-time pace: sharing the load added
            // no measurable delay (docs/benchmarks.md, "At real-time pace").
            concurrent_streams: true,
        }
    }

    fn stream(&self, _ctx: &SttContext) -> Result<Box<dyn SttStream>> {
        Err(Error::Unsupported {
            engine: ENGINE_ID,
            what: "streaming; the core segments for it",
        })
    }

    fn transcribe(&self, audio: &AudioSegment, ctx: &SttContext) -> Result<Transcript> {
        let options = transcribe_cpp::RunOptions {
            // The model detects the language itself; a hint narrows it.
            language: ctx
                .language
                .as_deref()
                .and_then(|l| PARAKEET_TDT_V3.language_for(l))
                .map(str::to_string),
            ..Default::default()
        };
        self.batch.run(audio, &options)
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
    fn transcribes_the_control_fixture_when_the_model_is_here() {
        let store = crate::models::Store::shared();
        let Some(path) = store.installed(&PARAKEET_TDT_V3) else {
            eprintln!("skipped: no Parakeet model in {}", store.dir().display());
            return;
        };
        let Some(wav) = crate::test_fixtures::audio("control-harvard.aiff") else {
            return;
        };
        let engine = ParakeetEngine::load(&path).unwrap();
        let samples = crate::apple::decode_file(&wav).unwrap();
        let audio = AudioSegment::new(samples, 0);
        let t = engine.transcribe(&audio, &SttContext::default()).unwrap();
        assert!(t.text.contains("birch canoe"), "{}", t.text);
        assert!(t.text.contains("stockings"), "{}", t.text);
    }
}
