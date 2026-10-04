// SPDX-License-Identifier: MIT
//! The tray icon and its menu.
//!
//! The UI rules fix the idle menu at six items, in this order:
//! Start Dictation, Microphone (a submenu), Open Macalla…, History…, About,
//! Quit. While a dictation runs, Start Dictation reads Stop Dictation and a
//! seventh item, Cancel Dictation, sits directly below it
//! ([`cancel_change`]): a way to throw the dictation away that needs no key,
//! for when another app's secure keyboard entry hides Esc from Macalla. The
//! three that open something all open the one main window, on the view
//! named; the window is a card list with History and About as views of it,
//! not extra windows to find.
//!
//! The icon is the echo mark: a source dot with three fading repeats. Idle
//! shows the whole trail at rest; while a dictation runs the repeats cycle
//! outward, which is the only thing on screen when the overlay is hidden and
//! the engine has not produced a word yet.

use crate::commands::{self, MicrophoneView};
use crate::dictation::{Command, Dictator};
use crate::history::HistoryHandle;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, Submenu};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Emitter, Listener, Manager, Wry};

const START_DICTATION: &str = "start-dictation";
const CANCEL_DICTATION: &str = "cancel-dictation";
/// Where Cancel Dictation goes: directly below Start/Stop Dictation, which is
/// the menu's first item. Inserted at this index every time rather than
/// appended, so any number of start/cancel cycles leave the order alone.
const CANCEL_POSITION: usize = 1;
/// The Microphone submenu's own "System default" row. A device row's id is
/// this prefix followed by the device's name verbatim (`MIC_PREFIX`), so
/// `on_menu_event` tells the two apart with [`str::strip_prefix`] rather than
/// parsing -- a name is never split or re-escaped, whatever characters the
/// OS gave it, including a literal `:`.
const MIC_DEFAULT: &str = "mic-default";
const MIC_PREFIX: &str = "mic:";
const OPEN_MACALLA: &str = "open-macalla";
const HISTORY: &str = "history";
const ABOUT: &str = "about";
const QUIT: &str = "quit";

/// How long each listening frame is held. The six frames are one outward
/// sweep of the echo, so the whole cycle is a little under a second — slow
/// enough to read as breathing rather than as a spinner saying "busy".
const FRAME: Duration = Duration::from_millis(120);

// Frame 6 is byte-identical to frame 1: the generator's table gives both the
// same all-dim opacities, so the sweep is five distinct images and a return
// to rest. Looping all six therefore holds the rest frame for 240 ms once per
// cycle, which is the beat between sweeps and is deliberate. Do not "fix" it
// by dropping the sixth frame or by regenerating the set; that removes the
// pause and turns breathing back into a spinner.

// The menu-bar artwork.
//
// `include_image!` decodes the PNG during compilation and stores raw pixels,
// so the binary carries no PNG decoder and nothing is parsed at startup. The
// cost is 36x36x4 bytes a frame, about 36 KB for all seven, which is why the
// macro's warning about bloat does not apply at this size. Its paths resolve
// from the crate manifest, not from this file. They are statics rather than
// consts because the animator passes them by reference, and a const is
// materialised as a fresh temporary at each use site.
//
// These are the 36 px assets, not the 18 px ones. tray-icon scales whatever
// it is handed to 18 pt of menu-bar height, so passing the @2x frame is what
// makes it sharp on a Retina display rather than merely correct.
//
// They are template images: solid black with an alpha channel, tinted by
// macOS to match the menu bar and inverted when the bar is dark or the item
// is selected. That is why there is no light and dark pair here.
static IDLE: Image<'static> = tauri::include_image!("./icons/MenuBar/StatusIdle@2x.png");

static LISTENING: [Image<'static>; 6] = [
    tauri::include_image!("./icons/MenuBar/StatusListening-1@2x.png"),
    tauri::include_image!("./icons/MenuBar/StatusListening-2@2x.png"),
    tauri::include_image!("./icons/MenuBar/StatusListening-3@2x.png"),
    tauri::include_image!("./icons/MenuBar/StatusListening-4@2x.png"),
    tauri::include_image!("./icons/MenuBar/StatusListening-5@2x.png"),
    tauri::include_image!("./icons/MenuBar/StatusListening-6@2x.png"),
];

