// SPDX-License-Identifier: MIT
//! Audio files into 16 kHz mono, for `macalla transcribe`.
//!
//! Two readers and no bundled codec. WAV and AIFF are read here, because a
//! plain recording and every fixture should need no tool at all. Everything
//! else -- mp3, m4a, mp4, mov, opus, webm -- is handed to `ffmpeg` on the
//! PATH, which transcodes to raw 16 kHz mono float on its stdout and is read
//! as a stream. That is the transcribe-and-summarise shape: use the tool
//! that is already there rather than ship a decoder, and say plainly what
//! to install when it is not.
//!
//! What stays in memory, measured rather than assumed. The ffmpeg reader holds
//! the decoded samples in the vector it returns, which grows as it fills. The
//! WAV and AIFF readers hold the file's own bytes and the decoded vector, and
//! nothing else the size of the file: the sample data is borrowed from those
//! bytes, and it is converted and resampled a block at a time. An earlier
//! version of this note said the samples were held once whatever the reader.
//! That was never true of these two, which held four whole-file copies and
//! resampled in quadratic time: 392 MB and 12.6 s for a 13-minute AIFF.

use crate::{Error, Result};
use macalla_core::audio::resample::Resampler;
use macalla_core::audio::SAMPLE_RATE;
use std::io::Read;
use std::path::Path;

/// Decode a file to 16 kHz mono. See the module note for which reader.
pub fn decode_file(path: &Path) -> Result<Vec<f32>> {
    let mut head = [0u8; 12];
    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::Audio(format!("cannot open {}: {e}", path.display())))?;
    let n = file
        .read(&mut head)
        .map_err(|e| Error::Audio(format!("cannot read {}: {e}", path.display())))?;
    drop(file);
    let head = &head[..n];
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return read_wav(path);
    }
    if head.len() >= 12
        && &head[..4] == b"FORM"
        && (&head[8..12] == b"AIFF" || &head[8..12] == b"AIFC")
    {
        return read_aiff(path);
    }
    ffmpeg(path)
}

/// Whether `ffmpeg` can be found, for a doctor line.
pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn ffmpeg(path: &Path) -> Result<Vec<f32>> {
    let mut child = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-vn", "-f", "f32le", "-ac", "1", "-ar"])
        .arg(SAMPLE_RATE.to_string())
        .arg("-")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Audio(format!(
                    "{} is not a wav or aiff file, and decoding anything else needs ffmpeg on \
                     the PATH. Install it (brew install ffmpeg, apt install ffmpeg, winget \
                     install ffmpeg) and try again.",
                    path.display()
                ))
            } else {
                Error::Audio(format!("starting ffmpeg: {e}"))
            }
        })?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Read stderr on its own thread so a chatty ffmpeg cannot deadlock
    // against a full pipe while we drain stdout.
    let complaints = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut samples = Vec::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = stdout
            .read(&mut buffer)
            .map_err(|e| Error::Audio(format!("reading from ffmpeg: {e}")))?;
        if n == 0 {
            break;
        }
        carry.extend_from_slice(&buffer[..n]);
        let whole = carry.len() / 4 * 4;
        // as_chunks rather than chunks_exact: the chunk size is a constant, so
        // this hands back [u8; 4] arrays and from_le_bytes takes one directly,
        // with no per-sample bounds checks and no rebuilding the array by
        // index. Stable since 1.88, which is the workspace floor.
        let (frames, _) = carry[..whole].as_chunks::<4>();
        samples.extend(frames.iter().copied().map(f32::from_le_bytes));
        carry.drain(..whole);
    }
    let status = child
        .wait()
        .map_err(|e| Error::Audio(format!("waiting for ffmpeg: {e}")))?;
    let complaints = complaints.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::Audio(format!(
            "ffmpeg could not decode {}: {}",
            path.display(),
            complaints.trim()
        )));
    }
    Ok(samples)
}

/// What a PCM container told us, enough to turn its bytes into samples.
///
/// Borrowed from the file's own bytes: copying the sample data out of a
/// buffer that already holds it was a whole second copy of the file.
struct Pcm<'a> {
    channels: u16,
    rate: u32,
    bits: u16,
    float: bool,
    big_endian: bool,
    data: &'a [u8],
}

fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::Audio(format!("cannot read {}: {e}", path.display())))?;
    let bad = |what: &str| Error::Audio(format!("{}: {what}", path.display()));
    let mut at = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format tag, channels, rate, bits)
    let mut data: Option<&[u8]> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        match id {
            b"fmt " if body.len() >= 16 => {
                let tag = u16::from_le_bytes([body[0], body[1]]);
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                // WAVE_FORMAT_EXTENSIBLE carries the real tag in its GUID.
                let tag = if tag == 0xFFFE && body.len() >= 26 {
                    u16::from_le_bytes([body[24], body[25]])
                } else {
                    tag
                };
                fmt = Some((tag, channels, rate, bits));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + len + (len & 1);
    }
    let (tag, channels, rate, bits) = fmt.ok_or_else(|| bad("no fmt chunk"))?;
    let data = data.ok_or_else(|| bad("no data chunk"))?;
    let float = match tag {
        1 => false,
        3 => true,
        other => return Err(bad(&format!("WAV format {other} is not PCM; use ffmpeg"))),
    };
    to_mono_16k(Pcm {
        channels,
        rate,
        bits,
        float,
        big_endian: false,
        data,
    })
}

fn read_aiff(path: &Path) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::Audio(format!("cannot read {}: {e}", path.display())))?;
    let bad = |what: &str| Error::Audio(format!("{}: {what}", path.display()));
    let aifc = &bytes[8..12] == b"AIFC";
    let mut at = 12;
    let mut comm: Option<(u16, u16, u32, Vec<u8>)> = None; // channels, bits, rate, compression
    let mut data: Option<&[u8]> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let len = u32::from_be_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        match id {
            b"COMM" if body.len() >= 18 => {
                let channels = u16::from_be_bytes([body[0], body[1]]);
                let bits = u16::from_be_bytes([body[6], body[7]]);
                let rate = extended_to_u32(&body[8..18]);
                let compression = if aifc && body.len() >= 22 {
                    body[18..22].to_vec()
                } else {
                    b"NONE".to_vec()
                };
                comm = Some((channels, bits, rate, compression));
            }
            b"SSND" if body.len() >= 8 => {
                let offset = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
                data = Some(&body[(8 + offset).min(body.len())..]);
            }
            _ => {}
        }
        at += 8 + len + (len & 1);
    }
    let (channels, bits, rate, compression) = comm.ok_or_else(|| bad("no COMM chunk"))?;
    let data = data.ok_or_else(|| bad("no SSND chunk"))?;
    let (float, big_endian) = match compression.as_slice() {
        b"NONE" | b"twos" => (false, true),
        b"sowt" => (false, false),
        b"fl32" | b"FL32" => (true, true),
        other => {
            return Err(bad(&format!(
                "AIFF compression {:?} is not PCM; use ffmpeg",
                String::from_utf8_lossy(other)
            )))
        }
    };
    to_mono_16k(Pcm {
        channels,
        rate,
        bits,
        float,
        big_endian,
        data,
    })
}

/// An 80-bit IEEE extended float, as AIFF stores the sample rate.
fn extended_to_u32(b: &[u8]) -> u32 {
    let exponent = (((b[0] & 0x7F) as i32) << 8 | b[1] as i32) - 16383;
    let mantissa = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if !(0..=62).contains(&exponent) {
        return 0;
    }
    (mantissa >> (63 - exponent)) as u32
}

/// Frames converted and resampled per block when decoding a file.
const BLOCK_FRAMES: usize = 16_384;

