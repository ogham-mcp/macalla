// SPDX-License-Identifier: MIT
//! Read a WebVTT transcript into timed cues with speaker names.
//!
//! A meeting summary can start from a VTT file the person received from their
//! meeting tool. This module turns that file into cues -- a start and end
//! time, who spoke if the file says, and the words -- reads consecutive cues
//! from one speaker as a turn, and gives the spoken words alone for matching
//! summary quotes against. It also writes turns back out as a file [`parse`]
//! reads to the same speakers, times and words ([`write`]). No I/O.
//!
//! The rules follow the W3C WebVTT Candidate Recommendation Draft of
//! 20 May 2026 (<https://www.w3.org/TR/2026/CRD-webvtt1-20260520/>): the file
//! structure in section 4.1, cue text and the voice span in section 4.2.2,
//! and the parsing algorithms in sections 6.1, 6.3 and 6.4, which this module
//! follows wherever they decide what a reader keeps.
//!
//! # Leniency
//!
//! A file that does not start with the `WEBVTT` signature is an error. Past
//! that, nothing is fatal. A block whose timing line does not parse is skipped
//! and counted in [`Transcript::skipped_blocks`], and so is any other block
//! that is neither a cue nor a `NOTE`, `STYLE` or `REGION` block. Lines
//! straight after the signature line, such as `Kind: captions`, are header
//! and are ignored, as the spec's parser ignores them.
//!
//! # What counts as spoken text
//!
//! Tags are removed. `<i>`, `<b>`, `<u>`, `<c>`, `<lang>` and `<ruby>` keep
//! their text; inline timestamps such as `<00:00:01.000>` disappear; the text
//! of `<rt>`, a reading aid rather than more words, is left out. When the
//! speaker is identified, their name is left out of the text; a name that is
//! not identified stays in the words, as described under Speakers.
//!
//! Character references are decoded: numeric ones (`&#233;`, `&#xE9;`) and
//! the named ones caption files use, `&amp;`, `&lt;`, `&gt;`, `&quot;`,
//! `&apos;`, `&nbsp;`, `&lrm;` and `&rlm;`. Section 4.2.2 allows every HTML
//! named reference; any other name is left as written rather than carrying
//! HTML's table of more than two thousand entries. Whitespace, including a
//! line break inside a cue, collapses to single spaces, and a cue with no
//! words left is dropped.
//!
//! # Speakers
//!
//! A voice span, `<v Name>`, names the speaker. Microsoft Teams writes its
//! transcripts this way, unless an administrator has turned speaker names
//! off. When one cue holds several voice spans, each voice becomes its own
//! cue with the same times.
//!
//! Some files name the speaker in a prefix instead, `Name: words`. A real Zoom
//! transcript export does this on every cue, continuation cues from the same
//! speaker included, and Zoom display names are free text: parentheses, full
//! stops and bars appear in them, and a speaker Zoom could not identify is
//! "Unknown Speaker". The two tools found that were written against real Zoom
//! files take the text before the first colon as the name, with no test of
//! what it looks like.
//! The prefix ends at a colon followed by a space, so a time such as 10:30 or
//! a link does not split a cue.
//!
//! So when every cue that is not voiced has words before a first `: ` and
//! words after it, and there are at least two such cues, the text before the
//! colon is the speaker, whatever it holds.
//!
//! Otherwise ordinary speech may be what holds the colon ("the plan: ship
//! Friday"), and a prefix is read as a speaker only when both of these hold:
//!
//! - It looks like a name: one to four words before the first `: `, with
//!   words after it. It starts with a letter and holds only letters, digits,
//!   apostrophes and hyphens, plus a full stop straight after a word of one
//!   to three letters, as in "Dr." or "J.".
//! - The shape recurs: at least two cues, and at least half of the cues that
//!   are not voiced, start with such a prefix.
//!
//! Otherwise every cue's text is kept whole, and so is a prefix that does not
//! look like a name, such as `Surname, Firstname:`: it stays in the words.
//!
//! A known limit: one cue without a prefix turns the every-cue reading off for
//! the whole file, and a free-text name then stays in the words. The one real
//! export read had a prefix on every cue, but one of those tools treats a line
//! with no colon as the previous speaker's words carrying on, so Zoom may write
//! such lines. A cue's lines are joined before its prefix is read, so a name on
//! a cue's second line would take the first line into the name; no Zoom output
//! read has shown that.
//!
//! A voiced cue is never read for a prefix. A cue is voiced when its text sat
//! inside a voice span, or when its payload holds an empty voice span
//! anywhere: an empty span means the file tried to say who spoke and said
//! nothing, so no text in that cue is trusted for a prefix. Text outside a
//! named span is not voiced, so in `<v Mary>Are you coming?</v> Brian: In a
//! minute.` the words after the span can still be read as Brian's.

/// Why a file could not be read as WebVTT at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VttError {
    #[error("this is not a WebVTT file: it does not start with WEBVTT")]
    MissingHeader,
}

/// A stretch of time, who spoke in it if the file says, and the words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    /// Who spoke: the name in the voice span around the words, or a name
    /// prefix read as the module documentation describes. `None` when the
    /// file does not say.
    pub speaker: Option<String>,
    /// The spoken words: tags removed, references decoded, whitespace
    /// collapsed. Never empty.
    pub text: String,
}

/// Consecutive cues from the same speaker, read as one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub speaker: Option<String>,
    /// The first cue's start.
    pub start_ms: u64,
    /// The latest end among the turn's cues, since cues may overlap.
    pub end_ms: u64,
    /// The cues' words joined with spaces.
    pub text: String,
}

/// A WebVTT file, read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    /// Cues in file order.
    pub cues: Vec<Cue>,
    /// Blocks skipped because they could not be read as a cue.
    pub skipped_blocks: usize,
}

impl Transcript {
    /// Consecutive cues with the same speaker merged into turns, for a summary
    /// to attribute points to people. Consecutive cues with no speaker merge
    /// too.
    pub fn turns(&self) -> Vec<Turn> {
        let mut turns: Vec<Turn> = Vec::new();
        for cue in &self.cues {
            match turns.last_mut() {
                Some(turn) if turn.speaker == cue.speaker => {
                    turn.end_ms = turn.end_ms.max(cue.end_ms);
                    turn.text.push(' ');
                    turn.text.push_str(&cue.text);
                }
                _ => turns.push(Turn {
                    speaker: cue.speaker.clone(),
                    start_ms: cue.start_ms,
                    end_ms: cue.end_ms,
                    text: cue.text.clone(),
                }),
            }
        }
        turns
    }

