//! Shared realtime-WS plumbing for the DIRECT providers (xAI, OpenAI): the
//! host-pinned connect, the split [`RealtimeSink`] / [`RealtimeStream`]
//! halves, and the [`ServerEvent`] → [`VoiceEvent`] mapping.
//!
//! Both providers speak GA-style event names, so ONE sink/stream pair serves
//! both; the per-provider differences (endpoint, host-pin, `session.update`
//! shape, key resolution, default voice) live entirely in `xai.rs` /
//! `openai.rs` and `wire.rs`. The engine never sees provider JSON.

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
use crate::{VoiceError, VoiceEvent, VoiceSink, VoiceStream, VoiceToolCall};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsWrite = SplitSink<WsStream, Message>;
type WsRead = SplitStream<WsStream>;

/// Host-pin the URL, open the WS with `Authorization: Bearer`, send the
/// provider's handshake frames (`session.update` + optional cold-start) as ONE
/// batched flush, and return the split sink/stream pair.
///
/// `allowed_host` is the provider's pinned API host — the anti-exfiltration
/// guard that refuses to send the BYOK key anywhere else.
///
/// `truncate_enabled` gates [`VoiceSink::truncate_item`]: `true` sends
/// `conversation.item.truncate` on barge-in. BOTH providers support it and
/// default to `true` (OpenAI GA documents/requires it; xAI was live-verified to
/// attach `item_id` and confirm with `conversation.item.truncated`). A too-long
/// `audio_end_ms` only draws a benign `error` event — handled as informational,
/// never a reconnect — so leaving it on is safe. `AURA_XAI_TRUNCATE=0` flips
/// xAI off for debugging.
pub(crate) async fn connect_realtime(
    url: &str,
    allowed_host: &'static str,
    key: Zeroizing<String>,
    handshake_frames: Vec<Value>,
    truncate_enabled: bool,
    manual_turn_detection: bool,
) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError> {
    ensure_crypto_provider();
    wire::validate_realtime_url_for(url, allowed_host).map_err(|e| match e {
        WireError::UnsafeEndpoint { host, .. } => VoiceError::HostNotAllowed(host),
        other => VoiceError::Handshake(other.to_string()),
    })?;

    let mut request = url
        .into_client_request()
        .map_err(|e| VoiceError::Handshake(e.to_string()))?;
    let bearer = Zeroizing::new(format!("Bearer {}", key.as_str()));
    let header =
        HeaderValue::from_str(bearer.as_str()).map_err(|e| VoiceError::Handshake(e.to_string()))?;
    request.headers_mut().insert("Authorization", header);

    let (ws, _resp) = connect_async(request).await.map_err(classify_ws_error)?;
    let (mut write, read) = ws.split();

    // One batched flush: session.update first, then any cold-start frames in
    // the SAME flush so the first phoneme is ready early.
    for frame in &handshake_frames {
        write
            .feed(Message::text(frame.to_string()))
            .await
            .map_err(|e| VoiceError::Transport(e.to_string()))?;
    }
    write
        .flush()
        .await
        .map_err(|e| VoiceError::Transport(e.to_string()))?;

    Ok((
        Box::new(RealtimeSink {
            write,
            truncate_enabled,
            manual_turn_detection,
        }),
        Box::new(RealtimeStream { read }),
    ))
}

