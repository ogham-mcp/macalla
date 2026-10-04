// SPDX-License-Identifier: MIT
//! What the app says about a live meeting, worded here and nowhere else
//! (user-facing wording lives in one place per app). The
//! session hands over typed reasons -- `macalla_session::Error`,
//! `macalla_platform::system_audio::OpenError`, a source's signal health --
//! and no sentence at all; `files.rs`, `dictation.rs` and `say.rs` pick an
//! entry here and never build one.
//!
//! Each entry is a whole sentence, or two, never glued from fragments. A
//! placeholder is data only: a device's name as the OS gives it, or a time
//! already written as minutes and seconds.
//!
//! The speakers a live meeting's turns carry, "You" and "Others", stay
//! English data in the turn, and in anything exported from it; the page
//! shows them through [`speaker`].
//!
//! It also says where a summary's quote was said, for any meeting on the
//! Files page: [`quote_said_by`] and [`quote_said_at`]; that a summary is
//! out of date once the transcript is corrected; the transcript's find
//! and correcting, from [`FIND`] to [`EDIT_HELD_NOT_WRITTEN`], with
//! [`edit_turn`]; and everything a batch of files says, from
//! [`BATCH_HEADING`] on.
//!
//! A summary's list headings ([`KEY_POINTS`], [`DECISIONS`],
//! [`ACTION_ITEMS`]) and Markdown export's transcript heading
//! ([`MARKDOWN_TRANSCRIPT`]) live here too, so the Files page and a Markdown
//! file never spell them differently.
//!
//! So does saving's own wording: the one choice of format on the Files
//! card ([`SAVE_TRANSCRIPTS_AS`], with [`FORMAT_TEXT`], [`FORMAT_VTT`],
//! [`FORMAT_SRT`] and [`FORMAT_MARKDOWN`]), which both the current meeting's
//! [`SAVE`] and a batch save in, and the sentence below the current meeting
//! once it has a summary ([`TRANSCRIPT_AND_SUMMARY_GONE_UNLESS_MARKDOWN`]).
//!
//! A first draft, for review.

use macalla_platform::system_audio::OpenError;
use macalla_session::live_meeting::{SignalHealth, OTHERS, YOU};
use macalla_session::Error;

/// The Files page's live meeting card: its heading.
pub(crate) const HEADING: &str = "Live meeting";

/// The live meeting card's one sentence.
pub(crate) const ABOUT: &str = "Turn a call, or a meeting in the room, into text while it \
                                happens. The sound is heard on this Mac and not kept.";

/// The button that starts a live meeting.
pub(crate) const START: &str = "Start a live meeting";

/// The button that ends a live meeting and keeps what was heard.
pub(crate) const STOP: &str = "Stop and keep";

/// The button that ends a live meeting and keeps nothing.
pub(crate) const DISCARD: &str = "Discard";

/// Shown from the press until the meeting's first progress.
pub(crate) const STARTING: &str = "Starting…";

/// Shown once Stop and keep or Discard was pressed, until the meeting ends.
pub(crate) const STOPPING: &str = "Stopping…";

/// A live meeting's name on the page, and at the front of a saved file's
/// name, where a file meeting has the file's.
pub(crate) const NAME: &str = "Live meeting";

/// How much of the meeting has been heard. `{clock}` is minutes and
/// seconds, "3:07".
pub(crate) fn heard(clock: &str) -> String {
    format!("{clock} heard")
}

/// A turn's speaker as the page shows it. [`YOU`] and [`OTHERS`] are the
/// live meeting's; any other name came from a file and is shown as it is.
pub(crate) fn speaker(name: &str) -> String {
    match name {
        YOU => "You".to_string(),
        OTHERS => "Others".to_string(),
        other => other.to_string(),
    }
}

/// Where a summary item's quote was said, shown beside the quote, which goes
/// to that turn in the transcript when pressed: "{speaker}, {time}".
/// `{speaker}` is the turn's speaker as the page shows them, and `{time}`
/// when the turn starts, minutes and seconds, "3:07".
pub(crate) fn quote_said_by(speaker: &str, time: &str) -> String {
    format!("{speaker}, {time}")
}

/// [`quote_said_by`] for a turn with no speaker, as in a recording heard
/// here: "{time}", the time alone.
pub(crate) fn quote_said_at(time: &str) -> String {
    time.to_string()
}