/// The channel that tells the Microphone submenu's refresher thread
/// ([`spawn_microphone_menu_refresher`]) to rebuild it, managed as app state
/// (reached only through [`ping_microphone_menu`], including from outside
/// this module -- `on_menu_event` is a plain function, not a closure, since
/// tauri calls it by pointer, and `commands::microphones` is in another
/// module entirely). The `Sender` itself is wrapped in a `Mutex` only so
/// this type is `Sync` without depending on whether `mpsc::Sender` is;
/// sending is still the cheap, non-blocking push the listeners in `build`
/// need (see its doc comment on `app.listen` for why that matters on the
/// dictation thread).
struct MicrophoneMenuRefresh(Mutex<Sender<()>>);

impl MicrophoneMenuRefresh {
    fn ping(&self) {
        if let Ok(tx) = self.0.lock() {
            let _ = tx.send(());
        }
    }
}

/// Tell the Microphone submenu to rebuild -- the one entry point every
/// trigger uses, in this module and out of it (`commands::microphones`
/// calls this too, after listing devices for the settings page). `try_state`
/// rather than `state`, which panics if `T` is not managed: nothing calls
/// this before [`build`] has run in the shipped app, but a test harness that
/// exercises a command without building the tray first must not crash on a
/// call that, for it, is rightly a no-op.
pub(crate) fn ping_microphone_menu(app: &AppHandle<Wry>) {
    if let Some(refresh) = app.try_state::<MicrophoneMenuRefresh>() {
        refresh.ping();
    }
}

pub fn build(app: &AppHandle<Wry>) -> tauri::Result<()> {
    let start_dictation =
        MenuItem::with_id(app, START_DICTATION, "Start Dictation", true, None::<&str>)?;
    let microphone = Submenu::new(app, crate::microphone_words::MICROPHONE, true)?;
    let open_macalla = MenuItem::with_id(app, OPEN_MACALLA, "Open Macalla…", true, None::<&str>)?;
    let history = MenuItem::with_id(app, HISTORY, "History…", true, None::<&str>)?;
    let about = MenuItem::with_id(app, ABOUT, "About", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT, "Quit", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &start_dictation,
            &microphone,
            &open_macalla,
            &history,
            &about,
            &quit,
        ],
    )?;

    let tray = TrayIconBuilder::new()
        .icon(IDLE.clone())
        // No-op away from macOS. Elsewhere the frames render as the flat
        // black glyph they are, which is right on a light taskbar and wrong
        // on a dark one; Windows and Linux packaging picks that up.
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(on_menu_event)
        .build(app)?;

    // Built once and moved in and out of the menu, never rebuilt: the same
    // item each time, so removing it always finds it.
    let cancel_dictation = MenuItem::with_id(
        app,
        CANCEL_DICTATION,
        crate::tray_words::CANCEL_DICTATION,
        true,
        None::<&str>,
    )?;
    let animator = animate(tray, start_dictation, menu, cancel_dictation);

    let refresh = spawn_microphone_menu_refresher(app.clone(), microphone);
    app.manage(MicrophoneMenuRefresh(Mutex::new(refresh)));
    // The first population: nothing has emitted `dictation-device` or
    // `dictation-devices` yet.
    ping_microphone_menu(app);

    // A Rust `listen` callback runs on whichever thread calls `emit`
    // (tauri's `Listeners::emit_filter`, event/listener.rs) -- here, the
    // dictation thread, including its last "dictation-state" while the app
    // is quitting. `MenuItem::set_text` blocks on a reply from the main
    // thread (`run_item_main_thread!`, menu/mod.rs), and during
    // `RunEvent::Exit` the main thread is itself waiting on the dictation
    // thread to end (`Dictator::shut_down`). Calling `set_text` here would
    // deadlock both: this closure only ever sends a `bool` down the
    // channel, cheap and non-blocking, and the animator thread -- which
    // nothing at quit waits on -- does the relabelling.
    app.listen("dictation-state", move |event| {
        let listening = event.payload().contains("listening");
        // A closed channel means the animator thread is gone, which happens
        // only on the way out. The icon and the label simply stop moving.
        let _ = animator.send(listening);
    });

    // The same deadlock risk applies to both listeners below: rebuilding the
    // submenu calls the same main-thread-blocking menu APIs `set_text` does,
    // so each -- the dictation thread calls both, including at quit -- only
    // pings the refresher thread ([`ping_microphone_menu`]), never rebuilds
    // directly. Between them these are the tray's whole non-polling signal
    // for the device list: `dictation-device` fires whenever the device
    // actually in use changes (an explicit switch, or `follow_device`'s
    // catching a moved default); `dictation-devices` fires from that same
    // idle tick when the connected list or the default itself moves, even
    // before anything re-arms on it (`dictation.rs`'s `device_snapshot_changed`).
    // Both already exist for other reasons -- the settings card, and
    // `follow_device`'s own re-arming -- so the tray adds no polling of its
    // own by riding on them.
    let device_app = app.clone();
    app.listen("dictation-device", move |_event| {
        ping_microphone_menu(&device_app);
    });
    let devices_app = app.clone();
    app.listen("dictation-devices", move |_event| {
        ping_microphone_menu(&devices_app);
    });

    Ok(())
}

