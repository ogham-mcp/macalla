// SPDX-License-Identifier: MIT
//! Which stages run, derived from what the engine already does.
//!
//! Measured 2026-09-08: Apple SpeechTranscriber already punctuates,
//! capitalises, and does not emit filler sounds. Running Tier 1 after it
//! damaged the text on every change it made across four prompts, because there
//! was no work left to do. Parakeet may be different.
//!
//! So the pipeline is not fixed. It is derived from engine capability, the same
//! way [`crate::stt::EngineCaps::owns_segmentation`] already decides whether
//! VAD segments or merely gates.
//!
//! **Unmeasured engines get the full pipeline.** That is deliberate: assuming
//! an engine cleans up when it does not means shipping raw disfluency to the
//! user, whereas assuming it does not when it does costs some latency and a
//! measurable regression we will catch. Fail toward doing the work.

use crate::block::Document;
use crate::dictionary::Dictionary;
use crate::enhance::Segment;
use crate::stt::{EngineCaps, Transcript};
use crate::tier0::Tier0Rules;

/// What an engine already does to its own output, so downstream stages can
/// avoid repeating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextCaps {
    /// Emits sentence-ending punctuation and commas.
    pub punctuates: bool,
    /// Emits sentence case and capitalised proper nouns.
    pub capitalises: bool,
    /// Transcribes filler sounds ("um", "uh") rather than discarding them.
    pub emits_fillers: bool,
    /// Someone has actually measured the three flags above against real output
    /// from this engine. When false the flags are ignored and every stage runs.
    pub measured: bool,
}

impl TextCaps {
    /// The safe default for an engine nobody has measured: assume it does
    /// nothing, and run the full pipeline.
    pub const fn unmeasured() -> Self {
        Self {
            punctuates: false,
            capitalises: false,
            emits_fillers: true,
            measured: false,
        }
    }

    /// Apple SpeechTranscriber, corrected 2026-09-08 against **real human
    /// speech**.
    ///
    /// An earlier reading set `emits_fillers: false` on the strength of 15
    /// `say`-synthesised fixtures, where Apple discarded every filler. Against
    /// a person it does not:
    ///
    /// ```text
    /// Um, so the thing is, uh, we need to, we need to ship the parcel by Friday.
    /// Hmm, uh, We deploy to AWS as well.
    /// ```
    ///
    /// Synthetic "um" is a crisply articulated word; a human "um" is a
    /// vocalisation, and the engine treats them differently. The synthetic
    /// measurement was not merely imprecise, it gave the opposite answer.
    pub const fn apple_speech() -> Self {
        Self {
            punctuates: true,
            capitalises: true,
            emits_fillers: true,
            measured: true,
        }
    }

    /// What has been measured about an engine, by id.
    ///
    /// **Unknown ids get [`TextCaps::unmeasured`]**, which runs every stage.
    /// One place decides this, so a new engine cannot arrive with an optimistic
    /// guess attached to it.
    pub fn for_engine_id(id: &str) -> Self {
        match id {
            "apple-speech" => Self::apple_speech(),
            "parakeet-tdt-v3" => Self::parakeet_tdt_v3(),
            _ => Self::unmeasured(),
        }
    }

    /// Parakeet TDT v3, measured 2026-09-08 on the same audio.
    ///
    /// It punctuates and capitalises like Apple, but **transcribes fillers
    /// verbatim**: the same clip came back as "Um so the thing is uh we need to
    /// we need to ship the the passer by Friday uh and then um run the
    /// benchmarks you know." Tier 0 therefore has real work on this path that
    /// it does not have on Apple's.
    pub const fn parakeet_tdt_v3() -> Self {
        Self {
            punctuates: true,
            capitalises: true,
            emits_fillers: true,
            measured: true,
        }
    }
}

