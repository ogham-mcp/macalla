// SPDX-License-Identifier: MIT
//! A meeting transcript file, summarised with the provider chosen for Enhancement.
//!
//! The meeting's own rules live in the core: cutting the turns into parts,
//! what the model is asked, and keeping only items whose quote is found in
//! their part ([`macalla_core::meeting_summary`]). This module is the use
//! case around them: read a transcript file into turns, build the model from
//! the cleanup choice, say where the words will go, and hand the turns to the
//! core.
//!
//! - **Files.** A WebVTT file keeps its speakers and times
//!   ([`macalla_core::vtt`]). Anything else is plain text: one turn with no
//!   speaker, so no item in its summary can have an owner.
//! - **Where the words go.** [`Summariser::destination`] and
//!   [`Summariser::reach`] use cleanup's own rules, so a caller can say where
//!   the transcript is going before anything is sent. Building sends nothing.
//! - **A pool.** Several ticked models are asked as cleanup asks them
//!   ([`crate::failover`]): in ticked order, the next taking over only when a
//!   request fails. Every part starts again from the first model.
//! - **Speaker labels.** Turns reach the core with the labels the file gave,
//!   and an owner is only confirmed there, against those labels. Nothing
//!   here decides an owner, so a later rename of a speaker can map the labels
//!   before [`Summariser::summarise`] is called.

use crate::choice::{self, CleanupReach, EnhancerChoice};
use crate::cleanup::{pool_label, provider_and_model};
use crate::failover::{Candidate, Failover, Work};
use crate::{Error, Result};
use macalla_core::meeting_summary::{self, PartProgress, PartSummary};
use macalla_core::text_model::{Shape, ShapedModel, TextModel};
use macalla_core::vtt::{self, Turn, VttError};
use macalla_platform::secrets::SecretStore;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// A transcript file, read into turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFile {
    /// Consecutive lines from one speaker, in meeting order. Never empty.
    pub turns: Vec<Turn>,
    /// Whether any turn has a speaker. A plain text file, or a VTT file with
    /// no names in it, has none, so its summary has no owners.
    pub named_speakers: bool,
    /// Blocks of a VTT file that could not be read as a cue. Always 0 for
    /// plain text.
    pub skipped_blocks: usize,
}

/// Read the transcript at `path`: WebVTT when it starts with the `WEBVTT`
/// signature, plain text otherwise. A file named `.vtt` that is not WebVTT is
/// refused: read as plain text it would lose its speakers and times without
/// saying so.
pub fn read_transcript(path: &Path) -> Result<TranscriptFile> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::TranscriptRead {
        path: path.to_path_buf(),
        source,
    })?;
    let named_vtt = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("vtt"));
    if named_vtt && matches!(vtt::parse(&text), Err(VttError::MissingHeader)) {
        return Err(Error::NotWebVtt {
            path: path.to_path_buf(),
        });
    }
    transcript_from_text(&text).ok_or_else(|| Error::EmptyTranscript {
        path: path.to_path_buf(),
    })
}

/// `None` when the text holds no words.
fn transcript_from_text(text: &str) -> Option<TranscriptFile> {
    let (turns, skipped_blocks) = match vtt::parse(text) {
        Ok(transcript) => (transcript.turns(), transcript.skipped_blocks),
        Err(VttError::MissingHeader) => (plain_text_turns(text), 0),
    };
    if turns.is_empty() {
        return None;
    }
    let named_speakers = turns.iter().any(|turn| turn.speaker.is_some());
    Some(TranscriptFile {
        turns,
        named_speakers,
        skipped_blocks,
    })
}

/// Plain text as one turn with no speaker, its whitespace collapsed. A plain
/// file says nothing about who spoke or when, so a `Name:` in it stays in the
/// words and the times are zero.
fn plain_text_turns(text: &str) -> Vec<Turn> {
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.is_empty() {
        return Vec::new();
    }
    vec![Turn {
        speaker: None,
        start_ms: 0,
        end_ms: 0,
        text: words,
    }]
}

/// The provider chosen for Enhancement, ready to summarise a meeting.
pub struct Summariser {
    asker: Asker,
    reach: CleanupReach,
    destination: String,
    /// The choices that built, as [`choice::fingerprint`] names them.
    fingerprint: String,
}

enum Asker {
    One(Box<dyn TextModel>),
    /// Apple Intelligence, asked for the three lists as a shaped reply, since
    /// asked for them as plain text it left out every quote.
    Shaped(Box<dyn ShapedModel>),
    Pool(Failover<TextMember>),
}

/// One ticked model in a summary pool, built.
struct TextMember {
    model: Box<dyn TextModel>,
    provider: String,
    model_id: String,
}

impl Candidate for TextMember {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model_id
    }
    fn ready(&self) -> bool {
        self.model.ready()
    }
}

impl Summariser {
    /// Build the model the cleanup `choices` name, with keys from `keys`, as
    /// cleanup builds them ([`crate::choice::cleanup_enhancers`] turns a stored
    /// choice into these). Building sends nothing.
    ///
    /// - No choices: refused, since cleanup is off.
    /// - None builds: refused, with every reason.
    /// - One builds: that model, with its own reach and label.
    /// - More than one builds: a pool, always remote, labelled as cleanup
    ///   labels it. A ticked model that could not start is logged and left
    ///   out. A pool only exists once two models have built, so it is never
    ///   empty.
    pub fn build(choices: &[EnhancerChoice], keys: &dyn SecretStore) -> Result<Self> {
        if choices.is_empty() {
            return Err(Error::SummaryNeedsProvider);
        }
        let mut reasons = Vec::new();
        let mut built = Vec::new();
        for picked in choices {
            match choice::text_model_for(picked, keys) {
                Ok(model) => built.push((picked, model)),
                Err(e) => reasons.push(e.to_string()),
            }
        }
        if !built.is_empty() && !reasons.is_empty() {
            tracing::info!(
                skipped = %reasons.join("; "),
                "a summary provider could not start"
            );
        }
        // Fingerprinted by what built, not by what was chosen: a pool member
        // that gains or loses its key changes where the transcript can go.
        let built_choices: Vec<EnhancerChoice> =
            built.iter().map(|(picked, _)| (*picked).clone()).collect();
        if built.len() > 1 {
            // A pool is asked for plain text only. That is safe because a
            // cleanup pool is built from cloud models alone
            // (`choice::cleanup_enhancers`), so Apple Intelligence, which must
            // be asked for a shaped reply, never enters one.
            debug_assert!(
                built
                    .iter()
                    .all(|(_, model)| !matches!(model, choice::TextModelKind::Apple(_))),
                "Apple Intelligence was built into a summary pool, which asks for plain text"
            );
            let destination = pool_label(built.iter().map(|(picked, _)| picked.destination()));
            let members = built
                .into_iter()
                .map(|(picked, model)| {
                    let (provider, model_id) = provider_and_model(picked);
                    TextMember {
                        model: model.into_box(),
                        provider,
                        model_id,
                    }
                })
                .collect();
            return Ok(Self {
                asker: Asker::Pool(Failover::doing(Work::Summary, members)),
                reach: CleanupReach::Remote,
                destination,
                fingerprint: choice::fingerprint(&built_choices),
            });
        }
        match built.pop() {
            Some((picked, model)) => Ok(Self {
                asker: match model {
                    choice::TextModelKind::Apple(apple) => Asker::Shaped(Box::new(apple)),
                    other => Asker::One(other.into_box()),
                },
                reach: picked.reach(),
                destination: picked.destination().to_string(),
                fingerprint: choice::fingerprint(&built_choices),
            }),
            None => Err(Error::SummaryProviderUnavailable(reasons.join("; "))),
        }
    }

