//! Adaptive jitter buffer for inbound tunnel audio frames.
//!
//! Frames arrive over UDP with network jitter and occasional reordering or
//! loss. This buffer reorders by the full Noise per-packet nonce
//! (wraparound-aware per RFC 1982), holds a target depth
//! between 40 ms and 80 ms (2–4 frames at the 20 ms profile) to absorb jitter,
//! drops already-played late packets, and on a persistent gap (loss) skips
//! ahead rather than stalling — bumping the target depth up so future jitter is
//! absorbed (the "adaptive" part). The outbound side paces sends at
//! [`FRAME_MS`] via a ticker in `endpoint`.

use std::collections::HashMap;

/// One audio frame is 20 ms (the standard Opus frame duration).
pub const FRAME_MS: u64 = 20;
/// Minimum buffered depth: 40 ms.
pub const MIN_DEPTH_FRAMES: usize = 2;
/// Maximum buffered depth: 80 ms.
pub const MAX_DEPTH_FRAMES: usize = 4;
/// During initial prebuffering accept wider reordering than the steady jitter
/// target. A reliable terminal control can arrive first while up to 300 ms of
/// preceding audio is delayed; the 100 ms terminal fallback still bounds a
/// truly lone control's latency.
const INITIAL_REORDER_WINDOW_FRAMES: u64 = 16;

/// Release an in-order control after this many 20 ms ticks even if no later
/// sequence marker arrives. Normally the following authenticated keepalive
/// satisfies prebuffering on the next tick; this is the loss fallback.
const TERMINAL_RELEASE_TICKS: usize = 5;
/// Sustained in-order transport positions needed before reducing an adapted
/// target by one frame. Markers count: a quiet, healthy peer must recover from
/// a transient loss without waiting for another audio burst.
const TARGET_DECAY_RELEASES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushResult {
    Accepted,
    Duplicate,
    TooLate,
}

struct Entry {
    payload: Vec<u8>,
    is_terminal_control: bool,
}

/// RFC 1982 serial comparison for full-width transport sequence numbers: is `a` strictly
/// after `b` (accounting for wraparound)?
fn seq_after(a: u64, b: u64) -> bool {
    a != b && a.wrapping_sub(b) < (1_u64 << 63)
}

/// Reordering, depth-gating jitter buffer over Opus packets keyed by RTP seq.
pub struct JitterBuffer {
    entries: HashMap<u64, Entry>,
    /// Next sequence number to release (set from the first pushed packet).
    next_pop: Option<u64>,
    /// Highest sequence number seen so far.
    highest: Option<u64>,
    /// Current adaptive target depth, in frames, within [MIN, MAX].
    target: usize,
    /// `false` until the initial target depth is reached (pre-buffering); once
    /// `true`, frames are released continuously until an underrun re-buffers.
    playing: bool,
    /// Consecutive `pop()` ticks spent waiting on a missing slot while later
    /// frames are buffered behind it. Bounds reorder tolerance: after
    /// `LOSS_SKIP_TICKS` the gap is declared lost and skipped, so a tail-of-
    /// spurt loss can't strand the trailing frames forever.
    stuck: usize,
    /// Ticks a contiguous terminal burst (audio followed by a control) has
    /// waited below the adaptive target during prebuffering.
    terminal_wait: usize,
    /// In-order positions released since the last gap skip/target decrease.
    stable_releases: usize,
}

/// Ticks (×20 ms) to wait on a gap with later frames buffered before declaring
/// the missing packet lost and skipping it.
const LOSS_SKIP_TICKS: usize = 3;