fn to_mono_16k(pcm: Pcm<'_>) -> Result<Vec<f32>> {
    if pcm.channels == 0 || pcm.rate == 0 {
        return Err(Error::Audio(
            "audio file with no channels or no rate".into(),
        ));
    }
    let width = (pcm.bits / 8) as usize;
    if !matches!(
        (pcm.bits, pcm.float),
        (8, false) | (16, false) | (24, false) | (32, _)
    ) {
        return Err(Error::Audio(format!(
            "{}-bit {} audio is not supported; use ffmpeg",
            pcm.bits,
            if pcm.float { "float" } else { "integer" }
        )));
    }
    let sample = |b: &[u8]| -> f32 {
        let bytes: [u8; 4] = match (width, pcm.big_endian) {
            (1, _) => return (b[0] as i8) as f32 / 128.0,
            (2, true) => [0, 0, b[1], b[0]],
            (2, false) => [0, 0, b[0], b[1]],
            (3, true) => [0, b[2], b[1], b[0]],
            (3, false) => [0, b[0], b[1], b[2]],
            (_, true) => [b[3], b[2], b[1], b[0]],
            (_, false) => [b[0], b[1], b[2], b[3]],
        };
        if pcm.float {
            f32::from_le_bytes(bytes)
        } else {
            // The bytes above sit in the high end of the word, so every
            // width is already left-aligned and scales the same way.
            i32::from_le_bytes(bytes) as f32 / 2_147_483_648.0
        }
    };
    let frame = width * pcm.channels as usize;
    let frames = pcm.data.len() / frame;
    let to_mono = |f: usize| -> f32 {
        let start = f * frame;
        let mut sum = 0.0f32;
        for c in 0..pcm.channels as usize {
            let at = start + c * width;
            sum += sample(&pcm.data[at..at + width]);
        }
        sum / pcm.channels as f32
    };
    if pcm.rate == SAMPLE_RATE {
        return Ok((0..frames).map(to_mono).collect());
    }
    let core = |e: macalla_core::Error| Error::Audio(e.to_string());
    let mut resampler = Resampler::new(pcm.rate).map_err(core)?;
    let mut out = Vec::with_capacity(frames * SAMPLE_RATE as usize / pcm.rate as usize + 64);
    // A block at a time. Building the whole file as one mono buffer first
    // was another full copy at the source rate, the largest of them, and
    // handing it to the resampler in one call is what made decoding slow.
    let mut block = Vec::with_capacity(BLOCK_FRAMES);
    for first in (0..frames).step_by(BLOCK_FRAMES) {
        block.clear();
        block.extend((first..(first + BLOCK_FRAMES).min(frames)).map(to_mono));
        resampler.process_into(&block, &mut out).map_err(core)?;
    }
    resampler.finish_into(&mut out).map_err(core)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/audio")
            .join(name)
    }

    /// The fixture audio is generated, never committed (fixtures/generate.sh),
    /// so these skip loudly where it is absent, as the engine tests do.
    fn fixture_or_skip(name: &str) -> Option<std::path::PathBuf> {
        let path = fixture(name);
        if path.exists() {
            Some(path)
        } else {
            eprintln!("skipped: no {name}; run fixtures/generate.sh");
            None
        }
    }

    #[test]
    fn an_aiff_fixture_decodes_to_its_duration_at_16k() {
        // short-burst.aiff: 22050 Hz mono 16-bit big-endian, 0.593 s.
        let Some(path) = fixture_or_skip("short-burst.aiff") else {
            return;
        };
        let samples = decode_file(&path).unwrap();
        let seconds = samples.len() as f64 / SAMPLE_RATE as f64;
        assert!((seconds - 0.593).abs() < 0.02, "{seconds}");
        assert!(samples.iter().any(|s| s.abs() > 0.01), "not silent");
        // The resampler may ring a touch past full scale; not by much.
        assert!(samples.iter().all(|s| s.abs() <= 1.05));
    }

    #[test]
    fn a_wav_written_here_round_trips() {
        // 16-bit stereo at 16 kHz: 100 frames of a ramp on the left, silence
        // on the right, so the mono mix is half the ramp.
        let mut data = Vec::new();
        for i in 0..100i16 {
            data.extend_from_slice(&(i * 100).to_le_bytes());
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&64_000u32.to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        let path = std::env::temp_dir().join(format!("macalla-decode-{}.wav", std::process::id()));
        std::fs::write(&path, &wav).unwrap();
        let samples = decode_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(samples.len(), 100);
        let expected = 50.0 * 100.0 / 32768.0 / 2.0;
        assert!((samples[50] - expected).abs() < 1e-4, "{}", samples[50]);
    }

    #[test]
    fn anything_else_goes_to_ffmpeg_when_it_is_there() {
        if !ffmpeg_available() {
            eprintln!("skipped: no ffmpeg on the PATH");
            return;
        }
        // The mp3 fixture is 66.35 s according to afinfo.
        let Some(path) = fixture_or_skip("sampletext-kevin-technical.mp3") else {
            return;
        };
        let samples = decode_file(&path).unwrap();
        let seconds = samples.len() as f64 / SAMPLE_RATE as f64;
        assert!((seconds - 66.35).abs() < 0.5, "{seconds}");
    }

    #[test]
    fn a_missing_file_says_so() {
        let err = decode_file(Path::new("/nonexistent/nope.wav")).unwrap_err();
        assert!(err.to_string().contains("nope.wav"), "{err}");
    }

    /// A mono 16-bit WAV at `rate`, `seconds` long, of a 440 Hz tone.
    fn tone_wav(rate: u32, seconds: f32) -> Vec<u8> {
        let n = (rate as f32 * seconds) as usize;
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin();
            data.extend_from_slice(&((v * 16_000.0) as i16).to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        wav
    }

    #[test]
    fn a_resampled_file_keeps_its_length_and_its_last_moments() {
        // Longer than a decode block and at a rate that needs resampling, so
        // feeding the resampler in pieces is held to the length and the tail
        // that one whole-file push produced.
        let (rate, seconds) = (44_100, 3.0);
        let dir = std::env::temp_dir().join(format!(
            "macalla-decode-tone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        std::fs::write(&path, tone_wav(rate, seconds)).unwrap();
        let samples = decode_file(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let expected = (seconds * SAMPLE_RATE as f32) as usize;
        assert!(
            samples.len().abs_diff(expected) <= 64,
            "{} samples, expected about {expected}",
            samples.len()
        );
        let tail = &samples[samples.len() - 1_600..];
        let peak = tail.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.3, "the last 100 ms went quiet: peak {peak}");
    }
}