/// A summary's key points, shown above their list on the Files page and, in
/// Markdown export, above the same list there.
pub(crate) const KEY_POINTS: &str = "Key points";

/// A summary's decisions, shown above their list on the Files page and, in
/// Markdown export, above the same list there.
pub(crate) const DECISIONS: &str = "Decisions";

/// A summary's action items, shown above their list on the Files page and,
/// in Markdown export, above the same list there.
pub(crate) const ACTION_ITEMS: &str = "Action items";

/// Markdown export's heading above the transcript, after the summary's
/// lists when there is one. The CLI writes Markdown with the same writer
/// and its own copy of this heading, the three list headings above and
/// [`SUMMARY_OUT_OF_DATE`], in `apps/cli/src/words.rs` (`MARKDOWN`); change
/// the two together.
pub(crate) const MARKDOWN_TRANSCRIPT: &str = "Transcript";

/// The label beside the Files card's one choice of format: what the
/// current meeting's Save button writes, and what each file of a batch is
/// saved as.
pub(crate) const SAVE_TRANSCRIPTS_AS: &str = "Save transcripts as";

/// "Save", on two buttons: the one beside the current meeting, which saves
/// it to Downloads, and the one under a turn being corrected, which keeps
/// the correction. One entry, so the two never drift apart.
pub(crate) const SAVE: &str = "Save";

/// A format a transcript can be saved in: plain text, one turn per
/// paragraph.
pub(crate) const FORMAT_TEXT: &str = "Text (.txt)";

/// A format a transcript can be saved in: WebVTT, with each turn's times.
pub(crate) const FORMAT_VTT: &str = "WebVTT subtitles (.vtt)";

/// A format a transcript can be saved in: SRT subtitles, with each turn's
/// times.
pub(crate) const FORMAT_SRT: &str = "SRT subtitles (.srt)";

/// A format a transcript can be saved in: Markdown, with the summary when
/// there is one.
pub(crate) const FORMAT_MARKDOWN: &str = "Markdown (.md)";

/// Below the current meeting's buttons, when it has a summary: which
/// format keeps it too, since only Markdown export writes the summary
/// alongside the transcript. The no-summary case ("The transcript is gone
/// when you quit, unless you save it.") is unchanged inline JSX, not moved
/// here, since this entry is the only one of the pair that is new wording.
pub(crate) const TRANSCRIPT_AND_SUMMARY_GONE_UNLESS_MARKDOWN: &str =
    "The transcript and its summary are gone when you quit. Saving as Markdown keeps both; \
     the other formats keep the transcript alone.";

/// When the Downloads folder cannot be found, for anything that writes there:
/// a Files-page meeting in any format, a batch, or the dictionary's shared
/// copy.
pub(crate) const DOWNLOADS_NOT_FOUND: &str = "Could not find the Downloads folder.";

/// When a Files-page meeting could not be written to Downloads, whatever
/// the format.
pub(crate) const COULD_NOT_SAVE_FILE: &str = "Could not save the file to the Downloads folder.";

/// Above a summary, what its quotes promise and what they do not: each quote
/// is words really said, placed where they were said, and whether it backs
/// its point is the reader's to judge. One entry for both sentences.
pub(crate) const SUMMARY_QUOTES: &str = "Each point quotes what was really said, and who said \
     it and when. Whether the quote backs the point is yours to judge.";

/// The open panel's title. It takes one file or several: one opens as the
/// current meeting, several start a batch.
pub(crate) const CHOOSE_TITLE: &str = "Choose one or more recordings or transcripts";

/// The Files page's batch card: its heading, shown while several files are
/// heard in turn and after, until the list is cleared.
pub(crate) const BATCH_HEADING: &str = "Several files";

/// The batch card's one sentence: what a batch does, and what it does not.
pub(crate) const BATCH_ABOUT: &str = "Each file is heard in turn on the same speech engine, and \
                                      its transcript is saved to your Downloads folder as soon as \
                                      it is done. Tidying and summaries work on one file at a \
                                      time.";

/// The batch card's button that stops the file being heard and every one
/// still waiting.
pub(crate) const BATCH_STOP: &str = "Stop";

/// The batch card's button that takes a finished batch's list off the page.
/// The saved files stay where they are.
pub(crate) const BATCH_CLEAR: &str = "Clear list";

