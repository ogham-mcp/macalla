// SPDX-License-Identifier: MIT
//! Synthetic keystrokes on macOS, posted as CGEvents: the paste chord and,
//! as the fallback, the text itself.
//!
//! This replaced `enigo` on macOS on 2026-09-09, the same day and for the
//! same reason the event tap replaced `rdev`: enigo turns the letter of a
//! chord into a keycode by asking HIToolbox about the keyboard layout, macOS
//! 26 asserts that call is made on the main dispatch queue, and the dictation
//! thread is not it. The app died at the exact moment it would have pasted.
//!
//! No layout lookup happens here. The V key is posted by its virtual keycode
//! with the Command flag and nothing else: a key event that also carries a
//! string is delivered as typed text, and a chord with text on it pastes
//! nothing (measured against the paste harness, 2026-09-09). On a layout
//! where that physical key is not V -- Dvorak, mostly -- the chord will not
//! read as paste; the typing fallback still works there, one event per
//! character with the character set on it, which is how macOS itself
//! delivers input from a non-keyboard.
//!
//! Posting events needs Accessibility, which is what [`crate::paste`] checks
//! and asks for before it gets here.

use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

use crate::hotkey::keycode;

/// Virtual keycode of the V key on an ANSI layout.
const KEY_V: u16 = 9;

pub struct CgKeys {
    source: CGEventSource,
}

impl CgKeys {
    pub fn new() -> crate::Result<Self> {
        // The private state, as enigo used: a source of our own rather than
        // a view of the HID system's, so our modifier flags are ours.
        CGEventSource::new(CGEventSourceStateID::Private)
            .map(|source| Self { source })
            .map_err(|()| crate::Error::Input("could not create an event source".into()))
    }

    fn key(
        &self,
        code: u16,
        down: bool,
        flags: CGEventFlags,
        text: Option<&str>,
    ) -> crate::Result<()> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), code, down).map_err(|()| {
            crate::Error::Input(format!("could not create a key event for {code}"))
        })?;
        event.set_flags(flags);
        if let Some(text) = text {
            event.set_string(text);
        }
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl crate::paste::Keystrokes for CgKeys {
    fn paste_chord(&mut self) -> crate::Result<()> {
        let cmd = CGEventFlags::CGEventFlagCommand;
        self.key(keycode::COMMAND_LEFT, true, cmd, None)?;
        let tapped = self
            .key(KEY_V, true, cmd, None)
            .and_then(|()| self.key(KEY_V, false, cmd, None));
        // Release Command whatever happened to the tap. Leaving it held turns
        // the user's next keystroke into a shortcut.
        let released = self.key(
            keycode::COMMAND_LEFT,
            false,
            CGEventFlags::CGEventFlagNull,
            None,
        );
        tapped.and(released)
    }

    fn type_text(&mut self, text: &str) -> crate::Result<()> {
        let mut buf = [0u8; 4];
        for c in text.chars() {
            let s: &str = c.encode_utf8(&mut buf);
            // Keycode 0 is A; the string on the event is what gets typed.
            self.key(0, true, CGEventFlags::CGEventFlagNull, Some(s))?;
            self.key(0, false, CGEventFlags::CGEventFlagNull, Some(s))?;
        }
        Ok(())
    }
}
