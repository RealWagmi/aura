//! `XaiRealtimeProvider` — the v1 [`VoiceProvider`]: DIRECT
//! `wss://api.x.ai/v1/realtime`, BYOK, host-pinned.
//!
//! [`VoiceProvider::connect`] resolves the key, host-pins the URL, opens the
//! WS with a `Bearer` header, sends `session.update` (+ optional cold-start)
//! as ONE batched flush, and returns the **split** [`XaiSink`] / [`XaiStream`]
//! pair over the underlying `SplitSink`/`SplitStream` (the
//! mic-pump and event-loop tasks can't share `&mut self` to one WS). The
//! stream maps each [`ServerEvent`] to a [`VoiceEvent`]; the engine never sees
//! provider JSON.

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use zeroize::Zeroizing;

use crate::wire::{self, ServerEvent, WireError};
use crate::{
    AudioCaps, VoiceError, VoiceEvent, VoiceProvider, VoiceSessionConfig, VoiceSink, VoiceStream,
    VoiceToolCall,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWrite = SplitSink<WsStream, Message>;
type WsRead = SplitStream<WsStream>;

/// Default voice id used when a session config doesn't override it.
pub const DEFAULT_VOICE: &str = "eve";

/// Keychain service name under which the BYOK key is stored.
const KEYCHAIN_SERVICE: &str = "aura";
/// Keychain entry/user name for the xAI key.
const KEYCHAIN_USER: &str = "XAI_API_KEY";

/// Resolve the BYOK `XAI_API_KEY`, wrapped in [`Zeroizing`] so the plaintext is
/// wiped on drop. The key is never placed in a struct with `Debug`/`Serialize`,
/// a URL, argv, or a log line.
///
/// Resolution order ("env → OS-keychain"), with NO
/// silent fallback — every failure path is explicit:
///
/// 1. **Env** `XAI_API_KEY` (primary). Trimmed; an empty/whitespace value is
///    rejected, not used as a key.
/// 2. **OS keychain** (the [`keyring`] crate): service `"aura"`, entry
///    `"XAI_API_KEY"`. A non-empty secret is used (trimmed). The keychain being
///    *unavailable* (e.g. a headless Linux VPS with no `org.freedesktop.secrets`
///    service) or the entry being *absent* is NOT an error by itself — it falls
///    through to the typed error below. See [`probe_keychain_key`].
/// 3. If neither source yields a key, a [`VoiceError::MissingKey`] is returned
///    explaining that BOTH sources were tried (env unset AND keychain
///    absent/unavailable) and how to set either. We never panic and never
///    return an empty key.
///
/// ## Headless-VPS story
///
/// On a headless server there is typically no secret-service daemon, so the
/// keychain probe in step 2 fails soft (it is caught and treated as "absent" —
/// no crash, no hang, no silently-empty key); **the env path is the supported
/// source there**. The "encrypted `0o600` file" alternative is a
/// documented non-goal for v1: env + keychain already cover both the
/// interactive-desktop and the headless-server cases.
pub fn resolve_xai_key() -> Result<Zeroizing<String>, VoiceError> {
    // 1. Env (primary; the only source that works on a headless VPS).
    if let Ok(k) = std::env::var("XAI_API_KEY") {
        let trimmed = k.trim();
        if !trimmed.is_empty() {
            return Ok(Zeroizing::new(trimmed.to_owned()));
        }
    }

    // 2. OS keychain — fails soft to "absent" (never an error by itself).
    if let Some(key) = probe_keychain_key() {
        return Ok(key);
    }

    // 3. Neither source yielded a usable key.
    Err(VoiceError::MissingKey(format!(
        "no BYOK xAI key found: env XAI_API_KEY is unset/empty AND the OS keychain \
         (service \"{KEYCHAIN_SERVICE}\", entry \"{KEYCHAIN_USER}\") has no entry or is \
         unavailable. Set the env var (recommended on a headless server), e.g. \
         `export XAI_API_KEY=...`, or store it in the OS keychain under that \
         service/entry on a desktop."
    )))
}

/// Probe the OS keychain for the BYOK key. Returns `Some` only for a non-empty
/// secret; returns `None` for *every* "no key here" outcome — entry absent,
/// keychain locked, no secret-service daemon (headless Linux), construction
/// failure, or any other backend error. This is the soft-fail that makes step 2
/// of [`resolve_xai_key`] safe on a headless VPS: it can neither hang nor panic,
/// and it never produces a silently-empty key.
///
/// The secret is wrapped in [`Zeroizing`] the instant it leaves the keychain so
/// no lingering plaintext `String` copy survives.
fn probe_keychain_key() -> Option<Zeroizing<String>> {
    // `Entry::new` itself is fallible (some platforms reject construction);
    // a failure here means "no usable keychain" → absent.
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER).ok()?;
    // Move the returned `String` straight into `Zeroizing` so the only copy of
    // the plaintext is the zeroizing one.
    let secret = Zeroizing::new(entry.get_password().ok()?);
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `trimmed` borrows `secret`; re-own into a fresh `Zeroizing` and let the
    // original (which may hold surrounding whitespace) drop/zeroize.
    Some(Zeroizing::new(trimmed.to_owned()))
}