/// The stages to run for a given engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelinePlan {
    /// VAD cuts segments, rather than only gating the stream.
    pub vad_segments: bool,
    /// Phonetic dictionary correction. Never skipped: vocabulary errors are
    /// what every engine gets wrong, and no engine fixes them for us.
    pub dictionary: bool,
    /// Tier 0 filler removal and stutter collapse.
    pub tier0_fillers: bool,
    /// Tier 0 spoken punctuation and structure commands. Always on: "new
    /// paragraph" is an instruction from the user, not a transcription artefact,
    /// and no engine acts on it.
    pub tier0_structure: bool,
    /// Whether Tier 1 cleanup has work left in this engine's text. Advice,
    /// not a gate: cleanup a person has chosen runs whatever this says,
    /// because the choice is theirs and a prompt they wrote may ask for more
    /// than punctuation. It is what `macalla doctor` explains, and what a
    /// settings card can say about the engine.
    pub tier1: bool,
}

impl PipelinePlan {
    /// Derive the plan from engine capability.
    pub fn for_engine(caps: &EngineCaps, text: &TextCaps) -> Self {
        // Unmeasured: ignore the flags entirely and run everything.
        if !text.measured {
            return Self {
                vad_segments: !caps.owns_segmentation,
                dictionary: true,
                tier0_fillers: true,
                tier0_structure: true,
                tier1: true,
            };
        }
        Self {
            vad_segments: !caps.owns_segmentation,
            dictionary: true,
            // Nothing to remove if the engine never transcribes fillers.
            tier0_fillers: text.emits_fillers,
            tier0_structure: true,
            // Tier 1's only remaining contribution would be punctuation and
            // case. If the engine supplies both, it has no job -- and measured
            // it does not merely add nothing, it actively damages the text.
            tier1: !(text.punctuates && text.capitalises),
        }
    }

    /// Why Tier 1 has no work for this engine, for the log and the settings
    /// card. `None` when it has. Advice like [`PipelinePlan::tier1`]: chosen
    /// cleanup still runs.
    pub fn tier1_disabled_reason(caps: &TextCaps) -> Option<&'static str> {
        if !caps.measured {
            return None;
        }
        match (caps.punctuates, caps.capitalises) {
            (true, true) => Some("the engine already punctuates and capitalises"),
            _ => None,
        }
    }
}

/// What [`Pipeline::segment`] carries from one committed transcript to the
/// next. One per dictation, starting from the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SegmentState {
    /// Whether the previous segment left a sentence open. Tier 0 needs to
    /// know: a sentence-initial filler at the start of a continuation is a
    /// word. An engine that does not punctuate never closes one, so its seams
    /// always continue and the filler stays -- declining is the rule where
    /// Tier 0 cannot tell. Read from Tier 0's blocks, never Tier 1's: the next
    /// segment is shaped before cleanup of this one has come back.
    sentence_open: bool,
}

impl SegmentState {
    /// Whether the last segment [`Pipeline::segment`] made left a sentence
    /// open. A caller that replaces that segment's blocks with a model's
    /// reply reads this to keep the sentence open across the seam: a reply to
    /// half a sentence often ends it with a full stop.
    pub fn sentence_open(&self) -> bool {
        self.sentence_open
    }
}

/// Runs the stages a [`PipelinePlan`] calls for.
///
/// This is the state machine the design draws, minus the parts that do not
/// exist yet. The hotkey, the CLI and the agent interface are three triggers
/// for this one object, because the core is callable without the hotkey or the
/// UI. If a stage only runs from the tray, it is in the wrong crate.
///
/// Order is dictionary, then Tier 0, and it must not be inverted. Tier 0 turns
/// "kube control" into a word boundary the dictionary can no longer see whole,
/// and Tier 1's change-ratio guard is measured against Tier 0's output, not the
/// raw transcript (see [`crate::enhance::MAX_CHANGE_RATIO`]).
///
/// Tier 1 is not a stage here: a model is I/O and runs outside the core. It
/// fits between [`Pipeline::segment`] and [`Pipeline::finish`], replacing a
/// segment's blocks, and `macalla-session` schedules it there. The seam pass
/// ([`crate::seam`]) runs last, over every segment at once, because a
/// boundary is only visible from both sides.
pub struct Pipeline {
    plan: PipelinePlan,
    dictionary: Dictionary,
    tier0: Tier0Rules,
}

