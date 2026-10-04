// SPDX-License-Identifier: MIT
//! Apple Intelligence, through FoundationModels: the first enhancement
//! provider on a Mac that has it.
//!
//! Nothing to download and no model memory held by Macalla. It needs macOS 26,
//! an eligible Mac, and Apple Intelligence switched on, which a person may have
//! declined at setup and can turn off at any time, so availability is asked
//! before every request. Its speed has been measured on one machine only, the
//! M1 Pro reference machine: 853 ms p50 guided and 511 ms unguided
//! (`docs/benchmarks.md`, Spike B, with an earlier prompt). Newer Apple
//! silicon is unmeasured, which is why the provider times itself.
//!
//! The Swift half is `swift/MacallaIntelligence.swift`, built by `build.rs`
//! under the `apple-intelligence` feature. Without the feature, off macOS, or
//! on an SDK older than macOS 26, this still compiles and reports
//! [`Availability::Unsupported`].

use std::sync::Mutex;
use std::time::{Duration, Instant};

use macalla_core::block::Block;
use macalla_core::enhance::{self, Enhancer, EnhancerCaps, RecentTimes};
use macalla_core::text_model::{Shape, ShapedModel, TextModel};
use macalla_core::{Error, Result};

pub const BACKEND_ID: &str = "apple-intelligence";

/// The default request shape for [`AppleIntelligence::new`]: plain text, not
/// the one-field schema. Dictation's `cleanup_enhancers` and `macalla
/// enhance` both build the choice from this constant, so the library, the
/// app and the CLI cannot disagree about the default. Never behind a feature
/// or target `cfg`: every build that reads it, including one without
/// `apple-intelligence` compiled in, must see the same value. If this ever
/// becomes `true`, `macalla enhance` needs a working way to ask for plain
/// text again: its `--unguided` flag is a no-op while this is `false`.
pub const DEFAULT_GUIDED: bool = false;

/// Cleanup's limit for one block, and the default for
/// [`AppleIntelligence::new`]. Dictation's 5 s wait before the paste falls
/// inside this limit: an answer within 5 s is pasted; one that answers
/// later than that but still within this limit only reaches History; past
/// this limit the request is cancelled and History keeps what was pasted. A
/// caller that asks for a longer reply, such as a meeting summary, sets its
/// own with [`AppleIntelligence::answering_within`]. Not measured.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Whether Apple Intelligence can take a request now, and if not, why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    /// This Mac cannot run Apple Intelligence.
    DeviceNotEligible,
    /// It is switched off in System Settings.
    NotEnabled,
    /// It is on, but its model is not ready yet.
    ModelNotReady,
    /// A reason newer than this build of Macalla.
    UnknownReason,
    /// This build has no Apple Intelligence: not macOS, the feature is off,
    /// or it was built with an SDK older than macOS 26.
    Unsupported,
}

impl Availability {
    /// Asks the system. Cheap, and the answer can change between calls.
    pub fn now() -> Self {
        #[cfg(apple_intelligence)]
        {
            // SAFETY: no arguments, returns a plain integer.
            match unsafe { ffi::macalla_ai_availability() } {
                0 => Self::Available,
                1 => Self::DeviceNotEligible,
                2 => Self::NotEnabled,
                3 => Self::ModelNotReady,
                _ => Self::UnknownReason,
            }
        }
        #[cfg(not(apple_intelligence))]
        {
            Self::Unsupported
        }
    }

    /// A sentence for the person saying why it cannot be used, or `None` when
    /// it can.
    pub fn explain(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::DeviceNotEligible => Some("This Mac can't run Apple Intelligence."),
            Self::NotEnabled => Some("Apple Intelligence is turned off in System Settings."),
            Self::ModelNotReady => {
                Some("Apple Intelligence is still getting its model ready. Try again later.")
            }
            Self::UnknownReason => Some("Apple Intelligence isn't available right now."),
            Self::Unsupported => Some("This build of Macalla doesn't include Apple Intelligence."),
        }
    }
}

