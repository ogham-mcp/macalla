// SPDX-License-Identifier: MIT
//! Platform integration: global hotkey, text insertion, permission checks.

pub mod audio;
pub mod decode;
pub mod hotkey;
#[cfg(target_os = "macos")]
mod keys;
#[cfg(target_os = "macos")]
pub mod mute;
pub mod onboarding;
pub mod paste;
pub mod secrets;
pub mod system_audio;
#[cfg(all(feature = "hotkey", target_os = "macos"))]
mod tap;

/// The operating system, as data: what differs between the three is
/// knowledge this crate owns, and a caller that has to word something per
/// platform asks for [`Os::HERE`] rather than reaching for `cfg!` itself.
/// Every per-OS answer takes one of these, so a test on any machine can ask
/// what another platform would say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Windows,
    /// Linux, and any other Unix this builds on: none of them has a
    /// permission model of its own here.
    Linux,
}

impl Os {
    /// The platform this build is for.
    pub const HERE: Os = if cfg!(target_os = "macos") {
        Os::MacOs
    } else if cfg!(target_os = "windows") {
        Os::Windows
    } else {
        Os::Linux
    };
}

/// A permission the app needs, and whether it has been granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// To hear you.
    Microphone,
    /// To notice when you hold the shortcut. Needed for key-release events
    /// on macOS, which is why hotkeys use `rdev`.
    InputMonitoring,
    /// To paste into other apps. Granted by hand; cannot be granted
    /// programmatically.
    Accessibility,
}

/// What can go wrong in this crate.
///
/// The variants exist so a caller can ACT on the difference without reading
/// message text. Onboarding matches on [`Error::Permission`] to send someone to
/// System Settings; everything else is a runtime failure it can only report.
/// `thiserror` in library crates, `anyhow` only at the app boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A permission the OS has not granted. The only variant a user can fix,
    /// and the only one with somewhere to send them.
    #[error("{permission:?} has not been granted -- System Settings > {}", permission.settings_pane())]
    Permission { permission: Permission },

    /// A microphone could not be opened, listed, or read.
    #[error("{0}")]
    Audio(String),

    /// The capture device stopped mid-recording and the stream is no longer
    /// running. Separate from [`Error::Audio`] because the user can act on it:
    /// press again. The commonest cause on macOS is a Bluetooth headset
    /// switching from A2DP into HFP when its microphone is activated, which
    /// changes the device's sample rate and invalidates the stream.
    #[error(
        "the microphone {0:?} stopped mid-recording -- a Bluetooth headset \
         switching into headset mode will do this. Whatever was captured up to \
         that point has been kept; start again for the rest."
    )]
    DeviceLost(String),

    /// The clipboard could not be read or written.
    #[error("the clipboard could not be used: {0}")]
    Clipboard(String),

    /// Synthetic key events could not be sent.
    #[error("key events could not be sent: {0}")]
    Input(String),

    /// A shortcut string that is not a shortcut.
    #[error("{0}")]
    Shortcut(String),

    /// The global listener stopped, or never started.
    #[error("the global shortcut listener stopped: {0}")]
    Listener(String),

    /// A secret for `account` could not be stored, read, or deleted. Never
    /// carries the secret itself -- only the account name and what the store
    /// said went wrong. See [`secrets`].
    #[error("the secret for {account:?} could not be used: {reason}")]
    Secret { account: String, reason: String },

    /// The secret store itself could not be reached: the keychain or the
    /// Credential Manager refused, or this platform has none here (Linux, for
    /// now).
    /// Distinct from [`Error::Secret`] because there is nothing
    /// account-specific to say.
    #[error("the secret store is unavailable: {0}")]
    SecretStoreUnavailable(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Whether the OS has granted a permission to this process.
///
/// `Unknown` is a real and important third state on macOS: it means nobody has
/// asked yet, so a prompt will appear. `Denied` means the user said no and TCC
/// will NOT ask again -- the app has to send them to System Settings instead of
/// waiting for a dialog that is never coming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Granted,
    Denied,
    /// Never asked. Requesting will prompt.
    Unknown,
    /// This platform does not gate the capability at all.
    NotRequired,
}

#[cfg(target_os = "macos")]
mod tcc {
    use std::ffi::c_void;

