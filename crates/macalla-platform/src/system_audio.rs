// SPDX-License-Identifier: MIT
//! System audio: what this Mac is playing, heard through a Core Audio
//! process tap, as the call's side of a live meeting.
//!
//! The same shape as the microphone in [`crate::audio`]: a callback that
//! copies samples into a ring, and [`CaptureChain`] on the other end turning
//! them into 16 kHz mono. The downmix, the resample and that contract stay in
//! `macalla_core::audio`. Not called `tap`: `tap.rs` is the keyboard's event
//! tap.
//!
//! Humble (Clean Architecture): this opens the tap, copies samples, and
//! decides nothing about them.
//!
//! - **Never this process's output.** The tap is every process's output
//!   mixed to stereo, except this process's, so a sound this process plays
//!   is not heard as the call. Only this process is left out: in the desktop
//!   app a sound played by the web view is played by one of WebKit's helper
//!   processes, not by this one, and is heard.
//! - **The call stays audible.** The tap is unmuted: capturing a process's
//!   output does not take it away from the person's speakers.
//! - **A refused tap is silent, not an error.** macOS asks for "system audio
//!   recording" the first time the tap starts. Refused, the tap opens and
//!   starts as if granted and delivers correctly-clocked zeros. Nothing here
//!   treats zeros as a fault; whoever reads the samples judges by signal.
//! - **No entitlement beyond the usage string.** Observed on 2026-09-23 on a
//!   Mac mini (Apple M1, macOS 27.0), in a Macalla Dev-signed build with the
//!   hardened runtime on: the prompt appeared, was granted, and the tap
//!   delivered what the speakers played. The app's `Info.plist` carries
//!   `NSAudioCaptureUsageDescription`; nothing in its entitlements is for
//!   the tap.
//!
//! Peaks above full scale arrive and pass through untouched: 1.28 was
//! measured from a tap on 2026-09-22. Nothing here clamps them. Whether each
//! engine tolerates them has not been tested.
//!
//! macOS 14.2 or later, where process taps exist; the app requires macOS 26.
//! The tap's symbols are linked directly, so a binary holding this module
//! does not load on an older macOS at all, rather than reporting anything.
//! On every other platform [`SystemAudio::open`] is
//! [`OpenError::Unsupported`].

use macalla_core::audio::chain::CaptureChain;
use macalla_core::audio::ring::{capture_ring, CaptureSink};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Why system audio could not be opened. A live meeting with no system
/// audio is heard from the microphone alone, so each of these is a reason to
/// report, not a failed meeting.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// This platform has no system audio capture here.
    #[error("system audio capture is not available on this platform")]
    Unsupported,

    /// Core Audio found no audio object for this process, so its output could
    /// not be left out of the tap. Not opened rather than risk hearing this
    /// process's own sounds as the call.
    #[error(
        "Core Audio has no audio object for this process, so its own output cannot be excluded"
    )]
    OwnProcessUnknown,

    /// A Core Audio call failed.
    #[error("Core Audio failed at {step:?}: OSStatus {status}")]
    CoreAudio { step: Step, status: i32 },

    /// The tap delivers audio in a format this does not read.
    #[error("the tap's format is not one this reads: {0:?}")]
    Format(TapFormat),

    /// The resampler could not be built for the tap's rate.
    #[error(transparent)]
    Chain(#[from] macalla_core::Error),
}

/// What ends a capture that is running.
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The format the aggregate device's input delivers is no longer the one
    /// the capture was built for: an output device dropping to a Bluetooth
    /// headset's call rate would do it. Core Audio's notice only prompts a
    /// re-read; this is reported when the re-read disagrees, or fails.
    /// Audio from then on would be resampled at the wrong rate and come out
    /// the wrong speed, so the capture ends here, keeping what came before.
    /// Nothing rebuilds the chain at the new rate yet.
    ///
    /// "What came before" is up to one poll too generous: the reader drains
    /// the ring before it looks at the notice, so up to one poll of audio at
    /// the new rate, heard at the old one, can be in what is kept.
    #[error("the system audio format changed during the capture")]
    FormatChanged,

    /// The resampler failed.
    #[error(transparent)]
    Chain(#[from] macalla_core::Error),
}

/// The Core Audio call that failed, in the order they are made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Finding this process's audio object, to leave it out.
    FindOwnProcess,
    CreateTap,
    ReadTapUid,
    ReadTapFormat,
    CreateAggregateDevice,
    /// Listening for a change to the tap's format or the device's rate.
    Listen,
    /// Reading the format the aggregate device's input stream delivers.
    ReadStreamFormat,
    CreateIoProc,
    /// Starting the aggregate device: the call that asks for permission.
    Start,
}

/// The tap's stream format, as Core Audio reports it: the parts this reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TapFormat {
    pub sample_rate: f64,
    pub channels: u32,
    pub bits_per_channel: u32,
    /// Linear PCM, not a compressed format.
    pub linear_pcm: bool,
    pub float: bool,
    /// One buffer per channel rather than one buffer of interleaved frames.
    pub non_interleaved: bool,
}

/// The most channels a tap may have. The tap is built as a stereo mixdown, so
/// two is what arrives; this bounds the callback's stack buffer.
const MAX_CHANNELS: u32 = 8;

/// What the ring and the chain are built for, and what the callback
/// expects each cycle to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    rate: u32,
    channels: u16,
    /// One buffer of interleaved frames, rather than one buffer per channel.
    interleaved: bool,
}

/// The chain's configuration for a tap format, or why it cannot be read:
/// 32-bit float linear PCM, one to [`MAX_CHANNELS`] channels, at a whole
/// positive rate. Pure, so the rule is tested without a tap.
fn layout(format: &TapFormat) -> Result<Layout, OpenError> {
    let rate = format.sample_rate;
    let readable = format.linear_pcm
        && format.float
        && format.bits_per_channel == 32
        && (1..=MAX_CHANNELS).contains(&format.channels)
        && rate.is_finite()
        && rate >= 1.0
        && rate <= u32::MAX as f64
        && rate.fract() == 0.0;
    if !readable {
        return Err(OpenError::Format(*format));
    }
    Ok(Layout {
        rate: rate as u32,
        channels: format.channels as u16,
        interleaved: !format.non_interleaved,
    })
}

/// A ring holding `ring_seconds` of the tap's audio, and the chain that
/// drains it to 16 kHz mono. Sized as the microphone's is. What the ring
/// drops when it is full comes back out of the chain as zeros, so the
/// source keeps its clock.
fn chain_for(layout: Layout, ring_seconds: f32) -> Result<(CaptureSink, CaptureChain), OpenError> {
    let frames = (ring_seconds.max(1.0) * layout.rate as f32) as usize;
    let (sink, source) = capture_ring(frames * layout.channels as usize, layout.channels);
    let chain = CaptureChain::new(source, layout.channels, layout.rate)?.filling_overruns();
    Ok((sink, chain))
}

/// The longest gap owed as silence, in seconds. A longer one, a sleep and
/// wake mid-meeting, is owed this much and no more, and the Others voice
/// then runs behind the wall clock by the rest of the gap. It bounds what
/// the reader puts back in one go.
const MAX_GAP_SECONDS: u32 = 10;

/// How much more than [`MAX_GAP_SECONDS`] the ring holds at the least, in
/// seconds: the reader puts a counted gap back up to one poll late, and can
/// be a push behind besides (a piece is transcribed inside the push that
/// closes it), so the speech that follows a long gap needs room of its own.
const RING_MARGIN_SECONDS: u32 = 5;

