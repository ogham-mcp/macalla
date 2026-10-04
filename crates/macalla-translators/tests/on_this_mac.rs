// SPDX-License-Identifier: MIT
//! Apple's backend for real on this Mac: English into German, through the
//! core's `translate_all`, with every rule it holds a backend to. Synthetic
//! sentences only.
//!
//! Its own `main` (`harness = false`), because the Translation framework
//! answers only while the main thread serves its run loop
//! (`apple::serve_main_thread_until`), and the test harness's main thread
//! never does. Does nothing unless `MACALLA_TRANSLATE_LIVE=1` is set, so CI
//! never depends on the system's translation service or on which languages a
//! runner has:
//!
//! ```sh
//! MACALLA_TRANSLATE_LIVE=1 cargo test -p macalla-translators \
//!     --features translate-apple --test on_this_mac
//! ```

use macalla_core::translate::{translate_all, TranslateError, Translator, Unavailable};
use macalla_translators::apple::{serve_main_thread_until, AppleTranslation};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

fn main() {
    if std::env::var_os("MACALLA_TRANSLATE_LIVE").is_none() {
        eprintln!("on_this_mac: skipped; set MACALLA_TRANSLATE_LIVE=1 to ask the system");
        return;
    }
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let outcome = std::panic::catch_unwind(run);
            done.store(true, Ordering::Release);
            outcome
        });
        serve_main_thread_until(&done);
        if let Err(panic) = worker.join().expect("the worker joins") {
            std::panic::resume_unwind(panic);
        }
    });
    eprintln!("on_this_mac: passed");
}

fn run() {
    let backend = AppleTranslation::new();
    let started = Instant::now();
    let languages = backend.languages();
    eprintln!(
        "{} languages listed in {} ms; ga listed: {}",
        languages.len(),
        started.elapsed().as_millis(),
        languages.iter().any(|l| l == "ga")
    );
    if let Err(reason) = backend.availability("en-US", "de") {
        eprintln!("skipped: en-US into de is {reason:?}");
        return;
    }
    let texts: Vec<String> = [
        "Good morning, everyone, let's start the weekly platform sync.",
        "",
        "We moved the network modules to Terragrunt so each environment has its own state.",
        "Can someone take the review before Thursday?",
        "I ran kubectl describe on the pods and the memory limit was hit.",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let started = Instant::now();
    let mut seen = Vec::new();
    let out = translate_all(
        &backend,
        "en-US",
        "de",
        &texts,
        &mut |p| seen.push((p.done, p.of, started.elapsed().as_millis())),
        &AtomicBool::new(false),
    )
    .expect("a translation");
    eprintln!("progress (done, of, ms since start): {seen:?}");
    for (source, line) in texts.iter().zip(&out) {
        eprintln!("  {source}\n    -> {line}");
    }
    assert_eq!(out.len(), texts.len());
    assert_eq!(out[1], "", "a blank text stays blank");
    assert!(out[2].contains("Terragrunt"), "{}", out[2]);
    assert!(out[4].contains("kubectl"), "{}", out[4]);
    assert_ne!(out[0], texts[0]);

    let irish = backend.availability("en-US", "ga");
    eprintln!("en-US into ga: {irish:?}");
    assert!(matches!(irish, Err(Unavailable::UnsupportedPair { .. })));

    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let stopped = translate_all(
        &backend,
        "en-US",
        "de",
        &texts,
        &mut |p| {
            if p.done >= 2 {
                stop.store(true, Ordering::Relaxed);
            }
        },
        &stop,
    );
    eprintln!(
        "stopped after the first answer: {stopped:?} in {} ms",
        started.elapsed().as_millis()
    );
    assert_eq!(stopped, Err(TranslateError::Stopped));
}
