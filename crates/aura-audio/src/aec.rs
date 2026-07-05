//! The client's anti-echo stage: AEC3 echo cancellation over the mic uplink.
//!
//! On open speakers the mic re-captures the model's own voice; the realtime
//! provider's VAD runs SERVER-side on whatever PCM we upload and has no access
//! to the far-end signal, so it hears the echo as "the user started speaking"
//! and the model barges in on itself in a loop. Only the client holds both
//! signals — the mic capture (near-end) and what the speaker actually plays
//! (far-end) — so echo cancellation must happen here, before the mic PCM
//! enters the tunnel.
//!
//! [`EchoStage`] wraps the pure-Rust WebRTC audio-processing port (`sonora`,
//! AEC3 + noise suppression): the far-end reference comes from the
//! [`FarEndTap`] the output callback feeds at playout-pop time, the near-end
//! is every mic chunk passing through `CpalTransport::recv_pcm24`. Frames are
//! strictly 10 ms (240 samples @ 24 kHz) — the APM only `debug_assert`s frame
//! lengths, so this module enforces them by construction. AEC3 estimates the
//! render↔capture delay internally; its transparent mode detects the headset
//! (echo-free) case and backs off suppression, so the stage is safe to keep
//! always-on.
//!
//! Degradation ladder (never a crashed call):
//! * `AURA_AEC=on` (default) — full-duplex AEC3; barge-in works on speakers.
//!   While the canceller is still converging (the warmup window) the uplink is
//!   muted during playout so the not-yet-cancelled echo can't trip the
//!   server VAD once at call start.
//! * any APM processing error → the stage permanently falls back to the gate
//!   for the rest of the call (logged once).
//! * `AURA_AEC=gate` — no AEC: the mic uplink is muted while the speaker is
//!   playing (plus a short hangover). Kills barge-in but guarantees no loop.
//! * `AURA_AEC=off` — raw passthrough (headset users who want zero DSP).

use std::sync::Arc;
use std::time::{Duration, Instant};

use sonora::config::{
    EchoCanceller, NoiseSuppression as ApmNoiseSuppression, NoiseSuppressionLevel,
};
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::{FarEndTap, NoiseSuppression};

/// The uplink sample rate the stage operates at (the wire format).
const RATE: u32 = 24_000;
/// One APM frame: exactly 10 ms — 240 samples @ 24 kHz.
const FRAME: usize = RATE as usize / 100;
/// Post-playout hangover for the gate paths: keeps the uplink muted while the
/// speaker's tail + room reverb decay, so the fading echo can't re-trigger the
/// server VAD right after the queue empties.
const GATE_HANGOVER: Duration = Duration::from_millis(250);
/// Warmup: how much ACTIVE (non-silent) far-end audio AEC3 must have seen
/// before the uplink is trusted during playout. 150 frames = 1.5 s of speech —
/// AEC3 typically converges well within it.
const WARMUP_ACTIVE_FRAMES: u32 = 150;
/// A render frame counts as "active" when any sample clears this amplitude
/// (~-44 dBFS) — pure silence teaches the canceller nothing.
const ACTIVITY_AMPLITUDE: i16 = 200;
/// Ceiling on far-end samples drained per mic chunk. Normally a 20 ms chunk
/// drains ~20 ms of playout; after a long mic stall the tap may hold seconds —
/// draining it all at once is fine (it is just buffered arithmetic), this only
/// bounds the transient allocation.
const MAX_DRAIN_PER_CALL: usize = 48_000;

/// Operating mode, resolved once at client start from `AURA_AEC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AecMode {
    /// Full-duplex echo cancellation (default).
    On,
    /// No AEC — mute the mic uplink while the speaker plays (+ hangover).
    GateOnly,
    /// Raw passthrough (no AEC, no gate).
    Off,
}

impl AecMode {
    /// Parse `AURA_AEC`: `off|0|false|no` → [`Off`](Self::Off),
    /// `gate|half-duplex` → [`GateOnly`](Self::GateOnly), anything else
    /// (including unset) → [`On`](Self::On).
    pub fn from_env() -> Self {
        match std::env::var("AURA_AEC") {
            Ok(v) => Self::parse(&v),
            Err(_) => Self::On,
        }
    }

    fn parse(v: &str) -> Self {
        match v.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "no" => Self::Off,
            "gate" | "half-duplex" => Self::GateOnly,
            _ => Self::On,
        }
    }
}

