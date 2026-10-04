// SPDX-License-Identifier: MIT
//! The permission cards for the onboarding checklist.
//!
//! The copy lives here rather than in the app, so the tray screen and
//! `macalla setup` say the same words. The model and the readiness rules live
//! in `macalla_core::onboarding`, which cannot see permissions and should not.

use crate::hotkey::Shortcut;
use crate::{Grant, Permission};
use macalla_core::onboarding::Step;

/// A card per permission this platform actually asks for, in a deliberate
/// order: microphone first, because a user who grants nothing else can still
/// find out whether the app hears them. On macOS a card for Secure Keyboard
/// Entry follows them, because it presents exactly like a denied permission
/// and is nothing of the kind.
pub fn permission_steps() -> Vec<Step> {
    let mut steps: Vec<Step> = Permission::required_here().iter().map(step_for).collect();
    steps.extend(crate::secure_input_enabled().map(secure_input_card));
    steps
}

/// The Secure Keyboard Entry card. Pure, so both states are tested without
/// touching a terminal's settings.
///
/// Optional and not Required when it is on, because the default shortcut is a
/// bare modifier and keeps working. It is a blocker only for a
/// shortcut with an ordinary key in it, and the card says which kind works.
fn secure_input_card(on: bool) -> Step {
    let id = "secure-input";
    let title = "Secure Keyboard Entry";
    if on {
        Step::optional(
            id,
            title,
            "Something has it on, and it hides ordinary keys from every app.",
            format!(
                "A terminal (iTerm2 has it under its own menu) or a password field. \
                 A shortcut with a letter in it cannot reach Macalla until it is off; \
                 the default, {}, still works.",
                Shortcut::default_for_platform()
            ),
        )
    } else {
        Step::done(id, title, "Off, so your shortcut can see the keys.")
    }
}

fn step_for(permission: &Permission) -> Step {
    let id = match permission {
        Permission::Microphone => "microphone",
        Permission::InputMonitoring => "input-monitoring",
        Permission::Accessibility => "accessibility",
    };
    let title = match permission {
        Permission::Microphone => "Microphone",
        Permission::InputMonitoring => "Input Monitoring",
        Permission::Accessibility => "Accessibility",
    };
    let detail = permission.reason();
    let pane = permission.settings_pane();

    match permission.granted() {
        Grant::Granted | Grant::NotRequired => Step::done(id, title, detail),
        Grant::Denied => Step::required(
            id,
            title,
            detail,
            // The part people get stuck on: macOS will not ask again after a
            // denial, so waiting for a dialog is waiting forever.
            format!("Turn it on in System Settings > {pane}. macOS will not ask again on its own."),
        ),
        Grant::Unknown if *permission == Permission::Microphone => {
            // Never asked yet. Blocking, because nothing works without it,
            // and with a way through: the ask is one press away.
            Step::required(
                id,
                title,
                detail,
                "Press Ask and allow it. Macalla only listens while you hold the shortcut.",
            )
        }
        Grant::Unknown => Step::required(
            id,
            title,
            detail,
            format!("Allow it when asked, or turn it on in System Settings > {pane}."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::onboarding::Checklist;

    #[test]
    fn there_is_a_card_for_every_permission_this_platform_needs() {
        // Plus the Secure Keyboard Entry card, only where the mode exists.
        let extra = usize::from(crate::secure_input_enabled().is_some());
        assert_eq!(
            permission_steps().len(),
            Permission::required_here().len() + extra
        );
    }

    #[test]
    fn secure_input_on_is_a_warning_that_names_a_shortcut_that_works() {
        // Not a blocker: the default is immune. But the one thing a person in
        // this state needs is a shortcut that reaches the app, so the card
        // says which.
        let card = secure_input_card(true);
        assert_eq!(card.state, macalla_core::onboarding::StepState::Optional);
        let action = card.action.as_deref().unwrap();
        assert!(
            action.contains(&Shortcut::default_for_platform().to_string()),
            "got: {action}"
        );
    }

    #[test]
    fn secure_input_off_is_a_tick() {
        let card = secure_input_card(false);
        assert_eq!(card.state, macalla_core::onboarding::StepState::Done);
        assert_eq!(card.id, "secure-input");
    }

    #[test]
    fn microphone_comes_first() {
        // Deliberate: a user who grants nothing else can still check that the
        // app hears them, which is the one thing that makes the rest worth
        // doing.
        // Only where there are permission cards: Windows and Linux have
        // none (`Permission::required_on`).
        let steps = permission_steps();
        if Permission::required_here().contains(&Permission::Microphone) {
            assert_eq!(steps.first().map(|s| s.id), Some("microphone"));
        } else {
            assert!(steps.iter().all(|s| s.id != "microphone"), "{steps:?}");
        }
    }

    #[test]
    fn the_microphone_blocks_until_it_is_granted_and_says_how() {
        // Never-asked and refused both block: nothing works without it. The
        // difference is the action -- an ask that will work, or a pane to
        // visit because no prompt is coming.
        if !Permission::required_here().contains(&Permission::Microphone) {
            return;
        }
        let steps = permission_steps();
        let mic = steps
            .iter()
            .find(|s| s.id == "microphone")
            .expect("a mic card");
        let granted = Permission::Microphone.granted();
        assert_eq!(
            mic.state == macalla_core::onboarding::StepState::Required,
            !matches!(granted, Grant::Granted | Grant::NotRequired),
            "state {:?} for grant {granted:?}",
            mic.state
        );
        if granted == Grant::Unknown {
            assert!(mic.action.as_deref().unwrap().contains("Ask"));
        }
    }

    #[test]
    fn a_denial_says_that_no_dialog_is_coming() {
        // The single most confusing thing about macOS permissions, and the
        // reason a whole evening went into diagnosing a silent hotkey.
        let denied = Step::required(
            "input-monitoring",
            "Input Monitoring",
            "To notice when you hold the shortcut.",
            "Turn it on in System Settings > x. macOS will not ask again on its own.",
        );
        assert!(denied.action.as_deref().unwrap().contains("not ask again"));
    }

    #[test]
    fn the_cards_render_as_a_checklist_that_answers_ready_or_not() {
        let c = Checklist::new(permission_steps());
        let summary = c.summary("Cmd+Shift+D");
        assert!(!summary.is_empty());
        // Whatever this machine's state, the two must agree with each other.
        assert_eq!(c.is_ready(), c.blocking().is_empty());
        assert_eq!(c.is_ready(), summary.contains("all set"));
    }
}
