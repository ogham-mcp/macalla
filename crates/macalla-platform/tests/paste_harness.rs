// SPDX-License-Identifier: MIT
//! The one test that proves text actually lands in another application.
//!
//! Every other test of the paste path uses a fake clipboard and a fake key
//! sender. Those prove the ORDER -- save, write, chord, wait, restore -- and
//! prove nothing about whether anything arrives, which is the only part the
//! user experiences.
//!
//! ```text
//! cargo test -p macalla-platform --test paste_harness -- --ignored --nocapture
//! ```
//!
//! IGNORED, AND IT CANNOT BE OTHERWISE. The design called for paste correctness
//! to be "asserted in CI, not by hand". That is not achievable: a synthetic Cmd+V
//! into a focused window needs a logged-in GUI session with an Accessibility
//! grant, and a CI runner has neither. What CI can assert is everything up to
//! the point the OS is involved, which the unit tests already do. This is the
//! last mile and it is run by a person on a real desktop.
//!
//! Needs: macOS, a desktop session, and Accessibility granted to whatever is
//! running the test (the terminal -- an unsigned test binary has no TCC
//! identity of its own).

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What we paste. Deliberately awkward: a dictionary term whose case must
/// survive, an apostrophe, a comma, and a non-ASCII character, because the
/// clipboard round-trip is where encoding problems show up.
const PAYLOAD: &str = "kubectl applied the VNet -- it's ready, naïvely.";

fn scratch() -> PathBuf {
    std::env::temp_dir().join(format!("macalla-paste-{}", std::process::id()))
}

fn build_harness(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/paste-harness/main.swift")
        .canonicalize()
        .expect("the harness source should exist at fixtures/paste-harness/main.swift");
    // A BUNDLE, not a bare executable. A loose binary cannot reliably become
    // frontmost on modern macOS, and a harness that is not frontmost does not
    // fail politely -- the synthetic Cmd+V lands in whatever window the tester
    // was looking at instead.
    let bundle = dir.join("PasteHarness.app");
    let macos = bundle.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).expect("bundle layout");
    std::fs::write(
        bundle.join("Contents/Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>PasteHarness</string>
  <key>CFBundleIdentifier</key><string>dev.macalla.pasteharness</string>
  <key>CFBundleExecutable</key><string>PasteHarness</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><false/>
</dict></plist>
"#,
    )
    .expect("Info.plist");

    let binary = macos.join("PasteHarness");
    let status = std::process::Command::new("swiftc")
        .args(["-O", "-o"])
        .arg(&binary)
        .arg(&source)
        .status()
        .expect("run swiftc");
    assert!(status.success(), "the harness did not compile");
    binary
}

/// Wait for a file to appear, or give up.
fn wait_for(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
#[ignore = "needs a desktop session and Accessibility"]
fn pasted_text_arrives_intact_in_another_application() {
    use macalla_platform::{Grant, Permission};

    // Fail with the reason rather than with a mystery. Accessibility is what
    // enigo needs to synthesise the chord, and without it BOTH the paste and
    // the typing fallback fail -- there is no degraded path to observe.
    let grant = Permission::Accessibility.granted();
    assert_eq!(
        grant,
        Grant::Granted,
        "Accessibility is not granted, so no keystroke can be synthesised and \
         neither the paste nor the typing fallback can work. Grant it to your \
         TERMINAL (an unsigned test binary has no TCC identity of its own) in \
         System Settings > {}. If no prompt ever appeared, it was denied once \
         and macOS will not ask again: `tccutil reset Accessibility \
         com.googlecode.iterm2` clears that.",
        Permission::Accessibility.settings_pane()
    );

    // Printed, not asserted. Secure Event Input does NOT stop a posted paste
    // -- measured on macOS 27 on 2026-09-18, pastes into a terminal with
    // Secure Keyboard Entry on arrived intact -- so a run under it is a valid
    // run. It is still the condition the result was obtained under, and the
    // next person to read this output should not have to guess it.
    println!(
        "secure input: {:?} (does not block a posted paste; it hides keys from event taps)",
        macalla_platform::secure_input_enabled()
    );

    let dir = scratch();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let ready = dir.join("ready");
    let output = dir.join("received");
    let _ = std::fs::remove_file(&ready);
    let _ = std::fs::remove_file(&output);

    let binary = build_harness(&dir);
    let mut child = std::process::Command::new(&binary)
        .arg(&ready)
        .arg(&output)
        .arg("20")
        .spawn()
        .expect("launch the harness");

    assert!(
        wait_for(&ready, Duration::from_secs(15)),
        "the harness never became frontmost, so it never signalled ready. It \
         refuses to signal otherwise, deliberately: a paste aimed at an \
         unfocused harness lands in whatever window you were looking at."
    );
    // The window is up; give the window server a moment to make it key before
    // aiming keystrokes at it.
    std::thread::sleep(Duration::from_millis(400));

    let before = std::process::Command::new("pbpaste")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string());

    let outcome = macalla_platform::paste::system_inserter()
        .expect("open the clipboard and input device")
        .insert(PAYLOAD);

    let arrived = wait_for(&output, Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();

    let outcome = outcome.expect("insert");
    println!("outcome: {outcome:?}");
    assert!(arrived, "nothing was pasted into the harness window");

    let received = std::fs::read_to_string(&output).expect("read what arrived");
    assert_eq!(
        received.trim(),
        PAYLOAD,
        "what arrived is not what was sent"
    );

    // And the rule the whole design is built around: the clipboard the user
    // had is the clipboard they get back.
    if let Some(before) = before {
        let after = std::process::Command::new("pbpaste")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .expect("pbpaste");
        assert_eq!(
            after, before,
            "the clipboard was not restored -- paste must save and restore it"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
