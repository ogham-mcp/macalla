// SPDX-License-Identifier: MIT
//! A meeting's summary, part by part, with every item's quote checked.
//!
//! The summary is three short lists: key points, decisions and action items.
//! Every item quotes the transcript. A long meeting does not fit in one model
//! call, so its turns are cut into parts in meeting order, each part is
//! summarised on its own, and the parts come back in that order. Nothing
//! merges parts or rewrites items afterwards: a second pass over the model's
//! own words would lose the quotes that make each item checkable.
//!
//! This is policy only. It builds the text a model is sent, asks through the
//! [`TextModel`] port for plain text or the [`ShapedModel`] port for a reply
//! shaped as the three lists, and checks every item with the same quote rule
//! ([`crate::summary::guard_summary_labelled`]). Where the
//! turns came from (a VTT file, a live meeting) and how big a part may be (a
//! provider's context limit) are the caller's.
//!
//! # Parts
//!
//! Turns are sent one per line, as `Name: words` when the speaker is known and
//! as the words alone when not, so the model can say who said what. A part
//! holds as many whole turns as fit the caller's character budget, and a turn
//! that fits is never split. A turn longer than the budget on its own is split
//! at sentence ends, and a sentence longer than the budget at the spaces
//! between words. Every piece of a split turn keeps its speaker's label and
//! the turn's times. A single word longer than the budget is sent as it is,
//! over the budget, rather than cut.
//!
//! # Reading the reply
//!
//! The three headings are found whatever their style: Markdown `#`, bold or
//! italics, a leading numbered-list marker (`1.` or `2)`), a trailing colon,
//! any case, singular or plural, and whatever words follow the heading
//! phrase ("Action Items Discussed", "Key points from the meeting") — as
//! long as the line has at most six words and does not end in sentence
//! punctuation (`.`, `!` or `?`; a trailing colon is fine). That length and
//! punctuation check keeps a sentence that happens to start with a heading
//! phrase, such as "Decisions were made to ship.", from being read as a
//! heading. A line holding a quote mark is never read as a heading either,
//! so an item line is never mistaken for one. Non-empty lines before the
//! first recognised heading are
//! counted in [`PartLists::dropped`] with [`DropCause::OutsideSections`],
//! since they belong to none of the three sections. Each section's lines go
//! to [`crate::summary::guard_summary_labelled`] against the part's words
//! without the speaker labels. A quote that starts with exactly one of the
//! part's labels and `: ` is checked without that prefix; a label anywhere
//! else in a quote is not found, and drops the item. A line with
//! no quote marks whose first word is "none", "nothing", "n/a" or "no", such
//! as "None." or "No decisions were made.", says its list is empty; it is
//! neither kept nor dropped, but counted in [`PartLists::assumed_empty`] so a
//! UI can tell an intentionally empty list from a filtered one.
//!
//! An action item may start with its owner in square brackets, as
//! [`INSTRUCTIONS`] asks. The owner is taken out of the item's text, and kept
//! only when the part confirms the name: every one of its word pieces must
//! be among a single speaker's label pieces in that part (so `[Aoife]`
//! matches the label "Aoife Byrne"). Nothing said in the speech, however
//! plainly it names someone, ever confirms an owner on its own — a full name
//! spoken but never labelling a turn, such as "Priya Shah" in a transcript
//! where nobody speaks under that label, gives no owner. This is a
//! deliberate consequence, not a gap: a live meeting where every turn is
//! labelled only "You" and "Others", or a VTT file with no names at all,
//! will have almost no action items with an owner. Otherwise the item stays
//! and the owner is dropped.
//!
//! # A shaped reply
//!
//! A model that can constrain its reply ([`ShapedModel`]) is asked for the
//! three lists as a [`Shape`], with the same [`INSTRUCTIONS`]: each item a
//! record of its text and then its quote, and an action item's owner first, a
//! choice of the part's speaker labels or [`NO_OWNER`], asked for only when
//! the part has named speakers. Each item is checked with the same rule as a
//! line of plain text, and an owner is kept only when the part confirms it, as
//! above. A reply that is not the shape asked for is an error for its part.
//!
//! # Where a quote was said
//!
//! Every kept item carries [`Said`]: the turn its quote's first word is in,
//! and that turn's start. A quote found more than once in the
//! part is placed where it is first said; one that runs from the end of one
//! turn into the next is placed in the turn where it starts. This says where
//! the words are and nothing about whether they support the item: that is
//! for the reader, who can now look.

use crate::meeting_parts::{self, Labels};
use crate::summary::{check_point, guard_summary_labelled, pieces, DropReason, SummaryPoint};
use crate::text_model::{Field, Shape, ShapedModel, TextModel};
use crate::vtt::Turn;
use crate::{Error, Result};

/// What the model is asked: the instructions sent with every part.
///
/// Why each rule is there:
///
/// - Three fixed headings, so the reply can be read without guessing, and an
///   empty list said rather than filled: a model asked for decisions in a
///   part where none were made will otherwise invent one.
/// - One item per line, ending with one quote of at least four words copied
///   exactly. [`crate::summary::guard_summary_labelled`] checks each line on
///   its own and drops an item
///   whose quote is not in the part, and four words is
///   [`crate::summary::MIN_QUOTE_WORDS`].
/// - Double quotes for nothing else, because the guard reads the line's quote
///   from its quote marks.
/// - No speaker's name inside a quote, because quotes are checked against the
///   words without their `Name:` labels. A quote that starts with its line's
///   own label has that label taken off before the check; anywhere else, a
///   name drops the item.
/// - An owner only when the transcript says who, in exactly this form:
///
///   ```text
///   - [Owner] What they will do "a quote of at least four words"
///   ```
///
///   Square brackets never pair as quote marks, cannot be mistaken for the
///   `Name: words` labels the part is sent with, and an item without an owner
///   simply has no bracket. The model may still write any `[Owner]` it
///   thinks it heard; after the reply comes back, an owner is kept only when
///   it is one of that part's speakers (see the module doc), so most of what
///   the model writes here ends up dropped rather than shown.
pub const INSTRUCTIONS: &str = "\
You summarise one part of a meeting transcript. Each line of the transcript is one turn, written as 'Name: words' when the speaker is known.

Reply with exactly three headed lists, in this order:
Key points
Decisions
Action items

Rules:
- One item per line, starting with '- '.
- End every item with one quote of at least four words, copied exactly from the transcript, inside double quotes. Use double quotes for nothing else.
- Never put a speaker's name or label inside a quote.
- In Action items, name the owner only when the transcript says who will do it, in square brackets at the start of the item: - [Owner] What they will do \"a quote of at least four words\"
- If a list has nothing in it, write None. under its heading. Do not invent items.
- Write nothing else.";

/// Which list an item belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    KeyPoints,
    Decisions,
    ActionItems,
}

impl Section {
    /// The list's name in a machine-readable reply, such as the CLI's `--json`
    /// and the desktop app.
    pub fn name(self) -> &'static str {
        match self {
            Self::KeyPoints => "key_points",
            Self::Decisions => "decisions",
            Self::ActionItems => "action_items",
        }
    }
}

/// Where a kept item's quote was said: the turn its first word is in.
///
/// This says where the words are, nothing more. Whether they support the item
/// is for the reader to judge, and this is what lets them look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Said {
    /// The turn, by its index in the turns summarised, whose speaker is the
    /// caller's to show.
    pub turn: usize,
    /// When the turn starts.
    pub start_ms: u64,
}

/// A key point or a decision that passed the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
    /// The point as the model wrote it, without bullet or quote.
    pub text: String,
    /// The quote, as the model wrote it, without the quote marks.
    pub quote: String,
    /// Where the quote was said.
    pub said: Said,
}

/// An action item that passed the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionItem {
    /// What is to be done, without its owner, bullet or quote.
    pub text: String,
    /// The quote, as the model wrote it, without the quote marks.
    pub quote: String,
    /// Who will do it, when the model named someone and the part confirms
    /// the name.
    pub owner: Option<String>,
    /// Where the quote was said.
    pub said: Said,
}

/// Why a reply line did not become a kept item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropCause {
    /// The guard rejected the line; see [`DropReason`].
    Guard(DropReason),
    /// The line came before the first recognised heading, so it belongs to
    /// none of the three sections.
    OutsideSections,
}

impl DropCause {
    /// The cause's name in a machine-readable reply: the variant's own name,
    /// in snake case.
    pub fn name(self) -> &'static str {
        match self {
            Self::Guard(DropReason::NoQuote) => "no_quote",
            Self::Guard(DropReason::TooShort) => "too_short",
            Self::Guard(DropReason::NotFound) => "not_found",
            Self::OutsideSections => "outside_sections",
        }
    }

    /// Why the line was dropped, as a clause a sentence can end with: "it has
    /// no complete quote".
    pub fn explain(self) -> &'static str {
        match self {
            Self::Guard(DropReason::NoQuote) => "it has no complete quote",
            // "four" is crate::summary::MIN_QUOTE_WORDS; a test holds them
            // together.
            Self::Guard(DropReason::TooShort) => "its quote has fewer than four words",
            Self::Guard(DropReason::NotFound) => {
                "its quote's words are not in this part, in order and together (speaker names do not count)"
            }
            Self::OutsideSections => "it came before the first list heading",
        }
    }
}

