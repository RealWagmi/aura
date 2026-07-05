//! `aura-audio` — client-side mic/speaker via cpal, plus `CpalTransport`.
//!
//! The cpal capture+playback worker, the lock-free `PlaybackHandle`, the
//! device-follower, and the integer nearest-resample. `CpalTransport` (see
//! [`transport`]) exposes inherent 24 kHz frame I/O (`recv_pcm24`/`send_pcm24`)
//! that the thin client pumps into/out of the `aura-tunnel` endpoint — no
//! `aura-engine` dependency. The [`aec`] module is the client's anti-echo
//! stage (AEC3 echo cancellation over the mic uplink) fed its far-end
//! reference by the [`FarEndTap`] wired into the playback pop path.

pub mod aec;
pub mod transport;
pub use aec::{AecMode, EchoStage};
pub use transport::CpalTransport;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Public audio-settings types
// ---------------------------------------------------------------------------

/// Runtime audio I/O configuration passed to [`start_live_audio`].
///
/// All fields have sensible defaults (see [`Default`] impl below):
/// no device preference, unity gain, medium noise suppression.
/// The [`Default`] impl is intentionally `#[derive]`-free so the
/// `input_gain` field can default to `1.0` rather than `0.0`.
#[derive(Debug, Clone)]
pub struct AudioSettings {
    /// Case-insensitive substring match against the cpal device name.
    /// `None` (or no substring match) falls back to the cpal host
    /// default.
    pub input_device_name: Option<String>,
    /// Same semantics as [`input_device_name`] for the output side.
    pub output_device_name: Option<String>,
    /// Linear amplitude multiplier applied to every input sample.
    /// Clamped to `0.0..=2.0` at stream-build time; `1.0` is unity.
    pub input_gain: f32,
    /// Noise-suppression level. See [`NoiseSuppression`].
    pub noise_suppression: NoiseSuppression,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            input_device_name: None,
            output_device_name: None,
            input_gain: 1.0,
            noise_suppression: NoiseSuppression::Medium,
        }
    }
}

/// Requested noise-suppression level.
///
/// `Off` is a pure pass-through. `Soft`, `Medium`, and `Strong` map to the
/// WebRTC APM noise-suppression levels (Low/Moderate/High) applied by the
/// client's echo-cancel stage ([`aec::EchoStage`]) on the mic uplink. NS runs
/// only while that stage's canceller is live (`AURA_AEC=on`, the default): in
/// `gate`/`off` modes — and after a mid-call APM-error demotion to the gate —
/// no APM exists, so no noise filtering is applied and the levels are inert
/// (the startup notice says so).
///
/// `Medium` is the [`Default`] variant, matching the APM default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NoiseSuppression {
    Off,
    Soft,
    #[default]
    Medium,
    Strong,
}

impl std::fmt::Display for NoiseSuppression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoiseSuppression::Off => write!(f, "off"),
            NoiseSuppression::Soft => write!(f, "soft"),
            NoiseSuppression::Medium => write!(f, "medium"),
            NoiseSuppression::Strong => write!(f, "strong"),
        }
    }
}

// ---------------------------------------------------------------------------
// Small pure helpers (also used by unit tests)
// ---------------------------------------------------------------------------

/// Clamp an input-gain value to the valid `0.0..=2.0` range.
pub fn clamp_gain(gain: f32) -> f32 {
    gain.clamp(0.0, 2.0)
}

/// Return `true` when `device_name` case-insensitively *contains*
/// `requested`.  An empty `requested` string never matches (we treat
/// that the same as `None` — caller should just use the default device).
pub fn matches_device_name(device_name: &str, requested: &str) -> bool {
    if requested.is_empty() {
        return false;
    }
    device_name
        .to_lowercase()
        .contains(&requested.to_lowercase())
}

fn normalized_requested_device_name(requested: Option<&str>) -> Option<&str> {
    let requested = requested?.trim();
    if requested.is_empty() || requested.eq_ignore_ascii_case("system default") {
        None
    } else {
        Some(requested)
    }
}

/// Pick an input device whose name contains `requested` (case-insensitive
/// substring), falling back to the host default when not found.
///
/// Logs an `[aura-audio]` message when the requested name was not found
/// so users debugging "why does my AirPods setting do nothing" can see it.
fn pick_input_device(host: &cpal::Host, requested: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = normalized_requested_device_name(requested) {
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                if let Ok(device_name) = device.name() {
                    if matches_device_name(&device_name, name) {
                        return Some(device);
                    }
                }
            }
        }
        eprintln!(
            "[aura-audio] requested input device '{}' not found; falling back to system default",
            name
        );
    }
    host.default_input_device()
}

