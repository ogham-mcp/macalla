// SPDX-License-Identifier: MIT
//! What a dictation looked like once it was done, and where it goes.
//!
//! This module owns the MODEL (`Entry`, `NewEntry`) and the PORT
//! (`HistoryStore`) that history is expressed through. It does not own a
//! database. `macalla-core` builds and tests headless -- no microphone, no
//! Tauri, and by design no SQLite either, because a database driver is a
//! detail an inference-and-pipeline crate should never need to link, let
//! alone cross-compile. The SQLite adapter lives in `macalla-session`, which
//! already depends on this crate; the dependency points inward, from the
//! adapter to the port, never the other way.
//!
//! The design names the columns this shadows: `id, ts, app_id, engine,
//! raw, enhanced, blocks_json, schema_version, latency_json, audio_path`.
//! `Entry` carries the same information as typed Rust rather than JSON blobs,
//! because a caller of the port should not have to know it is backed by
//! SQLite at all.

use crate::block::Document;
use crate::latency;

/// One dictation as history keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The store's row id. Assigned on [`HistoryStore::record`]; never chosen
    /// by the caller.
    pub id: i64,
    /// Milliseconds since the Unix epoch. A caller-supplied clock reading,
    /// not `SystemTime::now()` taken inside the store, so a test can pick an
    /// exact value and stay deterministic.
    pub ts_ms: u64,
    /// The frontmost app when the dictation happened, when something knows
    /// it. Nothing does yet -- see [`NewEntry`].
    pub app_id: Option<String>,
    /// The engine id that produced this dictation, e.g. `"apple"`.
    pub engine: String,
    /// What the engine said, uncorrected. "Add to
    /// dictionary" needs this because the user has to recognise what was
    /// misheard.
    pub raw: String,
    /// The cleaned-up text that was actually pasted, as plain text.
    pub enhanced: String,
    /// The structured form `enhanced` was rendered from. Kept alongside the
    /// plain text rather than instead of it so a UI can re-render markdown
    /// without re-parsing prose.
    pub document: Document,
    /// The schema version [`Document::schema_version`] was written at. A
    /// stored dictation outlives the code that wrote it (see block.rs), so
    /// this travels with the row even though it also lives on the document.
    pub schema_version: u32,
    /// Where the time went, when it was measured. `None` for a dictation
    /// replayed from a fixture with no timing to report.
    pub latency: Option<latency::Dictation>,
    /// A path to retained audio, when audio retention is on. `None` by
    /// default -- retention is off unless the user
    /// turns it on, and this crate does not implement retention at all.
    pub audio_path: Option<String>,
}

/// An [`Entry`] before the store has assigned it an id.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEntry {
    pub ts_ms: u64,
    pub app_id: Option<String>,
    pub engine: String,
    pub raw: String,
    pub enhanced: String,
    pub document: Document,
    pub schema_version: u32,
    pub latency: Option<latency::Dictation>,
    pub audio_path: Option<String>,
}

/// The port history is written and read through. `macalla-session` implements
/// this over SQLite; a test implements it in memory or with a `HashMap`.
///
/// Recording to history happens AFTER paste and must never delay it
/// (paste waits for enhancement for at most 5 seconds). That rule belongs to the caller of
/// `record`, not to the port -- nothing here schedules or defers a call, so
/// an implementation that blocked on disk I/O would still violate the rule
/// even though it satisfies this trait.
pub trait HistoryStore {
    /// Store one dictation and return the id it was assigned.
    fn record(&mut self, entry: NewEntry) -> crate::Result<i64>;

    /// The most recent entries, newest first, capped at `limit`.
    fn list(&self, limit: usize) -> crate::Result<Vec<Entry>>;

    /// One entry by id, or `None` if it does not exist (or was deleted).
    fn get(&self, id: i64) -> crate::Result<Option<Entry>>;

    /// Replace one entry's text with a later version of the same dictation:
    /// cleanup that came back after the paste had gone ahead without it
    /// (the 5-second paste rule: "let history catch up"). The plain text
    /// and the blocks change together, so a row never disagrees with itself;
    /// what the engine heard, the time and the latency stay as recorded.
    ///
    /// The entry is named by its id and the time it was recorded at, together.
    /// A store may give a deleted entry's id to the next one it records --
    /// SQLite does, for its newest row -- and cleanup that comes back late
    /// must never land on a different dictation that took the id.
    ///
    /// Returns whether the entry was there. One deleted or purged in the
    /// meantime is not written back, which keeps a retention rule's promise.
    fn replace_document(&mut self, id: i64, ts_ms: u64, document: &Document)
        -> crate::Result<bool>;