    // A ListenOnly CGEventTap -- which is what rdev creates -- is gated on
    // Input Monitoring (kIOHIDRequestTypeListenEvent), NOT on Accessibility.
    // Verified 2026-09-08 on a machine with Input Monitoring granted and
    // Accessibility denied: the tap was created and delivered events.
    // Accessibility is what `enigo` needs to SYNTHESISE the paste keystroke.
    const LISTEN_EVENT: u32 = 1;
    const GRANTED: i32 = 0;
    const DENIED: i32 = 1;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOHIDCheckAccess(request: u32) -> i32;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
        static kAXTrustedCheckOptionPrompt: *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: *const c_void;
        fn CFRelease(cf: *const c_void);
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
    }

    #[link(name = "objc", kind = "dylib")]
    extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *const c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *const c_void;
        fn objc_msgSend();
    }

    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        static AVMediaTypeAudio: *const c_void;
    }

    /// `[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`.
    /// Reports; never prompts. The prompt comes from the first capture.
    pub fn microphone() -> super::Grant {
        const NOT_DETERMINED: isize = 0;
        const RESTRICTED: isize = 1;
        const DENIED: isize = 2;
        const AUTHORIZED: isize = 3;
        // SAFETY: class and selector names are static C strings; the call is
        // a class method taking one object argument and returning an integer,
        // and objc_msgSend is cast to exactly that shape.
        let status = unsafe {
            let class = objc_getClass(c"AVCaptureDevice".as_ptr());
            let selector = sel_registerName(c"authorizationStatusForMediaType:".as_ptr());
            if class.is_null() || selector.is_null() {
                return super::Grant::Unknown;
            }
            let send: extern "C" fn(*const c_void, *const c_void, *const c_void) -> isize =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            send(class, selector, AVMediaTypeAudio)
        };
        match status {
            AUTHORIZED => super::Grant::Granted,
            DENIED | RESTRICTED => super::Grant::Denied,
            NOT_DETERMINED => super::Grant::Unknown,
            _ => super::Grant::Unknown,
        }
    }

    pub fn input_monitoring() -> super::Grant {
        // SAFETY: no arguments, returns a plain enum value.
        match unsafe { IOHIDCheckAccess(LISTEN_EVENT) } {
            GRANTED => super::Grant::Granted,
            DENIED => super::Grant::Denied,
            _ => super::Grant::Unknown,
        }
    }

    /// Ask macOS to show the Accessibility prompt.
    ///
    /// Returns whether it is granted right now, which for a first ask is
    /// almost always false -- the dialog is modal to the USER, not to us, and
    /// the grant lands later. After a denial macOS shows nothing at all.
    pub fn request_accessibility() -> bool {
        let key = unsafe { kAXTrustedCheckOptionPrompt };
        let value = unsafe { kCFBooleanTrue };
        let keys = [key];
        let values = [value];
        // SAFETY: one key, one value, both static CF constants.
        let options = unsafe {
            CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        // SAFETY: a valid options dictionary; the call takes it by reference.
        let trusted = unsafe { AXIsProcessTrustedWithOptions(options) };
        if !options.is_null() {
            // SAFETY: we created it, so we release it.
            unsafe { CFRelease(options) };
        }
        trusted
    }

    pub fn accessibility() -> super::Grant {
        // Null options means "do not prompt" -- this reports, it never asks.
        // SAFETY: a null options dictionary is explicitly allowed.
        if unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) } {
            super::Grant::Granted
        } else {
            // The API cannot distinguish "denied" from "never asked", so this
            // is deliberately not reported as Denied.
            super::Grant::Unknown
        }
    }
}

/// Whether macOS Secure Event Input is on anywhere in this login session.
/// `None` where the question does not exist.
///
/// This is the fact behind an evening lost on 2026-09-08. Any process -- a
/// terminal with *Secure Keyboard Entry* ticked, a password field -- can switch
/// on a session-wide mode that hides every ordinary key from every event tap,
/// and the setting survives relaunches. Under it a shortcut with a letter in it
/// does nothing, with no message, and looks exactly like a denied permission.
/// Modifier-class events still arrive, which is why the default shortcut is a
/// bare modifier and why this is a warning rather than a blocker.
///
/// Only the fact is reported, never the holder. The IORegistry does carry a
/// `kCGSSessionSecureInputPID`, but it names whichever app was *active* when
/// secure input was switched on rather than the process that switched it on
/// (rdar://48953777), and it was observed still naming a process that had
/// exited. A wrong name sent to the user is worse than none.
pub fn secure_input_enabled() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        Some(secure_input::enabled())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod secure_input {
    // `Boolean IsSecureEventInputEnabled(void)`, HIToolbox/CarbonEventsCore.h,
    // public since 10.3 and exported by the Command Line Tools SDK. No
    // argument, because the answer is per session, not per process.
    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn IsSecureEventInputEnabled() -> u8;
    }

    pub fn enabled() -> bool {
        // SAFETY: no arguments, returns a C Boolean.
        unsafe { IsSecureEventInputEnabled() != 0 }
    }
}

