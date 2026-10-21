// SPDX-License-Identifier: MIT
//! Tauri commands: the only bridge between the UI and the Rust core.
//! These are plain response models, not the core types, so a
//! core refactor cannot silently change the wire format the UI depends on.

use crate::command_error::{CommandError, OrSay};
use macalla_core::onboarding::StepState;
use macalla_engines::registry::{self, Availability};
use macalla_platform::secrets::SecretStore;
use macalla_session::providers::{self, Kind};
use serde::{Deserialize, Serialize};

/// What the About screen shows: the version, the licence, and who to thank
/// for each engine this build knows about.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct About {
    pub version: String,
    pub licence: String,
    pub attributions: Vec<Attribution>,
    /// The website and its guide (`links.rs`).
    pub links: Vec<crate::links::Link>,
}

/// One line of credit. Parakeet is CC-BY-4.0, so once it ships this line is
/// a licence obligation and not a courtesy (MODELS.md).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Attribution {
    pub name: String,
    pub licence: String,
    pub notice: String,
    pub status: String,
}

/// One row the Microphone card's `<select>` can offer, and the Microphone
/// submenu's own row before the tray adds its checkmark and enabled state
/// (`tray.rs`'s `microphone_menu_items`): `value` for the `<option>` or, in
/// the tray, the device name a click reports back (`""` for the system
/// default row, matching the `<select>`'s own empty-value convention); label
/// built once, in [`crate::microphone_words`], so the two surfaces never say
/// it differently.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MicrophoneRow {
    pub value: String,
    pub label: String,
}

/// What the Microphone card offers.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MicrophoneView {
    /// Every input device connected now, as the OS names it.
    pub names: Vec<String>,
    /// The device chosen by name, or `None` to use the system default.
    pub chosen: Option<String>,
    /// The system default first, then the saved choice's row if it is no
    /// longer connected, then every connected device -- see
    /// [`microphone_rows`].
    pub rows: Vec<MicrophoneRow>,
}

/// What the onboarding checklist shows for one step.
///
/// The three flags are this platform's answer, from `macalla-platform`, so
/// the page never decides which OS it is on: it groups a step under the
/// permissions, offers Ask, and offers the settings pane only where they say.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StepSummary {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub state: String,
    pub action: Option<String>,
    /// A permission this platform asks for, rather than the rest of the list.
    pub permission: bool,
    /// The permission has an ask the page can offer
    /// ([`request_permission`]).
    pub ask: bool,
    /// The permission has a settings pane the page can open
    /// ([`open_settings_pane`]).
    pub settings: bool,
}

/// `ready`, `not included in this build`, or the reason it cannot run,
/// verbatim -- the Engine card shows it directly rather than re-deriving
/// copy.
fn status_for(availability: &Availability) -> String {
    match availability {
        Availability::Ready => "ready".to_string(),
        Availability::NotCompiledIn => "not included in this build".to_string(),
        Availability::NeedsDownload(what) => format!("needs a download: {what}"),
        Availability::Unavailable(reason) => reason.clone(),
    }
}

fn state_for(state: StepState) -> &'static str {
    match state {
        StepState::Done => "done",
        StepState::Required => "required",
        StepState::Optional => "optional",
    }
}

/// The licence and the notice for an engine, by id. MODELS.md is the record.
///
/// A downloadable model's notice is read from its catalogue entry rather
/// than repeated here. There used to be two copies of the Parakeet line, and
/// a licence notice that exists twice is one that will eventually say two
/// different things.
fn attribution_for(id: &str) -> Option<(&'static str, &'static str)> {
    if id == "apple-speech" {
        return Some((
            "Apple system component",
            "Speech recognition by Apple's Speech framework, part of macOS.",
        ));
    }
    macalla_engines::models::catalogue()
        .iter()
        .find(|m| m.engine == id && !m.attribution.is_empty())
        .map(|m| (m.licence, m.attribution))
}

fn about_summary() -> About {
    let attributions = registry::engines()
        .into_iter()
        .filter_map(|e| {
            let (licence, notice) = attribution_for(e.id)?;
            Some(Attribution {
                name: e.name.to_string(),
                licence: licence.to_string(),
                notice: notice.to_string(),
                status: status_for(&e.availability),
            })
        })
        .collect();
    About {
        version: env!("CARGO_PKG_VERSION").to_string(),
        licence: "MIT".to_string(),
        attributions,
        links: crate::links::all(),
    }
}

/// Find engine `id` and refuse it unless it is ready to dictate on: a chosen
/// engine with no model would leave the app mute. Used by
/// [`transcription_choose`]'s local branch.
fn ready_local_engine(id: &str) -> Result<registry::EngineInfo, CommandError> {
    let info = registry::find(id)
        .ok_or_else(|| CommandError::from("That is not an engine Macalla knows about."))?;
    if !info.availability.is_ready() {
        return Err(format!(
            "{} is not ready: {}",
            info.name,
            status_for(&info.availability)
        )
        .into());
    }
    Ok(info)
}

/// What the Models page's Transcription card asks for: dictate on a local
/// engine (`id` is a registry engine id), or send the recording to a
/// provider (`id` is a [`providers::Provider::id`], `model` is its own model
/// id where one is needed -- OpenRouter).
#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptionChoice {
    /// `"local"` or `"provider"`.
    pub source: String,
    pub id: String,
    pub model: Option<String>,
}

/// The plain-function core of [`transcription_choose`], taking the key store
/// explicitly so a test never touches the real keychain.
fn transcription_choose_with(
    history: &crate::history::HistoryHandle,
    keys: &dyn SecretStore,
    choice: &TranscriptionChoice,
) -> Result<(), CommandError> {
    match choice.source.as_str() {
        "local" => {
            ready_local_engine(&choice.id)?;
            // One lock, one write of both settings: a `cloud_choice()` read
            // landing between "the engine changed" and "the cloud choice is
            // cleared" must never see both at once.
            history.choose_local(&choice.id)
        }
        "provider" => {
            if !macalla_session::choice::cloud_transcription_compiled_in() {
                return Err("This copy of Macalla was built without cloud transcription.".into());
            }
            let provider = providers::find(&choice.id)
                .ok_or_else(|| CommandError::from(format!("no provider called {:?}", choice.id)))?;
            if !provider.offers(Kind::Transcription) {
                return Err(format!("{} does not transcribe.", provider.name).into());
            }
            match providers::key_state(keys, provider.id)? {
                providers::KeyState::Saved => {}
                providers::KeyState::Missing => {
                    return Err(
                        format!("Add a key for {} under Providers first.", provider.name).into(),
                    );
                }
                providers::KeyState::StoreUnavailable(reason) => {
                    return Err(format!(
                        "Macalla can't read the Keychain right now, so {} can't be used. ({reason})",
                        provider.name
                    )
                    .into());
                }
                providers::KeyState::Unreadable(reason) => {
                    return Err(
                        format!("{}'s key could not be read. ({reason})", provider.name).into(),
                    );
                }
            }
            if provider.id == "openrouter" && choice.model.as_deref().unwrap_or("").is_empty() {
                return Err("Choose an OpenRouter model first.".into());
            }
            history.set_cloud_choice(&choice.id, choice.model.as_deref())
        }
        other => {
            tracing::warn!(source = other, "not a transcription source");
            Err("Macalla does not recognise that way to transcribe.".into())
        }
    }
}

/// Choose what every dictation transcribes on from now on: a local engine,
/// or a provider's whole-recording request at key release. Async:
/// checking a provider's key state can block on the macOS keychain access
/// prompt, like [`provider_keys`].
#[tauri::command(async)]
pub fn transcription_choose(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    choice: TranscriptionChoice,
) -> Result<(), CommandError> {
    transcription_choose_with(
        &history,
        &macalla_platform::secrets::SystemSecrets::new(),
        &choice,
    )?;
    dictator.send(crate::dictation::Command::SwitchEngine);
    Ok(())
}

/// The engine step. On a Mac the default is Apple Speech, part of macOS, so
/// there is nothing to choose or download and the step is a tick. Elsewhere
/// the registry says what is missing.
/// `chosen` is the engine the person picked, by id, or `None` for the
/// platform's default: the same rule dictation opens its engine by, so the
/// checklist names the engine dictation will use and asks whether that one is
/// ready, not the default.
fn engine_step(chosen: Option<&str>) -> macalla_core::onboarding::Step {
    use macalla_core::onboarding::Step;
    let id = chosen.unwrap_or_else(|| registry::default_engine_id());
    match registry::find(id) {
        Some(info) if info.availability.is_ready() => {
            // "Built in" is only true of the macOS default (Apple Speech, part
            // of the OS). Off macOS a ready engine was downloaded, so it is
            // "ready", not "built in".
            let detail = match macalla_platform::Os::HERE {
                macalla_platform::Os::MacOs => {
                    format!("{} is built in and ready.", info.name)
                }
                _ => format!("{} is ready.", info.name),
            };
            Step::done("engine", "Engine", detail)
        }
        Some(info) => Step::required(
            "engine",
            "Engine",
            format!("{} turns your voice into text.", info.name),
            status_for(&info.availability),
        ),
        None => Step::required(
            "engine",
            "Engine",
            "Turns your voice into text.",
            "This build has no engine for this platform.",
        ),
    }
}

/// The first-launch checklist: the permissions, then the
/// engine. The same steps as `macalla setup`, plus the engine line.
pub fn all_steps(chosen: Option<&str>) -> Vec<macalla_core::onboarding::Step> {
    let mut steps = macalla_platform::onboarding::permission_steps();
    steps.push(engine_step(chosen));
    steps
}

/// Whether nothing required is left, so the window can stay out of the way.
pub fn setup_is_complete(chosen: Option<&str>) -> bool {
    ready(all_steps(chosen))
}

/// Whether `steps` leave nothing required. An optional step is never "done",
/// and it does not hold setup up ([`macalla_core::onboarding::Checklist::is_ready`]),
/// so waiting for every step to be done kept the checklist coming back.
/// The Setup card's Continue follows the same rule.
fn ready(steps: Vec<macalla_core::onboarding::Step>) -> bool {
    macalla_core::onboarding::Checklist::new(steps).is_ready()
}

/// The onboarding checklist: what still stands between the user and dictating.
fn step_summaries(chosen: Option<&str>) -> Vec<StepSummary> {
    // In the log, so a checklist that disagrees with what the person did in
    // System Settings can be read back afterwards.
    let grants: Vec<(macalla_platform::Permission, macalla_platform::Grant)> =
        macalla_platform::Permission::required_here()
            .iter()
            .map(|p| (*p, p.granted()))
            .collect();
    if grants
        .iter()
        .any(|(_, g)| *g != macalla_platform::Grant::Granted)
    {
        tracing::info!(?grants, "checklist read");
    }
    all_steps(chosen)
        .into_iter()
        .map(|s| summary_of(s, macalla_platform::Os::HERE))
        .collect()
}

/// One step as the page gets it, with what `os` says about its permission,
/// if it is one.
fn summary_of(step: macalla_core::onboarding::Step, os: macalla_platform::Os) -> StepSummary {
    use macalla_platform::Permission;
    let permission =
        Permission::from_step_id(step.id).filter(|p| Permission::required_on(os).contains(p));
    StepSummary {
        id: step.id.to_string(),
        title: step.title,
        detail: step.detail,
        state: state_for(step.state).to_string(),
        action: step.action,
        permission: permission.is_some(),
        ask: permission.is_some_and(|p| p.can_ask_on(os)),
        settings: permission.is_some_and(|p| p.settings_url_on(os).is_some()),
    }
}

#[tauri::command]
pub fn setup_steps(history: tauri::State<'_, crate::history::HistoryHandle>) -> Vec<StepSummary> {
    step_summaries(history.engine_choice().as_deref())
}

/// Open the System Settings pane for a step, where the grant is made.
#[tauri::command]
pub fn open_settings_pane(step: String) -> Result<(), CommandError> {
    let permission = macalla_platform::Permission::from_step_id(&step)
        .ok_or_else(|| format!("{step} has no settings pane"))?;
    let url = permission
        .settings_url()
        .ok_or_else(|| CommandError::from("This platform has no settings pane to open."))?;
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg(url)
            .status()
            .or_say("Could not open System Settings.")?;
        if !status.success() {
            tracing::warn!(status = %status, "open exited without success");
            return Err("Could not open System Settings.".into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        Err("This platform has no settings pane to open.".into())
    }
}

/// Ask macOS to prompt for a step's permission, where a prompt exists.
///
/// Accessibility has a real ask. The microphone's prompt comes from the first
/// capture, so asking opens the microphone for a moment. Input Monitoring is
/// asked when the tap is created at launch, so there is nothing to do but say
/// so; if it was refused, only the pane can fix it.
#[tauri::command]
pub fn request_permission(step: String) -> Result<String, CommandError> {
    use macalla_platform::{Grant, Permission};
    let permission =
        Permission::from_step_id(&step).ok_or_else(|| format!("{step} cannot be asked for"))?;
    match permission {
        Permission::InputMonitoring => Ok("asked at launch".to_string()),
        // The ask itself is the permission's own: a prompt for Accessibility,
        // a moment of capture for the microphone.
        _ => Ok(match permission.request() {
            Grant::Granted => "granted".to_string(),
            _ => "asked".to_string(),
        }),
    }
}

/// Setup is done: put the window away until it is wanted.
#[tauri::command]
pub fn finish_setup(app: tauri::AppHandle) -> Result<(), CommandError> {
    use tauri::Manager;
    match app.get_webview_window("main") {
        Some(window) => window.hide().or_say("Could not hide the window."),
        None => Ok(()),
    }
}

/// The overlay's ×: throw the running dictation away, as Esc twice does.
/// Does nothing if none is running (`Dictator::cancel`). Only the overlay may
/// call it (`capabilities/overlay.json`).
#[tauri::command]
pub fn dictation_cancel(dictator: tauri::State<'_, crate::dictation::Dictator>) {
    dictator.cancel();
}

/// The overlay's ✓: stop the running dictation and paste, as the tray's Stop
/// Dictation does. Does nothing if none is running or its recording has
/// already ended (`Dictator::finish`), so it never starts one. Only the
/// overlay may call it (`capabilities/overlay.json`).
#[tauri::command]
pub fn dictation_finish(dictator: tauri::State<'_, crate::dictation::Dictator>) {
    dictator.finish();
}

/// The most recent dictations, newest first. The row whose recording is held
/// after a failed cloud request is marked, so the page offers Send again on
/// that row and on no other.
#[tauri::command]
pub fn history_list(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    limit: usize,
) -> Result<Vec<crate::history::HistoryItem>, CommandError> {
    let mut items = history.list(limit)?;
    crate::history::offer_send_again(&mut items, dictator.held_row());
    Ok(items)
}

/// Send the held recording of a failed cloud dictation to the provider again.
/// The row is named by its id and its time, as History lists it, so a row
/// that took a deleted row's id is never sent for. The session lives on the
/// dictation thread, so the request goes there and the answer comes back as
/// `history-sent-again`.
#[tauri::command]
pub fn history_send_again(
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    id: i64,
    ts_ms: u64,
) {
    dictator.send(crate::dictation::Command::SendAgain {
        row: crate::history::Recorded { id, ts_ms },
    });
}

/// Put a past dictation's text on the clipboard.
#[tauri::command]
pub fn history_copy(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    id: i64,
) -> Result<(), CommandError> {
    use macalla_platform::paste::{Clipboard, SystemClipboard};
    let Some(text) = history.text_of(id)? else {
        return Err("that dictation is no longer in history".into());
    };
    Ok(SystemClipboard::new().and_then(|mut c| c.write_text(&text))?)
}

/// Forget a past dictation. When its recording is the one held after a failed
/// cloud request, that goes too: there is nowhere left to send it again from.
#[tauri::command]
pub fn history_delete(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    id: i64,
) -> Result<(), CommandError> {
    history.delete(id)?;
    if let Some(row) = crate::dictation::held_row_gone(&history, dictator.held_row()) {
        dictator.send(crate::dictation::Command::LetGo { row });
    }
    Ok(())
}

/// One retention choice as the History card lists it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RetentionChoice {
    pub value: String,
    pub label: String,
}

