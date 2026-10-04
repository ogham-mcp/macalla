// SPDX-License-Identifier: MIT
//! Apple's on-device translation, through the Translation framework's
//! `TranslationSession` (macOS 26).
//!
//! **On this Mac only.** Apple documents that every `TranslationSession`
//! translation is processed on the device, and a run with the network
//! unplugged translated a whole meeting the same as one online. The API can
//! neither report nor force that, so it rests on Apple's documentation and
//! that run; no text is sent anywhere by Macalla here.
//!
//! One session per batch, with `translate(batch:)`: its answers come back one
//! at a time, about 0.7 s apart on the reference Mac with the high-fidelity
//! model, so the caller can show each as it lands and stop between them. The
//! first answer takes 1 to 3 s, which is why a session is not opened per text.
//!
//! The Swift half is `swift/MacallaTranslation.swift`, built by `build.rs`
//! under the `translate-apple` feature. Without the feature, off macOS, or on
//! an SDK older than macOS 26, this still compiles and reports
//! [`Unavailable::BackendUnavailable`].

use std::time::Duration;

use macalla_core::translate::{Flow, TranslateError, Translator, Unavailable};

pub const BACKEND_ID: &str = "apple-translation";

/// The backend. Holds nothing: the system keeps the models, and a session is
/// opened per batch.
#[derive(Debug, Default, Clone, Copy)]
pub struct AppleTranslation;

impl AppleTranslation {
    pub fn new() -> Self {
        Self
    }
}

/// The bridge's codes, as `swift/MacallaTranslation.swift` documents them.
const READY: i32 = 0;
const NOT_INSTALLED: i32 = 1;
const UNSUPPORTED: i32 = 2;
const STOPPED: i32 = 4;
const TIMED_OUT: i32 = 6;

/// How long one batch may take in all, before the bridge cancels it: a fixed
/// allowance for opening the session and the first answer, which took 1 to
/// 3 s on the reference Mac, and one per text, which took about 0.7 s each.
/// Generous on purpose: the limit is there for a service that has stopped
/// answering, not for a slow Mac.
const BATCH_ALLOWANCE: Duration = Duration::from_secs(30);
const PER_TEXT_ALLOWANCE: Duration = Duration::from_secs(10);

/// The whole time a batch of `count` texts may take ([`BATCH_ALLOWANCE`]
/// plus [`PER_TEXT_ALLOWANCE`] for each), in the milliseconds the bridge
/// takes, capped at what it can carry.
#[cfg_attr(not(translate_apple), allow(dead_code))]
fn time_limit_ms(count: usize) -> u32 {
    let per_text = u32::try_from(count)
        .ok()
        .and_then(|count| PER_TEXT_ALLOWANCE.checked_mul(count));
    per_text
        .and_then(|per_text| BATCH_ALLOWANCE.checked_add(per_text))
        .and_then(|limit| u32::try_from(limit.as_millis()).ok())
        .unwrap_or(u32::MAX)
}

