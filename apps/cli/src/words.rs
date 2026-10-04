// SPDX-License-Identifier: MIT
//! What the terminal says about the wait for Enhancement, each sentence
//! whole, with its placeholders named, in one place. The session hands over
//! typed reasons -- [`EnhancingWith`], [`EnhancementOutcome`] -- and nothing
//! here is glued together from fragments: only a provider's name, which is
//! data, fills a placeholder. Apple Intelligence and each kind of server have
//! sentences of their own, worded as the desktop overlay words them.

use macalla_platform::paste::NotDelivered;
use macalla_session::{providers, EnhancementOutcome, EnhancingWith, ENHANCEMENT_WAIT};
use std::time::Duration;

/// A provider's own name for its id, or the id as it is when no provider has
/// it.
fn provider_name(id: &str) -> &str {
    providers::find(id).map_or(id, |p| p.name)
}

/// Said when a paste could not be delivered: the text was never posted,
/// because without the grant no keystroke this process sends reaches any app.
/// One whole sentence per reason. The terminal is not History's surface, so
/// it claims nothing about where the text was kept: the text itself still
/// goes to stdout, and `--history` says the rest for itself.
pub fn could_not_paste(reason: NotDelivered) -> &'static str {
    match reason {
        NotDelivered::NotTrustedForAccessibility => {
            "not pasted -- macalla is not trusted for Accessibility, so the keystrokes it sends \
             go nowhere. System Settings > Privacy & Security > Accessibility"
        }
    }
}

/// Said about once a second while the paste waits for Enhancement.
pub fn enhancing(with: &EnhancingWith, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match with {
        EnhancingWith::AppleIntelligence => {
            format!("  enhancing with Apple Intelligence ({seconds} s)")
        }
        EnhancingWith::ServerOnThisComputer => {
            format!("  enhancing on this computer's server ({seconds} s)")
        }
        EnhancingWith::Server => format!("  enhancing on your own server ({seconds} s)"),
        EnhancingWith::Provider(id) => {
            let provider = provider_name(id);
            format!("  enhancing with {provider} ({seconds} s)")
        }
    }
}

/// Said once after a paste that went without Enhancement, or `None` when it
/// went with it, or Enhancement was not waited for.
pub fn pasted_without_enhancement(
    outcome: EnhancementOutcome,
    with: &EnhancingWith,
) -> Option<String> {
    use EnhancementOutcome::{Applied, Failed, OutOfTime, Skipped};
    let seconds = ENHANCEMENT_WAIT.as_secs();
    Some(match (outcome, with) {
        (Applied | Skipped, _) => return None,
        (OutOfTime, EnhancingWith::AppleIntelligence) => {
            format!("  pasted without enhancement: Apple Intelligence took longer than {seconds} s")
        }
        (Failed, EnhancingWith::AppleIntelligence) => {
            "  pasted without enhancement: Apple Intelligence didn't answer".to_string()
        }
        (OutOfTime, EnhancingWith::ServerOnThisComputer) => format!(
            "  pasted without enhancement: this computer's server took longer than {seconds} s"
        ),
        (Failed, EnhancingWith::ServerOnThisComputer) => {
            "  pasted without enhancement: this computer's server didn't answer".to_string()
        }
        (OutOfTime, EnhancingWith::Server) => {
            format!("  pasted without enhancement: your own server took longer than {seconds} s")
        }
        (Failed, EnhancingWith::Server) => {
            "  pasted without enhancement: your own server didn't answer".to_string()
        }
        (OutOfTime, EnhancingWith::Provider(id)) => {
            let provider = provider_name(id);
            format!("  pasted without enhancement: {provider} took longer than {seconds} s")
        }
        (Failed, EnhancingWith::Provider(id)) => {
            let provider = provider_name(id);
            format!("  pasted without enhancement: {provider} didn't answer")
        }
    })
}

/// Said as a live meeting starts: it hears the microphone alone, and why.
pub const LIVE_MICROPHONE_ONLY: &str = "live meeting from the microphone only -- run from a \
                                        terminal, macOS gives the call's sound to the terminal and \
                                        not to macalla, and it arrives as silence. The desktop app \
                                        records a call whole";

/// Said once the live meeting is recording.
pub const LIVE_RECORDING: &str = "recording -- press Enter or Ctrl-C to stop and print what was \
                                  heard, or Ctrl-C twice to throw it away";