/// The retention choices, and which is in force.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RetentionView {
    pub current: String,
    pub choices: Vec<RetentionChoice>,
}

/// How long history is kept, and the choices.
#[tauri::command]
pub fn history_retention(
    history: tauri::State<'_, crate::history::HistoryHandle>,
) -> Result<RetentionView, CommandError> {
    use macalla_core::history::Retention;
    Ok(RetentionView {
        current: history.retention()?.to_string(),
        choices: Retention::OPTIONS
            .iter()
            .map(|r| RetentionChoice {
                value: r.to_string(),
                label: r.label(),
            })
            .collect(),
    })
}

/// Change how long history is kept. Takes effect at once. A rule that erased
/// the row whose recording is held after a failed cloud request -- "Not at
/// all" erases every row -- lets that recording go too: History no longer
/// keeps the dictation it would be sent again for.
#[tauri::command]
pub fn history_set_retention(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    value: String,
) -> Result<(), CommandError> {
    use macalla_core::history::Retention;
    let retention = Retention::parse(&value)
        .ok_or_else(|| format!("{value:?} is not a retention; 30 days is the most"))?;
    history.set_retention(retention)?;
    if let Some(row) = crate::dictation::held_row_gone(&history, dictator.held_row()) {
        dictator.send(crate::dictation::Command::LetGo { row });
    }
    Ok(())
}

/// Where the dictionary is and how many terms it has.
#[tauri::command]
pub fn dictionary_info(
    app: tauri::AppHandle,
) -> Result<crate::dictionary::DictionaryInfo, CommandError> {
    let path = dictionary_path(&app)?;
    Ok(crate::dictionary::info(&path))
}

pub(crate) fn dictionary_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, CommandError> {
    crate::dictionary::ensure(app)
        .or_say("Could not open the dictionary file. Check it on the Dictionary card.")
}

/// Tell the dictation thread the file changed, so the next dictation uses it.
fn dictionary_changed(dictator: &tauri::State<'_, crate::dictation::Dictator>) {
    dictator.send(crate::dictation::Command::ReloadDictionary);
}

/// Every term, for the Dictionary view.
#[tauri::command]
pub fn dictionary_terms(
    app: tauri::AppHandle,
) -> Result<Vec<crate::dictionary::TermView>, CommandError> {
    crate::dictionary::terms(&dictionary_path(&app)?)
}

/// Add spoken forms to a written form, creating the term if it is new.
/// Returns whether a term was created.
#[tauri::command]
pub fn dictionary_add(
    app: tauri::AppHandle,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    written: String,
    spoken: Vec<String>,
) -> Result<bool, CommandError> {
    let created = crate::dictionary::add(&dictionary_path(&app)?, &written, &spoken)?;
    dictionary_changed(&dictator);
    Ok(created)
}

/// Replace one term, possibly renaming it.
#[tauri::command]
pub fn dictionary_update(
    app: tauri::AppHandle,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    previous: String,
    written: String,
    spoken: Vec<String>,
) -> Result<(), CommandError> {
    crate::dictionary::update(&dictionary_path(&app)?, &previous, &written, &spoken)?;
    dictionary_changed(&dictator);
    Ok(())
}

/// Forget a term.
#[tauri::command]
pub fn dictionary_remove(
    app: tauri::AppHandle,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    written: String,
) -> Result<(), CommandError> {
    crate::dictionary::remove(&dictionary_path(&app)?, &written)?;
    dictionary_changed(&dictator);
    Ok(())
}

/// Merge a shared JSON file in. Returns how many terms were added or changed.
#[tauri::command]
pub fn dictionary_import(
    app: tauri::AppHandle,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    json: String,
) -> Result<usize, CommandError> {
    let changed = crate::dictionary::import(&dictionary_path(&app)?, &json)?;
    dictionary_changed(&dictator);
    Ok(changed)
}

/// Write a copy to Downloads and return its path.
#[tauri::command]
pub fn dictionary_export(app: tauri::AppHandle) -> Result<String, CommandError> {
    let target = crate::dictionary::export(&app, &dictionary_path(&app)?)?;
    Ok(target.display().to_string())
}

/// The dictionary's own catalogue words: History's "Add to dictionary"
/// button and its two notices (`dictionary_words.rs`).
#[tauri::command]
pub fn dictionary_words() -> crate::dictionary::DictionaryWordsView {
    crate::dictionary::words()
}

/// Show a file in the file manager.
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), CommandError> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .status()
            .or_say("Could not show that file.")?;
        if !status.success() {
            tracing::warn!(status = %status, "open -R exited without success");
            return Err("Could not show that file.".into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Err("Showing a file in the file manager is not available on this platform yet.".into())
    }
}

/// Version, licence and model attributions for the About screen.
#[tauri::command]
pub fn about() -> About {
    about_summary()
}

/// One third-party crate under a licence: its own copyright line(s) and a link
/// to its source, so the notice says who as well as what. `copyright` is empty
/// only when the crate ships no licence file to harvest one from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CrateNotice {
    pub name: String,
    pub version: String,
    pub repository: String,
    pub copyright: Vec<String>,
}

/// The crates linked into the app under one SPDX licence family: the licence
/// terms once, then every crate that uses them. The About screen's
/// "Open-source software" page renders these.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LicenseFamily {
    pub spdx: String,
    pub name: String,
    pub text: String,
    pub crates: Vec<CrateNotice>,
}

#[derive(Deserialize)]
struct LicenseFile {
    families: Vec<LicenseFamily>,
}

/// Third-party crate licences, grouped by SPDX family. Generated from
/// `Cargo.lock` by `scripts/gen-licenses.py` into `licenses.json` and compiled
/// in here, so the notices ship with the binary and need no file at runtime.
/// Model weights are credited separately (see [`about`]); this is code only.
#[tauri::command]
pub fn licenses() -> Vec<LicenseFamily> {
    const RAW: &str = include_str!("../licenses.json");
    serde_json::from_str::<LicenseFile>(RAW)
        .map(|f| f.families)
        .unwrap_or_default()
}

/// The input device the next dictation listens through, as the OS names it.
/// `None` until the dictation thread has opened a microphone.
#[tauri::command]
pub fn input_device(dictator: tauri::State<'_, crate::dictation::Dictator>) -> Option<String> {
    dictator.device()
}

/// The system default row, then the saved choice's row if it is no longer
/// among `names`, then every connected device -- the order and the entries
/// the Microphone card's `<select>` has always shown (`App.tsx`'s
/// `MicrophoneCard`), and what the tray's Microphone submenu mirrors.
pub(crate) fn microphone_rows(
    names: &[String],
    chosen: Option<&str>,
    system_default: Option<&str>,
) -> Vec<MicrophoneRow> {
    let mut rows = Vec::with_capacity(names.len() + 2);
    rows.push(MicrophoneRow {
        value: String::new(),
        label: crate::microphone_words::system_default_label(system_default),
    });
    if let Some(name) = chosen.filter(|c| !names.iter().any(|n| n == c)) {
        rows.push(MicrophoneRow {
            value: name.to_string(),
            label: crate::microphone_words::gone_label(name),
        });
    }
    for name in names {
        rows.push(MicrophoneRow {
            value: name.clone(),
            label: name.clone(),
        });
    }
    rows
}

/// The microphones connected now, and which one is chosen. Listed afresh on
/// every call, because headsets and docks come and go. No ping of its own:
/// [`microphones`], the Tauri command, and `tray.rs`'s own refresher both
/// call this, and only the command pings the tray back (see `microphones`'s
/// doc comment) -- calling it here too would have the two ping each other
/// on every read.
pub(crate) fn microphone_view(
    history: &crate::history::HistoryHandle,
) -> Result<MicrophoneView, CommandError> {
    use macalla_platform::audio;
    let mut names = audio::input_device_names()?;
    // A device is chosen by name, so two with the same name are one choice,
    // and a nameless one could not be told from the system default's row.
    let mut seen = std::collections::HashSet::new();
    names.retain(|name| !name.is_empty() && seen.insert(name.clone()));
    let chosen = history.device_choice();
    let system_default = audio::default_input_device_name().ok();
    let rows = microphone_rows(&names, chosen.as_deref(), system_default.as_deref());
    Ok(MicrophoneView {
        names,
        chosen,
        rows,
    })
}

/// The Microphone card's own read: the settings page listing the
/// microphones is exactly the moment the tray's Microphone submenu is worth
/// refreshing too (its device list otherwise only moves on the idle poll or
/// an explicit choice), so this also pings it. [`microphone_view`] is the
/// shared, ping-free read the tray's own refresh calls -- see its doc
/// comment for why the ping lives only here.
#[tauri::command]
pub fn microphones(
    app: tauri::AppHandle,
    history: tauri::State<'_, crate::history::HistoryHandle>,
) -> Result<MicrophoneView, CommandError> {
    let view = microphone_view(&history)?;
    crate::tray::ping_microphone_menu(&app);
    Ok(view)
}

/// Listen through this microphone from now on, or through the system default
/// for `None`. Refused unless it is connected. Kept in the settings store and
/// taken up by the dictation thread, after any dictation in progress.
#[tauri::command]
pub fn microphone_choose(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    name: Option<String>,
) -> Result<(), CommandError> {
    if let Some(name) = name.as_deref() {
        let names = macalla_platform::audio::input_device_names()?;
        if !names.iter().any(|n| n == name) {
            return Err(format!("{name} is not connected.").into());
        }
    }
    history.set_device_choice(name.as_deref())?;
    dictator.send(crate::dictation::Command::SwitchDevice);
    Ok(())
}

/// One unload choice as the Engine card lists it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UnloadChoice {
    pub value: String,
    pub label: String,
}

/// How long an idle local engine stays loaded, and the choices.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UnloadView {
    pub current: String,
    pub choices: Vec<UnloadChoice>,
}

/// How long an idle local engine stays loaded before it is dropped to free its
/// memory, and the choices, for the Engine card.
#[tauri::command]
pub fn unload_choice(history: tauri::State<'_, crate::history::HistoryHandle>) -> UnloadView {
    UnloadView {
        current: history.unload_choice(),
        choices: crate::history::UNLOAD_OPTIONS
            .iter()
            .map(|(value, label)| UnloadChoice {
                value: (*value).to_string(),
                label: (*label).to_string(),
            })
            .collect(),
    }
}

/// Set how long an idle local engine stays loaded. The dictation loop reads the
/// new value on its next idle tick, so nothing needs reloading or restarting.
#[tauri::command]
pub fn unload_choose(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    value: String,
) -> Result<(), CommandError> {
    history.set_unload_choice(&value)
}

/// One provider, for the Models page's key list. Never carries the key
/// itself -- `saved` is all a settings screen needs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderKeyView {
    pub id: String,
    pub name: String,
    pub kinds: Vec<String>,
    pub saved: bool,
    /// Set when there IS an entry for this provider but reading it failed --
    /// the OS access prompt was denied, or the item does not decode. `saved`
    /// is `false` alongside it on purpose: a settings screen must not offer
    /// "add a key" over an entry that already exists, which would overwrite
    /// it, so this is shown instead of folding into `saved`.
    pub error: Option<String>,
}

/// Every provider this build knows about, and whether the store itself could
/// be reached at all.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderKeysView {
    pub providers: Vec<ProviderKeyView>,
    /// Set once, in place of every provider's own answer, when the key store
    /// itself could not be reached (no keychain on this platform yet, or the
    /// OS refused it) -- one reason shown, not one failed row per provider.
    pub store_unavailable: Option<String>,
}

fn kind_names(kinds: &[Kind]) -> Vec<String> {
    kinds
        .iter()
        .map(|k| match k {
            Kind::Text => "text",
            Kind::Transcription => "transcription",
        })
        .map(str::to_string)
        .collect()
}

/// Every provider from [`providers::PROVIDERS`], with whether a key is
/// stored for it. Takes the store as a trait object so a test can pass
/// [`macalla_platform::secrets::MemoryStore`] instead of the real keychain.
fn provider_keys_view(store: &dyn SecretStore) -> ProviderKeysView {
    let mut views = Vec::with_capacity(providers::PROVIDERS.len());
    for provider in providers::PROVIDERS {
        match providers::key_state(store, provider.id) {
            Ok(providers::KeyState::Saved) => views.push(ProviderKeyView {
                id: provider.id.to_string(),
                name: provider.name.to_string(),
                kinds: kind_names(provider.kinds),
                saved: true,
                error: None,
            }),
            Ok(providers::KeyState::Missing) => views.push(ProviderKeyView {
                id: provider.id.to_string(),
                name: provider.name.to_string(),
                kinds: kind_names(provider.kinds),
                saved: false,
                error: None,
            }),
            Ok(providers::KeyState::StoreUnavailable(reason)) => {
                return ProviderKeysView {
                    providers: Vec::new(),
                    // `report_platform`, not `platform`: this runs on every
                    // render of the Providers card. On Linux, which has no
                    // store at all yet, `SecretStoreUnavailable` logs at
                    // debug (`say::platform_level`), not warn, so that is not
                    // a render-storm; on macOS and Windows it does warn each
                    // time, because there it means the store is genuinely
                    // broken, which is worth knowing about repeatedly.
                    store_unavailable: Some(crate::say::report_platform(
                        &macalla_platform::Error::SecretStoreUnavailable(reason),
                    )),
                };
            }
            // There IS an entry for this provider; reading it failed. The
            // Display reason only -- `classify` in macalla-platform never
            // puts the secret in it, unlike Debug on some of the errors it
            // wraps.
            Ok(providers::KeyState::Unreadable(reason)) => views.push(ProviderKeyView {
                id: provider.id.to_string(),
                name: provider.name.to_string(),
                kinds: kind_names(provider.kinds),
                saved: false,
                error: Some(crate::say::report_platform(
                    &macalla_platform::Error::Secret {
                        account: provider.account.to_string(),
                        reason,
                    },
                )),
            }),
            // `provider.id` came from providers::PROVIDERS itself, so
            // `key_state` cannot fail with UnknownProvider here; this is
            // unreached in practice.
            Err(e) => views.push(ProviderKeyView {
                id: provider.id.to_string(),
                name: provider.name.to_string(),
                kinds: kind_names(provider.kinds),
                saved: false,
                error: Some(crate::say::report(&e)),
            }),
        }
    }
    ProviderKeysView {
        providers: views,
        store_unavailable: None,
    }
}

