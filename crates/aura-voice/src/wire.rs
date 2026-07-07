//! The realtime wire protocol: server-event parsing, client-event builders,
//! PCM/base64 helpers, the `session.update` builders, and the host-pin
//! validator.
//!
//! The v1 product is xAI
//! DIRECT only, so the Cloudflare-proxy endpoints, the STT sidecar
//! (`SttServerEvent`/`parse_stt_event`/`stt_url`), the `AURA_TOKEN` managed
//! path, and the OpenAI OAuth resolution are all left out. The
//! OpenAI `session.update` builder is kept **dormant** (compiled, unit-tested,
//! never dialed) as a witness that the provider seam is real.
//!
//! The runtime above [`crate::VoiceStream`] never sees these JSON shapes —
//! [`parse_server_event`] + the `ServerEvent` → `VoiceEvent` mapping in
//! `xai.rs` is the only place the wire format is touched.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

use crate::VoiceSessionConfig;

// --- Endpoints / pinning -----------------------------------------------------

/// The only realtime endpoint v1 dials: xAI DIRECT (no broker).
pub const DIRECT_REALTIME_ENDPOINT: &str = "wss://api.x.ai/v1/realtime";
/// Default realtime model. Keep current; never write a stale id.
pub const DEFAULT_MODEL: &str = "grok-voice-think-fast-1.0";
/// The only host the BYOK key may be sent to.
pub const DIRECT_XAI_ALLOWED_HOST: &str = "api.x.ai";
/// Escape hatch for isolated local testing only (loudly logged).
pub const UNSAFE_URL_OPT_IN_ENV: &str = "AURA_XAI_DIRECT_ALLOW_UNSAFE_URL";
/// Optional realtime URL override (staging/tests); still host-pinned.
pub const REALTIME_URL_OVERRIDE_ENV: &str = "AURA_REALTIME_URL";

/// Dormant OpenAI seam — compiled, never dialed in v1.
pub const OPENAI_REALTIME_ENDPOINT: &str = "wss://api.openai.com/v1/realtime";
/// Dormant OpenAI default model. Keep current.
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-realtime-2";
/// Allowed host for the dormant OpenAI seam.
pub const DIRECT_OPENAI_ALLOWED_HOST: &str = "api.openai.com";

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("invalid realtime event JSON: {0}")]
    InvalidEvent(#[from] serde_json::Error),
    #[error("invalid audio delta: {0}")]
    InvalidAudio(#[from] base64::DecodeError),
    #[error(
        "refusing to send BYOK key to realtime endpoint {endpoint} (host {host}); \
         direct mode only allows host {allowed} unless {opt_in}=1 is set"
    )]
    UnsafeEndpoint {
        endpoint: String,
        host: String,
        allowed: &'static str,
        opt_in: &'static str,
    },
}

// --- Server events -----------------------------------------------------------

/// Streaming events the realtime WS sends. Three legacy transcript-delta
/// tags (`response.output_audio_transcript.delta`, `response.text.delta`,
/// `response.output_text.delta`) are folded onto [`ServerEvent::TextDelta`] by
/// [`parse_server_event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerEvent {
    #[serde(rename = "session.created")]
    SessionCreated { session: Option<Value> },
    #[serde(rename = "response.output_audio.delta")]
    OutputAudioDelta { delta: String },
    #[serde(rename = "conversation.item.input_audio_transcription.completed")]
    InputAudioTranscriptionCompleted {
        transcript: Option<String>,
        item_id: Option<String>,
    },
    #[serde(rename = "conversation.item.input_audio_transcription.delta")]
    InputAudioTranscriptionDelta {
        delta: String,
        item_id: Option<String>,
    },
    /// Unified transcript delta. See type-level docs.
    #[serde(rename = "response.text.delta")]
    TextDelta { delta: String },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        call_id: Option<String>,
        name: String,
        arguments: String,
    },
    #[serde(rename = "response.done")]
    ResponseDone { response: Option<Value> },
    #[serde(rename = "input_audio_buffer.speech_started")]
    SpeechStarted,
    #[serde(rename = "input_audio_buffer.speech_stopped")]
    SpeechStopped,
    #[serde(rename = "error")]
    Error { error: Value },
    #[serde(other)]
    Unknown,
}

