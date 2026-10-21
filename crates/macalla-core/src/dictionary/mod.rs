// SPDX-License-Identifier: MIT
//! The cross-engine dictionary.
//!
//! Every engine measured gets vocabulary wrong, and none of them fixes it:
//! Apple produced `azure`, `V net`, `passer`; Parakeet produced `Cube control`
//! and `passer`. Punctuation and case, by contrast, both engines already
//! handle. That is why this layer is the differentiator that survives an engine
//! swap, and why [`crate::pipeline::PipelinePlan`] never switches it off.
//!
//! # Two mechanisms, in order
//!
//! 1. **Explicit spoken forms.** A term lists the ways it gets heard, and those
//!    are matched as n-grams. `kubectl` lists "kube control" and "cube
//!    control" -- no fuzzy metric reaches that from `kubectl` (similarity 0.55),
//!    and none should. Far transformations are data, not inference.
//! 2. **Fuzzy fallback**, for variants nobody listed, requiring Soundex
//!    agreement *and* similarity above the threshold. Not for a case-sensitive
//!    term, which fires only on an input written as one of its listed forms.
//!
//!    Deliberately conservative, because no threshold separates the cases:
//!    `ogam` -> `Ogham` and `postgresql` -> `Postgres` are both 0.800 similar,
//!    and the first must fire while the second must not. With every seeded term
//!    listing its observed spoken forms, the exact path carries the load and
//!    fuzzy only has to catch what nobody wrote down. A false correction is
//!    worse than a missed one: the user cannot see what was changed.

pub mod phonetic;

use crate::block::Block;
use phonetic::{similarity_at_least, soundex};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Default similarity threshold.
pub const DEFAULT_THRESHOLD: f32 = 0.85;

/// Longest spoken form, in words, that will be considered.
pub const MAX_NGRAM: usize = 3;

/// Longest run of words [`correction_offer`] will offer, on either side.
/// Independent of [`MAX_NGRAM`] -- one bounds what a dictionary term can
/// match, the other what a single hand correction can teach it -- even
/// though today they hold the same number.
const MAX_OFFER_WORDS: usize = 3;

/// Longest run [`correction_offer`] will offer, in characters, on either
/// side -- alongside [`MAX_OFFER_WORDS`], because a script with no spaces
/// between words, such as CJK, turns a whole sentence into one
/// `split_whitespace` "word", which the word cap alone would never catch.
const MAX_OFFER_CHARS: usize = 40;

/// ASCII punctuation stripped from a word's, or a run's, two edges before
/// they are compared or offered, so "Jira." and "Jira," count as the same
/// word. Punctuation inside a word ("O'Brien", "e.g.") is never touched.
const EDGE_PUNCTUATION: [char; 10] = ['.', ',', ';', ':', '!', '?', '"', '\'', '(', ')'];

/// Unicode punctuation, beyond [`EDGE_PUNCTUATION`]'s ASCII set, stripped
/// the same way: the marks ordinary typing produces that
/// `char::is_ascii_punctuation` was never going to cover -- curly quotes,
/// guillemets, the ellipsis character, and the dash family (en, em,
/// horizontal bar, the Unicode minus sign). Not the full Unicode
/// "punctuation" general category (`P*`): that needs a table this crate has
/// no dependency for, so this is a bounded, explicit set of the marks
/// dictation text has actually been seen to produce at a run's edge, kept
/// short enough to read at a glance and extend by hand.
const EDGE_PUNCTUATION_UNICODE: [char; 13] = [
    '\u{2018}', '\u{2019}', // ‘ ’
    '\u{201C}', '\u{201D}', // " "
    '\u{2039}', '\u{203A}', // ‹ ›
    '\u{00AB}', '\u{00BB}', // « »
    '\u{2026}', // …
    '\u{2013}', '\u{2014}', '\u{2015}', // – — ―
    '\u{2212}', // − (minus sign)
];

/// `s` with its two edges' punctuation stripped, ASCII and Unicode alike:
/// nothing in the middle is touched. Used both to compare a word ("Jira."
/// against "Jira,") and, in [`correction_offer`], to strip a whole joined
/// run ("quarter." to "quarter").
fn stripped(s: &str) -> &str {
    s.trim_matches(|c| EDGE_PUNCTUATION.contains(&c) || EDGE_PUNCTUATION_UNICODE.contains(&c))
}