/// A batch's row for a file not yet started.
pub(crate) const BATCH_WAITING: &str = "Waiting";

/// A batch's row for a file being opened or decoded, before any of it is
/// heard.
pub(crate) const BATCH_OPENING: &str = "Opening…";

/// A batch's row for a file being heard. `{done}` and `{of}` are minutes and
/// seconds, "3:07".
pub(crate) fn batch_hearing(done: &str, of: &str) -> String {
    format!("{done} of {of} heard")
}

/// A batch's row for a file whose transcript was saved. `{path}` is where,
/// the whole path.
pub(crate) fn batch_saved(path: &str) -> String {
    format!("Saved to {path}.")
}

/// A batch's row for a file heard, or read, whose transcript could not be
/// written to Downloads.
pub(crate) const BATCH_NOT_SAVED: &str =
    "The transcript could not be saved to the Downloads folder.";

/// A batch's row for a file still waiting when the batch was stopped.
pub(crate) const BATCH_NOT_STARTED: &str = "Not heard, because the batch was stopped.";

/// A batch's row for a file that went wrong in a way no other sentence
/// names (a panic). The rest of the batch carries on.
pub(crate) const BATCH_WENT_WRONG: &str =
    "Something went wrong working on this file, and nothing from it was saved.";

/// A batch's row for a recording left when the speech engine was changed
/// part of the way through: a batch hears every recording on the engine it
/// started with, and never switches without saying so.
pub(crate) const BATCH_ENGINE_CHANGED: &str = "The speech engine was changed while these files \
                                               were being heard, so this one was not heard. \
                                               Choose or drop it again to hear it on the new engine.";

/// Said when a batch's thread could not be started.
pub(crate) const BATCH_COULD_NOT_START: &str = "Could not start working on these files. Try again.";

/// Said once Stop was pressed, until the file being heard lets go.
pub(crate) const BATCH_STOPPING: &str = "Stopping…";

/// A batch's line while it runs: how many files have finished, saved or
/// not, of how many. A batch has two files or more, so `{count}` is always
/// plural.
pub(crate) fn batch_working(finished: usize, count: usize) -> String {
    format!("{finished} of {count} files finished.")
}

/// A batch's line once it ended: how many transcripts were saved. Picks
/// between [`batch_all_saved`], [`BATCH_NONE_SAVED`], [`batch_one_saved`]
/// and [`batch_some_saved`], each a sentence of its own.
pub(crate) fn batch_ended(saved: usize, count: usize) -> String {
    match saved {
        0 => BATCH_NONE_SAVED.to_string(),
        s if s == count => batch_all_saved(count),
        1 => batch_one_saved(count),
        s => batch_some_saved(s, count),
    }
}

/// [`batch_ended`] when every file's transcript was saved.
fn batch_all_saved(count: usize) -> String {
    format!("All {count} transcripts were saved to your Downloads folder.")
}

/// [`batch_ended`] when none was.
const BATCH_NONE_SAVED: &str = "No transcript was saved.";

/// [`batch_ended`] when exactly one of several was.
fn batch_one_saved(count: usize) -> String {
    format!("1 of {count} transcripts was saved to your Downloads folder.")
}

/// [`batch_ended`] when two or more, but not all, were.
fn batch_some_saved(saved: usize, count: usize) -> String {
    format!("{saved} of {count} transcripts were saved to your Downloads folder.")
}

/// Above a summary written before the transcript was corrected by hand:
/// its quotes may no longer be in the turns they point to, and a new
/// summary brings it up to date. Shown until one is made.
pub(crate) const SUMMARY_OUT_OF_DATE: &str = "This summary was written before the transcript was \
     corrected, so some quotes may no longer match it. Summarise again to bring it up to date.";

// Find in the transcript, and correcting a turn by hand. The templates
// below carry named placeholders, `{current}` and the like, which the page
// fills with data only: a count, a speaker's name as the page shows it, or
// a time already written as minutes and seconds.

/// The search box above the transcript: its placeholder, and its label for
/// a screen reader.
pub(crate) const FIND: &str = "Find in the transcript";

/// Which match is shown, of how many: "{current} of {total}".
pub(crate) const FIND_COUNT: &str = "{current} of {total}";

/// Beside the search box when nothing in the transcript matches.
pub(crate) const FIND_NONE: &str = "No matches";

/// The button that goes to the match before the one shown.
pub(crate) const FIND_PREVIOUS: &str = "Previous";