    /// Every cue's words in order, without speaker names or times: the text a
    /// summary quote is matched against.
    pub fn spoken_text(&self) -> String {
        self.cues
            .iter()
            .map(|cue| cue.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// `turns` as a WebVTT file, one cue per turn, that [`parse`] reads back to
/// the same speakers, times and words: a named turn in a voice span, an
/// unnamed one behind an empty voice span, so no words in it are ever read
/// as a speaker's name, and `&`, `<` and `>` as references, so no words can
/// close a tag or end a timing line. Consecutive turns from one speaker come
/// back as one turn, as any file's do.
pub fn write(turns: &[Turn]) -> String {
    let mut out = String::from("WEBVTT\n");
    for turn in turns {
        let text = escape(&collapse_ascii_whitespace(&turn.text));
        if text.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&timestamp(turn.start_ms));
        out.push_str(" --> ");
        out.push_str(&timestamp(turn.end_ms.max(turn.start_ms)));
        out.push('\n');
        match &turn.speaker {
            Some(name) => {
                out.push_str("<v ");
                out.push_str(&escape(&collapse_ascii_whitespace(name)));
                out.push('>');
            }
            None => out.push_str("<v></v>"),
        }
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// A time as WebVTT writes it: hours, minutes, seconds and milliseconds.
fn timestamp(ms: u64) -> String {
    let (hours, minutes) = (ms / 3_600_000, ms / 60_000 % 60);
    let (seconds, millis) = (ms / 1_000 % 60, ms % 1_000);
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// Cue text with the three characters that could end it or open a tag
/// written as references.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Read a WebVTT file.
pub fn parse(input: &str) -> Result<Transcript, VttError> {
    let text = normalise(input.strip_prefix('\u{feff}').unwrap_or(input));
    check_signature(&text)?;

    let mut lines: Vec<&str> = text.split('\n').collect();
    // A line terminator at the very end closes the last line; it does not
    // start an empty one.
    if text.ends_with('\n') {
        lines.pop();
    }

    let mut transcript = Transcript::default();
    // Section 6.1 steps 7 to 12: past the signature line, then any header
    // lines, which run to the first blank line or the first timing line.
    let mut i = 1;
    if lines.get(i).is_some_and(|line| !line.is_empty()) {
        collect_block(&lines, &mut i, true);
    }

    // For each cue, whether it is voiced for the name-prefix rule, as
    // cues_from_payload decides. The speaker alone cannot say: an empty span
    // also leaves it `None`.
    let mut voiced: Vec<bool> = Vec::new();
    loop {
        while lines.get(i).is_some_and(|line| line.is_empty()) {
            i += 1;
        }
        if i >= lines.len() {
            break;
        }
        match collect_block(&lines, &mut i, false) {
            Block::Cue {
                start_ms,
                end_ms,
                payload,
            } => {
                let (cues, cues_voiced) = cues_from_payload(start_ms, end_ms, &payload);
                voiced.extend(cues_voiced);
                transcript.cues.extend(cues);
            }
            Block::Skipped => transcript.skipped_blocks += 1,
            Block::Ignored => {}
        }
    }

    attribute_name_prefixes(&mut transcript.cues, &voiced);
    Ok(transcript)
}

/// Read `Name: words` prefixes as speakers on the cues that are not voiced,
/// as the module documentation describes: every such cue's prefix, whatever
/// it holds, when there are at least two and all of them have one; otherwise
/// name-shaped prefixes, when that shape recurs. `voiced` says, cue by cue,
/// whether the cue's text sat inside a voice span or its payload held an empty
/// voice span.
fn attribute_name_prefixes(cues: &mut [Cue], voiced: &[bool]) {
    let unvoiced = voiced.iter().filter(|voiced| !**voiced).count();
    let every_cue_prefixed = unvoiced >= 2
        && cues
            .iter()
            .zip(voiced)
            .filter(|(_, voiced)| !**voiced)
            .all(|(cue, _)| speaker_before_first_colon(&cue.text).is_some());
    if every_cue_prefixed {
        for (cue, _) in cues.iter_mut().zip(voiced).filter(|(_, voiced)| !**voiced) {
            if let Some((name, words)) = speaker_before_first_colon(&cue.text) {
                let (name, words) = (name.to_string(), words.to_string());
                cue.speaker = Some(name);
                cue.text = words;
            }
        }
        return;
    }
    let prefixed = cues
        .iter()
        .zip(voiced)
        .filter(|(cue, voiced)| !**voiced && name_prefix(&cue.text).is_some())
        .count();
    if prefixed < 2 || prefixed * 2 < unvoiced {
        return;
    }
    for (cue, _) in cues.iter_mut().zip(voiced).filter(|(_, voiced)| !**voiced) {
        if let Some((name, words)) = name_prefix(&cue.text) {
            let (name, words) = (name.to_string(), words.to_string());
            cue.speaker = Some(name);
            cue.text = words;
        }
    }
}

/// Split `Name: words` when the prefix looks like a name: one to four words
/// before the first `: `, starting with a letter, holding only letters,
/// digits, apostrophes and hyphens, plus a full stop straight after a word of
/// one to three letters. `None` when there is no such prefix, or no words
/// after it.
fn name_prefix(text: &str) -> Option<(&str, &str)> {
    let (name, words) = text.split_once(": ")?;
    let words = words.trim();
    if words.is_empty() || !name.starts_with(char::is_alphabetic) {
        return None;
    }
    let name_words: Vec<&str> = name.split(' ').collect();
    if name_words.len() > 4 {
        return None;
    }
    let looks_like_a_name = name_words.iter().all(|word| {
        let bare = word
            .strip_suffix('.')
            .filter(|bare| (1..=3).contains(&bare.chars().count()))
            .unwrap_or(word);
        !bare.is_empty()
            && bare
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '\'' | '’' | '-'))
    });
    looks_like_a_name.then_some((name, words))
}

/// Split `Name: words` at the first `: `, whatever the name holds, for a file
/// that has such a prefix on every cue. A space before the colon is not part
/// of the name. `None` when there is no `: `, the name is empty or starts with
/// a space, or no words follow.
fn speaker_before_first_colon(text: &str) -> Option<(&str, &str)> {
    let (name, words) = text.split_once(": ")?;
    let name = name.trim_end();
    let words = words.trim();
    if name.is_empty() || name.starts_with(char::is_whitespace) || words.is_empty() {
        return None;
    }
    Some((name, words))
}

/// Section 6.1 step 1: NULs become U+FFFD, and CRLF and lone CR become LF.
fn normalise(input: &str) -> String {
    input
        .replace('\0', "\u{fffd}")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Section 6.1 steps 4 to 6: `WEBVTT`, then the end of the input, a space, a
/// tab or a line break.
fn check_signature(text: &str) -> Result<(), VttError> {
    let rest = text.strip_prefix("WEBVTT").ok_or(VttError::MissingHeader)?;
    match rest.chars().next() {
        None | Some(' ' | '\t' | '\n') => Ok(()),
        Some(_) => Err(VttError::MissingHeader),
    }
}

enum Block {
    Cue {
        start_ms: u64,
        end_ms: u64,
        payload: String,
    },
    Skipped,
    Ignored,
}

/// Section 6.1, "collect a WebVTT block", keeping what a transcript needs: a
/// cue's timings and payload. Style sheets and regions are not built.
fn collect_block(lines: &[&str], i: &mut usize, in_header: bool) -> Block {
    let mut line_count = 0;
    let mut first_line = "";
    let mut buffer = String::new();
    let mut seen_arrow = false;
    let mut timings = None;

    while let Some(&line) = lines.get(*i) {
        *i += 1;
        line_count += 1;
        if line_count == 1 {
            first_line = line;
        }

        if line.contains("-->") {
            if !in_header && (line_count == 1 || (line_count == 2 && !seen_arrow)) {
                seen_arrow = true;
                timings = parse_timings(line);
                if timings.is_some() {
                    // Whatever came before the timing line was the identifier.
                    buffer.clear();
                }
            } else {
                // This line starts the next block.
                *i -= 1;
                break;
            }
        } else if line.is_empty() {
            break;
        } else {
            if !buffer.is_empty() {
                buffer.push('\n');
            }
            buffer.push_str(line);
        }
    }

    if in_header {
        return Block::Ignored;
    }
    match timings {
        Some((start_ms, end_ms)) => Block::Cue {
            start_ms,
            end_ms,
            payload: buffer,
        },
        None if !seen_arrow && is_note_style_or_region(first_line) => Block::Ignored,
        None => Block::Skipped,
    }
}

/// A comment block starts `NOTE`, alone or followed by a space or tab
/// (section 4.1). A style or region block starts `STYLE` or `REGION` followed
/// only by whitespace, which is how section 6.1 recognises them.
fn is_note_style_or_region(first_line: &str) -> bool {
    let note = first_line
        .strip_prefix("NOTE")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']));
    let style_or_region = ["STYLE", "REGION"].iter().any(|keyword| {
        first_line
            .strip_prefix(keyword)
            .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_whitespace()))
    });
    note || style_or_region
}

/// Section 6.3: a start timestamp, `-->`, an end timestamp. What follows is
/// cue settings, which a transcript does not need.
///
/// An end before the start is kept, not skipped: section 6.3 does not compare
/// the two times, and the words are still worth reading.
fn parse_timings(line: &str) -> Option<(u64, u64)> {
    let mut rest = line.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let start = collect_timestamp(&mut rest)?;
    rest = rest
        .trim_start_matches(|c: char| c.is_ascii_whitespace())
        .strip_prefix("-->")?
        .trim_start_matches(|c: char| c.is_ascii_whitespace());
    let end = collect_timestamp(&mut rest)?;
    Some((start, end))
}

/// Section 6.3, "collect a WebVTT timestamp": moves `input` past the
/// timestamp and returns it in milliseconds.
///
/// The first number is hours when it is not exactly two digits or is over 59;
/// otherwise it is minutes, unless a third number follows. Minutes and
/// seconds are two digits and at most 59, and the fraction is exactly three
/// digits.
fn collect_timestamp(input: &mut &str) -> Option<u64> {
    fn digits<'a>(input: &mut &'a str) -> &'a str {
        let end = input
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(input.len());
        let (digits, rest) = input.split_at(end);
        *input = rest;
        digits
    }

