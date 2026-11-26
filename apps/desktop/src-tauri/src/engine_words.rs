// SPDX-License-Identifier: MIT
//! Wording for a dictation with no speech model to run on (the app's wording
//! rule: one whole sentence per entry, a placeholder named in braces and
//! filled in here; a model's name, its size and a key's name are data). Each
//! entry says where it is shown.

/// The setup checklist's engine step (`commands.rs`'s `engine_step`), when
/// the chosen engine needs a download. `{model}` is the model's name and
/// `{size}` its size in whole megabytes.
const DOWNLOAD_TO_START: &str = "Download {model} ({size} MB) to start dictating.";

/// The overlay (`dictation.rs`'s `refuse_dictation`), for a few seconds after
/// a dictation was asked for with no engine ready. One short line, because
/// the overlay cuts off long text.
pub(crate) const NO_MODEL_OVERLAY: &str = "No speech model yet. Open Macalla to get one.";

/// The main window's notice bar, after the same press, where the platform has
/// a global shortcut. `{shortcut}` is the key as its cap reads.
const NO_MODEL_NOTICE: &str = "No speech model is downloaded yet. Download one on the Models \
     page, then hold {shortcut} and speak.";

/// The same notice where the platform has no global shortcut (Linux today),
/// so the tray is the way in.
const NO_MODEL_NOTICE_TRAY: &str = "No speech model is downloaded yet. Download one on the Models \
     page, then choose Start Dictation from the tray.";

pub(crate) fn download_to_start(model: &str, megabytes: u64) -> String {
    DOWNLOAD_TO_START
        .replace("{model}", model)
        .replace("{size}", &megabytes.to_string())
}

/// The main window's notice: `shortcut` is the key as its cap reads, or
/// `None` where nothing listens for one.
pub(crate) fn no_model_notice(shortcut: Option<&str>) -> String {
    match shortcut {
        Some(name) => NO_MODEL_NOTICE.replace("{shortcut}", name),
        None => NO_MODEL_NOTICE_TRAY.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_download_line_fills_the_model_and_the_size() {
        let said = download_to_start("Parakeet", 640);
        assert_eq!(said, "Download Parakeet (640 MB) to start dictating.");
    }

    #[test]
    fn the_notice_names_the_shortcut_where_there_is_one() {
        let said = no_model_notice(Some("Right Ctrl"));
        assert!(said.ends_with("then hold Right Ctrl and speak."), "{said}");
        assert!(!said.contains('{'), "a placeholder was left in: {said}");
    }

    #[test]
    fn the_notice_sends_to_the_tray_where_there_is_no_shortcut() {
        let said = no_model_notice(None);
        assert!(
            said.ends_with("choose Start Dictation from the tray."),
            "{said}"
        );
        assert!(!said.contains("hold"), "{said}");
    }
}
