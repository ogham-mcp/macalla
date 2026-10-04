// SPDX-License-Identifier: MIT
//! The web pages the About screen links to. The page names a link by id and
//! never sends a URL, so nothing it does can make Macalla open an address
//! that is not listed here.

use serde::Serialize;

use crate::about_words;
use crate::command_error::{CommandError, OrSay};

/// Every page Macalla links to: an id the page sends back, the address, and
/// its label from the catalogue.
const LINKS: &[(&str, &str, &str)] = &[
    ("website", "https://macalla.dev/", about_words::WEBSITE),
    ("help", "https://macalla.dev/docs/", about_words::HELP),
];

/// One link, as the About screen draws it. The address is shown, so the
/// reader can see where it goes before following it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Link {
    pub id: &'static str,
    pub label: &'static str,
    pub url: &'static str,
}

pub(crate) fn all() -> Vec<Link> {
    LINKS
        .iter()
        .map(|&(id, url, label)| Link { id, label, url })
        .collect()
}

/// Open one of [`LINKS`] in the default browser.
#[tauri::command]
pub fn open_link(id: String) -> Result<(), CommandError> {
    open(&id)
}

/// [`open_link`] for Rust callers: the macOS menu bar's Help items.
pub(crate) fn open(id: &str) -> Result<(), CommandError> {
    let url = LINKS
        .iter()
        .find(|(known, _, _)| *known == id)
        .map(|&(_, url, _)| url)
        .ok_or_else(|| format!("{id:?} is not a link Macalla knows"))?;
    let status = browser(url).status().or_say(about_words::COULD_NOT_OPEN)?;
    if !status.success() {
        tracing::warn!(%status, url, "the browser opener exited without success");
        return Err(about_words::COULD_NOT_OPEN.into());
    }
    Ok(())
}

/// The system's own way to open an address. The address is always one of
/// [`LINKS`], never text from the page.
fn browser(url: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(url);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_link_is_https_on_the_website() {
        for link in all() {
            assert!(link.url.starts_with("https://macalla.dev/"), "{link:?}");
            assert!(!link.label.is_empty(), "{link:?}");
        }
    }

    #[test]
    fn an_unknown_id_is_refused_without_opening_anything() {
        let err = open_link("https://example.com".to_string()).unwrap_err();
        assert!(err.to_string().contains("not a link"), "{err}");
    }
}
