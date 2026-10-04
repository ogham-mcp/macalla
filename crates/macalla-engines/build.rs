// SPDX-License-Identifier: MIT
//! Compiles the Swift half of `engine-apple` into a static library and links it.
//!
//! Verified 2026-09-08 on Command Line Tools 26.5 with Swift 6.3.3, no Xcode:
//! `swiftc -emit-library -static` produces an archive that rustc links with no
//! extra flags, resolving the Swift runtime against `/usr/lib/swift`, which the
//! OS has shipped since 10.14.4. Nothing to bundle.

use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};

const SWIFT_SOURCES: &[&str] = &["swift/MacallaApple.swift"];

fn main() {
    for src in SWIFT_SOURCES {
        println!("cargo:rerun-if-changed={src}");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=MACALLA_NO_APPLE_SPEECH");
    // Declared so the `apple_speech` cfg below is not an "unexpected cfg".
    println!("cargo:rustc-check-cfg=cfg(apple_speech)");

    if env::var_os("CARGO_FEATURE_ENGINE_APPLE").is_none() {
        return;
    }
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "macos" {
        panic!("the engine-apple feature is macOS only, but the target is {target_os}");
    }

    // SpeechTranscriber is macOS 26. An older SDK cannot compile the Swift at
    // all, so the feature degrades to "engine not available" rather than
    // breaking the build -- which is what a user on macOS 15 wants, and what
    // lets CI run this job on whatever the runner image happens to be.
    if !speech_sdk_available() {
        println!(
            "cargo:warning=Apple SpeechTranscriber needs the macOS 26 SDK; \
             building without it. `macalla doctor` will report the engine as unavailable."
        );
        return;
    }
    println!("cargo:rustc-cfg=apple_speech");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let lib = out_dir.join("libmacallaapple.a");
    let _ = fs::remove_file(&lib);

    let mut cmd = Command::new("swiftc");
    cmd.args([
        "-emit-library",
        "-static",
        // No `@main` here; this is a library of C entry points.
        "-parse-as-library",
        "-O",
        "-swift-version",
        "6",
    ])
    .arg("-o")
    .arg(&lib)
    .args(SWIFT_SOURCES);

    // Match what cargo is building for, so a cross build does not silently
    // produce a host-architecture archive.
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
    assert!(status.success(), "swiftc failed building the Apple engine");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=macallaapple");

    // The archive asks the linker for -lswiftCore, -lswift_Concurrency and
    // friends through LC_LINKER_OPTION. rustc does not search the SDK's Swift
    // directory, and without it the linker resolves swift_Concurrency to the
    // BACK-DEPLOYMENT copy, whose install name is `@rpath/...` -- so the binary
    // builds and then dies at startup with "Library not loaded". The SDK stub
    // has the absolute `/usr/lib/swift/...` install name, which is what we want,
    // since the OS has shipped these since macOS 12.
    //
    // A trivial Swift function does not hit this. It only appears once the
    // bridge uses Swift Concurrency, which the streaming API forces.
    if let Some(sdk) = sdk_path() {
        println!(
            "cargo:rustc-link-search=native={}/usr/lib/swift",
            sdk.display()
        );
    }
    // The runtime search path lives in .cargo/config.toml, NOT here. Emitting
    // it from both produced `ld: duplicate -rpath '/usr/lib/swift' ignored` on
    // every link. config.toml is the one that has to exist, because
    // cargo:rustc-link-arg does not propagate to a binary that merely depends
    // on this crate -- which is how `macalla doctor` died at startup once.
    println!("cargo:rustc-link-lib=framework=Speech");
    println!("cargo:rustc-link-lib=framework=AVFoundation");
    println!("cargo:rustc-link-lib=framework=CoreMedia");
    println!("cargo:rustc-link-lib=framework=Foundation");
}

/// Whether the SDK on this machine has SpeechTranscriber, which arrived in
/// macOS 26. `MACALLA_NO_APPLE_SPEECH=1` forces "no", which is how the
/// degraded path gets exercised without an old machine.
fn speech_sdk_available() -> bool {
    if env::var_os("MACALLA_NO_APPLE_SPEECH").is_some() {
        return false;
    }
    let Ok(out) = Command::new("xcrun").arg("--show-sdk-version").output() else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let version = String::from_utf8_lossy(&out.stdout);
    version
        .trim()
        .split('.')
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 26)
}

/// The active SDK, via `xcrun`. `None` if it cannot be determined, in which
/// case the link is left to the defaults and will say so loudly.
fn sdk_path() -> Option<PathBuf> {
    let out = Command::new("xcrun").arg("--show-sdk-path").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = PathBuf::from(path.trim());
    path.is_dir().then_some(path)
}