#[cfg(apple_intelligence)]
mod ffi {
    use std::ffi::{c_char, CStr};

    extern "C" {
        pub fn macalla_ai_availability() -> i32;
        pub fn macalla_ai_respond(
            instructions: *const c_char,
            text: *const c_char,
            guided: bool,
            timeout_ms: u32,
            declined_out: *mut bool,
            error_out: *mut *mut c_char,
        ) -> *mut c_char;
        pub fn macalla_ai_respond_shaped(
            instructions: *const c_char,
            text: *const c_char,
            shape: *const c_char,
            timeout_ms: u32,
            declined_out: *mut bool,
            error_out: *mut *mut c_char,
        ) -> *mut c_char;
        pub fn macalla_ai_prewarm(instructions: *const c_char);
        pub fn macalla_ai_string_free(p: *mut c_char);
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
        macalla_ai_string_free(p);
        Some(owned)
    }
}

fn failure(message: impl Into<String>) -> Error {
    Error::Enhance {
        backend: BACKEND_ID,
        message: message.into(),
    }
}

/// What a decline should turn into: [`Enhancer::enhance`] and
/// [`macalla_core::text_model::TextModel::complete`] disagree about this,
/// and neither port's policy belongs in the other.
///
/// [`Enhancer`]'s contract keeps Tier 0's words when Tier 1 has nothing to
/// add, so a decline there is not a failure. [`TextModel`]'s contract is
/// "the reply is exactly what the model returned" -- echoing the caller's
/// own input back as if it were an answer would be silently wrong there, so
/// a decline is a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnDecline {
    /// Return `text` unchanged, so a rejected block keeps its Tier 0 text.
    ReturnInput,
    /// Return an error naming the decline.
    Fail,
}

/// What the Swift bridge reported for one call, decoupled from FFI and
/// pointer handling so [`resolve`] -- the decision below -- is testable
/// without the device. [`respond`] builds this from the real call; a test
/// builds it directly.
#[cfg(apple_intelligence)]
enum BridgeOutcome {
    /// The model answered.
    Replied(String),
    /// The model's safety checks refused the text. They refused ordinary
    /// dictation on 2026-09-11.
    Declined,
    /// The call itself failed, with whatever message the bridge gave, or a
    /// fallback when it gave none.
    Failed(String),
}

/// Turns what the bridge reported into this call's result, purely: no FFI,
/// so a test can drive it with a synthetic [`BridgeOutcome`] instead of the
/// on-device model.
#[cfg(apple_intelligence)]
fn resolve(outcome: BridgeOutcome, text: &str, on_decline: OnDecline) -> Result<String> {
    match outcome {
        BridgeOutcome::Replied(reply) => Ok(reply),
        BridgeOutcome::Declined => match on_decline {
            OnDecline::ReturnInput => {
                tracing::debug!("Apple Intelligence declined a block; it keeps its Tier 0 text");
                Ok(text.to_string())
            }
            OnDecline::Fail => Err(failure("Apple Intelligence declined to answer")),
        },
        BridgeOutcome::Failed(message) => Err(failure(message)),
    }
}

/// One request to the on-device model, in a fresh session. `on_decline`
/// decides what a safety-check refusal turns into -- see [`OnDecline`].
#[cfg(apple_intelligence)]
fn respond(
    instructions: &str,
    text: &str,
    guided: bool,
    on_decline: OnDecline,
    time_limit: Duration,
) -> Result<String> {
    use std::ffi::CString;

    let c_instructions =
        CString::new(instructions).map_err(|_| failure("the prompt contains a null byte"))?;
    let c_text = CString::new(text).map_err(|_| failure("the text contains a null byte"))?;
    let timeout_ms = u32::try_from(time_limit.as_millis()).unwrap_or(u32::MAX);
    let mut declined = false;
    let mut error: *mut std::ffi::c_char = std::ptr::null_mut();
    // SAFETY: both strings outlive the call, and `declined` and `error` are
    // valid out pointers.
    let reply = unsafe {
        ffi::macalla_ai_respond(
            c_instructions.as_ptr(),
            c_text.as_ptr(),
            guided,
            timeout_ms,
            &mut declined,
            &mut error,
        )
    };
    // SAFETY: Swift returns heap strings it allocated, or null.
    let outcome = unsafe { outcome_of(reply, declined, error) };
    resolve(outcome, text, on_decline)
}

