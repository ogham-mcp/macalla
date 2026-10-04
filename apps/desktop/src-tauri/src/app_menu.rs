// SPDX-License-Identifier: MIT
//! The macOS menu bar, shown while a Macalla window is in front.
//!
//! Tauri's default menu, with two changes. "About Macalla" opens the app's
//! own About page, as the tray's About does, instead of the system panel,
//! which on macOS shows only a name, a version, a copyright and plain text,
//! never a link. And Help, empty by default, links to the website's guide and
//! home page through `links.rs`. Everything else is Tauri's own list
//! (`tauri::menu::Menu::default`, 2.12): Edit is what lets the text fields
//! take Cmd-C and Cmd-V.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Wry};

use crate::about_words;

const ABOUT: &str = "app-menu-about";
const HELP: &str = "app-menu-help";
const WEBSITE: &str = "app-menu-website";

pub(crate) fn build(app: &AppHandle<Wry>) -> tauri::Result<Menu<Wry>> {
    let name = app.package_info().name.clone();
    let about = MenuItem::with_id(app, ABOUT, about_words::ABOUT_MACALLA, true, None::<&str>)?;
    let help = MenuItem::with_id(app, HELP, about_words::MACALLA_HELP, true, None::<&str>)?;
    let website = MenuItem::with_id(app, WEBSITE, about_words::WEBSITE, true, None::<&str>)?;

    Menu::with_items(
        app,
        &[
            &Submenu::with_items(
                app,
                name,
                true,
                &[
                    &about,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::services(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "File",
                true,
                &[&PredefinedMenuItem::close_window(app, None)?],
            )?,
            &Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?,
            &Submenu::with_items(
                app,
                "View",
                true,
                &[&PredefinedMenuItem::fullscreen(app, None)?],
            )?,
            &Submenu::with_items(
                app,
                "Window",
                true,
                &[
                    &PredefinedMenuItem::minimize(app, None)?,
                    &PredefinedMenuItem::maximize(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::close_window(app, None)?,
                ],
            )?,
            &Submenu::with_items(app, "Help", true, &[&help, &website])?,
        ],
    )
}

/// The app-wide menu handler. The tray's menu has its own (`tray.rs`), and
/// every id here is prefixed so the two can never answer for each other.
pub(crate) fn on_menu_event(app: &AppHandle<Wry>, event: tauri::menu::MenuEvent) {
    let result = match event.id().as_ref() {
        ABOUT => {
            crate::tray::open_main_window(app, "about");
            Ok(())
        }
        HELP => crate::links::open("help"),
        WEBSITE => crate::links::open("website"),
        _ => Ok(()),
    };
    if let Err(e) = result {
        tracing::info!("menu bar: {e}");
    }
}
