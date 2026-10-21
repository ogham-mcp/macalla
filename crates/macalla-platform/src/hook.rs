// SPDX-License-Identifier: MIT
//! A listen-only low-level keyboard hook for Windows: virtual-key codes, down
//! and up, nothing else.
//!
//! The Windows counterpart of `tap.rs`. `WH_KEYBOARD_LL` is what delivers a
//! global key-release, which push-to-talk is made of; `RegisterHotKey` and
//! the shortcut plugins built on it report a press only. The hook needs no
//! permission and no elevation. It does not see keys pressed in a window
//! that runs elevated when Macalla does not, or on the secure desktop (a
//! User Account Control prompt, the lock screen).
//!
//! The hook never swallows a key: every event goes on to `CallNextHookEx`.
//!
//! Windows calls the hook on the thread that installed it, from that
//! thread's message loop, and removes a hook whose callback is slow, without
//! telling anyone (`LowLevelHooksTimeout`). So the callback does as little as
//! the recogniser needs, and the thread does nothing else.

use crate::vkey::{from_vkey, is_altgr_control, reconcile, HookEvent};
use std::cell::RefCell;
use std::collections::HashSet;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KillTimer, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx,
    HC_ACTION, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_TIMER,
};

/// How often, while a key is remembered as down, the thread checks that it
/// still is. Short enough that a key let go out of the hook's sight stops a
/// dictation promptly; the timer does not run while nothing is held.
const RECONCILE_MS: u32 = 250;

struct State {
    on_event: Box<dyn Fn(HookEvent)>,
    /// The shortcut keys seen going down and not yet seen coming up.
    held: HashSet<u32>,
    /// The thread timer's id while it runs, 0 otherwise.
    timer: usize,
}

thread_local! {
    // The hook procedure takes no user pointer, and Windows calls it on the
    // installing thread, so the state lives in that thread.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Whether the key is physically down right now, whichever window has focus.
pub(crate) fn physically_down(code: u32) -> bool {
    // SAFETY: takes a virtual-key code by value and reads only. The top bit
    // of the result is the key's state.
    let state = unsafe { GetAsyncKeyState(code as i32) };
    (state as u16) & 0x8000 != 0
}

impl State {
    fn key(&mut self, code: u32, down: bool) {
        // Only keys the recogniser knows are remembered or delivered.
        if from_vkey(code).is_none() {
            return;
        }
        if down {
            let repeat = !self.held.insert(code);
            (self.on_event)(HookEvent::Down { code, repeat });
        } else {
            self.held.remove(&code);
            (self.on_event)(HookEvent::Up(code));
        }
        self.keep_timer();
    }

    /// Deliver a key-up for every remembered key that is no longer down.
    fn look(&mut self) {
        for code in reconcile(&mut self.held, physically_down) {
            (self.on_event)(HookEvent::Up(code));
        }
        self.keep_timer();
    }

    /// The timer runs exactly while something is held.
    fn keep_timer(&mut self) {
        if self.held.is_empty() && self.timer != 0 {
            // SAFETY: a thread timer this thread created, by its id.
            unsafe { KillTimer(std::ptr::null_mut(), self.timer) };
            self.timer = 0;
        } else if !self.held.is_empty() && self.timer == 0 {
            // SAFETY: no window and no callback: the timer posts WM_TIMER to
            // this thread's queue, where `listen` picks it up. Returns 0 on
            // failure, which leaves the timer off and is tried again on the
            // next key.
            self.timer = unsafe { SetTimer(std::ptr::null_mut(), 0, RECONCILE_MS, None) };
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a
        // KBDLLHOOKSTRUCT that is valid for the length of this call.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let (vk_code, scan_code) = (event.vkCode, event.scanCode);
        // Macalla's own keys (the paste's Ctrl+V, or typed text), marked by
        // enigo: never the person pressing the shortcut.
        let ours = event.dwExtraInfo == enigo::EVENT_MARKER as usize;
        let down = match wparam as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
            WM_KEYUP | WM_SYSKEYUP => Some(false),
            _ => None,
        };
        // AltGr's made-up Left Ctrl, and Macalla's own keys, are not keys
        // the person pressed: they are passed on, but never reach the
        // recogniser.
        let down = down.filter(|_| !ours && !is_altgr_control(vk_code, scan_code));
        if let Some(down) = down {
            STATE.with(|state| {
                // `try_borrow_mut`: a callback that somehow re-entered would
                // skip the event rather than abort the process from inside
                // a foreign call.
                if let Ok(mut state) = state.try_borrow_mut() {
                    if let Some(state) = state.as_mut() {
                        state.key(vk_code, down);
                    }
                }
            });
        }
    }
    // SAFETY: passes the event on unchanged, as a hook must.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Run the hook on the current thread until its message loop ends, which it
/// does not on its own -- this is the listener thread's whole life.
///
/// Fails when the hook cannot be installed.
pub fn listen(on_event: impl Fn(HookEvent) + 'static) -> Result<(), String> {
    STATE.with(|state| {
        *state.borrow_mut() = Some(State {
            on_event: Box::new(on_event),
            held: HashSet::new(),
            timer: 0,
        });
    });
    // SAFETY: a null name asks for the handle of this process's own module;
    // the hook procedure is a plain function that lives as long as the
    // process; thread id 0 makes the hook global, which a low-level hook
    // must be.
    let hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(hook_proc),
            GetModuleHandleW(std::ptr::null()),
            0,
        )
    };
    if hook.is_null() {
        return Err(format!(
            "the keyboard hook could not be installed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: MSG is plain data and GetMessageW fills it in.
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: a valid pointer to a MSG, no window filter. Windows calls
        // `hook_proc` from inside this call.
        let got = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        // 0 is WM_QUIT and -1 an error; either way the loop is over.
        if got <= 0 {
            break;
        }
        if message.message == WM_TIMER {
            STATE.with(|state| {
                if let Some(state) = state.borrow_mut().as_mut() {
                    state.look();
                }
            });
        }
    }
    // SAFETY: the hook this function installed.
    unsafe { UnhookWindowsHookEx(hook) };
    Ok(())
}
