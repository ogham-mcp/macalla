// SPDX-License-Identifier: MIT
//! The mute gesture on the user's own hardware: an AirPods stem press, an
//! AirPods Max crown press, the mute button on a headset.
//!
//! On macOS 14 and later, while an app is capturing from AirPods, pressing
//! the stem is routed to that app as a request to mute its input. An app
//! with no handler gets a "Cannot Control Mic with AirPods" notification
//! and a rejection chime, and the recording carries on. That is the default
//! behaviour of every app that never registered, and a dictation app is
//! the one place the gesture has an obvious meaning: the stem is already in
//! the user's hand, and "mute me" during a dictation is "stop".
//!
//! `AVAudioApplication.sharedInstance().setInputMuteStateChangeHandler:` is
//! in-process and needs no permission. It takes an Objective-C block and
//! calls it with whether input should now be muted; returning true says the
//! app handled it, which is what stops the notification.

use crate::{Error, Result};
use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyClass, AnyObject, Bool};

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

/// Register `handler` for the hardware mute gesture. It is called with
/// `true` for a mute and `false` for an unmute, from whatever thread the
/// system chooses, so it must only send a message. Registered once for the
/// process; a second call replaces the first.
///
/// Fails on macOS before 14, where the class does not exist, and the caller
/// carries on without the gesture.
pub fn on_input_mute_request(handler: impl Fn(bool) + Send + Sync + 'static) -> Result<()> {
    let Some(class) = AnyClass::get(c"AVAudioApplication") else {
        return Err(Error::Audio(
            "the mute gesture needs macOS 14 or later (no AVAudioApplication)".into(),
        ));
    };
    let block = RcBlock::new(move |muted: Bool| -> Bool {
        handler(muted.as_bool());
        Bool::YES
    });
    // Caught, not trusted: an Objective-C exception thrown here would unwind
    // through the app's launch callback, which cannot unwind, and the app
    // aborts at start with "panic in a function that cannot unwind" and no
    // reason anywhere. Seen on 2026-09-09, launched from Finder. With the
    // catch, the reason is logged and the gesture is simply not available.
    // AssertUnwindSafe: nothing inside is observed again if it unwinds; the
    // block is dropped with the rest.
    let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `sharedInstance` is a class method returning the process
        // singleton, which lives for the process; the setter takes one
        // block argument of the shape declared above, and the block is
        // copied by the receiver, so the local reference may go.
        unsafe {
            let app: *mut AnyObject = msg_send![class, sharedInstance];
            if app.is_null() {
                return false;
            }
            // The Swift name is `setInputMuteStateChangeHandler(_:)`; the
            // Objective-C selector carries an error out-parameter and
            // returns whether it took. Sending the Swift name raised
            // "unrecognized selector", which is the exception the catch
            // above turned from a crash into this comment.
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let took: Bool = msg_send![
                app,
                setInputMuteStateChangeHandler: &*block,
                error: &mut error as *mut *mut AnyObject
            ];
            if !took.as_bool() {
                return false;
            }
            // Declare the input unmuted, as the AirMute project does. NOT
            // sufficient either: with the handler and this call the system
            // still logs "not allowed to input mute (isOptedIn: 0,
            // hasHandler: 1)" and draws the banner (2026-09-09, macOS 26.6,
            // AirPods Pro). Whatever marks an app as opted in is not in
            // Apple's documentation for this class; the task tracks the
            // hypotheses. Kept because it is correct and harmless.
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let _: Bool = msg_send![
                app,
                setInputMuted: Bool::NO,
                error: &mut error as *mut *mut AnyObject
            ];
            true
        }
    }));
    match result {
        Ok(true) => {
            tracing::debug!("the hardware mute gesture will stop a dictation");
            Ok(())
        }
        Ok(false) => Err(Error::Audio(
            "AVAudioApplication would not take the mute handler".into(),
        )),
        Err(exception) => Err(Error::Audio(format!(
            "registering the mute gesture: {}",
            describe(exception.as_deref())
        ))),
    }
}

/// Cancel the handler, as at the end of a dictation. Apple's header: "A nil
/// value may be provided to cancel the block being called, e.g. at end of
/// call lifecycle."
pub fn clear_input_mute_handler() -> Result<()> {
    let Some(class) = AnyClass::get(c"AVAudioApplication") else {
        return Ok(());
    };
    let result = objc2::exception::catch(std::panic::AssertUnwindSafe(|| {
        // SAFETY: as in `on_input_mute_request`, with a nil block.
        unsafe {
            let app: *mut AnyObject = msg_send![class, sharedInstance];
            if app.is_null() {
                return;
            }
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let nil: *const std::ffi::c_void = std::ptr::null();
            let _: Bool = msg_send![
                app,
                setInputMuteStateChangeHandler: nil,
                error: &mut error as *mut *mut AnyObject
            ];
        }
    }));
    result.map_err(|exception| {
        Error::Audio(format!(
            "clearing the mute gesture: {}",
            describe(exception.as_deref())
        ))
    })
}

/// An exception's reason as text, for the log.
fn describe(exception: Option<&objc2::exception::Exception>) -> String {
    let Some(exception) = exception else {
        return "an Objective-C exception with no object".into();
    };
    // SAFETY: `reason` returns an NSString or nil; `UTF8String` on an
    // NSString returns a C string that lives as long as the string does,
    // which is the length of this function.
    unsafe {
        let reason: *mut AnyObject = msg_send![exception, reason];
        if reason.is_null() {
            return "an Objective-C exception with no reason".into();
        }
        let text: *const std::ffi::c_char = msg_send![reason, UTF8String];
        if text.is_null() {
            return "an Objective-C exception with no reason".into();
        }
        std::ffi::CStr::from_ptr(text)
            .to_string_lossy()
            .into_owned()
    }
}
