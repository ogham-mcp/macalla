// SPDX-License-Identifier: MIT
//! Showing and hiding the overlay on Windows without taking focus.
//!
//! Tauri's `show()` reaches tao, whose every show after the first is a plain
//! `ShowWindow(SW_SHOW)`. That activates the window whenever Macalla already
//! owns the foreground, so with Macalla's own window in front the overlay took
//! focus. The overlay is a WebView2 window, and while one of those has focus
//! Windows calls no low-level keyboard hook in this process, so the release of
//! the shortcut was never heard and the dictation ran on. The window's
//! `focusable: false` (`WS_EX_NOACTIVATE`) only stops a click from activating
//! it, not a show.
//!
//! So on Windows the overlay is shown and hidden here, through Win32, and never
//! through tao: showing it here and hiding it through tao would leave tao
//! thinking it was still hidden, and its hide would do nothing. The web view
//! inside stays visible throughout, so nothing else needs telling. The show
//! asks for the frame to be worked out again (`SWP_FRAMECHANGED`), as tao's own
//! show does: tao takes the title bar off an undecorated window only when
//! Windows asks it for the frame, and without that the overlay appeared with a
//! title bar and a close button.
//!
//! Both calls are posted to the main thread, the window's own, rather than
//! made from the dictation thread, which must never wait on the main thread
//! (see `dictation.rs` on quitting). Posted after `set_position`, which goes
//! the same way, so the overlay appears where it was placed.

use tauri::{WebviewWindow, Wry};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, ShowWindow, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
};

/// Show `overlay` on top of everything, leaving focus where it is.
pub(crate) fn show(overlay: &WebviewWindow<Wry>) {
    let window = overlay.clone();
    let _ = overlay.run_on_main_thread(move || {
        let Ok(hwnd) = window.hwnd() else {
            return;
        };
        // SAFETY: the overlay's own window handle, on its own thread; no
        // pointer is passed.
        unsafe {
            SetWindowPos(
                hwnd.0 as _,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
            )
        };
    });
}

/// Hide `overlay`.
pub(crate) fn hide(overlay: &WebviewWindow<Wry>) {
    let window = overlay.clone();
    let _ = overlay.run_on_main_thread(move || {
        let Ok(hwnd) = window.hwnd() else {
            return;
        };
        // SAFETY: as in `show`.
        unsafe { ShowWindow(hwnd.0 as _, SW_HIDE) };
    });
}
