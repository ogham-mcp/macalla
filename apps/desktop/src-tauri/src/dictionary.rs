// SPDX-License-Identifier: MIT
//! Where the app's dictionary lives, and how it gets there the first time.
//!
//! The CLI looks in the working directory, which is right for a terminal and
//! wrong for an app launched from Finder, whose working directory is `/`.
//! The app keeps one file under the platform's application-support directory
//! and seeds it from the repository's seed on first run, so the first
//! dictation already has a vocabulary and the Dictionary card has something
//! to show.
//!
//! Editing goes through here too, but the rules of an edit are the core's
//! ([`macalla_core::dictionary::TermFile`]) and the file handling is the
//! session crate's ([`macalla_session::dictionary`]); this module maps the
//! app's paths onto them and shapes what the Dictionary view shows.
//!
//! [`correction_offer`] is the Files page's own use of the dictionary: the
//! core's [`macalla_core::dictionary::correction_offer`] decides whether a
//! hand correction looks like a mishearing, and this module adds the one
//! check that needs the dictionary itself -- whether it already has the
//! pair offered, which the core cannot know.

use crate::command_error::{CommandError, OrSay};
use macalla_core::dictionary::{Dictionary, Offer, Term, TermFile};
use macalla_session::dictionary as file;
use serde::Serialize;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, Wry};

/// The file name inside the app-data directory.
pub const FILE_NAME: &str = "macalla-dictionary.json";

/// The repository's seed, compiled in so the app needs no working directory.
const SEED: &str = include_str!("../../../../fixtures/dictionary/seed.json");

/// What the Dictionary card shows.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DictionaryInfo {
    pub path: String,
    pub terms: usize,
}

/// The app's dictionary path, creating the file from the seed if it is not
/// there. An existing file is never touched.
pub fn ensure(app: &AppHandle<Wry>) -> std::io::Result<PathBuf> {
    let dir = app.path().app_data_dir().map_err(|e| {
        tracing::warn!(error = %e, "could not find the app data directory");
        std::io::Error::other("could not find where Macalla keeps its files")
    })?;
    ensure_in(&dir)
}

/// The same, for a directory the caller chose. This is what the tests use.
fn ensure_in(dir: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(FILE_NAME);
    if !path.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::write(&path, SEED)?;
        tracing::info!(path = %path.display(), "seeded the dictionary");
    }
    Ok(path)
}