/// What a hand correction to one turn offers to teach the dictionary: the
/// run of words Macalla heard, and the run the person corrected them to.
/// From [`correction_offer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    /// The words as they were before the correction, as heard -- with the
    /// run's own leading and trailing punctuation stripped, since that is a
    /// sentence's punctuation, not the term's. Punctuation in the middle of
    /// the run stays.
    pub heard: String,
    /// The words as the person corrected them to, the same way.
    pub written: String,
}

/// Whether a hand correction to one turn's text looks like a single
/// mishearing worth offering to the dictionary, and if so, which words.
///
/// The rule, kept as simple as can be honest, and deliberately blind to a
/// turn edited in two places at once (see the note below):
///
/// 1. Split `old` and `new` into words on whitespace.
/// 2. Trim the common prefix and the common suffix of words, comparing each
///    pair with its own leading and trailing punctuation stripped
///    ([`stripped`]) -- so a word that only lost a comma still counts as
///    unchanged and drops out of the run left over. Case is never folded
///    here, so a word whose only difference is its case ends the common run
///    too. What is left is the old run and the new run.
/// 3. Offer only when both runs are non-empty (a pure insertion or deletion
///    is not a mishearing of something, so there is nothing to teach), and
///    each is [`MAX_OFFER_WORDS`] words or fewer, by the plain word count
///    from step 1. This word cap alone bounds nothing for a script with no
///    spaces between words, where a whole sentence is one `split_whitespace`
///    "word" -- see the character cap below. This step is also what refuses
///    a punctuation-only fix: step 2 trims words that differ only in
///    punctuation away as common, so nothing is left.
/// 4. What is offered is each run joined back into one string, then that
///    whole string's own leading and trailing punctuation is stripped
///    ([`stripped`]) -- "quarter." offers "quarter", not "quarter.": a
///    sentence's full stop is not part of the term, and neither are curly
///    quotes wrapped around a name. Punctuation in the middle of a run, on
///    a word that is not at the run's own edge, is left alone, so a run
///    like "Mr. Bob" keeps its internal hyphen or period. Offer only when,
///    after that stripping, both of these still hold:
///    - each run is [`MAX_OFFER_CHARS`] characters or fewer -- checked
///      after stripping, so a term's own trailing punctuation, already
///      gone, never counts against it: a 40-character term with a full
///      stop after it is still offered, not rejected as 41 characters;
///    - each run has an alphanumeric character left in it -- a run that is
///      only punctuation, or symbols with no letters or digits in them at
///      all, is not offered.
///
/// A case-only change, such as "github" to "GitHub", counts: [`stripped`]
/// only strips punctuation, never folds case, so the runs still differ and
/// the offer is made.
///
/// Two real edits sitting either side of an unchanged word -- "the cat sat"
/// to "a cat sits" -- are offered as one three-word run, which is not
/// really a single mishearing. Deliberate: distinguishing that from a
/// genuine three-word phrase needs a rule that is not simple and not
/// obviously right either, and the offer already asks before it teaches
/// anything -- it opens `TermForm` pre-filled, and the person confirms or
/// edits it there, so an occasional bad grouping costs a look, never a bad
/// dictionary entry written without being seen.
///
/// This function knows nothing about a dictionary: whether the pair it
/// offers is already in one is checked where the dictionary is available
/// (the desktop app's `dictionary.rs`), not here.
pub fn correction_offer(old: &str, new: &str) -> Option<Offer> {
    let old_words: Vec<&str> = old.split_whitespace().collect();
    let new_words: Vec<&str> = new.split_whitespace().collect();

    let mut start = 0;
    while start < old_words.len()
        && start < new_words.len()
        && stripped(old_words[start]) == stripped(new_words[start])
    {
        start += 1;
    }

    let mut old_end = old_words.len();
    let mut new_end = new_words.len();
    while old_end > start
        && new_end > start
        && stripped(old_words[old_end - 1]) == stripped(new_words[new_end - 1])
    {
        old_end -= 1;
        new_end -= 1;
    }

    let heard_words = &old_words[start..old_end];
    let written_words = &new_words[start..new_end];

    if heard_words.is_empty() || written_words.is_empty() {
        return None;
    }
    if heard_words.len() > MAX_OFFER_WORDS || written_words.len() > MAX_OFFER_WORDS {
        return None;
    }
    let heard_joined = heard_words.join(" ");
    let written_joined = written_words.join(" ");
    let heard = stripped(&heard_joined);
    let written = stripped(&written_joined);
    if heard.chars().count() > MAX_OFFER_CHARS || written.chars().count() > MAX_OFFER_CHARS {
        return None;
    }
    if !heard.chars().any(char::is_alphanumeric) || !written.chars().any(char::is_alphanumeric) {
        return None;
    }

    Some(Offer {
        heard: heard.to_string(),
        written: written.to_string(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Term {
    /// The form to produce.
    pub written: String,
    /// Ways it gets heard. Matched exactly, as n-grams, before any fuzzy work.
    #[serde(default)]
    pub spoken: Vec<String>,
    /// When true, the term fires only on an input written, apart from its
    /// surrounding punctuation, as one of `spoken` or as `written`, exactly or
    /// with only a lower-case form's first letter capitalised, as at the start
    /// of a sentence: "om" is left alone when only "OM" and "Om" are listed.
    /// Such a term gets no fuzzy fallback, and a later stage's re-casing is
    /// only undone for a word written the same way. When false, a match is
    /// accepted regardless of the input's case.
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "default_threshold")]
    pub threshold: f32,
}

fn default_threshold() -> f32 {
    DEFAULT_THRESHOLD
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TermFile {
    #[serde(default)]
    pub terms: Vec<Term>,
}

/// Why an edit to a [`TermFile`] was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("a term needs a written form")]
    EmptyWritten,
    #[error("there is already a term for {0:?}")]
    Duplicate(String),
    #[error("there is no term for {0:?}")]
    Unknown(String),
}

/// The rules of the file every editing surface shares -- the settings list,
/// "add to dictionary" from history, a JSON import, and one day the CLI.
///
/// Written forms are unique ignoring case: "kubectl" and "Kubectl" are one
/// term with one right answer, not two. Spoken forms are trimmed, deduplicated
/// ignoring case, and never equal to the written form, which matches itself
/// already -- except for a case-sensitive term, whose spoken forms can differ
/// only by case on purpose ("terraform", "TerraForm"), so there both rules
/// compare exactly. The shipped seed is an example set and gets no special
/// treatment.
impl TermFile {
    fn position(&self, written: &str) -> Option<usize> {
        let wanted = written.trim().to_lowercase();
        self.terms
            .iter()
            .position(|t| t.written.trim().to_lowercase() == wanted)
    }

    /// The term for a written form, ignoring case.
    pub fn get(&self, written: &str) -> Option<&Term> {
        self.position(written).map(|i| &self.terms[i])
    }

    /// Add spoken forms to a written form, creating the term if it is new.
    /// Returns `true` when a term was created.
    pub fn add(&mut self, written: &str, spoken: &[String]) -> Result<bool, EditError> {
        let written = written.trim();
        if written.is_empty() {
            return Err(EditError::EmptyWritten);
        }
        match self.position(written) {
            Some(i) => {
                let term = &mut self.terms[i];
                let mut forms = std::mem::take(&mut term.spoken);
                forms.extend(spoken.iter().cloned());
                term.spoken = tidy_spoken(&term.written, forms, term.case_sensitive);
                Ok(false)
            }
            None => {
                self.terms.push(Term {
                    written: written.to_string(),
                    spoken: tidy_spoken(written, spoken.to_vec(), false),
                    case_sensitive: false,
                    threshold: DEFAULT_THRESHOLD,
                });
                Ok(true)
            }
        }
    }

    /// Replace the term that was `previous` with `term`, which may rename it.
    pub fn update(&mut self, previous: &str, term: Term) -> Result<(), EditError> {
        let i = self
            .position(previous)
            .ok_or_else(|| EditError::Unknown(previous.to_string()))?;
        let written = term.written.trim().to_string();
        if written.is_empty() {
            return Err(EditError::EmptyWritten);
        }
        if let Some(other) = self.position(&written) {
            if other != i {
                return Err(EditError::Duplicate(written));
            }
        }
        self.terms[i] = Term {
            spoken: tidy_spoken(&written, term.spoken, term.case_sensitive),
            written,
            case_sensitive: term.case_sensitive,
            threshold: term.threshold,
        };
        Ok(())
    }

    /// Forget a term. Returns whether there was one.
    pub fn remove(&mut self, written: &str) -> bool {
        match self.position(written) {
            Some(i) => {
                self.terms.remove(i);
                true
            }
            None => false,
        }
    }

    /// Take every term of another file: new written forms are added, known
    /// ones gain the other file's spoken forms. Returns how many terms were
    /// added or changed. An import never removes anything.
    /// Read a dictionary in any of the three shapes people have.
    ///
    /// There is no shared interchange format for dictation dictionaries, so
    /// the JSON export is ours, and what people already hold is simpler:
    /// a plain word list, one term per line (a spell-checker's custom words,
    /// a pasted glossary), or replacement pairs `heard,written` as CSV or
    /// TSV (a text expander's list). Detected by content, not by file name:
    /// JSON starts with `{`; a line with a comma or a tab is a pair; any
    /// other line is a written form with no spoken forms. Blank lines and
    /// `#` comments are skipped, quotes around a field are dropped, and a
    /// `heard,written` header is recognised and skipped. Several pairs for
    /// one written form give it several spoken forms.
    pub fn parse(text: &str) -> crate::Result<TermFile> {
        let trimmed = text.trim_start_matches('\u{feff}').trim();
        if trimmed.starts_with('{') {
            return serde_json::from_str(trimmed)
                .map_err(|e| crate::Error::Audio(format!("dictionary: {e}")));
        }
        let mut order: Vec<String> = Vec::new();
        let mut spoken: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (n, raw) in trimmed.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (heard, written) = match line.split_once(['\t', ',']) {
                Some((heard, written)) => (Some(unquote(heard)), unquote(written)),
                None => (None, unquote(line)),
            };
            if n == 0
                && heard
                    .as_deref()
                    .is_some_and(|h| h.eq_ignore_ascii_case("heard"))
                && written.eq_ignore_ascii_case("written")
            {
                continue; // a CSV header, not a term
            }
            if written.is_empty() {
                return Err(crate::Error::Audio(format!(
                    "dictionary: line {} has no written form",
                    n + 1
                )));
            }
            let key = written.to_lowercase();
            let forms = spoken.entry(key.clone()).or_insert_with(|| {
                order.push(written.clone());
                Vec::new()
            });
            if let Some(heard) = heard.filter(|h| !h.is_empty()) {
                forms.push(heard);
            }
        }
        let mut file = TermFile::default();
        for written in order {
            let forms = spoken.remove(&written.to_lowercase()).unwrap_or_default();
            file.add(&written, &forms)
                .map_err(|e| crate::Error::Audio(format!("dictionary: {e}")))?;
        }
        Ok(file)
    }

    pub fn merge(&mut self, other: &TermFile) -> usize {
        let mut changed = 0;
        for term in &other.terms {
            let before = self.get(&term.written).cloned();
            if self.add(&term.written, &term.spoken).is_err() {
                continue;
            }
            // A term new to this file keeps the imported case sensitivity and
            // threshold: `add` creates every term as not case-sensitive at the
            // default threshold, and tidies its spoken forms that way. A known
            // term keeps its own.
            if before.is_none() {
                if let Some(i) = self.position(&term.written) {
                    let added = &mut self.terms[i];
                    added.case_sensitive = term.case_sensitive;
                    added.threshold = term.threshold;
                    added.spoken =
                        tidy_spoken(&added.written, term.spoken.clone(), term.case_sensitive);
                }
            }
            let after = self.get(&term.written).cloned();
            if before.map(|t| t.spoken) != after.map(|t| t.spoken) {
                changed += 1;
            }
        }
        changed
    }
}

/// A CSV field without its surrounding quotes, trimmed.
fn unquote(field: &str) -> String {
    let field = field.trim();
    let inner = field
        .strip_prefix('"')
        .and_then(|f| f.strip_suffix('"'))
        .unwrap_or(field);
    inner.trim().to_string()
}

/// Trim, drop empties and the written form itself, and deduplicate, keeping
/// first-seen order and spelling. Ignoring case, except for a case-sensitive
/// term, whose forms can differ only by case on purpose, so there it compares
/// exactly: "terraform" and "TerraForm" both stay beside "Terraform".
fn tidy_spoken(written: &str, forms: Vec<String>, case_sensitive: bool) -> Vec<String> {
    let fold = |s: &str| {
        if case_sensitive {
            s.to_string()
        } else {
            s.to_lowercase()
        }
    };
    let written = fold(written.trim());
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for form in forms {
        let form = form.trim().to_string();
        let key = fold(&form);
        if form.is_empty() || key == written || !seen.insert(key) {
            continue;
        }
        out.push(form);
    }
    out
}

/// A term with its phonetic keys precomputed, so matching does no allocation
/// per candidate. Matching must take under a millisecond for 500 terms.
#[derive(Debug, Clone)]
struct Compiled {
    written: String,
    /// Normalised spoken forms, longest first, with their word counts.
    spoken: Vec<(usize, String)>,
    threshold: f32,
    case_sensitive: bool,
    /// The spoken forms and the written form exactly as listed, for a
    /// case-sensitive term's exact-case check.
    listed: Vec<String>,
}

/// Strip everything but alphanumerics and lowercase. "Terra Grunt" and
/// "terragrunt" normalise to the same key, which is why the joined-n-gram
/// approach works at all.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `said` is written as one of a case-sensitive term's `listed` forms:
/// exactly, or with only its first letter capitalised, since an engine or a
/// model capitalises the first word of a sentence ("Cube control" for a listed
/// "cube control"). A form already starting with a capital allows nothing
/// more, so "om" stays unmatched when "OM" and "Om" are listed, and "It" when
/// a term lists "IT".
fn written_as_listed(listed: &[String], said: &str) -> bool {
    listed.iter().any(|form| {
        if form == said {
            return true;
        }
        let mut chars = form.chars();
        match chars.next() {
            Some(first) if first.is_lowercase() => {
                let capitalised: String = first.to_uppercase().chain(chars).collect();
                capitalised == said
            }
            _ => false,
        }
    })
}

/// Whether `word` is cased like an ordinary word: every letter after the first
/// is lower case ("it", "It"), where "IT" or "PGVector" is not.
fn shaped_like_an_ordinary_word(word: &str) -> bool {
    word.chars()
        .filter(|c| c.is_alphabetic())
        .skip(1)
        .all(char::is_lowercase)
}

/// True for a token that, once its surrounding punctuation is stripped, is
/// exactly one alphabetic character: "c", "T", "l.", "X,". A speech engine
/// spells a term out this way sometimes ("cube c t l" for "kubectl"), and a
/// RUN of two or more of these is what `group_words` folds into one logical
/// word. A single one on its own ("I", "a") is an ordinary word and is left
/// alone -- see `group_words`.
fn is_spelled_letter(token: &str) -> bool {
    let core = token.trim_matches(|c: char| !c.is_alphanumeric());
    let mut chars = core.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_alphabetic())
}