/// Serve the main thread's run loop until `done` is set, for a program whose
/// main thread has no event loop of its own, such as the CLI, while this
/// backend runs on another thread. Call it on the main thread.
///
/// Every call to this backend blocks its own thread, and the Translation
/// framework finishes its work on the main queue: with the main thread
/// blocked, or never serving its run loop, it never answers (found on
/// 2026-09-27; a status that answered in 40 ms with the run loop served was
/// still waiting after 60 s without). The desktop app's main thread runs its
/// event loop and needs nothing more, but must never call this backend
/// itself: that thread blocked is the same deadlock. Without the backend this
/// only waits.
pub fn serve_main_thread_until(done: &std::sync::atomic::AtomicBool) {
    #[cfg(translate_apple)]
    {
        extern "C" fn is_done(context: *mut std::ffi::c_void) -> bool {
            // SAFETY: `context` is the `AtomicBool` borrowed below, alive for
            // the whole call.
            let done = unsafe { &*context.cast::<std::sync::atomic::AtomicBool>() };
            done.load(std::sync::atomic::Ordering::Acquire)
        }
        // SAFETY: `done` outlives the call, which reads it only through
        // `is_done` on this thread.
        unsafe {
            ffi::macalla_tr_serve_main(std::ptr::from_ref(done).cast_mut().cast(), is_done);
        }
    }
    #[cfg(not(translate_apple))]
    while !done.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// A pair's status code from the bridge, as a typed reason. Pure, so it is
/// tested without the device.
#[cfg_attr(not(translate_apple), allow(dead_code))]
fn availability_of(code: i32, source: &str, target: &str) -> Result<(), Unavailable> {
    match code {
        READY => Ok(()),
        NOT_INSTALLED => Err(Unavailable::NotInstalled {
            source: source.to_string(),
            target: target.to_string(),
        }),
        UNSUPPORTED => Err(Unavailable::UnsupportedPair {
            source: source.to_string(),
            target: target.to_string(),
        }),
        // The system did not answer, or answered with a status newer than
        // this build knows.
        _ => Err(Unavailable::BackendUnavailable),
    }
}

/// A translate call's code from the bridge, as its result. `asked_to_stop`
/// is whether the caller's `each` said [`Flow::Stop`]: a stop the caller
/// asked for is not a failure, and the core says it was stopped. A
/// cancellation nobody here asked for is. Pure, so it is tested without the
/// device.
#[cfg_attr(not(translate_apple), allow(dead_code))]
fn outcome_of(
    code: i32,
    asked_to_stop: bool,
    message: Option<String>,
    source: &str,
    target: &str,
) -> Result<(), TranslateError> {
    match code {
        READY => Ok(()),
        STOPPED if asked_to_stop => Ok(()),
        STOPPED => Err(failure("the system cancelled the translation")),
        TIMED_OUT => Err(failure(message.map_or_else(
            || "the translation ran out of time".to_string(),
            |message| format!("the translation ran out of time: {message}"),
        ))),
        NOT_INSTALLED | UNSUPPORTED => availability_of(code, source, target).map_err(Into::into),
        _ => Err(failure(message.unwrap_or_else(|| {
            "the translation failed and gave no reason".into()
        }))),
    }
}

fn failure(message: impl Into<String>) -> TranslateError {
    TranslateError::Failed {
        backend: BACKEND_ID,
        message: message.into(),
    }
}

#[cfg(translate_apple)]
mod ffi {
    use std::ffi::{c_char, c_void, CStr};

    pub type Each = extern "C" fn(*mut c_void, u32, *const c_char) -> bool;

    extern "C" {
        pub fn macalla_tr_status(source: *const c_char, target: *const c_char) -> i32;
        pub fn macalla_tr_languages() -> *mut c_char;
        pub fn macalla_tr_translate(
            source: *const c_char,
            target: *const c_char,
            texts: *const *const c_char,
            count: u32,
            limit_ms: u32,
            context: *mut c_void,
            each: Each,
            error_out: *mut *mut c_char,
        ) -> i32;
        pub fn macalla_tr_string_free(p: *mut c_char);
        pub fn macalla_tr_serve_main(
            context: *mut c_void,
            done: extern "C" fn(*mut c_void) -> bool,
        );
    }

    /// Take ownership of a string Swift allocated, copy it, and free it.
    ///
    /// # Safety
    /// `p` must be a pointer returned by the Swift side, or null.
    pub unsafe fn take_string(p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let owned = CStr::from_ptr(p).to_string_lossy().into_owned();
        macalla_tr_string_free(p);
        Some(owned)
    }
}

/// What the bridge's callback reaches through its context pointer: the
/// caller's `each`, whether it asked to stop, and a panic from it, kept to be
/// raised again once the bridge has returned rather than unwound through
/// Swift.
#[cfg(translate_apple)]
struct Sink<'a> {
    each: &'a mut dyn FnMut(usize, String) -> Flow,
    asked_to_stop: bool,
    panic: Option<Box<dyn std::any::Any + Send>>,
}