/// Read the file and count its terms, for the card. A file that cannot be
/// read or parsed reports zero terms rather than an error: the card is
/// informational and the session reports the real failure.
pub fn info(path: &Path) -> DictionaryInfo {
    let terms = std::fs::read_to_string(path)
        .ok()
        .and_then(|json| Dictionary::from_json(&json).ok())
        .map_or(0, |d| d.len());
    DictionaryInfo {
        path: path.display().to_string(),
        terms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macalla-dictionary-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_first_run_seeds_the_dictionary_and_the_card_can_count_it() {
        let dir = temp_dir("seed");
        let path = ensure_in(&dir).unwrap();
        assert!(path.exists());
        let info = info(&path);
        assert!(info.terms > 0, "the seed has terms: {info:?}");
        assert!(info.path.ends_with(FILE_NAME));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_dictionary_is_never_overwritten() {
        // The user's own terms outrank the seed, every time.
        let dir = temp_dir("keep");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "{}").unwrap();
        let again = ensure_in(&dir).unwrap();
        assert_eq!(again, path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert_eq!(info(&path).terms, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_export_is_named_by_date_and_never_overwrites() {
        let dir = temp_dir("export");
        std::fs::create_dir_all(&dir).unwrap();
        let first = export_target(&dir, "2026-09-09");
        assert!(first.ends_with("macalla-dictionary-2026-09-09.json"));
        std::fs::write(&first, "{}").unwrap();
        let second = export_target(&dir, "2026-09-09");
        assert!(second.ends_with("macalla-dictionary-2026-09-09-2.json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_compiled_in_seed_parses() {
        // If the seed file in the repository ever breaks, this is where it
        // shows, not on someone's first launch.
        let d = Dictionary::from_json(SEED).expect("the seed parses");
        assert!(!d.is_empty());
    }

    #[test]
    fn correction_offer_offers_a_pair_the_dictionary_does_not_have() {
        let dir = temp_dir("offer-new");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        file::write(&path, &TermFile::default()).unwrap();

        let offer = correction_offer(&path, "I met Bob at noon", "I met Bill at noon")
            .unwrap()
            .expect("a one-word swap the dictionary does not have yet is offered");
        assert_eq!(offer.heard, "Bob");
        assert_eq!(offer.written, "Bill");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn correction_offer_is_none_for_a_pair_the_dictionary_already_has() {
        // An existing pair gets no offer -- the check
        // the core cannot make itself, since it knows nothing of a
        // dictionary.
        let dir = temp_dir("offer-existing");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let mut term_file = TermFile::default();
        term_file.add("Bill", &["Bob".to_string()]).unwrap();
        file::write(&path, &term_file).unwrap();

        let offer = correction_offer(&path, "I met Bob at noon", "I met Bill at noon").unwrap();
        assert_eq!(offer, None, "the dictionary already has this exact pair");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn correction_offer_is_none_when_the_dictionary_term_is_only_a_case_fix() {
        // `TermFile::add` drops a spoken form equal to the written form
        // ignoring case (`tidy_spoken`), so adding "github" -> "GitHub"
        // leaves the term with `spoken: []`. Without also checking the
        // written form itself, `already_has_pair` would say no and this
        // offer would come back forever.
        let dir = temp_dir("offer-case-fix");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let mut term_file = TermFile::default();
        term_file.add("GitHub", &["github".to_string()]).unwrap();
        file::write(&path, &term_file).unwrap();
        // The seed's own tidy_spoken already dropped "github": confirm the
        // fixture is exactly the shape that broke this, not something else.
        assert_eq!(
            term_file.get("GitHub").unwrap().spoken,
            Vec::<String>::new()
        );

        let offer = correction_offer(&path, "use github for this", "use GitHub for this").unwrap();
        assert_eq!(
            offer, None,
            "the dictionary already fixes this exact casing"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// One term as the Dictionary view lists it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TermView {
    pub written: String,
    pub spoken: Vec<String>,
}

impl From<&Term> for TermView {
    fn from(t: &Term) -> Self {
        Self {
            written: t.written.clone(),
            spoken: t.spoken.clone(),
        }
    }
}

/// Every term, in file order.
pub fn terms(path: &Path) -> Result<Vec<TermView>, CommandError> {
    let file = file::read(path)?;
    Ok(file.terms.iter().map(TermView::from).collect())
}

/// The dictionary's own catalogue words (`dictionary_words.rs`): History's
/// "Add to dictionary" button and its two notices, `added` and `updated`
/// carrying `{written}` for the page to fill. The Files page's offer reads
/// the same three constants through `files::TranscriptWordsView`, built
/// from the same source -- one entry, two surfaces.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DictionaryWordsView {
    pub add_to_dictionary: &'static str,
    /// Template: `{written}`.
    pub added: &'static str,
    /// Template: `{written}`.
    pub updated: &'static str,
}

/// [`DictionaryWordsView`], read from the catalogue.
pub fn words() -> DictionaryWordsView {
    DictionaryWordsView {
        add_to_dictionary: crate::dictionary_words::ADD_TO_DICTIONARY,
        added: crate::dictionary_words::ADDED,
        updated: crate::dictionary_words::UPDATED,
    }
}

/// Whether the dictionary already maps `heard` to `written`, so offering
/// that pair again would be a no-op. Folds case the way a term's own spoken
/// forms do: exact for a case-sensitive term, folded otherwise -- the same
/// rule [`TermFile::add`] uses to decide a spoken form is new.
///
/// A term's own written form is not necessarily in its `spoken` list, even
/// when it should count as covering `heard`: `TermFile::add`'s own
/// `tidy_spoken` drops a spoken form equal to the written form ignoring
/// case, since it "matches itself already". Correcting "github" to
/// "GitHub" adds a term `written: "GitHub", spoken: []` -- no spoken form
/// at all -- so without also checking the written form itself, this would
/// say no and the offer would come back every time. Not for a
/// case-sensitive term, whose spoken forms can differ from the written form
/// only by case on purpose, so a written-form match there says nothing.
fn already_has_pair(file: &TermFile, heard: &str, written: &str) -> bool {
    let Some(term) = file.get(written) else {
        return false;
    };
    let fold = |s: &str| {
        if term.case_sensitive {
            s.trim().to_string()
        } else {
            s.trim().to_lowercase()
        }
    };
    let target = fold(heard);
    (!term.case_sensitive && fold(&term.written) == target)
        || term.spoken.iter().any(|s| fold(s) == target)
}

/// The offer the Files page shows under a turn just corrected by hand, or
/// none: [`macalla_core::dictionary::correction_offer`] on the turn's old
/// and new text, then [`already_has_pair`], the one check that needs the
/// dictionary itself -- an existing pair would make the offer a no-op.
pub fn correction_offer(path: &Path, old: &str, new: &str) -> Result<Option<Offer>, CommandError> {
    let Some(offer) = macalla_core::dictionary::correction_offer(old, new) else {
        return Ok(None);
    };
    let file = file::read(path)?;
    if already_has_pair(&file, &offer.heard, &offer.written) {
        return Ok(None);
    }
    Ok(Some(offer))
}

fn edit<T>(
    path: &Path,
    change: impl FnOnce(&mut TermFile) -> Result<T, CommandError>,
) -> Result<T, CommandError> {
    let mut file = file::read(path)?;
    let out = change(&mut file)?;
    file::write(path, &file)?;
    Ok(out)
}

/// Add spoken forms to a written form, creating the term if new.
pub fn add(path: &Path, written: &str, spoken: &[String]) -> Result<bool, CommandError> {
    edit(path, |f| {
        f.add(written, spoken)
            .map_err(|e| CommandError::from(e.to_string()))
    })
}

/// Replace one term, possibly renaming it.
pub fn update(
    path: &Path,
    previous: &str,
    written: &str,
    spoken: &[String],
) -> Result<(), CommandError> {
    edit(path, |f| {
        let kept = f.get(previous).cloned();
        let term = Term {
            written: written.to_string(),
            spoken: spoken.to_vec(),
            case_sensitive: kept.as_ref().is_some_and(|t| t.case_sensitive),
            threshold: kept
                .as_ref()
                .map_or(macalla_core::dictionary::DEFAULT_THRESHOLD, |t| t.threshold),
        };
        f.update(previous, term)
            .map_err(|e| CommandError::from(e.to_string()))
    })
}

/// Forget a term.
pub fn remove(path: &Path, written: &str) -> Result<bool, CommandError> {
    edit(path, |f| Ok(f.remove(written)))
}

/// Merge a shared file in. Returns how many terms were added or changed.
pub fn import(path: &Path, json: &str) -> Result<usize, CommandError> {
    // `report_core`'s own sentence for `Error::Audio` ("Something could not
    // be read or converted correctly.") is written to fit both this and a
    // resampling failure; a JSON import that fails to parse gets a sentence
    // of its own.
    let theirs = file::parse(json).map_err(|e| {
        crate::say::report_core(&e);
        CommandError::from("That file is not a dictionary Macalla can read.")
    })?;
    edit(path, |f| Ok(f.merge(&theirs)))
}

/// Write a copy to the Downloads folder for sharing, and say where.
pub fn export(app: &AppHandle<Wry>, path: &Path) -> Result<PathBuf, CommandError> {
    let file = file::read(path)?;
    let dir = app
        .path()
        .download_dir()
        .or_say(crate::meeting_words::DOWNLOADS_NOT_FOUND)?;
    let target = export_target(&dir, &today());
    file::write(&target, &file)?;
    Ok(target)
}

/// A name a person can read in a folder listing: the date, and a counter if
/// the day already has one. An epoch number was the first version, and it
/// was unreadable on the card.
fn export_target(dir: &Path, date: &str) -> PathBuf {
    let first = dir.join(format!("macalla-dictionary-{date}.json"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("macalla-dictionary-{date}-{n}.json")))
        .find(|p| !p.exists())
        .expect("an unbounded range finds a free name")
}

/// Today's date in local time, as YYYY-MM-DD, from `date` where available
/// and from the epoch otherwise. Precise enough for a file name. Shared with
/// the Files page's Save as text.
pub(crate) fn today() -> String {
    let out = std::process::Command::new("date").arg("+%Y-%m-%d").output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("day-{}", secs / 86_400)
        }
    }
}