/// Run the icon on its own thread and hand back the switch. Also relabels
/// `start_dictation` -- "Stop Dictation" reads correctly while one is
/// running, since a click that started a dictation is the only way to end
/// it when the shortcut cannot see the keys -- and shows or hides
/// `cancel_dictation` in `menu` ([`cancel_change`]), next to `set_icon`, so
/// all three follow the same message and none runs on the thread that
/// emitted it (see `build`'s comment on `app.listen`).
///
/// One thread for the life of the app rather than one per dictation. It
/// blocks outright while idle, so an idle Macalla wakes up for nothing, and
/// only falls into the frame timer once a dictation is actually running.
fn animate(
    tray: TrayIcon<Wry>,
    start_dictation: MenuItem<Wry>,
    menu: Menu<Wry>,
    cancel_dictation: MenuItem<Wry>,
) -> Sender<bool> {
    let (tx, rx) = mpsc::channel::<bool>();

    std::thread::spawn(move || {
        // Cloning an Image built by include_image! copies a borrow of static
        // pixel data, not the pixels, so this is free.
        let show = |image: &Image<'static>| {
            // set_icon followed by set_icon_as_template renders twice and
            // flickers; this sets both at once. Away from macOS it falls
            // back to a plain set_icon.
            if let Err(e) = tray.set_icon_with_as_template(Some(image.clone()), true) {
                tracing::info!("tray: could not set the icon: {e}");
            }
        };
        let relabel = |now: bool| {
            let text = if now {
                "Stop Dictation"
            } else {
                "Start Dictation"
            };
            if let Err(e) = start_dictation.set_text(text) {
                tracing::info!("tray: could not relabel the dictation item: {e}");
            }
        };

        // Whether Cancel Dictation is in the menu now: tracked here, the
        // one thread that adds or removes it, rather than asked of the menu.
        let mut cancel_shown = false;
        let mut show_cancel = |now: bool| {
            let result = match cancel_change(cancel_shown, now) {
                Some(CancelChange::Insert(position)) => menu.insert(&cancel_dictation, position),
                Some(CancelChange::Remove) => menu.remove(&cancel_dictation),
                None => return,
            };
            match result {
                Ok(()) => cancel_shown = now,
                Err(e) => tracing::info!("tray: could not show or hide Cancel Dictation: {e}"),
            }
        };

        let mut listening = false;
        let mut frame = 0usize;

        loop {
            let next = if listening {
                rx.recv_timeout(FRAME)
            } else {
                // Idle: no timer, no wake-ups. Park until something happens.
                rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            };

            match next {
                Ok(now) if now == listening => {}
                Ok(now) => {
                    listening = now;
                    frame = 0;
                    show(if now { &LISTENING[0] } else { &IDLE });
                    relabel(now);
                    show_cancel(now);
                }
                Err(RecvTimeoutError::Timeout) => {
                    frame = (frame + 1) % LISTENING.len();
                    show(&LISTENING[frame]);
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });

    tx
}

/// What to do to the menu's Cancel Dictation item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelChange {
    /// Put it in at this index.
    Insert(usize),
    Remove,
}

/// Whether Cancel Dictation should be in the menu, given whether it is now
/// (`shown`) and whether a dictation is running (`listening`): present only
/// while one runs, so the idle menu is the six fixed items. `None`
/// when the menu is already right, so a repeated message neither inserts a
/// second copy nor removes what is not there. Pure, so a test drives it
/// without a running app.
fn cancel_change(shown: bool, listening: bool) -> Option<CancelChange> {
    match (shown, listening) {
        (false, true) => Some(CancelChange::Insert(CANCEL_POSITION)),
        (true, false) => Some(CancelChange::Remove),
        _ => None,
    }
}

/// One row of the Microphone submenu before it becomes a live
/// `CheckMenuItem`: a plain value, so [`microphone_menu_items`] is pure and
/// testable without a running Tauri app.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MicMenuRow {
    id: String,
    label: String,
    checked: bool,
    enabled: bool,
}