/// Said after the turns of a live meeting that stopped early and kept what
/// it heard: why it stopped. One whole sentence per reason; a device's name,
/// which is data, is the only placeholder. The CLI's meeting has no call
/// audio, so only the microphone's stream and the engine can end it; any
/// other error gets the general sentence.
pub fn live_ended_early(error: &macalla_session::Error) -> String {
    use macalla_session::Error;
    match error {
        Error::StreamPanicked(_) => "  ! the meeting stopped early because something went wrong \
                                     while hearing it; what was heard until then is printed"
            .to_string(),
        Error::Platform(macalla_platform::Error::DeviceLost(device)) => format!(
            "  ! the meeting stopped early because {device} went away; what was heard until then \
             is printed"
        ),
        Error::Platform(_) => "  ! the meeting stopped early because the microphone stopped \
                               working; what was heard until then is printed"
            .to_string(),
        Error::Core(macalla_core::Error::Stt { .. }) => "  ! the meeting stopped early because \
                                                          the speech engine stopped; what was \
                                                          heard until then is printed"
            .to_string(),
        _ => "  ! the meeting stopped early because something went wrong; what was heard until \
              then is printed"
            .to_string(),
    }
}

/// A live meeting that kept nothing, for the reasons the CLI's meeting can
/// have and the error's own text does not say well; `None` for any other,
/// which is said as the error says itself. The CLI's meeting has no call
/// audio, so it never needs a second engine instance and never has a call
/// to fail: those reasons are not here.
pub fn live_not_kept(error: &macalla_session::Error) -> Option<&'static str> {
    use macalla_session::Error;
    match error {
        Error::Stopped => Some("discarded -- nothing from the meeting was kept"),
        Error::NothingHeard => Some("nothing was heard in this meeting"),
        Error::StreamPanicked(_) => {
            Some("something went wrong while hearing the meeting, before anything was heard")
        }
        _ => None,
    }
}

/// Said when `macalla transcribe` is given several files and nowhere to
/// write them.
pub const TRANSCRIBE_NEEDS_OUT_DIR: &str = "several files need --out-dir DIR: each transcript is \
                                            written there, one file per input, and nothing is \
                                            overwritten";

/// `macalla transcribe --format` in `--help`.
pub const TRANSCRIBE_FORMAT_HELP: &str =
    "The transcript's format: plain text, or Markdown with each turn's time. The CLI's Markdown \
     has the transcript and no summary. Markdown is heard as timed turns, as watch hears a file, \
     so its wording can differ slightly from the plain text of the same file.";

/// `--format txt` in `macalla transcribe --help`.
pub const TRANSCRIBE_FORMAT_TXT: &str = "Plain text, as dictation writes it";

/// `--format md` in `macalla transcribe --help`.
pub const TRANSCRIBE_FORMAT_MD: &str =
    "Markdown: the file's name, without its extension, as the title, then each turn with its time; no summary";

/// Said when `macalla transcribe --format md` is given with --json, --raw or
/// --cloud, before any engine is opened.
pub const TRANSCRIBE_MARKDOWN_ALONE: &str =
    "--format md cannot be used with --json, --raw or --cloud";

/// Said on stderr when one of several files could not be transcribed, and
/// the rest carry on: one whole sentence per reason. `{name}` is the file as
/// given, which is data.
pub fn transcribe_failed(name: &str, why: crate::NotTranscribed) -> String {
    use crate::NotTranscribed;
    match why {
        NotTranscribed::Missing => format!("{name}: not transcribed -- there is no file there"),
        NotTranscribed::NoFfmpeg => format!(
            "{name}: not transcribed -- it is not a wav or aiff file, and reading anything else \
             needs ffmpeg on the PATH"
        ),
        NotTranscribed::Undecodable => {
            format!("{name}: not transcribed -- it could not be read as audio")
        }
        NotTranscribed::Engine => {
            format!("{name}: not transcribed -- the speech engine could not hear it")
        }
        NotTranscribed::NotWritten => format!(
            "{name}: heard, but its transcript could not be written to the folder, and nothing \
             was left there"
        ),
    }
}

/// Said on stderr when one of several files had no speech in it, so no file
/// was written for it.
pub fn transcribe_no_speech(name: &str) -> String {
    format!("{name}: no speech was detected, so nothing was written")
}

/// The error `macalla transcribe` ends with when any of several files
/// failed: how many, of how many. One sentence for one file, another for
/// more.
pub fn transcribe_not_all(failed: usize, count: usize) -> String {
    if failed == 1 {
        format!("1 of {count} files was not transcribed")
    } else {
        format!("{failed} of {count} files were not transcribed")
    }
}