/// The smallest ring [`SystemAudio::open`] builds, in seconds: the longest
/// gap owed as silence and a margin. A smaller ring could be filled by a
/// gap's zeros, and the real speech after them would be dropped as
/// overruns. A caller asking for less gets this.
pub const MIN_RING_SECONDS: f32 = (MAX_GAP_SECONDS + RING_MARGIN_SECONDS) as f32;

/// The ring's size for a caller that asked for `asked` seconds: never below
/// [`MIN_RING_SECONDS`].
fn ring_seconds_for(asked: f32) -> f32 {
    asked.max(MIN_RING_SECONDS)
}

/// The most of a gap the callback writes into the ring itself, in seconds:
/// at 48 kHz stereo, 384 KB of zeros, filled in place with no copy. The
/// rest of the gap is counted in one step and the reader puts it back
/// ([`CaptureSink::skip_frames`]), so the audio thread never spends ten
/// seconds' worth of zeros in one cycle, and a gap never fills the ring
/// and turns the speech after it into overruns. The part put back by the
/// reader can land up to one poll late.
const IN_BAND_GAP_SECONDS: u32 = 1;

/// Where the tap's sample clock should be at the start of the next cycle.
///
/// Core Audio stamps each cycle's input with its sample time. A cycle that
/// starts later than the last one ended means cycles were skipped: the
/// output device was reconfigured (AirPods connecting, the default output
/// changing), the IO thread overloaded, or the Mac slept. Those frames are
/// owed as zeros, so every later sample keeps its place in the meeting.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Clock {
    next: Option<f64>,
}

impl Clock {
    /// The zero frames owed before a cycle stamped `sample_time` (when its
    /// stamp says the sample time is valid) holding `frames` frames, at most
    /// `max_gap`.
    ///
    /// - A jump forward of at least one frame is owed, up to `max_gap`.
    /// - A jump backward (a device that restarted its count) owes nothing:
    ///   how long it was silent cannot be told from the sample time. The
    ///   clock starts again from the new stamp.
    /// - A cycle with no valid stamp owes nothing and moves the clock on by
    ///   its frames.
    fn advance(&mut self, sample_time: Option<f64>, frames: usize, max_gap: usize) -> usize {
        let owed = match (self.next, sample_time) {
            (Some(next), Some(at)) => {
                let gap = (at - next).round();
                if gap >= 1.0 {
                    gap.min(max_gap as f64) as usize
                } else {
                    0
                }
            }
            _ => 0,
        };
        self.next = match sample_time {
            Some(at) => Some(at + frames as f64),
            None => self.next.map(|next| next + frames as f64),
        };
        owed
    }
}

/// One buffer of a cycle, as Core Audio describes it. `bytes` is set even
/// when `data` is absent, which is how a disabled stream is marked.
#[derive(Debug, Clone, Copy)]
struct Buffer<'a> {
    channels: u32,
    bytes: u32,
    data: Option<&'a [f32]>,
}

/// Frames in a cycle, from the first buffer's size and the layout: one
/// buffer of all the channels, or one channel per buffer.
fn cycle_frames(buffers: &[Buffer<'_>], layout: Layout) -> usize {
    let per_frame = if layout.interleaved {
        layout.channels as usize
    } else {
        1
    };
    buffers.first().map_or(0, |b| {
        b.bytes as usize / std::mem::size_of::<f32>() / per_frame
    })
}

/// The whole of the audio callback: zeros for any cycles skipped since the
/// last one, then this cycle's frames, interleaved, into the ring. No
/// allocation, no locking, no downmixing, no clamping.
///
/// The buffers must be exactly what `layout` says: one buffer of all the
/// channels, or one per channel, each with its data. Anything else, a
/// missing buffer or a wrong channel count, is not guessed at: the cycle is
/// written as that many frames of zeros, so the clock holds.
///
/// Only the macOS callback calls it; elsewhere only its tests do.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn write_cycle(
    clock: &mut Clock,
    sample_time: Option<f64>,
    buffers: &[Buffer<'_>],
    layout: Layout,
    sink: &mut CaptureSink,
) {
    let channels = layout.channels as usize;
    let frames = cycle_frames(buffers, layout);
    let max_gap = layout.rate as usize * MAX_GAP_SECONDS as usize;
    let in_band = layout.rate as usize * IN_BAND_GAP_SECONDS as usize;
    sink.skip_frames(clock.advance(sample_time, frames, max_gap), in_band);

    let whole = |b: &Buffer<'_>, per_frame: usize| {
        b.channels as usize == per_frame && b.data.is_some_and(|d| d.len() == frames * per_frame)
    };
    let as_expected = if layout.interleaved {
        buffers.len() == 1 && whole(&buffers[0], channels)
    } else {
        buffers.len() == channels && buffers.iter().all(|b| whole(b, 1))
    };
    if !as_expected {
        sink.skip_frames(frames, frames);
        return;
    }
    if layout.interleaved || channels == 1 {
        if let Some(data) = buffers[0].data {
            sink.write(data);
        }
        return;
    }
    const FRAMES: usize = 256;
    let mut buf = [0.0f32; FRAMES * MAX_CHANNELS as usize];
    let mut at = 0;
    while at < frames {
        let count = FRAMES.min(frames - at);
        for (c, buffer) in buffers.iter().enumerate() {
            if let Some(data) = buffer.data {
                for frame in 0..count {
                    buf[frame * channels + c] = data[at + frame];
                }
            }
        }
        sink.write(&buf[..count * channels]);
        at += count;
    }
}

/// What this Mac is playing, less this process's own output, as a live
/// source.
///
/// The same contract as the microphone's [`crate::audio::Capture`]: open,
/// play, drain as 16 kHz mono, finish. Dropping it stops the tap and tears
/// down everything it built, in reverse order.
pub struct SystemAudio {
    reader: Reader,
    input_rate: u32,
    channels: u16,
    /// Declared after `reader`, so the chain goes first; the order does not
    /// matter for safety, as the two ends of the ring are independent.
    tap: platform::Tap,
}

/// Reads the format the aggregate device's input delivers now. The tap's,
/// on macOS; a fake, in a test.
type ReadFormat = Box<dyn FnMut() -> Result<TapFormat, OpenError> + Send>;

/// The reading end: the chain, the layout it was built for, and whether
/// Core Audio has said something about the format since. Apart from the
/// tap, so the rule is tested without one.
struct Reader {
    chain: CaptureChain,
    built: Layout,
    /// Set by Core Audio's listeners. It means "look again", not "changed":
    /// a notice can be about a format already read, or come from starting
    /// the device, and only a re-read can tell.
    recheck: Arc<AtomicBool>,
    read_format: ReadFormat,
}

impl Reader {
    /// Drain the chain, then report a format change once. As with the
    /// microphone, the audio drained before the change is reported is in
    /// `out` either way.
    fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize, CaptureError> {
        let n = self.chain.poll_into(out)?;
        self.changed()?;
        Ok(n)
    }

    fn finish_into(&mut self, out: &mut Vec<f32>) -> Result<usize, CaptureError> {
        let n = self.chain.finish_into(out)?;
        self.changed()?;
        Ok(n)
    }

