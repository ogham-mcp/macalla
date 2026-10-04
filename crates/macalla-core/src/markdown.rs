// SPDX-License-Identifier: MIT
//! Write a transcript, and its summary when it has one, as Markdown
//! (CommonMark), alongside [`crate::vtt::write`]'s WebVTT and
//! [`crate::srt::write`]'s SRT.
//!
//! [`write`] is the one way Macalla writes Markdown: every app that saves a
//! transcript as Markdown builds a [`Document`] and hands it here with its
//! own [`Words`], so the structure and the escaping are the same from each.
//! What goes in is each app's own: the desktop app titles a meeting with the
//! file's name and extension, the CLI with its stem, and only the desktop
//! app has a summary. This module returns no sentences of its own: the
//! headings and the out-of-date note come in as [`Words`], from each app's
//! catalogue.
//!
//! The document is built as a node tree and written by comrak's CommonMark
//! writer, not assembled as a formatted string: every field taken from the
//! [`Document`] goes in as its own text node, so a value that happens to look
//! like `# `, `___`, `` ``` `` or `<pre>` stays the text of a paragraph, a
//! list item or a heading's own title. comrak's writer escapes the
//! characters that would otherwise start a block or inline construct, so a
//! hostile `**bold**` or `[link](x)` renders as literal characters and not
//! as emphasis or a link. Flattening fields into strings by hand, the
//! approach this replaced, kept failing that kind of check one construct at
//! a time. No comrak type appears in this module's interface.
//!
//! Two things it does not do. It does not escape leading whitespace: a field
//! that opens a list item's paragraph with four spaces would be read back as
//! an indented code block inside the item, so an item's owner and its text,
//! whichever opens the paragraph, are trimmed first. The other fields cannot
//! open a block that way: the title is an ATX heading, whose leading spaces
//! CommonMark drops; a turn's paragraph opens with its `Strong` name or its
//! `(time)`; and the out-of-date note is the app's own sentence. And a bare
//! URL in any field is left alone: its text is its address, so nothing is
//! hidden or changed.
//!
//! Some punctuation joins fields, as literal text nodes of Macalla's own
//! making rather than words a person reads: `": "` after an owner, the curly
//! quotes around a quote, and the parentheses around a turn's time and an
//! item's who-said-it.

use comrak::nodes::{AstNode, ListDelimType, ListType, NodeHeading, NodeList, NodeValue};
use comrak::Arena;
use std::borrow::Cow;

/// A transcript to write as Markdown: its title, its turns, and a summary
/// when it has one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    /// The level-1 heading: a meeting's name, or a file's.
    pub title: String,
    pub turns: Vec<DocTurn>,
    pub summary: Option<DocSummary>,
    /// The summary was written before the transcript was corrected, so a
    /// quote may no longer match it. [`Words::summary_out_of_date`] is then
    /// written directly above the summary's first heading. Nothing is
    /// written for it when there is no summary to put it above.
    pub summary_out_of_date: bool,
}

/// One turn of the transcript: a paragraph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocTurn {
    /// The name to print in bold in front of the turn, or `None` for none.
    pub speaker: Option<String>,
    /// When the turn starts, written as `(m:ss)`, or `(h:mm:ss)` once there
    /// are hours.
    pub start_ms: u64,
    pub text: String,
}

/// A summary's three lists, each already flattened across whatever parts
/// the summary was made in. A list with nothing in it is left out along
/// with its heading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocSummary {
    pub key_points: Vec<DocItem>,
    pub decisions: Vec<DocItem>,
    pub action_items: Vec<DocItem>,
}

/// One item of a summary's list, written as
/// `owner: text “quote” (said)`, without the owner when it has none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocItem {
    pub text: String,
    pub owner: Option<String>,
    pub quote: String,
    /// Who said the quote and when, already worded by the app.
    pub said: String,
}

/// The words [`write`] puts in the document that are not the document's
/// own: each app fills these from its catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Words<'a> {
    pub key_points: &'a str,
    pub decisions: &'a str,
    pub action_items: &'a str,
    pub transcript: &'a str,
    /// A paragraph, written above a summary that is out of date.
    pub summary_out_of_date: &'a str,
}

/// A tight bullet list, and the same shape for one of its items: the bullet
/// character and the rest only matter on the list itself (comrak reads an
/// item's own marker from its parent list, [`NodeList::start`] aside, which
/// only ordered lists use), so one constant serves as both.
const BULLET_LIST: NodeList = NodeList {
    list_type: ListType::Bullet,
    marker_offset: 0,
    padding: 2,
    start: 1,
    delimiter: ListDelimType::Period,
    bullet_char: b'-',
    tight: true,
    is_task_list: false,
};