/// A forward sequence jump larger than this (5 s of frames) is a stream
/// restart, not loss. This remains a defensive recovery path for sequence
/// markers lost during a long partition; normal authenticated keepalives are
/// represented explicitly and keep the sequence timeline continuous.
const RESYNC_GAP_FRAMES: u64 = 250;

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_pop: None,
            highest: None,
            target: MIN_DEPTH_FRAMES,
            playing: false,
            stuck: 0,
            terminal_wait: 0,
            stable_releases: 0,
        }
    }

    /// Current adaptive target depth in frames.
    pub fn target_frames(&self) -> usize {
        self.target
    }

    /// Packets currently buffered.
    pub fn buffered(&self) -> usize {
        self.entries.len()
    }

    /// The span from `next_pop` to `highest` inclusive (frames of "distance",
    /// including any gaps). 0 before the first push or once drained past
    /// `highest`.
    pub fn span(&self) -> usize {
        match (self.next_pop, self.highest) {
            (Some(np), Some(h)) if np == h => 1,
            (Some(np), Some(h)) if seq_after(h, np) => (h.wrapping_sub(np) as usize) + 1,
            // next_pop has advanced past highest → buffer drained.
            _ => 0,
        }
    }

    /// Insert a packet. Packets older than `next_pop` (already released) are
    /// dropped as too-late. The first packet establishes the release baseline;
    /// a huge forward jump (sender was idle, e.g. a gated push-to-talk mic)
    /// re-establishes it instead of walking thousands of phantom losses.
    pub fn push(&mut self, seq: u64, payload: Vec<u8>) -> PushResult {
        self.push_entry(seq, payload, false)
    }

    /// Insert an ordered control frame. Controls share the audio sequence but
    /// may use the bounded lone-control fallback while prebuffering.
    pub fn push_control(&mut self, seq: u64, payload: Vec<u8>, is_terminal: bool) -> PushResult {
        self.push_entry(seq, payload, is_terminal)
    }

    /// Record an authenticated transport sequence position that carries no
    /// application payload (keepalive or internal ACK).
    pub fn push_marker(&mut self, seq: u64) -> PushResult {
        self.push_entry(seq, Vec::new(), false)
    }

    /// Drop buffered application data through an authenticated sequence
    /// position while the consumer is blocked on a reliable control. This is
    /// the explicit backpressure policy: later audio is lossy, later controls
    /// retry because they are not ACKed, and memory remains constant while
    /// decrypt/replay/liveness processing continues.
    pub fn discard_through(&mut self, seq: u64) {
        if self
            .next_pop
            .is_some_and(|np| seq != np && !seq_after(seq, np))
        {
            return;
        }
        self.entries.clear();
        self.next_pop = Some(seq.wrapping_add(1));
        self.highest = Some(seq);
        self.playing = false;
        self.stuck = 0;
        self.terminal_wait = 0;
        self.stable_releases = 0;
    }

    fn push_entry(&mut self, seq: u64, payload: Vec<u8>, is_terminal_control: bool) -> PushResult {
        match self.next_pop {
            None => self.next_pop = Some(seq),
            Some(np)
                if !self.playing
                    && seq_after(np, seq)
                    && np.wrapping_sub(seq) <= INITIAL_REORDER_WINDOW_FRAMES =>
            {
                // The first arrival is not necessarily the first packet. Move
                // the baseline back while pre-buffering so an initially
                // reordered PttOpen is not classified as already played.
                self.next_pop = Some(seq);
            }
            Some(np) if seq_after(seq, np) && seq.wrapping_sub(np) > RESYNC_GAP_FRAMES => {
                // Keepalives consume transport nonces but are not buffered.
                // Resync on the full u64 distance before classifying a packet
                // as late (the old u16 truncation failed after 0x8000 idles).
                self.entries.clear();
                self.next_pop = Some(seq);
                self.highest = None;
                self.playing = false;
                self.stuck = 0;
                self.terminal_wait = 0;
                self.stable_releases = 0;
            }
            Some(np) if seq != np && !seq_after(seq, np) => return PushResult::TooLate,
            _ => {}
        }
        if self.entries.contains_key(&seq) {
            return PushResult::Duplicate;
        }
        match self.highest {
            None => self.highest = Some(seq),
            Some(h) if seq_after(seq, h) => self.highest = Some(seq),
            _ => {}
        }
        self.entries.insert(
            seq,
            Entry {
                payload,
                is_terminal_control,
            },
        );
        PushResult::Accepted
    }

    /// True when every position from `next_pop` through `highest` is present
    /// and the burst contains a terminal control. Releasing this burst after a
    /// bounded wait cannot overtake earlier audio: it starts at `next_pop` and
    /// has no holes. This handles adapted targets of 3/4 when the ACK or the
    /// following keepalive marker is lost.
    fn has_contiguous_terminal_burst(&self) -> bool {
        let (Some(mut seq), Some(highest)) = (self.next_pop, self.highest) else {
            return false;
        };
        loop {
            let Some(entry) = self.entries.get(&seq) else {
                return false;
            };
            if seq == highest {
                return entry.is_terminal_control;
            }
            seq = seq.wrapping_add(1);
        }
    }

    fn note_stable_release(&mut self) {
        self.stable_releases += 1;
        if self.target > MIN_DEPTH_FRAMES && self.stable_releases >= TARGET_DECAY_RELEASES {
            self.target -= 1;
            self.stable_releases = 0;
        }
    }

    /// Release the next in-order frame, or `None` to keep waiting. Waits until
    /// the span reaches the target depth; on a missing packet with an overfull
    /// buffer (span > MAX) it treats the gap as loss, skips it, and bumps the
    /// target up (adaptation).
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        if !self.playing {
            self.next_pop?;
            // Pre-buffer: wait until the initial target depth is reached.
            if self.span() < self.target {
                if self.has_contiguous_terminal_burst() {
                    self.terminal_wait += 1;
                    if self.terminal_wait < TERMINAL_RELEASE_TICKS {
                        return None;
                    }
                } else {
                    self.terminal_wait = 0;
                    return None;
                }
            }
            self.playing = true;
            self.terminal_wait = 0;
        }
        // Iterative (a skip re-examines the NEXT slot; a long run of losses
        // must not grow the stack).
        loop {
            let np = self.next_pop?;
            if let Some(entry) = self.entries.remove(&np) {
                self.next_pop = Some(np.wrapping_add(1));
                self.stuck = 0;
                self.note_stable_release();
                return Some(entry.payload);
            }
            // Missing packet at `np`. Are later frames already buffered behind it?
            let have_later =
                self.highest.is_some_and(|h| seq_after(h, np)) && !self.entries.is_empty();
            // Skip the gap as a confirmed loss when the buffer is overfull, OR when
            // later frames have sat behind it for `LOSS_SKIP_TICKS` (a tail-of-spurt
            // loss never grows span past MAX, so the time bound is what frees those
            // trailing frames instead of stranding them). Skipping adapts target up.
            if self.span() > MAX_DEPTH_FRAMES || (have_later && self.stuck + 1 >= LOSS_SKIP_TICKS) {
                self.target = (self.target + 1).min(MAX_DEPTH_FRAMES);
                self.next_pop = Some(np.wrapping_add(1));
                self.stuck = 0;
                self.stable_releases = 0;
                continue;
            }
            if have_later {
                // Wait a few ticks for a reordered packet before declaring loss.
                self.stuck += 1;
                return None;
            }
            // True underrun: nothing buffered ahead; re-buffer to the target.
            self.playing = false;
            self.stuck = 0;
            self.terminal_wait = 0;
            return None;
        }
    }
}

