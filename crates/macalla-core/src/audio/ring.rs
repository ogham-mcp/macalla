// SPDX-License-Identifier: MIT
//! The single-producer, single-consumer ring between the audio callback and
//! the pipeline thread.
//!
//! The rule: *the audio callback does nothing but copy samples into a ring
//! buffer.* No allocation, no locking, no inference, no downmixing -- all of
//! that happens on the other side. A callback that blocks is a dropout, and a
//! dropout is a lost word.
//!
//! When the consumer falls behind, the newest samples are dropped and counted
//! rather than overwriting what is already queued. Dropping the oldest would
//! corrupt audio the engine is part-way through; dropping the newest loses the
//! tail, which is at least contiguous. Either way it is a defect, so
//! [`CaptureSource::overruns`] exists to make it visible rather than silent.
//!
//! Samples go in and are dropped as whole frames: a ring that dropped half
//! of a stereo frame would put every later frame's right channel on the
//! left.

use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Build a ring holding `capacity` f32 samples of `channels` interleaved
/// channels.
///
/// Size this generously: 30 seconds of stereo 48 kHz is 2.9 M samples, about
/// 11 MB, which is nothing against the 400 MB idle budget and buys the pipeline
/// thread a very long stall before anything is lost.
pub fn capture_ring(capacity: usize, channels: u16) -> (CaptureSink, CaptureSource) {
    let (producer, consumer) = RingBuffer::<f32>::new(capacity);
    let overruns = Arc::new(AtomicU64::new(0));
    let gaps = Arc::new(AtomicU64::new(0));
    (
        CaptureSink {
            producer,
            overruns: Arc::clone(&overruns),
            gaps: Arc::clone(&gaps),
            frame: channels.max(1) as usize,
        },
        CaptureSource {
            consumer,
            overruns,
            gaps,
        },
    )
}

/// The audio-callback end. Lives in the cpal callback and does nothing else.
pub struct CaptureSink {
    producer: Producer<f32>,
    overruns: Arc<AtomicU64>,
    /// Samples of silence owed for a gap but not written. See
    /// [`CaptureSink::skip_frames`].
    gaps: Arc<AtomicU64>,
    /// Samples in one frame: the channel count.
    frame: usize,
}

/// The pipeline end.
pub struct CaptureSource {
    consumer: Consumer<f32>,
    overruns: Arc<AtomicU64>,
    gaps: Arc<AtomicU64>,
}

impl CaptureSink {
    /// Copy `samples`, whole interleaved frames, into the ring. Returns how
    /// many had to be dropped because the consumer had not kept up.
    ///
    /// Whole frames only, both ways: what fits is kept a frame at a time from
    /// the front, and the rest is dropped a frame at a time, so a full ring
    /// never splits a frame. A trailing part-frame is a caller's slicing
    /// mistake, and is dropped and counted with the rest.
    pub fn write(&mut self, samples: &[f32]) -> usize {
        let frame = self.frame;
        let whole = samples.len() / frame * frame;
        let room = (self.producer.slots() / frame * frame).min(whole);
        if room > 0 {
            if let Ok(chunk) = self.producer.write_chunk_uninit(room) {
                chunk.fill_from_iter(samples[..room].iter().copied());
            }
        }
        let dropped = samples.len() - room;
        if dropped > 0 {
            self.overruns.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        dropped
    }

    /// Stand in for `frames` frames the device never delivered, a gap.
    /// At most `in_band` of them, and no more than fit, are written as
    /// zeros in their place; the rest are counted in one step, not written,
    /// for the reader to put back ([`CaptureSource::gaps`]). Returns how
    /// many frames were counted rather than written.
    ///
    /// Counted apart from [`CaptureSource::overruns`]: a gap is the device's,
    /// an overrun is the reader falling behind, and a log must not mistake
    /// a sleep for a slow reader. Zeros written in place are not counted.
    pub fn skip_frames(&mut self, frames: usize, in_band: usize) -> usize {
        let frame = self.frame;
        let written = frames.min(in_band).min(self.producer.slots() / frame);
        if written > 0 {
            if let Ok(chunk) = self.producer.write_chunk_uninit(written * frame) {
                chunk.fill_from_iter(std::iter::repeat(0.0));
            }
        }
        let counted = frames - written;
        if counted > 0 {
            self.gaps
                .fetch_add((counted * frame) as u64, Ordering::Relaxed);
        }
        counted
    }
}

impl CaptureSource {
    /// Drain everything available, appending to `out`. Returns how many
    /// samples were appended.
    pub fn read_into(&mut self, out: &mut Vec<f32>) -> usize {
        let available = self.consumer.slots();
        if available == 0 {
            return 0;
        }
        let Ok(chunk) = self.consumer.read_chunk(available) else {
            return 0;
        };
        let (first, second) = chunk.as_slices();
        out.extend_from_slice(first);
        out.extend_from_slice(second);
        chunk.commit_all();
        available
    }

