// SPDX-License-Identifier: MIT
//! Translating the Files page's current meeting: a copy in another language,
//! kept beside the original. Humble: the rules are
//! `macalla_session::translate` and `macalla_core::translate`. This maps them
//! to commands and events, runs the work on a thread of its own, and keeps
//! the one copy in memory.
//!
//! - **The original is never changed.** The copy is a [`MeetingView`] of its
//!   own, so it is saved by the same [`FORMATS`] as the original.
//! - **The summary is not translated.** The copy has none, and its turns
//!   keep their times and speakers.
//! - **A translation is a job,** like a summary: one at a time, stopped by
//!   `meeting_cancel`, between turns. It holds no model, so quitting never
//!   waits for it.
//! - **One copy,** of the current meeting only. A new meeting, or Clear,
//!   drops it; a correction to the original marks it out of date.
//! - **Every call to the translator runs off the main thread.** Apple's
//!   framework finishes its work on the main queue, which the app's event
//!   loop serves; a call made on the main thread would block it and never
//!   return. So the commands that ask it are `async`, and a translation runs
//!   on its own thread.

use super::{
    save_new, transcript_text, vtt_turns, CommandError, Dictator, MeetingView, Meetings, OrSay,
    Running, FORMATS,
};
use crate::translate_words as words;
use macalla_core::translate::{Progress, TranslateError, Translator, Unavailable};
use macalla_session::meeting::MeetingTranscript;
use serde::Serialize;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State, Wry};

/// The translator the app was built with, chosen at its composition root
/// (`lib.rs`).
#[derive(Clone)]
pub struct Translation(pub Arc<dyn Translator>);

/// The translated copy kept beside the current meeting.
#[derive(Debug, Clone)]
pub(super) struct Copy {
    /// The meeting it translates.
    of_job: u64,
    /// That meeting's turns' words as they were translated, to tell a
    /// correction made since.
    translated_from: Vec<String>,
    source: String,
    target: String,
    view: MeetingView,
}

/// A translation running: its language, and the last progress it sent.
#[derive(Debug, Clone)]
pub(super) struct TranslationRun {
    target: String,
    progress: Option<TranslationProgressEvent>,
}

/// A catalogue sentence, with the languages its `{source}` and `{target}`
/// name, as BCP-47 codes for the page to name. A sentence with neither
/// ignores them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Said {
    pub text: &'static str,
    pub source: String,
    pub target: String,
}

/// The translated copy, on `meeting-translation` and from
/// `meeting_translation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranslationView {
    /// The meeting it translates, as `MeetingView::job`.
    pub job: u64,
    pub source: String,
    pub target: String,
    pub meeting: MeetingView,
    /// A turn of the original was corrected after this was translated.
    pub out_of_date: bool,
}

/// On `translation-progress`: `done` turns of `of`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranslationProgressEvent {
    /// The translation's own job, as `meeting_translate` returned it.
    pub job: u64,
    pub done: u64,
    pub of: u64,
}

/// On `translation-error`: the translation's job ended with nothing kept,
/// and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranslationFailedEvent {
    pub job: u64,
    pub said: Said,
}

/// A translation still running, for a page that opens while it works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunningTranslationView {
    pub job: u64,
    pub target: String,
    pub stopping: bool,
    pub progress: Option<TranslationProgressEvent>,
}

/// What the page needs to offer a translation: the language it would
/// translate from, the languages the translator lists, why there is no
/// translator at all when there is not, and the card's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranslationOptionsView {
    pub source: String,
    pub languages: Vec<String>,
    pub unavailable: Option<Said>,
    pub words: TranslationWordsView,
}

/// The translation card's words (`translate_words.rs`). `progress` has
/// `{current}` and `{total}`; `made` has `{source}` and `{target}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TranslationWordsView {
    pub heading: &'static str,
    pub about: &'static str,
    pub into: &'static str,
    pub translate: &'static str,
    pub stop: &'static str,
    pub progress: &'static str,
    pub stopping: &'static str,
    pub made: &'static str,
    pub out_of_date: &'static str,
    pub save: &'static str,
    /// `{path}`.
    pub saved: &'static str,
    pub copy: &'static str,
    pub copied: &'static str,
}

