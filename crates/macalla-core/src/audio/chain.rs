// SPDX-License-Identifier: MIT
//! The consumer half of capture: ring to mono to 16 kHz, and nothing else.
//!
//! There is deliberately no device here. [`CaptureChain`] is fed by a
//! [`CaptureSource`](super::ring::CaptureSource), so the tests below drive it
//! from fixtures with no microphone, as the project requires -- and `macalla-core`
//! builds on a machine with no audio stack at all.
//!
//! The device that fills the ring lives in `macalla-platform::audio`
//! (a design decision). The seam between them is the ring itself: the callback writes
//! interleaved device-rate samples and returns, and downmixing and resampling
//! both happen on this side, on the pipeline thread.

use super::mix::downmix_into;
use super::resample::Resampler;
use super::ring::CaptureSource;
use crate::Result;

/// Everything between the audio callback and the engine.
pub struct CaptureChain {
    source: CaptureSource,
    channels: u16,
    resampler: Resampler,
    /// Interleaved device-rate samples drained from the ring.
    raw: Vec<f32>,
    /// `raw` downmixed to mono, still at the device rate.
    mono: Vec<f32>,
    /// Whether samples the ring dropped come back out as zeros. See
    /// [`CaptureChain::filling_overruns`].
    fill_overruns: bool,
    /// Dropped samples already put back as zeros.
    overruns_filled: u64,
}

impl CaptureChain {
    /// Wrap the consumer end of a capture ring fed at `input_rate` with
    /// `channels` interleaved channels.
    pub fn new(source: CaptureSource, channels: u16, input_rate: u32) -> Result<Self> {
        Ok(Self {
            source,
            channels: channels.max(1),
            resampler: Resampler::new(input_rate)?,
            raw: Vec::new(),
            mono: Vec::new(),
            fill_overruns: false,
            overruns_filled: 0,
        })
    }

    /// Put zeros where the ring dropped samples, as many as it dropped, so
    /// what comes out keeps the device's clock: a source that promises no
    /// gaps needs this, as a live meeting's second voice does. Off by
    /// default; the microphone does not use it yet.
    ///
    /// The zeros go after what the poll that first sees the loss drained.
    /// The ring drops only while it is full, and while it is full nothing
    /// newer than its contents can be in it, so that is where the lost
    /// samples belonged, to within one poll.
    ///
    /// Silence a gap was owed but the ring did not hold
    /// ([`CaptureSink::skip_frames`](super::ring::CaptureSink::skip_frames))
    /// is put back the same way. That one can land up to one poll late:
    /// audio that followed the gap within the same poll comes out before
    /// its zeros.
    pub fn filling_overruns(mut self) -> Self {
        self.fill_overruns = true;
        self
    }

    /// Drain the ring and append 16 kHz mono to `out`. Returns how many
    /// samples were appended.
    pub fn poll_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
        let before = out.len();
        self.source.read_into(&mut self.raw);
        if self.fill_overruns {
            // Read after the drain: a loss counted from here on belongs
            // after what was just drained. Whole frames only; a part-frame
            // of loss waits for the rest of its frame.
            let n = self.channels as u64;
            let missed = self.source.overruns() + self.source.gaps() - self.overruns_filled;
            let whole = missed / n * n;
            self.overruns_filled += whole;
            self.raw.resize(self.raw.len() + whole as usize, 0.0);
        }