/// `doc` as Markdown: a level-1 heading for the title; the summary's key
/// points, decisions and action items when there is one, each a level-2
/// heading and a tight bullet list, with [`Words::summary_out_of_date`] as a
/// paragraph above the first when [`Document::summary_out_of_date`] says so;
/// then a level-2 transcript heading and one paragraph per turn.
pub fn write(doc: &Document, words: &Words<'_>) -> String {
    let arena = Arena::new();
    let root = arena.alloc(NodeValue::Document.into());

    root.append(heading_node(&arena, 1, doc.title.clone()));

    if let Some(summary) = &doc.summary {
        let lists: Vec<_> = [
            (words.key_points, &summary.key_points),
            (words.decisions, &summary.decisions),
            (words.action_items, &summary.action_items),
        ]
        .into_iter()
        .filter(|(_, items)| !items.is_empty())
        .collect();
        // A summary written before the transcript was corrected says so,
        // directly above its first heading.
        if doc.summary_out_of_date && !lists.is_empty() {
            let paragraph = arena.alloc(NodeValue::Paragraph.into());
            paragraph.append(text_node(&arena, words.summary_out_of_date.to_string()));
            root.append(paragraph);
        }
        for (heading, items) in lists {
            root.append(heading_node(&arena, 2, heading.to_string()));

            let list = arena.alloc(NodeValue::List(BULLET_LIST).into());
            for item in items {
                let list_item = arena.alloc(NodeValue::Item(BULLET_LIST).into());
                let paragraph = arena.alloc(NodeValue::Paragraph.into());
                // The owner, or the text when there is no owner, opens the
                // item's paragraph, and comrak does not escape leading
                // spaces: four of them, or a tab, would make it an indented
                // code block. Both are trimmed, since either can open it.
                if let Some(owner) = &item.owner {
                    paragraph.append(text_node(&arena, owner.trim().to_string()));
                    paragraph.append(text_node(&arena, ": "));
                }
                paragraph.append(text_node(&arena, item.text.trim().to_string()));
                paragraph.append(text_node(&arena, " \u{201c}"));
                paragraph.append(text_node(&arena, item.quote.clone()));
                paragraph.append(text_node(&arena, "\u{201d} ("));
                paragraph.append(text_node(&arena, item.said.clone()));
                paragraph.append(text_node(&arena, ")"));
                list_item.append(paragraph);
                list.append(list_item);
            }
            root.append(list);
        }
    }

    root.append(heading_node(&arena, 2, words.transcript.to_string()));
    for turn in &doc.turns {
        let paragraph = arena.alloc(NodeValue::Paragraph.into());
        match &turn.speaker {
            Some(name) => {
                let strong = arena.alloc(NodeValue::Strong.into());
                strong.append(text_node(&arena, name.clone()));
                paragraph.append(strong);
                paragraph.append(text_node(&arena, format!(" ({}) ", clock(turn.start_ms))));
            }
            None => paragraph.append(text_node(&arena, format!("({}) ", clock(turn.start_ms)))),
        }
        paragraph.append(text_node(&arena, turn.text.clone()));
        root.append(paragraph);
    }

    let mut out = String::new();
    comrak::format_commonmark(root, &comrak::Options::default(), &mut out)
        .expect("formatting to a String never fails");
    out
}

/// A `NodeValue::Text` node holding exactly `value` and nothing else. This
/// is the only way a field of the [`Document`] reaches the output: never
/// mixed into a formatted string first, so comrak's own writer, not a
/// hand-rolled rule, decides whether it needs escaping and what kind.
fn text_node<'a>(arena: &'a Arena<'a>, value: impl Into<Cow<'static, str>>) -> &'a AstNode<'a> {
    arena.alloc(NodeValue::Text(value.into()).into())
}

/// An ATX heading of `level`, holding `text` as its one child.
fn heading_node<'a>(
    arena: &'a Arena<'a>,
    level: u8,
    text: impl Into<Cow<'static, str>>,
) -> &'a AstNode<'a> {
    let heading = arena.alloc(
        NodeValue::Heading(NodeHeading {
            level,
            setext: false,
            closed: false,
        })
        .into(),
    );
    heading.append(text_node(arena, text));
    heading
}