impl Pipeline {
    /// Build the pipeline an engine calls for.
    pub fn new(
        caps: &EngineCaps,
        text: &TextCaps,
        dictionary: Dictionary,
        language: Option<&str>,
    ) -> Self {
        Self {
            plan: PipelinePlan::for_engine(caps, text),
            dictionary,
            tier0: Tier0Rules::new(language)
                .trusting_engine_case(text.capitalises)
                .expecting_punctuation(text.punctuates),
        }
    }

    pub fn plan(&self) -> &PipelinePlan {
        &self.plan
    }

    pub fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    /// Correct one piece of text without running any other stage.
    ///
    /// Exposed because the dictionary is the product's differentiator and
    /// wants to be inspectable on its own, from `macalla transcribe` and from
    /// the settings screen's preview.
    pub fn correct(&self, text: &str) -> String {
        if self.plan.dictionary {
            self.dictionary.correct(text)
        } else {
            text.to_string()
        }
    }

    /// Committed transcripts in, the document to paste out.
    ///
    /// Volatile results are dropped here rather than by the caller. Preview
    /// text is shown and never pasted, and making that the pipeline's rule
    /// means no trigger can get it wrong.
    ///
    /// The composition of [`Pipeline::segment`] over every transcript and
    /// [`Pipeline::finish`] over what they made. A dictation calls the two
    /// apart, so Tier 1 can clean each segment while the person is still
    /// speaking; a file, or a caller with no cleanup, calls
    /// this.
    pub fn assemble(&self, transcripts: &[Transcript]) -> Document {
        let mut state = SegmentState::default();
        let segments = transcripts
            .iter()
            .filter_map(|t| self.segment(t, &mut state))
            .collect();
        self.finish(segments)
    }

    /// One committed transcript in, its corrected Tier 0 segment out: the
    /// step that runs per segment, during the recording.
    ///
    /// `None` for a volatile transcript, which is preview text and never
    /// pasted, and for one the dictionary leaves blank; neither changes
    /// `state`. Call it in the order the transcripts committed, with one
    /// `state` per dictation.
    pub fn segment(&self, transcript: &Transcript, state: &mut SegmentState) -> Option<Segment> {
        if !transcript.is_final {
            return None;
        }
        let corrected = self.correct(&transcript.text);
        if corrected.trim().is_empty() {
            return None;
        }
        let segment = if self.plan.tier0_fillers || self.plan.tier0_structure {
            self.tier0
                .segment_continuing(&corrected, state.sentence_open)
        } else {
            Segment::paragraph(corrected)
        };
        if let Some(last) = segment.blocks.last() {
            state.sentence_open =
                segment.trailing.is_none() && !last.text().trim_end().ends_with(['.', '?', '!']);
        } else if segment.trailing.is_some() || segment.opened_by.is_some() {
            state.sentence_open = false;
        }
        Some(segment)
    }