/// `macalla watch` in `--help`, one line.
pub const WATCH_ABOUT: &str = "Transcribe audio files as they land in a folder, until Ctrl-C.";

/// `macalla watch --help`, whole. The interval and the audio extensions are
/// those of `crate::watch`, and a test holds the two together.
pub const WATCH_LONG_ABOUT: &str = "Transcribe audio files as they land in a folder, until Ctrl-C.

The folder is looked at every 2 seconds, and only while this command runs: \
nothing watches it in between or afterwards. A file is finished once its size \
and modification time are the same at two looks in a row and it was last \
modified at least 10 seconds before; until then it is left alone. A copy that \
stalls for longer than that can still be taken part-way: its transcript is \
then partial, and once the copy finishes it is transcribed again into a -2 \
file. If the folder cannot be looked at 5 times in a row, the watch stops. \
Only the folder itself is looked at, not folders inside it. Files \
named .wav, .aif, .aiff, .caf, .flac, .mp3, .m4a, .mp4, .mov, .aac, .ogg, \
.opus or .webm are heard; hidden files, partial downloads (.part, \
.crdownload, .download) and anything else are left alone.

Each finished file is heard on the one engine and written to --out-dir as \
<name>.txt, .vtt, .srt or .md, never over a file already there. The Markdown \
has the transcript and no summary. Files already in \
the folder when the watch starts are transcribed too, unless they were done \
before. The folder and --out-dir must be different folders. Two watches must \
not share an --out-dir: nothing stops them, and between them a file can be \
transcribed twice, or skipped because the other noted a file of the same name.

What is done is noted in a file named .macalla-watch in --out-dir: each \
input's name, size and modification time, once its transcript is written, or \
once it was heard to the end with no speech in it. Starting the watch again \
skips every file noted there. A file that changes after it was done is \
transcribed again, into a new file; one replaced by a copy that keeps its size \
and modification time, as cp -p or rsync -t can, is not seen as a change and \
is not transcribed again. A file that fails is said on stderr and \
the rest carry on; it is tried again when it changes, or when the watch \
starts again.

For each file written, stdout gets a line \"<input>\\t<output>\"; with --json, \
stdout gets one JSON object per input instead: {\"input\", \"output\", \
\"error\", \"no_speech\"}, as `macalla transcribe` gives them.

Ctrl-C stops the watch: a file being heard is dropped and nothing is written \
for it, and every transcript already written is kept. A third Ctrl-C ends it \
at once. On Windows, Ctrl-C ends it at once. A watch that is stopped exits \
with status 0 whatever failed on the way, unlike `macalla transcribe`; each \
failure was said on stderr as it happened.";

/// `macalla watch`'s folder argument in `--help`.
pub const WATCH_DIR_HELP: &str = "The folder to watch for audio files.";

/// `macalla watch --out-dir` in `--help`.
pub const WATCH_OUT_DIR_HELP: &str =
    "Where each transcript is written, and the .macalla-watch note of what is done. Made if it \
     is not there.";

/// `macalla watch --format` in `--help`.
pub const WATCH_FORMAT_HELP: &str =
    "The transcript's format: plain text, WebVTT or SRT with each turn's times, or Markdown with \
     each turn's time and no summary.";

/// `--format txt` in `macalla watch --help`.
pub const WATCH_FORMAT_TXT: &str = "Plain text, one turn per paragraph";

/// `--format vtt` in `macalla watch --help`.
pub const WATCH_FORMAT_VTT: &str = "WebVTT, each turn with its times";

/// `--format srt` in `macalla watch --help`.
pub const WATCH_FORMAT_SRT: &str = "SRT, each turn with its times";

/// `--format md` in `macalla watch --help`.
pub const WATCH_FORMAT_MD: &str =
    "Markdown: the file's name, without its extension, as the title, then each turn with its time; no summary";

/// `macalla watch --json` in `--help`.
pub const WATCH_JSON_HELP: &str = "Report each input on stdout as one JSON object per line.";

/// Said on stderr as the watch starts. `{dir}` and `{out_dir}` are the
/// folders as given, and `{seconds}` the interval, all data.
pub fn watch_started(dir: &str, out_dir: &str, seconds: u64) -> String {
    format!(
        "watching {dir} every {seconds} s and writing transcripts to {out_dir} -- Ctrl-C to stop"
    )
}