/// Integer nearest resampler for the far-end drain (output rate → 24 kHz) —
/// the same accumulator scheme as `resample_nearest` in `lib.rs`, kept as a
/// struct because the drain is incremental across calls.
struct DrainConverter {
    input_rate: u32,
    accumulator: u32,
}

impl DrainConverter {
    fn new(input_rate: u32) -> Self {
        Self {
            input_rate: input_rate.max(1),
            accumulator: 0,
        }
    }

    fn push(&mut self, sample: i16, out: &mut Vec<i16>) {
        self.accumulator = self.accumulator.saturating_add(RATE);
        while self.accumulator >= self.input_rate {
            out.push(sample);
            self.accumulator -= self.input_rate;
        }
    }
}

/// The live AEC3 pipeline state (present only in [`AecMode::On`] until an
/// error demotes the stage to the gate).
struct AecCore {
    apm: AudioProcessing,
    /// Rate the tapped far-end samples are at; converter rebuilt on change.
    farend_rate: u32,
    converter: DrainConverter,
    /// 24 kHz far-end samples awaiting a full 240-sample render frame.
    render_pending: Vec<i16>,
    /// Mic samples awaiting a full 240-sample capture frame.
    capture_pending: Vec<i16>,
    /// Cleaned samples ready to emit (lags input by < one frame).
    processed: Vec<i16>,
    /// Scratch frame for APM output (reused; no steady-state allocation).
    scratch: Vec<i16>,
    /// Non-silent render frames fed so far — drives the warmup gate.
    active_render_frames: u32,
}

impl AecCore {
    fn new(noise: NoiseSuppression) -> Self {
        let config = Config {
            echo_canceller: Some(EchoCanceller::default()),
            noise_suppression: map_noise(noise),
            ..Default::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(RATE, 1))
            .render_config(StreamConfig::new(RATE, 1))
            .build();
        Self {
            apm,
            farend_rate: 0,
            converter: DrainConverter::new(1),
            render_pending: Vec::with_capacity(4 * FRAME),
            capture_pending: Vec::with_capacity(4 * FRAME),
            processed: Vec::with_capacity(4 * FRAME),
            scratch: vec![0; FRAME],
            active_render_frames: 0,
        }
    }

    /// True while AEC3 has not yet seen enough active far-end audio to be
    /// trusted with the uplink during playout.
    fn warming_up(&self) -> bool {
        self.active_render_frames < WARMUP_ACTIVE_FRAMES
    }

    /// Drain the far-end tap, feed complete render frames, then process the
    /// mic chunk through the canceller. Returns `Err` on any APM error (the
    /// caller demotes the stage to the gate).
    fn process(&mut self, farend: &FarEndTap, chunk: &[i16]) -> Result<Vec<i16>, sonora::Error> {
        // --- far-end: tap (at output rate) → 24 kHz → 240-sample frames ------
        let tap_rate = farend.sample_rate();
        if tap_rate != self.farend_rate {
            // Device/rate change: the tap was cleared by the rebuild; any
            // partial pending frame mixes rates — drop it and start clean.
            self.farend_rate = tap_rate;
            self.converter = DrainConverter::new(tap_rate);
            self.render_pending.clear();
        }
        let mut raw = Vec::new();
        farend.drain_into(&mut raw, MAX_DRAIN_PER_CALL);
        for s in raw {
            self.converter.push(s, &mut self.render_pending);
        }
        let mut offset = 0;
        while self.render_pending.len() - offset >= FRAME {
            let frame = &self.render_pending[offset..offset + FRAME];
            if frame.iter().any(|s| s.abs() >= ACTIVITY_AMPLITUDE) {
                self.active_render_frames = self.active_render_frames.saturating_add(1);
            }
            self.apm.process_render_i16(frame, &mut self.scratch)?;
            offset += FRAME;
        }
        self.render_pending.drain(..offset);

        // --- near-end: mic chunk → 240-sample frames → canceller -------------
        self.capture_pending.extend_from_slice(chunk);
        let mut offset = 0;
        while self.capture_pending.len() - offset >= FRAME {
            let frame = &self.capture_pending[offset..offset + FRAME];
            self.apm.process_capture_i16(frame, &mut self.scratch)?;
            self.processed.extend_from_slice(&self.scratch);
            offset += FRAME;
        }
        self.capture_pending.drain(..offset);

        // --- emit: same length as the input chunk (constant sub-frame lag) ---
        let want = chunk.len();
        let mut out = Vec::with_capacity(want);
        let have = self.processed.len().min(want);
        // Zero-pad the (at most once, at stage start) shortfall so the chunk
        // cadence the pacer sees never changes.
        out.resize(want - have, 0);
        out.extend(self.processed.drain(..have));
        Ok(out)
    }
}