/// Group whitespace-split words into logical words, merging a run of two or
/// more consecutive spelled-out letters into one. This runs on both sides of
/// matching -- counting a spoken or written form's length at load, and
/// building n-grams from the input in `correct` -- so the two agree on what
/// "one word" means and a form like "cube c t l" is found however the input
/// happens to be spaced. Each returned range indexes into the word slice
/// passed in.
fn group_words(words: &[&str]) -> Vec<std::ops::Range<usize>> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if is_spelled_letter(words[i]) {
            let mut j = i + 1;
            while j < words.len() && is_spelled_letter(words[j]) {
                j += 1;
            }
            if j - i >= 2 {
                groups.push(i..j);
                i = j;
                continue;
            }
        }
        groups.push(i..i + 1);
        i += 1;
    }
    groups
}

/// A form's length in logical words, clamped to `MAX_NGRAM` the same way the
/// raw word count always was. Goes through `group_words` so "cube c t l"
/// counts as two words rather than four, which is what lets it be found by
/// an n-gram window that only ever grows to `MAX_NGRAM`.
fn counted_words(s: &str) -> usize {
    let words: Vec<&str> = s.split_whitespace().collect();
    group_words(&words).len().clamp(1, MAX_NGRAM)
}

/// One spoken form, flattened out of its term so it can be indexed.
#[derive(Debug, Clone)]
struct Form {
    term: usize,
    words: usize,
    norm: String,
    key: String,
}

