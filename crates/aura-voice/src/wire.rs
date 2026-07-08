//! The realtime wire protocol: server-event parsing, client-event builders,
//! PCM/base64 helpers, the `session.update` builders, and the host-pin
//! validator.
//!
//! Two DIRECT providers speak through this module: xAI Grok voice and OpenAI
//! `gpt-realtime-2.1`. Both use GA-style event names, so ONE
//! [`parse_server_event`] serves both; the per-provider differences are the
//! `session.update` builders and the endpoint/host-pin constants below. The
//! Cloudflare-proxy endpoints, the STT sidecar, and the `AURA_TOKEN` managed
//! path are deliberately left out (BYOK DIRECT only).
//!
//! The runtime above [`crate::VoiceStream`] never sees these JSON shapes —
//! [`parse_server_event`] + the `ServerEvent` → `VoiceEvent` mapping in
//! `realtime_ws.rs` is the only place the wire format is touched.

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

/// OpenAI GA realtime endpoint — the second first-class provider.
pub const OPENAI_REALTIME_ENDPOINT: &str = "wss://api.openai.com/v1/realtime";
/// Default OpenAI realtime model. Keep current; never write a stale id.
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-realtime-2.1";
/// The cheaper OpenAI realtime model (~3x cheaper audio tokens).
pub const OPENAI_MINI_MODEL: &str = "gpt-realtime-2.1-mini";
/// The only host the OpenAI BYOK key may be sent to.
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
    OutputAudioDelta {
        delta: String,
        item_id: Option<String>,
    },
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
    /// The provider's confirmation of our `conversation.item.truncate` — the
    /// heard-position sync worked (the unheard tail left the model's context).
    /// Carried for observability only; the engine takes no action on it.
    #[serde(rename = "conversation.item.truncated")]
    ItemTruncated {
        item_id: Option<String>,
        audio_end_ms: Option<u64>,
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

/// Barge-in context sync: truncate a cancelled assistant item's audio at the
/// position the user actually heard; the server drops the unheard tail (and
/// its transcript) from the model's conversation state. Required on the WS
/// transport, where the server cannot observe client playback.
pub fn conversation_item_truncate_event(item_id: &str, audio_end_ms: u64) -> Value {
    json!({
        "type": "conversation.item.truncate",
        "item_id": item_id,
        "content_index": 0,
        "audio_end_ms": audio_end_ms
    })
}

pub fn input_audio_buffer_append_event(audio_base64: &str) -> Value {
    json!({"type": "input_audio_buffer.append", "audio": audio_base64})
}

pub fn input_audio_buffer_commit_event() -> Value {
    json!({"type": "input_audio_buffer.commit"})
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
    let turn_detection = if cfg.manual_turn_detection {
        Value::Null
    } else {
        turn_detection_from_latency(cfg.latency_target_ms, cfg.end_of_turn_timeout_ms)
    };
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

/// The OpenAI GA `session.update` builder. OpenAI nests voice /
/// turn_detection / transcription under `session.audio.*`, requires
/// `session.type: "realtime"`, and rejects top-level `temperature` (removed
/// from the GA API — the knob simply doesn't exist there).
///
/// Input transcription is ALWAYS on: it is not an optional nicety — it feeds
/// the `[developer]` lines of the in-call transcript, the reconnect/pause
/// digests, and the post-call recap. `whisper-1` is the stable choice; the
/// optional ISO-639-1 hint improves non-English accuracy. The audio path
/// itself stays direct speech-to-speech — this sidecar only affects the
/// transcript TEXT.
///
/// `reasoning.effort`: `low` is the documented production-voice
/// recommendation for `gpt-realtime-2.1`; the mini model is raised to
/// `medium` to compensate for the smaller model.
pub fn openai_session_update_event(cfg: &VoiceSessionConfig, model: &str) -> Value {
    let turn_detection = if cfg.manual_turn_detection {
        Value::Null
    } else {
        turn_detection_from_latency(cfg.latency_target_ms, cfg.end_of_turn_timeout_ms)
    };
    let mut transcription = json!({"model": "whisper-1"});
    if let Some(lang) = &cfg.transcription_language {
        transcription["language"] = json!(lang);
    }
    let effort = if model.contains("mini") {
        "medium"
    } else {
        "low"
    };
    let mut session = json!({
        "type": "realtime",
        "output_modalities": ["audio"],
        "instructions": cfg.instructions,
        "audio": {
            "input": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "turn_detection": turn_detection,
                "transcription": transcription
            },
            "output": {
                "format": {"type": "audio/pcm", "rate": 24000},
                "voice": cfg.voice
            }
        },
        "tools": cfg.tools,
        "reasoning": {"effort": effort}
    });
    if let Some(speed) = cfg.output_speed {
        session["audio"]["output"]["speed"] = json!(speed);
    }
    json!({"type": "session.update", "session": session})
}

// --- URL + host-pin ----------------------------------------------------------

/// Build a realtime URL: `AURA_REALTIME_URL` override if set, else the given
/// provider endpoint, with `?model=` appended.
fn realtime_url_with_default(default_endpoint: &str, model: &str) -> String {
    let endpoint =
        std::env::var(REALTIME_URL_OVERRIDE_ENV).unwrap_or_else(|_| default_endpoint.to_owned());
    let mut url = Url::parse(&endpoint)
        .unwrap_or_else(|_| Url::parse(default_endpoint).expect("built-in endpoint parses"));
    url.query_pairs_mut().append_pair("model", model);
    url.to_string()
}

/// The xAI realtime URL (override-aware, still host-pinned).
pub fn xai_realtime_url(model: &str) -> String {
    realtime_url_with_default(DIRECT_REALTIME_ENDPOINT, model)
}

/// The OpenAI realtime URL (override-aware, still host-pinned).
pub fn openai_realtime_url(model: &str) -> String {
    realtime_url_with_default(OPENAI_REALTIME_ENDPOINT, model)
}

/// Host-pin against the xAI host. See [`validate_realtime_url_for`].
pub fn validate_realtime_url(realtime_url: &str) -> Result<(), WireError> {
    validate_realtime_url_for(realtime_url, DIRECT_XAI_ALLOWED_HOST)
}

/// Host-pin: refuse to send a BYOK key to any host other than the provider's
/// pinned API host (`api.x.ai` / `api.openai.com`). The escape hatch
/// (`AURA_XAI_DIRECT_ALLOW_UNSAFE_URL=1`) is loudly logged and for isolated
/// local testing only. This is the single anti-exfiltration guard for the
/// DIRECT path.
pub fn validate_realtime_url_for(
    realtime_url: &str,
    allowed_host: &'static str,
) -> Result<(), WireError> {
    let host = Url::parse(realtime_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<invalid-url>".to_owned());
    if host == allowed_host {
        return Ok(());
    }
    if std::env::var(UNSAFE_URL_OPT_IN_ENV).as_deref() == Ok("1") {
        eprintln!(
            "AURA SECURITY WARNING: {UNSAFE_URL_OPT_IN_ENV}=1 allows the BYOK API key to be sent \
             to realtime endpoint {realtime_url} (host {host}). Use only for isolated local \
             testing."
        );
        return Ok(());
    }
    Err(WireError::UnsafeEndpoint {
        endpoint: realtime_url.to_owned(),
        host,
        allowed: allowed_host,
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
            manual_turn_detection: false,
            output_speed: None,
            cold_start_kick: true,
            transcription_language: None,
        }
    }

    #[test]
    fn parses_audio_delta_and_folds_text_tags() {
        // item_id absent → None (xAI may omit it).
        let e =
            parse_server_event(r#"{"type":"response.output_audio.delta","delta":"AAA="}"#).unwrap();
        assert!(matches!(
            e,
            ServerEvent::OutputAudioDelta { item_id: None, .. }
        ));
        // item_id present → captured (drives barge-in truncate).
        let e = parse_server_event(
            r#"{"type":"response.output_audio.delta","delta":"AAA=","item_id":"it_1"}"#,
        )
        .unwrap();
        assert!(
            matches!(e, ServerEvent::OutputAudioDelta { item_id: Some(id), .. } if id == "it_1")
        );
        // output_text.delta folds onto TextDelta.
        let e =
            parse_server_event(r#"{"type":"response.output_text.delta","delta":"hi"}"#).unwrap();
        assert!(matches!(e, ServerEvent::TextDelta { delta } if delta == "hi"));
    }

    #[test]
    fn truncate_event_shape() {
        let v = conversation_item_truncate_event("it_9", 1234);
        assert_eq!(v["type"], "conversation.item.truncate");
        assert_eq!(v["item_id"], "it_9");
        assert_eq!(v["content_index"], 0);
        assert_eq!(v["audio_end_ms"], 1234);
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
    fn manual_turn_detection_disables_vad_for_push_to_talk() {
        let mut cfg = cfg();
        cfg.manual_turn_detection = true;
        let xai = xai_session_update_event(&cfg);
        assert!(xai["session"]["turn_detection"].is_null());

        let openai = openai_session_update_event(&cfg, OPENAI_DEFAULT_MODEL);
        assert!(openai["session"]["audio"]["input"]["turn_detection"].is_null());
    }

    #[test]
    fn commit_event_shape() {
        let v = input_audio_buffer_commit_event();
        assert_eq!(v["type"], "input_audio_buffer.commit");
    }

    #[test]
    fn openai_builder_nests_voice_under_audio_output() {
        let v = openai_session_update_event(&cfg(), OPENAI_DEFAULT_MODEL);
        assert_eq!(v["session"]["type"], "realtime");
        assert_eq!(v["session"]["output_modalities"], json!(["audio"]));
        assert_eq!(v["session"]["audio"]["output"]["voice"], "eve");
        assert_eq!(v["session"]["audio"]["input"]["format"]["rate"], 24000);
        assert_eq!(v["session"]["audio"]["output"]["format"]["rate"], 24000);
        // GA removed the temperature knob — it must never be sent.
        assert!(v["session"]["voice"].is_null());
        assert!(v["session"]["temperature"].is_null());
    }

    #[test]
    fn openai_builder_transcription_always_on_with_optional_language() {
        // No hint → whisper-1 with auto language detection.
        let v = openai_session_update_event(&cfg(), OPENAI_DEFAULT_MODEL);
        let t = &v["session"]["audio"]["input"]["transcription"];
        assert_eq!(t["model"], "whisper-1");
        assert!(t["language"].is_null());
        // Hint set → forwarded as ISO-639-1.
        let mut c = cfg();
        c.transcription_language = Some("ru".into());
        let v = openai_session_update_event(&c, OPENAI_DEFAULT_MODEL);
        assert_eq!(
            v["session"]["audio"]["input"]["transcription"]["language"],
            "ru"
        );
    }

    #[test]
    fn openai_builder_reasoning_effort_low_full_medium_mini() {
        let full = openai_session_update_event(&cfg(), OPENAI_DEFAULT_MODEL);
        assert_eq!(full["session"]["reasoning"]["effort"], "low");
        let mini = openai_session_update_event(&cfg(), OPENAI_MINI_MODEL);
        assert_eq!(mini["session"]["reasoning"]["effort"], "medium");
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

    #[test]
    fn host_pin_is_per_provider() {
        std::env::remove_var(UNSAFE_URL_OPT_IN_ENV);
        let openai = openai_realtime_url(OPENAI_DEFAULT_MODEL);
        assert!(openai.starts_with("wss://api.openai.com/v1/realtime?model="));
        assert!(openai.contains(OPENAI_DEFAULT_MODEL));
        assert!(validate_realtime_url_for(&openai, DIRECT_OPENAI_ALLOWED_HOST).is_ok());
        // The pin is not interchangeable: each key only travels to ITS host.
        assert!(validate_realtime_url_for(&openai, DIRECT_XAI_ALLOWED_HOST).is_err());
        assert!(validate_realtime_url_for(
            "wss://api.x.ai/v1/realtime?model=x",
            DIRECT_OPENAI_ALLOWED_HOST
        )
        .is_err());
    }
}