/// A decoded tool/function call from the realtime stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCall {
    pub call_id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

/// Parse one raw WS text frame into a [`ServerEvent`]. Folds the three
/// legacy text-delta tags onto `response.text.delta`.
pub fn parse_server_event(raw: &str) -> Result<ServerEvent, WireError> {
    let mut value: Value = serde_json::from_str(raw)?;
    if let Some(Value::String(tag)) = value.get("type") {
        if tag == "response.output_audio_transcript.delta" || tag == "response.output_text.delta" {
            value["type"] = Value::String("response.text.delta".to_owned());
        }
    }
    Ok(serde_json::from_value(value)?)
}

/// Extract a [`FunctionCall`] from a `FunctionCallArgumentsDone` event.
pub fn extract_function_call(event: ServerEvent) -> Result<Option<FunctionCall>, WireError> {
    match event {
        ServerEvent::FunctionCallArgumentsDone {
            call_id,
            name,
            arguments,
        } => {
            let arguments = if arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&arguments)?
            };
            Ok(Some(FunctionCall {
                call_id,
                name,
                arguments,
            }))
        }
        _ => Ok(None),
    }
}

/// `true` if a `ServerEvent::Error { error }` payload means the session is
/// terminally out of credit (stop retrying). Classifies only on structured
/// fields (`status`/`code`/`type`), never free-text — that duck-typing was the
/// bug this guards against.
pub fn is_terminal_balance_zero(error: &Value) -> bool {
    if let Some(status) = error.get("status").and_then(Value::as_i64) {
        if status == 402 {
            return true;
        }
    }
    if let Some(status) = error.get("status").and_then(Value::as_str) {
        if status == "402" {
            return true;
        }
    }
    const TERMINAL_CODES: &[&str] = &[
        "insufficient_credits",
        "insufficient_quota",
        "balance_zero",
        "payment_required",
        "billing_hard_limit_reached",
        "credit_required",
    ];
    if let Some(code) = error.get("code").and_then(Value::as_str) {
        if TERMINAL_CODES.iter().any(|c| code.eq_ignore_ascii_case(c)) {
            return true;
        }
    }
    if let Some(error_type) = error.get("type").and_then(Value::as_str) {
        if TERMINAL_CODES
            .iter()
            .any(|c| error_type.eq_ignore_ascii_case(c))
        {
            return true;
        }
    }
    false
}

// --- Client-event builders ---------------------------------------------------

pub fn function_call_output_event(call_id: Option<&str>, output: Value) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "function_call_output",
            "call_id": call_id.unwrap_or("local-call"),
            "output": output.to_string()
        }
    })
}

pub fn response_create_event() -> Value {
    json!({"type": "response.create"})
}

pub fn response_cancel_event() -> Value {
    json!({"type": "response.cancel"})
}

pub fn input_audio_buffer_append_event(audio_base64: &str) -> Value {
    json!({"type": "input_audio_buffer.append", "audio": audio_base64})
}

pub fn user_text_message_event(text: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}]
        }
    })
}

/// Inject a system-role item — the feeder's ambient channel. Per Realtime API
/// convention this does NOT trigger a response.
pub fn system_context_inject_event(text: &str) -> Value {
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "message",
            "role": "system",
            "content": [{"type": "input_text", "text": text}]
        }
    })
}

/// The hidden cold-start kick: a user item + `response.create` batched into the
/// initial flush so the first phoneme is ready early. Returns
/// `(user_item, response_create)`.
pub fn cold_start_kick_events() -> (Value, Value) {
    let kick = "Begin the voice call. Briefly acknowledge you are listening and ask what we should work on.";
    (user_text_message_event(kick), response_create_event())
}

// --- PCM / base64 ------------------------------------------------------------

pub fn pcm16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

pub fn pcm16_to_base64(samples: &[i16]) -> String {
    STANDARD.encode(pcm16_to_le_bytes(samples))
}

pub fn base64_to_pcm16(encoded: &str) -> Result<Vec<i16>, WireError> {
    let bytes = STANDARD.decode(encoded)?;
    Ok(bytes
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect())
}

// --- session.update builders -------------------------------------------------