    let first = digits(input);
    if first.is_empty() {
        return None;
    }
    let value1: u64 = first.parse().ok()?;
    let hours_first = first.len() != 2 || value1 > 59;

    *input = input.strip_prefix(':')?;
    let second = digits(input);
    if second.len() != 2 {
        return None;
    }
    let value2: u64 = second.parse().ok()?;

    let (hours, minutes, seconds) = if hours_first || input.starts_with(':') {
        *input = input.strip_prefix(':')?;
        let third = digits(input);
        if third.len() != 2 {
            return None;
        }
        (value1, value2, third.parse::<u64>().ok()?)
    } else {
        (0, value1, value2)
    };

    *input = input.strip_prefix('.')?;
    let fraction = digits(input);
    if fraction.len() != 3 {
        return None;
    }
    let millis: u64 = fraction.parse().ok()?;
    if minutes > 59 || seconds > 59 {
        return None;
    }

    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000 + seconds * 1_000 + millis)
}

enum Tag {
    Start { name: String, annotation: String },
    End(String),
    Timestamp,
}

/// Section 6.4's parsing rules, reduced to what a transcript keeps: the text,
/// and the name of the innermost voice span around it.
///
/// Alongside the cues, says for each one whether it is voiced, which keeps it
/// from the name-prefix rule: its text sat inside a voice span, or the
/// payload holds an empty voice span anywhere. An empty span means the file
/// tried to say who spoke and said nothing, so no text in that cue is trusted
/// for a prefix. Text outside a named span is not voiced.
fn cues_from_payload(start_ms: u64, end_ms: u64, payload: &str) -> (Vec<Cue>, Vec<bool>) {
    let chars: Vec<char> = payload.chars().collect();
    // Open spans, innermost last: the tag name and its annotation.
    let mut open: Vec<(String, String)> = Vec::new();
    // Text segments: the innermost voice's name, whether any voice span was
    // open around the text, and the text.
    let mut segments: Vec<(Option<String>, bool, String)> = Vec::new();
    let mut had_empty_voice_span = false;
    let mut pos = 0;

    while let Some(&c) = chars.get(pos) {
        match c {
            '<' => {
                let (tag, next) = read_tag(&chars, pos + 1);
                pos = next;
                if matches!(&tag, Tag::Start { name, annotation } if name == "v" && annotation.is_empty())
                {
                    had_empty_voice_span = true;
                }
                apply_tag(&mut open, tag);
            }
            '&' => {
                let (decoded, next) = decode_reference(&chars, pos + 1);
                pos = next;
                push_text(&mut segments, &open, &decoded);
            }
            _ => {
                pos += 1;
                push_text(&mut segments, &open, c.encode_utf8(&mut [0; 4]));
            }
        }
    }

    let mut cues: Vec<Cue> = Vec::new();
    let mut voiced: Vec<bool> = Vec::new();
    for (speaker, in_voice_span, text) in segments {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            continue;
        }
        let cue_voiced = in_voice_span || had_empty_voice_span;
        match (cues.last_mut(), voiced.last()) {
            (Some(last), Some(&last_voiced))
                if last.speaker == speaker && last_voiced == cue_voiced =>
            {
                last.text.push(' ');
                last.text.push_str(&text);
            }
            _ => {
                cues.push(Cue {
                    start_ms,
                    end_ms,
                    speaker,
                    text,
                });
                voiced.push(cue_voiced);
            }
        }
    }
    (cues, voiced)
}