/// Said when `macalla watch` is given a folder that is not there, or is not
/// a folder. `{dir}` is the folder as given.
pub fn watch_not_a_folder(dir: &str) -> String {
    format!("{dir} is not a folder, so there is nothing to watch")
}

/// Said when the watched folder and `--out-dir` are one folder.
pub const WATCH_SAME_FOLDER: &str = "the watched folder and --out-dir are the same folder; give \
                                     --out-dir a folder of its own, so transcripts never land \
                                     among the recordings";

/// Said when the note of what is done cannot be read as the watch starts.
/// `{path}` is the note's path.
pub fn watch_ledger_unreadable(path: &str) -> String {
    format!("{path} could not be read, so what was already done is not known")
}

/// Said when the watched folder could not be listed several times in a
/// row, and the watch ends. `{dir}` is the folder as given, and `{count}`
/// how many times in a row, never fewer than two.
pub fn watch_folder_gone(dir: &str, count: usize) -> String {
    format!("{dir} could not be looked at {count} times in a row, so the watch has stopped")
}

/// Said when `--out-dir` is not there and cannot be made. `{dir}` is the
/// folder as given.
pub fn watch_out_dir_not_made(dir: &str) -> String {
    format!("{dir} is not there and could not be made, so there is nowhere to write transcripts")
}

/// Said when a folder given to `macalla watch` cannot be followed to where
/// it really is, so it cannot be told apart from the other. `{dir}` is the
/// folder as given.
pub fn watch_folder_unresolved(dir: &str) -> String {
    format!(
        "{dir} could not be followed to where it is on this machine, so the watch cannot tell \
         whether it is the other folder"
    )
}

/// Said once as the watch starts, when no file in the folder is waiting.
pub const WATCH_WAITING_NONE: &str = "no file in the folder is waiting to be transcribed";

/// Said once as the watch starts, when one file in the folder is waiting.
pub const WATCH_WAITING_ONE: &str =
    "1 file already in the folder will be transcribed once it is finished";

/// Said once as the watch starts: how many files already in the folder are
/// not yet done, and will be transcribed. `{count}` is two or more.
pub fn watch_waiting_many(count: usize) -> String {
    format!("{count} files already in the folder will be transcribed once they are finished")
}

/// The sentence for `count` files waiting as the watch starts: one whole
/// entry for none, one for one, one for more.
pub fn watch_waiting(count: usize) -> String {
    match count {
        0 => WATCH_WAITING_NONE.to_string(),
        1 => WATCH_WAITING_ONE.to_string(),
        count => watch_waiting_many(count),
    }
}

/// Said once per file whose name is not UTF-8, which the watch skips.
/// `{name}` is the file, shown as near to its name as text allows.
pub fn watch_name_not_utf8(name: &str) -> String {
    format!(
        "{name}: skipped -- its name is not valid UTF-8, so it could not be noted as done; \
         renaming it lets the watch take it"
    )
}

/// Said on stderr when a transcript was written but could not be noted as
/// done. `{name}` is the input as given.
pub fn watch_not_noted(name: &str) -> String {
    format!(
        "{name}: written, but it could not be noted as done, so starting the watch again will \
         transcribe it again"
    )
}

/// Said on stderr when Ctrl-C stopped the watch while a file was being
/// heard. `{name}` is the input as given.
pub fn watch_stopped_during(name: &str) -> String {
    format!("{name}: stopped while it was being heard, so nothing was written for it")
}

/// Said on stderr as the watch ends on Ctrl-C.
pub const WATCH_STOPPED: &str = "stopped watching -- every transcript written is kept";

/// `macalla translate`'s own help.
pub const TRANSLATE_ABOUT: &str = "Translate a transcript file into another language, on this \
                                   Mac, turn by turn.";
pub const TRANSLATE_LONG_ABOUT: &str = "Translate a transcript file into another language, on \
this Mac, turn by turn.

Reads a .vtt, .srt or .txt transcript and prints a copy in --format: each turn keeps its times and \
its speaker, and only its words are translated. Translates from --language (en-US unless given) \
into --to. Uses Apple's on-device translation, so nothing is sent anywhere; it needs macOS 26 or \
later, and the language installed in System Settings > General > Language & Region > \
Translation Languages.";
pub const TRANSLATE_PATH_HELP: &str = "The transcript file: .vtt, .srt or .txt.";
pub const TRANSLATE_TO_HELP: &str =
    "The language to translate into, as a code, for example de or fr.";
pub const TRANSLATE_FORMAT_HELP: &str = "How to print the translated transcript.";

