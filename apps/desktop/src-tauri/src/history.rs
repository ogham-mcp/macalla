// SPDX-License-Identifier: MIT
//! The history store, as the app holds it.
//!
//! One SQLite file under the app-data directory, behind a mutex so the
//! dictation thread can record into it and a command on the UI thread can
//! list from it. Recording happens after the paste and never before it
//! (the paste comes first): the paste is what the user is waiting for,
//! and a slow disk must not be in front of it.

use crate::command_error::{CommandError, OrSay};
use macalla_core::history::{self, HistoryStore, Moment, Retention};
use macalla_session::history::{entry_from, SqliteHistory};
use macalla_session::Dictation;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, Wry};

/// The file name inside the app-data directory.
pub const FILE_NAME: &str = "history.sqlite";

/// A safety net under the time rule: however busy a month, the list stays
/// a list. The rule people set is [`Retention`], by age, capped at 30 days.
const KEEP: usize = 500;

/// Said when the store could not be opened at launch ([`HistoryHandle::open`],
/// the only place that opens it) and a setting that lives in it is asked
/// for. Restarting really is the fix: nothing here reopens it later.
/// Deliberately not "History is off", which the History card already uses
/// for the "Keep for: Not at all" retention choice -- a different thing a
/// person chose, not a failure at launch.
const HISTORY_OFF: &str =
    "History could not be opened when Macalla started, so nothing is kept. Restart Macalla to \
     try again.";

/// The same, when what was asked for was saving a choice rather than reading
/// one.
const HISTORY_OFF_CHOICE: &str = "History could not be opened when Macalla started, so this \
                                   choice could not be saved. Restart Macalla to try again.";

/// Shared access to the store. `None` when it could not be opened, in which
/// case dictation carries on and the History view says so.
#[derive(Clone)]
pub struct HistoryHandle(Arc<Mutex<Option<SqliteHistory>>>);

/// The settings key for the chosen engine id.
const ENGINE_SETTING: &str = "engine";

/// The settings key for the chosen input device, by name. Empty means the
/// system default, because the store has no way to remove a key.
const DEVICE_SETTING: &str = "device";

/// The settings key for a chosen cloud transcription provider id. Beside
/// [`ENGINE_SETTING`], not instead of it: choosing a provider does not
/// forget the local engine, which keeps showing the live preview while the
/// provider transcribes the whole recording. Empty means no
/// cloud choice -- the same convention [`DEVICE_SETTING`] uses, because the
/// store has no way to remove a key.
const CLOUD_PROVIDER_SETTING: &str = "cloud_provider";

/// The settings key for the chosen provider's model, when one is needed
/// (OpenRouter, which offers many). Empty means none chosen.
const CLOUD_MODEL_SETTING: &str = "cloud_model";

/// A stored cloud transcription choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudChoice {
    pub provider: String,
    pub model: Option<String>,
}

/// The settings key for the cleanup choice's kind: `"off"`,
/// `"apple-intelligence"`, `"server"` or `"pool"`. Empty (before anyone has
/// chosen) is read as `Off`, the same convention [`CLOUD_PROVIDER_SETTING`]
/// uses.
const CLEANUP_KIND_SETTING: &str = "cleanup_kind";

/// The settings key for a server choice's address. Empty for every other kind.
const CLEANUP_ADDRESS_SETTING: &str = "cleanup_address";

/// The settings key for a server choice's model. Empty for every other kind.
const CLEANUP_MODEL_SETTING: &str = "cleanup_model";

/// The settings key for a pool choice's ticked models, as a JSON list in
/// ticked order. Empty for every other kind.
const CLEANUP_POOL_SETTING: &str = "cleanup_pool";

/// One ticked model in a stored pool: a provider id and that provider's own
/// model id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolEntry {
    pub provider: String,
    pub model: String,
}

/// A stored cleanup choice, mirroring
/// [`macalla_session::choice::CleanupChoice`] as its own type -- kept apart
/// for the same reason [`CloudChoice`] is kept apart from
/// [`macalla_session::choice::CloudTranscription`]: what this module
/// persists and what a use case builds from can change independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupChoice {
    Off,
    AppleIntelligence,
    Server {
        address: String,
        model: String,
    },
    /// Models from cloud providers, in ticked order.
    Pool {
        models: Vec<PoolEntry>,
    },
}

