// SPDX-License-Identifier: MIT
//! `macalla watch`: which files in a watched folder are finished and not yet
//! transcribed. No file-watching crate: the folder is listed every [`POLL`],
//! and a file counts as finished once its size and modification time are the
//! same in two lists in a row, and it was last modified at least [`SETTLE`]
//! before the list. Until then it is left alone, so a recording still being
//! copied or saved is not heard half-written. A copy that stalls for longer
//! than both can still be taken part-way; when it finishes it has changed,
//! and is heard again into a new file.
//!
//! What is done is kept in a ledger, [`LEDGER`], in the output folder: one
//! JSON line per input, with its name, size and modification time, appended
//! only once the input's transcript is written, or once it was heard to the
//! end with no speech in it. A restart reads the ledger and skips every file
//! that matches a line. A file that changed after it was done matches no line
//! and is transcribed again, into a new file beside the first. A file that
//! failed is not in the ledger: this run leaves it alone until it changes,
//! and the next run tries it again.
//!
//! The rules here are pure over snapshots ([`Tracker::take`]), so they are
//! tested without a clock; [`snapshot`] and the ledger's read and append are
//! the only I/O.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

/// How often the watched folder is listed. A file is taken no sooner than
/// one interval after it last changed.
pub const POLL: Duration = Duration::from_secs(2);

/// How long before a list a file must last have been modified to be taken.
/// Two equal lists alone would take a copy that has stalled for one
/// [`POLL`], as copies over SMB and from sync clients do.
pub const SETTLE: Duration = Duration::from_secs(10);

/// How many lists of the watched folder may fail in a row before the watch
/// stops. One that fails now and then, as a network folder's can, does not
/// end the run.
pub const LIST_ATTEMPTS: usize = 5;

/// The ledger's file name, in the output folder.
pub const LEDGER: &str = ".macalla-watch";

/// The extensions a watched file is heard for, lower case: the ones Macalla's
/// own reader, the macOS decoder or ffmpeg read as audio. Anything else in
/// the folder is left alone.
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "wav", "aif", "aiff", "caf", "flac", "mp3", "m4a", "mp4", "mov", "aac", "ogg", "opus", "webm",
];

/// The endings of a download or copy still in progress. Such a file is
/// renamed when it is finished, and is taken then, under its own name.
const PARTIAL_EXTENSIONS: &[&str] = &["part", "crdownload", "download"];

/// A file's size and modification time, as nanoseconds since 1970.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Stamp {
    pub size: u64,
    pub mtime_ns: u64,
}

/// A file in the watched folder, by name, as it was when it was listed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Seen {
    pub name: String,
    pub stamp: Stamp,
}

/// Whether a file named `name` is one to hear: not hidden, not a partial
/// download, not the ledger, and named as audio.
pub fn wanted(name: &str) -> bool {
    if name.starts_with('.') || name == LEDGER {
        return false;
    }
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();
    !PARTIAL_EXTENSIONS.contains(&extension.as_str())
        && AUDIO_EXTENSIONS.contains(&extension.as_str())
}

/// What the watch knows between polls: each file as the last list had it,
/// and every file that is done, from the ledger or taken in this run.
#[derive(Default)]
pub struct Tracker {
    last: HashMap<String, Stamp>,
    settled: HashSet<Seen>,
}

impl Tracker {
    /// A tracker that treats every one of `done` as already taken: the
    /// ledger's lines, on a restart.
    pub fn with_done(done: impl IntoIterator<Item = Seen>) -> Self {
        Self {
            last: HashMap::new(),
            settled: done.into_iter().collect(),
        }
    }

    /// How many of `now` are not yet done: what the watch will transcribe
    /// once each is finished.
    pub fn waiting(&self, now: &[Seen]) -> usize {
        now.iter()
            .filter(|seen| !self.settled.contains(*seen))
            .count()
    }