/// The app in front, by its bundle identifier on macOS: asked as a dictation
/// starts, and again before a cloud engine's late text is pasted, so text
/// never lands in a window the person has moved to.
///
/// `None` where the question has no answer: nothing in front, an app with no
/// bundle identifier, and Linux and Windows, which have no query here yet. A
/// caller reads `None` as "cannot tell" and never as "another app", so on
/// those platforms text pastes as it always has.
pub fn frontmost_app() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        front::bundle_identifier()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod front {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::{c_char, CStr};

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    /// `[[[NSWorkspace sharedWorkspace] frontmostApplication] bundleIdentifier]`.
    pub fn bundle_identifier() -> Option<String> {
        let class = AnyClass::get(c"NSWorkspace")?;
        // The getters hand back autoreleased objects, and the dictation thread
        // has no pool of its own; the text is copied out before this one drains.
        objc2::rc::autoreleasepool(|_| {
            // SAFETY: `sharedWorkspace` is a class method returning the process
            // singleton; `frontmostApplication` returns an NSRunningApplication
            // or nil; `bundleIdentifier` returns an NSString or nil; and
            // `UTF8String` returns a C string that lives as long as the string,
            // which the pool keeps until the copy below is made. Each result is
            // checked for nil before it is sent anything.
            unsafe {
                let workspace: *mut AnyObject = msg_send![class, sharedWorkspace];
                if workspace.is_null() {
                    return None;
                }
                let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
                if app.is_null() {
                    return None;
                }
                let id: *mut AnyObject = msg_send![app, bundleIdentifier];
                if id.is_null() {
                    return None;
                }
                let text: *const c_char = msg_send![id, UTF8String];
                if text.is_null() {
                    return None;
                }
                Some(CStr::from_ptr(text).to_string_lossy().into_owned())
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use objc2::runtime::AnyClass;

        #[test]
        fn nsworkspace_is_linked_so_the_question_can_be_asked() {
            // Without AppKit in the binary the class lookup fails and every
            // answer is `None`, which reads as "cannot tell" and would pass
            // the shape check above for the wrong reason.
            assert!(AnyClass::get(c"NSWorkspace").is_some());
        }
    }
}

impl Permission {
    /// The onboarding card copy. Plain English, one sentence.
    pub fn reason(&self) -> &'static str {
        match self {
            Permission::Microphone => "To hear you.",
            Permission::InputMonitoring => "To notice when you hold the shortcut.",
            Permission::Accessibility => "To paste into other apps.",
        }
    }

    /// Whether this process has the permission right now.
    ///
    /// Reports; never prompts. The microphone's state comes from
    /// AVFoundation's authorisation status, which distinguishes "never
    /// asked" from "refused" the way TCC does for the others.
    pub fn granted(&self) -> Grant {
        #[cfg(target_os = "macos")]
        {
            match self {
                Permission::InputMonitoring => tcc::input_monitoring(),
                Permission::Accessibility => tcc::accessibility(),
                Permission::Microphone => tcc::microphone(),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            match self {
                Permission::Microphone => Grant::Unknown,
                _ => Grant::NotRequired,
            }
        }
    }

    /// Where the user goes to change it.
    pub fn settings_pane(&self) -> &'static str {
        match self {
            Permission::Microphone => "Privacy & Security > Microphone",
            Permission::InputMonitoring => "Privacy & Security > Input Monitoring",
            Permission::Accessibility => "Privacy & Security > Accessibility",
        }
    }

    /// A URL that opens that pane, on platforms that have one.
    pub fn settings_url(&self) -> Option<&'static str> {
        self.settings_url_on(Os::HERE)
    }

    /// [`Permission::settings_url`], as `os` would answer it.
    pub fn settings_url_on(&self, os: Os) -> Option<&'static str> {
        if os != Os::MacOs {
            return None;
        }
        Some(match self {
            Permission::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            Permission::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
            Permission::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
        })
    }

    /// The permission behind an onboarding step id, if it is one.
    pub fn from_step_id(id: &str) -> Option<Permission> {
        Some(match id {
            "microphone" => Permission::Microphone,
            "input-monitoring" => Permission::InputMonitoring,
            "accessibility" => Permission::Accessibility,
            _ => return None,
        })
    }

    /// Ask the OS for this permission, showing its prompt.
    ///
    /// Distinct from [`Permission::granted`], which only reports. Onboarding
    /// needs both: a checklist that can be drawn without throwing dialogs at
    /// someone, and a button that actually asks.
    ///
    /// Returns whether it is granted immediately afterwards, which on a first
    /// ask is almost always false -- the dialog is modal to the user, and the
    /// grant arrives whenever they get round to it. After a denial macOS shows
    /// nothing and this returns false forever.
    pub fn request(&self) -> Grant {
        #[cfg(target_os = "macos")]
        {
            match self {
                Permission::Accessibility => {
                    if tcc::request_accessibility() {
                        Grant::Granted
                    } else {
                        Grant::Unknown
                    }
                }
                // The microphone's prompt comes from capturing. Opening a
                // stream and starting it IS the ask; it is dropped straight
                // after, and the indicator lights for the moment it takes.
                // The bundle must carry NSMicrophoneUsageDescription or macOS
                // shows nothing and the app never appears in the list.
                Permission::Microphone => {
                    match crate::audio::Capture::open(None, 1.0).and_then(|mut c| c.play()) {
                        Ok(()) => {}
                        Err(e) => tracing::debug!(error = %e, "the microphone ask failed"),
                    }
                    self.granted()
                }
                // Input Monitoring prompts when the event tap is created,
                // which the listener does at startup. No ask-me-now call.
                Permission::InputMonitoring => self.granted(),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.granted()
        }
    }

    /// Whether `os` has an ask for this permission that onboarding can
    /// offer as a button: a prompt, or a moment of capture that brings one
    /// up ([`Permission::request`]).
    pub fn can_ask_on(&self, os: Os) -> bool {
        os == Os::MacOs && matches!(self, Permission::Microphone | Permission::Accessibility)
    }

    /// What this platform actually asks for.
    pub fn required_here() -> &'static [Permission] {
        Self::required_on(Os::HERE)
    }

    /// [`Permission::required_here`], as `os` would answer it.
    ///
    /// Nothing on Windows or Linux. Windows does have a microphone switch
    /// for desktop apps under Settings > Privacy & security > Microphone,
    /// but this crate has no query for it and no ask ([`Permission::granted`]
    /// can only say `Unknown`, and [`Permission::request`] does nothing), so
    /// a card for it could never tick and onboarding would never finish.
    /// A capture the OS refuses still arrives as [`Error::Permission`] when
    /// dictation starts, and the checklist's Try it row proves the
    /// microphone is heard. Linux has no microphone permission here at all.
    pub fn required_on(os: Os) -> &'static [Permission] {
        match os {
            Os::MacOs => &[
                Permission::Microphone,
                Permission::InputMonitoring,
                Permission::Accessibility,
            ],
            Os::Windows | Os::Linux => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checking_and_asking_are_different_operations() {
        // `granted` draws the checklist; `request` is the button on it. A
        // codebase with only the first never shows anyone the dialog, and a
        // codebase with only the second throws dialogs to render a screen.
        let checked = Permission::Accessibility.granted();
        if cfg!(target_os = "macos") {
            assert_ne!(
                checked,
                Grant::NotRequired,
                "macOS gates paste on Accessibility, so it is required here"
            );
        } else {
            // Windows and Linux do not gate synthetic input on a permission,
            // and NotRequired is the honest answer rather than a missing one.
            // The first version of this test listed only the macOS variants
            // and failed on Windows -- a platform assumption inside a
            // cross-platform test, which is the thing this crate exists to
            // avoid.
            assert_eq!(
                checked,
                Grant::NotRequired,
                "only macOS gates this, so elsewhere it must say so"
            );
        }
    }

    #[test]
    fn onboarding_can_spot_a_permission_problem_without_reading_the_message() {
        // The whole reason these are typed. Matching on message text is how
        // "no Accessibility" and "clipboard busy" end up handled the same way.
        let cases: Vec<Error> = vec![
            Error::Permission {
                permission: Permission::Accessibility,
            },
            Error::Clipboard("busy".into()),
            Error::Input("no device".into()),
        ];
        let fixable: Vec<&Permission> = cases
            .iter()
            .filter_map(|e| match e {
                Error::Permission { permission } => Some(permission),
                _ => None,
            })
            .collect();
        assert_eq!(fixable, vec![&Permission::Accessibility]);
    }

    #[test]
    fn a_permission_error_says_where_to_go_and_fix_it() {
        let e = Error::Permission {
            permission: Permission::InputMonitoring,
        };
        let message = e.to_string();
        assert!(message.contains("Input Monitoring"), "got: {message}");
        assert!(message.contains("System Settings"), "got: {message}");
    }

    #[test]
    fn asking_about_a_permission_never_prompts_and_never_panics() {
        // The whole point of `granted` is that onboarding can render a
        // checklist without throwing dialogs at the user to draw it.
        for p in Permission::required_here() {
            let g = p.granted();
            println!("{p:?}: {g:?}  -> {}", p.settings_pane());
            assert!(!p.settings_pane().is_empty());
        }
    }

    #[test]
    fn a_denial_is_distinct_from_never_having_asked() {
        // These must not collapse into one "not granted" state. macOS does not
        // re-prompt after a denial, so an app that treats them the same waits
        // for a dialog that is never coming -- which is exactly what happened
        // on 2026-09-08.
        assert_ne!(Grant::Denied, Grant::Unknown);
        assert_ne!(Grant::Denied, Grant::NotRequired);
    }

    #[test]
    fn macos_needs_input_monitoring_for_key_release() {
        assert!(Permission::required_on(Os::MacOs).contains(&Permission::InputMonitoring));
        assert_eq!(Permission::required_on(Os::MacOs).len(), 3);
        assert_eq!(
            Permission::required_here(),
            Permission::required_on(Os::HERE)
        );
    }

    #[test]
    fn windows_and_linux_have_no_permission_this_crate_can_check_or_ask_for() {
        // Neither has a query here for the microphone, and `request` does
        // nothing off macOS, so a card for it could only ever say "not
        // asked yet" and never tick: onboarding would never finish. Whether
        // the microphone is heard is proved by the checklist's Try it row,
        // and a capture the OS refuses still arrives as
        // `Error::Permission` at dictation time.
        for os in [Os::Windows, Os::Linux] {
            assert_eq!(Permission::required_on(os), &[] as &[Permission], "{os:?}");
            for p in [
                Permission::Microphone,
                Permission::InputMonitoring,
                Permission::Accessibility,
            ] {
                assert!(!p.can_ask_on(os), "{os:?} {p:?}");
                assert_eq!(p.settings_url_on(os), None, "{os:?} {p:?}");
            }
        }
    }

    #[test]
    fn macos_asks_for_the_microphone_and_accessibility_only() {
        // Input Monitoring is asked when the tap is created at launch.
        assert!(Permission::Microphone.can_ask_on(Os::MacOs));
        assert!(Permission::Accessibility.can_ask_on(Os::MacOs));
        assert!(!Permission::InputMonitoring.can_ask_on(Os::MacOs));
    }

    #[test]
    fn every_permission_has_a_pane_to_open_on_macos_and_none_elsewhere() {
        for p in Permission::required_here() {
            let url = p.settings_url();
            if cfg!(target_os = "macos") {
                assert!(
                    url.is_some_and(|u| u.starts_with("x-apple.systempreferences:")),
                    "{p:?}: {url:?}"
                );
            } else {
                assert_eq!(url, None);
            }
            assert_eq!(
                Permission::from_step_id(match p {
                    Permission::Microphone => "microphone",
                    Permission::InputMonitoring => "input-monitoring",
                    Permission::Accessibility => "accessibility",
                }),
                Some(*p)
            );
        }
        assert_eq!(Permission::from_step_id("engine"), None);
    }

    #[test]
    fn secure_input_is_a_question_only_macos_can_answer() {
        // Never prompts, never panics, and says "no such thing" rather than
        // "off" where the mode does not exist -- a Linux checklist must not
        // grow a card for a macOS setting.
        let answer = secure_input_enabled();
        if cfg!(target_os = "macos") {
            assert!(answer.is_some());
            println!("secure input: {answer:?}");
        } else {
            assert_eq!(answer, None);
        }
    }

    #[test]
    fn the_app_in_front_is_a_question_only_macos_answers_here() {
        // Never panics, and never says "another app" by returning an empty
        // name. Which app is in front depends on where the tests run, and a
        // CI runner may have nothing in front at all, so only the shape is
        // checked.
        let front = frontmost_app();
        if cfg!(target_os = "macos") {
            println!("in front: {front:?}");
            assert!(front.as_deref().is_none_or(|id| !id.is_empty()));
        } else {
            assert_eq!(front, None);
        }
    }

    #[test]
    fn every_permission_has_plain_english_copy() {
        for p in Permission::required_here() {
            let r = p.reason();
            assert!(r.ends_with('.'), "{r:?} should be a sentence");
            assert!(
                r.split_whitespace().count() <= 8,
                "{r:?} is too long for a card"
            );
        }
    }
}
