// SPDX-License-Identifier: MIT
//! Tier 0 versus Tier 1, on the same fixtures, scored the same way.
//!
//! This is the comparison the 0.1 release requires as an exit criterion and
//! that nothing had actually run: every earlier measurement compared models to
//! each other, or Tier 1 to the raw transcript. The question that decides
//! whether Tier 1 earns its place on the latency-critical path is whether it
//! beats deterministic cleanup.
//!
//! Tier 1 output is read from JSON produced by the bench harness, so this
//! binary needs no network.

use macalla_core::enhance::Tier0;
use macalla_core::tier0::Tier0Rules;
use std::collections::HashSet;

fn norm(t: &str) -> Vec<String> {
    t.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Fraction of non-filler source words missing from the output.
fn dropped(src: &str, out: &str, fillers: &HashSet<String>) -> f32 {
    let s: Vec<String> = norm(src)
        .into_iter()
        .filter(|w| !fillers.contains(w))
        .collect();
    if s.is_empty() {
        return 0.0;
    }
    let mut pool = norm(out);
    let mut kept = 0usize;
    for w in &s {
        if let Some(i) = pool.iter().position(|p| p == w) {
            pool.remove(i);
            kept += 1;
        }
    }
    1.0 - kept as f32 / s.len() as f32
}

/// Fraction of output words that were never spoken.
fn invented(src: &str, out: &str) -> f32 {
    let o = norm(out);
    if o.is_empty() {
        return 0.0;
    }
    let mut pool = norm(src);
    let mut new = 0usize;
    for w in &o {
        if let Some(i) = pool.iter().position(|p| p == w) {
            pool.remove(i);
        } else {
            new += 1;
        }
    }
    new as f32 / o.len() as f32
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bench/tier1-out.json".into());
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(1);
    });
    let cases: Vec<serde_json::Value> = serde_json::from_str(&raw).expect("case array");
    let t0 = Tier0Rules::new(Some("en"));

    let fillers: HashSet<String> = [
        "um",
        "uh",
        "erm",
        "er",
        "ah",
        "hmm",
        "so",
        "basically",
        "okay",
        "like",
        "you",
        "know",
        "right",
        "i",
        "mean",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    let (mut d0, mut d1, mut i0, mut i1) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut worse = 0usize;
    println!(
        "{:<5}{:>9}{:>9}{:>9}{:>9}",
        "case", "T0 drop", "T1 drop", "T0 inv", "T1 inv"
    );
    for (n, c) in cases.iter().enumerate() {
        let src = c["raw"].as_str().unwrap_or("");
        let t1_text = c["tier1"].as_str().unwrap_or("");
        let t0_text = t0
            .process(src, None)
            .iter()
            .map(|b| b.text().to_string())
            .collect::<Vec<_>>()
            .join(" ");

        let (a, b) = (
            dropped(src, &t0_text, &fillers),
            dropped(src, t1_text, &fillers),
        );
        let (x, y) = (invented(src, &t0_text), invented(src, t1_text));
        d0 += a;
        d1 += b;
        i0 += x;
        i1 += y;
        if b > a || y > x {
            worse += 1;
        }
        println!("{:<5}{:>9.3}{:>9.3}{:>9.3}{:>9.3}", n, a, b, x, y);
        if b > a + 0.05 || y > x + 0.05 {
            println!("     src: {src}");
            println!("     T0 : {t0_text}");
            println!("     T1 : {t1_text}");
        }
    }
    let n = cases.len() as f32;
    println!("\n=== mean over {} cases ===", cases.len());
    println!(
        "  content_dropped   Tier 0 {:.3}   Tier 1 {:.3}",
        d0 / n,
        d1 / n
    );
    println!(
        "  invented          Tier 0 {:.3}   Tier 1 {:.3}",
        i0 / n,
        i1 / n
    );
    println!(
        "  Tier 1 worse than Tier 0 on {worse} of {} cases",
        cases.len()
    );
}
