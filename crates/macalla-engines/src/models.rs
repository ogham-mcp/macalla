// SPDX-License-Identifier: MIT
//! Model weights: what exists, where it lives, and how it gets there.
//!
//! Model management: download on demand with progress and
//! cancel, a SHA-256 check, stored under the OS data dir, listed with size and
//! licence, deletable, importable from a local file, mirror configurable. The
//! macOS default engine needs none of this; Parakeet, the default elsewhere,
//! needs all of it before it can say a word.
//!
//! This lives in the engines crate because a vendor's weights are the vendor
//! adapter's business. The store's default location is resolved
//! here from the OS data dir rather than injected by every trigger, and the
//! HTTP client is called directly rather than through a port: both are noted
//! as deliberate bends, at the outer layer, with a path to separation.

use macalla_core::{Error, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use ureq::ResponseExt;

/// One downloadable model, pinned to a file and a hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// Stable id, used by the engines and the CLI.
    pub id: &'static str,
    /// The registry id of the engine that runs on it.
    pub engine: &'static str,
    /// Plain name for a card.
    pub name: &'static str,
    /// File name in the store.
    pub file: &'static str,
    /// Where it comes from. A mirror replaces the host part.
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
    /// SPDX id, for the list and About.
    pub licence: &'static str,
    /// The attribution line the licence requires, or empty.
    pub attribution: &'static str,
    /// Which engine code runs it.
    pub family: Family,
    /// What the model takes as a language, in its own spelling: BCP-47
    /// primary tags for most, locales for a model that wants them. Its own
    /// list, read from the model's capabilities, not a published summary.
    pub languages: &'static [&'static str],
    /// The engine's name on the Engine card, shorter than `name`.
    pub engine_name: &'static str,
    /// One sentence for the Engine card: what it is good for, where it is
    /// weaker, and the download. Claims trace to docs/benchmarks.md or the
    /// model card; the size is checked against `bytes` by a test.
    pub blurb: &'static str,
    /// The heading the Engine card lists it under: who made the model, as
    /// text. No logos -- they are other companies' marks.
    pub group: &'static str,
}

/// The transcribe.cpp families Macalla has an engine for. The family decides
/// batch or streaming and which options a run takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Parakeet,
    Nemotron,
    Whisper,
}

impl ModelSpec {
    /// Size for a card, in whole megabytes.
    pub fn megabytes(&self) -> u64 {
        self.bytes / 1_000_000
    }

    /// The language to ask the model for, in its own spelling: the same tag,
    /// or failing that the first of the same language. So `en-GB` stays
    /// `en-GB` on a model with locales and becomes `en` on one without, and a
    /// bare `en` becomes the first English locale. `None` when the model does
    /// not speak it.
    pub fn language_for(&self, language: &str) -> Option<&'static str> {
        let wanted = language.replace('_', "-");
        if let Some(exact) = self
            .languages
            .iter()
            .find(|l| l.eq_ignore_ascii_case(&wanted))
        {
            return Some(exact);
        }
        let primary = wanted.split('-').next().unwrap_or("");
        if primary.is_empty() {
            return None;
        }
        self.languages
            .iter()
            .find(|l| {
                l.split('-')
                    .next()
                    .is_some_and(|p| p.eq_ignore_ascii_case(primary))
            })
            .copied()
    }
}

/// Parakeet TDT 0.6B v3, Q8_0: the quantisation the evaluation ran (same WER
/// as F32 on LibriSpeech within 0.01, 740 MB). Ported and published by
/// handy-computer from NVIDIA's checkpoint; CC-BY-4.0, so the attribution is
/// a licence condition (MODELS.md).
pub const PARAKEET_TDT_V3: ModelSpec = ModelSpec {
    id: "parakeet-tdt-0.6b-v3",
    engine: "parakeet-tdt-v3",
    name: "Parakeet TDT 0.6B v3",
    file: "parakeet-tdt-0.6b-v3-Q8_0.gguf",
    url: "https://huggingface.co/handy-computer/parakeet-tdt-0.6b-v3-gguf/resolve/main/parakeet-tdt-0.6b-v3-Q8_0.gguf",
    sha256: "5859f77944efcd8eafa23a6350731960b2b55b2203df51f319665c807d802cc7",
    bytes: 739_508_576,
    licence: "CC-BY-4.0",
    // CC BY 4.0 section 3(a)(1) asks for the creator, a link to the material,
    // a note of any modification, and the licence or a link to it. This file
    // is a modified copy -- handy-computer's model card lists it as a
    // quantisation of NVIDIA's checkpoint -- so the notice says so and links
    // both. Macalla checks the hash and loads the file as published. The
    // About screen reads this string; there is no second copy to drift.
    attribution: "Parakeet TDT 0.6B v3 by NVIDIA (huggingface.co/nvidia/parakeet-tdt-0.6b-v3), \
                  converted to GGUF and quantised to Q8_0 by handy-computer \
                  (huggingface.co/handy-computer/parakeet-tdt-0.6b-v3-gguf). Licensed under \
                  CC BY 4.0 (creativecommons.org/licenses/by/4.0). Macalla uses the file unmodified.",
    family: Family::Parakeet,
    languages: &[
        "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv", "lt",
        "mt", "pl", "pt", "ro", "ru", "sk", "sl", "es", "sv", "uk",
    ],
    engine_name: "Parakeet",
    group: "NVIDIA",
    blurb: "25 European languages and a stronger ear for technical terms. \
            A 739 MB download. Text arrives at each pause rather than as you speak.",
};

