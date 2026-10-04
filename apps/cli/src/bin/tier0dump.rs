// SPDX-License-Identifier: MIT
//! Emit Tier 0 output for the bench fixtures as JSON.
//!
//! Exists because Tier 1 must be evaluated on what it actually receives in
//! production. Tier 0 comes first in the pipeline, so the earlier bake-off — which fed
//! raw transcripts straight to the model — was measuring a pipeline we do not
//! ship.

use macalla_core::enhance::Tier0;
use macalla_core::tier0::Tier0Rules;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "bench/fixtures.json".into());
    let raw = std::fs::read_to_string(&path).expect("fixtures");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let t0 = Tier0Rules::new(Some("en"));

    let out: Vec<serde_json::Value> = v["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .map(|c| {
            let src = c["raw"].as_str().unwrap_or("");
            let text = t0
                .process(src, None)
                .iter()
                .map(|b| b.text().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            serde_json::json!({ "raw": src, "tier0": text })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&out).expect("serialise"));
}
