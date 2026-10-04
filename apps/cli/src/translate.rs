// SPDX-License-Identifier: MIT
//! `macalla translate`: a transcript file translated into another language,
//! turn by turn, on this machine, and printed in one of the four formats.
//!
//! The use case is `macalla_session::translate`; this only reads the file,
//! builds the translator, and says what happened in `words`' sentences. The
//! translator is Apple's on-device one, built here at this binary's
//! composition root: nothing is sent anywhere, so nothing needs saying about
//! where the text goes. Without the `translate-apple` feature, or off macOS,
//! it says translating isn't available.
//!
//! The Translation framework answers only while the main thread serves its
//! run loop, so the translation runs on a thread of its own and the main
//! thread serves the run loop until it ends
//! (`macalla_translators::apple::serve_main_thread_until`).

use crate::{words, WatchFormat};
use anyhow::Result;
use macalla_core::translate::TranslateError;
use macalla_session::meeting::{self, MeetingFile};
use macalla_session::translate::translate_meeting;
use macalla_translators::apple::{serve_main_thread_until, AppleTranslation};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) fn translate(
    path: &Path,
    source: &str,
    target: &str,
    format: WatchFormat,
) -> Result<()> {
    let meeting = match meeting::open_file(path)? {
        MeetingFile::Transcript(meeting) => meeting,
        MeetingFile::Recording => anyhow::bail!(words::TRANSLATE_NEEDS_TRANSCRIPT),
    };
    let translator = AppleTranslation::new();
    let done = AtomicBool::new(false);
    let outcome = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                translate_meeting(
                    &translator,
                    &meeting,
                    source,
                    target,
                    &mut |p| {
                        if p.done < p.of {
                            eprintln!("{}", words::translating(p.done + 1, p.of));
                        }
                    },
                    &AtomicBool::new(false),
                )
            }));
            done.store(true, Ordering::Release);
            outcome
        });
        serve_main_thread_until(&done);
        worker.join()
    });
    let translated = match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(panic)) | Err(panic) => std::panic::resume_unwind(panic),
    };
    let translated = translated.map_err(|error| {
        if matches!(
            error,
            TranslateError::Failed { .. } | TranslateError::Mismatch { .. }
        ) {
            tracing::debug!(%error, "the translation failed");
        }
        anyhow::anyhow!(words::translate_failed(&error))
    })?;
    print!("{}", format.render(path, &translated.meeting)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::translate::Unavailable;

    #[test]
    fn a_recording_is_refused_before_anything_is_translated() {
        let dir =
            std::env::temp_dir().join(format!("macalla-cli-translate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("call.wav");
        std::fs::write(&path, b"RIFF").unwrap();
        let error = translate(&path, "en-US", "de", WatchFormat::Txt).unwrap_err();
        assert_eq!(error.to_string(), words::TRANSLATE_NEEDS_TRANSCRIPT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_translator_is_worded_for_the_platform() {
        assert!(words::TRANSLATE_UNAVAILABLE_MACOS.contains("macOS 26"));
        assert!(!words::TRANSLATE_UNAVAILABLE_ELSEWHERE.contains("macOS"));
        assert_ne!(
            words::TRANSLATE_UNAVAILABLE_MACOS,
            words::TRANSLATE_UNAVAILABLE_ELSEWHERE
        );
        let said = words::translate_failed(&TranslateError::Unavailable(
            Unavailable::BackendUnavailable,
        ));
        if cfg!(target_os = "macos") {
            assert_eq!(said, words::TRANSLATE_UNAVAILABLE_MACOS);
        } else {
            assert_eq!(said, words::TRANSLATE_UNAVAILABLE_ELSEWHERE);
        }
    }

    #[test]
    fn each_reason_has_its_own_sentence_naming_the_languages() {
        let not_installed =
            words::translate_failed(&TranslateError::Unavailable(Unavailable::NotInstalled {
                source: "en-US".into(),
                target: "de".into(),
            }));
        assert!(not_installed.starts_with("de isn't installed for translating from en-US"));
        assert!(not_installed.contains("Translation Languages"));
        let unsupported =
            words::translate_failed(&TranslateError::Unavailable(Unavailable::UnsupportedPair {
                source: "en-US".into(),
                target: "ga".into(),
            }));
        assert_eq!(unsupported, "this Mac can't translate from en-US into ga");
        let sentences = [
            words::translate_failed(&TranslateError::Unavailable(
                Unavailable::BackendUnavailable,
            )),
            words::translate_failed(&TranslateError::Stopped),
            words::translate_failed(&TranslateError::Failed {
                backend: "apple-translation",
                message: "internalError".into(),
            }),
        ];
        for sentence in &sentences {
            assert!(!sentence.contains("internalError"), "{sentence}");
        }
        assert_ne!(sentences[0], sentences[1]);
    }
}
