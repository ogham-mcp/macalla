// SPDX-License-Identifier: MIT
//! The microphone: a cpal input stream filling a ring that `macalla-core`
//! drains.
//!
//! This is the device half of capture and it lives here, not in the core, for
//! this reason: `cpal` is a vendor library over a device, and
//! on Linux it pulls `alsa-sys`, which made the domain crate impossible to
//! cross-compile. Everything downstream of the ring -- downmix, resample, the
//! 16 kHz mono contract -- stays in `macalla_core::audio` and is tested from
//! fixtures with no microphone.
//!
//! Adapters here are humble (Clean Architecture): this module opens a device,
//! copies samples, and makes no decision about them.

use crate::{Error, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use macalla_core::audio::chain::CaptureChain;
use macalla_core::audio::ring::{capture_ring, CaptureSink};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// What the stream's error callback saw.
///
/// Atomics only, deliberately: cpal calls the error callback from a helper
/// thread for device faults but **from the realtime audio thread** for overload
/// notifications (`cpal-0.18.2/src/host/coreaudio/macos/mod.rs:183`). A mutex
/// or an allocation in here would be a priority-inversion bug that only shows
/// up as a glitch under load.
#[derive(Debug, Default)]
struct Faults {
    /// A [`Fatal`] discriminant, or `NONE`. Set once, read once.
    fatal: AtomicU8,
    /// Non-fatal stream errors: overloads, and reroutes that cpal handled.
    /// Counted so a dropout can be attributed rather than guessed at.
    warnings: AtomicU64,
}

/// The two ways a stream can stop being worth reading from. Both need the user
/// to do something, and they need to do different things, which is the whole
/// reason this is not one flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fatal {
    /// The stream is dead and must be rebuilt.
    Lost = 1,
    /// The OS refused the microphone.
    Denied = 2,
}

const NONE: u8 = 0;

