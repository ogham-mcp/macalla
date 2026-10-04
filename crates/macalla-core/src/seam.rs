// SPDX-License-Identifier: MIT
//! The seam pass. Segments are not sentences, whoever produced them.
//!
//! An engine that owns segmentation commits a result at every pause, and a
//! VAD cuts where the speaker breathes. Neither is a sentence boundary, yet
//! each segment arrives as its own block, and rendered one block per line the
//! text reads as if every pause were a paragraph. Seen in the first day of
//! real use: "But otherwise, I mean," on one line, "The detections, I think,
//! are actually forcing new sentences." on the next.
//!
//! This pass rejoins adjacent segments at assembly. It is deterministic and
//! it declines where a rule would be a guess. The only boundaries that
//! survive are the ones the user asked for -- a spoken "new paragraph", "new
//! line", a list command or a heading -- and the ones between blocks of
//! different kinds.
//!
//! What it does at a plain seam, in order:
//!
//! 1. Leading punctuation on the second side is moved onto the first side, or
//!    dropped if the first side already ends with punctuation.
//! 2. After a sentence terminator (`.`, `?`, `!`) the second side starts with
//!    a capital.
//! 3. After anything else the second side is a continuation, and a plain
//!    capitalised word is lowercased. Engines restart their casing at a
//!    boundary even mid-sentence ("I mean," then "The detections"), and that
//!    is the commoner error. A word that carries its own case -- "I" and its
//!    contractions, an acronym, mixed case, digits -- is left alone; a proper
//!    noun that is none of those is the accepted miss, and the dictionary's
//!    written forms are restored after this pass so a known term survives.
//!    After a colon the case is left as it is: both readings are common.
//! 4. The two sides are joined with one space.

use crate::block::Block;
use crate::enhance::{Opening, Segment};

/// Join the segments of one dictation into the document's blocks.
pub fn join(segments: Vec<Segment>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    // A command that closed the previous segment, still to be honoured.
    let mut pending: Option<Opening> = None;

    for segment in segments {
        let Segment {
            blocks,
            opened_by,
            trailing,
        } = segment;
        if blocks.is_empty() {
            // "new paragraph" on its own, or a segment Tier 0 emptied. A
            // command it carried still applies to whatever comes next.
            pending = trailing.or(opened_by).or(pending);
            continue;
        }
        let mut blocks = blocks.into_iter();
        let first = blocks.next().expect("checked non-empty");
        match (pending.take(), opened_by) {
            // The user closed the previous segment with a command: the text
            // that follows opens as that command asked, whatever the engine
            // made of it.
            (Some(command), None) => out.push(reopen(first, command, out.last())),
            // The user opened this segment with a command. Tier 0 already
            // shaped the block; the seam only keeps a list's numbering.
            (_, Some(command)) => out.push(reopen(first, command, out.last())),
            // A plain boundary: the engine's, or the VAD's. Rejoin.
            (None, None) => match out.pop() {
                Some(previous) => match stitch(previous, first) {
                    Ok(joined) => out.push(joined),
                    Err((previous, first)) => {
                        out.push(previous);
                        out.push(first);
                    }
                },
                None => out.push(first),
            },
        }
        out.extend(blocks);
        pending = trailing;
    }
    out
}

/// Give a block the kind a command at the seam asked for.
///
/// Tier 0 could not see across the boundary, so a "next item" that opened a
/// segment became an unordered item even inside a numbered list, and a
/// "numbered list" that closed one left the next segment a paragraph.
fn reopen(block: Block, command: Opening, previous: Option<&Block>) -> Block {
    let text = block.text().to_string();
    match command {
        Opening::Paragraph | Opening::Line => match block {
            Block::Paragraph { .. } => Block::Paragraph { text },
            other => other,
        },
        Opening::Heading => Block::Heading { text },
        Opening::List { ordered } => Block::ListItem { text, ordered },
        Opening::NextItem => {
            let ordered = match (previous, &block) {
                (Some(Block::ListItem { ordered, .. }), _) => *ordered,
                (_, Block::ListItem { ordered, .. }) => *ordered,
                _ => false,
            };
            Block::ListItem { text, ordered }
        }
    }
}