    /// Samples lost to overruns since capture started. Non-zero is a bug worth
    /// logging loudly.
    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }

    /// Samples of silence a gap was owed that the ring did not hold, since
    /// capture started. See [`CaptureSink::skip_frames`].
    pub fn gaps(&self) -> u64 {
        self.gaps.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_come_out_in_the_order_they_went_in() {
        let (mut sink, mut source) = capture_ring(64, 1);
        sink.write(&[1.0, 2.0, 3.0]);
        sink.write(&[4.0]);
        let mut out = Vec::new();
        assert_eq!(source.read_into(&mut out), 4);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn reading_an_empty_ring_appends_nothing_and_does_not_block() {
        let (_sink, mut source) = capture_ring(8, 1);
        let mut out = vec![9.0];
        assert_eq!(source.read_into(&mut out), 0);
        assert_eq!(out, vec![9.0], "existing contents must survive");
    }

    #[test]
    fn the_ring_wraps_rather_than_filling_up_permanently() {
        let (mut sink, mut source) = capture_ring(8, 1);
        let mut out = Vec::new();
        // Ten times round a ring that holds eight.
        for round in 0..10u32 {
            let block: Vec<f32> = (0..8).map(|i| (round * 8 + i) as f32).collect();
            assert_eq!(sink.write(&block), 0, "round {round} should fit");
            source.read_into(&mut out);
        }
        assert_eq!(out.len(), 80);
        assert_eq!(out[79], 79.0);
    }

    #[test]
    fn a_stalled_consumer_costs_the_newest_samples_and_they_are_counted() {
        let (mut sink, source) = capture_ring(4, 1);
        assert_eq!(sink.write(&[1.0, 2.0, 3.0, 4.0]), 0);
        assert_eq!(
            sink.write(&[5.0, 6.0]),
            2,
            "the ring is full, so both are dropped"
        );
        assert_eq!(source.overruns(), 2);
    }

    #[test]
    fn a_partial_write_keeps_what_fits_rather_than_dropping_the_whole_block() {
        let (mut sink, mut source) = capture_ring(4, 1);
        sink.write(&[1.0, 2.0, 3.0]);
        assert_eq!(sink.write(&[4.0, 5.0]), 1, "room for one of the two");
        let mut out = Vec::new();
        source.read_into(&mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn a_real_producer_thread_delivers_every_sample_in_order() {
        // The callback runs on its own thread. This is the shape that matters
        // and the one a single-threaded test would not exercise.
        const BLOCK: usize = 128;
        const BLOCKS: usize = 800;
        const TOTAL: usize = BLOCK * BLOCKS;
        let (mut sink, mut source) = capture_ring(4_096, 1);

        let producer = std::thread::spawn(move || {
            for chunk in 0..BLOCKS {
                let block: Vec<f32> = (0..BLOCK).map(|i| (chunk * BLOCK + i) as f32).collect();
                // Retry rather than drop: this test is about ordering, and the
                // overrun path has its own test. `write` is a PARTIAL write, so
                // the retry must resume from what it accepted -- re-sending the
                // whole block duplicates whatever already fit.
                let mut sent = 0;
                while sent < block.len() {
                    let dropped = sink.write(&block[sent..]);
                    sent = block.len() - dropped;
                    if dropped > 0 {
                        std::thread::yield_now();
                    }
                }
            }
        });

        let mut out = Vec::with_capacity(TOTAL);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while out.len() < TOTAL {
            source.read_into(&mut out);
            assert!(
                std::time::Instant::now() < deadline,
                "only {} of {TOTAL} samples arrived -- fail rather than hang",
                out.len()
            );
        }
        producer.join().expect("producer thread");

        assert_eq!(out.len(), TOTAL);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(*v, i as f32, "sample {i} arrived out of order");
        }
        // Deliberately no assertion on `overruns` here. Whether the consumer
        // keeps up is a scheduling race, and every retry above increments the
        // counter by what did not fit. Asserting zero made this test flaky. The
        // overrun path has its own deterministic tests.
    }

    #[test]
    fn a_full_ring_drops_whole_frames_never_half_of_one() {
        // Room for five samples, which is two and a half stereo frames.
        let (mut sink, mut source) = capture_ring(5, 2);
        assert_eq!(
            sink.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            2,
            "the third frame"
        );
        assert_eq!(source.overruns(), 2);
        let mut out = Vec::new();
        source.read_into(&mut out);
        assert_eq!(
            out,
            [1.0, 2.0, 3.0, 4.0],
            "two whole frames, left then right"
        );

        // Three slots: one frame fits, the next finds one slot and is
        // dropped whole, so the frame after it still starts on a left sample.
        assert_eq!(sink.write(&[7.0, 8.0]), 0);
        assert_eq!(sink.write(&[9.0, 10.0, 11.0, 12.0]), 2);
        source.read_into(&mut out);
        assert_eq!(sink.write(&[13.0, 14.0]), 0);
        source.read_into(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0, 7.0, 8.0, 9.0, 10.0, 13.0, 14.0]);
    }

    #[test]
    fn a_trailing_part_frame_is_dropped_and_counted() {
        let (mut sink, mut source) = capture_ring(64, 2);
        assert_eq!(sink.write(&[1.0, 2.0, 3.0]), 1);
        let mut out = Vec::new();
        source.read_into(&mut out);
        assert_eq!(out, [1.0, 2.0]);
        assert_eq!(source.overruns(), 1);
    }

    #[test]
    fn a_gap_is_written_in_place_up_to_the_limit_and_the_rest_counted_apart() {
        let (mut sink, mut source) = capture_ring(64, 2);
        sink.write(&[1.0, 1.0]);
        // Ten frames owed, four allowed in place.
        assert_eq!(sink.skip_frames(10, 4), 6);
        sink.write(&[2.0, 2.0]);

        let mut out = Vec::new();
        source.read_into(&mut out);
        assert_eq!(
            out,
            [1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 2.0]
        );
        assert_eq!(source.gaps(), 12, "six stereo frames");
        assert_eq!(source.overruns(), 0, "a gap is not the reader's lag");
    }

    #[test]
    fn a_gap_in_a_full_ring_is_counted_not_dropped() {
        let (mut sink, source) = capture_ring(4, 2);
        sink.write(&[1.0, 1.0]);
        assert_eq!(sink.skip_frames(5, usize::MAX), 4, "room for one frame");
        assert_eq!(source.gaps(), 8);
        assert_eq!(source.overruns(), 0);
    }

    #[test]
    fn overruns_are_shared_between_the_two_ends() {
        let (mut sink, source) = capture_ring(2, 1);
        sink.write(&[1.0, 2.0, 3.0]);
        assert_eq!(
            source.overruns(),
            1,
            "the consumer end must see what the callback lost"
        );
    }
}
