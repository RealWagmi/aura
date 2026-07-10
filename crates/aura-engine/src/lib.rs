//! `aura-engine` — the mode-agnostic call runtime, and the home of the
//! single seam between LOCAL and REMOTE: the [`AudioTransport`] trait.
//!
//! The engine is mode-agnostic: the only thing that differs is which
//! [`AudioTransport`] implementation is passed in (`CpalTransport` in
//! `aura-audio` for the in-process LOCAL path; the unified Noise/UDP
//! `TunnelTransport` in `aura-tunnel` — used by both LOCAL-loopback and REMOTE).
//! Everything above the transport — the voice provider, the host adapter,
//! the brief packer, the barge-in state machine, the in-call dispatch — is
//! byte-for-byte identical across modes. That is the whole architecture.
//!
//! [`CallSession::run`] is the single-task event loop both binaries drive: it
//! pumps mic frames to the provider, plays model audio back through the
//! transport, runs the barge-in state machine (cancel + suppress as a unit),
//! injects feeder digests, and reconnects with bounded backoff on transient
//! failure. One task owns the transport, sink, and stream, so a two-task
//! command-channel is unnecessary — `tokio::select!` drops the
//! losing branch's borrow before a handler runs, so the event handler can use
//! the transport/sink the mic branch was awaiting. In-call dispatch routes the
//! model's tool calls through the `ToolRouter` voice-approval boundary to the
//! host (which executes with full repo + tool access) on a `JoinSet`, speaks
//! the result back, and also delivers it into the host chat via
//! [`HostAdapter::deliver_callback`]. The live ambient feeder
//! injects mid-call digests via the [`AmbientFeeder`] seam.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinSet;

use aura_core::config::SafetyConfig;
use aura_core::tools::{
    AgentRuntime, TaskEnvelope, TaskHandoffState, TaskResult, ToolCall, ToolError, ToolResponse,
    ToolRouter,
};
use aura_core::CallbackMode;
use aura_hosts::HostAdapter;
use aura_voice::{
    VoiceEvent, VoiceProvider, VoiceRuntimeEvent, VoiceSessionConfig, VoiceSink, VoiceStream,
    VoiceToolCall,
};

pub mod barge_in;
pub mod inbox;
pub mod orchestrator;

use barge_in::speech_started_decision;
use inbox::Inbox;
use orchestrator::{OrchestratedRuntime, OrchestratorConfig};

/// The tool that lets the model hang the call up by voice (no dispatch).
const END_VOICE_SESSION_TOOL: &str = "end_voice_session";
/// The tool the model calls to pause the call until a condition.
const PAUSE_CALL_TOOL: &str = "pause_call_until";
/// Safety ceiling for an `event`-conditioned pause until an external unpause
/// channel is wired (v1) — never stay paused forever.
const PAUSE_EVENT_SAFETY_SECS: u64 = 20 * 60;
/// Max lines of pre-pause dialogue carried into the resume digest.
const PAUSE_DIGEST_MAX_LINES: usize = 12;
/// Max transcript lines retained for the post-call recap; bounds
/// memory while keeping far more than the pause window.
const RECAP_MAX_LINES: usize = 200;
/// Cap on the in-progress transcript buffer for a turn that never finalizes
/// (bounds memory; the tail is kept).
const USER_BUF_CAP: usize = 8_192;
/// Ceiling on the call-end summary delivery so a misbehaving host adapter can't
/// stall teardown.
const RECAP_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// A successful `response.cancel` must terminate with `response.done` before
/// another `response.create` is sent. Reconnect if the provider never closes
/// that lifecycle, rather than leaving a committed PTT turn unanswered.
const CANCEL_DONE_TIMEOUT: Duration = Duration::from_secs(5);
/// Hard safety ceiling for a manual turn whose close/cancel control is lost.
/// Cancel rather than commit: an unattended open microphone must never cause
/// Aura to submit ambient audio as an intentional user message.
const PTT_OPEN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// A callback is best-effort and must not outlive the call indefinitely.
const CALLBACK_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// What a finished in-call dispatch carries back to the event loop: the
/// provider's tool `call_id`, the routed result to speak back, and — for a
/// genuine worker dispatch — the metadata needed to deliver the finished task
/// back into the host chat via [`HostAdapter::deliver_callback`].
type DispatchOutcome = (
    Option<String>,
    Result<ToolResponse, ToolError>,
    Option<CallbackMeta>,
);

/// The minimum a completed dispatch needs to compose a [`TaskResult`] for the
/// host's chat callback. Captured from the tool-call arguments at spawn time so
/// the developer's spoken intent is recited faithfully (e.g. Hermes's
/// "Request:" line) rather than reconstructed lossily from the spoken result.
struct CallbackMeta {
    user_intent: String,
    constraints: Vec<String>,
    project: String,
}

/// What the model wants the call paused until.
#[derive(Debug, Clone)]
enum PauseCondition {
    /// Resume when the in-flight dispatched task completes.
    TaskComplete,
    /// Resume after a fixed delay.
    Timeout(Duration),
    /// Resume on a named external event (key/feeder). v1 falls back to
    /// a bounded safety timeout until that channel is wired.
    Event(String),
}

/// A result latched while paused, woven into the resume turn.
enum Latched {
    None,
    Task(Result<ToolResponse, ToolError>),
}

enum PausedOutcome {
    Resume(Latched),
    HangUp,
}

/// How the active phase ended.
enum Transition {
    Ended(EndReason),
    Pause(PauseCondition),
}

/// The single seam between LOCAL and REMOTE.
///
/// The engine consumes and produces **PCM16, mono, little-endian,
/// 24 000 Hz** frames (the realtime xAI contract). Only the source/sink of
/// those frames differs between modes — that is this trait. All
/// resampling/codec work lives *inside* the REMOTE implementation; the
/// engine never sees Opus, RTP, or a sample rate other than 24k.
#[async_trait]
pub trait AudioTransport: Send {
    /// The next ~20 ms frame of audio (mic) as PCM16 mono LE @ 24k. `None`
    /// means the transport closed (hang-up / peer left).
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>>;
    /// The next transport input. Normal transports can return only audio by
    /// relying on this default; remote PTT transports may also surface control
    /// events such as "commit this user turn now".
    async fn recv_input(&mut self) -> Option<TransportInput> {
        self.recv_pcm24().await.map(TransportInput::Audio)
    }
    /// Play a model frame (PCM16 mono LE @ 24k) to the speaker.
    async fn send_pcm24(&mut self, pcm: &[i16]) -> Result<(), TransportError>;
    /// Drop everything queued for playout (barge-in).
    fn clear_playout(&self);
    /// Milliseconds currently queued for playout — drives the barge-in
    /// decision (an open speaker with audio queued echoes into the mic).
    fn queued_ms(&self) -> u64;
    /// Flush any buffered outbound audio that doesn't fill a full transport
    /// frame — e.g. the `<20 ms` tail of a finished model response that a
    /// fixed-frame reframer would otherwise hold back, swallowing phrase
    /// endings. Called by the engine on `ResponseDone`. Default: no-op (LOCAL
    /// pushes all samples straight to the speaker, so it has no such tail).
    async fn flush_output(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportControl {
    PttOpen,
    PttClose,
    /// Abandon the open push-to-talk turn WITHOUT committing it (the client
    /// discarded a too-short recording): drop the already-streamed frames from
    /// the provider input buffer so they cannot prefix the next turn.
    PttCancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportInput {
    Audio(Vec<i16>),
    Control(TransportControl),
}

/// Errors a transport can surface on send. Receive failures are modeled as
/// `recv_pcm24() -> None` (transport closed).
#[derive(Debug, Clone, thiserror::Error)]
pub enum TransportError {
    /// The transport is closed; no further frames can be sent.
    #[error("transport closed")]
    Closed,
    /// An I/O or device error while sending.
    #[error("transport io error: {0}")]
    Io(String),
}

/// Why a call ended. Surfaced to the binary so it can report and exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// The user or peer hung up / the transport closed normally.
    HangUp,
    /// The provider returned a terminal error (e.g. balance exhausted).
    ProviderFatal,
    /// Reconnect attempts were exhausted.
    ReconnectExhausted,
}

/// The result of a completed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOutcome {
    /// Why the call ended.
    pub reason: EndReason,
}

/// Errors that abort `CallSession::run` before a normal [`CallOutcome`].
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A voice-provider error that could not be recovered.
    #[error("voice error: {0}")]
    Voice(#[from] aura_voice::VoiceError),
    /// A transport error that could not be recovered.
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
}

/// A live ambient-context feeder. The engine injects each digest via
/// [`VoiceSink::inject_system_context`] WITHOUT triggering a response, making
/// "the AI already knows the chat" true as the call runs. The engine may run
/// with `None`; the `aura-feeder` crate supplies the implementation.
#[async_trait]
pub trait AmbientFeeder: Send + Sync {
    /// The next digest to inject, or `None` when the feeder is idle/closed.
    async fn next_digest(&self) -> Option<String>;
}

/// Mutable call state tracked across the event loop.
#[derive(Debug, Default)]
struct CallState {
    /// After a barge-in cancel, drop the cancelled response's late
    /// audio/text/tool deltas until the next `ResponseDone`. Carried as a UNIT
    /// with the `cancel` send ("no cancel without guard" rule).
    suppress: bool,
    /// The provider emitted `response.created` (or output, for compatibility)
    /// and the response has not reached `response.done` yet.
    response_created: bool,
    /// A `response.create` was sent but the provider has not yet emitted
    /// `response.created` or output. This closes the pre-audio cancellation
    /// window for cold starts, manual commits, and tool-result responses.
    response_requested: bool,
    /// Set only for a follow-up response requested while handling the prior
    /// response's terminal event. A duplicate late terminal event is ignored
    /// until the follow-up proves it started. Ordinary/legacy requests keep
    /// accepting a legitimate done-before-created sequence.
    ignore_stale_done_until_created: bool,
    /// Deadline for the terminal `response.done` belonging to a successfully
    /// sent cancellation. While set, every new `response.create` is deferred.
    cancel_done_deadline: Option<tokio::time::Instant>,
    /// One or more conversation items are committed, but their response must
    /// wait for `cancel_done_deadline` to clear (and for an open PTT turn to
    /// close).
    deferred_response: bool,
    /// A legacy sink combines commit and response.create in
    /// `commit_user_turn`. It cannot be invoked until the prior response has
    /// terminated, and must not be followed by a second request_response.
    deferred_legacy_commit: bool,
    /// Locally retained frames for legacy combined-commit sinks. Rebuilding
    /// the provider buffer at commit time prevents a later PTT open/clear or a
    /// reconnect from silently discarding an earlier deferred turn.
    deferred_legacy_audio: Vec<Vec<i16>>,
    /// Ordered, typed conversation items sent while cancellation is pending.
    /// The explicit PTT item boundaries are required for timeout replay: two
    /// user turns must never become one combined input-audio item.
    deferred_items: Vec<DeferredConversationItem>,
    /// Audio for the currently open PTT turn. It is separate from committed
    /// items so timeout recovery can restore a partially recorded turn without
    /// accidentally committing or combining it.
    deferred_open_ptt_audio: Vec<Vec<i16>>,
    /// Whether a manual PTT turn is currently open. Duplicate `PttOpen`
    /// controls are idempotent and must not cancel again or reset the timeout.
    ptt_open: bool,
    /// Fixed deadline for an open manual turn. Audio does not extend it.
    ptt_open_deadline: Option<tokio::time::Instant>,
    /// Whether the user is currently speaking (server-VAD).
    user_speaking: bool,
    /// The model called `end_voice_session`; hang up once its farewell turn
    /// finishes (next `ResponseDone`).
    ending: bool,
    /// The model called `pause_call_until`; enter the paused phase once its
    /// "pausing" turn finishes (next `ResponseDone`).
    pending_pause: Option<PauseCondition>,
    /// Conversation-item id of the response audio currently playing (when the
    /// provider reports one) — the barge-in truncate target.
    current_item: Option<String>,
    /// PCM samples delivered to the transport for `current_item`. Divided by
    /// 24 this is the item's delivered milliseconds; minus the transport's
    /// still-queued milliseconds it approximates what the user actually HEARD
    /// — the `audio_end_ms` for `VoiceSink::truncate_item`.
    item_delivered_samples: u64,
    /// PCM samples received for the current manual push-to-talk user turn.
    ptt_input_samples: u64,
}

impl CallState {
    fn response_in_flight(&self) -> bool {
        self.response_requested || self.response_created
    }

    fn mark_response_requested(&mut self) {
        self.response_requested = true;
    }

    fn mark_response_created(&mut self) {
        self.response_requested = false;
        self.response_created = true;
        self.ignore_stale_done_until_created = false;
    }

    fn mark_response_done(&mut self) {
        self.response_requested = false;
        self.response_created = false;
        self.ignore_stale_done_until_created = false;
    }

    fn mark_cancel_pending(&mut self) {
        if self.cancel_done_deadline.is_none() {
            self.cancel_done_deadline = Some(tokio::time::Instant::now() + CANCEL_DONE_TIMEOUT);
        }
    }

    fn clear_cancel_pending(&mut self) {
        self.cancel_done_deadline = None;
    }

    fn track_manual_audio(&mut self, pcm: &[i16]) {
        if !self.ptt_open {
            return;
        }
        self.ptt_input_samples = self.ptt_input_samples.saturating_add(pcm.len() as u64);
        self.deferred_open_ptt_audio.push(pcm.to_vec());
    }

    /// Reset after a reconnect: the new session has no in-flight response and
    /// no pending cancel to suppress.
    fn on_reconnect(&mut self) {
        self.suppress = false;
        self.mark_response_done();
        self.clear_cancel_pending();
        self.deferred_response = false;
        self.deferred_legacy_commit = false;
        self.deferred_legacy_audio.clear();
        self.deferred_items.clear();
        self.deferred_open_ptt_audio.clear();
        self.ptt_open = false;
        self.ptt_open_deadline = None;
        self.user_speaking = false;
        self.current_item = None;
        self.item_delivered_samples = 0;
        self.ptt_input_samples = 0;
        self.ending = false;
        self.pending_pause = None;
    }
}

#[derive(Debug)]
enum DeferredConversationItem {
    PttAudio(Vec<Vec<i16>>),
    ToolResult {
        call_id: Option<String>,
        content: serde_json::Value,
    },
}

const MIN_PTT_COMMIT_SAMPLES: u64 = 2_400; // 100 ms at 24 kHz

/// What the event loop should do after handling one event.
enum Flow {
    Continue,
    End(EndReason),
    Reconnect,
    /// Enter the paused phase (a `pause_call_until` turn just finished).
    Pause,
}

/// The call runtime entry point — identical for LOCAL and REMOTE.
pub struct CallSession;

struct ManualTurnProvider {
    inner: Arc<dyn VoiceProvider>,
}

#[async_trait]
impl VoiceProvider for ManualTurnProvider {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn default_voice(&self) -> &str {
        self.inner.default_voice()
    }