/// The xAI Grok realtime voice provider.
#[derive(Debug, Clone)]
pub struct XaiRealtimeProvider {
    model: String,
    voice: String,
}

impl XaiRealtimeProvider {
    /// Default provider: current Grok voice model + default voice.
    pub fn new() -> Self {
        Self {
            model: wire::DEFAULT_MODEL.to_owned(),
            voice: DEFAULT_VOICE.to_owned(),
        }
    }

    /// Override model and/or default voice.
    pub fn with_model_and_voice(model: impl Into<String>, voice: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            voice: voice.into(),
        }
    }
}

impl Default for XaiRealtimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VoiceProvider for XaiRealtimeProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn default_voice(&self) -> &str {
        &self.voice
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
        cfg: &VoiceSessionConfig,
    ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError> {
        ensure_crypto_provider();
        let key = resolve_xai_key()?;
        let url = wire::xai_realtime_url(&self.model);
        // Host-pin: refuse to send the key anywhere but api.x.ai.
        wire::validate_realtime_url(&url).map_err(|e| match e {
            WireError::UnsafeEndpoint { host, .. } => VoiceError::HostNotAllowed(host),
            other => VoiceError::Handshake(other.to_string()),
        })?;

        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| VoiceError::Handshake(e.to_string()))?;
        let bearer = Zeroizing::new(format!("Bearer {}", key.as_str()));
        let header = HeaderValue::from_str(bearer.as_str())
            .map_err(|e| VoiceError::Handshake(e.to_string()))?;
        request.headers_mut().insert("Authorization", header);

        let (ws, _resp) = connect_async(request).await.map_err(classify_ws_error)?;
        let (mut write, read) = ws.split();

        // One batched flush: session.update first, then the optional cold-start
        // (user item + response.create) in the SAME flush.
        let mut frames = vec![wire::xai_session_update_event(cfg)];
        if cfg.cold_start_kick {
            let (user_msg, response_create) = wire::cold_start_kick_events();
            frames.push(user_msg);
            frames.push(response_create);
        }
        for frame in &frames {
            write
                .feed(Message::text(frame.to_string()))
                .await
                .map_err(|e| VoiceError::Transport(e.to_string()))?;
        }
        write
            .flush()
            .await
            .map_err(|e| VoiceError::Transport(e.to_string()))?;

        Ok((Box::new(XaiSink { write }), Box::new(XaiStream { read })))
    }
}

/// Install the `ring` rustls `CryptoProvider` as the process default, once.
///
/// rustls 0.23 (pulled in by `tokio-tungstenite`) requires a process-level
/// crypto provider before any TLS config is built; with both `ring` and
/// `aws-lc-rs` potentially compiled in it cannot auto-pick one and panics. We
/// select `ring` explicitly. Idempotent: the `Err` (provider already installed)
/// is ignored.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Map a connect-time WS error to a [`VoiceError`]. A `402 Payment Required` on
/// upgrade is terminal (balance exhausted); everything else is a transient
/// handshake failure the engine may retry.
fn classify_ws_error(err: WsError) -> VoiceError {
    if let WsError::Http(resp) = &err {
        if resp.status().as_u16() == 402 {
            return VoiceError::BalanceZero;
        }
    }
    VoiceError::Handshake(err.to_string())
}

/// The write half of the WS — owns outbound frames (mic audio, cancel, tool
/// results, system context, response triggers).
struct XaiSink {
    write: WsWrite,
}

impl XaiSink {
    async fn send(&mut self, value: Value) -> Result<(), VoiceError> {
        self.write
            .send(Message::text(value.to_string()))
            .await
            .map_err(|e| VoiceError::Transport(e.to_string()))
    }
}

#[async_trait]
impl VoiceSink for XaiSink {
    async fn send_audio(&mut self, pcm16: &[i16]) -> Result<(), VoiceError> {
        let frame = wire::input_audio_buffer_append_event(&wire::pcm16_to_base64(pcm16));
        self.send(frame).await
    }

    async fn cancel_response(&mut self) -> Result<(), VoiceError> {
        self.send(wire::response_cancel_event()).await
    }

    async fn send_tool_result(
        &mut self,
        call_id: Option<&str>,
        output: Value,
    ) -> Result<(), VoiceError> {
        self.send(wire::function_call_output_event(call_id, output))
            .await
    }