/// `Clone` because a session with a cloud engine fits one pipeline to the
/// on-device preview engine and another to the cloud engine, each owning its
/// dictionary.
#[derive(Clone)]
pub struct Dictionary {
    terms: Vec<Compiled>,
    forms: Vec<Form>,
    /// Normalised form -> form index. The common case, and O(1).
    exact: HashMap<String, usize>,
    /// Soundex key -> form indices, bounding the fuzzy scan to phonetic
    /// neighbours rather than every term.
    by_key: HashMap<String, Vec<usize>>,
    /// Form length -> form indices, so the distance path only scores
    /// candidates close enough in length to clear the threshold.
    by_len: HashMap<usize, Vec<usize>>,
    max_ngram: usize,
    /// Lowercased written form -> its term, for [`Dictionary::restore_case`].
    /// A later stage's own case rules -- Tier 0's sentence case, or a
    /// free-form Tier 1 reply -- can turn "kubectl" into "Kubectl"; the
    /// written form is exactly what the user asked to get, so it is put back
    /// afterwards. Built once; the lookup is per word.
    written_forms: HashMap<String, usize>,
}

impl Dictionary {
    pub fn new(file: &TermFile) -> Self {
        let mut max_ngram = 1;
        let terms: Vec<Compiled> = file
            .terms
            .iter()
            .map(|t| {
                let mut spoken: Vec<(usize, String)> = t
                    .spoken
                    .iter()
                    .map(|s| (counted_words(s), norm(s)))
                    .collect();
                spoken.push((counted_words(&t.written), norm(&t.written)));
                spoken.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
                for (n, _) in &spoken {
                    max_ngram = max_ngram.max(*n);
                }
                Compiled {
                    written: t.written.clone(),
                    spoken,
                    threshold: t.threshold,
                    case_sensitive: t.case_sensitive,
                    listed: t
                        .spoken
                        .iter()
                        .map(|s| s.trim().to_string())
                        .chain(std::iter::once(t.written.trim().to_string()))
                        .collect(),
                }
            })
            .collect();

        // Flatten every spoken form and index it. Soundex is computed once per
        // form here and never again: precomputing phonetic keys at load is what
        // keeps correction sub-millisecond.
        let mut forms = Vec::new();
        let mut exact = HashMap::new();
        let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_len: HashMap<usize, Vec<usize>> = HashMap::new();
        for (ti, t) in terms.iter().enumerate() {
            for (words, n) in &t.spoken {
                if n.is_empty() {
                    continue;
                }
                let idx = forms.len();
                let key = soundex(n);
                exact.entry(n.clone()).or_insert(idx);
                by_key.entry(key.clone()).or_default().push(idx);
                by_len.entry(n.chars().count()).or_default().push(idx);
                forms.push(Form {
                    term: ti,
                    words: *words,
                    norm: n.clone(),
                    key,
                });
            }
        }
        let written_forms = terms
            .iter()
            .enumerate()
            .map(|(i, t)| (t.written.to_lowercase(), i))
            .collect();
        Self {
            terms,
            forms,
            exact,
            by_key,
            by_len,
            max_ngram: max_ngram.min(MAX_NGRAM),
            written_forms,
        }
    }