/// Rejoin two blocks that a boundary separated, or hand them back untouched
/// when they are of kinds that do not join.
///
/// A paragraph continues a paragraph, and a paragraph continues a list item
/// -- inside one segment the words after "next item" all belong to that
/// item until the next command, and a pause must not change that. A heading
/// ends at the boundary: it is short, the pause after it is natural, and the
/// text that follows is the body.
fn stitch(previous: Block, next: Block) -> Result<Block, (Block, Block)> {
    match (previous, next) {
        (Block::Paragraph { text: a }, Block::Paragraph { text: b }) => Ok(Block::Paragraph {
            text: stitch_text(&a, &b),
        }),
        (Block::ListItem { text: a, ordered }, Block::Paragraph { text: b }) => {
            Ok(Block::ListItem {
                text: stitch_text(&a, &b),
                ordered,
            })
        }
        (previous, next) => Err((previous, next)),
    }
}

const TERMINATORS: &[char] = &['.', '?', '!'];
const PUNCTUATION: &[char] = &['.', ',', '?', '!', ':', ';', '—', '"'];

/// Join the text of two sides of a seam.
pub fn stitch_text(a: &str, b: &str) -> String {
    let mut a = a.trim_end().to_string();
    let mut b = b.trim_start();

    // 1. Punctuation that opens the second side belongs to the first.
    while let Some(first) = b.chars().next() {
        if !PUNCTUATION.contains(&first) {
            break;
        }
        if !a.ends_with(PUNCTUATION) {
            a.push(first);
        }
        b = b[first.len_utf8()..].trim_start();
    }
    if b.is_empty() {
        return a;
    }
    if a.is_empty() {
        return b.to_string();
    }

    // 2 and 3. Case at the seam.
    let b = match a.chars().last() {
        Some(last) if TERMINATORS.contains(&last) => capitalise_first(b),
        Some(':') => b.to_string(),
        _ => decapitalise_plain_word(b),
    };

    // 4.
    a.push(' ');
    a.push_str(&b);
    a
}

fn capitalise_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Lowercase the first word when its capital can only have come from the
/// engine restarting a sentence. A word whose case means something -- "I",
/// "I'm", "AWS", "iPhone", "1st" -- is left alone.
fn decapitalise_plain_word(s: &str) -> String {
    let word: &str = s.split_whitespace().next().unwrap_or("");
    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return s.to_string();
    };
    let rest = chars.as_str();
    let plain = first.is_uppercase()
        && !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_lowercase() || c == '\'' || c == '’')
        && !is_first_person(word);
    if !plain {
        return s.to_string();
    }
    first.to_lowercase().collect::<String>() + &s[first.len_utf8()..]
}

