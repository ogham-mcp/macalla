// SPDX-License-Identifier: MIT
//! The Tauri shell: tray, settings window, installers.
//!
//! The core is callable without the hotkey or the UI: if a feature only works from the tray, it is in the
//! wrong crate. Nothing here does dictation, engine selection or history --
//! that lives in `macalla-session` and the crates beneath it, and this crate
//! only calls it. This module wires the window and the tray; the commands are
//! in [`commands`] and the trigger in [`dictation`].

mod about_words;
#[cfg(target_os = "macos")]
mod app_menu;
mod command_error;
mod commands;
mod dictation;
mod dictionary;
mod dictionary_words;
mod enhancement_words;
mod files;
mod history;
mod links;
mod logging;
mod login;
mod login_words;
mod meeting_words;
mod microphone_words;
mod models;
#[cfg(target_os = "macos")]
mod overlay_panel;
mod paste_words;
mod say;
mod translate_words;
mod tray;
mod tray_words;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(login::plugin())
        // Read as text by `command_scopes::registered` below and kept in step
        // with `app_commands.rs`: leave it a plain comma-separated list of
        // paths, with no attributes or comments inside.
        .invoke_handler(tauri::generate_handler![
            commands::transcription_choose,
            commands::setup_steps,
            commands::shortcut,
            commands::input_device,
            commands::microphones,
            commands::microphone_choose,
            commands::about,
            commands::licenses,
            commands::dictionary_info,
            commands::dictionary_terms,
            commands::dictionary_add,
            commands::dictionary_update,
            commands::dictionary_remove,
            commands::dictionary_import,
            commands::dictionary_export,
            commands::dictionary_words,
            commands::reveal_path,
            commands::history_list,
            commands::history_copy,
            commands::history_delete,
            commands::history_send_again,
            commands::history_retention,
            commands::history_set_retention,
            commands::open_settings_pane,
            commands::request_permission,
            commands::finish_setup,
            commands::provider_keys,
            commands::provider_key_save,
            commands::provider_key_remove,
            commands::models_overview,
            commands::cleanup_choose,
            commands::server_models,
            commands::openrouter_models,
            commands::pool_models,
            models::models_list,
            models::model_download,
            models::model_cancel,
            models::model_remove,
            models::models_dir,
            files::meeting_choose_file,
            files::meeting_open_file,
            files::meeting_summarise,
            files::meeting_cancel,
            files::meeting_current,
            files::meeting_running_summary,
            files::meeting_clear,
            files::meeting_destinations,
            files::meeting_copy,
            files::meeting_save,
            files::meeting_edit_turn,
            files::meeting_correction_offer,
            files::meeting_live_start,
            files::meeting_live_stop,
            files::meeting_running_live,
            files::meeting_live_words,
            files::meeting_batch_start,
            files::meeting_batch,
            files::meeting_batch_clear,
            files::translation::meeting_translation_options,
            files::translation::meeting_translation_check,
            files::translation::meeting_translate,
            files::translation::meeting_translation,
            files::translation::meeting_running_translation,
            files::translation::meeting_save_translation,
            files::translation::meeting_copy_translation,
            commands::dictation_cancel,
            commands::dictation_finish,
            login::launch_at_login,
            login::launch_at_login_set,
            links::open_link
        ])
        .setup(|app| {
            // History first, because the dictation thread records into it;
            // then the session and the shortcut, on their own threads. The
            // tray and the commands reach both through managed state.
            let history = history::HistoryHandle::open(app.handle());
            app.manage(history.clone());
            app.manage(models::Downloads::default());
            let meetings = files::Meetings::default();
            // A live meeting the last quit held is offered back on the
            // Files page, until the person copies, saves or clears it.
            if let Ok(dir) = app.path().app_data_dir() {
                meetings.offer_held(&dir);
            }
            app.manage(meetings);
            app.manage(files::ChosenFiles::default());
            // The translator for the Files page. Apple's is on-device: Apple
            // documents that every TranslationSession translation runs on
            // the device, and a whole meeting translated the same with the
            // network unplugged, so it needs no "leaves this Mac" label
            // (nothing leaves the machine unless turned on). A backend that sends text anywhere would.
            // Without the `translate-apple` feature, off macOS, or on an
            // older SDK, it reports itself unavailable and the page says so
            // (rule 4: the app is complete without it).
            app.manage(files::translation::Translation(std::sync::Arc::new(
                macalla_translators::apple::AppleTranslation::new(),
            )));
            // The overlay's web view moves into a non-activating panel on
            // macOS before the dictation thread exists, so no dictation can
            // show Tauri's window first (`overlay_panel`).
            #[cfg(target_os = "macos")]
            if let Some(overlay) = app.get_webview_window("overlay") {
                overlay_panel::install(&overlay);
            }
            app.manage(dictation::start(app.handle().clone(), history));
            tray::build(app.handle())?;
            // The menu bar while a window is in front: Tauri's default with
            // About and Help changed (`app_menu`).
            #[cfg(target_os = "macos")]
            {
                app.set_menu(app_menu::build(app.handle())?)?;
                app.on_menu_event(app_menu::on_menu_event);
            }

            // First launch, or a rebuild that lost its grants: the checklist
            // comes to the front until everything on it is done. A tray app
            // with nothing to say stays hidden.
            if commands::setup_is_complete() {
                tracing::info!("setup complete; staying in the tray");
            } else if let Some(window) = app.get_webview_window("main") {
                tracing::info!("setup incomplete; showing the checklist");
                let _ = window.show();
                let _ = window.set_focus();
            }

            // A tray app: the window starts hidden (tauri.conf.json) and
            // closing it hides it rather than quitting, so "Open Macalla…"
            // and the tray icon stay meaningful.
            // Recorded here, from the window's own native drag-drop event,
            // rather than trusted from the page: only the main window's own
            // drops are recorded, never the overlay's, so `meeting_open_file`
            // and `meeting_batch_start` can trust a path that came from a
            // drop. The page then hears `files-dropped` with exactly the
            // paths recorded, and opens those: never a folder, or a path
            // that was not recorded, and only once they are recorded.
            if let Some(window) = app.get_webview_window("main") {
                let hide_on_close = window.clone();
                let drops_to = app.handle().clone();
                let chosen = app.state::<files::ChosenFiles>().inner().clone();
                window.on_window_event(move |event| match event {
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        if let Err(e) = hide_on_close.hide() {
                            tracing::info!("could not hide the main window: {e}");
                        }
                    }
                    tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) => {
                        let recorded = chosen.record_dropped(paths);
                        if !recorded.is_empty() {
                            use tauri::Emitter;
                            let _ = drops_to.emit_to("main", "files-dropped", recorded);
                        }
                    }
                    _ => {}
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // A borrowed model, or the dictation thread's own, still open at
            // exit finds ggml's own teardown mid-free and aborts on the
            // Metal assert every model on this Mac shares one device's
            // buffers under (`crates/macalla-engines/src/transcribe.rs`'s
            // `Worker` doc; `Dictator::shut_down`). Files' stop and the
            // warm-up gate's quitting flag are set here, before the shared
            // deadline below is even computed; `Dictator::shut_down` sets
            // its own quitting flag and sends `Command::Shutdown` as the
            // first thing it does, before its own wait -- so the stop or
            // flag each wait below depends on is always set before that
            // wait can block. The dictation thread rereads its flag at
            // several points before it would start a fresh warm-up, borrow
            // or dictation (`idle_step`, and again right before the overlay
            // in `run`), which closes the window down to a handful of
            // instructions rather than closing it outright -- there is no
            // single point that serialises the dictation thread against
            // this one.
            // Also forget any path the panel or a drop gave that was never
            // opened, though the process exiting would free it regardless.
            if let tauri::RunEvent::Exit = event {
                app.state::<files::ChosenFiles>().clear();

                let files = app.state::<files::Meetings>();
                let downloads = app.state::<models::Downloads>();
                let dictator = app.state::<dictation::Dictator>();
                files.stop_now();
                downloads.begin_quitting();

                // One deadline shared by every wait below: a model still
                // compiling its first Metal shaders (about ten seconds on a
                // fresh install) can still outlast it -- known, not fixed
                // here.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let total_started = std::time::Instant::now();

                // If a dictation was still in progress, `shut_down` waits out
                // more than just the thread joining: a local one delivers
                // what is ready, without waiting on Enhancement, and pastes
                // inside this same wait (`dictation.rs`'s `give_up_on`), so
                // this is also that dictation's paste deadline. A cloud
                // request is given up rather than waited for;
                // the on-device engine finishing still counts in either case.
                // A miss below means the paste, if any, did not make it, the
                // same as any other quit deadline miss.
                let dictation_started = std::time::Instant::now();
                let dictation_finished = dictator.shut_down(deadline);
                let dictation_ms = dictation_started.elapsed().as_millis() as u64;
                if !dictation_finished {
                    tracing::warn!("the dictation thread did not end before the quit deadline");
                }

                let warm_up_started = std::time::Instant::now();
                let warm_up_finished = downloads.wait_until(deadline);
                let warm_up_ms = warm_up_started.elapsed().as_millis() as u64;
                if !warm_up_finished {
                    tracing::warn!("a model warm-up did not finish before the quit deadline");
                }

                // A live meeting that was recording is stopped, not
                // discarded (`stop_now`): held for the next launch, whole if
                // it finishes inside this wait, or as far as it had
                // committed if it does not. Only text, and a few
                // milliseconds' write after the wait.
                let files_started = std::time::Instant::now();
                let files_finished =
                    files.finish_for_quit(deadline, app.path().app_data_dir().ok().as_deref());
                let files_ms = files_started.elapsed().as_millis() as u64;
                if !files_finished {
                    tracing::warn!("a Files job did not finish before the quit deadline");
                }

                // How long quit spent on each part,
                // at `info` rather than only the `warn` lines above, which
                // fire only on a miss. `dictation_ms` covers a local
                // dictation's finish-and-paste as well as the thread joining;
                // the dictation thread's own log says whether it pasted, and
                // whether the app was closing when it did.
                tracing::info!(
                    dictation_ms,
                    dictation_finished,
                    warm_up_ms,
                    warm_up_finished,
                    files_ms,
                    files_finished,
                    total_ms = total_started.elapsed().as_millis() as u64,
                    "quit: released the speech engine and stopped background work"
                );
            }
        });
}

