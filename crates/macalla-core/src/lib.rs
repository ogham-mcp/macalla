// SPDX-License-Identifier: MIT
//! Core of Macalla: the pipeline, the engine and enhancer traits, and the
//! block model everything downstream is expressed in.
//!
//! This crate has no Tauri dependency and no platform dependency. It builds
//! and tests headless, driven from fixtures, with no microphone. That is a
//! hard rule, not a convenience: the CLI, the desktop app and the agent
//! interface are three triggers for the same state machine.

pub mod audio;
pub mod block;
pub mod bounded_read;
pub mod dictionary;
pub mod enhance;
pub mod error;
pub mod history;
pub mod latency;
pub mod markdown;
pub mod meeting_parts;
pub mod meeting_summary;
pub mod onboarding;
pub mod pipeline;
pub mod seam;
pub mod srt;
pub mod stt;
pub mod summary;
pub mod text_model;
pub mod tier0;
pub mod translate;
pub mod vad;
pub mod vtt;

pub use error::{Error, Result};
