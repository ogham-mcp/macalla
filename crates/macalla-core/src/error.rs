// SPDX-License-Identifier: MIT
//! Library errors. `thiserror` here, `anyhow` only at the app boundary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("audio: {0}")]
    Audio(String),

    #[error("stt engine {engine}: {message}")]
    Stt {
        engine: &'static str,
        message: String,
    },

    #[error("enhancer {backend}: {message}")]
    Enhance {
        backend: &'static str,
        message: String,
    },

    #[error("engine {engine} does not support {what}")]
    Unsupported {
        engine: &'static str,
        what: &'static str,
    },

    #[error("block schema version {found} is not supported (expected {expected})")]
    SchemaVersion { found: u32, expected: u32 },

    #[error("history: {0}")]
    History(String),

    /// A model file: missing, damaged, or a download that failed or was
    /// cancelled. The message is fit to show a person.
    #[error("model: {0}")]
    Model(String),
}

pub type Result<T> = std::result::Result<T, Error>;
