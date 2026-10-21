// SPDX-License-Identifier: MIT
//! Paste known text through the app's own [`Inserter`] into the terminal this
//! probe runs in, and check what arrived. For finding whether, and when, a
//! terminal receives a paste cut short.
//!
//! The probe pastes into its own window and reads its own input, raw, so
//! nothing between the paste and the comparison edits the text, and there is
//! no second window to lose focus to. Run it in the terminal under test, with
//! that window in front and nothing else touched until it ends:
//!
//! ```sh
//! cargo run -p macalla-platform --example paste_probe -- \
//!     --lengths 30,200,1000 --waits 50,150,300 --reps 5 --report /tmp/probe.tsv
//! ```
//!
//! `--bracketed` turns on bracketed paste first, as shells and most
//! full-screen programs do. Before each paste the clipboard is set to a
//! marker of its own, so a paste that read the restored clipboard instead of
//! the text shows up as that marker. If a paste arrives empty the window has
//! probably lost focus, and the probe stops at once rather than paste again
//! into whatever is in front. The terminal needs Accessibility, as the app
//! does.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use macalla_platform::paste::{system_inserter, Clipboard, Inserted, SystemClipboard};

struct Args {
    lengths: Vec<usize>,
    waits: Vec<u64>,
    reps: usize,
    start_delay: u64,
    settle: u64,
    bracketed: bool,
    report: Option<std::path::PathBuf>,
}

fn list<T: std::str::FromStr>(value: &str) -> Vec<T> {
    value
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect()
}

fn args() -> Args {
    let mut args = Args {
        lengths: vec![30, 200, 1000],
        waits: vec![150],
        reps: 5,
        start_delay: 3,
        settle: 800,
        bracketed: false,
        report: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        if flag == "--bracketed" {
            args.bracketed = true;
            continue;
        }
        let value = it.next().unwrap_or_default();
        match flag.as_str() {
            "--lengths" => args.lengths = list(&value),
            "--waits" => args.waits = list(&value),
            "--reps" => args.reps = value.parse().unwrap_or(args.reps),
            "--start-delay" => args.start_delay = value.parse().unwrap_or(args.start_delay),
            "--settle" => args.settle = value.parse().unwrap_or(args.settle),
            "--report" => args.report = Some(value.into()),
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

/// Text of exactly `len` bytes that reads like dictation, starting with the
/// trial's id so a stray or late paste can be told apart. It never ends in a
/// space: iTerm2 and Terminal both drop a trailing space from a paste, which
/// would read as a paste cut short by one character.
fn text(id: usize, len: usize) -> String {
    const WORDS: [&str; 12] = [
        "the", "quick", "kubectl", "deploy", "rolled", "back,", "and", "then", "we", "checked",
        "the", "logs.",
    ];
    let mut s = format!("T{id:04} ");
    let mut i = id;
    while s.len() < len {
        s.push_str(WORDS[i % WORDS.len()]);
        s.push(' ');
        i += 1;
    }
    s.truncate(len);
    if s.ends_with(' ') {
        s.pop();
        s.push('.');
    }
    s
}

fn stty(arg: &str) {
    let _ = Command::new("stty")
        .arg(arg)
        .stdin(std::fs::File::open("/dev/tty").expect("a terminal"))
        .status();
}

/// Everything read from the terminal since the last call, with bracketed-paste
/// markers taken off.
fn arrived(rx: &mpsc::Receiver<u8>) -> String {
    let bytes: Vec<u8> = rx.try_iter().collect();
    String::from_utf8_lossy(&bytes)
        .replace("\u{1b}[200~", "")
        .replace("\u{1b}[201~", "")
}

fn main() {
    let args = args();
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .expect("a terminal");
    let mut marker = SystemClipboard::new().expect("clipboard");

    stty("raw");
    stty("-echo");
    if args.bracketed {
        let _ = tty.write_all(b"\x1b[?2004h");
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut input = std::fs::File::open("/dev/tty").expect("a terminal");
        let mut byte = [0u8; 1];
        while input.read(&mut byte).is_ok_and(|n| n == 1) {
            if tx.send(byte[0]).is_err() {
                break;
            }
        }
    });

    std::thread::sleep(Duration::from_secs(args.start_delay));
    let _ = arrived(&rx);

    let mut lines = vec!["trial\tlength\twait_ms\toutcome\treceived\tverdict\tms".to_string()];
    let mut summary: BTreeMap<(u64, usize), (usize, usize)> = BTreeMap::new();
    let mut id = 0;
    'trials: for &wait in &args.waits {
        for &len in &args.lengths {
            for _ in 0..args.reps {
                id += 1;
                let sent = text(id, len);
                marker
                    .write_text(&format!("PREVIOUS-CLIPBOARD-{id:04}"))
                    .expect("set the previous clipboard");
                let mut inserter = system_inserter()
                    .expect("inserter (Accessibility for this terminal?)")
                    .with_restore_wait(Duration::from_millis(wait));
                let started = Instant::now();
                let outcome = inserter.insert(&sent).expect("insert");
                // Whatever the terminal is still reading, give it time to land.
                std::thread::sleep(Duration::from_millis(args.settle));
                let got = arrived(&rx);
                let verdict = if got == sent {
                    "ok"
                } else if got.contains("PREVIOUS-CLIPBOARD") {
                    "read-restored-clipboard"
                } else if got.is_empty() {
                    "nothing"
                } else if sent.starts_with(&got) {
                    "cut-short"
                } else {
                    "different"
                };
                let entry = summary.entry((wait, len)).or_default();
                entry.1 += 1;
                if verdict == "ok" {
                    entry.0 += 1;
                }
                lines.push(format!(
                    "{id}\t{len}\t{wait}\t{outcome:?}\t{}\t{verdict}\t{}",
                    got.len(),
                    started.elapsed().as_millis()
                ));
                if verdict == "nothing" || matches!(outcome, Inserted::NotDelivered(_)) {
                    lines.push(
                        "stopped: nothing arrived, so this window may not be in front".into(),
                    );
                    break 'trials;
                }
            }
        }
    }

    if args.bracketed {
        let _ = tty.write_all(b"\x1b[?2004l");
    }
    stty("-raw");
    stty("echo");
    lines.push(String::new());
    lines.push("wait_ms\tlength\tok/of".into());
    for ((wait, len), (ok, of)) in summary {
        lines.push(format!("{wait}\t{len}\t{ok}/{of}"));
    }
    let report = lines.join("\n") + "\n";
    match &args.report {
        Some(path) => std::fs::write(path, &report).expect("write the report"),
        None => print!("{report}"),
    }
}