fn words_view() -> TranslationWordsView {
    TranslationWordsView {
        heading: words::HEADING,
        about: words::ABOUT,
        into: words::INTO,
        translate: words::TRANSLATE,
        stop: words::STOP,
        progress: words::PROGRESS,
        stopping: words::STOPPING,
        made: words::MADE,
        out_of_date: words::OUT_OF_DATE,
        save: words::SAVE,
        saved: words::SAVED,
        copy: words::COPY,
        copied: words::COPIED,
    }
}

/// Why there is no translator, as this platform says it: a Mac can get one
/// with a newer macOS; Windows and Linux have none yet.
#[cfg(target_os = "macos")]
const UNAVAILABLE: &str = words::UNAVAILABLE_MACOS;
#[cfg(not(target_os = "macos"))]
const UNAVAILABLE: &str = words::UNAVAILABLE_ELSEWHERE;

/// A reason a pair cannot be translated, as the page says it.
pub(super) fn said_unavailable(reason: &Unavailable) -> Said {
    let (text, source, target) = match reason {
        Unavailable::NotInstalled { source, target } => (words::NOT_INSTALLED, source, target),
        Unavailable::UnsupportedPair { source, target } => (words::UNSUPPORTED, source, target),
        Unavailable::BackendUnavailable => {
            return Said {
                text: UNAVAILABLE,
                source: String::new(),
                target: String::new(),
            }
        }
    };
    Said {
        text,
        source: source.clone(),
        target: target.clone(),
    }
}

/// Why a translation ended with nothing, as the page says it. A failure with
/// no sentence of its own is logged here, with the backend's own text.
pub(super) fn said_error(error: &TranslateError, source: &str, target: &str) -> Said {
    let text = match error {
        TranslateError::Unavailable(reason) => return said_unavailable(reason),
        TranslateError::Stopped => words::STOPPED,
        TranslateError::Mismatch { .. } | TranslateError::Failed { .. } => {
            tracing::warn!(%error, "a translation failed");
            words::FAILED
        }
    };
    Said {
        text,
        source: source.to_string(),
        target: target.to_string(),
    }
}