    /// The files in `now`, the folder as listed at `listed_ns` (nanoseconds
    /// since 1970), that are finished and not yet taken, in name order.
    /// Finished means the last list had the same size and modification time
    /// for the same name, and that time is at least [`SETTLE`] before the
    /// list. Each is returned once: it is taken here, whatever then becomes
    /// of it, and is taken again only once it has changed and settled again.
    pub fn take(&mut self, now: Vec<Seen>, listed_ns: u64) -> Vec<Seen> {
        let mut ready: Vec<Seen> = now
            .iter()
            .filter(|seen| self.last.get(&seen.name) == Some(&seen.stamp))
            .filter(|seen| settled_by(seen.stamp.mtime_ns, listed_ns))
            .filter(|seen| !self.settled.contains(*seen))
            .cloned()
            .collect();
        ready.sort_by(|a, b| a.name.cmp(&b.name));
        self.settled.extend(ready.iter().cloned());
        self.last = now
            .into_iter()
            .map(|seen| (seen.name, seen.stamp))
            .collect();
        ready
    }
}

/// Whether a file last modified at `mtime_ns` has been still long enough by
/// `listed_ns`. A time after the list (a network volume's clock running
/// ahead, or a time kept from elsewhere) can't be aged, so only the
/// two-equal-lists rule applies to it: waiting for it could leave the file
/// untaken indefinitely.
fn settled_by(mtime_ns: u64, listed_ns: u64) -> bool {
    let settle = u64::try_from(SETTLE.as_nanos()).unwrap_or(u64::MAX);
    mtime_ns > listed_ns || listed_ns - mtime_ns >= settle
}

/// Now, as nanoseconds since 1970, the clock a list is compared against.
pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        })
}

/// The watched folder as listed once.
#[derive(Debug, Default)]
pub struct Listing {
    /// The files [`wanted`] lets through, as they are now.
    pub seen: Vec<Seen>,
    /// Names that are not UTF-8, which are left out: the ledger could not
    /// hold them as they are.
    pub not_utf8: Vec<OsString>,
}

/// The name of a directory entry as text, or back as it is when it is not
/// UTF-8.
fn utf8_name(name: OsString) -> Result<String, OsString> {
    name.into_string()
}

/// The watched folder as it is now. Only `dir` itself is listed, never a
/// folder inside it, so an output folder inside the watched one is never
/// read. An entry that cannot be read, or is gone before it is looked at,
/// is passed over; only a folder that cannot be listed at all is an error.
pub fn snapshot(dir: &Path) -> std::io::Result<Listing> {
    let mut listing = Listing::default();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = match utf8_name(entry.file_name()) {
            Ok(name) => name,
            Err(name) => {
                listing.not_utf8.push(name);
                continue;
            }
        };
        if !wanted(&name) {
            continue;
        }
        // Followed through a link, so a linked recording counts as a file.
        // Gone between the list and this look: not there.
        let Ok(metadata) = std::fs::metadata(entry.path()) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| {
                u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
            });
        listing.seen.push(Seen {
            name,
            stamp: Stamp {
                size: metadata.len(),
                mtime_ns,
            },
        });
    }
    Ok(listing)
}

/// Every file the ledger at `path` says is done. No ledger is nothing done.
/// Read as bytes and line by line, so a line that does not read, such as
/// one cut short inside a character when the process was killed, is passed
/// over alone: its file is transcribed again, never lost, and every other
/// line still counts.
pub fn read_ledger(path: &Path) -> std::io::Result<Vec<Seen>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    Ok(bytes
        .split(|&byte| byte == b'\n')
        .filter_map(ledger_line)
        .collect())
}

fn ledger_line(line: &[u8]) -> Option<Seen> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    Some(Seen {
        name: value.get("name")?.as_str()?.to_string(),
        stamp: Stamp {
            size: value.get("size")?.as_u64()?,
            mtime_ns: value.get("mtime_ns")?.as_u64()?,
        },
    })
}