    pub fn from_json(json: &str) -> crate::Result<Self> {
        let file: TermFile = serde_json::from_str(json)
            .map_err(|e| crate::Error::Audio(format!("dictionary: {e}")))?;
        Ok(Self::new(&file))
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The written forms, for engines that accept vocabulary biasing and for
    /// the Tier 1 prompt.
    pub fn written_forms(&self) -> Vec<&str> {
        self.terms.iter().map(|t| t.written.as_str()).collect()
    }

    /// Best matching term for a normalised n-gram, or None.
    ///
    /// Exact first through the hash index, then a fuzzy pass bounded to
    /// phonetic neighbours and to candidates close enough in length to clear
    /// the threshold. The naive version scored every term for every n-gram and
    /// took 30 ms for 500 terms against a 1 ms budget.
    fn best_match(&self, key: &str, words: usize) -> Option<&Compiled> {
        if let Some(&i) = self.exact.get(key) {
            if self.forms[i].words == words {
                return Some(&self.terms[self.forms[i].term]);
            }
        }

        let klen = key.chars().count();
        let ksdx = soundex(key);
        let kfirst = key.as_bytes().first().copied();

        let mut best: Option<(&Compiled, f32)> = None;

        // Two index paths, scanned directly. Duplicates between them are
        // harmless because the best score wins, which is why no merged and
        // deduplicated candidate list is built -- allocating and sorting one
        // per n-gram cost more than the scoring did.
        let slack = ((1.0 - DEFAULT_THRESHOLD) * klen as f32).ceil() as usize + 1;
        let phonetic = self.by_key.get(&ksdx).map(Vec::as_slice).unwrap_or(&[]);
        let lengths = (klen.saturating_sub(slack)..=klen + slack)
            .filter_map(|l| self.by_len.get(&l).map(Vec::as_slice));

        for i in phonetic.iter().copied().chain(lengths.flatten().copied()) {
            let f = &self.forms[i];
            if f.words != words {
                continue;
            }
            // Cheap guard before the distance call: every one of the eleven
            // measured mishearings shares its first character with its target,
            // and Soundex requires that too.
            if f.norm.as_bytes().first().copied() != kfirst {
                continue;
            }
            let t = &self.terms[f.term];

            // Length guard, and it is load-bearing. A Soundex key ignores
            // vowels and truncates to four characters, so "proxmoxlxc" and
            // "proxmox" share one -- which let a two-word n-gram match a
            // two-word spoken form and SWALLOW the adjacent word, turning
            // "Proxmox LXC over Tailscale" into "Proxmox over Tailscale". A
            // deletion the user cannot see is the worst failure this layer has.
            //
            // The bound also stops "PostgreSQL" being rewritten to "Postgres":
            // that is a legitimate alternative name, not a mishearing.
            let (la, lb) = (f.norm.chars().count(), key.chars().count());
            let allowed = ((la.min(lb) as f32) * 0.2).max(1.0);
            if la.abs_diff(lb) as f32 > allowed {
                continue;
            }

            // 0.6 is the floor below which nothing can match, so the cutoff
            // abandons the computation there rather than finishing it.
            let Some(sim) = similarity_at_least(&f.norm, key, 0.6) else {
                continue;
            };
            // Soundex agreement AND distance above threshold. This was an OR
            // while fuzzy matching carried the load; now that every seeded term
            // lists its observed spoken forms, the exact path handles those and
            // fuzzy only catches what nobody wrote down.
            //
            // The OR had to go because no threshold separates the cases:
            // "ogam" -> "Ogham" and "postgresql" -> "Postgres" are BOTH 0.800
            // similar, and the first must fire while the second must not.
            if f.key == ksdx && sim >= t.threshold && best.map(|(_, s)| sim > s).unwrap_or(true) {
                best = Some((t, sim));
            }
        }

        best.map(|(t, _)| t)
    }

    /// Correct a transcript. Longest n-grams win, so "kube control" is tried
    /// before "kube". N-grams are built from logical words, not raw
    /// whitespace tokens: `group_words` folds a run of two or more
    /// spelled-out letters ("cube c t l") into one logical word, so that
    /// phrase is two words wide rather than four and still fits inside an
    /// n-gram window bounded by `MAX_NGRAM`, the same as any other spoken
    /// form.
    pub fn correct(&self, text: &str) -> String {
        if self.terms.is_empty() {
            return text.to_string();
        }
        let words: Vec<&str> = text.split_whitespace().collect();
        let groups = group_words(&words);
        let mut out: Vec<String> = Vec::with_capacity(words.len());
        let mut gi = 0;
        while gi < groups.len() {
            let mut matched = false;
            let max = self.max_ngram.min(groups.len() - gi);
            for n in (1..=max).rev() {
                let start = groups[gi].start;
                let end = groups[gi + n - 1].end;
                let slice = &words[start..end];
                let key = norm(&slice.join(" "));
                if key.is_empty() {
                    continue;
                }
                if let Some(t) = self.best_match(&key, n) {
                    // A case-sensitive term fires only on an input written,
                    // apart from its surrounding punctuation, as one of its
                    // spoken forms or its written form ([`written_as_listed`]):
                    // "om" is left alone when Ogham lists "OM" and "Om", and
                    // "it" when a term "IT" lists only "I T". So such a term
                    // gets no fuzzy fallback. Any other case tries a shorter
                    // n-gram, or passes through.
                    if t.case_sensitive {
                        let joined = slice.join(" ");
                        let as_said = joined.trim_matches(|c: char| !c.is_alphanumeric());
                        if !written_as_listed(&t.listed, as_said) {
                            continue;
                        }
                    }
                    // Carry surrounding punctuation through: "postgress."
                    // keeps its full stop, "(postgress)" keeps both
                    // parentheses, and a spelled-out run keeps whatever
                    // punctuation led its first letter or trailed its last
                    // ("l."). Symmetric on both ends, because a model reply
                    // wraps a term in backticks or bold on both sides, not
                    // just after it.
                    let head: String = slice
                        .first()
                        .map(|w| w.chars().take_while(|c| !c.is_alphanumeric()).collect())
                        .unwrap_or_default();
                    let tail: String = slice
                        .last()
                        .map(|w| {
                            w.chars()
                                .rev()
                                .take_while(|c| !c.is_alphanumeric())
                                .collect()
                        })
                        .unwrap_or_default();
                    let tail: String = tail.chars().rev().collect();
                    // Except what the written form already has at that end:
                    // ".NET" typed as ".NET" carries its own full stop, and
                    // carrying it again wrote "..NET" ("C#" became "C##").
                    let written = t.written.as_str();
                    let own_head: String = written
                        .chars()
                        .take_while(|c| !c.is_alphanumeric())
                        .collect();
                    let own_tail = &written[written
                        .trim_end_matches(|c: char| !c.is_alphanumeric())
                        .len()..];
                    let head = head.strip_suffix(own_head.as_str()).unwrap_or(&head);
                    let tail = tail.strip_prefix(own_tail).unwrap_or(&tail);
                    out.push(format!("{head}{written}{tail}"));
                    gi += n;
                    matched = true;
                    break;
                }
            }
            if !matched {
                // Nothing matched, not even the merged run by itself: pass
                // the original tokens through exactly as they were tokenised,
                // never as the merged form, so "a b" with no such term stays
                // two words.
                for w in &words[groups[gi].start..groups[gi].end] {
                    out.push((*w).to_string());
                }
                gi += 1;
            }
        }
        out.join(" ")
    }

    /// Put back the exact case of any dictionary term a later stage re-cased.
    ///
    /// "kubectl" is lowercase by convention and "Kubectl" is wrong, so
    /// another stage's own case rules must not win over an explicit written
    /// form. Only the case is touched: a word is replaced only when it
    /// already matches a written form ignoring case, and, for a case-sensitive
    /// term, a word cased like an ordinary word ("it", "It") only when it is
    /// also written as that term lists it ([`written_as_listed`]), so a term
    /// "IT" does not turn "it" into "IT" after `correct` left it alone. Any
    /// other casing ("PGVector", "VNET") is put back, including an ordinary
    /// word in capitals: "GO" becomes a term's "Go", which `correct` itself
    /// would have left alone. Shared by
    /// [`crate::pipeline::Pipeline`], which runs it after Tier 0's sentence
    /// case, and [`Dictionary::settle`], which runs it after Tier 1.
    pub(crate) fn restore_case(&self, text: &str) -> String {
        if self.written_forms.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut word = String::new();
        for ch in text.chars() {
            if ch.is_alphanumeric() || ch == '-' || ch == '_' {
                word.push(ch);
                continue;
            }
            self.push_case(&mut out, &word);
            word.clear();
            out.push(ch);
        }
        self.push_case(&mut out, &word);
        out
    }

    fn push_case(&self, out: &mut String, word: &str) {
        if word.is_empty() {
            return;
        }
        // For a case-sensitive term, only a word shaped like an ordinary word
        // ("it", "It") needs to be written as the term lists it before it is
        // re-cased; any other shape ("PGVector", "VNET") can only be the
        // term, re-cased by another stage, so it is put back.
        match self.written_forms.get(&word.to_lowercase()) {
            Some(&i)
                if !self.terms[i].case_sensitive
                    || !shaped_like_an_ordinary_word(word)
                    || written_as_listed(&self.terms[i].listed, word) =>
            {
                out.push_str(&self.terms[i].written)
            }
            _ => out.push_str(word),
        }
    }

    /// The dictionary's last word: what `macalla enhance` runs on Tier 1's
    /// accepted output, since nothing else applies the dictionary after it.
    /// A free-form reply may respell a term the model was not told to
    /// preserve exactly -- a person's own prompt may change
    /// up to 40% of the words -- and Tier 1's own case and sentence rules can
    /// re-case a term it was told to keep, the same way Tier 0's sentence
    /// case does. So this runs the full dictionary -- [`Dictionary::correct`]
    /// -- over each block's text exactly as it would over a fresh transcript,
    /// so a word a person's own prompt added is corrected the same as a
    /// spoken one, then [`Dictionary::restore_case`] for the written form's
    /// exact case, keeping each block's kind. A dictionary with no terms
    /// returns the blocks unchanged.
    pub fn settle(&self, blocks: &[Block]) -> Vec<Block> {
        if self.terms.is_empty() {
            return blocks.to_vec();
        }
        blocks.iter().map(|b| self.settle_one(b)).collect()
    }

    /// One block through [`Dictionary::settle`], built fresh from the
    /// input's kind rather than cloned and overwritten.
    fn settle_one(&self, block: &Block) -> Block {
        let text = self.restore_case(&self.correct(block.text()));
        match block {
            Block::Paragraph { .. } => Block::Paragraph { text },
            Block::ListItem { ordered, .. } => Block::ListItem {
                text,
                ordered: *ordered,
            },
            Block::Heading { .. } => Block::Heading { text },
        }
    }
}

#[cfg(test)]
mod tests;