/// Nemotron 3.5 ASR streaming 0.6B, Q8_0: the file the evaluation ran, which
/// streams through transcribe.cpp with text about a second after speech
/// starts (docs/benchmarks.md). Ported and published by handy-computer.
pub const NEMOTRON_STREAMING: ModelSpec = ModelSpec {
    id: "nemotron-3.5-asr-streaming-0.6b",
    engine: "nemotron-3.5-streaming",
    name: "Nemotron 3.5 ASR Streaming 0.6B",
    file: "nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
    url: "https://huggingface.co/handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf/resolve/main/nemotron-3.5-asr-streaming-0.6b-Q8_0.gguf",
    sha256: "b94545b313b3223fda7b2857a52681da813935c2127643d1e9ff0c23d988089c",
    bytes: 751_094_240,
    licence: "OpenMDW-1.1",
    // OpenMDW-1.1 puts its conditions on whoever distributes the model
    // (MODELS.md). Macalla fetches the published file rather than shipping
    // it, and still credits it in About the way it credits Parakeet: the
    // creator, the converter, and the licence, from the model card.
    attribution: "Nemotron 3.5 ASR Streaming 0.6B by NVIDIA \
                  (huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b), converted to GGUF \
                  and quantised to Q8_0 by handy-computer \
                  (huggingface.co/handy-computer/nemotron-3.5-asr-streaming-0.6b-gguf). \
                  Licensed under OpenMDW-1.1. Macalla uses the file unmodified.",
    family: Family::Nemotron,
    // The 32 locales the model reports, in its own order. Where a language
    // has two, the first is the one a bare language tag gets.
    languages: &[
        "en-US", "en-GB", "es-US", "es-ES", "fr-FR", "fr-CA", "it-IT", "pt-BR", "pt-PT", "nl-NL",
        "de-DE", "tr-TR", "ru-RU", "ar-AR", "hi-IN", "ja-JP", "ko-KR", "vi-VN", "uk-UA", "pl-PL",
        "sv-SE", "cs-CZ", "nb-NO", "da-DK", "bg-BG", "fi-FI", "hr-HR", "sk-SK", "zh-CN", "hu-HU",
        "ro-RO", "et-EE",
    ],
    engine_name: "Nemotron",
    group: "NVIDIA",
    blurb: "Shows words as you speak, in 28 languages including Japanese, Chinese and \
            Arabic. Less accurate than Parakeet on long dictation. A 751 MB download.",
};

/// The 99 languages multilingual Whisper reports about itself.
const WHISPER_LANGUAGES: &[&str] = &[
    "af", "am", "ar", "as", "az", "ba", "be", "bg", "bn", "bo", "br", "bs", "ca", "cs", "cy", "da",
    "de", "el", "en", "es", "et", "eu", "fa", "fi", "fo", "fr", "gl", "gu", "haw", "ha", "he",
    "hi", "hr", "ht", "hu", "hy", "id", "is", "it", "ja", "jw", "ka", "kk", "km", "kn", "ko", "la",
    "lb", "ln", "lo", "lt", "lv", "mg", "mi", "mk", "ml", "mn", "mr", "ms", "mt", "my", "ne", "nl",
    "nn", "no", "oc", "pa", "pl", "ps", "pt", "ro", "ru", "sa", "sd", "si", "sk", "sl", "sn", "so",
    "sq", "sr", "su", "sv", "sw", "ta", "te", "tg", "th", "tk", "tl", "tr", "tt", "uk", "ur", "uz",
    "vi", "yi", "yo", "zh",
];

// The three Whisper files were evaluated on 2026-09-10 (docs/benchmarks.md,
// "Canary 180M Flash and Whisper, the small models"). OpenAI's Whisper
// repository on GitHub is MIT; the model cards on Hugging Face, OpenAI's and
// handy-computer's alike, declare Apache-2.0, and the files come from there.
// Apache-2.0 asks for its licence and notices when the model is distributed;
// Macalla fetches the published file, and credits it in About regardless.