/// The button that goes to the match after the one shown.
pub(crate) const FIND_NEXT: &str = "Next";

/// The button beside a turn that opens it for correcting.
pub(crate) const EDIT: &str = "Edit";

/// That button's label for a screen reader, which hears every turn's Edit
/// button alike otherwise, chosen by the turn's speaker as data: a live
/// meeting's [`YOU`] and [`OTHERS`] have entries of their own, any other
/// name came from a file and is said as it is, and a turn with no speaker
/// is named by its time alone. `time` is when the turn starts, minutes and
/// seconds, "3:07". Each arm is a whole entry.
pub(crate) fn edit_turn(speaker: Option<&str>, time: &str) -> String {
    match speaker {
        Some(YOU) => format!("Edit what you said at {time}"),
        Some(OTHERS) => format!("Edit what others said at {time}"),
        Some(name) => format!("Edit what {name} said at {time}"),
        None => format!("Edit the turn at {time}"),
    }
}

/// The text area a turn is corrected in, for a screen reader.
pub(crate) const EDIT_FIELD: &str = "The turn's words";

/// Leaves the turn as it was. Escape does the same.
pub(crate) const EDIT_CANCEL: &str = "Cancel";

/// A correction to nothing at all was refused: an empty turn would drop out
/// of a saved WebVTT file, and every summary quote after it would then
/// point at the wrong turn.
pub(crate) const EDIT_EMPTY: &str =
    "A turn cannot be left empty. Type what was said, or press Cancel.";

/// A correction was sent for a meeting that is no longer the one on the
/// page: it was cleared, or another file or meeting replaced it.
pub(crate) const EDIT_STALE: &str =
    "That transcript is no longer on the Files page, so the correction was not saved.";

/// A correction was sent for a turn the meeting does not have.
pub(crate) const EDIT_NO_SUCH_TURN: &str =
    "That turn is not in the transcript, so the correction was not saved.";

/// The meeting is the one held from the last quit, and its file could not
/// be written with the correction. Not made, so what is on the page and
/// what the next launch would offer back never differ.
pub(crate) const EDIT_HELD_NOT_WRITTEN: &str = "The correction could not be saved with the \
     meeting held from the last quit, so it was not made. Try again.";

/// The microphone's line while the meeting runs.
pub(crate) fn microphone_health(health: SignalHealth) -> &'static str {
    if health.signal {
        "Your microphone is hearing sound."
    } else {
        "No sound from your microphone yet."
    }
}

/// The call's line while the meeting runs, before [`NO_CALL_SOUND_YET`]
/// takes its place.
pub(crate) fn call_health(health: SignalHealth) -> &'static str {
    if health.signal {
        "The call's sound is coming through."
    } else {
        "Waiting for sound from the call."
    }
}

/// Said when the call has made no sound at all for a while
/// (`files::NO_CALL_SOUND_AFTER_MS`). A refused permission gives silence
/// and no error, so this is the only sign of one; it cannot tell a refusal
/// from a call where nobody has spoken, and says so.
pub(crate) const NO_CALL_SOUND_YET: &str = "No sound from the call yet. If someone on the call is \
                                            speaking, check that Macalla may record system audio, \
                                            under Privacy & Security in System Settings.";

/// Why a meeting hears the microphone alone: the call's sound could not be
/// opened. Said while it runs.
pub(crate) fn microphone_only(reason: &OpenError) -> &'static str {
    match reason {
        OpenError::Unsupported => {
            "Recording a call's sound works on macOS only for now, so this meeting hears your \
             microphone alone."
        }
        OpenError::OwnProcessUnknown
        | OpenError::CoreAudio { .. }
        | OpenError::Format(_)
        | OpenError::Chain(_) => {
            "The call's sound could not be opened, so this meeting hears your microphone alone."
        }
    }
}