/// Mirrors the Microphone settings card's `<select>` (`MicrophoneCard` in
/// `apps/desktop/src/App.tsx`) by mapping over the very rows
/// `commands::microphone_rows` built for it (`view.rows`, on the
/// [`MicrophoneView`] `commands::microphone_view` returns): same entries,
/// same order, same labels -- the app's wording rule, one source
/// (`crate::microphone_words`) for both surfaces. This adds only what a menu
/// needs and a `<select>` does not: an id to route a click back
/// (`on_menu_event`), whether the row is checked, and whether it is
/// enabled.
///
/// The system default row's id is [`MIC_DEFAULT`]; every other row's is
/// [`MIC_PREFIX`] followed by the row's `value` (its device name) verbatim.
/// The saved-but-disconnected device's row -- `commands::microphone_rows`
/// adds it only when `chosen` names no connected device -- is mirrored but
/// disabled rather than clickable, since the tray has nowhere to show why
/// choosing it again would fail the way the card's own error line does.
///
/// The checked row is the one dictation would actually use, not merely
/// `chosen`: the saved choice while it names a connected device, the default
/// otherwise. That is the same fallback `macalla-session`'s `Session` already
/// applies when it arms a microphone (`wanted_device`, `follow_device`), not
/// a new one invented here -- with nothing armed yet, "would actually use"
/// and "will fall back to" are the same thing.
fn microphone_menu_items(view: &MicrophoneView) -> Vec<MicMenuRow> {
    let chosen = view.chosen.as_deref();
    let chosen_connected = chosen.is_some_and(|name| view.names.iter().any(|n| n == name));

    view.rows
        .iter()
        .map(|row| {
            let is_default = row.value.is_empty();
            // The only row whose `value` can equal `chosen` while it is not
            // connected is the gone row `microphone_rows` adds for exactly
            // that case: no device row exists for a disconnected name.
            let is_chosen_row = !is_default && Some(row.value.as_str()) == chosen;
            let is_gone_row = is_chosen_row && !chosen_connected;
            MicMenuRow {
                id: if is_default {
                    MIC_DEFAULT.to_string()
                } else {
                    format!("{MIC_PREFIX}{}", row.value)
                },
                label: escape_menu_text(&row.label),
                checked: if is_default {
                    !chosen_connected
                } else {
                    is_chosen_row && chosen_connected
                },
                enabled: !is_gone_row,
            }
        })
        .collect()
}

/// muda reads an unescaped `&` in menu text as a mnemonic marker and drops
/// it (`&&` shows one literal `&`, muda's own escape). A device name is data
/// the OS handed over, not copy written with that convention in mind, so
/// every `&` in it is escaped before the name reaches a menu item.
fn escape_menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

/// Rebuild the Microphone submenu's rows off the thread that asked for it:
/// the same deadlock this avoids is `set_text`'s (see `build`'s doc comment
/// on `app.listen`), since every menu-editing call here blocks on a reply
/// from the main thread. Each ping re-reads the microphones and the choice
/// fresh, so pings that stack up while a rebuild is already running just
/// repeat cheap, idempotent work rather than needing to be coalesced.
fn spawn_microphone_menu_refresher(app: AppHandle<Wry>, submenu: Submenu<Wry>) -> Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();

    std::thread::spawn(move || {
        for () in rx {
            refresh_microphone_menu(&app, &submenu);
        }
    });

    tx
}

/// The read-only counterpart of [`commands::microphones`], the Tauri
/// command: that command pings this submenu after it lists devices for the
/// settings page (so opening Settings freshens the tray too), and calling it
/// again here -- instead of `commands::microphone_view`, its ping-free
/// twin -- would have the tray's own refresh trigger another ping, and that
/// ping trigger another refresh, forever.
fn refresh_microphone_menu(app: &AppHandle<Wry>, submenu: &Submenu<Wry>) {
    let history = app.state::<HistoryHandle>();
    let view = match commands::microphone_view(&history) {
        Ok(view) => view,
        Err(e) => {
            tracing::info!("tray: could not list microphones: {e}");
            return;
        }
    };

    if let Ok(current) = submenu.items() {
        for item in current {
            let _ = submenu.remove(&item);
        }
    }
    for row in microphone_menu_items(&view) {
        match CheckMenuItem::with_id(
            app,
            row.id.as_str(),
            row.label.as_str(),
            row.enabled,
            row.checked,
            None::<&str>,
        ) {
            Ok(item) => {
                if let Err(e) = submenu.append(&item) {
                    tracing::info!("tray: could not add a microphone menu item: {e}");
                }
            }
            Err(e) => tracing::info!("tray: could not build a microphone menu item: {e}"),
        }
    }
}