/// Note `seen` as done in the ledger at `path`, with where its transcript
/// went, or none for a file with no speech in it. One line, written in one
/// call and synced. A last line cut short, with no line break after it, is
/// ended first, so this line is never joined onto it and lost with it.
pub fn append_ledger(path: &Path, seen: &Seen, output: Option<&Path>) -> std::io::Result<()> {
    let line = serde_json::json!({
        "name": seen.name,
        "size": seen.stamp.size,
        "mtime_ns": seen.stamp.mtime_ns,
        "output": output.map(|output| output.display().to_string()),
    });
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(path)?;
    let torn = match file.metadata()?.len() {
        0 => false,
        len => {
            let mut last = [0u8];
            file.seek(SeekFrom::Start(len - 1))?;
            file.read_exact(&mut last)?;
            last[0] != b'\n'
        }
    };
    let mut bytes = Vec::new();
    if torn {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(format!("{line}\n").as_bytes());
    file.write_all(&bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(name: &str, size: u64, mtime_ns: u64) -> Seen {
        Seen {
            name: name.to_string(),
            stamp: Stamp { size, mtime_ns },
        }
    }

    /// A list made long after every test file's modification time, so the
    /// settle age never holds a file back unless a test asks it to.
    const LATE: u64 = 1_000 * 1_000_000_000;

    fn names(taken: &[Seen]) -> Vec<&str> {
        taken.iter().map(|seen| seen.name.as_str()).collect()
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("macalla-cli-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_still_growing_is_not_taken() {
        let mut tracker = Tracker::default();
        assert!(tracker
            .take(vec![seen("call.m4a", 100, 1)], LATE)
            .is_empty());
        assert!(tracker
            .take(vec![seen("call.m4a", 200, 2)], LATE)
            .is_empty());
        assert!(
            tracker
                .take(vec![seen("call.m4a", 200, 3)], LATE)
                .is_empty(),
            "the same size with a newer time is still being written"
        );
        assert_eq!(
            names(&tracker.take(vec![seen("call.m4a", 200, 3)], LATE)),
            ["call.m4a"]
        );
    }

    #[test]
    fn a_stable_file_is_taken_exactly_once() {
        let mut tracker = Tracker::default();
        let file = seen("standup.wav", 10, 5);
        assert!(
            tracker.take(vec![file.clone()], LATE).is_empty(),
            "one list is not two"
        );
        assert_eq!(
            tracker.take(vec![file.clone()], LATE),
            std::slice::from_ref(&file)
        );
        for _ in 0..3 {
            assert!(tracker.take(vec![file.clone()], LATE).is_empty());
        }
    }

    #[test]
    fn after_a_restart_the_ledger_skips_a_file_already_done() {
        let dir = temp_dir("restart");
        let ledger = dir.join(LEDGER);
        let file = seen("standup.wav", 10, 5);
        append_ledger(&ledger, &file, Some(&dir.join("standup.txt"))).unwrap();
        // A line cut short by a kill is passed over, not an error.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&ledger)
            .unwrap()
            .write_all(b"{\"name\":\"ret")
            .unwrap();

        let mut tracker = Tracker::with_done(read_ledger(&ledger).unwrap());
        let other = seen("retro.wav", 20, 6);
        assert_eq!(tracker.waiting(&[file.clone(), other.clone()]), 1);
        tracker.take(vec![file.clone(), other.clone()], LATE);

        assert_eq!(
            tracker.take(vec![file.clone(), other.clone()], LATE),
            std::slice::from_ref(&other)
        );

        // Appending after the tear ends the torn line first, so the new
        // record is read back and not lost with it.
        append_ledger(&ledger, &other, None).unwrap();
        assert_eq!(read_ledger(&ledger).unwrap(), [file, other]);
    }

    #[test]
    fn a_ledger_line_torn_inside_a_character_is_skipped_and_the_lines_before_it_count() {
        let dir = temp_dir("torn-utf8");
        let ledger = dir.join(LEDGER);
        let first = seen("standup.wav", 10, 5);
        append_ledger(&ledger, &first, None).unwrap();
        let cafe = serde_json::json!({"name": "café.m4a", "size": 1, "mtime_ns": 2}).to_string();
        let cut = cafe.find('é').unwrap() + 1;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&ledger)
            .unwrap()
            .write_all(&cafe.as_bytes()[..cut])
            .unwrap();
        assert!(std::str::from_utf8(&std::fs::read(&ledger).unwrap()).is_err());

        assert_eq!(read_ledger(&ledger).unwrap(), std::slice::from_ref(&first));
        let second = seen("café.m4a", 1, 2);
        append_ledger(&ledger, &second, None).unwrap();
        assert_eq!(read_ledger(&ledger).unwrap(), [first, second]);
    }

    #[test]
    fn a_file_modified_less_than_the_settle_age_before_the_list_waits_for_it() {
        let second = 1_000_000_000;
        let settle = SETTLE.as_secs() * second;
        let mut tracker = Tracker::default();
        let copy = seen("call.m4a", 100, 50 * second);
        assert!(tracker.take(vec![copy.clone()], 50 * second).is_empty());
        assert!(
            tracker
                .take(vec![copy.clone()], 50 * second + settle - 1)
                .is_empty(),
            "two equal lists, but a stalled copy could look like this"
        );
        assert_eq!(
            tracker.take(vec![copy.clone()], 50 * second + settle),
            std::slice::from_ref(&copy)
        );

        let mut tracker = Tracker::default();
        let kept = seen("old.m4a", 1, 90 * second);
        tracker.take(vec![kept.clone()], 60 * second);
        assert_eq!(
            tracker.take(vec![kept.clone()], 62 * second),
            [kept],
            "a time after the list was kept from elsewhere, not stamped by a copy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_name_that_is_not_utf8_is_given_back_as_it_is() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"call-\xff.m4a".to_vec());
        assert_eq!(utf8_name(raw.clone()), Err(raw));
        assert_eq!(
            utf8_name(OsString::from("café.m4a")),
            Ok("café.m4a".to_string())
        );
    }

    #[test]
    fn no_ledger_is_nothing_done() {
        let dir = temp_dir("no-ledger");
        assert!(read_ledger(&dir.join(LEDGER)).unwrap().is_empty());
    }

    #[test]
    fn a_file_that_changed_after_it_was_done_is_taken_again_once_it_settles() {
        let mut tracker = Tracker::with_done([seen("memo.m4a", 10, 5)]);
        tracker.take(vec![seen("memo.m4a", 10, 5)], LATE);
        assert!(tracker.take(vec![seen("memo.m4a", 10, 5)], LATE).is_empty());

        assert!(tracker.take(vec![seen("memo.m4a", 30, 9)], LATE).is_empty());
        assert_eq!(
            tracker.take(vec![seen("memo.m4a", 30, 9)], LATE),
            [seen("memo.m4a", 30, 9)]
        );
    }

    #[test]
    fn hidden_partial_and_ledger_files_and_anything_not_audio_are_ignored() {
        for name in [
            LEDGER,
            ".hidden.wav",
            "call.m4a.part",
            "call.m4a.crdownload",
            "call.m4a.download",
            "notes.txt",
            "transcript.vtt",
            "no-extension",
        ] {
            assert!(!wanted(name), "{name}");
        }
        for name in ["call.m4a", "Standup.WAV", "memo.mp3", "clip.webm"] {
            assert!(wanted(name), "{name}");
        }

        let dir = temp_dir("ignored");
        for name in [".hidden.wav", "call.m4a.part", "notes.txt", "call.m4a"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        std::fs::write(dir.join(LEDGER), b"").unwrap();
        std::fs::create_dir(dir.join("folder.wav")).unwrap();
        assert_eq!(names(&snapshot(&dir).unwrap().seen), ["call.m4a"]);
    }

    #[cfg(unix)]
    #[test]
    fn an_entry_that_cannot_be_read_is_passed_over_and_the_rest_are_listed() {
        let dir = temp_dir("dangling");
        std::fs::write(dir.join("real.wav"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.join("nowhere.wav"), dir.join("x.wav")).unwrap();

        let listing = snapshot(&dir).expect("a dangling link is not an error");

        assert_eq!(names(&listing.seen), ["real.wav"]);
    }

    #[test]
    fn an_output_folder_inside_the_watched_one_is_never_listed() {
        let dir = temp_dir("inside");
        let out = dir.join("transcripts");
        std::fs::create_dir(&out).unwrap();
        std::fs::write(out.join("standup.wav"), b"x").unwrap();
        std::fs::write(out.join("standup.txt"), b"x").unwrap();
        std::fs::write(out.join(LEDGER), b"").unwrap();
        std::fs::write(dir.join("retro.wav"), b"x").unwrap();

        assert_eq!(names(&snapshot(&dir).unwrap().seen), ["retro.wav"]);
    }
}