impl Faults {
    fn record(&self, e: &cpal::Error) {
        let fatal = match e.kind() {
            // cpal's macOS backend PAUSES the audio unit and then calls us, so
            // there is nothing left to read. StreamInvalidated is what a
            // Bluetooth headset produces when it switches A2DP -> HFP and the
            // device's nominal sample rate changes underneath the stream.
            cpal::ErrorKind::StreamInvalidated
            | cpal::ErrorKind::DeviceNotAvailable
            | cpal::ErrorKind::HostUnavailable => Fatal::Lost,

            cpal::ErrorKind::PermissionDenied => Fatal::Denied,

            // DeviceChanged is explicitly NOT fatal: cpal documents that the
            // route moved, capture continues on the new device, and no rebuild
            // is required. Counting it is right; aborting on it would end a
            // dictation because someone unplugged their headphones.
            _ => {
                self.warnings.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        // First fault wins. A dead stream often emits several, and the first is
        // the one that explains the rest.
        let _ = self
            .fatal
            .compare_exchange(NONE, fatal as u8, Ordering::AcqRel, Ordering::Relaxed);
    }

    /// Report a fatal fault at most once, so a polling loop cannot spin on it.
    fn take_fatal(&self) -> Option<Fatal> {
        match self.fatal.swap(NONE, Ordering::AcqRel) {
            x if x == Fatal::Lost as u8 => Some(Fatal::Lost),
            x if x == Fatal::Denied as u8 => Some(Fatal::Denied),
            _ => None,
        }
    }

    fn warnings(&self) -> u64 {
        self.warnings.load(Ordering::Relaxed)
    }
}

/// A live microphone: a cpal input stream writing into a ring, with
/// [`CaptureChain`](macalla_core::audio::chain::CaptureChain) on the other end.
///
/// The stream stops when this is dropped. Note that cpal's `Stream` is not
/// `Send` on every platform, so build a `Capture` on the thread that will poll
/// it -- which is the pipeline thread, never the UI thread and never the audio
/// callback.
pub struct Capture {
    /// Held only so that dropping `Capture` stops the device.
    _stream: cpal::Stream,
    chain: CaptureChain,
    faults: Arc<Faults>,
    playing: bool,
    device_name: String,
    input_rate: u32,
    channels: u16,
    /// Kept so the stream can be rebuilt on the same device when its sample
    /// rate moves underneath us. See [`recovery_for`].
    dev: cpal::Device,
    ring_seconds: f32,
    /// How many times the stream was rebuilt mid-capture. Non-zero means a
    /// short gap in the audio around the rebuild.
    reopens: u64,
}

/// What to do about a fatal stream fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// The device is still there and now runs at this rate: it changed its
    /// sample rate, which is what a Bluetooth headset does when the
    /// microphone activates (A2DP at 48 kHz to HFP at 16 kHz). cpal
    /// invalidates the stream; rebuild it at the new rate and carry on.
    Reopen { rate: u32 },
    /// Tell the caller. The device is gone, or the OS refused it.
    Report,
}

/// Decide from the fault and the device's rate now against the rate the
/// stream was built for. Pure, so the rule is tested without a device: the
/// first version of this file sized the resampler once, at open, and the
/// task that found it noted that a slower headset would have produced audio
/// at the wrong speed and a transcript of nonsense.
fn recovery_for(fault: Fatal, built_rate: u32, rate_now: Option<u32>) -> Recovery {
    match (fault, rate_now) {
        (Fatal::Lost, Some(rate)) if rate != built_rate => Recovery::Reopen { rate },
        _ => Recovery::Report,
    }
}

/// The parts of a capture that a rebuild replaces.
struct Built {
    stream: cpal::Stream,
    chain: CaptureChain,
    faults: Arc<Faults>,
    input_rate: u32,
    channels: u16,
}

/// Build a stream and its chain for the device's current default config.
fn build_for(dev: &cpal::Device, device_name: &str, ring_seconds: f32) -> Result<Built> {
    let supported = dev
        .default_input_config()
        .map_err(|e| Error::Audio(format!("{device_name}: default input config: {e}")))?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let input_rate = config.sample_rate;
    let channels = config.channels;

    let frames = (ring_seconds.max(1.0) * input_rate as f32) as usize;
    let (sink, source) = capture_ring(frames * channels as usize, channels);
    let chain = CaptureChain::new(source, channels, input_rate)
        .map_err(|e| Error::Audio(format!("{device_name}: {e}")))?;

    // NOT just a log line. On macOS cpal watches the device's nominal
    // sample rate and, when it changes, PAUSES the audio unit and reports
    // StreamInvalidated ("Device sample rate changed"). A Bluetooth headset
    // switching A2DP -> HFP does exactly that, mid-dictation. Logging it at
    // warn and carrying on means the user sees "no speech was detected" and
    // is never told the microphone stopped.
    let faults = Arc::new(Faults::default());
    let on_error = {
        let faults = Arc::clone(&faults);
        move |e: cpal::Error| {
            tracing::warn!(error = %e, kind = ?e.kind(), "audio input stream error");
            faults.record(&e);
        }
    };
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream::<f32>(dev, config, sink, on_error),
        cpal::SampleFormat::I16 => build_stream::<i16>(dev, config, sink, on_error),
        cpal::SampleFormat::I32 => build_stream::<i32>(dev, config, sink, on_error),
        cpal::SampleFormat::I8 => build_stream::<i8>(dev, config, sink, on_error),
        cpal::SampleFormat::U8 => build_stream::<u8>(dev, config, sink, on_error),
        other => Err(Error::Audio(format!(
            "{device_name}: unsupported sample format {other}"
        ))),
    }?;
    tracing::debug!(
        device = %device_name,
        rate = input_rate,
        channels,
        format = %sample_format,
        "capture stream built"
    );
    Ok(Built {
        stream,
        chain,
        faults,
        input_rate,
        channels,
    })
}

