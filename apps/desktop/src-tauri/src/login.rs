// SPDX-License-Identifier: MIT
//! Opening Macalla when the person logs in. Off until they turn it on in the
//! General card; the system's own record is the only state, so there is no
//! setting of ours to drift from it.
//!
//! Through `tauri-plugin-autostart`, pinned at 2.5.1: 2.7 moves macOS from a
//! LaunchAgent to SMAppService, a behaviour change to test on its own. On macOS it writes a LaunchAgent to
//! `~/Library/LaunchAgents` that runs the executable inside the bundle; on
//! Windows it writes a value under the current user's `Run` key. Neither
//! needs a permission. The window starts hidden (`tauri.conf.json`), so a
//! login launch comes up in the tray like any other, or shows the checklist
//! if setup is not finished.
//!
//! The plugin also registers its own `enable`, `disable` and `is_enabled`
//! commands; no capability grants them, so the page reaches this only through
//! the two commands below.

use serde::Serialize;
use tauri_plugin_autostart::ManagerExt;

use crate::command_error::{CommandError, OrSay};
use crate::login_words;

/// The plugin, built for this app. On macOS the LaunchAgent's file name and
/// label are a reverse-DNS name rather than the plugin's default, the product
/// name, so `launchctl list` and `~/Library/LaunchAgents` say whose it is.
/// The bundle identifier itself is not reused, so the job's label cannot be
/// mistaken for the app's own.
/// `macos_launcher` exists only in the plugin's macOS build, so both calls
/// sit behind the same `cfg`.
pub(crate) fn plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    let builder = tauri_plugin_autostart::Builder::new();
    #[cfg(target_os = "macos")]
    let builder = builder
        .macos_launcher(tauri_plugin_autostart::MacosLauncher::LaunchAgent)
        .app_name("dev.macalla.desktop.login");
    builder.build()
}

/// What the General card draws: its words, and whether Macalla opens at login.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LaunchAtLoginView {
    pub heading: &'static str,
    pub about: &'static str,
    pub label: &'static str,
    pub on: bool,
}

/// The General card. A system that cannot say counts as off, and is logged.
#[tauri::command]
pub fn launch_at_login(app: tauri::AppHandle) -> LaunchAtLoginView {
    let on = app.autolaunch().is_enabled().unwrap_or_else(|error| {
        tracing::warn!(%error, "could not read whether Macalla opens at login");
        false
    });
    LaunchAtLoginView {
        heading: login_words::HEADING,
        about: login_words::ABOUT,
        label: login_words::OPEN_AT_LOGIN,
        on,
    }
}

/// Open Macalla when the person logs in, or stop doing so.
#[tauri::command]
pub fn launch_at_login_set(app: tauri::AppHandle, on: bool) -> Result<(), CommandError> {
    let manager = app.autolaunch();
    let changed = if on {
        manager.enable()
    } else {
        manager.disable()
    };
    changed.or_say(login_words::COULD_NOT_CHANGE)?;
    tracing::info!(on, "open at login changed");
    Ok(())
}