/// Pick an output device whose name contains `requested` (case-insensitive
/// substring), falling back to the host default when not found.
///
/// Logs an `[aura-audio]` message when the requested name was not found.
fn pick_output_device(host: &cpal::Host, requested: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = normalized_requested_device_name(requested) {
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                if let Ok(device_name) = device.name() {
                    if matches_device_name(&device_name, name) {
                        return Some(device);
                    }
                }
            }
        }
        eprintln!(
            "[aura-audio] requested output device '{}' not found; falling back to system default",
            name
        );
    }
    host.default_output_device()
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("audio host error: {0}")]
    Host(String),
    #[error("missing default {0} device")]
    MissingDevice(&'static str),
    #[error("unsupported {0} sample format: {1:?}")]
    UnsupportedSampleFormat(&'static str, cpal::SampleFormat),
    #[error("audio stream error: {0}")]
    Stream(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceReport {
    pub input_available: bool,
    pub output_available: bool,
    pub input_name: Option<String>,
    pub output_name: Option<String>,
}

pub fn doctor() -> AudioDeviceReport {
    let host = cpal::default_host();
    let input = host.default_input_device();
    let output = host.default_output_device();

    AudioDeviceReport {
        input_available: input.is_some(),
        output_available: output.is_some(),
        input_name: input.and_then(|device| device.name().ok()),
        output_name: output.and_then(|device| device.name().ok()),
    }
}

/// The AEC far-end reference tap: a lock-free mirror of what the speaker is
/// ACTUALLY playing, written by the cpal output callback at the moment each
/// sample is popped for playout.
///
/// The tap point matters: the model streams a whole answer in a few seconds
/// while the playback queue holds up to 30 s, so a reference taken at
/// `push_pcm_24k` time would lead the acoustic echo by tens of seconds — far
/// beyond the echo canceller's delay-estimation window. Popped samples (data
/// or silence) lag the sound from the speaker only by the device's output
/// latency, which AEC3's delay estimator absorbs.
///
/// Disarmed by default (zero cost when no [`aec::EchoStage`] consumes it);
/// the AEC stage arms it at construction.
#[derive(Debug)]
pub struct FarEndTap {
    queue: ArrayQueue<i16>,
    /// The output-device sample rate the tapped samples are at.
    sample_rate: AtomicU32,
    /// Only an armed tap records; keeps the output callback overhead at one
    /// atomic load when AEC is off.
    armed: AtomicBool,
}

/// Tap capacity: ~2 s at 96 kHz (or ~4 s at 48 kHz). The AEC stage drains it
/// every mic chunk (~20 ms); the headroom covers a stalled mic stream during
/// a device rebuild without unbounded memory.
const FAREND_TAP_CAPACITY: usize = 192_000;

impl FarEndTap {
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            queue: ArrayQueue::new(FAREND_TAP_CAPACITY),
            sample_rate: AtomicU32::new(sample_rate),
            armed: AtomicBool::new(false),
        }
    }

    /// Start recording playout samples (called once by the AEC stage).
    pub fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    /// The sample rate the tapped samples are at (the output-device rate).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Acquire)
    }

    /// Record one played-out sample. Called from the realtime output callback:
    /// lock-free, never blocks; on overflow the oldest sample is dropped (the
    /// AEC stage resynchronizes via its delay estimator).
    pub(crate) fn push(&self, sample: i16) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        if self.queue.push(sample).is_err() {
            let _ = self.queue.pop();
            let _ = self.queue.push(sample);
        }
    }

    /// Drain up to `out`'s spare capacity of tapped samples (oldest first).
    /// Returns how many were appended. Consumer side (AEC stage) only.
    pub fn drain_into(&self, out: &mut Vec<i16>, max: usize) -> usize {
        let mut n = 0;
        while n < max {
            match self.queue.pop() {
                Some(s) => {
                    out.push(s);
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Forget everything tapped so far (rate change / rebuild).
    fn clear(&self) {
        while self.queue.pop().is_some() {}
    }

    pub(crate) fn reconfigure(&self, sample_rate: u32) {
        self.sample_rate.store(sample_rate, Ordering::Release);
        self.clear();
    }
}

/// Lock-free playback queue shared between the producer (tokio task that
/// receives Grok audio deltas and drives barge-in) and the cpal output
/// callback that runs on a real-time audio thread.
///
/// Both push and clear are called from the same producer task; the cpal
/// callback only pops. Using `crossbeam_queue::ArrayQueue` keeps the audio
/// thread off any mutex so playback can't be stalled by the producer.
#[derive(Debug, Clone)]
pub struct PlaybackHandle {
    queue: Arc<ArrayQueue<i16>>,
    output_sample_rate: Arc<AtomicU32>,
    max_live_frames: Arc<AtomicUsize>,
    /// AEC far-end reference: mirrors every sample the output callback pops.
    farend: Arc<FarEndTap>,
}

impl PlaybackHandle {
    fn new(capacity: usize, output_sample_rate: u32) -> Self {
        let max_live_frames = live_playback_max_frames(output_sample_rate);
        Self {
            queue: Arc::new(ArrayQueue::new(capacity.max(1))),
            output_sample_rate: Arc::new(AtomicU32::new(output_sample_rate)),
            max_live_frames: Arc::new(AtomicUsize::new(max_live_frames)),
            farend: Arc::new(FarEndTap::new(output_sample_rate)),
        }
    }

    /// The AEC far-end tap fed by this handle's playout pops.
    pub fn farend(&self) -> Arc<FarEndTap> {
        Arc::clone(&self.farend)
    }

    pub fn push_pcm_24k(&self, samples: &[i16]) {
        let output_sample_rate = self.output_sample_rate();
        let mut converted = Vec::with_capacity(samples.len());
        resample_nearest(samples, 24_000, output_sample_rate, &mut converted);
        let max_live_frames = self.max_live_frames.load(Ordering::Acquire);
        for frame in converted {
            while self.queue.len() >= max_live_frames {
                let _ = self.queue.pop();
            }
            // ArrayQueue rejects on full; preserve the prior "drop oldest"
            // behaviour so live audio stays responsive instead of stalling.
            if self.queue.push(frame).is_err() {
                let _ = self.queue.pop();
                let _ = self.queue.push(frame);
            }
        }
    }

    pub fn clear_for_barge_in(&self) -> usize {
        let mut dropped = 0;
        while self.queue.pop().is_some() {
            dropped += 1;
        }
        dropped
    }

    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }

    pub fn queued_ms(&self) -> u64 {
        (self.queue.len() as u64 * 1000) / u64::from(self.output_sample_rate().max(1))
    }

    pub fn output_sample_rate(&self) -> u32 {
        self.output_sample_rate.load(Ordering::Acquire)
    }

    fn reconfigure_output_sample_rate(&self, output_sample_rate: u32) -> usize {
        self.output_sample_rate
            .store(output_sample_rate, Ordering::Release);
        self.max_live_frames.store(
            live_playback_max_frames(output_sample_rate),
            Ordering::Release,
        );
        // Stale far-end samples were tapped at the OLD rate — drop them so the
        // AEC drain never mixes rates within one converter run.
        self.farend.reconfigure(output_sample_rate);
        self.clear_for_barge_in()
    }

    fn pop_or_silence(&self) -> i16 {
        let sample = self.queue.pop().unwrap_or(0);
        // Mirror what is ACTUALLY played (data or silence) into the AEC
        // far-end tap — see `FarEndTap` for why the tap lives at pop time.
        self.farend.push(sample);
        sample
    }
}

fn live_playback_max_frames(output_sample_rate: u32) -> usize {
    // Keep enough buffered speech that long answers do not skip words.
    // Barge-in clears this queue immediately, so responsiveness comes
    // from clear_for_barge_in rather than dropping older audio mid-turn.
    const MAX_LIVE_PLAYBACK_MS: usize = 30_000;
    (output_sample_rate.max(1) as usize * MAX_LIVE_PLAYBACK_MS / 1_000).max(1)
}

/// Phase B handle: owns the cpal streams on a dedicated worker thread and
/// rebuilds them when the macOS system-default device changes mid-session.
/// `cpal::Stream` is `!Send` on every platform (the macOS coreaudio host
/// uses a `RefCell`-shaped `Arc<Mutex<…>>` that the cpal platform layer
/// deliberately marks `!Send + !Sync` to prevent driving an audio unit
/// from the wrong thread). The streams therefore live for their entire
/// lifetime on the worker thread that built them; the only thing the
/// outer `LiveAudioSession` keeps is a shutdown signal + the join handle.
struct DeviceFollower {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DeviceFollower {
    fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // The worker checks the shutdown flag at every poll-tick
            // boundary (default ~5 s) and on every channel timeout. Give
            // it that long to see the flag plus a cushion for cpal's own
            // Stream::Drop joins; if the join takes longer we don't
            // block forever — the OS will reap the thread on process
            // exit, and live_call uses std::process::exit(0) for
            // end_voice_session anyway.
            let _ = handle.join();
        }
    }
}

impl Drop for DeviceFollower {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Best-effort join — the live_call shutdown path calls
        // `.shutdown()` explicitly which already joined. This Drop only
        // runs on panics / early returns.
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub struct LiveAudioSession {
    /// Worker thread that owns the cpal input + output streams. Held in
    /// an `Option` so `Drop` can take it and signal shutdown without
    /// double-borrowing the field. The follower joins on drop, which
    /// stops the audio threads cleanly when the session goes out of
    /// scope.
    follower: Option<DeviceFollower>,
    /// `Option` because the Phase A API exposed these as plain public
    /// fields for `let mut input_audio = audio.input_audio;`-style
    /// destructuring. With Drop now in play, partial moves are
    /// disallowed (E0509). Wrapping in Option lets callers use
    /// `.take()` to claim ownership without the compiler thinking
    /// they're tearing the session apart.
    input_audio: Option<mpsc::Receiver<Vec<i16>>>,
    pub playback: PlaybackHandle,
}

impl LiveAudioSession {
    /// Take exclusive ownership of the input-audio receiver. Should be
    /// called exactly once per session; subsequent calls return None
    /// because the receiver moved out.
    pub fn take_input_audio(&mut self) -> Option<mpsc::Receiver<Vec<i16>>> {
        self.input_audio.take()
    }

    /// Reference accessor for the input-audio receiver, for callers
    /// that want to peek without taking ownership (e.g. test diagnostics
    /// that need to confirm the channel is wired up but plan to keep
    /// `&mut session` for further calls).
    pub fn input_audio_ref(&self) -> Option<&mpsc::Receiver<Vec<i16>>> {
        self.input_audio.as_ref()
    }
}

impl Drop for LiveAudioSession {
    fn drop(&mut self) {
        if let Some(follower) = self.follower.take() {
            follower.shutdown();
        }
    }
}

/// Polling cadence for "did the system default change?". 5 s is the
/// sweet spot per the Phase B design notes: short enough that the user
/// barely notices the lag (Bluetooth handshake itself is usually 2-4 s
/// on macOS), long enough that we don't shell out to system_profiler
/// constantly. Each poll costs ~100 ms wall — the worker sleeps the
/// rest of the interval.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum gap between two consecutive *rebuilds*. Independent of
/// `POLL_INTERVAL`: an AirPods reconnect can fire 3-5 default-device
/// flips in 200 ms (handshake retries), and we don't want to chase
/// every transient. The poll loop notices drift on the first tick after
/// it stabilises, which is at most POLL_INTERVAL late — better than
/// thrashing.
const MIN_REBUILD_INTERVAL: Duration = Duration::from_millis(750);
const AUDIO_FOLLOWER_START_TIMEOUT: Duration = Duration::from_secs(20);

/// Capacity of the tokio `mpsc` channel that carries raw input chunks from
/// the cpal callback thread to the voice-processing consumer task.
///
/// When this channel is full, `enqueue_input_chunk` drops the incoming chunk
/// (we must never block the realtime cpal callback). In practice it rarely
/// fills because the consumer (Grok encoder) runs faster than real-time, but
/// under heavy CPU load or a slow consumer it can. Drops are counted and
/// logged on a throttle (see `INPUT_DROPPED_FRAMES` / `enqueue_input_chunk`)
/// so sustained back-pressure is observable in the logs instead of surfacing
/// only as unexplained audio gaps.
///
/// Capacity is deliberately small: a larger buffer would hide back-pressure by
/// adding latency rather than exposing it — at ~100 ms chunks, 128 frames is
/// already ~12.8 s of worst-case queued mic audio.
const INPUT_AUDIO_CHANNEL_FRAMES: usize = 128;

/// Running count of mic chunks dropped because the input channel was full.
/// Lets `enqueue_input_chunk` surface sustained back-pressure without ever
/// blocking the realtime cpal callback.
static INPUT_DROPPED_FRAMES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Try to enqueue one captured audio chunk. Returns `true` on success,
/// `false` when the channel is full. A full channel means the consumer (Grok
/// encoder) fell behind; the chunk is dropped because we must not block the
/// realtime cpal callback. Drops are counted and logged on a throttle (the
/// first drop, then every 100th) so back-pressure is observable instead of
/// silent.
fn enqueue_input_chunk(tx: &mpsc::Sender<Vec<i16>>, chunk: Vec<i16>) -> bool {
    if tx.try_send(chunk).is_ok() {
        return true;
    }
    let dropped = INPUT_DROPPED_FRAMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if dropped == 1 || dropped.is_multiple_of(100) {
        eprintln!(
            "[aura-audio] input channel full: dropped {dropped} mic chunk(s) so far (consumer behind real-time)"
        );
    }
    false
}

pub fn start_live_audio(
    target_sample_rate: u32,
    chunk_ms: u32,
    settings: AudioSettings,
) -> Result<LiveAudioSession, AudioError> {
    // Probe the initial devices on the *caller's* thread so we can
    // surface the AudioError synchronously (the LiveAudioSession API
    // contract). Once we've confirmed both sides exist, we build the
    // PlaybackHandle + mpsc channel here too — those are Send and the
    // worker thread will receive clones.
    let host = cpal::default_host();
    let initial_output = pick_output_device(&host, settings.output_device_name.as_deref())
        .ok_or(AudioError::MissingDevice("output"))?;
    let initial_output_name = initial_output.name().unwrap_or_default();
    let initial_input = pick_input_device(&host, settings.input_device_name.as_deref())
        .ok_or(AudioError::MissingDevice("input"))?;
    let initial_input_name = initial_input.name().unwrap_or_default();
    let initial_input_config = initial_input
        .default_input_config()
        .map_err(|err| AudioError::Host(err.to_string()))?;
    let initial_output_config = initial_output
        .default_output_config()
        .map_err(|err| AudioError::Host(err.to_string()))?;
    let initial_problematic_bluetooth =
        looks_like_problematic_bluetooth_pair(&initial_input_name, &initial_output_name);
    // When the mic+speaker are the same Bluetooth headset, prefer an output
    // config at the target voice rate so the SCO/HFP duplex path matches the
    // headset's voice channel instead of forcing CoreAudio to resample (see
    // `choose_output_config` / `emit_bluetooth_pair_warning`).
    let initial_output_config = choose_output_config(
        &initial_output,
        initial_output_config,
        target_sample_rate,
        initial_problematic_bluetooth,
    );
    let output_sample_rate = initial_output_config.sample_rate().0;

    log_device_diagnostics(
        &initial_input,
        &initial_output,
        &initial_input_config,
        &initial_output_config,
    );
    warn_on_default_device_drift(&initial_input_name, &initial_output_name, &settings);
    if initial_problematic_bluetooth {
        emit_bluetooth_pair_warning();
    }
    // Queue capacity = 30 seconds of audio at the output rate. Earlier
    // version capped at 5s, which was too tight for real Grok responses
    // — the realtime API streams a full turn's worth of decoded PCM in
    // 3-5s of wall time (much faster than playback), and the 5s cap
    // dropped the oldest ~7s of every >12s response. To the user that
    // sounded like "she sped up and I couldn't understand her" — the
    // start of the sentence was thrown away and only the tail played.
    // 30 seconds at 48 kHz = 2.88M i16 samples = ~5.8 MB. Trivial cost.
    const PLAYBACK_BUFFER_SECONDS: usize = 30;
    let playback = PlaybackHandle::new(
        output_sample_rate as usize * PLAYBACK_BUFFER_SECONDS,
        output_sample_rate,
    );
    let (tx, rx) = mpsc::channel(INPUT_AUDIO_CHANNEL_FRAMES);

    // The synchronous-probe device handles + configs are no-op-Drop
    // values; the worker re-resolves them inside its own scope so
    // cpal builds the audio units on the right thread (CoreAudio ties
    // the run-loop dispatch to the building thread). Let the locals
    // fall out of scope naturally; explicit `drop` would be a clippy
    // `drop_non_drop` hit.
    let _ = (
        initial_input,
        initial_output,
        initial_input_config,
        initial_output_config,
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), AudioError>>();
    let worker_shutdown = Arc::clone(&shutdown);
    let worker_tx = tx.clone();
    let worker_playback = playback.clone();
    let initial_input_name_for_worker = initial_input_name.clone();
    let initial_output_name_for_worker = initial_output_name.clone();
    let worker_settings = settings.clone();
    let handle = std::thread::Builder::new()
        .name("aura-audio-follower".to_owned())
        .spawn(move || {
            run_follower(
                target_sample_rate,
                chunk_ms,
                worker_tx,
                worker_playback,
                worker_shutdown,
                ready_tx,
                initial_input_name_for_worker,
                initial_output_name_for_worker,
                worker_settings,
            );
        })
        .map_err(|err| AudioError::Stream(format!("failed to spawn audio follower: {err}")))?;

    // Drop the original sender — the worker's clone is the only one
    // pushing to the receiver from now on. Without this drop, a
    // future where the worker exits would never close the receiver.
    drop(tx);

    // Wait for the worker to confirm initial streams built (or fail).
    // Worst case: failure to build a Stream (~50-200 ms on macOS),
    // success: same. No real risk of long blocks here — if cpal hangs
    // we'd be just as stuck without Phase B.
    match ready_rx.recv_timeout(AUDIO_FOLLOWER_START_TIMEOUT) {
        Ok(Ok(())) => Ok(LiveAudioSession {
            follower: Some(DeviceFollower {
                shutdown,
                handle: Some(handle),
            }),
            input_audio: Some(rx),
            playback,
        }),
        Ok(Err(err)) => {
            // Worker failed to build streams; tell it to exit and join.
            shutdown.store(true, Ordering::Release);
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            shutdown.store(true, Ordering::Release);
            let _ = handle.join();
            Err(AudioError::Stream(
                "audio follower failed to start within 20s".to_owned(),
            ))
        }
    }
}

/// Worker-thread main. Owns the cpal `Host` + the active streams, polls
/// `system_profiler` for device drift, rebuilds streams when drift is
/// detected. All `cpal::Stream` values stay on this thread for their
/// entire lifetime.
#[allow(clippy::too_many_arguments)]
fn run_follower(
    target_sample_rate: u32,
    chunk_ms: u32,
    tx: mpsc::Sender<Vec<i16>>,
    playback: PlaybackHandle,
    shutdown: Arc<AtomicBool>,
    ready_tx: std_mpsc::Sender<Result<(), AudioError>>,
    initial_input_name: String,
    initial_output_name: String,
    settings: AudioSettings,
) {
    let host = cpal::default_host();
    // Hold the synchronous-probe names only until the worker's own
    // build_streams_for_current_default re-resolves them. We don't
    // trust the synchronous values for drift comparison — the OS could
    // have changed defaults during the spawn handoff, and cpal will
    // tell us the truth from this thread's perspective.
    let _ = (initial_input_name, initial_output_name);
    let initial_streams = match build_streams_for_current_default(
        &host,
        target_sample_rate,
        chunk_ms,
        tx.clone(),
        playback.clone(),
        &settings,
    ) {
        Ok(streams) => streams,
        Err(err) => {
            let _ = ready_tx.send(Err(err));
            return;
        }
    };
    // The "active" names track whatever cpal actually opened. drift
    // comparison is name-only (cpal::Device doesn't impl PartialEq and
    // CoreAudio reuses AudioDeviceID after unplug/replug, so name is
    // the only stable identity).
    let mut active_input_name = initial_streams.input_name.clone();
    let mut active_output_name = initial_streams.output_name.clone();
    let mut current_streams = initial_streams;
    let _ = ready_tx.send(Ok(()));
    let mut last_rebuild = Instant::now();
    let follow_input_default =
        normalized_requested_device_name(settings.input_device_name.as_deref()).is_none();
    let follow_output_default =
        normalized_requested_device_name(settings.output_device_name.as_deref()).is_none();
    while !shutdown.load(Ordering::Acquire) {
        std::thread::sleep(POLL_INTERVAL);
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        if !follow_input_default && !follow_output_default {
            continue;
        }
        // Re-query the OS-level default. We deliberately use the same
        // `query_system_default_devices()` helper Phase A built — that
        // function is the source of truth for "what does macOS think
        // the default is", because cpal's view of the default is
        // exactly the thing that gets stuck.
        let (sys_input, sys_output) = query_system_default_devices();
        let drift_input = if follow_input_default {
            sys_input.as_deref()
        } else {
            Some(active_input_name.as_str())
        };
        let drift_output = if follow_output_default {
            sys_output.as_deref()
        } else {
            Some(active_output_name.as_str())
        };
        let drift = detect_drift(
            &active_input_name,
            &active_output_name,
            drift_input,
            drift_output,
        );
        if !should_rebuild_for_drift(
            &active_input_name,
            &active_output_name,
            drift_input,
            drift_output,
            drift,
        ) {
            continue;
        }
        // Coalesce reconnect storms: if we just rebuilt, sleep through
        // the storm before swapping again. The next poll-tick will
        // pick up the new state.
        if last_rebuild.elapsed() < MIN_REBUILD_INTERVAL {
            continue;
        }
        eprintln!(
            "[aura-audio] system default changed (input='{}'→'{}', output='{}'→'{}'); rebuilding cpal streams",
            active_input_name,
            drift_input.unwrap_or("?"),
            active_output_name,
            drift_output.unwrap_or("?"),
        );
        // Drop the old streams BEFORE building the new ones. cpal's
        // Stream::Drop joins the audio thread and stops the underlying
        // AudioUnit, freeing the device for the new stream to grab.
        // The brief gap (~50-150 ms) is the user's "audio dropout"
        // window — well under the SCO/HFP handshake itself, which is
        // what the user actually perceives as the device-switch lag.
        drop(current_streams);
        match build_streams_for_current_default(
            &host,
            target_sample_rate,
            chunk_ms,
            tx.clone(),
            playback.clone(),
            &settings,
        ) {
            Ok(new_streams) => {
                active_input_name = new_streams.input_name.clone();
                active_output_name = new_streams.output_name.clone();
                if looks_like_problematic_bluetooth_pair(&active_input_name, &active_output_name) {
                    // Re-emit the SCO/HFP warning post-swap — the new
                    // pair may be dangerous even when the old one
                    // wasn't (AirPods auto-connect is the canonical
                    // scenario).
                    emit_bluetooth_pair_warning();
                }
                current_streams = new_streams;
                last_rebuild = Instant::now();
                eprintln!(
                    "[aura-audio] active devices now: input='{}', output='{}'",
                    active_input_name, active_output_name
                );
            }
            Err(err) => {
                // Rebuild failed (e.g. new device disappeared between
                // the listener fire and the lookup). Keep the call
                // alive — the user prefers stale audio over a crashed
                // call. Try again on the next poll tick.
                eprintln!("[aura-audio] rebuild failed: {err}; will retry on next poll");
                // Without the old streams (we already dropped them)
                // and without new ones, audio is silent until the next
                // poll succeeds. To avoid that gap we'd need to keep
                // the old streams until rebuild succeeded — but cpal
                // can't share an exclusive AudioUnit and the new
                // device may BE the old device's slot, so the safe
                // protocol is "drop first, build second, retry on
                // failure".
                match build_streams_for_current_default(
                    &host,
                    target_sample_rate,
                    chunk_ms,
                    tx.clone(),
                    playback.clone(),
                    &settings,
                ) {
                    Ok(retry_streams) => {
                        active_input_name = retry_streams.input_name.clone();
                        active_output_name = retry_streams.output_name.clone();
                        current_streams = retry_streams;
                        last_rebuild = Instant::now();
                    }
                    Err(_) => {
                        // Second attempt also failed. Sleep through
                        // the next poll cycle and retry. Audio is
                        // briefly silent — better than panic.
                        eprintln!(
                            "[aura-audio] retry also failed; audio will resume on next successful poll"
                        );
                        // Build a placeholder using whatever cpal
                        // currently considers default so we have *some*
                        // active streams. If even that fails we give
                        // up on this tick.
                        match build_streams_for_current_default(
                            &host,
                            target_sample_rate,
                            chunk_ms,
                            tx.clone(),
                            playback.clone(),
                            &settings,
                        ) {
                            Ok(fallback) => {
                                active_input_name = fallback.input_name.clone();
                                active_output_name = fallback.output_name.clone();
                                current_streams = fallback;
                                last_rebuild = Instant::now();
                            }
                            Err(err) => {
                                eprintln!(
                                    "[aura-audio] all rebuild attempts failed: {err}; follower exiting"
                                );
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
    // Shutdown: drop the streams explicitly so cpal joins the audio
    // thread before our worker thread exits. The Drop impl on
    // current_streams handles this even without the explicit drop, but
    // making it explicit pins the ordering for future readers.
    drop(current_streams);
}

/// The two streams plus the names cpal returned for them at build time.
/// The streams field is private — cpal::Stream is !Send so we never let
/// it leave the worker thread.
struct ActiveStreams {
    _input_stream: cpal::Stream,
    _output_stream: cpal::Stream,
    input_name: String,
    output_name: String,
}

fn build_streams_for_current_default(
    host: &cpal::Host,
    target_sample_rate: u32,
    chunk_ms: u32,
    tx: mpsc::Sender<Vec<i16>>,
    playback: PlaybackHandle,
    settings: &AudioSettings,
) -> Result<ActiveStreams, AudioError> {
    let output_device = pick_output_device(host, settings.output_device_name.as_deref())
        .ok_or(AudioError::MissingDevice("output"))?;
    let output_name = output_device.name().unwrap_or_default();
    let input_device = pick_input_device(host, settings.input_device_name.as_deref())
        .ok_or(AudioError::MissingDevice("input"))?;
    let input_name = input_device.name().unwrap_or_default();
    let input_config = input_device
        .default_input_config()
        .map_err(|err| AudioError::Host(err.to_string()))?;
    let default_output_config = output_device
        .default_output_config()
        .map_err(|err| AudioError::Host(err.to_string()))?;
    let problematic_bluetooth = looks_like_problematic_bluetooth_pair(&input_name, &output_name);
    // Same-headset Bluetooth pair → ask for the target voice rate so the
    // duplex SCO/HFP path matches the headset instead of resampling. This
    // call site also covers the Phase-B rebuild path (a fresh
    // AirPods auto-connect mid-call routes through here).
    let output_config = choose_output_config(
        &output_device,
        default_output_config.clone(),
        target_sample_rate,
        problematic_bluetooth,
    );
    let dropped_on_reconfigure =
        playback.reconfigure_output_sample_rate(output_config.sample_rate().0);
    if dropped_on_reconfigure > 0 {
        eprintln!(
            "[aura-audio] cleared {} queued output samples after output-rate change",
            dropped_on_reconfigure
        );
    }
    if problematic_bluetooth {
        emit_bluetooth_pair_warning();
    }

    // Noise suppression is applied by the client's echo-cancel stage
    // (`aec::EchoStage`, WebRTC APM) on the mic uplink — not by these cpal
    // streams, and only while the canceller itself is live (AEC on). Log once
    // so operators can confirm where the setting acts.
    if settings.noise_suppression != NoiseSuppression::Off {
        static NOISE_SUPPRESSION_NOTICE: std::sync::Once = std::sync::Once::new();
        let level = settings.noise_suppression;
        NOISE_SUPPRESSION_NOTICE.call_once(move || {
            eprintln!(
                "Aura: noise suppression '{}' is applied by the echo-cancel stage on the mic uplink (active only while AURA_AEC=on, the default).",
                level
            );
        });
    }

    let gain = clamp_gain(settings.input_gain);

    let input_stream = build_input_stream(
        &input_device,
        input_config,
        target_sample_rate,
        chunk_ms,
        tx,
        gain,
    )?;
    let output_stream = build_output_stream_with_fallback(
        &output_device,
        output_config,
        default_output_config,
        playback,
    )?;
    input_stream
        .play()
        .map_err(|err| AudioError::Stream(err.to_string()))?;
    output_stream
        .play()
        .map_err(|err| AudioError::Stream(err.to_string()))?;
    Ok(ActiveStreams {
        _input_stream: input_stream,
        _output_stream: output_stream,
        input_name,
        output_name,
    })
}

fn log_device_diagnostics(
    input_device: &cpal::Device,
    output_device: &cpal::Device,
    input_config: &cpal::SupportedStreamConfig,
    output_config: &cpal::SupportedStreamConfig,
) {
    let output_sample_rate = output_config.sample_rate().0;
    let output_channels = output_config.channels();
    let output_format = output_config.sample_format();
    let input_sample_rate = input_config.sample_rate().0;
    let input_channels = input_config.channels();
    let input_format = input_config.sample_format();
    eprintln!(
        "[aura-audio] input  device  : {} (rate={} Hz, channels={}, format={:?})",
        input_device
            .name()
            .unwrap_or_else(|_| "<unknown>".to_owned()),
        input_sample_rate,
        input_channels,
        input_format
    );
    eprintln!(
        "[aura-audio] output device  : {} (rate={} Hz, channels={}, format={:?})",
        output_device
            .name()
            .unwrap_or_else(|_| "<unknown>".to_owned()),
        output_sample_rate,
        output_channels,
        output_format
    );
    eprintln!(
        "[aura-audio] resampler plan : push 24 kHz mono → cpal at {} Hz × {} channel(s); ratio = {:.3}x output samples per input sample",
        output_sample_rate,
        output_channels,
        output_sample_rate as f64 / 24_000.0
    );
}

/// Select the output `SupportedStreamConfig` to use when building the
/// output stream.
///
/// When `prefer_target_rate` is `true` and the device supports
/// `target_sample_rate` with the same format/channel count as the default,
/// the function returns a config at that rate. This supports a Bluetooth
/// "duplex mode" where output runs at 8 / 16 / 24 kHz to match the SCO/HFP
/// headset voice channel — avoiding the OS sample-rate mismatch that causes
/// CoreAudio to silently downgrade the codec.
///
/// `prefer_target_rate` is wired to the `looks_like_problematic_bluetooth_pair`
/// flag at both call sites (initial build in `start_live_audio` and the
/// Phase-B rebuild in `build_streams_for_current_default`), so this branch is
/// live whenever the same Bluetooth headset is serving both mic and speaker —
/// the exact case `emit_bluetooth_pair_warning` warns about. When the flag is
/// `false`, or the device default already runs at the target rate, or no
/// matching supported config exists, the device default is returned unchanged.
fn choose_output_config(
    device: &cpal::Device,
    default_config: cpal::SupportedStreamConfig,
    target_sample_rate: u32,
    prefer_target_rate: bool,
) -> cpal::SupportedStreamConfig {
    if !prefer_target_rate || default_config.sample_rate().0 == target_sample_rate {
        return default_config;
    }
    let Ok(configs) = device.supported_output_configs() else {
        return default_config;
    };
    match pick_rate_matched_config(configs, &default_config, target_sample_rate) {
        Some(config) => {
            eprintln!(
                "[aura-audio] Bluetooth duplex mode: opening output at {} Hz to match headset voice mode.",
                target_sample_rate
            );
            config
        }
        None => default_config,
    }
}

/// Pure selection core of [`choose_output_config`]: from the device's
/// supported output config ranges, pick the first that matches the default
/// config's sample format and channel count and whose range spans
/// `target_sample_rate`, returning that range pinned to the target rate.
/// Returns `None` when no range qualifies (caller falls back to the default).
///
/// Factored out from the device-driven wrapper so the rate-match selection is
/// unit-testable without real audio hardware.
fn pick_rate_matched_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    default_config: &cpal::SupportedStreamConfig,
    target_sample_rate: u32,
) -> Option<cpal::SupportedStreamConfig> {
    let target = cpal::SampleRate(target_sample_rate);
    let sample_format = default_config.sample_format();
    let channels = default_config.channels();
    configs
        .filter(|config| {
            config.sample_format() == sample_format
                && config.channels() == channels
                && config.min_sample_rate() <= target
                && config.max_sample_rate() >= target
        })
        .map(|config| config.with_sample_rate(target))
        .next()
}

fn emit_bluetooth_pair_warning() {
    eprintln!("[aura-audio] NOTICE: Bluetooth/AirPods detected on BOTH mic and speaker.");
    eprintln!(
        "[aura-audio]   Aura is keeping the speaker on the device default route and resampling playback."
    );
    eprintln!(
        "[aura-audio]   If the headset mic blocks CoreAudio, change the Mac input device in Sound settings;"
    );
    eprintln!("[aura-audio]   Aura will follow that system default on the next rebuild.");
}

/// Pure-fn drift detection. Compares the names cpal currently has open
/// against the names `system_profiler` reports as the OS default, with
/// the same case/whitespace tolerance the Phase A drift warning uses.
/// Returned per-direction so the caller can log which side moved.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DriftResult {
    input: bool,
    output: bool,
}

impl DriftResult {
    fn any(&self) -> bool {
        self.input || self.output
    }
}

fn detect_drift(
    cpal_input: &str,
    cpal_output: &str,
    sys_input: Option<&str>,
    sys_output: Option<&str>,
) -> DriftResult {
    DriftResult {
        // If the OS-level lookup failed (system_profiler unreachable or
        // user has no default of that direction), don't report drift —
        // we have nothing to compare against. Same fail-quiet shape as
        // the Phase A warning code.
        input: sys_input
            .map(|sys| !cpal_input.is_empty() && !names_match(cpal_input, sys))
            .unwrap_or(false),
        output: sys_output
            .map(|sys| !cpal_output.is_empty() && !names_match(cpal_output, sys))
            .unwrap_or(false),
    }
}

fn should_rebuild_for_drift(
    _active_input: &str,
    _active_output: &str,
    _sys_input: Option<&str>,
    _sys_output: Option<&str>,
    drift: DriftResult,
) -> bool {
    drift.any()
}

fn build_input_stream(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    target_sample_rate: u32,
    chunk_ms: u32,
    tx: mpsc::Sender<Vec<i16>>,
    gain: f32,
) -> Result<cpal::Stream, AudioError> {
    let sample_format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    // Build a convert-then-gain closure for each sample format.
    // When gain is effectively 1.0 we skip the multiply (fast path).
    // For f32 input the pipeline is: f32 → apply_gain_f32 → i16.
    // For i16/u16 input we first convert to i16, then apply gain via
    // the i16 path (convert to f32, multiply, clamp, back to i16) so
    // the arithmetic stays in f32 and avoids i16 overflow.
    let use_gain = (gain - 1.0).abs() >= f32::EPSILON;
    match sample_format {
        cpal::SampleFormat::F32 => {
            if use_gain {
                build_input_stream_for(
                    device,
                    config,
                    target_sample_rate,
                    chunk_ms,
                    tx,
                    move |s: f32| f32_to_i16((s * gain).clamp(-1.0, 1.0)),
                )
            } else {
                build_input_stream_for(device, config, target_sample_rate, chunk_ms, tx, f32_to_i16)
            }
        }
        cpal::SampleFormat::I16 => {
            if use_gain {
                build_input_stream_for(
                    device,
                    config,
                    target_sample_rate,
                    chunk_ms,
                    tx,
                    move |s: i16| apply_gain_i16(s, gain),
                )
            } else {
                build_input_stream_for(device, config, target_sample_rate, chunk_ms, tx, |sample| {
                    sample
                })
            }
        }
        cpal::SampleFormat::U16 => {
            if use_gain {
                build_input_stream_for(
                    device,
                    config,
                    target_sample_rate,
                    chunk_ms,
                    tx,
                    move |s: u16| apply_gain_i16(u16_to_i16(s), gain),
                )
            } else {
                build_input_stream_for(device, config, target_sample_rate, chunk_ms, tx, u16_to_i16)
            }
        }
        other => Err(AudioError::UnsupportedSampleFormat("input", other)),
    }
}

/// Apply linear gain to an i16 sample.  Converts to f32, multiplies,
/// clamps to the valid i16 range, then converts back.  Called only when
/// `gain != 1.0` (the caller guards the fast path).
#[inline]
fn apply_gain_i16(sample: i16, gain: f32) -> i16 {
    let f = sample as f32 / i16::MAX as f32;
    let gained = (f * gain).clamp(-1.0, 1.0);
    (gained * i16::MAX as f32) as i16
}

fn build_input_stream_for<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    target_sample_rate: u32,
    chunk_ms: u32,
    tx: mpsc::Sender<Vec<i16>>,
    convert: F,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample + Send + 'static,
    F: Fn(T) -> i16 + Send + Sync + Copy + 'static,
{
    let input_sample_rate = config.sample_rate.0;
    let channels = usize::from(config.channels.max(1));
    let chunk_frames = ((target_sample_rate as u64 * chunk_ms as u64) / 1000).max(1) as usize;
    let mut converter = RateConverter::new(input_sample_rate, target_sample_rate);
    let mut pending = Vec::with_capacity(chunk_frames);
    device
        .build_input_stream(
            &config,
            move |data: &[T], _| {
                for frame in data.chunks(channels) {
                    let sum = frame
                        .iter()
                        .copied()
                        .map(convert)
                        .map(i32::from)
                        .sum::<i32>();
                    let mono = (sum / frame.len().max(1) as i32)
                        .clamp(i16::MIN as i32, i16::MAX as i32)
                        as i16;
                    converter.push(mono, &mut pending);
                    if pending.len() >= chunk_frames {
                        let mut ready = Vec::with_capacity(chunk_frames);
                        std::mem::swap(&mut ready, &mut pending);
                        let _ = enqueue_input_chunk(&tx, ready);
                    }
                }
            },
            |_err| {},
            None,
        )
        .map_err(|err| AudioError::Stream(err.to_string()))
}

fn build_output_stream(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    playback: PlaybackHandle,
) -> Result<cpal::Stream, AudioError> {
    let sample_format = config.sample_format();
    let config: cpal::StreamConfig = config.into();
    match sample_format {
        cpal::SampleFormat::F32 => build_output_stream_for(device, config, playback, i16_to_f32),
        cpal::SampleFormat::I16 => {
            build_output_stream_for(device, config, playback, |sample| sample)
        }
        cpal::SampleFormat::U16 => build_output_stream_for(device, config, playback, i16_to_u16),
        other => Err(AudioError::UnsupportedSampleFormat("output", other)),
    }
}

fn build_output_stream_with_fallback(
    device: &cpal::Device,
    preferred_config: cpal::SupportedStreamConfig,
    default_config: cpal::SupportedStreamConfig,
    playback: PlaybackHandle,
) -> Result<cpal::Stream, AudioError> {
    let preferred_rate = preferred_config.sample_rate().0;
    let default_rate = default_config.sample_rate().0;
    match build_output_stream(device, preferred_config, playback.clone()) {
        Ok(stream) => Ok(stream),
        Err(err) if preferred_rate != default_rate => {
            eprintln!(
                "[aura-audio] Bluetooth duplex mode: output open at {} Hz failed ({err}); falling back to default output at {} Hz.",
                preferred_rate,
                default_rate
            );
            let dropped = playback.reconfigure_output_sample_rate(default_rate);
            if dropped > 0 {
                eprintln!(
                    "[aura-audio] cleared {} queued output samples after output-rate fallback",
                    dropped
                );
            }
            build_output_stream(device, default_config, playback).map_err(|fallback_err| {
                AudioError::Stream(format!(
                    "{err}; fallback default output at {default_rate} Hz also failed: {fallback_err}"
                ))
            })
        }
        Err(err) => Err(err),
    }
}

fn build_output_stream_for<T, F>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    playback: PlaybackHandle,
    convert: F,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample + Send + 'static,
    F: Fn(i16) -> T + Send + Sync + Copy + 'static,
{
    let channels = usize::from(config.channels.max(1));
    device
        .build_output_stream(
            &config,
            move |data: &mut [T], _| {
                for frame in data.chunks_mut(channels) {
                    let sample = playback.pop_or_silence();
                    for channel in frame.iter_mut() {
                        *channel = convert(sample);
                    }
                }
            },
            |_err| {},
            None,
        )
        .map_err(|err| AudioError::Stream(err.to_string()))
}

#[derive(Debug, Clone)]
struct RateConverter {
    input_rate: u32,
    output_rate: u32,
    accumulator: u32,
}

impl RateConverter {
    fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate: input_rate.max(1),
            output_rate: output_rate.max(1),
            accumulator: 0,
        }
    }

    fn push(&mut self, sample: i16, out: &mut Vec<i16>) {
        self.accumulator = self.accumulator.saturating_add(self.output_rate);
        while self.accumulator >= self.input_rate {
            out.push(sample);
            self.accumulator -= self.input_rate;
        }
    }
}

fn resample_nearest(samples: &[i16], input_rate: u32, output_rate: u32, out: &mut Vec<i16>) {
    let mut converter = RateConverter::new(input_rate, output_rate);
    for sample in samples {
        converter.push(*sample, out);
    }
}

/// Convert a normalized f32 sample in `[-1.0, 1.0]` to a signed i16
/// sample.
///
/// Both positive and negative halves scale by `32_768.0` (matching the
/// divisor used by `i16_to_f32` and standard PCM pipeline convention).
/// The result is clamped to `[i16::MIN, i16::MAX]` after conversion:
///
/// - `-1.0` → `(-32_768.0) as i32` = `-32_768` → `i16::MIN` (exact)
/// - `+1.0` → `+32_768.0 as i32` = `+32_768` → clamped to `i16::MAX`
///   (32_767): `+32_768` is out of range for i16, clamp prevents overflow
/// - `0.0`  → `0`
///
/// The clamp is the only correct way to handle `+1.0` without overflow;
/// one LSB of headroom is the standard convention in PCM codecs.
fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    let scaled = (clamped * 32_768.0) as i32;
    scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

fn u16_to_i16(sample: u16) -> i16 {
    (sample as i32 - 32_768).clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Convert a signed i16 sample to a normalized f32 in approximately
/// `[-1.0, 1.0]`.
///
/// Divides by `32_768.0` (not `i16::MAX = 32_767.0`), so:
///   - `i16::MIN` (-32_768) maps to exactly `-1.0`
///   - `i16::MAX` (+32_767) maps to `≈ 0.9999695` (never quite reaches
///     `+1.0`)
///
/// This matches the convention used by most PCM pipelines and is the
/// exact inverse of `f32_to_i16`'s scale factor — both use `32_768.0`.
fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 / 32_768.0
}

fn i16_to_u16(sample: i16) -> u16 {
    (sample as i32 + 32_768).clamp(0, u16::MAX as i32) as u16
}

/// Detect the dangerous "Bluetooth headset on both ends" CoreAudio
/// configuration that triggers macOS's SCO/HFP fallback. Conservative on
/// purpose — only fires when the SAME device name appears on both input
/// and output AND the name matches a known Bluetooth headset family. A
/// matching headset on only one side (e.g. AirPods mic + MacBook
/// speakers) is the recommended setup, so we deliberately let it pass.
fn looks_like_problematic_bluetooth_pair(input_name: &str, output_name: &str) -> bool {
    let input = input_name.trim().to_lowercase();
    let output = output_name.trim().to_lowercase();
    if input.is_empty() || output.is_empty() {
        return false;
    }
    if input != output {
        return false;
    }
    name_looks_like_bluetooth_headset(&input)
}

fn name_looks_like_bluetooth_headset(lower: &str) -> bool {
    // Hand-rolled keyword sniff to avoid pulling in `regex`. Mirrors the
    // documented heuristic: airpods | bluetooth | bose | sony.*wh |
    // jabra | beats. The `sony.*wh` shape is preserved by requiring both
    // tokens — covers WH-1000XM*, WH-CH*, etc. without flagging plain
    // "Sony TV".
    if lower.contains("airpods")
        || lower.contains("bluetooth")
        || lower.contains("bose")
        || lower.contains("jabra")
        || lower.contains("beats")
    {
        return true;
    }
    if lower.contains("sony") && lower.contains("wh") {
        return true;
    }
    false
}

/// Cached `system_profiler SPAudioDataType` lookup of the current macOS
/// default input + output device names. We probe the OS exactly once per
/// process for the *startup* warning path — Phase B's polling loop calls
/// `query_system_default_devices()` directly without the cache so it
/// sees fresh state every tick.
///
/// Returns `(input_name, output_name)`. Either side is `None` on:
///   - non-macOS targets (the binary doesn't exist)
///   - parse failures (output format changed in a future macOS)
///   - the user not having a default of that direction set at all
fn cached_system_default_devices() -> &'static (Option<String>, Option<String>) {
    static CACHED: OnceLock<(Option<String>, Option<String>)> = OnceLock::new();
    CACHED.get_or_init(query_system_default_devices)
}

#[cfg(target_os = "macos")]
fn query_system_default_devices() -> (Option<String>, Option<String>) {
    use std::process::Command;
    // `-detailLevel mini` keeps the output small (no SPI / hardware IDs)
    // while still emitting the "Default Input Device: Yes" / "Default
    // Output Device: Yes" markers we depend on.
    let out = match Command::new("system_profiler")
        .arg("SPAudioDataType")
        .arg("-detailLevel")
        .arg("mini")
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        // Either system_profiler is missing (non-Apple unix variants
        // running our Darwin code path under emulation) or it errored —
        // either way the warning becomes a no-op and cpal's choice is
        // accepted. Belt-and-braces over the OS.
        _ => return (None, None),
    };
    let text = String::from_utf8_lossy(&out);
    parse_system_profiler_audio(&text)
}

#[cfg(not(target_os = "macos"))]
fn query_system_default_devices() -> (Option<String>, Option<String>) {
    // No macOS, no system_profiler. Treat as "unknown" so the warning
    // path becomes a no-op rather than a false-positive on Linux/CI.
    (None, None)
}

/// Parse `system_profiler SPAudioDataType` output, looking for the
/// `Default Input Device: Yes` and `Default Output Device: Yes` markers
/// and returning the most recent device-section header name above each.
///
/// Format we rely on (stable since at least macOS 10.10):
/// ```text
///         Studio Display Speakers:
///           Manufacturer: Apple Inc.
///           Default Output Device: Yes
///           ...
/// ```
/// The header line always ends with `:` and is indented less than the
/// property lines beneath it. We track the most recent header by indent
/// level and capture it when the magic flag appears.
// macOS-only: parses `system_profiler SPAudioDataType`. Unused on other OSes.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_system_profiler_audio(text: &str) -> (Option<String>, Option<String>) {
    let mut input_default: Option<String> = None;
    let mut output_default: Option<String> = None;
    // Track the most recent header at each indent depth. macOS nests
    // device sections under "Devices:" which itself nests under "Audio:".
    // We only care about the deepest header above the flag line, which
    // by the format is always the device name itself.
    let mut current_header: Option<String> = None;
    let mut current_header_indent: usize = usize::MAX;
    for raw in text.lines() {
        let trimmed = raw.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        let indent = trimmed.len() - trimmed.trim_start().len();
        let body = trimmed.trim_start();
        // Property lines look like "Key: Value" — we treat anything
        // containing ": " (with non-empty value) as a property, NOT a
        // header. Pure section headers end with a bare ':' and have no
        // value after it.
        let is_header = body.ends_with(':') && !body.contains(": ");
        if is_header {
            let name = body.trim_end_matches(':').trim().to_string();
            // Skip the structural wrappers ("Audio:", "Devices:") — they
            // have no "Default Input/Output Device" flag under them, so
            // they'd never be captured anyway, but tracking them as
            // current_header would mis-attribute a flag that appears
            // before any device header (which never happens, but be
            // defensive).
            if !name.eq_ignore_ascii_case("audio") && !name.eq_ignore_ascii_case("devices") {
                current_header = Some(name);
                current_header_indent = indent;
            }
            continue;
        }
        // Property line. If we've descended back to the header's depth
        // or shallower without seeing a new header, the previous header
        // is no longer in scope. (This guards against the unlikely case
        // of a flag line appearing between sibling devices.)
        if indent <= current_header_indent {
            current_header = None;
            current_header_indent = usize::MAX;
        }
        if let Some(rest) = body.strip_prefix("Default Input Device:") {
            if rest.trim().eq_ignore_ascii_case("Yes") {
                if let Some(ref name) = current_header {
                    input_default = Some(name.clone());
                }
            }
        } else if let Some(rest) = body.strip_prefix("Default Output Device:") {
            if rest.trim().eq_ignore_ascii_case("Yes") {
                if let Some(ref name) = current_header {
                    output_default = Some(name.clone());
                }
            }
        }
    }
    (input_default, output_default)
}

/// Compare cpal's chosen input/output names against the macOS system
/// defaults reported by `system_profiler`. On disagreement, emit the
/// startup-only Phase A drift warning.
///
/// Phase B's polling loop replaces the *fix* for this drift (the swap)
/// but the warning still fires once at startup so users with an older
/// state of the system see the message before the first poll-tick
/// rebuilds. After the rebuild, names match and no further warnings.
fn warn_on_default_device_drift(cpal_input: &str, cpal_output: &str, settings: &AudioSettings) {
    let follow_input_default =
        normalized_requested_device_name(settings.input_device_name.as_deref()).is_none();
    let follow_output_default =
        normalized_requested_device_name(settings.output_device_name.as_deref()).is_none();
    let (sys_input, sys_output) = cached_system_default_devices();
    if follow_input_default {
        if let Some(sys) = sys_input.as_deref() {
            if !cpal_input.is_empty() && !names_match(cpal_input, sys) {
                print_drift_warning("input", cpal_input, sys);
            }
        }
    }
    if follow_output_default {
        if let Some(sys) = sys_output.as_deref() {
            if !cpal_output.is_empty() && !names_match(cpal_output, sys) {
                print_drift_warning("output", cpal_output, sys);
            }
        }
    }
}

/// Case-insensitive trimmed equality. CoreAudio device names round-trip
/// through user locale (curly-quote apostrophes, accented characters)
/// and may pick up trailing whitespace from the system_profiler
/// columnar output, so a strict `==` would false-positive a
/// disagreement on the AirPods example name "Georgiy's AirPods".
fn names_match(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn print_drift_warning(direction: &str, cpal_name: &str, system_name: &str) {
    eprintln!(
        "[aura-audio] WARNING: cpal selected {} device '{}' but system default is '{}'.",
        direction, cpal_name, system_name
    );
    eprintln!(
        "[aura-audio]   The follower thread will rebuild the cpal stream on the next poll tick (~5s)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_for_barge_in_drops_buffered_audio() {
        let handle = PlaybackHandle::new(4, 24_000);
        handle.push_pcm_24k(&[1, 2]);
        assert_eq!(handle.queued_frames(), 2);
        assert_eq!(handle.clear_for_barge_in(), 2);
        assert_eq!(handle.queued_frames(), 0);
    }

    #[test]
    fn playback_reconfigure_updates_rate_and_clears_stale_audio() {
        let handle = PlaybackHandle::new(8, 48_000);
        handle.push_pcm_24k(&[1, 2]);
        assert_eq!(handle.output_sample_rate(), 48_000);
        assert_eq!(handle.queued_frames(), 4);

        let dropped = handle.reconfigure_output_sample_rate(24_000);

        assert_eq!(dropped, 4);
        assert_eq!(handle.output_sample_rate(), 24_000);
        assert_eq!(handle.queued_frames(), 0);
        handle.push_pcm_24k(&[7, 8]);
        assert_eq!(handle.queued_frames(), 2);
    }

    #[test]
    fn playback_queue_drops_oldest_when_full() {
        let handle = PlaybackHandle::new(2, 24_000);
        handle.push_pcm_24k(&[1, 2, 3]);
        // Capacity 2: oldest sample falls off; newest stays.
        assert_eq!(handle.queued_frames(), 2);
        assert_eq!(handle.pop_or_silence(), 2);
        assert_eq!(handle.pop_or_silence(), 3);
        assert_eq!(handle.pop_or_silence(), 0);
    }

    #[test]
    fn playback_queue_caps_live_latency() {
        let handle = PlaybackHandle::new(480_000, 24_000);
        let samples = vec![7; 360_000];
        handle.push_pcm_24k(&samples);

        assert_eq!(handle.queued_ms(), 15_000);
    }

    #[test]
    fn input_capture_queue_drops_when_full() {
        let (tx, mut rx) = mpsc::channel(1);

        assert!(enqueue_input_chunk(&tx, vec![1]));
        assert!(!enqueue_input_chunk(&tx, vec![2]));
        assert_eq!(rx.try_recv().unwrap(), vec![1]);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resampler_downsamples_48k_to_24k() {
        let mut out = Vec::new();
        resample_nearest(&[1, 2, 3, 4], 48_000, 24_000, &mut out);
        assert_eq!(out, vec![2, 4]);
    }

    #[test]
    fn resampler_upsamples_24k_to_48k() {
        let mut out = Vec::new();
        resample_nearest(&[7, 8], 24_000, 48_000, &mut out);
        assert_eq!(out, vec![7, 7, 8, 8]);
    }

    #[test]
    fn looks_like_problematic_bluetooth_pair_flags_airpods_pair() {
        // Same AirPods used for both directions — the exact CoreAudio
        // configuration that triggers SCO/HFP fallback on macOS.
        assert!(looks_like_problematic_bluetooth_pair(
            "XoAnonXo's AirPods Pro",
            "XoAnonXo's AirPods Pro",
        ));
    }

    #[test]
    fn looks_like_problematic_bluetooth_pair_passes_macbook_built_in() {
        // The healthy default: built-in mic + built-in speakers. Must
        // never warn here — false positives would teach users to ignore
        // the warning.
        assert!(!looks_like_problematic_bluetooth_pair(
            "MacBook Pro Microphone",
            "MacBook Pro Speakers",
        ));
    }

    #[test]
    fn looks_like_problematic_bluetooth_pair_passes_split_devices() {
        // The recommended workaround for the SCO/HFP bug: AirPods as
        // input, MacBook speakers as output. Names differ, so even
        // though the input matches the Bluetooth keyword set the
        // heuristic must let it through.
        assert!(!looks_like_problematic_bluetooth_pair(
            "XoAnonXo's AirPods Pro",
            "MacBook Pro Speakers",
        ));
    }

    #[test]
    fn looks_like_problematic_bluetooth_pair_handles_case_variations() {
        // CoreAudio device names round-trip through user-set strings
        // and OS locale capitalization — the sniff has to be
        // case-insensitive on both keyword AND equality check.
        assert!(looks_like_problematic_bluetooth_pair(
            "BOSE QUIETCOMFORT 45",
            "bose quietcomfort 45",
        ));
        assert!(looks_like_problematic_bluetooth_pair(
            "Sony WH-1000XM5",
            "sony wh-1000xm5",
        ));
        assert!(looks_like_problematic_bluetooth_pair(
            "Jabra Elite 75t",
            "JABRA ELITE 75T",
        ));
    }

    /// Build a `SupportedStreamConfig` with the given fixed sample rate
    /// (the shape `default_output_config()` returns).
    fn fixed_config(
        channels: u16,
        sample_rate: u32,
        sample_format: cpal::SampleFormat,
    ) -> cpal::SupportedStreamConfig {
        cpal::SupportedStreamConfig::new(
            channels,
            cpal::SampleRate(sample_rate),
            cpal::SupportedBufferSize::Unknown,
            sample_format,
        )
    }

    /// Build a `SupportedStreamConfigRange` spanning [min, max] Hz (the
    /// shape `supported_output_configs()` yields).
    fn config_range(
        channels: u16,
        min_rate: u32,
        max_rate: u32,
        sample_format: cpal::SampleFormat,
    ) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            cpal::SampleRate(min_rate),
            cpal::SampleRate(max_rate),
            cpal::SupportedBufferSize::Unknown,
            sample_format,
        )
    }

    #[test]
    fn pick_rate_matched_config_selects_target_rate_when_supported() {
        // The Bluetooth duplex case the #32 fix activates: the device
        // default runs at 48 kHz but also advertises a range covering the
        // 24 kHz voice rate with the same format + channel count. With
        // prefer_target_rate, we must open at 24 kHz to match the headset.
        let default_config = fixed_config(2, 48_000, cpal::SampleFormat::F32);
        let ranges = vec![
            // A non-matching format range that must be skipped.
            config_range(2, 8_000, 96_000, cpal::SampleFormat::I16),
            // The matching range: same format/channels, spans 24 kHz.
            config_range(2, 8_000, 48_000, cpal::SampleFormat::F32),
        ];

        let selected =
            pick_rate_matched_config(ranges.into_iter(), &default_config, 24_000).unwrap();

        assert_eq!(selected.sample_rate().0, 24_000);
        assert_eq!(selected.channels(), 2);
        assert_eq!(selected.sample_format(), cpal::SampleFormat::F32);
    }

    #[test]
    fn pick_rate_matched_config_returns_none_when_rate_unsupported() {
        // No advertised range covers 24 kHz, so selection must yield None
        // and the caller keeps the device default unchanged.
        let default_config = fixed_config(2, 48_000, cpal::SampleFormat::F32);
        let ranges = vec![config_range(2, 44_100, 96_000, cpal::SampleFormat::F32)];

        assert!(pick_rate_matched_config(ranges.into_iter(), &default_config, 24_000).is_none());
    }

    #[test]
    fn pick_rate_matched_config_skips_format_and_channel_mismatches() {
        // A range covers 24 kHz but differs in channel count, and another
        // covers it but differs in sample format. Neither may be selected —
        // we must not hand cpal a config the default stream can't consume.
        let default_config = fixed_config(2, 48_000, cpal::SampleFormat::F32);
        let ranges = vec![
            config_range(1, 8_000, 48_000, cpal::SampleFormat::F32), // wrong channels
            config_range(2, 8_000, 48_000, cpal::SampleFormat::I16), // wrong format
        ];

        assert!(pick_rate_matched_config(ranges.into_iter(), &default_config, 24_000).is_none());
    }

    #[test]
    fn choose_output_config_keeps_default_when_not_preferring_target_rate() {
        // The non-Bluetooth path: prefer_target_rate=false short-circuits to
        // the device default without touching supported_output_configs (so
        // it stays hardware-free and testable). A null device is never
        // queried because the early return fires first.
        let default_config = fixed_config(2, 48_000, cpal::SampleFormat::F32);
        let host = cpal::default_host();
        if let Some(device) = host.default_output_device() {
            let chosen = choose_output_config(&device, default_config.clone(), 24_000, false);
            assert_eq!(chosen.sample_rate().0, 48_000);
            assert_eq!(chosen, default_config);
        }
        // No output device on this host (e.g. headless CI): the pure-core
        // tests above already cover the selection logic; nothing to assert.
    }

    /// Realistic `system_profiler SPAudioDataType` capture taken from
    /// the dispatching macOS host. AirPods are the active default in
    /// both directions, with Studio Display + MacBook Pro built-ins
    /// listed as alternates. This is the canonical "lid-closed +
    /// AirPods connected" shape the parser has to handle, including
    /// the same-named header appearing twice (once with the input
    /// flag, once with the output flag).
    const SAMPLE_SP_OUTPUT: &str = "Audio:

    Devices:

        iPhone (110) Microphone:

          Input Channels: 1
          Manufacturer: Apple Inc.
          Current SampleRate: 48000
          Transport: Unknown
          Input Source: Default

        Studio Display Speakers:

          Manufacturer: Apple Inc.
          Output Channels: 8
          Current SampleRate: 48000
          Transport: USB
          Output Source: Default

        Studio Display Microphone:

          Input Channels: 1
          Manufacturer: Apple Inc.
          Current SampleRate: 48000
          Transport: USB
          Input Source: Default

        MacBook Pro Microphone:

          Input Channels: 1
          Manufacturer: Apple Inc.
          Current SampleRate: 48000
          Transport: Built-in
          Input Source: MacBook Pro Microphone

        MacBook Pro Speakers:

          Manufacturer: Apple Inc.
          Output Channels: 2
          Current SampleRate: 48000
          Transport: Built-in
          Output Source: MacBook Pro Speakers

        Georgiy's AirPods:

          Default Input Device: Yes
          Input Channels: 1
          Manufacturer: Apple Inc.
          Current SampleRate: 24000
          Transport: Bluetooth
          Input Source: Default

        Georgiy's AirPods:

          Default Output Device: Yes
          Default System Output Device: Yes
          Manufacturer: Apple Inc.
          Output Channels: 2
          Current SampleRate: 48000
          Transport: Bluetooth
          Output Source: Default
";

    #[test]
    fn parser_extracts_default_devices_from_realistic_output() {
        // The exact lid-closed-with-AirPods shape: input + output both
        // attribute to the same device-section header above their flag
        // line, even though that header appears twice in the listing.
        let (inp, out) = parse_system_profiler_audio(SAMPLE_SP_OUTPUT);
        assert_eq!(inp.as_deref(), Some("Georgiy's AirPods"));
        assert_eq!(out.as_deref(), Some("Georgiy's AirPods"));
    }

    #[test]
    fn parser_extracts_split_default_devices() {
        // Lid-closed scenario: built-in mic still default, Studio
        // Display Speakers picked up because the lid clamshell mode
        // routes audio out through USB. cpal's stale cache here would
        // still report MacBook Pro Speakers — exactly the case Phase
        // A is built to flag.
        let text = "Audio:

    Devices:

        MacBook Pro Microphone:

          Default Input Device: Yes
          Input Channels: 1

        Studio Display Speakers:

          Default Output Device: Yes
          Default System Output Device: Yes
          Output Channels: 8
";
        let (inp, out) = parse_system_profiler_audio(text);
        assert_eq!(inp.as_deref(), Some("MacBook Pro Microphone"));
        assert_eq!(out.as_deref(), Some("Studio Display Speakers"));
    }

    #[test]
    fn parser_returns_none_when_no_default_flagged() {
        // A user with no audio devices at all (headless Mac mini in
        // CI, for example) gets an essentially empty SPAudioDataType.
        // Parser must return (None, None) rather than panicking or
        // making something up — the warning path then becomes a
        // no-op.
        let text = "Audio:\n\n    Devices:\n";
        let (inp, out) = parse_system_profiler_audio(text);
        assert!(inp.is_none());
        assert!(out.is_none());
    }

    #[test]
    fn parser_ignores_audio_and_devices_wrappers() {
        // Defensive guard: if a future macOS rev moved the `Default
        // Input Device:` flag up to the section-wrapper level (very
        // unlikely but possible), we don't want the wrapper name
        // ("Audio" / "Devices") leaking out as the device name.
        let text = "Audio:

    Devices:

          Default Input Device: Yes
          Default Output Device: Yes
";
        let (inp, out) = parse_system_profiler_audio(text);
        assert!(
            inp.is_none(),
            "wrappers must not be captured as device names"
        );
        assert!(
            out.is_none(),
            "wrappers must not be captured as device names"
        );
    }

    #[test]
    fn names_match_is_case_and_whitespace_insensitive() {
        // CoreAudio device names round-trip through capture pipelines
        // that normalize case and may inject trailing whitespace; the
        // drift check has to tolerate both or it false-positives.
        assert!(names_match(
            "MacBook Pro Microphone",
            "macbook pro microphone"
        ));
        assert!(names_match("  AirPods  ", "AirPods"));
        assert!(!names_match(
            "MacBook Pro Microphone",
            "Studio Display Microphone"
        ));
    }

    #[test]
    fn detect_drift_flags_input_only_when_input_changes() {
        // The lid-clamshell scenario: user routes input to the built-in
        // mic, output stays on Studio Display Speakers. Only the input
        // side drifted — output didn't move.
        let drift = detect_drift(
            "Georgiy's AirPods",             // cpal still has AirPods open as input
            "Studio Display Speakers",       // cpal still has Studio as output (correct)
            Some("MacBook Pro Microphone"),  // OS now reports built-in mic
            Some("Studio Display Speakers"), // OS still reports Studio
        );
        assert!(drift.input);
        assert!(!drift.output);
        assert!(drift.any());
    }

    #[test]
    fn detect_drift_flags_output_only_when_output_changes() {
        // AirPods auto-connect on output, mic stays on built-in. The
        // canonical "AirPods just paired" shape.
        let drift = detect_drift(
            "MacBook Pro Microphone",
            "MacBook Pro Speakers",
            Some("MacBook Pro Microphone"),
            Some("Georgiy's AirPods"),
        );
        assert!(!drift.input);
        assert!(drift.output);
        assert!(drift.any());
    }

    #[test]
    fn detect_drift_flags_both_when_both_change() {
        // Studio Display unplug: both input and output flip to the
        // built-in MacBook devices in the same OS event.
        let drift = detect_drift(
            "Studio Display Microphone",
            "Studio Display Speakers",
            Some("MacBook Pro Microphone"),
            Some("MacBook Pro Speakers"),
        );
        assert!(drift.input);
        assert!(drift.output);
        assert!(drift.any());
    }

    #[test]
    fn detect_drift_is_quiet_when_nothing_changed() {
        // The steady-state case: cpal's open names match the OS's
        // reported defaults. Must return all-false so the worker doesn't
        // tear down a happy stream pair.
        let drift = detect_drift(
            "MacBook Pro Microphone",
            "MacBook Pro Speakers",
            Some("MacBook Pro Microphone"),
            Some("MacBook Pro Speakers"),
        );
        assert!(!drift.input);
        assert!(!drift.output);
        assert!(!drift.any());
    }

    #[test]
    fn detect_drift_tolerates_case_and_whitespace() {
        // The same-with-different-encoding case. system_profiler
        // sometimes pads with trailing spaces; CoreAudio device names
        // round-trip through user locale. The drift check must use the
        // same `names_match` that Phase A's startup warning used or it
        // false-positives every poll tick.
        let drift = detect_drift(
            "MacBook Pro Microphone",
            "MacBook Pro Speakers",
            Some("  macbook pro microphone  "),
            Some("MACBOOK PRO SPEAKERS"),
        );
        assert!(!drift.any());
    }

    #[test]
    fn detect_drift_no_false_positive_when_system_unknown() {
        // system_profiler unreachable / non-macOS host / parser failed.
        // The poll path must NOT report drift — we have nothing to
        // compare against and tearing down healthy streams over a
        // missing data source would be a worse outcome than no follow.
        let drift = detect_drift("MacBook Pro Microphone", "MacBook Pro Speakers", None, None);
        assert!(!drift.any());
    }

    #[test]
    fn detect_drift_no_false_positive_when_cpal_name_empty() {
        // If cpal's `device.name()` failed (rare; usually a transient
        // CoreAudio property-read failure on a newly-connected device),
        // we get an empty cpal name. Don't report drift in that
        // direction — there's nothing meaningful to compare. The
        // worker will pick up the real name on the next successful
        // poll.
        let drift = detect_drift(
            "",
            "MacBook Pro Speakers",
            Some("MacBook Pro Microphone"),
            Some("MacBook Pro Speakers"),
        );
        assert!(!drift.input);
        assert!(!drift.output);
    }

    /// Mock-style swap test that doesn't open real audio devices.
    /// Simulates a swap by:
    ///   1. constructing a PlaybackHandle directly
    ///   2. pushing samples through it
    ///   3. asserting the new "stream" (here just the queue) sees the
    ///      same samples.
    ///
    /// The point is to pin the contract that PlaybackHandle::clone +
    /// the underlying ArrayQueue stays consistent across a (simulated)
    /// stream rebuild — which is the entire reason the swap protocol
    /// is safe.
    #[test]
    fn playback_handle_survives_simulated_stream_swap() {
        let handle = PlaybackHandle::new(64, 24_000);
        // Old "stream" pushes samples.
        handle.push_pcm_24k(&[100, 200, 300]);
        let pre_swap_count = handle.queued_frames();
        assert_eq!(pre_swap_count, 3);
        // Simulate the swap: the worker thread drops the old cpal::Stream
        // and builds a new one. The PlaybackHandle (cloned for the new
        // stream) points at the SAME ArrayQueue — that's the invariant
        // we're testing. We model "the new stream pops" by calling
        // pop_or_silence directly.
        let cloned_for_new_stream = handle.clone();
        // The simulated new output stream consumes the queue.
        assert_eq!(cloned_for_new_stream.pop_or_silence(), 100);
        assert_eq!(cloned_for_new_stream.pop_or_silence(), 200);
        assert_eq!(cloned_for_new_stream.pop_or_silence(), 300);
        // The original handle and the clone share the queue, so the
        // original now reads 0 (silence) too.
        assert_eq!(handle.queued_frames(), 0);
        assert_eq!(handle.pop_or_silence(), 0);
    }

    #[test]
    fn drift_result_any_combines_directions() {
        // Tiny pin so a future contributor can't silently change `any()`
        // to require BOTH directions to flip — the worker uses `any()`
        // as the rebuild gate and a regression to AND-semantics would
        // mean drift on only one side never triggers a rebuild.
        assert!(!DriftResult::default().any());
        assert!(DriftResult {
            input: true,
            output: false
        }
        .any());
        assert!(DriftResult {
            input: false,
            output: true
        }
        .any());
        assert!(DriftResult {
            input: true,
            output: true
        }
        .any());
    }

    #[test]
    fn full_headset_route_ignores_drift_to_room_devices() {
        let drift = detect_drift(
            "Georgiy's AirPods",
            "Georgiy's AirPods",
            Some("Studio Display Microphone"),
            Some("Studio Display Speakers"),
        );

        assert!(should_rebuild_for_drift(
            "Georgiy's AirPods",
            "Georgiy's AirPods",
            Some("Studio Display Microphone"),
            Some("Studio Display Speakers"),
            drift,
        ));
    }

    #[test]
    fn full_headset_route_can_follow_another_full_headset() {
        let drift = detect_drift(
            "Georgiy's AirPods",
            "Georgiy's AirPods",
            Some("Jabra Elite 75t"),
            Some("Jabra Elite 75t"),
        );

        assert!(should_rebuild_for_drift(
            "Georgiy's AirPods",
            "Georgiy's AirPods",
            Some("Jabra Elite 75t"),
            Some("Jabra Elite 75t"),
            drift,
        ));
    }

    #[test]
    fn split_route_follows_system_default_back_to_headset_mic() {
        let drift = detect_drift(
            "MacBook Pro Microphone",
            "Georgiy's AirPods",
            Some("Georgiy's AirPods"),
            Some("Georgiy's AirPods"),
        );

        assert!(should_rebuild_for_drift(
            "MacBook Pro Microphone",
            "Georgiy's AirPods",
            Some("Georgiy's AirPods"),
            Some("Georgiy's AirPods"),
            drift,
        ));
    }

    /// Integration-style smoke test: actually start the live audio
    /// session, verify it returns successfully, then drop it and
    /// confirm the follower thread joins cleanly. Gated on the host
    /// having both an input and output device — CI runners and
    /// headless Mac minis will skip.
    #[test]
    fn live_audio_session_starts_and_shuts_down() {
        // Bail if no devices — the gate keeps CI green.
        let host = cpal::default_host();
        if host.default_input_device().is_none() || host.default_output_device().is_none() {
            eprintln!("[test] no default audio devices; skipping live_audio_session smoke test");
            return;
        }
        let session = match start_live_audio(24_000, 100, AudioSettings::default()) {
            Ok(s) => s,
            Err(err) => {
                // Some test environments (containerized macOS, locked
                // mics) advertise devices but reject build_input_stream.
                // Treat as a skip rather than a failure.
                eprintln!("[test] start_live_audio failed: {err}; skipping smoke test");
                return;
            }
        };
        // Smoke: the playback handle's queue is the right size for the
        // device's sample rate × 30 s buffer. Drop the session and
        // confirm we don't hang on shutdown.
        assert!(session.playback.queued_frames() == 0);
        drop(session);
    }

    // -----------------------------------------------------------------------
    // AudioSettings / AudioSettings::default tests
    // -----------------------------------------------------------------------

    #[test]
    fn audio_settings_default_has_unity_gain_and_medium_suppression() {
        let s = AudioSettings::default();
        assert_eq!(s.input_gain, 1.0, "default gain must be 1.0 (unity)");
        assert_eq!(
            s.noise_suppression,
            NoiseSuppression::Medium,
            "default noise suppression must be Medium"
        );
        assert!(
            s.input_device_name.is_none(),
            "default input device must be None (use cpal default)"
        );
        assert!(
            s.output_device_name.is_none(),
            "default output device must be None (use cpal default)"
        );
    }

    // -----------------------------------------------------------------------
    // Device-name matching tests (pure, no hardware needed)
    // -----------------------------------------------------------------------

    #[test]
    fn device_name_exact_match_case_insensitive() {
        assert!(
            matches_device_name("MacBook Pro Microphone", "macbook pro microphone"),
            "exact match should succeed regardless of case"
        );
        assert!(
            matches_device_name("AirPods Pro", "AIRPODS PRO"),
            "all-caps requested should match"
        );
    }

    #[test]
    fn device_name_substring_match() {
        assert!(
            matches_device_name("Georgiy's AirPods Pro", "airpods"),
            "substring 'airpods' should match within longer name"
        );
        assert!(
            matches_device_name("Studio Display Microphone", "studio"),
            "substring 'studio' should match"
        );
    }

    #[test]
    fn device_name_no_match_returns_false() {
        assert!(
            !matches_device_name("MacBook Pro Speakers", "airpods"),
            "non-matching substring must return false"
        );
    }

    #[test]
    fn device_name_empty_requested_never_matches() {
        // An empty requested string is treated as "no preference" — it
        // must never match anything, so the caller falls back to the
        // default device as expected.
        assert!(
            !matches_device_name("MacBook Pro Microphone", ""),
            "empty requested string must never match any device"
        );
        assert!(
            !matches_device_name("", ""),
            "empty vs empty must never match"
        );
    }

    #[test]
    fn normalized_requested_device_treats_system_default_as_no_preference() {
        assert_eq!(normalized_requested_device_name(None), None);
        assert_eq!(normalized_requested_device_name(Some("")), None);
        assert_eq!(
            normalized_requested_device_name(Some("  System default  ")),
            None
        );
        assert_eq!(
            normalized_requested_device_name(Some("Studio Display Speakers")),
            Some("Studio Display Speakers")
        );
    }

    #[test]
    fn device_name_first_match_selected() {
        // Simulate choosing the first match from a list of device names.
        let devices = [
            "MacBook Pro Microphone",
            "Studio Display Microphone",
            "Georgiy's AirPods Pro",
            "iPhone Microphone",
        ];
        let requested = "airpods";
        let matched: Option<&&str> = devices
            .iter()
            .find(|name| matches_device_name(name, requested));
        assert_eq!(matched, Some(&"Georgiy's AirPods Pro"));
    }

    #[test]
    fn device_name_fallback_when_no_match() {
        let devices = ["MacBook Pro Microphone", "Studio Display Microphone"];
        let requested = "jabra";
        let matched = devices
            .iter()
            .find(|name| matches_device_name(name, requested));
        assert!(
            matched.is_none(),
            "no match → caller should fall back to cpal default"
        );
    }

    // -----------------------------------------------------------------------
    // Input-gain clamping tests
    // -----------------------------------------------------------------------

    #[test]
    fn clamp_gain_negative_clamps_to_zero() {
        assert_eq!(clamp_gain(-1.0), 0.0, "negative gain must clamp to 0.0");
        assert_eq!(clamp_gain(f32::NEG_INFINITY), 0.0, "-inf must clamp to 0.0");
    }

    #[test]
    fn clamp_gain_above_max_clamps_to_two() {
        assert_eq!(clamp_gain(3.0), 2.0, "gain > 2.0 must clamp to 2.0");
        assert_eq!(clamp_gain(f32::INFINITY), 2.0, "+inf must clamp to 2.0");
    }

    #[test]
    fn clamp_gain_midrange_passthrough() {
        assert_eq!(
            clamp_gain(1.5),
            1.5,
            "in-range gain must pass through unchanged"
        );
        assert_eq!(
            clamp_gain(0.0),
            0.0,
            "0.0 is the lower bound; must pass through"
        );
        assert_eq!(
            clamp_gain(2.0),
            2.0,
            "2.0 is the upper bound; must pass through"
        );
        assert_eq!(
            clamp_gain(1.0),
            1.0,
            "unity gain must pass through unchanged"
        );
    }

    // -----------------------------------------------------------------------
    // apply_gain_i16 correctness tests
    // -----------------------------------------------------------------------

    #[test]
    fn apply_gain_i16_silence_stays_silence() {
        assert_eq!(apply_gain_i16(0, 2.0), 0, "0 * any gain = 0");
    }

    #[test]
    fn apply_gain_i16_unity_gain_preserves_sample() {
        // 1.0 gain should not change the sample (modulo the f32 round-trip).
        let s: i16 = 1000;
        assert_eq!(apply_gain_i16(s, 1.0), s);
    }

    #[test]
    fn apply_gain_i16_double_gain_approximately_doubles() {
        let s: i16 = 8192;
        let result = apply_gain_i16(s, 2.0);
        // 8192 * 2 = 16384; within 1 LSB of f32 round-trip
        assert!(
            (result - 16384).abs() <= 1,
            "2× gain on 8192 should yield ~16384, got {result}"
        );
    }

    #[test]
    fn apply_gain_i16_clamps_at_max() {
        // A large positive value × 2 should clamp to i16::MAX, not wrap.
        let s: i16 = i16::MAX;
        let result = apply_gain_i16(s, 2.0);
        assert_eq!(result, i16::MAX, "saturating mul must not exceed i16::MAX");
    }

    #[test]
    fn apply_gain_i16_clamps_at_min() {
        let s: i16 = i16::MIN;
        let result = apply_gain_i16(s, 2.0);
        // i16::MIN / i16::MAX as f32 * 2.0 clamps to -1.0 → i16::MIN-ish
        // The exact value depends on the f32 rounding of i16::MIN/i16::MAX;
        // just assert it doesn't overflow past i16::MIN.
        assert!(
            result <= 0,
            "negative saturating mul must not exceed i16::MIN, got {result}"
        );
    }

    #[test]
    fn noise_suppression_default_is_medium() {
        assert_eq!(NoiseSuppression::default(), NoiseSuppression::Medium);
    }

    #[test]
    fn noise_suppression_display_strings() {
        assert_eq!(NoiseSuppression::Off.to_string(), "off");
        assert_eq!(NoiseSuppression::Soft.to_string(), "soft");
        assert_eq!(NoiseSuppression::Medium.to_string(), "medium");
        assert_eq!(NoiseSuppression::Strong.to_string(), "strong");
    }

    // -----------------------------------------------------------------------
    // PCM scale / round-trip tests (#31)
    // -----------------------------------------------------------------------

    /// +1.0 must clamp to i16::MAX (32_767), NOT overflow to -32_768 or
    /// panic. This is the key invariant: 32_768 is out of range for i16,
    /// so the post-scale clamp is essential.
    #[test]
    fn f32_to_i16_pos_one_clamps_to_max() {
        assert_eq!(
            f32_to_i16(1.0),
            i16::MAX,
            "+1.0 must clamp to i16::MAX (32_767), not overflow"
        );
    }

    /// -1.0 must map exactly to i16::MIN (-32_768).
    #[test]
    fn f32_to_i16_neg_one_maps_to_min() {
        assert_eq!(
            f32_to_i16(-1.0),
            i16::MIN,
            "-1.0 must map exactly to i16::MIN (-32_768)"
        );
    }

    /// 0.0 must map to 0.
    #[test]
    fn f32_to_i16_zero_maps_to_zero() {
        assert_eq!(f32_to_i16(0.0), 0);
    }

    /// Both scale factors are now `32_768.0`, so the round-trip
    /// `i16 → f32 → i16` must be lossless for all i16 values except
    /// i16::MAX (which maps to ≈0.9999695 in f32 and back to 32_767 —
    /// still exact for i16::MAX itself, but let's verify mid-range).
    #[test]
    fn f32_to_i16_round_trip_midrange() {
        // Exact mid-range values that fit within f32 precision.
        for &v in &[0_i16, 1000, -1000, 16_000, -16_000, i16::MIN] {
            let f = i16_to_f32(v);
            let back = f32_to_i16(f);
            assert_eq!(
                back, v,
                "round-trip i16({v}) → f32({f}) → i16 failed: got {back}"
            );
        }
    }

    /// Symmetric scale check: positive and negative samples of equal
    /// magnitude must produce equal-magnitude i16 results (within 1 LSB
    /// of f32 rounding).
    #[test]
    fn f32_to_i16_symmetric_scale() {
        // 0.5 → 16_384; -0.5 → -16_384. Both use the same 32_768.0 factor.
        let pos = f32_to_i16(0.5);
        let neg = f32_to_i16(-0.5);
        assert_eq!(pos, 16_384, "0.5 * 32768 = 16384");
        assert_eq!(neg, -16_384, "-0.5 * 32768 = -16384");
    }

    /// Values outside [-1.0, 1.0] must be clamped before scaling, not
    /// wrap or panic.
    #[test]
    fn f32_to_i16_out_of_range_clamped() {
        assert_eq!(f32_to_i16(2.0), i16::MAX, "2.0 must clamp to i16::MAX");
        assert_eq!(f32_to_i16(-2.0), i16::MIN, "-2.0 must clamp to i16::MIN");
        assert_eq!(
            f32_to_i16(f32::INFINITY),
            i16::MAX,
            "+inf must clamp to i16::MAX"
        );
        assert_eq!(
            f32_to_i16(f32::NEG_INFINITY),
            i16::MIN,
            "-inf must clamp to i16::MIN"
        );
    }
}
