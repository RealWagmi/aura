//! `aura-voice` — the realtime voice "brain" and the swappable provider
//! seam.
//!
//! This crate exports the provider-neutral contract the engine talks to —
//! [`VoiceProvider`], the split [`VoiceSink`] / [`VoiceStream`] pair, and
//! the [`VoiceEvent`] stream — plus two DIRECT implementations:
//! `XaiRealtimeProvider` (`wss://api.x.ai/v1/realtime`) and
//! `OpenAiRealtimeProvider` (`wss://api.openai.com/v1/realtime`, GA protocol).
//! Both are BYOK with a per-provider host-pin; they share one WS/event
//! plumbing (`realtime_ws`) because both speak GA-style event names.
//!
//! ## The sink/stream split
//!
//! A realtime call has two concurrent tasks over one WebSocket: a
//! mic-pump that *writes* audio, and an event-loop that *reads* server
//! events. They cannot both hold `&mut self` to one connection, so
//! [`VoiceProvider::connect`] returns a **split** pair built over the
//! underlying `SplitSink`/`SplitStream`. Barge-in `cancel` is a method on
//! [`VoiceSink`], but the *decision* is made by the event-loop (which owns
//! the [`VoiceStream`]); the engine bridges the two with an mpsc command
//! channel (see `aura-engine::barge_in`).
//!
//! ## Audio contract (fixed)
//!
//! PCM16, mono, little-endian, 24 000 Hz, both directions. At the trait
//! boundary a frame is `&[i16]` — never base64, never bytes. The base64
//! framing the xAI wire wants is hidden inside the sink impl. `AudioCaps`
//! advertises the rate so a future provider with a different rate flips
//! one field rather than touching the engine.

use async_trait::async_trait;

mod byok;
pub mod compose;
pub mod openai;
mod realtime_ws;
pub mod wire;
pub mod xai;

pub use openai::OpenAiRealtimeProvider;
pub use xai::XaiRealtimeProvider;

/// Capabilities a provider advertises so the engine can drive it without
/// knowing which model is behind the seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioCaps {
    /// `true` when the provider runs server-side VAD (voice-activity
    /// detection) and segments turns itself — then the engine does NOT
    /// send a commit / `response.create` per turn. A future client-VAD
    /// provider sets this `false` and the engine drives `request_response`.
    pub server_vad: bool,
    /// Input (mic → model) sample rate in Hz. v1 = 24 000.
    pub input_sample_rate_hz: u32,
    /// Output (model → speaker) sample rate in Hz. v1 = 24 000.
    pub output_sample_rate_hz: u32,
}

/// A tool/function call surfaced by the model mid-call. The args are the
/// raw provider JSON; the engine routes them through the voice-approval
/// boundary before any host dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceToolCall {
    /// Provider-assigned id, echoed back on `send_tool_result`.
    pub call_id: Option<String>,
    /// Function name the model asked to invoke.
    pub name: String,
    /// Function arguments as provider JSON.
    pub args: serde_json::Value,
}

/// One decoded event from the provider's realtime stream. The runtime
/// above [`VoiceStream::next_event`] never sees provider JSON — the wire
/// demux happens inside the stream impl.
#[derive(Clone, Debug)]
pub enum VoiceEvent {
    /// Session negotiated; safe to start pumping audio.
    SessionReady,
    /// Decoded model audio, already base64-decoded to PCM16 @ 24k.
    /// `item_id` is the provider's conversation-item id for this response's
    /// audio (when reported) — the engine tracks it so a barge-in can
    /// [`VoiceSink::truncate_item`] the cancelled item at the position the
    /// user actually heard.
    OutputAudio {
        pcm: Vec<i16>,
        item_id: Option<String>,
    },
    /// Incremental assistant text (for transcript/UI; not the audio path).
    OutputTextDelta(String),
    /// User transcript, arriving inline over the same realtime WS (xAI: the
    /// model's own events; OpenAI: its built-in transcription sidecar). Feeds
    /// the transcript/recap TEXT only — never the audio path.
    InputTranscriptDelta { delta: String, final_: bool },
    /// Provider server-VAD detected the user started speaking (barge-in
    /// trigger).
    UserSpeechStarted,
    /// Provider server-VAD detected the user stopped speaking.
    UserSpeechStopped,
    /// The model requested a tool call.
    ToolCall(VoiceToolCall),
    /// The provider accepted a `response.create` request and started a model
    /// response. This can arrive before the first audio delta, which lets the
    /// engine cancel a cold-start or manually committed response immediately.
    ResponseCreated,
    /// A model response finished; carries usage if the provider reports it.
    ResponseDone { input_tokens: Option<u32> },
    /// A protocol/transport error. Carries [`VoiceError::is_terminal`].
    Error(VoiceError),
}

