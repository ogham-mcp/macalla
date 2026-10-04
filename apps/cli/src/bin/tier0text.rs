// SPDX-License-Identifier: MIT
//! Read a JSON array of strings on stdin, emit Tier 0 output as a JSON array.
use macalla_core::enhance::Tier0;
use macalla_core::tier0::Tier0Rules;
use std::io::Read;

fn main() {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).expect("stdin");
    let inputs: Vec<String> = serde_json::from_str(&buf).expect("array of strings");
    let t0 = Tier0Rules::new(Some("en"));
    let out: Vec<String> = inputs
        .iter()
        .map(|s| {
            t0.process(s, None)
                .iter()
                .map(|b| b.text().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    println!("{}", serde_json::to_string(&out).expect("serialise"));
}