/// Install the `ring` rustls `CryptoProvider` as the process default, once.
///
/// rustls 0.23 (pulled in by `tokio-tungstenite`) requires a process-level
/// crypto provider before any TLS config is built; with both `ring` and
/// `aws-lc-rs` potentially compiled in it cannot auto-pick one and panics. We
/// select `ring` explicitly. Idempotent: the `Err` (provider already installed)
/// is ignored.
pub(crate) fn ensure_crypto_provider() {
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
struct RealtimeSink {
    write: WsWrite,
    /// Whether this provider supports `conversation.item.truncate` (see
    /// [`connect_realtime`]).
    truncate_enabled: bool,
    manual_turn_detection: bool,
}

impl RealtimeSink {
    async fn send(&mut self, value: Value) -> Result<(), VoiceError> {
        self.write
            .send(Message::text(value.to_string()))
            .await
            .map_err(|e| VoiceError::Transport(e.to_string()))
    }
}

#[async_trait::async_trait]
impl VoiceSink for RealtimeSink {
    async fn send_audio(&mut self, pcm16: &[i16]) -> Result<(), VoiceError> {
        let frame = wire::input_audio_buffer_append_event(&wire::pcm16_to_base64(pcm16));
        self.send(frame).await
    }

    async fn cancel_response(&mut self) -> Result<(), VoiceError> {
        self.send(wire::response_cancel_event()).await
    }

    async fn truncate_item(&mut self, item_id: &str, audio_end_ms: u64) -> Result<(), VoiceError> {
        if !self.truncate_enabled {
            return Ok(());
        }
        self.send(wire::conversation_item_truncate_event(
            item_id,
            audio_end_ms,
        ))
        .await?;
        // Observable marker for the live-truncate experiment (AURA_XAI_TRUNCATE)
        // and for OpenAI barge-in diagnostics: logged ONLY when the event was
        // actually sent (the disabled path above stays silent).
        eprintln!(
            "aura-voice: sent conversation.item.truncate (item {item_id}, audio_end_ms \
             {audio_end_ms})."
        );
        Ok(())
    }

    async fn clear_user_audio(&mut self) -> Result<(), VoiceError> {
        if !self.manual_turn_detection {
            return Ok(());
        }
        self.send(wire::input_audio_buffer_clear_event()).await
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

    async fn commit_user_turn(&mut self) -> Result<(), VoiceError> {
        if !self.manual_turn_detection {
            return self.request_response().await;
        }
        self.send(wire::input_audio_buffer_commit_event()).await?;
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
struct RealtimeStream {
    read: WsRead,
}

#[async_trait::async_trait]
impl VoiceStream for RealtimeStream {
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

/// Ground-truth probe: on the VERY FIRST output-audio delta of the process,
/// log whether the provider attached an `item_id`. This is the only reliable
/// way to know whether barge-in `conversation.item.truncate` can target the
/// item (xAI's docs don't render the delta's field schema). Fires once, is
/// free after that, and never prints audio — safe to leave in.
fn debug_first_item_id(item_id: Option<&str>) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| match item_id {
        Some(id) => eprintln!(
            "aura-voice: first output-audio delta carries item_id={id:?} — barge-in truncate CAN target it"
        ),
        None => eprintln!(
            "aura-voice: first output-audio delta has NO item_id — barge-in truncate cannot target this response"
        ),
    });
}

/// Map a parsed [`ServerEvent`] to a [`VoiceEvent`]. Returns `None` for events
/// the engine doesn't care about (so the stream keeps reading).
fn map_event(event: ServerEvent) -> Option<Result<VoiceEvent, VoiceError>> {
    let mapped = match event {
        ServerEvent::SessionCreated { .. } => VoiceEvent::SessionReady,
        ServerEvent::OutputAudioDelta { delta, item_id } => {
            debug_first_item_id(item_id.as_deref());
            match wire::base64_to_pcm16(&delta) {
                Ok(pcm) => VoiceEvent::OutputAudio { pcm, item_id },
                Err(e) => VoiceEvent::Error(VoiceError::Protocol(format!("bad audio delta: {e}"))),
            }
        }
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
        ServerEvent::ItemTruncated {
            item_id,
            audio_end_ms,
        } => {
            // Confirmation that the provider accepted our barge-in truncate.
            // Observability only (proves the heard-position sync landed); the
            // engine keeps reading.
            eprintln!(
                "aura-voice: provider confirmed conversation.item.truncated (item {}, audio_end_ms {})",
                item_id.as_deref().unwrap_or("?"),
                audio_end_ms.map(|m| m.to_string()).unwrap_or_else(|| "?".into())
            );
            return None;
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
    fn maps_audio_delta_to_pcm_with_item_id() {
        let pcm = vec![1i16, -2, 3];
        let b64 = wire::pcm16_to_base64(&pcm);
        let ev = map_event(ServerEvent::OutputAudioDelta {
            delta: b64,
            item_id: Some("it_1".into()),
        })
        .unwrap()
        .unwrap();
        match ev {
            VoiceEvent::OutputAudio { pcm: p, item_id } => {
                assert_eq!(p, pcm);
                assert_eq!(item_id.as_deref(), Some("it_1"));
            }
            other => panic!("expected OutputAudio, got {other:?}"),
        }
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

    #[test]
    fn crypto_provider_installs() {
        // Verifies the rustls provider fix without a network call: after
        // ensure_crypto_provider, a process-default provider exists (so
        // tokio-tungstenite can build a TLS config instead of panicking).
        ensure_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
