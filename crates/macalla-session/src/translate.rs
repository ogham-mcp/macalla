// SPDX-License-Identifier: MIT
//! A meeting translated into another language: a copy, turn by turn.
//!
//! - **The original is never touched.** The meeting is borrowed, and what
//!   comes back is a new [`MeetingTranscript`].
//! - **Each turn is translated on its own** and keeps its speaker and its
//!   times; only its words change. A speaker's name is data and is not
//!   translated.
//! - **A summary is not translated.** It belongs to the app's view of the
//!   meeting, not to its turns, and never reaches this.
//! - **Progress is turns, and Stop is asked between turns** (the core's
//!   [`translate_all`]). Apple's backend answers about one turn every 0.7 s
//!   on the reference Mac, so a long meeting takes minutes and must be
//!   stoppable. A stopped translation keeps nothing.
//!
//! The translator is the app's to choose and build; this crate only uses the
//! port. Reasons it cannot translate are typed ([`TranslateError`]), for each
//! app to word.

use crate::meeting::MeetingTranscript;
use macalla_core::translate::{translate_all, Progress, TranslateError, Translator};
use std::sync::atomic::AtomicBool;

/// A translated copy of a meeting, and the languages it went between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedMeeting {
    pub meeting: MeetingTranscript,
    /// BCP-47 codes, as given.
    pub source: String,
    pub target: String,
}

/// Translate `meeting` from `source` into `target` with `translator`, turn by
/// turn. `on_progress` hears how many turns are done, of how many; `stop` is
/// asked between turns.
pub fn translate_meeting(
    translator: &dyn Translator,
    meeting: &MeetingTranscript,
    source: &str,
    target: &str,
    on_progress: &mut dyn FnMut(Progress),
    stop: &AtomicBool,
) -> Result<TranslatedMeeting, TranslateError> {
    let texts: Vec<String> = meeting.turns.iter().map(|turn| turn.text.clone()).collect();
    let translated = translate_all(translator, source, target, &texts, on_progress, stop)?;
    let mut copy = meeting.clone();
    for (turn, text) in copy.turns.iter_mut().zip(translated) {
        turn.text = text;
    }
    Ok(TranslatedMeeting {
        meeting: copy,
        source: source.to_string(),
        target: target.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::translate::{Flow, Unavailable};
    use macalla_core::vtt::Turn;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    /// Answers each text with its words upper-cased and the target in front,
    /// and counts what it answered.
    #[derive(Default)]
    struct Shouting {
        answered: Mutex<usize>,
        unavailable: Option<Unavailable>,
    }

    impl Translator for Shouting {
        fn id(&self) -> &'static str {
            "shouting"
        }
        fn languages(&self) -> Vec<String> {
            vec!["en".into(), "de".into()]
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
                if each(index, format!("[{target}] {}", text.to_uppercase())) == Flow::Stop {
                    break;
                }
            }
            Ok(())
        }
    }

    fn meeting() -> MeetingTranscript {
        let turn = |speaker: Option<&str>, start_ms, end_ms, text: &str| Turn {
            speaker: speaker.map(str::to_string),
            start_ms,
            end_ms,
            text: text.to_string(),
        };
        MeetingTranscript {
            turns: vec![
                turn(Some("Aoife"), 0, 2_000, "we ship on friday"),
                turn(Some("Others"), 2_000, 5_500, "who reviews the plan"),
                turn(None, 5_500, 9_000, "thursday works"),
            ],
            named_speakers: true,
            audio_ms: Some(9_000),
        }
    }

    fn run(
        translator: &dyn Translator,
        meeting: &MeetingTranscript,
    ) -> Result<TranslatedMeeting, TranslateError> {
        translate_meeting(
            translator,
            meeting,
            "en-US",
            "de",
            &mut |_| {},
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn every_turn_keeps_its_speaker_and_times_and_only_its_words_change() {
        let original = meeting();
        let translated = run(&Shouting::default(), &original).unwrap();
        assert_eq!(translated.source, "en-US");
        assert_eq!(translated.target, "de");
        assert_eq!(translated.meeting.turns.len(), original.turns.len());
        for (copy, turn) in translated.meeting.turns.iter().zip(&original.turns) {
            assert_eq!(copy.speaker, turn.speaker);
            assert_eq!((copy.start_ms, copy.end_ms), (turn.start_ms, turn.end_ms));
            assert_eq!(copy.text, format!("[de] {}", turn.text.to_uppercase()));
        }
        assert_eq!(translated.meeting.named_speakers, original.named_speakers);
        assert_eq!(translated.meeting.audio_ms, original.audio_ms);
    }

    #[test]
    fn the_original_meeting_is_never_touched() {
        let original = meeting();
        let before = original.clone();
        let translated = run(&Shouting::default(), &original).unwrap();
        assert_eq!(original, before);
        assert_ne!(translated.meeting, original);
    }

    #[test]
    fn a_stop_between_turns_keeps_nothing_and_sends_no_more() {
        let original = meeting();
        let translator = Shouting::default();
        let stop = AtomicBool::new(false);
        let error = translate_meeting(
            &translator,
            &original,
            "en-US",
            "de",
            &mut |p| {
                if p.done == 1 {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            &stop,
        )
        .unwrap_err();
        assert_eq!(error, TranslateError::Stopped);
        assert_eq!(*translator.answered.lock().unwrap(), 1);
        assert_eq!(original, meeting());
    }

    #[test]
    fn a_pair_that_cannot_be_translated_says_why_and_translates_nothing() {
        let translator = Shouting {
            unavailable: Some(Unavailable::UnsupportedPair {
                source: "en-US".into(),
                target: "ga".into(),
            }),
            ..Shouting::default()
        };
        let error = run(&translator, &meeting()).unwrap_err();
        assert!(matches!(
            error,
            TranslateError::Unavailable(Unavailable::UnsupportedPair { .. })
        ));
        assert_eq!(*translator.answered.lock().unwrap(), 0);
    }

    #[test]
    fn progress_is_counted_in_turns() {
        let mut seen = Vec::new();
        translate_meeting(
            &Shouting::default(),
            &meeting(),
            "en-US",
            "de",
            &mut |p| seen.push((p.done, p.of)),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(seen, vec![(0, 3), (1, 3), (2, 3), (3, 3)]);
    }
}