/// What the bridge reported, from its return value and out parameters.
///
/// # Safety
/// `reply` and `error` must be strings the Swift side allocated, or null.
#[cfg(apple_intelligence)]
unsafe fn outcome_of(
    reply: *mut std::ffi::c_char,
    declined: bool,
    error: *mut std::ffi::c_char,
) -> BridgeOutcome {
    if let Some(reply) = ffi::take_string(reply) {
        BridgeOutcome::Replied(reply)
    } else if declined {
        BridgeOutcome::Declined
    } else {
        let message = ffi::take_string(error);
        BridgeOutcome::Failed(message.unwrap_or_else(|| "Apple Intelligence gave no answer".into()))
    }
}

/// One request whose reply is constrained to `shape`, in a fresh session: the
/// reply's JSON. A decline fails the call, as for [`TextModel::complete`].
#[cfg(apple_intelligence)]
fn respond_shaped(
    instructions: &str,
    text: &str,
    shape: &Shape,
    time_limit: Duration,
) -> Result<String> {
    use std::ffi::CString;

    let c_instructions =
        CString::new(instructions).map_err(|_| failure("the prompt contains a null byte"))?;
    let c_text = CString::new(text).map_err(|_| failure("the text contains a null byte"))?;
    let c_shape = CString::new(shape_json(shape).to_string())
        .map_err(|_| failure("the reply shape contains a null byte"))?;
    let timeout_ms = u32::try_from(time_limit.as_millis()).unwrap_or(u32::MAX);
    let mut declined = false;
    let mut error: *mut std::ffi::c_char = std::ptr::null_mut();
    // SAFETY: the three strings outlive the call, and `declined` and `error`
    // are valid out pointers.
    let reply = unsafe {
        ffi::macalla_ai_respond_shaped(
            c_instructions.as_ptr(),
            c_text.as_ptr(),
            c_shape.as_ptr(),
            timeout_ms,
            &mut declined,
            &mut error,
        )
    };
    // SAFETY: Swift returns heap strings it allocated, or null.
    let outcome = unsafe { outcome_of(reply, declined, error) };
    resolve(outcome, text, OnDecline::Fail)
}

#[cfg(not(apple_intelligence))]
fn respond(
    _instructions: &str,
    _text: &str,
    _guided: bool,
    _on_decline: OnDecline,
    _time_limit: Duration,
) -> Result<String> {
    Err(failure(
        Availability::Unsupported.explain().unwrap_or_default(),
    ))
}

#[cfg(not(apple_intelligence))]
fn respond_shaped(
    _instructions: &str,
    _text: &str,
    _shape: &Shape,
    _time_limit: Duration,
) -> Result<String> {
    Err(failure(
        Availability::Unsupported.explain().unwrap_or_default(),
    ))
}

/// The provider. Holds no session: FoundationModels needs a fresh one per
/// request, so the Swift side opens one each time.
pub struct AppleIntelligence {
    prompt: String,
    guided: bool,
    times: Mutex<RecentTimes>,
    /// How long one request waits for its reply: [`TIMEOUT`] unless
    /// [`AppleIntelligence::answering_within`] says otherwise.
    time_limit: Duration,
}

impl Default for AppleIntelligence {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleIntelligence {
    /// Plain text by default, not the one-field schema
    /// ([`AppleIntelligence::guided`] turns that back on for comparison
    /// runs). Scored on the M1 Pro (16 GB, macOS 26.7, build 25G229) with
    /// `bench/enhance_ab_score.py` on 2026-09-16: schema-first got 0 of 20
    /// fixtures exactly punctuated (18 left as Tier 0: 17 returned exactly as
    /// sent, and 1 rejected by the guard) against 12 of 20 plain. The same
    /// comparison on 2026-09-11 (macOS 26.6.2) changed 4 of 20 fixtures
    /// schema-first, capitals only, against 17 plain (`docs/benchmarks.md`).
    pub fn new() -> Self {
        Self {
            prompt: enhance::DEFAULT_PROMPT.to_string(),
            guided: DEFAULT_GUIDED,
            times: Mutex::default(),
            time_limit: TIMEOUT,
        }
    }

