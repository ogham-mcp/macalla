// SPDX-License-Identifier: MIT
//! List cadence: "one …, two …, three …" said aloud is a list.
//!
//! Runs over the assembled document rather than per segment, because the
//! pauses between items are exactly where an engine cuts, and a single
//! segment then holds one marker -- never enough on its own.
//!
//! The rules, all deterministic:
//!
//! - Markers are the language's number words or ordinal words, one family
//!   per list, in order from the first. An engine's own number formatting is
//!   accepted too ("1st", "2nd", "3"), since Apple writes "my 1st dictation".
//! - Every marker must be followed by at least one word that is not the next
//!   marker: "one, two, three" in a row is counting, not a list.
//! - At least `minimum_markers` in sequence, or the text is left alone. The
//!   English table says three: two is ordinary speech about numbers.
//! - With an engine that punctuates, a marker must open the text or follow
//!   punctuation. "I have one apple, two pears and three oranges" stays a
//!   sentence. An engine that does not punctuate gives nothing to check, and
//!   that sentence becomes a list; the minimum is the only guard there.
//!
//! The text before the first marker stays a paragraph. Each item drops its
//! marker and any comma or colon glued to it, opens with a capital, and loses
//! a trailing comma or semicolon, which are item separators, not content.

use super::{bare, capitalise_first};
use crate::block::Block;

const BOUNDARY: &[char] = &['.', ',', '?', '!', ':', ';', '—'];

/// One family of markers, in order from the first.
struct Family {
    words: Vec<String>,
}

impl Family {
    fn number(words: &[String]) -> Self {
        Self {
            words: words.to_vec(),
        }
    }

    fn ordinal(words: &[String]) -> Self {
        Self {
            words: words.to_vec(),
        }
    }

    /// Which marker this word is, if any. The engine's own digit forms count
    /// alongside the spoken words.
    fn index_of(&self, word: &str, digits: fn(usize) -> String) -> Option<usize> {
        let w = bare(word);
        if w.is_empty() {
            return None;
        }
        self.words.iter().position(|m| *m == w).or_else(|| {
            (1..=self.words.len())
                .find(|n| digits(*n) == w)
                .map(|n| n - 1)
        })
    }
}

fn cardinal_digits(n: usize) -> String {
    n.to_string()
}

fn ordinal_digits(n: usize) -> String {
    let suffix = match n % 100 {
        11..=13 => "th",
        _ => match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{n}{suffix}")
}

pub(super) struct Cadence<'a> {
    pub numbers: &'a [String],
    pub ordinals: &'a [String],
    pub minimum: usize,
    pub engine_punctuates: bool,
}

impl Cadence<'_> {
    /// Split every paragraph that carries a cadence into its list.
    pub fn apply(&self, blocks: Vec<Block>) -> Vec<Block> {
        let mut out = Vec::with_capacity(blocks.len());
        for block in blocks {
            match block {
                Block::Paragraph { text } => match self.split(&text) {
                    Some(list) => out.extend(list),
                    None => out.push(Block::Paragraph { text }),
                },
                other => out.push(other),
            }
        }
        out
    }

    fn split(&self, text: &str) -> Option<Vec<Block>> {
        if self.minimum == 0 || (self.numbers.is_empty() && self.ordinals.is_empty()) {
            return None;
        }
        let words: Vec<&str> = text.split_whitespace().collect();
        let families = [
            (
                Family::number(self.numbers),
                cardinal_digits as fn(usize) -> String,
            ),
            (Family::ordinal(self.ordinals), ordinal_digits),
        ];
        for (family, digits) in &families {
            if family.words.is_empty() {
                continue;
            }
            if let Some(chain) = self.chain(&words, family, *digits) {
                return Some(self.cut(&words, &chain));
            }
        }
        None
    }

    fn at_boundary(&self, words: &[&str], i: usize) -> bool {
        !self.engine_punctuates || i == 0 || words[i - 1].ends_with(BOUNDARY)
    }

    /// The positions of the first qualifying marker chain, if any.
    fn chain(
        &self,
        words: &[&str],
        family: &Family,
        digits: fn(usize) -> String,
    ) -> Option<Vec<usize>> {
        for start in 0..words.len() {
            if family.index_of(words[start], digits) != Some(0) || !self.at_boundary(words, start) {
                continue;
            }
            let mut chain = vec![start];
            let mut expected = 1;
            for j in start + 1..words.len() {
                let is_next = family.index_of(words[j], digits) == Some(expected);
                let has_content = j > chain[chain.len() - 1] + 1;
                if is_next && has_content && self.at_boundary(words, j) {
                    chain.push(j);
                    expected += 1;
                }
            }
            let last_has_content = chain[chain.len() - 1] + 1 < words.len();
            if chain.len() >= self.minimum && last_has_content {
                return Some(chain);
            }
        }
        None
    }

    fn cut(&self, words: &[&str], chain: &[usize]) -> Vec<Block> {
        let mut out = Vec::new();
        let before = words[..chain[0]].join(" ");
        if !before.trim().is_empty() {
            out.push(Block::Paragraph { text: before });
        }
        for (k, &m) in chain.iter().enumerate() {
            let end = chain.get(k + 1).copied().unwrap_or(words.len());
            let mut item = words[m + 1..end].join(" ");
            while item.ends_with([',', ';']) {
                item.pop();
            }
            let item = capitalise_first(item.trim());
            if item.is_empty() {
                continue;
            }
            out.push(Block::ListItem {
                text: item,
                ordered: true,
            });
        }
        out
    }
}