/// Every provider and whether a key is stored, for the Models page. Async so
/// this never blocks the main thread: a keychain read can block on a macOS
/// access prompt, and this reads eight items every time the page opens.
#[tauri::command(async)]
pub fn provider_keys() -> ProviderKeysView {
    provider_keys_view(&macalla_platform::secrets::SystemSecrets::new())
}

/// Store `key` for provider `id`, through [`providers::store_key`], which
/// trims it and refuses one that is empty or spans more than one line. The
/// error string is exactly what `store_key` reports -- never the key itself.
fn provider_key_save_with(
    store: &dyn SecretStore,
    id: &str,
    key: &str,
) -> Result<(), CommandError> {
    Ok(providers::store_key(store, id, key)?)
}

/// Whether the stored transcription choice reads provider `id`'s key. A
/// session built with it has the key in memory, so a key saved or removed
/// for `id` reaches it only when the session is opened again.
fn transcription_uses(history: &crate::history::HistoryHandle, id: &str) -> bool {
    history.cloud_choice().is_some_and(|c| c.provider == id)
}

/// Whether the stored cleanup pool reads provider `id`'s key. A server
/// choice is keyless and Apple Intelligence needs no key, so only a pool
/// ever does; built the same way, with the key in memory until Tier 1 is
/// built again.
fn pool_uses(history: &crate::history::HistoryHandle, id: &str) -> bool {
    matches!(
        history.cleanup_choice(),
        crate::history::CleanupChoice::Pool { models } if models.iter().any(|m| m.provider == id)
    )
}

/// Which command a save or remove for `id` should send, or `None` when
/// nothing dictation is doing right now reads that key. The transcription
/// choice always reopens the whole session ([`Command::SwitchEngine`]),
/// which rebuilds cleanup along the way; a pool that reads it and is not
/// also the transcription choice only needs its cleanup provider swapped
/// ([`Command::SwitchCleanup`]) -- `open_with_keys` never reloads the
/// speech engine for that.
fn key_command(
    history: &crate::history::HistoryHandle,
    id: &str,
) -> Option<crate::dictation::Command> {
    if transcription_uses(history, id) {
        Some(crate::dictation::Command::SwitchEngine)
    } else if pool_uses(history, id) {
        Some(crate::dictation::Command::SwitchCleanup)
    } else {
        None
    }
}

/// Whether the cleanup dictation is running now has nothing a rebuild could
/// repair ([`crate::dictation::OpenedStatus::cleanup_whole`]). `false` before
/// the first open, so a change made then is always applied.
fn cleanup_whole(dictator: &crate::dictation::Dictator) -> bool {
    dictator.status().is_some_and(|s| s.cleanup_whole())
}

/// Make `change` to provider `id`'s key in `store`, and return the command
/// dictation needs for it: [`key_command`]'s answer, except that a pool
/// alone is left as it is when the stored key is the same afterwards and
/// the running cleanup is `whole`. A pool holds each member's key in
/// memory, so a new value, a first key or a removed one all need a
/// rebuild; so does any save while a member failed or was left out, since
/// the key it could not read at build time may be readable now. Otherwise a
/// rebuild would only throw away what the providers have learned. The two
/// reads go through [`providers::read_key`] and are only compared, never
/// kept or logged; a read that fails counts as a change. The transcription
/// choice is untouched: it reopens on every save or remove.
fn key_change_command(
    history: &crate::history::HistoryHandle,
    store: &dyn SecretStore,
    id: &str,
    whole: bool,
    change: impl FnOnce() -> Result<(), CommandError>,
) -> Result<Option<crate::dictation::Command>, CommandError> {
    let command = key_command(history, id);
    if command != Some(crate::dictation::Command::SwitchCleanup) {
        change()?;
        return Ok(command);
    }
    let before = providers::read_key(store, id);
    change()?;
    let after = providers::read_key(store, id);
    let unchanged = matches!((&before, &after), (Ok(a), Ok(b)) if a == b);
    Ok((!(unchanged && whole)).then_some(crate::dictation::Command::SwitchCleanup))
}

/// Async for the same reason as [`provider_keys`]: writing to the keychain
/// can block on the OS access prompt. Sends whichever command
/// [`key_change_command`] says this save needs, so a pool model that just
/// gained its key joins the pool without reloading the speech engine, and
/// the transcription choice reopens as it always did.
#[tauri::command(async)]
pub fn provider_key_save(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    id: String,
    key: String,
) -> Result<(), CommandError> {
    let store = macalla_platform::secrets::SystemSecrets::new();
    let whole = cleanup_whole(&dictator);
    if let Some(command) = key_change_command(&history, &store, &id, whole, || {
        provider_key_save_with(&store, &id, &key)
    })? {
        dictator.send(command);
    }
    Ok(())
}

/// Remove provider `id`'s key. The stored choice is kept exactly as it is,
/// transcription and a cleanup pool alike, by design: a key removed by
/// mistake, or for a moment, is restored by adding it back, with nothing to
/// re-pick. Dictation's next open finds it missing on its own
/// and falls back to the local engine
/// ([`crate::dictation`]'s `try_open`/`is_cloud_reason`), saying so next to
/// the still-selected choice ([`crate::dictation::OpenedStatus`]) rather
/// than this command silently swapping the choice out from under it.
/// Whether dictation needs to reopen is [`key_command`]'s answer, asked
/// separately by the caller; this only removes the key.
fn provider_key_remove_with(store: &dyn SecretStore, id: &str) -> Result<(), CommandError> {
    providers::remove_key(store, id)?;
    Ok(())
}

/// Async for the same reason as [`provider_keys`]. Sends whichever command
/// [`key_change_command`] says this removal needs -- removing any other
/// provider's key changes nothing dictation is doing right now.
#[tauri::command(async)]
pub fn provider_key_remove(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    id: String,
) -> Result<(), CommandError> {
    let store = macalla_platform::secrets::SystemSecrets::new();
    let whole = cleanup_whole(&dictator);
    if let Some(command) = key_change_command(&history, &store, &id, whole, || {
        provider_key_remove_with(&store, &id)
    })? {
        dictator.send(command);
    }
    Ok(())
}

/// How ready one transcription option is, for the Models page. Never
/// `ready` for a provider unless a key is both stored and readable.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TranscriptionStatus {
    Ready,
    NeedsDownload,
    NotCompiledIn,
    Unavailable,
    NeedsKey,
    KeyStoreUnavailable,
    KeyUnreadable,
}

/// One row the Models page's transcription list can show: a local engine, or
/// a cloud provider offering transcription.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TranscriptionOptionView {
    /// `"local"` or `"provider"`.
    pub source: String,
    pub id: String,
    pub name: String,
    /// The maker, for a local engine ([`registry::EngineInfo::group`]);
    /// empty for a provider -- simpler than repeating the provider's own
    /// name here, which the page already shows as `name`.
    pub group: String,
    /// One sentence on what it is good for. Empty for a provider: cloud
    /// providers do not carry a blurb today.
    pub blurb: String,
    pub status: TranscriptionStatus,
    /// The download description, or the reason it is unavailable, needs a
    /// key, or the key could not be read. `None` when `status` needs no
    /// further explanation (ready, needs a key with nothing more to say).
    pub detail: Option<String>,
    /// Whether dictations run on this option now.
    pub chosen: bool,
    /// The catalogue id of the model a local engine runs on. For a
    /// provider, the stored choice's own model id (OpenRouter only) when
    /// this row is the one `chosen`; `None` for a provider that is not
    /// chosen, or whose choice carries no model.
    pub model: Option<String>,
    /// Words wrong on Macalla's own measured clips
    /// ([`macalla_session::choice::measured_transcription`]), or `None` when
    /// nobody has measured this id.
    pub measured_words_wrong: Option<[u16; 3]>,
}

fn transcription_status_and_detail(
    availability: &Availability,
) -> (TranscriptionStatus, Option<String>) {
    match availability {
        Availability::Ready => (TranscriptionStatus::Ready, None),
        Availability::NotCompiledIn => (TranscriptionStatus::NotCompiledIn, None),
        Availability::NeedsDownload(what) => {
            (TranscriptionStatus::NeedsDownload, Some(what.clone()))
        }
        Availability::Unavailable(reason) => {
            (TranscriptionStatus::Unavailable, Some(reason.clone()))
        }
    }
}

fn cloud_status_and_detail(
    availability: &macalla_session::choice::CloudAvailability,
) -> (TranscriptionStatus, Option<String>) {
    use macalla_session::choice::CloudAvailability;
    match availability {
        CloudAvailability::Ready => (TranscriptionStatus::Ready, None),
        CloudAvailability::NeedsKey => (TranscriptionStatus::NeedsKey, None),
        CloudAvailability::KeyStoreUnavailable(reason) => (
            TranscriptionStatus::KeyStoreUnavailable,
            Some(reason.clone()),
        ),
        CloudAvailability::KeyUnreadable(reason) => {
            (TranscriptionStatus::KeyUnreadable, Some(reason.clone()))
        }
        CloudAvailability::NotCompiledIn => (TranscriptionStatus::NotCompiledIn, None),
    }
}

/// Every transcription choice this build knows about, built from
/// [`macalla_session::choice::transcription_options`], the local half of
/// which is [`macalla_session::choice::local_transcription_options`] --
/// the Engine card is a one-line summary of this same overview
/// not a second listing.
fn transcription_option_views(
    keys: &dyn SecretStore,
    chosen: Option<&str>,
    chosen_model: Option<&str>,
) -> Vec<TranscriptionOptionView> {
    use macalla_session::choice::TranscriptionOption;
    let in_use = chosen.unwrap_or(macalla_engines::default_engine_id());
    macalla_session::choice::transcription_options(keys)
        .into_iter()
        .map(|option| match option {
            TranscriptionOption::Local(e) => {
                let (status, detail) = transcription_status_and_detail(&e.availability);
                TranscriptionOptionView {
                    source: "local".to_string(),
                    id: e.id.to_string(),
                    name: e.name.to_string(),
                    group: e.group.to_string(),
                    blurb: e.blurb.to_string(),
                    status,
                    detail,
                    chosen: e.id == in_use,
                    model: e.model.map(str::to_string),
                    measured_words_wrong: macalla_session::choice::measured_transcription(e.id),
                }
            }
            TranscriptionOption::Cloud {
                provider,
                availability,
            } => {
                let (status, detail) = cloud_status_and_detail(&availability);
                let is_chosen = provider.id == in_use;
                TranscriptionOptionView {
                    source: "provider".to_string(),
                    id: provider.id.to_string(),
                    name: provider.name.to_string(),
                    group: String::new(),
                    blurb: String::new(),
                    status,
                    detail,
                    chosen: is_chosen,
                    model: if is_chosen {
                        chosen_model.map(str::to_string)
                    } else {
                        None
                    },
                    measured_words_wrong: macalla_session::choice::measured_transcription(
                        provider.id,
                    ),
                }
            }
        })
        .collect()
}

/// A provider's key state, for the Models page's cleanup list. Mirrors
/// [`providers::KeyState`] without carrying the reason string twice -- the
/// reason is [`CleanupOptionView::reason`] instead, alongside Apple's own.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum KeyStateView {
    Saved,
    Missing,
    StoreUnavailable,
    Unreadable,
}

/// One row the Models page's cleanup list can show: off, Apple Intelligence,
/// a person's own server, or a cloud provider.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CleanupOptionView {
    /// `"off"`, `"apple-intelligence"`, `"local-server"` or `"provider"`.
    pub kind: String,
    pub id: String,
    pub name: String,
    pub available: bool,
    /// Apple's own [`macalla_enhancers::apple::Availability::explain`] text
    /// when it is not available, or a provider's key-state reason. `None`
    /// when nothing more needs saying.
    pub reason: Option<String>,
    /// The stored key's state, for a provider option only.
    pub key: Option<KeyStateView>,
}

fn key_state_view(key: &providers::KeyState) -> (bool, Option<String>, KeyStateView) {
    match key {
        providers::KeyState::Saved => (true, None, KeyStateView::Saved),
        providers::KeyState::Missing => (false, None, KeyStateView::Missing),
        providers::KeyState::StoreUnavailable(reason) => {
            (false, Some(reason.clone()), KeyStateView::StoreUnavailable)
        }
        providers::KeyState::Unreadable(reason) => {
            (false, Some(reason.clone()), KeyStateView::Unreadable)
        }
    }
}

/// Why Apple Intelligence cannot be used on `os`, or `None` when it can.
///
/// A build without it is `Unsupported` on every platform, but only a Mac can
/// get it from a different build: Windows and Linux say the platform has
/// none, from [`crate::enhancement_words`]. Every other reason is Apple's own.
fn apple_intelligence_reason(
    availability: macalla_session::choice::AppleIntelligenceAvailability,
    os: macalla_platform::Os,
) -> Option<&'static str> {
    use macalla_platform::Os;
    use macalla_session::choice::AppleIntelligenceAvailability as A;
    match (availability, os) {
        (A::Unsupported, Os::Windows) => {
            Some(crate::enhancement_words::APPLE_INTELLIGENCE_NOT_ON_WINDOWS)
        }
        (A::Unsupported, Os::Linux) => {
            Some(crate::enhancement_words::APPLE_INTELLIGENCE_NOT_ON_LINUX)
        }
        _ => availability.explain(),
    }
}

/// Every cleanup choice this build knows about, built from
/// [`macalla_session::choice::cleanup_options`].
fn cleanup_option_views(keys: &dyn SecretStore) -> Vec<CleanupOptionView> {
    use macalla_session::choice::CleanupOption;
    macalla_session::choice::cleanup_options(keys)
        .into_iter()
        .map(|option| match option {
            CleanupOption::Off => CleanupOptionView {
                kind: "off".to_string(),
                id: "off".to_string(),
                name: "Off".to_string(),
                available: true,
                reason: None,
                key: None,
            },
            CleanupOption::AppleIntelligence(availability) => {
                let reason = apple_intelligence_reason(availability, macalla_platform::Os::HERE);
                CleanupOptionView {
                    kind: "apple-intelligence".to_string(),
                    id: "apple-intelligence".to_string(),
                    name: "Apple Intelligence".to_string(),
                    available: reason.is_none(),
                    reason: reason.map(str::to_string),
                    key: None,
                }
            }
            CleanupOption::LocalServer => CleanupOptionView {
                kind: "local-server".to_string(),
                id: "local-server".to_string(),
                name: "Your own server".to_string(),
                available: true,
                reason: None,
                key: None,
            },
            CleanupOption::Provider { provider, key } => {
                let (available, reason, key_view) = key_state_view(&key);
                CleanupOptionView {
                    kind: "provider".to_string(),
                    id: provider.id.to_string(),
                    name: provider.name.to_string(),
                    available,
                    reason,
                    key: Some(key_view),
                }
            }
        })
        .collect()
}