    async fn inject_system_context(&mut self, text: &str) -> Result<(), VoiceError> {
        self.send(wire::system_context_inject_event(text)).await
    }

    async fn request_response(&mut self) -> Result<(), VoiceError> {
        self.send(wire::response_create_event()).await
    }

    async fn close(&mut self) -> Result<(), VoiceError> {
        self.write
            .close()
            .await
            .map_err(|e| VoiceError::Transport(e.to_string()))
    }
}

/// The read half of the WS — owns the event loop, maps `ServerEvent` →
/// `VoiceEvent`, and silently skips unmappable frames (pings, unknown events).
struct XaiStream {
    read: WsRead,
}

#[async_trait]
impl VoiceStream for XaiStream {
    async fn next_event(&mut self) -> Option<Result<VoiceEvent, VoiceError>> {
        loop {
            match self.read.next().await {
                None => return None,
                Some(Err(e)) => return Some(Err(VoiceError::Transport(e.to_string()))),
                Some(Ok(Message::Text(text))) => match wire::parse_server_event(text.as_str()) {
                    Ok(event) => {
                        if let Some(mapped) = map_event(event) {
                            return Some(mapped);
                        }
                        // Unmappable (Unknown) — keep reading.
                    }
                    // Unparseable frame: skip rather than kill the call.
                    Err(_) => continue,
                },
                Some(Ok(Message::Close(_))) => return None,
                // Binary / Ping / Pong / Frame: nothing to surface; tungstenite
                // auto-responds to pings. Keep reading.
                Some(Ok(_)) => continue,
            }
        }
    }
}