impl Capture {
    /// Open an input device and start capturing.
    ///
    /// `device` is a name from [`input_device_names`], or `None` for the system
    /// default. `ring_seconds` is how far the pipeline thread may fall behind
    /// before samples are lost; 30 is about 11 MB at 48 kHz stereo and is a
    /// sane default against the 400 MB idle budget.
    /// Open an input device and start capturing, in one step.
    ///
    /// Equivalent to [`Capture::open`] followed by [`Capture::play`]. Use it
    /// where there is nothing to overlap the cost with; a trigger that knows in
    /// advance that a dictation is coming should open early and play late.
    pub fn start(device: Option<&str>, ring_seconds: f32) -> Result<Self> {
        let mut capture = Self::open(device, ring_seconds)?;
        capture.play()?;
        Ok(capture)
    }

    /// Build the input stream **without starting it**.
    ///
    /// This is the expensive half and the whole reason the two are separable.
    /// Measured 2026-09-08 on a cold Sony WH-1000XM2 over Bluetooth: building
    /// the stream took 2788.5 ms and playing it took 139.8 ms, so 95% of the
    /// cost of opening a microphone can be paid before the user presses
    /// anything. On a built-in microphone the whole thing is ~115 ms and this
    /// distinction does not matter.
    ///
    /// **Observed 2026-09-08, and the prewarm depends on it: an armed stream
    /// does NOT light the macOS recording indicator.** The dot stayed dark
    /// while the stream was built and not playing, and lit as soon as it
    /// played -- that second half is the control, and without it "I saw no
    /// dot" would not be distinguishable from "I was looking at the wrong
    /// thing".
    ///
    /// This is load bearing. The reason capture is opened per-dictation rather
    /// than at startup is so the orange dot means "recording" and not
    /// "installed". If a future macOS lights it on an armed stream, the prewarm
    /// goes, not the indicator.
    ///
    /// The ignored test at the bottom of this file re-makes the observation,
    /// and it waits for a keypress rather than a timer because the whole point
    /// is that a person has to look.
    ///
    /// An armed stream that is never played still stops when it is dropped.
    pub fn open(device: Option<&str>, ring_seconds: f32) -> Result<Self> {
        let host = cpal::default_host();
        let dev = match device {
            Some(want) => host
                .input_devices()
                .map_err(|e| Error::Audio(format!("listing input devices: {e}")))?
                .find(|d| {
                    d.description()
                        .map(|desc| desc.name() == want)
                        .unwrap_or(false)
                })
                .ok_or_else(|| Error::Audio(format!("no input device named {want:?}")))?,
            None => host
                .default_input_device()
                .ok_or_else(|| Error::Audio("no default input device".into()))?,
        };
        let device_name = dev
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "unknown".into());
        let built = build_for(&dev, &device_name, ring_seconds)?;
        tracing::debug!(device = %device_name, "capture armed (stream built, not playing)");
        Ok(Self {
            _stream: built.stream,
            chain: built.chain,
            faults: built.faults,
            playing: false,
            device_name,
            input_rate: built.input_rate,
            channels: built.channels,
            dev,
            ring_seconds,
            reopens: 0,
        })
    }

    /// The device's default input rate now, if it can be asked.
    fn rate_now(&self) -> Option<u32> {
        self.dev
            .default_input_config()
            .ok()
            .map(|c| c.sample_rate())
    }

    /// Rebuild the stream at the device's new rate and resume. What the old
    /// chain still held goes into `out` first, so nothing captured before
    /// the switch is lost; the switch itself costs a short gap.
    fn reopen(&mut self, out: &mut Vec<f32>, rate: u32) -> Result<()> {
        let _ = self.chain.finish_into(out);
        let built = build_for(&self.dev, &self.device_name, self.ring_seconds)?;
        tracing::warn!(
            device = %self.device_name,
            from = self.input_rate,
            to = rate,
            "the microphone changed its sample rate; stream rebuilt"
        );
        self._stream = built.stream;
        self.chain = built.chain;
        self.faults = built.faults;
        self.input_rate = built.input_rate;
        self.channels = built.channels;
        self.reopens += 1;
        if self.playing {
            self._stream.play().map_err(|e| {
                Error::Audio(format!("{}: restarting the stream: {e}", self.device_name))
            })?;
        }
        Ok(())
    }

    /// Start the device. Cheap if [`Capture::open`] already ran, which is the
    /// point: this is what a keypress pays for.
    ///
    /// Calling it twice is a no-op rather than an error, so a caller does not
    /// have to track whether a capture was armed or started.
    pub fn play(&mut self) -> Result<()> {
        if self.playing {
            return Ok(());
        }
        self._stream
            .play()
            .map_err(|e| Error::Audio(format!("{}: starting the stream: {e}", self.device_name)))?;
        self.playing = true;
        tracing::info!(device = %self.device_name, rate = self.input_rate, "capture started");
        Ok(())
    }

    /// Drain whatever the callback has produced, as 16 kHz mono.
    ///
    /// Note the order: whatever the ring held is appended to `out` FIRST, and
    /// only then is a fatal device fault reported. So an `Err` here still
    /// leaves the audio captured up to that point in `out`, and the caller can
    /// transcribe it rather than throwing away a part-finished dictation. The
    /// fault is reported once and then cleared, so a polling loop does not spin
    /// on it.
    pub fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
        let n = self.chain.poll_into(out).map_err(resample_failed)?;
        if let Some(f) = self.faults.take_fatal() {
            match recovery_for(f, self.input_rate, self.rate_now()) {
                Recovery::Reopen { rate } => self.reopen(out, rate)?,
                Recovery::Report => return Err(self.fatal_error(f)),
            }
        }
        Ok(n)
    }

    /// Stop capturing and flush the tail.
    pub fn finish_into(mut self, out: &mut Vec<f32>) -> Result<usize> {
        let n = self.chain.finish_into(out).map_err(resample_failed)?;
        if let Some(f) = self.faults.take_fatal() {
            return Err(self.fatal_error(f));
        }
        Ok(n)
    }

    /// A denied microphone gets the permission error, so onboarding can send
    /// the user to System Settings instead of reporting a generic audio fault.
    fn fatal_error(&self, f: Fatal) -> Error {
        match f {
            Fatal::Lost => Error::DeviceLost(self.device_name.clone()),
            Fatal::Denied => Error::Permission {
                permission: crate::Permission::Microphone,
            },
        }
    }

    /// Non-fatal stream errors since the device opened: overloads, and route
    /// changes cpal absorbed. Non-zero means a gap in the audio the user may
    /// hear as a missing word -- distinct from [`Capture::overruns`], which is
    /// this process falling behind rather than the device.
    pub fn stream_warnings(&self) -> u64 {
        // A rebuild is a gap too, and the faults it counted went with the
        // old stream.
        self.faults.warnings() + self.reopens
    }

    /// How many times the stream was rebuilt because the device changed its
    /// sample rate mid-capture.
    pub fn reopens(&self) -> u64 {
        self.reopens
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Samples lost because the pipeline thread fell behind. Non-zero means a
    /// dropout the user probably heard as a missing word.
    pub fn overruns(&self) -> u64 {
        self.chain.overruns()
    }
}

