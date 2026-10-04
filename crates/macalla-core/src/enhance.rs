// SPDX-License-Identifier: MIT
//! The enhancement traits, and the policy every Tier 1 provider shares.
//!
//! Tier 0 is deterministic, sub-millisecond and always on -- even when Tier 1
//! is disabled by an app profile. Tier 1 is a model the person already has:
//! Apple Intelligence, an OpenAI-compatible endpoint, or a cloud
//! model. None of them runs inside Macalla. What they share lives here: the
//! port, the default prompt, and the guard on what comes back.
//!
//! **Session lifecycle belongs to the backend, not the pipeline**.
//! The FoundationModels backend wants a *fresh* session per segment: reusing
//! one accumulates a transcript, which was measured degrading latency from
//! 4 s to 14 s and making the model echo its input verbatim instead of
//! cleaning it. An endpoint holds no session at all. The trait must not assume
//! either shape, so [`Enhancer::enhance`] takes `&self` and each backend
//! manages its own state internally.

use std::collections::VecDeque;
use std::time::Duration;

use crate::block::Block;
use crate::Result;

/// What a spoken structure command asks the *next* text to become.
///
/// Reported at the edges of a segment so the seam pass can honour a command
/// that fell at a boundary: "new paragraph" is often the last thing said
/// before a pause, and the pause is where the engine cuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    /// "new paragraph".
    Paragraph,
    /// "new line".
    Line,
    /// "new list" / "numbered list": a list starts here.
    List { ordered: bool },
    /// "next item": another item of whatever list is open.
    NextItem,
    /// "heading".
    Heading,
}

/// One engine segment after Tier 0, with what it knew about its own edges.
///
/// Segment boundaries are not sentence boundaries, whoever produced them
/// (the seam pass). The blocks inside a segment are separated by
/// spoken commands and are therefore real; the two edges are where a boundary
/// may have fallen mid-sentence, so each edge says whether a command was the
/// reason for it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Segment {
    pub blocks: Vec<Block>,
    /// The command that opened this segment, if the first thing said was one.
    /// `None` means the first block is plain text that may continue the
    /// previous segment.
    pub opened_by: Option<Opening>,
    /// The command that closed this segment, if the last thing said was one.
    /// Whatever the next segment opens with should honour it.
    pub trailing: Option<Opening>,
}

impl Segment {
    /// A single plain paragraph with nothing known about its edges. What the
    /// pipeline builds when Tier 0 is off.
    pub fn paragraph(text: String) -> Self {
        Self {
            blocks: vec![Block::Paragraph { text }],
            opened_by: None,
            trailing: None,
        }
    }
}

/// Deterministic cleanup. Always on. Runs before Tier 1 so the LLM never sees
/// raw spoken punctuation or a mangled technical term.
pub trait Tier0: Send + Sync {
    /// Turn one engine transcript into blocks.
    fn process(&self, text: &str, language: Option<&str>) -> Vec<Block>;

    /// Turn one engine transcript into a [`Segment`]: the blocks, plus what
    /// the edges know. The pipeline calls this; [`Tier0::process`] is the
    /// same thing for callers that only want the text.
    fn segment(&self, text: &str, language: Option<&str>) -> Segment {
        Segment {
            blocks: self.process(text, language),
            opened_by: None,
            trailing: None,
        }
    }
}

/// What a Tier 1 backend reports about itself, so the pipeline can schedule
/// against it rather than guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnhancerCaps {
    /// Backend identifier, for logs and the latency breakdown.
    pub id: &'static str,
    /// The median time one segment took over the backend's recent requests,
    /// on this machine, in milliseconds; `None` until it has served one.
    /// Measured rather than written in, because the same provider is several
    /// times faster or slower with the Mac, the model and the network: Apple
    /// Intelligence took 853 ms p50 on the M1 Pro reference machine, and newer
    /// Apple silicon is unmeasured. The scheduler uses it to decide whether the
    /// last segment is worth waiting for.
    pub typical_ms: Option<u32>,
    /// The backend enforces the reply's shape while decoding (Apple's guided
    /// generation), rather than taking free text and relying on the guard.
    pub constrained_output: bool,
}