/// The copy's name, from the original's: the target before the extension,
/// so it saves as "{stem} ({target}) transcript {date}" beside the
/// original's "{stem} transcript {date}". The code is data, not words. A
/// name with no extension gets `.txt`, so the stem is never cut at the code.
fn copy_name(name: &str, target: &str) -> String {
    let path = std::path::Path::new(name);
    let extension = path
        .extension()
        .map_or_else(|| "txt".to_string(), |e| e.to_string_lossy().into_owned());
    let stem = path.file_stem().map_or_else(
        || name.to_string(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    format!("{stem} ({target}).{extension}")
}

/// Translate `current` from `source` into `target`, with no Tauri in it: the
/// copy to keep, or why there is none. The copy has the original's turns,
/// with only their words translated, and no summary, tidying note or held
/// note; `current` is never changed.
pub(super) fn run_translation(
    translator: &dyn Translator,
    current: &MeetingView,
    source: &str,
    target: &str,
    on_progress: &mut dyn FnMut(Progress),
    stop: &AtomicBool,
) -> Result<Copy, Said> {
    let meeting = MeetingTranscript {
        turns: vtt_turns(current),
        named_speakers: current.named_speakers,
        audio_ms: current.audio_ms,
    };
    let translated = macalla_session::translate::translate_meeting(
        translator,
        &meeting,
        source,
        target,
        on_progress,
        stop,
    )
    .map_err(|error| said_error(&error, source, target))?;
    let mut view = current.clone();
    for (turn, translated) in view.turns.iter_mut().zip(translated.meeting.turns) {
        turn.text = translated.text;
    }
    view.name = copy_name(&current.name, target);
    view.tidied = None;
    view.summary = None;
    view.summary_by = None;
    view.summary_out_of_date = false;
    view.ended_early = None;
    view.held = None;
    Ok(Copy {
        of_job: current.job,
        translated_from: current.turns.iter().map(|t| t.text.clone()).collect(),
        source: translated.source,
        target: translated.target,
        view,
    })
}

impl Copy {
    fn view_beside(&self, current: &MeetingView) -> TranslationView {
        TranslationView {
            job: self.of_job,
            source: self.source.clone(),
            target: self.target.clone(),
            meeting: self.view.clone(),
            out_of_date: current.turns.len() != self.translated_from.len()
                || current
                    .turns
                    .iter()
                    .zip(&self.translated_from)
                    .any(|(turn, was)| turn.text != *was),
        }
    }
}

impl Meetings {
    /// Start a translation of the current meeting into `target`, taken under
    /// the same lock, so a file finishing in between cannot change which
    /// meeting it is.
    fn begin_translation(
        &self,
        target: &str,
    ) -> Result<(u64, Arc<AtomicBool>, MeetingView), CommandError> {
        let mut inner = self
            .0
            .lock()
            .map_err(|_| crate::meeting_words::STATE_UNREADABLE)?;
        let current = inner
            .current
            .clone()
            .ok_or_else(|| CommandError::from(words::NOTHING_TO_TRANSLATE))?;
        let (job, stop) = Self::begin_in(&mut inner)?;
        if let Some(running) = inner.running.as_mut() {
            running.translation = Some(TranslationRun {
                target: target.to_string(),
                progress: None,
            });
        }
        Ok((job, stop, current))
    }

    fn translation_progressed(&self, job: u64, progress: TranslationProgressEvent) {
        if let Ok(mut inner) = self.0.lock() {
            if let Some(run) = inner
                .running
                .as_mut()
                .filter(|r| r.job == job)
                .and_then(|r| r.translation.as_mut())
            {
                run.progress = Some(progress);
            }
        }
    }

    /// Translation `job` has ended. Its copy is kept only if the meeting it
    /// translates is still the current one. Returns the copy as the page
    /// shows it, when it was kept.
    pub(super) fn end_translation(&self, job: u64, made: Option<Copy>) -> Option<TranslationView> {
        let mut inner = self.0.lock().ok()?;
        if inner.running.as_ref().is_some_and(|r| r.job == job) {
            inner.running = None;
        }
        let made = made?;
        let current = inner.current.as_ref().filter(|c| c.job == made.of_job)?;
        let view = made.view_beside(current);
        inner.translation = Some(made);
        Some(view)
    }

    /// The copy kept, as the page shows it.
    pub(super) fn translation(&self) -> Option<TranslationView> {
        let inner = self.0.lock().ok()?;
        let copy = inner.translation.as_ref()?;
        let current = inner.current.as_ref().filter(|c| c.job == copy.of_job)?;
        Some(copy.view_beside(current))
    }

    fn running_translation(&self) -> Option<RunningTranslationView> {
        let inner = self.0.lock().ok()?;
        let Running {
            job,
            stop,
            translation,
            ..
        } = inner.running.as_ref()?;
        let run = translation.as_ref()?;
        Some(RunningTranslationView {
            job: *job,
            target: run.target.clone(),
            stopping: stop.load(std::sync::atomic::Ordering::Relaxed),
            progress: run.progress.clone(),
        })
    }

    fn translated_copy(&self) -> Option<MeetingView> {
        let inner = self.0.lock().ok()?;
        let copy = inner.translation.as_ref()?;
        inner
            .current
            .as_ref()
            .filter(|c| c.job == copy.of_job)
            .map(|_| copy.view.clone())
    }
}

/// The language a translation translates from: the one dictation hears in,
/// which is what a recording on the page was heard in. The session's default
/// when no session is open.
fn source_language(dictator: &Dictator) -> String {
    dictator
        .meeting_tools()
        .and_then(|tools| tools.language().map(str::to_string))
        .unwrap_or_else(|| macalla_session::Config::default().language)
}

/// What the page needs to offer a translation. `async`: the translator is
/// asked, which must never happen on the main thread.
#[tauri::command(async)]
pub fn meeting_translation_options(
    translation: State<'_, Translation>,
    dictator: State<'_, Dictator>,
) -> TranslationOptionsView {
    let source = source_language(&dictator);
    let languages = translation.0.languages();
    let unavailable = languages
        .is_empty()
        .then(|| said_unavailable(&Unavailable::BackendUnavailable));
    TranslationOptionsView {
        source,
        languages,
        unavailable,
        words: words_view(),
    }
}

/// Whether the current meeting can be translated into `target`, and if not,
/// why: `None` when it can. `async`, as `meeting_translation_options`.
#[tauri::command(async)]
pub fn meeting_translation_check(
    translation: State<'_, Translation>,
    dictator: State<'_, Dictator>,
    target: String,
) -> Option<Said> {
    let source = source_language(&dictator);
    translation
        .0
        .availability(&source, &target)
        .err()
        .map(|reason| said_unavailable(&reason))
}

/// Translate the current meeting into `target` on a thread of its own.
/// Returns the job's number at once; the copy arrives as
/// `meeting-translation`, a failure or a stop as `translation-error`, and
/// progress, turn by turn, as `translation-progress`. Refused, with a
/// sentence, while any other job runs. Stopped by `meeting_cancel`.
///
/// Not `async`, for the reason `meeting_open_file` gives; it only starts the
/// thread, which is where the translator is asked.
#[tauri::command]
pub fn meeting_translate(
    app: AppHandle<Wry>,
    meetings: State<'_, Meetings>,
    dictator: State<'_, Dictator>,
    translation: State<'_, Translation>,
    target: String,
) -> Result<u64, CommandError> {
    let source = source_language(&dictator);
    let (job, stop, current) = meetings.begin_translation(&target)?;
    let meetings_for_thread = meetings.inner().clone();
    let translator = Arc::clone(&translation.0);
    let spawned = std::thread::Builder::new()
        .name("macalla-translation".into())
        .spawn(move || {
            let mut progress = |p: Progress| {
                let event = TranslationProgressEvent {
                    job,
                    done: p.done as u64,
                    of: p.of as u64,
                };
                meetings_for_thread.translation_progressed(job, event.clone());
                let _ = app.emit("translation-progress", event);
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_translation(
                    translator.as_ref(),
                    &current,
                    &source,
                    &target,
                    &mut progress,
                    &stop,
                )
            }))
            .unwrap_or_else(|_| {
                Err(Said {
                    text: words::FAILED,
                    source: source.clone(),
                    target: target.clone(),
                })
            });
            match outcome {
                Ok(copy) => match meetings_for_thread.end_translation(job, Some(copy)) {
                    Some(view) => {
                        let _ = app.emit("meeting-translation", view);
                    }
                    // The meeting was cleared or replaced meanwhile: the
                    // copy is of nothing on the page, and nothing is said.
                    None => {
                        let _ = app.emit("translation-ended", job);
                    }
                },
                Err(said) => {
                    meetings_for_thread.end_translation(job, None);
                    let _ = app.emit("translation-error", TranslationFailedEvent { job, said });
                }
            }
        });
    match spawned {
        Ok(thread) => {
            meetings.attach(job, thread);
            Ok(job)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not start a thread for a translation");
            meetings.end_translation(job, None);
            Err(words::COULD_NOT_START.into())
        }
    }
}

