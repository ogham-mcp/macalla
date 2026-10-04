// SPDX-License-Identifier: MIT
//! The onboarding checklist: what still stands between the user and dictating.
//!
//! One screen, cards, live status tick, one Continue, no
//! modals, plain English -- "To hear you." not "Enables audio input capture".
//!
//! Only the MODEL and the readiness rules live here, because core cannot see
//! permissions or engines and should not. Each outer crate produces its own
//! steps -- `macalla_platform` the permission cards, `macalla_engines` the
//! engine card -- and the app concatenates them. That keeps the decision
//! ("are we ready, and what is stopping us") in one place while leaving the
//! facts with whoever owns them.
//!
//! The same list drives the tray's onboarding screen and `macalla setup`.
//! The core is callable without the hotkey or the UI: if a feature only works
//! from the tray, it is in the wrong crate.

/// Where a step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Nothing to do.
    Done,
    /// Dictation does not work until this is resolved.
    Required,
    /// Dictation works without it; something is merely unavailable.
    Optional,
}

/// One card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Stable across releases; settings and telemetry-free logs key on it.
    pub id: &'static str,
    /// The card heading. A noun, not a sentence.
    pub title: String,
    /// One plain sentence saying why it is wanted.
    pub detail: String,
    pub state: StepState,
    /// What the user should do, when there is something. `None` for a step
    /// that is Done, or one nobody can act on.
    pub action: Option<String>,
}

impl Step {
    pub fn done(id: &'static str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            detail: detail.into(),
            state: StepState::Done,
            action: None,
        }
    }

    pub fn required(
        id: &'static str,
        title: impl Into<String>,
        detail: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            detail: detail.into(),
            state: StepState::Required,
            action: Some(action.into()),
        }
    }

    pub fn optional(
        id: &'static str,
        title: impl Into<String>,
        detail: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            id,
            title: title.into(),
            detail: detail.into(),
            state: StepState::Optional,
            action: Some(action.into()),
        }
    }
}

/// The whole screen.
#[derive(Debug, Clone, Default)]
pub struct Checklist {
    steps: Vec<Step>,
}

impl Checklist {
    pub fn new(steps: Vec<Step>) -> Self {
        Self { steps }
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Whether dictation will work. Optional steps do not hold it up.
    pub fn is_ready(&self) -> bool {
        self.blocking().is_empty()
    }

    /// The steps standing in the way, in the order they were given.
    pub fn blocking(&self) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|s| s.state == StepState::Required)
            .collect()
    }

    /// The one thing to do next. A checklist that says "four things are wrong"
    /// is a wall; a checklist that says "do this" is a step.
    pub fn next_action(&self) -> Option<&Step> {
        self.blocking().into_iter().next()
    }

    /// The line under the list.
    pub fn summary(&self, shortcut: &str) -> String {
        match self.next_action() {
            None => format!("You're all set. Press {shortcut} to start."),
            // One thing. A list of four problems is a wall, and a user who
            // reads a wall closes it.
            Some(step) => match &step.action {
                Some(action) => format!("Next: {} -- {action}", step.title),
                None => format!("Next: {}", step.title),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic_done() -> Step {
        Step::done("microphone", "Microphone", "To hear you.")
    }

    fn input_needed() -> Step {
        Step::required(
            "input-monitoring",
            "Input Monitoring",
            "To notice when you hold the shortcut.",
            "System Settings > Privacy & Security > Input Monitoring",
        )
    }

    fn engine_optional() -> Step {
        Step::optional(
            "engine-assets",
            "Language",
            "Spanish is supported but not downloaded yet.",
            "Download it in Settings > Engine",
        )
    }

    #[test]
    fn everything_done_is_ready() {
        let c = Checklist::new(vec![mic_done()]);
        assert!(c.is_ready());
        assert!(c.blocking().is_empty());
        assert_eq!(c.next_action(), None);
    }

    #[test]
    fn a_required_step_blocks() {
        let c = Checklist::new(vec![mic_done(), input_needed()]);
        assert!(!c.is_ready());
        assert_eq!(c.blocking().len(), 1);
        assert_eq!(c.next_action().map(|s| s.id), Some("input-monitoring"));
    }

    #[test]
    fn an_optional_step_does_not_block() {
        // Not having Spanish downloaded is not a reason to refuse to dictate
        // in English. That card is shown only when it applies, and
        // it must never gate Continue.
        let c = Checklist::new(vec![mic_done(), engine_optional()]);
        assert!(
            c.is_ready(),
            "an undownloaded language must not block English"
        );
        assert!(c.blocking().is_empty());
    }

    #[test]
    fn the_next_action_is_the_first_one_in_the_given_order() {
        // The order is deliberate: microphone
        // first, because a user who grants nothing else can still see whether
        // the app hears them.
        let c = Checklist::new(vec![
            input_needed(),
            Step::required(
                "accessibility",
                "Accessibility",
                "To paste into other apps.",
                "System Settings > Privacy & Security > Accessibility",
            ),
        ]);
        assert_eq!(c.next_action().map(|s| s.id), Some("input-monitoring"));
        assert_eq!(c.blocking().len(), 2);
    }

    #[test]
    fn an_empty_checklist_is_ready() {
        // A platform that requires nothing is not a platform that is broken.
        assert!(Checklist::default().is_ready());
    }

    #[test]
    fn the_summary_tells_a_ready_user_the_shortcut() {
        let c = Checklist::new(vec![mic_done()]);
        let s = c.summary("Cmd+Shift+D");
        assert!(s.contains("Cmd+Shift+D"), "got: {s}");
        assert!(
            s.to_lowercase().contains("all set") || s.to_lowercase().contains("ready"),
            "got: {s}"
        );
    }

    #[test]
    fn the_summary_of_a_blocked_checklist_names_one_thing_not_four() {
        let c = Checklist::new(vec![
            input_needed(),
            Step::required(
                "accessibility",
                "Accessibility",
                "To paste.",
                "System Settings",
            ),
            Step::required(
                "microphone",
                "Microphone",
                "To hear you.",
                "System Settings",
            ),
        ]);
        let s = c.summary("Cmd+Shift+D");
        assert!(
            s.contains("Input Monitoring"),
            "should name the next step: {s}"
        );
        assert!(
            !s.contains("Accessibility"),
            "a wall of four problems is not a next step: {s}"
        );
    }

    #[test]
    fn plain_english_is_enforced_not_merely_requested() {
        // The copy rule: "To hear you." not "Enables audio input capture". A rule
        // in a document is a suggestion; a rule in a test is a rule.
        for step in [mic_done(), input_needed(), engine_optional()] {
            assert!(
                step.detail.ends_with('.'),
                "{:?} should be a sentence",
                step.detail
            );
            assert!(
                step.detail.split_whitespace().count() <= 12,
                "{:?} is too long for a card",
                step.detail
            );
            assert!(
                !step.title.ends_with('.'),
                "{:?} is a heading, not a sentence",
                step.title
            );
        }
    }
}
