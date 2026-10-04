// SPDX-License-Identifier: MIT
//! Write turns as SubRip (.srt): a plain subtitle format almost every video
//! and caption tool reads, alongside [`crate::vtt::write`]'s WebVTT.
//!
//! [`write`] follows the same rules as [`crate::vtt::write`] wherever SRT
//! lets it: turns with no words left after collapsing whitespace are
//! skipped, and an end before its start is clamped to the start. SRT has no
//! voice span, so a named turn's speaker is written as a `"Name: "` prefix
//! in front of the words (none when the name is empty or only
//! whitespace) -- data joined with `": "`, the same shape
//! `transcript_text` in the desktop app already builds for plain text, not
//! a sentence for a person to read.
//!
//! No reader: nothing in Macalla needs to read an SRT file back, so unlike
//! [`crate::vtt`] this module is write-only.
//!
//! A cue's text is written exactly as its `Turn` carries it, past
//! whitespace collapsing: nothing is stripped or escaped. If it holds
//! `<i>`, `<b>` or another tag some SRT viewers read as styling (the
//! long-standing, unofficial "extended SRT" convention), that viewer shows
//! it as formatting, not as the literal characters. A `Turn`'s text is
//! ordinarily plain -- [`crate::vtt::parse`] already strips such tags from
//! anything it reads -- so this only matters for text built some other way.

use crate::vtt::{collapse_ascii_whitespace, Turn};

/// `turns` as an SRT file: a 1-based cue counter, then
/// `HH:MM:SS,mmm --> HH:MM:SS,mmm`, then the cue's text, with a blank line
/// between cues and none after the last one.
pub fn write(turns: &[Turn]) -> String {
    let mut out = String::new();
    let mut counter = 1u64;
    for turn in turns {
        let text = collapse_ascii_whitespace(&turn.text);
        if text.is_empty() {
            continue;
        }
        if counter > 1 {
            out.push('\n');
        }
        out.push_str(&counter.to_string());
        out.push('\n');
        out.push_str(&timestamp(turn.start_ms));
        out.push_str(" --> ");
        out.push_str(&timestamp(turn.end_ms.max(turn.start_ms)));
        out.push('\n');
        // A speaker that is empty, or only whitespace, is no speaker: the
        // cue carries the words alone, never a bare ": ".
        let name = turn
            .speaker
            .as_deref()
            .map(collapse_ascii_whitespace)
            .filter(|name| !name.is_empty());
        if let Some(name) = name {
            out.push_str(&name);
            out.push_str(": ");
        }
        out.push_str(&text);
        out.push('\n');
        counter += 1;
    }
    out
}

/// A time as SRT writes it: hours, minutes, seconds and milliseconds, a
/// comma before the milliseconds where WebVTT uses a dot.
fn timestamp(ms: u64) -> String {
    let (hours, minutes) = (ms / 3_600_000, ms / 60_000 % 60);
    let (seconds, millis) = (ms / 1_000 % 60, ms % 1_000);
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(speaker: Option<&str>, start_ms: u64, end_ms: u64, text: &str) -> Turn {
        Turn {
            speaker: speaker.map(str::to_string),
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    #[test]
    fn a_golden_file_with_named_and_unnamed_turns() {
        let turns = vec![
            turn(Some("Aoife Byrne"), 1_200, 3_450, "We ship on Friday."),
            turn(None, 3_600, 5_000, "Agreed."),
            turn(Some("Brian O'Neill"), 3_723_004, 3_724_000, "An hour in."),
        ];
        let expected = "1\n\
                        00:00:01,200 --> 00:00:03,450\n\
                        Aoife Byrne: We ship on Friday.\n\
                        \n\
                        2\n\
                        00:00:03,600 --> 00:00:05,000\n\
                        Agreed.\n\
                        \n\
                        3\n\
                        01:02:03,004 --> 01:02:04,000\n\
                        Brian O'Neill: An hour in.\n";
        assert_eq!(write(&turns), expected);
    }

    #[test]
    fn empty_text_turns_are_skipped() {
        let turns = vec![
            turn(Some("Aoife"), 0, 1_000, "Hello."),
            turn(None, 1_000, 2_000, "   "),
            turn(Some("Brian"), 2_000, 3_000, "Fine."),
        ];
        let expected = "1\n\
                        00:00:00,000 --> 00:00:01,000\n\
                        Aoife: Hello.\n\
                        \n\
                        2\n\
                        00:00:02,000 --> 00:00:03,000\n\
                        Brian: Fine.\n";
        assert_eq!(write(&turns), expected);
    }

    #[test]
    fn an_end_before_the_start_is_clamped_to_the_start() {
        let turns = vec![turn(None, 5_000, 2_000, "Backwards")];
        assert_eq!(
            write(&turns),
            "1\n00:00:05,000 --> 00:00:05,000\nBackwards\n"
        );
    }

    #[test]
    fn no_turns_writes_nothing() {
        assert_eq!(write(&[]), "");
    }

    #[test]
    fn an_empty_or_blank_speaker_writes_the_words_alone() {
        let turns = vec![
            turn(Some(""), 0, 1_000, "Hello."),
            turn(Some(" \t\n "), 1_000, 2_000, "Fine."),
        ];
        let expected = "1\n\
                        00:00:00,000 --> 00:00:01,000\n\
                        Hello.\n\
                        \n\
                        2\n\
                        00:00:01,000 --> 00:00:02,000\n\
                        Fine.\n";
        assert_eq!(write(&turns), expected);
    }

    #[test]
    fn whitespace_in_text_and_speaker_is_collapsed() {
        let turns = vec![turn(
            Some("Aoife  Byrne"),
            0,
            1_000,
            "We   ship\non   Friday.",
        )];
        assert_eq!(
            write(&turns),
            "1\n00:00:00,000 --> 00:00:01,000\nAoife Byrne: We ship on Friday.\n"
        );
    }
}
