// SPDX-License-Identifier: MIT
//! One error type for every Tauri command's `Result::Err`, so a command
//! function never turns its own error into text a second time.
//!
//! [`CommandError`] holds only the sentence a page is meant to show. It
//! serializes with a hand-written `serialize_str`, not `#[derive(Serialize)]`,
//! because Tauri's own conversion from a command's `Err` to the rejection the
//! page's `invoke()` throws (`impl<T: Serialize> From<T> for InvokeError`,
//! `ipc/mod.rs`) calls `serde_json::to_value(value)` -- a derived `Serialize`
//! on a newtype struct would produce `{"CommandError":"..."}`, and the page's
//! `.catch((e: unknown) => set…Error(String(e)))` wants a bare string. See
//! Tauri's documentation on calling Rust from the frontend.
//!
//! `From<macalla_session::Error>`, `From<macalla_core::Error>` and
//! `From<macalla_platform::Error>` call [`crate::say`]'s `report`,
//! `report_core` and `report_platform`, so converting one of these with `?`
//! is the one place the raw error gets logged -- a call site that already
//! logs it, or that builds the sentence itself, must not also log it here.
//! [`OrSay`] does the equivalent for an error `say` has no sentence for at
//! all (`std::io::Error`, `tauri::Error`, `serde_json::Error`): it logs the
//! raw error at `warn` with the given sentence, then returns that sentence
//! and only that sentence -- never the raw error's own text, which can carry
//! a path or OS wording not fit for a page (see `say.rs`'s module doc).

use std::fmt;

/// A sentence already fit to show on a page. See the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError(String);

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for CommandError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl From<&'static str> for CommandError {
    fn from(sentence: &'static str) -> Self {
        Self(sentence.to_string())
    }
}

impl From<String> for CommandError {
    fn from(sentence: String) -> Self {
        Self(sentence)
    }
}

impl From<macalla_session::Error> for CommandError {
    fn from(error: macalla_session::Error) -> Self {
        Self(crate::say::report(&error))
    }
}

impl From<macalla_core::Error> for CommandError {
    fn from(error: macalla_core::Error) -> Self {
        Self(crate::say::report_core(&error))
    }
}

impl From<macalla_platform::Error> for CommandError {
    fn from(error: macalla_platform::Error) -> Self {
        Self(crate::say::report_platform(&error))
    }
}

/// Turn a `Result` whose error type is not one [`crate::say`] has a sentence
/// for (`std::io::Error`, `tauri::Error`, `serde_json::Error`, a poisoned
/// lock) into a [`CommandError`] carrying `sentence`, logging the raw error
/// at `warn` first so the detail is not lost, only kept off the page.
pub trait OrSay<T> {
    fn or_say(self, sentence: &'static str) -> Result<T, CommandError>;
}

impl<T, E: fmt::Display> OrSay<T> for Result<T, E> {
    fn or_say(self, sentence: &'static str) -> Result<T, CommandError> {
        self.map_err(|error| {
            tracing::warn!(%error, sentence, "command failed");
            CommandError(sentence.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_error_serializes_to_a_bare_json_string() {
        let error = CommandError::from("It broke.");
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            serde_json::Value::String("It broke.".to_string())
        );
    }

    #[test]
    fn a_session_error_gives_the_same_sentence_say_would() {
        let expected = crate::say::say(&macalla_session::Error::Stopped);
        let error: CommandError = macalla_session::Error::Stopped.into();
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn a_core_error_gives_the_same_sentence_say_would() {
        let make = || macalla_core::Error::History("the file is locked".to_string());
        let expected = crate::say::core(&make());
        let error: CommandError = make().into();
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn a_platform_error_gives_the_same_sentence_say_would() {
        let make = || macalla_platform::Error::Clipboard("busy".to_string());
        let expected = crate::say::platform(&make());
        let error: CommandError = make().into();
        assert_eq!(error.to_string(), expected);
    }

    #[test]
    fn or_say_keeps_the_given_sentence_and_drops_the_raw_text() {
        let result: Result<(), std::io::Error> = Err(std::io::Error::other("os error 13"));
        let error = result.or_say("Could not do that.").unwrap_err();
        assert_eq!(error.to_string(), "Could not do that.");
        assert!(!error.to_string().contains("os error"));
    }
}