/// The Models page's cleanup choice: what the Enhancement section saves and
/// reads back. Mirrors [`macalla_session::choice::CleanupChoice`] and
/// [`crate::history::CleanupChoice`], but is its own wire shape -- this
/// module's own convention (see the doc comment at the top of this file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CleanupChoiceView {
    Off,
    AppleIntelligence,
    Server {
        address: String,
        model: String,
    },
    /// Cloud models in ticked order: the first is asked first, and the next
    /// takes over when one fails.
    Pool {
        models: Vec<PoolModelView>,
    },
}

/// One ticked model in a pool choice. Mirrors
/// [`macalla_session::choice::PoolModel`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PoolModelView {
    pub provider: String,
    pub model: String,
}

impl From<crate::history::CleanupChoice> for CleanupChoiceView {
    fn from(choice: crate::history::CleanupChoice) -> Self {
        match choice {
            crate::history::CleanupChoice::Off => CleanupChoiceView::Off,
            crate::history::CleanupChoice::AppleIntelligence => {
                CleanupChoiceView::AppleIntelligence
            }
            crate::history::CleanupChoice::Server { address, model } => {
                CleanupChoiceView::Server { address, model }
            }
            crate::history::CleanupChoice::Pool { models } => CleanupChoiceView::Pool {
                models: models
                    .into_iter()
                    .map(|m| PoolModelView {
                        provider: m.provider,
                        model: m.model,
                    })
                    .collect(),
            },
        }
    }
}

impl From<&CleanupChoiceView> for crate::history::CleanupChoice {
    fn from(choice: &CleanupChoiceView) -> Self {
        match choice {
            CleanupChoiceView::Off => crate::history::CleanupChoice::Off,
            CleanupChoiceView::AppleIntelligence => {
                crate::history::CleanupChoice::AppleIntelligence
            }
            CleanupChoiceView::Server { address, model } => crate::history::CleanupChoice::Server {
                address: address.clone(),
                model: model.clone(),
            },
            CleanupChoiceView::Pool { models } => crate::history::CleanupChoice::Pool {
                models: models
                    .iter()
                    .map(|m| crate::history::PoolEntry {
                        provider: m.provider.clone(),
                        model: m.model.clone(),
                    })
                    .collect(),
            },
        }
    }
}

impl From<&CleanupChoiceView> for macalla_session::choice::CleanupChoice {
    fn from(choice: &CleanupChoiceView) -> Self {
        match choice {
            CleanupChoiceView::Off => macalla_session::choice::CleanupChoice::Off,
            CleanupChoiceView::AppleIntelligence => {
                macalla_session::choice::CleanupChoice::AppleIntelligence
            }
            CleanupChoiceView::Server { address, model } => {
                macalla_session::choice::CleanupChoice::Server {
                    address: address.clone(),
                    model: model.clone(),
                }
            }
            CleanupChoiceView::Pool { models } => macalla_session::choice::CleanupChoice::Pool {
                models: models
                    .iter()
                    .map(|m| macalla_session::choice::PoolModel {
                        provider: m.provider.clone(),
                        model: m.model.clone(),
                    })
                    .collect(),
            },
        }
    }
}

/// The plain-function core of [`cleanup_choose`]: validated in the session
/// ([`macalla_session::choice::validate_cleanup_choice`]), not here --
/// commands stay translators. Returns whether the stored choice
/// changed: one equal to it, field for field and a pool in the same order
/// (the order is the failover order), writes nothing and returns `false`.
/// A stored choice that cannot be read equals nothing, so choosing Off
/// rewrites it.
fn cleanup_choose_with(
    history: &crate::history::HistoryHandle,
    choice: &CleanupChoiceView,
) -> Result<bool, CommandError> {
    let session_choice: macalla_session::choice::CleanupChoice = choice.into();
    macalla_session::choice::validate_cleanup_choice(&session_choice)?;
    let stored: crate::history::CleanupChoice = choice.into();
    if history.cleanup_choice_as_stored().as_ref() == Some(&stored) {
        return Ok(false);
    }
    history.set_cleanup_choice(&stored)?;
    Ok(true)
}

/// Tidy what you say with `choice` from now on: off, Apple Intelligence, or
/// a person's own server. Refused when Apple Intelligence is not available,
/// or a server choice is missing its address or model. Dictation swaps its
/// Tier 1 cleanup provider in place to take it -- the speech engine, and
/// everything else the session holds, is untouched
/// ([`macalla_session::Session::replace_cleanup`]); a dictation in progress
/// finishes first. See [`cleanup_choose_command`] for when nothing is sent.
#[tauri::command]
pub fn cleanup_choose(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
    choice: CleanupChoiceView,
) -> Result<(), CommandError> {
    let whole = cleanup_whole(&dictator);
    if let Some(command) = cleanup_choose_command(&history, &choice, whole)? {
        dictator.send(command);
    }
    Ok(())
}

/// Store `choice` ([`cleanup_choose_with`]) and say whether dictation must
/// build it: always for a changed choice, and for the one already in force
/// unless the running cleanup is `whole` -- picking it again is how a
/// person retries one that failed, is not ready, or left a member out. A
/// whole one is not swapped again, so its providers keep what they have
/// learned.
fn cleanup_choose_command(
    history: &crate::history::HistoryHandle,
    choice: &CleanupChoiceView,
    whole: bool,
) -> Result<Option<crate::dictation::Command>, CommandError> {
    let changed = cleanup_choose_with(history, choice)?;
    Ok((changed || !whole).then_some(crate::dictation::Command::SwitchCleanup))
}

/// Everything the Models page shows in one call: the transcription options,
/// the cleanup options, every provider's key state, whether this build has
/// cloud transcription compiled in at all, and which local engine previews.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ModelsOverview {
    pub transcription: Vec<TranscriptionOptionView>,
    pub cleanup: Vec<CleanupOptionView>,
    pub keys: ProviderKeysView,
    pub cloud_transcription_compiled_in: bool,
    /// The local engine that shows the live preview while the person
    /// speaks: the stored engine choice, or the platform default -- the
    /// same one whether or not a cloud choice is also stored.
    /// The single source the page reads this from, so it never makes its
    /// own second call to find it.
    pub preview_engine_id: String,
    /// The saved cleanup choice, so the page shows it selected when it
    /// opens without a second call.
    pub cleanup_chosen: CleanupChoiceView,
    /// The providers a cleanup pool can take models from: those with a
    /// saved key, in the provider table's order.
    pub pool_providers: Vec<PoolProviderView>,
    /// What dictation actually opened with, the truth next to `chosen` and
    /// `cleanup_chosen` above when a stored choice could not be fully used
    /// (a removed key, most often): the stored choice is kept as it is, by
    /// design, and this says what is really running instead.
    /// `None` before the dictation thread's first open, in which case the
    /// page shows the stored choice alone, with no "in use" claim.
    pub in_use: Option<OpenedStatusView>,
}

/// Mirrors the fields of [`crate::dictation::OpenedStatus`] the Models page
/// reads: `transcription_issue` does not cross this boundary.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OpenedStatusView {
    pub transcription: String,
    pub enhancement: Option<String>,
    /// The pool panel's own "Saved" state (not built, but not stored-choice
    /// mismatched either) reads this; the panel's own reason line no longer
    /// repeats it (the Enhancement card's `enhancement_reason` does).
    pub enhancement_issue: Option<String>,
    pub transcription_reason: Option<String>,
    pub enhancement_reason: Option<String>,
}

impl From<crate::dictation::OpenedStatus> for OpenedStatusView {
    fn from(status: crate::dictation::OpenedStatus) -> Self {
        Self {
            transcription: status.transcription,
            enhancement: status.enhancement,
            enhancement_issue: status.enhancement_issue,
            transcription_reason: status.transcription_reason,
            enhancement_reason: status.enhancement_reason,
        }
    }
}

/// One provider a cleanup pool can take models from. Mirrors the id and name
/// of [`macalla_session::providers::Provider`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolProviderView {
    pub id: String,
    pub name: String,
}

/// `chosen` is the id dictations run on now: a provider id when a cloud
/// choice is stored, a local engine id otherwise, or `None` for the
/// platform's default engine. `chosen_model` is that choice's own model id
/// (OpenRouter's own transcription models; every other provider has none),
/// carried onto the chosen row's [`TranscriptionOptionView::model`].
/// `cleanup_chosen` is the saved cleanup choice, verbatim.
fn models_overview_with(
    keys: &dyn SecretStore,
    chosen: Option<&str>,
    chosen_model: Option<&str>,
    preview_engine_id: &str,
    cleanup_chosen: CleanupChoiceView,
    in_use: Option<OpenedStatusView>,
) -> ModelsOverview {
    ModelsOverview {
        transcription: transcription_option_views(keys, chosen, chosen_model),
        cleanup: cleanup_option_views(keys),
        keys: provider_keys_view(keys),
        cloud_transcription_compiled_in: macalla_session::choice::cloud_transcription_compiled_in(),
        preview_engine_id: preview_engine_id.to_string(),
        cleanup_chosen,
        pool_providers: macalla_session::cloud_pool::pool_providers(keys)
            .into_iter()
            .map(|p| PoolProviderView {
                id: p.id.to_string(),
                name: p.name.to_string(),
            })
            .collect(),
        in_use,
    }
}

/// Everything the Models page needs: transcription options, cleanup options
/// and every provider's key state, in one call, so the page and the Engine
/// card can never read a different answer for the same provider.
/// Async for the same reason as [`provider_keys`]: a keychain read can block
/// on a macOS access prompt.
#[tauri::command(async)]
pub fn models_overview(
    history: tauri::State<'_, crate::history::HistoryHandle>,
    dictator: tauri::State<'_, crate::dictation::Dictator>,
) -> ModelsOverview {
    // A stored cloud choice is what dictations run on now; the local engine
    // choice still matters (it is the live preview), but only the provider
    // is marked `chosen` on the Models page while one is set.
    let cloud = history.cloud_choice();
    let chosen = cloud
        .as_ref()
        .map(|c| c.provider.clone())
        .or_else(|| history.engine_choice());
    let chosen_model = cloud.as_ref().and_then(|c| c.model.clone());
    let preview_engine_id = history
        .engine_choice()
        .unwrap_or_else(|| macalla_engines::default_engine_id().to_string());
    models_overview_with(
        &macalla_platform::secrets::SystemSecrets::new(),
        chosen.as_deref(),
        chosen_model.as_deref(),
        &preview_engine_id,
        history.cleanup_choice().into(),
        dictator.status().map(OpenedStatusView::from),
    )
}

/// One exact model id's measured accuracy on cleanup, mirroring
/// [`macalla_session::choice::MeasuredCleanup`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MeasuredCleanupView {
    pub right: u8,
    pub of: u8,
    pub machine: String,
}

/// One model a person's own server offers, for the local-server cleanup
/// option's own page. Mirrors [`macalla_session::choice::ServerModelOffer`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ServerModelView {
    pub id: String,
    pub size_bytes: Option<u64>,
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub measured: Option<MeasuredCleanupView>,
}

/// What checking a person's own server shows: what it offers, and whether it
/// is this machine (`loopback`) or somewhere else, so the page can label it
/// "Local" or "Your server".
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ServerModelsView {
    pub models: Vec<ServerModelView>,
    pub loopback: bool,
}

fn server_models_with(address: &str) -> Result<ServerModelsView, CommandError> {
    let offers = macalla_session::choice::server_models(address)?;
    Ok(ServerModelsView {
        models: offers
            .into_iter()
            .map(|o| ServerModelView {
                id: o.id,
                size_bytes: o.size_bytes,
                parameter_size: o.parameter_size,
                quantization: o.quantization,
                capabilities: o.capabilities,
                measured: o.measured.map(|m| MeasuredCleanupView {
                    right: m.right,
                    of: m.of,
                    machine: m.machine.to_string(),
                }),
            })
            .collect(),
        loopback: macalla_session::choice::is_loopback(address),
    })
}

/// List what a person's own OpenAI-compatible server offers, for the Models
/// page's local-server card. Async: this is a blocking network call, unlike
/// everything else on this page, and must not stall the UI thread.
#[tauri::command(async)]
pub fn server_models(address: String) -> Result<ServerModelsView, CommandError> {
    server_models_with(&address)
}

// -------------------------------------------------------- openrouter_models

/// Mirrors [`macalla_session::openrouter::ReasoningClass`].
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningClassView {
    NoReasoning,
    CanTurnOff,
    Minimal,
}

/// One cleanup model OpenRouter offers, for the Models page's OpenRouter
/// card. Mirrors [`macalla_session::openrouter::CleanupOffer`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OpenRouterCleanupOfferView {
    pub id: String,
    pub name: String,
    pub reasoning: ReasoningClassView,
    pub measured: Option<MeasuredCleanupView>,
}

/// Mirrors [`macalla_session::openrouter::MeasuredTranscription`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MeasuredTranscriptionView {
    pub words_wrong: [u16; 3],
    pub clip_words: [u16; 3],
}

/// One transcription model OpenRouter offers. Mirrors
/// [`macalla_session::openrouter::TranscriptionOffer`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OpenRouterTranscriptionOfferView {
    pub id: String,
    pub name: String,
    pub measured: Option<MeasuredTranscriptionView>,
    pub also_runs_locally: bool,
}

/// What one OpenRouter listing request returns: which kind was asked for,
/// its offers, when the underlying listing was fetched, and (when the fetch
/// failed and this is the last good cached copy instead) why.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum OpenRouterModelsView {
    Text {
        offers: Vec<OpenRouterCleanupOfferView>,
        fetched_at_ms: u64,
        stale_reason: Option<String>,
    },
    Transcription {
        offers: Vec<OpenRouterTranscriptionOfferView>,
        fetched_at_ms: u64,
        stale_reason: Option<String>,
    },
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn reasoning_class_view(class: macalla_session::openrouter::ReasoningClass) -> ReasoningClassView {
    use macalla_session::openrouter::ReasoningClass;
    match class {
        ReasoningClass::NoReasoning => ReasoningClassView::NoReasoning,
        ReasoningClass::CanTurnOff => ReasoningClassView::CanTurnOff,
        ReasoningClass::Minimal => ReasoningClassView::Minimal,
    }
}

