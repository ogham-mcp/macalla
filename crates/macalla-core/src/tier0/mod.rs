// SPDX-License-Identifier: MIT
//! Tier 0: deterministic cleanup. Always on, sub-millisecond, and it must
//! never damage text.
//!
//! That last constraint drives the design. Tier 0 runs before Tier 1 and its
//! output is what gets pasted when Tier 1 misses the paste budget,
//! so a wrong deletion here reaches the user with nothing to catch it. Where a
//! rule would be ambiguous, Tier 0 declines and leaves the text alone; judgement
//! is Tier 1's job.
//!
//! Rules are per-language JSON tables under `lang/`. English is populated;
//! German, Spanish and Italian are stubs that pass text through unchanged.
//! They are not translatable by lookup -- filler sounds, spoken punctuation and
//! list cadence differ per language.

use crate::block::Block;
use crate::enhance::{Opening, Segment, Tier0};

mod cadence;
use serde::Deserialize;
use std::collections::HashMap;

const UNIVERSAL: &str = include_str!("lang/universal.json");
const EN: &str = include_str!("lang/en.json");
const DE: &str = include_str!("lang/de.json");
const ES: &str = include_str!("lang/es.json");
const IT: &str = include_str!("lang/it.json");
const FR: &str = include_str!("lang/fr.json");
const PT: &str = include_str!("lang/pt.json");
const JA: &str = include_str!("lang/ja.json");
const KO: &str = include_str!("lang/ko.json");
const ZH: &str = include_str!("lang/zh.json");
const YUE: &str = include_str!("lang/yue.json");