    /// On a notice, read the format again, here on the polling thread, and
    /// report a change only if its layout differs from the chain's. A
    /// re-read that fails is reported as a change: the capture cannot show
    /// the audio is still what the chain expects, and time-scaled audio
    /// heard as speech is worse than a meeting that ends early and says so.
    fn changed(&mut self) -> Result<(), CaptureError> {
        if !self.recheck.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        match (self.read_format)() {
            Ok(format) => match layout(&format) {
                Ok(now) if now == self.built => Ok(()),
                now => {
                    tracing::warn!(built = ?self.built, ?now, "the system audio format changed");
                    Err(CaptureError::FormatChanged)
                }
            },
            Err(error) => {
                tracing::warn!(%error, "re-reading the system audio format failed; ending as changed");
                Err(CaptureError::FormatChanged)
            }
        }
    }
}

impl SystemAudio {
    /// Open the tap and start it, in one step.
    pub fn start(ring_seconds: f32) -> Result<Self, OpenError> {
        let mut audio = Self::open(ring_seconds)?;
        audio.play()?;
        Ok(audio)
    }

    /// Build the tap, its private aggregate device and the callback, without
    /// starting it. `ring_seconds` is how far the reader may fall behind
    /// before samples are lost, as for the microphone, and never less than
    /// [`MIN_RING_SECONDS`].
    pub fn open(ring_seconds: f32) -> Result<Self, OpenError> {
        let recheck = Arc::new(AtomicBool::new(false));
        let (tap, format) = platform::Tap::create(Arc::clone(&recheck))?;
        let layout = layout(&format)?;
        let read_format = tap.format_reader();
        let (sink, chain) = chain_for(layout, ring_seconds_for(ring_seconds))?;
        let tap = tap.with_callback(sink, layout)?;
        tracing::debug!(
            rate = layout.rate,
            channels = layout.channels,
            interleaved = layout.interleaved,
            "system audio armed (tap built, not started)"
        );
        Ok(Self {
            reader: Reader {
                chain,
                built: layout,
                recheck,
                read_format,
            },
            input_rate: layout.rate,
            channels: layout.channels,
            tap,
        })
    }

    /// Start the tap. The first start asks the person for permission to
    /// record system audio; refused, it still starts, and delivers zeros.
    /// Calling it twice is a no-op.
    pub fn play(&mut self) -> Result<(), OpenError> {
        self.tap.start()?;
        tracing::info!(rate = self.input_rate, "system audio started");
        Ok(())
    }

    /// Drain whatever the callback has produced, as 16 kHz mono. Fails with
    /// [`CaptureError::FormatChanged`], once, when Core Audio has said
    /// something about the format and a re-read shows it moved; what was
    /// drained before it is in `out`.
    pub fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize, CaptureError> {
        self.reader.poll_into(out)
    }

    /// Stop the tap and flush the tail.
    pub fn finish_into(mut self, out: &mut Vec<f32>) -> Result<usize, CaptureError> {
        self.tap.stop();
        self.reader.finish_into(out)
    }