    fn audio_caps(&self) -> aura_voice::AudioCaps {
        self.inner.audio_caps()
    }

    async fn connect(
        &self,
        cfg: &VoiceSessionConfig,
    ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), aura_voice::VoiceError> {
        self.inner
            .connect_with_manual_turn_detection(cfg, true)
            .await
    }
}

impl CallSession {
    /// Run a call to completion. Connects the provider, then drives the
    /// single-task loop until the transport closes (hang-up), the provider is
    /// terminally unavailable, or reconnect attempts are exhausted.
    pub async fn run(
        transport: Box<dyn AudioTransport>,
        provider: Arc<dyn VoiceProvider>,
        host: Arc<dyn HostAdapter>,
        feeder: Option<Arc<dyn AmbientFeeder>>,
        cfg: VoiceSessionConfig,
    ) -> Result<CallOutcome, EngineError> {
        Self::run_inner(transport, provider, host, feeder, cfg, false).await
    }

    pub async fn run_with_manual_turn_detection(
        transport: Box<dyn AudioTransport>,
        provider: Arc<dyn VoiceProvider>,
        host: Arc<dyn HostAdapter>,
        feeder: Option<Arc<dyn AmbientFeeder>>,
        cfg: VoiceSessionConfig,
    ) -> Result<CallOutcome, EngineError> {
        let provider: Arc<dyn VoiceProvider> = Arc::new(ManualTurnProvider { inner: provider });
        Self::run_inner(transport, provider, host, feeder, cfg, true).await
    }

    async fn run_inner(
        mut transport: Box<dyn AudioTransport>,
        provider: Arc<dyn VoiceProvider>,
        host: Arc<dyn HostAdapter>,
        feeder: Option<Arc<dyn AmbientFeeder>>,
        cfg: VoiceSessionConfig,
        manual_turn_detection: bool,
    ) -> Result<CallOutcome, EngineError> {
        // Open-speaker echo suppression defaults off — headsets stay
        // interruptible; detecting an open speaker is a follow-up.
        let echo_risk = false;

        // Mid-call reconnects rebuild their session config PER ATTEMPT via
        // `build_reconnect_cfg`: original instructions + the dialogue-so-far
        // digest + a continuity directive. A static config here (the original
        // instructions alone) made every reconnect an AMNESIAC fresh session —
        // the model forgot the conversation of seconds ago and could switch
        // languages whenever the link blipped.

        let (mut sink, mut stream) = provider.connect(&cfg).await?;
        // Call-duration metric: wall-clock from a connected session
        // to hang-up. Logged at the end; no content, safe to emit.
        let call_started = std::time::Instant::now();
        let mut state = CallState {
            // Both providers batch this `response.create` into the successful
            // connect flush, so it is already cancellable before any server
            // event or output-audio delta arrives.
            response_requested: cfg.cold_start_kick,
            ..CallState::default()
        };

        // In-call dispatch: the model's tool calls route through the
        // `ToolRouter` voice-approval boundary to the host, which executes with
        // full repo + tool access; results are spoken back.
        //
        // Scheme 2: wrap the host runtime in the orchestrator decorator, which
        // routes dispatch through the `.aura/inbox/` coordination layer to the
        // LIVE host chat session (if its watch-loop is running) and only falls
        // back to a direct spawn otherwise. The wrap is a no-op — a single
        // heartbeat stat per dispatch — when no orchestrator loop is live, so it
        // is always safe to apply.
        //
        // The inbox roots at `AURA_STATE_DIR` (else the process cwd), which MUST
        // match what the host's `aura-inbox` watch-loop resolves — a mismatch
        // silently degrades every dispatch to the direct fallback. The env
        // override exists exactly for hosts whose exec tool gives every command
        // a fresh/implicit cwd (messenger gateways): set it once and both sides
        // converge regardless of where each process starts. Log the ABSOLUTE
        // inbox dir once so an operator can diagnose a mismatch instead of
        // guessing. `open_for_call` resets the inbox for this fresh call (see
        // its docs), so a prior call's orphaned task can't resurface.
        let raw_agent: Arc<dyn AgentRuntime> = host.clone();
        let state_root = state_root_from(std::env::var("AURA_STATE_DIR").ok().as_deref());
        let agent: Arc<dyn AgentRuntime> = match Inbox::open_for_call(&state_root) {
            Ok(inbox) => {
                // Best-effort absolute path so the log is diagnostic regardless of
                // the process cwd; fall back to the relative path if canonicalize
                // fails (e.g. a transient FS error).
                let shown = std::fs::canonicalize(inbox.dir())
                    .unwrap_or_else(|_| inbox.dir().to_path_buf());
                eprintln!(
                    "aura-engine: in-call dispatch inbox at {} (the host's `aura-inbox` \
                     watch-loop must run in this directory)",
                    shown.display()
                );
                Arc::new(OrchestratedRuntime::new(
                    raw_agent,
                    inbox,
                    OrchestratorConfig::default(),
                ))
            }
            // Can't create the inbox dir → dispatch directly (unchanged behaviour).
            Err(e) => {
                eprintln!(
                    "aura-engine: in-call dispatch inbox unavailable ({e}); dispatching directly"
                );
                raw_agent
            }
        };
        let router = Arc::new(ToolRouter::with_safety(
            agent,
            CallbackMode::SpeakImmediately,
            SafetyConfig::default(),
        ));
        let mut dispatch: JoinSet<DispatchOutcome> = JoinSet::new();
        let mut callback_delivery: JoinSet<()> = JoinSet::new();
        // Accumulates the developer's spoken lines so a resumed-from-pause
        // session doesn't lose the conversation context.
        let mut transcript = InCallTranscript::default();

        let outcome = 'call: loop {
            // --- ACTIVE phase: the realtime session is live. ---
            let transition = 'active: loop {
                tokio::select! {
                    mic = transport.recv_input() => match mic {
                        Some(TransportInput::Audio(pcm)) => {
                            if manual_turn_detection {
                                state.track_manual_audio(&pcm);
                            }
                            if sink.send_audio(&pcm).await.is_err() {
                                match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "mic-audio send failed", &mut state).await {
                                    Some((s, st)) => { sink = s; stream = st; }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                }
                            }
                        }
                        Some(TransportInput::Control(TransportControl::PttOpen)) => {
                            if manual_turn_detection {
                                // Reliable controls are deduplicated by the
                                // tunnel, but keep the engine idempotent too:
                                // a repeated open must not erase this turn,
                                // resend cancel, or extend the cancel timeout.
                                if state.ptt_open {
                                    continue;
                                }
                                state.ptt_open = true;
                                state.ptt_open_deadline =
                                    Some(tokio::time::Instant::now() + PTT_OPEN_TIMEOUT);
                                state.deferred_open_ptt_audio.clear();
                                // A PTT press while Aura is speaking is a
                                // barge-in and gets the SAME atomic unit as the
                                // VAD path: heard-position capture → clear
                                // playout → cancel + suppress → truncate at the
                                // heard position. Plus PTT hygiene: clear any
                                // un-committed provider input (a discarded
                                // too-short snippet, or a prior turn's stray
                                // tail) so it cannot prepend onto this turn.
                                state.ptt_input_samples = 0;
                                if let Err(flow_reason) = ptt_barge_in(transport.as_mut(), sink.as_mut(), &mut state).await {
                                    match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), flow_reason, &mut state).await {
                                        Some((s, st)) => { sink = s; stream = st; }
                                        None => break Transition::Ended(EndReason::ReconnectExhausted),
                                    }
                                } else if sink.clear_user_audio().await.is_err() {
                                    match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "push-to-talk clear failed", &mut state).await {
                                        Some((s, st)) => { sink = s; stream = st; }
                                        None => break Transition::Ended(EndReason::ReconnectExhausted),
                                    }
                                }
                            } else {
                                eprintln!("aura-engine: ignoring PTT open in voice mode.");
                            }
                        }
                        Some(TransportInput::Control(TransportControl::PttClose)) => {
                            if !manual_turn_detection {
                                eprintln!("aura-engine: ignoring PTT close in voice mode.");
                                continue;
                            }
                            if !state.ptt_open {
                                continue;
                            }
                            if state.ptt_input_samples < MIN_PTT_COMMIT_SAMPLES {
                                state.ptt_input_samples = 0;
                                state.ptt_open = false;
                                state.ptt_open_deadline = None;
                                state.deferred_open_ptt_audio.clear();
                                if sink.clear_user_audio().await.is_err() {
                                    match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "push-to-talk clear failed", &mut state).await {
                                        Some((s, st)) => { sink = s; stream = st; }
                                        None => break Transition::Ended(EndReason::ReconnectExhausted),
                                    }
                                }
                                if state.deferred_response
                                    && request_response_when_ready(sink.as_mut(), &mut state)
                                        .await
                                        .is_err()
                                {
                                    match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "push-to-talk response request failed", &mut state).await {
                                        Some((s, st)) => { sink = s; stream = st; }
                                        None => break Transition::Ended(EndReason::ReconnectExhausted),
                                    }
                                }
                                continue;
                            }
                            state.ptt_input_samples = 0;
                            state.ptt_open = false;
                            state.ptt_open_deadline = None;
                            if commit_ptt_turn_when_ready(sink.as_mut(), &mut state).await.is_err() {
                                match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "push-to-talk commit failed", &mut state).await {
                                    Some((s, st)) => { sink = s; stream = st; }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                }
                            }
                        }
                        Some(TransportInput::Control(TransportControl::PttCancel)) => {
                            if !manual_turn_detection {
                                eprintln!("aura-engine: ignoring PTT cancel in voice mode.");
                                continue;
                            }
                            if !state.ptt_open {
                                continue;
                            }
                            // The client discarded the open turn: drop the
                            // frames it already streamed so they cannot prefix
                            // the next committed turn.
                            state.ptt_input_samples = 0;
                            state.ptt_open = false;
                            state.ptt_open_deadline = None;
                            state.deferred_open_ptt_audio.clear();
                            if sink.clear_user_audio().await.is_err() {
                                match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "push-to-talk clear failed", &mut state).await {
                                    Some((s, st)) => { sink = s; stream = st; }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                }
                            }
                            if state.deferred_response
                                && request_response_when_ready(sink.as_mut(), &mut state)
                                    .await
                                    .is_err()
                            {
                                match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "push-to-talk response request failed", &mut state).await {
                                    Some((s, st)) => { sink = s; stream = st; }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                }
                            }
                        }
                        None => break Transition::Ended(EndReason::HangUp),
                    },
                    ev = stream.next_runtime_event() => match ev {
                        None => match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "the provider closed the event stream", &mut state).await {
                            Some((s, st)) => { sink = s; stream = st; }
                            None => break Transition::Ended(EndReason::ReconnectExhausted),
                        },
                        Some(Err(e)) => {
                            if e.is_terminal() {
                                break Transition::Ended(EndReason::ProviderFatal);
                            }
                            match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), &format!("event-stream error: {e}"), &mut state).await {
                                Some((s, st)) => { sink = s; stream = st; }
                                None => break Transition::Ended(EndReason::ReconnectExhausted),
                            }
                        }
                        Some(Ok(VoiceRuntimeEvent::ResponseCreated)) => {
                            state.mark_response_created();
                        }
                        Some(Ok(VoiceRuntimeEvent::Voice(VoiceEvent::ToolCall(call)))) => {
                            // Cancel + suppress as a unit: a tool call that
                            // belongs to a response the user barged-in over (cancelled)
                            // must NOT be acted on — including the control tools. Acting
                            // on a cancelled `end_voice_session`/`pause_call_until` would
                            // hang up or pause the call against the developer's just-
                            // expressed intent to keep talking. Drop it; the next
                            // ResponseDone clears `suppress`.
                            if state.suppress {
                                // Suppressed: ignore this cancelled response's tool call.
                            } else {
                                // A tool call is definitive response output.
                                // Legacy VoiceStream implementations do not
                                // emit the runtime-only ResponseCreated marker,
                                // so promote the lifecycle here before queuing
                                // the tool result's follow-up response.
                                state.mark_response_created();
                                if call.name == END_VOICE_SESSION_TOOL {
                                state.ending = true;
                                } else if call.name == PAUSE_CALL_TOOL {
                                // Session-control (NOT routed through ToolRouter — no
                                // voice-approval). Ack, then enter pause
                                // on this turn's ResponseDone.
                                let content = match parse_pause_condition(&call.args) {
                                    Some(cond) => {
                                        state.pending_pause = Some(cond);
                                        serde_json::json!({ "paused": true })
                                    }
                                    None => serde_json::json!({ "error": "unknown pause condition" }),
                                };
                                state.deferred_items.push(
                                    DeferredConversationItem::ToolResult {
                                        call_id: call.call_id.clone(),
                                        content: content.clone(),
                                    },
                                );
                                state.deferred_response = true;
                                let sent = sink
                                    .send_tool_result(call.call_id.as_deref(), content.clone())
                                    .await
                                    .is_ok();
                                if sent {
                                    if request_response_when_ready(sink.as_mut(), &mut state)
                                        .await
                                        .is_err()
                                    {
                                        match reconnect_with_state(
                                            &provider,
                                            &build_reconnect_cfg(&cfg, &transcript),
                                            "pause tool-result response request failed",
                                            &mut state,
                                        ).await {
                                            Some((s, st)) => { sink = s; stream = st; }
                                            None => break Transition::Ended(EndReason::ReconnectExhausted),
                                        }
                                    }
                                } else {
                                    match reconnect_with_state(
                                        &provider,
                                        &build_reconnect_cfg(&cfg, &transcript),
                                        "pause tool-result send failed",
                                        &mut state,
                                    ).await {
                                        Some((s, st)) => { sink = s; stream = st; }
                                        None => break Transition::Ended(EndReason::ReconnectExhausted),
                                    }
                                }
                                } else {
                                    spawn_dispatch(&router, &mut dispatch, call);
                                }
                            }
                        }
                        Some(Ok(VoiceRuntimeEvent::Voice(event))) => {
                            transcript.observe(&event);
                            match handle_event(event, transport.as_mut(), sink.as_mut(), &mut state, echo_risk).await {
                                Flow::Continue => {}
                                Flow::End(reason) => break Transition::Ended(reason),
                                Flow::Pause => {
                                    let cond = state.pending_pause.take().unwrap_or(PauseCondition::TaskComplete);
                                    break Transition::Pause(cond);
                                }
                                Flow::Reconnect => match reconnect_with_state(&provider, &build_reconnect_cfg(&cfg, &transcript), "recovering from an in-call error (see the line above)", &mut state).await {
                                    Some((s, st)) => { sink = s; stream = st; }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                },
                            }
                        }
                    },
                    _ = wait_cancel_done_timeout(state.cancel_done_deadline) => {
                        // The provider accepted `response.cancel` but never
                        // closed that response. Preserve any PTT audio locally,
                        // replace the wedged session, and replay the user item
                        // before requesting its answer.
                        let recovery = CancelTimeoutRecovery::take(&mut state);
                        match reconnect(
                            &provider,
                            &build_reconnect_cfg(&cfg, &transcript),
                            "cancelled response did not emit response.done",
                        ).await {
                            Some((s, st)) => {
                                sink = s;
                                stream = st;
                                state.on_reconnect();
                                if replay_after_cancel_timeout(
                                    sink.as_mut(),
                                    &mut state,
                                    recovery,
                                )
                                .await
                                .is_err()
                                {
                                    break 'active Transition::Ended(EndReason::ReconnectExhausted);
                                }
                            }
                            None => break Transition::Ended(EndReason::ReconnectExhausted),
                        }
                    }
                    digest = next_feeder_digest(feeder.as_ref()) => {
                        // Ambient inject never triggers a response.
                        let _ = sink.inject_system_context(&digest).await;
                    }
                    joined = dispatch.join_next(), if !dispatch.is_empty() => {
                        if let Some(Ok((call_id, result, callback))) = joined {
                            let content = match &result {
                                Ok(resp) => resp.content.clone(),
                                Err(e) => serde_json::json!({ "error": e.to_string() }),
                            };
                            state.deferred_items.push(
                                DeferredConversationItem::ToolResult {
                                    call_id: call_id.clone(),
                                    content: content.clone(),
                                },
                            );
                            state.deferred_response = true;
                            let sent = sink
                                .send_tool_result(call_id.as_deref(), content.clone())
                                .await
                                .is_ok();
                            if sent {
                                if request_response_when_ready(sink.as_mut(), &mut state)
                                    .await
                                    .is_err()
                                {
                                    match reconnect_with_state(
                                        &provider,
                                        &build_reconnect_cfg(&cfg, &transcript),
                                        "dispatch tool-result response request failed",
                                        &mut state,
                                    ).await {
                                        Some((s, st)) => { sink = s; stream = st; }
                                        None => break Transition::Ended(EndReason::ReconnectExhausted),
                                    }
                                }
                            } else {
                                match reconnect_with_state(
                                    &provider,
                                    &build_reconnect_cfg(&cfg, &transcript),
                                    "dispatch tool-result send failed",
                                    &mut state,
                                ).await {
                                    Some((s, st)) => { sink = s; stream = st; }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                }
                            }
                            // Universal callback seam: a completed
                            // worker dispatch is also delivered back into the
                            // host chat, not only spoken. Best-effort/fail-open.
                            deliver_chat_callback(
                                &host,
                                callback,
                                &result,
                                &mut callback_delivery,
                            );
                        }
                    }
                    _ = wait_ptt_open_timeout(state.ptt_open_deadline) => {
                        // A missing close/cancel must not leave the provider's
                        // input buffer recording forever. Fail closed: discard
                        // the partial turn and unblock any earlier deferred
                        // response without treating ambient audio as intent.
                        if cancel_stale_open_ptt(sink.as_mut(), &mut state).await.is_err() {
                            match reconnect_with_state(
                                &provider,
                                &build_reconnect_cfg(&cfg, &transcript),
                                "push-to-talk watchdog clear failed",
                                &mut state,
                            ).await {
                                Some((s, st)) => { sink = s; stream = st; }
                                None => break Transition::Ended(EndReason::ReconnectExhausted),
                            }
                        } else if state.deferred_response
                            && request_response_when_ready(sink.as_mut(), &mut state)
                                .await
                                .is_err()
                        {
                            match reconnect_with_state(
                                &provider,
                                &build_reconnect_cfg(&cfg, &transcript),
                                "push-to-talk watchdog response request failed",
                                &mut state,
                            ).await {
                                Some((s, st)) => { sink = s; stream = st; }
                                None => break Transition::Ended(EndReason::ReconnectExhausted),
                            }
                        }
                    }
                    _ = callback_delivery.join_next(), if !callback_delivery.is_empty() => {}
                }
            };

            match transition {
                Transition::Ended(reason) => break 'call CallOutcome { reason },
                Transition::Pause(cond) => {
                    // --- PAUSED phase: collapse the realtime leg so xAI stops
                    // billing for idle keep-alive. The dispatched subagent keeps
                    // running; the transport stays up for instant resume. ---
                    transport.clear_playout();
                    let _ = sink.close().await;
                    let latched = match run_paused(
                        cond,
                        &mut dispatch,
                        &host,
                        &mut callback_delivery,
                        transport.as_mut(),
                    )
                    .await
                    {
                        PausedOutcome::Resume(latched) => latched,
                        PausedOutcome::HangUp => {
                            break 'call CallOutcome {
                                reason: EndReason::HangUp,
                            };
                        }
                    };

                    // --- RESUME: bring the realtime leg back with the pre-pause
                    // dialogue digest + the latched result, then speak it. ---
                    let resume_cfg = build_resume_cfg(&cfg, &transcript, &latched);
                    match reconnect(&provider, &resume_cfg, "pause condition met").await {
                        Some((s, st)) => {
                            sink = s;
                            stream = st;
                            state.on_reconnect();
                            let _ = request_response_when_ready(sink.as_mut(), &mut state).await;
                        }
                        None => {
                            break 'call CallOutcome {
                                reason: EndReason::ReconnectExhausted,
                            }
                        }
                    }
                }
            }
        };

        // Drain any in-flight dispatches so a spawned `claude -p` isn't aborted
        // mid-write when the call ends.
        dispatch.shutdown().await;
        while callback_delivery.join_next().await.is_some() {}
        let _ = sink.close().await;
        eprintln!(
            "aura-engine: metric call_duration_s={:.1} reason={:?}",
            call_started.elapsed().as_secs_f64(),
            outcome.reason
        );
        // Post-call summary: close the context loop call→chat.
        // Best-effort/fail-open and time-bounded — neither a delivery error nor
        // a slow host adapter changes the outcome or stalls teardown. An empty
        // transcript still delivers a minimal note: "the recap is silently
        // missing" and "the call produced nothing" must be distinguishable to
        // the host (a 15-minute call with no recap looks like a delivery bug).
        let recap = transcript.recap();
        let payload = if recap.is_empty() {
            format!(
                "The voice call ended ({:?}) after {:.0} s. No transcript lines were \
                 captured and no in-call tasks were dispatched.",
                outcome.reason,
                call_started.elapsed().as_secs_f64()
            )
        } else {
            recap
        };
        match tokio::time::timeout(RECAP_DELIVERY_TIMEOUT, host.deliver_call_summary(&payload))
            .await
        {
            Ok(Err(e)) => eprintln!("aura-engine: call summary delivery failed: {e}"),
            Err(_) => eprintln!("aura-engine: call summary delivery timed out"),
            Ok(Ok(_)) => {}
        }
        Ok(outcome)
    }
}

