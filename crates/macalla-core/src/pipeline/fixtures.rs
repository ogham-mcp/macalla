// SPDX-License-Identifier: MIT
//! Fixture cases run through the whole pipeline: dictionary, Tier 0, seam,
//! cadence, written forms. One JSON shape, one harness, several files:
//!
//! - `fixtures/seam/cases.json` -- segment boundaries (see its README)
//! - `fixtures/cadence/cases.json` -- spoken list cadence

use crate::block::Document;
use crate::dictionary::{Dictionary, TermFile};
use crate::pipeline::{Pipeline, SegmentState, TextCaps};
use crate::stt::{EngineCaps, Transcript};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    engine: String,
    segments: Vec<String>,
    plain: String,
    #[serde(default)]
    markdown: Option<String>,
}

fn pipeline_for(engine: &str) -> Pipeline {
    let seed: TermFile =
        serde_json::from_str(include_str!("../../../../fixtures/dictionary/seed.json"))
            .expect("seed dictionary must parse");
    let dictionary = Dictionary::new(&seed);
    match engine {
        "apple" => Pipeline::new(
            &EngineCaps::stream_first(),
            &TextCaps::apple_speech(),
            dictionary,
            Some("en"),
        ),
        "vad" => Pipeline::new(
            &EngineCaps::batch(),
            &TextCaps {
                punctuates: false,
                capitalises: false,
                emits_fillers: true,
                measured: true,
            },
            dictionary,
            Some("en"),
        ),
        other => panic!("unknown engine shape {other:?} in fixtures"),
    }
}

fn transcripts(segments: &[String]) -> Vec<Transcript> {
    segments
        .iter()
        .enumerate()
        .map(|(i, text)| Transcript {
            text: text.clone(),
            is_final: true,
            start_ms: (i as u64) * 1_000,
            end_ms: (i as u64 + 1) * 1_000,
        })
        .collect()
}

fn assemble(engine: &str, segments: &[String]) -> Document {
    pipeline_for(engine).assemble(&transcripts(segments))
}

/// The same case the way a dictation builds it: each transcript through the
/// per-segment step as it commits, then the finishing step at release. It
/// must be the same document, byte for byte, or cleanup that replaces a
/// segment's blocks would change a dictation it never touched.
fn step_by_step(engine: &str, segments: &[String]) -> Document {
    let pipeline = pipeline_for(engine);
    let mut state = SegmentState::default();
    let pieces = transcripts(segments)
        .iter()
        .filter_map(|t| pipeline.segment(t, &mut state))
        .collect();
    pipeline.finish(pieces)
}

fn run(raw: &str, at_least: usize) {
    let cases: Vec<Case> = serde_json::from_str(raw).expect("case file must parse");
    assert!(
        cases.len() >= at_least,
        "expected at least {at_least} cases, got {}",
        cases.len()
    );
    let mut failures = Vec::new();
    for case in &cases {
        let document = assemble(&case.engine, &case.segments);
        if step_by_step(&case.engine, &case.segments) != document {
            failures.push(format!(
                "{} [{}] (step by step)\n  the per-segment and finishing steps disagree with assemble",
                case.name, case.engine
            ));
        }
        let plain = document.to_plain();
        if plain != case.plain {
            failures.push(format!(
                "{} [{}]\n  expected: {:?}\n  got:      {:?}",
                case.name, case.engine, case.plain, plain
            ));
        }
        if let Some(markdown) = &case.markdown {
            let got = document.to_markdown();
            if &got != markdown {
                failures.push(format!(
                    "{} [{}] (markdown)\n  expected: {:?}\n  got:      {:?}",
                    case.name, case.engine, markdown, got
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn seam_cases() {
    // The design asks for ~40 seam cases.
    run(include_str!("../../../../fixtures/seam/cases.json"), 40);
}

#[test]
fn cadence_cases() {
    run(include_str!("../../../../fixtures/cadence/cases.json"), 10);
}

#[test]
fn a_case_sensitive_term_leaves_another_case_alone_in_the_pasted_text() {
    // correct() leaves "it" for a term "IT"; the case put back after Tier 0's
    // sentence case must not undo that.
    let mut seed: TermFile =
        serde_json::from_str(include_str!("../../../../fixtures/dictionary/seed.json"))
            .expect("seed dictionary must parse");
    seed.terms.push(crate::dictionary::Term {
        written: "IT".into(),
        spoken: vec!["I T".into()],
        case_sensitive: true,
        threshold: crate::dictionary::DEFAULT_THRESHOLD,
    });
    let pipeline = Pipeline::new(
        &EngineCaps::batch(),
        &TextCaps {
            punctuates: false,
            capitalises: false,
            emits_fillers: true,
            measured: true,
        },
        Dictionary::new(&seed),
        Some("en"),
    );
    let plain = |text: &str| {
        pipeline
            .assemble(&transcripts(&[text.to_string()]))
            .to_plain()
    };
    assert_eq!(plain("is it ready"), "Is it ready");
    assert_eq!(plain("the I T team"), "The IT team");
}

#[test]
fn a_capitalised_mishearing_and_a_sentence_cased_term_come_out_as_written() {
    for engine in ["vad", "apple"] {
        for (said, want) in [
            ("Cube control showed the pods", "kubectl showed the pods"),
            ("cube control showed the pods", "kubectl showed the pods"),
            ("Tail scale is up", "Tailscale is up"),
        ] {
            let plain = assemble(engine, &[said.to_string()]).to_plain();
            assert_eq!(plain, want, "{engine}: {said}");
        }
    }
}

#[test]
fn a_repeated_term_is_corrected_and_every_copy_kept() {
    // "azure" is not on the stutterable list, so Tier 0 removes no copy, and
    // the dictionary corrects each one.
    for engine in ["vad", "apple"] {
        let plain = assemble(engine, &["azure azure terraform azure".to_string()]).to_plain();
        assert_eq!(plain, "Azure Azure Terraform Azure", "{engine}");
    }
}