fn on_menu_event(app: &AppHandle<Wry>, event: tauri::menu::MenuEvent) {
    let id = event.id().as_ref();
    if id == MIC_DEFAULT {
        choose_microphone(app, None);
        return;
    }
    if let Some(name) = id.strip_prefix(MIC_PREFIX) {
        choose_microphone(app, Some(name.to_string()));
        return;
    }
    match id {
        OPEN_MACALLA => open_main_window(app, "settings"),
        HISTORY => open_main_window(app, "history"),
        ABOUT => open_main_window(app, "about"),
        QUIT => {
            // "Until Macalla quits" is kept here. A crash skips it, and the
            // launch purge covers that.
            app.state::<crate::history::HistoryHandle>()
                .purge(macalla_core::history::Moment::Quit);
            app.exit(0)
        }
        // The same trigger as the shortcut, as a click: starts if idle, stops
        // if recording. The overlay shows which.
        START_DICTATION => app.state::<Dictator>().send(Command::Toggle),
        // Esc twice as a click: throws the running dictation away, and does
        // nothing if it has already ended (`Dictator::cancel`).
        CANCEL_DICTATION => app.state::<Dictator>().cancel(),
        other => tracing::info!("tray: unknown menu item {other:?}"),
    }
}

/// Choose a microphone from the tray: the very command the settings card's
/// dropdown calls (`commands::microphone_choose`), so both write the same
/// setting and neither can disagree with the other about what "chosen" is.
/// Refreshes the submenu afterwards either way -- on success, so the tick
/// catches up without waiting for the next `dictation-device` event; on
/// failure (the device a stale row named went away between the click and
/// this call), so the row stops offering it.
fn choose_microphone(app: &AppHandle<Wry>, name: Option<String>) {
    let history = app.state::<HistoryHandle>();
    let dictator = app.state::<Dictator>();
    if let Err(e) = commands::microphone_choose(history, dictator, name) {
        tracing::info!("tray: could not switch microphone: {e}");
    }
    ping_microphone_menu(app);
}