/// Handle one decoded [`VoiceEvent`]. Pure of reconnect/transport-lifecycle
/// concerns except for signalling them back to the loop via [`Flow`].
async fn handle_event(
    event: VoiceEvent,
    transport: &mut dyn AudioTransport,
    sink: &mut dyn VoiceSink,
    state: &mut CallState,
    echo_risk: bool,
) -> Flow {
    match event {
        VoiceEvent::SessionReady => Flow::Continue,
        VoiceEvent::OutputAudio { pcm, item_id } => {
            // Drop the cancelled response's late audio while suppressing.
            if state.suppress {
                return Flow::Continue;
            }
            // Be tolerant of providers that omit `response.created`: output is
            // definitive proof that the requested response is now active.
            state.mark_response_created();
            // Track the playing item for barge-in truncate: a new item id
            // starts a fresh delivered-samples count.
            if item_id != state.current_item {
                state.current_item = item_id;
                state.item_delivered_samples = 0;
            }
            state.item_delivered_samples += pcm.len() as u64;
            match transport.send_pcm24(&pcm).await {
                Ok(()) => Flow::Continue,
                // A closed transport ends the call; a transient IO error drops
                // this frame (reconnecting the provider wouldn't fix the
                // local device).
                Err(TransportError::Closed) => Flow::End(EndReason::HangUp),
                Err(TransportError::Io(_)) => Flow::Continue,
            }
        }
        VoiceEvent::OutputTextDelta(_) => {
            if !state.suppress {
                state.mark_response_created();
            }
            Flow::Continue
        }
        VoiceEvent::InputTranscriptDelta { .. } => Flow::Continue,
        VoiceEvent::UserSpeechStarted => {
            // Heard-audio estimate for truncate, taken BEFORE clear_playout
            // wipes the queue: delivered minus still-queued = what actually
            // reached the user's ears (the tunnel pacer sends at real-time
            // rate, so "left the queue" ≈ "was played").
            let heard_ms =
                (state.item_delivered_samples / 24).saturating_sub(transport.queued_ms());
            let decision = speech_started_decision(
                transport.queued_ms(),
                state.response_in_flight(),
                echo_risk,
            );
            if decision.clear_local_playback {
                transport.clear_playout();
            }
            if decision.cancel_active_response {
                // Only cancel a LIVE response. The provider bursts a whole turn
                // far faster than realtime, so `response.done` arrives while the
                // audio is still DRAINING from the transport queue; a barge-in
                // during that drain has no in-flight response to cancel, and
                // sending `response.cancel` then only draws a benign
                // `invalid_request_error`. `suppress` guards a live response's
                // late deltas, so it is set exactly when we actually cancel —
                // never lingering to eat the NEXT response's audio/tool calls
                // (that stale-suppress bug refused to hang up on a spoken "end
                // the call").
                if state.response_in_flight() && state.cancel_done_deadline.is_none() {
                    if let Err(e) = sink.cancel_response().await {
                        eprintln!("aura-engine: barge-in cancel send failed: {e}");
                        return Flow::Reconnect;
                    }
                    state.suppress = true;
                    state.mark_cancel_pending();
                    state.ending = false;
                    state.pending_pause = None;
                }
                // Context sync for BOTH live and post-done barge-ins: truncate
                // the item at what the user actually heard so the model's
                // context drops the unheard tail and does not repeat it on the
                // next turn. `current_item` deliberately survives
                // `response.done` for exactly this (it is reset only when a new
                // item's audio starts). No-op for providers without truncate;
                // only sent when audio was in fact cut off (`heard < delivered`,
                // so `audio_end_ms` can never exceed the generated length).
                if let Some(item_id) = state.current_item.take() {
                    let delivered_ms = state.item_delivered_samples / 24;
                    if heard_ms < delivered_ms
                        && sink.truncate_item(&item_id, heard_ms).await.is_err()
                    {
                        return Flow::Reconnect;
                    }
                    state.item_delivered_samples = 0;
                }
            }
            if decision.mark_user_speaking {
                state.user_speaking = true;
            }
            Flow::Continue
        }
        VoiceEvent::UserSpeechStopped => {
            state.user_speaking = false;
            Flow::Continue
        }
        // Tool calls are intercepted in the run loop (they need the dispatch
        // JoinSet + router); this arm keeps the match exhaustive.
        VoiceEvent::ToolCall(_) => Flow::Continue,
        VoiceEvent::ResponseDone { .. } => {
            // A deferred response is marked requested immediately after the
            // preceding response's terminal event. Until its own
            // `response.created` arrives, another late/duplicate done can only
            // belong to the preceding lifecycle. Do not let it clear the new
            // request. A pre-created cancellation is the exception: its done
            // is exactly the terminal event we are waiting for.
            if state.ignore_stale_done_until_created
                && state.response_requested
                && !state.response_created
                && state.cancel_done_deadline.is_none()
            {
                return Flow::Continue;
            }
            // Play the response's trailing partial frame so phrase endings
            // aren't swallowed (REMOTE reframer tail; no-op for LOCAL).
            let _ = transport.flush_output().await;
            state.mark_response_done();
            state.clear_cancel_pending();
            state.suppress = false;
            // Do NOT clear `current_item`/`item_delivered_samples` here: the
            // response is done GENERATING but its audio is still draining from
            // the transport queue, and a barge-in during that drain must be
            // able to truncate this item at the heard position. They are reset
            // when the next item's audio starts (the `OutputAudio` arm) or on
            // reconnect; a barge-in after the queue fully drains is harmless
            // because `heard == delivered` then, so no truncate is sent.
            // If the model asked to hang up, end once its farewell turn is done.
            if state.ending {
                return Flow::End(EndReason::HangUp);
            }
            // If the model asked to pause, enter the paused phase once its
            // "pausing" turn is done.
            if state.pending_pause.is_some() {
                return Flow::Pause;
            }
            if state.deferred_response
                && request_response_after_terminal(sink, state).await.is_err()
            {
                return Flow::Reconnect;
            }
            Flow::Continue
        }
        VoiceEvent::Error(e) => {
            // The code/kind is the whole diagnosis (e.g. a provider that answers
            // a client event with an error). Non-terminal used to reconnect
            // SILENTLY, which hid a reconnect-per-barge-in pattern for months.
            eprintln!("aura-engine: provider error event: {e}");
            if e.is_terminal() {
                Flow::End(EndReason::ProviderFatal)
            } else {
                // An in-band error EVENT on a live stream is informational —
                // the session itself is fine. Live-diagnosed example: a
                // barge-in `response.cancel` racing a response that already
                // finished server-side makes xAI answer `invalid_request_error`;
                // reconnecting on that dropped the whole session (fresh
                // context, language resets) on EVERY late barge-in. Genuine
                // session death still reconnects via the transport paths
                // (stream close / read error / send failure).
                Flow::Continue
            }
        }
    }
}

