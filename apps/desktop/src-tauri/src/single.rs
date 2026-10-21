// SPDX-License-Identifier: MIT
//! One copy of Macalla at a time. Two copies would each arm the shortcut, so
//! one press could record and paste twice, and both would write to History.
//!
//! A second copy starts when two launchers do not know about each other: on
//! macOS the login item (`login.rs`) runs the bundle's executable directly,
//! so when the system also reopens the apps from the last session it starts
//! another; on Windows the `Run` key and a launch from the Start menu do the
//! same. `tauri-plugin-single-instance` makes the second copy hand over to the
//! first and exit before `setup` runs.
//!
//! What the first copy does then differs by system. On macOS a launch the
//! person asked for (Finder, Dock, Spotlight) reaches the running copy through
//! the system, not through here, so a handover here is a launcher and the
//! window stays where it was: popping it open at every login would be worse
//! than the bug. On Windows a handover is usually the person starting Macalla
//! again from the Start menu, so the window opens.

use tauri::{AppHandle, Wry};

pub(crate) fn plugin() -> tauri::plugin::TauriPlugin<Wry> {
    tauri_plugin_single_instance::init(|app, _args, _cwd| handed_over(app))
}

fn handed_over(app: &AppHandle<Wry>) {
    tracing::info!("a second copy of Macalla started and exited; this one carries on");
    #[cfg(not(target_os = "macos"))]
    crate::tray::open_main_window(app, "settings");
    #[cfg(target_os = "macos")]
    let _ = app;
}