/// Tier 1. Refines Tier 0 blocks; never invents content.
pub trait Enhancer: Send + Sync {
    fn caps(&self) -> EnhancerCaps;

    /// Whether the backend can serve a request right now. FoundationModels
    /// reports `deviceNotEligible`, `appleIntelligenceNotEnabled` or
    /// `modelNotReady`, and Apple Intelligence is a setting the user can turn
    /// off at any moment -- so this is checked per call, not once at startup.
    fn is_available(&self) -> bool;

    /// Get ready for a request that is likely to come soon, such as when a
    /// dictation starts or enhancement is switched on: a backend that loads a
    /// model starts loading it. Returns at once. The default does nothing.
    fn prewarm(&self) {}

    /// Enhance one segment's blocks. `terms` are the dictionary's written
    /// forms, which the model is told to keep exactly.
    fn enhance(&self, blocks: &[Block], terms: &[String]) -> Result<Vec<Block>>;
}

/// The instructions a provider gets unless the person has written their own.
///
/// The "narrow" candidate from the prompt bake-off in `docs/benchmarks.md`: on
/// real engine output it damaged 1 of 15 transcripts, where the prompt asking
/// for a rewrite damaged 4. Tier 0 has already removed fillers and stutters,
/// so punctuation and capitals are all that is left for a model to do. That
/// bake-off ran with the reply's shape enforced during decoding; an endpoint
/// returns free text, and how often a model wraps its answer is measured per
/// provider, with the guard below as the backstop.
pub const DEFAULT_PROMPT: &str = "You add punctuation and capitalisation to dictated text. \
The words are already correct. Add sentence-ending punctuation and commas where a speaker's \
pauses imply them. Capitalise the first word of each sentence, proper nouns, and acronyms. \
You MUST NOT add, remove, replace or reorder any word. Return the same words, punctuated.";

/// The prompt with the dictionary's terms added, as the design asks:
/// "known terms; preserve exactly, including case".
pub fn instructions(prompt: &str, terms: &[String]) -> String {
    if terms.is_empty() {
        return prompt.to_string();
    }
    format!(
        "{prompt}\n\nKnown terms; preserve exactly, including case: {}.",
        terms.join(", ")
    )
}

/// Enhance blocks one at a time: `rewrite` is handed a block's text and
/// returns the model's reply.
///
/// Each block keeps its kind, because Tier 1 returns text and never a block
/// kind. A reply the guard rejects leaves that block's Tier 0
/// text in place and the rest of the segment carries on. An error from
/// `rewrite` -- the provider could not be reached, or answered with an error --
/// fails the whole call, and the pipeline pastes Tier 0. A blank block is not
/// sent. `guard` decides what a reply may change; [`Guard::for_prompt`] picks
/// it from the prompt that was sent.
pub fn rewrite_each(
    blocks: &[Block],
    guard: Guard,
    mut rewrite: impl FnMut(&str) -> Result<String>,
) -> Result<Vec<Block>> {
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        if block.text().trim().is_empty() {
            out.push(block.clone());
            continue;
        }
        let mut candidate = block.clone();
        *candidate.text_mut() = rewrite(block.text())?.trim().to_string();
        let (before, after) = (
            std::slice::from_ref(block),
            std::slice::from_ref(&candidate),
        );
        if guard.accepts(before, after) {
            out.push(candidate);
        } else {
            // The ratio only: the text is what the person said.
            tracing::debug!(
                ?guard,
                ratio = change_ratio(before, after),
                "Tier 1 reply rejected; the block keeps its Tier 0 text"
            );
            out.push(block.clone());
        }
    }
    Ok(out)
}