    /// Where the transcript goes, in the words cleanup's label uses.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    /// The choices that built, as [`choice::fingerprint`] names them,
    /// for a caller that showed [`Summariser::destination`] to pass back, so a
    /// changed choice is refused even when its label reads the same.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Whether the transcript stays on this machine.
    pub fn reach(&self) -> CleanupReach {
        self.reach
    }

    /// Summarise `turns`, part by part, in meeting order. Blocks until every
    /// part has an answer or an error; a caller with a window runs it on a
    /// worker.
    ///
    /// `stop`, once set, sends nothing more, and a pool asks no further model
    /// for the part in hand. A summary stopped before its last reply came back
    /// is [`Error::Stopped`], even when that reply still arrives: the stop was
    /// asked for while the part was out. Only a stop that lands after every
    /// reply came back keeps every part.
    ///
    /// `on_progress` hears each part as it starts and as it finishes, until
    /// `stop` is seen, and then nothing more. A part reported as starting can
    /// still go unsent: when the stop lands at that moment, or the model is not
    /// ready.
    pub fn summarise(
        &self,
        turns: &[Turn],
        on_progress: &mut dyn FnMut(PartProgress),
        stop: &AtomicBool,
    ) -> Result<Vec<PartSummary>> {
        self.summarise_in(turns, on_progress, stop, PART_BUDGET_CHARS)
    }

    /// A sentence on how long a part can take, for a caller to show while it
    /// waits, or `None` when it was not measured for this provider.
    pub fn pace(&self) -> Option<&'static str> {
        matches!(self.asker, Asker::Shaped(_)).then_some(APPLE_SUMMARY_PACE)
    }

    fn summarise_in(
        &self,
        turns: &[Turn],
        on_progress: &mut dyn FnMut(PartProgress),
        stop: &AtomicBool,
        budget: usize,
    ) -> Result<Vec<PartSummary>> {
        let mut report = |progress: PartProgress| {
            if !stop.load(Ordering::Relaxed) {
                on_progress(progress);
            }
        };
        let held_back = AtomicBool::new(false);
        let parts = if let Asker::Shaped(model) = &self.asker {
            let model = AskingShaped {
                model: model.as_ref(),
                stop,
                held_back: &held_back,
            };
            meeting_summary::summarise_shaped(&model, turns, budget, &mut report)
        } else {
            let model = Asking {
                asker: &self.asker,
                stop,
                held_back: &held_back,
            };
            meeting_summary::summarise(&model, turns, budget, &mut report)
        };
        if held_back.load(Ordering::Relaxed) {
            return Err(Error::Stopped);
        }
        Ok(parts)
    }
}

/// The summary's model, as the core asks it for each part.
///
/// - Stopped work sends nothing, and says so in `held_back`. A stop seen when
///   the core asks whether the model is ready, or as a request comes back,
///   answered or not, says so too: the stop was asked for before the last
///   reply was back, so the summary is not kept. For a pool that also covers
///   a stop that kept the next model from being asked.
/// - A pool is asked through the failover, so the next model takes over only
///   when a request fails.
/// - An empty reply, or one of whitespace only, is not an answer: it is an
///   error, so a pool asks the next model. That is the only reply treated as
///   unusable. A reply cut short cannot be told apart here, since no
///   provider says so through [`TextModel`].
struct Asking<'a> {
    asker: &'a Asker,
    stop: &'a AtomicBool,
    held_back: &'a AtomicBool,
}

impl TextModel for Asking<'_> {
    fn ready(&self) -> bool {
        // Asked before each part is sent, and a part a model cannot take is
        // never sent, so a stop seen here counts too: without it a stop
        // before a last part the model was not ready for kept the summary.
        // The model is asked first, so a stop that lands while it answers is
        // seen as well.
        let ready = match self.asker {
            Asker::One(model) => model.ready(),
            Asker::Shaped(model) => model.ready(),
            Asker::Pool(pool) => pool.is_available(),
        };
        if self.stop.load(Ordering::Relaxed) {
            self.held_back.store(true, Ordering::Relaxed);
        }
        ready
    }

    fn complete(&self, instructions: &str, text: &str) -> macalla_core::Result<String> {
        if self.stop.load(Ordering::Relaxed) {
            self.held_back.store(true, Ordering::Relaxed);
            return Err(stopped());
        }
        let reply = match self.asker {
            Asker::One(model) => usable(model.complete(instructions, text)),
            // Summariser::summarise_in asks a shaped model through
            // AskingShaped, never through here.
            Asker::Shaped(_) => Err(macalla_core::Error::Enhance {
                backend: "summary",
                message: "a shaped model is asked for a shaped reply".to_string(),
            }),
            Asker::Pool(pool) => pool.ask(self.stop, |member| {
                usable(member.model.complete(instructions, text))
            }),
        };
        if self.stop.load(Ordering::Relaxed) {
            self.held_back.store(true, Ordering::Relaxed);
        }
        reply
    }
}

/// The summary's shaped model, as the core asks it for each part: stopped
/// work sends nothing, and a stop seen when the core asks whether the model
/// is ready, or as the reply comes back, counts as stopped too, all saying so
/// in `held_back`; an empty reply is an error, as for [`Asking`].
struct AskingShaped<'a> {
    model: &'a dyn ShapedModel,
    stop: &'a AtomicBool,
    held_back: &'a AtomicBool,
}