/// Append text to the segment for the innermost open voice, or start a new
/// segment when the voice, or whether any voice span is open, has changed.
/// Ruby text is not spoken, so it is not kept.
fn push_text(
    segments: &mut Vec<(Option<String>, bool, String)>,
    open: &[(String, String)],
    text: &str,
) {
    if open.iter().any(|(name, _)| name == "rt") {
        return;
    }
    let in_voice_span = open.iter().any(|(name, _)| name == "v");
    let speaker = open
        .iter()
        .rev()
        .find(|(name, _)| name == "v")
        .map(|(_, annotation)| annotation.clone())
        .filter(|annotation| !annotation.is_empty());
    match segments.last_mut() {
        Some((last, last_in_voice_span, buffer))
            if *last == speaker && *last_in_voice_span == in_voice_span =>
        {
            buffer.push_str(text)
        }
        _ => segments.push((speaker, in_voice_span, text.to_string())),
    }
}

/// Section 6.4's start and end tag steps: a known start tag opens a span
/// (`rt` only inside `ruby`), and an end tag closes the innermost span only
/// if the names match, or closes `rt` and its `ruby` together.
fn apply_tag(open: &mut Vec<(String, String)>, tag: Tag) {
    match tag {
        Tag::Start { name, annotation } => {
            let known = matches!(name.as_str(), "c" | "i" | "b" | "u" | "ruby" | "v" | "lang");
            let ruby_text = name == "rt" && open.last().is_some_and(|(top, _)| top == "ruby");
            if known || ruby_text {
                open.push((name, annotation));
            }
        }
        Tag::End(name) => {
            let top = open.last().map(|(top, _)| top.as_str());
            if top == Some(name.as_str()) {
                open.pop();
            } else if name == "ruby" && top == Some("rt") {
                open.pop();
                open.pop();
            }
        }
        // An inline timestamp marks time within the cue; it is not a word.
        Tag::Timestamp => {}
    }
}

/// Section 6.4's tokenizer from just after a `<` to just after the tag's `>`,
/// or to the end of the payload when the tag is never closed.
fn read_tag(chars: &[char], mut pos: usize) -> (Tag, usize) {
    enum State {
        Tag,
        Name,
        Class,
        Annotation,
        End,
        Timestamp,
    }

    let mut state = State::Tag;
    let mut name = String::new();
    let mut annotation = String::new();

    while let Some(&c) = chars.get(pos) {
        pos += 1;
        match state {
            State::Tag => match c {
                '\t' | '\n' | '\x0c' | ' ' => state = State::Annotation,
                '.' => state = State::Class,
                '/' => state = State::End,
                '0'..='9' => state = State::Timestamp,
                '>' => break,
                _ => {
                    name.push(c);
                    state = State::Name;
                }
            },
            State::Name => match c {
                '\t' | '\n' | '\x0c' | ' ' => state = State::Annotation,
                '.' => state = State::Class,
                '>' => break,
                _ => name.push(c),
            },
            // Class names style a span; they do not change the words.
            State::Class => match c {
                '\t' | '\n' | '\x0c' | ' ' => state = State::Annotation,
                '>' => break,
                _ => {}
            },
            State::Annotation => match c {
                '&' => {
                    let (decoded, next) = decode_reference(chars, pos);
                    annotation.push_str(&decoded);
                    pos = next;
                }
                '>' => break,
                _ => annotation.push(c),
            },
            State::End => match c {
                '>' => break,
                _ => name.push(c),
            },
            State::Timestamp => {
                if c == '>' {
                    break;
                }
            }
        }
    }

    let tag = match state {
        State::End => Tag::End(name),
        State::Timestamp => Tag::Timestamp,
        State::Tag | State::Name | State::Class | State::Annotation => Tag::Start {
            name,
            annotation: collapse_ascii_whitespace(&annotation),
        },
    };
    (tag, pos)
}