/// The chain reports core errors (a resampler that could not run). Out here
/// they are just audio failures, so they carry across as one variant rather
/// than adding a `From` that would make every core error look like an audio
/// one.
fn resample_failed(e: macalla_core::Error) -> Error {
    Error::Audio(e.to_string())
}

/// Samples in the callback's conversion buffer.
const CONVERT: usize = 1_024;

/// How many samples the callback converts and writes at a time: the most
/// whole frames of `channels` that fit in [`CONVERT`]. The ring takes whole
/// frames, and 1,024 is not a multiple of three or six channels.
fn chunk_step(channels: u16) -> usize {
    let frame = (channels as usize).clamp(1, CONVERT);
    CONVERT / frame * frame
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut sink: CaptureSink,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    // The whole of the audio callback. It converts to f32 through a fixed
    // stack buffer and copies into the ring: no allocation, no locking, no
    // downmixing, no inference.
    let step = chunk_step(config.channels);
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut buf = [0.0f32; CONVERT];
                for chunk in data.chunks(step) {
                    for (dst, src) in buf.iter_mut().zip(chunk) {
                        *dst = src.to_sample::<f32>();
                    }
                    sink.write(&buf[..chunk.len()]);
                }
            },
            on_error,
            None,
        )
        .map_err(|e| Error::Audio(format!("building the input stream: {e}")))
}