impl ShapedModel for AskingShaped<'_> {
    fn ready(&self) -> bool {
        // As for Asking: a stop seen before a part is sent counts, whether or
        // not the model can then take it, and the model is asked first.
        let ready = self.model.ready();
        if self.stop.load(Ordering::Relaxed) {
            self.held_back.store(true, Ordering::Relaxed);
        }
        ready
    }

    fn complete_shaped(
        &self,
        instructions: &str,
        text: &str,
        shape: &Shape,
    ) -> macalla_core::Result<String> {
        if self.stop.load(Ordering::Relaxed) {
            self.held_back.store(true, Ordering::Relaxed);
            return Err(stopped());
        }
        let reply = usable(self.model.complete_shaped(instructions, text, shape));
        if self.stop.load(Ordering::Relaxed) {
            self.held_back.store(true, Ordering::Relaxed);
        }
        reply
    }
}

fn stopped() -> macalla_core::Error {
    macalla_core::Error::Enhance {
        backend: "summary",
        message: "the summary was stopped before this part was sent".to_string(),
    }
}

fn usable(reply: macalla_core::Result<String>) -> macalla_core::Result<String> {
    let reply = reply?;
    if reply.trim().is_empty() {
        return Err(macalla_core::Error::Enhance {
            backend: "summary",
            message: "the model's reply was empty".to_string(),
        });
    }
    Ok(reply)
}

/// Apple's on-device foundation model's whole window, per session: "Apple's
/// on-device foundation model has a context window of 4096 tokens per
/// session", and "every session interaction with the model consumes input
/// and output tokens from this window. This includes all prompts,
/// instructions, … and all of the model's responses."
/// (Apple's documentation on managing the context window).
const APPLE_CONTEXT_TOKENS: usize = 4096;

/// The length of [`macalla_core::meeting_summary::INSTRUCTIONS`], measured
/// when this budget was set. A test keeps the instructions from growing past
/// it, so a longer prompt cannot quietly eat the room left for the part.
const INSTRUCTIONS_CHARS_MEASURED: usize = 773;

/// "In Latin alphabet languages such as English, a token typically represents
/// three to four characters" (same file). The instructions are English, and
/// the lower figure counts more tokens, so it is the careful one.
const INSTRUCTIONS_CHARS_PER_TOKEN: usize = 3;

/// The tokens left for one part of the transcript: the window, less the
/// instructions, halved. The half is a chosen split, not Apple's figure: the
/// reply is three lists that each quote the transcript, so it can come close
/// to the part's own length, and it must fit in the same window.
///
/// (4096 - ceil(773 / 3)) / 2 = (4096 - 258) / 2 = 1919.
const PART_TOKENS: usize =
    (APPLE_CONTEXT_TOKENS - INSTRUCTIONS_CHARS_MEASURED.div_ceil(INSTRUCTIONS_CHARS_PER_TOKEN)) / 2;

/// Characters per token for the transcript itself: the English figure, "a
/// token typically represents three to four characters", at its lower end
/// (same file). A decision, taken with its cost below.
const TRANSCRIPT_CHARS_PER_TOKEN: usize = 3;

/// The most characters of transcript one part carries: [`PART_TOKENS`] at
/// [`TRANSCRIPT_CHARS_PER_TOKEN`], 1919 x 3 = 5757.
///
/// Known limits:
///
/// - The shaped reply's schema counts against Apple's window too ("generable
///   type schemas", same file), and is not in this arithmetic. Measured on
///   2026-09-14, a 5,702-character part with the summary's schema and the owner
///   choice was answered without a context-window error in every run.
/// - One budget serves every provider, sized for Apple Intelligence. A remote
///   model with a far larger context still gets parts this small, so a long
///   meeting means more requests to it.
/// - Apple Intelligence cannot summarise most of a long meeting in Chinese,
///   Japanese, Korean or Vietnamese. For those languages "a token typically
///   represents one character" (same file). The instructions leave 4096 - 258
///   = 3838 tokens for a part and its reply, and this budget plans on half,
///   [`PART_TOKENS`], for the part. At one character per token, a part longer
///   than 1919 characters can come back as an error, whenever its reply needs
///   more than the room left, and one longer than 3838 characters always does.
///   Parts are cut at up to 5757 characters, so most of a long meeting is
///   lost, while a short meeting, or a short last part, is still summarised. A
///   cloud model with a larger window summarises all of it. Counting each
///   part's tokens at run time, with the model's own `tokenCount`, would fix
///   it.
pub(crate) const PART_BUDGET_CHARS: usize = PART_TOKENS * TRANSCRIPT_CHARS_PER_TOKEN;