/// Section 6.4, end of the annotation state: trim ASCII whitespace and
/// collapse each run of it to one space. `pub(crate)` so [`crate::srt::write`]
/// can collapse a turn's text and speaker the same way [`write`] does.
pub(crate) fn collapse_ascii_whitespace(text: &str) -> String {
    text.split(|c: char| c.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The named references decoded, and whether HTML also accepts the name
/// without its semicolon.
const NAMED_REFERENCES: &[(&str, &str, bool)] = &[
    ("amp", "&", true),
    ("lt", "<", true),
    ("gt", ">", true),
    ("quot", "\"", true),
    ("nbsp", "\u{a0}", true),
    ("apos", "'", false),
    ("lrm", "\u{200e}", false),
    ("rlm", "\u{200f}", false),
];

/// Decode a character reference starting just after its `&`. Returns the
/// decoded text and the position after the reference; when there is no
/// reference there, returns a literal `&` and leaves the position alone, so
/// the characters that follow are read as ordinary text.
fn decode_reference(chars: &[char], pos: usize) -> (String, usize) {
    let not_a_reference = ("&".to_string(), pos);
    match chars.get(pos) {
        Some('#') => decode_numeric(chars, pos + 1).unwrap_or(not_a_reference),
        Some(c) if c.is_ascii_alphanumeric() => {
            for (name, value, without_semicolon) in NAMED_REFERENCES {
                let matches = name
                    .chars()
                    .enumerate()
                    .all(|(k, n)| chars.get(pos + k) == Some(&n));
                if !matches {
                    continue;
                }
                let end = pos + name.len();
                if chars.get(end) == Some(&';') {
                    return (value.to_string(), end + 1);
                }
                if *without_semicolon {
                    return (value.to_string(), end);
                }
            }
            not_a_reference
        }
        _ => not_a_reference,
    }
}

/// A numeric reference from just after its `#`: decimal digits, or `x` and
/// hex digits, with an optional `;`. A code point that is not a character, or
/// zero, becomes U+FFFD.
fn decode_numeric(chars: &[char], mut pos: usize) -> Option<(String, usize)> {
    let hex = matches!(chars.get(pos), Some('x' | 'X'));
    if hex {
        pos += 1;
    }
    let radix = if hex { 16 } else { 10 };
    let start = pos;
    while chars.get(pos).is_some_and(|c| c.is_digit(radix)) {
        pos += 1;
    }
    if pos == start {
        return None;
    }
    let digits: String = chars[start..pos].iter().collect();
    if chars.get(pos) == Some(&';') {
        pos += 1;
    }
    let decoded = u32::from_str_radix(&digits, radix)
        .ok()
        .and_then(char::from_u32)
        .filter(|&c| c != '\0')
        .unwrap_or('\u{fffd}');
    Some((decoded.to_string(), pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start_ms: u64, end_ms: u64, speaker: Option<&str>, text: &str) -> Cue {
        Cue {
            start_ms,
            end_ms,
            speaker: speaker.map(str::to_string),
            text: text.to_string(),
        }
    }

    fn only_cue(input: &str) -> Cue {
        let transcript = parse(input).unwrap();
        assert_eq!(transcript.cues.len(), 1, "{transcript:?}");
        assert_eq!(transcript.skipped_blocks, 0, "{transcript:?}");
        transcript.cues[0].clone()
    }

    #[test]
    fn lf_crlf_and_cr_line_endings_read_the_same() {
        let lf = "WEBVTT\n\n00:01.000 --> 00:02.000\nHello there\n\n\
                  00:02.000 --> 00:03.500\nGeneral\nKenobi\n";
        let expected = vec![
            cue(1_000, 2_000, None, "Hello there"),
            cue(2_000, 3_500, None, "General Kenobi"),
        ];
        for input in [
            lf.to_string(),
            lf.replace('\n', "\r\n"),
            lf.replace('\n', "\r"),
        ] {
            let transcript = parse(&input).unwrap();
            assert_eq!(transcript.cues, expected, "{input:?}");
            assert_eq!(transcript.skipped_blocks, 0, "{input:?}");
        }
    }

    #[test]
    fn a_byte_order_mark_is_allowed() {
        let only = only_cue("\u{feff}WEBVTT\n\n00:01.000 --> 00:02.000\nHi\n");
        assert_eq!(only, cue(1_000, 2_000, None, "Hi"));
    }

    #[test]
    fn a_file_without_the_signature_is_an_error() {
        for input in [
            "",
            "WEBVT",
            "webvtt\n\n",
            "WEBVTTX\n\n",
            "\n\nWEBVTT\n\n",
            "00:01.000 --> 00:02.000\nHi\n",
        ] {
            assert_eq!(parse(input), Err(VttError::MissingHeader), "{input:?}");
        }
    }

    #[test]
    fn text_after_the_signature_and_header_lines_are_ignored() {
        let input = "WEBVTT - Weekly sync\nKind: captions\nLanguage: en\n\n\
                     00:00:01.000 --> 00:00:02.000\nHello\n";
        assert_eq!(only_cue(input), cue(1_000, 2_000, None, "Hello"));

        let tab = "WEBVTT\tcaptions\n\n00:01.000 --> 00:02.000\nHello\n";
        assert_eq!(only_cue(tab), cue(1_000, 2_000, None, "Hello"));

        // One line break after the signature instead of two: the parser still
        // finds the cue, because a header block ends at a timing line.
        let tight = "WEBVTT\n00:01.000 --> 00:02.000\nHello\n";
        assert_eq!(only_cue(tight), cue(1_000, 2_000, None, "Hello"));
    }

    #[test]
    fn note_style_and_region_blocks_are_neither_cues_nor_skipped() {
        let input = "WEBVTT\n\n\
                     REGION\nid:fred width:40%\n\n\
                     STYLE\n::cue { color: yellow }\n\n\
                     NOTE This is a comment\nover two lines\n\n\
                     00:01.000 --> 00:02.000\nOne\n\n\
                     NOTE\nbetween cues\n\n\
                     00:02.000 --> 00:03.000\nTwo\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, None, "One"),
                cue(2_000, 3_000, None, "Two")
            ]
        );
        assert_eq!(transcript.skipped_blocks, 0);
    }

    #[test]
    fn cues_with_and_without_identifiers_hours_and_settings() {
        let input = "WEBVTT\n\n\
                     1\n00:00:01.000 --> 00:00:02.000 align:start line:0%\nIdentifier, hours, settings\n\n\
                     00:02.500 --> 00:03.000\nNo identifier, no hours\n\n\
                     intro-2\n01:02:03.004 --> 01:02:04.000\tposition:10%\nAn hour in\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, None, "Identifier, hours, settings"),
                cue(2_500, 3_000, None, "No identifier, no hours"),
                cue(3_723_004, 3_724_000, None, "An hour in"),
            ]
        );
        assert_eq!(transcript.skipped_blocks, 0);
    }

    #[test]
    fn timestamps_follow_the_spec_collection_rules() {
        let timestamp = |text: &str| {
            let mut rest = text;
            collect_timestamp(&mut rest).filter(|_| rest.is_empty())
        };
        assert_eq!(timestamp("00:01.000"), Some(1_000));
        assert_eq!(timestamp("59:59.999"), Some(3_599_999));
        assert_eq!(timestamp("00:00:01.500"), Some(1_500));
        assert_eq!(timestamp("100:00:00.000"), Some(360_000_000));
        // One hour digit is an authoring error, but parsers read it as hours.
        assert_eq!(timestamp("1:02:03.004"), Some(3_723_004));
        // Over 59 must be hours, so a third number is needed.
        assert_eq!(timestamp("60:00.000"), None);
        assert_eq!(timestamp("00:60.000"), None);
        assert_eq!(timestamp("00:01.00"), None);
        assert_eq!(timestamp("00:1.000"), None);
        assert_eq!(timestamp("00:01,000"), None);
    }

    #[test]
    fn a_closed_voice_span_names_the_speaker() {
        let only = only_cue(
            "WEBVTT\n\n00:01.000 --> 00:04.000\n<v Roger Bingham>We are in New York City</v>\n",
        );
        assert_eq!(
            only,
            cue(
                1_000,
                4_000,
                Some("Roger Bingham"),
                "We are in New York City"
            )
        );
    }

    #[test]
    fn an_unclosed_voice_span_covers_the_whole_cue() {
        let only =
            only_cue("WEBVTT\n\n00:01.000 --> 00:04.000\n<v Neil deGrasse Tyson>Hi there,\nsir\n");
        assert_eq!(
            only,
            cue(1_000, 4_000, Some("Neil deGrasse Tyson"), "Hi there, sir")
        );
    }

    #[test]
    fn a_voice_span_with_classes_still_names_the_speaker() {
        let only = only_cue(
            "WEBVTT\n\n00:01.000 --> 00:04.000\n<v.first.loud  Esme >It's a blue apple tree!\n",
        );
        assert_eq!(
            only,
            cue(1_000, 4_000, Some("Esme"), "It's a blue apple tree!")
        );
    }

    #[test]
    fn several_voices_in_one_cue_become_cues_with_the_same_times() {
        let input =
            "WEBVTT\n\n00:05.000 --> 00:07.000\n<v Mary>Are you coming?</v> <v Esme>In a minute.</v>\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(5_000, 7_000, Some("Mary"), "Are you coming?"),
                cue(5_000, 7_000, Some("Esme"), "In a minute."),
            ]
        );
    }

    #[test]
    fn consecutive_cues_from_one_speaker_merge_into_turns() {
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:03.000\n<v Aoife>We need to ship\n\n\
                     00:03.000 --> 00:05.000\n<v Aoife>the parser by Friday.\n\n\
                     00:04.500 --> 00:06.000\n<v Brian>Agreed.\n\n\
                     00:06.000 --> 00:08.000\n<v Aoife>Then we test it.\n\n\
                     00:08.000 --> 00:09.000\nNobody named.\n";
        let transcript = parse(input).unwrap();
        let turn = |speaker: Option<&str>, start_ms, end_ms, text: &str| Turn {
            speaker: speaker.map(str::to_string),
            start_ms,
            end_ms,
            text: text.to_string(),
        };
        assert_eq!(
            transcript.turns(),
            vec![
                turn(
                    Some("Aoife"),
                    1_000,
                    5_000,
                    "We need to ship the parser by Friday."
                ),
                turn(Some("Brian"), 4_500, 6_000, "Agreed."),
                turn(Some("Aoife"), 6_000, 8_000, "Then we test it."),
                turn(None, 8_000, 9_000, "Nobody named."),
            ]
        );
        assert_eq!(
            transcript.spoken_text(),
            "We need to ship the parser by Friday. Agreed. Then we test it. Nobody named."
        );
    }

    #[test]
    fn tags_and_inline_timestamps_are_stripped() {
        let input = "WEBVTT\n\n00:00:01.000 --> 00:00:05.000\n\
                     <v Kai><i>Fish</i> and <b>chips</b> <00:00:02.000>cost\n\
                     <c.highlight>far</c> <u>too</u> <lang en-GB>much</lang> <unknown>today\n";
        assert_eq!(
            only_cue(input),
            cue(
                1_000,
                5_000,
                Some("Kai"),
                "Fish and chips cost far too much today"
            )
        );

        let ruby = "WEBVTT\n\n00:01.000 --> 00:02.000\n<ruby>漢<rt>kan</rt>字<rt>ji</ruby> desu\n";
        assert_eq!(only_cue(ruby).text, "漢字 desu");
    }

    #[test]
    fn character_references_are_decoded() {
        let input = "WEBVTT\n\n00:01.000 --> 00:02.000\n\
                     Salt &amp; vinegar &lt;3 &quot;crisps&quot; &#8364;2 or &#x20AC;3, \
                     AT&T &unknown; &apos;ok&apos; caf&#233;&nbsp;now &amp;lt;once\n";
        // An escaped reference decodes once: "&amp;lt;" is the text "&lt;".
        assert_eq!(
            only_cue(input).text,
            "Salt & vinegar <3 \"crisps\" €2 or €3, AT&T &unknown; 'ok' café now &lt;once"
        );

        let speaker = "WEBVTT\n\n00:01.000 --> 00:02.000\n<v Ben &amp; Jerry>Hello\n";
        assert_eq!(only_cue(speaker).speaker.as_deref(), Some("Ben & Jerry"));
    }

    #[test]
    fn a_malformed_timing_line_is_skipped_and_counted() {
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\nKept\n\n\
                     00:02.000 -> 00:03.000\nArrow too short\n\n\
                     00:03.000 --> 00:03.00\nFraction too short\n\n\
                     id\n00:61.000 --> 00:62.000\nSixty-one seconds\n\n\
                     00:04.000 --> 00:05.000\nAlso kept\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, None, "Kept"),
                cue(4_000, 5_000, None, "Also kept")
            ]
        );
        assert_eq!(transcript.skipped_blocks, 3);
    }

    #[test]
    fn a_file_with_no_cues_is_empty_not_an_error() {
        for input in [
            "WEBVTT",
            "WEBVTT\n",
            "WEBVTT\n\n",
            "WEBVTT header text\n\nNOTE nothing was said\n",
        ] {
            let transcript = parse(input).unwrap();
            assert_eq!(transcript, Transcript::default(), "{input:?}");
            assert!(transcript.turns().is_empty());
            assert_eq!(transcript.spoken_text(), "");
        }

        // A cue with no words left after its tags adds nothing.
        let silent = parse("WEBVTT\n\n00:01.000 --> 00:02.000\n<v Kai><00:01.500></v>\n").unwrap();
        assert_eq!(silent, Transcript::default());
    }

    #[test]
    fn a_teams_style_file_reads_voice_spans() {
        // Hand-written in the shape of Microsoft's Graph documentation sample.
        let input = "WEBVTT\r\n\r\n\
                     00:00:16.246 --> 00:00:17.726\r\n<v User Name>This is a transcript test.</v>\r\n\r\n\
                     00:00:18.100 --> 00:00:21.000\r\n<v Aoife Byrne>Brian: that is not a speaker.</v>\r\n\r\n\
                     00:00:21.500 --> 00:00:23.000\r\n<v User Name>Agreed.</v>\r\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(
                    16_246,
                    17_726,
                    Some("User Name"),
                    "This is a transcript test."
                ),
                cue(
                    18_100,
                    21_000,
                    Some("Aoife Byrne"),
                    "Brian: that is not a speaker."
                ),
                cue(21_500, 23_000, Some("User Name"), "Agreed."),
            ]
        );
        assert_eq!(transcript.skipped_blocks, 0);
    }

    #[test]
    fn a_zoom_style_file_reads_name_prefixes() {
        // Hand-written in the shape community parsers expect of Zoom: numbered
        // cues, CRLF, and the speaker as a prefix on the cue text.
        let input = "WEBVTT\r\n\r\n\
                     1\r\n00:00:01.000 --> 00:00:04.000\r\nAoife Byrne: We need to ship the parser by Friday.\r\n\r\n\
                     2\r\n00:00:04.500 --> 00:00:06.000\r\nBrian O'Neill: The plan: ship it, then test.\r\n\r\n\
                     3\r\n00:00:06.500 --> 00:00:08.000\r\nDr. J. Smith-Jones: Agreed.\r\n\r\n\
                     4\r\n00:00:08.000 --> 00:00:09.000\r\nAoife Byrne: Good.\r\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(
                    1_000,
                    4_000,
                    Some("Aoife Byrne"),
                    "We need to ship the parser by Friday."
                ),
                cue(
                    4_500,
                    6_000,
                    Some("Brian O'Neill"),
                    "The plan: ship it, then test."
                ),
                cue(6_500, 8_000, Some("Dr. J. Smith-Jones"), "Agreed."),
                cue(8_000, 9_000, Some("Aoife Byrne"), "Good."),
            ]
        );
        assert_eq!(
            transcript.spoken_text(),
            "We need to ship the parser by Friday. The plan: ship it, then test. Agreed. Good."
        );
    }

    #[test]
    fn a_colon_in_ordinary_speech_is_not_a_speaker() {
        // One name-shaped prefix is not a pattern.
        let once = "WEBVTT\n\n\
                    00:01.000 --> 00:02.000\nThe plan: ship Friday.\n\n\
                    00:02.000 --> 00:03.000\nThen we test it.\n";
        // Two, but on fewer than half of the cues, is not a pattern either.
        let few = "WEBVTT\n\n\
                   00:01.000 --> 00:02.000\nThe plan: ship Friday.\n\n\
                   00:02.000 --> 00:03.000\nNext step: test it.\n\n\
                   00:03.000 --> 00:04.000\nThen review.\n\n\
                   00:04.000 --> 00:05.000\nThen release.\n\n\
                   00:05.000 --> 00:06.000\nThen rest.\n";
        for input in [once, few] {
            let transcript = parse(input).unwrap();
            assert!(
                transcript.cues.iter().all(|cue| cue.speaker.is_none()),
                "{transcript:?}"
            );
            assert_eq!(transcript.cues[0].text, "The plan: ship Friday.");
        }
    }

    #[test]
    fn a_name_prefix_must_look_like_a_name() {
        assert_eq!(name_prefix("Aoife: Hello"), Some(("Aoife", "Hello")));
        assert_eq!(name_prefix("Aoife B.: Hello"), Some(("Aoife B.", "Hello")));
        assert_eq!(
            name_prefix("Seán Ó Briain: Dia duit"),
            Some(("Seán Ó Briain", "Dia duit"))
        );
        for text in [
            "Okay, so: we ship",
            "Okay. So the plan: ship",
            "Is it ready?: yes",
            "One two three four five: ship",
            "10 minutes: then we test",
            "\"Quoted\": no",
            "Aoife:",
            "Aoife:Hello",
            "Aoife : Hello",
        ] {
            assert_eq!(name_prefix(text), None, "{text}");
        }
    }

    #[test]
    fn an_empty_voice_span_still_keeps_the_prefix_rule_away() {
        // A cue with a voice span is never read for a prefix, even when the
        // span names nobody.
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\n<v ></v>Aoife Byrne: Hello there.\n\n\
                     00:02.000 --> 00:03.000\n<v.loud></v>Aoife Byrne: Hello again.\n\n\
                     00:03.000 --> 00:04.000\n<v ></v>Aoife Byrne: Goodbye.\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, None, "Aoife Byrne: Hello there."),
                cue(2_000, 3_000, None, "Aoife Byrne: Hello again."),
                cue(3_000, 4_000, None, "Aoife Byrne: Goodbye."),
            ]
        );

        // Among cues without a voice span, the rule reads only those, and
        // counts only those towards the pattern.
        let mixed = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\nAoife Byrne: Hello there.\n\n\
                     00:02.000 --> 00:03.000\n<v ></v>Brian: Hello again.\n\n\
                     00:03.000 --> 00:04.000\nAoife Byrne: Goodbye.\n";
        let transcript = parse(mixed).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, Some("Aoife Byrne"), "Hello there."),
                cue(2_000, 3_000, None, "Brian: Hello again."),
                cue(3_000, 4_000, Some("Aoife Byrne"), "Goodbye."),
            ]
        );
    }

    #[test]
    fn a_comma_name_prefix_on_every_cue_is_the_speaker() {
        // A real Zoom transcript export puts `Name: ` before the words of
        // every cue, and Zoom display names are free text: of 17 names in one
        // real export, 2 did not look like a name to the shape rule.
        // When
        // every cue carries a prefix, its text is the speaker whatever it
        // holds, so "Surname, Firstname" is a name here.
        let input = "WEBVTT\r\n\r\n\
                     00:01.000 --> 00:02.000\r\nByrne, Aoife: We need to ship.\r\n\r\n\
                     00:02.000 --> 00:03.000\r\nO'Neill, Brian: Agreed.\r\n\r\n\
                     00:03.000 --> 00:04.000\r\nByrne, Aoife: Good.\r\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, Some("Byrne, Aoife"), "We need to ship."),
                cue(2_000, 3_000, Some("O'Neill, Brian"), "Agreed."),
                cue(3_000, 4_000, Some("Byrne, Aoife"), "Good."),
            ]
        );
        assert_eq!(transcript.spoken_text(), "We need to ship. Agreed. Good.");
    }

    #[test]
    fn a_free_text_name_on_every_cue_is_the_speaker() {
        // Zoom's shape, with display names the shape rule turns away: a
        // parenthesis and a full stop, and a bar. A colon later in the words
        // is speech, since the name ends at the first one.
        let input = "WEBVTT\r\n\r\n\
                     1\r\n00:00:01.000 --> 00:00:04.000\r\nAoife Byrne (Acme Ltd.): We need to ship the parser by Friday.\r\n\r\n\
                     2\r\n00:00:04.500 --> 00:00:06.000\r\nBrian O'Neill | Sales: The plan: ship it, then test.\r\n\r\n\
                     3\r\n00:00:06.500 --> 00:00:08.000\r\nUnknown Speaker: Can everyone hear me?\r\n\r\n\
                     4\r\n00:00:08.000 --> 00:00:09.000\r\nAoife Byrne (Acme Ltd.): Yes.\r\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(
                    1_000,
                    4_000,
                    Some("Aoife Byrne (Acme Ltd.)"),
                    "We need to ship the parser by Friday."
                ),
                cue(
                    4_500,
                    6_000,
                    Some("Brian O'Neill | Sales"),
                    "The plan: ship it, then test."
                ),
                cue(
                    6_500,
                    8_000,
                    Some("Unknown Speaker"),
                    "Can everyone hear me?"
                ),
                cue(8_000, 9_000, Some("Aoife Byrne (Acme Ltd.)"), "Yes."),
            ]
        );
    }

    #[test]
    fn a_file_where_some_cue_has_no_prefix_keeps_the_name_shape_rule() {
        // Not every cue starts with `Name: `, so this is not Zoom's shape: the
        // free-text name stays in the words, and so does a colon in speech on
        // a minority of cues. Today's behaviour, pinned.
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\nAoife Byrne (Acme Ltd.): Hello there.\n\n\
                     00:02.000 --> 00:03.000\nThe plan: ship Friday.\n\n\
                     00:03.000 --> 00:04.000\nThen we test it.\n\n\
                     00:04.000 --> 00:05.000\nThen we ship it.\n";
        let transcript = parse(input).unwrap();
        assert!(
            transcript.cues.iter().all(|cue| cue.speaker.is_none()),
            "{transcript:?}"
        );
        assert_eq!(
            transcript.cues[0].text,
            "Aoife Byrne (Acme Ltd.): Hello there."
        );
        assert_eq!(transcript.cues[1].text, "The plan: ship Friday.");
    }

    #[test]
    fn one_cue_with_a_colon_is_not_a_pattern() {
        let only = only_cue("WEBVTT\n\n00:01.000 --> 00:02.000\nHello: world.\n");
        assert_eq!(only, cue(1_000, 2_000, None, "Hello: world."));
    }

    #[test]
    fn a_time_or_a_link_in_speech_does_not_split_a_cue() {
        // The split is at a colon followed by a space, so a time and a link
        // stay whole, and this mixed file keeps the name shape rule.
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\nAoife Byrne (Acme Ltd.): Hello there.\n\n\
                     00:02.000 --> 00:03.000\nWe meet at 10:30 tomorrow.\n\n\
                     00:03.000 --> 00:04.000\nThe notes are at https://example.com/notes.\n";
        let transcript = parse(input).unwrap();
        assert!(
            transcript.cues.iter().all(|cue| cue.speaker.is_none()),
            "{transcript:?}"
        );
        assert_eq!(transcript.cues[1].text, "We meet at 10:30 tomorrow.");
        assert_eq!(
            transcript.cues[2].text,
            "The notes are at https://example.com/notes."
        );
    }

    #[test]
    fn two_cues_each_with_a_prefix_are_enough_for_zoom_shape() {
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\nAoife Byrne (Acme Ltd.): We meet at 10:30.\n\n\
                     00:02.000 --> 00:03.000\nBrian O'Neill | Sales: The notes are at https://example.com/notes.\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(
                    1_000,
                    2_000,
                    Some("Aoife Byrne (Acme Ltd.)"),
                    "We meet at 10:30."
                ),
                cue(
                    2_000,
                    3_000,
                    Some("Brian O'Neill | Sales"),
                    "The notes are at https://example.com/notes."
                ),
            ]
        );
    }

    #[test]
    fn a_speaker_before_the_first_colon_needs_a_name_and_words() {
        assert_eq!(
            speaker_before_first_colon("Aoife Byrne (Acme Ltd.): Hello: there"),
            Some(("Aoife Byrne (Acme Ltd.)", "Hello: there"))
        );
        // A space before the colon is not part of the name.
        assert_eq!(
            speaker_before_first_colon("Aoife : Hello"),
            Some(("Aoife", "Hello"))
        );
        for text in [
            ": Hello",
            " Aoife: Hello",
            "Aoife:Hello",
            "Aoife: ",
            "Hello",
        ] {
            assert_eq!(speaker_before_first_colon(text), None, "{text:?}");
        }
    }

    #[test]
    fn an_end_before_the_start_is_kept() {
        // Section 6.3 does not compare the two times, so neither does this.
        let only = only_cue("WEBVTT\n\n00:05.000 --> 00:02.000\nBackwards\n");
        assert_eq!(only, cue(5_000, 2_000, None, "Backwards"));
    }

    #[test]
    fn exactly_half_the_cues_is_enough_for_the_prefix_pattern() {
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\nAoife: Hello there.\n\n\
                     00:02.000 --> 00:03.000\nThen we test it.\n\n\
                     00:03.000 --> 00:04.000\nBrian: Fine.\n\n\
                     00:04.000 --> 00:05.000\nThen we ship it.\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript
                .cues
                .iter()
                .map(|cue| cue.speaker.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("Aoife"), None, Some("Brian"), None]
        );
    }

    #[test]
    fn text_outside_a_named_voice_span_can_still_carry_a_name_prefix() {
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\n<v Mary>Are you coming?</v> Brian: In a minute.\n\n\
                     00:02.000 --> 00:03.000\n<v Mary>Now?</v> Brian: Nearly there.\n\n\
                     00:03.000 --> 00:04.000\n<v Mary>Good.</v> Brian: On my way.\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, Some("Mary"), "Are you coming?"),
                cue(1_000, 2_000, Some("Brian"), "In a minute."),
                cue(2_000, 3_000, Some("Mary"), "Now?"),
                cue(2_000, 3_000, Some("Brian"), "Nearly there."),
                cue(3_000, 4_000, Some("Mary"), "Good."),
                cue(3_000, 4_000, Some("Brian"), "On my way."),
            ]
        );
    }

    #[test]
    fn an_empty_voice_span_keeps_its_whole_cue_from_the_prefix_rule() {
        // A named span and an empty one in the same payload. The empty span
        // means the file tried to say who spoke and said nothing, so no text
        // in that cue is read for a prefix, even outside the named span.
        let input = "WEBVTT\n\n\
                     00:01.000 --> 00:02.000\n<v Mary>Are you coming?</v><v ></v> Brian: In a minute.\n\n\
                     00:02.000 --> 00:03.000\n<v Mary>Now?</v><v ></v> Brian: Nearly there.\n\n\
                     00:03.000 --> 00:04.000\n<v Mary>Good.</v><v ></v> Brian: On my way.\n";
        let transcript = parse(input).unwrap();
        assert_eq!(
            transcript.cues,
            vec![
                cue(1_000, 2_000, Some("Mary"), "Are you coming?"),
                cue(1_000, 2_000, None, "Brian: In a minute."),
                cue(2_000, 3_000, Some("Mary"), "Now?"),
                cue(2_000, 3_000, None, "Brian: Nearly there."),
                cue(3_000, 4_000, Some("Mary"), "Good."),
                cue(3_000, 4_000, None, "Brian: On my way."),
            ]
        );
    }

    #[test]
    fn written_turns_read_back_to_the_same_speakers_times_and_words() {
        let turns = vec![
            Turn {
                speaker: Some("You".into()),
                start_ms: 1_200,
                end_ms: 3_450,
                text: "Salt & vinegar <3, the plan: ship --> Friday".into(),
            },
            Turn {
                speaker: Some("Others".into()),
                start_ms: 3_600_500,
                end_ms: 3_602_000,
                text: "Agreed.".into(),
            },
        ];
        assert_eq!(parse(&write(&turns)).unwrap().turns(), turns);
    }

    #[test]
    fn unnamed_turns_with_colons_are_never_read_as_named() {
        // Two cues shaped like a name prefix would be read as speakers if
        // they were written bare.
        let turns = vec![
            Turn {
                speaker: None,
                start_ms: 0,
                end_ms: 1_000,
                text: "Plan: ship Friday".into(),
            },
            Turn {
                speaker: Some("Aoife".into()),
                start_ms: 1_000,
                end_ms: 2_000,
                text: "Fine.".into(),
            },
            Turn {
                speaker: None,
                start_ms: 2_000,
                end_ms: 3_000,
                text: "Risk: none".into(),
            },
        ];
        let read = parse(&write(&turns)).unwrap();
        assert_eq!(read.skipped_blocks, 0);
        assert_eq!(read.turns(), turns);
    }
}
