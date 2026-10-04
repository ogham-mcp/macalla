// SPDX-License-Identifier: MIT
//! The no-invention guard for meeting summaries.
//!
//! Every summary point must quote the transcript, and a point whose quote is
//! not found word for word is dropped. The model can still be wrong about
//! what a quote *means*, but it cannot put words in anyone's mouth: whatever
//! survives this guard was said.
//!
//! This is policy only. It takes the transcript and the model's reply as
//! text and does no I/O; the prompt that asks for this format and the call
//! to the provider live elsewhere.
//!
//! # Reply format
//!
//! One point per line, the point first and its quote after it, in quote
//! marks:
//!
//! ```text
//! - Ship the parser by Friday "we need to ship the parser by friday"
//! 2. Testing waits for the parser: “then we test the parser”
//! ```
//!
//! A leading bullet is optional: `-`, `*` or `•`, or a number followed by
//! `.` or `)` and a space. So is a colon or dash between the point and its
//! quote. Blank lines are ignored.
//!
//! # Quote marks
//!
//! These pairs are accepted, because models vary and so do languages:
//!
//! - `"` and `"`, straight double quotes;
//! - `“` and `”`, English curly quotes;
//! - `«` and `»`, guillemets, as Spanish, Italian and French write them;
//! - `„` closed by `“` or `”`, German quotes. After an open `„`, a `“` closes
//!   it rather than opening a new quote.
//!
//! Marks pair left to right, and a close mark must close the quote opened
//! most recently. Different marks can nest; straight marks cannot, so they
//! pair in the order they appear. The quote is the last pair to close, which
//! for a nested quote is the outer one. If a mark on the line is left
//! unpaired, or a close mark has nothing to close, the line has no quote: the
//! guard never guesses which span the model meant. Single quotes are not
//! quote marks, because they double as apostrophes.
//!
//! # Matching
//!
//! Both sides are split into pieces at every character that is not a letter
//! or a digit, and lowercased. The quote is kept when its pieces appear as
//! one contiguous run in the transcript's pieces. So a comma, a capital or a
//! line break does not drop a real quote, and a quote may run across a
//! sentence boundary, but a changed, missing, added or reordered word does
//! drop it.
//!
//! # Speaker labels
//!
//! A meeting is sent to the model one turn per line, as `Name: words`, and a
//! quote is checked against the words without those labels. A quote that
//! starts with exactly one of the part's speaker labels followed by `: ` has
//! that prefix taken off, once, before it is checked
//! ([`guard_summary_labelled`]), so a model that copies a line's label along
//! with the words it quotes is not dropped for the label alone. Nothing else is
//! taken off: not a label anywhere else in the quote, not the label of someone
//! who does not speak in the part, not part of a label or a near match, and
//! not any other word before a colon. What is left must pass every rule above,
//! and it is what the kept point quotes. [`guard_summary`] has no labels, so
//! there a quote that copies a label is not found and is dropped.

/// The fewest words a quote may have and still vouch for a point.
///
/// Short runs of common words occur all over a meeting: "yes", "we will" or
/// "I think so" would be found in almost any transcript, so they would let
/// an invented point through. Four words is the smallest length at which a
/// run starts to pin a point to one place in the conversation. It is a
/// judgement, not a measurement; revisit it against real meeting replies.
///
/// Words are counted as written, separated by whitespace, so "isn't that
/// odd" is three words even though matching splits "isn't" into two pieces.
pub const MIN_QUOTE_WORDS: usize = 4;

/// A point that passed the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPoint {
    /// The point as the model wrote it, without bullet or quote.
    pub text: String,
    /// The quote as the model wrote it, without the quote marks.
    pub quote: String,
    /// Where the quote was found: the index of its first word among the
    /// transcript's words, split as matching splits them (see the module doc,
    /// "Matching"), at the earliest place the quote is found.
    pub at: usize,
}

/// Why a line of the reply was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The line has no complete pair of quote marks, has a mark left
    /// unpaired, or its quote holds no words.
    NoQuote,
    /// The quote has fewer than [`MIN_QUOTE_WORDS`] words.
    TooShort,
    /// The quote's words do not appear, in order and together, in the
    /// transcript.
    NotFound,
}

