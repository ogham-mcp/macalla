// SPDX-License-Identifier: MIT
//! Compiles the Swift half of Apple's on-device translation backend into a
//! static library and links it, when the `translate-apple` feature is on.
//!
//! The same steps as `crates/macalla-enhancers/build.rs`, and through it
//! `crates/macalla-engines/build.rs`, which explains each one, including why
//! the Swift Concurrency link search path is needed. `swiftc` straight, not
//! `swift build`, whose default build system fails for every package on the
//! reference toolchain. This is the third copy of these steps: the helper
//! that comment in the enhancers' build.rs promises is now due, and is left
//! for its own change so this one touches no other crate's build.

use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

const SWIFT_SOURCES: &[&str] = &["swift/MacallaTranslation.swift"];

fn main() {
    for src in SWIFT_SOURCES {
        println!("cargo:rerun-if-changed={src}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MACALLA_NO_APPLE_TRANSLATION");
    // Declared so the `translate_apple` cfg below is not an "unexpected cfg".
    println!("cargo:rustc-check-cfg=cfg(translate_apple)");

    if env::var_os("CARGO_FEATURE_TRANSLATE_APPLE").is_none() {
        return;
    }
    // The apps turn the feature on for every target, because Cargo features
    // cannot depend on the target. Off macOS there is no Translation framework,
    // so the build goes on without the backend, which reports itself
    // unavailable, the same as with an SDK older than macOS 26 below.
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "macos" {
        return;
    }

    // `TranslationSession(installedSource:target:)` is macOS 26. An older SDK
    // builds without the backend, which then reports itself unavailable
    // rather than breaking the build.
    // `MACALLA_NO_APPLE_TRANSLATION=1` forces that path.
    if env::var_os("MACALLA_NO_APPLE_TRANSLATION").is_some() || !sdk_is_26_or_later() {
        println!(
            "cargo:warning=Apple translation needs the macOS 26 SDK; \
             building without it, and the backend will report itself unavailable."
        );
        return;
    }
    println!("cargo:rustc-cfg=translate_apple");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let lib = out_dir.join("libmacallatranslation.a");
    let _ = fs::remove_file(&lib);

    let mut cmd = Command::new("swiftc");
    cmd.args([
        "-emit-library",
        "-static",
        "-parse-as-library",
        "-O",
        "-swift-version",
        "6",
        // Named, so its symbols cannot collide with the other Swift libraries
        // linked into the same binary.
        "-module-name",
        "MacallaTranslation",
    ])
    .arg("-o")
    .arg(&lib)
    .args(SWIFT_SOURCES);
    if let Ok(arch) = env::var("CARGO_CFG_TARGET_ARCH") {
        let triple = match arch.as_str() {
            "aarch64" => Some("arm64-apple-macosx26.0"),
            "x86_64" => Some("x86_64-apple-macosx26.0"),
            _ => None,
        };
        if let Some(triple) = triple {
            cmd.args(["-target", triple]);
        }
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("running swiftc: {e}. Is the Swift toolchain installed?"));
    assert!(
        status.success(),
        "swiftc failed building the Apple translation backend"
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=macallatranslation");
    if let Some(sdk) = sdk_path() {
        println!(
            "cargo:rustc-link-search=native={}/usr/lib/swift",
            sdk.display()
        );
    }
    println!("cargo:rustc-link-lib=framework=Translation");
    println!("cargo:rustc-link-lib=framework=Foundation");
}

fn sdk_is_26_or_later() -> bool {
    let Ok(out) = Command::new("xcrun").arg("--show-sdk-version").output() else {
        return false;
    };
    out.status.success()
        && String::from_utf8_lossy(&out.stdout)
            .trim()
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 26)
}

fn sdk_path() -> Option<PathBuf> {
    let out = Command::new("xcrun").arg("--show-sdk-path").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    path.is_dir().then_some(path)
}