    /// Remove one entry. Deleting an id that is not there is not an error --
    /// the caller's goal ("this id must not be in history") is already true.
    fn delete(&mut self, id: i64) -> crate::Result<()>;

    /// Keep the newest `keep` entries and delete the rest. Returns how many
    /// were removed.
    fn prune(&mut self, keep: usize) -> crate::Result<usize>;

    /// Delete every entry recorded before `cutoff_ms`. Returns how many.
    fn delete_older_than(&mut self, cutoff_ms: u64) -> crate::Result<usize>;

    /// A setting kept beside the data it governs, so the store's rules travel
    /// with the file and two programs opening it agree.
    fn setting(&self, key: &str) -> crate::Result<Option<String>>;

    fn set_setting(&mut self, key: &str, value: &str) -> crate::Result<()>;
}

/// How long history is kept. Never longer than [`Retention::MAX_DAYS`]:
/// history is for recognising a mishearing and copying something back, not
/// an archive, and what is not kept cannot leak. It is stored locally and
/// never sent anywhere; the retention rule is the only thing that touches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Nothing is kept. A dictation is never written, and choosing this
    /// erases whatever an earlier rule had kept.
    Off,
    /// Gone when the app quits. Rows from an earlier run are gone at launch
    /// too, so a crash does not keep them.
    UntilQuit,
    /// Gone after this many days, 1 to [`Retention::MAX_DAYS`].
    Days(u32),
}

impl Retention {
    pub const MAX_DAYS: u32 = 30;
    pub const DEFAULT: Retention = Retention::Days(30);
    /// The choices a settings card offers, in order.
    pub const OPTIONS: [Retention; 5] = [
        Retention::Off,
        Retention::UntilQuit,
        Retention::Days(1),
        Retention::Days(7),
        Retention::Days(30),
    ];
    /// The key under which a store keeps it.
    pub const SETTING: &'static str = "retention";

    /// The stored form: `off`, `until-quit`, or a day count with a `d`.
    pub fn parse(text: &str) -> Option<Retention> {
        let text = text.trim();
        if text == "off" {
            return Some(Retention::Off);
        }
        if text == "until-quit" {
            return Some(Retention::UntilQuit);
        }
        let days: u32 = text.strip_suffix('d')?.parse().ok()?;
        (1..=Self::MAX_DAYS)
            .contains(&days)
            .then_some(Retention::Days(days))
    }

    /// Whether this rule keeps anything at all. The question a caller asks
    /// before writing; see [`record_if_kept`].
    pub fn keeps(self) -> bool {
        self != Retention::Off
    }

    /// Plain words for a card.
    pub fn label(self) -> String {
        match self {
            Retention::Off => "Not at all".into(),
            Retention::UntilQuit => "Until Macalla quits".into(),
            Retention::Days(1) => "1 day".into(),
            Retention::Days(n) => format!("{n} days"),
        }
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Retention::Off => f.write_str("off"),
            Retention::UntilQuit => f.write_str("until-quit"),
            Retention::Days(n) => write!(f, "{n}d"),
        }
    }
}

/// When a purge is being asked for. The rule differs by moment: a session's
/// own rows must survive its dictations under "until quit".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    Launch,
    AfterDictation,
    Quit,
}

/// The store's retention, or the default when none was ever set. An
/// unreadable value is treated as the default rather than as "keep
/// everything": the cap is the rule.
pub fn retention_of(store: &dyn HistoryStore) -> crate::Result<Retention> {
    Ok(store
        .setting(Retention::SETTING)?
        .and_then(|v| Retention::parse(&v))
        .unwrap_or(Retention::DEFAULT))
}

/// Apply the store's retention at `moment`, `now_ms` being the clock.
/// Returns how many entries went.
pub fn purge(store: &mut dyn HistoryStore, moment: Moment, now_ms: u64) -> crate::Result<usize> {
    match retention_of(store)? {
        Retention::Off => store.delete_older_than(u64::MAX),
        Retention::UntilQuit => match moment {
            Moment::Launch | Moment::Quit => store.delete_older_than(u64::MAX),
            Moment::AfterDictation => Ok(0),
        },
        Retention::Days(days) => {
            let age_ms = u64::from(days) * 24 * 60 * 60 * 1_000;
            store.delete_older_than(now_ms.saturating_sub(age_ms))
        }
    }
}