/// The row a dictation was kept in: its id and the time it was recorded at.
/// Both, because the store gives a deleted newest row's id to the next
/// dictation, and late cleanup must land on the dictation it cleaned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recorded {
    pub id: i64,
    pub ts_ms: u64,
}

/// One row as the History view shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HistoryItem {
    pub id: i64,
    pub ts_ms: u64,
    /// What was pasted.
    pub text: String,
    /// What the engine heard, before the dictionary and Tier 0. Shown beside
    /// the text so a mishearing can be recognised and added to the
    /// dictionary later.
    pub raw: String,
    /// This row's recording is held after its cloud request failed, so the
    /// page offers to send it to the provider again. Only ever one row: see
    /// [`offer_send_again`]. False as [`HistoryHandle::list`] reads it.
    pub send_again: bool,
}

impl HistoryHandle {
    /// Open the app's history file, or log why not and carry on without.
    pub fn open(app: &AppHandle<Wry>) -> Self {
        let store = app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())
            .and_then(|dir| SqliteHistory::open(&dir.join(FILE_NAME)).map_err(|e| e.to_string()));
        match store {
            Ok(store) => {
                let handle = Self::from_store(store);
                handle.purge(Moment::Launch);
                handle
            }
            Err(e) => {
                tracing::error!(error = %e, "history is off: the store could not be opened");
                Self(Arc::new(Mutex::new(None)))
            }
        }
    }

    fn from_store(store: SqliteHistory) -> Self {
        Self(Arc::new(Mutex::new(Some(store))))
    }

    /// An in-memory store, for other modules' tests (`commands.rs`'s
    /// `transcription_choose` tests need one and cannot reach this module's
    /// own private `handle()` helper). Production code always goes through
    /// [`HistoryHandle::open`].
    #[cfg(test)]
    pub(crate) fn in_memory_for_test() -> Self {
        Self::from_store(SqliteHistory::in_memory().unwrap())
    }

    /// Every row as stored, newest first, for tests that check which engine
    /// a row names and what it heard, which [`HistoryItem`] does not carry.
    #[cfg(test)]
    pub(crate) fn entries_for_test(&self) -> Vec<history::Entry> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .as_ref()
            .map(|store| store.list(KEEP).unwrap())
            .unwrap_or_default()
    }

    /// Keep a dictation, and say which row it is in so cleanup that finishes
    /// after the paste can replace its text. `None` when nothing was kept.
    /// Errors are logged, never returned: by the time this runs the text is
    /// already in front of the user.
    pub fn record(&self, dictation: &Dictation, engine: &str) -> Option<Recorded> {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let store = guard.as_mut()?;
        let ts_ms = now_ms();
        let entry = entry_from(dictation, engine, ts_ms);
        // Through the core's gate, not store.record: with history off the
        // text must never reach the file, and "written then purged" is not
        // that. The CLI goes through the same function.
        let id = match history::record_if_kept(store, entry) {
            Ok(Some(id)) => {
                tracing::debug!(id, "recorded in history");
                Some(Recorded { id, ts_ms })
            }
            Ok(None) => {
                tracing::debug!("history is off; nothing recorded");
                None
            }
            Err(e) => {
                tracing::error!(error = %e, "could not record in history");
                None
            }
        };
        if let Err(e) = store.prune(KEEP) {
            tracing::error!(error = %e, "could not prune history");
        }
        drop(guard);
        self.purge(Moment::AfterDictation);
        id
    }

    /// Put the cleaned document in place of the text a dictation was pasted
    /// with, once Enhancement that missed the paste has come back. Errors
    /// are logged: the pasted text is still there, and still what was said.
    /// Whether the row was there to take it: false when it was deleted or
    /// purged in the meantime, when History is off, or on an error.
    pub fn replace_document(
        &self,
        row: Recorded,
        document: &macalla_core::block::Document,
    ) -> bool {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return false;
        };
        let id = row.id;
        match store.replace_document(row.id, row.ts_ms, document) {
            Ok(true) => {
                tracing::debug!(id, "late cleanup reached history");
                true
            }
            Ok(false) => {
                tracing::debug!(id, "late cleanup came back for a row that has gone");
                false
            }
            Err(e) => {
                tracing::error!(error = %e, "could not put late cleanup in history");
                false
            }
        }
    }

    /// Put a finished dictation in place of the row kept while its cloud
    /// request was out: the text, and the engine and what it heard with it,
    /// so the row never names the preview engine for the provider's words.
    /// Whether the row was there to take it: false when it was deleted or
    /// purged in the meantime, when History is off, or on an error, which is
    /// logged.
    pub fn replace_entry(&self, row: Recorded, dictation: &Dictation, engine: &str) -> bool {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return false;
        };
        let entry = entry_from(dictation, engine, row.ts_ms);
        match replace_in(store, row, &entry) {
            Ok(replaced) => {
                if !replaced {
                    tracing::debug!(id = row.id, "the row to replace has gone");
                }
                replaced
            }
            Err(e) => {
                tracing::error!(error = %e, "could not replace a dictation in history");
                false
            }
        }
    }

    /// Put what Send again brought back in place of row `row`: the cloud
    /// engine's words, under that engine and with what it heard, as a
    /// dictation it transcribed is kept. The time, the app and the timing
    /// stay as the dictation recorded them. Whether the row was there to take
    /// it: false when it was deleted or purged in the meantime, when History
    /// is off, or on an error, which is logged.
    pub fn replace_sent_again(
        &self,
        row: Recorded,
        engine: &str,
        transcripts: &[macalla_core::stt::Transcript],
        document: &macalla_core::block::Document,
    ) -> bool {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return false;
        };
        let kept = match store.get(row.id) {
            Ok(Some(entry)) if entry.ts_ms == row.ts_ms => entry,
            Ok(_) => {
                tracing::debug!(id = row.id, "the row to replace has gone");
                return false;
            }
            Err(e) => {
                tracing::error!(error = %e, "could not read a dictation's row");
                return false;
            }
        };
        let entry = history::NewEntry {
            ts_ms: kept.ts_ms,
            app_id: kept.app_id,
            engine: engine.to_string(),
            raw: macalla_core::stt::final_text(transcripts),
            enhanced: document.to_plain(),
            document: document.clone(),
            schema_version: document.schema_version,
            latency: kept.latency,
            audio_path: kept.audio_path,
        };
        match replace_in(store, row, &entry) {
            Ok(replaced) => replaced,
            Err(e) => {
                tracing::error!(error = %e, "could not replace a dictation in history");
                false
            }
        }
    }

    /// Remove the row a dictation was kept in, matched on its time as well as
    /// its id, as [`HistoryHandle::replace_document`] is: a dictation thrown
    /// away after its row was written is never kept, and a row that has since
    /// taken the id is not this one. Errors are logged.
    pub fn forget(&self, row: Recorded) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return;
        };
        let deleted = match store.get(row.id) {
            Ok(Some(entry)) if entry.ts_ms == row.ts_ms => store.delete(row.id).map(|()| true),
            Ok(_) => Ok(false),
            Err(e) => Err(e),
        };
        match deleted {
            Ok(true) => tracing::debug!(id = row.id, "forgot a discarded dictation's row"),
            Ok(false) => tracing::debug!(id = row.id, "the row to forget has already gone"),
            Err(e) => tracing::error!(error = %e, "could not remove a discarded dictation's row"),
        }
    }

    /// Whether the row a dictation was kept in is still there, matched on its
    /// time as well as its id. False with History off, and on an error, which
    /// is logged.
    pub fn holds(&self, row: Recorded) -> bool {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_ref() else {
            return false;
        };
        match store.get(row.id) {
            Ok(entry) => entry.is_some_and(|entry| entry.ts_ms == row.ts_ms),
            Err(e) => {
                tracing::error!(error = %e, "could not read a dictation's row");
                false
            }
        }
    }

    /// Apply the retention rule now. Errors are logged: a purge that failed
    /// is retried at the next moment, and there is always a next moment.
    pub fn purge(&self, moment: Moment) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return;
        };
        match history::purge(store, moment, now_ms()) {
            Ok(0) => {}
            Ok(n) => tracing::info!(removed = n, ?moment, "history purged"),
            Err(e) => tracing::error!(error = %e, ?moment, "could not purge history"),
        }
    }

    /// The engine the person chose, by id, or `None` for the platform's
    /// default. Kept beside the retention rule: the history file is the
    /// one settings store there is.
    pub fn engine_choice(&self) -> Option<String> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let store = guard.as_ref()?;
        store.setting(ENGINE_SETTING).ok().flatten()
    }

    /// Dictate on local engine `id` from now on, clearing any stored cloud
    /// choice in the same lock -- the write [`transcription_choose`]'s local
    /// arm needs, so a `cloud_choice()` read between "the engine changed"
    /// and "the cloud choice is cleared" can never land in between and see
    /// both at once.
    ///
    /// [`transcription_choose`]: crate::commands::transcription_choose
    pub fn choose_local(&self, id: &str) -> Result<(), CommandError> {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return Err(HISTORY_OFF_CHOICE.into());
        };
        store.set_setting(ENGINE_SETTING, id)?;
        store.set_setting(CLOUD_PROVIDER_SETTING, "")?;
        Ok(store.set_setting(CLOUD_MODEL_SETTING, "")?)
    }

    /// The microphone the person chose, by name, or `None` for the system
    /// default. Kept beside the engine choice, for the same reason.
    pub fn device_choice(&self) -> Option<String> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let store = guard.as_ref()?;
        store
            .setting(DEVICE_SETTING)
            .ok()
            .flatten()
            .filter(|name| !name.is_empty())
    }

    pub fn set_device_choice(&self, name: Option<&str>) -> Result<(), CommandError> {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return Err(HISTORY_OFF_CHOICE.into());
        };
        Ok(store.set_setting(DEVICE_SETTING, name.unwrap_or(""))?)
    }

    /// The cloud transcription provider chosen, and its model where one
    /// applies, or `None` when dictations run on the local engine alone.
    pub fn cloud_choice(&self) -> Option<CloudChoice> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let store = guard.as_ref()?;
        let provider = store.setting(CLOUD_PROVIDER_SETTING).ok().flatten()?;
        if provider.is_empty() {
            return None;
        }
        let model = store
            .setting(CLOUD_MODEL_SETTING)
            .ok()
            .flatten()
            .filter(|m| !m.is_empty());
        Some(CloudChoice { provider, model })
    }

    /// Dictate through `provider` from now on, with `model` where the
    /// provider needs one (OpenRouter).
    pub fn set_cloud_choice(
        &self,
        provider: &str,
        model: Option<&str>,
    ) -> Result<(), CommandError> {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return Err(HISTORY_OFF_CHOICE.into());
        };
        store.set_setting(CLOUD_PROVIDER_SETTING, provider)?;
        Ok(store.set_setting(CLOUD_MODEL_SETTING, model.unwrap_or(""))?)
    }

    /// The cleanup choice in force: `Off` before anyone has chosen, or when
    /// history is off (there is nowhere to have kept a different answer).
    pub fn cleanup_choice(&self) -> CleanupChoice {
        // A stored choice that cannot be read (from a later shape, or
        // damaged) cleans nothing rather than guessing at a pool.
        self.cleanup_choice_as_stored()
            .unwrap_or(CleanupChoice::Off)
    }

    /// [`HistoryHandle::cleanup_choice`], but `None` when what is stored is
    /// not a choice at all -- a pool that cannot be parsed, or a kind this
    /// build does not know -- so a caller comparing a new choice with the
    /// stored one never takes a damaged row for `Off` and skips rewriting it.
    pub fn cleanup_choice_as_stored(&self) -> Option<CleanupChoice> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_ref() else {
            return Some(CleanupChoice::Off);
        };
        match store
            .setting(CLEANUP_KIND_SETTING)
            .ok()
            .flatten()
            .as_deref()
        {
            None | Some("") => Some(CleanupChoice::Off),
            Some("apple-intelligence") => Some(CleanupChoice::AppleIntelligence),
            Some("server") => Some(CleanupChoice::Server {
                address: store
                    .setting(CLEANUP_ADDRESS_SETTING)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
                model: store
                    .setting(CLEANUP_MODEL_SETTING)
                    .ok()
                    .flatten()
                    .unwrap_or_default(),
            }),
            Some("pool") => store
                .setting(CLEANUP_POOL_SETTING)
                .ok()
                .flatten()
                .and_then(|json| serde_json::from_str::<Vec<PoolEntry>>(&json).ok())
                .map(|models| CleanupChoice::Pool { models }),
            Some(_) => None,
        }
    }

    /// Store `kind` and `pool` as they are, unchecked, for tests of a
    /// damaged stored choice.
    #[cfg(test)]
    pub(crate) fn set_raw_cleanup_for_test(&self, kind: &str, pool: &str) {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let store = guard.as_mut().expect("an open store");
        store.set_setting(CLEANUP_KIND_SETTING, kind).unwrap();
        store.set_setting(CLEANUP_POOL_SETTING, pool).unwrap();
    }

    /// A handle whose store never opened, as when History fails at start.
    #[cfg(test)]
    pub(crate) fn unopened_for_test() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Tidy with `choice` from now on. One lock, four settings written
    /// together, so a read landing mid-write can never see a `"server"` or
    /// `"pool"` kind with the previous choice's address, model or pool still
    /// in place.
    pub fn set_cleanup_choice(&self, choice: &CleanupChoice) -> Result<(), CommandError> {
        let pool = match choice {
            CleanupChoice::Pool { models } => {
                serde_json::to_string(models).or_say("That Enhancement pool could not be saved.")?
            }
            _ => String::new(),
        };
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return Err(HISTORY_OFF_CHOICE.into());
        };
        let (kind, address, model) = match choice {
            CleanupChoice::Off => ("", "", ""),
            CleanupChoice::AppleIntelligence => ("apple-intelligence", "", ""),
            CleanupChoice::Server { address, model } => {
                ("server", address.as_str(), model.as_str())
            }
            CleanupChoice::Pool { .. } => ("pool", "", ""),
        };
        store.set_setting(CLEANUP_KIND_SETTING, kind)?;
        store.set_setting(CLEANUP_ADDRESS_SETTING, address)?;
        store.set_setting(CLEANUP_MODEL_SETTING, model)?;
        Ok(store.set_setting(CLEANUP_POOL_SETTING, &pool)?)
    }

    /// The rule in force, as its stored form.
    pub fn retention(&self) -> Result<Retention, CommandError> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_ref() else {
            return Err(HISTORY_OFF.into());
        };
        Ok(history::retention_of(store)?)
    }

    /// Whether a dictation that finishes now would be kept: History is on,
    /// under any rule but "Not at all". False when the store could not be
    /// opened, or its rule not read, since nothing could be kept then either.
    pub fn keeps_dictations(&self) -> bool {
        self.retention()
            .is_ok_and(|rule| !matches!(rule, Retention::Off))
    }

    /// Change the rule and apply it at once, as after a dictation: a shorter
    /// rule takes effect immediately, and "until quit" keeps this session's
    /// rows until it quits.
    pub fn set_retention(&self, retention: Retention) -> Result<(), CommandError> {
        {
            let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let Some(store) = guard.as_mut() else {
                return Err(HISTORY_OFF_CHOICE.into());
            };
            store.set_setting(Retention::SETTING, &retention.to_string())?;
        }
        self.purge(Moment::AfterDictation);
        Ok(())
    }

    pub fn list(&self, limit: usize) -> Result<Vec<HistoryItem>, CommandError> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_ref() else {
            return Err(HISTORY_OFF.into());
        };
        let entries = store.list(limit)?;
        Ok(entries
            .into_iter()
            .map(|e| HistoryItem {
                id: e.id,
                ts_ms: e.ts_ms,
                text: e.enhanced,
                raw: e.raw,
                send_again: false,
            })
            .collect())
    }

    pub fn text_of(&self, id: i64) -> Result<Option<String>, CommandError> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_ref() else {
            return Err(HISTORY_OFF.into());
        };
        Ok(store.get(id)?.map(|e| e.enhanced))
    }

    pub fn delete(&self, id: i64) -> Result<(), CommandError> {
        let mut guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = guard.as_mut() else {
            return Err(HISTORY_OFF.into());
        };
        Ok(store.delete(id)?)
    }
}

