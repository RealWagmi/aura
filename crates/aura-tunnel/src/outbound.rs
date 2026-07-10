//! The shared outbound frame queue behind both transports' 20 ms pacers
//! (`endpoint` for Noise/UDP, `iroh_transport` for iroh QUIC).
//!
//! One queue carries both audio frames and in-band control frames so wire
//! order matches call order. Two rules keep control semantics safe on a queue
//! sized for lossy audio:
//! - **Overflow trims audio, never controls.** Dropping 20 ms of audio is
//!   graceful degradation; dropping a `PttClose` strands the whole turn.
//! - **`clear_audio` (barge-in) keeps controls.** Clearing playout must not
//!   swallow a queued state transition.

use std::collections::VecDeque;

use crate::reframe::Reframer;
use crate::wire::{encode_control_ack, encode_tunnel_control, is_tunnel_control, TunnelControl};

/// One audio frame is 20 ms.
const FRAME_MS: u64 = 20;

/// Cap on the outbound queue — a MEMORY backstop for a dead/stalled pacer, NOT
/// an audio limiter. The realtime API streams a full answer's PCM far faster
/// than the 20 ms realtime pacer drains it, so a LONG answer legitimately backs
/// up MINUTES here. Live-diagnosed 2026-07-07: a ~90 s fable overflowed the old
/// 30 s cap about 40 s into playback and the unlogged drop-oldest audibly ate
/// words ("stumbles ~40 s into long speech"). The cap must exceed any single
/// answer: 5 min @ 50 frames/s = 15 000 frames ≈ 14 MB — still a trivial
/// backstop. Barge-in (`clear_audio`) drains the whole queue instantly
/// regardless of size, so a large cap costs zero interruption latency; hitting
/// the cap is logged loudly (never unlogged again).
pub(crate) const MAX_OUTBOUND_FRAMES: usize = 15_000;
/// Local application controls are reliable but still bounded. A broken caller
/// must not turn the "never evict controls for audio" rule into unbounded RAM.
pub(crate) const MAX_OUTBOUND_CONTROLS: usize = 256;
/// Duplicate-control replay may request the same ACK many times. Keep ACKs
/// separate from media, coalesce by id, and cap the set to the replay window.
pub(crate) const MAX_PENDING_ACKS: usize = 128;