/// Whisper small.en, Q8_0: English only, the smallest model offered.
pub const WHISPER_SMALL_EN: ModelSpec = ModelSpec {
    id: "whisper-small.en",
    engine: "whisper-small.en",
    name: "Whisper small.en",
    file: "whisper-small.en-Q8_0.gguf",
    url: "https://huggingface.co/handy-computer/whisper-small.en-gguf/resolve/main/whisper-small.en-Q8_0.gguf",
    sha256: "9614e6b7fda2d26018e4f268aece8ca25a83296ea0b534169a585b740bfd71ef",
    bytes: 269_674_144,
    licence: "Apache-2.0",
    attribution: "Whisper small.en by OpenAI (huggingface.co/openai/whisper-small.en), \
                  converted to GGUF and quantised to Q8_0 by handy-computer \
                  (huggingface.co/handy-computer/whisper-small.en-gguf). Licensed under \
                  Apache-2.0. Macalla uses the file unmodified.",
    family: Family::Whisper,
    languages: &["en"],
    engine_name: "Whisper Small (English)",
    group: "OpenAI",
    blurb: "English only, the smallest model here, and it listens for the words in your \
            dictionary. Can write a stray phrase over loud background noise. \
            A 269 MB download.",
};

/// Whisper medium.en, Q8_0: English only, and ahead of small.en on the
/// technical fixtures without a prompt (docs/benchmarks.md).
pub const WHISPER_MEDIUM_EN: ModelSpec = ModelSpec {
    id: "whisper-medium.en",
    engine: "whisper-medium.en",
    name: "Whisper medium.en",
    file: "whisper-medium.en-Q8_0.gguf",
    url: "https://huggingface.co/handy-computer/whisper-medium.en-gguf/resolve/main/whisper-medium.en-Q8_0.gguf",
    sha256: "03d7257fef498750ce272631bc6a34de322fc2b438aab5c268ff49dfd1b64c49",
    bytes: 831_460_928,
    licence: "Apache-2.0",
    attribution: "Whisper medium.en by OpenAI (huggingface.co/openai/whisper-medium.en), \
                  converted to GGUF and quantised to Q8_0 by handy-computer \
                  (huggingface.co/handy-computer/whisper-medium.en-gguf). Licensed under \
                  Apache-2.0. Macalla uses the file unmodified.",
    family: Family::Whisper,
    languages: &["en"],
    engine_name: "Whisper Medium (English)",
    group: "OpenAI",
    blurb: "English only, more accurate than Whisper Small on technical speech, and it \
            listens for the words in your dictionary. Slower, and can write a stray phrase \
            over loud background noise. An 831 MB download.",
};

/// Whisper medium, Q8_0: 99 languages by its own list. Only German, French,
/// Spanish and Italian have been measured, all four of them Parakeet languages.
pub const WHISPER_MEDIUM: ModelSpec = ModelSpec {
    id: "whisper-medium",
    engine: "whisper-medium",
    name: "Whisper medium",
    file: "whisper-medium-Q8_0.gguf",
    url: "https://huggingface.co/handy-computer/whisper-medium-gguf/resolve/main/whisper-medium-Q8_0.gguf",
    sha256: "09e6a65e7de377aa5b10bae24608bc6f8ca2ed04b3993ef10d4a02bcd9a82adf",
    bytes: 831_538_144,
    licence: "Apache-2.0",
    attribution: "Whisper medium by OpenAI (huggingface.co/openai/whisper-medium), \
                  converted to GGUF and quantised to Q8_0 by handy-computer \
                  (huggingface.co/handy-computer/whisper-medium-gguf). Licensed under \
                  Apache-2.0. Macalla uses the file unmodified.",
    family: Family::Whisper,
    languages: WHISPER_LANGUAGES,
    engine_name: "Whisper Medium",
    group: "OpenAI",
    blurb: "99 languages, including many Parakeet does not speak; only German, French, \
            Spanish and Italian have been tested here. It listens for the words in your \
            dictionary. Slower, and can write a stray phrase over loud background noise. An \
            831 MB download.",
};

/// Every model this build knows how to fetch, in the order the Engine card
/// lists them.
pub fn catalogue() -> &'static [ModelSpec] {
    &[
        PARAKEET_TDT_V3,
        NEMOTRON_STREAMING,
        WHISPER_SMALL_EN,
        WHISPER_MEDIUM_EN,
        WHISPER_MEDIUM,
    ]
}

pub fn find(id: &str) -> Option<&'static ModelSpec> {
    catalogue().iter().find(|m| m.id == id)
}

/// What the store holds for one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStatus {
    /// The file is there at its full size. The hash is checked on download
    /// and import, not on every look.
    Installed(PathBuf),
    /// A download stopped partway; it resumes from here.
    Partial {
        bytes: u64,
    },
    Absent,
}

/// Where model files live. One per machine, shared by every app: the CLI and
/// the desktop must never download the same 740 MB twice.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

/// Overrides the default location. For tests, CI, and a person who keeps
/// models on another disk.
pub const DIR_ENV: &str = "MACALLA_MODELS_DIR";