/// A line of the reply that did not pass the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedPoint {
    /// The line as the model wrote it, trimmed.
    pub line: String,
    pub reason: DropReason,
}

/// What survived the guard, and what did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardedSummary {
    pub kept: Vec<SummaryPoint>,
    pub dropped: Vec<DroppedPoint>,
}

impl GuardedSummary {
    /// How many points were dropped, for the UI to say so.
    pub fn dropped_count(&self) -> usize {
        self.dropped.len()
    }
}

/// Keep only the points in `reply` whose quote is found in `transcript`.
pub fn guard_summary(transcript: &str, reply: &str) -> GuardedSummary {
    guard_summary_labelled(transcript, &[], reply)
}

/// [`guard_summary`] for a part whose speakers are `labels`: a quote that
/// starts with exactly one of them and `: ` is checked without that prefix
/// (see the module doc, "Speaker labels").
pub fn guard_summary_labelled(transcript: &str, labels: &[String], reply: &str) -> GuardedSummary {
    let transcript_pieces = pieces(transcript);
    let mut guarded = GuardedSummary::default();

    for line in reply.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let checked = match split_quote(line) {
            Some((point, quote)) => {
                check_point(&transcript_pieces, labels, &clean_point(point), quote)
            }
            None => Err(DropReason::NoQuote),
        };
        match checked {
            Ok(point) => guarded.kept.push(point),
            Err(reason) => guarded.dropped.push(DroppedPoint {
                line: line.to_string(),
                reason,
            }),
        }
    }

    guarded
}

/// The rule every point passes, whether it came as a line of a reply or as a
/// structured item: its quote, less an exact leading speaker label from
/// `labels`, must hold at least [`MIN_QUOTE_WORDS`] words and be found word for
/// word in `transcript_pieces`. The kept point has `text` as given, the quote
/// as it was checked, and where the quote was found.
///
/// A quote found more than once is placed at its EARLIEST occurrence: the
/// guard knows only that the words were said, not which saying the model
/// read, so it takes the first, as a reader scanning from the top would.
/// A quote that runs across a sentence end, or from one speaker's words into
/// the next, is placed where it starts.
pub(crate) fn check_point(
    transcript_pieces: &[String],
    labels: &[String],
    text: &str,
    quote: &str,
) -> Result<SummaryPoint, DropReason> {
    let quote = without_label(quote.trim(), labels).trim();
    let quote_pieces = pieces(quote);
    if quote_pieces.is_empty() {
        Err(DropReason::NoQuote)
    } else if written_word_count(quote) < MIN_QUOTE_WORDS {
        Err(DropReason::TooShort)
    } else {
        let at = find_run(transcript_pieces, &quote_pieces).ok_or(DropReason::NotFound)?;
        Ok(SummaryPoint {
            text: text.to_string(),
            quote: quote.to_string(),
            at,
        })
    }
}

/// `quote` without a leading `Label: ` for exactly one of `labels`, taken off
/// once; otherwise `quote` as it is.
fn without_label<'a>(quote: &'a str, labels: &[String]) -> &'a str {
    labels
        .iter()
        .filter(|label| !label.is_empty())
        .find_map(|label| quote.strip_prefix(label.as_str())?.strip_prefix(": "))
        .unwrap_or(quote)
}

/// Split a line into the text before its quote and the quote, pairing quote
/// marks as the module documentation describes. `None` when the line has no
/// complete pair, or a mark that does not pair.
fn split_quote(line: &str) -> Option<(&str, &str)> {
    // Quotes opened and not yet closed, innermost last: the mark and where it
    // starts.
    let mut open: Vec<(char, usize)> = Vec::new();
    // The last pair to close: where its open mark and its close mark start.
    let mut last: Option<(usize, usize)> = None;

    for (at, mark) in line.char_indices() {
        let top = open.last().map(|&(open_mark, _)| open_mark);
        let closes = match mark {
            '"' => top == Some('"'),
            '“' => top == Some('„'),
            '”' => matches!(top, Some('“' | '„')),
            '»' => top == Some('«'),
            '«' | '„' => false,
            _ => continue,
        };
        if closes {
            let (_, from) = open.pop()?;
            last = Some((from, at));
        } else if matches!(mark, '”' | '»') {
            // A close mark with nothing to close, or closing the wrong quote.
            return None;
        } else {
            open.push((mark, at));
        }
    }

    if !open.is_empty() {
        return None;
    }
    let (from, to) = last?;
    let open_len = line[from..].chars().next()?.len_utf8();
    Some((&line[..from], &line[from + open_len..to]))
}