/// Said when `macalla translate` is given a recording.
pub const TRANSLATE_NEEDS_TRANSCRIPT: &str = "translate reads a transcript file (.vtt, .srt or \
                                              .txt), not a recording; save the recording's \
                                              transcript as WebVTT or SRT first";

/// Said on stderr as each turn is sent.
pub fn translating(current: usize, total: usize) -> String {
    format!("translating turn {current} of {total}")
}

/// Said when there is no translator, on a Mac: a macOS older than 26, or a
/// build without the `translate-apple` feature.
pub const TRANSLATE_UNAVAILABLE_MACOS: &str = "translating isn't available here: it needs macOS \
                                               26 or later, and a build of macalla with \
                                               translation in it";

/// Said when there is no translator, on Windows or Linux, which have none
/// yet.
pub const TRANSLATE_UNAVAILABLE_ELSEWHERE: &str =
    "translating isn't available on this platform yet";

/// Why `macalla translate` printed nothing. One whole sentence per reason;
/// the languages are codes, which are data.
pub fn translate_failed(error: &macalla_core::translate::TranslateError) -> String {
    use macalla_core::translate::{TranslateError, Unavailable};
    match error {
        TranslateError::Unavailable(Unavailable::NotInstalled { source, target }) => format!(
            "{target} isn't installed for translating from {source}. Add it in System Settings > \
             General > Language & Region > Translation Languages, then try again."
        ),
        TranslateError::Unavailable(Unavailable::UnsupportedPair { source, target }) => {
            format!("this Mac can't translate from {source} into {target}")
        }
        TranslateError::Unavailable(Unavailable::BackendUnavailable) => {
            if cfg!(target_os = "macos") {
                TRANSLATE_UNAVAILABLE_MACOS.to_string()
            } else {
                TRANSLATE_UNAVAILABLE_ELSEWHERE.to_string()
            }
        }
        TranslateError::Stopped => {
            "the translation was stopped, and nothing was printed".to_string()
        }
        TranslateError::Mismatch { .. } | TranslateError::Failed { .. } => {
            "the translation didn't finish, and nothing was printed".to_string()
        }
    }
}