/// Which window may call which app command. `build.rs` hands the list in
/// `app_commands.rs` to tauri-build, which then allows a command only where a
/// capability grants its `allow-` permission; these check that list against
/// `generate_handler!` above and against every capability file.
#[cfg(test)]
mod command_scopes {
    include!("app_commands.rs");

    /// The command names `generate_handler!` in this file registers.
    fn registered() -> Vec<String> {
        let source = include_str!("lib.rs");
        let open = "generate_handler![";
        let start = source.find(open).expect("a handler list") + open.len();
        let end = start + source[start..].find(']').expect("the list's end");
        source[start..end]
            .split(',')
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(|path| path.rsplit("::").next().unwrap_or(path).to_string())
            .collect()
    }

    /// Each capability in `capabilities/`: its file name, the windows and
    /// webviews it names (Tauri grants a capability to either), and the
    /// identifiers of the permissions it grants.
    fn capabilities() -> Vec<(String, Vec<String>, Vec<String>)> {
        // A capability written inline in tauri.conf.json would not be read
        // here, and listing any there would also switch off every file in
        // `capabilities/` it leaves out, so none may be listed.
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("the Tauri config");
        assert!(
            config["app"]["security"]["capabilities"].is_null(),
            "tauri.conf.json lists capabilities, which these checks do not read"
        );
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("the capabilities directory") {
            let path = entry.expect("a directory entry").path();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // A TOML capability would not be checked here, so none may exist.
            assert!(
                name.ends_with(".json"),
                "{name}: only JSON capabilities are checked"
            );
            let text = std::fs::read_to_string(&path).expect("a readable capability");
            let json: serde_json::Value = serde_json::from_str(&text).expect("a JSON capability");
            let strings = |key: &str| -> Vec<String> {
                json[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str().or_else(|| value["identifier"].as_str()))
                    .map(str::to_string)
                    .collect()
            };
            let mut targets = strings("windows");
            targets.extend(strings("webviews"));
            found.push((name, targets, strings("permissions")));
        }
        found
    }