/// The bridge's callback, called on the thread that called
/// `macalla_tr_translate`, never on another.
#[cfg(translate_apple)]
extern "C" fn each_answer(
    context: *mut std::ffi::c_void,
    index: u32,
    text: *const std::ffi::c_char,
) -> bool {
    // SAFETY: `context` is the `Sink` `translate` passed in, alive and not
    // otherwise borrowed while the bridge runs; `text` is a C string the
    // bridge keeps alive for this call.
    let sink = unsafe { &mut *context.cast::<Sink<'_>>() };
    let text = unsafe { std::ffi::CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned();
    let flow = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        (sink.each)(index as usize, text)
    }));
    match flow {
        Ok(Flow::Continue) => true,
        Ok(Flow::Stop) => {
            sink.asked_to_stop = true;
            false
        }
        Err(panic) => {
            sink.panic = Some(panic);
            sink.asked_to_stop = true;
            false
        }
    }
}

impl Translator for AppleTranslation {
    fn id(&self) -> &'static str {
        BACKEND_ID
    }

    fn languages(&self) -> Vec<String> {
        #[cfg(translate_apple)]
        {
            // SAFETY: no arguments; returns a heap string Swift allocated, or null.
            let listed = unsafe { ffi::take_string(ffi::macalla_tr_languages()) };
            listed
                .map(|codes| {
                    codes
                        .lines()
                        .filter(|code| !code.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        }
        #[cfg(not(translate_apple))]
        {
            Vec::new()
        }
    }

    fn availability(&self, source: &str, target: &str) -> Result<(), Unavailable> {
        #[cfg(translate_apple)]
        {
            use std::ffi::CString;
            let (Ok(c_source), Ok(c_target)) = (CString::new(source), CString::new(target)) else {
                return Err(Unavailable::UnsupportedPair {
                    source: source.to_string(),
                    target: target.to_string(),
                });
            };
            // SAFETY: both strings outlive the call.
            let code = unsafe { ffi::macalla_tr_status(c_source.as_ptr(), c_target.as_ptr()) };
            availability_of(code, source, target)
        }
        #[cfg(not(translate_apple))]
        {
            let _ = (source, target);
            Err(Unavailable::BackendUnavailable)
        }
    }

    fn translate(
        &self,
        source: &str,
        target: &str,
        texts: &[String],
        each: &mut dyn FnMut(usize, String) -> Flow,
    ) -> Result<(), TranslateError> {
        #[cfg(translate_apple)]
        {
            use std::ffi::CString;
            let c_source =
                CString::new(source).map_err(|_| failure("the source language has a null byte"))?;
            let c_target =
                CString::new(target).map_err(|_| failure("the target language has a null byte"))?;
            let c_texts = texts
                .iter()
                .map(|text| CString::new(text.as_str()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| failure("a text contains a null byte"))?;
            let pointers: Vec<*const std::ffi::c_char> =
                c_texts.iter().map(|text| text.as_ptr()).collect();
            let count = u32::try_from(pointers.len())
                .map_err(|_| failure("too many texts in one batch"))?;
            let mut sink = Sink {
                each,
                asked_to_stop: false,
                panic: None,
            };
            let mut error: *mut std::ffi::c_char = std::ptr::null_mut();
            // SAFETY: every string, the pointer array and `sink` outlive the
            // call, which blocks until it returns and calls `each_answer` only
            // on this thread; `error` is a valid out pointer.
            let code = unsafe {
                ffi::macalla_tr_translate(
                    c_source.as_ptr(),
                    c_target.as_ptr(),
                    pointers.as_ptr(),
                    count,
                    time_limit_ms(texts.len()),
                    std::ptr::addr_of_mut!(sink).cast(),
                    each_answer,
                    &mut error,
                )
            };
            // SAFETY: Swift returns a heap string it allocated, or null.
            let message = unsafe { ffi::take_string(error) };
            if let Some(panic) = sink.panic.take() {
                std::panic::resume_unwind(panic);
            }
            let result = outcome_of(code, sink.asked_to_stop, message, source, target);
            if let Err(error) = &result {
                tracing::debug!(%error, "Apple translation did not finish");
            }
            result
        }
        #[cfg(not(translate_apple))]
        {
            let _ = (source, target, texts, each);
            Err(Unavailable::BackendUnavailable.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_status_code_is_its_typed_reason() {
        assert_eq!(availability_of(0, "en", "de"), Ok(()));
        assert_eq!(
            availability_of(1, "en", "de"),
            Err(Unavailable::NotInstalled {
                source: "en".into(),
                target: "de".into()
            })
        );
        assert_eq!(
            availability_of(2, "en", "ga"),
            Err(Unavailable::UnsupportedPair {
                source: "en".into(),
                target: "ga".into()
            })
        );
        assert_eq!(
            availability_of(5, "en", "de"),
            Err(Unavailable::BackendUnavailable)
        );
    }

    #[test]
    fn a_stop_asked_for_is_not_a_failure_and_one_nobody_asked_for_is() {
        assert_eq!(outcome_of(4, true, None, "en", "de"), Ok(()));
        assert!(matches!(
            outcome_of(4, false, None, "en", "de"),
            Err(TranslateError::Failed { .. })
        ));
    }

    #[test]
    fn a_batch_may_take_thirty_seconds_and_ten_more_per_text() {
        assert_eq!(time_limit_ms(0), 30_000);
        assert_eq!(time_limit_ms(1), 40_000);
        assert_eq!(time_limit_ms(30), 330_000);
        assert_eq!(time_limit_ms(usize::MAX), u32::MAX);
    }

    /// The bridge's timeout, as the core and each app see it: a failure with
    /// a message the log can read, which the apps word as "didn't finish".
    #[test]
    fn running_out_of_time_is_a_failure_the_log_can_read() {
        let outcome = outcome_of(
            6,
            false,
            Some("no end to the batch within 40000 ms: 0 of 1 answers had come".into()),
            "en",
            "de",
        );
        assert_eq!(
            outcome,
            Err(TranslateError::Failed {
                backend: BACKEND_ID,
                message: "the translation ran out of time: no end to the batch within 40000 ms: \
                          0 of 1 answers had come"
                    .into()
            })
        );
        assert!(matches!(
            outcome_of(6, false, None, "en", "de"),
            Err(TranslateError::Failed { message, .. }) if message.contains("ran out of time")
        ));
    }

    #[test]
    fn a_thrown_reason_keeps_its_pair_and_a_failure_keeps_its_message() {
        assert_eq!(
            outcome_of(1, false, None, "en", "de"),
            Err(TranslateError::Unavailable(Unavailable::NotInstalled {
                source: "en".into(),
                target: "de".into()
            }))
        );
        assert_eq!(
            outcome_of(2, false, None, "en", "ga"),
            Err(TranslateError::Unavailable(Unavailable::UnsupportedPair {
                source: "en".into(),
                target: "ga".into()
            }))
        );
        assert_eq!(
            outcome_of(5, false, Some("internalError".into()), "en", "de"),
            Err(TranslateError::Failed {
                backend: BACKEND_ID,
                message: "internalError".into()
            })
        );
    }

    #[cfg(not(translate_apple))]
    #[test]
    fn a_build_without_apple_translation_says_the_backend_is_unavailable() {
        let backend = AppleTranslation::new();
        assert!(backend.languages().is_empty());
        assert_eq!(
            backend.availability("en", "de"),
            Err(Unavailable::BackendUnavailable)
        );
        let error = backend
            .translate("en", "de", &["hello".into()], &mut |_, _| Flow::Continue)
            .unwrap_err();
        assert_eq!(
            error,
            TranslateError::Unavailable(Unavailable::BackendUnavailable)
        );
    }
}