    /// Ask for a one-field schema first (guided generation), for comparison
    /// runs. Slower: on the M1 Pro, plain text was 70 ms faster at p50 on
    /// 2026-09-11 (340 ms in Spike B, with an earlier prompt) and scored
    /// higher again on 2026-09-16 (`docs/benchmarks.md`); the guard checks
    /// the reply either way.
    pub fn guided(mut self) -> Self {
        self.guided = true;
        self
    }

    /// Use the person's own instructions instead of the default.
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = prompt.into();
        self
    }

    /// Wait up to `limit` for each reply instead of cleanup's limit. A
    /// summary asks for a longer reply than one dictated segment.
    pub fn answering_within(mut self, limit: Duration) -> Self {
        self.time_limit = limit;
        self
    }

    /// How long a request waits for its reply.
    pub fn time_limit(&self) -> Duration {
        self.time_limit
    }
}

impl Enhancer for AppleIntelligence {
    fn caps(&self) -> EnhancerCaps {
        EnhancerCaps {
            id: BACKEND_ID,
            typical_ms: self.times.lock().ok().and_then(|t| t.median_ms()),
            constrained_output: self.guided,
        }
    }

    fn is_available(&self) -> bool {
        Availability::now() == Availability::Available
    }

    /// Asks the system to load the model, as another dictation app does when
    /// cleanup is switched on. Does nothing when Apple Intelligence is not available.
    ///
    /// On the M1 Pro on 2026-09-11, the first request after 90 s idle took
    /// 7.8 s three times without it, and 3.8 s twice with it; the third
    /// prewarmed request timed out (`docs/benchmarks.md`). The session calls
    /// it when it arms the microphone, so the model is loading before the
    /// key goes down, and again whenever Tier 1 is built
    /// (`Session::replace_cleanup`), so a freshly chosen or swapped-to
    /// choice is warm too, not only the one a session opened with.
    fn prewarm(&self) {
        #[cfg(apple_intelligence)]
        {
            if Availability::now() != Availability::Available {
                return;
            }
            let instructions = apple_instructions(&self.prompt, &[]);
            if let Ok(instructions) = std::ffi::CString::new(instructions) {
                // SAFETY: the string outlives the call, which returns at once.
                unsafe { ffi::macalla_ai_prewarm(instructions.as_ptr()) };
            }
        }
    }

    fn enhance(&self, blocks: &[Block], terms: &[String]) -> Result<Vec<Block>> {
        if let Some(reason) = Availability::now().explain() {
            return Err(failure(reason));
        }
        let instructions = apple_instructions(&self.prompt, terms);
        let guard = enhance::Guard::for_prompt(&self.prompt);
        let started = Instant::now();
        let out = enhance::rewrite_each(blocks, guard, |text| {
            respond(
                &instructions,
                text,
                self.guided,
                OnDecline::ReturnInput,
                self.time_limit,
            )
        })?;
        if let Ok(mut times) = self.times.lock() {
            times.record(started.elapsed());
        }
        Ok(out)
    }
}

impl TextModel for AppleIntelligence {
    /// Same per-call check as [`Enhancer::is_available`]: Apple Intelligence
    /// is a setting the user can turn off at any moment.
    fn ready(&self) -> bool {
        Availability::now() == Availability::Available
    }

    /// Always the plain-text path, even on a provider built with
    /// [`AppleIntelligence::guided`], whose `enhance()` asks for the one-field
    /// schema first: a free-text reply has no one field to constrain it to.
    /// A decline fails the call -- see [`OnDecline`] -- rather than silently
    /// handing back `text` as if it were the model's reply.
    fn complete(&self, instructions: &str, text: &str) -> Result<String> {
        if let Some(reason) = Availability::now().explain() {
            return Err(failure(reason));
        }
        respond(instructions, text, false, OnDecline::Fail, self.time_limit)
    }
}