/// A Markdown transcript's words, for [`macalla_core::markdown::write`]: the
/// same English as the desktop app's catalogue
/// (`apps/desktop/src-tauri/src/meeting_words.rs`: `KEY_POINTS`,
/// `DECISIONS`, `ACTION_ITEMS`, `MARKDOWN_TRANSCRIPT`,
/// `SUMMARY_OUT_OF_DATE`); change the two together. The CLI's Markdown has no
/// summary, so only the transcript's heading is ever written; the rest are
/// here because the writer takes them all.
pub const MARKDOWN: macalla_core::markdown::Words<'static> = macalla_core::markdown::Words {
    key_points: "Key points",
    decisions: "Decisions",
    action_items: "Action items",
    transcript: "Transcript",
    summary_out_of_date: "This summary was written before the transcript was corrected, so some \
                          quotes may no longer match it. Summarise again to bring it up to date.",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_meeting_that_kept_its_words_says_why_it_stopped_and_never_that_nothing_was_kept() {
        use macalla_session::Error;
        let reasons = [
            Error::StreamPanicked(String::new()),
            Error::Platform(macalla_platform::Error::DeviceLost("USB Microphone".into())),
            Error::Platform(macalla_platform::Error::Audio("stream error".into())),
            Error::Core(macalla_core::Error::Stt {
                engine: "apple-speech",
                message: String::new(),
            }),
            Error::NothingHeard,
        ];
        let said: Vec<String> = reasons.iter().map(live_ended_early).collect();
        let mut seen = std::collections::HashSet::new();
        for sentence in &said {
            assert!(
                seen.insert(sentence),
                "{sentence:?} is said for two reasons"
            );
            assert!(
                sentence.ends_with("what was heard until then is printed"),
                "{sentence}"
            );
            assert!(!sentence.contains("nothing"), "{sentence}");
            assert!(
                !sentence.contains("  ,") && !sentence.contains("  w"),
                "{sentence:?}"
            );
        }
        assert!(said[1].contains("USB Microphone"));
    }

    #[test]
    fn a_live_meeting_that_kept_nothing_says_why_in_its_own_words_or_as_the_error_does() {
        use macalla_session::Error;
        assert_eq!(
            live_not_kept(&Error::NothingHeard),
            Some("nothing was heard in this meeting")
        );
        assert!(live_not_kept(&Error::StreamPanicked(String::new()))
            .is_some_and(|said| said.contains("before anything was heard")));
        assert_eq!(
            live_not_kept(&Error::Platform(macalla_platform::Error::Audio(
                "no default input device".into()
            ))),
            None
        );
    }

    #[test]
    fn the_live_meeting_says_it_hears_the_microphone_alone() {
        assert!(LIVE_MICROPHONE_ONLY.contains("microphone only"));
        assert!(
            !LIVE_MICROPHONE_ONLY.contains("  "),
            "{LIVE_MICROPHONE_ONLY:?}"
        );
        assert!(LIVE_RECORDING.contains("Enter") && LIVE_RECORDING.contains("Ctrl-C twice"));
        assert!(!LIVE_RECORDING.contains("  "), "{LIVE_RECORDING:?}");
        assert_eq!(
            live_not_kept(&macalla_session::Error::Stopped),
            Some("discarded -- nothing from the meeting was kept")
        );
    }

    #[test]
    fn the_wait_names_who_it_waits_for_and_counts_whole_seconds() {
        assert_eq!(
            enhancing(
                &EnhancingWith::Provider("openai".into()),
                Duration::from_millis(2_400)
            ),
            "  enhancing with OpenAI (2 s)"
        );
        assert_eq!(
            enhancing(&EnhancingWith::AppleIntelligence, Duration::from_secs(1)),
            "  enhancing with Apple Intelligence (1 s)"
        );
        assert_eq!(
            enhancing(&EnhancingWith::Server, Duration::from_secs(3)),
            "  enhancing on your own server (3 s)"
        );
        assert_eq!(
            enhancing(&EnhancingWith::ServerOnThisComputer, Duration::from_secs(4)),
            "  enhancing on this computer's server (4 s)"
        );
        assert_eq!(
            enhancing(
                &EnhancingWith::Provider("no-such-provider".into()),
                Duration::from_secs(1)
            ),
            "  enhancing with no-such-provider (1 s)",
            "an id no provider has is shown as it is"
        );
    }

    #[test]
    fn a_paste_without_enhancement_says_why_once_and_one_with_it_says_nothing() {
        let openai = EnhancingWith::Provider("openai".into());
        let said = |outcome, with: &EnhancingWith| pasted_without_enhancement(outcome, with);
        assert_eq!(
            said(EnhancementOutcome::OutOfTime, &openai).as_deref(),
            Some("  pasted without enhancement: OpenAI took longer than 5 s")
        );
        assert_eq!(
            said(EnhancementOutcome::Failed, &openai).as_deref(),
            Some("  pasted without enhancement: OpenAI didn't answer")
        );
        assert_eq!(
            said(
                EnhancementOutcome::OutOfTime,
                &EnhancingWith::AppleIntelligence
            )
            .as_deref(),
            Some("  pasted without enhancement: Apple Intelligence took longer than 5 s")
        );
        assert_eq!(
            said(
                EnhancementOutcome::Failed,
                &EnhancingWith::AppleIntelligence
            )
            .as_deref(),
            Some("  pasted without enhancement: Apple Intelligence didn't answer")
        );
        assert_eq!(
            said(EnhancementOutcome::OutOfTime, &EnhancingWith::Server).as_deref(),
            Some("  pasted without enhancement: your own server took longer than 5 s")
        );
        assert_eq!(
            said(EnhancementOutcome::Failed, &EnhancingWith::Server).as_deref(),
            Some("  pasted without enhancement: your own server didn't answer")
        );
        assert_eq!(
            said(
                EnhancementOutcome::OutOfTime,
                &EnhancingWith::ServerOnThisComputer
            )
            .as_deref(),
            Some("  pasted without enhancement: this computer's server took longer than 5 s")
        );
        assert_eq!(
            said(
                EnhancementOutcome::Failed,
                &EnhancingWith::ServerOnThisComputer
            )
            .as_deref(),
            Some("  pasted without enhancement: this computer's server didn't answer")
        );
        assert_eq!(said(EnhancementOutcome::Applied, &openai), None);
        assert_eq!(
            said(EnhancementOutcome::Skipped, &openai),
            None,
            "not asked, or not waited for: nothing to explain"
        );
    }

    #[test]
    fn the_refusal_says_what_stopped_it_and_where_to_go() {
        let said = could_not_paste(NotDelivered::NotTrustedForAccessibility);
        assert!(said.contains("Accessibility"));
        assert!(said.contains("System Settings"));
    }
}