/// The session parameters handed to [`VoiceProvider::connect`]. Built once
/// from the composed brief; reused verbatim on reconnect (no re-inject).
#[derive(Clone, Debug)]
pub struct VoiceSessionConfig {
    /// Composed system instructions from `compose_instructions_by_priority`.
    pub instructions: String,
    /// Voice id the provider should speak with.
    pub voice: String,
    /// Tool/function schema (provider JSON) the model may call.
    pub tools: serde_json::Value,
    /// Target end-to-end latency in ms (drives turn-detection tuning).
    pub latency_target_ms: u64,
    /// Optional sampling temperature.
    pub temperature: Option<f64>,
    /// Optional end-of-turn silence timeout in ms (server-VAD).
    pub end_of_turn_timeout_ms: Option<u64>,
    /// Optional output speed multiplier.
    pub output_speed: Option<f64>,
    /// When `true`, `connect` includes a cold-start user item + response
    /// so the model greets first (batched into the handshake flush).
    pub cold_start_kick: bool,
    /// Optional ISO-639-1 language hint (e.g. `"ru"`) for OpenAI's input
    /// transcription sidecar; `None` = auto-detect. Ignored by xAI (its inline
    /// transcription takes no config).
    pub transcription_language: Option<String>,
}

impl VoiceSessionConfig {
    /// Build a session config with the required provider-facing values and
    /// stable defaults for optional behavior. Prefer this constructor over a
    /// full struct literal so adding another optional setting does not require
    /// every caller to change at once.
    pub fn new(
        instructions: impl Into<String>,
        voice: impl Into<String>,
        tools: serde_json::Value,
    ) -> Self {
        Self {
            instructions: instructions.into(),
            voice: voice.into(),
            tools,
            ..Self::default()
        }
    }
}

impl Default for VoiceSessionConfig {
    fn default() -> Self {
        Self {
            instructions: String::new(),
            voice: String::new(),
            tools: serde_json::json!([]),
            latency_target_ms: 800,
            temperature: None,
            end_of_turn_timeout_ms: None,
            output_speed: None,
            cold_start_kick: false,
            transcription_language: None,
        }
    }
}