/// Said when a meeting ended early and kept what it heard: why it ended,
/// and that its words are kept. Never the stopped-and-nothing-kept
/// sentence: this meeting has words.
pub(crate) fn ended_early(error: &Error) -> String {
    use macalla_platform::Error as Platform;
    match error {
        Error::StreamPanicked(_) => {
            "The meeting stopped early because something went wrong while hearing it. What was \
             heard until then is kept."
                .to_string()
        }
        Error::CallAudioFormatChanged => {
            "The meeting stopped early because the call's sound changed, as it does when \
             headphones connect. What was heard until then is kept."
                .to_string()
        }
        Error::Platform(Platform::DeviceLost(device)) => format!(
            "The meeting stopped early because {device} went away. What was heard until then is \
             kept."
        ),
        Error::Platform(_) => "The meeting stopped early because the microphone stopped \
                               working. What was heard until then is kept."
            .to_string(),
        Error::Core(macalla_core::Error::Stt { .. }) => {
            "The meeting stopped early because the speech engine stopped. What was heard until \
             then is kept."
                .to_string()
        }
        _ => "The meeting stopped early because something went wrong. What was heard until then \
              is kept."
            .to_string(),
    }
}

/// The engine's streams cannot share one instance, and a second would not
/// load. The engine is loaded, so the likely cause is memory.
pub(crate) const SECOND_ENGINE: &str = "The speech engine could not be loaded a second time for \
                                        the call, most likely because there is not enough \
                                        memory. Close other apps and try again, or choose \
                                        another engine under Transcription on the Models page.";

/// A stream panicked before anything was heard.
pub(crate) const WENT_WRONG_BEFORE_ANY_WORDS: &str =
    "Something went wrong while hearing the meeting, before anything was heard.";

/// The call's sound changed format before anything was heard.
pub(crate) const CALL_CHANGED_BEFORE_ANY_WORDS: &str =
    "The call's sound changed before anything was heard, so the meeting stopped. Start it again.";

/// The call's sound opened and would not start.
pub(crate) const CALL_WOULD_NOT_START: &str =
    "The call's sound could not be started, so the meeting did not start.";

/// Shown above a live meeting offered back at launch: the app quit while
/// it recorded, so what was heard was held until the person decides.
pub(crate) const HELD_FROM_QUIT: &str = "Macalla quit while this meeting was recording, so what \
                                         was heard was held for you. Copy it or save it to keep \
                                         it; Clear lets it go.";

/// Shown with a held meeting whose last piece was still being heard when
/// the quit's time ran out: what came before it is all there.
pub(crate) const LAST_PART_MISSING: &str =
    "The last part of the meeting may be missing: Macalla quit before it was heard.";

/// A live meeting was discarded.
pub(crate) const DISCARDED: &str = "Discarded. Nothing from the meeting was kept.";

/// A live meeting was stopped with no words in it.
pub(crate) const NOTHING_HEARD: &str = "Nothing was heard in this meeting.";

/// A live meeting ended with nothing kept: the sentence for `error`.
/// Pure: the caller logs.
pub(crate) fn not_kept(error: &Error) -> String {
    match error {
        Error::Stopped => DISCARDED.to_string(),
        Error::NothingHeard => NOTHING_HEARD.to_string(),
        other => crate::say::say(other),
    }
}

/// Start was pressed and no session is open to lend an engine.
pub(crate) const NO_ENGINE: &str = "No speech engine is ready to hear a meeting. Choose one \
                                    under Transcription on the Models page.";

/// The live meeting's thread panicked.
pub(crate) const WENT_WRONG: &str =
    "Something went wrong recording the meeting, and nothing was kept.";

/// The live meeting's thread could not be started.
pub(crate) const COULD_NOT_START: &str = "Could not start the meeting. Try again.";

/// The Files page's state could not be read (a poisoned lock).
pub(crate) const STATE_UNREADABLE: &str =
    "The Files page's state could not be read. Restart Macalla.";

/// A file, a summary or a second meeting was asked for while a live
/// meeting records.
pub(crate) const LIVE_BUSY: &str = "A live meeting is recording. Stop it first.";

/// A live meeting was asked for while a dictation runs.
pub(crate) const MEETING_DURING_DICTATION: &str = "Finish dictating first, then start the meeting.";

