//! `Reframer` — accumulate arbitrary-length PCM into exact fixed-size frames
//! with a carry buffer.
//!
//! The realtime deltas from xAI and the optional Opus frames are NOT aligned to
//! a fixed sample count, but the resampler in [`crate::pipeline`] has FIXED
//! input frames (960 in for 48→24, 480 in for 24→48) and would
//! truncate `>frame` / silence-pad `<frame` — losing speech and breaking FFT
//! continuity. The `Reframer` sits in front of the pipeline: it appends input
//! to a carry buffer and emits only complete frames of exactly `frame_size`,
//! keeping the remainder for next time. It NEVER truncates or pads mid-stream.
//! Padding happens only at end-of-stream via [`Reframer::flush`].
//!
//! Invariant (the thing the tests pin): for any sequence of `push` calls,
//! `sum(emitted_frame_lengths) + carry_len == sum(input_lengths)` — no sample
//! is ever dropped or invented mid-stream.

/// Reframes a PCM stream into fixed-size frames, carrying the remainder.
#[derive(Debug, Clone)]
pub struct Reframer {
    frame_size: usize,
    carry: Vec<i16>,
}

impl Reframer {
    /// Build a reframer that emits frames of exactly `frame_size` samples.
    pub fn new(frame_size: usize) -> Self {
        assert!(frame_size > 0, "frame_size must be non-zero");
        Self {
            frame_size,
            carry: Vec::with_capacity(frame_size * 2),
        }
    }

    /// The frame size this reframer emits.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Samples currently held in the carry buffer (always `< frame_size`
    /// after `push` returns).
    pub fn carry_len(&self) -> usize {
        self.carry.len()
    }

    /// Append `input` and return every complete `frame_size` frame it
    /// produces, in order. The remainder stays in the carry buffer. Never
    /// truncates or pads — partial tails are carried, not mangled.
    pub fn push(&mut self, input: &[i16]) -> Vec<Vec<i16>> {
        self.carry.extend_from_slice(input);
        let full = self.carry.len() / self.frame_size;
        let mut frames = Vec::with_capacity(full);
        for _ in 0..full {
            frames.push(self.carry.drain(..self.frame_size).collect());
        }
        frames
    }

    /// At end-of-stream, emit whatever remains as a final frame, zero-padded
    /// up to `frame_size`. Returns `None` if the carry is empty. This is the
    /// ONLY place padding is allowed (a graceful tail, not mid-stream loss).
    pub fn flush(&mut self) -> Option<Vec<i16>> {
        if self.carry.is_empty() {
            return None;
        }
        let mut frame = std::mem::take(&mut self.carry);
        frame.resize(self.frame_size, 0);
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_multiple_emits_whole_frames_no_carry() {
        let mut rf = Reframer::new(480);
        let input: Vec<i16> = (0..960).map(|i| i as i16).collect();
        let frames = rf.push(&input);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].len(), 480);
        assert_eq!(frames[1].len(), 480);
        assert_eq!(rf.carry_len(), 0);
        // Order preserved across the split.
        assert_eq!(frames[0][0], 0);
        assert_eq!(frames[1][0], 480);
    }

    #[test]
    fn non_multiple_carries_remainder_without_loss() {
        let mut rf = Reframer::new(480);
        let buf = vec![7i16; 700];
        let frames = rf.push(&buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 480);
        assert_eq!(rf.carry_len(), 220); // 700 - 480, carried, NOT padded
    }

    #[test]
    fn carry_invariant_holds_across_many_pushes() {
        // The core property: nothing lost or invented mid-stream.
        let mut rf = Reframer::new(960);
        let chunk_sizes = [300usize, 300, 500, 1, 959, 961, 13, 2000];
        let mut total_in = 0usize;
        let mut total_emitted = 0usize;
        let mut next: i32 = 0;
        for &n in &chunk_sizes {
            let chunk: Vec<i16> = (0..n)
                .map(|_| {
                    let v = (next % 30000) as i16;
                    next += 1;
                    v
                })
                .collect();
            total_in += n;
            for frame in rf.push(&chunk) {
                assert_eq!(frame.len(), 960);
                total_emitted += frame.len();
            }
            // Carry is always a proper partial frame.
            assert!(rf.carry_len() < 960);
        }
        assert_eq!(total_emitted + rf.carry_len(), total_in);
    }

    #[test]
    fn reassembled_stream_matches_input_order() {
        // Push a known ramp in odd chunks; concatenating emitted frames +
        // (unpadded) carry must reproduce the exact input sequence.
        let mut rf = Reframer::new(480);
        let input: Vec<i16> = (0..2000).map(|i| (i % 1000) as i16).collect();
        let mut out: Vec<i16> = Vec::new();
        for chunk in input.chunks(123) {
            for frame in rf.push(chunk) {
                out.extend_from_slice(&frame);
            }
        }
        out.extend_from_slice(&rf.carry); // remaining tail, unpadded
        assert_eq!(out, input);
    }

    #[test]
    fn flush_pads_only_at_end() {
        let mut rf = Reframer::new(480);
        let buf = vec![5i16; 100];
        rf.push(&buf);
        let tail = rf.flush().expect("carry present");
        assert_eq!(tail.len(), 480);
        assert!(tail[..100].iter().all(|&x| x == 5));
        assert!(tail[100..].iter().all(|&x| x == 0)); // zero-padded tail
        assert_eq!(rf.carry_len(), 0);
        assert!(rf.flush().is_none()); // nothing left
    }
}