/// The swappable provider seam. Two DIRECT implementations: xAI Grok voice
/// and OpenAI `gpt-realtime-2.1`; the server picks one per call (by which
/// BYOK key the operator provided).
#[async_trait]
pub trait VoiceProvider: Send + Sync {
    /// Realtime model id, e.g. `"grok-voice-think-fast-1.0"`.
    fn model_id(&self) -> &str;
    /// Default voice id.
    fn default_voice(&self) -> &str;
    /// Advertised audio capabilities.
    fn audio_caps(&self) -> AudioCaps;
    /// Host-pin the endpoint, open the WS with `Authorization: Bearer`,
    /// send `session.update` (+ optional cold-start) as ONE batched flush,
    /// and return the split sink/stream pair.
    async fn connect(
        &self,
        cfg: &VoiceSessionConfig,
    ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError>;
    /// Connect with explicit manual turn detection. The default preserves
    /// source compatibility for third-party providers; built-in realtime
    /// providers override it to disable server VAD for push-to-talk.
    async fn connect_with_manual_turn_detection(
        &self,
        cfg: &VoiceSessionConfig,
        _manual_turn_detection: bool,
    ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError> {
        self.connect(cfg).await
    }
}

/// The write half: owns the mic-pump (LOCAL) / inbound side (REMOTE).
#[async_trait]
pub trait VoiceSink: Send {
    /// Send a PCM16 mono LE @ 24k frame to the model. base64 framing is
    /// internal to the impl.
    async fn send_audio(&mut self, pcm16: &[i16]) -> Result<(), VoiceError>;
    /// Barge-in: ask the model to cancel the in-flight response.
    async fn cancel_response(&mut self) -> Result<(), VoiceError>;
    /// Barge-in context sync: truncate the cancelled assistant item at the
    /// audio position the user actually heard, so the model's conversation
    /// state doesn't retain the unheard tail (on a WS transport the server
    /// can't observe client playback). Providers without
    /// `conversation.item.truncate` support keep this default no-op.
    async fn truncate_item(
        &mut self,
        _item_id: &str,
        _audio_end_ms: u64,
    ) -> Result<(), VoiceError> {
        Ok(())
    }
    /// Clear the provider-side input audio buffer. Useful when a manual
    /// push-to-talk turn is discarded before commit.
    async fn clear_user_audio(&mut self) -> Result<(), VoiceError> {
        Ok(())
    }
    /// Return a tool result to the model.
    async fn send_tool_result(
        &mut self,
        call_id: Option<&str>,
        output: serde_json::Value,
    ) -> Result<(), VoiceError>;
    /// Inject system context WITHOUT triggering a response (feeder digests).
    async fn inject_system_context(&mut self, text: &str) -> Result<(), VoiceError>;
    /// Ask the model to produce a response now (used when `server_vad` is
    /// off, or after a tool result).
    async fn request_response(&mut self) -> Result<(), VoiceError>;
    /// Commit the current input-audio buffer as one user conversation item
    /// without starting a response. Manual push-to-talk uses this split form
    /// so a turn can be preserved while a cancelled response is still waiting
    /// for its terminal `response.done` event.
    async fn commit_user_audio(&mut self) -> Result<(), VoiceError> {
        Err(VoiceError::Protocol(
            "provider does not support split manual-turn commit".to_owned(),
        ))
    }
    /// Commit the current input-audio buffer as one user turn, then ask the
    /// model to answer. Used by manual push-to-talk sessions. Server-VAD
    /// sessions and older provider implementations keep the original default
    /// response-request behaviour.
    async fn commit_user_turn(&mut self) -> Result<(), VoiceError> {
        self.request_response().await
    }
    /// Close the connection.
    async fn close(&mut self) -> Result<(), VoiceError>;
}

/// The read half: owns the event-loop.
#[async_trait]
pub trait VoiceStream: Send {
    /// Next decoded event, or `None` when the stream is closed.
    async fn next_event(&mut self) -> Option<Result<VoiceEvent, VoiceError>>;
}

/// Errors from the voice provider seam. [`VoiceError::is_terminal`]
/// distinguishes "retry/reconnect" from "stop the call" (e.g. a zero
/// balance or `402 Payment Required` on upgrade is terminal).
#[derive(Debug, Clone, thiserror::Error)]
pub enum VoiceError {
    /// Refused to send the key to a non-pinned host (anti-exfiltration).
    #[error("endpoint host not allowed (host-pin): {0}")]
    HostNotAllowed(String),
    /// No usable BYOK key (`XAI_API_KEY` / `OPENAI_API_KEY`) from env or
    /// keychain.
    #[error("no API key available: {0}")]
    MissingKey(String),
    /// Handshake / upgrade failed.
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// Provider account balance exhausted / payment required. Terminal.
    #[error("provider balance exhausted")]
    BalanceZero,
    /// Transport-level error (WS closed, IO). Usually transient.
    #[error("transport error: {0}")]
    Transport(String),
    /// A malformed or unexpected server message.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The selected provider is wired at the type level but not yet
    /// implemented (e.g. OpenAI in v1). Terminal.
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
}

impl VoiceError {
    /// `true` when the call must end (no point reconnecting): balance
    /// exhausted, host-pin refusal, missing key, unavailable provider.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            VoiceError::BalanceZero
                | VoiceError::HostNotAllowed(_)
                | VoiceError::MissingKey(_)
                | VoiceError::ProviderUnavailable(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_classification() {
        assert!(VoiceError::BalanceZero.is_terminal());
        assert!(VoiceError::HostNotAllowed("evil.example".into()).is_terminal());
        assert!(!VoiceError::Transport("ws closed".into()).is_terminal());
        assert!(!VoiceError::Protocol("bad json".into()).is_terminal());
    }

    #[test]
    fn session_config_constructor_keeps_optional_defaults_stable() {
        let cfg = VoiceSessionConfig::new("instructions", "voice", serde_json::json!([]));
        assert_eq!(cfg.instructions, "instructions");
        assert_eq!(cfg.voice, "voice");
        assert_eq!(cfg.latency_target_ms, 800);
        assert!(!cfg.cold_start_kick);
        assert!(cfg.transcription_language.is_none());
    }
}