/// The barge-in unit for a push-to-talk press, mirroring the VAD
/// `UserSpeechStarted` path: heard-position capture BEFORE the queue is wiped,
/// clear playout, cancel + suppress as one block (all tool calls are gated on
/// `suppress`, so a barged-over response can't pause/hang up the call), then
/// `conversation.item.truncate` at the heard position so the model's context
/// drops the unheard tail. Unlike the VAD path there is no decision table: a
/// PTT press is an explicit interruption, so the unit runs unconditionally.
/// `Err(reason)` asks the caller to reconnect (same contract as the other
/// sink-send failures in the loop).
async fn ptt_barge_in(
    transport: &mut dyn AudioTransport,
    sink: &mut dyn VoiceSink,
    state: &mut CallState,
) -> Result<(), &'static str> {
    let heard_ms = (state.item_delivered_samples / 24).saturating_sub(transport.queued_ms());
    transport.clear_playout();
    if state.response_in_flight() && state.cancel_done_deadline.is_none() {
        if sink.cancel_response().await.is_err() {
            return Err("push-to-talk barge-in cancel failed");
        }
        state.suppress = true;
        state.mark_cancel_pending();
        state.ending = false;
        state.pending_pause = None;
    }
    if let Some(item_id) = state.current_item.take() {
        let delivered_ms = state.item_delivered_samples / 24;
        if heard_ms < delivered_ms && sink.truncate_item(&item_id, heard_ms).await.is_err() {
            return Err("push-to-talk barge-in truncate failed");
        }
        state.item_delivered_samples = 0;
    }
    Ok(())
}

async fn cancel_stale_open_ptt(
    sink: &mut dyn VoiceSink,
    state: &mut CallState,
) -> Result<(), aura_voice::VoiceError> {
    state.ptt_open = false;
    state.ptt_open_deadline = None;
    state.ptt_input_samples = 0;
    state.deferred_open_ptt_audio.clear();
    sink.clear_user_audio().await
}

/// Send `response.create` only when no successfully cancelled response is
/// awaiting its terminal event. The user/tool item is already committed, so
/// deferring here preserves it without asking the provider to run two
/// responses concurrently.
async fn request_response_when_ready(
    sink: &mut dyn VoiceSink,
    state: &mut CallState,
) -> Result<(), aura_voice::VoiceError> {
    if state.cancel_done_deadline.is_some() || state.response_in_flight() || state.ptt_open {
        state.deferred_response = true;
        return Ok(());
    }
    if state.deferred_legacy_commit {
        sink.clear_user_audio().await?;
        for pcm in &state.deferred_legacy_audio {
            sink.send_audio(pcm).await?;
        }
        sink.commit_user_turn().await?;
        state.deferred_legacy_commit = false;
        state.deferred_legacy_audio.clear();
    } else {
        sink.request_response().await?;
    }
    state.deferred_response = false;
    state.deferred_items.clear();
    state.deferred_open_ptt_audio.clear();
    state.mark_response_requested();
    Ok(())
}

async fn request_response_after_terminal(
    sink: &mut dyn VoiceSink,
    state: &mut CallState,
) -> Result<(), aura_voice::VoiceError> {
    request_response_when_ready(sink, state).await?;
    if state.response_requested {
        state.ignore_stale_done_until_created = true;
    }
    Ok(())
}

async fn commit_ptt_turn_when_ready(
    sink: &mut dyn VoiceSink,
    state: &mut CallState,
) -> Result<(), aura_voice::VoiceError> {
    if !sink.supports_split_manual_turn() {
        state
            .deferred_legacy_audio
            .append(&mut state.deferred_open_ptt_audio);
        state.deferred_legacy_commit = true;
        state.deferred_response = true;
        return request_response_when_ready(sink, state).await;
    }
    sink.commit_user_audio().await?;
    if state.response_in_flight() || state.cancel_done_deadline.is_some() {
        state
            .deferred_items
            .push(DeferredConversationItem::PttAudio(std::mem::take(
                &mut state.deferred_open_ptt_audio,
            )));
    }
    request_response_when_ready(sink, state).await
}

/// Everything needed to reconstruct the conversation tail after a provider
/// ignores `response.cancel`. Committed items and the open input buffer are
/// intentionally separate so replay preserves every original boundary.
#[derive(Debug)]
struct CancelTimeoutRecovery {
    items: Vec<DeferredConversationItem>,
    open_ptt_audio: Vec<Vec<i16>>,
    open_ptt_samples: u64,
    ptt_open: bool,
    open_ptt_remaining: Duration,
    response_needed: bool,
    legacy_commit_needed: bool,
    legacy_audio: Vec<Vec<i16>>,
}

impl CancelTimeoutRecovery {
    fn take(state: &mut CallState) -> Self {
        Self {
            items: std::mem::take(&mut state.deferred_items),
            open_ptt_audio: std::mem::take(&mut state.deferred_open_ptt_audio),
            open_ptt_samples: state.ptt_input_samples,
            ptt_open: state.ptt_open,
            open_ptt_remaining: state
                .ptt_open_deadline
                .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
                .unwrap_or(PTT_OPEN_TIMEOUT),
            response_needed: state.deferred_response,
            legacy_commit_needed: state.deferred_legacy_commit,
            legacy_audio: std::mem::take(&mut state.deferred_legacy_audio),
        }
    }
}

async fn replay_after_cancel_timeout(
    sink: &mut dyn VoiceSink,
    state: &mut CallState,
    recovery: CancelTimeoutRecovery,
) -> Result<(), aura_voice::VoiceError> {
    for item in recovery.items {
        match item {
            DeferredConversationItem::PttAudio(frames) => {
                for pcm in frames {
                    sink.send_audio(&pcm).await?;
                }
                sink.commit_user_audio().await?;
            }
            DeferredConversationItem::ToolResult { call_id, content } => {
                sink.send_tool_result(call_id.as_deref(), content).await?;
            }
        }
    }

    // Restore an in-progress turn only after every earlier committed item.
    // It remains open and uncommitted; its eventual PttClose owns the single
    // response.create for all deferred conversation items.
    for pcm in &recovery.open_ptt_audio {
        sink.send_audio(pcm).await?;
    }
    state.ptt_open = recovery.ptt_open;
    state.ptt_open_deadline = recovery
        .ptt_open
        .then(|| tokio::time::Instant::now() + recovery.open_ptt_remaining);
    state.ptt_input_samples = recovery.open_ptt_samples;
    state.deferred_open_ptt_audio = recovery.open_ptt_audio;
    state.deferred_response = recovery.response_needed;
    state.deferred_legacy_commit = recovery.legacy_commit_needed;
    state.deferred_legacy_audio = recovery.legacy_audio;
    if recovery.response_needed {
        request_response_when_ready(sink, state).await?;
    }
    Ok(())
}

/// Pend forever when there is no cancellation in flight, otherwise wake at
/// its deadline. Keeping this as a standalone future avoids borrowing call
/// state across the other `tokio::select!` branches.
async fn wait_cancel_done_timeout(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn wait_ptt_open_timeout(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Issue a voice-approval token for the spoken intent, inject it, and spawn the
/// routed tool call into the dispatch `JoinSet`. The result is collected by the
/// loop's `join_next` arm and spoken back to the user.
fn spawn_dispatch(
    router: &Arc<ToolRouter>,
    dispatch: &mut JoinSet<DispatchOutcome>,
    call: VoiceToolCall,
) {
    // Capture the chat-callback metadata BEFORE the args are moved/mutated by
    // approval-token injection — a worker dispatch's result is delivered back
    // into the host chat; read-only queries and control tools are
    // spoken-only and produce no callback.
    let callback = callback_meta(&call.name, &call.args);
    let args = prepare_dispatch_args(router, &call.name, call.args);
    let router = router.clone();
    let call_id = call.call_id;
    let name = call.name;
    dispatch.spawn(async move {
        let result = router
            .handle(ToolCall {
                name,
                arguments: args,
            })
            .await;
        (call_id, result, callback)
    });
}

/// Extract the chat-callback metadata for a tool call, or `None` when the call
/// should not be delivered back into the host chat. Only a genuine
/// `start_agent_task` dispatch produces a chat callback; read-only
/// worker questions and session-control tools are spoken-back only.
fn callback_meta(name: &str, args: &serde_json::Value) -> Option<CallbackMeta> {
    if name != "start_agent_task" {
        return None;
    }
    let user_intent = args.get("user_intent").and_then(|v| v.as_str())?.to_owned();
    if user_intent.trim().is_empty() {
        return None;
    }
    let constraints = args
        .get("constraints")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let project = args
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    Some(CallbackMeta {
        user_intent,
        constraints,
        project,
    })
}

/// Compose a [`TaskResult`] from the dispatch's captured intent and the spoken
/// result, for delivery into the host chat. The voice-approval token is already
/// consumed by the time the task finishes, so the rebuilt envelope carries an
/// empty approval id — it is a record of what ran, not a new dispatch request.
fn build_callback_result(meta: CallbackMeta, speech: &str) -> TaskResult {
    let task_id = {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&meta.user_intent, &mut hasher);
        format!("voice-task-{:016x}", std::hash::Hasher::finish(&hasher))
    };
    let envelope = TaskEnvelope::new(
        meta.user_intent,
        meta.constraints,
        meta.project,
        CallbackMode::default(),
        String::new(),
    );
    TaskResult {
        task_id,
        handoff_state: TaskHandoffState::Accepted,
        speech_update: speech.to_owned(),
        envelope,
    }
}

/// Resolve the `.aura` state root from an `AURA_STATE_DIR`-style value: a
/// non-empty trimmed value wins, else the process cwd. Must stay in lockstep
/// with `aura-server`'s own resolution (`state_root()` in its `main.rs`) — the
/// server's `call-status.json` and this engine's inbox share the same root.
fn state_root_from(raw: Option<&str>) -> std::path::PathBuf {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from("."),
    }
}