/// A reply line that did not pass the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedItem {
    /// The section the line was read under, or `None` for a line that came
    /// before the first heading.
    pub section: Option<Section>,
    /// The line as the model wrote it, trimmed; for a shaped reply, the item
    /// written out as `[Owner] text "quote"`, without the owner when it named
    /// none.
    pub line: String,
    pub cause: DropCause,
}

/// One part's three lists, and the lines dropped from them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartLists {
    pub key_points: Vec<Point>,
    pub decisions: Vec<Point>,
    pub action_items: Vec<ActionItem>,
    pub dropped: Vec<DroppedItem>,
    /// Lines the model said meant "this list is empty" ("None.", "No
    /// decisions were made."): neither kept nor dropped, but counted so a UI
    /// can tell an intentionally empty list from a filtered one.
    pub assumed_empty: usize,
}

/// One part of a meeting, and its lists or why there are none.
#[derive(Debug)]
pub struct PartSummary {
    /// The first turn's start.
    pub start_ms: u64,
    /// The latest end among the part's turns.
    pub end_ms: u64,
    /// The part's lists, or the error from asking the model for them. A part
    /// that fails does not stop the others.
    pub lists: Result<PartLists>,
}

/// How far a summary has got. Parts count from 1, in meeting order, and `of`
/// is the number of parts the meeting was cut into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartProgress {
    /// The part is about to be asked for.
    Started { part: usize, of: usize },
    /// The part has its lists, or its error.
    Finished { part: usize, of: usize },
}

/// Summarise a meeting's turns, part by part, in meeting order.
///
/// `budget_chars` is the most characters of transcript one call may carry,
/// not counting [`INSTRUCTIONS`]. The caller derives it from the provider's
/// context limit; this module does not count tokens.
///
/// `on_progress` hears each part as it starts and as it finishes.
pub fn summarise(
    model: &dyn TextModel,
    turns: &[Turn],
    budget_chars: usize,
    on_progress: &mut dyn FnMut(PartProgress),
) -> Vec<PartSummary> {
    each_part(turns, budget_chars, on_progress, |part| {
        summarise_part(model, part)
    })
}

/// Cut `turns` into parts and `ask` for each in meeting order, saying when
/// each starts and finishes.
fn each_part(
    turns: &[Turn],
    budget_chars: usize,
    on_progress: &mut dyn FnMut(PartProgress),
    ask: impl Fn(&Part) -> Result<PartLists>,
) -> Vec<PartSummary> {
    let parts = split_into_parts(turns, budget_chars);
    let of = parts.len();
    parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            on_progress(PartProgress::Started { part: i + 1, of });
            let lists = ask(part);
            on_progress(PartProgress::Finished { part: i + 1, of });
            PartSummary {
                start_ms: part.start_ms,
                end_ms: part.end_ms,
                lists,
            }
        })
        .collect()
}

fn summarise_part(model: &dyn TextModel, part: &Part) -> Result<PartLists> {
    if !model.ready() {
        return Err(Error::Enhance {
            backend: "text model",
            message: "the model is not ready".to_string(),
        });
    }
    let reply = model.complete(INSTRUCTIONS, &part.labelled)?;
    Ok(read_reply(&reply, part))
}

/// Summarise a meeting's turns with a model that shapes its reply, part by
/// part, in meeting order, as [`summarise`] does with plain text. See the
/// module doc, "A shaped reply".
pub fn summarise_shaped(
    model: &dyn ShapedModel,
    turns: &[Turn],
    budget_chars: usize,
    on_progress: &mut dyn FnMut(PartProgress),
) -> Vec<PartSummary> {
    each_part(turns, budget_chars, on_progress, |part| {
        summarise_part_shaped(model, part)
    })
}

fn summarise_part_shaped(model: &dyn ShapedModel, part: &Part) -> Result<PartLists> {
    if !model.ready() {
        return Err(Error::Enhance {
            backend: "text model",
            message: "the model is not ready".to_string(),
        });
    }
    let reply = model.complete_shaped(INSTRUCTIONS, &part.labelled, &reply_shape(part))?;
    read_shaped_reply(&reply, part)
}

/// The owner a shaped action item names when the transcript does not say who
/// will do it.
pub const NO_OWNER: &str = "none";

/// The no-owner choice in a part where a speaker is labelled [`NO_OWNER`], so
/// the model can still say "no owner" without naming that speaker. A speaker's
/// label cannot hold it: a WebVTT voice label ends at its first `>`.
pub const NO_OWNER_BESIDE_A_NONE_SPEAKER: &str = "<no owner>";

/// The choice that means no owner in `part`: [`NO_OWNER`], unless a speaker is
/// labelled that, when it is [`NO_OWNER_BESIDE_A_NONE_SPEAKER`].
fn no_owner(part: &Part) -> &'static str {
    if part.speakers.iter().any(|speaker| speaker == NO_OWNER) {
        NO_OWNER_BESIDE_A_NONE_SPEAKER
    } else {
        NO_OWNER
    }
}

/// The shape a part's reply is asked for: the three lists in [`INSTRUCTIONS`]'
/// order. Each item is a record of its text and then its quote; an action item
/// has its owner first, a choice of the part's speaker labels or [`NO_OWNER`],
/// and no owner field at all when the part has no named speakers.
///
/// The text comes before the quote. A model writes the fields in the order
/// they are declared, and with the quote first Apple Intelligence copied each
/// quote into its item's text instead of summarising. Measured on a Mac mini
/// (M1) on 2026-09-14 with today's [`INSTRUCTIONS`], 3 runs of each arm, the
/// replies identical within an arm:
///
/// - On an invented 5,702-character part with the owner choice, text then quote
///   gave 7 real summary lines out of 8 items, kept 2 key points, 2 decisions
///   and 3 action items, and dropped 1; 4 of its 8 quotes started with a
///   speaker's label, which the label strip takes off. Quote then text kept 10
///   and dropped none, but every one of the 10 items' text was a copy of its
///   quote. Capping each list at 5 gave 12 of 12 copies, with the same sentence
///   filed in more than one list, so the lists are not capped.
/// - On a seven-turn part of about 600 characters, neither order copied: no
///   item's text was its quote.
///
/// A prewarmed session made no consistent difference to the time. The field
/// descriptions are the ones measured.
fn reply_shape(part: &Part) -> Shape {
    let field = |name: &str, description: &str, shape: Shape| Field {
        name: name.to_string(),
        description: description.to_string(),
        shape,
    };
    let quote = || {
        field(
            "quote",
            "At least four words copied exactly from the transcript, without the speaker's name.",
            Shape::Text,
        )
    };
    let point = Shape::Record {
        name: "Point".to_string(),
        fields: vec![
            field("text", "The point, in a few words.", Shape::Text),
            quote(),
        ],
    };
    let mut action = Vec::new();
    if !part.speakers.is_empty() {
        let no_owner = no_owner(part);
        let mut choices = part.speakers.clone();
        choices.push(no_owner.to_string());
        action.push(field(
            "owner",
            &format!(
                "Who will do it, as the transcript labels that speaker, or {no_owner} when the transcript does not say who."
            ),
            Shape::OneOf {
                name: "Owner".to_string(),
                choices,
            },
        ));
    }
    action.push(field(
        "text",
        "What is to be done, in a few words.",
        Shape::Text,
    ));
    action.push(quote());
    let action = Shape::Record {
        name: "Action".to_string(),
        fields: action,
    };
    Shape::Record {
        name: "Summary".to_string(),
        fields: vec![
            field(
                "key_points",
                "Key points.",
                Shape::List(Box::new(point.clone())),
            ),
            field("decisions", "Decisions made.", Shape::List(Box::new(point))),
            field(
                "action_items",
                "Action items.",
                Shape::List(Box::new(action)),
            ),
        ],
    }
}

/// A shaped reply's three lists, each item checked against the part with the
/// same rule as a line of plain text ([`check_point`]), and an action item's
/// owner kept only when the part confirms it. JSON that is not the shape asked
/// for is an error for the part.
fn read_shaped_reply(reply: &str, part: &Part) -> Result<PartLists> {
    #[derive(serde::Deserialize)]
    struct Reply {
        key_points: Vec<Item>,
        decisions: Vec<Item>,
        action_items: Vec<Item>,
    }
    #[derive(serde::Deserialize)]
    struct Item {
        text: String,
        quote: String,
        #[serde(default)]
        owner: Option<String>,
    }

    let reply: Reply = serde_json::from_str(reply).map_err(|e| Error::Enhance {
        backend: "summary",
        message: format!("the model's reply was not the shape asked for: {e}"),
    })?;
    let spoken = pieces(&part.spoken);
    let mut lists = PartLists::default();
    for (section, items) in [
        (Section::KeyPoints, reply.key_points),
        (Section::Decisions, reply.decisions),
        (Section::ActionItems, reply.action_items),
    ] {
        for item in items {
            // The part's no-owner choice means no owner; a speaker labelled
            // "none" is then named by "none" like any other label.
            let owner = item
                .owner
                .as_deref()
                .map(str::trim)
                .filter(|owner| !owner.is_empty() && *owner != no_owner(part));
            match check_point(&spoken, &part.speakers, item.text.trim(), &item.quote) {
                Ok(point) => match section {
                    Section::KeyPoints => lists.key_points.push(part.point(point)),
                    Section::Decisions => lists.decisions.push(part.point(point)),
                    Section::ActionItems => lists.action_items.push(ActionItem {
                        owner: owner
                            .filter(|owner| confirmed(owner, part))
                            .map(str::to_string),
                        said: part.said_at(point.at),
                        text: point.text,
                        quote: point.quote,
                    }),
                },
                Err(reason) => lists.dropped.push(DroppedItem {
                    section: Some(section),
                    line: match owner {
                        Some(owner) => {
                            format!("[{owner}] {} \"{}\"", item.text.trim(), item.quote.trim())
                        }
                        None => format!("{} \"{}\"", item.text.trim(), item.quote.trim()),
                    },
                    cause: DropCause::Guard(reason),
                }),
            }
        }
    }
    Ok(lists)
}

