// SPDX-License-Identifier: MIT
//! The SQLite adapter for `macalla_core::history::HistoryStore`.
//!
//! The model and the port live in `macalla-core` (see its `history` module);
//! this crate owns the database because `macalla-core` must build and test
//! headless, with no database driver to cross-compile. Schema is
//! the design's `dictations` table, verbatim.
//!
//! **With Enhancement on, the paste waits for it
//! up to [`crate::ENHANCEMENT_WAIT`] and no longer, and nothing here adds to
//! that.** Recording to history happens AFTER the paste, outside that wait.
//! This module does not enforce that -- a
//! `record` call here is a synchronous disk write like any other SQLite
//! insert -- so the rule is the caller's to keep: call `record` after paste
//! has already happened, never before or during it.

use macalla_core::block::Document;
use macalla_core::history::{Entry, HistoryStore, NewEntry};
use macalla_core::latency;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

use crate::{Error, Result};

/// The `dictations` table, plus a `meta` table so a future migration has a
/// version to read. `user_version` (SQLite's own pragma) would do the same
/// job; `meta` was chosen so the version sits next to the data it describes,
/// in a table the schema itself already needs for anything else it later
/// wants to remember (Ogham sync cursor, audio retention cap...).
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS dictations (
        id              INTEGER PRIMARY KEY,
        ts              INTEGER NOT NULL,
        app_id          TEXT,
        engine          TEXT NOT NULL,
        raw             TEXT NOT NULL,
        enhanced        TEXT NOT NULL,
        blocks_json     TEXT NOT NULL,
        schema_version  INTEGER NOT NULL,
        latency_json    TEXT,
        audio_path      TEXT
    );
    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT
    );
";

/// The current `db_version`, recorded in `meta` on first open so a later
/// migration knows what it is starting from.
const DB_VERSION: &str = "1";

fn sqlite_error(context: &str, e: rusqlite::Error) -> macalla_core::Error {
    macalla_core::Error::History(format!("{context}: {e}"))
}

/// A history store backed by a SQLite file (or an in-memory database for
/// tests).
pub struct SqliteHistory {
    conn: Connection,
}

impl SqliteHistory {
    /// Open (or create) a history database at `path`, creating its parent
    /// directory if needed. This is the OS app-data dir path the design
    /// names; callers pick the path, this just makes sure it is reachable.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::from(macalla_core::Error::History(format!(
                        "creating {}: {e}",
                        parent.display()
                    )))
                })?;
            }
        }
        let conn = Connection::open(path).map_err(|e| sqlite_error("opening the database", e))?;
        Self::from_connection(conn)
    }

    /// An in-memory database. No file, no cleanup -- for tests and the CLI's
    /// dry runs.
    pub fn in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().map_err(|e| sqlite_error("opening in memory", e))?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL is the one mechanism used to keep the store readable while a
        // write is in flight; the `meta.db_version` row is the one mechanism
        // used to version the schema. SQLite quietly keeps an in-memory
        // database in its own (non-WAL) journal mode, so this is a no-op
        // there rather than an error.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| sqlite_error("setting journal_mode", e))?;
        // Deleted history has to be gone from the bytes, not only from the
        // index. Without this SQLite frees a row's space and leaves its text
        // there, readable with a hex editor, until something overwrites it.
        // `scrub` deals with the second copy that WAL mode keeps in the log.
        conn.pragma_update(None, "secure_delete", "ON")
            .map_err(|e| sqlite_error("setting secure_delete", e))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| sqlite_error("creating the schema", e))?;
        conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('db_version', ?1)",
            params![DB_VERSION],
        )
        .map_err(|e| sqlite_error("recording db_version", e))?;
        Ok(Self { conn })
    }

    /// Make deleted rows unrecoverable from the files on disk. secure_delete
    /// overwrites the freed space, but in WAL mode an earlier copy of the
    /// page still sits in the log until a checkpoint, so fold the log into
    /// the database and truncate it to nothing. Called only after a delete
    /// that removed something, so an ordinary dictation pays nothing.
    fn scrub(&self) -> macalla_core::Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .map_err(|e| sqlite_error("clearing the write-ahead log", e))
    }

    /// Replace one row with a later version of the same dictation, named by
    /// its id and time as [`HistoryStore::replace_document`] names it: what a
    /// cloud engine answered, in place of what this machine heard while the
    /// request was out. Unlike late cleanup this is a different engine's
    /// words, so the engine, what it heard and the timing change with the
    /// text. The time, the app and the audio path stay as recorded.
    ///
    /// Returns whether the row was there. One deleted, or purged by
    /// retention, in the meantime is not written back.
    pub fn replace_entry(
        &mut self,
        id: i64,
        ts_ms: u64,
        entry: &NewEntry,
    ) -> macalla_core::Result<bool> {
        let blocks_json = serde_json::to_string(&entry.document)
            .map_err(|e| macalla_core::Error::History(format!("encoding the document: {e}")))?;
        let latency_json = entry
            .latency
            .map(|l| serde_json::to_string(&l))
            .transpose()
            .map_err(|e| macalla_core::Error::History(format!("encoding the latency: {e}")))?;
        // An UPDATE, never an upsert, for the reason replace_document gives.
        let changed = self
            .conn
            .execute(
                "UPDATE dictations
                 SET engine = ?1, raw = ?2, enhanced = ?3, blocks_json = ?4,
                     schema_version = ?5, latency_json = ?6
                 WHERE id = ?7 AND ts = ?8",
                params![
                    entry.engine,
                    entry.raw,
                    entry.enhanced,
                    blocks_json,
                    entry.schema_version,
                    latency_json,
                    id,
                    ts_ms as i64
                ],
            )
            .map_err(|e| sqlite_error("replacing a dictation", e))?;
        Ok(changed > 0)
    }
}