/// The point without its bullet or the separator before the quote.
fn clean_point(point: &str) -> String {
    let point = point.trim();
    strip_numbered_bullet(point)
        .unwrap_or(point)
        .trim_start_matches(['-', '*', '•'])
        .trim_end_matches([':', '-', '–', '—'])
        .trim()
        .to_string()
}

/// The point after a numbered bullet such as `1.` or `12)`. The bullet must
/// be followed by whitespace or nothing, so a point that starts with a number
/// such as "3.5 days" keeps it, and has at most three digits, so a point that
/// starts with a year such as "2026." keeps it too.
fn strip_numbered_bullet(point: &str) -> Option<&str> {
    let rest = point.trim_start_matches(|c: char| c.is_ascii_digit());
    if !(1..=3).contains(&(point.len() - rest.len())) {
        return None;
    }
    let rest = rest.strip_prefix(['.', ')'])?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest.trim_start())
}

/// Words as written: whitespace-separated runs holding a letter or digit.
fn written_word_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count()
}

/// Lowercased pieces, split at every character that is not a letter or digit.
/// Shared with the meeting summary, so a name and a quote are matched the
/// same way.
pub(crate) fn pieces(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Where `needle` first appears as one contiguous run in `haystack`: the
/// index of its first piece at the earliest occurrence.
fn find_run(haystack: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSCRIPT: &str = "Okay, so the plan is this. We need to ship the parser by Friday. \
         Then we test the parser over the weekend, and Aoife reviews it on Monday.";

    fn kept_quotes(reply: &str) -> Vec<String> {
        guard_summary(TRANSCRIPT, reply)
            .kept
            .into_iter()
            .map(|p| p.quote)
            .collect()
    }

    fn only_drop(reply: &str) -> DropReason {
        let guarded = guard_summary(TRANSCRIPT, reply);
        assert!(guarded.kept.is_empty(), "{guarded:?}");
        assert_eq!(guarded.dropped_count(), 1, "{guarded:?}");
        guarded.dropped[0].reason
    }

    #[test]
    fn an_exact_quote_is_kept() {
        let guarded = guard_summary(
            TRANSCRIPT,
            "- Ship the parser by Friday \"We need to ship the parser by Friday.\"",
        );
        assert_eq!(
            guarded.kept,
            vec![SummaryPoint {
                text: "Ship the parser by Friday".to_string(),
                quote: "We need to ship the parser by Friday.".to_string(),
                at: 6,
            }]
        );
        assert_eq!(guarded.dropped_count(), 0);
    }

    #[test]
    fn a_punctuation_or_case_difference_is_kept() {
        let quotes = kept_quotes("- Deadline: \"we NEED to ship, the parser by friday\"");
        assert_eq!(quotes, vec!["we NEED to ship, the parser by friday"]);
    }

    #[test]
    fn one_changed_word_is_dropped() {
        let reason = only_drop("- Deadline \"we need to ship the parser by Thursday\"");
        assert_eq!(reason, DropReason::NotFound);
    }

    #[test]
    fn reordered_words_are_dropped() {
        let reason = only_drop("- Deadline \"by Friday we need to ship the parser\"");
        assert_eq!(reason, DropReason::NotFound);
    }

    #[test]
    fn a_quote_may_span_two_sentences() {
        let quotes = kept_quotes("- Testing follows \"the parser by Friday. Then we test\"");
        assert_eq!(quotes, vec!["the parser by Friday. Then we test"]);
    }

    #[test]
    fn a_quote_across_a_sentence_end_is_placed_where_it_starts() {
        // "the" is first said at word 2 ("the plan"), but the run starts at
        // word 10 ("the parser"), before the full stop.
        let guarded = guard_summary(
            TRANSCRIPT,
            "- Testing follows \"the parser by Friday. Then we test\"",
        );
        assert_eq!(guarded.kept.len(), 1, "{guarded:?}");
        assert_eq!(guarded.kept[0].at, 10);
    }

    #[test]
    fn a_quote_said_twice_is_placed_where_it_is_first_said() {
        let transcript = "First, we ship the parser on Friday. Then, as I said, \
                          we ship the parser on Friday.";
        let guarded = guard_summary(transcript, "- Friday \"we ship the parser on Friday\"");
        assert_eq!(guarded.kept.len(), 1, "{guarded:?}");
        assert_eq!(guarded.kept[0].at, 1);

        // The same words are said again from word 11: not that one.
        let pieces = pieces(transcript);
        assert_eq!(&pieces[11..13], ["we", "ship"]);
        let point = check_point(&pieces, &[], "Friday", "we ship the parser on Friday");
        assert_eq!(point.map(|p| p.at), Ok(1));
    }

    #[test]
    fn curly_quotes_are_accepted() {
        let guarded = guard_summary(
            TRANSCRIPT,
            "• Aoife reviews on Monday — “and Aoife reviews it on Monday”",
        );
        assert_eq!(guarded.kept.len(), 1, "{guarded:?}");
        assert_eq!(guarded.kept[0].text, "Aoife reviews on Monday");
        assert_eq!(guarded.kept[0].quote, "and Aoife reviews it on Monday");
    }

    #[test]
    fn a_line_without_a_quote_is_dropped() {
        assert_eq!(
            only_drop("- The team will ship the parser"),
            DropReason::NoQuote
        );
        assert_eq!(only_drop("- An empty quote \"\""), DropReason::NoQuote);
        assert_eq!(
            only_drop("- An unclosed quote \"we need to ship"),
            DropReason::NoQuote
        );
    }

    #[test]
    fn a_short_quote_cannot_vouch_for_a_point() {
        assert_eq!(
            only_drop("- Everyone agreed to double the budget \"the plan is\""),
            DropReason::TooShort
        );
    }

    #[test]
    fn an_empty_reply_keeps_and_drops_nothing() {
        assert_eq!(guard_summary(TRANSCRIPT, ""), GuardedSummary::default());
        assert_eq!(
            guard_summary(TRANSCRIPT, "\n  \n\n"),
            GuardedSummary::default()
        );
    }

    #[test]
    fn a_quote_longer_than_the_transcript_is_dropped() {
        let guarded = guard_summary(
            "we ship on friday",
            "- Ship Friday \"we ship on friday and test on monday\"",
        );
        assert!(guarded.kept.is_empty());
        assert_eq!(guarded.dropped[0].reason, DropReason::NotFound);
    }

    #[test]
    fn kept_and_dropped_points_are_counted_together() {
        let reply = "- Ship the parser by Friday \"We need to ship the parser by Friday\"\n\
                     - Budget doubles \"we agreed to double the budget\"\n\
                     \n\
                     - Testing over the weekend \"test the parser over the weekend\"\n\
                     - Nobody objected";
        let guarded = guard_summary(TRANSCRIPT, reply);
        assert_eq!(guarded.kept.len(), 2, "{guarded:?}");
        assert_eq!(guarded.dropped_count(), 2, "{guarded:?}");
        assert_eq!(
            guarded.dropped.iter().map(|d| d.reason).collect::<Vec<_>>(),
            vec![DropReason::NotFound, DropReason::NoQuote]
        );
        assert_eq!(guarded.dropped[1].line, "- Nobody objected");
    }

    #[test]
    fn a_stray_mark_after_a_complete_pair_is_not_a_quote() {
        // Three marks: a pair, then a stray one. Taking the last two marks as
        // the quote would check words the model never quoted, and keep the
        // point on a real run of transcript words.
        let reply = "- Point \"totally unrelated content\" we test the parser over the weekend \"";
        assert_eq!(only_drop(reply), DropReason::NoQuote);
    }

    #[test]
    fn an_unpaired_close_mark_is_not_a_quote() {
        for reply in [
            "- Point we need to ship the parser by Friday”",
            "- Point we need to ship the parser by Friday»",
            "- Point “we need to ship the parser by Friday” later”",
        ] {
            assert_eq!(only_drop(reply), DropReason::NoQuote, "{reply}");
        }
    }

    #[test]
    fn a_nested_quote_is_read_by_its_marks() {
        // Different marks nest: the outer pair closes last, so the whole outer
        // quote is checked, the embedded marks with it.
        let guarded = guard_summary(
            TRANSCRIPT,
            "- Deadline “we need to \"ship the parser\" by Friday”",
        );
        assert_eq!(guarded.kept.len(), 1, "{guarded:?}");
        assert_eq!(
            guarded.kept[0].quote,
            "we need to \"ship the parser\" by Friday"
        );

        // Straight marks cannot nest: they pair in order, so the last pair is
        // " by Friday", too short to vouch for the point.
        assert_eq!(
            only_drop("- Deadline \"we need to \"ship the parser\" by Friday\""),
            DropReason::TooShort
        );
    }

    #[test]
    fn a_leading_year_is_not_a_numbered_bullet() {
        let guarded = guard_summary(
            TRANSCRIPT,
            "2026. The plan is set \"we need to ship the parser by Friday\"\n\
             123. Test it \"then we test the parser over the weekend\"",
        );
        assert_eq!(
            guarded
                .kept
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            vec!["2026. The plan is set", "Test it"]
        );
    }

    #[test]
    fn a_numbered_bullet_is_not_part_of_the_point() {
        let guarded = guard_summary(
            TRANSCRIPT,
            "1. Ship the parser \"we need to ship the parser by Friday\"\n\
             12) Test it \"then we test the parser over the weekend\"\n\
             3.5 days of testing \"test the parser over the weekend\"",
        );
        assert_eq!(
            guarded
                .kept
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Ship the parser", "Test it", "3.5 days of testing"]
        );
    }

    #[test]
    fn guillemets_and_german_quotes_are_accepted() {
        let guarded = guard_summary(
            TRANSCRIPT,
            "- Entrega el viernes «we need to ship the parser by Friday»\n\
             - Consegna « then we test the parser over the weekend »\n\
             - Montag „and Aoife reviews it on Monday“\n\
             - Montag „and Aoife reviews it on Monday”",
        );
        assert_eq!(
            guarded
                .kept
                .iter()
                .map(|p| p.quote.as_str())
                .collect::<Vec<_>>(),
            vec![
                "we need to ship the parser by Friday",
                "then we test the parser over the weekend",
                "and Aoife reviews it on Monday",
                "and Aoife reviews it on Monday",
            ]
        );
        assert_eq!(guarded.dropped_count(), 0, "{guarded:?}");
    }

    #[test]
    fn the_length_gate_counts_spoken_words_not_pieces_of_them() {
        let transcript = "Well, isn't that odd, said Aoife, and nobody is sure why.";

        // Three words, although matching splits "isn't" into two pieces.
        let short = guard_summary(transcript, "- Surprise \"isn't that odd\"");
        assert_eq!(
            short.dropped.first().map(|d| d.reason),
            Some(DropReason::TooShort),
            "{short:?}"
        );

        let long = guard_summary(transcript, "- Surprise \"isn't that odd, said\"");
        assert_eq!(long.kept.len(), 1, "{long:?}");
    }

    /// The speakers of the part [`TRANSCRIPT`] stands for.
    fn labels() -> Vec<String> {
        vec!["Aoife Byrne".to_string(), "Brian".to_string()]
    }

    fn labelled(reply: &str) -> GuardedSummary {
        guard_summary_labelled(TRANSCRIPT, &labels(), reply)
    }

    #[test]
    fn a_quote_that_starts_with_a_speaker_label_is_checked_without_it() {
        let guarded = labelled("- Deadline \"Aoife Byrne: We need to ship the parser by Friday.\"");

        assert_eq!(
            guarded.kept,
            vec![SummaryPoint {
                text: "Deadline".to_string(),
                quote: "We need to ship the parser by Friday.".to_string(),
                at: 6,
            }]
        );
        assert_eq!(guarded.dropped_count(), 0);
    }

    #[test]
    fn only_an_exact_label_of_this_part_is_taken_off_and_only_at_the_start() {
        for (reply, why) in [
            (
                "- Deadline \"Note: we need to ship the parser by Friday\"",
                "a word before a colon is not a label",
            ),
            (
                "- Deadline \"so the plan is this. Brian: we need to ship the parser\"",
                "a label inside the quote stays",
            ),
            (
                "- Deadline \"Ciara: we need to ship the parser by Friday\"",
                "Ciara does not speak in this part",
            ),
            (
                "- Deadline \"Aoife: we need to ship the parser by Friday\"",
                "part of a label is not the label",
            ),
            (
                "- Deadline \"Brian:we need to ship the parser by Friday\"",
                "the label's form has a space after the colon",
            ),
            (
                "- Deadline \"Brian: Brian: we need to ship the parser by Friday\"",
                "a label is taken off once",
            ),
        ] {
            let guarded = labelled(reply);
            assert!(guarded.kept.is_empty(), "{why}: {guarded:?}");
            assert_eq!(
                guarded.dropped.first().map(|d| d.reason),
                Some(DropReason::NotFound),
                "{why}"
            );
        }
    }

    #[test]
    fn a_labelled_quote_with_a_changed_word_is_still_dropped() {
        let guarded =
            labelled("- Deadline \"Aoife Byrne: We need to ship the parser by Thursday.\"");

        assert!(guarded.kept.is_empty(), "{guarded:?}");
        assert_eq!(guarded.dropped[0].reason, DropReason::NotFound);
    }

    #[test]
    fn a_quote_that_is_only_a_label_is_dropped() {
        for reply in ["- Deadline \"Aoife Byrne: \"", "- Deadline \"Brian:\""] {
            let guarded = labelled(reply);
            assert!(guarded.kept.is_empty(), "{reply}: {guarded:?}");
            assert_eq!(guarded.dropped_count(), 1, "{reply}");
        }
    }

    #[test]
    fn without_the_part_labels_a_labelled_quote_is_dropped_as_before() {
        let guarded = guard_summary(
            TRANSCRIPT,
            "- Deadline \"Aoife Byrne: We need to ship the parser by Friday.\"",
        );

        assert!(guarded.kept.is_empty(), "{guarded:?}");
        assert_eq!(guarded.dropped[0].reason, DropReason::NotFound);
    }

    #[test]
    fn a_structured_item_passes_the_same_check_as_a_line() {
        let transcript = pieces(TRANSCRIPT);

        assert_eq!(
            check_point(
                &transcript,
                &labels(),
                "Deadline",
                "Brian: we need to ship the parser by Friday"
            ),
            Ok(SummaryPoint {
                text: "Deadline".to_string(),
                quote: "we need to ship the parser by Friday".to_string(),
                at: 6,
            })
        );
        assert_eq!(
            check_point(&transcript, &labels(), "Deadline", "the plan is"),
            Err(DropReason::TooShort)
        );
        assert_eq!(
            check_point(&transcript, &labels(), "Deadline", "  "),
            Err(DropReason::NoQuote)
        );
    }

    #[test]
    fn a_label_that_begins_another_label_takes_off_only_its_own_prefix() {
        let transcript = pieces(TRANSCRIPT);
        let quote_after = |labels: &[String], quote: &str| {
            check_point(&transcript, labels, "Deadline", quote).map(|point| point.quote)
        };

        for labels in [
            vec!["Brian".to_string(), "Brian Walsh".to_string()],
            vec!["Brian Walsh".to_string(), "Brian".to_string()],
        ] {
            assert_eq!(
                quote_after(&labels, "Brian Walsh: we need to ship the parser by Friday"),
                Ok("we need to ship the parser by Friday".to_string()),
                "{labels:?}"
            );
            assert_eq!(
                quote_after(&labels, "Brian: we need to ship the parser by Friday"),
                Ok("we need to ship the parser by Friday".to_string()),
                "{labels:?}"
            );
        }
    }
}
