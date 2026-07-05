//! `CpalTransport` — the thin client's audio I/O: cpal mic → 24k PCM frames,
//! and 24k PCM frames → speaker.
//!
//! A thin wrapper over [`start_live_audio`]: the mic side is the input receiver
//! (480-sample / 20 ms frames @ 24k), the speaker side is the lock-free
//! [`PlaybackHandle`]. Every outgoing mic frame passes through the
//! [`EchoStage`] (AEC3 echo cancellation — see [`crate::aec`]) so an open
//! speaker can't feed the model's own voice back into the server-side VAD. It
//! exposes plain inherent methods (no engine trait) so the client binary does
//! not pull `aura-engine`; the REMOTE client pumps these frames straight
//! into/out of the `aura-tunnel` endpoint.

use tokio::sync::mpsc;

use crate::{
    start_live_audio, AecMode, AudioError, AudioSettings, EchoStage, LiveAudioSession,
    PlaybackHandle,
};

/// Frames: PCM16 mono LE @ 24 kHz, ~20 ms chunks.
const TARGET_SAMPLE_RATE: u32 = 24_000;
const CHUNK_MS: u32 = 20;

/// cpal mic + speaker, streaming 24 kHz / 20 ms frames for the client.
pub struct CpalTransport {
    /// The live session owns the cpal worker thread; kept alive for the call's
    /// duration. Dropping it stops the audio threads cleanly.
    _session: LiveAudioSession,
    /// Mic frames (already downmixed/resampled to 24k, 20 ms each).
    mic: mpsc::Receiver<Vec<i16>>,
    /// Lock-free playout queue for incoming audio.
    playback: PlaybackHandle,
    /// Anti-echo stage on the mic uplink (mode from `AURA_AEC`, default on).
    aec: EchoStage,
}

impl CpalTransport {
    /// Acquire the default (or configured) mic + speaker and start streaming at
    /// 24 kHz / 20 ms. Surfaces device/permission errors synchronously.
    pub fn start(settings: AudioSettings) -> Result<Self, AudioError> {
        let noise = settings.noise_suppression;
        let mut session = start_live_audio(TARGET_SAMPLE_RATE, CHUNK_MS, settings)?;
        let mic = session.take_input_audio().ok_or_else(|| {
            // Unreachable on a fresh session (the receiver is only taken once),
            // but mapped to a typed error rather than an unwrap (audio path).
            AudioError::Stream("input audio receiver already taken".to_owned())
        })?;
        let playback = session.playback.clone();
        let aec = EchoStage::new(playback.farend(), noise, AecMode::from_env());
        Ok(Self {
            _session: session,
            mic,
            playback,
            aec,
        })
    }

    /// Next 20 ms mic frame (24k mono) with echo cancellation applied, or
    /// `None` when the device/channel closes (the client treats that as the
    /// end of the call).
    pub async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
        let chunk = self.mic.recv().await?;
        Some(self.aec.process_capture(chunk, self.playback.queued_ms()))
    }

    /// Play a 24k frame to the speaker. Infallible: `push_pcm_24k` drops the
    /// oldest queued audio on a full queue so playout stays responsive.
    pub fn send_pcm24(&mut self, pcm: &[i16]) {
        self.playback.push_pcm_24k(pcm);
    }

    /// Drop everything queued for playout (barge-in).
    pub fn clear_playout(&self) {
        self.playback.clear_for_barge_in();
    }

    /// Milliseconds currently queued for playout.
    pub fn queued_ms(&self) -> u64 {
        self.playback.queued_ms()
    }
}
