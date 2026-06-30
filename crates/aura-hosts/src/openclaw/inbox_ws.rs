//! Runtime-inbox WebSocket callback transport (the `tool_result` leg).
//!
//! Implements the reply leg of OpenClaw's `runtime-inbox-client.js handleToolCall`: the async
//! result of an `openclaw_agent_consult` is delivered back as an encrypted
//! `tool_result` frame over the runtime-inbox WS. The frame is
//! `{ type: "tool_result", tool_call_id, result_b64u }` when the per-call
//! tool-envelope key is known (encrypted path), or `{ ..., result }` when the
//! inbound was plaintext.
//!
//! ## Degrades gracefully
//!
//! The runtime-inbox WS is hosted and NOT reachable on the build machine, so
//! this is **implemented but unverifiable** here. A missing endpoint or send
//! failure returns a [`InboxWsError`]; it never panics and never hangs (a
//! bounded timeout wraps the send).

use std::time::Duration;

use futures_util::SinkExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};

use super::crypto::{self, KEY_LEN};

/// Errors from the runtime-inbox callback transport.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InboxWsError {
    #[error("runtime_inbox_endpoint_missing")]
    EndpointMissing,
    #[error("runtime_inbox_connect_failed: {0}")]
    ConnectFailed(String),
    #[error("runtime_inbox_send_failed: {0}")]
    SendFailed(String),
    #[error("runtime_inbox_encrypt_failed: {0}")]
    EncryptFailed(String),
    #[error("runtime_inbox_timeout")]
    Timeout,
}

/// The destination + per-call envelope key for one callback delivery.
#[derive(Debug, Clone)]
pub struct InboxTarget {
    /// The runtime-inbox WS endpoint.
    pub endpoint: String,
    /// The `tool_call_id` the inbound consult arrived with.
    pub tool_call_id: String,
    /// The per-call tool-envelope key (base64url, 32 bytes). `None` when the
    /// inbound was plaintext (the result is sent unencrypted).
    pub key_b64u: Option<String>,
    pub timeout: Duration,
}

/// Build the `tool_result` frame for `result`, encrypting it into `result_b64u`
/// when a key is present (mirrors `encryptToolResultEnvelope`). Pure; exposed
/// for unit testing without a socket.
pub fn build_tool_result_frame(
    target: &InboxTarget,
    result: &Value,
) -> Result<Value, InboxWsError> {
    let body = serde_json::to_string(result).unwrap_or_else(|_| "{}".to_owned());
    match &target.key_b64u {
        Some(key_b64u) => {
            let key = crypto::decode_key(key_b64u)
                .map_err(|e| InboxWsError::EncryptFailed(e.to_string()))?;
            let key32: &[u8; KEY_LEN] = &key;
            let wire = crypto::encrypt_aes_gcm(body.as_bytes(), key32)
                .map_err(|e| InboxWsError::EncryptFailed(e.to_string()))?;
            Ok(json!({
                "type": "tool_result",
                "tool_call_id": target.tool_call_id,
                "result_b64u": wire,
            }))
        }
        None => Ok(json!({
            "type": "tool_result",
            "tool_call_id": target.tool_call_id,
            "result": result,
        })),
    }
}

/// Send the encrypted `tool_result` frame over a fresh runtime-inbox WS
/// connection. Bounded by `target.timeout`; degrades to an [`InboxWsError`].
pub async fn send_tool_result(target: &InboxTarget, result: &Value) -> Result<(), InboxWsError> {
    if target.endpoint.trim().is_empty() {
        return Err(InboxWsError::EndpointMissing);
    }
    let frame = build_tool_result_frame(target, result)?;
    let timeout = target
        .timeout
        .clamp(Duration::from_secs(1), Duration::from_secs(120));
    match tokio::time::timeout(timeout, send_once(&target.endpoint, frame)).await {
        Ok(result) => result,
        Err(_) => Err(InboxWsError::Timeout),
    }
}

async fn send_once(endpoint: &str, frame: Value) -> Result<(), InboxWsError> {
    let request = endpoint
        .into_client_request()
        .map_err(|e| InboxWsError::ConnectFailed(e.to_string()))?;
    let (mut ws, _resp) = connect_async(request)
        .await
        .map_err(|e| InboxWsError::ConnectFailed(e.to_string()))?;
    ws.send(Message::text(frame.to_string()))
        .await
        .map_err(|e| InboxWsError::SendFailed(e.to_string()))?;
    // Best-effort close; ignore close errors.
    let _ = ws.close(None).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openclaw::crypto::{self, decode_key};

    #[test]
    fn encrypted_frame_round_trips() {
        let mut key = [0_u8; KEY_LEN];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let key_b64u = crypto::b64u_encode(&key);
        let target = InboxTarget {
            endpoint: "ws://x/".to_owned(),
            tool_call_id: "tc-1".to_owned(),
            key_b64u: Some(key_b64u.clone()),
            timeout: Duration::from_secs(5),
        };
        let result = json!({ "summary": "all done", "status": "completed" });
        let frame = build_tool_result_frame(&target, &result).unwrap();
        assert_eq!(frame["type"], "tool_result");
        assert_eq!(frame["tool_call_id"], "tc-1");
        let wire = frame["result_b64u"].as_str().unwrap();

        // Decrypt back with the same key and confirm the JSON survives.
        let decoded = decode_key(&key_b64u).unwrap();
        let key32: &[u8; KEY_LEN] = &decoded;
        let plaintext = crypto::decrypt_aes_gcm(wire, key32).unwrap();
        let back: Value = serde_json::from_slice(&plaintext).unwrap();
        assert_eq!(back, result);
        assert!(frame.get("result").is_none());
    }

    #[test]
    fn plaintext_frame_when_no_key() {
        let target = InboxTarget {
            endpoint: "ws://x/".to_owned(),
            tool_call_id: "tc-2".to_owned(),
            key_b64u: None,
            timeout: Duration::from_secs(5),
        };
        let result = json!({ "summary": "ok" });
        let frame = build_tool_result_frame(&target, &result).unwrap();
        assert_eq!(frame["result"], result);
        assert!(frame.get("result_b64u").is_none());
    }

    #[test]
    fn bad_key_is_a_clean_encrypt_error() {
        let target = InboxTarget {
            endpoint: "ws://x/".to_owned(),
            tool_call_id: "tc-3".to_owned(),
            key_b64u: Some(crypto::b64u_encode(&[0_u8; 16])), // 16 bytes != 32
            timeout: Duration::from_secs(5),
        };
        let err = build_tool_result_frame(&target, &json!({})).unwrap_err();
        assert!(matches!(err, InboxWsError::EncryptFailed(_)));
    }

    #[tokio::test]
    async fn send_to_unreachable_endpoint_degrades() {
        let target = InboxTarget {
            endpoint: "ws://127.0.0.1:9/".to_owned(),
            tool_call_id: "tc-4".to_owned(),
            key_b64u: None,
            timeout: Duration::from_secs(2),
        };
        let err = send_tool_result(&target, &json!({ "summary": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            InboxWsError::ConnectFailed(_) | InboxWsError::Timeout
        ));
    }

    #[tokio::test]
    async fn send_with_empty_endpoint_is_missing() {
        let target = InboxTarget {
            endpoint: "  ".to_owned(),
            tool_call_id: "tc".to_owned(),
            key_b64u: None,
            timeout: Duration::from_secs(2),
        };
        let err = send_tool_result(&target, &json!({})).await.unwrap_err();
        assert!(matches!(err, InboxWsError::EndpointMissing));
    }
}