/// [`SqliteHistory::replace_entry`] for `row`. A test can make it fail as an
/// SQLite error would ([`HistoryHandle::fail_replaces_for_test`]), which no
/// real input can, before anything is written.
fn replace_in(
    store: &mut SqliteHistory,
    row: Recorded,
    entry: &history::NewEntry,
) -> macalla_core::Result<bool> {
    #[cfg(test)]
    if REPLACE_FAILS.with(std::cell::Cell::get) {
        return Err(macalla_core::Error::History(
            "made to fail by a test".into(),
        ));
    }
    store.replace_entry(row.id, row.ts_ms, entry)
}

#[cfg(test)]
thread_local! {
    /// Set by [`HistoryHandle::fail_replaces_for_test`]. Per thread, so tests
    /// running alongside are not affected.
    static REPLACE_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
impl HistoryHandle {
    /// Make every replace on this thread fail as an SQLite error would, or
    /// stop doing so.
    pub(crate) fn fail_replaces_for_test(fail: bool) {
        REPLACE_FAILS.with(|fails| fails.set(fail));
    }
}

/// Offer Send again on the row whose recording the session holds, and on no
/// other. Matched on the time as well as the id, because a row that took a
/// deleted row's id is another dictation. `held` is
/// [`crate::dictation::Dictator::held_row`].
pub fn offer_send_again(items: &mut [HistoryItem], held: Option<Recorded>) {
    for item in items {
        item.send_again = held.is_some_and(|row| row.id == item.id && row.ts_ms == item.ts_ms);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::block::{Block, Document};
    use macalla_core::latency;
    use macalla_core::stt::Transcript;

    fn dictation(text: &str) -> Dictation {
        let t = Transcript {
            text: text.to_string(),
            is_final: true,
            start_ms: 0,
            end_ms: 500,
        };
        Dictation {
            document: Document::new(vec![Block::Paragraph {
                text: text.to_string(),
            }]),
            transcripts: vec![t],
            timing: latency::Dictation::default(),
            device: "Fake Microphone".into(),
            device_fault: None,
            engine_fault: None,
            stream_warnings: 0,
            heard_speech: None,
            cloud: None,
            local_document: None,
            front_app: None,
            late_cleanup: None,
            enhancement: None,
            discarded: false,
        }
    }

    fn handle() -> HistoryHandle {
        HistoryHandle::from_store(SqliteHistory::in_memory().unwrap())
    }

    #[test]
    fn a_recorded_dictation_is_listed_newest_first_with_its_text() {
        let h = handle();
        h.record(&dictation("first"), "fake");
        h.record(&dictation("second"), "fake");
        let items = h.list(10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text.trim(), "second");
        assert_eq!(items[0].raw, "second");
    }

    #[test]
    fn copy_reads_the_text_back_and_delete_removes_it() {
        let h = handle();
        h.record(&dictation("keep me"), "fake");
        let id = h.list(1).unwrap()[0].id;
        assert_eq!(h.text_of(id).unwrap().unwrap().trim(), "keep me");
        h.delete(id).unwrap();
        assert!(h.list(10).unwrap().is_empty());
        assert_eq!(h.text_of(id).unwrap(), None);
    }

    #[test]
    fn late_cleanup_replaces_the_row_it_was_recorded_as_and_no_other() {
        let h = handle();
        let row = h
            .record(&dictation("kubectl ran"), "fake")
            .expect("history keeps 30 days until someone chooses");
        let other = h.record(&dictation("another one"), "fake").unwrap();
        let cleaned = Document::new(vec![Block::Paragraph {
            text: "kubectl ran.".into(),
        }]);
        h.replace_document(row, &cleaned);
        assert_eq!(h.text_of(row.id).unwrap().as_deref(), Some("kubectl ran."));
        assert_eq!(h.text_of(other.id).unwrap().as_deref(), Some("another one"));

        // The same id recorded at another time is another dictation.
        h.delete(other.id).unwrap();
        let reused = h.record(&dictation("a third one"), "fake").unwrap();
        assert_eq!(reused.id, other.id, "the id was given out again");
        h.replace_document(
            Recorded {
                ts_ms: reused.ts_ms.wrapping_sub(1),
                ..reused
            },
            &cleaned,
        );
        assert_eq!(
            h.text_of(reused.id).unwrap().as_deref(),
            Some("a third one")
        );

        h.set_retention(Retention::Off).unwrap();
        assert_eq!(
            h.record(&dictation("not kept"), "fake"),
            None,
            "no row, so nothing for late cleanup to replace"
        );
    }

    #[test]
    fn send_again_is_offered_on_the_held_row_and_no_other() {
        let h = handle();
        let failed = h.record(&dictation("what this Mac heard"), "fake").unwrap();
        h.record(&dictation("a later one"), "fake").unwrap();
        let offered = |held: Option<Recorded>| {
            let mut items = h.list(10).unwrap();
            offer_send_again(&mut items, held);
            items
                .into_iter()
                .filter(|item| item.send_again)
                .map(|item| item.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(offered(Some(failed)), [failed.id]);
        assert!(offered(None).is_empty(), "nothing held, nothing offered");
        assert!(
            offered(Some(Recorded {
                ts_ms: failed.ts_ms.wrapping_sub(1),
                ..failed
            }))
            .is_empty(),
            "the same id at another time is another dictation"
        );
        assert!(
            h.list(10).unwrap().iter().all(|item| !item.send_again),
            "never offered by listing alone"
        );
    }

    #[test]
    fn forgetting_a_row_removes_that_dictation_and_never_the_one_that_took_its_id() {
        let h = handle();
        let first = h.record(&dictation("thrown away"), "fake").unwrap();
        h.forget(first);
        assert!(h.list(10).unwrap().is_empty());
        h.forget(first); // already gone: nothing to do, and no error

        let kept = h.record(&dictation("kept"), "fake").unwrap();
        assert_eq!(kept.id, first.id, "the id was given out again");
        h.forget(Recorded {
            ts_ms: kept.ts_ms.wrapping_sub(1),
            ..kept
        });
        assert_eq!(h.text_of(kept.id).unwrap().as_deref(), Some("kept"));
    }

    #[test]
    fn a_chosen_microphone_is_kept_and_going_back_to_the_default_forgets_it() {
        let h = handle();
        assert_eq!(h.device_choice(), None, "the default until someone chooses");
        h.set_device_choice(Some("USB Microphone")).unwrap();
        assert_eq!(h.device_choice().as_deref(), Some("USB Microphone"));
        h.set_device_choice(None).unwrap();
        assert_eq!(h.device_choice(), None);
    }

    #[test]
    fn a_cloud_choice_is_kept_and_choosing_local_again_clears_it() {
        let h = handle();
        assert_eq!(
            h.cloud_choice(),
            None,
            "local until someone chooses a provider"
        );
        h.set_cloud_choice("elevenlabs", None).unwrap();
        assert_eq!(
            h.cloud_choice(),
            Some(CloudChoice {
                provider: "elevenlabs".to_string(),
                model: None,
            })
        );
        h.choose_local("apple-speech").unwrap();
        assert_eq!(h.cloud_choice(), None);
    }

    #[test]
    fn a_cloud_choice_with_a_model_keeps_the_model_too() {
        let h = handle();
        h.set_cloud_choice("openrouter", Some("openai/gpt-transcribe"))
            .unwrap();
        assert_eq!(
            h.cloud_choice(),
            Some(CloudChoice {
                provider: "openrouter".to_string(),
                model: Some("openai/gpt-transcribe".to_string()),
            })
        );
    }

    #[test]
    fn a_cleanup_choice_is_off_until_someone_chooses() {
        let h = handle();
        assert_eq!(h.cleanup_choice(), CleanupChoice::Off);
    }

    #[test]
    fn a_cleanup_choice_round_trips_and_choosing_off_again_clears_it() {
        let h = handle();
        h.set_cleanup_choice(&CleanupChoice::AppleIntelligence)
            .unwrap();
        assert_eq!(h.cleanup_choice(), CleanupChoice::AppleIntelligence);

        h.set_cleanup_choice(&CleanupChoice::Server {
            address: "http://localhost:11434".to_string(),
            model: "granite4.1:3b".to_string(),
        })
        .unwrap();
        assert_eq!(
            h.cleanup_choice(),
            CleanupChoice::Server {
                address: "http://localhost:11434".to_string(),
                model: "granite4.1:3b".to_string(),
            }
        );

        h.set_cleanup_choice(&CleanupChoice::Off).unwrap();
        assert_eq!(h.cleanup_choice(), CleanupChoice::Off);
    }

    #[test]
    fn a_pool_choice_round_trips_in_ticked_order_and_another_choice_clears_it() {
        let entry = |provider: &str, model: &str| PoolEntry {
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let h = handle();
        let pool = CleanupChoice::Pool {
            models: vec![
                entry("mistral", "mistral-test"),
                entry("openai", "gpt-test"),
            ],
        };
        h.set_cleanup_choice(&pool).unwrap();
        assert_eq!(h.cleanup_choice(), pool);

        h.set_cleanup_choice(&CleanupChoice::Server {
            address: "http://localhost:11434".to_string(),
            model: "granite4.1:3b".to_string(),
        })
        .unwrap();
        let stored_pool =
            h.0.lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .setting(CLEANUP_POOL_SETTING)
                .unwrap()
                .unwrap_or_default();
        assert_eq!(stored_pool, "", "choosing a server clears the pool");

        h.set_cleanup_choice(&CleanupChoice::Pool {
            models: vec![entry("anthropic", "claude-test")],
        })
        .unwrap();
        assert_eq!(
            h.cleanup_choice(),
            CleanupChoice::Pool {
                models: vec![entry("anthropic", "claude-test")],
            }
        );
    }

    #[test]
    fn a_stored_pool_that_cannot_be_read_cleans_nothing() {
        let h = handle();
        {
            let mut guard = h.0.lock().unwrap();
            let store = guard.as_mut().unwrap();
            store.set_setting(CLEANUP_KIND_SETTING, "pool").unwrap();
            store.set_setting(CLEANUP_POOL_SETTING, "not json").unwrap();
        }
        assert_eq!(h.cleanup_choice(), CleanupChoice::Off);
    }

    #[test]
    fn a_cleanup_choice_survives_reopening_the_database() {
        let dir = std::env::temp_dir().join(format!(
            "macalla-desktop-history-cleanup-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db_path = dir.join("history.sqlite");

        {
            let h = HistoryHandle::from_store(SqliteHistory::open(&db_path).unwrap());
            h.set_cleanup_choice(&CleanupChoice::Server {
                address: "http://localhost:11434".to_string(),
                model: "granite4.1:3b".to_string(),
            })
            .unwrap();
        }

        {
            let h = HistoryHandle::from_store(SqliteHistory::open(&db_path).unwrap());
            assert_eq!(
                h.cleanup_choice(),
                CleanupChoice::Server {
                    address: "http://localhost:11434".to_string(),
                    model: "granite4.1:3b".to_string(),
                }
            );
        }

        std::fs::remove_dir_all(&dir).expect("clean up the temp dir");
    }

    #[test]
    fn with_no_store_recording_is_silent_and_listing_says_why() {
        // Dictation must not fail because history could not open.
        let h = HistoryHandle(Arc::new(Mutex::new(None)));
        h.record(&dictation("lost"), "fake");
        let err = h.list(10).unwrap_err();
        assert_eq!(err.to_string(), HISTORY_OFF);
    }
}
