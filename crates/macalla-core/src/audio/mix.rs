// SPDX-License-Identifier: MIT
//! Interleaved multi-channel f32 to mono.
//!
//! This runs on the pipeline thread, not in the audio callback. The callback
//! copies interleaved samples into the ring and does nothing else; everything
//! that costs time happens on this side of it.

/// Average `channels` interleaved channels into mono, appending to `out`.
///
/// A trailing partial frame is ignored rather than mixed short: capture hands
/// over whole frames, and a partial one means the caller sliced the buffer in
/// the wrong place.
pub fn downmix_into(interleaved: &[f32], channels: u16, out: &mut Vec<f32>) {
    let n = channels.max(1) as usize;
    if n == 1 {
        out.extend_from_slice(interleaved);
        return;
    }
    let scale = 1.0 / n as f32;
    out.reserve(interleaved.len() / n);
    for frame in interleaved.chunks_exact(n) {
        out.push(frame.iter().sum::<f32>() * scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_input_passes_through_untouched() {
        let mut out = Vec::new();
        downmix_into(&[0.25, -0.5, 1.0], 1, &mut out);
        assert_eq!(out, vec![0.25, -0.5, 1.0]);
    }

    #[test]
    fn stereo_is_averaged_not_summed() {
        // Summing is the easy mistake and it clips: two channels at 0.8 give
        // 1.6, which is out of range for f32 audio.
        let mut out = Vec::new();
        downmix_into(&[0.8, 0.8, 1.0, -1.0], 2, &mut out);
        assert_eq!(out, vec![0.8, 0.0]);
    }

    #[test]
    fn a_partial_trailing_frame_is_dropped_not_mixed_short() {
        let mut out = Vec::new();
        downmix_into(&[1.0, 1.0, 0.5], 2, &mut out);
        assert_eq!(out, vec![1.0], "the lone 0.5 is half a frame");
    }

    #[test]
    fn output_is_appended_so_the_caller_can_reuse_one_buffer() {
        let mut out = vec![-1.0];
        downmix_into(&[0.5, 0.5], 2, &mut out);
        assert_eq!(out, vec![-1.0, 0.5]);
    }

    #[test]
    fn six_channel_input_averages_every_channel() {
        let mut out = Vec::new();
        downmix_into(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0], 6, &mut out);
        assert_eq!(out, vec![2.5]);
    }
}