/// The turns one call carries.
struct Part {
    start_ms: u64,
    end_ms: u64,
    /// One line per turn, or per piece of a turn: what the model is sent.
    labelled: String,
    /// The same words without their labels: what quotes are checked against.
    spoken: String,
    /// Everyone who speaks in the part.
    speakers: Vec<String>,
    /// Each line's turn, in order, with the index among the words of
    /// `spoken` (split as quotes are matched) where the line's words begin.
    lines: Vec<(usize, Said)>,
}

impl Part {
    /// The turn the word at `at` in `spoken` belongs to: the last line whose
    /// words begin at or before it. A line with no words begins where the
    /// next one does, so the later of the two, the one with the word, is
    /// taken.
    fn said_at(&self, at: usize) -> Said {
        self.lines
            .iter()
            .rev()
            .find(|(first, _)| *first <= at)
            .map(|(_, said)| said.clone())
            .expect("a part has a line, and its first line's words begin at 0")
    }

    /// A kept point with where its quote was said.
    fn point(&self, point: SummaryPoint) -> Point {
        Point {
            said: self.said_at(point.at),
            text: point.text,
            quote: point.quote,
        }
    }
}

/// The parts [`meeting_parts::split`] cuts, each line sent with its speaker's
/// name, and each part's words and speakers gathered for the quote and owner
/// checks.
fn split_into_parts(turns: &[Turn], budget: usize) -> Vec<Part> {
    meeting_parts::split(turns, budget, Labels::Named)
        .into_iter()
        .map(|lines| {
            let mut part = Part {
                start_ms: lines.first().map_or(0, |line| line.start_ms),
                end_ms: 0,
                labelled: String::new(),
                spoken: String::new(),
                speakers: Vec::new(),
                lines: Vec::new(),
            };
            let mut words = 0;
            for line in &lines {
                part.lines.push((
                    words,
                    Said {
                        turn: line.turn,
                        start_ms: line.start_ms,
                    },
                ));
                // Lines are joined with a space, which matching splits at, so
                // the part's words are each line's words in turn.
                words += pieces(&line.text).len();
                if !part.labelled.is_empty() {
                    part.labelled.push('\n');
                    part.spoken.push(' ');
                }
                part.labelled.push_str(&line.sent(Labels::Named));
                part.spoken.push_str(&line.text);
                part.end_ms = part.end_ms.max(line.end_ms);
                if let Some(name) = line.speaker {
                    if !part.speakers.iter().any(|speaker| speaker == name) {
                        part.speakers.push(name.to_string());
                    }
                }
            }
            part
        })
        .collect()
}

/// A reply's three lists, each checked against the part.
fn read_reply(reply: &str, part: &Part) -> PartLists {
    let mut sections: [(Section, Vec<&str>); 3] = [
        (Section::KeyPoints, Vec::new()),
        (Section::Decisions, Vec::new()),
        (Section::ActionItems, Vec::new()),
    ];
    let mut current: Option<Section> = None;
    let mut lists = PartLists::default();
    for line in reply.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(section) = heading(line) {
            current = Some(section);
        } else if let Some(section) = current {
            if says_empty(line) {
                lists.assumed_empty += 1;
            } else if let Some((_, lines)) = sections.iter_mut().find(|(s, _)| *s == section) {
                lines.push(line);
            }
        } else {
            lists.dropped.push(DroppedItem {
                section: None,
                line: line.to_string(),
                cause: DropCause::OutsideSections,
            });
        }
    }

    for (section, lines) in sections {
        let guarded = guard_summary_labelled(&part.spoken, &part.speakers, &lines.join("\n"));
        lists
            .dropped
            .extend(guarded.dropped.into_iter().map(|dropped| DroppedItem {
                section: Some(section),
                line: dropped.line,
                cause: DropCause::Guard(dropped.reason),
            }));
        let points = |kept: Vec<SummaryPoint>| kept.into_iter().map(|p| part.point(p)).collect();
        match section {
            Section::KeyPoints => lists.key_points = points(guarded.kept),
            Section::Decisions => lists.decisions = points(guarded.kept),
            Section::ActionItems => {
                lists.action_items = guarded
                    .kept
                    .into_iter()
                    .map(|point| action_item(point, part))
                    .collect()
            }
        }
    }
    lists
}

/// The phrases a heading may start with, whole-word, case-insensitive. The
/// same section is reached from more than one phrase, so the phrase itself
/// carries no information beyond which section it names.
const HEADING_PHRASES: &[(&str, Section)] = &[
    ("key points", Section::KeyPoints),
    ("key point", Section::KeyPoints),
    ("decisions", Section::Decisions),
    ("decision", Section::Decisions),
    ("action items", Section::ActionItems),
    ("action item", Section::ActionItems),
];

/// The section a heading line names, whatever its Markdown (`#`, bold or
/// italics), a leading numbered-list marker (`1.` or `2)`), a trailing
/// colon, any case, singular or plural, and whatever words follow the
/// heading phrase ("Action Items Discussed", "Key points from the
/// meeting") — provided the line, after that stripping, is at most six
/// words and does not end in sentence punctuation (`.`, `!` or `?`; a
/// trailing colon is fine). Those two checks are what keep a sentence that
/// happens to start with a heading phrase, such as "Decisions were made to
/// ship.", from being read as a heading: it is neither short nor
/// unpunctuated the way an actual heading is. A line holding a quote mark
/// is never a heading either, so an item line is never mistaken for one.
///
/// This is a prefix rule, and it has two accepted limits, both consequences
/// of matching only at the start of the line: "Next steps / action items"
/// is not recognised, because the heading phrase is not the first thing on
/// the line; and "Action items agreed by the team" IS recognised, because it
/// starts with the phrase, is six words or fewer, and does not end in
/// sentence punctuation, even though it names who agreed the list rather
/// than only what follows it.
fn heading(line: &str) -> Option<Section> {
    if line.contains(['"', '“', '”', '«', '»', '„']) {
        return None;
    }
    let bare: String = line
        .chars()
        .filter(|c| !matches!(c, '#' | '*' | '_'))
        .collect();
    let bare = strip_numbered_marker(bare.trim()).trim();
    if bare.ends_with(['.', '!', '?']) || bare.split_whitespace().count() > 6 {
        return None;
    }
    let bare = bare.trim_end_matches(':').trim().to_lowercase();

    HEADING_PHRASES.iter().find_map(|(phrase, section)| {
        let rest = bare.strip_prefix(phrase)?;
        let ends_the_word = rest.chars().next().is_none_or(|c| !c.is_alphanumeric());
        ends_the_word.then_some(*section)
    })
}

/// A line after a leading numbered-list marker (a run of digits then `.` or
/// `)` then whitespace or the end of the line) is removed. A line without
/// one is returned unchanged.
fn strip_numbered_marker(line: &str) -> &str {
    let digits_end = line
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map_or(line.len(), |(at, _)| at);
    if digits_end == 0 {
        return line;
    }
    let rest = &line[digits_end..];
    match rest.strip_prefix(['.', ')']) {
        Some(after) if after.is_empty() || after.starts_with(char::is_whitespace) => {
            after.trim_start()
        }
        _ => line,
    }
}

/// Whether a line says its list is empty: no quote marks, and a first word
/// of "none", "nothing", "n/a" or "no".
fn says_empty(line: &str) -> bool {
    if line.contains(['"', '“', '”', '«', '»', '„']) {
        return false;
    }
    let first = line
        .trim_start_matches(['-', '*', '•'])
        .split_whitespace()
        .next()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/')
                .to_lowercase()
        });
    matches!(first.as_deref(), Some("none" | "nothing" | "n/a" | "no"))
}

/// An action item from a kept point: the owner taken out of its text, and
/// kept only when the part confirms the name.
fn action_item(point: SummaryPoint, part: &Part) -> ActionItem {
    let (owner, text) = match split_owner(&point.text) {
        Some((owner, rest)) => (
            confirmed(owner, part).then(|| owner.to_string()),
            rest.to_string(),
        ),
        None => (None, point.text.clone()),
    };
    ActionItem {
        text,
        quote: point.quote,
        owner,
        said: part.said_at(point.at),
    }
}