/// How long Apple Intelligence may take to answer one part of a summary: 120
/// seconds.
///
/// It covers the slowest reply measured on a full part. On a Mac mini
/// (Macmini9,1, Apple M1) on 2026-09-14, guided summaries of one invented
/// 5,702-character meeting were timed in two probes:
///
/// - With today's [`macalla_core::meeting_summary::INSTRUCTIONS`], 24 runs,
///   across schemas with and without the owner choice, both field orders, and
///   cold and prewarmed sessions: 20 took 14.14 to 33.24 s, and the other four
///   took 42.14, 61.21, 71.52 and 73.65 s.
/// - With a shorter set of instructions, 3 runs: 23.25, 32.25 and 85.00 s.
///
/// What made the slow replies slow is not known: no build or CI ran then, and
/// load was not sampled. Cleanup's limit, 10 seconds, is sized for one dictated
/// segment, and the first real summary ran out of it.
pub(crate) const PART_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Said while Apple Intelligence summarises, so a long wait reads as expected.
/// From the 24 full parts timed with today's instructions for
/// [`PART_TIME_LIMIT`], on the Mac named there: 20 took 14.14 to 33.24 s, and
/// three of the other four took over a minute.
pub(crate) const APPLE_SUMMARY_PACE: &str = "On M1 Macs, Apple Intelligence can take 15 to 35 seconds for most parts of a file, and over a minute for a few.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choice::KeySource;
    use macalla_platform::secrets::MemoryStore;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const NAMED_VTT: &str = "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\n<v Aoife Byrne>We need to ship the parser by Friday.\n\n00:00:04.000 --> 00:00:08.000\n<v Brian>She will write the release notes.\n";

    const UNNAMED_VTT: &str = "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nWe need to ship the parser by Friday.\n\n00:00:04.000 --> 00:00:08.000\nShe will write the release notes.\n";

    #[test]
    fn a_vtt_file_with_names_keeps_its_speakers() {
        let file = transcript_from_text(NAMED_VTT).expect("words in it");
        assert!(file.named_speakers);
        let speakers: Vec<Option<&str>> = file.turns.iter().map(|t| t.speaker.as_deref()).collect();
        assert_eq!(speakers, vec![Some("Aoife Byrne"), Some("Brian")]);
        assert_eq!(file.skipped_blocks, 0);
    }

    #[test]
    fn a_vtt_file_without_names_has_no_speakers() {
        let file = transcript_from_text(UNNAMED_VTT).expect("words in it");
        assert!(!file.named_speakers);
        assert!(file.turns.iter().all(|t| t.speaker.is_none()));
    }

    #[test]
    fn plain_text_is_one_turn_with_no_speaker() {
        let file = transcript_from_text("Aoife: we need to ship\nthe parser   by Friday.\n")
            .expect("words in it");
        assert!(!file.named_speakers);
        assert_eq!(
            file.turns,
            vec![Turn {
                speaker: None,
                start_ms: 0,
                end_ms: 0,
                text: "Aoife: we need to ship the parser by Friday.".to_string(),
            }]
        );
    }

    #[test]
    fn a_file_with_no_words_is_not_a_transcript() {
        assert_eq!(transcript_from_text(""), None);
        assert_eq!(transcript_from_text(" \n\t\n"), None);
        assert_eq!(transcript_from_text("WEBVTT\n\n"), None);
    }

    #[test]
    fn reading_says_which_file_could_not_be_read_or_was_empty() {
        let dir = std::env::temp_dir().join(format!("macalla-summary-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("missing.vtt");
        let error = read_transcript(&missing).unwrap_err();
        assert!(
            matches!(&error, Error::TranscriptRead { path, .. } if path == &missing),
            "{error}"
        );

        let empty = dir.join("empty.txt");
        std::fs::write(&empty, "   \n").unwrap();
        let error = read_transcript(&empty).unwrap_err();
        assert!(
            matches!(&error, Error::EmptyTranscript { path } if path == &empty),
            "{error}"
        );

        let named = dir.join("named.vtt");
        std::fs::write(&named, NAMED_VTT).unwrap();
        assert!(read_transcript(&named).unwrap().named_speakers);

        // A .vtt without the header is refused, not read as plain text: the
        // CLI's summarise reads files through here.
        let headerless = dir.join("headerless.vtt");
        std::fs::write(&headerless, "00:00:01.000 --> 00:00:03.000\nHello\n").unwrap();
        let error = read_transcript(&headerless).unwrap_err();
        assert!(
            matches!(&error, Error::NotWebVtt { path } if path == &headerless),
            "{error}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn endpoint(url: &str, key: KeySource) -> EnhancerChoice {
        EnhancerChoice::Endpoint {
            url: url.to_string(),
            model: "test-model".to_string(),
            key,
            reasoning: None,
        }
    }

    #[test]
    fn no_provider_chosen_is_refused() {
        let error = Summariser::build(&[], &MemoryStore::new()).err().unwrap();
        assert!(matches!(error, Error::SummaryNeedsProvider), "{error}");
    }

    #[test]
    fn a_server_on_this_machine_is_local_and_a_keyed_cloud_model_is_remote() {
        let local = Summariser::build(
            &[endpoint("http://localhost:11434", KeySource::None)],
            &MemoryStore::new(),
        )
        .unwrap();
        assert_eq!(
            (local.reach(), local.destination()),
            (CleanupReach::Local, "Local")
        );

        let keys = MemoryStore::new();
        crate::providers::store_key(&keys, "openai", "sk-test").unwrap();
        let cloud = Summariser::build(
            &[endpoint(
                "https://api.openai.com/v1",
                KeySource::Stored("openai".to_string()),
            )],
            &keys,
        )
        .unwrap();
        assert_eq!(
            (cloud.reach(), cloud.destination()),
            (CleanupReach::Remote, "OpenAI")
        );
    }

    #[test]
    fn a_provider_that_cannot_start_says_why() {
        let error = Summariser::build(
            &[EnhancerChoice::Anthropic {
                model: "claude-test".to_string(),
                key: KeySource::Stored("anthropic".to_string()),
                reasoning: None,
            }],
            &MemoryStore::new(),
        )
        .err()
        .unwrap();
        assert!(
            matches!(&error, Error::SummaryProviderUnavailable(reason) if reason.contains("anthropic needs a key")),
            "{error}"
        );
    }

    #[test]
    fn two_models_that_build_are_a_remote_pool_labelled_as_cleanup_labels_it() {
        let keys = MemoryStore::new();
        crate::providers::store_key(&keys, "openai", "sk-test").unwrap();
        crate::providers::store_key(&keys, "anthropic", "sk-test").unwrap();
        let pool = Summariser::build(
            &[
                endpoint(
                    "https://api.openai.com/v1",
                    KeySource::Stored("openai".to_string()),
                ),
                EnhancerChoice::Anthropic {
                    model: "claude-test".to_string(),
                    key: KeySource::Stored("anthropic".to_string()),
                    reasoning: None,
                },
            ],
            &keys,
        )
        .unwrap();
        assert_eq!(
            (pool.reach(), pool.destination()),
            (CleanupReach::Remote, "OpenAI, falling back to Anthropic")
        );
        assert!(matches!(pool.asker, Asker::Pool(_)));
    }

    #[test]
    fn a_stored_cleanup_choice_builds_for_the_summary_exactly_when_it_builds_for_cleanup() {
        // What this checks: cleanup and the summary build their models by
        // separate code (choice::session_enhancer and
        // choice::text_model_for). For each stored choice, read through
        // the one mapping both use, and with Anthropic's key missing, the
        // summary builds exactly when cleanup does; when neither can, it is
        // for the same reason; when both do, they report the same destination
        // and reach.
        use crate::choice::{CleanupChoice, PoolModel};
        let keys = MemoryStore::new();
        crate::providers::store_key(&keys, "openai", "sk-test").unwrap();
        let ticked = |provider: &str, model: &str| PoolModel {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let stored = [
            CleanupChoice::Pool {
                models: vec![
                    ticked("openai", "gpt-test"),
                    ticked("anthropic", "claude-test"),
                ],
            },
            CleanupChoice::Pool {
                models: vec![ticked("anthropic", "claude-test")],
            },
            CleanupChoice::Pool {
                models: vec![ticked("openai", "gpt-test")],
            },
            CleanupChoice::AppleIntelligence,
            CleanupChoice::Server {
                address: "http://localhost:11434".to_string(),
                model: "granite4.1:3b".to_string(),
            },
        ];
        let mut refused = 0;
        for choice in &stored {
            let choices = crate::choice::cleanup_enhancers(choice);
            let (cleanup, cleanup_reason, _skipped) = crate::cleanup::build(&choices, &keys);
            match (cleanup, Summariser::build(&choices, &keys)) {
                (Some(cleanup), Ok(summariser)) => assert_eq!(
                    (summariser.destination(), summariser.reach()),
                    (cleanup.destination(), cleanup.reach()),
                    "{choice:?}"
                ),
                (None, Err(error)) => {
                    refused += 1;
                    let reason = cleanup_reason.expect("cleanup says why").to_string();
                    assert!(
                        matches!(&error, Error::SummaryProviderUnavailable(r) if r == &reason),
                        "{choice:?}: summary {error}, cleanup {reason}"
                    );
                }
                (cleanup, summary) => panic!(
                    "{choice:?}: cleanup built {}, the summary built {}",
                    cleanup.is_some(),
                    summary.is_ok()
                ),
            }
        }
        assert_eq!(refused, 1, "only Anthropic alone lacks its key");
    }

    /// A model that answers from a script, one reply per request, and counts
    /// what it was asked.
    struct Scripted {
        replies: Mutex<VecDeque<macalla_core::Result<String>>>,
        asked: Arc<AtomicUsize>,
    }

    fn scripted(
        replies: Vec<macalla_core::Result<String>>,
    ) -> (Box<dyn TextModel>, Arc<AtomicUsize>) {
        let asked = Arc::new(AtomicUsize::new(0));
        (
            Box::new(Scripted {
                replies: Mutex::new(replies.into()),
                asked: Arc::clone(&asked),
            }),
            asked,
        )
    }

    impl TextModel for Scripted {
        fn ready(&self) -> bool {
            true
        }
        fn complete(&self, _instructions: &str, _text: &str) -> macalla_core::Result<String> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(refused()))
        }
    }

    fn refused() -> macalla_core::Error {
        macalla_core::Error::Enhance {
            backend: "fake",
            message: "the request to https://api.example.com/v1 failed: http status: 503"
                .to_string(),
        }
    }

    fn lists_quoting(quote: &str) -> macalla_core::Result<String> {
        Ok(format!(
            "Key points\n- A point \"{quote}\"\nDecisions\nNone.\nAction items\nNone."
        ))
    }

    fn pool_of(members: Vec<(&str, Box<dyn TextModel>)>) -> Summariser {
        Summariser {
            asker: Asker::Pool(Failover::new(
                members
                    .into_iter()
                    .map(|(provider, model)| TextMember {
                        model,
                        provider: provider.to_string(),
                        model_id: format!("{provider}-test"),
                    })
                    .collect(),
            )),
            reach: CleanupReach::Remote,
            destination: "OpenAI, falling back to Anthropic".to_string(),
            fingerprint: String::new(),
        }
    }

    #[test]
    fn apple_intelligence_is_asked_for_a_shaped_summary() {
        let summariser = Summariser::build(
            &[EnhancerChoice::AppleIntelligence { guided: true }],
            &MemoryStore::new(),
        )
        .unwrap();

        assert!(matches!(summariser.asker, Asker::Shaped(_)));
    }

    /// A shaped model that always replies with the same text, and counts what
    /// it was asked.
    struct ShapedScripted {
        reply: String,
        asked: Arc<AtomicUsize>,
    }

    impl ShapedModel for ShapedScripted {
        fn ready(&self) -> bool {
            true
        }
        fn complete_shaped(
            &self,
            _instructions: &str,
            _text: &str,
            _shape: &Shape,
        ) -> macalla_core::Result<String> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            Ok(self.reply.clone())
        }
    }

    #[test]
    fn a_shaped_summary_stopped_sends_nothing_and_an_empty_shaped_reply_is_an_error() {
        let asked = Arc::new(AtomicUsize::new(0));
        let summariser = Summariser {
            asker: Asker::Shaped(Box::new(ShapedScripted {
                reply: "  ".to_string(),
                asked: Arc::clone(&asked),
            })),
            reach: CleanupReach::Local,
            destination: "Apple Intelligence".to_string(),
            fingerprint: String::new(),
        };

        let stopped =
            summariser.summarise_in(&two_turns(), &mut |_| {}, &AtomicBool::new(true), 60);
        assert!(matches!(stopped, Err(Error::Stopped)));
        assert_eq!(asked.load(Ordering::Relaxed), 0, "nothing is sent");

        let empty = summariser
            .summarise_in(&two_turns(), &mut |_| {}, &AtomicBool::new(false), 60)
            .unwrap();
        assert!(empty.iter().all(|part| part
            .lists
            .as_ref()
            .is_err_and(|e| e.to_string().contains("empty"))));
        assert_eq!(asked.load(Ordering::Relaxed), 2);
    }

    fn two_turns() -> Vec<Turn> {
        vec![
            Turn {
                speaker: Some("Aoife Byrne".to_string()),
                start_ms: 0,
                end_ms: 4_000,
                text: "We need to ship the parser by Friday.".to_string(),
            },
            Turn {
                speaker: Some("Brian".to_string()),
                start_ms: 4_000,
                end_ms: 8_000,
                text: "She will write the release notes.".to_string(),
            },
        ]
    }

    #[test]
    fn a_pool_hands_each_part_to_the_next_model_when_the_first_fails() {
        let (first, first_asked) = scripted(vec![Err(refused()), Err(refused())]);
        let (second, second_asked) = scripted(vec![
            lists_quoting("ship the parser by Friday"),
            lists_quoting("write the release notes"),
        ]);
        let summariser = pool_of(vec![("openai", first), ("anthropic", second)]);

        // A budget that fits one labelled turn, so the two turns are two parts.
        let parts = summariser
            .summarise_in(&two_turns(), &mut |_| {}, &AtomicBool::new(false), 60)
            .unwrap();

        assert_eq!(parts.len(), 2);
        for part in &parts {
            let lists = part.lists.as_ref().expect("the second model answered");
            assert_eq!(lists.key_points.len(), 1, "{lists:?}");
        }
        assert_eq!(
            (
                first_asked.load(Ordering::Relaxed),
                second_asked.load(Ordering::Relaxed)
            ),
            (2, 2),
            "every part starts again from the first model"
        );
    }

    #[test]
    fn a_part_every_model_fails_is_an_error_and_the_other_parts_still_come_back() {
        let (first, _) = scripted(vec![
            Err(refused()),
            lists_quoting("write the release notes"),
        ]);
        let (second, _) = scripted(vec![Err(refused())]);
        let summariser = pool_of(vec![("openai", first), ("anthropic", second)]);

        let parts = summariser
            .summarise_in(&two_turns(), &mut |_| {}, &AtomicBool::new(false), 60)
            .unwrap();

        assert_eq!(parts.len(), 2);
        assert!(parts[0].lists.is_err(), "both models failed the first part");
        assert!(
            parts[1].lists.is_ok(),
            "the first model answered the second part"
        );
    }

    #[test]
    fn a_summary_stopped_before_it_starts_sends_nothing() {
        let (first, first_asked) = scripted(vec![lists_quoting("ship the parser by Friday")]);
        let (second, second_asked) = scripted(vec![lists_quoting("ship the parser by Friday")]);
        let pool = pool_of(vec![("openai", first), ("anthropic", second)]);
        let parts = pool.summarise_in(&two_turns(), &mut |_| {}, &AtomicBool::new(true), 60);
        assert!(matches!(parts, Err(Error::Stopped)));
        assert_eq!(
            (
                first_asked.load(Ordering::Relaxed),
                second_asked.load(Ordering::Relaxed)
            ),
            (0, 0),
            "no model in the pool is asked"
        );

        let (only, only_asked) = scripted(vec![lists_quoting("ship the parser by Friday")]);
        let one = Summariser {
            asker: Asker::One(only),
            reach: CleanupReach::Local,
            destination: "Local".to_string(),
            fingerprint: String::new(),
        };
        let parts = one.summarise_in(&two_turns(), &mut |_| {}, &AtomicBool::new(true), 60);
        assert!(matches!(parts, Err(Error::Stopped)));
        assert_eq!(
            only_asked.load(Ordering::Relaxed),
            0,
            "the one model is not asked"
        );
    }

    #[test]
    fn a_stop_while_the_first_model_works_asks_no_further_model() {
        let stop = Arc::new(AtomicBool::new(false));
        let first = Box::new(StopsWhileAsked(Arc::clone(&stop)));
        let (second, second_asked) = scripted(vec![lists_quoting("ship the parser by Friday")]);
        let summariser = pool_of(vec![("openai", first), ("anthropic", second)]);

        let parts = summariser.summarise_in(&two_turns()[..1], &mut |_| {}, &stop, 5_000);

        assert!(
            matches!(parts, Err(Error::Stopped)),
            "the stop kept the next model from being asked"
        );
        assert_eq!(second_asked.load(Ordering::Relaxed), 0);
    }

    /// A model that fails, and sets the stop flag while it is being asked.
    struct StopsWhileAsked(Arc<AtomicBool>);

    impl TextModel for StopsWhileAsked {
        fn ready(&self) -> bool {
            true
        }
        fn complete(&self, _instructions: &str, _text: &str) -> macalla_core::Result<String> {
            self.0.store(true, Ordering::Relaxed);
            Err(refused())
        }
    }

    /// A model that answers each part of [`two_turns`] with a point quoting
    /// it, and sets the stop flag while it answers request `stop_on`, counting
    /// from 1.
    struct AnswersThenStops {
        stop: Arc<AtomicBool>,
        asked: AtomicUsize,
        stop_on: usize,
    }

    impl AnswersThenStops {
        fn on(stop_on: usize, stop: &Arc<AtomicBool>) -> Self {
            Self {
                stop: Arc::clone(stop),
                asked: AtomicUsize::new(0),
                stop_on,
            }
        }
    }

    impl TextModel for AnswersThenStops {
        fn ready(&self) -> bool {
            true
        }
        fn complete(&self, _instructions: &str, text: &str) -> macalla_core::Result<String> {
            let asked = self.asked.fetch_add(1, Ordering::Relaxed) + 1;
            if asked == self.stop_on {
                self.stop.store(true, Ordering::Relaxed);
            }
            if text.contains("ship the parser by Friday") {
                lists_quoting("ship the parser by Friday")
            } else {
                lists_quoting("write the release notes")
            }
        }
    }

    #[test]
    fn a_stop_while_the_last_part_is_out_means_stopped_for_one_model_or_a_pool() {
        // The stop lands while the second and last part is with the model, as
        // Stop pressed while the card says "Stopping when this part comes
        // back". The reply still arrives, but the person asked to stop before
        // it did, so the summary is not kept.
        let stop = Arc::new(AtomicBool::new(false));
        let one = Summariser {
            asker: Asker::One(Box::new(AnswersThenStops::on(2, &stop))),
            reach: CleanupReach::Local,
            destination: "Local".to_string(),
            fingerprint: String::new(),
        };
        let parts = one.summarise_in(&two_turns(), &mut |_| {}, &stop, 60);
        assert!(matches!(parts, Err(Error::Stopped)), "one model: {parts:?}");

        let stop = Arc::new(AtomicBool::new(false));
        let (second, second_asked) = scripted(Vec::new());
        let pool = pool_of(vec![
            (
                "openai",
                Box::new(AnswersThenStops::on(2, &stop)) as Box<dyn TextModel>,
            ),
            ("anthropic", second),
        ]);
        let parts = pool.summarise_in(&two_turns(), &mut |_| {}, &stop, 60);
        assert!(matches!(parts, Err(Error::Stopped)), "a pool: {parts:?}");
        assert_eq!(second_asked.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_stop_after_the_last_reply_is_back_keeps_every_part() {
        // The stop lands only once the last part has its answer: the report
        // that the part finished is the first moment after its reply.
        let stop = AtomicBool::new(false);
        let (only, _) = scripted(vec![
            lists_quoting("ship the parser by Friday"),
            lists_quoting("write the release notes"),
        ]);
        let one = Summariser {
            asker: Asker::One(only),
            reach: CleanupReach::Local,
            destination: "Local".to_string(),
            fingerprint: String::new(),
        };
        let parts = one
            .summarise_in(
                &two_turns(),
                &mut |progress| {
                    if progress == (PartProgress::Finished { part: 2, of: 2 }) {
                        stop.store(true, Ordering::Relaxed);
                    }
                },
                &stop,
                60,
            )
            .expect("every reply was back before the stop");
        assert_eq!(parts.len(), 2);
        assert!(
            parts
                .iter()
                .all(|part| part.lists.as_ref().is_ok_and(|l| l.key_points.len() == 1)),
            "{parts:?}"
        );
    }

    #[test]
    fn each_part_is_reported_as_it_starts_and_finishes_with_the_number_of_parts() {
        let expected = vec![
            PartProgress::Started { part: 1, of: 2 },
            PartProgress::Finished { part: 1, of: 2 },
            PartProgress::Started { part: 2, of: 2 },
            PartProgress::Finished { part: 2, of: 2 },
        ];

        let (first, _) = scripted(vec![
            lists_quoting("ship the parser by Friday"),
            lists_quoting("write the release notes"),
        ]);
        let (second, _) = scripted(Vec::new());
        let pool = pool_of(vec![("openai", first), ("anthropic", second)]);
        let mut heard = Vec::new();
        pool.summarise_in(
            &two_turns(),
            &mut |progress| heard.push(progress),
            &AtomicBool::new(false),
            60,
        )
        .unwrap();
        assert_eq!(heard, expected, "a pool, asked for plain text");

        let shaped = Summariser {
            asker: Asker::Shaped(Box::new(ShapedScripted {
                reply: "{}".to_string(),
                asked: Arc::new(AtomicUsize::new(0)),
            })),
            reach: CleanupReach::Local,
            destination: "Apple Intelligence".to_string(),
            fingerprint: String::new(),
        };
        let mut heard = Vec::new();
        shaped
            .summarise_in(
                &two_turns(),
                &mut |progress| heard.push(progress),
                &AtomicBool::new(false),
                60,
            )
            .unwrap();
        assert_eq!(
            heard, expected,
            "Apple Intelligence, asked for a shaped reply"
        );
    }

    #[test]
    fn a_stopped_summary_reports_no_part_it_will_not_send() {
        let stop = Arc::new(AtomicBool::new(false));
        let one = Summariser {
            asker: Asker::One(Box::new(StopsWhileAsked(Arc::clone(&stop)))),
            reach: CleanupReach::Local,
            destination: "Local".to_string(),
            fingerprint: String::new(),
        };
        let mut heard = Vec::new();
        let parts = one.summarise_in(
            &two_turns(),
            &mut |progress| heard.push(progress),
            &stop,
            60,
        );
        assert!(matches!(parts, Err(Error::Stopped)));
        assert_eq!(
            heard,
            vec![PartProgress::Started { part: 1, of: 2 }],
            "stopped while the first part was asked: the second is not sent, so not reported"
        );

        let shaped = Summariser {
            asker: Asker::Shaped(Box::new(ShapedScripted {
                reply: "{}".to_string(),
                asked: Arc::new(AtomicUsize::new(0)),
            })),
            reach: CleanupReach::Local,
            destination: "Apple Intelligence".to_string(),
            fingerprint: String::new(),
        };
        let mut heard = Vec::new();
        let parts = shaped.summarise_in(
            &two_turns(),
            &mut |progress| heard.push(progress),
            &AtomicBool::new(true),
            60,
        );
        assert!(matches!(parts, Err(Error::Stopped)));
        assert!(heard.is_empty(), "stopped before it started: {heard:?}");

        let stop = Arc::new(AtomicBool::new(false));
        let asked = Arc::new(AtomicUsize::new(0));
        let apple = Summariser {
            asker: Asker::Shaped(Box::new(ShapedStopsWhileAsked {
                stop: Arc::clone(&stop),
                asked: Arc::clone(&asked),
            })),
            reach: CleanupReach::Local,
            destination: "Apple Intelligence".to_string(),
            fingerprint: String::new(),
        };
        let mut heard = Vec::new();
        let parts = apple.summarise_in(
            &two_turns(),
            &mut |progress| heard.push(progress),
            &stop,
            60,
        );
        assert!(matches!(parts, Err(Error::Stopped)));
        assert_eq!(
            heard,
            vec![PartProgress::Started { part: 1, of: 2 }],
            "Apple Intelligence stopped while the first part was asked"
        );
        assert_eq!(
            asked.load(Ordering::Relaxed),
            1,
            "the second part is not sent"
        );
    }

    /// A shaped reply that fits the shape asked for, with one key point quoting
    /// the first of [`two_turns`].
    const GOOD_SHAPED_REPLY: &str = r#"{"key_points":[{"text":"The parser ships on Friday.","quote":"ship the parser by Friday"}],"decisions":[],"action_items":[]}"#;

    /// A model that answers its first request with a point quoting the first
    /// of [`two_turns`], and is not ready once it has answered.
    struct AnswersOnceThenUnready(AtomicUsize);

    impl TextModel for AnswersOnceThenUnready {
        fn ready(&self) -> bool {
            self.0.load(Ordering::Relaxed) == 0
        }
        fn complete(&self, _instructions: &str, _text: &str) -> macalla_core::Result<String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            lists_quoting("ship the parser by Friday")
        }
    }

    /// The same, as a shaped model.
    struct ShapedAnswersOnceThenUnready(AtomicUsize);

    impl ShapedModel for ShapedAnswersOnceThenUnready {
        fn ready(&self) -> bool {
            self.0.load(Ordering::Relaxed) == 0
        }
        fn complete_shaped(
            &self,
            _instructions: &str,
            _text: &str,
            _shape: &Shape,
        ) -> macalla_core::Result<String> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(GOOD_SHAPED_REPLY.to_string())
        }
    }

    #[test]
    fn a_stop_before_a_last_part_the_model_is_not_ready_for_means_stopped() {
        // The stop lands after the first part's reply is back. The model is
        // then not ready for the second, so the core returns that part as an
        // error without sending it and never asks the model to complete it.
        // The stop still came before the last reply, so it is Stopped.
        let summarisers = [
            (
                "one model",
                Summariser {
                    asker: Asker::One(Box::new(AnswersOnceThenUnready(AtomicUsize::new(0)))),
                    reach: CleanupReach::Local,
                    destination: "Local".to_string(),
                    fingerprint: String::new(),
                },
            ),
            (
                "Apple Intelligence",
                Summariser {
                    asker: Asker::Shaped(Box::new(ShapedAnswersOnceThenUnready(AtomicUsize::new(
                        0,
                    )))),
                    reach: CleanupReach::Local,
                    destination: "Apple Intelligence".to_string(),
                    fingerprint: String::new(),
                },
            ),
            (
                // The first member answers part 1 and is then not ready, and
                // the second never is, so the pool is not available for part 2.
                "a pool",
                pool_of(vec![
                    (
                        "openai",
                        Box::new(AnswersOnceThenUnready(AtomicUsize::new(0))) as Box<dyn TextModel>,
                    ),
                    (
                        "anthropic",
                        Box::new(AnswersOnceThenUnready(AtomicUsize::new(1))) as Box<dyn TextModel>,
                    ),
                ]),
            ),
        ];
        for (who, summariser) in summarisers {
            let stop = AtomicBool::new(false);
            let parts = summariser.summarise_in(
                &two_turns(),
                &mut |progress| {
                    if progress == (PartProgress::Finished { part: 1, of: 2 }) {
                        stop.store(true, Ordering::Relaxed);
                    }
                },
                &stop,
                60,
            );
            assert!(matches!(parts, Err(Error::Stopped)), "{who}: {parts:?}");
        }
    }

    /// A shaped model that sets the stop flag while it is being asked, and
    /// counts what it was asked.
    struct ShapedStopsWhileAsked {
        stop: Arc<AtomicBool>,
        asked: Arc<AtomicUsize>,
    }

    impl ShapedModel for ShapedStopsWhileAsked {
        fn ready(&self) -> bool {
            true
        }
        fn complete_shaped(
            &self,
            _instructions: &str,
            _text: &str,
            _shape: &Shape,
        ) -> macalla_core::Result<String> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            Ok(GOOD_SHAPED_REPLY.to_string())
        }
    }

    #[test]
    fn a_stop_while_the_only_part_is_out_means_stopped() {
        // A short meeting on Apple Intelligence: one part, and Stop pressed
        // while it is with the model. Found in the app: the card said
        // "Stopping when this part comes back" and then showed the summary.
        let stop = Arc::new(AtomicBool::new(false));
        let asked = Arc::new(AtomicUsize::new(0));
        let apple = Summariser {
            asker: Asker::Shaped(Box::new(ShapedStopsWhileAsked {
                stop: Arc::clone(&stop),
                asked: Arc::clone(&asked),
            })),
            reach: CleanupReach::Local,
            destination: "Apple Intelligence".to_string(),
            fingerprint: String::new(),
        };

        // The reply the model sends is a good one: unstopped, it is kept as a
        // point. So what the stop throws away below is a real summary.
        let unstopped = Summariser {
            asker: Asker::Shaped(Box::new(ShapedScripted {
                reply: GOOD_SHAPED_REPLY.to_string(),
                asked: Arc::new(AtomicUsize::new(0)),
            })),
            reach: CleanupReach::Local,
            destination: "Apple Intelligence".to_string(),
            fingerprint: String::new(),
        }
        .summarise_in(
            &two_turns()[..1],
            &mut |_| {},
            &AtomicBool::new(false),
            5_000,
        )
        .expect("nothing stopped it");
        assert!(
            unstopped[0]
                .lists
                .as_ref()
                .is_ok_and(|l| l.key_points.len() == 1),
            "{unstopped:?}"
        );

        // One turn within the budget is one part.
        let parts = apple.summarise_in(&two_turns()[..1], &mut |_| {}, &stop, 5_000);

        assert!(matches!(parts, Err(Error::Stopped)), "{parts:?}");
        assert_eq!(asked.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn only_apple_intelligence_says_how_long_a_part_can_take() {
        let apple = Summariser::build(
            &[EnhancerChoice::AppleIntelligence { guided: true }],
            &MemoryStore::new(),
        )
        .unwrap();
        assert_eq!(apple.pace(), Some(APPLE_SUMMARY_PACE));

        let local = Summariser::build(
            &[endpoint("http://localhost:11434", KeySource::None)],
            &MemoryStore::new(),
        )
        .unwrap();
        assert_eq!(local.pace(), None);
    }

    #[test]
    fn an_empty_reply_is_not_an_answer_so_the_next_model_is_asked() {
        let (first, first_asked) = scripted(vec![Ok("  \n".to_string())]);
        let (second, second_asked) = scripted(vec![lists_quoting("ship the parser by Friday")]);
        let summariser = pool_of(vec![("openai", first), ("anthropic", second)]);

        let parts = summariser
            .summarise_in(
                &two_turns()[..1],
                &mut |_| {},
                &AtomicBool::new(false),
                5_000,
            )
            .unwrap();

        let lists = parts[0].lists.as_ref().expect("the second model answered");
        assert_eq!(lists.key_points.len(), 1, "{lists:?}");
        assert_eq!(
            (
                first_asked.load(Ordering::Relaxed),
                second_asked.load(Ordering::Relaxed)
            ),
            (1, 1)
        );
    }

    #[test]
    fn the_part_budget_leaves_room_for_the_instructions_and_the_reply() {
        assert!(
            macalla_core::meeting_summary::INSTRUCTIONS.chars().count()
                <= INSTRUCTIONS_CHARS_MEASURED,
            "the instructions grew past the length the budget was set for"
        );
        assert_eq!(PART_TOKENS, 1919);
        assert_eq!(
            PART_BUDGET_CHARS,
            (4096 - 773_usize.div_ceil(3)) / 2 * 3,
            "the documented arithmetic: the window, less the instructions, halved, at 3 characters per token"
        );
        assert_eq!(PART_BUDGET_CHARS, 5757);

        // For a language at one character per token: a part up to 1919
        // characters fits with the planned room for its reply, and one past
        // 3838 cannot fit in Apple Intelligence's window at all.
        let multibyte_chars_per_token = 1;
        assert_eq!(
            PART_TOKENS * multibyte_chars_per_token,
            1919,
            "the documented figure for Chinese, Japanese, Korean and Vietnamese"
        );
        assert_eq!(
            (APPLE_CONTEXT_TOKENS
                - INSTRUCTIONS_CHARS_MEASURED.div_ceil(INSTRUCTIONS_CHARS_PER_TOKEN))
                * multibyte_chars_per_token,
            3838,
            "past this, a part at one character per token always overflows the window"
        );
    }

    #[test]
    fn a_pool_member_gaining_its_key_changes_the_summary_fingerprint() {
        let keys = MemoryStore::new();
        keys.set("provider-key.anthropic", "test-key").unwrap();
        let choices = crate::choice::cleanup_enhancers(&crate::choice::CleanupChoice::Pool {
            models: vec![
                crate::choice::PoolModel {
                    provider: "openai".into(),
                    model: "gpt-test".into(),
                },
                crate::choice::PoolModel {
                    provider: "anthropic".into(),
                    model: "claude-test".into(),
                },
            ],
        });

        let before = Summariser::build(&choices, &keys).unwrap();
        keys.set("provider-key.openai", "test-key").unwrap();
        let after = Summariser::build(&choices, &keys).unwrap();

        // The same stored choice, but the transcript can now reach OpenAI too.
        assert_ne!(before.fingerprint(), after.fingerprint());
    }
}