/// What a reply may change before [`rewrite_each`] throws it away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// The same words in the same order, and no Markdown the block did not
    /// already have: case and punctuation only. For [`DEFAULT_PROMPT`], which
    /// asks for exactly that, so any other change is the model not doing as it
    /// was told. Measured on an endpoint on 2026-09-11: the 40 % ratio let
    /// postgres become PostgreSQL and notarisation become notarization. The
    /// first is arguably right, since PostgreSQL is the database's full name,
    /// but choosing the word is the dictionary's job, or a prompt the person
    /// wrote, and not the basic prompt's.
    SameWords,
    /// Up to [`MAX_CHANGE_RATIO`] of the block's words dropped or added. For a
    /// prompt the person wrote, which may ask for more than punctuation.
    Ratio,
}

impl Guard {
    /// The strict guard for the default prompt, the ratio for any prompt a
    /// person has edited.
    pub fn for_prompt(prompt: &str) -> Self {
        if prompt.trim() == DEFAULT_PROMPT {
            Guard::SameWords
        } else {
            Guard::Ratio
        }
    }

    /// Whether `after` may replace `before`.
    pub fn accepts(self, before: &[Block], after: &[Block]) -> bool {
        match self {
            Guard::SameWords => words(before) == words(after) && markup(after) <= markup(before),
            Guard::Ratio => is_acceptable(before, after),
        }
    }
}

/// A provider's most recent request times, for [`EnhancerCaps::typical_ms`].
#[derive(Debug, Default)]
pub struct RecentTimes {
    times: VecDeque<u32>,
}

impl RecentTimes {
    /// Enough to ride out one slow request, few enough to follow a change of
    /// network or model within a few dictations.
    const KEPT: usize = 20;

    pub fn record(&mut self, took: Duration) {
        if self.times.len() == Self::KEPT {
            self.times.pop_front();
        }
        self.times
            .push_back(u32::try_from(took.as_millis()).unwrap_or(u32::MAX));
    }

    /// The middle time, or the upper of the two middles; `None` before any.
    pub fn median_ms(&self) -> Option<u32> {
        let mut sorted: Vec<u32> = self.times.iter().copied().collect();
        sorted.sort_unstable();
        sorted.get(sorted.len() / 2).copied()
    }
}

/// Reject a Tier 1 result that changed too much of the input.
///
/// A small model asked to tidy speech will occasionally answer it, summarise
/// it, drop a clause, or wrap it in "Here is the cleaned text:". Guided
/// generation, where a backend has it, stops it emitting the wrong *shape*;
/// this stops it emitting the wrong *content*. Both are needed, and an
/// endpoint has only this.
///
/// # Measured against Tier 0 output, never the raw transcript
///
/// For a prompt the person wrote, the guard rejects a reply that drops or
/// adds more than 40 % of the block's words ([`Guard::Ratio`]; the default
/// prompt gets the stricter [`Guard::SameWords`]). That has to mean *Tier 0's*
/// words. Measured against the raw
/// engine transcript, ordinary correct cleanup fails it: the real example
///
/// ```text
/// raw:    um so the thing is uh we need to we need to ship the parser by friday
/// tier 1: We need to ship the parser by Friday.
/// ```
///
/// keeps 8 of 17 raw words and drops 9 -- a ratio of 0.53, rejected, despite
/// being exactly the output we want. Filler and stutter removal is Tier 0's job
/// and has already happened by the time Tier 1 is called, so
/// the comparison is against the cleaned blocks. This is why the pipeline order
/// matters and must not be inverted.
pub const MAX_CHANGE_RATIO: f32 = 0.40;