impl ShapedModel for AppleIntelligence {
    /// Same per-call check as [`TextModel::ready`].
    fn ready(&self) -> bool {
        Availability::now() == Availability::Available
    }

    /// Guided generation: the reply is constrained to `shape` while it is
    /// generated, and comes back as its JSON. A decline, or a shape the bridge
    /// cannot build into a schema, fails the call. There is no plain-text
    /// retry: a summary asked for as plain text left out every quote when
    /// measured.
    fn complete_shaped(&self, instructions: &str, text: &str, shape: &Shape) -> Result<String> {
        if let Some(reason) = Availability::now().explain() {
            return Err(failure(reason));
        }
        respond_shaped(instructions, text, shape, self.time_limit)
    }
}

/// A [`Shape`] as the JSON the Swift bridge reads (`ShapeReader` in
/// `swift/MacallaIntelligence.swift`).
#[cfg_attr(not(apple_intelligence), allow(dead_code))]
fn shape_json(shape: &Shape) -> serde_json::Value {
    use serde_json::json;
    match shape {
        Shape::Text => json!({ "type": "text" }),
        Shape::OneOf { name, choices } => {
            json!({ "type": "one_of", "name": name, "choices": choices })
        }
        Shape::List(of) => json!({ "type": "list", "of": shape_json(of) }),
        Shape::Record { name, fields } => json!({
            "type": "record",
            "name": name,
            "fields": fields
                .iter()
                .map(|field| json!({
                    "name": field.name,
                    "description": field.description,
                    "shape": shape_json(&field.shape),
                }))
                .collect::<Vec<_>>(),
        }),
    }
}