/// Show the main window on the named view. The view is an event the page
/// listens for, so the window need not be rebuilt to change what it shows.
pub(crate) fn open_main_window(app: &AppHandle<Wry>, view: &str) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::info!("tray: no main window to open");
        return;
    };
    let _ = app.emit_to("main", "navigate", view);
    if let Err(e) = window.show() {
        tracing::info!("tray: could not show the main window: {e}");
        return;
    }
    if let Err(e) = window.set_focus() {
        tracing::info!("tray: could not focus the main window: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the same [`MicrophoneView`] `commands::microphone_view` would,
    /// reusing its real row builder (`commands::microphone_rows`) rather
    /// than a second, hand-rolled one here, so these tests exercise the
    /// rows the card and the tray actually both receive.
    fn view(names: &[&str], chosen: Option<&str>, system_default: Option<&str>) -> MicrophoneView {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let rows = commands::microphone_rows(&names, chosen, system_default);
        MicrophoneView {
            chosen: chosen.map(str::to_string),
            names,
            rows,
        }
    }

    /// The menu's item ids in order, with `change` applied the way
    /// `animate` applies it to the live menu.
    fn apply(menu: &mut Vec<&'static str>, change: Option<CancelChange>) {
        match change {
            Some(CancelChange::Insert(position)) => menu.insert(position, CANCEL_DICTATION),
            Some(CancelChange::Remove) => menu.retain(|id| *id != CANCEL_DICTATION),
            None => {}
        }
    }

    #[test]
    fn cancel_dictation_is_shown_only_while_a_dictation_runs() {
        assert_eq!(
            cancel_change(false, true),
            Some(CancelChange::Insert(CANCEL_POSITION))
        );
        assert_eq!(cancel_change(true, false), Some(CancelChange::Remove));
        assert_eq!(
            cancel_change(false, false),
            None,
            "the idle menu is unchanged"
        );
        assert_eq!(cancel_change(true, true), None, "never a second copy");
    }

    #[test]
    fn cancel_dictation_stays_directly_below_start_dictation_across_cycles() {
        let idle = vec![
            START_DICTATION,
            "microphone",
            OPEN_MACALLA,
            HISTORY,
            ABOUT,
            QUIT,
        ];
        let mut menu = idle.clone();
        let mut shown = false;
        // Start, a repeated "listening", cancel, a repeated "idle", three times.
        for listening in [true, true, false, false].repeat(3) {
            apply(&mut menu, cancel_change(shown, listening));
            shown = listening;
            if listening {
                assert_eq!(menu[0], START_DICTATION);
                assert_eq!(menu[1], CANCEL_DICTATION);
                assert_eq!(&menu[2..], &idle[1..]);
            } else {
                assert_eq!(menu, idle);
            }
        }
    }

    #[test]
    fn no_devices_still_offers_the_system_default_row() {
        let rows = microphone_menu_items(&view(&[], None, None));
        assert_eq!(
            rows,
            vec![MicMenuRow {
                id: MIC_DEFAULT.to_string(),
                label: "System default".to_string(),
                checked: true,
                enabled: true,
            }]
        );
    }

    #[test]
    fn one_device_on_the_default_is_shown_twice_and_the_default_row_is_ticked() {
        let rows = microphone_menu_items(&view(&["USB Mic"], None, Some("USB Mic")));
        assert_eq!(
            rows,
            vec![
                MicMenuRow {
                    id: MIC_DEFAULT.to_string(),
                    label: "System default (USB Mic)".to_string(),
                    checked: true,
                    enabled: true,
                },
                MicMenuRow {
                    id: format!("{MIC_PREFIX}USB Mic"),
                    label: "USB Mic".to_string(),
                    checked: false,
                    enabled: true,
                },
            ]
        );
    }

    #[test]
    fn two_devices_ticks_only_the_chosen_one() {
        let rows = microphone_menu_items(&view(
            &["Built-in", "USB Mic"],
            Some("USB Mic"),
            Some("Built-in"),
        ));
        assert_eq!(
            rows,
            vec![
                MicMenuRow {
                    id: MIC_DEFAULT.to_string(),
                    label: "System default (Built-in)".to_string(),
                    checked: false,
                    enabled: true,
                },
                MicMenuRow {
                    id: format!("{MIC_PREFIX}Built-in"),
                    label: "Built-in".to_string(),
                    checked: false,
                    enabled: true,
                },
                MicMenuRow {
                    id: format!("{MIC_PREFIX}USB Mic"),
                    label: "USB Mic".to_string(),
                    checked: true,
                    enabled: true,
                },
            ]
        );
    }

    #[test]
    fn a_saved_device_that_has_gone_falls_back_to_default_and_shows_a_disabled_ghost_row() {
        let rows = microphone_menu_items(&view(&["Built-in"], Some("USB Mic"), Some("Built-in")));
        assert_eq!(
            rows,
            vec![
                MicMenuRow {
                    id: MIC_DEFAULT.to_string(),
                    label: "System default (Built-in)".to_string(),
                    checked: true,
                    enabled: true,
                },
                MicMenuRow {
                    id: format!("{MIC_PREFIX}USB Mic"),
                    label: "USB Mic (not connected)".to_string(),
                    checked: false,
                    enabled: false,
                },
                MicMenuRow {
                    id: format!("{MIC_PREFIX}Built-in"),
                    label: "Built-in".to_string(),
                    checked: false,
                    enabled: true,
                },
            ]
        );
    }

    #[test]
    fn unresolvable_default_reads_as_plain_system_default() {
        let rows = microphone_menu_items(&view(&["Built-in"], None, None));
        assert_eq!(rows[0].label, "System default");
        assert!(rows[0].checked);
    }

    #[test]
    fn an_ampersand_in_a_device_name_is_escaped_so_muda_does_not_read_it_as_a_mnemonic() {
        let rows = microphone_menu_items(&view(&["Bose A&B"], None, Some("Bose A&B")));
        assert_eq!(rows[0].label, "System default (Bose A&&B)");
        assert_eq!(rows[1].label, "Bose A&&B");
    }
}