    /// The permission tauri-build generates to allow `command`.
    fn allow(command: &str) -> String {
        format!("allow-{}", command.replace('_', "-"))
    }

    /// Whether a capability's window or webview pattern takes in `label`.
    /// Tauri matches full globs; this reads a plain label or one ending in
    /// `*`, and fails on any other pattern rather than matching it wrongly.
    fn names_window(pattern: &str, label: &str) -> bool {
        assert!(
            !pattern.contains(['?', '[']) && !pattern.trim_end_matches('*').contains('*'),
            "{pattern}: a pattern these checks cannot read"
        );
        match pattern.strip_suffix('*') {
            Some(prefix) => label.starts_with(prefix),
            None => pattern == label,
        }
    }

    #[test]
    fn every_registered_command_is_in_the_app_manifest() {
        let listed: Vec<String> = APP_COMMANDS.iter().map(|c| c.to_string()).collect();
        assert_eq!(registered(), listed);
    }

    /// The app commands the overlay may call, and the only ones: its cancel
    /// and finish buttons'. No other window may call them.
    const OVERLAY_COMMANDS: &[&str] = &["dictation_cancel", "dictation_finish"];

    #[test]
    fn the_main_window_may_call_every_app_command_but_the_overlays() {
        let granted: Vec<String> = capabilities()
            .into_iter()
            .filter(|(_, windows, _)| windows.iter().any(|w| names_window(w, "main")))
            .flat_map(|(_, _, permissions)| permissions)
            .collect();
        for command in APP_COMMANDS {
            if OVERLAY_COMMANDS.contains(command) {
                assert!(
                    !granted.contains(&allow(command)),
                    "the main window can call the overlay's {command}"
                );
            } else {
                assert!(
                    granted.contains(&allow(command)),
                    "the main window cannot call {command}"
                );
            }
        }
    }