/// The copy kept, if any, for the page to show again when it opens or after
/// a correction to the original.
#[tauri::command]
pub fn meeting_translation(meetings: State<'_, Meetings>) -> Option<TranslationView> {
    meetings.translation()
}

/// The translation running, if any, for the page to show again when it
/// opens.
#[tauri::command]
pub fn meeting_running_translation(
    meetings: State<'_, Meetings>,
) -> Option<RunningTranslationView> {
    meetings.running_translation()
}

/// The translated copy as plain text, as Copy gives the original: one turn
/// per paragraph, with its speaker's name in front when it has one.
fn translation_text(meetings: &Meetings) -> Result<String, CommandError> {
    meetings
        .translated_copy()
        .map(|view| transcript_text(&view))
        .ok_or_else(|| CommandError::from(words::NOTHING_TO_COPY))
}

/// Put the translated copy's text on the clipboard, as `meeting_copy` does
/// for the meeting.
#[tauri::command]
pub fn meeting_copy_translation(meetings: State<'_, Meetings>) -> Result<(), CommandError> {
    use macalla_platform::paste::{Clipboard, SystemClipboard};
    let text = translation_text(&meetings)?;
    SystemClipboard::new().and_then(|mut clipboard| clipboard.write_text(&text))?;
    Ok(())
}