/// A mirror for every download: replaces the scheme and host of the
/// published URL, for a machine that cannot reach Hugging Face. The CLI's
/// `--mirror` takes precedence over it.
pub const MIRROR_ENV: &str = "MACALLA_MODELS_MIRROR";

impl Store {
    /// The machine's store: `$MACALLA_MODELS_DIR`, or `<data dir>/Macalla/models`
    /// (`~/Library/Application Support/Macalla/models` on macOS).
    pub fn shared() -> Store {
        if let Some(dir) = std::env::var_os(DIR_ENV).filter(|d| !d.is_empty()) {
            return Store::at(PathBuf::from(dir));
        }
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        Store::at(base.join("Macalla").join("models"))
    }

    pub fn at(dir: PathBuf) -> Store {
        Store { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, spec: &ModelSpec) -> PathBuf {
        self.dir.join(spec.file)
    }

    fn part_for(&self, spec: &ModelSpec) -> PathBuf {
        self.dir.join(format!("{}.part", spec.file))
    }

    pub fn status(&self, spec: &ModelSpec) -> ModelStatus {
        let path = self.path_for(spec);
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() == spec.bytes {
                return ModelStatus::Installed(path);
            }
        }
        if let Ok(meta) = fs::metadata(self.part_for(spec)) {
            return ModelStatus::Partial { bytes: meta.len() };
        }
        ModelStatus::Absent
    }

    pub fn installed(&self, spec: &ModelSpec) -> Option<PathBuf> {
        match self.status(spec) {
            ModelStatus::Installed(path) => Some(path),
            _ => None,
        }
    }

    /// Delete the model and any partial download. Not an error if absent.
    pub fn remove(&self, spec: &ModelSpec) -> Result<()> {
        for path in [self.path_for(spec), self.part_for(spec)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Model(format!("removing {}: {e}", path.display()))),
            }
        }
        Ok(())
    }

    /// Install from a file already on this machine, for an offline install.
    /// Copied, then hashed; a wrong file is refused and nothing is kept.
    pub fn import(&self, spec: &ModelSpec, from: &Path) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir)
            .map_err(|e| Error::Model(format!("creating {}: {e}", self.dir.display())))?;
        let part = self.part_for(spec);
        fs::copy(from, &part)
            .map_err(|e| Error::Model(format!("copying {}: {e}", from.display())))?;
        self.finish(spec, &part, None)
    }

    /// Hash the file at `part`, refuse it if wrong, rename it into place.
    /// `hash` is the running digest of a download that hashed as it wrote;
    /// `None` hashes the file now.
    fn finish(&self, spec: &ModelSpec, part: &Path, hash: Option<Sha256>) -> Result<PathBuf> {
        let digest = match hash {
            Some(h) => h,
            None => hash_file(part)?,
        };
        let got = hex(&digest.finalize());
        if got != spec.sha256 {
            let _ = fs::remove_file(part);
            return Err(Error::Model(format!(
                "{} did not match its published checksum, so it was not kept. \
                 Try the download again.",
                spec.name
            )));
        }
        let path = self.path_for(spec);
        fs::rename(part, &path)
            .map_err(|e| Error::Model(format!("moving into place {}: {e}", path.display())))?;
        Ok(path)
    }
}

/// What a download reports as it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Bytes on disk so far, out of the model's size.
    Received { bytes: u64, total: u64 },
    /// The connection stalled or dropped. The download carries on from
    /// `bytes` on a new one; nothing already on disk is fetched again.
    Reconnecting { bytes: u64, total: u64 },
    /// Every byte is down; the hash is being checked.
    Verifying,
}

/// How long a download waits on a connection before trying another.
///
/// One connection to a CDN can crawl while a fresh one to the same URL runs
/// far faster: measured on 2026-09-10 at 0.19 MB/s against 14.5 MB/s for the
/// same file at the same moment. And an HTTP read with no deadline can wait
/// forever, which also means a cancel is never seen. So a model is fetched in
/// pieces, and a piece that arrives slower than `floor` loses its connection
/// and carries on from where it got to on a new one. Nothing is given up while
/// bytes still arrive, however slowly; only `give_up_after` attempts in a row
/// that deliver nothing end the download, and the partial file is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Patience {
    /// Bytes asked for in one request.
    piece: u64,
    /// The slowest average rate, in bytes a second, a piece may arrive at.
    floor: u64,
    /// Added to each piece's time, for a slow start.
    slack: Duration,
    /// For connecting, and for the response headers to come back.
    connect: Duration,
    /// Attempts in a row that deliver nothing before the download stops.
    give_up_after: u32,
    /// Waited before trying again, once for each attempt in a row that
    /// delivered nothing, so a network that blinks gets a moment to return.
    backoff: Duration,
}