/// The transcription half of [`openrouter_models_with`]. Called
/// unconditionally, like [`macalla_session::choice::cloud_engine`] --
/// `macalla_session::openrouter::transcription_offers` answers
/// [`macalla_session::Error::CloudNotCompiledIn`] itself in a build with no
/// cloud transcription compiled in (the app is complete without cloud engines), so this command has
/// no feature check of its own.
fn openrouter_transcription_models(
    cache_dir: &std::path::Path,
    refresh: macalla_session::openrouter::Refresh,
    now_ms: u64,
) -> Result<OpenRouterModelsView, CommandError> {
    let result = macalla_session::openrouter::transcription_offers(cache_dir, refresh, now_ms)?;
    Ok(OpenRouterModelsView::Transcription {
        offers: result
            .offers
            .into_iter()
            .map(|o| OpenRouterTranscriptionOfferView {
                id: o.id,
                name: o.name,
                measured: o.measured.map(|m| MeasuredTranscriptionView {
                    words_wrong: m.words_wrong,
                    clip_words: m.clip_words,
                }),
                also_runs_locally: o.also_runs_locally,
            })
            .collect(),
        fetched_at_ms: result.fetched_at_ms,
        stale_reason: result.stale_reason,
    })
}

fn openrouter_models_with(
    cache_dir: &std::path::Path,
    kind: &str,
    refresh: bool,
) -> Result<OpenRouterModelsView, CommandError> {
    use macalla_session::openrouter::Refresh;
    let refresh = if refresh {
        Refresh::Now
    } else {
        Refresh::IfStale
    };
    let now = now_ms();
    match kind {
        "text" => {
            let result = macalla_session::openrouter::cleanup_offers(cache_dir, refresh, now)?;
            Ok(OpenRouterModelsView::Text {
                offers: result
                    .offers
                    .into_iter()
                    .map(|o| OpenRouterCleanupOfferView {
                        id: o.id,
                        name: o.name,
                        reasoning: reasoning_class_view(o.reasoning),
                        measured: o.measured.map(|m| MeasuredCleanupView {
                            right: m.right,
                            of: m.of,
                            machine: m.machine.to_string(),
                        }),
                    })
                    .collect(),
                fetched_at_ms: result.fetched_at_ms,
                stale_reason: result.stale_reason,
            })
        }
        "transcription" => openrouter_transcription_models(cache_dir, refresh, now),
        other => {
            tracing::warn!(kind = other, "not a listing kind");
            Err("That is not a kind of model list Macalla knows about.".into())
        }
    }
}

/// Where the OpenRouter listing cache lives: a subfolder of the app's own
/// cache directory, distinct from application-support (which holds the
/// dictionary and history) -- a stale or corrupt listing is safe to delete
/// and refetch, so it belongs where the OS expects disposable data.
fn openrouter_cache_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, CommandError> {
    use tauri::Manager;
    let dir = app
        .path()
        .app_cache_dir()
        .or_say("Could not find where Macalla keeps its downloaded model lists.")?;
    Ok(dir.join("provider-models"))
}

/// List OpenRouter's models of one `kind` (`"text"` for cleanup,
/// `"transcription"` for STT), from the on-disk cache or fresh from
/// OpenRouter. `refresh: true` always fetches -- used when a key
/// is saved and by the page's own Refresh action; `false` fetches only when
/// there is no cache or it is a week old or more -- used when the page
/// opens. Async: this can be a live network request.
#[tauri::command(async)]
pub fn openrouter_models(
    app: tauri::AppHandle,
    kind: String,
    refresh: bool,
) -> Result<OpenRouterModelsView, CommandError> {
    let cache_dir = openrouter_cache_dir(&app)?;
    openrouter_models_with(&cache_dir, &kind, refresh)
}

// ------------------------------------------------------------- pool_models

/// One model a cleanup pool can hold. Mirrors
/// [`macalla_session::cloud_pool::PoolOffer`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolOfferView {
    pub id: String,
    pub name: String,
    pub input_token_limit: Option<u64>,
}

/// What one provider's pool listing returns: its offers, when the underlying
/// list was fetched, and (when the fetch failed and this is the last good
/// cached copy instead) why.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolModelsView {
    pub provider: String,
    pub offers: Vec<PoolOfferView>,
    pub fetched_at_ms: u64,
    pub stale_reason: Option<String>,
}

fn pool_models_with(
    keys: &dyn SecretStore,
    cache_dir: &std::path::Path,
    provider: &str,
    refresh: bool,
) -> Result<PoolModelsView, CommandError> {
    use macalla_session::openrouter::Refresh;
    let refresh = if refresh {
        Refresh::Now
    } else {
        Refresh::IfStale
    };
    let result =
        macalla_session::cloud_pool::pool_offers(provider, keys, cache_dir, refresh, now_ms())?;
    Ok(PoolModelsView {
        provider: provider.to_string(),
        offers: result
            .offers
            .into_iter()
            .map(|o| PoolOfferView {
                id: o.id,
                name: o.name,
                input_token_limit: o.input_token_limit,
            })
            .collect(),
        fetched_at_ms: result.fetched_at_ms,
        stale_reason: result.stale_reason,
    })
}

