// SPDX-License-Identifier: MIT
//! Which engines exist, whether each can run here, and how to open one.
//!
//! Every trigger asks this: the CLI, the tray, and later the agent interface.
//! Before it existed the CLI carried its own `cfg(target_os)` block, the
//! availability check, the construction and the error text -- and the tray was
//! about to grow a second copy that could disagree with the first about which
//! engine was running and why it was unavailable (the core is callable without the hotkey or the UI).
//!
//! Deliberately more than a factory. Choosing the platform's engine and
//! deciding whether it is usable are decisions that drive onboarding copy and
//! the settings screen, not wiring. What IS wiring -- picking an id, handling
//! the error -- stays with the caller.

use crate::models::ModelSpec;
use macalla_core::stt::SttEngine;
use macalla_core::{Error, Result};

/// Whether an engine can be used on this machine, right now.
///
/// Four states, not a bool, because they need different answers from the UI:
/// offer it, offer a download, say why not, or do not offer it at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Usable now.
    Ready,
    /// Not compiled into this build. Nothing the user can do.
    NotCompiledIn,
    /// Compiled in and would run here once its model is fetched. The string
    /// names what to fetch and how big it is, for the button.
    NeedsDownload(String),
    /// Compiled in but unusable here, with a reason fit to show a person.
    Unavailable(String),
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready)
    }
}

/// What a trigger needs to know about one engine without opening it.
#[derive(Debug, Clone)]
pub struct EngineInfo {
    pub id: &'static str,
    /// For a settings card. Plain English, no version numbers.
    pub name: &'static str,
    /// The heading the card lists it under: who made it.
    pub group: &'static str,
    /// The catalogue id of the model it runs on, or `None` for an engine
    /// that needs no download.
    pub model: Option<&'static str>,
    /// One sentence on what it is good for, so a person can choose without
    /// reading the website.
    pub blurb: &'static str,
    pub availability: Availability,
    /// BCP-47 tags usable right now. Empty unless [`Availability::Ready`].
    pub languages: Vec<String>,
    /// Tags the engine supports but which need downloading first. This is what
    /// onboarding offers; it is not an error.
    pub downloadable: Vec<String>,
}

/// Every engine this build knows about, whether or not it can run.
///
/// Includes engines that are not compiled in, so the settings screen can
/// describe an engine it cannot open rather than silently omitting it.
pub fn engines() -> Vec<EngineInfo> {
    let mut all = vec![apple_info()];
    all.extend(crate::models::catalogue().iter().map(downloaded_info));
    all
}

fn apple_info() -> EngineInfo {
    let (availability, languages, downloadable) = apple_state();
    EngineInfo {
        id: crate::apple::ENGINE_ID,
        name: "Apple Speech",
        group: "Apple",
        model: None,
        blurb: "Built into macOS. Nothing to download, shows words as you speak, \
                and knows the languages installed on this Mac.",
        availability,
        languages,
        downloadable,
    }
}

/// All the knowledge about whether Apple Speech can run, in one place.
///
/// Three ways it cannot, and they need different words: wrong platform, not
/// compiled in, or compiled in on macOS 26 with no speech assets downloaded --
/// which is a fresh Mac, and the only one of the three the user can fix.
fn apple_state() -> (Availability, Vec<String>, Vec<String>) {
    if !cfg!(target_os = "macos") {
        return (
            Availability::Unavailable(format!(
                "Apple Speech is macOS only, and this is {}",
                std::env::consts::OS
            )),
            Vec::new(),
            Vec::new(),
        );
    }
    if !crate::apple::is_available() {
        // Either the feature is off, or the SDK was too old to build the
        // bridge, or the OS reports the engine unusable. From the caller's
        // side these are the same: it cannot be used and no download fixes it.
        return (Availability::NotCompiledIn, Vec::new(), Vec::new());
    }
    let installed = crate::apple::installed_locales();
    if installed.is_empty() {
        let supported = crate::apple::supported_locales();
        return (
            Availability::Unavailable(
                "no speech assets are installed for any language yet".to_string(),
            ),
            Vec::new(),
            supported,
        );
    }
    let downloadable = crate::apple::supported_locales()
        .into_iter()
        .filter(|l| !installed.contains(l))
        .collect();
    (Availability::Ready, installed, downloadable)
}

/// An engine that runs on a downloaded model. What its card says comes from
/// the model's catalogue entry, so adding a model on transcribe.cpp is one
/// entry there and nothing here.
fn downloaded_info(spec: &'static ModelSpec) -> EngineInfo {
    let (availability, languages) = downloaded_state(spec);
    EngineInfo {
        id: spec.engine,
        name: spec.engine_name,
        group: spec.group,
        model: Some(spec.id),
        blurb: spec.blurb,
        availability,
        languages,
        downloadable: Vec::new(),
    }
}

