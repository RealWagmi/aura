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
    VoiceEvent, VoiceProvider, VoiceSessionConfig, VoiceSink, VoiceStream, VoiceToolCall,
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
    /// Whether a model response is currently in flight.
    assistant_active: bool,
    /// Whether the user is currently speaking (server-VAD).
    user_speaking: bool,
    /// The model called `end_voice_session`; hang up once its farewell turn
    /// finishes (next `ResponseDone`).
    ending: bool,
    /// The model called `pause_call_until`; enter the paused phase once its
    /// "pausing" turn finishes (next `ResponseDone`).
    pending_pause: Option<PauseCondition>,
}

impl CallState {
    /// Reset after a reconnect: the new session has no in-flight response and
    /// no pending cancel to suppress.
    fn on_reconnect(&mut self) {
        self.suppress = false;
        self.assistant_active = false;
        self.user_speaking = false;
    }
}

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

impl CallSession {
    /// Run a call to completion. Connects the provider, then drives the
    /// single-task loop until the transport closes (hang-up), the provider is
    /// terminally unavailable, or reconnect attempts are exhausted.
    pub async fn run(
        mut transport: Box<dyn AudioTransport>,
        provider: Arc<dyn VoiceProvider>,
        host: Arc<dyn HostAdapter>,
        feeder: Option<Arc<dyn AmbientFeeder>>,
        cfg: VoiceSessionConfig,
    ) -> Result<CallOutcome, EngineError> {
        // Open-speaker echo suppression defaults off — headsets stay
        // interruptible; detecting an open speaker is a follow-up.
        let echo_risk = false;

        // The reconnect handshake re-establishes the session with the same
        // composed instructions (no re-inject) but must NOT replay the
        // cold-start greeting mid-call.
        let reconnect_cfg = VoiceSessionConfig {
            cold_start_kick: false,
            ..cfg.clone()
        };

        let (mut sink, mut stream) = provider.connect(&cfg).await?;
        // Call-duration metric: wall-clock from a connected session
        // to hang-up. Logged at the end; no content, safe to emit.
        let call_started = std::time::Instant::now();
        let mut state = CallState::default();

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
        // Accumulates the developer's spoken lines so a resumed-from-pause
        // session doesn't lose the conversation context.
        let mut transcript = InCallTranscript::default();

        let outcome = 'call: loop {
            // --- ACTIVE phase: the realtime session is live. ---
            let transition = loop {
                tokio::select! {
                    mic = transport.recv_pcm24() => match mic {
                        Some(pcm) => {
                            if sink.send_audio(&pcm).await.is_err() {
                                match reconnect(&provider, &reconnect_cfg).await {
                                    Some((s, st)) => { sink = s; stream = st; state.on_reconnect(); }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                }
                            }
                        }
                        None => break Transition::Ended(EndReason::HangUp),
                    },
                    ev = stream.next_event() => match ev {
                        None => match reconnect(&provider, &reconnect_cfg).await {
                            Some((s, st)) => { sink = s; stream = st; state.on_reconnect(); }
                            None => break Transition::Ended(EndReason::ReconnectExhausted),
                        },
                        Some(Err(e)) => {
                            if e.is_terminal() {
                                break Transition::Ended(EndReason::ProviderFatal);
                            }
                            match reconnect(&provider, &reconnect_cfg).await {
                                Some((s, st)) => { sink = s; stream = st; state.on_reconnect(); }
                                None => break Transition::Ended(EndReason::ReconnectExhausted),
                            }
                        }
                        Some(Ok(VoiceEvent::ToolCall(call))) => {
                            // Cancel + suppress as a unit: a tool call that
                            // belongs to a response the user barged-in over (cancelled)
                            // must NOT be acted on — including the control tools. Acting
                            // on a cancelled `end_voice_session`/`pause_call_until` would
                            // hang up or pause the call against the developer's just-
                            // expressed intent to keep talking. Drop it; the next
                            // ResponseDone clears `suppress`.
                            if state.suppress {
                                // Suppressed: ignore this cancelled response's tool call.
                            } else if call.name == END_VOICE_SESSION_TOOL {
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
                                let _ = sink.send_tool_result(call.call_id.as_deref(), content).await;
                                let _ = sink.request_response().await;
                            } else {
                                spawn_dispatch(&router, &mut dispatch, call);
                            }
                        }
                        Some(Ok(event)) => {
                            transcript.observe(&event);
                            match handle_event(event, transport.as_mut(), sink.as_mut(), &mut state, echo_risk).await {
                                Flow::Continue => {}
                                Flow::End(reason) => break Transition::Ended(reason),
                                Flow::Pause => {
                                    let cond = state.pending_pause.take().unwrap_or(PauseCondition::TaskComplete);
                                    break Transition::Pause(cond);
                                }
                                Flow::Reconnect => match reconnect(&provider, &reconnect_cfg).await {
                                    Some((s, st)) => { sink = s; stream = st; state.on_reconnect(); }
                                    None => break Transition::Ended(EndReason::ReconnectExhausted),
                                },
                            }
                        }
                    },
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
                            let _ = sink.send_tool_result(call_id.as_deref(), content).await;
                            let _ = sink.request_response().await;
                            // Universal callback seam: a completed
                            // worker dispatch is also delivered back into the
                            // host chat, not only spoken. Best-effort/fail-open.
                            deliver_chat_callback(&host, callback, &result);
                        }
                    }
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
                    let latched = run_paused(cond, &mut dispatch, &host).await;

                    // --- RESUME: bring the realtime leg back with the pre-pause
                    // dialogue digest + the latched result, then speak it. ---
                    let resume_cfg = build_resume_cfg(&cfg, &transcript, &latched);
                    match reconnect(&provider, &resume_cfg).await {
                        Some((s, st)) => {
                            sink = s;
                            stream = st;
                            state.on_reconnect();
                            let _ = sink.request_response().await;
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
        VoiceEvent::OutputAudio(pcm) => {
            // Drop the cancelled response's late audio while suppressing.
            if state.suppress {
                return Flow::Continue;
            }
            state.assistant_active = true;
            match transport.send_pcm24(&pcm).await {
                Ok(()) => Flow::Continue,
                // A closed transport ends the call; a transient IO error drops
                // this frame (reconnecting the provider wouldn't fix the
                // local device).
                Err(TransportError::Closed) => Flow::End(EndReason::HangUp),
                Err(TransportError::Io(_)) => Flow::Continue,
            }
        }
        VoiceEvent::OutputTextDelta(_) | VoiceEvent::InputTranscriptDelta { .. } => Flow::Continue,
        VoiceEvent::UserSpeechStarted => {
            let decision =
                speech_started_decision(transport.queued_ms(), state.assistant_active, echo_risk);
            if decision.clear_local_playback {
                transport.clear_playout();
            }
            if decision.cancel_active_response {
                // cancel + suppress move together: a cancel without the guard
                // lets late deltas overlap the user's speech.
                if sink.cancel_response().await.is_err() {
                    return Flow::Reconnect;
                }
                state.suppress = true;
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
            // Play the response's trailing partial frame so phrase endings
            // aren't swallowed (REMOTE reframer tail; no-op for LOCAL).
            let _ = transport.flush_output().await;
            state.assistant_active = false;
            state.suppress = false;
            // If the model asked to hang up, end once its farewell turn is done.
            if state.ending {
                return Flow::End(EndReason::HangUp);
            }
            // If the model asked to pause, enter the paused phase once its
            // "pausing" turn is done.
            if state.pending_pause.is_some() {
                return Flow::Pause;
            }
            Flow::Continue
        }
        VoiceEvent::Error(e) => {
            if e.is_terminal() {
                Flow::End(EndReason::ProviderFatal)
            } else {
                Flow::Reconnect
            }
        }
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
    tokio::spawn(async move {
        if let Err(e) = host.deliver_callback(&task_result).await {
            eprintln!("aura-engine: chat callback delivery failed: {e}");
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
) -> Latched {
    match cond {
        PauseCondition::TaskComplete => match dispatch.join_next().await {
            Some(Ok((_call_id, result, callback))) => {
                // The task finished while paused: deliver it into the host chat
                // now and latch the spoken result for the resume turn.
                deliver_chat_callback(host, callback, &result);
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
) -> Option<(Box<dyn VoiceSink>, Box<dyn VoiceStream>)> {
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
        injects: Vec<String>,
        tool_results: usize,
        requests: usize,
        closed: bool,
    }

    struct FakeSink {
        log: Arc<Mutex<SinkLog>>,
    }

    #[async_trait]
    impl VoiceSink for FakeSink {
        async fn send_audio(&mut self, pcm16: &[i16]) -> Result<(), VoiceError> {
            self.log.lock().unwrap().audio.push(pcm16.to_vec());
            Ok(())
        }
        async fn cancel_response(&mut self) -> Result<(), VoiceError> {
            self.log.lock().unwrap().cancels += 1;
            Ok(())
        }
        async fn send_tool_result(
            &mut self,
            _call_id: Option<&str>,
            _output: serde_json::Value,
        ) -> Result<(), VoiceError> {
            self.log.lock().unwrap().tool_results += 1;
            Ok(())
        }
        async fn inject_system_context(&mut self, text: &str) -> Result<(), VoiceError> {
            self.log.lock().unwrap().injects.push(text.to_owned());
            Ok(())
        }
        async fn request_response(&mut self) -> Result<(), VoiceError> {
            self.log.lock().unwrap().requests += 1;
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
    }

    #[async_trait]
    impl VoiceStream for FakeStream {
        async fn next_event(&mut self) -> Option<Result<VoiceEvent, VoiceError>> {
            if let Some(ev) = self.events.pop_front() {
                return Some(Ok(ev));
            }
            match self.end {
                End::Closed => None,
                End::Pending => std::future::pending().await,
            }
        }
    }

    enum Script {
        Ok { events: Vec<VoiceEvent>, end: End },
        Err,
    }

    struct FakeProvider {
        scripts: Mutex<VecDeque<Script>>,
        sink_log: Arc<Mutex<SinkLog>>,
        connects: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl VoiceProvider for FakeProvider {
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
                    }),
                )),
                Some(Script::Err) | None => {
                    Err(VoiceError::Transport("fake connect failure".to_owned()))
                }
            }
        }
    }

    struct FakeTransport {
        mic: Mutex<VecDeque<Vec<i16>>>,
        hangup_when_empty: bool,
        played: Arc<Mutex<Vec<Vec<i16>>>>,
        cleared: Arc<AtomicUsize>,
        queued_ms: AtomicU64,
    }

    #[async_trait]
    impl AudioTransport for FakeTransport {
        async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
            if let Some(frame) = self.mic.lock().unwrap().pop_front() {
                return Some(frame);
            }
            if self.hangup_when_empty {
                return None;
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
            mic: Mutex::new(mic.into()),
            hangup_when_empty,
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

    #[tokio::test]
    async fn plays_output_audio_until_terminal_error() {
        let h = harness(
            vec![Script::Ok {
                events: vec![
                    VoiceEvent::SessionReady,
                    VoiceEvent::OutputAudio(vec![1, 2, 3]),
                    VoiceEvent::OutputAudio(vec![4, 5]),
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
                    VoiceEvent::OutputAudio(vec![1]),
                    VoiceEvent::UserSpeechStarted,
                    VoiceEvent::OutputAudio(vec![2]), // late delta of cancelled response — dropped
                    VoiceEvent::ResponseDone { input_tokens: None },
                    VoiceEvent::OutputAudio(vec![3]), // suppress cleared — played
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
                    VoiceEvent::OutputAudio(vec![1]), // assistant speaking -> barge-in cancels
                    VoiceEvent::UserSpeechStarted,    // barge-in: sets suppress
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
        // The pause ack + the resume kick both asked for a response.
        assert!(log.requests >= 2, "requests = {}", log.requests);
        assert!(log.closed, "the collapsed session's sink was closed");
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
    async fn run_paused_task_complete_with_no_inflight_returns_immediately() {
        let mut dispatch: JoinSet<DispatchOutcome> = JoinSet::new();
        let latched = run_paused(PauseCondition::TaskComplete, &mut dispatch, &host()).await;
        assert!(matches!(latched, Latched::None));
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
        // No callback meta (e.g. a read-only query) -> nothing delivered, no panic.
        deliver_chat_callback(
            &host,
            None,
            &Ok(ToolResponse {
                name: "ask_worker_question".into(),
                content: serde_json::json!({}),
                speech: "answered".into(),
            }),
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
        );
        // Let any (here, none) spawned delivery task run before the test ends.
        tokio::task::yield_now().await;
    }
}
