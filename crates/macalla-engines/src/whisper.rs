// SPDX-License-Identifier: MIT
//! Whisper through transcribe.cpp: small.en, medium.en and medium.
//!
//! Batch, like Parakeet: the core's segmenter cuts speech at the pauses, at
//! most 30 s a piece, which is also Whisper's own window. What sets it apart
//! is the prompt. Whisper takes an initial prompt, and the dictionary's
//! written forms go in as that prompt, so the model is steered towards the
//! terms before it writes anything instead of corrected afterwards
//! (`EngineCaps::vocab_bias`; measured in docs/benchmarks.md). The phonetic
//! pass still runs behind it.
//!
//! Which model, its languages and its card copy come from its catalogue entry.
//!
//! Licence: Apache-2.0 on the published files; see MODELS.md.

use crate::models::ModelSpec;
use crate::transcribe::Batch;
use macalla_core::audio::AudioSegment;
use macalla_core::stt::{EngineCaps, SttContext, SttEngine, SttStream, Transcript};
use macalla_core::{Error, Result};
use std::path::Path;
use std::sync::Mutex;

/// The prompt tokens Whisper keeps. Its text context is 448 tokens and a
/// prompt gets half of that, less one. transcribe.cpp, like the reference
/// implementation, keeps the last 223 of a longer prompt and drops the front
/// without saying so.
const PROMPT_TOKENS: usize = 223;

pub struct WhisperEngine {
    spec: &'static ModelSpec,
    batch: Batch,
    /// The prompt last built and the terms it came from. The terms change only
    /// when the dictionary does, so this is not rebuilt per utterance.
    prompt: Mutex<Option<(Vec<String>, Option<String>)>>,
}

impl WhisperEngine {
    /// Load the model from a file in the store; see
    /// [`crate::transcribe::load`] for what a first load costs.
    pub fn load(spec: &'static ModelSpec, path: &Path) -> Result<Self> {
        let loaded = crate::transcribe::load(path, spec.engine)?;
        Ok(Self {
            spec,
            batch: Batch::new(loaded, spec.engine),
            prompt: Mutex::new(None),
        })
    }

    fn prompt(&self, terms: &[String]) -> Option<String> {
        let Ok(mut cached) = self.prompt.lock() else {
            return None;
        };
        if let Some((built_from, prompt)) = cached.as_ref() {
            if built_from == terms {
                return prompt.clone();
            }
        }
        let model = self.batch.model();
        let prompt = fit(terms, |text| model.tokenize(text).ok().map(|t| t.len()));
        *cached = Some((terms.to_vec(), prompt.clone()));
        prompt
    }
}

/// How the terms are introduced. Whisper copies a prompt's style as well as
/// its words: a bare list of lower-case words made medium.en write whole
/// sentences with no capital or full stop, and the same list said as a
/// sentence left its capitals and punctuation as they were with no prompt
/// (docs/benchmarks.md).
const PROMPT_LEAD: &str = "Words that may come up: ";

/// The dictionary as a prompt: whole terms, in the dictionary's order, said as
/// one sentence, until the next term would not fit. Left to the library, a
/// long dictionary would lose the terms a person added first, and the term at
/// the cut would be left in pieces. `count` is a text's length in tokens, or
/// `None` when the tokenizer cannot say, in which case there is no prompt.
fn fit(terms: &[String], count: impl Fn(&str) -> Option<usize>) -> Option<String> {
    let mut listed = String::new();
    let mut kept = 0;
    for term in terms {
        let term = term.trim();
        // A special-token literal makes transcribe.cpp refuse the whole run,
        // which would cost the person their dictation.
        if term.is_empty() || term.contains("<|") {
            continue;
        }
        let candidate = if listed.is_empty() {
            term.to_string()
        } else {
            format!("{listed}, {term}")
        };
        // Counted as it will be sent, and tokenised the way the library
        // tokenises a prompt: after one space.
        match count(&format!(" {PROMPT_LEAD}{candidate}.")) {
            Some(tokens) if tokens <= PROMPT_TOKENS => {
                listed = candidate;
                kept += 1;
            }
            Some(_) => break,
            None => return None,
        }
    }
    if kept < terms.len() {
        tracing::info!(
            kept,
            of = terms.len(),
            "not every dictionary term fits Whisper's prompt; the first that fit are used"
        );
    }
    (!listed.is_empty()).then(|| format!("{PROMPT_LEAD}{listed}."))
}