/// List the models `provider` offers a cleanup pool, with its saved key,
/// from the on-disk cache or fresh from the provider. `refresh` as for
/// [`openrouter_models`]. Async: this can be a keychain read and a live
/// network request.
#[tauri::command(async)]
pub fn pool_models(
    app: tauri::AppHandle,
    provider: String,
    refresh: bool,
) -> Result<PoolModelsView, CommandError> {
    let cache_dir = openrouter_cache_dir(&app)?;
    pool_models_with(
        &macalla_platform::secrets::SystemSecrets::new(),
        &cache_dir,
        &provider,
        refresh,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_platform::secrets::MemoryStore;

    fn row(value: &str, label: &str) -> MicrophoneRow {
        MicrophoneRow {
            value: value.to_string(),
            label: label.to_string(),
        }
    }

    /// A build without Apple Intelligence says why in the platform's own
    /// words: on a Mac, that this build left it out; on Windows and Linux,
    /// that the platform has none. Every other state is Apple's own reason.
    #[test]
    fn apple_intelligence_missing_from_the_build_is_worded_per_platform() {
        use macalla_platform::Os;
        use macalla_session::choice::AppleIntelligenceAvailability as A;
        assert_eq!(
            apple_intelligence_reason(A::Unsupported, Os::MacOs),
            A::Unsupported.explain()
        );
        assert_eq!(
            apple_intelligence_reason(A::Unsupported, Os::Windows),
            Some("Apple Intelligence is not available on Windows.")
        );
        assert_eq!(
            apple_intelligence_reason(A::Unsupported, Os::Linux),
            Some("Apple Intelligence is not available on Linux.")
        );
        for os in [Os::MacOs, Os::Windows, Os::Linux] {
            assert_eq!(apple_intelligence_reason(A::Available, os), None);
            assert_eq!(
                apple_intelligence_reason(A::NotEnabled, os),
                A::NotEnabled.explain()
            );
        }
    }

    /// The checklist rows for `os`'s permissions, each as `state` with no
    /// action, and a non-permission step after them: what `summary_of` does
    /// with them is independent of whether this machine granted anything.
    fn summaries_on(os: macalla_platform::Os) -> Vec<StepSummary> {
        use macalla_core::onboarding::Step;
        let mut steps: Vec<Step> = ["microphone", "input-monitoring", "accessibility"]
            .into_iter()
            .map(|id| Step::done(id, "t", "d."))
            .collect();
        steps.push(Step::done("secure-input", "t", "d."));
        steps.push(Step::done("engine", "t", "d."));
        steps.into_iter().map(|s| summary_of(s, os)).collect()
    }

    fn flags(rows: &[StepSummary], id: &str) -> (bool, bool, bool) {
        let row = rows.iter().find(|r| r.id == id).expect("a row");
        (row.permission, row.ask, row.settings)
    }

    /// macOS keeps exactly what the page used to decide for itself: three
    /// permissions, Ask on the microphone and Accessibility (Input
    /// Monitoring is asked when the tap is made at launch), and a pane for
    /// all three.
    #[test]
    fn macos_checklist_flags_are_what_the_page_hardcoded() {
        let rows = summaries_on(macalla_platform::Os::MacOs);
        assert_eq!(flags(&rows, "microphone"), (true, true, true));
        assert_eq!(flags(&rows, "input-monitoring"), (true, false, true));
        assert_eq!(flags(&rows, "accessibility"), (true, true, true));
        assert_eq!(flags(&rows, "secure-input"), (false, false, false));
        assert_eq!(flags(&rows, "engine"), (false, false, false));
    }

    /// Windows and Linux have no permission step: nothing to group, ask for
    /// or open, so no macOS flow can show.
    #[test]
    fn windows_and_linux_checklists_have_no_permission_flow() {
        for os in [macalla_platform::Os::Windows, macalla_platform::Os::Linux] {
            let rows = summaries_on(os);
            assert!(
                rows.iter().all(|r| !r.permission && !r.ask && !r.settings),
                "{os:?}: {rows:?}"
            );
        }
    }

    /// And on this machine, the real checklist is macOS's.
    #[test]
    #[cfg(target_os = "macos")]
    fn this_macs_checklist_keeps_its_three_permissions_first() {
        let rows = step_summaries(None);
        let ids: Vec<&str> = rows
            .iter()
            .filter(|r| r.permission)
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(ids, ["microphone", "input-monitoring", "accessibility"]);
        assert_eq!(rows.last().map(|r| r.id.as_str()), Some("engine"));
    }

    #[test]
    fn no_devices_still_offers_the_system_default_row() {
        assert_eq!(
            microphone_rows(&[], None, None),
            vec![row("", "System default")]
        );
    }

    #[test]
    fn a_connected_device_gets_a_row_named_after_it_and_no_ghost_row() {
        let names = vec!["Built-in".to_string()];
        assert_eq!(
            microphone_rows(&names, Some("Built-in"), Some("Built-in")),
            vec![
                row("", "System default (Built-in)"),
                row("Built-in", "Built-in"),
            ]
        );
    }

    #[test]
    fn a_saved_device_that_has_gone_gets_a_ghost_row_right_after_the_default() {
        let names = vec!["Built-in".to_string()];
        assert_eq!(
            microphone_rows(&names, Some("USB Mic"), Some("Built-in")),
            vec![
                row("", "System default (Built-in)"),
                row("USB Mic", "USB Mic (not connected)"),
                row("Built-in", "Built-in"),
            ]
        );
    }

    #[test]
    fn setup_is_complete_once_every_required_step_is_done_with_an_optional_step_left() {
        use macalla_core::onboarding::Step;
        let optional = || Step::optional("language", "Language", "More languages.", "Add one.");
        let steps = vec![
            Step::done("microphone", "Microphone", "To hear you."),
            optional(),
        ];
        assert!(ready(steps), "an optional step left does not hold setup up");

        let blocked = vec![
            Step::done("microphone", "Microphone", "To hear you."),
            Step::required("accessibility", "Accessibility", "To paste.", "Allow it."),
            optional(),
        ];
        assert!(!ready(blocked), "a required step left does");
    }

    /// [`models_overview_with`] with no stored model and the platform
    /// default as the preview engine, for the tests that only care about
    /// `chosen`.
    fn overview_with(store: &dyn SecretStore, chosen: Option<&str>) -> ModelsOverview {
        models_overview_with(
            store,
            chosen,
            None,
            macalla_engines::default_engine_id(),
            CleanupChoiceView::Off,
            None,
        )
    }

    // -------------------------------------------------- transcription_choose

    fn local_choice(id: &str) -> TranscriptionChoice {
        TranscriptionChoice {
            source: "local".to_string(),
            id: id.to_string(),
            model: None,
        }
    }

    fn provider_choice(id: &str, model: Option<&str>) -> TranscriptionChoice {
        TranscriptionChoice {
            source: "provider".to_string(),
            id: id.to_string(),
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn choosing_local_saves_the_engine_and_clears_a_stored_cloud_choice() {
        let Some(engine) = registry::engines()
            .into_iter()
            .find(|e| e.availability.is_ready())
        else {
            // No local engine is ready with this build's compiled-in
            // features (e.g. no engine-apple, no engine-transcribe); there
            // is then nothing this test can choose successfully.
            return;
        };
        let history = crate::history::HistoryHandle::in_memory_for_test();
        history.set_cloud_choice("elevenlabs", None).unwrap();
        let store = MemoryStore::new();
        transcription_choose_with(&history, &store, &local_choice(engine.id)).unwrap();
        assert_eq!(history.engine_choice().as_deref(), Some(engine.id));
        assert_eq!(history.cloud_choice(), None);
    }

    #[test]
    fn choosing_a_provider_with_no_key_is_refused() {
        if !macalla_session::choice::cloud_transcription_compiled_in() {
            return; // covered by not_compiled_in_is_refused below
        }
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let store = MemoryStore::new();
        let error = transcription_choose_with(&history, &store, &provider_choice("openai", None))
            .unwrap_err();
        assert!(error.to_string().contains("Add a key"), "{error}");
        assert_eq!(history.cloud_choice(), None);
    }

    #[test]
    fn choosing_a_provider_with_a_saved_key_is_kept() {
        if !macalla_session::choice::cloud_transcription_compiled_in() {
            return;
        }
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let store = MemoryStore::new();
        providers::store_key(&store, "openai", "sk-test").unwrap();
        transcription_choose_with(&history, &store, &provider_choice("openai", None)).unwrap();
        assert_eq!(
            history.cloud_choice(),
            Some(crate::history::CloudChoice {
                provider: "openai".to_string(),
                model: None,
            })
        );
    }

    #[test]
    fn openrouter_needs_a_model_chosen_first() {
        if !macalla_session::choice::cloud_transcription_compiled_in() {
            return;
        }
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let store = MemoryStore::new();
        providers::store_key(&store, "openrouter", "sk-test").unwrap();
        let error =
            transcription_choose_with(&history, &store, &provider_choice("openrouter", None))
                .unwrap_err();
        assert!(error.to_string().contains("OpenRouter"), "{error}");
        assert_eq!(history.cloud_choice(), None);

        transcription_choose_with(
            &history,
            &store,
            &provider_choice("openrouter", Some("openai/gpt-transcribe")),
        )
        .unwrap();
        assert_eq!(
            history.cloud_choice(),
            Some(crate::history::CloudChoice {
                provider: "openrouter".to_string(),
                model: Some("openai/gpt-transcribe".to_string()),
            })
        );
    }

    #[test]
    fn without_cloud_transcription_compiled_in_a_provider_is_refused() {
        if macalla_session::choice::cloud_transcription_compiled_in() {
            // `cargo test --workspace` unifies the feature on through
            // macalla-cli's default features; only
            // `cargo test -p macalla-desktop --lib --no-default-features`
            // (CI's proof that the app is complete without cloud engines) turns it off. Ask the session
            // what it actually compiled in, rather than assuming either.
            return;
        }
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let store = MemoryStore::new();
        let error = transcription_choose_with(&history, &store, &provider_choice("openai", None))
            .unwrap_err();
        assert!(error.to_string().contains("cloud transcription"), "{error}");
    }

    #[test]
    fn the_overview_marks_the_chosen_provider_when_a_cloud_choice_is_stored() {
        let store = MemoryStore::new();
        providers::store_key(&store, "openai", "sk-test").unwrap();
        let overview = overview_with(&store, Some("openai"));
        let openai = overview
            .transcription
            .iter()
            .find(|t| t.id == "openai")
            .expect("openai offers transcription");
        assert!(openai.chosen, "{openai:?}");
        assert!(
            overview
                .transcription
                .iter()
                .filter(|t| t.source == "local")
                .all(|t| !t.chosen),
            "no local engine is chosen once a provider is"
        );
    }

    #[test]
    fn the_overview_carries_the_chosen_openrouter_model() {
        let store = MemoryStore::new();
        providers::store_key(&store, "openrouter", "sk-test").unwrap();
        let overview = models_overview_with(
            &store,
            Some("openrouter"),
            Some("openai/gpt-transcribe"),
            macalla_engines::default_engine_id(),
            CleanupChoiceView::Off,
            None,
        );
        let openrouter = overview
            .transcription
            .iter()
            .find(|t| t.id == "openrouter")
            .expect("openrouter offers transcription");
        assert!(openrouter.chosen, "{openrouter:?}");
        assert_eq!(openrouter.model.as_deref(), Some("openai/gpt-transcribe"));

        // A provider that is not the chosen one never carries a model, even
        // when one happens to be passed in.
        let mistral = overview
            .transcription
            .iter()
            .find(|t| t.id == "mistral")
            .expect("mistral offers transcription");
        assert_eq!(mistral.model, None);
    }

    #[test]
    fn the_overview_reports_the_preview_engine_id() {
        let store = MemoryStore::new();
        let overview = models_overview_with(
            &store,
            None,
            None,
            "apple-speech",
            CleanupChoiceView::Off,
            None,
        );
        assert_eq!(overview.preview_engine_id, "apple-speech");
    }

    #[test]
    fn the_overview_carries_no_in_use_status_before_the_first_open() {
        let store = MemoryStore::new();
        let overview = overview_with(&store, None);
        assert_eq!(overview.in_use, None);
    }

    #[test]
    fn the_overview_carries_the_stored_in_use_status() {
        let store = MemoryStore::new();
        let status = OpenedStatusView {
            transcription: "Apple Speech".to_string(),
            enhancement: None,
            enhancement_issue: None,
            transcription_reason: Some(
                "OpenAI has no key saved, so dictation is using Apple Speech on this Mac. Add \
                 the key again under Providers on the Models page, or choose another provider."
                    .to_string(),
            ),
            enhancement_reason: None,
        };
        let overview = models_overview_with(
            &store,
            None,
            None,
            macalla_engines::default_engine_id(),
            CleanupChoiceView::Off,
            Some(status.clone()),
        );
        assert_eq!(overview.in_use, Some(status));
    }

    #[test]
    fn the_overview_carries_the_measured_table() {
        let store = MemoryStore::new();
        let overview = overview_with(&store, None);
        let apple = overview
            .transcription
            .iter()
            .find(|t| t.id == "apple-speech")
            .expect("apple-speech is always listed");
        assert_eq!(apple.measured_words_wrong, Some([2, 6, 18]));
        let openrouter = overview.transcription.iter().find(|t| t.id == "openrouter");
        if let Some(openrouter) = openrouter {
            assert_eq!(openrouter.measured_words_wrong, None, "{openrouter:?}");
        }
    }

    #[test]
    fn every_provider_is_listed_and_none_are_saved_at_first() {
        let store = MemoryStore::new();
        let view = provider_keys_view(&store);
        assert!(view.store_unavailable.is_none());
        assert_eq!(view.providers.len(), providers::PROVIDERS.len());
        assert!(view.providers.iter().all(|p| !p.saved));
        let openai = view
            .providers
            .iter()
            .find(|p| p.id == "openai")
            .expect("openai is a provider");
        assert_eq!(openai.name, "OpenAI");
        assert_eq!(openai.kinds, vec!["text", "transcription"]);
    }

    #[test]
    fn a_saved_provider_reports_saved_and_removing_it_reports_unsaved_again() {
        let store = MemoryStore::new();
        provider_key_save_with(&store, "openai", "sk-test").unwrap();
        let saved = provider_keys_view(&store)
            .providers
            .into_iter()
            .find(|p| p.id == "openai")
            .expect("openai is a provider");
        assert!(saved.saved);

        provider_key_remove_with(&store, "openai").unwrap();
        let removed = provider_keys_view(&store)
            .providers
            .into_iter()
            .find(|p| p.id == "openai")
            .expect("openai is a provider");
        assert!(!removed.saved);
    }

    #[test]
    fn removing_the_chosen_providers_key_keeps_the_stored_choice_and_rebuilds_dictation() {
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        providers::store_key(&store, "elevenlabs", "sk-test").unwrap();
        history.set_cloud_choice("elevenlabs", None).unwrap();
        provider_key_remove_with(&store, "elevenlabs").unwrap();
        assert_eq!(
            key_command(&history, "elevenlabs"),
            Some(crate::dictation::Command::SwitchEngine),
            "the session must reopen without the key"
        );
        assert_eq!(
            history.cloud_choice(),
            Some(crate::history::CloudChoice {
                provider: "elevenlabs".to_string(),
                model: None,
            }),
            "the stored choice stays -- re-adding the key restores it with no re-pick"
        );
    }

    #[test]
    fn adding_the_removed_key_back_is_used_again_with_no_re_pick() {
        // Mirrors `provider_key_save`'s own `key_command` check: a key
        // saved for the stored choice's provider reaches the session the
        // same way removing it did, so the round trip needs no re-pick on
        // the Models page.
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        providers::store_key(&store, "elevenlabs", "sk-test").unwrap();
        history.set_cloud_choice("elevenlabs", None).unwrap();
        provider_key_remove_with(&store, "elevenlabs").unwrap();
        assert_eq!(
            history.cloud_choice().map(|c| c.provider),
            Some("elevenlabs".to_string())
        );
        assert!(
            provider_key_save_with(&store, "elevenlabs", "sk-test-again").is_ok(),
            "saving the key back must not be refused for a choice still on record"
        );
        assert!(
            key_command(&history, "elevenlabs").is_some(),
            "the still-stored choice is the session's, so it reopens with the new key"
        );
    }

    fn stored_pool(history: &crate::history::HistoryHandle, providers: &[&str]) {
        history
            .set_cleanup_choice(&crate::history::CleanupChoice::Pool {
                models: providers
                    .iter()
                    .map(|provider| crate::history::PoolEntry {
                        provider: provider.to_string(),
                        model: format!("{provider}-test"),
                    })
                    .collect(),
            })
            .unwrap();
    }

    #[test]
    fn removing_a_pooled_providers_key_rebuilds_dictation_and_keeps_the_pool() {
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        providers::store_key(&store, "openai", "sk-test").unwrap();
        providers::store_key(&store, "anthropic", "sk-test").unwrap();
        stored_pool(&history, &["openai", "anthropic"]);
        provider_key_remove_with(&store, "openai").unwrap();
        assert_eq!(
            key_command(&history, "openai"),
            Some(crate::dictation::Command::SwitchCleanup),
            "the session's pool holds OpenAI's key in memory"
        );
        assert!(matches!(
            history.cleanup_choice(),
            crate::history::CleanupChoice::Pool { models } if models.len() == 2
        ));
    }

    #[test]
    fn removing_a_pooled_providers_key_leaves_the_stored_pool_exactly_as_it_was() {
        // `removing_a_pooled_providers_key_rebuilds_dictation_and_keeps_the_pool`
        // only counts the members; this checks every one, and their order,
        // stand exactly as ticked -- `provider_key_remove_with` only takes
        // the key out of the keychain, never the stored choice
        // (dictation.rs's `try_open` doc: "adding the key back restores it
        // with nothing to re-pick").
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        providers::store_key(&store, "openai", "sk-test").unwrap();
        providers::store_key(&store, "anthropic", "sk-test").unwrap();
        stored_pool(&history, &["openai", "anthropic"]);
        provider_key_remove_with(&store, "openai").unwrap();
        assert_eq!(
            history.cleanup_choice(),
            crate::history::CleanupChoice::Pool {
                models: vec![
                    crate::history::PoolEntry {
                        provider: "openai".to_string(),
                        model: "openai-test".to_string(),
                    },
                    crate::history::PoolEntry {
                        provider: "anthropic".to_string(),
                        model: "anthropic-test".to_string(),
                    },
                ],
            }
        );
    }

    #[test]
    fn removing_a_key_outside_the_pool_and_the_transcription_choice_rebuilds_nothing() {
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        providers::store_key(&store, "mistral", "sk-test").unwrap();
        history.set_cloud_choice("elevenlabs", None).unwrap();
        stored_pool(&history, &["anthropic"]);
        provider_key_remove_with(&store, "mistral").unwrap();
        assert_eq!(key_command(&history, "mistral"), None);
    }

    #[test]
    fn saving_a_key_reaches_the_session_only_for_a_provider_it_uses() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai"]);
        history.set_cloud_choice("elevenlabs", None).unwrap();
        assert!(key_command(&history, "openai").is_some(), "a pool member");
        assert!(
            key_command(&history, "elevenlabs").is_some(),
            "the transcription choice"
        );
        assert!(key_command(&history, "mistral").is_none());
    }

    #[test]
    fn key_command_reopens_for_transcription_swaps_cleanup_in_place_for_a_pool_alone() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai"]);
        history.set_cloud_choice("elevenlabs", None).unwrap();

        assert_eq!(
            key_command(&history, "openai"),
            Some(crate::dictation::Command::SwitchCleanup),
            "only the pool uses it"
        );
        assert_eq!(
            key_command(&history, "elevenlabs"),
            Some(crate::dictation::Command::SwitchEngine),
            "the transcription choice always reopens"
        );
        assert_eq!(key_command(&history, "mistral"), None);
    }

    fn save_command_when(
        whole: bool,
        history: &crate::history::HistoryHandle,
        store: &dyn SecretStore,
        id: &str,
        key: &str,
    ) -> Option<crate::dictation::Command> {
        key_change_command(history, store, id, whole, || {
            provider_key_save_with(store, id, key)
        })
        .unwrap()
    }

    /// A save while the running cleanup is whole.
    fn save_command(
        history: &crate::history::HistoryHandle,
        store: &dyn SecretStore,
        id: &str,
        key: &str,
    ) -> Option<crate::dictation::Command> {
        save_command_when(true, history, store, id, key)
    }

    fn remove_command(
        history: &crate::history::HistoryHandle,
        store: &dyn SecretStore,
        id: &str,
    ) -> Option<crate::dictation::Command> {
        key_change_command(history, store, id, true, || {
            provider_key_remove_with(store, id)
        })
        .unwrap()
    }

    #[test]
    fn the_same_key_saved_again_rebuilds_a_pool_that_is_not_whole() {
        // A member was left out -- its key could not be read when the pool
        // was built -- so saving the same key again is the repair.
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai", "anthropic"]);
        providers::store_key(&store, "openai", "sk-one").unwrap();
        assert_eq!(
            save_command_when(false, &history, &store, "openai", "sk-one"),
            Some(crate::dictation::Command::SwitchCleanup)
        );
        assert_eq!(
            save_command_when(true, &history, &store, "openai", "sk-one"),
            None
        );
    }

    #[test]
    fn a_pooled_providers_key_rebuilds_cleanup_only_when_it_changes() {
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai"]);
        let switch = Some(crate::dictation::Command::SwitchCleanup);

        assert_eq!(
            remove_command(&history, &store, "openai"),
            None,
            "none was there"
        );
        assert_eq!(
            save_command(&history, &store, "openai", "sk-one"),
            switch,
            "a first key"
        );
        assert_eq!(
            save_command(&history, &store, "openai", "sk-one"),
            None,
            "the same key"
        );
        assert_eq!(
            save_command(&history, &store, "openai", "  sk-one\n"),
            None,
            "the same key once trimmed, as it is stored"
        );
        assert_eq!(
            save_command(&history, &store, "openai", "sk-two"),
            switch,
            "a new value"
        );
        assert_eq!(
            providers::read_key(&store, "openai").unwrap().as_deref(),
            Some("sk-two"),
            "every save is still written"
        );
        assert_eq!(
            remove_command(&history, &store, "openai"),
            switch,
            "one was there"
        );
        assert_eq!(providers::read_key(&store, "openai").unwrap(), None);
    }

    #[test]
    fn the_transcription_choice_reopens_on_every_save_or_remove_as_before() {
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai"]);
        history.set_cloud_choice("openai", None).unwrap();
        let reopen = Some(crate::dictation::Command::SwitchEngine);
        assert_eq!(save_command(&history, &store, "openai", "sk-one"), reopen);
        assert_eq!(save_command(&history, &store, "openai", "sk-one"), reopen);
        assert_eq!(remove_command(&history, &store, "openai"), reopen);
        assert_eq!(remove_command(&history, &store, "openai"), reopen);
        assert_eq!(save_command(&history, &store, "mistral", "sk-one"), None);
    }

    #[test]
    fn a_refused_save_sends_nothing_and_says_why() {
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai"]);
        let result = key_change_command(&history, &store, "openai", true, || {
            provider_key_save_with(&store, "openai", "   ")
        });
        assert!(result.is_err());
    }

    #[test]
    fn a_pooled_key_that_cannot_be_read_counts_as_changed() {
        struct Unreadable(MemoryStore);
        impl SecretStore for Unreadable {
            fn get(&self, account: &str) -> macalla_platform::Result<Option<String>> {
                Err(macalla_platform::Error::Secret {
                    account: account.to_string(),
                    reason: "the item could not be decoded".to_string(),
                })
            }
            fn set(&self, account: &str, secret: &str) -> macalla_platform::Result<()> {
                self.0.set(account, secret)
            }
            fn delete(&self, account: &str) -> macalla_platform::Result<()> {
                self.0.delete(account)
            }
        }
        let store = Unreadable(MemoryStore::new());
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai"]);
        let switch = Some(crate::dictation::Command::SwitchCleanup);
        assert_eq!(save_command(&history, &store, "openai", "sk-one"), switch);
        assert_eq!(save_command(&history, &store, "openai", "sk-one"), switch);
        assert_eq!(remove_command(&history, &store, "openai"), switch);
    }

    #[test]
    fn key_command_prefers_switch_engine_when_a_provider_is_both() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        stored_pool(&history, &["openai"]);
        history.set_cloud_choice("openai", None).unwrap();

        assert_eq!(
            key_command(&history, "openai"),
            Some(crate::dictation::Command::SwitchEngine),
            "the transcription choice already rebuilds cleanup when it reopens"
        );
    }

    #[test]
    fn removing_a_different_providers_key_leaves_the_choice_alone() {
        let store = MemoryStore::new();
        let history = crate::history::HistoryHandle::in_memory_for_test();
        providers::store_key(&store, "elevenlabs", "sk-test").unwrap();
        providers::store_key(&store, "openai", "sk-test").unwrap();
        history.set_cloud_choice("elevenlabs", None).unwrap();
        provider_key_remove_with(&store, "openai").unwrap();
        assert_eq!(key_command(&history, "openai"), None);
        assert_eq!(
            history.cloud_choice(),
            Some(crate::history::CloudChoice {
                provider: "elevenlabs".to_string(),
                model: None,
            })
        );
    }

    #[test]
    fn an_unavailable_store_is_reported_once_not_per_provider() {
        struct Unavailable;
        impl SecretStore for Unavailable {
            fn get(&self, _: &str) -> macalla_platform::Result<Option<String>> {
                Err(macalla_platform::Error::SecretStoreUnavailable(
                    "no store on this platform".to_string(),
                ))
            }
            fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
        }
        let view = provider_keys_view(&Unavailable);
        assert!(view.providers.is_empty());
        assert_eq!(
            view.store_unavailable.as_deref(),
            Some(
                "This device's secure key storage is not available, so no key can be checked or saved."
            )
        );
    }

    #[test]
    fn a_stored_key_that_fails_to_read_is_unsaved_with_a_reason_not_missing() {
        // The bug: a per-item read failure (denied access prompt, bad
        // decoding) fell into the same catch-all as "no key stored", so the
        // page would offer "add a key" over one that already exists and
        // saving would overwrite it.
        struct Unreadable;
        impl SecretStore for Unreadable {
            fn get(&self, account: &str) -> macalla_platform::Result<Option<String>> {
                Err(macalla_platform::Error::Secret {
                    account: account.to_string(),
                    reason: "the item could not be decoded".to_string(),
                })
            }
            fn set(&self, _: &str, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> macalla_platform::Result<()> {
                unreachable!()
            }
        }
        let view = provider_keys_view(&Unreadable);
        assert!(view.store_unavailable.is_none());
        let openai = view
            .providers
            .iter()
            .find(|p| p.id == "openai")
            .expect("openai is a provider");
        assert!(!openai.saved);
        assert_eq!(
            openai.error.as_deref(),
            Some("The key for OpenAI could not be used. Enter it again under Providers on the Models page.")
        );
    }

    #[test]
    fn saving_an_unknown_provider_id_is_an_error() {
        let store = MemoryStore::new();
        assert!(provider_key_save_with(&store, "watson", "sk-test").is_err());
    }

    #[test]
    fn saving_an_empty_key_is_refused() {
        let store = MemoryStore::new();
        assert!(provider_key_save_with(&store, "openai", "  ").is_err());
        assert!(!providers::has_key(&store, "openai").unwrap());
    }

    #[test]
    fn nothing_returned_or_reported_ever_contains_the_key() {
        let store = MemoryStore::new();
        let key = "sk-super-secret-value";
        provider_key_save_with(&store, "openai", key).unwrap();
        let view = provider_keys_view(&store);
        let view_json = serde_json::to_string(&view).unwrap();
        assert!(!view_json.contains(key), "{view_json}");

        let save_err = provider_key_save_with(&store, "watson", key).unwrap_err();
        assert!(!save_err.to_string().contains(key), "{save_err}");

        // A key containing a line break is refused by name in the error, but
        // the error must not echo the invalid value either.
        let multi_line = "line-one\nline-two";
        let invalid_err = provider_key_save_with(&store, "openai", multi_line).unwrap_err();
        assert!(
            !invalid_err.to_string().contains(multi_line),
            "{invalid_err}"
        );
    }

    #[test]
    fn the_parakeet_notice_carries_what_cc_by_asks_for() {
        // CC BY 4.0 section 3(a)(1): the creator, a link to the material, a
        // note of any modification, and the licence or a link to it. The file
        // Macalla fetches is a quantised copy, so all four have to be there.
        let (licence, notice) =
            attribution_for("parakeet-tdt-v3").expect("Parakeet must be attributed");
        assert_eq!(licence, "CC-BY-4.0");
        for needed in [
            "NVIDIA",
            "huggingface.co/nvidia/parakeet-tdt-0.6b-v3",
            "handy-computer",
            "quantised",
            "creativecommons.org/licenses/by/4.0",
        ] {
            assert!(notice.contains(needed), "notice lacks {needed:?}: {notice}");
        }
    }

    #[test]
    fn the_embedded_licences_parse_and_carry_the_required_notices() {
        // licenses.json is generated (scripts/gen-licenses.py) and compiled in,
        // so a bad regenerate would ship an empty or broken notices page. Guard
        // the two obligations that named this work: comrak (BSD-2-Clause) and
        // finl_unicode (Unicode-DFS-2016) must each appear with a real
        // copyright line, not the generic SPDX placeholder.
        let families = licenses();
        assert!(!families.is_empty(), "licences failed to parse");

        let bsd2 = families
            .iter()
            .find(|f| f.spdx == "BSD-2-Clause")
            .expect("a BSD-2-Clause family");
        let comrak = bsd2
            .crates
            .iter()
            .find(|c| c.name == "comrak")
            .expect("comrak under BSD-2-Clause");
        assert!(
            comrak.copyright.iter().any(|l| l.contains("Comrak")),
            "comrak lacks its real copyright: {:?}",
            comrak.copyright
        );

        let has_unicode_dfs = families.iter().any(|f| f.spdx == "Unicode-DFS-2016");
        assert!(has_unicode_dfs, "Unicode-DFS-2016 (finl_unicode) missing");

        // The C/C++ libraries vendored inside transcribe-cpp-sys are invisible to
        // cargo-about (not Cargo crates); gen-licenses.py folds them into MIT by
        // hand (VENDORED_MIT). MIT obliges us to carry their copyright, so guard
        // that each is present with its real holder, not dropped on a regenerate.
        let mit = families
            .iter()
            .find(|f| f.spdx == "MIT")
            .expect("an MIT family");
        for (lib, holder) in [("ggml", "ggml authors"), ("miniz", "Rich Geldreich")] {
            let c = mit
                .crates
                .iter()
                .find(|c| c.name == lib)
                .unwrap_or_else(|| panic!("{lib} missing from the MIT family"));
            assert!(
                c.copyright.iter().any(|l| l.contains(holder)),
                "{lib} lacks its real copyright: {:?}",
                c.copyright
            );
        }

        // No family's text is the unfilled SPDX template.
        for f in &families {
            assert!(
                !f.text.contains("<copyright holders>") && !f.text.contains("<owner>"),
                "{} text is the generic placeholder",
                f.spdx
            );
        }
    }

    #[test]
    fn a_not_compiled_in_engine_is_never_reported_ready() {
        let s = status_for(&Availability::NotCompiledIn);
        assert_eq!(s, "not included in this build", "got {s:?}");
    }

    #[test]
    fn an_unavailable_engine_reports_its_reason() {
        let s = status_for(&Availability::Unavailable(
            "no assets installed".to_string(),
        ));
        assert_eq!(s, "no assets installed", "got {s:?}");
    }

    #[test]
    fn a_ready_engine_reports_ready() {
        let s = status_for(&Availability::Ready);
        assert_eq!(s, "ready", "got {s:?}");
    }

    #[test]
    fn the_platform_default_engine_is_listed() {
        // Listed, with an availability -- not necessarily ready. Apple
        // Speech needs the macOS 26 SDK, and a CI runner on an older SDK
        // builds the engine as "not included in this build" by design (see
        // macalla-engines' build.rs). The ready case is the ignored test
        // below, run on a machine that has it.
        let id = macalla_engines::default_engine_id();
        let options = macalla_session::choice::local_transcription_options();
        let found = options.iter().find(|e| e.id == id);
        assert!(found.is_some(), "{id} missing from {options:?}");
    }

    #[test]
    fn every_downloadable_engine_points_at_a_model_the_card_can_show() {
        // The overview joins an engine to its download state by this id, so
        // an id the models list does not have would be a row with no button.
        let ids: Vec<&str> = macalla_engines::models::catalogue()
            .iter()
            .map(|m| m.id)
            .collect();
        for e in macalla_session::choice::local_transcription_options() {
            assert!(!e.group.is_empty(), "{e:?}");
            if let Some(model) = e.model {
                assert!(ids.contains(&model), "{e:?}");
            }
        }
    }

    /// Apple Intelligence is the first cleanup provider on a Mac that has it
    ///, and this crate only has it with its `apple-intelligence`
    /// feature on. It was once missing from the default features, so `tauri
    /// dev` and `tauri build`, which build this crate alone, shipped without
    /// it, and every workspace build hid that. This reads the default feature
    /// list from this crate's own manifest, so it runs in every build, with
    /// default features or without, and fails whenever the defaults leave
    /// Apple Intelligence out.
    #[test]
    fn the_desktop_apps_default_features_include_apple_intelligence() {
        let manifest = include_str!("../Cargo.toml");
        let default = manifest
            .lines()
            .find(|line| line.trim_start().starts_with("default ="))
            .expect("the manifest has a default feature list");
        assert!(
            default.contains("\"apple-intelligence\""),
            "the desktop app's default features must include Apple Intelligence: {default}"
        );
    }

    /// The same for Apple's on-device translation, the Files page's one
    /// translator: `tauri dev` and `tauri build` build this crate alone, so a
    /// default list that leaves it out ships an app that says translating
    /// isn't available. Proving the backend itself is compiled in needs the
    /// system's translation service and the main thread's run loop, so that
    /// is `macalla-translators`' own live test, not one here.
    #[test]
    fn the_desktop_apps_default_features_include_apple_translation() {
        let manifest = include_str!("../Cargo.toml");
        let default = manifest
            .lines()
            .find(|line| line.trim_start().starts_with("default ="))
            .expect("the manifest has a default feature list");
        assert!(
            default.contains("\"translate-apple\""),
            "the desktop app's default features must include Apple translation: {default}"
        );
    }

    /// The feature above, followed all the way to the provider: this build of
    /// the app includes Apple Intelligence, whether or not it is switched on
    /// on this Mac. Ignored like the engine test below, because a build with an
    /// SDK older than macOS 26 leaves the provider out; CI runs it on the
    /// self-hosted runner, building this crate alone.
    #[test]
    #[ignore = "needs the macOS 26 SDK; proves Apple Intelligence is compiled into this crate"]
    #[cfg(all(target_os = "macos", feature = "apple-intelligence"))]
    fn a_default_desktop_build_includes_apple_intelligence() {
        use macalla_session::choice::AppleIntelligenceAvailability;
        let availability = AppleIntelligenceAvailability::now();
        assert_ne!(
            availability,
            AppleIntelligenceAvailability::Unsupported,
            "this build says it does not include Apple Intelligence"
        );
    }

    #[test]
    #[ignore = "needs the macOS 26 SDK; proves the Swift engine links into this crate"]
    #[cfg(target_os = "macos")]
    fn the_default_engine_reports_ready_on_macos() {
        let id = macalla_engines::default_engine_id();
        let options = macalla_session::choice::local_transcription_options();
        let found = options.iter().find(|e| e.id == id);
        assert!(
            found.is_some_and(|e| e.availability.is_ready()),
            "expected {id} ready, got {options:?}"
        );
    }

    #[test]
    fn every_engine_with_a_licence_obligation_is_credited() {
        // Parakeet's CC-BY-4.0 requires attribution once it ships. The screen
        // must carry it whether or not this build compiled the engine in, so
        // nobody has to remember to add it later.
        let about = about_summary();
        assert!(!about.version.is_empty());
        assert_eq!(about.licence, "MIT");
        let parakeet = about
            .attributions
            .iter()
            .find(|a| a.name == "Parakeet")
            .expect("Parakeet is credited");
        assert!(parakeet.licence.contains("CC-BY"), "{parakeet:?}");
        assert!(parakeet.notice.contains("NVIDIA"), "{parakeet:?}");
    }

    #[test]
    fn the_engine_step_names_the_engine_chosen_not_the_default() {
        // Dictation opens the chosen engine, or the default when none is
        // chosen, so the checklist asks about that same one: Whisper chosen
        // on Windows is the engine the line names, ready or not.
        let engines = registry::engines();
        assert!(!engines.is_empty(), "this build has engines");
        for info in engines {
            let step = engine_step(Some(info.id));
            assert!(
                step.detail.contains(info.name),
                "{} chosen, but the step says {:?}",
                info.id,
                step.detail
            );
            assert_eq!(
                step.state == StepState::Done,
                info.availability.is_ready(),
                "{}: done exactly when that engine is ready",
                info.id
            );
        }
        let default = registry::find(registry::default_engine_id()).expect("a default");
        assert!(engine_step(None).detail.contains(default.name));
    }

    #[test]
    fn the_engine_is_a_step_and_on_a_mac_it_is_already_done() {
        // "Apple is there by default on an Apple device?" -- yes, and the
        // checklist says so rather than asking the person to choose.
        let steps = all_steps(None);
        let engine = steps
            .iter()
            .find(|s| s.id == "engine")
            .expect("an engine step");
        assert_eq!(
            steps.last().map(|s| s.id),
            Some("engine"),
            "after the permissions"
        );
        if cfg!(target_os = "macos")
            && registry::find(registry::default_engine_id())
                .is_some_and(|e| e.availability.is_ready())
        {
            assert_eq!(engine.state, StepState::Done);
            assert!(engine.detail.contains("Apple"), "{engine:?}");
        }
    }

    /// The checklist's Try it row waits for the permissions it needs, and on
    /// Windows and Linux there are none (`Permission::required_on`), so that
    /// wait alone let it show on the first launch, before the default model
    /// had downloaded. It must wait for the engine step as well.
    #[test]
    fn try_it_waits_for_the_engine_step() {
        let source = include_str!("../../src/App.tsx");
        let row = source
            .find("<ul className=\"steps steps-try\">")
            .expect("the Try it row");
        let gate_start = source[..row].rfind("*/}").expect("the gate's comment");
        let gate = &source[gate_start..row];
        assert!(gate.contains("\"engine\""), "{gate}");
        assert!(gate.contains("\"required\""), "{gate}");
    }

    #[test]
    fn asking_for_a_step_that_is_not_a_permission_is_an_error_not_a_panic() {
        assert!(request_permission("engine".into()).is_err());
        assert!(open_settings_pane("engine".into()).is_err());
    }

    #[test]
    fn every_step_state_maps_to_a_lowercase_word() {
        for s in step_summaries(None) {
            assert!(
                matches!(s.state.as_str(), "done" | "required" | "optional"),
                "{s:?} has an unmapped state"
            );
        }
    }

    // -------------------------------------------------------- models_overview

    #[test]
    fn without_the_cloud_feature_every_provider_is_not_compiled_in() {
        // Release and dev builds of the desktop app turn on
        // macalla-session's `cloud-transcription` feature by default, and
        // `cargo test --workspace` unifies it on regardless, because
        // macalla-cli's default features include it too. Only
        // `cargo test -p macalla-desktop --lib --no-default-features` (CI's
        // proof that the app is complete without cloud engines) actually compiles it out -- so this asks
        // the session itself what it actually compiled in, rather than
        // assuming either, the same way choice::tests handles it on the
        // session side.
        let store = MemoryStore::new();
        let overview = overview_with(&store, None);
        let compiled_in = macalla_session::choice::cloud_transcription_compiled_in();
        assert_eq!(overview.cloud_transcription_compiled_in, compiled_in);
        let providers: Vec<_> = overview
            .transcription
            .iter()
            .filter(|t| t.source == "provider")
            .collect();
        assert!(!providers.is_empty(), "{:?}", overview.transcription);
        for p in providers {
            if compiled_in {
                assert_ne!(p.status, TranscriptionStatus::NotCompiledIn, "{p:?}");
            } else {
                assert_eq!(p.status, TranscriptionStatus::NotCompiledIn, "{p:?}");
            }
        }
    }

    #[test]
    fn cleanup_providers_are_never_not_compiled_in() {
        // Unlike transcription, the cleanup adapters are always compiled in
        //, so a cleanup provider must never report the
        // transcription-only not-compiled-in state -- there is no such
        // status on CleanupOptionView at all, but a stored key must still
        // report available.
        let store = MemoryStore::new();
        providers::store_key(&store, "openai", "sk-test").unwrap();
        let overview = overview_with(&store, None);
        let openai = overview
            .cleanup
            .iter()
            .find(|c| c.kind == "provider" && c.id == "openai")
            .expect("openai offers cleanup");
        assert!(openai.available, "{openai:?}");
        assert_eq!(openai.key, Some(KeyStateView::Saved));
    }

    #[test]
    fn cleanup_starts_with_off_apple_intelligence_and_the_local_server() {
        let store = MemoryStore::new();
        let overview = overview_with(&store, None);
        assert_eq!(overview.cleanup[0].kind, "off");
        assert_eq!(overview.cleanup[1].kind, "apple-intelligence");
        assert_eq!(overview.cleanup[2].kind, "local-server");
        assert_eq!(overview.cleanup[2].name, "Your own server");
    }

    #[test]
    fn the_overview_carries_the_saved_cleanup_choice() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        history
            .set_cleanup_choice(&crate::history::CleanupChoice::Server {
                address: "http://localhost:11434".to_string(),
                model: "granite4.1:3b".to_string(),
            })
            .unwrap();
        assert_eq!(
            models_overview_with(
                &MemoryStore::new(),
                None,
                None,
                macalla_engines::default_engine_id(),
                history.cleanup_choice().into(),
                None,
            )
            .cleanup_chosen,
            CleanupChoiceView::Server {
                address: "http://localhost:11434".to_string(),
                model: "granite4.1:3b".to_string(),
            }
        );
    }

    // -------------------------------------------------------- cleanup_choose

    #[test]
    fn choosing_off_saves_and_a_stored_server_choice_is_cleared() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        history
            .set_cleanup_choice(&crate::history::CleanupChoice::AppleIntelligence)
            .unwrap();
        cleanup_choose_with(&history, &CleanupChoiceView::Off).unwrap();
        assert_eq!(history.cleanup_choice(), crate::history::CleanupChoice::Off);
    }

    #[test]
    fn choosing_apple_intelligence_validates_against_what_the_system_actually_says() {
        // Same reasoning as choice::tests::apple_intelligence_validates_against_what_the_system_actually_says:
        // ask the system rather than assuming either state.
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let result = cleanup_choose_with(&history, &CleanupChoiceView::AppleIntelligence);
        match macalla_session::choice::AppleIntelligenceAvailability::now().explain() {
            Some(reason) => {
                let error = result.unwrap_err();
                assert!(error.to_string().contains(reason), "{error}");
                assert_eq!(history.cleanup_choice(), crate::history::CleanupChoice::Off);
            }
            None => {
                result.unwrap();
                assert_eq!(
                    history.cleanup_choice(),
                    crate::history::CleanupChoice::AppleIntelligence
                );
            }
        }
    }

    #[test]
    fn a_server_choice_with_an_empty_address_is_refused() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let error = cleanup_choose_with(
            &history,
            &CleanupChoiceView::Server {
                address: "  ".to_string(),
                model: "granite4.1:3b".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "That Enhancement choice cannot be used as it is. Check it on the Models page."
        );
        assert_eq!(history.cleanup_choice(), crate::history::CleanupChoice::Off);
    }

    #[test]
    fn a_server_choice_with_an_empty_model_is_refused() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let error = cleanup_choose_with(
            &history,
            &CleanupChoiceView::Server {
                address: "http://localhost:11434".to_string(),
                model: "".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "That Enhancement choice cannot be used as it is. Check it on the Models page."
        );
    }

    #[test]
    fn a_server_choice_with_an_address_and_model_is_kept() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        cleanup_choose_with(
            &history,
            &CleanupChoiceView::Server {
                address: "http://localhost:11434".to_string(),
                model: "granite4.1:3b".to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            history.cleanup_choice(),
            crate::history::CleanupChoice::Server {
                address: "http://localhost:11434".to_string(),
                model: "granite4.1:3b".to_string(),
            }
        );
    }

    #[test]
    fn a_pool_choice_arrives_from_the_page_in_its_wire_shape() {
        let choice: CleanupChoiceView = serde_json::from_str(
            r#"{"kind":"pool","models":[{"provider":"openai","model":"gpt-test"}]}"#,
        )
        .unwrap();
        assert_eq!(
            choice,
            CleanupChoiceView::Pool {
                models: vec![PoolModelView {
                    provider: "openai".to_string(),
                    model: "gpt-test".to_string(),
                }],
            }
        );
    }

    #[test]
    fn a_pool_choice_is_kept_in_ticked_order() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let models = vec![
            PoolModelView {
                provider: "mistral".to_string(),
                model: "mistral-test".to_string(),
            },
            PoolModelView {
                provider: "openai".to_string(),
                model: "gpt-test".to_string(),
            },
        ];
        cleanup_choose_with(
            &history,
            &CleanupChoiceView::Pool {
                models: models.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            CleanupChoiceView::from(history.cleanup_choice()),
            CleanupChoiceView::Pool { models }
        );
    }

    fn pool_view(models: &[(&str, &str)]) -> CleanupChoiceView {
        CleanupChoiceView::Pool {
            models: models
                .iter()
                .map(|(provider, model)| PoolModelView {
                    provider: provider.to_string(),
                    model: model.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn the_choice_in_force_is_built_again_only_when_the_running_cleanup_is_not_whole() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let switch = Some(crate::dictation::Command::SwitchCleanup);
        let pool = pool_view(&[("openai", "gpt-test")]);
        assert_eq!(
            cleanup_choose_command(&history, &pool, true).unwrap(),
            switch,
            "changed"
        );
        assert_eq!(
            cleanup_choose_command(&history, &pool, true).unwrap(),
            None,
            "unchanged and serving"
        );
        assert_eq!(
            cleanup_choose_command(&history, &pool, false).unwrap(),
            switch,
            "unchanged, but failed, not ready or a member skipped"
        );
        assert!(cleanup_choose_command(&history, &pool_view(&[]), false).is_err());
    }

    #[test]
    fn choosing_off_rewrites_a_stored_pool_that_cannot_be_read() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        history.set_raw_cleanup_for_test("pool", "not json");
        assert_eq!(history.cleanup_choice(), crate::history::CleanupChoice::Off);
        assert_eq!(history.cleanup_choice_as_stored(), None);
        assert!(cleanup_choose_with(&history, &CleanupChoiceView::Off).unwrap());
        assert_eq!(
            history.cleanup_choice_as_stored(),
            Some(crate::history::CleanupChoice::Off)
        );
        assert!(!cleanup_choose_with(&history, &CleanupChoiceView::Off).unwrap());
    }

    #[test]
    fn choosing_off_with_history_unopened_is_no_change() {
        let history = crate::history::HistoryHandle::unopened_for_test();
        assert!(!cleanup_choose_with(&history, &CleanupChoiceView::Off).unwrap());
        assert!(cleanup_choose_with(&history, &pool_view(&[("openai", "gpt-test")])).is_err());
    }

    #[test]
    fn the_choice_already_in_force_is_not_chosen_again() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        assert!(
            !cleanup_choose_with(&history, &CleanupChoiceView::Off).unwrap(),
            "off is where it starts"
        );

        let pool = pool_view(&[("openai", "gpt-test"), ("mistral", "mistral-test")]);
        assert!(
            cleanup_choose_with(&history, &pool).unwrap(),
            "a first pool"
        );
        assert!(
            !cleanup_choose_with(&history, &pool).unwrap(),
            "the same pool"
        );

        let reordered = pool_view(&[("mistral", "mistral-test"), ("openai", "gpt-test")]);
        assert!(
            cleanup_choose_with(&history, &reordered).unwrap(),
            "another order is another failover order"
        );
        assert_eq!(CleanupChoiceView::from(history.cleanup_choice()), reordered);

        let shorter = pool_view(&[("mistral", "mistral-test")]);
        assert!(
            cleanup_choose_with(&history, &shorter).unwrap(),
            "a member fewer"
        );

        let server = CleanupChoiceView::Server {
            address: "http://localhost:11434".to_string(),
            model: "granite4.1:3b".to_string(),
        };
        assert!(cleanup_choose_with(&history, &server).unwrap());
        assert!(!cleanup_choose_with(&history, &server).unwrap());
        let other_model = CleanupChoiceView::Server {
            address: "http://localhost:11434".to_string(),
            model: "qwen3:4b".to_string(),
        };
        assert!(cleanup_choose_with(&history, &other_model).unwrap());
        assert!(cleanup_choose_with(&history, &CleanupChoiceView::Off).unwrap());
    }

    #[test]
    fn an_empty_pool_is_refused() {
        let history = crate::history::HistoryHandle::in_memory_for_test();
        let error = cleanup_choose_with(&history, &CleanupChoiceView::Pool { models: Vec::new() })
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "That Enhancement choice cannot be used as it is. Check it on the Models page."
        );
        assert_eq!(history.cleanup_choice(), crate::history::CleanupChoice::Off);
    }

    #[test]
    fn the_overview_lists_pool_providers_with_a_saved_key_in_table_order() {
        let store = MemoryStore::new();
        for provider in ["mistral", "anthropic", "openrouter"] {
            providers::store_key(&store, provider, "sk-test").unwrap();
        }
        let overview = overview_with(&store, None);
        let ids: Vec<&str> = overview
            .pool_providers
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(ids, vec!["anthropic", "mistral"]);
        assert!(overview.pool_providers.iter().all(|p| !p.name.is_empty()));
    }

    #[test]
    fn pool_models_refuses_a_provider_with_no_saved_key_before_any_fetch() {
        let dir = std::env::temp_dir().join(format!(
            "macalla-desktop-pool-models-test-{}",
            std::process::id()
        ));
        let error = pool_models_with(&MemoryStore::new(), &dir, "anthropic", false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Anthropic needs a key before it can be used. Add one under Providers on the Models page."
        );
        assert!(!dir.join("anthropic-text.json").exists());
    }

    #[test]
    fn the_overview_never_contains_a_stored_key() {
        let store = MemoryStore::new();
        let key = "sk-super-secret-value";
        providers::store_key(&store, "openai", key).unwrap();
        let overview = overview_with(&store, None);
        let json = serde_json::to_string(&overview).unwrap();
        assert!(!json.contains(key), "{json}");
    }

    #[test]
    fn local_transcription_options_takes_no_key_store() {
        // The local list is local engines only; a runtime test with a
        // panicking `SecretStore` fake would be vacuous here -- the function
        // takes none to hand the fake to -- so the type signature itself is
        // the proof: coercing it to a bare `fn` pointer with no
        // `&dyn SecretStore` parameter fails to compile the moment it
        // regains one.
        let _: fn() -> Vec<macalla_engines::registry::EngineInfo> =
            macalla_session::choice::local_transcription_options;
    }

    // ------------------------------------------------------- server_models

    /// A single-request fake server, the same shape used in
    /// `macalla-session::choice`'s own tests -- neither crate can reach the
    /// other's private test-only module, so this is its own copy.
    fn respond_once(status: &str, body: String) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        address
    }

    #[test]
    fn server_models_reports_the_measured_table_and_whether_the_server_is_loopback() {
        let address = respond_once(
            "200 OK",
            serde_json::json!({"object": "list", "data": [
                {"id": "granite4.1:3b", "object": "model", "created": 1, "owned_by": "library"},
            ]})
            .to_string(),
        );
        let view = server_models_with(&address).unwrap();
        assert!(view.loopback, "{address} is on 127.0.0.1");
        let model = &view.models[0];
        assert_eq!(model.id, "granite4.1:3b");
        assert_eq!(
            model.measured,
            Some(MeasuredCleanupView {
                right: 17,
                of: 20,
                machine: "M1 Mac mini, 16 GB".to_string(),
            })
        );
    }

    #[test]
    fn server_models_reports_an_unreachable_address_as_an_error_string() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert!(server_models_with(&address).is_err());
    }

    // -------------------------------------------------------- openrouter_models

    fn temp_cache_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "macalla-desktop-openrouter-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// An unknown `kind` is rejected before anything is fetched -- the "text"
    /// kind is never exercised here at all: it has no feature gate, so a
    /// test that called it would make a real request to openrouter.ai, which
    /// tests must never do. Session- and adapter-level tests already cover
    /// its parsing, filtering and caching against fakes and saved snapshots.
    #[test]
    fn an_unknown_listing_kind_is_rejected_before_any_fetch() {
        let dir = temp_cache_dir("unknown-kind");
        let error = openrouter_models_with(&dir, "audio", false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "That is not a kind of model list Macalla knows about."
        );
    }

    #[test]
    fn transcription_without_the_cloud_feature_says_cloud_transcription_is_not_included() {
        if macalla_session::choice::cloud_transcription_compiled_in() {
            // `cargo test --workspace` unifies macalla-session's `cloud`
            // feature on via macalla-cli's default features; exercising the compiled-in path here would make a
            // real request to openrouter.ai, which tests must never do. Ask
            // the session what it actually compiled in, the same way
            // `without_the_cloud_feature_every_provider_is_not_compiled_in`
            // does, and skip rather than assume either.
            return;
        }
        let dir = temp_cache_dir("transcription-not-compiled-in");
        let error = openrouter_models_with(&dir, "transcription", false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Cloud transcription is not included in this build."
        );
    }
}