/// The name of the device the system would hand over for input right now.
/// Cheap enough to ask every couple of seconds: a headset coming or going
/// moves the default, and whoever armed a microphone earlier is still
/// holding the old one.
pub fn default_input_device_name() -> Result<String> {
    let host = cpal::default_host();
    let dev = host
        .default_input_device()
        .ok_or_else(|| Error::Audio("no default input device".into()))?;
    dev.description()
        .map(|d| d.name().to_string())
        .map_err(|e| Error::Audio(format!("naming the default input device: {e}")))
}

/// Every input device the default host can see, by name.
pub fn input_device_names() -> Result<Vec<String>> {
    let host = cpal::default_host();
    Ok(host
        .input_devices()
        .map_err(|e| Error::Audio(format!("listing input devices: {e}")))?
        .filter_map(|d| d.description().ok().map(|desc| desc.name().to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::audio::SAMPLE_RATE;

    #[test]
    fn the_callback_writes_whole_frames_at_a_time() {
        assert_eq!(chunk_step(1), 1_024);
        assert_eq!(chunk_step(2), 1_024);
        assert_eq!(chunk_step(3), 1_023);
        assert_eq!(chunk_step(6), 1_020);
    }

    #[test]
    fn a_three_channel_buffer_reaches_the_ring_whole() {
        // What the callback does with a device buffer, minus the sample
        // conversion: cut at the step, write each piece.
        let (mut sink, mut source) = capture_ring(1 << 16, 3);
        let buffer: Vec<f32> = (0..3 * 1_000).map(|i| i as f32).collect();
        for piece in buffer.chunks(chunk_step(3)) {
            sink.write(piece);
        }
        let mut out = Vec::new();
        source.read_into(&mut out);
        assert_eq!(source.overruns(), 0, "no part-frame dropped");
        assert_eq!(out, buffer);
    }

    /// cpal's `Error` is opaque enough that these have to be built through its
    /// own constructor, which is also what proves the `kind()` match arms in
    /// `Faults::record` name real variants.
    fn err(kind: cpal::ErrorKind) -> cpal::Error {
        cpal::Error::with_message(kind, "test")
    }

    #[test]
    fn a_sample_rate_change_is_fatal_and_is_reported_exactly_once() {
        // The Bluetooth A2DP -> HFP case. cpal has already paused the audio
        // unit by the time the callback runs, so anything short of telling the
        // caller leaves them with silence and no explanation.
        let f = Faults::default();
        f.record(&err(cpal::ErrorKind::StreamInvalidated));

        assert_eq!(f.take_fatal(), Some(Fatal::Lost));
        assert_eq!(f.take_fatal(), None, "or a polling loop spins on it");
    }

    #[test]
    fn a_rate_change_reopens_and_anything_else_is_reported() {
        // The Bluetooth case: built at 48 kHz in A2DP, the microphone
        // activates, the headset drops to 16 kHz in HFP, cpal invalidates the
        // stream. The device is still there; rebuild for the new rate.
        assert_eq!(
            recovery_for(Fatal::Lost, 48_000, Some(16_000)),
            Recovery::Reopen { rate: 16_000 }
        );
        // Same rate: the stream died for another reason, and a rebuild
        // would only hide it.
        assert_eq!(
            recovery_for(Fatal::Lost, 48_000, Some(48_000)),
            Recovery::Report
        );
        // The device cannot even be asked: it is gone.
        assert_eq!(recovery_for(Fatal::Lost, 48_000, None), Recovery::Report);
        // A refusal is never a rate problem.
        assert_eq!(
            recovery_for(Fatal::Denied, 48_000, Some(16_000)),
            Recovery::Report
        );
    }

    #[test]
    fn an_unplugged_device_and_a_dead_host_are_fatal_too() {
        for kind in [
            cpal::ErrorKind::DeviceNotAvailable,
            cpal::ErrorKind::HostUnavailable,
        ] {
            let f = Faults::default();
            f.record(&err(kind));
            assert_eq!(f.take_fatal(), Some(Fatal::Lost), "{kind:?}");
        }
    }

    #[test]
    fn a_denied_microphone_is_a_permission_problem_not_an_audio_one() {
        // It has somewhere to send the user, which no other fault does.
        let f = Faults::default();
        f.record(&err(cpal::ErrorKind::PermissionDenied));
        assert_eq!(f.take_fatal(), Some(Fatal::Denied));
    }

    #[test]
    fn a_reroute_is_counted_but_does_not_stop_the_dictation() {
        // cpal documents DeviceChanged as "capture continues, no rebuild
        // required". Ending a dictation because someone unplugged their
        // headphones mid-sentence would be worse than the dropout.
        let f = Faults::default();
        f.record(&err(cpal::ErrorKind::DeviceChanged));
        f.record(&err(cpal::ErrorKind::DeviceBusy));

        assert_eq!(f.warnings(), 2);
        assert_eq!(f.take_fatal(), None);
    }

    #[test]
    fn the_first_fatal_fault_wins() {
        // A dying stream emits several; the first is the one that explains the
        // rest, so a later generic failure must not overwrite the diagnosis.
        let f = Faults::default();
        f.record(&err(cpal::ErrorKind::PermissionDenied));
        f.record(&err(cpal::ErrorKind::DeviceNotAvailable));
        assert_eq!(f.take_fatal(), Some(Fatal::Denied));
    }

    #[test]
    fn a_healthy_stream_reports_nothing() {
        let f = Faults::default();
        assert_eq!(f.take_fatal(), None);
        assert_eq!(f.warnings(), 0);
    }

    /// Wait for the person running this to look and press Enter.
    ///
    /// Falls back to a sleep when stdin is not a terminal, so the test still
    /// completes if it is run from a script or a harness that closes stdin --
    /// but the observation it exists for needs a human, so run it yourself.
    fn wait_for_a_human(prompt: &str) {
        use std::io::Write;
        print!("\n   >>> {prompt}\n   >>> press Enter when you have looked: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                println!("(no terminal on stdin -- falling back to an 8 second pause)");
                std::thread::sleep(std::time::Duration::from_secs(8));
            }
            Ok(_) => {}
        }
    }

    /// The measurement that decides how the Bluetooth cold open gets fixed.
    ///
    /// ```text
    /// cargo test -p macalla-platform --lib -- --ignored --nocapture where_the_bluetooth
    /// ```
    ///
    /// **Pair a Bluetooth headset and make it the default input first**, and
    /// leave it idle for a minute so the link is cold. On the built-in
    /// microphone this costs ~115 ms and proves nothing.
    ///
    /// It separates two things that `Capture::start` does together -- BUILDING
    /// the input stream and PLAYING it -- and times each, because the whole
    /// prewarm question is whether the three seconds sits in the half that can
    /// be moved off the keypress.
    ///
    /// It also needs a human, which is why it can never run in CI: the second
    /// question is whether an open-but-not-playing stream lights the macOS
    /// recording indicator. If it does, prewarming costs the honesty of the
    /// indicator, which is the reason the device is opened per-press at all.
    /// Nothing in Rust can read that dot; you have to look at it.
    #[test]
    #[ignore = "needs a Bluetooth microphone and a human watching the menu bar"]
    fn where_the_bluetooth_cold_open_cost_actually_lands() {
        use std::time::Instant;

        let host = cpal::default_host();
        let dev = host.default_input_device().expect("a default input device");
        let name = dev
            .description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "unknown".into());

        println!("\n=== device: {name} ===");
        println!(
            "The recording indicator is an orange dot at the RIGHT-HAND END of the\n\
             menu bar, beside the Control Center icon. Opening Control Center also\n\
             names the app using the microphone, which is the surer check."
        );
        wait_for_a_human("Baseline: nothing is open yet. The dot should be OFF.");

        // 1. Reading the format. another dictation app polls this for up to three seconds
        //    waiting for a Bluetooth device to settle out of A2DP, so the rate
        //    printed here is the one worth watching.
        let t = Instant::now();
        let supported = dev.default_input_config().expect("default input config");
        let read_ms = t.elapsed().as_secs_f64() * 1000.0;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        println!(
            "1. read format:  {read_ms:8.1} ms  -> {} Hz, {} ch, {sample_format}",
            config.sample_rate, config.channels
        );

        // 2. Building the stream. If the cost is here, prewarming can move it.
        let (sink, _source) = capture_ring(48_000 * 4, config.channels);
        let t = Instant::now();
        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_stream::<f32>(&dev, config, sink, |_| {}),
            cpal::SampleFormat::I16 => build_stream::<i16>(&dev, config, sink, |_| {}),
            cpal::SampleFormat::I32 => build_stream::<i32>(&dev, config, sink, |_| {}),
            other => panic!("unhandled sample format {other} -- extend this probe"),
        }
        .expect("build the stream");
        let build_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("2. build stream: {build_ms:8.1} ms   (NOT playing yet)");

        wait_for_a_human(
            "THIS IS THE ONE THAT MATTERS. The stream is BUILT but NOT PLAYING.\n\
   >>> Is the orange dot lit? Dark here means a prewarm is free.\n\
   >>> Lit here means a prewarm spends the indicator's honesty.",
        );

        // 3. Playing it. If the cost is here, prewarming does NOT help and the
        //    three seconds has to be waited for or surfaced instead.
        let t = Instant::now();
        stream.play().expect("play");
        let play_ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("3. play stream:  {play_ms:8.1} ms");

        wait_for_a_human(
            "Control: the stream is PLAYING now, so the dot must be lit.\n\
   >>> If it is dark here too, the indicator is not telling you anything\n\
   >>> and the observation above proves nothing.",
        );

        // 4. Whether the rate moved under us, which is the latent half of the
        //    same bug. A different number here than line 1 is the A2DP -> HFP
        //    switch happening after we committed to a resampler.
        let after = dev
            .default_input_config()
            .expect("re-read config")
            .sample_rate();
        println!(
            "\n4. rate now:     {after} Hz (was {} Hz)",
            config.sample_rate
        );

        println!(
            "\n=== verdict ===\n\
             build {build_ms:.0} ms vs play {play_ms:.0} ms.\n\
             If BUILD dominates, prewarm at arm time and the keypress gets cheap.\n\
             If PLAY dominates, prewarming buys nothing and the wait has to be\n\
             surfaced to the user instead.\n\
             Record both numbers, and the dot, in docs/benchmarks.md."
        );
    }

    /// The one test that needs a microphone, so it never runs in CI.
    ///
    /// ```text
    /// cargo test -p macalla-platform --lib -- --ignored --nocapture opens_the_default
    /// ```
    ///
    /// On macOS the terminal running it needs the Microphone permission, and
    /// the first run will prompt for it.
    #[test]
    #[ignore = "needs a real microphone"]
    fn opens_the_default_device_and_captures_a_second_of_audio() {
        let names = input_device_names().expect("list devices");
        println!("input devices: {names:?}");

        let mut cap = Capture::start(None, 30.0).expect("start capture");
        println!(
            "capturing from {:?} at {} Hz, {} channel(s)",
            cap.device_name(),
            cap.input_rate(),
            cap.channels()
        );

        let mut out = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < until {
            cap.poll_into(&mut out).expect("poll");
        }
        let overruns = cap.overruns();
        cap.finish_into(&mut out).expect("finish");

        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len().max(1) as f32).sqrt();
        println!(
            "{} samples at 16 kHz ({:.2} s), peak {peak:.4}, rms {rms:.4} ({:.1} dB), overruns {overruns}",
            out.len(),
            out.len() as f32 / SAMPLE_RATE as f32,
            20.0 * rms.max(1e-9).log10(),
        );

        let drift = (out.len() as i64 - SAMPLE_RATE as i64).abs();
        assert!(
            drift < 2_000,
            "expected ~1 s of audio, got {} samples",
            out.len()
        );
        assert_eq!(
            overruns, 0,
            "the pipeline thread must keep up with the callback"
        );
    }
}