impl Patience {
    /// 8 MB a piece at no less than 256 kB a second: a connection that stops
    /// earning its keep is dropped within about 36 seconds, and a network
    /// that is merely slow still finishes.
    const DEFAULT: Patience = Patience {
        piece: 8_000_000,
        floor: 256_000,
        slack: Duration::from_secs(5),
        connect: Duration::from_secs(15),
        give_up_after: 3,
        backoff: Duration::from_secs(1),
    };

    /// How long `bytes` may take: the time at the floor, plus the slack.
    fn deadline(&self, bytes: u64) -> Duration {
        Duration::from_millis(bytes.saturating_mul(1_000) / self.floor.max(1)) + self.slack
    }
}

/// Fetch a model into the store. Blocking; call it on its own thread.
///
/// Resumes a partial download, and hashes as it writes, so a completed
/// download is verified without a second pass over the file. It fetches in
/// pieces and changes connection when one stalls (see [`Patience`]),
/// reporting [`Progress::Reconnecting`] when it does. `cancel` is looked at
/// between reads, and no read waits longer than its piece's deadline, so a
/// cancel is seen within about half a minute even on a connection that has
/// gone silent. A cancelled or abandoned download keeps its partial file for
/// next time and returns [`Error::Model`]. `mirror` replaces the scheme and
/// host of the published URL, for a machine that cannot reach Hugging Face.
pub fn download(
    spec: &ModelSpec,
    store: &Store,
    mirror: Option<&str>,
    on_progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    let env_mirror = std::env::var(MIRROR_ENV).ok().filter(|m| !m.is_empty());
    let url = match mirror.or(env_mirror.as_deref()) {
        Some(m) => rehost(spec.url, m),
        None => spec.url.to_string(),
    };
    download_from(spec, store, &url, Patience::DEFAULT, on_progress, cancel)
}

/// Why a request ended before its piece was in.
enum Stop {
    Cancelled,
    /// Writing the file failed. Another connection will not help.
    Disk(Error),
    /// The connection was slow, silent, closed early or refused. Another
    /// connection may do better.
    Connection(String),
    /// The server ignored the byte range and is sending the whole file.
    NoRanges,
}

/// Where a piece's bytes go: the part file, the running hash, and the count.
struct Sink<'a> {
    file: &'a mut fs::File,
    hash: &'a mut Sha256,
    have: &'a mut u64,
    part: &'a Path,
}

fn download_from(
    spec: &ModelSpec,
    store: &Store,
    url: &str,
    patience: Patience,
    on_progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    if let Some(path) = store.installed(spec) {
        return Ok(path);
    }
    fs::create_dir_all(store.dir())
        .map_err(|e| Error::Model(format!("creating {}: {e}", store.dir().display())))?;
    let part = store.part_for(spec);

    // Resume: hash what is already there so the final check still covers
    // the whole file.
    let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut hash = Sha256::new();
    if have > 0 {
        if have >= spec.bytes {
            // A stale part file as big as the model or bigger. Start over.
            let _ = fs::remove_file(&part);
            have = 0;
        } else {
            hash = hash_file(&part)?;
        }
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)
        .map_err(|e| Error::Model(format!("writing {}: {e}", part.display())))?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(patience.connect))
        .timeout_recv_response(Some(patience.connect))
        .build()
        .into();
    let cancelled = |have: u64| {
        Error::Model(format!(
            "the download of {} was cancelled; it will resume from {} MB next time",
            spec.name,
            have / 1_000_000
        ))
    };

    // Where pieces come from: the published URL at first, then wherever it
    // redirected to, so each piece does not pay for the redirect again. Back
    // to the published URL whenever a connection fails, since a CDN address
    // can expire and a fresh redirect may land on a better server.
    let mut from = url.to_string();
    let mut ranges = true;
    let mut empty = 0u32;
    on_progress(Progress::Received {
        bytes: have,
        total: spec.bytes,
    });
    while have < spec.bytes {
        if cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            return Err(cancelled(have));
        }
        let before = have;
        let span = ranges.then(|| (have, (have + patience.piece).min(spec.bytes) - 1));
        let deadline = patience.deadline(span.map_or(spec.bytes - have, |(a, b)| b + 1 - a));
        let outcome = {
            let mut sink = Sink {
                file: &mut file,
                hash: &mut hash,
                have: &mut have,
                part: &part,
            };
            fetch(
                &agent,
                &from,
                span,
                deadline,
                spec.bytes,
                &mut sink,
                on_progress,
                cancel,
            )
        };
        match outcome {
            Ok(landed_at) => {
                empty = 0;
                from = landed_at;
            }
            Err(Stop::Cancelled) => {
                let _ = file.flush();
                return Err(cancelled(have));
            }
            Err(Stop::Disk(e)) => return Err(e),
            Err(Stop::NoRanges) => {
                // Take the whole file from the top, in one request. Reopened
                // truncated rather than `set_len(0)` on the handle: on
                // Windows an append-only handle has no right to change the
                // file's length, and fails with "Access is denied".
                file = fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&part)
                    .map_err(|e| Error::Model(format!("writing {}: {e}", part.display())))?;
                have = 0;
                hash = Sha256::new();
                ranges = false;
            }
            Err(Stop::Connection(reason)) => {
                empty = if have > before { 0 } else { empty + 1 };
                if empty >= patience.give_up_after {
                    let _ = file.flush();
                    return Err(Error::Model(format!(
                        "the download of {} stopped at {} of {} MB ({reason}); it will resume next time",
                        spec.name,
                        have / 1_000_000,
                        spec.megabytes()
                    )));
                }
                tracing::info!(
                    model = spec.id,
                    mb = have / 1_000_000,
                    %reason,
                    "download reconnecting"
                );
                on_progress(Progress::Reconnecting {
                    bytes: have,
                    total: spec.bytes,
                });
                from = url.to_string();
                if !wait(patience.backoff * empty, cancel) {
                    let _ = file.flush();
                    return Err(cancelled(have));
                }
            }
        }
    }
    file.flush()
        .map_err(|e| Error::Model(format!("writing {}: {e}", part.display())))?;
    drop(file);
    on_progress(Progress::Verifying);
    store.finish(spec, &part, Some(hash))
}