#[derive(Debug, Clone, Deserialize)]
pub struct Fillers {
    /// Real words in other languages -- "um" is Portuguese "a/an" and German
    /// "at/around" -- so only removable once the language is known to be this
    /// one. The cross-language axis, adopted from Handy's
    /// `audio_toolkit/text.rs` (MIT).
    /// <https://github.com/cjpais/Handy/blob/main/src-tauri/src/audio_toolkit/text.rs>
    #[serde(default)]
    pub language_gated: Vec<String>,
    /// Real words in *this* language used as discourse markers. Only removed
    /// when they open an utterance -- deleting "so" everywhere mangles "so the
    /// thing is", and deleting "like" everywhere turns "I like it" into "I it".
    pub sentence_initial: Vec<String>,
    /// Multi-word discourse markers, also opening-only. "you know" mid-sentence
    /// is frequently literal ("you know the answer"), so position is the only
    /// safe discriminator available without parsing.
    #[serde(default)]
    pub phrases_initial: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Cadence {
    pub ordinal_words: Vec<String>,
    pub number_words: Vec<String>,
    /// Below this many consecutive markers, it is speech about numbers, not a
    /// list. Two is far too easy to trigger by accident.
    pub minimum_markers: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Numbers {
    #[serde(default)]
    pub enabled: bool,
    /// Small numbers read better as words. Only digitise at or above this.
    #[serde(default)]
    pub minimum_value: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct Universal {
    fillers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rules {
    pub language: String,
    pub fillers: Fillers,
    /// Words that can legitimately stutter. Any other word repeated once is
    /// emphasis: "very very good" must survive, and a word-length heuristic
    /// gets that wrong because "very" is short.
    #[serde(default)]
    pub stutterable: Vec<String>,
    pub punctuation: HashMap<String, String>,
    pub structure: HashMap<String, String>,
    /// Structure commands that are also ordinary words, so they only act while
    /// a list is open. "bullet points" is a phrase; "bullet" inside a spoken
    /// list is the next item.
    #[serde(default)]
    pub structure_in_list_only: Vec<String>,
    /// Commands that are also ordinary words and act only as the first
    /// words of a block that does not continue a sentence: bare "heading"
    /// and "title". Their two-word forms act anywhere.
    #[serde(default)]
    pub structure_at_block_start_only: Vec<String>,
    pub cadence: Cadence,
    pub numbers: Numbers,
}

impl Rules {
    /// Load the table for a BCP-47 language tag. Unknown languages get an
    /// empty ruleset that passes text through rather than applying English
    /// rules to non-English speech.
    pub fn for_language(tag: Option<&str>) -> Self {
        // Fails closed. `None` means the language is unknown, and an unknown
        // language gets an empty ruleset rather than English -- defaulting to
        // English here would strip "um" from Portuguese, where it means
        // "a/an". The principle is Handy's: skip rather than guess.
        let Some(tag) = tag else {
            return serde_json::from_str(ES).expect("stub table must parse");
        };
        let primary = tag.split(['-', '_']).next().unwrap_or("").to_lowercase();
        let raw = match primary.as_str() {
            "en" => EN,
            "de" => DE,
            "es" => ES,
            "it" => IT,
            "fr" => FR,
            "pt" => PT,
            "ja" => JA,
            "ko" => KO,
            "zh" => ZH,
            "yue" => YUE,
            // Unknown language: an empty ruleset, not English.
            _ => ES,
        };
        serde_json::from_str(raw).expect("built-in language table must parse")
    }

    fn max_phrase_words(&self) -> usize {
        self.punctuation
            .keys()
            .chain(self.structure.keys())
            .map(|k| k.split_whitespace().count())
            .max()
            .unwrap_or(1)
    }
}

/// What a phrase lookup matched. Plain words are not tokenised -- they fall
/// through the `None` arm and are pushed verbatim.
enum Tok {
    Punct(String),
    Structure(String),
}

pub struct Tier0Rules {
    rules: Rules,
    /// Universal fillers, shared across every language. Held here rather than
    /// in `rules` because they are not a property of any one language.
    universal: Vec<String>,
    /// The engine capitalises, so the case of the first word of a segment is
    /// its judgement and is kept: a lowercase opening means the engine heard a
    /// sentence continue across a boundary, and the seam pass needs to see
    /// that. Off, every block opens with a capital.
    trust_engine_case: bool,
    /// The engine punctuates, so a list marker must open the text or follow
    /// punctuation. See [`cadence`].
    engine_punctuates: bool,
}

impl Tier0Rules {
    pub fn new(language: Option<&str>) -> Self {
        let universal: Universal =
            serde_json::from_str(UNIVERSAL).expect("universal table must parse");
        Self {
            rules: Rules::for_language(language),
            universal: universal.fillers,
            trust_engine_case: false,
            engine_punctuates: false,
        }
    }

    /// The engine punctuates. Pass what
    /// [`crate::pipeline::TextCaps::punctuates`] says.
    pub fn expecting_punctuation(mut self, punctuates: bool) -> Self {
        self.engine_punctuates = punctuates;
        self
    }

    /// Turn paragraphs spoken as "one …, two …, three …" into ordered lists.
    /// Runs over the whole document after the seam pass; see [`cadence`].
    pub fn detect_lists(&self, blocks: Vec<Block>) -> Vec<Block> {
        cadence::Cadence {
            numbers: &self.rules.cadence.number_words,
            ordinals: &self.rules.cadence.ordinal_words,
            minimum: self.rules.cadence.minimum_markers,
            engine_punctuates: self.engine_punctuates,
        }
        .apply(blocks)
    }

    /// Keep the engine's case on the first word of a segment. Pass what
    /// [`crate::pipeline::TextCaps::capitalises`] says.
    pub fn trusting_engine_case(mut self, trust: bool) -> Self {
        self.trust_engine_case = trust;
        self
    }

    /// A command that only acts while a list is open, heard outside one.
    fn is_gated_outside_lists(&self, words: &[String], at: usize, len: usize, kind: &str) -> bool {
        if kind == "list_item" || kind == "ordered_item" {
            return false;
        }
        let phrase = words[at..at + len]
            .iter()
            .map(|w| bare(w))
            .collect::<Vec<_>>()
            .join(" ");
        self.rules.structure_in_list_only.contains(&phrase)
    }

    /// "We are heading home" and "the title of the book": bare "heading" and
    /// "title" are commands only when nothing has been said yet in this
    /// block, and the block is not the continuation of a sentence from the
    /// segment before. A block already under way keeps them as words.
    fn is_gated_mid_block(
        &self,
        words: &[String],
        at: usize,
        len: usize,
        block_has_words: bool,
        continues_sentence: bool,
    ) -> bool {
        if !block_has_words && !continues_sentence {
            return false;
        }
        let phrase = words[at..at + len]
            .iter()
            .map(|w| bare(w))
            .collect::<Vec<_>>()
            .join(" ");
        self.rules.structure_at_block_start_only.contains(&phrase)
    }

    /// [`Tier0::segment`], told whether the segment continues a sentence the
    /// previous segment left open.
    ///
    /// A sentence-initial filler is only a filler at the start of a sentence.
    /// "so" opening a segment that continues "and then," is the word "so",
    /// and deleting it is damage. Universal fillers still go: "um" is never
    /// a word in the language being spoken.
    pub fn segment_continuing(&self, text: &str, continues: bool) -> Segment {
        self.segment_inner(text, continues)
    }

    pub fn language(&self) -> &str {
        &self.rules.language
    }

    /// Longest-match lookup so "new paragraph" wins over "new", and
    /// "full stop" over "full".
    ///
    /// Matched on the bare words: an engine that punctuates hears "new
    /// paragraph" as speech and writes "New paragraph.", and the command must
    /// still be one.
    fn match_phrase(&self, words: &[String], at: usize) -> Option<(Tok, usize)> {
        let max = self.rules.max_phrase_words().min(words.len() - at);
        for len in (1..=max).rev() {
            let phrase = words[at..at + len]
                .iter()
                .map(|w| bare(w))
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(p) = self.rules.punctuation.get(&phrase) {
                return Some((Tok::Punct(p.clone()), len));
            }
            if let Some(s) = self.rules.structure.get(&phrase) {
                return Some((Tok::Structure(s.clone()), len));
            }
        }
        None
    }

    /// Removable regardless of language, plus the language-gated set -- which
    /// is only populated because a language was named at construction, so
    /// reaching them already implies the evidence.
    ///
    /// Matched on the bare word: a punctuating engine writes "Um," and "Okay,"
    /// and the comma goes with the filler.
    fn is_always_filler(&self, w: &str) -> bool {
        let lw = bare(w);
        self.universal.contains(&lw) || self.rules.fillers.language_gated.contains(&lw)
    }

    /// Longest multi-word discourse marker at `at`, if any. Returns its length
    /// in words.
    fn match_initial_phrase(&self, words: &[String], at: usize) -> Option<usize> {
        let max = self
            .rules
            .fillers
            .phrases_initial
            .iter()
            .map(|p| p.split_whitespace().count())
            .max()
            .unwrap_or(0)
            .min(words.len() - at);
        for len in (1..=max).rev() {
            let phrase = words[at..at + len]
                .iter()
                .map(|w| bare(w))
                .collect::<Vec<_>>()
                .join(" ");
            if self.rules.fillers.phrases_initial.contains(&phrase) {
                return Some(len);
            }
        }
        None
    }

    fn is_initial_filler(&self, w: &str) -> bool {
        self.rules.fillers.sentence_initial.contains(&bare(w))
    }
}

/// Collapse an immediately repeated run of 1..=3 words: "we need to we need to"
/// becomes "we need to", and "the the" becomes "the".
///
/// Only *adjacent* repeats, and only exact matches. "I said that I said that"
/// is a stutter; "very very good" is emphasis and survives because "very" is
/// not repeated as a multi-word run -- see the test.
fn collapse_false_starts(words: Vec<String>, stutterable: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let mut skipped = false;
        for n in (1..=3).rev() {
            if i + 2 * n > words.len() {
                continue;
            }
            let a = &words[i..i + n];
            let b = &words[i + n..i + 2 * n];
            let same = a
                .iter()
                .zip(b.iter())
                .all(|(x, y)| x.to_lowercase() == y.to_lowercase());
            // A single repeated word is only a stutter for short function
            // words; "had had" and "that that" are grammatical.
            let single_ok = n > 1 || stutterable.iter().any(|w| *w == a[0].to_lowercase());
            if same && single_ok {
                out.extend_from_slice(a);
                i += 2 * n;
                skipped = true;
                break;
            }
        }
        if !skipped {
            out.push(words[i].clone());
            i += 1;
        }
    }
    out
}

/// The word without punctuation the engine attached to it, lowercased.
fn bare(w: &str) -> String {
    w.trim_matches(|c: char| ".,?!:;—\"".contains(c))
        .to_lowercase()
}

/// True for a token that is purely a punctuation mark.
fn is_punct_token(w: &str) -> bool {
    !w.is_empty() && w.chars().all(|c| ".,?!:;—\"".contains(c))
}

/// Remove punctuation left stranded by a deleted filler.
///
/// "well comma um comma I think" loses "um" and would otherwise render as
/// "Well, , I think". A punctuation token is dropped when it opens a block or
/// directly follows another punctuation token.
///
/// The problem was found in sebsto/wispr's `FillerWordCleaner.swift`
/// (Apache-2.0), which solves it with two regex passes over a string; this is
/// an independent token-stream implementation of the same observation.
/// <https://github.com/sebsto/wispr/blob/main/Sources/WisprApp/Utilities/FillerWordCleaner.swift>
fn drop_orphaned_punctuation(words: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    for w in words {
        if is_punct_token(&w) && out.last().map(|p| is_punct_token(p)).unwrap_or(true) {
            continue;
        }
        out.push(w);
    }
    out
}

fn capitalise_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Join words into a sentence, attaching punctuation to the preceding word and
/// capitalising after a terminator. `capitalise_start` says whether the first
/// word gets a capital too; a capitalising engine's own case is kept otherwise.
fn render(words: &[String], capitalise_start: bool) -> String {
    let mut out = String::new();
    let mut capitalise_next = capitalise_start;
    for w in words {
        if is_punct_token(w) {
            out.push_str(w);
            if ".?!".contains(w.as_str()) {
                capitalise_next = true;
            }
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        if capitalise_next {
            out.push_str(&capitalise_first(w));
            capitalise_next = false;
        } else {
            out.push_str(w);
        }
    }
    out.trim().to_string()
}

/// What a structure command asks the following text to become.
fn opening_for(command: &str) -> Option<Opening> {
    Some(match command {
        "paragraph_break" => Opening::Paragraph,
        "line_break" => Opening::Line,
        "bullet_list" => Opening::List { ordered: false },
        "ordered_list" => Opening::List { ordered: true },
        "next_item" => Opening::NextItem,
        "heading" => Opening::Heading,
        _ => return None,
    })
}

impl Tier0 for Tier0Rules {
    fn process(&self, text: &str, language: Option<&str>) -> Vec<Block> {
        self.segment(text, language).blocks
    }

    fn segment(&self, text: &str, _language: Option<&str>) -> Segment {
        self.segment_inner(text, false)
    }
}

impl Tier0Rules {
    fn segment_inner(&self, text: &str, continues: bool) -> Segment {
        let raw: Vec<String> = text.split_whitespace().map(str::to_string).collect();
        if raw.is_empty() {
            return Segment::default();
        }

        // 1. Structure and punctuation commands, before any word is deleted --
        //    "period" is a filler-adjacent word and must be recognised first.
        //    Each block remembers whether a spoken command opened it: those
        //    get a capital whatever the engine did, because the user asked
        //    for a fresh block and the engine heard the command as words.
        let mut blocks: Vec<(Vec<String>, String, bool)> = Vec::new();
        let mut current: Vec<String> = Vec::new();
        let mut kind = "paragraph".to_string();
        let mut opened_by: Option<Opening> = None;
        let mut last_command: Option<Opening> = None;
        let mut block_opened_by_command = false;
        let mut i = 0;
        while i < raw.len() {
            match self.match_phrase(&raw, i) {
                Some((Tok::Punct(p), len)) => {
                    current.push(p);
                    last_command = None;
                    i += len;
                }
                Some((Tok::Structure(s), len))
                    if !self.is_gated_outside_lists(&raw, i, len, &kind)
                        && !self.is_gated_mid_block(
                            &raw,
                            i,
                            len,
                            !current.is_empty(),
                            continues && blocks.is_empty() && !block_opened_by_command,
                        ) =>
                {
                    // The engine wrote "it, new paragraph": the comma was its
                    // guess at the pause, not something the user said.
                    if let Some(last) = current.last_mut() {
                        if last.len() > 1 && last.ends_with(',') {
                            last.pop();
                        }
                    }
                    // Every command closes the block before it. "Two things,
                    // numbered list, review the spec" must not make "Two
                    // things" the first item.
                    if !current.is_empty() {
                        blocks.push((
                            std::mem::take(&mut current),
                            kind.clone(),
                            block_opened_by_command,
                        ));
                    }
                    kind = match s.as_str() {
                        "heading" => "heading".into(),
                        "bullet_list" => "list_item".into(),
                        "ordered_list" => "ordered_item".into(),
                        // "next item" continues whatever list is open, so it
                        // must not silently turn an ordered list unordered.
                        "next_item" => {
                            if kind == "ordered_item" {
                                "ordered_item".into()
                            } else {
                                "list_item".into()
                            }
                        }
                        _ => {
                            if kind == "heading" {
                                "paragraph".into()
                            } else {
                                kind.clone()
                            }
                        }
                    };
                    let opening = opening_for(&s);
                    if i == 0 {
                        opened_by = opening;
                    }
                    last_command = opening;
                    block_opened_by_command = true;
                    i += len;
                }
                _ => {
                    current.push(raw[i].clone());
                    last_command = None;
                    i += 1;
                }
            }
        }
        if !current.is_empty() {
            blocks.push((current, kind, block_opened_by_command));
        }

        // 2. Per block: fillers, then false starts, then render.
        let mut out = Vec::new();
        for (index, (words, kind, opened_by_command)) in blocks.into_iter().enumerate() {
            // The first block of a segment that continues a sentence has no
            // utterance start for an initial filler to sit at.
            let mid_sentence = index == 0 && continues && !opened_by_command;
            let mut kept: Vec<String> = Vec::with_capacity(words.len());
            // A deleted word hands its capital to the next word kept: "Um,
            // the tests pass" must not become "the tests pass".
            let mut inherit_capital = false;
            let mut idx = 0usize;
            while idx < words.len() {
                let w = &words[idx];
                if self.is_always_filler(w) {
                    inherit_capital |= starts_upper(w);
                    idx += 1;
                    continue;
                }
                // Opening position: nothing kept yet, or the last thing kept
                // was a sentence terminator.
                let at_start = (kept.is_empty() && !mid_sentence)
                    || kept
                        .last()
                        .map(|p| p == "." || p == "?" || p == "!")
                        .unwrap_or(false);
                if at_start {
                    if let Some(len) = self.match_initial_phrase(&words, idx) {
                        inherit_capital |= starts_upper(w);
                        idx += len;
                        continue;
                    }
                    if self.is_initial_filler(w) {
                        inherit_capital |= starts_upper(w);
                        idx += 1;
                        continue;
                    }
                }
                if inherit_capital && !is_punct_token(w) {
                    kept.push(capitalise_first(w));
                    inherit_capital = false;
                } else {
                    kept.push(w.clone());
                }
                idx += 1;
            }
            let kept = collapse_false_starts(kept, &self.rules.stutterable);
            let kept = drop_orphaned_punctuation(kept);
            if kept.iter().all(|w| w.chars().all(|c| !c.is_alphanumeric())) {
                continue;
            }
            let text = render(&kept, !self.trust_engine_case || opened_by_command);
            if text.is_empty() {
                continue;
            }
            out.push(match kind.as_str() {
                "heading" => Block::Heading { text },
                "list_item" => Block::ListItem {
                    text,
                    ordered: false,
                },
                "ordered_item" => Block::ListItem {
                    text,
                    ordered: true,
                },
                _ => Block::Paragraph { text },
            });
        }
        Segment {
            blocks: out,
            opened_by,
            trailing: last_command,
        }
    }
}

fn starts_upper(w: &str) -> bool {
    w.chars().next().map(char::is_uppercase).unwrap_or(false)
}

#[cfg(test)]
mod combinatorial_tests;
#[cfg(test)]
mod tests;