        // Whole frames only. A poll can land mid-frame, and the leftover
        // samples stay in `raw` until the callback delivers their partners --
        // mixing a half frame would put a click in the audio and shift every
        // frame after it by one channel.
        let n = self.channels as usize;
        let whole = (self.raw.len() / n) * n;
        if whole > 0 {
            self.mono.clear();
            downmix_into(&self.raw[..whole], self.channels, &mut self.mono);
            self.raw.drain(..whole);
            self.resampler.process_into(&self.mono, out)?;
        }
        Ok(out.len() - before)
    }

    /// Drain the ring one last time and flush the resampler's tail.
    pub fn finish_into(&mut self, out: &mut Vec<f32>) -> Result<usize> {
        let before = out.len();
        self.poll_into(out)?;
        self.resampler.finish_into(out)?;
        Ok(out.len() - before)
    }

    /// Samples lost because the pipeline thread fell behind the callback.
    pub fn overruns(&self) -> u64 {
        self.source.overruns()
    }

    /// Samples of silence a gap was owed that the ring could not hold. See
    /// [`CaptureSink::skip_frames`](super::ring::CaptureSink::skip_frames).
    pub fn gaps(&self) -> u64 {
        self.source.gaps()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::ring::capture_ring;
    use crate::audio::SAMPLE_RATE;

    /// `seconds` of a `hz` sine at `rate`, duplicated across `channels`.
    fn interleaved_tone(hz: f32, rate: u32, channels: u16, seconds: f32) -> Vec<f32> {
        let frames = (rate as f32 * seconds) as usize;
        let mut v = Vec::with_capacity(frames * channels as usize);
        for i in 0..frames {
            let s = (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin();
            for _ in 0..channels {
                v.push(s);
            }
        }
        v
    }

    #[test]
    fn a_second_of_stereo_forty_eight_k_comes_out_as_a_second_of_mono_sixteen_k() {
        let (mut sink, source) = capture_ring(48_000 * 2 * 2, 2);
        let mut chain = CaptureChain::new(source, 2, 48_000).expect("chain");

        assert_eq!(
            sink.write(&interleaved_tone(440.0, 48_000, 2, 1.0)),
            0,
            "the ring was sized for this"
        );

        let mut out = Vec::new();
        chain.poll_into(&mut out).expect("poll");
        chain.finish_into(&mut out).expect("finish");

        let drift = (out.len() as i64 - SAMPLE_RATE as i64).abs();
        assert!(drift <= 64, "got {} samples (drift {drift})", out.len());
        assert_eq!(chain.overruns(), 0);
    }

    #[test]
    fn polling_repeatedly_while_audio_arrives_gives_the_same_total() {
        // This is what the pipeline thread actually does: poll on a timer while
        // the callback keeps writing.
        let audio = interleaved_tone(440.0, 48_000, 1, 1.0);
        let (mut sink, source) = capture_ring(48_000 * 2, 1);
        let mut chain = CaptureChain::new(source, 1, 48_000).expect("chain");

        let mut out = Vec::new();
        for block in audio.chunks(512) {
            sink.write(block);
            chain.poll_into(&mut out).expect("poll");
        }
        chain.finish_into(&mut out).expect("finish");

        let drift = (out.len() as i64 - SAMPLE_RATE as i64).abs();
        assert!(drift <= 64, "got {} samples (drift {drift})", out.len());
    }

    #[test]
    fn a_device_already_at_sixteen_k_mono_passes_straight_through() {
        let (mut sink, source) = capture_ring(64, 1);
        let mut chain = CaptureChain::new(source, 1, SAMPLE_RATE).expect("chain");
        sink.write(&[0.1, -0.2, 0.3, -0.4]);
        let mut out = Vec::new();
        assert_eq!(chain.poll_into(&mut out).expect("poll"), 4);
        assert_eq!(out, vec![0.1, -0.2, 0.3, -0.4]);
    }

    #[test]
    fn polling_an_idle_microphone_yields_nothing_and_costs_nothing() {
        let (_sink, source) = capture_ring(64, 2);
        let mut chain = CaptureChain::new(source, 2, 48_000).expect("chain");
        let mut out = Vec::new();
        assert_eq!(chain.poll_into(&mut out).expect("poll"), 0);
        assert!(out.is_empty());
    }

    #[test]
    fn an_overrun_is_reported_rather_than_swallowed() {
        // A ring far too small for the block: the callback loses samples and
        // the pipeline has to be able to say so.
        let (mut sink, source) = capture_ring(16, 1);
        let chain = CaptureChain::new(source, 1, SAMPLE_RATE).expect("chain");
        sink.write(&vec![0.0; 100]);
        assert_eq!(chain.overruns(), 84);
    }

    #[test]
    fn dropped_samples_come_back_as_zeros_when_asked() {
        // A ring of four stereo frames, fed six while the reader is away:
        // the last two frames are dropped, and come out as two zero frames.
        let (mut sink, source) = capture_ring(8, 2);
        let mut chain = CaptureChain::new(source, 2, SAMPLE_RATE)
            .expect("chain")
            .filling_overruns();
        let frames: Vec<f32> = (1..=6).flat_map(|f| [f as f32, f as f32]).collect();
        assert_eq!(sink.write(&frames), 4);

        let mut out = Vec::new();
        chain.poll_into(&mut out).expect("poll");
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0], "the clock held");

        // Counted once: the next poll adds no more.
        sink.write(&[7.0, 7.0]);
        chain.poll_into(&mut out).expect("poll");
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0, 7.0]);
        assert_eq!(chain.overruns(), 4, "still reported");
    }

    #[test]
    fn a_gap_the_ring_did_not_hold_comes_back_as_zeros() {
        let (mut sink, source) = capture_ring(64, 1);
        let mut chain = CaptureChain::new(source, 1, SAMPLE_RATE)
            .expect("chain")
            .filling_overruns();
        sink.write(&[1.0]);
        assert_eq!(sink.skip_frames(3, 0), 3);

        let mut out = Vec::new();
        chain.poll_into(&mut out).expect("poll");
        assert_eq!(out, [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(chain.gaps(), 3);
        assert_eq!(chain.overruns(), 0);
    }

    #[test]
    fn dropped_samples_are_not_filled_unless_asked() {
        let (mut sink, source) = capture_ring(2, 1);
        let mut chain = CaptureChain::new(source, 1, SAMPLE_RATE).expect("chain");
        sink.write(&[1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        chain.poll_into(&mut out).expect("poll");
        assert_eq!(out, [1.0, 2.0]);
    }

    #[test]
    fn a_half_frame_left_in_the_ring_is_not_mixed_short() {
        // The callback writes whole frames, but a poll can land mid-block. The
        // odd sample must wait for its partner, not be treated as a frame.
        // A mono ring under a stereo chain: the ring itself only ever holds
        // whole frames, so this is the only way to land a poll mid-frame.
        let (mut sink, source) = capture_ring(64, 1);
        let mut chain = CaptureChain::new(source, 2, SAMPLE_RATE).expect("chain");
        let mut out = Vec::new();

        sink.write(&[1.0, 1.0, 0.5]);
        assert_eq!(chain.poll_into(&mut out).expect("poll"), 1);
        assert_eq!(out, vec![1.0]);

        sink.write(&[0.5]);
        assert_eq!(chain.poll_into(&mut out).expect("poll"), 1);
        assert_eq!(out, vec![1.0, 0.5], "the split frame was reassembled");
    }
}