/// "I" and its contractions keep their capital anywhere in a sentence.
fn is_first_person(word: &str) -> bool {
    let mut chars = word.chars();
    chars.next() == Some('I') && matches!(chars.next(), None | Some('\'') | Some('’'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(text: &str) -> Block {
        Block::Paragraph { text: text.into() }
    }

    fn segment(blocks: Vec<Block>) -> Segment {
        Segment {
            blocks,
            opened_by: None,
            trailing: None,
        }
    }

    #[test]
    fn two_sentences_become_one_paragraph() {
        let out = join(vec![
            segment(vec![para("Hey there, Andres.")]),
            segment(vec![para("I'm talking to you now.")]),
        ]);
        assert_eq!(
            out,
            vec![para("Hey there, Andres. I'm talking to you now.")]
        );
    }

    #[test]
    fn a_sentence_split_by_a_pause_is_rejoined_lowercase() {
        assert_eq!(
            stitch_text("one, two, three, or", "bullet points, then a list."),
            "one, two, three, or bullet points, then a list."
        );
    }

    #[test]
    fn a_capital_after_a_comma_is_the_engine_restarting() {
        assert_eq!(
            stitch_text(
                "But otherwise, I mean,",
                "The detections are forcing new sentences."
            ),
            "But otherwise, I mean, the detections are forcing new sentences."
        );
    }

    #[test]
    fn a_lowercase_start_after_a_full_stop_gets_its_capital() {
        assert_eq!(
            stitch_text("Well, I'm typing to you now.", "courtesy of Macalla."),
            "Well, I'm typing to you now. Courtesy of Macalla."
        );
    }

    #[test]
    fn words_that_own_their_case_keep_it() {
        for word in ["I", "I'm", "I’ve", "AWS", "iPhone", "1st", "HCL"] {
            let joined = stitch_text("and then", &format!("{word} did it"));
            assert_eq!(joined, format!("and then {word} did it"));
        }
    }

    #[test]
    fn contractions_are_plain_words() {
        assert_eq!(
            stitch_text("we tried,", "Don't do that"),
            "we tried, don't do that"
        );
    }

    #[test]
    fn after_a_colon_the_case_is_left_alone() {
        assert_eq!(
            stitch_text("Two things:", "First, the"),
            "Two things: First, the"
        );
        assert_eq!(
            stitch_text("Two things:", "first, the"),
            "Two things: first, the"
        );
    }

    #[test]
    fn leading_punctuation_moves_across_the_seam() {
        assert_eq!(stitch_text("ship it", ". Then rest"), "ship it. Then rest");
        assert_eq!(stitch_text("ship it.", ". Then rest"), "ship it. Then rest");
        assert_eq!(stitch_text("ship it", ", then rest"), "ship it, then rest");
    }

    #[test]
    fn a_trailing_command_keeps_the_next_segment_apart() {
        let out = join(vec![
            Segment {
                blocks: vec![para("We shipped it.")],
                opened_by: None,
                trailing: Some(Opening::Paragraph),
            },
            segment(vec![para("The tests pass.")]),
        ]);
        assert_eq!(out, vec![para("We shipped it."), para("The tests pass.")]);
    }

    #[test]
    fn a_leading_command_keeps_the_segments_apart() {
        let out = join(vec![
            segment(vec![para("We shipped it.")]),
            Segment {
                blocks: vec![para("The tests pass.")],
                opened_by: Some(Opening::Paragraph),
                trailing: None,
            },
        ]);
        assert_eq!(out, vec![para("We shipped it."), para("The tests pass.")]);
    }

    #[test]
    fn a_segment_that_is_only_a_command_still_counts() {
        let out = join(vec![
            segment(vec![para("We shipped it.")]),
            Segment {
                blocks: vec![],
                opened_by: Some(Opening::Paragraph),
                trailing: Some(Opening::Paragraph),
            },
            segment(vec![para("The tests pass.")]),
        ]);
        assert_eq!(out, vec![para("We shipped it."), para("The tests pass.")]);
    }

    #[test]
    fn a_trailing_list_command_makes_the_next_segment_an_item() {
        let out = join(vec![
            Segment {
                blocks: vec![para("Two things.")],
                opened_by: None,
                trailing: Some(Opening::List { ordered: true }),
            },
            segment(vec![para("Review the spec")]),
            Segment {
                blocks: vec![Block::ListItem {
                    text: "Run the benchmarks".into(),
                    ordered: false,
                }],
                opened_by: Some(Opening::NextItem),
                trailing: None,
            },
        ]);
        assert_eq!(
            out,
            vec![
                para("Two things."),
                Block::ListItem {
                    text: "Review the spec".into(),
                    ordered: true
                },
                Block::ListItem {
                    text: "Run the benchmarks".into(),
                    ordered: true
                },
            ]
        );
    }

    #[test]
    fn a_pause_inside_a_list_item_continues_the_item() {
        let out = join(vec![
            segment(vec![Block::ListItem {
                text: "Apples and".into(),
                ordered: false,
            }]),
            segment(vec![para("pears")]),
        ]);
        assert_eq!(
            out,
            vec![Block::ListItem {
                text: "Apples and pears".into(),
                ordered: false
            }]
        );
    }

    #[test]
    fn a_heading_ends_at_the_pause() {
        let out = join(vec![
            segment(vec![Block::Heading {
                text: "Resource budgets".into(),
            }]),
            segment(vec![para("Idle RSS is low.")]),
        ]);
        assert_eq!(
            out,
            vec![
                Block::Heading {
                    text: "Resource budgets".into()
                },
                para("Idle RSS is low.")
            ]
        );
    }

    #[test]
    fn nothing_in_gives_nothing_out() {
        assert!(join(vec![]).is_empty());
        assert!(join(vec![segment(vec![])]).is_empty());
    }
}