/// Ready when the model file is in the store; a download otherwise. The
/// store is looked at on every call because a download can finish while the
/// app is open and the card must follow.
#[cfg(feature = "engine-transcribe")]
fn downloaded_state(spec: &ModelSpec) -> (Availability, Vec<String>) {
    if crate::models::Store::shared().installed(spec).is_some() {
        return (
            Availability::Ready,
            spec.languages.iter().map(|l| l.to_string()).collect(),
        );
    }
    (
        Availability::NeedsDownload(format!("{}, {} MB", spec.name, spec.megabytes())),
        Vec::new(),
    )
}

#[cfg(not(feature = "engine-transcribe"))]
fn downloaded_state(_spec: &ModelSpec) -> (Availability, Vec<String>) {
    (Availability::NotCompiledIn, Vec::new())
}

/// Open an engine on a model in the store, once the language is known to be
/// one it speaks.
#[cfg(feature = "engine-transcribe")]
fn open_downloaded(spec: &'static ModelSpec, language: &str) -> Result<Box<dyn SttEngine>> {
    if spec.language_for(language).is_none() {
        return Err(Error::Stt {
            engine: "registry",
            message: format!(
                "{} does not speak {language}. It speaks: {}",
                spec.engine,
                spec.languages.join(", ")
            ),
        });
    }
    let path = crate::models::Store::shared()
        .installed(spec)
        .ok_or_else(|| Error::Model(format!("{} is not in the store", spec.name)))?;
    crate::transcribe::open(spec, &path)
}

/// The engine preferred on this platform when nothing has been configured.
pub fn default_engine_id() -> &'static str {
    crate::default_engine_id()
}

/// Look one up by id.
pub fn find(id: &str) -> Option<EngineInfo> {
    engines().into_iter().find(|e| e.id == id)
}

/// Open an engine, or say why not.
pub fn open(id: &str, language: &str) -> Result<Box<dyn SttEngine>> {
    let Some(info) = find(id) else {
        let known: Vec<&str> = engines().iter().map(|e| e.id).collect();
        return Err(Error::Stt {
            engine: "registry",
            message: format!(
                "no engine called {id:?}. This build has: {}",
                known.join(", ")
            ),
        });
    };

    // Refuse at open, not at the first dictation -- by then the user has
    // already spoken, and losing what they said to a knowable failure is the
    // worst version of this error.
    match &info.availability {
        Availability::Ready => {}
        Availability::NotCompiledIn => {
            return Err(Error::Stt {
                engine: "registry",
                message: format!("{id} is not compiled into this build"),
            })
        }
        Availability::NeedsDownload(what) => {
            return Err(Error::Stt {
                engine: "registry",
                message: format!("{id} needs its model first: {what}. Download it on the Engine card in Macalla, or with `macalla models download`."),
            })
        }
        Availability::Unavailable(reason) => {
            return Err(Error::Stt {
                engine: "registry",
                message: format!("{id} cannot run here: {reason}"),
            })
        }
    }

    if id == crate::apple::ENGINE_ID {
        return Ok(Box::new(crate::apple::AppleEngine::new(language)));
    }
    let model = crate::models::catalogue().iter().find(|m| m.engine == id);
    match model {
        #[cfg(feature = "engine-transcribe")]
        Some(spec) => open_downloaded(spec, language),
        // Ready but unconstructable would be a bug in this table, so say that
        // rather than something vaguer.
        _ => Err(Error::Stt {
            engine: "registry",
            message: format!("{id} reports itself ready but has no constructor here"),
        }),
    }
}