/// Map the user-facing noise-suppression setting to the APM config.
fn map_noise(noise: NoiseSuppression) -> Option<ApmNoiseSuppression> {
    let level = match noise {
        NoiseSuppression::Off => return None,
        NoiseSuppression::Soft => NoiseSuppressionLevel::Low,
        NoiseSuppression::Medium => NoiseSuppressionLevel::Moderate,
        NoiseSuppression::Strong => NoiseSuppressionLevel::High,
    };
    Some(ApmNoiseSuppression {
        level,
        ..Default::default()
    })
}

/// The mic-uplink echo stage. One per call, owned by `CpalTransport`; every
/// outgoing mic chunk passes through [`process_capture`](Self::process_capture)
/// before it reaches the tunnel.
pub struct EchoStage {
    farend: Arc<FarEndTap>,
    /// `Some` while AEC3 is live; `None` after demotion (gate) or in the
    /// gate/off modes.
    core: Option<AecCore>,
    mode: AecMode,
    /// Gate state: muted until this instant (playout + hangover).
    gate_until: Option<Instant>,
    hangover: Duration,
}

impl EchoStage {
    /// Build the stage for `mode`, arming the far-end tap when AEC is on.
    pub fn new(farend: Arc<FarEndTap>, noise: NoiseSuppression, mode: AecMode) -> Self {
        let core = match mode {
            AecMode::On => {
                farend.arm();
                Some(AecCore::new(noise))
            }
            AecMode::GateOnly | AecMode::Off => None,
        };
        match mode {
            AecMode::On => eprintln!(
                "[aura-audio] echo cancellation ON (AEC3; noise suppression '{noise}'); \
                 set AURA_AEC=off to bypass"
            ),
            AecMode::GateOnly => eprintln!(
                "[aura-audio] echo GATE mode: mic muted while the speaker plays (no barge-in, \
                 noise suppression inactive); set AURA_AEC=on for full-duplex echo cancellation"
            ),
            AecMode::Off => eprintln!(
                "[aura-audio] echo processing OFF (AURA_AEC=off): use headphones, or the \
                 model may hear itself and barge-in on its own voice"
            ),
        }
        Self {
            farend,
            core,
            mode,
            gate_until: None,
            hangover: GATE_HANGOVER,
        }
    }

    /// Process one outgoing mic chunk. `queued_ms` is the playback queue depth
    /// (`PlaybackHandle::queued_ms`) — the "is the speaker busy" signal for the
    /// gate paths. Always returns a chunk of the same length; never panics.
    pub fn process_capture(&mut self, chunk: Vec<i16>, queued_ms: u64) -> Vec<i16> {
        match self.mode {
            AecMode::Off => chunk,
            AecMode::GateOnly => self.gate(chunk, queued_ms),
            AecMode::On => {
                // (result, warming-up) — computed in one scope so the mutable
                // borrow of `core` ends before the gate paths borrow `self`.
                let outcome = self
                    .core
                    .as_mut()
                    .map(|core| (core.process(&self.farend, &chunk), core.warming_up()));
                match outcome {
                    // Demoted after an earlier APM error — permanent gate.
                    None => self.gate(chunk, queued_ms),
                    Some((Ok(cleaned), warming)) => {
                        if warming {
                            // Not yet converged: echo may leak through — mute
                            // the uplink while the speaker is busy so the
                            // first response can't barge-in on itself.
                            self.gate(cleaned, queued_ms)
                        } else {
                            self.gate_until = None;
                            cleaned
                        }
                    }
                    Some((Err(err), _)) => {
                        eprintln!(
                            "[aura-audio] echo canceller error ({err:?}); falling back to the \
                             half-duplex gate (noise suppression off) for the rest of the call"
                        );
                        self.core = None;
                        // Mute this chunk and arm the gate — same length out.
                        self.gate_until = Some(Instant::now() + self.hangover);
                        let mut muted = chunk;
                        muted.iter_mut().for_each(|s| *s = 0);
                        muted
                    }
                }
            }
        }
    }

