// SPDX-License-Identifier: MIT
//! A port for a free-text model call, for callers that want a reply and not
//! refined blocks.
//!
//! [`crate::enhance::Enhancer`] does not fit a summary: its contract is
//! "Refines Tier 0 blocks; never invents content", enforced by the guard in
//! [`crate::enhance::rewrite_each`]. A summary is asked to invent -- it
//! condenses -- so it needs its own port. [`TextModel`] is deliberately the
//! smallest thing that calls a model and gets text back: no blocks, no
//! prompt, no guard. The prompt a caller sends and any check it runs on the
//! reply (a summary's quote check, say) are the caller's, not this port's --
//! keep it this small and do not add cleanup options here.
//!
//! A provider only translates a call into its wire format and back; it does
//! not know why it was called.
//!
//! [`ShapedModel`] is the second port here, for a backend that can constrain
//! its reply to a [`Shape`] the caller describes, as Apple Intelligence's
//! guided generation does. It is a port of its own rather than a method on
//! [`TextModel`], so that no provider carries a capability it does not have and
//! [`TextModel`] stays as small as it is. The caller owns the shape and what its
//! fields mean; a provider only translates the shape into its own schema form.
//!
//! **Every implementation asks for the least reasoning it can, always** --
//! independent of any Enhancer prompt the same provider value carries.
//! The design runs a hybrid reasoning model with thinking disabled, and
//! some servers otherwise put the answer on a reasoning channel and return
//! empty content. Unlike [`crate::enhance::Enhancer::enhance`], there is no
//! prompt here to pick the setting from -- `complete`'s `instructions` may
//! ask for anything, including a summary that benefits from reasoning -- so
//! this is a blunt default, not a considered one. Revisit it once summaries
//! have been measured against both settings.

use crate::Result;

/// One free-text model call: send instructions and text, get the model's
/// reply back as-is.
///
/// The reply is exactly the text the model returned -- no guard, no
/// trimming beyond what the provider's own transport already does. A caller
/// that needs the reply checked (for a summary, that its claims trace back
/// to the transcript) does that itself.
pub trait TextModel: Send + Sync {
    /// Whether the backend can serve a request right now, checked the same
    /// way the matching [`crate::enhance::Enhancer`] backend checks it (Apple
    /// Intelligence is a setting the user can turn off at any moment, so it
    /// is asked per call; an endpoint's own reachability is only found by the
    /// request itself).
    fn ready(&self) -> bool;

    /// Send `instructions` as the system role and `text` as the user's, and
    /// return the model's reply. Always asks for the least reasoning the
    /// backend supports -- see the module doc.
    fn complete(&self, instructions: &str, text: &str) -> Result<String>;
}

/// The shape a structured reply must take, described by the caller that owns
/// its meaning, in terms any backend that constrains its output can follow.
/// Every field is required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// Free text.
    Text,
    /// Exactly one of `choices`. `name` names the choice in the backend's
    /// schema.
    OneOf { name: String, choices: Vec<String> },
    /// Any number of items of one shape.
    List(Box<Shape>),
    /// Named fields, in the order the model should write them. Records with the
    /// same `name` in one shape must be the same record, so a backend may build
    /// it once and refer to it by name.
    Record { name: String, fields: Vec<Field> },
}

/// One field of a [`Shape::Record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    /// What the field holds, for the model. It is sent with the request, so it
    /// is written for the model and costs its words.
    pub description: String,
    pub shape: Shape,
}

/// A model call whose reply is constrained to a [`Shape`]: send instructions,
/// text and the shape, and get the reply back as JSON text, as the model wrote
/// it.
///
/// No provider options: the shape says what the reply looks like, not how the
/// backend is driven. The reply is not checked here; a caller parses it and
/// applies its own rules, as a summary checks every quote.
pub trait ShapedModel: Send + Sync {
    /// Whether the backend can serve a request right now, as
    /// [`TextModel::ready`].
    fn ready(&self) -> bool;

    /// Send `instructions` and `text` with `shape`, and return the reply as
    /// JSON text matching it.
    fn complete_shaped(&self, instructions: &str, text: &str, shape: &Shape) -> Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    /// A fake backend, so a caller of [`TextModel`] can be exercised without
    /// a real provider: it echoes what it was sent, so a test can assert on
    /// the exact instructions and text a caller built.
    struct Fake {
        available: bool,
        reply: Result<String>,
    }

    impl TextModel for Fake {
        fn ready(&self) -> bool {
            self.available
        }

        fn complete(&self, instructions: &str, text: &str) -> Result<String> {
            self.reply
                .as_ref()
                .map(|reply| format!("{reply} (asked: {instructions} | {text})"))
                .map_err(|e| Error::Enhance {
                    backend: "fake",
                    message: e.to_string(),
                })
        }
    }

    #[test]
    fn a_fake_backend_reports_availability_and_echoes_the_call() {
        let model = Fake {
            available: true,
            reply: Ok("done".into()),
        };
        assert!(model.ready());
        assert_eq!(
            model
                .complete("Summarise it.", "the meeting ran long")
                .unwrap(),
            "done (asked: Summarise it. | the meeting ran long)"
        );
    }

    #[test]
    fn an_unavailable_fake_backend_still_reports_its_error_from_complete() {
        let model = Fake {
            available: false,
            reply: Err(Error::Enhance {
                backend: "fake",
                message: "not configured".into(),
            }),
        };
        assert!(!model.ready());
        let error = model.complete("Summarise it.", "text").unwrap_err();
        assert!(error.to_string().contains("not configured"), "{error}");
    }
}
