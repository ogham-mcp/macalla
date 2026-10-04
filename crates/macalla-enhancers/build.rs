// SPDX-License-Identifier: MIT
//! Compiles the Swift half of the Apple Intelligence provider into a static
//! library and links it, when the `apple-intelligence` feature is on.
//!
//! The same steps as `crates/macalla-engines/build.rs`, which explains each
//! one, including why the Swift Concurrency link search path is needed.
//! Repeating them was accepted over tying text cleanup to the speech
//! engines; if a third Swift library appears, the two become one helper.

use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

const SWIFT_SOURCES: &[&str] = &["swift/MacallaIntelligence.swift"];

fn main() {
    for src in SWIFT_SOURCES {
        println!("cargo:rerun-if-changed={src}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MACALLA_NO_APPLE_INTELLIGENCE");
    // Declared so the `apple_intelligence` cfg below is not an "unexpected cfg".
    println!("cargo:rustc-check-cfg=cfg(apple_intelligence)");

    if env::var_os("CARGO_FEATURE_APPLE_INTELLIGENCE").is_none() {
        return;
    }
    // The apps turn the feature on for every target, because Cargo features
    // cannot depend on the target. Off macOS there is no FoundationModels, so
    // the build goes on without the provider, which reports itself unsupported,
    // the same as with an SDK older than macOS 26 below.
    if env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "macos" {
        return;
    }

    // FoundationModels is macOS 26. An older SDK builds without the provider,
    // which then reports itself unsupported rather than breaking the build.
    // `MACALLA_NO_APPLE_INTELLIGENCE=1` forces that path.
    if env::var_os("MACALLA_NO_APPLE_INTELLIGENCE").is_some() || !sdk_is_26_or_later() {
        println!(
            "cargo:warning=Apple Intelligence needs the macOS 26 SDK; \
             building without it, and the provider will report itself unsupported."
        );
        return;
    }
    println!("cargo:rustc-cfg=apple_intelligence");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let lib = out_dir.join("libmacallaintelligence.a");
    let _ = fs::remove_file(&lib);

    let mut cmd = Command::new("swiftc");
    cmd.args([
        "-emit-library",
        "-static",
        "-parse-as-library",
        "-O",
        "-swift-version",
        "6",
        // Named, so its symbols cannot collide with the speech engine's Swift
        // library when both are linked into one binary.
        "-module-name",
        "MacallaIntelligence",
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
        "swiftc failed building the Apple Intelligence provider"
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=macallaintelligence");
    if let Some(sdk) = sdk_path() {
        println!(
            "cargo:rustc-link-search=native={}/usr/lib/swift",
            sdk.display()
        );
    }
    println!("cargo:rustc-link-lib=framework=FoundationModels");
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