/// A dictation was asked for, by the shortcut, the tray or the overlay,
/// while a live meeting records. Said on the overlay and the page.
pub(crate) const DICTATION_DURING_MEETING: &str = "Dictation is off while a live meeting records. \
                                                   Stop the meeting on the Files page to dictate \
                                                   again.";

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_platform::system_audio::Step;

    /// Every error a live meeting can end early with and keep its words:
    /// the three it names, and the rest a source or an engine can give.
    fn ended_early_reasons() -> Vec<Error> {
        vec![
            Error::StreamPanicked("the tap fell over".into()),
            Error::CallAudioFormatChanged,
            Error::Platform(macalla_platform::Error::DeviceLost("USB Microphone".into())),
            Error::Platform(macalla_platform::Error::Permission {
                permission: macalla_platform::Permission::Microphone,
            }),
            Error::Platform(macalla_platform::Error::Audio("stream error".into())),
            Error::Core(macalla_core::Error::Stt {
                engine: "apple-speech",
                message: "the analyzer stopped".into(),
            }),
            Error::Core(macalla_core::Error::Audio("resampler".into())),
            Error::NothingHeard,
        ]
    }

    #[test]
    fn a_meeting_that_kept_its_words_never_says_nothing_was_kept() {
        let stopped = crate::say::say(&Error::Stopped);
        for error in ended_early_reasons() {
            let said = ended_early(&error);
            assert_ne!(said, stopped, "{error:?}");
            assert!(!said.contains("Nothing was kept"), "{error:?}: {said}");
            assert!(
                said.ends_with("What was heard until then is kept."),
                "{error:?}: {said}"
            );
        }
    }

    #[test]
    fn each_named_reason_says_what_happened_in_a_sentence_of_its_own() {
        let said: Vec<String> = [
            Error::StreamPanicked(String::new()),
            Error::CallAudioFormatChanged,
            Error::Platform(macalla_platform::Error::DeviceLost("USB Microphone".into())),
            Error::Platform(macalla_platform::Error::Audio("stream error".into())),
            Error::Core(macalla_core::Error::Stt {
                engine: "apple-speech",
                message: String::new(),
            }),
            Error::NothingHeard,
        ]
        .iter()
        .map(ended_early)
        .collect();
        let mut seen = std::collections::HashSet::new();
        for sentence in &said {
            assert!(
                seen.insert(sentence),
                "{sentence:?} is said for two reasons"
            );
        }
        assert!(said[2].contains("USB Microphone"), "the device is named");
        assert!(said[1].contains("call's sound changed"));
    }

    #[test]
    fn a_meeting_that_kept_nothing_says_why_in_its_own_words() {
        assert_eq!(not_kept(&Error::Stopped), DISCARDED);
        assert_eq!(not_kept(&Error::NothingHeard), NOTHING_HEARD);
        let second = Error::SecondEngine(macalla_core::Error::Stt {
            engine: "registry",
            message: "out of memory".into(),
        });
        assert_eq!(not_kept(&second), SECOND_ENGINE);
        assert_ne!(
            not_kept(&second),
            crate::say::say(&Error::NoEngine(macalla_core::Error::Stt {
                engine: "registry",
                message: String::new(),
            })),
            "not the no-engine sentence: the engine is loaded"
        );
        assert_eq!(
            not_kept(&Error::StreamPanicked(String::new())),
            WENT_WRONG_BEFORE_ANY_WORDS
        );
        assert_eq!(
            not_kept(&Error::CallAudioFormatChanged),
            CALL_CHANGED_BEFORE_ANY_WORDS
        );
        assert_eq!(
            not_kept(&Error::CallAudioStart(OpenError::CoreAudio {
                step: Step::Start,
                status: -1,
            })),
            CALL_WOULD_NOT_START
        );
    }

    #[test]
    fn every_reason_the_call_is_unheard_says_the_meeting_hears_the_microphone_alone() {
        let reasons = [
            OpenError::Unsupported,
            OpenError::OwnProcessUnknown,
            OpenError::CoreAudio {
                step: Step::CreateTap,
                status: -1,
            },
            OpenError::Chain(macalla_core::Error::Audio("resampler".into())),
        ];
        for reason in &reasons {
            assert!(
                microphone_only(reason).ends_with("hears your microphone alone."),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn the_speakers_are_shown_through_the_catalogue_and_other_names_as_they_are() {
        assert_eq!(speaker(YOU), "You");
        assert_eq!(speaker(OTHERS), "Others");
        assert_eq!(speaker("Aoife Byrne"), "Aoife Byrne");
    }

    #[test]
    fn a_quote_says_who_said_it_and_when_or_when_alone() {
        assert_eq!(quote_said_by("Aoife Byrne", "2:38"), "Aoife Byrne, 2:38");
        assert_eq!(quote_said_at("1:02:05"), "1:02:05");
    }

    /// The placeholders a template carries, in order.
    fn placeholders(template: &str) -> Vec<&str> {
        template
            .split('{')
            .skip(1)
            .filter_map(|rest| rest.split_once('}').map(|(name, _)| name))
            .collect()
    }

    #[test]
    fn an_edit_button_is_named_for_whose_turn_it_is_and_when() {
        assert_eq!(edit_turn(Some(YOU), "0:01"), "Edit what you said at 0:01");
        assert_eq!(
            edit_turn(Some(OTHERS), "0:05"),
            "Edit what others said at 0:05"
        );
        assert_eq!(
            edit_turn(Some("Aoife Byrne"), "1:02:05"),
            "Edit what Aoife Byrne said at 1:02:05"
        );
        assert_eq!(edit_turn(None, "3:07"), "Edit the turn at 3:07");
        // The live meeting's speakers are data, never put into the named
        // form: not "Edit what You said", nor "Edit what Others said".
        for speaker in [YOU, OTHERS] {
            assert_ne!(
                edit_turn(Some(speaker), "0:01"),
                format!("Edit what {speaker} said at 0:01")
            );
        }
    }

    #[test]
    fn each_template_carries_exactly_the_placeholders_the_page_fills() {
        assert_eq!(placeholders(FIND_COUNT), ["current", "total"]);
        for words in [
            FIND,
            FIND_NONE,
            FIND_PREVIOUS,
            FIND_NEXT,
            EDIT,
            EDIT_FIELD,
            SAVE,
            EDIT_CANCEL,
            SUMMARY_OUT_OF_DATE,
            EDIT_EMPTY,
            EDIT_STALE,
            EDIT_NO_SUCH_TURN,
            EDIT_HELD_NOT_WRITTEN,
        ] {
            assert!(placeholders(words).is_empty(), "{words:?}");
        }
    }

    #[test]
    fn a_batch_says_how_many_were_saved_in_a_sentence_for_each_count() {
        assert_eq!(batch_ended(0, 3), "No transcript was saved.");
        assert_eq!(
            batch_ended(1, 3),
            "1 of 3 transcripts was saved to your Downloads folder."
        );
        assert_eq!(
            batch_ended(2, 3),
            "2 of 3 transcripts were saved to your Downloads folder."
        );
        assert_eq!(
            batch_ended(3, 3),
            "All 3 transcripts were saved to your Downloads folder."
        );
        assert_eq!(batch_working(1, 4), "1 of 4 files finished.");
        assert_eq!(batch_hearing("0:05", "3:07"), "0:05 of 3:07 heard");
        assert_eq!(batch_saved("/a/b.txt"), "Saved to /a/b.txt.");
    }

    #[test]
    fn every_sentence_is_whole_and_no_two_are_the_same() {
        let all = [
            ABOUT,
            NO_CALL_SOUND_YET,
            SECOND_ENGINE,
            WENT_WRONG_BEFORE_ANY_WORDS,
            CALL_CHANGED_BEFORE_ANY_WORDS,
            CALL_WOULD_NOT_START,
            DISCARDED,
            NOTHING_HEARD,
            NO_ENGINE,
            WENT_WRONG,
            COULD_NOT_START,
            STATE_UNREADABLE,
            HELD_FROM_QUIT,
            LAST_PART_MISSING,
            LIVE_BUSY,
            MEETING_DURING_DICTATION,
            DICTATION_DURING_MEETING,
            TRANSCRIPT_AND_SUMMARY_GONE_UNLESS_MARKDOWN,
            SUMMARY_OUT_OF_DATE,
            EDIT_EMPTY,
            EDIT_STALE,
            EDIT_NO_SUCH_TURN,
            EDIT_HELD_NOT_WRITTEN,
            BATCH_ABOUT,
            BATCH_NOT_SAVED,
            BATCH_NOT_STARTED,
            BATCH_WENT_WRONG,
            BATCH_ENGINE_CHANGED,
            DOWNLOADS_NOT_FOUND,
            BATCH_COULD_NOT_START,
            BATCH_NONE_SAVED,
            COULD_NOT_SAVE_FILE,
        ];
        let mut seen = std::collections::HashSet::new();
        for sentence in all {
            assert!(seen.insert(sentence), "{sentence:?} is listed twice");
            assert!(sentence.ends_with('.'), "{sentence:?}");
            assert!(!sentence.contains("  "), "{sentence:?} has a double space");
        }
    }
}