/// One request: a byte range of the file, or all of it once ranges are off.
/// Writes what arrives as it arrives, so an interrupted piece keeps its
/// bytes. Returns where the request landed after any redirects.
#[allow(clippy::too_many_arguments)]
fn fetch(
    agent: &ureq::Agent,
    url: &str,
    span: Option<(u64, u64)>,
    deadline: Duration,
    total: u64,
    sink: &mut Sink<'_>,
    on_progress: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> std::result::Result<String, Stop> {
    let mut request = agent
        .get(url)
        .config()
        .timeout_recv_body(Some(deadline))
        .build();
    if let Some((first, last)) = span {
        request = request.header("Range", &format!("bytes={first}-{last}"));
    }
    let mut response = request
        .call()
        .map_err(|e| Stop::Connection(e.to_string()))?;
    let landed_at = response.get_uri().to_string();
    match (response.status().as_u16(), span) {
        (206, Some(_)) | (200, None) => {}
        (200, Some(_)) => return Err(Stop::NoRanges),
        (status, _) => return Err(Stop::Connection(format!("the server answered {status}"))),
    }
    let end = span.map_or(total, |(_, last)| last + 1);
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; 1 << 16];
    while *sink.have < end {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        // A deadline that passes surfaces here, as a read error.
        let n = reader
            .read(&mut buffer)
            .map_err(|e| Stop::Connection(e.to_string()))?;
        if n == 0 {
            return Err(Stop::Connection("the connection closed early".into()));
        }
        // Never past the piece asked for, whatever the server sends.
        let n = n.min((end - *sink.have) as usize);
        sink.file.write_all(&buffer[..n]).map_err(|e| {
            Stop::Disk(Error::Model(format!(
                "writing {}: {e}",
                sink.part.display()
            )))
        })?;
        sink.hash.update(&buffer[..n]);
        *sink.have += n as u64;
        on_progress(Progress::Received {
            bytes: *sink.have,
            total,
        });
    }
    Ok(landed_at)
}