impl SttEngine for WhisperEngine {
    // The engine's id is the catalogue entry's `engine`, not its `id`, which
    // names the model file: two namespaces, as `parakeet-tdt-v3` runs
    // `parakeet-tdt-0.6b-v3`. Clippy reads the field names and guesses wrong.
    #[allow(clippy::misnamed_getters)]
    fn id(&self) -> &'static str {
        self.spec.engine
    }

    fn caps(&self) -> EngineCaps {
        EngineCaps {
            streaming: false,
            owns_segmentation: false,
            vocab_bias: true,
            cpu_ok: true,
            languages: self.spec.languages.iter().map(|l| l.to_string()).collect(),
            // Measured on the path a live meeting uses: Whisper medium.en,
            // paced at real time through a live meeting on one instance, two
            // streams finished 2.00 to 2.04 s after their audio against 2.00
            // to 2.03 s for one stream alone, with the same words
            // (docs/benchmarks.md, "A live meeting on Whisper, one instance").
            // Two streams take turns on the model a piece at a time through
            // `transcribe::Batch`, as Parakeet's do. The smaller Whisper
            // models are faster on the same path, so medium.en is the case
            // that decides it.
            concurrent_streams: true,
        }
    }

    fn stream(&self, _ctx: &SttContext) -> Result<Box<dyn SttStream>> {
        Err(Error::Unsupported {
            engine: self.spec.engine,
            what: "streaming; the core segments for it",
        })
    }

    fn transcribe(&self, audio: &AudioSegment, ctx: &SttContext) -> Result<Transcript> {
        let language = match ctx.language.as_deref() {
            None => None,
            Some(tag) => Some(
                self.spec
                    .language_for(tag)
                    .ok_or_else(|| Error::Stt {
                        engine: self.spec.engine,
                        message: format!("it does not speak {tag}"),
                    })?
                    .to_string(),
            ),
        };
        let family = self.prompt(&ctx.terms).map(|initial_prompt| {
            transcribe_cpp::RunExtension::Whisper(transcribe_cpp::WhisperRunOptions {
                initial_prompt: Some(initial_prompt),
                ..Default::default()
            })
        });
        self.batch.run(
            audio,
            &transcribe_cpp::RunOptions {
                language,
                family,
                ..Default::default()
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(list: &[&str]) -> Vec<String> {
        list.iter().map(|t| t.to_string()).collect()
    }

    /// One token per character: crude, and enough to put the edge where the
    /// test wants it.
    fn chars(text: &str) -> Option<usize> {
        Some(text.chars().count())
    }

    #[test]
    fn a_prompt_holds_whole_terms_in_order_until_the_next_would_not_fit() {
        let long: Vec<String> = (0..10).map(|i| format!("{i}{}", "x".repeat(59))).collect();
        // With the space, the lead and the full stop: one term is 86, two are
        // 148, three 210, and four 272, over 223.
        let prompt = fit(&long, chars).unwrap();
        let listed = prompt
            .strip_prefix(PROMPT_LEAD)
            .and_then(|p| p.strip_suffix('.'))
            .unwrap();
        let kept: Vec<&str> = listed.split(", ").collect();
        assert_eq!(kept, [&long[0], &long[1], &long[2]]);
    }

    #[test]
    fn a_term_that_would_break_the_run_is_left_out_and_the_rest_kept() {
        let prompt = fit(&terms(&["Terragrunt", "<|en|>", "", "kubectl"]), chars).unwrap();
        assert_eq!(prompt, "Words that may come up: Terragrunt, kubectl.");
    }

    /// Needs a Whisper model in the store, and takes the smallest one there.
    /// Skips itself otherwise. macOS only until the fixture decoder leaves the
    /// Apple module.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_prompt_carries_the_dictionary_and_puts_nothing_into_silence() {
        use crate::models::{Store, WHISPER_MEDIUM, WHISPER_MEDIUM_EN, WHISPER_SMALL_EN};
        let store = Store::shared();
        let Some((spec, path)) = [&WHISPER_SMALL_EN, &WHISPER_MEDIUM_EN, &WHISPER_MEDIUM]
            .into_iter()
            .find_map(|s| store.installed(s).map(|p| (s, p)))
        else {
            eprintln!("skipped: no Whisper model in {}", store.dir().display());
            return;
        };
        let Some(wav) = crate::test_fixtures::audio("tech-terms.aiff") else {
            return;
        };
        let engine = WhisperEngine::load(spec, &path).unwrap();
        let samples = crate::apple::decode_file(&wav).unwrap();
        let ctx = SttContext {
            terms: terms(&[
                "Terragrunt",
                "Azure",
                "Terraform",
                "VNet",
                "HCL",
                "AWS",
                "kubectl",
                "Reykjavik",
                "bouillabaisse",
            ]),
            language: Some("en-US".into()),
        };
        let heard = engine
            .transcribe(&AudioSegment::new(samples, 0), &ctx)
            .unwrap();
        for said in ["Terragrunt", "kubectl", "VNet"] {
            assert!(heard.text.contains(said), "{said} missing: {}", heard.text);
        }
        for unsaid in ["Reykjavik", "bouillabaisse"] {
            assert!(
                !heard.text.contains(unsaid),
                "{unsaid} put in: {}",
                heard.text
            );
        }

        // Five seconds of nothing, which the VAD would never send, sent anyway.
        let silence = engine
            .transcribe(&AudioSegment::new(vec![0.0; 16_000 * 5], 0), &ctx)
            .unwrap();
        eprintln!("{} on silence: {:?}", spec.engine, silence.text);
        for term in &ctx.terms {
            assert!(
                !silence.text.contains(term.as_str()),
                "{term} came out of silence: {:?}",
                silence.text
            );
        }
    }

    #[test]
    fn no_terms_or_no_tokenizer_means_no_prompt() {
        assert_eq!(fit(&[], chars), None);
        assert_eq!(fit(&terms(&["  "]), chars), None);
        assert_eq!(fit(&terms(&["Terragrunt"]), |_| None), None);
    }
}