#[cfg(test)]
#[allow(clippy::useless_vec)]
mod tests {
    use super::*;

    fn pkt(b: u8) -> Vec<u8> {
        vec![b]
    }

    #[test]
    fn seq_after_handles_wraparound() {
        assert!(seq_after(5, 4));
        assert!(!seq_after(4, 5));
        assert!(seq_after(0, u64::MAX)); // wrap forward
        assert!(!seq_after(u64::MAX, 0));
        assert!(!seq_after(3, 3));
    }

    #[test]
    fn releases_in_order_after_target_filled() {
        let mut jb = JitterBuffer::new();
        // target is 2 frames; need span >= 2 before release.
        jb.push(10, pkt(10));
        assert!(jb.pop().is_none()); // span 1 < target 2
        jb.push(11, pkt(11));
        assert_eq!(jb.pop(), Some(pkt(10)));
        assert_eq!(jb.pop(), Some(pkt(11)));
        assert!(jb.pop().is_none());
    }

    #[test]
    fn tail_end_loss_does_not_strand_trailing_frames() {
        let mut jb = JitterBuffer::new();
        jb.push(0, pkt(0));
        jb.push(1, pkt(1));
        assert_eq!(jb.pop(), Some(pkt(0)));
        assert_eq!(jb.pop(), Some(pkt(1))); // playing; next_pop = 2
                                            // Seq 2 is lost; 3 and 4 arrive, then the talker goes silent.
        jb.push(3, pkt(3));
        jb.push(4, pkt(4));
        // span(2..=4) = 3 (never > MAX), so the time bound frees the gap: wait
        // LOSS_SKIP_TICKS, then skip 2 and release the stranded 3, 4.
        assert!(jb.pop().is_none()); // stuck = 1
        assert!(jb.pop().is_none()); // stuck = 2
        assert_eq!(jb.pop(), Some(pkt(3)), "skip lost 2, release trailing 3");
        assert_eq!(jb.pop(), Some(pkt(4)), "and 4 — not stranded");
        assert!(jb.pop().is_none());
    }