    #[test]
    fn every_window_in_the_config_is_named_by_a_capability() {
        // A window no capability names gets no permission at all, not even
        // the core defaults, and fails only when its page calls something.
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("the Tauri config");
        let labels: Vec<&str> = config["app"]["windows"]
            .as_array()
            .expect("the config's windows")
            .iter()
            .filter_map(|window| window["label"].as_str())
            .collect();
        assert!(!labels.is_empty(), "the config names no windows");
        let capabilities = capabilities();
        for label in labels {
            assert!(
                capabilities
                    .iter()
                    .any(|(_, windows, _)| windows.iter().any(|w| names_window(w, label))),
                "no capability names the {label} window"
            );
        }
    }

    #[test]
    fn the_overlay_may_call_only_its_own_commands_so_no_meeting_or_history_command() {
        let allowed: Vec<String> = OVERLAY_COMMANDS.iter().map(|c| allow(c)).collect();
        for command in OVERLAY_COMMANDS {
            assert!(
                APP_COMMANDS.contains(command),
                "{command} is not a registered app command"
            );
        }
        let mut granted = Vec::new();
        for (file, windows, permissions) in capabilities() {
            if !windows.iter().any(|w| names_window(w, "overlay")) {
                continue;
            }
            // Core and plugin permissions carry a prefix ("core:default");
            // an app command's permission does not.
            let app: Vec<&String> = permissions
                .iter()
                .filter(|p| !p.contains(':') && !allowed.contains(p))
                .collect();
            assert!(app.is_empty(), "{file} lets the overlay call {app:?}");
            granted.extend(permissions.iter().filter(|p| allowed.contains(p)).cloned());
            for command in APP_COMMANDS
                .iter()
                .filter(|c| c.starts_with("meeting_") || c.starts_with("history_"))
            {
                assert!(!permissions.contains(&allow(command)), "{file}: {command}");
            }
        }
        for permission in &allowed {
            assert!(
                granted.contains(permission),
                "the overlay cannot call {permission}"
            );
        }
    }

    #[test]
    fn only_the_overlay_may_call_the_overlays_commands() {
        for (file, windows, permissions) in capabilities() {
            if windows.iter().any(|w| names_window(w, "overlay")) {
                continue;
            }
            for command in OVERLAY_COMMANDS {
                assert!(
                    !permissions.contains(&allow(command)),
                    "{file} lets another window call {command}"
                );
            }
        }
    }
}

/// Pins the overlay window's focus settings in `tauri.conf.json`, which has
/// no comment syntax. `focus: false` only says the window is not focused when
/// it is created (tauri-utils' `WindowConfig::focus`); `focusable: false` is
/// what makes tao's window answer no to `canBecomeKeyWindow`, so a click on
/// the overlay's cancel button never makes it the key window. Without
/// `acceptFirstMouse`, a click while another app is in front only activates
/// Macalla, and the button never sees it. Neither setting stops macOS
/// activating the app on that click: tao makes a plain `NSWindow`. On macOS
/// the web view moves into a non-activating panel at setup (`overlay_panel`);
/// `acceptFirstMouse` belongs to the web view and goes with it, and
/// `focusable` still keeps Tauri's hidden window from becoming key, and
/// matters again if the panel could not be set up. Check a change here by
/// clicking the button in a built app while typing in another one.
#[cfg(test)]
mod overlay_window {
    #[test]
    fn the_overlay_never_becomes_key_and_takes_the_first_click() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("the Tauri config");
        let overlay = config["app"]["windows"]
            .as_array()
            .expect("the config's windows")
            .iter()
            .find(|window| window["label"] == "overlay")
            .expect("the overlay window");
        assert_eq!(overlay["focus"], false);
        assert_eq!(overlay["focusable"], false);
        assert_eq!(overlay["acceptFirstMouse"], true);
    }
}

/// Pins the CSP in `tauri.conf.json`, so a change to it is a deliberate edit
/// here, not silent. Checked only against the config's own JSON: `tauri dev`
/// serves pages straight from Vite with no CSP applied at all
/// (Tauri's documentation on CSP), so a change to this value needs an
/// on-screen pass of a *built* app -- the settings window, the overlay, and
/// the Files page's Choose a file button and file drop -- not just `tauri
/// dev`.
#[cfg(test)]
mod csp {
    #[test]
    fn the_content_security_policy_is_the_one_the_inventory_allows() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("the Tauri config");
        assert_eq!(
            config["app"]["security"]["csp"],
            serde_json::json!({
                "default-src": "'self'",
                "script-src": "'self'",
                "style-src": "'self'",
                "connect-src": "ipc: http://ipc.localhost",
                "object-src": "'none'",
                "base-uri": "'none'",
                "frame-ancestors": "'none'",
                "form-action": "'none'"
            })
        );
    }
}