/// Normalise a word for comparison: case-folded, surrounding punctuation
/// stripped. Applying punctuation and sentence case *is* Tier 1's job, so
/// neither may count as a change.
fn normalise(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// A hyphen or dash, which joins words in writing but is never spoken:
/// hyphen-minus, the Unicode hyphen and non-breaking hyphen, en and em dash.
fn is_dash(c: char) -> bool {
    matches!(c, '-' | '\u{2010}' | '\u{2011}' | '\u{2013}' | '\u{2014}')
}

/// The blocks' words, normalised, in order. A hyphen or dash separates words
/// as a space does: "ninety five" written as "ninety-five", or two clauses
/// joined as "stream—not", is punctuation the model added, and "e-mail" for
/// "email" is still two words where there was one.
fn words(blocks: &[Block]) -> Vec<String> {
    blocks
        .iter()
        .flat_map(|b| {
            b.text()
                .split(|c: char| c.is_whitespace() || is_dash(c))
                .map(normalise)
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Markdown characters, which a plain-text reply has no business adding.
/// Counted rather than matched, so a block that already had some keeps them.
fn markup(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .flat_map(|b| b.text().chars())
        .filter(|c| "`*_#~[]<>".contains(*c))
        .count()
}

/// Word-level change between Tier 0 and Tier 1 output, in `0.0..=1.0`: the
/// larger of the words dropped and the words added, as a share of Tier 0's
/// word count.
///
/// Added words count because an endpoint returns free text, and a reply that
/// wraps the whole input in a preamble drops nothing. A preamble short enough
/// against a long block still passes: the prompt forbids it, and this bounds
/// how much one can add.
pub fn change_ratio(before: &[Block], after: &[Block]) -> f32 {
    let (a, b) = (words(before), words(after));
    if a.is_empty() {
        return if b.is_empty() { 0.0 } else { 1.0 };
    }
    let mut remaining = b.clone();
    let mut kept = 0usize;
    for word in &a {
        if let Some(pos) = remaining.iter().position(|w| w == word) {
            remaining.remove(pos);
            kept += 1;
        }
    }
    let (dropped, added) = (a.len() - kept, b.len() - kept);
    (dropped.max(added) as f32 / a.len() as f32).min(1.0)
}

/// Whether a Tier 1 result should be accepted over its Tier 0 input.
pub fn is_acceptable(before: &[Block], after: &[Block]) -> bool {
    change_ratio(before, after) <= MAX_CHANGE_RATIO
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn para(text: &str) -> Vec<Block> {
        vec![Block::Paragraph { text: text.into() }]
    }

    #[test]
    fn identical_output_has_not_changed_at_all() {
        assert_eq!(
            change_ratio(&para("the latency budget"), &para("the latency budget")),
            0.0
        );
    }

    #[test]
    fn ordinary_cleanup_is_accepted() {
        // The real measured example from docs/benchmarks.md, compared against
        // TIER 0 output as the pipeline actually does: fillers and the stutter
        // are already gone, so Tier 1 only adds case and punctuation.
        let before = para("we need to ship the parser by friday");
        let after = para("We need to ship the parser by Friday.");
        assert_eq!(change_ratio(&before, &after), 0.0);
        assert!(is_acceptable(&before, &after));
    }

    #[test]
    fn measuring_against_the_raw_transcript_would_reject_correct_cleanup() {
        // Guards the ordering rule (Tier 0 runs before Tier 1). If Tier 1 were ever fed
        // the raw engine transcript instead of Tier 0 blocks, this guard would
        // throw away exactly the output we want.
        let raw = para("um so the thing is uh we need to we need to ship the parser by friday");
        let good = para("We need to ship the parser by Friday.");
        assert!((change_ratio(&raw, &good) - 9.0 / 17.0).abs() < 1e-6);
        assert!(
            !is_acceptable(&raw, &good),
            "if this passes, the note on MAX_CHANGE_RATIO is stale"
        );
    }

    #[test]
    fn case_and_punctuation_alone_are_never_a_change() {
        // Applying these is Tier 1's job, so they must cost nothing.
        let before = para("the vad gates the stream not the other way round");
        let after = para("The VAD gates the stream, not the other way round.");
        assert_eq!(change_ratio(&before, &after), 0.0);
    }

    #[test]
    fn a_model_that_answers_instead_of_cleaning_is_rejected() {
        let before = para("what is the latency budget for the parser");
        let after = para("The latency budget is three hundred milliseconds.");
        assert!(
            !is_acceptable(&before, &after),
            "ratio was {}",
            change_ratio(&before, &after)
        );
    }

    #[test]
    fn a_model_that_drops_the_content_is_rejected() {
        let before = para("one two three four five six seven eight nine ten");
        let after = para("one two three");
        assert!(!is_acceptable(&before, &after));
    }

    #[test]
    fn empty_input_producing_output_counts_as_a_total_change() {
        assert_eq!(change_ratio(&[], &para("invented")), 1.0);
        assert_eq!(change_ratio(&[], &[]), 0.0);
    }

    #[test]
    fn dropping_half_the_content_is_rejected_even_without_invention() {
        // Every output word came from the input, so an invention-only metric
        // would wave this through. Deletion has to be caught too.
        let before = para("review the spec and then run the benchmarks");
        let after = para("Review the spec.");
        assert!(
            !is_acceptable(&before, &after),
            "ratio was {}",
            change_ratio(&before, &after)
        );
    }

    #[test]
    fn a_reply_that_wraps_every_word_in_a_preamble_is_rejected() {
        // Drops nothing, so a deletion-only metric would accept it. This is
        // the reply an endpoint returning free text is most likely to send.
        let before = para("we need to ship the parser by friday");
        let after = para("Sure, here is the text: We need to ship the parser by Friday.");
        assert!(
            !is_acceptable(&before, &after),
            "ratio was {}",
            change_ratio(&before, &after)
        );
    }

    #[test]
    fn a_rejected_reply_keeps_that_block_as_tier_0_wrote_it_and_the_rest_carry_on() {
        let blocks = vec![
            Block::ListItem {
                text: "review the spec".into(),
                ordered: true,
            },
            Block::ListItem {
                text: "what is the latency budget for the parser".into(),
                ordered: true,
            },
        ];
        let out = rewrite_each(&blocks, Guard::SameWords, |text| {
            Ok(if text.starts_with("review") {
                "Review the spec.".into()
            } else {
                "The latency budget is three hundred milliseconds.".into()
            })
        })
        .unwrap();
        assert_eq!(
            out,
            vec![
                Block::ListItem {
                    text: "Review the spec.".into(),
                    ordered: true
                },
                blocks[1].clone(),
            ]
        );
    }

    #[test]
    fn a_provider_error_fails_the_segment_so_tier_0_is_pasted() {
        let failed = rewrite_each(&para("review the spec"), Guard::Ratio, |_| {
            Err(Error::Enhance {
                backend: "test",
                message: "no answer".into(),
            })
        });
        assert!(failed.is_err());
    }

    #[test]
    fn a_blank_block_is_not_sent() {
        let mut sent = 0;
        let out = rewrite_each(&para("  "), Guard::SameWords, |text| {
            sent += 1;
            Ok(text.into())
        })
        .unwrap();
        assert_eq!((sent, out), (0, para("  ")));
    }

    #[test]
    fn with_the_default_prompt_one_swapped_word_is_rejected_though_the_ratio_allows_it() {
        // Real replies from qwen2.5:1.5b on an endpoint, 2026-09-11.
        for (tier0, reply) in [
            (
                "The postgres database uses pgvector for the embeddings you know",
                "The PostgreSQL database uses `pgvector` for the embeddings you know.",
            ),
            (
                "The notarisation step runs in ci not locally",
                "The notarization step runs in CI not locally.",
            ),
            (
                "The thing is that tailscale runs on the proxmox lxc container",
                "The thing is, tailscale runs on the proxmox LXC container.",
            ),
        ] {
            let (before, after) = (para(tier0), para(reply));
            assert!(Guard::Ratio.accepts(&before, &after), "{reply}");
            assert!(!Guard::SameWords.accepts(&before, &after), "{reply}");
        }
    }

    #[test]
    fn with_the_default_prompt_case_and_punctuation_pass() {
        // A real reply from the same run.
        let before = para("What i mean is the vad gates the stream not the other way round");
        let after = para("What I mean is: the vad gates the stream, not the other way round.");
        assert!(Guard::SameWords.accepts(&before, &after));
    }

    #[test]
    fn with_the_default_prompt_a_hyphen_or_dash_joining_two_spoken_words_passes() {
        // The first two are real replies from ministral-3b and ministral-8b,
        // 2026-09-11, which the guard threw out because it read each joined
        // pair as one word.
        for (tier0, reply) in [
            (
                "We need to measure the p ninety five not just the median",
                "We need to measure the p ninety-five, not just the median.",
            ),
            (
                "What i mean is the vad gates the stream not the other way round",
                "What I mean is the VAD gates the stream\u{2014}not the other way round.",
            ),
            (
                "the paste that is the whole point",
                "The paste\u{2013}that is the whole point.",
            ),
            ("ninety five", "Ninety\u{2010}five."),
        ] {
            assert!(
                Guard::SameWords.accepts(&para(tier0), &para(reply)),
                "{reply}"
            );
        }
    }

    #[test]
    fn with_the_default_prompt_a_hyphen_that_splits_or_adds_a_word_is_rejected() {
        let before = para("send me an email about the stream");
        assert!(!Guard::SameWords.accepts(&before, &para("Send me an e-mail about the stream.")));
        assert!(!Guard::SameWords.accepts(
            &before,
            &para("Send me an email about the stream\u{2014}today.")
        ));
        assert!(!Guard::SameWords.accepts(&before, &para("Send me an **email** about the stream.")));
    }

    #[test]
    fn a_joining_dash_is_no_change_to_the_ratio_either() {
        let before = para("we need to measure the p ninety five");
        assert_eq!(
            change_ratio(&before, &para("We need to measure the p ninety-five.")),
            0.0
        );
    }

    #[test]
    fn with_the_default_prompt_added_markdown_is_rejected_but_markdown_already_there_is_not() {
        let before = para("the postgres database uses pgvector");
        assert!(!Guard::SameWords.accepts(&before, &para("The postgres database uses `pgvector`.")));
        let had = para("rename `max_ratio` first");
        assert!(Guard::SameWords.accepts(&had, &para("Rename `max_ratio` first.")));
    }

    #[test]
    fn with_the_default_prompt_reordered_words_are_rejected() {
        let before = para("ship the parser by friday");
        assert!(!Guard::SameWords.accepts(&before, &para("By Friday, ship the parser.")));
    }

    #[test]
    fn the_default_prompt_gets_the_strict_guard_and_an_edited_one_the_ratio() {
        assert_eq!(Guard::for_prompt(DEFAULT_PROMPT), Guard::SameWords);
        let edited = format!("{DEFAULT_PROMPT} Use British spelling.");
        assert_eq!(Guard::for_prompt(&edited), Guard::Ratio);
    }

    #[test]
    fn known_terms_follow_the_prompt_and_no_terms_leave_it_alone() {
        assert_eq!(instructions(DEFAULT_PROMPT, &[]), DEFAULT_PROMPT);
        let with = instructions(DEFAULT_PROMPT, &["Macalla".into(), "pgvector".into()]);
        assert!(with.starts_with(DEFAULT_PROMPT));
        assert!(with.ends_with("including case: Macalla, pgvector."));
    }

    #[test]
    fn the_typical_time_is_the_median_of_recent_requests_and_old_ones_age_out() {
        let mut times = RecentTimes::default();
        assert_eq!(times.median_ms(), None);
        for ms in [300, 100, 200] {
            times.record(Duration::from_millis(ms));
        }
        assert_eq!(times.median_ms(), Some(200));
        // A slow model swapped for a fast one: twenty requests later the old
        // times are gone.
        for _ in 0..RecentTimes::KEPT {
            times.record(Duration::from_millis(900));
        }
        for _ in 0..11 {
            times.record(Duration::from_millis(100));
        }
        assert_eq!(times.median_ms(), Some(100));
    }
}
