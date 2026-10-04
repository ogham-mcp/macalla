// SPDX-License-Identifier: MIT
//! A transcript written to a new file, never over one already there. Both
//! apps save transcripts this way: the desktop app to Downloads, the CLI to
//! `--out-dir`. Each app chooses the base of the name; the rule for the rest
//! of it, and for a write that fails, is here, so neither owns it.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `bytes` to a new file in `dir` named `{base}.{extension}`, or
/// `{base}-{n}.{extension}` from 2 on when that name is taken, and say
/// where. Created only if no file has the name, so nothing is overwritten,
/// even one made a moment before. A write that fails part of the way removes
/// what it made, so no empty or cut-short file is left under the name, and
/// a retry takes the same name again.
pub fn write_new(
    dir: &Path,
    base: &str,
    extension: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    write_new_with(dir, base, extension, |file| {
        file.write_all(bytes)?;
        file.sync_all()
    })
}

/// [`write_new`], with the writing done by `write`: the seam a test uses to
/// fail a write part of the way.
fn write_new_with(
    dir: &Path,
    base: &str,
    extension: &str,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    for n in 1u64.. {
        let path = if n == 1 {
            dir.join(format!("{base}.{extension}"))
        } else {
            dir.join(format!("{base}-{n}.{extension}"))
        };
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        return match write(&mut file) {
            Ok(()) => Ok(path),
            Err(error) => {
                drop(file);
                if let Err(removing) = std::fs::remove_file(&path) {
                    tracing::warn!(%removing, "could not remove a file whose write failed");
                }
                Err(error)
            }
        };
    }
    unreachable!("an unbounded range finds a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macalla-session-output-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_name_that_is_taken_gets_a_counter_and_nothing_is_overwritten() {
        let dir = temp_dir("counter");
        std::fs::write(dir.join("standup.txt"), "keep me").unwrap();

        let second = write_new(&dir, "standup", "txt", b"second").unwrap();
        let third = write_new(&dir, "standup", "txt", b"third").unwrap();

        assert_eq!(second, dir.join("standup-2.txt"));
        assert_eq!(third, dir.join("standup-3.txt"));
        assert_eq!(
            std::fs::read_to_string(dir.join("standup.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(std::fs::read_to_string(second).unwrap(), "second");
    }

    #[test]
    fn a_write_that_fails_part_of_the_way_leaves_no_file_and_a_retry_takes_the_same_name() {
        let dir = temp_dir("fails");

        let failed = write_new_with(&dir, "standup", "txt", |file| {
            file.write_all(b"half of it")?;
            Err(std::io::Error::other("the disk is full"))
        });

        assert!(failed.is_err());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "nothing left behind"
        );
        let retried = write_new(&dir, "standup", "txt", b"whole").unwrap();
        assert_eq!(retried, dir.join("standup.txt"));
    }
}