/// Record `entry` only if the store's rule keeps anything, returning the new
/// id, or `None` when history is off.
///
/// The check comes BEFORE the write, and that is the whole point. "Off"
/// promises the text never reaches the store, and a write followed by a
/// purge does not keep that promise. A rule that cannot be read is an error
/// and nothing is written: a privacy setting fails closed. A rule that reads
/// as nonsense is still the default, as [`retention_of`] explains.
///
/// Both apps call this, so there is one answer to "may this be written".
pub fn record_if_kept(store: &mut dyn HistoryStore, entry: NewEntry) -> crate::Result<Option<i64>> {
    if !retention_of(store)?.keeps() {
        return Ok(None);
    }
    store.record(entry).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The smallest store that can exercise the policy: timestamps and
    /// settings, nothing else.
    #[derive(Default)]
    struct Fake {
        rows: Vec<u64>,
        settings: HashMap<String, String>,
    }

    impl HistoryStore for Fake {
        fn record(&mut self, entry: NewEntry) -> crate::Result<i64> {
            self.rows.push(entry.ts_ms);
            Ok(self.rows.len() as i64)
        }
        fn list(&self, _limit: usize) -> crate::Result<Vec<Entry>> {
            unimplemented!()
        }
        fn get(&self, _id: i64) -> crate::Result<Option<Entry>> {
            unimplemented!()
        }
        fn replace_document(
            &mut self,
            _id: i64,
            _ts_ms: u64,
            _document: &Document,
        ) -> crate::Result<bool> {
            unimplemented!()
        }
        fn delete(&mut self, _id: i64) -> crate::Result<()> {
            unimplemented!()
        }
        fn prune(&mut self, _keep: usize) -> crate::Result<usize> {
            unimplemented!()
        }
        fn delete_older_than(&mut self, cutoff_ms: u64) -> crate::Result<usize> {
            let before = self.rows.len();
            self.rows.retain(|&ts| ts >= cutoff_ms);
            Ok(before - self.rows.len())
        }
        fn setting(&self, key: &str) -> crate::Result<Option<String>> {
            Ok(self.settings.get(key).cloned())
        }
        fn set_setting(&mut self, key: &str, value: &str) -> crate::Result<()> {
            self.settings.insert(key.into(), value.into());
            Ok(())
        }
    }

    const DAY: u64 = 24 * 60 * 60 * 1_000;

    fn store_with(ages_in_days: &[u64], now: u64) -> Fake {
        Fake {
            rows: ages_in_days.iter().map(|d| now - d * DAY).collect(),
            settings: HashMap::new(),
        }
    }

    #[test]
    fn the_stored_form_round_trips_and_is_capped() {
        for r in Retention::OPTIONS {
            assert_eq!(Retention::parse(&r.to_string()), Some(r));
        }
        assert_eq!(Retention::parse("31d"), None, "30 days is the limit");
        assert_eq!(Retention::parse("0d"), None);
        assert_eq!(Retention::parse("never"), None, "there is no never");
        assert_eq!(Retention::DEFAULT, Retention::Days(Retention::MAX_DAYS));
    }

    #[test]
    fn no_setting_means_the_default_not_keep_everything() {
        let now = 100 * DAY;
        let mut store = store_with(&[0, 29, 31, 40], now);
        assert_eq!(retention_of(&store).unwrap(), Retention::DEFAULT);
        assert_eq!(purge(&mut store, Moment::AfterDictation, now).unwrap(), 2);
        assert_eq!(store.rows.len(), 2);
    }

    #[test]
    fn an_unreadable_setting_is_the_default_too() {
        let now = 100 * DAY;
        let mut store = store_with(&[40], now);
        store.set_setting(Retention::SETTING, "forever").unwrap();
        assert_eq!(purge(&mut store, Moment::Launch, now).unwrap(), 1);
    }

    #[test]
    fn days_purge_by_age_at_every_moment() {
        let now = 100 * DAY;
        for moment in [Moment::Launch, Moment::AfterDictation, Moment::Quit] {
            let mut store = store_with(&[0, 6, 8], now);
            store.set_setting(Retention::SETTING, "7d").unwrap();
            assert_eq!(purge(&mut store, moment, now).unwrap(), 1, "{moment:?}");
            assert_eq!(store.rows.len(), 2);
        }
    }

    #[test]
    fn until_quit_keeps_the_sessions_own_rows_and_clears_at_quit_and_launch() {
        let now = 100 * DAY;
        let mut store = store_with(&[0, 0], now);
        store.set_setting(Retention::SETTING, "until-quit").unwrap();
        assert_eq!(purge(&mut store, Moment::AfterDictation, now).unwrap(), 0);
        assert_eq!(purge(&mut store, Moment::Quit, now).unwrap(), 2);
        let mut store = store_with(&[0], now);
        store.set_setting(Retention::SETTING, "until-quit").unwrap();
        assert_eq!(
            purge(&mut store, Moment::Launch, now).unwrap(),
            1,
            "a crash must not keep the last run's rows"
        );
    }

    #[test]
    fn labels_are_plain_words() {
        assert_eq!(Retention::UntilQuit.label(), "Until Macalla quits");
        assert_eq!(Retention::Days(1).label(), "1 day");
        assert_eq!(Retention::Days(30).label(), "30 days");
        assert_eq!(Retention::Off.label(), "Not at all");
    }

    fn entry_at(ts_ms: u64) -> NewEntry {
        let document = Document::new(vec![crate::block::Block::Paragraph {
            text: "heard".into(),
        }]);
        NewEntry {
            ts_ms,
            app_id: None,
            engine: "apple".into(),
            raw: "heard".into(),
            enhanced: "heard".into(),
            schema_version: document.schema_version,
            document,
            latency: None,
            audio_path: None,
        }
    }

    /// A store whose settings cannot be read, as a locked or corrupt file.
    #[derive(Default)]
    struct Unreadable {
        written: usize,
    }

    impl HistoryStore for Unreadable {
        fn record(&mut self, _entry: NewEntry) -> crate::Result<i64> {
            self.written += 1;
            Ok(1)
        }
        fn list(&self, _limit: usize) -> crate::Result<Vec<Entry>> {
            unimplemented!()
        }
        fn get(&self, _id: i64) -> crate::Result<Option<Entry>> {
            unimplemented!()
        }
        fn replace_document(
            &mut self,
            _id: i64,
            _ts_ms: u64,
            _document: &Document,
        ) -> crate::Result<bool> {
            unimplemented!()
        }
        fn delete(&mut self, _id: i64) -> crate::Result<()> {
            unimplemented!()
        }
        fn prune(&mut self, _keep: usize) -> crate::Result<usize> {
            unimplemented!()
        }
        fn delete_older_than(&mut self, _cutoff_ms: u64) -> crate::Result<usize> {
            unimplemented!()
        }
        fn setting(&self, _key: &str) -> crate::Result<Option<String>> {
            Err(crate::Error::History("the file is locked".into()))
        }
        fn set_setting(&mut self, _key: &str, _value: &str) -> crate::Result<()> {
            unimplemented!()
        }
    }

    #[test]
    fn off_is_its_own_stored_form_and_is_not_never() {
        // "never" stays rejected, as the test above says: it reads as "never
        // delete", which the cap forbids. Off means the opposite, nothing
        // kept, so it gets a word that cannot be read both ways.
        assert_eq!(Retention::parse("off"), Some(Retention::Off));
        assert_eq!(Retention::Off.to_string(), "off");
        assert_eq!(Retention::parse("never"), None);
    }

    #[test]
    fn off_erases_everything_at_every_moment() {
        let now = 100 * DAY;
        for moment in [Moment::Launch, Moment::AfterDictation, Moment::Quit] {
            let mut store = store_with(&[0, 0, 3], now);
            store.set_setting(Retention::SETTING, "off").unwrap();
            assert_eq!(purge(&mut store, moment, now).unwrap(), 3, "{moment:?}");
            assert!(store.rows.is_empty());
        }
    }

    #[test]
    fn only_off_keeps_nothing() {
        assert!(!Retention::Off.keeps());
        for r in Retention::OPTIONS
            .into_iter()
            .filter(|r| *r != Retention::Off)
        {
            assert!(r.keeps(), "{r:?}");
        }
    }

    #[test]
    fn with_history_off_a_dictation_is_never_written() {
        let now = 100 * DAY;
        let mut store = store_with(&[], now);
        store.set_setting(Retention::SETTING, "off").unwrap();
        assert_eq!(record_if_kept(&mut store, entry_at(now)).unwrap(), None);
        assert!(
            store.rows.is_empty(),
            "written and deleted later is not the same promise"
        );

        store.set_setting(Retention::SETTING, "7d").unwrap();
        assert!(record_if_kept(&mut store, entry_at(now)).unwrap().is_some());
        assert_eq!(store.rows.len(), 1);
    }

    #[test]
    fn a_rule_that_cannot_be_read_writes_nothing_and_says_so() {
        // Fail closed. If the store cannot say whether history is on, a
        // privacy setting must not guess yes -- and must not hide that it
        // could not tell.
        let mut store = Unreadable::default();
        assert!(record_if_kept(&mut store, entry_at(DAY)).is_err());
        assert_eq!(store.written, 0);
    }
}