fn turn_detection_from_latency(latency_target_ms: u64, silence_override_ms: Option<u64>) -> Value {
    let target = latency_target_ms.clamp(400, 1_600);
    let prefix_padding_ms = ((target / 2) + 50).clamp(250, 650);
    let silence_duration_ms = match silence_override_ms {
        Some(0) => 0,
        Some(ms) => ms.clamp(300, 3_000),
        None => (target + 300).clamp(700, 1_900),
    };
    json!({
        "type": "server_vad",
        "threshold": 0.9,
        "prefix_padding_ms": prefix_padding_ms,
        "silence_duration_ms": silence_duration_ms,
    })
}

/// Build the xAI `session.update` frame from a [`VoiceSessionConfig`].
/// Top-level `voice` / `turn_detection` / `temperature`; PCM16 @ 24k both ways.
pub fn xai_session_update_event(cfg: &VoiceSessionConfig) -> Value {
    let turn_detection =
        turn_detection_from_latency(cfg.latency_target_ms, cfg.end_of_turn_timeout_ms);
    let mut session = json!({
        "voice": cfg.voice,
        "instructions": cfg.instructions,
        "turn_detection": turn_detection,
        "audio": {
            "input": {"format": {"type": "audio/pcm", "rate": 24000}},
            "output": {"format": {"type": "audio/pcm", "rate": 24000}}
        },
        "tools": cfg.tools
    });
    if let Some(t) = cfg.temperature {
        session["temperature"] = json!(t);
    }
    if let Some(speed) = cfg.output_speed {
        session["audio"]["output"]["speed"] = json!(speed);
    }
    json!({"type": "session.update", "session": session})
}

/// Dormant OpenAI `session.update` builder. OpenAI nests voice /
/// turn_detection / transcription under `session.audio.*`, requires
/// `session.type: "realtime"`, and rejects top-level `temperature`. Compiled
/// and unit-tested but never dialed in v1 — it witnesses the provider seam.
pub fn openai_session_update_event(cfg: &VoiceSessionConfig) -> Value {
    let turn_detection =
        turn_detection_from_latency(cfg.latency_target_ms, cfg.end_of_turn_timeout_ms);
    let mut session = json!({
        "type": "realtime",
        "instructions": cfg.instructions,
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "turn_detection": turn_detection,
                "transcription": {"model": "whisper-1"}
            },
            "output": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "voice": cfg.voice
            }
        },
        "tools": cfg.tools
    });
    if let Some(speed) = cfg.output_speed {
        session["audio"]["output"]["speed"] = json!(speed);
    }
    json!({"type": "session.update", "session": session})
}

// --- URL + host-pin ----------------------------------------------------------

/// Build the realtime URL: `AURA_REALTIME_URL` override if set, else the xAI
/// DIRECT endpoint, with `?model=` appended.
pub fn xai_realtime_url(model: &str) -> String {
    let endpoint = std::env::var(REALTIME_URL_OVERRIDE_ENV)
        .unwrap_or_else(|_| DIRECT_REALTIME_ENDPOINT.to_owned());
    let mut url = Url::parse(&endpoint).unwrap_or_else(|_| {
        Url::parse(DIRECT_REALTIME_ENDPOINT).expect("built-in endpoint parses")
    });
    url.query_pairs_mut().append_pair("model", model);
    url.to_string()
}