/// Sleep, looking at `cancel` every tenth of a second. False if cancelled.
fn wait(duration: Duration, cancel: &AtomicBool) -> bool {
    let until = Instant::now() + duration;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// Load the model's engine once and let it go, so the cost of a first load
/// is paid now, under a progress bar, rather than on a keypress. On Apple
/// Silicon that is the Metal shader compile, around ten seconds, and it is
/// paid once per build of the binary: the OS caches the result after that.
/// Returns how long it took.
pub fn warm(spec: &ModelSpec) -> Result<std::time::Duration> {
    let started = std::time::Instant::now();
    let engine = crate::registry::open(spec.engine, "en")?;
    drop(engine);
    let took = started.elapsed();
    tracing::info!(
        model = spec.id,
        ms = took.as_millis() as u64,
        "model warmed"
    );
    Ok(took)
}

fn hash_file(path: &Path) -> Result<Sha256> {
    let mut file = fs::File::open(path)
        .map_err(|e| Error::Model(format!("reading {}: {e}", path.display())))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| Error::Model(format!("reading {}: {e}", path.display())))?;
        if n == 0 {
            return Ok(hash);
        }
        hash.update(&buffer[..n]);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Swap the scheme and host of `url` for `mirror`'s, keeping the path.
fn rehost(url: &str, mirror: &str) -> String {
    let path = url
        .find("://")
        .and_then(|i| url[i + 3..].find('/').map(|j| &url[i + 3 + j..]))
        .unwrap_or("");
    format!("{}{}", mirror.trim_end_matches('/'), path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_language_setting_maps_to_what_each_model_takes() {
        // A model with locales.
        assert_eq!(NEMOTRON_STREAMING.language_for("en-GB"), Some("en-GB"));
        assert_eq!(NEMOTRON_STREAMING.language_for("EN-us"), Some("en-US"));
        assert_eq!(NEMOTRON_STREAMING.language_for("pt_BR"), Some("pt-BR"));
        assert_eq!(
            NEMOTRON_STREAMING.language_for("en"),
            Some("en-US"),
            "a bare tag gets the first"
        );
        assert_eq!(NEMOTRON_STREAMING.language_for("de-AT"), Some("de-DE"));
        assert_eq!(NEMOTRON_STREAMING.language_for("el-GR"), None);
        // Models with language tags.
        assert_eq!(PARAKEET_TDT_V3.language_for("en-GB"), Some("en"));
        assert_eq!(PARAKEET_TDT_V3.language_for("pt_BR"), Some("pt"));
        assert_eq!(PARAKEET_TDT_V3.language_for("ja-JP"), None);
        assert_eq!(PARAKEET_TDT_V3.language_for(""), None);
        assert_eq!(WHISPER_MEDIUM.language_for("ja-JP"), Some("ja"));
        assert_eq!(WHISPER_SMALL_EN.language_for("en-US"), Some("en"));
        assert_eq!(
            WHISPER_SMALL_EN.language_for("de-DE"),
            None,
            "English only means English only"
        );
    }

    #[test]
    fn every_catalogue_entry_is_its_own_engine_and_states_its_real_size() {
        let mut ids = std::collections::HashSet::new();
        for m in catalogue() {
            assert!(ids.insert(m.id), "{} is listed twice", m.id);
            assert!(!m.languages.is_empty(), "{} speaks nothing", m.id);
            // The card's size is written by hand, so hold it to the file's.
            assert!(
                m.blurb.contains(&format!(" {} MB", m.megabytes())),
                "{} says {:?} but the file is {} MB",
                m.id,
                m.blurb,
                m.megabytes()
            );
        }
        let engines: std::collections::HashSet<_> = catalogue().iter().map(|m| m.engine).collect();
        assert_eq!(engines.len(), catalogue().len(), "one engine per model");
    }

    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// How the test server treats one request: `None` answers it properly;
    /// `Some((sent, silence))` sends `sent` bytes of the body, goes silent for
    /// `silence`, and hangs up.
    type Conduct = Option<(usize, Duration)>;

    struct TestServer {
        url: String,
        requests: Arc<AtomicUsize>,
    }

    /// A model host on localhost serving `body`, with or without byte ranges,
    /// behaving on each numbered request as `conduct` says.
    fn serve(
        body: Vec<u8>,
        ranges: bool,
        conduct: impl Fn(usize) -> Conduct + Send + Sync + 'static,
    ) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.gguf", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let body = Arc::new(body);
        let conduct = Arc::new(conduct);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let n = counted.fetch_add(1, Ordering::SeqCst);
                let (body, conduct) = (body.clone(), conduct.clone());
                std::thread::spawn(move || answer(stream, &body, ranges, conduct(n)));
            }
        });
        TestServer { url, requests }
    }

    fn answer(mut stream: TcpStream, body: &[u8], ranges: bool, conduct: Conduct) {
        let mut range = None;
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                let (a, b) = v.trim().split_once('-').unwrap();
                range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
            }
            line.clear();
        }
        let (status, slice, extra) = match range.filter(|_| ranges) {
            Some((a, b)) => (
                "206 Partial Content",
                &body[a..=b],
                format!("Content-Range: bytes {a}-{b}/{}\r\n", body.len()),
            ),
            None => ("200 OK", body, String::new()),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
            slice.len()
        );
        let _ = stream.write_all(head.as_bytes());
        match conduct {
            None => {
                let _ = stream.write_all(slice);
            }
            Some((sent, silence)) => {
                let _ = stream.write_all(&slice[..sent.min(slice.len())]);
                let _ = stream.flush();
                std::thread::sleep(silence);
            }
        }
    }

    fn test_body() -> Vec<u8> {
        (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn spec_for(body: &[u8], file: &'static str) -> ModelSpec {
        let sha256: &'static str = Box::leak(hex(&Sha256::digest(body)).into_boxed_str());
        ModelSpec {
            bytes: body.len() as u64,
            file,
            sha256,
            ..PARAKEET_TDT_V3
        }
    }

    fn temp_store(name: &str) -> (Store, PathBuf) {
        let dir = std::env::temp_dir().join(format!("macalla-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Store::at(dir.clone()), dir)
    }

    /// Pieces of 1 kB that must arrive at 20 kB/s: 50 ms each, plus 150 ms.
    fn quick() -> Patience {
        Patience {
            piece: 1_000,
            floor: 20_000,
            slack: Duration::from_millis(150),
            connect: Duration::from_secs(2),
            give_up_after: 3,
            backoff: Duration::ZERO,
        }
    }

    #[test]
    fn a_piece_gets_the_time_it_would_take_at_the_floor_plus_the_slack() {
        assert_eq!(
            Patience::DEFAULT.deadline(8_000_000),
            Duration::from_millis(31_250) + Duration::from_secs(5)
        );
    }

    #[test]
    fn a_connection_that_goes_silent_is_dropped_and_the_download_carries_on() {
        let body = test_body();
        // The third request sends 300 bytes and then nothing for ten seconds.
        let server = serve(body.clone(), true, |n| {
            (n == 2).then_some((300, Duration::from_secs(10)))
        });
        let (store, dir) = temp_store("silent");
        let spec = spec_for(&body, "silent.gguf");
        let mut reconnects = 0;
        let started = Instant::now();
        let path = download_from(
            &spec,
            &store,
            &server.url,
            quick(),
            &mut |p| {
                if matches!(p, Progress::Reconnecting { .. }) {
                    reconnects += 1;
                }
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), body, "every byte, once, in order");
        assert_eq!(reconnects, 1);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited out the silence: {:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_host_that_sends_nothing_is_given_up_on_and_the_partial_file_kept() {
        let body = test_body();
        // Two pieces arrive, and then every request goes silent.
        let server = serve(body.clone(), true, |n| {
            (n >= 2).then_some((0, Duration::from_secs(10)))
        });
        let (store, dir) = temp_store("nothing");
        let spec = spec_for(&body, "nothing.gguf");
        let err = download_from(
            &spec,
            &store,
            &server.url,
            quick(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("resume next time"), "{err}");
        assert_eq!(fs::metadata(store.part_for(&spec)).unwrap().len(), 2_000);
        assert_eq!(server.requests.load(Ordering::SeqCst), 2 + 3);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cancel_is_seen_while_the_connection_is_silent() {
        let body = test_body();
        let server = serve(body.clone(), true, |_| Some((0, Duration::from_secs(10))));
        let (store, dir) = temp_store("cancel");
        let spec = spec_for(&body, "cancel.gguf");
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let err = download_from(
            &spec,
            &store,
            &server.url,
            Patience {
                give_up_after: 100,
                ..quick()
            },
            &mut |_| {},
            &cancel,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_host_that_ignores_ranges_still_delivers_the_whole_file() {
        let body = test_body();
        let server = serve(body.clone(), false, |_| None);
        let (store, dir) = temp_store("no-ranges");
        let spec = spec_for(&body, "no-ranges.gguf");
        let path = download_from(
            &spec,
            &store,
            &server.url,
            quick(),
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), body);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mirror_replaces_the_host_and_keeps_the_path() {
        assert_eq!(
            rehost(
                "https://huggingface.co/a/b/resolve/main/c.gguf",
                "http://mirror.local:8080/"
            ),
            "http://mirror.local:8080/a/b/resolve/main/c.gguf"
        );
    }

    #[test]
    fn the_store_reports_absent_partial_and_installed() {
        let dir = std::env::temp_dir().join(format!("macalla-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::at(dir.clone());
        let spec = ModelSpec {
            bytes: 4,
            file: "tiny.gguf",
            ..PARAKEET_TDT_V3
        };
        assert_eq!(store.status(&spec), ModelStatus::Absent);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tiny.gguf.part"), b"ab").unwrap();
        assert_eq!(store.status(&spec), ModelStatus::Partial { bytes: 2 });
        fs::write(dir.join("tiny.gguf"), b"abcd").unwrap();
        assert!(matches!(store.status(&spec), ModelStatus::Installed(_)));
        store.remove(&spec).unwrap();
        assert_eq!(store.status(&spec), ModelStatus::Absent);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_with_the_wrong_hash_keeps_nothing() {
        let dir = std::env::temp_dir().join(format!("macalla-import-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Store::at(dir.join("store"));
        let wrong = dir.join("wrong.gguf");
        fs::write(&wrong, b"not the model").unwrap();
        let err = store.import(&PARAKEET_TDT_V3, &wrong).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
        assert_eq!(store.status(&PARAKEET_TDT_V3), ModelStatus::Absent);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_with_the_right_hash_lands_in_place() {
        let dir = std::env::temp_dir().join(format!("macalla-import-ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Store::at(dir.join("store"));
        let src = dir.join("tiny.gguf");
        fs::write(&src, b"abcd").unwrap();
        let spec = ModelSpec {
            bytes: 4,
            file: "tiny.gguf",
            sha256: "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589",
            ..PARAKEET_TDT_V3
        };
        let path = store.import(&spec, &src).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"abcd");
        assert!(matches!(store.status(&spec), ModelStatus::Installed(_)));
        let _ = fs::remove_dir_all(&dir);
    }
}
