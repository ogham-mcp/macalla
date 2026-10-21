// SPDX-License-Identifier: MIT
//! The translation port, and the rules every translator is held to.
//!
//! Translation is not enhancement. [`crate::enhance::Enhancer`] refines text
//! in the language it was spoken in, behind a guard that keeps its words;
//! translation changes every word, so it has a port of its own and a
//! different check: what comes back must be exactly one translation per text
//! sent, each matched to its text by index. A backend only turns a batch into its own request and
//! streams each answer back; [`translate_all`] owns the rest: that 1:1 rule,
//! stopping between texts, progress, and leaving blank texts alone.
//!
//! Languages are BCP-47 codes (`en-US`, `de`, `ga`), which are data. Nothing
//! here names a language for a person, and nothing here is a sentence: a
//! translator that cannot serve says why with an [`Unavailable`], and each app
//! turns that into its own words.
//!
//! No I/O here, and no backend: Apple's on-device translation lives in
//! `macalla-translators`.

use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

/// Why a translator cannot translate from one language into another. Typed,
/// never words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// The backend can translate this pair, but its language files are not
    /// on this machine yet. The person can add them; a program cannot.
    NotInstalled { source: String, target: String },
    /// The backend cannot translate this pair at all, whatever is installed:
    /// either language, or the two together.
    UnsupportedPair { source: String, target: String },
    /// There is no working backend: not built into this app, not this
    /// operating system, or an operating system too old for it.
    BackendUnavailable,
}

/// Why a translation did not finish. The `Display` text is for logs and
/// developers; an app says each case in its own words.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TranslateError {
    #[error("translation is unavailable: {0:?}")]
    Unavailable(Unavailable),
    /// Stopped between texts, as asked. Nothing is kept.
    #[error("translation was stopped")]
    Stopped,
    /// The backend broke the 1:1 rule: an answer named a text that was not
    /// sent, a text was answered twice, or a text was left unanswered. Nothing is kept,
    /// because a translation on the wrong turn is worse than none.
    #[error("translator {backend} broke the 1:1 rule: sent {sent} texts, got {got} answers")]
    Mismatch {
        backend: &'static str,
        sent: usize,
        got: usize,
    },
    /// The backend failed for a reason it has no case for. `message` is the
    /// backend's own text, for the log.
    #[error("translator {backend}: {message}")]
    Failed {
        backend: &'static str,
        message: String,
    },
}

impl From<Unavailable> for TranslateError {
    fn from(reason: Unavailable) -> Self {
        Self::Unavailable(reason)
    }
}

/// What the caller of [`Translator::translate`] wants after each answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Send no more, and return as soon as the backend can.
    Stop,
}

/// A backend that translates text between two languages.
pub trait Translator: Send + Sync {
    /// A stable id for logs, such as `apple-translation`.
    fn id(&self) -> &'static str;

    /// The languages the backend can translate between, as BCP-47 codes, in
    /// no particular order. Empty when the backend is unavailable. A language
    /// being listed says nothing about whether a pair is installed: ask
    /// [`Translator::availability`].
    fn languages(&self) -> Vec<String>;

    /// Whether `source` into `target` can be translated right now, and if
    /// not, why. Cheap enough to ask whenever a person picks a language.
    fn availability(&self, source: &str, target: &str) -> Result<(), Unavailable>;

    /// Translate each of `texts` from `source` into `target`, and hand each
    /// translation to `each` with the index of the text it translates, as it
    /// arrives, in whatever order the backend answers. Exactly one answer per
    /// text. When `each` says
    /// [`Flow::Stop`], send no more and return. A backend that cannot serve
    /// the pair returns [`TranslateError::Unavailable`] with the reason.
    ///
    /// Callers use [`translate_all`], which holds a backend to that
    /// contract, rather than calling this themselves.
    fn translate(
        &self,
        source: &str,
        target: &str,
        texts: &[String],
        each: &mut dyn FnMut(usize, String) -> Flow,
    ) -> Result<(), TranslateError>;
}

/// How many texts [`translate_all`] has back, of how many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub of: usize,
}