/// Host-pin: refuse to send the BYOK key to any host other than `api.x.ai`.
/// The escape hatch (`AURA_XAI_DIRECT_ALLOW_UNSAFE_URL=1`) is loudly logged and
/// for isolated local testing only. This is the single anti-exfiltration guard
/// for the DIRECT path.
pub fn validate_realtime_url(realtime_url: &str) -> Result<(), WireError> {
    let host = Url::parse(realtime_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<invalid-url>".to_owned());
    if host == DIRECT_XAI_ALLOWED_HOST {
        return Ok(());
    }
    if std::env::var(UNSAFE_URL_OPT_IN_ENV).as_deref() == Ok("1") {
        eprintln!(
            "AURA SECURITY WARNING: {UNSAFE_URL_OPT_IN_ENV}=1 allows XAI_API_KEY to be sent to \
             realtime endpoint {realtime_url} (host {host}). Use only for isolated local testing."
        );
        return Ok(());
    }
    Err(WireError::UnsafeEndpoint {
        endpoint: realtime_url.to_owned(),
        host,
        allowed: DIRECT_XAI_ALLOWED_HOST,
        opt_in: UNSAFE_URL_OPT_IN_ENV,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VoiceSessionConfig {
        VoiceSessionConfig {
            instructions: "be brief".into(),
            voice: "eve".into(),
            tools: json!([]),
            latency_target_ms: 800,
            temperature: Some(0.5),
            end_of_turn_timeout_ms: None,
            output_speed: None,
            cold_start_kick: true,
        }
    }

    #[test]
    fn parses_audio_delta_and_folds_text_tags() {
        let e =
            parse_server_event(r#"{"type":"response.output_audio.delta","delta":"AAA="}"#).unwrap();
        assert!(matches!(e, ServerEvent::OutputAudioDelta { .. }));
        // output_text.delta folds onto TextDelta.
        let e =
            parse_server_event(r#"{"type":"response.output_text.delta","delta":"hi"}"#).unwrap();
        assert!(matches!(e, ServerEvent::TextDelta { delta } if delta == "hi"));
    }

    #[test]
    fn unknown_event_is_unknown_not_error() {
        let e = parse_server_event(r#"{"type":"some.future.event","x":1}"#).unwrap();
        assert_eq!(e, ServerEvent::Unknown);
    }

    #[test]
    fn pcm16_base64_round_trip() {
        let samples = vec![0i16, 1, -1, 32767, -32768, 1234];
        let encoded = pcm16_to_base64(&samples);
        assert_eq!(base64_to_pcm16(&encoded).unwrap(), samples);
    }

    #[test]
    fn function_call_extracts_args() {
        let e = ServerEvent::FunctionCallArgumentsDone {
            call_id: Some("c1".into()),
            name: "do_thing".into(),
            arguments: r#"{"a":1}"#.into(),
        };
        let fc = extract_function_call(e).unwrap().unwrap();
        assert_eq!(fc.name, "do_thing");
        assert_eq!(fc.arguments["a"], 1);
    }

    #[test]
    fn terminal_balance_classification() {
        assert!(is_terminal_balance_zero(&json!({"status": 402})));
        assert!(is_terminal_balance_zero(
            &json!({"code": "insufficient_credits"})
        ));
        assert!(!is_terminal_balance_zero(
            &json!({"message": "402 happened"})
        ));
        assert!(!is_terminal_balance_zero(&json!({"code": "rate_limited"})));
    }

    #[test]
    fn session_update_is_24k_pcm_with_top_level_voice() {
        let v = xai_session_update_event(&cfg());
        assert_eq!(v["type"], "session.update");
        assert_eq!(v["session"]["voice"], "eve");
        assert_eq!(v["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(v["session"]["audio"]["output"]["format"]["rate"], 24000);
        assert_eq!(v["session"]["temperature"], 0.5);
    }

    #[test]
    fn zero_silence_override_is_preserved_for_push_to_talk() {
        let mut cfg = cfg();
        cfg.end_of_turn_timeout_ms = Some(0);
        let v = xai_session_update_event(&cfg);
        assert_eq!(v["session"]["turn_detection"]["silence_duration_ms"], 0);
    }

    #[test]
    fn openai_builder_nests_voice_under_audio_output() {
        // Dormant seam: shape differs from xAI (witness it compiles + differs).
        let v = openai_session_update_event(&cfg());
        assert_eq!(v["session"]["type"], "realtime");
        assert_eq!(v["session"]["audio"]["output"]["voice"], "eve");
        assert!(v["session"]["voice"].is_null());
    }

    #[test]
    fn realtime_url_targets_xai_with_model() {
        // Clear any override so the test is hermetic.
        std::env::remove_var(REALTIME_URL_OVERRIDE_ENV);
        let url = xai_realtime_url(DEFAULT_MODEL);
        assert!(url.starts_with("wss://api.x.ai/v1/realtime?model="));
        assert!(url.contains(DEFAULT_MODEL));
    }

    #[test]
    fn host_pin_accepts_xai_rejects_other() {
        std::env::remove_var(UNSAFE_URL_OPT_IN_ENV);
        assert!(validate_realtime_url("wss://api.x.ai/v1/realtime?model=x").is_ok());
        let err = validate_realtime_url("wss://evil.example/v1/realtime").unwrap_err();
        assert!(matches!(err, WireError::UnsafeEndpoint { .. }));
    }
}