    pub fn input_rate(&self) -> u32 {
        self.input_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Samples lost because the reader fell behind the tap. Put back as
    /// zeros, so the clock holds, but each one was real audio.
    pub fn overruns(&self) -> u64 {
        self.reader.chain.overruns()
    }

    /// Samples of silence owed for the tap's own gaps (skipped cycles, a
    /// reconfigured device, sleep) that were counted rather than written
    /// into the ring; zeros written in place are not counted. Kept apart
    /// from [`SystemAudio::overruns`], which is the reader's lag.
    pub fn gaps(&self) -> u64 {
        self.reader.chain.gaps()
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{Layout, OpenError, TapFormat};
    use macalla_core::audio::ring::CaptureSink;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// Never made: [`Tap::create`] is the only way to one, and it refuses.
    pub(super) struct Tap(());

    impl Tap {
        pub(super) fn create(_recheck: Arc<AtomicBool>) -> Result<(Self, TapFormat), OpenError> {
            Err(OpenError::Unsupported)
        }
        pub(super) fn format_reader(&self) -> super::ReadFormat {
            Box::new(|| Err(OpenError::Unsupported))
        }
        pub(super) fn with_callback(
            self,
            _sink: CaptureSink,
            _layout: Layout,
        ) -> Result<Self, OpenError> {
            Ok(self)
        }
        pub(super) fn start(&mut self) -> Result<(), OpenError> {
            Ok(())
        }
        pub(super) fn stop(&mut self) {}
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! The sequence Apple's "Capturing system audio with Core Audio taps"
    //! describes, proven by a bundled probe on 2026-09-22 (48 kHz, two
    //! channels, 32-bit float): this process's audio object, a tap of every
    //! process's output but this one's, the tap's UID, a private aggregate
    //! device holding the tap, its input format, and an IO block on it.

    use super::{write_cycle, Buffer, Clock, Layout, OpenError, Step, TapFormat, MAX_CHANNELS};
    use block2::{Block, RcBlock};
    use macalla_core::audio::ring::CaptureSink;
    use objc2::AllocAnyThread;
    use objc2_core_audio::{
        kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
        kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
        kAudioAggregateDeviceUIDKey, kAudioDevicePropertyNominalSampleRate,
        kAudioDevicePropertyStreamFormat, kAudioHardwarePropertyTranslatePIDToProcessObject,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, kAudioObjectUnknown,
        kAudioSubTapUIDKey, kAudioTapPropertyFormat, kAudioTapPropertyUID,
        AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
        AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
        AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
        AudioHardwareDestroyProcessTap, AudioObjectAddPropertyListenerBlock,
        AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
        AudioObjectPropertySelector, AudioObjectRemovePropertyListenerBlock, CATapDescription,
        CATapMuteBehavior,
    };
    use objc2_core_audio_types::{
        kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM,
        AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
        AudioTimeStampFlags,
    };
    use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
    use objc2_foundation::{NSArray, NSNumber, NSString};
    use std::cell::{Cell, RefCell};
    use std::ffi::{c_void, CStr};
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;

    /// The IO block's signature, as `AudioDeviceIOBlock` declares it.
    type IoBlock = dyn Fn(
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
    );

    /// A property listener block's signature, as
    /// `AudioObjectPropertyListenerBlock` declares it.
    type ListenerBlock = dyn Fn(u32, NonNull<AudioObjectPropertyAddress>);

    /// A listener registered on one property of one object, and our own
    /// reference to its block, released after the listener is removed.
    struct Listener {
        object: AudioObjectID,
        address: AudioObjectPropertyAddress,
        block: NonNull<Block<ListenerBlock>>,
    }

    // SAFETY: the block captures only an `Arc<AtomicBool>`, which is Send
    // and Sync, and a block's retain and release are atomic, so the
    // reference may be released from whichever thread drops the tap.
    unsafe impl Send for Listener {}

    /// The Core Audio objects one tap owns. Each is filled in as it is made,
    /// so a failure partway tears down exactly what was built.
    pub(super) struct Tap {
        tap: AudioObjectID,
        aggregate: AudioObjectID,
        listeners: Vec<Listener>,
        io_proc: AudioDeviceIOProcID,
        running: bool,
    }

    impl Tap {
        /// The tap, with its aggregate device, and the format the device's
        /// input delivers. No callback yet: the ring it feeds is sized from
        /// the format. `recheck` is set whenever Core Audio says something
        /// about the tap's format or the device's sample rate from then on.
        pub(super) fn create(recheck: Arc<AtomicBool>) -> Result<(Self, TapFormat), OpenError> {
            let mut this = Self {
                tap: kAudioObjectUnknown,
                aggregate: kAudioObjectUnknown,
                listeners: Vec::new(),
                io_proc: None,
                running: false,
            };
            let own = own_process()?;

            // This process only. Another process working for the same app,
            // such as a WebKit helper playing the web view's sounds, is not
            // in the list.
            let excluded = NSArray::from_retained_slice(&[NSNumber::new_u32(own)]);
            // SAFETY: `alloc` is a fresh allocation handed straight to its
            // initialiser, which takes an array of NSNumber holding process
            // object ids, as `excluded` is. The setters take values of the
            // types declared. The description is only read by
            // `AudioHardwareCreateProcessTap` during the call; the tap keeps
            // its own (it is readable afterwards as
            // kAudioTapPropertyDescription), so ours may go when this returns.
            let status = unsafe {
                let description = CATapDescription::initStereoGlobalTapButExcludeProcesses(
                    CATapDescription::alloc(),
                    &excluded,
                );
                description.setName(&NSString::from_str("Macalla system audio"));
                description.setPrivate(true);
                description.setMuteBehavior(CATapMuteBehavior::Unmuted);
                AudioHardwareCreateProcessTap(Some(&description), &mut this.tap)
            };
            check(status, Step::CreateTap)?;

            let uid = tap_uid(this.tap)?;
            this.aggregate = aggregate_with(&uid)?;

            // Listening, clearing, then reading, so no change can fall
            // between the read and the listening. A notice from here on only
            // asks the reader to read again, so one about a format this read
            // already sees costs a re-read, not the meeting.
            this.listen(this.tap, global(kAudioTapPropertyFormat), &recheck)?;
            this.listen(
                this.aggregate,
                global(kAudioDevicePropertyNominalSampleRate),
                &recheck,
            )?;
            recheck.store(false, Ordering::Release);
            // The tap's format is the mix Core Audio makes; the aggregate
            // device's input stream is how that mix reaches the IO block.
            // Apple documents the tap's format as "the format of that data
            // that will be accessible in any aggregate device that contains
            // the tap", so the two should agree. The IO block is handed the
            // device's, so the chain is built from the device's, and a
            // disagreement is logged.
            let tap = format_of(
                this.tap,
                global(kAudioTapPropertyFormat),
                Step::ReadTapFormat,
            )?;
            let stream = stream_format(this.aggregate)?;
            if tap != stream {
                tracing::warn!(?tap, ?stream, "the tap's format and its device's differ");
            }
            Ok((this, stream))
        }

        /// Reads the aggregate device's input format again, for the reader
        /// to compare with the one its chain was built for. The stream
        /// format carries the rate, so a nominal-rate change shows in it.
        /// It holds only the device's id: once the device is gone, the read
        /// fails, which the reader treats as a change.
        pub(super) fn format_reader(&self) -> super::ReadFormat {
            let aggregate = self.aggregate;
            Box::new(move || stream_format(aggregate))
        }

        /// Set `flag` whenever the property at `address` of `object`
        /// changes, until the tap is dropped.
        fn listen(
            &mut self,
            object: AudioObjectID,
            address: AudioObjectPropertyAddress,
            flag: &Arc<AtomicBool>,
        ) -> Result<(), OpenError> {
            let flag = Arc::clone(flag);
            // Only an atomic store: with no dispatch queue Core Audio calls
            // the block directly, on a thread of its own choosing.
            let block: RcBlock<ListenerBlock> = RcBlock::new(
                move |_count: u32, _addresses: NonNull<AudioObjectPropertyAddress>| {
                    flag.store(true, Ordering::Release);
                },
            );
            let block = NonNull::new(RcBlock::into_raw(block)).ok_or(OpenError::CoreAudio {
                step: Step::Listen,
                status: 0,
            })?;
            // SAFETY: `address` is a valid local. The block is a heap block
            // of the listener signature; Core Audio `Block_copy`s it and
            // keeps that copy until the matching remove, and we keep our
            // own reference in `self.listeners` until after that remove.
            let status = unsafe {
                AudioObjectAddPropertyListenerBlock(
                    object,
                    NonNull::from(&address),
                    None,
                    block.as_ptr(),
                )
            };
            if let Err(error) = check(status, Step::Listen) {
                // SAFETY: not registered, so ours is the only reference,
                // taken by `into_raw` above and released here once.
                drop(unsafe { RcBlock::from_raw(block.as_ptr()) });
                return Err(error);
            }
            self.listeners.push(Listener {
                object,
                address,
                block,
            });
            Ok(())
        }

        /// Register the IO block that feeds `sink` with cycles laid out as
        /// `layout` says. The block owns the sink and its clock: Core Audio
        /// copies the block and keeps it until the IO proc is destroyed, and
        /// releasing it then drops the sink.
        pub(super) fn with_callback(
            mut self,
            sink: CaptureSink,
            layout: Layout,
        ) -> Result<Self, OpenError> {
            // Called only on the device's IO thread, one cycle at a time, so
            // neither cell is ever contended; `try_borrow_mut` rather than a
            // panic on a realtime thread all the same.
            let sink = RefCell::new(sink);
            let clock = Cell::new(Clock::default());
            let block: RcBlock<IoBlock> = RcBlock::new(
                move |_now: NonNull<AudioTimeStamp>,
                      input: NonNull<AudioBufferList>,
                      input_time: NonNull<AudioTimeStamp>,
                      _output: NonNull<AudioBufferList>,
                      _output_time: NonNull<AudioTimeStamp>| {
                    if let Ok(mut sink) = sink.try_borrow_mut() {
                        let mut now = clock.get();
                        // SAFETY: Core Audio passes a buffer list and an
                        // input time stamp valid for the duration of this
                        // call, and both are only read.
                        unsafe { copy_list(input, input_time, &mut now, layout, &mut sink) };
                        clock.set(now);
                    }
                },
            );
            // SAFETY: `io_proc` is a valid place for the new id. With no
            // dispatch queue the block is invoked directly on the device's
            // IO thread. The block is `Block_copy`'d and that copy kept until
            // `AudioDeviceDestroyIOProcID`, so our reference may be dropped at
            // the end of this function.
            let status = unsafe {
                AudioDeviceCreateIOProcIDWithBlock(
                    NonNull::from(&mut self.io_proc),
                    self.aggregate,
                    None,
                    RcBlock::as_ptr(&block),
                )
            };
            check(status, Step::CreateIoProc)?;
            Ok(self)
        }

        pub(super) fn start(&mut self) -> Result<(), OpenError> {
            if self.running {
                return Ok(());
            }
            // SAFETY: `io_proc` was created on this device and not destroyed.
            check(
                unsafe { AudioDeviceStart(self.aggregate, self.io_proc) },
                Step::Start,
            )?;
            self.running = true;
            Ok(())
        }

        pub(super) fn stop(&mut self) {
            if !self.running {
                return;
            }
            // SAFETY: as for `start`.
            let status = unsafe { AudioDeviceStop(self.aggregate, self.io_proc) };
            if status != 0 {
                tracing::warn!(status, "stopping the system audio tap failed");
            }
            self.running = false;
        }
    }

    impl Drop for Tap {
        /// Reverse order: stop, the listeners, the IO proc, the aggregate
        /// device, the tap.
        fn drop(&mut self) {
            self.stop();
            for listener in self.listeners.drain(..) {
                // SAFETY: registered on this object at this address with this
                // block and no queue, removed once, before the object goes.
                let status = unsafe {
                    AudioObjectRemovePropertyListenerBlock(
                        listener.object,
                        NonNull::from(&listener.address),
                        None,
                        listener.block.as_ptr(),
                    )
                };
                warn_on(status, "removing a system audio listener");
                // SAFETY: our reference from `listen`, released once, after
                // Core Audio has let go of its own.
                drop(unsafe { RcBlock::from_raw(listener.block.as_ptr()) });
            }
            if self.io_proc.is_some() {
                // SAFETY: created on this device, stopped above, destroyed
                // once. Core Audio releases the block, and the sink with it.
                let status = unsafe { AudioDeviceDestroyIOProcID(self.aggregate, self.io_proc) };
                warn_on(status, "destroying the system audio IO proc");
            }
            if self.aggregate != kAudioObjectUnknown {
                // SAFETY: an aggregate device this process created, destroyed
                // once. A plain id: nothing is dereferenced.
                let status = unsafe { AudioHardwareDestroyAggregateDevice(self.aggregate) };
                warn_on(status, "destroying the system audio aggregate device");
            }
            if self.tap != kAudioObjectUnknown {
                // SAFETY: a tap this process created, destroyed once, after
                // the device that held it.
                let status = unsafe { AudioHardwareDestroyProcessTap(self.tap) };
                warn_on(status, "destroying the system audio tap");
            }
        }
    }

    fn check(status: i32, step: Step) -> Result<(), OpenError> {
        match status {
            0 => Ok(()),
            status => Err(OpenError::CoreAudio { step, status }),
        }
    }

    fn warn_on(status: i32, what: &str) {
        if status != 0 {
            tracing::warn!(status, "{what} failed");
        }
    }

    fn global(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    /// Read the property at `address` of `object` into `out`.
    ///
    /// # Safety
    ///
    /// `T` must be the type Core Audio writes for `address` on `object`, and
    /// `Q`, when given, the type it reads as the qualifier.
    unsafe fn read<T, Q>(
        object: AudioObjectID,
        address: AudioObjectPropertyAddress,
        qualifier: Option<&Q>,
        out: &mut T,
        step: Step,
    ) -> Result<(), OpenError> {
        let mut size = std::mem::size_of::<T>() as u32;
        let (qualifier_size, qualifier) = match qualifier {
            Some(q) => (
                std::mem::size_of::<Q>() as u32,
                (q as *const Q).cast::<c_void>(),
            ),
            None => (0, std::ptr::null()),
        };
        // SAFETY: every pointer is to a live local or to `out`, sized as
        // stated; the caller vouches for the types.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&address),
                qualifier_size,
                qualifier,
                NonNull::from(&mut size),
                NonNull::from(out).cast(),
            )
        };
        check(status, step)
    }

    /// This process's audio object, so the tap can leave it out.
    fn own_process() -> Result<AudioObjectID, OpenError> {
        // A `pid_t`, which is an i32 on every Apple platform.
        let pid = std::process::id() as i32;
        let mut own: AudioObjectID = kAudioObjectUnknown;
        // SAFETY: TranslatePIDToProcessObject reads a pid_t qualifier and
        // writes an AudioObjectID.
        unsafe {
            read(
                kAudioObjectSystemObject as AudioObjectID,
                global(kAudioHardwarePropertyTranslatePIDToProcessObject),
                Some(&pid),
                &mut own,
                Step::FindOwnProcess,
            )?;
        }
        // Core Audio answers kAudioObjectUnknown, not an error, for a pid it
        // does not know.
        if own == kAudioObjectUnknown {
            return Err(OpenError::OwnProcessUnknown);
        }
        Ok(own)
    }

    /// The tap's UID, which the aggregate device's tap list names it by.
    fn tap_uid(tap: AudioObjectID) -> Result<CFRetained<CFString>, OpenError> {
        let mut uid: *const CFString = std::ptr::null();
        // SAFETY: kAudioTapPropertyUID writes a CFStringRef.
        unsafe {
            read::<_, ()>(
                tap,
                global(kAudioTapPropertyUID),
                None,
                &mut uid,
                Step::ReadTapUid,
            )?
        };
        let uid = NonNull::new(uid.cast_mut()).ok_or(OpenError::CoreAudio {
            step: Step::ReadTapUid,
            status: 0,
        })?;
        // SAFETY: CoreAudio's AudioHardware.h says of kAudioTapPropertyUID
        // that "the caller is responsible for releasing the returned
        // CFObject", so this reference is ours to release.
        Ok(unsafe { CFRetained::from_raw(uid) })
    }

    /// The format a device's input stream delivers.
    fn stream_format(device: AudioObjectID) -> Result<TapFormat, OpenError> {
        format_of(
            device,
            AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyStreamFormat,
                mScope: kAudioObjectPropertyScopeInput,
                mElement: kAudioObjectPropertyElementMain,
            },
            Step::ReadStreamFormat,
        )
    }

    /// A stream format property: the tap's, or a device's input stream's.
    fn format_of(
        object: AudioObjectID,
        address: AudioObjectPropertyAddress,
        step: Step,
    ) -> Result<TapFormat, OpenError> {
        let mut asbd = AudioStreamBasicDescription {
            mSampleRate: 0.0,
            mFormatID: 0,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: 0,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 0,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        // SAFETY: both kAudioTapPropertyFormat and
        // kAudioDevicePropertyStreamFormat write an
        // AudioStreamBasicDescription.
        unsafe { read::<_, ()>(object, address, None, &mut asbd, step)? };
        Ok(TapFormat {
            sample_rate: asbd.mSampleRate,
            channels: asbd.mChannelsPerFrame,
            bits_per_channel: asbd.mBitsPerChannel,
            linear_pcm: asbd.mFormatID == kAudioFormatLinearPCM,
            float: asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0,
            non_interleaved: asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0,
        })
    }

    fn cf_key(key: &CStr) -> CFRetained<CFString> {
        CFString::from_str(&key.to_string_lossy())
    }

    /// A private aggregate device whose only input is the tap, started with
    /// the device.
    fn aggregate_with(tap_uid: &CFString) -> Result<AudioObjectID, OpenError> {
        // Unique among this process's devices; a private device is visible
        // to no other process.
        static MADE: AtomicU64 = AtomicU64::new(0);
        let uid = CFString::from_str(&format!(
            "dev.macalla.system-audio.{}.{}",
            std::process::id(),
            MADE.fetch_add(1, Ordering::Relaxed)
        ));
        let name = CFString::from_str("Macalla system audio");
        let sub_tap = CFDictionary::<CFString, CFType>::from_slices(
            &[&*cf_key(kAudioSubTapUIDKey)],
            &[tap_uid.as_ref()],
        );
        let taps = CFArray::from_retained_objects(&[sub_tap]);
        let keys = [
            cf_key(kAudioAggregateDeviceNameKey),
            cf_key(kAudioAggregateDeviceUIDKey),
            cf_key(kAudioAggregateDeviceIsPrivateKey),
            cf_key(kAudioAggregateDeviceTapAutoStartKey),
            cf_key(kAudioAggregateDeviceTapListKey),
        ];
        let values: [&CFType; 5] = [
            name.as_ref(),
            uid.as_ref(),
            CFBoolean::new(true).as_ref(),
            CFBoolean::new(true).as_ref(),
            taps.as_ref(),
        ];
        let key_refs: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
        let description = CFDictionary::<CFString, CFType>::from_slices(&key_refs, &values);

        let mut aggregate: AudioObjectID = kAudioObjectUnknown;
        // SAFETY: the description is a CFDictionary of CFString keys to the
        // CF values Core Audio documents for each key; it is read during the
        // call only. `aggregate` is a valid place for the new id.
        let status = unsafe {
            AudioHardwareCreateAggregateDevice(
                description.as_opaque(),
                NonNull::from(&mut aggregate),
            )
        };
        check(status, Step::CreateAggregateDevice)?;
        Ok(aggregate)
    }

    /// Copy one IO cycle's input into the ring, with zeros first for any
    /// cycles skipped since the last: [`write_cycle`], over Core Audio's
    /// own structures.
    ///
    /// # Safety
    ///
    /// `list` must point to a valid `AudioBufferList` whose buffers hold
    /// 32-bit floats, as the tap's format says, and `time` to a valid
    /// `AudioTimeStamp`, both for the duration of the call.
    unsafe fn copy_list(
        list: NonNull<AudioBufferList>,
        time: NonNull<AudioTimeStamp>,
        clock: &mut Clock,
        layout: Layout,
        sink: &mut CaptureSink,
    ) {
        // SAFETY: the caller vouches for `time`; it is copied out, not kept.
        let time = unsafe { *time.as_ptr() };
        let valid = time.mFlags.0 & AudioTimeStampFlags::SampleTimeValid.0 != 0;
        let sample_time = valid.then_some(time.mSampleTime);

        let list = list.as_ptr();
        // SAFETY: the caller vouches for `list`. `mBuffers` is declared as a
        // one-element array but holds `mNumberBuffers` buffers in a row, as
        // the C struct's flexible array does.
        let buffers: &[AudioBuffer] = unsafe {
            let count = (*list).mNumberBuffers as usize;
            let first = std::ptr::addr_of!((*list).mBuffers).cast::<AudioBuffer>();
            std::slice::from_raw_parts(first, count)
        };
        let mut seen = [Buffer {
            channels: 0,
            bytes: 0,
            data: None,
        }; MAX_CHANNELS as usize];
        // More buffers than any layout here has is a mismatch: passing all
        // that fit keeps it one, and the cycle becomes zeros.
        let n = buffers.len().min(seen.len());
        for (slot, buffer) in seen.iter_mut().zip(buffers) {
            let data = (!buffer.mData.is_null()).then(|| {
                let len = buffer.mDataByteSize as usize / std::mem::size_of::<f32>();
                // SAFETY: a non-null `mData` holds `mDataByteSize` bytes of
                // 32-bit floats for this call, suitably aligned for them.
                unsafe { std::slice::from_raw_parts(buffer.mData.cast::<f32>(), len) }
            });
            *slot = Buffer {
                channels: buffer.mNumberChannels,
                bytes: buffer.mDataByteSize,
                data,
            };
        }
        write_cycle(clock, sample_time, &seen[..n], layout, sink);
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use macalla_core::audio::ring::capture_ring;

        #[test]
        fn a_buffer_list_from_the_callback_reaches_the_ring_unclamped() {
            // What Core Audio hands the block for one cycle of stereo
            // interleaved float, with a peak above full scale, as measured.
            let mut samples = [0.5f32, -0.5, 1.28, -1.28];
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: 2,
                    mDataByteSize: std::mem::size_of_val(&samples) as u32,
                    mData: samples.as_mut_ptr().cast(),
                }],
            };
            let (mut sink, mut source) = capture_ring(64, 2);
            let layout = Layout {
                rate: 48_000,
                channels: 2,
                interleaved: true,
            };
            let mut clock = Clock::default();
            // SAFETY: every field of AudioTimeStamp is a plain number or a
            // transparent wrapper of one, so all zeros is a valid value.
            let mut time: AudioTimeStamp = unsafe { std::mem::zeroed() };
            time.mFlags = AudioTimeStampFlags::SampleTimeValid;

            // Two cycles of two frames, the second stamped three frames late.
            for at in [100.0, 105.0] {
                time.mSampleTime = at;
                // SAFETY: `list` is a valid one-buffer list over `samples`,
                // and `time` a valid stamp, both outliving the call.
                unsafe {
                    copy_list(
                        NonNull::from(&mut list),
                        NonNull::from(&mut time),
                        &mut clock,
                        layout,
                        &mut sink,
                    )
                };
            }

            let mut out = Vec::new();
            source.read_into(&mut out);
            let cycle = [0.5, -0.5, 1.28, -1.28];
            let expected: Vec<f32> = cycle
                .iter()
                .chain(&[0.0; 6])
                .chain(&cycle)
                .copied()
                .collect();
            assert_eq!(
                out, expected,
                "unclamped, and the late cycle's gap is zeros"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use macalla_core::audio::ring::capture_ring;
    use macalla_core::audio::SAMPLE_RATE;

    /// What the bundled probe measured the tap delivering.
    fn probed() -> TapFormat {
        TapFormat {
            sample_rate: 48_000.0,
            channels: 2,
            bits_per_channel: 32,
            linear_pcm: true,
            float: true,
            non_interleaved: false,
        }
    }

    fn stereo() -> Layout {
        layout(&probed()).unwrap()
    }

    /// One interleaved buffer over `data`, as Core Audio would describe it.
    fn interleaved(data: &[f32], channels: u32) -> Buffer<'_> {
        Buffer {
            channels,
            bytes: std::mem::size_of_val(data) as u32,
            data: Some(data),
        }
    }

    /// Write `cycles`, each stamped with its sample time, as the callback
    /// does, and read back what the ring holds.
    fn written(layout: Layout, cycles: &[(Option<f64>, &[Buffer<'_>])]) -> Vec<f32> {
        let (mut sink, mut source) = capture_ring(1 << 16, layout.channels);
        let mut clock = Clock::default();
        for (at, buffers) in cycles {
            write_cycle(&mut clock, *at, buffers, layout, &mut sink);
        }
        let mut out = Vec::new();
        source.read_into(&mut out);
        out
    }

    #[test]
    fn the_probed_format_configures_the_chain_for_its_rate_and_channels() {
        assert_eq!(
            stereo(),
            Layout {
                rate: 48_000,
                channels: 2,
                interleaved: true,
            }
        );
        let one_buffer_per_channel = TapFormat {
            non_interleaved: true,
            ..probed()
        };
        assert!(!layout(&one_buffer_per_channel).unwrap().interleaved);
    }

    #[test]
    fn a_format_this_cannot_read_is_refused_with_the_format() {
        let refused = [
            TapFormat {
                float: false,
                bits_per_channel: 16,
                ..probed()
            },
            TapFormat {
                linear_pcm: false,
                ..probed()
            },
            TapFormat {
                channels: 0,
                ..probed()
            },
            TapFormat {
                channels: MAX_CHANNELS + 1,
                ..probed()
            },
            TapFormat {
                sample_rate: 0.0,
                ..probed()
            },
            TapFormat {
                sample_rate: f64::NAN,
                ..probed()
            },
            TapFormat {
                sample_rate: 44_100.5,
                ..probed()
            },
        ];
        for format in refused {
            // Compared as text: NaN is not equal to itself.
            match layout(&format) {
                Err(OpenError::Format(f)) => assert_eq!(format!("{f:?}"), format!("{format:?}")),
                other => panic!("{format:?} gave {other:?}"),
            }
        }
    }

    #[test]
    fn a_second_of_the_probed_format_comes_out_as_a_second_of_sixteen_k_mono() {
        let (mut sink, mut chain) = chain_for(stereo(), 2.0).unwrap();
        let second: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                [s, s]
            })
            .collect();
        let mut clock = Clock::default();
        for (i, cycle) in second.chunks(512 * 2).enumerate() {
            let at = Some((i * 512) as f64);
            write_cycle(
                &mut clock,
                at,
                &[interleaved(cycle, 2)],
                stereo(),
                &mut sink,
            );
        }

        let mut out = Vec::new();
        chain.poll_into(&mut out).unwrap();
        chain.finish_into(&mut out).unwrap();

        let drift = (out.len() as i64 - SAMPLE_RATE as i64).abs();
        assert!(drift <= 64, "got {} samples", out.len());
        assert_eq!(chain.overruns(), 0);
    }

    #[test]
    fn one_buffer_per_channel_is_interleaved_into_the_ring() {
        let apart = Layout {
            interleaved: false,
            ..stereo()
        };
        let left: Vec<f32> = (0..600).map(|i| i as f32).collect();
        let right: Vec<f32> = (0..600).map(|i| -(i as f32)).collect();

        let out = written(
            apart,
            &[(Some(0.0), &[interleaved(&left, 1), interleaved(&right, 1)])],
        );

        let expected: Vec<f32> = (0..600).flat_map(|i| [i as f32, -(i as f32)]).collect();
        assert_eq!(out, expected, "past the stack buffer's 256 frames too");
    }

    #[test]
    fn a_skipped_cycle_is_written_as_zero_frames_in_its_place() {
        // Four-frame cycles stamped 0, 4 and 8, then 20: the cycles at 12
        // and 16 never came, and are owed as eight frames of zeros.
        let cycle: Vec<f32> = vec![1.0; 4 * 2];
        let buffer = [interleaved(&cycle, 2)];
        let out = written(
            stereo(),
            &[
                (Some(0.0), &buffer),
                (Some(4.0), &buffer),
                (Some(8.0), &buffer),
                (Some(20.0), &buffer),
            ],
        );

        assert_eq!(out.len(), (4 * 4 + 8) * 2, "the clock held");
        assert!(out[..12 * 2].iter().all(|&s| s == 1.0));
        assert!(out[12 * 2..20 * 2].iter().all(|&s| s == 0.0), "{out:?}");
        assert!(out[20 * 2..].iter().all(|&s| s == 1.0));
    }

    #[test]
    fn a_long_gap_is_written_whole_between_the_cycles() {
        // Past the size of any one piece of zeros: the stamp jumps 10,000
        // frames, a little over 200 ms at the probed rate.
        let cycle: Vec<f32> = vec![1.0; 4 * 2];
        let buffer = [interleaved(&cycle, 2)];
        let out = written(stereo(), &[(Some(0.0), &buffer), (Some(10_004.0), &buffer)]);

        assert_eq!(out.len(), 8 + 10_000 * 2 + 8);
        assert!(out[..8].iter().all(|&s| s == 1.0));
        assert_eq!(
            out[8..8 + 20_000].iter().filter(|&&s| s == 0.0).count(),
            20_000,
            "exactly 10,000 stereo frames of zeros between the cycles"
        );
        assert!(out[8 + 20_000..].iter().all(|&s| s == 1.0));
    }

    #[test]
    fn a_gap_past_the_in_band_limit_is_counted_and_put_back_by_the_reader() {
        // Three seconds owed: one written in place, two counted apart from
        // the overruns and put back by the chain.
        let mono = Layout {
            rate: SAMPLE_RATE,
            channels: 1,
            interleaved: true,
        };
        let (mut sink, mut chain) = chain_for(mono, 30.0).unwrap();
        let cycle = [0.5f32; 160];
        let buffer = [interleaved(&cycle, 1)];
        let mut clock = Clock::default();
        write_cycle(&mut clock, Some(0.0), &buffer, mono, &mut sink);
        let late = 160.0 + 3.0 * SAMPLE_RATE as f64;
        write_cycle(&mut clock, Some(late), &buffer, mono, &mut sink);

        let mut out = Vec::new();
        chain.poll_into(&mut out).unwrap();
        assert_eq!(
            out.len(),
            160 + 3 * SAMPLE_RATE as usize + 160,
            "the clock held"
        );
        assert_eq!(chain.gaps(), 2 * SAMPLE_RATE as u64);
        assert_eq!(chain.overruns(), 0, "a gap is not the reader's lag");
    }

    #[test]
    fn a_jump_backward_owes_nothing_and_the_clock_starts_again_from_it() {
        let mut clock = Clock::default();
        assert_eq!(clock.advance(Some(1_000.0), 4, 100), 0);
        assert_eq!(clock.advance(Some(1_004.0), 4, 100), 0);
        // A device that restarted its count.
        assert_eq!(clock.advance(Some(0.0), 4, 100), 0);
        assert_eq!(
            clock.advance(Some(4.0), 4, 100),
            0,
            "counted from the new stamp"
        );
        assert_eq!(clock.advance(Some(10.0), 4, 100), 2);
    }

    #[test]
    fn a_huge_jump_is_filled_only_up_to_the_cap() {
        let mut clock = Clock::default();
        clock.advance(Some(0.0), 512, 480_000);
        assert_eq!(clock.advance(Some(3.6e9), 512, 480_000), 480_000);
        assert_eq!(
            MAX_GAP_SECONDS as usize * stereo().rate as usize,
            480_000,
            "ten seconds at the probed rate"
        );
    }

    #[test]
    fn the_ring_always_holds_the_longest_gap_and_room_after_it() {
        assert!(MIN_RING_SECONDS > MAX_GAP_SECONDS as f32);
        assert_eq!(
            ring_seconds_for(1.0),
            MIN_RING_SECONDS,
            "raised to the floor"
        );
        assert_eq!(ring_seconds_for(30.0), 30.0, "more is kept as asked");
    }

    #[test]
    fn a_cycle_with_no_valid_stamp_moves_the_clock_on_by_its_frames() {
        let mut clock = Clock::default();
        assert_eq!(clock.advance(Some(0.0), 4, 100), 0);
        assert_eq!(clock.advance(None, 4, 100), 0);
        assert_eq!(clock.advance(Some(8.0), 4, 100), 0, "no gap: 0, 4, 8");
        assert_eq!(
            clock.advance(Some(12.4), 4, 100),
            0,
            "under half a frame is jitter"
        );
    }

    #[test]
    fn a_missing_buffer_in_a_stereo_pair_is_a_whole_cycle_of_zeros() {
        // One buffer per channel, and the right one has no data. Counting
        // only the buffers with data would read a mono cycle and write half
        // the frames; the cycle is written as all of its frames, silent.
        let apart = Layout {
            interleaved: false,
            ..stereo()
        };
        let left = [0.5f32; 6];
        let right = Buffer {
            channels: 1,
            bytes: std::mem::size_of_val(&left) as u32,
            data: None,
        };
        let out = written(
            apart,
            &[
                (Some(0.0), &[interleaved(&left, 1), right]),
                (Some(6.0), &[interleaved(&left, 1), interleaved(&left, 1)]),
            ],
        );

        assert_eq!(out.len(), 6 * 2 * 2, "two cycles of six stereo frames");
        assert!(out[..12].iter().all(|&s| s == 0.0), "{out:?}");
        assert!(out[12..].iter().all(|&s| s == 0.5), "{out:?}");
    }

    #[test]
    fn a_buffer_that_is_not_the_expected_layout_is_zeros() {
        // Interleaved stereo expected; a buffer that says it holds one
        // channel is not guessed at.
        let samples = [0.5f32; 8];
        let out = written(stereo(), &[(Some(0.0), &[interleaved(&samples, 1)])]);
        assert_eq!(out, [0.0; 8], "four stereo frames of zeros");

        // Two buffers where one interleaved buffer was expected.
        let out = written(
            stereo(),
            &[(
                Some(0.0),
                &[interleaved(&samples, 2), interleaved(&samples, 2)],
            )],
        );
        assert_eq!(out, [0.0; 8]);
    }

    #[test]
    fn frames_the_ring_drops_come_out_of_the_chain_as_zeros() {
        // A one-second ring at 16 kHz mono, handed a second and a quarter
        // at once while the reader is away: the last quarter is dropped,
        // and comes out as a quarter second of zeros.
        let mono = Layout {
            rate: SAMPLE_RATE,
            channels: 1,
            interleaved: true,
        };
        let (mut sink, mut chain) = chain_for(mono, 1.0).unwrap();
        let cycle = vec![0.25f32; 20_000];
        write_cycle(
            &mut Clock::default(),
            Some(0.0),
            &[interleaved(&cycle, 1)],
            mono,
            &mut sink,
        );

        let mut out = Vec::new();
        chain.poll_into(&mut out).unwrap();
        assert_eq!(out.len(), 20_000, "the clock held");
        assert!(out[..16_000].iter().all(|&s| s == 0.25));
        assert!(out[16_000..].iter().all(|&s| s == 0.0));
        assert_eq!(chain.overruns(), 4_000);
    }

    #[test]
    fn samples_beyond_full_scale_pass_the_ring_and_the_chain_unclamped() {
        // Measured from a tap: a peak of 1.28. Nothing between the callback
        // and the engine may clamp or refuse it. Whether each engine
        // tolerates it is untested per engine. Tap audio has been heard as
        // Others turns in a signed build (2026-09-23, on the session's engine,
        // which was not recorded); each engine on its own is not yet checked.
        let mono = Layout {
            rate: SAMPLE_RATE,
            channels: 1,
            interleaved: true,
        };
        let (mut sink, mut chain) = chain_for(mono, 1.0).unwrap();
        let loud = [1.28f32, -1.28, 2.5, -0.3];
        write_cycle(
            &mut Clock::default(),
            Some(0.0),
            &[interleaved(&loud, 1)],
            mono,
            &mut sink,
        );
        let mut out = Vec::new();
        chain.poll_into(&mut out).unwrap();
        assert_eq!(out, loud, "straight through at 16 kHz");

        // And through a real downmix and resample: the probed format, loud.
        let (mut sink, mut chain) = chain_for(stereo(), 1.0).unwrap();
        let loud: Vec<f32> = (0..9_600)
            .flat_map(|i| {
                let s = 1.28 * (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                [s, s]
            })
            .collect();
        write_cycle(
            &mut Clock::default(),
            Some(0.0),
            &[interleaved(&loud, 2)],
            stereo(),
            &mut sink,
        );
        let mut out = Vec::new();
        chain.poll_into(&mut out).unwrap();
        chain.finish_into(&mut out).unwrap();
        let peak = out.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak > 1.0, "peak {peak}: clamped somewhere");
    }

    /// A reader of the probed format, built at 16 kHz mono so what goes in
    /// comes out unchanged, whose re-read answers from `now`.
    fn reader(
        now: impl FnMut() -> Result<TapFormat, OpenError> + Send + 'static,
    ) -> (CaptureSink, Reader, Arc<AtomicBool>) {
        let built = Layout {
            rate: SAMPLE_RATE,
            channels: 1,
            interleaved: true,
        };
        let (sink, chain) = chain_for(built, 1.0).unwrap();
        let recheck = Arc::new(AtomicBool::new(false));
        let reader = Reader {
            chain,
            built,
            recheck: Arc::clone(&recheck),
            read_format: Box::new(now),
        };
        (sink, reader, recheck)
    }

    /// The format a [`reader`] was built for.
    fn as_built() -> TapFormat {
        TapFormat {
            sample_rate: SAMPLE_RATE as f64,
            channels: 1,
            ..probed()
        }
    }

    #[test]
    fn a_notice_with_the_format_unchanged_does_not_end_the_capture() {
        // As a notice about a format already read, or from starting the
        // device, would be.
        let (mut sink, mut reader, recheck) = reader(|| Ok(as_built()));
        sink.write(&[0.5; 160]);
        recheck.store(true, Ordering::Release);
        let mut out = Vec::new();

        assert_eq!(reader.poll_into(&mut out).unwrap(), 160);
        assert!(!recheck.load(Ordering::Acquire), "the notice was taken");
        assert!(reader.finish_into(&mut out).is_ok());
    }

    #[test]
    fn a_notice_with_the_format_changed_ends_the_capture_after_what_came_before_it() {
        // The output dropped to a headset's call rate.
        let (mut sink, mut reader, recheck) = reader(|| {
            Ok(TapFormat {
                sample_rate: 24_000.0,
                ..as_built()
            })
        });
        sink.write(&[0.5; 160]);
        let mut out = Vec::new();
        assert_eq!(reader.poll_into(&mut out).unwrap(), 160);

        sink.write(&[0.25; 160]);
        recheck.store(true, Ordering::Release);
        let result = reader.poll_into(&mut out);

        assert!(
            matches!(result, Err(CaptureError::FormatChanged)),
            "{result:?}"
        );
        assert_eq!(out.len(), 320, "drained before the change was reported");
        // Reported once, so the finish that follows flushes the tail.
        assert!(reader.finish_into(&mut out).is_ok());
    }

    #[test]
    fn a_notice_whose_re_read_fails_ends_the_capture_as_changed() {
        let (_sink, mut reader, recheck) = reader(|| {
            Err(OpenError::CoreAudio {
                step: Step::ReadStreamFormat,
                status: -1,
            })
        });
        recheck.store(true, Ordering::Release);
        let result = reader.poll_into(&mut Vec::new());
        assert!(
            matches!(result, Err(CaptureError::FormatChanged)),
            "{result:?}"
        );
    }

    #[test]
    fn no_notice_is_no_re_read_and_no_error() {
        let (mut sink, mut reader, _recheck) = reader(|| panic!("read with no notice"));
        sink.write(&[0.5; 160]);
        let mut out = Vec::new();
        assert_eq!(reader.poll_into(&mut out).unwrap(), 160);
        assert!(reader.finish_into(&mut out).is_ok());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn elsewhere_system_audio_is_unsupported() {
        assert!(matches!(
            SystemAudio::open(1.0),
            Err(OpenError::Unsupported)
        ));
        assert!(matches!(
            SystemAudio::start(1.0),
            Err(OpenError::Unsupported)
        ));
    }
}