/// Map a parsed [`ServerEvent`] to a [`VoiceEvent`]. Returns `None` for events
/// the engine doesn't care about (so the stream keeps reading).
fn map_event(event: ServerEvent) -> Option<Result<VoiceEvent, VoiceError>> {
    let mapped = match event {
        ServerEvent::SessionCreated { .. } => VoiceEvent::SessionReady,
        ServerEvent::OutputAudioDelta { delta } => match wire::base64_to_pcm16(&delta) {
            Ok(pcm) => VoiceEvent::OutputAudio(pcm),
            Err(e) => VoiceEvent::Error(VoiceError::Protocol(format!("bad audio delta: {e}"))),
        },
        ServerEvent::TextDelta { delta } => VoiceEvent::OutputTextDelta(delta),
        ServerEvent::InputAudioTranscriptionDelta { delta, .. } => {
            VoiceEvent::InputTranscriptDelta {
                delta,
                final_: false,
            }
        }
        ServerEvent::InputAudioTranscriptionCompleted { transcript, .. } => {
            VoiceEvent::InputTranscriptDelta {
                delta: transcript.unwrap_or_default(),
                final_: true,
            }
        }
        ServerEvent::SpeechStarted => VoiceEvent::UserSpeechStarted,
        ServerEvent::SpeechStopped => VoiceEvent::UserSpeechStopped,
        ServerEvent::FunctionCallArgumentsDone {
            call_id,
            name,
            arguments,
        } => {
            let args = if arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                match serde_json::from_str(&arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        return Some(Ok(VoiceEvent::Error(VoiceError::Protocol(format!(
                            "bad tool args: {e}"
                        )))))
                    }
                }
            };
            VoiceEvent::ToolCall(VoiceToolCall {
                call_id,
                name,
                args,
            })
        }
        ServerEvent::ResponseDone { response } => {
            let input_tokens = response
                .as_ref()
                .and_then(|r| r.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(Value::as_u64)
                .map(|n| n as u32);
            VoiceEvent::ResponseDone { input_tokens }
        }
        ServerEvent::Error { error } => {
            if wire::is_terminal_balance_zero(&error) {
                VoiceEvent::Error(VoiceError::BalanceZero)
            } else {
                let code = error
                    .get("code")
                    .or_else(|| error.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("realtime_error");
                VoiceEvent::Error(VoiceError::Protocol(code.to_owned()))
            }
        }
        ServerEvent::Unknown => return None,
    };
    Some(Ok(mapped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_metadata() {
        let p = XaiRealtimeProvider::new();
        assert_eq!(p.model_id(), wire::DEFAULT_MODEL);
        assert_eq!(p.default_voice(), DEFAULT_VOICE);
        let caps = p.audio_caps();
        assert!(caps.server_vad);
        assert_eq!(caps.input_sample_rate_hz, 24_000);
        assert_eq!(caps.output_sample_rate_hz, 24_000);
    }

    #[test]
    fn maps_audio_delta_to_pcm() {
        let pcm = vec![1i16, -2, 3];
        let b64 = wire::pcm16_to_base64(&pcm);
        let ev = map_event(ServerEvent::OutputAudioDelta { delta: b64 })
            .unwrap()
            .unwrap();
        assert!(matches!(ev, VoiceEvent::OutputAudio(p) if p == pcm));
    }

    #[test]
    fn maps_speech_and_tool_and_done() {
        assert!(matches!(
            map_event(ServerEvent::SpeechStarted).unwrap().unwrap(),
            VoiceEvent::UserSpeechStarted
        ));
        let tool = map_event(ServerEvent::FunctionCallArgumentsDone {
            call_id: Some("c1".into()),
            name: "f".into(),
            arguments: r#"{"x":1}"#.into(),
        })
        .unwrap()
        .unwrap();
        match tool {
            VoiceEvent::ToolCall(tc) => {
                assert_eq!(tc.name, "f");
                assert_eq!(tc.args["x"], 1);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        let done = map_event(ServerEvent::ResponseDone {
            response: Some(json!({"usage": {"input_tokens": 42}})),
        })
        .unwrap()
        .unwrap();
        assert!(matches!(
            done,
            VoiceEvent::ResponseDone {
                input_tokens: Some(42)
            }
        ));
    }

    #[test]
    fn maps_terminal_error_to_balance_zero() {
        let ev = map_event(ServerEvent::Error {
            error: json!({"status": 402}),
        })
        .unwrap()
        .unwrap();
        match ev {
            VoiceEvent::Error(e) => assert!(e.is_terminal()),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_is_skipped() {
        assert!(map_event(ServerEvent::Unknown).is_none());
    }

    /// All `resolve_xai_key` cases in ONE test: `XAI_API_KEY` is process-global,
    /// so the env-mutating assertions must be serialized (the multi-threaded test
    /// runner would otherwise race). The keychain probe must stay soft (no hang,
    /// no panic) even on this headless CI box with no secret service — so the
    /// "absent" path is deterministic without ever populating a real keychain.
    #[test]
    fn resolve_key_order_and_failure() {
        // Snapshot and clear so the assertions are independent of the ambient env.
        let saved = std::env::var("XAI_API_KEY").ok();
        // SAFETY: serialized within this single test; no other thread reads the
        // var concurrently (this is the only test that touches it).
        unsafe { std::env::remove_var("XAI_API_KEY") };

        // env present (with surrounding whitespace) → trimmed env key wins.
        unsafe { std::env::set_var("XAI_API_KEY", "  sk-from-env  ") };
        let got = resolve_xai_key().expect("env-present should resolve");
        assert_eq!(got.as_str(), "sk-from-env");

        // env empty / whitespace-only → NOT used as a key.
        unsafe { std::env::set_var("XAI_API_KEY", "   ") };
        let empty = resolve_xai_key();
        // Either the keychain has a real entry (desktop dev box) or it doesn't
        // (headless CI). Whichever, the whitespace env value must never surface.
        if let Ok(k) = &empty {
            assert_ne!(k.as_str(), "   ");
            assert!(!k.trim().is_empty());
        }

        // env absent + keychain absent/unavailable → typed MissingKey error.
        // On this headless box the keychain probe fails soft to "absent", so the
        // error path is deterministic; it must not hang or panic.
        unsafe { std::env::remove_var("XAI_API_KEY") };
        match resolve_xai_key() {
            Err(VoiceError::MissingKey(msg)) => {
                assert!(msg.contains("XAI_API_KEY"));
                assert!(msg.contains(KEYCHAIN_SERVICE));
                assert!(msg.contains(KEYCHAIN_USER));
            }
            // A populated real keychain on a dev box is the only other valid
            // outcome; never an empty key, never a different error.
            Ok(k) => assert!(!k.trim().is_empty()),
            Err(other) => panic!("expected MissingKey, got {other:?}"),
        }

        // Restore the ambient env for any later code in the process.
        match saved {
            Some(v) => unsafe { std::env::set_var("XAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("XAI_API_KEY") },
        }
    }

    #[test]
    fn keychain_probe_is_soft_and_never_panics() {
        // The probe must return a value (Some/None) without hanging or panicking
        // regardless of whether a secret service exists. On headless CI this
        // exercises the unavailable/absent → None soft-fail.
        let probed = probe_keychain_key();
        // A non-empty result (dev box with a stored key) must be trimmed-nonempty;
        // None is the expected headless outcome. Both are acceptable.
        if let Some(k) = probed {
            assert!(!k.trim().is_empty());
        }
    }

    #[test]
    fn crypto_provider_installs() {
        // Verifies the rustls provider fix without a network call: after
        // ensure_crypto_provider, a process-default provider exists (so
        // tokio-tungstenite can build a TLS config instead of panicking).
        ensure_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