/// The language to translate from, given the one the text was heard in:
/// `source` itself when its pair with `target` is ready, or else another
/// variant of the same language whose pair is installed, such as `en-GB` for
/// `en-US`. Which English, or which Portuguese, does not change the
/// translation enough to send the person to System Settings for a second
/// copy. `source` when no variant is installed either, so the caller reports
/// the language it was asked about.
pub fn installed_source(translator: &dyn Translator, source: &str, target: &str) -> String {
    if translator.availability(source, target).is_ok() {
        return source.to_string();
    }
    let base = |code: &str| {
        code.split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let wanted = base(source);
    translator
        .languages()
        .into_iter()
        .filter(|code| code != source && base(code) == wanted)
        .find(|code| translator.availability(code, target).is_ok())
        .unwrap_or_else(|| source.to_string())
}

/// Translate `texts` from `source` into `target` with `translator`: one
/// translation per text, returned in the order of `texts`, or an error and
/// nothing.
///
/// - **1:1, by index.** Answers may arrive in any order; each goes into the
///   place of the text its index names, and the result is in the order of
///   `texts`. An index out of range, a second answer for one text, or a text
///   left without an answer is [`TranslateError::Mismatch`], and nothing is
///   returned.
/// - **Blank texts stay as they are** and are never sent: a backend may
///   refuse one (Apple's has a case for "nothing to translate"), and there is
///   nothing to translate in it.
/// - **Stop is asked between texts.** Once `stop` is set, the backend is told
///   to send no more at its next answer, and the result is
///   [`TranslateError::Stopped`] even if every answer came back.
/// - **Availability is asked first,** so an unavailable pair sends nothing.
///
/// `on_progress` hears once before the first text is sent and once per
/// answer, counting every text, blank ones included. It counts answers
/// received, not which text an answer was for.
pub fn translate_all(
    translator: &dyn Translator,
    source: &str,
    target: &str,
    texts: &[String],
    on_progress: &mut dyn FnMut(Progress),
    stop: &AtomicBool,
) -> Result<Vec<String>, TranslateError> {
    if stop.load(Ordering::Relaxed) {
        return Err(TranslateError::Stopped);
    }
    translator.availability(source, target)?;
    let sent: Vec<usize> = texts
        .iter()
        .enumerate()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(index, _)| index)
        .collect();
    let blanks = texts.len() - sent.len();
    let of = texts.len();
    on_progress(Progress { done: blanks, of });
    let batch: Vec<String> = sent.iter().map(|&index| texts[index].clone()).collect();
    // One slot per text sent, filled by the index each answer carries, in
    // whatever order the answers come: nothing promises a backend answers in
    // the order asked (Apple's `translate(batch:)` does not say it does,
    // which is why each request carries its own id).
    let mut answers: Vec<Option<String>> = vec![None; batch.len()];
    let mut received = 0;
    let mut broken = false;
    if !batch.is_empty() {
        translator.translate(source, target, &batch, &mut |index, text| {
            match answers.get_mut(index) {
                Some(slot @ None) if !broken => *slot = Some(text),
                // Out of range, or a second answer for one text.
                _ => {
                    broken = true;
                    return Flow::Stop;
                }
            }
            // Progress counts answers received, never the index of the one
            // that came, so it moves by one per answer whatever the order.
            received += 1;
            on_progress(Progress {
                done: blanks + received,
                of,
            });
            if stop.load(Ordering::Relaxed) {
                Flow::Stop
            } else {
                Flow::Continue
            }
        })?;
    }
    if stop.load(Ordering::Relaxed) {
        return Err(TranslateError::Stopped);
    }
    let answered = answers.iter().filter(|slot| slot.is_some()).count();
    if broken || answered != batch.len() {
        return Err(TranslateError::Mismatch {
            backend: translator.id(),
            sent: batch.len(),
            got: received,
        });
    }
    let mut translated = texts.to_vec();
    for (index, answer) in sent.into_iter().zip(answers.into_iter().flatten()) {
        translated[index] = answer;
    }
    Ok(translated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A translator that answers each text with `"{target}:{text}"`, so a test
    /// can see which text each answer belongs to. `order` is the order it
    /// answers the texts in, by index, repeats and strangers allowed, to break
    /// the 1:1 rule on purpose; `drop_last` leaves the last one out.
    struct Fake {
        availability: Result<(), Unavailable>,
        order: Option<Vec<usize>>,
        drop_last: bool,
        sent: Mutex<Vec<Vec<String>>>,
        answered: Mutex<usize>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                availability: Ok(()),
                order: None,
                drop_last: false,
                sent: Mutex::default(),
                answered: Mutex::default(),
            }
        }
    }

    impl Translator for Fake {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn languages(&self) -> Vec<String> {
            vec!["en".into(), "de".into()]
        }

        fn availability(&self, _source: &str, _target: &str) -> Result<(), Unavailable> {
            self.availability.clone()
        }

        fn translate(
            &self,
            _source: &str,
            target: &str,
            texts: &[String],
            each: &mut dyn FnMut(usize, String) -> Flow,
        ) -> Result<(), TranslateError> {
            self.sent.lock().unwrap().push(texts.to_vec());
            let order: Vec<usize> = self
                .order
                .clone()
                .unwrap_or_else(|| (0..texts.len() - usize::from(self.drop_last)).collect());
            for index in order {
                *self.answered.lock().unwrap() += 1;
                let text = texts.get(index).map_or("stranger", String::as_str);
                if each(index, format!("{target}:{text}")) == Flow::Stop {
                    break;
                }
            }
            Ok(())
        }
    }

    fn texts(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn run(fake: &Fake, input: &[String]) -> Result<Vec<String>, TranslateError> {
        translate_all(
            fake,
            "en",
            "de",
            input,
            &mut |_| {},
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn every_text_comes_back_translated_in_its_own_place() {
        let input = texts(&["one", "two", "three"]);
        let out = run(&Fake::new(), &input).unwrap();
        assert_eq!(out, texts(&["de:one", "de:two", "de:three"]));
    }

    #[test]
    fn answers_out_of_order_come_back_in_the_order_asked() {
        let fake = Fake {
            order: Some(vec![2, 0, 1]),
            ..Fake::new()
        };
        let out = run(&fake, &texts(&["one", "two", "three"])).unwrap();
        assert_eq!(out, texts(&["de:one", "de:two", "de:three"]));
    }

    #[test]
    fn a_second_answer_for_one_text_is_a_mismatch_and_nothing_is_kept() {
        let fake = Fake {
            order: Some(vec![0, 1, 1, 2]),
            ..Fake::new()
        };
        let error = run(&fake, &texts(&["one", "two", "three"])).unwrap_err();
        assert!(
            matches!(error, TranslateError::Mismatch { sent: 3, .. }),
            "{error:?}"
        );
        assert_eq!(*fake.answered.lock().unwrap(), 3, "stopped at the repeat");
    }

    #[test]
    fn an_answer_for_a_text_never_sent_is_a_mismatch() {
        let fake = Fake {
            order: Some(vec![0, 7, 1, 2]),
            ..Fake::new()
        };
        let error = run(&fake, &texts(&["one", "two", "three"])).unwrap_err();
        assert!(
            matches!(error, TranslateError::Mismatch { sent: 3, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_text_left_unanswered_among_answers_out_of_order_is_a_mismatch() {
        let fake = Fake {
            order: Some(vec![2, 0]),
            ..Fake::new()
        };
        let error = run(&fake, &texts(&["one", "two", "three"])).unwrap_err();
        assert!(
            matches!(
                error,
                TranslateError::Mismatch {
                    sent: 3,
                    got: 2,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    /// Progress is a count of answers, so it rises by one per answer and the
    /// stop still lands between texts when the answers come out of order.
    #[test]
    fn out_of_order_answers_count_up_one_by_one_and_still_stop_between_texts() {
        let fake = Fake {
            order: Some(vec![3, 1, 0, 2]),
            ..Fake::new()
        };
        let stop = AtomicBool::new(false);
        let mut seen = Vec::new();
        let error = translate_all(
            &fake,
            "en",
            "de",
            &texts(&["one", "two", "three", "four"]),
            &mut |p| {
                seen.push(p.done);
                if p.done == 2 {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            &stop,
        )
        .unwrap_err();
        assert_eq!(error, TranslateError::Stopped);
        assert_eq!(seen, vec![0, 1, 2]);
        assert_eq!(*fake.answered.lock().unwrap(), 2);
    }

    #[test]
    fn an_answer_missing_is_a_mismatch_and_nothing_is_kept() {
        let fake = Fake {
            drop_last: true,
            ..Fake::new()
        };
        let error = run(&fake, &texts(&["one", "two", "three"])).unwrap_err();
        assert_eq!(
            error,
            TranslateError::Mismatch {
                backend: "fake",
                sent: 3,
                got: 2
            }
        );
    }

    #[test]
    fn blank_texts_are_never_sent_and_stay_where_they_were() {
        let fake = Fake::new();
        let out = run(&fake, &texts(&["one", "  ", "three"])).unwrap();
        assert_eq!(out, texts(&["de:one", "  ", "de:three"]));
        assert_eq!(*fake.sent.lock().unwrap(), vec![texts(&["one", "three"])]);
    }

    #[test]
    fn nothing_but_blanks_sends_nothing() {
        let fake = Fake::new();
        assert_eq!(run(&fake, &texts(&["", " "])).unwrap(), texts(&["", " "]));
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn each_typed_reason_comes_back_as_it_is_and_nothing_is_sent() {
        for reason in [
            Unavailable::NotInstalled {
                source: "en".into(),
                target: "de".into(),
            },
            Unavailable::UnsupportedPair {
                source: "en".into(),
                target: "ga".into(),
            },
            Unavailable::BackendUnavailable,
        ] {
            let fake = Fake {
                availability: Err(reason.clone()),
                ..Fake::new()
            };
            let error = run(&fake, &texts(&["one"])).unwrap_err();
            assert_eq!(error, TranslateError::Unavailable(reason));
            assert!(fake.sent.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn a_stop_between_texts_stops_the_backend_and_keeps_nothing() {
        let fake = Fake::new();
        let stop = AtomicBool::new(false);
        let error = translate_all(
            &fake,
            "en",
            "de",
            &texts(&["one", "two", "three", "four"]),
            &mut |progress| {
                if progress.done == 2 {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            &stop,
        )
        .unwrap_err();
        assert_eq!(error, TranslateError::Stopped);
        assert_eq!(*fake.answered.lock().unwrap(), 2, "no text after the stop");
    }

    #[test]
    fn a_stop_before_anything_is_sent_sends_nothing() {
        let fake = Fake::new();
        let error = translate_all(
            &fake,
            "en",
            "de",
            &texts(&["one"]),
            &mut |_| {},
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert_eq!(error, TranslateError::Stopped);
        assert!(fake.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn progress_counts_every_text_blank_ones_included() {
        let mut seen = Vec::new();
        translate_all(
            &Fake::new(),
            "en",
            "de",
            &texts(&["one", "", "three"]),
            &mut |p| seen.push((p.done, p.of)),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(seen, vec![(1, 3), (2, 3), (3, 3)]);
    }

    /// Answers by pair: `installed` lists the pairs ready now.
    struct Pairs {
        languages: Vec<&'static str>,
        installed: Vec<(&'static str, &'static str)>,
    }

    impl Translator for Pairs {
        fn id(&self) -> &'static str {
            "pairs"
        }
        fn languages(&self) -> Vec<String> {
            self.languages.iter().map(|code| code.to_string()).collect()
        }
        fn availability(&self, source: &str, target: &str) -> Result<(), Unavailable> {
            if self.installed.contains(&(source, target)) {
                Ok(())
            } else {
                Err(Unavailable::NotInstalled {
                    source: source.into(),
                    target: target.into(),
                })
            }
        }
        fn translate(
            &self,
            _source: &str,
            _target: &str,
            _texts: &[String],
            _each: &mut dyn FnMut(usize, String) -> Flow,
        ) -> Result<(), TranslateError> {
            unreachable!("only availability is asked")
        }
    }

    #[test]
    fn the_heard_language_is_used_when_its_pair_is_installed() {
        let pairs = Pairs {
            languages: vec!["en", "en-GB", "de"],
            installed: vec![("en-US", "de"), ("en-GB", "de")],
        };
        assert_eq!(installed_source(&pairs, "en-US", "de"), "en-US");
    }

    #[test]
    fn another_variant_of_the_same_language_is_used_when_it_is_installed() {
        // Seen on a British-English Mac: en-GB into German installed, the
        // US pair only available to download.
        let pairs = Pairs {
            languages: vec!["de", "en", "en-GB", "fr"],
            installed: vec![("en-GB", "de"), ("fr", "de")],
        };
        assert_eq!(installed_source(&pairs, "en-US", "de"), "en-GB");
    }

    #[test]
    fn the_heard_language_is_kept_when_no_variant_is_installed() {
        let pairs = Pairs {
            languages: vec!["de", "en", "en-GB", "fr"],
            installed: vec![("fr", "de")],
        };
        // Another language is never used in its place.
        assert_eq!(installed_source(&pairs, "en-US", "de"), "en-US");
    }
}
