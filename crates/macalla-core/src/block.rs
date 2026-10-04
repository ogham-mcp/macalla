// SPDX-License-Identifier: MIT
//! The block model. Tier 0 produces these, Tier 1 refines them, the seam pass
//! rejoins them, and history stores them.
//!
//! The schema is versioned because history persists it: a stored dictation
//! outlives the code that wrote it.

use serde::{Deserialize, Serialize};

/// Bump when the shape of [`Block`] changes in a way old rows cannot satisfy.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Paragraph { text: String },
    ListItem { text: String, ordered: bool },
    Heading { text: String },
}

impl Block {
    pub fn text(&self) -> &str {
        match self {
            Block::Paragraph { text } | Block::ListItem { text, .. } | Block::Heading { text } => {
                text
            }
        }
    }

    pub fn text_mut(&mut self) -> &mut String {
        match self {
            Block::Paragraph { text } | Block::ListItem { text, .. } | Block::Heading { text } => {
                text
            }
        }
    }
}

/// A versioned envelope. History stores this, not a bare `Vec<Block>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub schema_version: u32,
    pub blocks: Vec<Block>,
}

impl Document {
    pub fn new(blocks: Vec<Block>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            blocks,
        }
    }

    /// Reject a document written by a future schema rather than guessing at it.
    pub fn validate(&self) -> crate::Result<()> {
        if self.schema_version > SCHEMA_VERSION {
            return Err(crate::Error::SchemaVersion {
                found: self.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Plain-text rendering. The default for terminals and editors.
    pub fn to_plain(&self) -> String {
        self.blocks
            .iter()
            .map(|b| b.text().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Markdown rendering. Ordered list numbering is positional within a run.
    pub fn to_markdown(&self) -> String {
        let mut out = Vec::new();
        let mut ordinal = 0usize;
        for block in &self.blocks {
            match block {
                Block::Paragraph { text } => {
                    ordinal = 0;
                    out.push(text.clone());
                }
                Block::Heading { text } => {
                    ordinal = 0;
                    out.push(format!("## {text}"));
                }
                Block::ListItem { text, ordered } => {
                    if *ordered {
                        ordinal += 1;
                        out.push(format!("{ordinal}. {text}"));
                    } else {
                        ordinal = 0;
                        out.push(format!("- {text}"));
                    }
                }
            }
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Document {
        Document::new(vec![
            Block::Heading {
                text: "Resource budgets".into(),
            },
            Block::ListItem {
                text: "Review the spec".into(),
                ordered: true,
            },
            Block::ListItem {
                text: "Run the benchmarks".into(),
                ordered: true,
            },
            Block::Paragraph {
                text: "That is all.".into(),
            },
        ])
    }

    #[test]
    fn markdown_numbers_ordered_items_positionally() {
        assert_eq!(
            sample().to_markdown(),
            "## Resource budgets\n1. Review the spec\n2. Run the benchmarks\nThat is all."
        );
    }

    #[test]
    fn a_paragraph_restarts_ordered_numbering() {
        let doc = Document::new(vec![
            Block::ListItem {
                text: "one".into(),
                ordered: true,
            },
            Block::Paragraph {
                text: "break".into(),
            },
            Block::ListItem {
                text: "one again".into(),
                ordered: true,
            },
        ]);
        assert_eq!(doc.to_markdown(), "1. one\nbreak\n1. one again");
    }

    #[test]
    fn plain_text_drops_all_markup() {
        assert_eq!(
            sample().to_plain(),
            "Resource budgets\nReview the spec\nRun the benchmarks\nThat is all."
        );
    }

    #[test]
    fn a_future_schema_version_is_rejected_not_guessed_at() {
        let mut doc = sample();
        doc.schema_version = SCHEMA_VERSION + 1;
        assert!(doc.validate().is_err());
        doc.schema_version = SCHEMA_VERSION;
        assert!(doc.validate().is_ok());
    }

    #[test]
    fn blocks_round_trip_through_json() {
        let doc = sample();
        let json = serde_json::to_string(&doc).expect("serialise");
        let back: Document = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(doc, back);
        assert!(json.contains("\"schema_version\":1"));
    }
}
