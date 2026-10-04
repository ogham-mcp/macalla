// SPDX-License-Identifier: MIT
//! A meeting's turns cut into parts, each within a budget of characters.
//!
//! Whatever sends a meeting to a model part by part cuts it here: its summary
//! ([`crate::meeting_summary`]) and its cleanup. One rule, so the two cannot
//! size a part differently.
//!
//! A turn goes whole when it fits. Otherwise it is cut at sentence ends, and a
//! sentence too long on its own at the spaces between its words. Nothing
//! smaller than a word is cut, so a word longer than the budget is still sent,
//! alone.

use crate::vtt::Turn;

/// Whether each line is sent with its speaker's name in front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Labels {
    /// `Name: words`, with the name counted against the budget: the summary,
    /// which needs to know who said what.
    Named,
    /// The words alone: cleanup, which changes only the words.
    Bare,
}

/// One line of a part: a turn, or a piece of a turn too long to send whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line<'a> {
    /// The turn it came from, by its index in the turns given.
    pub turn: usize,
    pub speaker: Option<&'a str>,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

impl Line<'_> {
    /// The line as it is sent: `Name: words` when named and the turn has a
    /// speaker, the words alone otherwise.
    pub fn sent(&self, labels: Labels) -> String {
        label(self.speaker, &self.text, labels)
    }
}

/// `turns` as parts, in meeting order. A part's lines, sent one per line,
/// come to at most `budget` characters, newlines included; only a single word
/// longer than the budget makes a part longer.
pub fn split(turns: &[Turn], budget: usize, labels: Labels) -> Vec<Vec<Line<'_>>> {
    let mut parts = Vec::new();
    let mut current: Vec<Line<'_>> = Vec::new();
    let mut current_chars = 0;
    for (index, turn) in turns.iter().enumerate() {
        for line in lines_of(index, turn, budget, labels) {
            let chars = line.sent(labels).chars().count();
            if !current.is_empty() && current_chars + 1 + chars > budget {
                parts.push(std::mem::take(&mut current));
                current_chars = 0;
            }
            if !current.is_empty() {
                current_chars += 1;
            }
            current_chars += chars;
            current.push(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// A turn as the lines a part carries: whole when it fits the budget with its
/// label, otherwise in pieces cut at sentence ends, and a sentence too long
/// on its own at the spaces between its words.
fn lines_of(index: usize, turn: &Turn, budget: usize, labels: Labels) -> Vec<Line<'_>> {
    let speaker = turn.speaker.as_deref();
    let room = budget.saturating_sub(label(speaker, "", labels).chars().count());
    let mut chunks: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for sentence in sentences(&turn.text) {
        let units: Vec<&str> = if sentence.chars().count() <= room {
            vec![sentence]
        } else {
            sentence.split_whitespace().collect()
        };
        for unit in units {
            if !chunk.is_empty() && chunk.chars().count() + 1 + unit.chars().count() > room {
                chunks.push(std::mem::take(&mut chunk));
            }
            if !chunk.is_empty() {
                chunk.push(' ');
            }
            chunk.push_str(unit);
        }
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
        .into_iter()
        .map(|text| Line {
            turn: index,
            speaker,
            start_ms: turn.start_ms,
            end_ms: turn.end_ms,
            text,
        })
        .collect()
}

/// A line as it is sent: `Name: words` when named, or the words alone.
fn label(speaker: Option<&str>, text: &str, labels: Labels) -> String {
    match (labels, speaker) {
        (Labels::Named, Some(name)) => format!("{name}: {text}"),
        _ => text.to_string(),
    }
}

/// Sentences, each keeping its closing mark: a cut after `.`, `!`, `?` or `…`
/// followed by whitespace or the end of the text.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if matches!(c, '.' | '!' | '?' | '…')
            && chars.peek().is_none_or(|(_, next)| next.is_whitespace())
        {
            let end = at + c.len_utf8();
            let sentence = text[start..end].trim();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            start = end;
        }
    }
    let rest = text[start..].trim();
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(speaker: Option<&str>, text: &str) -> Turn {
        Turn {
            speaker: speaker.map(str::to_string),
            start_ms: 1_000,
            end_ms: 2_000,
            text: text.into(),
        }
    }

    fn sent_chars(part: &[Line<'_>], labels: Labels) -> usize {
        part.iter()
            .map(|line| line.sent(labels).chars().count())
            .sum::<usize>()
            + part.len().saturating_sub(1)
    }

    #[test]
    fn turns_that_fit_go_whole_in_order_each_with_its_turn() {
        let turns = [
            turn(Some("Ann"), "One two."),
            turn(Some("Bob"), "Three four."),
        ];

        let parts = split(&turns, 100, Labels::Named);

        assert_eq!(parts.len(), 1);
        let lines = &parts[0];
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].turn, lines[1].turn), (0, 1));
        assert_eq!(lines[0].sent(Labels::Named), "Ann: One two.");
        assert_eq!(lines[1].sent(Labels::Bare), "Three four.");
        assert_eq!((lines[0].start_ms, lines[0].end_ms), (1_000, 2_000));
    }

    #[test]
    fn a_part_never_passes_the_budget_counting_the_newlines() {
        // Ten lines of ten characters: three fit in 32 with their two
        // newlines, and a fourth would take it to 43.
        let turns: Vec<Turn> = (0..10).map(|_| turn(None, "abcdefghi.")).collect();

        let parts = split(&turns, 32, Labels::Bare);

        let sizes: Vec<usize> = parts.iter().map(Vec::len).collect();
        assert_eq!(sizes, [3, 3, 3, 1]);
        assert!(parts
            .iter()
            .all(|part| sent_chars(part, Labels::Bare) <= 32));
        let order: Vec<usize> = parts.iter().flatten().map(|line| line.turn).collect();
        assert_eq!(order, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn a_turn_too_long_for_one_part_is_cut_at_sentence_ends_and_keeps_its_turn() {
        let text = "First sentence here. Second sentence here. Third one.";
        let turns = [turn(None, text)];

        let parts = split(&turns, 25, Labels::Bare);

        let lines: Vec<&Line<'_>> = parts.iter().flatten().collect();
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "First sentence here.",
                "Second sentence here.",
                "Third one."
            ]
        );
        assert!(lines.iter().all(|line| line.turn == 0));
        assert_eq!(texts.join(" "), text);
        assert!(parts
            .iter()
            .all(|part| sent_chars(part, Labels::Bare) <= 25));
    }

    #[test]
    fn a_name_counts_against_the_budget_only_when_named() {
        // "Speaker: " is nine characters, which leaves six of fifteen for the
        // words when named: "abcd efghi" is ten, so it is cut at the space.
        let turns = [turn(Some("Speaker"), "abcd efghi")];

        let bare: Vec<String> = split(&turns, 15, Labels::Bare)
            .into_iter()
            .flatten()
            .map(|line| line.text)
            .collect();
        let named: Vec<String> = split(&turns, 15, Labels::Named)
            .into_iter()
            .flatten()
            .map(|line| line.text)
            .collect();

        assert_eq!(bare, ["abcd efghi"]);
        assert_eq!(named, ["abcd", "efghi"]);
    }

    #[test]
    fn a_word_longer_than_the_budget_is_sent_alone() {
        let turns = [turn(None, "abcdefghijkl"), turn(None, "ok")];

        let parts = split(&turns, 5, Labels::Bare);

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0][0].text, "abcdefghijkl");
        assert_eq!(parts[1][0].text, "ok");
    }
}
