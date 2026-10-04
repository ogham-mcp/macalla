// SPDX-License-Identifier: MIT
//! The dictionary file: reading it to edit, writing it back, and the JSON
//! that goes in and out for sharing.
//!
//! The rules of an edit -- what makes a term, what a duplicate is, what an
//! import may change -- are [`TermFile`]'s, in the core. This module only
//! moves the file. Both apps use it, so neither owns the format.

use crate::{Error, Result};
use macalla_core::dictionary::TermFile;
use std::path::Path;

/// Read the file to edit it. A missing file is an empty dictionary: the
/// first "add to dictionary" must work before anything was ever saved.
pub fn read(path: &Path) -> Result<TermFile> {
    if !path.exists() {
        return Ok(TermFile::default());
    }
    let json = std::fs::read_to_string(path).map_err(|source| Error::DictionaryRead {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&json).map_err(|source| Error::DictionaryParse {
        path: path.to_path_buf(),
        source,
    })
}

/// Write the file back, whole. Written beside and renamed into place, so a
/// crash mid-write leaves the old dictionary rather than half of the new one.
pub fn write(path: &Path, file: &TermFile) -> Result<()> {
    let json = to_json(file);
    let io = |source| Error::DictionaryWrite {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// The JSON a user shares: the same shape the file has.
pub fn to_json(file: &TermFile) -> String {
    let mut json = serde_json::to_string_pretty(file).expect("a term file always serialises");
    json.push('\n');
    json
}

/// JSON from a user, checked. Comments and unknown keys are allowed, as in
/// the shipped seed.
pub fn parse(text: &str) -> macalla_core::Result<TermFile> {
    TermFile::parse(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macalla-dictionary-file-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("macalla-dictionary.json")
    }

    #[test]
    fn a_missing_file_reads_as_empty_and_writes_create_it() {
        let path = temp("missing");
        let mut file = read(&path).unwrap();
        assert!(file.terms.is_empty());
        file.add("kubectl", &["cube control".into()]).unwrap();
        write(&path, &file).unwrap();
        let again = read(&path).unwrap();
        assert_eq!(again.terms.len(), 1);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no temp file left"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_json_round_trips_and_is_the_seed_shape() {
        let seed = include_str!("../../../fixtures/dictionary/seed.json");
        let file = parse(seed).expect("the seed parses");
        let json = to_json(&file);
        let back = parse(&json).unwrap();
        assert_eq!(back.terms.len(), file.terms.len());
        assert!(json.contains("\"written\""));
    }

    #[test]
    fn bad_json_is_refused_with_a_reason() {
        let err = parse("{\"terms\": 3}").unwrap_err();
        assert!(err.to_string().contains("dictionary"), "{err}");
    }
}