pub(crate) fn pcm_to_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub(crate) fn bytes_to_pcm(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Outbound state behind one lock: the queue the pacer drains, plus the
/// reframer that chops engine audio into exact 20 ms frames. Co-locating them
/// lets `clear_audio` (barge-in) also reset the reframer carry so a stale
/// `<20 ms` partial frame can't prepend onto the next response.
pub(crate) struct Outbound {
    queue: VecDeque<Vec<u8>>,
    reframer: Reframer,
    /// Total frames dropped on overflow (diagnostic; see `MAX_OUTBOUND_FRAMES`).
    dropped_frames: u64,
    frame_samples: usize,
    pending_acks: VecDeque<u64>,
}

impl Outbound {
    pub(crate) fn new(frame_samples: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            reframer: Reframer::new(frame_samples),
            dropped_frames: 0,
            frame_samples,
            pending_acks: VecDeque::new(),
        }
    }

    /// Queue model/mic audio for sending, reframed to exact 20 ms frames. The
    /// queue is bounded (drop-oldest AUDIO) so a stalled/dead pacer can't grow
    /// memory; control frames are never the victim.
    pub(crate) fn push_pcm(&mut self, pcm: &[i16]) {
        let frames = self.reframer.push(pcm);
        for f in frames {
            self.queue.push_back(pcm_to_bytes(&f));
            self.trim_over(MAX_OUTBOUND_FRAMES);
        }
    }

    fn trim_over(&mut self, max: usize) {
        while self.queue.len() > max {
            // Evict the oldest AUDIO frame; skip past any controls at the
            // front so a queued PttOpen/PttClose survives overflow.
            let Some(pos) = self.queue.iter().position(|f| !is_tunnel_control(f)) else {
                return; // nothing but controls (can't happen at real sizes)
            };
            self.queue.remove(pos);
            self.dropped_frames += 1;
            // Loud, rate-limited (first drop, then ~1/s of loss): overflow
            // eats the audio the listener is ABOUT TO HEAR.
            if self.dropped_frames == 1 || self.dropped_frames.is_multiple_of(50) {
                eprintln!(
                    "aura-tunnel: outbound pacer queue FULL ({} min cap) — {} ms of audio \
                     dropped; words are being skipped",
                    max as u64 / 50 / 60,
                    self.dropped_frames * FRAME_MS
                );
            }
        }
    }

    /// Queue an authenticated control event for the peer, in-order with the
    /// audio already queued.
    pub(crate) fn push_control(&mut self, control: TunnelControl) -> bool {
        if self.queue.iter().filter(|f| is_tunnel_control(f)).count() >= MAX_OUTBOUND_CONTROLS {
            eprintln!("aura-tunnel: outbound control queue full; rejecting local control");
            return false;
        }
        self.queue.push_back(encode_tunnel_control(control));
        true
    }

    /// Coalesce a transport-internal acknowledgement. The bounded ACK lane is
    /// drained at one item per pacer tick and cannot displace or starve audio.
    pub(crate) fn queue_ack(&mut self, id: u64) {
        if self.pending_acks.contains(&id) {
            return;
        }
        if self.pending_acks.len() == MAX_PENDING_ACKS {
            self.pending_acks.pop_front();
        }
        self.pending_acks.push_back(id);
    }

    /// Remove an internal ACK even while application output is held behind an
    /// unacknowledged control (prevents simultaneous controls deadlocking).
    pub(crate) fn pop_priority_ack(&mut self) -> Option<Vec<u8>> {
        self.pending_acks.pop_front().map(encode_control_ack)
    }

    /// The next frame for the pacer, or `None` (send a keepalive instead).
    pub(crate) fn pop_next(&mut self) -> Option<Vec<u8>> {
        self.queue.pop_front()
    }

    /// Flush the `<20 ms` reframer tail (padded with silence) so a phrase
    /// ending isn't held back.
    pub(crate) fn flush_tail(&mut self) {
        if let Some(mut tail) = self.reframer.flush() {
            tail.resize(self.frame_samples, 0);
            self.queue.push_back(pcm_to_bytes(&tail));
        }
    }

    /// Drop all queued AUDIO and reset the reframer carry (barge-in must not
    /// leave a stale partial frame to prepend onto the next response). Queued
    /// control frames are kept — a barge-in must not swallow a state change.
    pub(crate) fn clear_audio(&mut self) {
        self.queue.retain(|f| is_tunnel_control(f));
        self.reframer = Reframer::new(self.frame_samples);
    }

    /// Milliseconds of audio queued for sending.
    pub(crate) fn queued_ms(&self) -> u64 {
        self.queue.len() as u64 * FRAME_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::decode_tunnel_control;

    fn frame(v: i16) -> Vec<i16> {
        vec![v; 480]
    }

    #[test]
    fn overflow_trims_oldest_audio_not_controls() {
        let mut ob = Outbound::new(480);
        ob.push_control(TunnelControl::PttOpen);
        for i in 0..4 {
            ob.push_pcm(&frame(i));
        }
        // Trim down to 3 entries: the control (front) must survive; the two
        // OLDEST audio frames after it are the victims.
        ob.trim_over(3);
        assert_eq!(ob.queue.len(), 3);
        assert_eq!(
            decode_tunnel_control(&ob.queue[0]),
            Some(TunnelControl::PttOpen)
        );
        assert_eq!(bytes_to_pcm(&ob.queue[1])[0], 2, "oldest audio evicted");
        assert_eq!(bytes_to_pcm(&ob.queue[2])[0], 3);
        assert_eq!(ob.dropped_frames, 2);
    }

    #[test]
    fn clear_audio_keeps_queued_controls() {
        let mut ob = Outbound::new(480);
        ob.push_pcm(&frame(1));
        ob.push_control(TunnelControl::PttClose);
        ob.push_pcm(&frame(2));
        ob.clear_audio();
        assert_eq!(ob.queue.len(), 1);
        assert_eq!(
            decode_tunnel_control(&ob.queue[0]),
            Some(TunnelControl::PttClose)
        );
    }

    #[test]
    fn pop_preserves_send_order_of_audio_and_controls() {
        let mut ob = Outbound::new(480);
        ob.push_pcm(&frame(1));
        ob.push_control(TunnelControl::PttClose);
        ob.push_pcm(&frame(2));
        assert_eq!(bytes_to_pcm(&ob.pop_next().unwrap())[0], 1);
        assert!(is_tunnel_control(&ob.pop_next().unwrap()));
        assert_eq!(bytes_to_pcm(&ob.pop_next().unwrap())[0], 2);
        assert!(ob.pop_next().is_none());
    }

    #[test]
    fn duplicate_ack_flood_is_coalesced_and_bounded() {
        let mut ob = Outbound::new(480);
        for _ in 0..10_000 {
            ob.queue_ack(7);
        }
        assert_eq!(ob.pending_acks.len(), 1);
        for id in 0..10_000 {
            ob.queue_ack(id);
        }
        assert_eq!(ob.pending_acks.len(), MAX_PENDING_ACKS);
        assert_eq!(ob.pop_priority_ack(), Some(encode_control_ack(9_872)));
    }

    #[test]
    fn local_control_queue_is_bounded_without_evicting_audio() {
        let mut ob = Outbound::new(480);
        ob.push_pcm(&frame(1));
        for _ in 0..MAX_OUTBOUND_CONTROLS {
            assert!(ob.push_control(TunnelControl::PttOpen));
        }
        assert!(!ob.push_control(TunnelControl::PttClose));
        assert_eq!(bytes_to_pcm(&ob.pop_next().unwrap())[0], 1);
    }
}