impl HistoryStore for SqliteHistory {
    fn record(&mut self, entry: NewEntry) -> macalla_core::Result<i64> {
        let blocks_json = serde_json::to_string(&entry.document)
            .map_err(|e| macalla_core::Error::History(format!("encoding the document: {e}")))?;
        let latency_json = entry
            .latency
            .map(|l| serde_json::to_string(&l))
            .transpose()
            .map_err(|e| macalla_core::Error::History(format!("encoding the latency: {e}")))?;
        self.conn
            .execute(
                "INSERT INTO dictations
                    (ts, app_id, engine, raw, enhanced, blocks_json, schema_version, latency_json, audio_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    entry.ts_ms as i64,
                    entry.app_id,
                    entry.engine,
                    entry.raw,
                    entry.enhanced,
                    blocks_json,
                    entry.schema_version,
                    latency_json,
                    entry.audio_path,
                ],
            )
            .map_err(|e| sqlite_error("recording a dictation", e))?;
        Ok(self.conn.last_insert_rowid())
    }

    fn list(&self, limit: usize) -> macalla_core::Result<Vec<Entry>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, ts, app_id, engine, raw, enhanced, blocks_json, schema_version,
                        latency_json, audio_path
                 FROM dictations
                 ORDER BY ts DESC, id DESC
                 LIMIT ?1",
            )
            .map_err(|e| sqlite_error("preparing list", e))?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_entry)
            .map_err(|e| sqlite_error("listing dictations", e))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| sqlite_error("reading a dictation row", e))
    }

    fn get(&self, id: i64) -> macalla_core::Result<Option<Entry>> {
        self.conn
            .query_row(
                "SELECT id, ts, app_id, engine, raw, enhanced, blocks_json, schema_version,
                        latency_json, audio_path
                 FROM dictations WHERE id = ?1",
                params![id],
                row_to_entry,
            )
            .optional()
            .map_err(|e| sqlite_error("reading a dictation", e))
    }

    fn replace_document(
        &mut self,
        id: i64,
        ts_ms: u64,
        document: &Document,
    ) -> macalla_core::Result<bool> {
        let blocks_json = serde_json::to_string(document)
            .map_err(|e| macalla_core::Error::History(format!("encoding the document: {e}")))?;
        // An UPDATE, never an upsert: a row deleted or purged while cleanup
        // ran stays gone. The time as well as the id, because `id INTEGER
        // PRIMARY KEY` without AUTOINCREMENT hands a deleted newest row's id
        // to the next dictation.
        let changed = self
            .conn
            .execute(
                "UPDATE dictations SET enhanced = ?1, blocks_json = ?2, schema_version = ?3
                 WHERE id = ?4 AND ts = ?5",
                params![
                    document.to_plain(),
                    blocks_json,
                    document.schema_version,
                    id,
                    ts_ms as i64
                ],
            )
            .map_err(|e| sqlite_error("replacing a dictation's text", e))?;
        Ok(changed > 0)
    }

    fn delete(&mut self, id: i64) -> macalla_core::Result<()> {
        // Idempotent: deleting an id that is already gone leaves the world in
        // the state the caller asked for ("this id is not in history"), so
        // it is not an error to ask twice.
        let removed = self
            .conn
            .execute("DELETE FROM dictations WHERE id = ?1", params![id])
            .map_err(|e| sqlite_error("deleting a dictation", e))?;
        if removed > 0 {
            self.scrub()?;
        }
        Ok(())
    }

    fn delete_older_than(&mut self, cutoff_ms: u64) -> macalla_core::Result<usize> {
        // i64::MAX rather than u64::MAX: SQLite integers are signed, and
        // "everything" must not become "nothing" at the type boundary.
        let cutoff = i64::try_from(cutoff_ms).unwrap_or(i64::MAX);
        let removed = self
            .conn
            .execute("DELETE FROM dictations WHERE ts < ?1", params![cutoff])
            .map_err(|e| sqlite_error("deleting old history", e))?;
        if removed > 0 {
            self.scrub()?;
        }
        Ok(removed)
    }

    fn setting(&self, key: &str) -> macalla_core::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(|e| sqlite_error("reading a history setting", e))
    }

    fn set_setting(&mut self, key: &str, value: &str) -> macalla_core::Result<()> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
            .map_err(|e| sqlite_error("writing a history setting", e))
    }

    fn prune(&mut self, keep: usize) -> macalla_core::Result<usize> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM dictations WHERE id NOT IN (
                    SELECT id FROM dictations ORDER BY ts DESC, id DESC LIMIT ?1
                 )",
                params![keep as i64],
            )
            .map_err(|e| sqlite_error("pruning history", e))?;
        if removed > 0 {
            self.scrub()?;
        }
        Ok(removed)
    }
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<Entry> {
    let blocks_json: String = row.get(6)?;
    let document: Document = serde_json::from_str(&blocks_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let latency_json: Option<String> = row.get(8)?;
    let latency: Option<latency::Dictation> = latency_json
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let ts_ms: i64 = row.get(1)?;
    Ok(Entry {
        id: row.get(0)?,
        ts_ms: ts_ms as u64,
        app_id: row.get(2)?,
        engine: row.get(3)?,
        raw: row.get(4)?,
        enhanced: row.get(5)?,
        document,
        schema_version: row.get(7)?,
        latency,
        audio_path: row.get(9)?,
    })
}

/// Build a [`NewEntry`] from a finished [`crate::Dictation`].
///
/// `app_id` is the app in front when the dictation started,
/// [`crate::Dictation::front_app`]: kept for a cloud dictation and for one
/// with Enhancement on, and `None` for a local dictation with Enhancement off
/// or where the platform could not say. `audio_path` is `None`: audio retention is off
/// by default and not
/// implemented anywhere in this crate.
pub fn entry_from(dictation: &crate::Dictation, engine: &str, ts_ms: u64) -> NewEntry {
    NewEntry {
        ts_ms,
        app_id: dictation.front_app.clone(),
        engine: engine.to_string(),
        raw: macalla_core::stt::final_text(&dictation.transcripts),
        enhanced: dictation.document.to_plain(),
        document: dictation.document.clone(),
        schema_version: dictation.document.schema_version,
        latency: Some(dictation.timing),
        audio_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::block::Block;

    fn sample_document() -> Document {
        Document::new(vec![
            Block::Heading {
                text: "Standup notes".into(),
            },
            Block::ListItem {
                text: "Shipped the history store".into(),
                ordered: true,
            },
            Block::Paragraph {
                text: "That is all.".into(),
            },
        ])
    }

    fn sample_latency() -> latency::Dictation {
        latency::Dictation {
            audio_ms: 1_200,
            open_ms: Some(12.0),
            engine_ms: Some(8.0),
            vad_ms: None,
            stt_ms: Some(150.0),
            cloud_ms: None,
            tier0_ms: Some(4.0),
            tier1_ms: None,
            paste_ms: Some(6.0),
            total_ms: 180.0,
            overruns: 0,
        }
    }

    fn sample_entry(ts_ms: u64) -> NewEntry {
        NewEntry {
            ts_ms,
            app_id: Some("com.apple.Notes".into()),
            engine: "apple".into(),
            raw: "shipped the history store".into(),
            enhanced: "Standup notes\nShipped the history store\nThat is all.".into(),
            document: sample_document(),
            schema_version: sample_document().schema_version,
            latency: Some(sample_latency()),
            audio_path: None,
        }
    }

    #[test]
    fn record_then_get_round_trips_every_field() {
        let mut store = SqliteHistory::in_memory().unwrap();
        let id = store.record(sample_entry(1_000)).unwrap();

        let got = store.get(id).unwrap().expect("just recorded it");
        assert_eq!(got.id, id);
        assert_eq!(got.ts_ms, 1_000);
        assert_eq!(got.app_id.as_deref(), Some("com.apple.Notes"));
        assert_eq!(got.engine, "apple");
        assert_eq!(got.raw, "shipped the history store");
        assert_eq!(
            got.enhanced,
            "Standup notes\nShipped the history store\nThat is all."
        );
        assert_eq!(got.document, sample_document(), "the document round-trips");
        assert_eq!(got.schema_version, sample_document().schema_version);
        assert_eq!(got.latency, Some(sample_latency()));
        assert_eq!(got.audio_path, None);

        // A cloud dictation's request time survives real storage as well.
        let cloud = latency::Dictation {
            cloud_ms: Some(2_400.0),
            ..sample_latency()
        };
        let id = store
            .record(NewEntry {
                latency: Some(cloud),
                ..sample_entry(2_000)
            })
            .unwrap();
        let got = store.get(id).unwrap().expect("just recorded it");
        assert_eq!(got.latency, Some(cloud));
    }

    #[test]
    fn get_of_a_missing_id_is_none_not_an_error() {
        let store = SqliteHistory::in_memory().unwrap();
        assert_eq!(store.get(999).unwrap(), None);
    }

    #[test]
    fn list_is_newest_first_and_honours_the_limit() {
        let mut store = SqliteHistory::in_memory().unwrap();
        store.record(sample_entry(1_000)).unwrap();
        store.record(sample_entry(2_000)).unwrap();
        store.record(sample_entry(3_000)).unwrap();

        let all = store.list(10).unwrap();
        assert_eq!(
            all.iter().map(|e| e.ts_ms).collect::<Vec<_>>(),
            vec![3_000, 2_000, 1_000],
            "newest first"
        );

        let capped = store.list(2).unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].ts_ms, 3_000);
        assert_eq!(capped[1].ts_ms, 2_000);
    }

    #[test]
    fn delete_removes_an_entry_and_is_idempotent() {
        let mut store = SqliteHistory::in_memory().unwrap();
        let id = store.record(sample_entry(1_000)).unwrap();

        store.delete(id).unwrap();
        assert_eq!(store.get(id).unwrap(), None);

        // Deleting the same id again is Ok: the world already matches what
        // the caller asked for.
        store.delete(id).unwrap();
    }

    #[test]
    fn prune_keeps_the_newest_n_and_reports_how_many_it_removed() {
        let mut store = SqliteHistory::in_memory().unwrap();
        for ts in [1_000, 2_000, 3_000, 4_000, 5_000] {
            store.record(sample_entry(ts)).unwrap();
        }

        let removed = store.prune(2).unwrap();
        assert_eq!(removed, 3);

        let remaining = store.list(10).unwrap();
        assert_eq!(
            remaining.iter().map(|e| e.ts_ms).collect::<Vec<_>>(),
            vec![5_000, 4_000]
        );
    }

    #[test]
    fn delete_older_than_is_by_timestamp_and_everything_means_everything() {
        let mut store = SqliteHistory::in_memory().unwrap();
        for ts in [1_000, 2_000, 3_000] {
            store.record(sample_entry(ts)).unwrap();
        }
        assert_eq!(store.delete_older_than(2_000).unwrap(), 1);
        assert_eq!(store.list(10).unwrap().len(), 2);
        assert_eq!(store.delete_older_than(u64::MAX).unwrap(), 2);
        assert!(store.list(10).unwrap().is_empty());
    }

    #[test]
    fn a_setting_lives_in_the_file_beside_the_data() {
        let mut store = SqliteHistory::in_memory().unwrap();
        assert_eq!(store.setting("retention").unwrap(), None);
        store.set_setting("retention", "7d").unwrap();
        store.set_setting("retention", "1d").unwrap();
        assert_eq!(store.setting("retention").unwrap().as_deref(), Some("1d"));
        assert_eq!(
            store.setting("db_version").unwrap().as_deref(),
            Some("1"),
            "the version row shares the table"
        );
    }

    #[test]
    fn open_on_a_fresh_path_creates_the_file_and_reopening_keeps_the_data() {
        let dir = std::env::temp_dir().join(format!(
            "macalla-history-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db_path = dir.join("history.sqlite3");
        assert!(!db_path.exists());

        let id = {
            let mut store = SqliteHistory::open(&db_path).unwrap();
            assert!(db_path.exists(), "opening creates the file");
            store.record(sample_entry(42)).unwrap()
        };

        {
            let store = SqliteHistory::open(&db_path).unwrap();
            let got = store.get(id).unwrap().expect("data survives reopening");
            assert_eq!(got.ts_ms, 42);
        }

        std::fs::remove_dir_all(&dir).expect("clean up the temp dir");
    }

    /// A fresh directory for a test that needs a real file. Same shape as the
    /// reopening test above.
    fn scratch_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "macalla-history-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Every byte SQLite keeps for a database: the file and its write-ahead
    /// log. Recovering deleted text means reading exactly these.
    fn bytes_on_disk(db: &Path) -> Vec<u8> {
        let mut all = std::fs::read(db).unwrap_or_default();
        let mut wal = db.as_os_str().to_owned();
        wal.push("-wal");
        all.extend(std::fs::read(std::path::PathBuf::from(wal)).unwrap_or_default());
        all
    }

    fn holds(bytes: &[u8], text: &str) -> bool {
        bytes.windows(text.len()).any(|w| w == text.as_bytes())
    }

    fn entry_saying(text: &str, ts_ms: u64) -> NewEntry {
        let mut entry = sample_entry(ts_ms);
        entry.raw = text.into();
        entry.enhanced = text.into();
        entry.document = Document::new(vec![Block::Paragraph { text: text.into() }]);
        entry
    }

    #[test]
    fn an_erased_dictation_leaves_nothing_readable_in_the_file() {
        // SQLite frees a deleted row's space without clearing it, and in WAL
        // mode an earlier copy also sits in the log. So "deleted" text can be
        // read straight out of the file with a hex editor until something
        // happens to overwrite it. History that has been erased must be
        // erased in the bytes, on every path that deletes.
        const MARKER: &str = "zebra-marmalade-quorum-7731";
        for how in ["delete", "delete_older_than", "prune"] {
            let dir = scratch_dir(how);
            let db = dir.join("history.sqlite3");
            let mut store = SqliteHistory::open(&db).unwrap();
            let id = store.record(entry_saying(MARKER, 1_000)).unwrap();
            assert!(
                holds(&bytes_on_disk(&db), MARKER),
                "{how}: the text should be on disk before it is erased, or this test proves nothing"
            );
            match how {
                "delete" => store.delete(id).unwrap(),
                "delete_older_than" => {
                    store.delete_older_than(u64::MAX).unwrap();
                }
                _ => {
                    store.prune(0).unwrap();
                }
            }
            // While the app still has the file open, which is how it runs.
            assert!(
                !holds(&bytes_on_disk(&db), MARKER),
                "{how}: erased text is still readable in the open file"
            );
            drop(store);
            assert!(
                !holds(&bytes_on_disk(&db), MARKER),
                "{how}: erased text is still readable after closing"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_later_document_replaces_the_text_and_blocks_and_leaves_the_rest() {
        // Cleanup that finished after the paste: History catches up.
        let mut store = SqliteHistory::in_memory().unwrap();
        let id = store.record(entry_saying("kubectl ran", 1_000)).unwrap();
        let cleaned = Document::new(vec![Block::Paragraph {
            text: "kubectl ran.".into(),
        }]);
        assert!(store.replace_document(id, 1_000, &cleaned).unwrap());

        let got = store.get(id).unwrap().expect("still there");
        assert_eq!(got.enhanced, "kubectl ran.");
        assert_eq!(got.document, cleaned, "the blocks agree with the text");
        assert_eq!(got.schema_version, cleaned.schema_version);
        assert_eq!(got.raw, "kubectl ran", "what the engine heard stays");
        assert_eq!(got.ts_ms, 1_000);
        assert_eq!(got.engine, "apple");
        assert_eq!(got.latency, Some(sample_latency()));

        // Deleted, or purged by retention, while cleanup ran: not written back.
        store.delete(id).unwrap();
        assert!(!store.replace_document(id, 1_000, &cleaned).unwrap());
        assert_eq!(store.get(id).unwrap(), None);
    }

    #[test]
    fn a_later_entry_replaces_the_engine_what_was_heard_and_the_text_in_the_same_row() {
        // What this machine heard, kept while a cloud request was out, then
        // the provider's answer: the row must name the engine whose words it
        // holds and what that engine heard, not the preview's.
        let mut store = SqliteHistory::in_memory().unwrap();
        let mut early = entry_saying("cube control ran", 1_000);
        early.app_id = Some("com.example.notes".into());
        let id = store.record(early).unwrap();

        let mut answer = entry_saying("kubectl ran.", 1_000);
        answer.engine = "cloud-fake".into();
        answer.raw = "kubectl ran".into();
        answer.latency = Some(latency::Dictation {
            cloud_ms: Some(4_000.0),
            ..sample_latency()
        });
        assert!(store.replace_entry(id, 1_000, &answer).unwrap());

        let got = store.get(id).unwrap().expect("still there");
        assert_eq!(got.engine, "cloud-fake");
        assert_eq!(got.raw, "kubectl ran");
        assert_eq!(got.enhanced, "kubectl ran.");
        assert_eq!(got.document, answer.document);
        assert_eq!(got.schema_version, answer.schema_version);
        assert_eq!(got.latency, answer.latency);
        assert_eq!(got.ts_ms, 1_000, "the same row");
        assert_eq!(got.app_id.as_deref(), Some("com.example.notes"));
        assert_eq!(store.list(10).unwrap().len(), 1, "replaced, not added to");

        // The time as well as the id, and never written back once gone.
        assert!(!store.replace_entry(id, 999, &answer).unwrap());
        store.delete(id).unwrap();
        assert!(!store.replace_entry(id, 1_000, &answer).unwrap());
        assert_eq!(store.get(id).unwrap(), None);
    }

    #[test]
    fn late_cleanup_for_a_deleted_dictation_never_lands_on_the_one_that_took_its_id() {
        let mut store = SqliteHistory::in_memory().unwrap();
        let first = store
            .record(entry_saying("the first dictation", 1_000))
            .unwrap();
        store.delete(first).unwrap();
        let second = store
            .record(entry_saying("the second dictation", 2_000))
            .unwrap();
        assert_eq!(
            second, first,
            "SQLite gives the id out again, or this test proves nothing"
        );

        let late = Document::new(vec![Block::Paragraph {
            text: "The first dictation.".into(),
        }]);
        assert!(!store.replace_document(first, 1_000, &late).unwrap());
        let got = store.get(second).unwrap().expect("the second is there");
        assert_eq!(got.enhanced, "the second dictation");
        assert_eq!(got.ts_ms, 2_000);
    }

    #[test]
    fn with_history_off_a_dictation_never_reaches_the_file() {
        const MARKER: &str = "quince-lantern-8842";
        let dir = scratch_dir("off");
        let db = dir.join("history.sqlite3");
        let mut store = SqliteHistory::open(&db).unwrap();
        store
            .set_setting(macalla_core::history::Retention::SETTING, "off")
            .unwrap();
        let written =
            macalla_core::history::record_if_kept(&mut store, entry_saying(MARKER, 1_000)).unwrap();
        assert_eq!(written, None);
        assert!(!holds(&bytes_on_disk(&db), MARKER), "open file");
        drop(store);
        assert!(!holds(&bytes_on_disk(&db), MARKER), "closed file");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