    /// Every segment of a dictation in, the document out: the step that runs
    /// once, at release, over all of them at once.
    ///
    /// A segment's blocks may be Tier 0's or what Tier 1 made of them; the
    /// seam pass, list cadence and written forms run over whichever arrived,
    /// because a boundary and a cadence are only visible from both sides.
    pub fn finish(&self, segments: Vec<Segment>) -> Document {
        let mut blocks = crate::seam::join(segments);
        // A spoken cadence spans the pauses, so it is only visible now.
        if self.plan.tier0_structure {
            blocks = self.tier0.detect_lists(blocks);
        }
        // After the seam: it re-cases the first word of a rejoined segment,
        // and a written form must win over that too. Shared with
        // `Dictionary::settle`, which runs the same restore after Tier 1.
        for block in &mut blocks {
            *block.text_mut() = self.dictionary.restore_case(block.text());
        }
        Document::new(blocks)
    }
}

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::dictionary::{Dictionary, Term, TermFile};

    fn dictionary_with(terms: &[(&str, &[&str])]) -> Dictionary {
        Dictionary::new(&TermFile {
            terms: terms
                .iter()
                .map(|(written, spoken)| Term {
                    written: (*written).to_string(),
                    spoken: spoken.iter().map(|s| (*s).to_string()).collect(),
                    case_sensitive: false,
                    threshold: crate::dictionary::DEFAULT_THRESHOLD,
                })
                .collect(),
        })
    }

    fn final_at(text: &str) -> Transcript {
        Transcript {
            text: text.into(),
            is_final: true,
            start_ms: 0,
            end_ms: 1_000,
        }
    }

    fn apple_pipeline(dictionary: Dictionary) -> Pipeline {
        Pipeline::new(
            &EngineCaps::stream_first(),
            &TextCaps::apple_speech(),
            dictionary,
            Some("en"),
        )
    }

    #[test]
    fn an_engine_nobody_has_measured_gets_the_cautious_answer() {
        assert!(!TextCaps::for_engine_id("some-new-engine").measured);
        assert_eq!(
            TextCaps::for_engine_id("apple-speech"),
            TextCaps::apple_speech()
        );
        assert_eq!(
            TextCaps::for_engine_id("parakeet-tdt-v3"),
            TextCaps::parakeet_tdt_v3()
        );
    }

    #[test]
    fn the_dictionary_corrects_before_anything_else_touches_the_text() {
        // The measured miss from fixtures/transcripts/tech-terms.json. This is
        // the whole argument for the dictionary in one assertion.
        let p = apple_pipeline(dictionary_with(&[("kubectl", &["cube control"])]));
        let doc = p.assemble(&[final_at("cube control showed the pods running.")]);
        assert!(
            doc.to_plain().contains("kubectl"),
            "got: {}",
            doc.to_plain()
        );
    }

    #[test]
    fn tier_0_sentence_case_does_not_override_a_written_form() {
        // "kubectl" is lowercase by convention. Tier 0 capitalises the first
        // word of a block, which turned the dictionary's own answer into
        // "Kubectl". An explicit written form outranks sentence case.
        let p = apple_pipeline(dictionary_with(&[("kubectl", &["cube control"])]));
        let plain = p
            .assemble(&[final_at("cube control showed the pods.")])
            .to_plain();
        assert!(plain.starts_with("kubectl"), "got: {plain}");
    }

    #[test]
    fn a_written_form_that_is_capitalised_stays_capitalised() {
        let p = apple_pipeline(dictionary_with(&[("Ogham", &["ogam"])]));
        let plain = p
            .assemble(&[final_at("we put it in ogam yesterday.")])
            .to_plain();
        assert!(plain.contains("Ogham"), "got: {plain}");
        assert!(!plain.contains("ogham"), "got: {plain}");
    }

    #[test]
    fn restoring_case_leaves_words_that_are_not_dictionary_terms_alone() {
        let p = apple_pipeline(dictionary_with(&[("kubectl", &["cube control"])]));
        let plain = p.assemble(&[final_at("The pods are running.")]).to_plain();
        assert!(plain.starts_with("The"), "got: {plain}");
    }

    #[test]
    fn volatile_results_never_reach_the_document() {
        let p = apple_pipeline(dictionary_with(&[]));
        let doc = p.assemble(&[
            Transcript {
                text: "half a sen".into(),
                is_final: false,
                start_ms: 0,
                end_ms: 500,
            },
            final_at("Half a sentence, then the rest."),
        ]);
        assert!(
            !doc.to_plain().contains("half a sen,"),
            "preview text is shown, never pasted: {}",
            doc.to_plain()
        );
        assert_eq!(doc.blocks.len(), 1);
    }

    #[test]
    fn nothing_said_gives_an_empty_document_rather_than_an_empty_paragraph() {
        let p = apple_pipeline(dictionary_with(&[]));
        assert!(p.assemble(&[]).blocks.is_empty());
        assert!(p.assemble(&[final_at("   ")]).blocks.is_empty());
    }

    #[test]
    fn every_committed_segment_contributes() {
        let p = apple_pipeline(dictionary_with(&[]));
        let doc = p.assemble(&[final_at("First part."), final_at("Second part.")]);
        let plain = doc.to_plain();
        assert!(plain.contains("First part"), "got: {plain}");
        assert!(plain.contains("Second part"), "got: {plain}");
    }

    #[test]
    fn a_dictation_segment_by_segment_is_the_same_document_as_assembling_it() {
        // The per-segment step runs as each transcript commits, the finishing
        // step at release; together they are `assemble`. A sentence left open
        // across the boundary is the state that has to travel between them.
        let p = apple_pipeline(dictionary_with(&[("kubectl", &["cube control"])]));
        let transcripts = [
            final_at("Um, so cube control showed"),
            Transcript {
                text: "the pods".into(),
                is_final: false,
                start_ms: 1_000,
                end_ms: 1_500,
            },
            final_at("the pods running. New paragraph."),
            final_at("   "),
            final_at("Uh, and then it stopped."),
        ];
        let mut state = SegmentState::default();
        let segments: Vec<Segment> = transcripts
            .iter()
            .filter_map(|t| p.segment(t, &mut state))
            .collect();
        assert_eq!(
            segments.len(),
            3,
            "the volatile and the blank transcripts are not segments"
        );
        assert_eq!(p.finish(segments), p.assemble(&transcripts));
    }

    #[test]
    fn a_volatile_or_blank_transcript_leaves_the_state_as_it_was() {
        let p = apple_pipeline(dictionary_with(&[]));
        let mut state = SegmentState::default();
        p.segment(&final_at("I mean"), &mut state).unwrap();
        let open = state;
        assert!(p
            .segment(
                &Transcript {
                    is_final: false,
                    ..final_at("the detections.")
                },
                &mut state
            )
            .is_none());
        assert!(p.segment(&final_at("  "), &mut state).is_none());
        assert_eq!(state, open);
    }

    #[test]
    fn the_finishing_step_seams_and_restores_written_forms_over_blocks_a_model_replaced() {
        // Cleanup replaces a segment's blocks between the two steps. The seam
        // still joins what it returned, and the dictionary's written form
        // still outranks the model's sentence case.
        let p = apple_pipeline(dictionary_with(&[("kubectl", &["cube control"])]));
        let mut state = SegmentState::default();
        let mut first = p
            .segment(&final_at("cube control ran"), &mut state)
            .unwrap();
        let second = p
            .segment(&final_at("And then it stopped."), &mut state)
            .unwrap();
        first.blocks = vec![crate::block::Block::Paragraph {
            text: "Kubectl ran,".into(),
        }];
        assert_eq!(
            p.finish(vec![first, second]).to_plain(),
            "kubectl ran, and then it stopped."
        );
    }

    #[test]
    fn the_pipeline_an_unmeasured_engine_gets_still_runs_the_dictionary() {
        // Failing open means running more stages, never fewer -- and the
        // dictionary is the one stage that is never skipped for anyone.
        let p = Pipeline::new(
            &EngineCaps::batch(),
            &TextCaps::unmeasured(),
            dictionary_with(&[("Ogham", &["ogam"])]),
            Some("en"),
        );
        assert!(p.plan().tier1, "unmeasured engines get the whole pipeline");
        assert!(p
            .assemble(&[final_at("I stored it in ogam.")])
            .to_plain()
            .contains("Ogham"));
    }

    #[test]
    fn correcting_one_string_does_not_need_a_whole_dictation() {
        // What the settings preview and `macalla transcribe` call.
        let p = apple_pipeline(dictionary_with(&[("Terragrunt", &["terra grunt"])]));
        assert_eq!(
            p.correct("I ran terra grunt today"),
            "I ran Terragrunt today"
        );
    }

    #[test]
    fn apple_speech_runs_tier0_but_not_tier1() {
        let plan = PipelinePlan::for_engine(&EngineCaps::stream_first(), &TextCaps::apple_speech());
        assert!(
            !plan.tier1,
            "Tier 1 damaged the text on every change it made here"
        );
        assert!(
            plan.tier0_fillers,
            "corrected against real speech: Apple DOES transcribe fillers; the \
             synthetic fixtures said otherwise and were wrong"
        );
        assert!(
            !plan.vad_segments,
            "stream-first engines segment themselves"
        );
        // The two that never turn off.
        assert!(
            plan.dictionary,
            "vocabulary errors are what every engine gets wrong"
        );
        assert!(
            plan.tier0_structure,
            "'new paragraph' is a user instruction, not an artefact"
        );
    }

    #[test]
    fn both_engines_need_tier0_and_neither_needs_tier1() {
        // Against real human speech the two engines agree on every flag: both
        // punctuate, both capitalise, and both transcribe fillers. The
        // synthetic fixtures had them differing on the last, which was an
        // artefact of `say` articulating "um" as a word.
        let apple =
            PipelinePlan::for_engine(&EngineCaps::stream_first(), &TextCaps::apple_speech());
        let para = PipelinePlan::for_engine(&EngineCaps::batch(), &TextCaps::parakeet_tdt_v3());
        assert!(
            apple.tier0_fillers,
            "Apple emits fillers against a real speaker"
        );
        assert!(para.tier0_fillers, "Parakeet emits fillers too");
        assert!(!apple.tier1);
        assert!(
            !para.tier1,
            "both punctuate, so neither leaves Tier 1 a job"
        );
    }

    #[test]
    fn an_unmeasured_engine_gets_the_whole_pipeline() {
        let plan = PipelinePlan::for_engine(&EngineCaps::batch(), &TextCaps::unmeasured());
        assert!(plan.tier1);
        assert!(plan.tier0_fillers);
        assert!(plan.vad_segments);
    }

    #[test]
    fn unmeasured_flags_are_ignored_even_if_optimistic() {
        // An engine claiming to punctuate but never measured must not be
        // trusted into skipping work. Failing toward doing the work costs
        // latency; failing the other way ships disfluency to the user.
        let optimistic = TextCaps {
            punctuates: true,
            capitalises: true,
            emits_fillers: false,
            measured: false,
        };
        let plan = PipelinePlan::for_engine(&EngineCaps::batch(), &optimistic);
        assert!(plan.tier1, "unmeasured claims must not disable a stage");
        assert!(plan.tier0_fillers);
    }

    #[test]
    fn an_engine_that_punctuates_but_does_not_capitalise_still_runs_tier1() {
        let half = TextCaps {
            punctuates: true,
            capitalises: false,
            emits_fillers: false,
            measured: true,
        };
        let plan = PipelinePlan::for_engine(&EngineCaps::batch(), &half);
        assert!(plan.tier1, "case is still missing, so Tier 1 has work");
    }

    #[test]
    fn the_disabled_reason_is_only_given_when_it_is_actually_disabled() {
        assert_eq!(
            PipelinePlan::tier1_disabled_reason(&TextCaps::apple_speech()),
            Some("the engine already punctuates and capitalises")
        );
        assert_eq!(
            PipelinePlan::tier1_disabled_reason(&TextCaps::unmeasured()),
            None
        );
    }
}