/// Decide whether a dispatch's result should be posted back into the host chat,
/// from the sanitized `ToolResponse.content` (a serialized `ProviderTaskResult`).
/// True only for a genuine cold-worker completion: `accepted == true` AND a
/// task id that is NOT orchestrator-owned (`vt-*`). An orchestrator-owned id was
/// run by the live chat session itself (no re-post), and a non-accepted handoff
/// never executed (no phantom post). Missing/garbled fields are treated as
/// "don't deliver" — fail closed on the redundant/false-post side.
fn dispatch_ran_in_cold_worker(content: &serde_json::Value) -> bool {
    let accepted = content
        .get("accepted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let task_id = content
        .get("task_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    accepted && !task_id.starts_with("vt-")
}

/// Best-effort delivery of a finished dispatch back into the host chat. Always
/// fail-open: a host with no callback sink returns `delivered: false` and a
/// delivery error is swallowed — neither must disturb the live call.
///
/// Delivery is SKIPPED when the work did not actually complete in a cold worker:
/// * a non-`accepted` handoff (e.g. the orchestrator-busy hand-back) never ran,
///   so posting it would announce a phantom result; and
/// * an orchestrator-owned task (`vt-*` task id) was executed by the LIVE chat
///   session itself, which already holds the result in its own context — posting
///   it back would be a duplicate. Only a genuine direct-worker completion (a
///   non-`vt-` id the chat session did NOT run) is delivered.
fn deliver_chat_callback(
    host: &Arc<dyn HostAdapter>,
    callback: Option<CallbackMeta>,
    result: &Result<ToolResponse, ToolError>,
    deliveries: &mut JoinSet<()>,
) {
    let (Some(meta), Ok(resp)) = (callback, result) else {
        return;
    };
    if !dispatch_ran_in_cold_worker(&resp.content) {
        return;
    }
    let host = host.clone();
    let task_result = build_callback_result(meta, &resp.speech);
    // Deliver on a detached task: a host sink can be a network WS (OpenClaw) or
    // a subprocess (Hermes/Claude), and awaiting it inline would stall the
    // realtime event loop — freezing mic pump and audio playout. Best-effort
    // and fail-open: a delivery error is logged, never surfaced to the call.
    deliveries.spawn(async move {
        match tokio::time::timeout(
            CALLBACK_DELIVERY_TIMEOUT,
            host.deliver_callback(&task_result),
        )
        .await
        {
            Ok(Err(e)) => eprintln!("aura-engine: chat callback delivery failed: {e}"),
            Err(_) => eprintln!("aura-engine: chat callback delivery timed out"),
            Ok(Ok(_)) => {}
        }
    });
}

/// For dispatch tools, mint a one-time approval token bound to the spoken intent
/// and inject it as `_local_voice_approval_id`. The model never sees the token —
/// the runtime issues it — so the model alone can't fabricate task dispatch.
/// Other tools pass through unchanged.
fn prepare_dispatch_args(
    router: &ToolRouter,
    name: &str,
    mut args: serde_json::Value,
) -> serde_json::Value {
    let intent = match name {
        "start_agent_task" => args
            .get("user_intent")
            .or_else(|| args.get("intent"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        "ask_worker_question" | "ask_claude_question" => args
            .get("question")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        _ => None,
    };
    if let Some(intent) = intent {
        if let Ok(token) = router.issue_task_approval(&intent) {
            if let serde_json::Value::Object(map) = &mut args {
                map.insert(
                    "_local_voice_approval_id".to_owned(),
                    serde_json::Value::String(token),
                );
            }
        }
    }
    args
}

/// Parse a `pause_call_until` tool call's arguments into a [`PauseCondition`].
fn parse_pause_condition(args: &serde_json::Value) -> Option<PauseCondition> {
    match args.get("until").and_then(|v| v.as_str())? {
        "task_complete" => Some(PauseCondition::TaskComplete),
        "timeout" => {
            let secs = args
                .get("seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(60)
                .clamp(1, PAUSE_EVENT_SAFETY_SECS);
            Some(PauseCondition::Timeout(Duration::from_secs(secs)))
        }
        "event" => Some(PauseCondition::Event(
            args.get("event")
                .and_then(|v| v.as_str())
                .unwrap_or("external")
                .to_owned(),
        )),
        _ => None,
    }
}

/// Wait, with the realtime leg collapsed, until the pause condition is met,
/// latching a finished task's result for the resume turn. The
/// dispatch `JoinSet` keeps running while we wait — that is the whole point:
/// the subagent's lifetime is decoupled from the (now-closed) voice session.
async fn run_paused(
    cond: PauseCondition,
    dispatch: &mut JoinSet<DispatchOutcome>,
    host: &Arc<dyn HostAdapter>,
    deliveries: &mut JoinSet<()>,
    transport: &mut dyn AudioTransport,
) -> PausedOutcome {
    let condition = async {
        match cond {
            PauseCondition::TaskComplete => match dispatch.join_next().await {
                Some(Ok((_call_id, result, callback))) => {
                    // The task finished while paused: deliver it into the host chat
                    // now and latch the spoken result for the resume turn.
                    deliver_chat_callback(host, callback, &result, deliveries);
                    Latched::Task(result)
                }
                // Nothing in flight (or it joined-errored) — resume right away.
                _ => Latched::None,
            },
            PauseCondition::Timeout(d) => {
                tokio::time::sleep(d).await;
                Latched::None
            }
            PauseCondition::Event(name) => {
                // v1: no external unpause channel (key/feeder) wired yet —
                // bounded safety wait so we never stay paused forever.
                eprintln!(
                    "aura-engine: paused on event '{name}' — external unpause not wired yet; \
                 resuming after the safety timeout"
                );
                tokio::time::sleep(Duration::from_secs(PAUSE_EVENT_SAFETY_SECS)).await;
                Latched::None
            }
        }
    };
    tokio::pin!(condition);
    loop {
        tokio::select! {
            latched = &mut condition => return PausedOutcome::Resume(latched),
            input = transport.recv_input() => match input {
                Some(_) => {}
                None => return PausedOutcome::HangUp,
            },
        }
    }
}

/// Build the session config for a MID-CALL reconnect: the original composed
/// instructions + the most recent dialogue lines + a strict continuity
/// directive. The reconnected session must feel like the same uninterrupted
/// conversation — same topic, same language, no fresh greeting.
fn build_reconnect_cfg(
    cfg: &VoiceSessionConfig,
    transcript: &InCallTranscript,
) -> VoiceSessionConfig {
    let mut instructions = cfg.instructions.clone();
    let digest = transcript.digest();
    if !digest.is_empty() {
        instructions.push_str(
            "\n\n[Connection recovery: the realtime link dropped for a moment and was \
             re-established MID-CALL. The conversation so far (most recent lines):\n",
        );
        instructions.push_str(&digest);
        instructions.push_str("\n]");
    }
    instructions.push_str(
        "\n\n[Continue the SAME conversation seamlessly: do NOT greet again, do NOT \
         restart or re-introduce yourself, do NOT change language — keep speaking \
         the language the conversation above is in, and pick up exactly where it \
         left off. The developer may not even have noticed the blip.]",
    );
    VoiceSessionConfig {
        instructions,
        cold_start_kick: false,
        ..cfg.clone()
    }
}

/// Build the session config used to bring the realtime leg back after a pause:
/// the original composed instructions + a digest of the pre-pause dialogue +
/// the latched task result + an instruction to announce the resume and relay
/// the result. Cold-start is off — the loop drives `request_response` itself.
fn build_resume_cfg(
    cfg: &VoiceSessionConfig,
    transcript: &InCallTranscript,
    latched: &Latched,
) -> VoiceSessionConfig {
    let mut instructions = cfg.instructions.clone();
    let digest = transcript.digest();
    if !digest.is_empty() {
        instructions.push_str(
            "\n\n[The voice call was paused and is now resuming. The developer's recent lines \
             before the pause:\n",
        );
        instructions.push_str(&digest);
        instructions.push_str("\n]");
    }
    match latched {
        Latched::Task(Ok(resp)) => {
            instructions.push_str(
                "\n\n[The task you dispatched finished while the call was paused. Result to relay: ",
            );
            instructions.push_str(&resp.speech);
            instructions.push_str(
                "\nResume now: briefly tell the developer the call is back, then give them this \
                 result naturally.]",
            );
        }
        Latched::Task(Err(_)) => instructions.push_str(
            "\n\n[The task you dispatched did not complete while the call was paused. Resume: tell \
             the developer the call is back and that the task didn't finish.]",
        ),
        Latched::None => instructions.push_str(
            "\n\n[The pause condition was met. Resume: briefly tell the developer the call is back \
             and continue the conversation.]",
        ),
    }
    VoiceSessionConfig {
        instructions,
        cold_start_kick: false,
        ..cfg.clone()
    }
}

/// Accumulates the call's spoken transcript — BOTH the developer's finalized
/// utterances (`[developer]`) and Aura's spoken-output transcript (`[aura]`,
/// from the realtime model's own output-audio-transcript deltas, committed one
/// line per turn on `ResponseDone`) — interleaved in conversation order. It
/// feeds the pause-resume digest (recent lines) and the post-call recap (the
/// whole conversation), which the host then SUMMARIZES into the chat.
/// NOT a separate STT — these are the realtime model's own transcript events.
#[derive(Default)]
struct InCallTranscript {
    lines: Vec<String>,
    user_buf: String,
    assistant_buf: String,
}

impl InCallTranscript {
    fn push_line(&mut self, line: String) {
        self.lines.push(line);
        // Bound memory; keep enough for the post-call recap, far more
        // than the pause digest's last-N window.
        if self.lines.len() > RECAP_MAX_LINES {
            self.lines.remove(0);
        }
    }

    fn observe(&mut self, event: &VoiceEvent) {
        match event {
            VoiceEvent::InputTranscriptDelta { delta, final_ } => {
                if *final_ {
                    // The provider's `...transcription.completed` event carries
                    // the FULL final transcript (a final_=true delta). Prefer it
                    // over the accumulated incremental deltas — appending would
                    // DOUBLE the line. Fall back to the buffer if it is empty.
                    let completed = delta.trim();
                    let line = if !completed.is_empty() {
                        completed.to_owned()
                    } else {
                        self.user_buf.trim().to_owned()
                    };
                    if !line.is_empty() {
                        self.push_line(format!("[developer] {line}"));
                    }
                    self.user_buf.clear();
                } else {
                    self.user_buf.push_str(delta);
                    cap_buf_tail(&mut self.user_buf);
                }
            }
            // Aura's spoken-output transcript streams as text deltas; accumulate
            // and commit ONE `[aura]` line per turn when the response finishes.
            VoiceEvent::OutputTextDelta(delta) => {
                self.assistant_buf.push_str(delta);
                cap_buf_tail(&mut self.assistant_buf);
            }
            VoiceEvent::ResponseDone { .. } => {
                let line = self.assistant_buf.trim().to_owned();
                if !line.is_empty() {
                    self.push_line(format!("[aura] {line}"));
                }
                self.assistant_buf.clear();
            }
            _ => {}
        }
    }

    /// The pause-resume digest: only the most recent lines.
    fn digest(&self) -> String {
        let start = self.lines.len().saturating_sub(PAUSE_DIGEST_MAX_LINES);
        self.lines[start..].join("\n")
    }

    /// The full accumulated transcript for the post-call recap.
    fn recap(&self) -> String {
        self.lines.join("\n")
    }
}

/// Keep only the tail of an over-long un-finalized transcript buffer, cut on a
/// char boundary (bounds a turn that never finalizes).
fn cap_buf_tail(buf: &mut String) {
    if buf.len() > USER_BUF_CAP {
        let mut cut = buf.len() - USER_BUF_CAP;
        while !buf.is_char_boundary(cut) {
            cut += 1;
        }
        buf.drain(..cut);
    }
}

/// Reconnect with bounded backoff (250 ms / 1 s / 3 s), re-establishing the
/// session with the same instructions and NO cold-start replay. Returns the
/// new sink/stream, or `None` if all attempts failed or hit a terminal error.
async fn reconnect(
    provider: &Arc<dyn VoiceProvider>,
    cfg: &VoiceSessionConfig,
    why: &str,
) -> Option<(Box<dyn VoiceSink>, Box<dyn VoiceStream>)> {
    // One diagnostic line per reconnect episode. `why` distinguishes a genuine
    // mid-call drop from the planned pause-resume: a healthy, pause-free call
    // never logs a drop, so a reconnect-per-barge-in pattern (a provider
    // rejecting an event we sent, e.g. an experimental truncate) is visible.
    eprintln!("aura-engine: {why}; reconnecting the realtime session.");
    const BACKOFF_MS: [u64; 3] = [250, 1_000, 3_000];
    for delay_ms in BACKOFF_MS {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        match provider.connect(cfg).await {
            Ok(pair) => return Some(pair),
            // Terminal mid-reconnect (e.g. balance exhausted): stop retrying.
            Err(e) if e.is_terminal() => return None,
            Err(_) => continue,
        }
    }
    None
}

/// Await the next feeder digest, or pend forever when there is no feeder (so
/// the `select!` branch simply never fires).
async fn next_feeder_digest(feeder: Option<&Arc<dyn AmbientFeeder>>) -> String {
    match feeder {
        Some(f) => match f.next_digest().await {
            Some(d) => d,
            None => std::future::pending::<String>().await,
        },
        None => std::future::pending::<String>().await,
    }
}

/// Reconnect without silently losing a currently open manual turn.
async fn reconnect_with_state(
    provider: &Arc<dyn VoiceProvider>,
    cfg: &VoiceSessionConfig,
    why: &str,
    state: &mut CallState,
) -> Option<(Box<dyn VoiceSink>, Box<dyn VoiceStream>)> {
    let recovery = CancelTimeoutRecovery::take(state);
    let (mut sink, stream) = reconnect(provider, cfg, why).await?;
    state.on_reconnect();
    if replay_after_cancel_timeout(sink.as_mut(), state, recovery)
        .await
        .is_err()
    {
        return None;
    }
    Some((sink, stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_hosts::ClaudeAdapter;
    use aura_voice::{AudioCaps, VoiceError};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct SinkLog {
        audio: Vec<Vec<i16>>,
        cancels: usize,
        truncates: Vec<(String, u64)>,
        injects: Vec<String>,
        tool_results: usize,
        requests: usize,
        commits: usize,
        clears: usize,
        closed: bool,
        actions: Vec<SinkAction>,
    }

    #[derive(Debug, PartialEq)]
    enum SinkAction {
        Audio(i16),
        Cancel,
        ToolResult(Option<String>, serde_json::Value),
        Request,
        Commit,
        Clear,
    }

    struct FakeSink {
        log: Arc<Mutex<SinkLog>>,
    }

    struct LegacySink {
        log: Arc<Mutex<SinkLog>>,
    }

    #[async_trait]
    impl VoiceSink for LegacySink {
        async fn send_audio(&mut self, pcm16: &[i16]) -> Result<(), VoiceError> {
            self.log.lock().unwrap().audio.push(pcm16.to_vec());
            Ok(())
        }
        async fn cancel_response(&mut self) -> Result<(), VoiceError> {
            Ok(())
        }
        async fn send_tool_result(
            &mut self,
            _call_id: Option<&str>,
            _output: serde_json::Value,
        ) -> Result<(), VoiceError> {
            Ok(())
        }
        async fn inject_system_context(&mut self, _text: &str) -> Result<(), VoiceError> {
            Ok(())
        }
        async fn request_response(&mut self) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.requests += 1;
            log.actions.push(SinkAction::Request);
            Ok(())
        }
        async fn commit_user_turn(&mut self) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.commits += 1;
            log.actions.push(SinkAction::Commit);
            Ok(())
        }
        async fn close(&mut self) -> Result<(), VoiceError> {
            Ok(())
        }
    }

    #[async_trait]
    impl VoiceSink for FakeSink {
        fn supports_split_manual_turn(&self) -> bool {
            true
        }

        async fn send_audio(&mut self, pcm16: &[i16]) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.audio.push(pcm16.to_vec());
            log.actions.push(SinkAction::Audio(
                pcm16.first().copied().unwrap_or_default(),
            ));
            Ok(())
        }
        async fn cancel_response(&mut self) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.cancels += 1;
            log.actions.push(SinkAction::Cancel);
            Ok(())
        }
        async fn truncate_item(
            &mut self,
            item_id: &str,
            audio_end_ms: u64,
        ) -> Result<(), VoiceError> {
            self.log
                .lock()
                .unwrap()
                .truncates
                .push((item_id.to_owned(), audio_end_ms));
            Ok(())
        }
        async fn send_tool_result(
            &mut self,
            call_id: Option<&str>,
            output: serde_json::Value,
        ) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.tool_results += 1;
            log.actions
                .push(SinkAction::ToolResult(call_id.map(str::to_owned), output));
            Ok(())
        }
        async fn inject_system_context(&mut self, text: &str) -> Result<(), VoiceError> {
            self.log.lock().unwrap().injects.push(text.to_owned());
            Ok(())
        }
        async fn request_response(&mut self) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.requests += 1;
            log.actions.push(SinkAction::Request);
            Ok(())
        }
        async fn commit_user_audio(&mut self) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.commits += 1;
            log.actions.push(SinkAction::Commit);
            Ok(())
        }
        async fn clear_user_audio(&mut self) -> Result<(), VoiceError> {
            let mut log = self.log.lock().unwrap();
            log.clears += 1;
            log.actions.push(SinkAction::Clear);
            Ok(())
        }
        async fn close(&mut self) -> Result<(), VoiceError> {
            self.log.lock().unwrap().closed = true;
            Ok(())
        }
    }

    enum End {
        Closed,
        Pending,
    }

    struct FakeStream {
        events: VecDeque<VoiceEvent>,
        end: End,
        cancel_done_after_commit: Option<Arc<Mutex<SinkLog>>>,
    }

    #[async_trait]
    impl VoiceStream for FakeStream {
        async fn next_event(&mut self) -> Option<Result<VoiceEvent, VoiceError>> {
            if let Some(ev) = self.events.pop_front() {
                return Some(Ok(ev));
            }
            if self.cancel_done_after_commit.is_some() {
                loop {
                    let ready = {
                        let log = self.cancel_done_after_commit.as_ref().unwrap();
                        let log = log.lock().unwrap();
                        log.cancels == 1 && log.commits == 1
                    };
                    if ready {
                        let log = self.cancel_done_after_commit.take().unwrap();
                        assert_eq!(
                            log.lock().unwrap().requests,
                            0,
                            "response.create was sent before cancelled response.done"
                        );
                        return Some(Ok(VoiceEvent::ResponseDone { input_tokens: None }));
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            match self.end {
                End::Closed => None,
                End::Pending => std::future::pending().await,
            }
        }
    }

    enum Script {
        Ok { events: Vec<VoiceEvent>, end: End },
        CancelDoneAfterCommit,
        Err,
    }

    struct FakeProvider {
        scripts: Mutex<VecDeque<Script>>,
        sink_log: Arc<Mutex<SinkLog>>,
        connects: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl VoiceProvider for FakeProvider {
        fn supports_manual_turn_detection(&self) -> bool {
            true
        }

        async fn connect_with_manual_turn_detection(
            &self,
            cfg: &VoiceSessionConfig,
            _manual_turn_detection: bool,
        ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError> {
            self.connect(cfg).await
        }

        fn model_id(&self) -> &str {
            "fake-model"
        }
        fn default_voice(&self) -> &str {
            "fake-voice"
        }
        fn audio_caps(&self) -> AudioCaps {
            AudioCaps {
                server_vad: true,
                input_sample_rate_hz: 24_000,
                output_sample_rate_hz: 24_000,
            }
        }
        async fn connect(
            &self,
            _cfg: &VoiceSessionConfig,
        ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            match self.scripts.lock().unwrap().pop_front() {
                Some(Script::Ok { events, end }) => Ok((
                    Box::new(FakeSink {
                        log: self.sink_log.clone(),
                    }),
                    Box::new(FakeStream {
                        events: events.into(),
                        end,
                        cancel_done_after_commit: None,
                    }),
                )),
                Some(Script::CancelDoneAfterCommit) => Ok((
                    Box::new(FakeSink {
                        log: self.sink_log.clone(),
                    }),
                    Box::new(FakeStream {
                        events: VecDeque::new(),
                        end: End::Pending,
                        cancel_done_after_commit: Some(self.sink_log.clone()),
                    }),
                )),
                Some(Script::Err) | None => {
                    Err(VoiceError::Transport("fake connect failure".to_owned()))
                }
            }
        }
    }

    struct FakeTransport {
        input: Mutex<VecDeque<TransportInput>>,
        hangup_when_empty: bool,
        hangup_after_request: Option<Arc<Mutex<SinkLog>>>,
        played: Arc<Mutex<Vec<Vec<i16>>>>,
        cleared: Arc<AtomicUsize>,
        queued_ms: AtomicU64,
    }

    #[async_trait]
    impl AudioTransport for FakeTransport {
        async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
            loop {
                match self.recv_input().await? {
                    TransportInput::Audio(pcm) => return Some(pcm),
                    TransportInput::Control(_) => continue,
                }
            }
        }
        async fn recv_input(&mut self) -> Option<TransportInput> {
            if let Some(input) = self.input.lock().unwrap().pop_front() {
                return Some(input);
            }
            if self.hangup_when_empty {
                return None;
            }
            if let Some(log) = &self.hangup_after_request {
                loop {
                    if log.lock().unwrap().requests > 0 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
            std::future::pending().await
        }
        async fn send_pcm24(&mut self, pcm: &[i16]) -> Result<(), TransportError> {
            self.played.lock().unwrap().push(pcm.to_vec());
            self.queued_ms.fetch_add(20, Ordering::SeqCst);
            Ok(())
        }
        fn clear_playout(&self) {
            self.queued_ms.store(0, Ordering::SeqCst);
            self.cleared.fetch_add(1, Ordering::SeqCst);
        }
        fn queued_ms(&self) -> u64 {
            self.queued_ms.load(Ordering::SeqCst)
        }
    }

    fn cfg() -> VoiceSessionConfig {
        VoiceSessionConfig {
            instructions: "persona".to_owned(),
            voice: "v".to_owned(),
            tools: serde_json::json!([]),
            latency_target_ms: 800,
            temperature: None,
            end_of_turn_timeout_ms: None,
            output_speed: None,
            cold_start_kick: false,
            transcription_language: None,
        }
    }

    fn host() -> Arc<dyn HostAdapter> {
        Arc::new(ClaudeAdapter::new("/tmp/aura-engine-test"))
    }

    struct Harness {
        provider: Arc<FakeProvider>,
        transport: Box<FakeTransport>,
        sink_log: Arc<Mutex<SinkLog>>,
        played: Arc<Mutex<Vec<Vec<i16>>>>,
        cleared: Arc<AtomicUsize>,
        connects: Arc<AtomicUsize>,
    }

    fn harness(scripts: Vec<Script>, mic: Vec<Vec<i16>>, hangup_when_empty: bool) -> Harness {
        harness_input(
            scripts,
            mic.into_iter().map(TransportInput::Audio).collect(),
            hangup_when_empty,
        )
    }

    fn harness_input(
        scripts: Vec<Script>,
        input: Vec<TransportInput>,
        hangup_when_empty: bool,
    ) -> Harness {
        let sink_log = Arc::new(Mutex::new(SinkLog::default()));
        let played = Arc::new(Mutex::new(Vec::new()));
        let cleared = Arc::new(AtomicUsize::new(0));
        let connects = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FakeProvider {
            scripts: Mutex::new(scripts.into()),
            sink_log: sink_log.clone(),
            connects: connects.clone(),
        });
        let transport = Box::new(FakeTransport {
            input: Mutex::new(input.into()),
            hangup_when_empty,
            hangup_after_request: None,
            played: played.clone(),
            cleared: cleared.clone(),
            queued_ms: AtomicU64::new(0),
        });
        Harness {
            provider,
            transport,
            sink_log,
            played,
            cleared,
            connects,
        }
    }

    fn ptt_cfg() -> VoiceSessionConfig {
        cfg()
    }

    #[tokio::test]
    async fn push_to_talk_close_commits_user_turn() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![1; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttClose),
            ],
            true,
        );

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            ptt_cfg(),
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.audio, vec![vec![1; MIN_PTT_COMMIT_SAMPLES as usize]]);
        assert_eq!(log.commits, 1);
        assert_eq!(log.requests, 1);
    }

    #[tokio::test]
    async fn next_ptt_open_cancels_committed_response_before_audio() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![1; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttClose),
                // The commit sent response.create, but no response.created or
                // audio delta has arrived yet. This press must still cancel it.
                TransportInput::Control(TransportControl::PttOpen),
            ],
            true,
        );

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            ptt_cfg(),
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.commits, 1);
        assert_eq!(log.requests, 1);
        assert_eq!(log.cancels, 1);
        assert!(log.truncates.is_empty());
    }

    #[tokio::test]
    async fn ptt_commit_waits_for_cancelled_response_done_before_requesting() {
        let mut h = harness_input(
            vec![Script::CancelDoneAfterCommit],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![1; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttClose),
            ],
            false,
        );
        h.transport.hangup_after_request = Some(h.sink_log.clone());
        let mut config = ptt_cfg();
        // Makes the initial response cancellable before its first provider
        // event. The fake stream withholds ResponseDone until it observes that
        // PttClose committed the >=100 ms user turn.
        config.cold_start_kick = true;

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            config,
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.cancels, 1);
        assert_eq!(log.commits, 1);
        assert_eq!(
            log.requests, 1,
            "response.create must be deferred until the cancelled response is done"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_done_timeout_reconnects_and_replays_committed_ptt_turn() {
        let mut h = harness_input(
            vec![
                Script::Ok {
                    events: vec![],
                    end: End::Pending,
                },
                Script::Ok {
                    events: vec![VoiceEvent::SessionReady],
                    end: End::Pending,
                },
            ],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![7; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttClose),
            ],
            false,
        );
        h.transport.hangup_after_request = Some(h.sink_log.clone());
        let mut config = ptt_cfg();
        config.cold_start_kick = true;

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            config,
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(h.connects.load(Ordering::SeqCst), 2);
        assert_eq!(log.cancels, 1);
        assert_eq!(log.commits, 2, "original commit plus replay commit");
        assert_eq!(log.requests, 1);
        assert_eq!(
            log.audio,
            vec![
                vec![7; MIN_PTT_COMMIT_SAMPLES as usize],
                vec![7; MIN_PTT_COMMIT_SAMPLES as usize],
            ],
            "the replacement session must receive the retained user audio"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_timeout_replay_preserves_two_committed_ptt_boundaries() {
        let mut h = harness_input(
            vec![
                Script::Ok {
                    events: vec![],
                    end: End::Pending,
                },
                Script::Ok {
                    events: vec![VoiceEvent::SessionReady],
                    end: End::Pending,
                },
            ],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![1; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttClose),
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![2; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttClose),
            ],
            false,
        );
        h.transport.hangup_after_request = Some(h.sink_log.clone());
        let mut config = ptt_cfg();
        config.cold_start_kick = true;

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            config,
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.cancels, 1, "the second open must not cancel again");
        assert_eq!(log.requests, 1);
        assert_eq!(log.commits, 4, "two original commits plus two replays");
        assert_eq!(
            &log.actions[log.actions.len() - 5..],
            &[
                SinkAction::Audio(1),
                SinkAction::Commit,
                SinkAction::Audio(2),
                SinkAction::Commit,
                SinkAction::Request,
            ],
            "recovery must replay two items, not one flattened audio buffer"
        );
    }

    #[tokio::test]
    async fn repeated_ptt_open_while_cancel_pending_is_idempotent() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![3; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttClose),
            ],
            true,
        );
        let mut config = ptt_cfg();
        config.cold_start_kick = true;

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            config,
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.cancels, 1);
        assert_eq!(log.clears, 1);
        assert_eq!(log.commits, 1);
        assert_eq!(log.audio.len(), 1, "duplicate open must not erase audio");
    }

    #[tokio::test]
    async fn timeout_replay_keeps_committed_items_separate_from_open_ptt() {
        let log = Arc::new(Mutex::new(SinkLog::default()));
        let mut sink = FakeSink { log: log.clone() };
        let mut state = CallState::default();
        let tool_content = serde_json::json!({ "ok": true });
        let recovery = CancelTimeoutRecovery {
            items: vec![
                DeferredConversationItem::PttAudio(vec![vec![4; 2_400]]),
                DeferredConversationItem::ToolResult {
                    call_id: Some("tool-1".to_owned()),
                    content: tool_content.clone(),
                },
            ],
            open_ptt_audio: vec![vec![5; 1_200]],
            open_ptt_samples: 1_200,
            ptt_open: true,
            open_ptt_remaining: PTT_OPEN_TIMEOUT,
            response_needed: true,
            legacy_commit_needed: false,
            legacy_audio: Vec::new(),
        };

        replay_after_cancel_timeout(&mut sink, &mut state, recovery)
            .await
            .unwrap();

        assert!(state.ptt_open);
        assert_eq!(state.ptt_input_samples, 1_200);
        assert!(state.deferred_response);
        assert_eq!(log.lock().unwrap().requests, 0);
        assert_eq!(
            log.lock().unwrap().actions,
            vec![
                SinkAction::Audio(4),
                SinkAction::Commit,
                SinkAction::ToolResult(Some("tool-1".to_owned()), tool_content),
                SinkAction::Audio(5),
            ]
        );

        // The still-open turn remains a distinct item. Closing it commits that
        // item once, then emits the one response.create deferred for the tail.
        state.ptt_open = false;
        commit_ptt_turn_when_ready(&mut sink, &mut state)
            .await
            .unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.commits, 2);
        assert_eq!(log.requests, 1);
        assert_eq!(
            &log.actions[log.actions.len() - 2..],
            &[SinkAction::Commit, SinkAction::Request]
        );
    }

    #[tokio::test]
    async fn first_ptt_open_cancels_cold_start_before_audio() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![TransportInput::Control(TransportControl::PttOpen)],
            true,
        );
        let mut config = ptt_cfg();
        config.cold_start_kick = true;

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            config,
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.cancels, 1);
        assert!(log.truncates.is_empty());
    }

    #[tokio::test]
    async fn ptt_controls_are_ignored_in_a_vad_session() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Control(TransportControl::PttClose),
            ],
            true,
        );

        let _ = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.commits, 0);
        assert_eq!(log.clears, 0);
    }

    #[tokio::test]
    async fn push_to_talk_close_with_too_little_audio_is_dropped_not_committed() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![1; 100]),
                TransportInput::Control(TransportControl::PttClose),
            ],
            true,
        );

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            ptt_cfg(),
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.commits, 0);
        assert_eq!(log.requests, 0);
        // One clear from PttOpen (stale-input hygiene) + one from the
        // under-minimum PttClose.
        assert_eq!(log.clears, 2);
    }

    #[tokio::test]
    async fn push_to_talk_cancel_clears_streamed_audio_without_committing() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![
                TransportInput::Control(TransportControl::PttOpen),
                TransportInput::Audio(vec![1; MIN_PTT_COMMIT_SAMPLES as usize]),
                TransportInput::Control(TransportControl::PttCancel),
            ],
            true,
        );

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            ptt_cfg(),
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        // Even a turn ABOVE the commit minimum is dropped on cancel — the
        // user discarded it; a commit would answer a discarded message.
        assert_eq!(log.commits, 0);
        assert_eq!(log.requests, 0);
        // One clear from PttOpen (hygiene) + one from the cancel.
        assert_eq!(log.clears, 2);
    }

    #[tokio::test]
    async fn push_to_talk_open_clears_stale_provider_input() {
        let h = harness_input(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![TransportInput::Control(TransportControl::PttOpen)],
            true,
        );

        let _ = CallSession::run_with_manual_turn_detection(
            h.transport,
            h.provider,
            host(),
            None,
            ptt_cfg(),
        )
        .await
        .unwrap();

        let log = h.sink_log.lock().unwrap();
        // A press with no in-flight response still clears the provider input
        // buffer, dropping any stray tail from a prior/discarded turn.
        assert_eq!(log.clears, 1);
        assert_eq!(log.cancels, 0);
    }

    #[tokio::test]
    async fn ptt_barge_in_runs_the_full_cancel_suppress_truncate_unit() {
        let sink_log = Arc::new(Mutex::new(SinkLog::default()));
        let mut sink = FakeSink {
            log: sink_log.clone(),
        };
        let cleared = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport {
            input: Mutex::new(VecDeque::new()),
            hangup_when_empty: true,
            hangup_after_request: None,
            played: Arc::new(Mutex::new(Vec::new())),
            cleared: cleared.clone(),
            // 400 ms still queued (unheard) at press time.
            queued_ms: AtomicU64::new(400),
        };
        let mut state = CallState {
            response_created: true,
            ending: true,
            pending_pause: Some(PauseCondition::TaskComplete),
            current_item: Some("item-7".to_owned()),
            item_delivered_samples: 24_000, // 1000 ms delivered
            ..CallState::default()
        };

        ptt_barge_in(&mut transport, &mut sink, &mut state)
            .await
            .unwrap();

        let log = sink_log.lock().unwrap();
        assert_eq!(log.cancels, 1);
        assert!(state.suppress, "cancel and suppress are one block");
        // Heard = delivered (1000 ms) minus queued (400 ms), captured BEFORE
        // clear_playout wiped the queue.
        assert_eq!(log.truncates, vec![("item-7".to_owned(), 600)]);
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
        assert_eq!(state.current_item, None);
        assert_eq!(state.item_delivered_samples, 0);
        assert!(
            !state.ending,
            "cancelled hangup intent must not survive barge-in"
        );
        assert!(
            state.pending_pause.is_none(),
            "cancelled pause intent must not survive barge-in"
        );
    }

    #[tokio::test]
    async fn ptt_barge_in_cancels_requested_response_before_first_audio() {
        let sink_log = Arc::new(Mutex::new(SinkLog::default()));
        let mut sink = FakeSink {
            log: sink_log.clone(),
        };
        let cleared = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport {
            input: Mutex::new(VecDeque::new()),
            hangup_when_empty: true,
            hangup_after_request: None,
            played: Arc::new(Mutex::new(Vec::new())),
            cleared: cleared.clone(),
            queued_ms: AtomicU64::new(0),
        };
        let mut state = CallState {
            response_requested: true,
            ..CallState::default()
        };

        ptt_barge_in(&mut transport, &mut sink, &mut state)
            .await
            .unwrap();

        let log = sink_log.lock().unwrap();
        assert_eq!(log.cancels, 1);
        assert!(state.suppress, "cancel and suppress remain one block");
        assert!(
            log.truncates.is_empty(),
            "no audio means no truncate target"
        );
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn response_created_is_active_before_first_audio() {
        let sink_log = Arc::new(Mutex::new(SinkLog::default()));
        let mut sink = FakeSink {
            log: sink_log.clone(),
        };
        let cleared = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport {
            input: Mutex::new(VecDeque::new()),
            hangup_when_empty: true,
            hangup_after_request: None,
            played: Arc::new(Mutex::new(Vec::new())),
            cleared,
            queued_ms: AtomicU64::new(0),
        };
        let mut state = CallState {
            response_requested: true,
            ..CallState::default()
        };

        state.mark_response_created();
        assert!(!state.response_requested);
        assert!(state.response_created);

        ptt_barge_in(&mut transport, &mut sink, &mut state)
            .await
            .unwrap();
        assert_eq!(sink_log.lock().unwrap().cancels, 1);
        assert!(state.suppress);
    }

    #[tokio::test]
    async fn ptt_barge_in_without_active_response_only_clears_playout() {
        let sink_log = Arc::new(Mutex::new(SinkLog::default()));
        let mut sink = FakeSink {
            log: sink_log.clone(),
        };
        let cleared = Arc::new(AtomicUsize::new(0));
        let mut transport = FakeTransport {
            input: Mutex::new(VecDeque::new()),
            hangup_when_empty: true,
            hangup_after_request: None,
            played: Arc::new(Mutex::new(Vec::new())),
            cleared: cleared.clone(),
            queued_ms: AtomicU64::new(0),
        };
        let mut state = CallState::default();

        ptt_barge_in(&mut transport, &mut sink, &mut state)
            .await
            .unwrap();

        let log = sink_log.lock().unwrap();
        assert_eq!(log.cancels, 0);
        assert!(!state.suppress);
        assert!(log.truncates.is_empty());
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn plays_output_audio_until_terminal_error() {
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::OutputAudio {
                        pcm: vec![1, 2, 3],
                        item_id: None,
                    },
                    VoiceEvent::OutputAudio {
                        pcm: vec![4, 5],
                        item_id: None,
                    },
                    VoiceEvent::Error(VoiceError::BalanceZero),
                ],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
        assert_eq!(*h.played.lock().unwrap(), vec![vec![1, 2, 3], vec![4, 5]]);
        assert_eq!(h.connects.load(Ordering::SeqCst), 1);
        assert!(h.sink_log.lock().unwrap().closed);
    }

    #[tokio::test]
    async fn forwards_mic_until_hangup() {
        let h = harness(
            vec![Script::Ok {
                events: vec![VoiceEvent::SessionReady],
                end: End::Pending,
            }],
            vec![vec![9, 9], vec![8, 8]],
            true,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::HangUp);
        assert_eq!(
            h.sink_log.lock().unwrap().audio,
            vec![vec![9, 9], vec![8, 8]]
        );
    }

    #[tokio::test]
    async fn barge_in_clears_cancels_and_suppresses_late_audio() {
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::OutputAudio {
                        pcm: vec![1],
                        item_id: None,
                    },
                    VoiceEvent::UserSpeechStarted,
                    VoiceEvent::OutputAudio {
                        pcm: vec![2],
                        item_id: None,
                    }, // late delta of cancelled response — dropped
                    VoiceEvent::ResponseDone { input_tokens: None },
                    VoiceEvent::OutputAudio {
                        pcm: vec![3],
                        item_id: None,
                    }, // suppress cleared — played
                    VoiceEvent::Error(VoiceError::BalanceZero),
                ],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
        // [2] dropped while suppressing; [1] and [3] played.
        assert_eq!(*h.played.lock().unwrap(), vec![vec![1], vec![3]]);
        assert_eq!(h.sink_log.lock().unwrap().cancels, 1);
        assert_eq!(h.cleared.load(Ordering::SeqCst), 1);
        // No item_id was reported → nothing to truncate.
        assert!(h.sink_log.lock().unwrap().truncates.is_empty());
    }

    #[tokio::test]
    async fn barge_in_truncates_cancelled_item_at_heard_position() {
        // 960 samples @24k = 40 ms delivered for item "it_1"; the fake
        // transport reports 20 ms still queued at the barge-in, so the user
        // heard 40 - 20 = 20 ms. The engine must cancel AND truncate the item
        // at 20 ms (dropping the unheard tail from the model's context).
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::OutputAudio {
                        pcm: vec![0; 960],
                        item_id: Some("it_1".into()),
                    },
                    VoiceEvent::UserSpeechStarted,
                    VoiceEvent::Error(VoiceError::BalanceZero),
                ],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.cancels, 1);
        assert_eq!(log.truncates, vec![("it_1".to_owned(), 20)]);
    }

    #[tokio::test]
    async fn post_done_barge_in_truncates_still_playing_item() {
        // The REAL fix for "the model repeats a line after I interrupt": the
        // provider bursts a long answer, `response.done` arrives while the
        // audio is still draining, and a barge-in during that drain must
        // truncate the item at the heard position (so the model's context drops
        // the unheard tail). 960 samples = 40 ms delivered; the fake reports
        // 20 ms still queued at barge-in → heard = 20 ms. Post-done there is no
        // live response, so no `response.cancel` is sent.
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::OutputAudio {
                        pcm: vec![0; 960],
                        item_id: Some("it_1".into()),
                    },
                    VoiceEvent::ResponseDone { input_tokens: None }, // done generating; audio still queued
                    VoiceEvent::UserSpeechStarted,                   // barge-in during drain
                    VoiceEvent::Error(VoiceError::BalanceZero),
                ],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
        let log = h.sink_log.lock().unwrap();
        assert_eq!(log.truncates, vec![("it_1".to_owned(), 20)]);
        assert_eq!(log.cancels, 0, "post-done barge-in must not cancel");
    }

    #[tokio::test]
    async fn barge_in_suppresses_cancelled_control_tool_call() {
        // A response the developer barged-in over carries an `end_voice_session`
        // tool call. Cancel + suppress as a unit: that call must be
        // DROPPED — the call must NOT hang up against the developer's just-
        // expressed intent to keep talking. If the suppressed control tool were
        // honored, `state.ending` would be set and the outcome would be HangUp;
        // instead the call continues and ends only on the later fatal error.
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::OutputAudio {
                        pcm: vec![1],
                        item_id: None,
                    }, // assistant speaking -> barge-in cancels
                    VoiceEvent::UserSpeechStarted, // barge-in: sets suppress
                    VoiceEvent::ToolCall(VoiceToolCall {
                        call_id: Some("e1".into()),
                        name: "end_voice_session".into(),
                        args: serde_json::json!({}),
                    }), // from the cancelled response -> must be ignored
                    VoiceEvent::ResponseDone { input_tokens: None }, // clears suppress; must NOT end
                    VoiceEvent::Error(VoiceError::BalanceZero),      // deterministic terminal end
                ],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnects_after_stream_close_then_fatal() {
        let h = harness(
            vec![
                Script::Ok {
                    events: vec![VoiceEvent::SessionReady],
                    end: End::Closed, // stream closes -> triggers reconnect
                },
                Script::Ok {
                    events: vec![VoiceEvent::Error(VoiceError::BalanceZero)],
                    end: End::Pending,
                },
            ],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
        assert_eq!(h.connects.load(Ordering::SeqCst), 2); // initial + one reconnect
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_exhausted_after_three_failures() {
        let h = harness(
            vec![
                Script::Ok {
                    events: vec![VoiceEvent::SessionReady],
                    end: End::Closed,
                },
                Script::Err,
                Script::Err,
                Script::Err,
            ],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ReconnectExhausted);
        assert_eq!(h.connects.load(Ordering::SeqCst), 4); // initial + 3 failed reconnects
    }

    #[tokio::test(start_paused = true)]
    async fn pause_until_timeout_collapses_session_and_resumes() {
        // The model pauses the call; the engine collapses the realtime leg
        // (closes the sink), waits out the timeout, then reconnects to resume.
        let h = harness(
            vec![
                Script::Ok {
                    events: vec![
                        VoiceEvent::SessionReady,
                        VoiceEvent::ToolCall(VoiceToolCall {
                            call_id: Some("p1".into()),
                            name: "pause_call_until".into(),
                            args: serde_json::json!({"until": "timeout", "seconds": 5}),
                        }),
                        VoiceEvent::ResponseDone { input_tokens: None },
                    ],
                    end: End::Pending,
                },
                // The resumed session immediately hits a terminal error to end
                // the test deterministically.
                Script::Ok {
                    events: vec![VoiceEvent::Error(VoiceError::BalanceZero)],
                    end: End::Pending,
                },
            ],
            vec![],
            false,
        );
        let sink_log = h.sink_log.clone();
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
        // Connected twice: the initial session + the resume after the pause.
        assert_eq!(h.connects.load(Ordering::SeqCst), 2);
        let log = sink_log.lock().unwrap();
        // The tool result must not create an overlapping response before the
        // tool-call response terminates. Pausing collapses that session, so
        // only the resumed session's kick is requested.
        assert_eq!(log.requests, 1, "requests = {}", log.requests);
        assert!(log.closed, "the collapsed session's sink was closed");
    }

    #[tokio::test]
    async fn post_done_barge_in_does_not_suppress_the_next_response() {
        // Live-diagnosed "the call refuses to hang up": the developer speaks
        // over the still-playing TAIL of a response whose `response.done` was
        // already processed. Post-done there is no live response, so we neither
        // cancel nor set suppress — the model's NEXT response (here: obeying the
        // spoken "end the call" with an `end_voice_session` tool call) must be
        // honored, so the call ends with HangUp, not the fallback fatal error.
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::OutputAudio {
                        pcm: vec![1],
                        item_id: None,
                    }, // queues 20 ms in the fake transport
                    VoiceEvent::ResponseDone { input_tokens: None }, // response over; tail still queued
                    VoiceEvent::UserSpeechStarted, // "end the call" spoken over the tail
                    VoiceEvent::ToolCall(VoiceToolCall {
                        call_id: Some("e1".into()),
                        name: "end_voice_session".into(),
                        args: serde_json::json!({}),
                    }), // the NEW response obeying the request -> must EXECUTE
                    VoiceEvent::ResponseDone { input_tokens: None }, // farewell turn done -> hang up
                    VoiceEvent::Error(VoiceError::BalanceZero),      // only reached on regression
                ],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::HangUp);
        // No live response to cancel post-done → no `response.cancel` (which
        // would only draw a benign invalid_request_error from the provider).
        assert_eq!(h.sink_log.lock().unwrap().cancels, 0);
    }

    #[tokio::test]
    async fn nonterminal_error_event_does_not_reconnect() {
        // Live-diagnosed regression guard: a barge-in `response.cancel` racing
        // an already-finished response makes the provider answer an in-band
        // `invalid_request_error` event. That must NOT drop the session — the
        // call continues on the SAME connection (connects stays 1) and audio
        // after the error still plays.
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::Error(VoiceError::Protocol("invalid_request_error".into())),
                    VoiceEvent::OutputAudio {
                        pcm: vec![7],
                        item_id: None,
                    },
                    VoiceEvent::Error(VoiceError::BalanceZero), // deterministic end
                ],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let outcome = CallSession::run(h.transport, h.provider, host(), None, cfg())
            .await
            .unwrap();
        assert_eq!(outcome.reason, EndReason::ProviderFatal);
        assert_eq!(h.connects.load(Ordering::SeqCst), 1, "no reconnect");
        assert_eq!(*h.played.lock().unwrap(), vec![vec![7]]);
    }

    #[tokio::test]
    async fn parse_pause_condition_variants() {
        assert!(matches!(
            parse_pause_condition(&serde_json::json!({"until": "task_complete"})),
            Some(PauseCondition::TaskComplete)
        ));
        assert!(matches!(
            parse_pause_condition(&serde_json::json!({"until": "timeout", "seconds": 30})),
            Some(PauseCondition::Timeout(d)) if d == Duration::from_secs(30)
        ));
        assert!(matches!(
            parse_pause_condition(&serde_json::json!({"until": "event", "event": "ci_done"})),
            Some(PauseCondition::Event(name)) if name == "ci_done"
        ));
        assert!(parse_pause_condition(&serde_json::json!({"until": "nonsense"})).is_none());
    }

    #[tokio::test]
    async fn tool_followup_waits_for_done_and_stale_done_cannot_clear_it() {
        let h = harness(vec![], vec![], false);
        let mut sink = FakeSink {
            log: h.sink_log.clone(),
        };
        let mut transport = h.transport;
        let mut state = CallState {
            response_created: true,
            ..CallState::default()
        };

        let tool_result = serde_json::json!({"ok": true});
        state
            .deferred_items
            .push(DeferredConversationItem::ToolResult {
                call_id: Some("tool-1".to_owned()),
                content: tool_result.clone(),
            });
        sink.send_tool_result(Some("tool-1"), tool_result)
            .await
            .unwrap();
        request_response_when_ready(&mut sink, &mut state)
            .await
            .unwrap();
        assert!(state.deferred_response);
        assert_eq!(state.deferred_items.len(), 1);
        assert_eq!(h.sink_log.lock().unwrap().requests, 0);

        assert!(matches!(
            handle_event(
                VoiceEvent::ResponseDone { input_tokens: None },
                transport.as_mut(),
                &mut sink,
                &mut state,
                false,
            )
            .await,
            Flow::Continue
        ));
        assert!(state.response_requested);
        assert_eq!(h.sink_log.lock().unwrap().requests, 1);
        assert!(state.deferred_items.is_empty());

        // A duplicate terminal event from the old response arrives before the
        // new response.created. It must not clear the new request.
        let _ = handle_event(
            VoiceEvent::ResponseDone { input_tokens: None },
            transport.as_mut(),
            &mut sink,
            &mut state,
            false,
        )
        .await;
        assert!(state.response_requested);
        assert_eq!(h.sink_log.lock().unwrap().requests, 1);

        state.mark_response_created();
        let _ = handle_event(
            VoiceEvent::ResponseDone { input_tokens: None },
            transport.as_mut(),
            &mut sink,
            &mut state,
            false,
        )
        .await;
        assert!(!state.response_in_flight());
    }

    #[tokio::test]
    async fn ordinary_request_accepts_legitimate_done_before_created() {
        let h = harness(vec![], vec![], false);
        let mut sink = FakeSink {
            log: h.sink_log.clone(),
        };
        let mut transport = h.transport;
        let mut state = CallState {
            response_requested: true,
            ..CallState::default()
        };

        let flow = handle_event(
            VoiceEvent::ResponseDone { input_tokens: None },
            transport.as_mut(),
            &mut sink,
            &mut state,
            false,
        )
        .await;
        assert!(matches!(flow, Flow::Continue));
        assert!(!state.response_in_flight());
        assert!(!state.ignore_stale_done_until_created);
    }

    #[tokio::test]
    async fn legacy_manual_commit_is_deferred_and_never_double_requests() {
        let log = Arc::new(Mutex::new(SinkLog::default()));
        let mut sink = LegacySink { log: log.clone() };
        let mut state = CallState {
            response_created: true,
            deferred_open_ptt_audio: vec![vec![1; 480]],
            ..CallState::default()
        };

        commit_ptt_turn_when_ready(&mut sink, &mut state)
            .await
            .unwrap();
        assert!(state.deferred_legacy_commit);
        assert_eq!(log.lock().unwrap().commits, 0);

        state.deferred_open_ptt_audio = vec![vec![2; 480]];
        commit_ptt_turn_when_ready(&mut sink, &mut state)
            .await
            .unwrap();

        state.mark_response_done();
        request_response_when_ready(&mut sink, &mut state)
            .await
            .unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.commits, 1);
        assert_eq!(
            log.requests, 0,
            "combined legacy commit owns response.create"
        );
        assert_eq!(log.actions, vec![SinkAction::Commit]);
        assert_eq!(log.audio, vec![vec![1; 480], vec![2; 480]]);
    }

    #[tokio::test]
    async fn paused_phase_discards_input_and_observes_hangup() {
        let mut dispatch: JoinSet<DispatchOutcome> = JoinSet::new();
        let mut deliveries = JoinSet::new();
        let mut h = harness_input(vec![], vec![TransportInput::Audio(vec![7; 480])], true);
        let outcome = run_paused(
            PauseCondition::Timeout(Duration::from_secs(60)),
            &mut dispatch,
            &host(),
            &mut deliveries,
            h.transport.as_mut(),
        )
        .await;
        assert!(matches!(outcome, PausedOutcome::HangUp));
        assert!(h.transport.input.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_ptt_close_is_cancelled_without_committing() {
        let log = Arc::new(Mutex::new(SinkLog::default()));
        let mut sink = FakeSink { log: log.clone() };
        let mut state = CallState {
            ptt_open: true,
            ptt_open_deadline: Some(tokio::time::Instant::now()),
            ptt_input_samples: 480,
            deferred_open_ptt_audio: vec![vec![5; 480]],
            ..CallState::default()
        };
        cancel_stale_open_ptt(&mut sink, &mut state).await.unwrap();
        assert!(!state.ptt_open);
        assert!(state.ptt_open_deadline.is_none());
        assert_eq!(state.ptt_input_samples, 0);
        assert!(state.deferred_open_ptt_audio.is_empty());
        let log = log.lock().unwrap();
        assert_eq!(log.clears, 1);
        assert_eq!(log.commits, 0);
        assert_eq!(log.requests, 0);
    }

    #[test]
    fn manual_audio_is_retained_only_for_an_open_turn() {
        let mut state = CallState {
            cancel_done_deadline: Some(tokio::time::Instant::now()),
            ..CallState::default()
        };
        state.track_manual_audio(&[1; 480]);
        assert!(state.deferred_open_ptt_audio.is_empty());
        assert_eq!(state.ptt_input_samples, 0);

        state.ptt_open = true;
        state.track_manual_audio(&[2; 480]);
        assert_eq!(state.deferred_open_ptt_audio, vec![vec![2; 480]]);
        assert_eq!(state.ptt_input_samples, 480);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_replays_and_restores_open_ptt_turn() {
        let h = harness(
            vec![Script::Ok {
                events: vec![],
                end: End::Pending,
            }],
            vec![],
            false,
        );
        let mut state = CallState {
            ptt_open: true,
            ptt_open_deadline: Some(tokio::time::Instant::now() + Duration::from_secs(60)),
            ptt_input_samples: 960,
            deferred_open_ptt_audio: vec![vec![3; 480], vec![4; 480]],
            ending: true,
            pending_pause: Some(PauseCondition::TaskComplete),
            ..CallState::default()
        };
        let provider: Arc<dyn VoiceProvider> = h.provider.clone();
        let pair = reconnect_with_state(&provider, &cfg(), "test reconnect", &mut state).await;
        assert!(pair.is_some());
        assert!(state.ptt_open);
        assert_eq!(state.ptt_input_samples, 960);
        assert_eq!(
            state.deferred_open_ptt_audio,
            vec![vec![3; 480], vec![4; 480]]
        );
        assert!(!state.ending);
        assert!(state.pending_pause.is_none());
        assert_eq!(
            h.sink_log.lock().unwrap().audio,
            vec![vec![3; 480], vec![4; 480]]
        );
    }

    #[tokio::test]
    async fn run_paused_task_complete_with_no_inflight_returns_immediately() {
        let mut dispatch: JoinSet<DispatchOutcome> = JoinSet::new();
        let mut deliveries = JoinSet::new();
        let mut h = harness(vec![], vec![], false);
        let latched = run_paused(
            PauseCondition::TaskComplete,
            &mut dispatch,
            &host(),
            &mut deliveries,
            h.transport.as_mut(),
        )
        .await;
        assert!(matches!(latched, PausedOutcome::Resume(Latched::None)));
    }

    #[test]
    fn transcript_digest_is_recent_window_recap_is_full() {
        let mut t = InCallTranscript::default();
        for i in 0..(PAUSE_DIGEST_MAX_LINES + 5) {
            t.observe(&VoiceEvent::InputTranscriptDelta {
                delta: format!("line {i}"),
                final_: true,
            });
        }
        // Pause digest keeps only the most recent window…
        let digest_lines = t.digest().lines().count();
        assert_eq!(digest_lines, PAUSE_DIGEST_MAX_LINES);
        assert!(t
            .digest()
            .contains(&format!("line {}", PAUSE_DIGEST_MAX_LINES + 4)));
        assert!(!t.digest().contains("line 0"));
        // …while the post-call recap keeps everything (within RECAP_MAX_LINES).
        let recap_lines = t.recap().lines().count();
        assert_eq!(recap_lines, PAUSE_DIGEST_MAX_LINES + 5);
        assert!(t.recap().contains("line 0"));
        // Only finalized lines are recorded.
        let mut partial = InCallTranscript::default();
        partial.observe(&VoiceEvent::InputTranscriptDelta {
            delta: "incomplete".into(),
            final_: false,
        });
        assert!(partial.recap().is_empty());
    }

    #[test]
    fn transcript_records_both_sides_interleaved() {
        let mut t = InCallTranscript::default();
        // Aura greets: output-text deltas accumulate, committed on ResponseDone.
        t.observe(&VoiceEvent::OutputTextDelta("Hi, ".into()));
        t.observe(&VoiceEvent::OutputTextDelta("how can I help?".into()));
        t.observe(&VoiceEvent::ResponseDone { input_tokens: None });
        // Developer answers.
        t.observe(&VoiceEvent::InputTranscriptDelta {
            delta: "fix the parser".into(),
            final_: true,
        });
        // Aura's next turn.
        t.observe(&VoiceEvent::OutputTextDelta("On it.".into()));
        t.observe(&VoiceEvent::ResponseDone { input_tokens: None });
        assert_eq!(
            t.recap(),
            "[aura] Hi, how can I help?\n[developer] fix the parser\n[aura] On it."
        );
        // A ResponseDone with no spoken output adds no line.
        t.observe(&VoiceEvent::ResponseDone { input_tokens: None });
        assert_eq!(t.recap().lines().count(), 3);
    }

    #[test]
    fn build_resume_cfg_weaves_digest_and_result() {
        let base = cfg();
        let mut transcript = InCallTranscript::default();
        transcript.observe(&VoiceEvent::InputTranscriptDelta {
            delta: "refactor the parser".into(),
            final_: true,
        });
        let resp = ToolResponse {
            name: "start_agent_task".into(),
            content: serde_json::json!({}),
            speech: "Renamed the parser module and all tests pass.".into(),
        };
        let resumed = build_resume_cfg(&base, &transcript, &Latched::Task(Ok(resp)));
        assert!(!resumed.cold_start_kick);
        assert!(resumed
            .instructions
            .contains("[developer] refactor the parser"));
        assert!(resumed
            .instructions
            .contains("Renamed the parser module and all tests pass."));
    }

    #[test]
    fn callback_meta_only_for_worker_dispatch() {
        // A genuine worker dispatch with a real intent produces callback meta.
        let meta = callback_meta(
            "start_agent_task",
            &serde_json::json!({
                "user_intent": "rename the parser module",
                "constraints": ["keep tests green", 42],
                "project": "aura",
            }),
        )
        .expect("start_agent_task yields callback meta");
        assert_eq!(meta.user_intent, "rename the parser module");
        assert_eq!(meta.constraints, vec!["keep tests green".to_owned()]);
        assert_eq!(meta.project, "aura");

        // Read-only questions and control tools are spoken-back only.
        assert!(callback_meta(
            "ask_worker_question",
            &serde_json::json!({ "question": "what does foo do?" })
        )
        .is_none());
        assert!(callback_meta("end_voice_session", &serde_json::json!({})).is_none());
        // An empty/whitespace intent is not a deliverable callback.
        assert!(callback_meta(
            "start_agent_task",
            &serde_json::json!({ "user_intent": "  " })
        )
        .is_none());
    }

    #[test]
    fn build_callback_result_recites_intent_and_result() {
        let meta = CallbackMeta {
            user_intent: "rename the parser module".to_owned(),
            constraints: vec!["keep tests green".to_owned()],
            project: "aura".to_owned(),
        };
        let result = build_callback_result(meta, "Renamed it and all tests pass.");
        // The envelope recites the developer's full ask (Hermes's "Request:").
        assert_eq!(result.envelope.user_intent, "rename the parser module");
        assert_eq!(result.speech_update, "Renamed it and all tests pass.");
        assert_eq!(result.handoff_state, TaskHandoffState::Accepted);
        // task_id is opaque, stable per intent, and never empty.
        assert!(result.task_id.starts_with("voice-task-"));
        assert!(result.accepted());
    }

    #[test]
    fn state_root_resolution_env_wins_else_cwd() {
        assert_eq!(
            state_root_from(Some("/home/u/.aura-state")),
            std::path::PathBuf::from("/home/u/.aura-state")
        );
        assert_eq!(state_root_from(Some("  ")), std::path::PathBuf::from("."));
        assert_eq!(state_root_from(None), std::path::PathBuf::from("."));
    }

    #[test]
    fn only_cold_worker_completions_deliver_to_chat() {
        // A genuine direct-worker completion (accepted, non-`vt-` id) delivers.
        assert!(dispatch_ran_in_cold_worker(&serde_json::json!({
            "task_id": "worker-7", "accepted": true
        })));
        // An orchestrator-owned task (`vt-*`) ran in the live chat session already
        // — do NOT re-post.
        assert!(!dispatch_ran_in_cold_worker(&serde_json::json!({
            "task_id": "vt-abc-1-2", "accepted": true
        })));
        // A non-accepted handoff (orchestrator-busy) never executed — no phantom post.
        assert!(!dispatch_ran_in_cold_worker(&serde_json::json!({
            "task_id": "worker-7", "accepted": false
        })));
        // Missing fields fail closed.
        assert!(!dispatch_ran_in_cold_worker(&serde_json::json!({})));
    }

    #[tokio::test]
    async fn deliver_chat_callback_is_a_noop_without_meta_or_on_error() {
        let host = host();
        let mut deliveries = JoinSet::new();
        // No callback meta (e.g. a read-only query) -> nothing delivered, no panic.
        deliver_chat_callback(
            &host,
            None,
            &Ok(ToolResponse {
                name: "ask_worker_question".into(),
                content: serde_json::json!({}),
                speech: "answered".into(),
            }),
            &mut deliveries,
        );
        // A failed dispatch -> nothing delivered even if meta is present.
        deliver_chat_callback(
            &host,
            Some(CallbackMeta {
                user_intent: "do the thing".to_owned(),
                constraints: vec![],
                project: String::new(),
            }),
            &Err(ToolError::UnknownTool("boom".into())),
            &mut deliveries,
        );
        // Let any (here, none) spawned delivery task run before the test ends.
        tokio::task::yield_now().await;
    }
}