    #[test]
    fn reorders_out_of_order_arrivals() {
        let mut jb = JitterBuffer::new();
        jb.push(1, pkt(1));
        jb.push(3, pkt(3)); // arrives before 2
        jb.push(2, pkt(2));
        // span = 3 (1..=3) >= target 2 → release in order.
        assert_eq!(jb.pop(), Some(pkt(1)));
        assert_eq!(jb.pop(), Some(pkt(2)));
        assert_eq!(jb.pop(), Some(pkt(3)));
    }

    #[test]
    fn drops_late_packet_already_past() {
        let mut jb = JitterBuffer::new();
        jb.push(5, pkt(5));
        jb.push(6, pkt(6));
        assert_eq!(jb.pop(), Some(pkt(5))); // next_pop now 6
                                            // A late seq 5 arriving now is older than next_pop → dropped.
        jb.push(5, pkt(99));
        jb.push(7, pkt(7));
        assert_eq!(jb.pop(), Some(pkt(6)));
        assert_eq!(jb.pop(), Some(pkt(7)));
        assert!(!jb.entries.contains_key(&5));
    }

    #[test]
    fn resyncs_after_long_idle_gap_instead_of_walking_it() {
        let mut jb = JitterBuffer::new();
        jb.push(0, pkt(0));
        jb.push(1, pkt(1));
        assert_eq!(jb.pop(), Some(pkt(0)));
        assert_eq!(jb.pop(), Some(pkt(1)));
        // Sender goes idle (keepalives consume seqs 2..=5000 but are never
        // pushed), then resumes: a fresh spurt far ahead of next_pop.
        jb.push(5_000, pkt(10));
        jb.push(5_001, pkt(11));
        // Re-buffers from the new baseline and releases the spurt in order —
        // no thousands of phantom-loss skips first.
        assert_eq!(jb.pop(), Some(pkt(10)));
        assert_eq!(jb.pop(), Some(pkt(11)));
    }

    #[test]
    fn skips_persistent_gap_and_adapts_target_up() {
        let mut jb = JitterBuffer::new();
        // 1,2 present, 3 missing, 4,5,6 present → span grows past MAX (4).
        for s in [1u64, 2, 4, 5, 6] {
            jb.push(s, pkt(s as u8));
        }
        assert_eq!(jb.pop(), Some(pkt(1)));
        assert_eq!(jb.pop(), Some(pkt(2)));
        // next_pop=3 missing; span (3..=6)=4 not > MAX yet → wait.
        // Push 7 so span (3..=7)=5 > MAX → skip 3, continue.
        jb.push(7, pkt(7));
        assert_eq!(jb.pop(), Some(pkt(4))); // 3 skipped as loss
        assert!(jb.target_frames() > MIN_DEPTH_FRAMES); // adapted up
    }

    #[test]
    fn resyncs_across_old_u16_half_range_boundary() {
        let mut jb = JitterBuffer::new();
        jb.push(10, pkt(1));
        jb.push(11, pkt(2));
        assert_eq!(jb.pop(), Some(pkt(1)));
        assert_eq!(jb.pop(), Some(pkt(2)));
        jb.push(10 + 0x8000, pkt(3));
        jb.push(11 + 0x8000, pkt(4));
        assert_eq!(jb.pop(), Some(pkt(3)));
        assert_eq!(jb.pop(), Some(pkt(4)));
    }