/// A time in the transcript as minutes and seconds, with hours once there
/// are any: data, not a sentence.
fn clock(ms: u64) -> String {
    let seconds = ms / 1_000;
    let (hours, minutes, seconds) = (seconds / 3_600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// English words, as the desktop app's catalogue has them, so the
    /// goldens below read as the desktop wrote them.
    const WORDS: Words<'static> = Words {
        key_points: "Key points",
        decisions: "Decisions",
        action_items: "Action items",
        transcript: "Transcript",
        summary_out_of_date: "This summary was written before the transcript was corrected, \
            so some quotes may no longer match it. Summarise again to bring it up to date.",
    };

    fn turn(speaker: Option<&str>, start_ms: u64, text: &str) -> DocTurn {
        DocTurn {
            speaker: speaker.map(str::to_string),
            start_ms,
            text: text.into(),
        }
    }

    fn item(text: &str, quote: &str, owner: Option<&str>, said: &str) -> DocItem {
        DocItem {
            text: text.into(),
            quote: quote.into(),
            owner: owner.map(str::to_string),
            said: said.into(),
        }
    }

    /// Every hostile string, checked against `pandoc -f gfm` and
    /// `commonmark.js` while comrak was chosen.
    const HOSTILE_STRINGS: &[&str] = &[
        "=== ",
        "___",
        "---",
        "<pre>",
        "```",
        "a\n    - nested",
        "a\r# h",
        " Aoife ",
        "Aoife\\",
        "**bold**",
        "[link](x)",
        "| a | b |\n|---|---|",
        "    indented",
        "\tcode",
    ];

    /// The golden documents: each one the desktop app wrote, byte for byte,
    /// before this writer moved here, for the same meeting. `rich` has a
    /// summary over two parts (flattened), an owner with spaces around it,
    /// a text with spaces around it, an empty list, a turn named by its
    /// label, one by its speaker, one by neither, and a time past an hour.
    fn golden_documents() -> Vec<(&'static str, Document)> {
        let rich = |out_of_date: bool| Document {
            title: "Team Sync.m4a".into(),
            turns: vec![
                turn(Some("Aoife Byrne"), 0, "We ship on Friday."),
                turn(Some("Others"), 65_000, "Agreed."),
                turn(None, 3_725_000, "Late words."),
            ],
            summary: Some(DocSummary {
                key_points: vec![item(
                    "  Ship on Friday. ",
                    "We ship on Friday",
                    None,
                    "Aoife Byrne, 0:00",
                )],
                decisions: vec![item(
                    "Friday it is.",
                    "Agreed",
                    Some(" Aoife Byrne "),
                    "Others, 1:05",
                )],
                action_items: vec![item("Confirm.", "Late words", Some("Ciara"), "1:02:05")],
            }),
            summary_out_of_date: out_of_date,
        };
        let mut cases = vec![
            ("rich-current", rich(false)),
            ("rich-out-of-date", rich(true)),
            (
                "no-summary-flag-set",
                Document {
                    summary: None,
                    ..rich(true)
                },
            ),
            (
                "empty-summary-out-of-date",
                Document {
                    summary: Some(DocSummary::default()),
                    ..rich(true)
                },
            ),
            (
                "no-turns",
                Document {
                    title: "Empty".into(),
                    ..Document::default()
                },
            ),
        ];
        const HOSTILE_NAMES: [&str; 14] = [
            "hostile-0",
            "hostile-1",
            "hostile-2",
            "hostile-3",
            "hostile-4",
            "hostile-5",
            "hostile-6",
            "hostile-7",
            "hostile-8",
            "hostile-9",
            "hostile-10",
            "hostile-11",
            "hostile-12",
            "hostile-13",
        ];
        for (name, h) in HOSTILE_NAMES.into_iter().zip(HOSTILE_STRINGS) {
            cases.push((
                name,
                Document {
                    title: (*h).into(),
                    turns: vec![turn(Some(h), 0, h), turn(Some(h), 1_000, h)],
                    summary: Some(DocSummary {
                        key_points: vec![item(h, h, Some(h), h)],
                        decisions: vec![item(h, h, None, h)],
                        action_items: Vec::new(),
                    }),
                    summary_out_of_date: true,
                },
            ));
        }
        cases
    }

    /// What the desktop app wrote for each of [`golden_documents`]' meetings
    /// before the writer moved here.
    const GOLDEN: &[(&str, &str)] = &[
        ("rich-current", "# Team Sync.m4a\n\n## Key points\n\n- Ship on Friday. “We ship on Friday” (Aoife Byrne, 0:00)\n\n## Decisions\n\n- Aoife Byrne: Friday it is. “Agreed” (Others, 1:05)\n\n## Action items\n\n- Ciara: Confirm. “Late words” (1:02:05)\n\n## Transcript\n\n**Aoife Byrne** (0:00) We ship on Friday.\n\n**Others** (1:05) Agreed.\n\n(1:02:05) Late words.\n"),
        ("rich-out-of-date", "# Team Sync.m4a\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- Ship on Friday. “We ship on Friday” (Aoife Byrne, 0:00)\n\n## Decisions\n\n- Aoife Byrne: Friday it is. “Agreed” (Others, 1:05)\n\n## Action items\n\n- Ciara: Confirm. “Late words” (1:02:05)\n\n## Transcript\n\n**Aoife Byrne** (0:00) We ship on Friday.\n\n**Others** (1:05) Agreed.\n\n(1:02:05) Late words.\n"),
        ("no-summary-flag-set", "# Team Sync.m4a\n\n## Transcript\n\n**Aoife Byrne** (0:00) We ship on Friday.\n\n**Others** (1:05) Agreed.\n\n(1:02:05) Late words.\n"),
        ("empty-summary-out-of-date", "# Team Sync.m4a\n\n## Transcript\n\n**Aoife Byrne** (0:00) We ship on Friday.\n\n**Others** (1:05) Agreed.\n\n(1:02:05) Late words.\n"),
        ("no-turns", "# Empty\n\n## Transcript\n"),
        ("hostile-0", "# \\=== \n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- \\===: === “=== ” (=== )\n\n## Decisions\n\n- \\=== “=== ” (=== )\n\n## Transcript\n\n**===&#32;** (0:00) === \n\n**===&#32;** (0:01) === \n"),
        ("hostile-1", "# \\_\\_\\_\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- \\_\\_\\_: \\_\\_\\_ “\\_\\_\\_” (\\_\\_\\_)\n\n## Decisions\n\n- \\_\\_\\_ “\\_\\_\\_” (\\_\\_\\_)\n\n## Transcript\n\n**\\_\\_\\_** (0:00) \\_\\_\\_\n\n**\\_\\_\\_** (0:01) \\_\\_\\_\n"),
        ("hostile-2", "# \\---\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- \\---: --- “---” (---)\n\n## Decisions\n\n- \\--- “---” (---)\n\n## Transcript\n\n**---** (0:00) ---\n\n**---** (0:01) ---\n"),
        ("hostile-3", "# \\<pre\\>\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- \\<pre\\>: \\<pre\\> “\\<pre\\>” (\\<pre\\>)\n\n## Decisions\n\n- \\<pre\\> “\\<pre\\>” (\\<pre\\>)\n\n## Transcript\n\n**\\<pre\\>** (0:00) \\<pre\\>\n\n**\\<pre\\>** (0:01) \\<pre\\>\n"),
        ("hostile-4", "# \\`\\`\\`\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- \\`\\`\\`: \\`\\`\\` “\\`\\`\\`” (\\`\\`\\`)\n\n## Decisions\n\n- \\`\\`\\` “\\`\\`\\`” (\\`\\`\\`)\n\n## Transcript\n\n**\\`\\`\\`** (0:00) \\`\\`\\`\n\n**\\`\\`\\`** (0:01) \\`\\`\\`\n"),
        ("hostile-5", "# a&#10;    - nested\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- a&#10;    - nested: a&#10;    - nested “a&#10;    - nested” (a&#10;    - nested)\n\n## Decisions\n\n- a&#10;    - nested “a&#10;    - nested” (a&#10;    - nested)\n\n## Transcript\n\n**a&#10;    - nested** (0:00) a&#10;    - nested\n\n**a&#10;    - nested** (0:01) a&#10;    - nested\n"),
        ("hostile-6", "# a&#13;\\# h\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- a&#13;\\# h: a&#13;\\# h “a&#13;\\# h” (a&#13;\\# h)\n\n## Decisions\n\n- a&#13;\\# h “a&#13;\\# h” (a&#13;\\# h)\n\n## Transcript\n\n**a&#13;\\# h** (0:00) a&#13;\\# h\n\n**a&#13;\\# h** (0:01) a&#13;\\# h\n"),
        ("hostile-7", "#  Aoife \n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- Aoife: Aoife “ Aoife ” ( Aoife )\n\n## Decisions\n\n- Aoife “ Aoife ” ( Aoife )\n\n## Transcript\n\n**&#32;Aoife&#32;** (0:00)  Aoife \n\n**&#32;Aoife&#32;** (0:01)  Aoife \n"),
        ("hostile-8", "# Aoife\\\\\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- Aoife\\\\: Aoife\\\\ “Aoife\\\\” (Aoife\\\\)\n\n## Decisions\n\n- Aoife\\\\ “Aoife\\\\” (Aoife\\\\)\n\n## Transcript\n\n**Aoife\\\\** (0:00) Aoife\\\\\n\n**Aoife\\\\** (0:01) Aoife\\\\\n"),
        ("hostile-9", "# \\*\\*bold\\*\\*\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- \\*\\*bold\\*\\*: \\*\\*bold\\*\\* “\\*\\*bold\\*\\*” (\\*\\*bold\\*\\*)\n\n## Decisions\n\n- \\*\\*bold\\*\\* “\\*\\*bold\\*\\*” (\\*\\*bold\\*\\*)\n\n## Transcript\n\n**\\*\\*bold\\*\\*** (0:00) \\*\\*bold\\*\\*\n\n**\\*\\*bold\\*\\*** (0:01) \\*\\*bold\\*\\*\n"),
        ("hostile-10", "# \\[link\\](x)\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- \\[link\\](x): \\[link\\](x) “\\[link\\](x)” (\\[link\\](x))\n\n## Decisions\n\n- \\[link\\](x) “\\[link\\](x)” (\\[link\\](x))\n\n## Transcript\n\n**\\[link\\](x)** (0:00) \\[link\\](x)\n\n**\\[link\\](x)** (0:01) \\[link\\](x)\n"),
        ("hostile-11", "# | a | b |&#10;|---|---|\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- | a | b |&#10;|---|---|: | a | b |&#10;|---|---| “| a | b |&#10;|---|---|” (| a | b |&#10;|---|---|)\n\n## Decisions\n\n- | a | b |&#10;|---|---| “| a | b |&#10;|---|---|” (| a | b |&#10;|---|---|)\n\n## Transcript\n\n**| a | b |&#10;|---|---|** (0:00) | a | b |&#10;|---|---|\n\n**| a | b |&#10;|---|---|** (0:01) | a | b |&#10;|---|---|\n"),
        ("hostile-12", "#     indented\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- indented: indented “    indented” (    indented)\n\n## Decisions\n\n- indented “    indented” (    indented)\n\n## Transcript\n\n**&#32;   indented** (0:00)     indented\n\n**&#32;   indented** (0:01)     indented\n"),
        ("hostile-13", "# &#9;code\n\nThis summary was written before the transcript was corrected, so some quotes may no longer match it. Summarise again to bring it up to date.\n\n## Key points\n\n- code: code “&#9;code” (&#9;code)\n\n## Decisions\n\n- code “&#9;code” (&#9;code)\n\n## Transcript\n\n**&#9;code** (0:00) &#9;code\n\n**&#9;code** (0:01) &#9;code\n"),
    ];

    #[test]
    fn writes_what_the_desktop_app_wrote_byte_for_byte() {
        let documents = golden_documents();
        assert_eq!(documents.len(), GOLDEN.len());
        for ((name, doc), (golden_name, golden)) in documents.iter().zip(GOLDEN) {
            assert_eq!(name, golden_name);
            assert_eq!(write(doc, &WORDS), *golden, "{name}");
        }
    }

    /// A parsed-back node's shape, ignoring `Document` and `Text`: what a
    /// test checks a document is built from, so a hostile string that
    /// talked its way into an extra heading, list, code block, HTML block or
    /// inline, link, image or table shows up as an unexpected entry here.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Shape {
        Heading(u8),
        List,
        Item,
        Paragraph,
        Strong,
        CodeBlock,
        HtmlBlock,
        HtmlInline,
        Link,
        Image,
        Table,
        ThematicBreak,
        BlockQuote,
        Other,
    }

    fn shape_of(value: &NodeValue) -> Option<Shape> {
        match value {
            NodeValue::Document | NodeValue::Text(_) => None,
            NodeValue::Heading(h) => Some(Shape::Heading(h.level)),
            NodeValue::List(_) => Some(Shape::List),
            NodeValue::Item(_) => Some(Shape::Item),
            NodeValue::Paragraph => Some(Shape::Paragraph),
            NodeValue::Strong => Some(Shape::Strong),
            NodeValue::CodeBlock(_) => Some(Shape::CodeBlock),
            NodeValue::HtmlBlock(_) => Some(Shape::HtmlBlock),
            NodeValue::HtmlInline(_) => Some(Shape::HtmlInline),
            NodeValue::Link(_) => Some(Shape::Link),
            NodeValue::Image(_) => Some(Shape::Image),
            NodeValue::Table(_) => Some(Shape::Table),
            NodeValue::ThematicBreak => Some(Shape::ThematicBreak),
            NodeValue::BlockQuote => Some(Shape::BlockQuote),
            _ => Some(Shape::Other),
        }
    }

    /// Every node's [`Shape`] under `root`, in document order (pre-order:
    /// each node before its children).
    fn shapes<'a>(root: &'a AstNode<'a>) -> Vec<Shape> {
        root.descendants()
            .filter_map(|n| shape_of(&n.data.borrow().value))
            .collect()
    }

    fn has_shape<'a>(root: &'a AstNode<'a>, shape: &Shape) -> bool {
        shapes(root).contains(shape)
    }

    fn is_paragraph<'a>(node: &'a AstNode<'a>) -> bool {
        matches!(node.data.borrow().value, NodeValue::Paragraph)
    }

    fn is_strong<'a>(node: &'a AstNode<'a>) -> bool {
        matches!(node.data.borrow().value, NodeValue::Strong)
    }

    /// A heading's level and its text, concatenated from every `Text` node
    /// under it (ordinarily just the one).
    fn heading_text<'a>(node: &'a AstNode<'a>) -> (u8, String) {
        match &node.data.borrow().value {
            NodeValue::Heading(h) => (h.level, all_text(node).join("")),
            other => panic!("not a heading: {other:?}"),
        }
    }

    /// Every `NodeValue::Text` node's content under `node`, in document
    /// order: what a field became, wherever [`write`] put it, exactly as
    /// comrak wrote it back.
    fn all_text<'a>(node: &'a AstNode<'a>) -> Vec<String> {
        node.descendants()
            .filter_map(|n| match &n.data.borrow().value {
                NodeValue::Text(t) => Some(t.to_string()),
                _ => None,
            })
            .collect()
    }

    fn two_turns() -> Vec<DocTurn> {
        vec![
            turn(Some("Aoife Byrne"), 0, "We ship on Friday."),
            turn(None, 4_000, "Agreed."),
        ]
    }

    #[test]
    fn a_document_with_no_summary_is_a_title_a_heading_and_its_turns() {
        let doc = Document {
            title: "Team Sync.m4a".into(),
            turns: two_turns(),
            ..Document::default()
        };

        let markdown = write(&doc, &WORDS);
        let arena = Arena::new();
        let root = comrak::parse_document(&arena, &markdown, &comrak::Options::default());
        let top: Vec<_> = root.children().collect();

        assert_eq!(top.len(), 4, "{markdown}");
        assert_eq!(
            heading_text(top[0]),
            (1, "Team Sync.m4a".to_string()),
            "{markdown}"
        );
        assert_eq!(
            heading_text(top[1]),
            (2, "Transcript".to_string()),
            "{markdown}"
        );
        assert!(is_paragraph(top[2]) && is_paragraph(top[3]), "{markdown}");
        assert!(!has_shape(root, &Shape::List), "{markdown}");
    }

    /// The whole-document structure: the title is one `Heading(1)`; each
    /// non-empty summary list is a `Heading(2)` plus a tight bullet `List`
    /// with the right number of `Item`s (Decisions has none, so it and its
    /// heading are both left out); each turn is a `Paragraph` whose `Strong`
    /// holds the speaker (or has none, for a turn with none) and whose text
    /// holds the words. A summary out of date has the out-of-date words as
    /// one paragraph between the title and the summary's first heading, and
    /// one that is not has nothing there.
    #[test]
    fn builds_the_intended_node_tree() {
        for out_of_date in [false, true] {
            node_tree(out_of_date);
        }
    }

    fn node_tree(out_of_date: bool) {
        let doc = Document {
            title: "Team Sync.m4a".into(),
            turns: two_turns(),
            summary: Some(DocSummary {
                key_points: vec![item(
                    "Ship on Friday.",
                    "We ship on Friday",
                    None,
                    "Aoife Byrne, 0:00",
                )],
                decisions: Vec::new(),
                action_items: vec![item(
                    "Confirm the release.",
                    "Agreed",
                    Some("Aoife Byrne"),
                    "0:04",
                )],
            }),
            summary_out_of_date: out_of_date,
        };

        let markdown = write(&doc, &WORDS);
        let arena = Arena::new();
        let root = comrak::parse_document(&arena, &markdown, &comrak::Options::default());
        let all: Vec<_> = root.children().collect();
        assert_eq!(all.len(), if out_of_date { 9 } else { 8 }, "{markdown}");

        assert_eq!(
            heading_text(all[0]),
            (1, "Team Sync.m4a".to_string()),
            "{markdown}"
        );
        let top: Vec<_> = if out_of_date {
            assert!(is_paragraph(all[1]), "{markdown}");
            assert_eq!(
                all_text(all[1]).join(""),
                WORDS.summary_out_of_date,
                "{markdown}"
            );
            // The rest reads as it does with no note: `top[0]` stays the
            // title, so the indices below serve both cases.
            std::iter::once(all[0])
                .chain(all[2..].iter().copied())
                .collect()
        } else {
            all.clone()
        };
        assert!(
            !markdown.contains(WORDS.summary_out_of_date) || out_of_date,
            "{markdown}"
        );

        assert_eq!(
            heading_text(top[1]),
            (2, "Key points".to_string()),
            "{markdown}"
        );
        let key_points: Vec<_> = top[2].children().collect();
        assert_eq!(key_points.len(), 1, "{markdown}");

        // Decisions had no items, so Action items' heading and list follow
        // Key points' list directly.
        assert_eq!(
            heading_text(top[3]),
            (2, "Action items".to_string()),
            "{markdown}"
        );
        let action_items: Vec<_> = top[4].children().collect();
        assert_eq!(action_items.len(), 1, "{markdown}");

        assert_eq!(
            heading_text(top[5]),
            (2, "Transcript".to_string()),
            "{markdown}"
        );

        let first_turn = top[6];
        assert!(is_paragraph(first_turn), "{markdown}");
        let first_children: Vec<_> = first_turn.children().collect();
        assert!(is_strong(first_children[0]), "{markdown}");
        assert_eq!(
            all_text(first_children[0]),
            vec!["Aoife Byrne"],
            "{markdown}"
        );
        // Everything after the Strong -- the time and the words -- is one
        // run of plain inline text once parsed back, since nothing else
        // marks a boundary inside it; a substring check is the right one.
        assert!(
            all_text(first_turn)
                .iter()
                .any(|t| t.contains("We ship on Friday.")),
            "{markdown}"
        );

        let second_turn = top[7];
        assert!(is_paragraph(second_turn), "{markdown}");
        assert!(!second_turn.children().any(|n| is_strong(n)), "{markdown}");
        assert!(
            all_text(second_turn).iter().any(|t| t.contains("Agreed.")),
            "{markdown}"
        );
    }

    /// The out-of-date words go above a summary, so with no list to put
    /// them above, whether no summary or one whose lists are all empty,
    /// they are not written at all.
    #[test]
    fn the_out_of_date_words_need_a_summary_to_sit_above() {
        for summary in [None, Some(DocSummary::default())] {
            let doc = Document {
                title: "Team Sync.m4a".into(),
                turns: two_turns(),
                summary,
                summary_out_of_date: true,
            };
            let markdown = write(&doc, &WORDS);
            assert!(!markdown.contains(WORDS.summary_out_of_date), "{markdown}");
        }
    }

    /// The words come from the caller, never from this module.
    #[test]
    fn every_heading_and_the_note_are_the_callers_words() {
        let words = Words {
            key_points: "KP",
            decisions: "DE",
            action_items: "AI",
            transcript: "TR",
            summary_out_of_date: "OOD",
        };
        let doc = Document {
            title: "T".into(),
            turns: two_turns(),
            summary: Some(DocSummary {
                key_points: vec![item("a", "b", None, "c")],
                decisions: vec![item("a", "b", None, "c")],
                action_items: vec![item("a", "b", None, "c")],
            }),
            summary_out_of_date: true,
        };
        assert_eq!(
            write(&doc, &words),
            "# T\n\nOOD\n\n## KP\n\n- a \u{201c}b\u{201d} (c)\n\n## DE\n\n\
             - a \u{201c}b\u{201d} (c)\n\n## AI\n\n- a \u{201c}b\u{201d} (c)\n\n## TR\n\n\
             **Aoife Byrne** (0:00) We ship on Friday.\n\n(0:04) Agreed.\n"
        );
    }

    /// A turn's speaker is its bold name; a turn with none has no `Strong`
    /// and opens with its time.
    #[test]
    fn a_turns_speaker_is_its_bold_name_and_none_leaves_it_out() {
        let doc = Document {
            title: "T".into(),
            turns: vec![
                turn(Some("Aoife Byrne"), 0, "Hello."),
                turn(None, 1_000, "Hi."),
            ],
            ..Document::default()
        };

        let markdown = write(&doc, &WORDS);
        let arena = Arena::new();
        let root = comrak::parse_document(&arena, &markdown, &comrak::Options::default());
        let strong_texts: Vec<_> = root
            .descendants()
            .filter(|n| is_strong(n))
            .flat_map(all_text)
            .collect();
        assert_eq!(strong_texts, vec!["Aoife Byrne"], "{markdown}");
        assert!(markdown.ends_with("\n\n(0:01) Hi.\n"), "{markdown}");
    }

    /// An item's owner, trimmed, opens it with `": "`; with no owner the
    /// trimmed text opens it.
    #[test]
    fn an_items_owner_opens_it_and_no_owner_leaves_the_text_first() {
        let doc = Document {
            title: "T".into(),
            summary: Some(DocSummary {
                key_points: vec![
                    item(" Ship. ", "q", Some("  Aoife Byrne "), "0:00"),
                    item("  Ship. ", "q", None, "0:00"),
                ],
                ..DocSummary::default()
            }),
            ..Document::default()
        };
        let markdown = write(&doc, &WORDS);
        assert!(
            markdown.contains(
                "- Aoife Byrne: Ship. \u{201c}q\u{201d} (0:00)\n- Ship. \u{201c}q\u{201d} (0:00)\n"
            ),
            "{markdown}"
        );
    }

    /// Every hostile string put into every field at once -- the title, an
    /// item's text, quote and owner, the quote's who-said-it, the turn's
    /// speaker, and the turn's text -- checking the combined worst case has
    /// exactly the intended shape: no extra heading, list, code block, HTML,
    /// link, image, table or thematic break any of them talked their way
    /// into. Whether each field's own text survives is
    /// [`each_field_survives_a_hostile_string_on_its_own`]'s job: once every
    /// field holds the same string, a re-parsed document merges them into
    /// one run of plain inline text per block, so a dropped field can hide
    /// behind an identical one still there, and only isolating one field at
    /// a time can catch that.
    #[test]
    fn keeps_its_shape_with_hostile_strings_in_every_field_at_once() {
        for hostile in HOSTILE_STRINGS {
            let doc = Document {
                title: (*hostile).to_string(),
                turns: vec![turn(Some(hostile), 0, hostile)],
                summary: Some(DocSummary {
                    key_points: vec![item(hostile, hostile, Some(hostile), hostile)],
                    ..DocSummary::default()
                }),
                summary_out_of_date: false,
            };

            let markdown = write(&doc, &WORDS);
            let arena = Arena::new();
            let root = comrak::parse_document(&arena, &markdown, &comrak::Options::default());

            assert_eq!(
                shapes(root),
                vec![
                    Shape::Heading(1),
                    Shape::Heading(2),
                    Shape::List,
                    Shape::Item,
                    Shape::Paragraph,
                    Shape::Heading(2),
                    Shape::Paragraph,
                    Shape::Strong,
                ],
                "{hostile:?}: {markdown}"
            );
        }
    }

    /// Which field of a [`Document`] [`document_with_hostile_field`] puts a
    /// hostile string into, holding every other field at a fixed, distinct,
    /// benign value -- so a dropped or mis-built field's own node has
    /// nowhere to hide behind an identical sibling, unlike the combined case
    /// above.
    #[derive(Debug, Clone, Copy)]
    enum HostileField {
        Title,
        ItemText,
        /// An item's text with no owner in front, so the text itself opens
        /// the item's paragraph.
        ItemTextWithoutOwner,
        ItemQuote,
        ItemOwner,
        ItemSaid,
        TurnSpeaker,
        TurnText,
    }

    fn document_with_hostile_field(field: HostileField, hostile: &str) -> Document {
        let mut doc = Document {
            title: "Team Sync.m4a".into(),
            turns: vec![turn(Some("Aoife Byrne"), 0, "We ship on Friday.")],
            summary: Some(DocSummary {
                key_points: vec![item(
                    "Ship on Friday.",
                    "We ship on Friday",
                    Some("Aoife Byrne"),
                    "Aoife Byrne, 0:00",
                )],
                ..DocSummary::default()
            }),
            summary_out_of_date: false,
        };

        let hostile = hostile.to_string();
        let item = &mut doc.summary.as_mut().unwrap().key_points[0];
        match field {
            HostileField::Title => doc.title = hostile,
            HostileField::ItemText => item.text = hostile,
            HostileField::ItemTextWithoutOwner => {
                item.owner = None;
                item.text = hostile;
            }
            HostileField::ItemQuote => item.quote = hostile,
            HostileField::ItemOwner => item.owner = Some(hostile),
            HostileField::ItemSaid => item.said = hostile,
            HostileField::TurnSpeaker => doc.turns[0].speaker = Some(hostile),
            HostileField::TurnText => doc.turns[0].text = hostile,
        }
        doc
    }

    /// Every hostile string, in each of the eight fields on its own, with
    /// every other field held at a fixed, distinct, benign value: sensitive
    /// to a dropped field, or a field glued into a formatted string instead
    /// of its own node, for whichever field is under test, without another
    /// field's identical copy masking the loss.
    #[test]
    fn each_field_survives_a_hostile_string_on_its_own() {
        const FIELDS: [HostileField; 8] = [
            HostileField::Title,
            HostileField::ItemText,
            HostileField::ItemTextWithoutOwner,
            HostileField::ItemQuote,
            HostileField::ItemOwner,
            HostileField::ItemSaid,
            HostileField::TurnSpeaker,
            HostileField::TurnText,
        ];

        for hostile in HOSTILE_STRINGS {
            for field in FIELDS {
                let doc = document_with_hostile_field(field, hostile);
                let markdown = write(&doc, &WORDS);
                let arena = Arena::new();
                let root = comrak::parse_document(&arena, &markdown, &comrak::Options::default());

                assert!(
                    !root.descendants().any(|n| matches!(
                        shape_of(&n.data.borrow().value),
                        Some(
                            Shape::CodeBlock
                                | Shape::HtmlBlock
                                | Shape::HtmlInline
                                | Shape::Link
                                | Shape::Image
                                | Shape::Table
                                | Shape::ThematicBreak
                                | Shape::BlockQuote
                        )
                    )),
                    "{field:?} {hostile:?}: {markdown}"
                );

                // A heading's text is trimmed by CommonMark itself on
                // re-parsing (an ATX heading's leading and trailing spaces
                // are never part of its content), so the title is compared
                // trimmed; every other field is checked as a substring of
                // whichever merged inline run it landed in.
                let survived = match field {
                    HostileField::Title => {
                        heading_text(root.children().next().unwrap()).1.trim() == hostile.trim()
                    }
                    _ => all_text(root).iter().any(|t| t.contains(hostile.trim())),
                };
                assert!(
                    survived,
                    "{field:?} {hostile:?} did not survive: {markdown}"
                );
            }
        }
    }

    #[test]
    fn the_clock_adds_hours_once_there_are_any() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(65_999), "1:05");
        assert_eq!(clock(3_725_000), "1:02:05");
    }
}
