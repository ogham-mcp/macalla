// SPDX-License-Identifier: MIT
//! Where the app's log goes.
//!
//! Launched from Finder an app has no terminal, and its stderr goes nowhere.
//! The first three human runs were diagnosed from crash reports because
//! there was nothing else; a log file beside the dictionary means the next
//! "it did something and then stopped" has a record. Not telemetry: it is a
//! file on this machine that nobody reads unless the person hands it over.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing_subscriber::prelude::*;

/// The file name inside the app-data directory.
pub const FILE_NAME: &str = "macalla.log";

/// Stderr always; the file too when the app-data directory can be found.
pub fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let file = log_path().and_then(|path| {
        std::fs::create_dir_all(path.parent()?).ok()?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|f| (path, f))
    });
    match file {
        Some((path, f)) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(Mutex::new(f)),
                )
                .init();
            tracing::info!(path = %path.display(), version = env!("CARGO_PKG_VERSION"), "log file");
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stderr)
                .init();
        }
    }
}

/// `~/Library/Application Support/<identifier>/macalla.log` on macOS, the
/// platform's equivalent elsewhere. Resolved without Tauri so it works
/// before the app is built.
pub(crate) fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let base = if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Application Support")
    } else {
        PathBuf::from(home).join(".local/share")
    };
    Some(base.join("dev.macalla.desktop").join(FILE_NAME))
}