    #[test]
    fn initial_reordering_moves_prebuffer_baseline_back() {
        let mut jb = JitterBuffer::new();
        jb.push(101, pkt(2));
        jb.push(100, pkt(1));
        assert_eq!(jb.pop(), Some(pkt(1)));
        assert_eq!(jb.pop(), Some(pkt(2)));
    }

    #[test]
    fn marker_advances_sequence_without_application_payload() {
        let mut jb = JitterBuffer::new();
        jb.push_control(10, pkt(7), true);
        assert!(jb.pop().is_none());
        jb.push_marker(11);
        assert_eq!(jb.pop(), Some(pkt(7)));
        assert_eq!(jb.pop(), Some(Vec::new()));
    }

    #[test]
    fn lone_control_releases_after_bounded_wait() {
        let mut jb = JitterBuffer::new();
        jb.push_control(20, pkt(9), true);
        for _ in 1..TERMINAL_RELEASE_TICKS {
            assert!(jb.pop().is_none());
        }
        assert_eq!(jb.pop(), Some(pkt(9)));
    }

    #[test]
    fn adapted_target_does_not_strand_audio_followed_by_control() {
        let mut jb = JitterBuffer::new();
        jb.target = MAX_DEPTH_FRAMES;
        jb.push(40, pkt(1));
        jb.push_control(41, pkt(9), true);
        for _ in 1..TERMINAL_RELEASE_TICKS {
            assert!(jb.pop().is_none());
        }
        assert_eq!(jb.pop(), Some(pkt(1)));
        assert_eq!(jb.pop(), Some(pkt(9)));
    }

    #[test]
    fn marker_only_health_decays_adapted_target() {
        let mut jb = JitterBuffer::new();
        jb.target = MAX_DEPTH_FRAMES;
        for seq in 0..(TARGET_DECAY_RELEASES as u64 + MAX_DEPTH_FRAMES as u64) {
            jb.push_marker(seq);
            let _ = jb.pop();
        }
        while jb.pop().is_some() {}
        assert!(jb.target_frames() < MAX_DEPTH_FRAMES);
    }

    #[test]
    fn push_control_reports_acceptance_before_ack_is_safe() {
        let mut jb = JitterBuffer::new();
        assert_eq!(jb.push_control(5, pkt(1), true), PushResult::Accepted);
        assert_eq!(jb.push_control(5, pkt(1), true), PushResult::Duplicate);
        jb.push_marker(6);
        assert_eq!(jb.pop(), Some(pkt(1)));
        assert_eq!(jb.push_control(5, pkt(1), true), PushResult::TooLate);
    }

    #[test]
    fn control_first_reordering_beyond_four_frames_preserves_audio_order() {
        let mut jb = JitterBuffer::new();
        jb.push_control(10, pkt(10), true);
        for seq in 5..10 {
            jb.push(seq, pkt(seq as u8));
        }
        for expected in 5..=10 {
            assert_eq!(jb.pop(), Some(pkt(expected as u8)));
        }
    }

    #[test]
    fn ptt_open_does_not_trigger_terminal_short_burst_release() {
        let mut jb = JitterBuffer::new();
        jb.target = MAX_DEPTH_FRAMES;
        jb.push_control(10, pkt(1), false);
        jb.push(11, pkt(2));
        for _ in 0..(TERMINAL_RELEASE_TICKS + 2) {
            assert!(jb.pop().is_none());
        }
    }

    #[test]
    fn discard_through_keeps_backpressure_memory_constant() {
        let mut jb = JitterBuffer::new();
        for seq in 0..10_000 {
            jb.push(seq, pkt(1));
            jb.discard_through(seq);
            assert_eq!(jb.buffered(), 0);
        }
        jb.push(10_000, pkt(9));
        jb.push_marker(10_001);
        assert_eq!(jb.pop(), Some(pkt(9)));
    }
}