/// `[Owner] what to do`: the name in square brackets at the very start of the
/// item, and the text after them. A bracket anywhere else is part of the item.
fn split_owner(text: &str) -> Option<(&str, &str)> {
    let (inside, rest) = text.strip_prefix('[')?.split_once(']')?;
    let owner = inside.trim();
    (!owner.is_empty()).then(|| (owner, rest.trim_start_matches([':', '-', ' ']).trim()))
}

/// Whether the part confirms a name: every one of its word pieces is among
/// one speaker's label pieces (lowercased) — so `[Aoife]` matches the label
/// "Aoife Byrne". This is the only rule; a name only said in the speech,
/// however it is capitalised or however many words it has, is never
/// confirmed.
fn confirmed(name: &str, part: &Part) -> bool {
    let name_pieces = pieces(name);
    if name_pieces.is_empty() {
        return false;
    }
    confirmed_by_label(&name_pieces, part)
}

/// Every one of `name_pieces` is among a single speaker's label pieces.
fn confirmed_by_label(name_pieces: &[String], part: &Part) -> bool {
    part.speakers.iter().any(|speaker| {
        let label_pieces = pieces(speaker);
        name_pieces.iter().all(|piece| label_pieces.contains(piece))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A model with canned replies, one per call in order, that records what
    /// it was sent. Once the replies run out it answers with nothing.
    struct Fake {
        ready: bool,
        replies: Mutex<Vec<Result<String>>>,
        sent: Mutex<Vec<(String, String)>>,
    }

    impl Fake {
        fn replying(replies: Vec<Result<String>>) -> Self {
            Self {
                ready: true,
                replies: Mutex::new(replies.into_iter().rev().collect()),
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent_texts(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|(_, text)| text.clone())
                .collect()
        }
    }

    impl TextModel for Fake {
        fn ready(&self) -> bool {
            self.ready
        }

        fn complete(&self, instructions: &str, text: &str) -> Result<String> {
            self.sent
                .lock()
                .unwrap()
                .push((instructions.to_string(), text.to_string()));
            self.replies
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Ok(String::new()))
        }
    }

    fn turn(speaker: Option<&str>, start_ms: u64, end_ms: u64, text: &str) -> Turn {
        Turn {
            speaker: speaker.map(str::to_string),
            start_ms,
            end_ms,
            text: text.to_string(),
        }
    }

    fn failure(message: &str) -> Error {
        Error::Enhance {
            backend: "fake",
            message: message.to_string(),
        }
    }

    /// A short meeting that fits in one part.
    fn meeting() -> Vec<Turn> {
        vec![
            turn(
                Some("Aoife Byrne"),
                0,
                4_000,
                "We need to ship the parser by Friday.",
            ),
            turn(
                Some("Brian"),
                4_000,
                7_000,
                "Then we test the parser over the weekend.",
            ),
            turn(
                Some("Aoife Byrne"),
                7_000,
                9_000,
                "Ciara said she will write the release notes.",
            ),
        ]
    }

    fn only_part(reply: &str) -> PartLists {
        let model = Fake::replying(vec![Ok(reply.to_string())]);
        let mut parts = summarise(&model, &meeting(), 10_000, &mut |_| {});
        assert_eq!(parts.len(), 1, "{parts:?}");
        parts.remove(0).lists.expect("the part was summarised")
    }

    fn point(text: &str, quote: &str, said: Said) -> Point {
        Point {
            text: text.to_string(),
            quote: quote.to_string(),
            said,
        }
    }

    /// Where a quote from turn `index` of [`meeting`] was said.
    fn said(index: usize) -> Said {
        Said {
            turn: index,
            start_ms: meeting()[index].start_ms,
        }
    }

    /// A model that shapes its reply: one canned reply for every call, and a
    /// record of what it was sent.
    struct Shaped {
        reply: std::result::Result<String, String>,
        sent: Mutex<Vec<(String, String, Shape)>>,
    }

    impl Shaped {
        fn replying(reply: &str) -> Self {
            Self {
                reply: Ok(reply.to_string()),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl ShapedModel for Shaped {
        fn ready(&self) -> bool {
            true
        }

        fn complete_shaped(&self, instructions: &str, text: &str, shape: &Shape) -> Result<String> {
            self.sent.lock().unwrap().push((
                instructions.to_string(),
                text.to_string(),
                shape.clone(),
            ));
            self.reply.clone().map_err(|message| failure(&message))
        }
    }

    /// The fields of the shape's action item record.
    fn action_fields(shape: &Shape) -> Vec<Field> {
        let Shape::Record { fields, .. } = shape else {
            panic!("the shape is not a record: {shape:?}");
        };
        let lists = fields
            .iter()
            .find(|field| field.name == "action_items")
            .expect("an action_items list");
        let Shape::List(item) = &lists.shape else {
            panic!("action_items is not a list");
        };
        let Shape::Record { fields, .. } = item.as_ref() else {
            panic!("an action item is not a record");
        };
        fields.clone()
    }

    #[test]
    fn a_shaped_reply_is_checked_item_by_item_with_owners_the_part_confirms() {
        let model = Shaped::replying(
            r#"{
                "key_points": [
                    {"text": "The parser ships Friday", "quote": "Aoife Byrne: We need to ship the parser by Friday."}
                ],
                "decisions": [
                    {"text": "Testing at the weekend", "quote": "we test the parser over the weekend"}
                ],
                "action_items": [
                    {"owner": "Brian", "text": "Test the parser", "quote": "Then we test the parser over the weekend."},
                    {"owner": "none", "text": "Write the release notes", "quote": "she will write the release notes"},
                    {"owner": "Ciara", "text": "Ship on Monday", "quote": "we will ship it on Monday for sure"}
                ]
            }"#,
        );

        let mut parts = summarise_shaped(&model, &meeting(), 10_000, &mut |_| {});

        assert_eq!(parts.len(), 1, "{parts:?}");
        let lists = parts.remove(0).lists.expect("the part was summarised");
        // The speaker's label at the start of the quote was taken off.
        assert_eq!(
            lists.key_points,
            vec![point(
                "The parser ships Friday",
                "We need to ship the parser by Friday.",
                said(0)
            )]
        );
        assert_eq!(lists.decisions.len(), 1);
        assert_eq!(
            lists.action_items,
            vec![
                ActionItem {
                    text: "Test the parser".to_string(),
                    quote: "Then we test the parser over the weekend.".to_string(),
                    owner: Some("Brian".to_string()),
                    said: said(1),
                },
                ActionItem {
                    text: "Write the release notes".to_string(),
                    quote: "she will write the release notes".to_string(),
                    owner: None,
                    said: said(2),
                },
            ]
        );
        assert_eq!(
            lists.dropped,
            vec![DroppedItem {
                section: Some(Section::ActionItems),
                line: "[Ciara] Ship on Monday \"we will ship it on Monday for sure\"".to_string(),
                cause: DropCause::Guard(DropReason::NotFound),
            }]
        );
    }

    #[test]
    fn the_shape_asks_for_an_owner_from_the_parts_speakers_only_when_it_has_names() {
        let named = Shaped::replying(r#"{"key_points":[],"decisions":[],"action_items":[]}"#);
        summarise_shaped(&named, &meeting(), 10_000, &mut |_| {});
        let sent = named.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, INSTRUCTIONS);
        assert!(
            sent[0].1.starts_with("Aoife Byrne: We need to ship"),
            "{}",
            sent[0].1
        );
        let fields = action_fields(&sent[0].2);
        assert_eq!(
            fields
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["owner", "text", "quote"]
        );
        assert_eq!(
            fields[0].shape,
            Shape::OneOf {
                name: "Owner".to_string(),
                choices: vec![
                    "Aoife Byrne".to_string(),
                    "Brian".to_string(),
                    "none".to_string()
                ],
            }
        );

        let unnamed = Shaped::replying(r#"{"key_points":[],"decisions":[],"action_items":[]}"#);
        let turns = vec![turn(
            None,
            0,
            4_000,
            "We need to ship the parser by Friday.",
        )];
        summarise_shaped(&unnamed, &turns, 10_000, &mut |_| {});
        let sent = unnamed.sent.lock().unwrap().clone();
        assert_eq!(
            action_fields(&sent[0].2)
                .iter()
                .map(|field| field.name.as_str())
                .collect::<Vec<_>>(),
            ["text", "quote"]
        );
    }

    /// Text comes before quote in every item record, pinned here rather than
    /// left to a tidy-up. With the quote first, Apple Intelligence copied each
    /// quote into the item's text instead of summarising: 10 of 10 items on a
    /// full 5,702-character part, measured on 2026-09-14.
    #[test]
    fn every_item_record_asks_for_its_text_before_its_quote() {
        let model = Shaped::replying(r#"{"key_points":[],"decisions":[],"action_items":[]}"#);
        summarise_shaped(&model, &meeting(), 10_000, &mut |_| {});
        let sent = model.sent.lock().unwrap().clone();

        let Shape::Record { fields, .. } = &sent[0].2 else {
            panic!("the shape is not a record");
        };
        assert_eq!(fields.len(), 3);
        for list in fields {
            let Shape::List(item) = &list.shape else {
                panic!("{} is not a list", list.name);
            };
            let Shape::Record {
                fields: item_fields,
                ..
            } = item.as_ref()
            else {
                panic!("an item of {} is not a record", list.name);
            };
            let order: Vec<&str> = item_fields
                .iter()
                .map(|field| field.name.as_str())
                .filter(|name| *name != "owner")
                .collect();
            assert_eq!(order, ["text", "quote"], "{}", list.name);
        }
    }

    #[test]
    fn a_speaker_labelled_none_keeps_the_action_items_they_own() {
        let model = Shaped::replying(
            r#"{"key_points": [], "decisions": [], "action_items": [
                {"owner": "none", "text": "Ship the parser", "quote": "We need to ship the parser by Friday."}
            ]}"#,
        );
        let turns = vec![turn(
            Some("none"),
            0,
            4_000,
            "We need to ship the parser by Friday.",
        )];

        let mut parts = summarise_shaped(&model, &turns, 10_000, &mut |_| {});

        let lists = parts.remove(0).lists.expect("the part was summarised");
        assert_eq!(lists.action_items.len(), 1, "{lists:?}");
        assert_eq!(lists.action_items[0].owner.as_deref(), Some("none"));
    }

    #[test]
    fn beside_a_speaker_labelled_none_the_model_can_still_say_no_owner() {
        let model = Shaped::replying(
            r#"{"key_points": [], "decisions": [], "action_items": [
                {"owner": "<no owner>", "text": "Ship the parser", "quote": "We need to ship the parser by Friday."}
            ]}"#,
        );
        let turns = vec![turn(
            Some("none"),
            0,
            4_000,
            "We need to ship the parser by Friday.",
        )];

        let mut parts = summarise_shaped(&model, &turns, 10_000, &mut |_| {});

        let sent = model.sent.lock().unwrap().clone();
        assert_eq!(
            action_fields(&sent[0].2)[0].shape,
            Shape::OneOf {
                name: "Owner".to_string(),
                choices: vec!["none".to_string(), "<no owner>".to_string()],
            }
        );
        let lists = parts.remove(0).lists.expect("the part was summarised");
        assert_eq!(lists.action_items.len(), 1, "{lists:?}");
        assert_eq!(lists.action_items[0].owner, None);
    }

    #[test]
    fn a_reply_that_is_not_the_shape_asked_for_is_an_error_for_its_part() {
        let model = Shaped::replying("Key points\n- A point \"we need to ship the parser\"");

        let parts = summarise_shaped(&model, &meeting(), 10_000, &mut |_| {});

        let error = parts[0].lists.as_ref().unwrap_err();
        assert!(
            error.to_string().contains("not the shape asked for"),
            "{error}"
        );
    }

    #[test]
    fn a_plain_reply_quote_that_starts_with_its_speakers_label_is_kept_without_it() {
        let lists = only_part(
            "Key points\n\
             - Ship on Friday \"Aoife Byrne: We need to ship the parser by Friday.\"\n\
             Decisions\nNone.\nAction items\nNone.",
        );

        assert_eq!(
            lists.key_points,
            vec![point(
                "Ship on Friday",
                "We need to ship the parser by Friday.",
                said(0)
            )]
        );
        assert!(lists.dropped.is_empty(), "{lists:?}");
    }

    #[test]
    fn headings_are_found_in_several_styles() {
        let replies = [
            "## Key Points\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             **Decisions:**\n\
             - Testing is at the weekend \"Then we test the parser over the weekend\"\n\
             ACTION ITEMS\n\
             - Write the release notes \"she will write the release notes\"",
            "Key point:\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             # decisions\n\
             - Testing is at the weekend \"Then we test the parser over the weekend\"\n\
             __Action items__\n\
             - Write the release notes \"she will write the release notes\"",
        ];
        for reply in replies {
            let lists = only_part(reply);
            assert_eq!(
                lists,
                PartLists {
                    key_points: vec![point(
                        "The parser ships Friday",
                        "We need to ship the parser by Friday",
                        said(0)
                    )],
                    decisions: vec![point(
                        "Testing is at the weekend",
                        "Then we test the parser over the weekend",
                        said(1)
                    )],
                    action_items: vec![ActionItem {
                        text: "Write the release notes".to_string(),
                        quote: "she will write the release notes".to_string(),
                        owner: None,
                        said: said(2),
                    }],
                    dropped: Vec::new(),
                    assumed_empty: 0,
                },
                "{reply}"
            );
        }
    }

    #[test]
    fn an_item_whose_quote_was_never_said_is_dropped() {
        let lists = only_part(
            "Key points\n\
             - The budget doubles \"we agreed to double the budget\"\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisions\n\
             None.\n\
             Action items\n\
             None.",
        );
        assert_eq!(
            lists.key_points,
            vec![point(
                "The parser ships Friday",
                "We need to ship the parser by Friday",
                said(0)
            )]
        );
        assert_eq!(
            lists.dropped,
            vec![DroppedItem {
                section: Some(Section::KeyPoints),
                line: "- The budget doubles \"we agreed to double the budget\"".to_string(),
                cause: DropCause::Guard(DropReason::NotFound),
            }]
        );
    }

    #[test]
    fn a_quote_that_copies_a_speaker_label_after_its_start_is_dropped() {
        // A label at the very start is taken off; anywhere else it stays in the
        // quote, which is then not found.
        let lists = only_part(
            "Key points\n\
             - The deadline \"We need to ship the parser by Friday. Brian: Then we test the parser\"",
        );
        assert!(lists.key_points.is_empty(), "{lists:?}");
        assert_eq!(lists.dropped.len(), 1, "{lists:?}");
        assert_eq!(lists.dropped[0].section, Some(Section::KeyPoints));
        assert_eq!(
            lists.dropped[0].cause,
            DropCause::Guard(DropReason::NotFound)
        );
    }

    #[test]
    fn an_owner_is_taken_out_of_the_item_and_kept_only_when_the_part_confirms_it() {
        let lists = only_part(
            "Key points\nNone.\nDecisions\nNone.\nAction items\n\
             - [Aoife Byrne] Ship the parser \"We need to ship the parser by Friday\"\n\
             - [ciara] Write the release notes \"she will write the release notes\"\n\
             - [Dermot] Test the parser \"Then we test the parser over the weekend\"",
        );
        assert_eq!(
            lists.action_items,
            vec![
                // Speaks in the part; the name has a space in it.
                ActionItem {
                    text: "Ship the parser".to_string(),
                    quote: "We need to ship the parser by Friday".to_string(),
                    owner: Some("Aoife Byrne".to_string()),
                    said: said(0),
                },
                // "ciara" is not a speaker label in this part, so there is no
                // owner: an owner is confirmed only from the speaker labels.
                ActionItem {
                    text: "Write the release notes".to_string(),
                    quote: "she will write the release notes".to_string(),
                    owner: None,
                    said: said(2),
                },
                // Neither speaks nor is named: the item stays, the owner goes.
                ActionItem {
                    text: "Test the parser".to_string(),
                    quote: "Then we test the parser over the weekend".to_string(),
                    owner: None,
                    said: said(1),
                },
            ]
        );
        assert!(lists.dropped.is_empty(), "{lists:?}");
    }

    #[test]
    fn a_bracket_after_the_start_is_part_of_the_item_not_an_owner() {
        let lists = only_part(
            "Action items\n\
             - Ship [the new] parser \"We need to ship the parser by Friday\"",
        );
        assert_eq!(
            lists.action_items,
            vec![ActionItem {
                text: "Ship [the new] parser".to_string(),
                quote: "We need to ship the parser by Friday".to_string(),
                owner: None,
                said: said(0),
            }]
        );
    }

    #[test]
    fn an_empty_list_stays_empty_and_nothing_is_counted_as_dropped() {
        let lists = only_part(
            "Key points\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisions\n\
             None.\n\
             Action items\n\
             - No action items were agreed.",
        );
        assert_eq!(lists.key_points.len(), 1, "{lists:?}");
        assert!(lists.decisions.is_empty(), "{lists:?}");
        assert!(lists.action_items.is_empty(), "{lists:?}");
        assert!(lists.dropped.is_empty(), "{lists:?}");
    }

    #[test]
    fn a_turn_longer_than_the_budget_is_split_at_sentence_ends() {
        let turns = vec![turn(
            Some("Aoife"),
            0,
            60_000,
            "We ship the parser on Friday. Then we test it over the weekend. \
             After that we write the notes. Then we tell everyone.",
        )];
        let model = Fake::replying(Vec::new());
        let parts = summarise(&model, &turns, 60, &mut |_| {});
        assert_eq!(
            model.sent_texts(),
            vec![
                "Aoife: We ship the parser on Friday.",
                "Aoife: Then we test it over the weekend.",
                "Aoife: After that we write the notes. Then we tell everyone.",
            ]
        );
        for text in model.sent_texts() {
            assert!(text.chars().count() <= 60, "{text}");
        }
        for part in &parts {
            assert_eq!((part.start_ms, part.end_ms), (0, 60_000));
        }
    }

    #[test]
    fn each_part_is_reported_as_it_starts_and_as_it_finishes_in_meeting_order() {
        let mut turns = meeting();
        turns.push(turn(Some("Brian"), 9_000, 12_000, "Agreed."));
        let model = Fake::replying(Vec::new());
        let mut heard = Vec::new();

        let parts = summarise(&model, &turns, 100, &mut |progress| heard.push(progress));

        assert_eq!(parts.len(), 2);
        assert_eq!(
            heard,
            vec![
                PartProgress::Started { part: 1, of: 2 },
                PartProgress::Finished { part: 1, of: 2 },
                PartProgress::Started { part: 2, of: 2 },
                PartProgress::Finished { part: 2, of: 2 },
            ]
        );
    }

    #[test]
    fn parts_come_back_in_meeting_order_each_checked_against_its_own_words() {
        let mut turns = meeting();
        turns.push(turn(Some("Brian"), 9_000, 12_000, "Agreed."));
        let model = Fake::replying(vec![
            Ok("Key points\n\
                - The parser ships Friday \"We need to ship the parser by Friday\""
                .to_string()),
            Ok("Key points\n\
                - Release notes are Ciara's \"she will write the release notes\"\n\
                - The parser ships Friday \"We need to ship the parser by Friday\""
                .to_string()),
        ]);
        let parts = summarise(&model, &turns, 100, &mut |_| {});

        assert_eq!(
            model.sent_texts(),
            vec![
                "Aoife Byrne: We need to ship the parser by Friday.\n\
                 Brian: Then we test the parser over the weekend.",
                "Aoife Byrne: Ciara said she will write the release notes.\n\
                 Brian: Agreed.",
            ]
        );
        assert_eq!(
            parts
                .iter()
                .map(|part| (part.start_ms, part.end_ms))
                .collect::<Vec<_>>(),
            vec![(0, 7_000), (7_000, 12_000)]
        );

        let first = parts[0].lists.as_ref().unwrap();
        assert_eq!(first.key_points.len(), 1);
        // The second part's reply repeats a quote from the first part, which
        // is not in the second part's words, so it is dropped there.
        let second = parts[1].lists.as_ref().unwrap();
        assert_eq!(
            second.key_points,
            vec![point(
                "Release notes are Ciara's",
                "she will write the release notes",
                said(2)
            )]
        );
        assert_eq!(second.dropped.len(), 1, "{second:?}");
        assert_eq!(
            second.dropped[0].cause,
            DropCause::Guard(DropReason::NotFound)
        );
    }

    #[test]
    fn a_failed_part_leaves_the_others_summarised() {
        let model = Fake::replying(vec![
            Ok("Key points\n- Deadline \"We need to ship the parser by Friday\"".to_string()),
            Err(failure("the request timed out")),
            Ok("Key points\n- Notes \"she will write the release notes\"".to_string()),
        ]);
        let parts = summarise(&model, &meeting(), 60, &mut |_| {});
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0].lists.as_ref().unwrap().key_points.len(), 1);
        let error = parts[1].lists.as_ref().unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(parts[2].lists.as_ref().unwrap().key_points.len(), 1);
    }

    #[test]
    fn a_model_that_is_not_ready_is_not_asked() {
        let model = Fake {
            ready: false,
            ..Fake::replying(Vec::new())
        };
        let parts = summarise(&model, &meeting(), 60, &mut |_| {});
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|part| part.lists.is_err()), "{parts:?}");
        assert!(model.sent_texts().is_empty());

        assert!(summarise(&model, &[], 60, &mut |_| {}).is_empty());
    }

    #[test]
    fn an_owner_confirmed_by_a_speaker_label_is_kept() {
        let lists = only_part(
            "Key points\nNone.\nDecisions\nNone.\nAction items\n\
             - [Aoife Byrne] Ship the parser \"We need to ship the parser by Friday\"",
        );
        assert_eq!(
            lists.action_items,
            vec![ActionItem {
                text: "Ship the parser".to_string(),
                quote: "We need to ship the parser by Friday".to_string(),
                owner: Some("Aoife Byrne".to_string()),
                said: said(0),
            }]
        );
    }

    #[test]
    fn an_owner_confirmed_from_a_speaker_label_even_when_named_by_first_name_only() {
        let lists = only_part(
            "Key points\nNone.\nDecisions\nNone.\nAction items\n\
             - [Aoife] Ship the parser \"We need to ship the parser by Friday\"",
        );
        assert_eq!(
            lists.action_items,
            vec![ActionItem {
                text: "Ship the parser".to_string(),
                quote: "We need to ship the parser by Friday".to_string(),
                owner: Some("Aoife".to_string()),
                said: said(0),
            }]
        );
    }

    #[test]
    fn a_full_name_said_only_in_speech_and_not_a_speaker_label_gives_no_owner() {
        // "Priya Shah" is a full name said in the transcript, but nobody in
        // the part speaks under that label, so there is no owner: only a
        // speaker label can confirm one now.
        let turns = vec![turn(
            Some("Brian"),
            0,
            4_000,
            "Let's ask Priya Shah to send the notes after the call.",
        )];
        let model = Fake::replying(vec![Ok(
            "Key points\nNone.\nDecisions\nNone.\nAction items\n\
             - [Priya Shah] Send the notes \"ask Priya Shah to send the notes\""
                .to_string(),
        )]);
        let parts = summarise(&model, &turns, 10_000, &mut |_| {});
        let lists = parts[0].lists.as_ref().unwrap();
        assert_eq!(lists.action_items.len(), 1, "the item is kept: {lists:?}");
        assert_eq!(lists.action_items[0].owner, None, "{lists:?}");
    }

    #[test]
    fn a_day_a_place_and_a_pronoun_bracketed_as_owner_give_no_owner() {
        // Each bracketed name is said in the speech, capitalised, as the
        // review found them -- [Project Atlas] and [New York] passed the old
        // full-name check that way -- and none is a speaker label in the
        // part, so none is confirmed.
        let turns = vec![
            turn(
                Some("Aoife Byrne"),
                0,
                4_000,
                "We need Project Atlas to ship the parser by Friday.",
            ),
            turn(
                Some("Brian"),
                4_000,
                8_000,
                "The New York team will write the release notes.",
            ),
        ];
        let model = Fake::replying(vec![Ok(
            "Key points\nNone.\nDecisions\nNone.\nAction items\n\
             - [Friday] Ship the parser \"ship the parser by Friday\"\n\
             - [Project Atlas] Ship the parser \"We need Project Atlas to ship\"\n\
             - [New York] Write the notes \"The New York team will write\"\n\
             - [We] Ship the parser \"We need Project Atlas to ship\""
                .to_string(),
        )]);
        let parts = summarise(&model, &turns, 10_000, &mut |_| {});
        let lists = parts[0].lists.as_ref().unwrap();
        assert_eq!(lists.action_items.len(), 4, "every item is kept: {lists:?}");
        for item in &lists.action_items {
            assert_eq!(item.owner, None, "{item:?}");
        }
    }

    #[test]
    fn a_name_stitched_across_a_turn_boundary_gives_no_owner() {
        // "...loop in Priya." ends one turn and "Kildare has..." starts the
        // next, spoken by a different, unrelated speaker. Neither speaker's
        // label is "Priya Kildare", so there is no owner.
        let turns = vec![
            turn(Some("Aoife Byrne"), 0, 4_000, "Let's loop in Priya."),
            turn(
                Some("Brian"),
                4_000,
                8_000,
                "Kildare has the context for this.",
            ),
        ];
        let model = Fake::replying(vec![Ok(
            "Key points\nNone.\nDecisions\nNone.\nAction items\n\
             - [Priya Kildare] Send the notes \"loop in Priya Kildare has\""
                .to_string(),
        )]);
        let parts = summarise(&model, &turns, 10_000, &mut |_| {});
        let lists = parts[0].lists.as_ref().unwrap();
        assert_eq!(lists.action_items[0].owner, None, "{lists:?}");
    }

    #[test]
    fn a_name_shared_by_two_speakers_is_still_confirmed() {
        let turns = vec![
            turn(
                Some("Aoife Byrne"),
                0,
                4_000,
                "We need to ship the parser by Friday.",
            ),
            turn(
                Some("Aoife Walsh"),
                4_000,
                8_000,
                "Then we test the parser over the weekend.",
            ),
        ];
        let model = Fake::replying(vec![Ok(
            "Key points\nNone.\nDecisions\nNone.\nAction items\n\
             - [Aoife] Ship the parser \"We need to ship the parser by Friday\""
                .to_string(),
        )]);
        let parts = summarise(&model, &turns, 10_000, &mut |_| {});
        let lists = parts[0].lists.as_ref().unwrap();
        assert_eq!(
            lists.action_items,
            vec![ActionItem {
                text: "Ship the parser".to_string(),
                quote: "We need to ship the parser by Friday".to_string(),
                owner: Some("Aoife".to_string()),
                said: said(0),
            }]
        );
    }

    #[test]
    fn headings_are_found_with_numbers_extra_words_and_a_lookalike_is_rejected() {
        let lists = only_part(
            "1. Key points\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisionsmaking\n\
             - Testing is at the weekend \"Then we test the parser over the weekend\"\n\
             Action Items Discussed\n\
             - Write the release notes \"she will write the release notes\"",
        );
        // "Decisionsmaking" is not a heading, so the current section (Key
        // points, from the numbered heading before it) does not change: the
        // stray line and the item that follows it both stay under Key
        // points, and Decisions never receives anything.
        assert_eq!(
            lists.key_points,
            vec![
                point(
                    "The parser ships Friday",
                    "We need to ship the parser by Friday",
                    said(0)
                ),
                point(
                    "Testing is at the weekend",
                    "Then we test the parser over the weekend",
                    said(1)
                ),
            ]
        );
        assert!(lists.decisions.is_empty(), "{lists:?}");
        assert_eq!(
            lists.action_items,
            vec![ActionItem {
                text: "Write the release notes".to_string(),
                quote: "she will write the release notes".to_string(),
                owner: None,
                said: said(2),
            }]
        );
        assert_eq!(
            lists.dropped,
            vec![DroppedItem {
                section: Some(Section::KeyPoints),
                line: "Decisionsmaking".to_string(),
                cause: DropCause::Guard(DropReason::NoQuote),
            }]
        );
    }

    #[test]
    fn a_heading_with_from_the_meeting_suffix_is_recognised() {
        let lists = only_part(
            "Key points from the meeting\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisions\nNone.\nAction items\nNone.",
        );
        assert_eq!(lists.key_points.len(), 1, "{lists:?}");
    }

    #[test]
    fn a_sentence_ending_in_a_full_stop_is_never_a_heading_even_first_in_a_section() {
        // Repro: heading("Decisions were made to ship.") used to return
        // Some(Decisions). A line ending in sentence punctuation is a
        // sentence, not a heading, however it starts.
        let lists = only_part(
            "Key points\nNone.\n\
             Decisions\n\
             Decisions were made to ship.\n\
             - Testing is at the weekend \"Then we test the parser over the weekend\"\n\
             Action items\nNone.",
        );
        assert_eq!(
            lists.decisions,
            vec![point(
                "Testing is at the weekend",
                "Then we test the parser over the weekend",
                said(1)
            )]
        );
        assert_eq!(
            lists.dropped,
            vec![DroppedItem {
                section: Some(Section::Decisions),
                line: "Decisions were made to ship.".to_string(),
                cause: DropCause::Guard(DropReason::NoQuote),
            }]
        );
    }

    #[test]
    fn a_sentence_ending_in_a_full_stop_in_the_middle_of_a_section_is_dropped_items_around_it_stay()
    {
        let lists = only_part(
            "Key points\nNone.\n\
             Decisions\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisions were made to ship.\n\
             - Testing is at the weekend \"Then we test the parser over the weekend\"\n\
             Action items\nNone.",
        );
        assert_eq!(
            lists.decisions,
            vec![
                point(
                    "The parser ships Friday",
                    "We need to ship the parser by Friday",
                    said(0)
                ),
                point(
                    "Testing is at the weekend",
                    "Then we test the parser over the weekend",
                    said(1)
                ),
            ]
        );
        assert_eq!(
            lists.dropped,
            vec![DroppedItem {
                section: Some(Section::Decisions),
                line: "Decisions were made to ship.".to_string(),
                cause: DropCause::Guard(DropReason::NoQuote),
            }]
        );
    }

    #[test]
    fn headings_stay_recognised_under_the_new_word_count_and_punctuation_rules() {
        let lists = only_part(
            "Key points from the meeting\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisions\nNone.\n\
             Action items:\n\
             - Write the release notes \"she will write the release notes\"",
        );
        assert_eq!(lists.key_points.len(), 1, "{lists:?}");
        assert_eq!(lists.action_items.len(), 1, "{lists:?}");
    }

    #[test]
    fn a_heading_phrase_line_over_six_words_is_not_a_heading() {
        // Seven words, no closing punctuation: a sentence, not a heading.
        assert_eq!(heading("Key points we agreed on this week"), None);
        // Six words is still within the limit.
        assert_eq!(
            heading("Key points we agreed this week"),
            Some(Section::KeyPoints)
        );
    }

    #[test]
    fn a_heading_phrase_not_at_the_start_of_the_line_is_not_recognised() {
        // Accepted limit of the prefix rule: the heading phrase must start
        // the line, so "Next steps / action items" is not read as a heading.
        assert_eq!(heading("Next steps / action items"), None);
    }

    #[test]
    fn a_short_unpunctuated_line_starting_with_the_phrase_is_read_as_a_heading() {
        // Accepted limit of the prefix rule, the other way: six words or
        // fewer with no end punctuation reads as a heading even when it
        // names who agreed it, not just what follows.
        assert_eq!(
            heading("Action items agreed by the team"),
            Some(Section::ActionItems)
        );
    }

    #[test]
    fn lines_before_the_first_heading_are_dropped_as_outside_the_sections() {
        let lists = only_part(
            "Here is a stray line before any heading.\n\
             Key points\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisions\nNone.\nAction items\nNone.",
        );
        assert_eq!(
            lists.dropped,
            vec![DroppedItem {
                section: None,
                line: "Here is a stray line before any heading.".to_string(),
                cause: DropCause::OutsideSections,
            }]
        );
    }

    #[test]
    fn a_line_treated_as_an_empty_list_is_counted_as_assumed_empty() {
        let lists = only_part(
            "Key points\n\
             - The parser ships Friday \"We need to ship the parser by Friday\"\n\
             Decisions\n\
             None.\n\
             Action items\n\
             - No action items were agreed.",
        );
        assert_eq!(lists.assumed_empty, 2, "{lists:?}");
        assert!(lists.dropped.is_empty(), "{lists:?}");
    }

    #[test]
    fn each_part_is_sent_with_the_instructions_and_its_speaker_labels() {
        let mut turns = meeting();
        turns.push(turn(None, 9_000, 10_000, "Right, thanks everyone."));
        let model = Fake::replying(Vec::new());
        summarise(&model, &turns, 10_000, &mut |_| {});

        let sent = model.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, INSTRUCTIONS);
        assert_eq!(
            sent[0].1,
            "Aoife Byrne: We need to ship the parser by Friday.\n\
             Brian: Then we test the parser over the weekend.\n\
             Aoife Byrne: Ciara said she will write the release notes.\n\
             Right, thanks everyone."
        );
        assert!(
            INSTRUCTIONS.contains("- [Owner] What they will do \"a quote of at least four words\"")
        );
    }

    #[test]
    fn every_drop_cause_and_section_has_a_name_and_every_cause_a_sentence() {
        let causes = [
            DropCause::Guard(DropReason::NoQuote),
            DropCause::Guard(DropReason::TooShort),
            DropCause::Guard(DropReason::NotFound),
            DropCause::OutsideSections,
        ];

        let names: Vec<&str> = causes.iter().map(|cause| cause.name()).collect();

        assert_eq!(
            names,
            ["no_quote", "too_short", "not_found", "outside_sections"]
        );
        assert!(causes
            .iter()
            .all(|cause| !cause.explain().trim().is_empty()));
        // The sentence names the guard's own minimum.
        assert_eq!(crate::summary::MIN_QUOTE_WORDS, 4);
        assert!(DropCause::Guard(DropReason::TooShort)
            .explain()
            .contains("fewer than four words"));
        assert_eq!(
            [Section::KeyPoints, Section::Decisions, Section::ActionItems].map(Section::name),
            ["key_points", "decisions", "action_items"]
        );
    }

    // Where a quote was said, on the invented long meeting and its answer key.

    const LONG_MEETING: &str = include_str!("../../../fixtures/long-meeting.vtt");
    const LONG_MEETING_KEY: &str = include_str!("../../../fixtures/long-meeting.key.json");

    fn long_meeting() -> Vec<Turn> {
        crate::vtt::parse(LONG_MEETING)
            .expect("the fixture is WebVTT")
            .turns()
    }

    /// A cue start as the VTT and the answer key write it, "00:01:52.000",
    /// in milliseconds.
    fn ms(time: &str) -> u64 {
        let (hms, millis) = time.split_once('.').expect("a time with milliseconds");
        let seconds = hms
            .split(':')
            .map(|part| part.parse::<u64>().expect("a number"))
            .fold(0, |total, part| total * 60 + part);
        seconds * 1_000 + millis.parse::<u64>().expect("milliseconds")
    }

    /// An item for a shaped reply: an action item when it has an owner.
    fn item(owner: Option<&str>, text: &str, quote: &str) -> serde_json::Value {
        match owner {
            Some(owner) => serde_json::json!({"owner": owner, "text": text, "quote": quote}),
            None => serde_json::json!({"text": text, "quote": quote}),
        }
    }

    /// Every item kept from the long meeting, cut at `budget`, when the model
    /// replies with `key_points` and `action_items` for every part: each
    /// item's text and where its quote was said, in meeting order.
    fn said_on_long_meeting(
        key_points: &[serde_json::Value],
        action_items: &[serde_json::Value],
        budget: usize,
    ) -> Vec<(String, Said)> {
        let reply = serde_json::json!({
            "key_points": key_points,
            "decisions": [],
            "action_items": action_items,
        });
        let model = Shaped::replying(&reply.to_string());
        let parts = summarise_shaped(&model, &long_meeting(), budget, &mut |_| {});
        let mut said = Vec::new();
        for part in parts {
            let lists = part.lists.expect("the part was summarised");
            said.extend(lists.key_points.into_iter().map(|p| (p.text, p.said)));
            said.extend(lists.action_items.into_iter().map(|a| (a.text, a.said)));
        }
        said
    }

    /// The whole meeting in one part, and cut into many.
    const BUDGETS: [usize; 2] = [10_000, 1_000];

    /// The misattributed quotes measured on this meeting: each is genuine
    /// transcript text from a turn that is not the one its item is about. The
    /// location is the turn where the words were really said, which is what
    /// lets a reader see the mismatch in one click. Nothing here judges
    /// whether the quote supports the item.
    #[test]
    fn a_misattributed_quote_is_placed_where_its_words_were_really_said() {
        // (owner, item text, quote, the cue where the words are, its speaker)
        let cases = [
            // The release notes item quotes Brian's later turn, not his
            // "I will write the release notes by Friday…".
            (
                Some("Brian Walsh"),
                "Write the release notes by Friday and send them to the whole team.",
                "I will put it in the first paragraph, not at the bottom. Do we keep the old parser around",
                "00:01:52.000",
                "Brian Walsh",
            ),
            // Brian's task, quoting Aoife's instruction, which runs on into the
            // next topic.
            (
                Some("Brian Walsh"),
                "Add the setting and the test this week, before the release candidate.",
                "Add the setting and the test this week, before the release candidate. Now the install guide. I read the draft on Tuesday.",
                "00:02:38.000",
                "Aoife Byrne",
            ),
            // The checksums task quoting Brian's lead-up, not Aoife's "Yes, add
            // the checksums to the script on Monday…".
            (
                Some("Brian Walsh"),
                "Add the checksums to the script on Monday, before the release candidate.",
                "I can add that to the script, but not this week, not with the setting and the test. Could it wait for the release candidate on Monday?",
                "00:08:30.000",
                "Brian Walsh",
            ),
            // A key point quoting the turn after its own (the key's trap): the
            // crash is at 00:00:45, the quote is Aoife's reply.
            (
                None,
                "The Windows build still crashes when a path has a space in it.",
                "Then the Windows build is not part of this release.",
                "00:01:04.000",
                "Aoife Byrne",
            ),
        ];
        let turns = long_meeting();
        let key_points: Vec<_> = cases
            .iter()
            .filter(|case| case.0.is_none())
            .map(|(owner, text, quote, ..)| item(*owner, text, quote))
            .collect();
        let action_items: Vec<_> = cases
            .iter()
            .filter(|case| case.0.is_some())
            .map(|(owner, text, quote, ..)| item(*owner, text, quote))
            .collect();

        for budget in BUDGETS {
            let said = said_on_long_meeting(&key_points, &action_items, budget);
            assert_eq!(said.len(), cases.len(), "budget {budget}: {said:?}");
            for (_, text, quote, cue, speaker) in cases {
                let (_, at) = said
                    .iter()
                    .find(|(kept, _)| kept == text)
                    .unwrap_or_else(|| panic!("{text} was kept"));
                assert_eq!(at.start_ms, ms(cue), "budget {budget}: {text}");
                assert_eq!(turns[at.turn].speaker.as_deref(), Some(speaker), "{text}");
                // The turn it names is the one in the transcript with those
                // words, read straight from the VTT.
                assert_eq!(turns[at.turn].start_ms, at.start_ms, "{text}");
                assert!(turns[at.turn].text.contains(quote), "{text}");
            }
        }
    }

    #[test]
    fn a_quote_from_the_supporting_turn_is_placed_at_the_turn_the_key_names() {
        let key: serde_json::Value =
            serde_json::from_str(LONG_MEETING_KEY).expect("the key is JSON");
        let entry = |list: &str, id: &str| -> (String, u64) {
            let entry = key[list]
                .as_array()
                .expect("a list")
                .iter()
                .find(|entry| entry["id"] == id)
                .unwrap_or_else(|| panic!("{id} is in the key"));
            (
                entry["text"].as_str().expect("text").to_string(),
                ms(entry["support"][0].as_str().expect("a supporting turn")),
            )
        };
        // Quotes copied from each item's first supporting turn.
        let (a1, a1_at) = entry("action_items", "A1");
        let (a9, a9_at) = entry("action_items", "A9");
        let (k9, k9_at) = entry("key_points", "K9");
        let action_items = [
            item(
                Some("Brian Walsh"),
                &a1,
                "I will write the release notes by Friday and send them to the whole team",
            ),
            item(
                Some("Aoife Byrne"),
                &a9,
                "I can take the queue in the mornings",
            ),
        ];
        let key_points = [item(
            None,
            &k9,
            "We published them by hand last time and one of them was wrong",
        )];

        for budget in BUDGETS {
            let said = said_on_long_meeting(&key_points, &action_items, budget);
            let at = |text: &str| {
                said.iter()
                    .find(|(kept, _)| kept == text)
                    .map(|(_, said)| said.start_ms)
            };
            assert_eq!(at(&a1), Some(a1_at), "budget {budget}");
            assert_eq!(at(&a9), Some(a9_at), "budget {budget}");
            assert_eq!(at(&k9), Some(k9_at), "budget {budget}");
        }
    }

    #[test]
    fn a_quote_said_twice_in_the_meeting_is_placed_where_it_is_first_said() {
        let turns = long_meeting();
        // Said by Aoife at 00:04:12 and again in her recap at 00:06:17.
        let quote = "the release candidate on Monday morning";
        let saying: Vec<u64> = turns
            .iter()
            .filter(|turn| turn.text.contains(quote))
            .map(|turn| turn.start_ms)
            .collect();
        assert_eq!(saying, [ms("00:04:12.000"), ms("00:06:17.000")]);

        let said = said_on_long_meeting(
            &[item(
                None,
                "The release candidate is on Monday morning.",
                quote,
            )],
            &[],
            10_000,
        );

        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].1.start_ms, ms("00:04:12.000"));
        assert_eq!(turns[said[0].1.turn].start_ms, ms("00:04:12.000"));
    }

    #[test]
    fn a_quote_that_runs_from_one_turn_into_the_next_is_placed_where_it_starts() {
        // The end of Aoife's turn at 00:04:44 and the start of Brian's at
        // 00:05:01.
        let quote = "the screenshots will take time. I can take the afternoons";
        let said = said_on_long_meeting(
            &[item(None, "The queue is split between them.", quote)],
            &[],
            10_000,
        );

        assert_eq!(said.len(), 1, "{said:?}");
        assert_eq!(said[0].1.start_ms, ms("00:04:44.000"));
        assert_eq!(long_meeting()[said[0].1.turn].start_ms, ms("00:04:44.000"));
    }

    #[test]
    fn a_plain_reply_places_each_quote_in_its_turn_with_that_turns_start() {
        let lists = only_part(
            "Key points\n\
             - Testing at the weekend \"we test the parser over the weekend\"\n\
             Decisions\n\
             - Friday \"We need to ship the parser by Friday\"\n\
             Action items\n\
             - [Aoife Byrne] Notes \"she will write the release notes\"",
        );

        assert_eq!(lists.key_points[0].said, said(1));
        assert_eq!(lists.decisions[0].said, said(0));
        assert_eq!(lists.action_items[0].said, said(2));
        assert_eq!(
            said(1),
            Said {
                turn: 1,
                start_ms: 4_000,
            }
        );
    }

    /// Where a quote that starts on word `at` of [`meeting`]'s one part is
    /// said. Turn 0, "We need to ship the parser by Friday.", is words 0 to 7;
    /// turn 1 starts at word 8 with "Then".
    fn said_at_word(quote: &str, at: usize) -> Said {
        let lists = only_part(&format!("Key points\n- Point \"{quote}\""));
        assert_eq!(lists.key_points.len(), 1, "{lists:?}");
        let point = crate::summary::check_point(
            &pieces(
                &meeting()
                    .iter()
                    .map(|t| t.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            &[],
            "Point",
            quote,
        )
        .expect("the quote is found");
        assert_eq!(point.at, at, "{quote}");
        lists.key_points[0].said.clone()
    }

    #[test]
    fn a_quote_that_starts_on_a_turns_last_word_is_placed_in_that_turn() {
        assert_eq!(said_at_word("Friday. Then we test", 7), said(0));
    }

    #[test]
    fn a_quote_that_starts_on_a_turns_first_word_is_placed_in_that_turn() {
        assert_eq!(said_at_word("Then we test the parser", 8), said(1));
    }

    #[test]
    fn a_turn_cut_into_lines_places_a_quote_in_that_turn() {
        // Too long for one part: the second turn is cut at its sentence ends,
        // and every piece keeps its turn and the turn's start.
        let turns = vec![
            turn(
                Some("Aoife Byrne"),
                0,
                4_000,
                "We need to ship the parser by Friday.",
            ),
            turn(
                Some("Brian"),
                4_000,
                9_000,
                "First sentence of a long turn. Then we test the parser over the weekend.",
            ),
        ];
        // Three parts at 60: the first turn, then each sentence of the second.
        let model = Fake::replying(vec![
            Ok(String::new()),
            Ok(String::new()),
            Ok("Key points\n- Weekend \"we test the parser over the weekend\"".to_string()),
        ]);

        let parts = summarise(&model, &turns, 60, &mut |_| {});
        assert_eq!(parts.len(), 3, "{parts:?}");

        let kept: Vec<Said> = parts
            .into_iter()
            .flat_map(|part| part.lists.expect("summarised").key_points)
            .map(|point| point.said)
            .collect();
        assert_eq!(
            kept,
            [Said {
                turn: 1,
                start_ms: 4_000,
            }]
        );
    }
}