/// The instructions Apple Intelligence gets: the prompt alone, never the
/// dictionary's known-terms line that the other providers get.
///
/// Measured on the M1 Pro on 2026-09-11 (`docs/benchmarks.md`): with the line,
/// Apple Intelligence returned the line itself as its reply to one of the 20
/// fixtures in every run, and got 11 of 20 right; without it, 12 of 20, half
/// the rejected replies, and a p50 about 40 ms lower. Rewording the line as a
/// spelling reference also stopped the echo and also scored 12, and was not
/// taken because leaving it out is simpler. The dictionary has already
/// corrected the text before Apple Intelligence sees it, so the spellings are
/// there either way. The prewarm uses the same instructions as a request.
fn apple_instructions(prompt: &str, _terms: &[String]) -> String {
    enhance::instructions(prompt, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the default request shape without touching the on-device model:
    /// `constrained_output` comes straight from `self.guided`, so this holds
    /// whatever Apple Intelligence reports on the machine running the test
    /// (this Mac has it on; CI runners may not).
    #[test]
    fn new_asks_for_plain_text_and_guided_asks_for_the_schema() {
        assert!(!AppleIntelligence::new().caps().constrained_output);
        assert!(AppleIntelligence::new().guided().caps().constrained_output);
    }

    #[test]
    fn a_request_waits_cleanups_limit_unless_asked_to_wait_longer() {
        assert_eq!(
            AppleIntelligence::new().time_limit(),
            Duration::from_secs(10)
        );
        let summary = AppleIntelligence::new().answering_within(Duration::from_secs(90));
        assert_eq!(summary.time_limit(), Duration::from_secs(90));
    }

    #[test]
    fn the_dictionary_terms_never_reach_apple_intelligence() {
        let terms = vec!["Terragrunt".to_string(), "kubectl".to_string()];
        let sent = apple_instructions(enhance::DEFAULT_PROMPT, &terms);
        assert_eq!(sent, enhance::DEFAULT_PROMPT);
        assert!(!sent.contains("Known terms"), "{sent}");
        assert!(!sent.contains("Terragrunt"), "{sent}");
    }

    #[test]
    fn every_reason_it_cannot_be_used_has_a_sentence_and_available_has_none() {
        use Availability::*;
        assert_eq!(Available.explain(), None);
        for reason in [
            DeviceNotEligible,
            NotEnabled,
            ModelNotReady,
            UnknownReason,
            Unsupported,
        ] {
            assert!(
                reason.explain().is_some_and(|s| s.ends_with('.')),
                "{reason:?}"
            );
        }
    }

    // -------------------------------------------------------------- resolve
    //
    // Pure, so the decline policy is testable without the on-device model:
    // no #[ignore], and gated exactly like resolve() itself, which only
    // exists under the apple-intelligence feature (respond()'s other half
    // is the FFI call these outcomes stand in for).

    #[cfg(apple_intelligence)]
    #[test]
    fn a_reply_wins_whatever_the_decline_policy() {
        for policy in [OnDecline::ReturnInput, OnDecline::Fail] {
            assert_eq!(
                resolve(BridgeOutcome::Replied("Ship it.".into()), "ship it", policy).unwrap(),
                "Ship it."
            );
        }
    }

    #[cfg(apple_intelligence)]
    #[test]
    fn a_decline_returns_the_input_when_the_policy_says_return_input() {
        let out = resolve(
            BridgeOutcome::Declined,
            "review the spec",
            OnDecline::ReturnInput,
        )
        .unwrap();
        assert_eq!(out, "review the spec");
    }

    #[cfg(apple_intelligence)]
    #[test]
    fn a_decline_fails_when_the_policy_says_fail() {
        let error =
            resolve(BridgeOutcome::Declined, "review the spec", OnDecline::Fail).unwrap_err();
        assert!(error.to_string().contains("declined to answer"), "{error}");
    }

    #[cfg(apple_intelligence)]
    #[test]
    fn a_bridge_failure_is_an_error_with_its_message_whatever_the_policy() {
        for policy in [OnDecline::ReturnInput, OnDecline::Fail] {
            let error = resolve(
                BridgeOutcome::Failed("no model loaded".into()),
                "text",
                policy,
            )
            .unwrap_err();
            assert!(error.to_string().contains("no model loaded"), "{error}");
        }
    }

    #[cfg(not(apple_intelligence))]
    #[test]
    fn a_build_without_apple_intelligence_says_so_and_does_not_enhance() {
        let provider = AppleIntelligence::new();
        assert_eq!(Availability::now(), Availability::Unsupported);
        assert!(!provider.is_available());
        let blocks = vec![Block::Paragraph {
            text: "review the spec".into(),
        }];
        let error = provider.enhance(&blocks, &[]).unwrap_err();
        assert!(error.to_string().contains("doesn't include"), "{error}");
    }

    /// Runs the on-device model, so it only asserts anything on a Mac where
    /// Apple Intelligence is on; elsewhere it says why and passes.
    ///
    /// Ignored by default: the model's time depends on what else the Mac is
    /// doing. On 2026-09-11 it passed on its own in 3 s and then timed out at
    /// 10 s inside the commit hook's full test run, which would make every
    /// commit and the self-hosted CI depend on system load.
    #[cfg(apple_intelligence)]
    #[test]
    #[ignore = "calls the on-device model; run with --ignored on a quiet Mac"]
    fn a_dictated_sentence_comes_back_with_its_words_intact_plain_and_guided() {
        let availability = Availability::now();
        if let Some(reason) = availability.explain() {
            eprintln!("skipped: {reason}");
            return;
        }
        let blocks = vec![Block::ListItem {
            text: "we need to ship the parser by friday".into(),
            ordered: true,
        }];
        for provider in [AppleIntelligence::new(), AppleIntelligence::new().guided()] {
            let out = provider.enhance(&blocks, &[]).expect("an answer");
            assert!(enhance::Guard::SameWords.accepts(&blocks, &out), "{out:?}");
            assert!(matches!(out[0], Block::ListItem { ordered: true, .. }));
            assert!(provider.caps().typical_ms.is_some());
        }
    }

    /// [`TextModel::complete`] returns free text through the plain-text
    /// path, not blocks and not the one-field schema: unlike `enhance()`,
    /// there is no guard here to keep the words intact, so this only asserts
    /// that a reply comes back and Apple Intelligence was asked for one.
    ///
    /// Ignored for the same reason as the test above: on-device timing.
    #[cfg(apple_intelligence)]
    #[test]
    #[ignore = "calls the on-device model; run with --ignored on a quiet Mac"]
    fn text_model_complete_returns_the_plain_text_reply() {
        let provider = AppleIntelligence::new();
        if let Some(reason) = Availability::now().explain() {
            eprintln!("skipped: {reason}");
            return;
        }
        let reply = TextModel::complete(
            &provider,
            "Summarise in three words.",
            "we shipped the parser today and it went well",
        )
        .expect("an answer");
        assert!(!reply.trim().is_empty());
    }

    #[test]
    fn a_shape_reaches_the_bridge_as_the_json_it_reads() {
        use macalla_core::text_model::Field;
        let point = Shape::Record {
            name: "Point".into(),
            fields: vec![Field {
                name: "quote".into(),
                description: "A quote.".into(),
                shape: Shape::Text,
            }],
        };
        let shape = Shape::Record {
            name: "Summary".into(),
            fields: vec![
                Field {
                    name: "points".into(),
                    description: "Points.".into(),
                    shape: Shape::List(Box::new(point)),
                },
                Field {
                    name: "owner".into(),
                    description: "Who.".into(),
                    shape: Shape::OneOf {
                        name: "Owner".into(),
                        choices: vec!["Aoife Byrne".into(), "none".into()],
                    },
                },
            ],
        };

        assert_eq!(
            shape_json(&shape),
            serde_json::json!({
                "type": "record",
                "name": "Summary",
                "fields": [
                    {
                        "name": "points",
                        "description": "Points.",
                        "shape": {
                            "type": "list",
                            "of": {
                                "type": "record",
                                "name": "Point",
                                "fields": [
                                    { "name": "quote", "description": "A quote.", "shape": { "type": "text" } }
                                ]
                            }
                        }
                    },
                    {
                        "name": "owner",
                        "description": "Who.",
                        "shape": { "type": "one_of", "name": "Owner", "choices": ["Aoife Byrne", "none"] }
                    }
                ]
            })
        );
    }

    #[cfg(not(apple_intelligence))]
    #[test]
    fn a_build_without_apple_intelligence_refuses_a_shaped_request() {
        let error = ShapedModel::complete_shaped(
            &AppleIntelligence::new(),
            "Answer.",
            "text",
            &Shape::Text,
        )
        .unwrap_err();
        assert!(error.to_string().contains("doesn't include"), "{error}");
    }

    /// Runs the on-device model through the shaped path: the reply is JSON
    /// holding the one field asked for, with one of its choices.
    ///
    /// Ignored for the same reason as the tests above: on-device timing.
    #[cfg(apple_intelligence)]
    #[test]
    #[ignore = "calls the on-device model; run with --ignored on a quiet Mac"]
    fn a_shaped_request_comes_back_as_json_with_the_field_asked_for() {
        use macalla_core::text_model::Field;
        if let Some(reason) = Availability::now().explain() {
            eprintln!("skipped: {reason}");
            return;
        }
        let shape = Shape::Record {
            name: "Answer".into(),
            fields: vec![Field {
                name: "colour".into(),
                description: "The colour the text names.".into(),
                shape: Shape::OneOf {
                    name: "Colour".into(),
                    choices: vec!["red".into(), "blue".into()],
                },
            }],
        };
        let provider = AppleIntelligence::new().answering_within(Duration::from_secs(60));
        let reply = ShapedModel::complete_shaped(
            &provider,
            "Answer from the text.",
            "The door is painted blue.",
            &shape,
        )
        .expect("an answer");
        let json: serde_json::Value = serde_json::from_str(&reply).expect("the reply is JSON");
        assert!(
            json["colour"] == "red" || json["colour"] == "blue",
            "{reply}"
        );
    }
}