    /// Half-duplex gate: zero the chunk while the speaker is playing and for a
    /// short hangover afterwards. Length-preserving.
    fn gate(&mut self, mut chunk: Vec<i16>, queued_ms: u64) -> Vec<i16> {
        let now = Instant::now();
        if queued_ms > 0 {
            self.gate_until = Some(now + self.hangover);
        }
        let muted = self.gate_until.is_some_and(|until| now < until);
        if muted {
            chunk.iter_mut().for_each(|s| *s = 0);
        }
        chunk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap_at(rate: u32) -> Arc<FarEndTap> {
        let tap = Arc::new(FarEndTap::new(rate));
        tap.arm();
        tap
    }

    /// Speech-like far-end: amplitude-modulated multi-tone with pauses.
    fn far_end_sample(n: usize) -> f32 {
        let t = n as f32 / RATE as f32;
        let syllable = (2.0 * std::f32::consts::PI * 4.0 * t).sin().max(0.0);
        let phrase = if (t % 2.0) < 1.4 { 1.0 } else { 0.0 };
        let carrier = (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.6
            + (2.0 * std::f32::consts::PI * 447.0 * t).sin() * 0.3
            + (2.0 * std::f32::consts::PI * 991.0 * t).sin() * 0.1;
        carrier * syllable * phrase * 0.5
    }

    /// Local user speech for the double-talk segment (different pitch).
    fn user_sample(n: usize) -> f32 {
        let t = n as f32 / RATE as f32;
        let syllable = (2.0 * std::f32::consts::PI * 3.0 * t + 1.0).sin().max(0.0);
        let carrier = (2.0 * std::f32::consts::PI * 150.0 * t).sin() * 0.7
            + (2.0 * std::f32::consts::PI * 310.0 * t).sin() * 0.3;
        carrier * syllable * 0.5
    }

    fn to_i16(x: f32) -> i16 {
        (x.clamp(-1.0, 1.0) * 32767.0) as i16
    }

    fn energy_db(samples: &[i16]) -> f32 {
        if samples.is_empty() {
            return -120.0;
        }
        let sum: f64 = samples
            .iter()
            .map(|&s| (s as f64 / 32768.0).powi(2))
            .sum::<f64>()
            / samples.len() as f64;
        10.0 * (sum.max(1e-12)).log10() as f32
    }

    /// End-to-end simulation on the REAL pipeline: the model's voice goes into
    /// the far-end tap (as the output callback would), the mic chunk carries a
    /// delayed + attenuated echo of it (speaker → room → mic), and the stage
    /// must attenuate that echo well past what a server VAD would trip on,
    /// while double-talk (the user interrupting) still passes.
    #[test]
    fn aec_cancels_speaker_echo_and_keeps_double_talk() {
        const CHUNK: usize = 480; // 20 ms @ 24 kHz — the CpalTransport cadence
        const ECHO_DELAY: usize = (RATE as usize * 60) / 1000; // 60 ms room path
        let tap = tap_at(RATE); // tap at 24 kHz: rate-identity drain
        let mut stage = EchoStage::new(tap.clone(), NoiseSuppression::Off, AecMode::On);

        let mut raw_tail: Vec<i16> = Vec::new(); // raw mic 6-8 s (echo only)
        let mut out_tail: Vec<i16> = Vec::new(); // processed 6-8 s
        let mut out_double_talk: Vec<i16> = Vec::new(); // processed 8.5-10.5 s

        for chunk_idx in 0..700 {
            let base = chunk_idx * CHUNK;
            let t_s = base as f32 / RATE as f32;
            // The output callback "plays" this chunk: mirror into the tap.
            for i in 0..CHUNK {
                tap.push(to_i16(far_end_sample(base + i)));
            }
            let double_talk = (8.0..11.0).contains(&t_s);
            let mic: Vec<i16> = (0..CHUNK)
                .map(|i| {
                    let n = base + i;
                    let echo = if n >= ECHO_DELAY {
                        far_end_sample(n - ECHO_DELAY) * 0.5
                    } else {
                        0.0
                    };
                    let user = if double_talk { user_sample(n) } else { 0.0 };
                    to_i16(echo + user)
                })
                .collect();
            let out = stage.process_capture(mic.clone(), 1_000);
            assert_eq!(out.len(), mic.len(), "stage must preserve chunk length");
            if (300..400).contains(&chunk_idx) {
                raw_tail.extend_from_slice(&mic);
                out_tail.extend_from_slice(&out);
            }
            if (425..525).contains(&chunk_idx) {
                out_double_talk.extend_from_slice(&out);
            }
        }

        let attenuation = energy_db(&raw_tail) - energy_db(&out_tail);
        assert!(
            attenuation >= 15.0,
            "echo must be attenuated by >= 15 dB after convergence, got {attenuation:.1} dB"
        );
        assert!(
            energy_db(&out_double_talk) > -35.0,
            "the user's interrupting speech must survive cancellation, got {:.1} dB",
            energy_db(&out_double_talk)
        );
    }

    #[test]
    fn warmup_gates_uplink_until_converged() {
        let tap = tap_at(RATE);
        let mut stage = EchoStage::new(tap.clone(), NoiseSuppression::Off, AecMode::On);
        // First chunk while the speaker is "playing": the canceller has seen no
        // far-end yet (warming up) → the uplink must be muted, not raw.
        let loud = vec![10_000i16; 480];
        let out = stage.process_capture(loud, 500);
        assert!(
            out.iter().all(|&s| s == 0),
            "uplink must be muted during warmup while the speaker plays"
        );
    }

    #[test]
    fn gate_mode_mutes_while_playing_then_releases_after_hangover() {
        let tap = tap_at(48_000);
        let mut stage = EchoStage::new(tap, NoiseSuppression::Off, AecMode::GateOnly);
        stage.hangover = Duration::from_millis(30); // fast test
        let chunk = vec![5_000i16; 480];
        // Speaker busy → muted.
        assert!(stage
            .process_capture(chunk.clone(), 120)
            .iter()
            .all(|&s| s == 0));
        // Speaker just went idle → still muted within the hangover.
        assert!(stage
            .process_capture(chunk.clone(), 0)
            .iter()
            .all(|&s| s == 0));
        // Past the hangover → passes through.
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(stage.process_capture(chunk.clone(), 0), chunk);
    }

    #[test]
    fn off_mode_is_a_raw_passthrough() {
        let tap = tap_at(48_000);
        let mut stage = EchoStage::new(tap, NoiseSuppression::Off, AecMode::Off);
        let chunk = vec![123i16; 480];
        // Even with the speaker busy, Off never touches the samples.
        assert_eq!(stage.process_capture(chunk.clone(), 1_000), chunk);
    }

    #[test]
    fn output_length_matches_input_for_odd_chunks() {
        let tap = tap_at(RATE);
        let mut stage = EchoStage::new(tap, NoiseSuppression::Off, AecMode::On);
        for &len in &[479usize, 480, 481, 240, 100, 1] {
            let out = stage.process_capture(vec![0i16; len], 0);
            assert_eq!(out.len(), len, "chunk length {len} must be preserved");
        }
    }

    #[test]
    fn farend_rate_change_rebuilds_cleanly() {
        let tap = tap_at(48_000);
        let mut stage = EchoStage::new(tap.clone(), NoiseSuppression::Off, AecMode::On);
        for i in 0..960 {
            tap.push((i % 100) as i16);
        }
        let _ = stage.process_capture(vec![0i16; 480], 0);
        // Simulate a device rebuild to a different output rate.
        tap.reconfigure(44_100);
        for i in 0..882 {
            tap.push((i % 100) as i16);
        }
        // Must not panic and must keep the length contract.
        let out = stage.process_capture(vec![0i16; 480], 0);
        assert_eq!(out.len(), 480);
    }

    #[test]
    fn env_mode_parsing() {
        assert_eq!(AecMode::parse("off"), AecMode::Off);
        assert_eq!(AecMode::parse("0"), AecMode::Off);
        assert_eq!(AecMode::parse("FALSE"), AecMode::Off);
        assert_eq!(AecMode::parse("no"), AecMode::Off);
        assert_eq!(AecMode::parse("gate"), AecMode::GateOnly);
        assert_eq!(AecMode::parse("half-duplex"), AecMode::GateOnly);
        assert_eq!(AecMode::parse("on"), AecMode::On);
        assert_eq!(AecMode::parse(""), AecMode::On);
        assert_eq!(AecMode::parse("anything"), AecMode::On);
    }

    #[test]
    fn noise_mapping_covers_all_levels() {
        assert!(map_noise(NoiseSuppression::Off).is_none());
        assert_eq!(
            map_noise(NoiseSuppression::Soft).unwrap().level,
            NoiseSuppressionLevel::Low
        );
        assert_eq!(
            map_noise(NoiseSuppression::Medium).unwrap().level,
            NoiseSuppressionLevel::Moderate
        );
        assert_eq!(
            map_noise(NoiseSuppression::Strong).unwrap().level,
            NoiseSuppressionLevel::High
        );
    }
}