/// The Engine card, or `None` when there is nothing to say.
///
/// Shown **only when the chosen language is supported but not
/// installed**. English and German are pre-installed on macOS and skip it
/// entirely; Spanish and Italian do not. A card that is always present teaches
/// people to ignore cards.
///
/// It is OPTIONAL, never blocking: not having Spanish downloaded is not a
/// reason to refuse to dictate in English.
pub fn engine_step(language: &str) -> Option<macalla_core::onboarding::Step> {
    use macalla_core::onboarding::Step;

    let info = find(default_engine_id())?;
    match &info.availability {
        Availability::Ready => {
            // Ready, and the language is usable: nothing to say.
            if info.languages.iter().any(|l| l == language) {
                return None;
            }
            if info.downloadable.iter().any(|l| l == language) {
                return Some(Step::optional(
                    "engine-assets",
                    "Language",
                    format!("{language} is supported but not downloaded yet."),
                    "Download it on the Engine card.",
                ));
            }
            Some(Step::optional(
                "engine-assets",
                "Language",
                format!("{language} is not one this engine speaks."),
                format!("Pick one of: {}.", info.languages.join(", ")),
            ))
        }
        Availability::NotCompiledIn => Some(Step::optional(
            "engine-assets",
            "Engine",
            "No speech engine is built into this copy.",
            "Install a build for this platform.",
        )),
        // The platform's engine with nothing to run on. Required, not
        // optional: without it there is no dictation at all.
        Availability::NeedsDownload(what) => Some(Step::required(
            "engine-model",
            "Speech model",
            format!("To turn your voice into text, on this machine. {what}."),
            "Download it on the Engine card.",
        )),
        Availability::Unavailable(reason) => Some(Step::optional(
            "engine-assets",
            "Engine",
            format!("The speech engine cannot run: {reason}."),
            "Download a language on the Engine card.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_installed_language_produces_no_engine_card() {
        // The card appears only when it has something to say.
        // A card that is always there teaches people to ignore cards.
        let Some(info) = find(default_engine_id()) else {
            return;
        };
        if let Some(installed) = info.languages.first() {
            assert_eq!(
                engine_step(installed),
                None,
                "{installed} is installed, so there is nothing to show"
            );
        }
    }

    #[test]
    fn a_downloadable_language_produces_an_optional_card() {
        use macalla_core::onboarding::StepState;
        let Some(info) = find(default_engine_id()) else {
            return;
        };
        let Some(missing) = info.downloadable.first() else {
            println!("skipped: nothing downloadable on this machine");
            return;
        };
        let step = engine_step(missing).expect("a downloadable language needs a card");
        assert_eq!(
            step.state,
            StepState::Optional,
            "not having {missing} must not block dictating in a language you do have"
        );
        assert!(step.detail.contains(missing), "got: {}", step.detail);
    }

    #[test]
    fn the_platform_default_is_one_of_the_listed_engines() {
        let ids: Vec<&str> = engines().iter().map(|e| e.id).collect();
        assert!(
            ids.contains(&default_engine_id()),
            "default {} is not in {ids:?}",
            default_engine_id()
        );
    }

    #[test]
    fn engines_that_are_not_compiled_in_are_still_listed() {
        // A settings screen that omits an engine cannot explain it. Every
        // build knows the whole catalogue; availability is the variable.
        assert!(
            engines().len() >= 2,
            "expected Apple and Parakeet in the catalogue, got {:?}",
            engines().iter().map(|e| e.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_engine_has_a_heading_and_a_downloaded_one_names_its_model() {
        for e in engines() {
            assert!(!e.group.is_empty(), "{} has no heading", e.id);
            if let Some(model) = e.model {
                let spec = crate::models::find(model).expect("a model the catalogue has");
                assert_eq!(spec.engine, e.id, "{model} runs a different engine");
            }
        }
    }

    #[test]
    fn every_engine_has_a_name_a_human_could_read() {
        for e in engines() {
            assert!(!e.name.is_empty(), "{} has no display name", e.id);
            assert!(
                !e.name.contains('-') || e.name.contains(' '),
                "{:?} looks like an id, not a name",
                e.name
            );
        }
    }

    #[test]
    fn an_unusable_engine_offers_no_languages() {
        // Offering a language on an engine that cannot run is offering a
        // failure, which is the same rule EngineCaps::languages follows.
        for e in engines() {
            if !e.availability.is_ready() {
                assert!(
                    e.languages.is_empty(),
                    "{} is {:?} but advertises {:?}",
                    e.id,
                    e.availability,
                    e.languages
                );
            }
        }
    }

    #[test]
    fn an_unavailable_engine_gives_a_reason_not_a_code() {
        for e in engines() {
            if let Availability::Unavailable(reason) = &e.availability {
                assert!(
                    reason.split_whitespace().count() >= 4,
                    "{} says {reason:?}, which is not an explanation",
                    e.id
                );
            }
        }
    }

    #[test]
    fn an_unknown_id_is_refused_and_the_message_names_it() {
        let message = match open("no-such-engine", "en-US") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("an unknown id must not open an engine"),
        };
        assert!(message.contains("no-such-engine"), "got: {message}");
        // And says what IS available, so the user can correct themselves.
        assert!(
            message.contains(default_engine_id()),
            "the error should list what exists: {message}"
        );
    }

    #[test]
    fn an_engine_that_cannot_run_here_refuses_rather_than_returning_a_dud() {
        // Opening an unavailable engine must fail at open, not at the first
        // dictation, where the user has already spoken.
        for e in engines() {
            if e.availability.is_ready() {
                continue;
            }
            let err = open(e.id, "en-US")
                .err()
                .unwrap_or_else(|| panic!("{} is {:?} but opened", e.id, e.availability));
            assert!(err.to_string().contains(e.id), "got: {err}");
        }
    }

    #[test]
    fn the_default_opens_wherever_it_reports_itself_ready() {
        let Some(info) = find(default_engine_id()) else {
            panic!("the default is not in the catalogue");
        };
        if info.availability.is_ready() {
            let engine = open(default_engine_id(), "en-US").expect("the default should open");
            assert_eq!(engine.id(), default_engine_id());
        } else {
            println!("skipped: default is {:?} here", info.availability);
        }
    }
}