/// Save the translated copy to a new file in Downloads in `format`, an
/// extension from `meeting_live_words`' `formats`, as `meeting_save` saves
/// the original, and say where.
#[tauri::command]
pub fn meeting_save_translation(
    app: AppHandle<Wry>,
    meetings: State<'_, Meetings>,
    format: String,
) -> Result<String, CommandError> {
    use tauri::Manager;
    let format = FORMATS
        .iter()
        .find(|f| f.extension == format)
        .ok_or_else(|| CommandError::from(crate::meeting_words::COULD_NOT_SAVE_FILE))?;
    let view = meetings
        .translated_copy()
        .ok_or_else(|| CommandError::from(words::NOTHING_TO_SAVE))?;
    let dir = app
        .path()
        .download_dir()
        .or_say(crate::meeting_words::DOWNLOADS_NOT_FOUND)?;
    let saved = save_new(
        &dir,
        &view.name,
        &crate::dictionary::today(),
        format.extension,
        &(format.render)(&view),
    )
    .or_say(crate::meeting_words::COULD_NOT_SAVE_FILE)?;
    Ok(saved.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::super::{PartView, TurnView};
    use super::*;
    use macalla_core::translate::Flow;
    use std::sync::Mutex;

    /// Answers each text with `"{target}: {text}"`, or says the pair is
    /// unavailable.
    struct Fake {
        unavailable: Option<Unavailable>,
        answered: Mutex<usize>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                unavailable: None,
                answered: Mutex::new(0),
            }
        }
        fn refusing(reason: Unavailable) -> Self {
            Self {
                unavailable: Some(reason),
                ..Self::new()
            }
        }
    }

    impl Translator for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn languages(&self) -> Vec<String> {
            vec!["de".into(), "fr".into()]
        }
        fn availability(&self, _source: &str, _target: &str) -> Result<(), Unavailable> {
            self.unavailable.clone().map_or(Ok(()), Err)
        }
        fn translate(
            &self,
            _source: &str,
            target: &str,
            texts: &[String],
            each: &mut dyn FnMut(usize, String) -> Flow,
        ) -> Result<(), TranslateError> {
            for (index, text) in texts.iter().enumerate() {
                *self.answered.lock().unwrap() += 1;
                if each(index, format!("{target}: {text}")) == Flow::Stop {
                    break;
                }
            }
            Ok(())
        }
    }

    fn turn(speaker: &str, start_ms: u64, text: &str) -> TurnView {
        TurnView {
            speaker: Some(speaker.into()),
            label: Some(speaker.into()),
            start_ms,
            end_ms: start_ms + 1_000,
            text: text.into(),
            edit_label: format!("Edit {speaker}"),
        }
    }

    fn summarised_meeting() -> MeetingView {
        MeetingView {
            job: 7,
            revision: 2,
            name: "standup.vtt".into(),
            turns: vec![
                turn("Aoife", 0, "we ship on friday"),
                turn("Cian", 1_000, "who reviews the plan"),
            ],
            named_speakers: true,
            audio_ms: None,
            tidied: None,
            summary: Some(vec![PartView {
                start_ms: 0,
                end_ms: 2_000,
                key_points: Vec::new(),
                decisions: Vec::new(),
                action_items: Vec::new(),
                dropped: 0,
                error: None,
            }]),
            summary_by: Some("Apple Intelligence".into()),
            summary_out_of_date: false,
            ended_early: None,
            held: None,
        }
    }

    fn translate(translator: &dyn Translator, current: &MeetingView) -> Result<Copy, Said> {
        run_translation(
            translator,
            current,
            "en-US",
            "de",
            &mut |_| {},
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn the_copy_translates_the_words_keeps_times_and_speakers_and_has_no_summary() {
        let current = summarised_meeting();
        let copy = translate(&Fake::new(), &current).unwrap();
        assert_eq!(copy.view.turns.len(), 2);
        for (copied, turn) in copy.view.turns.iter().zip(&current.turns) {
            assert_eq!(copied.text, format!("de: {}", turn.text));
            assert_eq!(copied.speaker, turn.speaker);
            assert_eq!(copied.label, turn.label);
            assert_eq!(
                (copied.start_ms, copied.end_ms),
                (turn.start_ms, turn.end_ms)
            );
        }
        assert_eq!(copy.view.summary, None);
        assert_eq!(copy.view.summary_by, None);
        assert_eq!(copy.view.name, "standup (de).vtt");
        assert_eq!(
            (copy.source.as_str(), copy.target.as_str()),
            ("en-US", "de")
        );
    }

    #[test]
    fn the_copy_saves_in_every_format_from_its_own_words() {
        let copy = translate(&Fake::new(), &summarised_meeting()).unwrap();
        for format in FORMATS {
            let text = (format.render)(&copy.view);
            assert!(
                text.contains("de: we ship on friday"),
                "{}",
                format.extension
            );
            assert!(
                !text.contains("\nwe ship on friday"),
                "{}",
                format.extension
            );
        }
    }

    /// The Files page's state keeps the original as it was, and the copy
    /// beside it, whatever the translation did.
    #[test]
    fn keeping_a_copy_never_changes_the_original() {
        let meetings = Meetings::default();
        let original = summarised_meeting();
        meetings.end(0, Some(original.clone()));
        let (job, stop, current) = meetings.begin_translation("de").unwrap();
        let copy =
            run_translation(&Fake::new(), &current, "en-US", "de", &mut |_| {}, &stop).unwrap();
        let view = meetings.end_translation(job, Some(copy)).expect("kept");
        assert_eq!(meetings.current(), Some(original.clone()));
        assert_eq!(view.meeting.turns[0].text, "de: we ship on friday");
        assert!(!view.out_of_date);
        assert_eq!(meetings.translation(), Some(view));
    }

    #[test]
    fn copy_gives_the_translated_turns_as_plain_text() {
        let meetings = Meetings::default();
        let refused = translation_text(&meetings).unwrap_err();
        assert_eq!(refused.to_string(), words::NOTHING_TO_COPY);
        meetings.end(0, Some(summarised_meeting()));
        let (job, stop, current) = meetings.begin_translation("de").unwrap();
        let copy =
            run_translation(&Fake::new(), &current, "en-US", "de", &mut |_| {}, &stop).unwrap();
        meetings.end_translation(job, Some(copy));
        assert_eq!(
            translation_text(&meetings).unwrap(),
            "Aoife: de: we ship on friday\n\nCian: de: who reviews the plan"
        );
    }

    #[test]
    fn a_correction_to_the_original_marks_the_copy_out_of_date() {
        let meetings = Meetings::default();
        meetings.end(0, Some(summarised_meeting()));
        let (job, stop, current) = meetings.begin_translation("de").unwrap();
        let copy =
            run_translation(&Fake::new(), &current, "en-US", "de", &mut |_| {}, &stop).unwrap();
        meetings.end_translation(job, Some(copy));
        meetings.edit_turn(7, 0, "we ship on monday").unwrap();
        assert!(meetings.translation().unwrap().out_of_date);
    }

    #[test]
    fn a_new_meeting_or_a_clear_drops_the_copy() {
        let meetings = Meetings::default();
        meetings.end(0, Some(summarised_meeting()));
        let (job, stop, current) = meetings.begin_translation("de").unwrap();
        let copy =
            run_translation(&Fake::new(), &current, "en-US", "de", &mut |_| {}, &stop).unwrap();
        meetings.end_translation(job, Some(copy));
        meetings.clear();
        assert_eq!(meetings.translation(), None);
        assert_eq!(meetings.translated_copy(), None);
    }

    #[test]
    fn a_translation_is_one_job_and_refuses_a_second() {
        let meetings = Meetings::default();
        meetings.end(0, Some(summarised_meeting()));
        let (job, _, _) = meetings.begin_translation("de").unwrap();
        assert!(meetings.begin_translation("fr").is_err());
        let running = meetings.running_translation().unwrap();
        assert_eq!((running.job, running.target.as_str()), (job, "de"));
        meetings.end_translation(job, None);
        assert!(meetings.running_translation().is_none());
    }

    #[test]
    fn nothing_on_the_page_is_refused_with_its_sentence() {
        let refused = Meetings::default().begin_translation("de").unwrap_err();
        assert_eq!(refused.to_string(), words::NOTHING_TO_TRANSLATE);
    }

    /// Each typed reason reaches the page as its own catalogue sentence, with
    /// the languages it names as codes for the page to name.
    #[test]
    fn each_typed_reason_reaches_the_pages_words() {
        let cases = [
            (
                Unavailable::NotInstalled {
                    source: "en-US".into(),
                    target: "de".into(),
                },
                words::NOT_INSTALLED,
                "de",
            ),
            (
                Unavailable::UnsupportedPair {
                    source: "en-US".into(),
                    target: "ga".into(),
                },
                words::UNSUPPORTED,
                "ga",
            ),
            (Unavailable::BackendUnavailable, UNAVAILABLE, ""),
        ];
        for (reason, sentence, target) in cases {
            let fake = Fake::refusing(reason);
            let said = translate(&fake, &summarised_meeting()).unwrap_err();
            assert_eq!(said.text, sentence);
            assert_eq!(said.target, target);
            assert_eq!(*fake.answered.lock().unwrap(), 0, "nothing was sent");
        }
        assert!(words::NOT_INSTALLED.contains("Translation Languages"));
        assert!(
            words::NOT_INSTALLED.contains("{target}") && words::NOT_INSTALLED.contains("{source}")
        );
        assert!(words::UNSUPPORTED.contains("{target}") && words::UNSUPPORTED.contains("{source}"));
    }

    /// Both platforms' sentences exist and differ, and this build picks its
    /// own: only one of them can run here, so the other is checked as data.
    #[test]
    fn no_translator_is_worded_for_the_platform() {
        assert!(words::UNAVAILABLE_MACOS.contains("macOS 26"));
        assert!(!words::UNAVAILABLE_ELSEWHERE.contains("macOS"));
        assert_ne!(words::UNAVAILABLE_MACOS, words::UNAVAILABLE_ELSEWHERE);
        let said = said_unavailable(&Unavailable::BackendUnavailable);
        if cfg!(target_os = "macos") {
            assert_eq!(said.text, words::UNAVAILABLE_MACOS);
        } else {
            assert_eq!(said.text, words::UNAVAILABLE_ELSEWHERE);
        }
    }

    #[test]
    fn a_stop_says_so_and_a_failure_says_nothing_was_kept() {
        let stop = AtomicBool::new(true);
        let said = run_translation(
            &Fake::new(),
            &summarised_meeting(),
            "en-US",
            "de",
            &mut |_| {},
            &stop,
        )
        .unwrap_err();
        assert_eq!(said.text, words::STOPPED);
        let failed = said_error(
            &TranslateError::Failed {
                backend: "fake",
                message: "internalError".into(),
            },
            "en-US",
            "de",
        );
        assert_eq!(failed.text, words::FAILED);
    }

    #[test]
    fn a_name_with_no_extension_keeps_its_whole_stem() {
        assert_eq!(copy_name("Live meeting", "de"), "Live meeting (de).txt");
        assert_eq!(copy_name("team.sync.vtt", "fr"), "team.sync (fr).vtt");
    }
}
