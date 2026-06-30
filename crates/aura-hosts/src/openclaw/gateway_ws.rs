//! Live OpenClaw gateway WebSocket transport (single request/response).
//!
//! Mirrors OpenClaw's `openclaw-gateway-ws.js`: open a WS to the gateway, answer the
//! `connect.challenge` with an Ed25519 device-signed `connect` frame, wait for
//! the connect `res ok`, send ONE request RPC frame, and return its payload.
//!
//! ## Degrades gracefully (the live-test boundary)
//!
//! The gateway (`ws://127.0.0.1:18789`) and a paired device-state directory are
//! NOT available on the build/CI machine, so this transport is **implemented
//! but unverifiable** here. Every failure path — connect refused, no device
//! state, challenge missing, RPC rejected, timeout — returns a
//! [`GatewayWsError`]; it never panics and never hangs (a bounded timeout wraps
//! the whole exchange).

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use zeroize::Zeroizing;

/// The four operator scopes the connect frame requests by default.
const DEFAULT_SCOPES: &[&str] = &[
    "operator.admin",
    "operator.approvals",
    "operator.pairing",
    "operator.read",
    "operator.talk.secrets",
    "operator.write",
];

/// The PKCS8-v1 DER prefix for an Ed25519 private key (`OneAsymmetricKey` with a
/// 32-byte raw seed following). Used to extract the seed from a PEM device key.
const PKCS8_ED25519_PREFIX: &[u8] = &[
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// Errors from the gateway WS transport. The `code()` strings mirror the JS
/// `OpenClawGatewayWsError` codes so logs/tests can match on them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GatewayWsError {
    #[error("openclaw_gateway_{0}_missing")]
    Missing(String),
    #[error("openclaw_gateway_device_state_unavailable: {0}")]
    DeviceStateUnavailable(String),
    #[error("openclaw_gateway_connect_failed: {0}")]
    ConnectFailed(String),
    #[error("openclaw_gateway_connect_challenge_missing")]
    ConnectChallengeMissing,
    #[error("openclaw_gateway_rpc_rejected: {0}")]
    RpcRejected(String),
    #[error("openclaw_gateway_rpc_timeout")]
    Timeout,
    #[error("openclaw_gateway_rpc_closed")]
    Closed,
    #[error("openclaw_gateway_protocol: {0}")]
    Protocol(String),
}

/// A loaded, paired device identity. The signing key seed is wiped on drop.
pub struct GatewayDevice {
    pub device_id: String,
    signing_key: SigningKey,
    public_key_raw: [u8; 32],
    pub token: String,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for GatewayDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the signing key.
        f.debug_struct("GatewayDevice")
            .field("device_id", &self.device_id)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// Load the paired Ed25519 device from `<state_dir>/identity/device.json` (+
/// `device-auth.json`). Mirrors `loadOpenClawGatewayDevice`. Fail-soft: any
/// read/parse error becomes a [`GatewayWsError`].
pub fn load_gateway_device(state_dir: &Path) -> Result<GatewayDevice, GatewayWsError> {
    let identity_path = state_dir.join("identity").join("device.json");
    let auth_path = state_dir.join("identity").join("device-auth.json");
    let identity = read_json(&identity_path)?;
    let auth = read_json(&auth_path)?;

    let device_id = required_str(&identity, "deviceId")?;
    let signing_key = load_signing_key(&identity)?;
    let public_key_raw = signing_key.verifying_key().to_bytes();

    // Token + scopes live under auth.tokens.operator.
    let operator = auth
        .get("tokens")
        .and_then(|t| t.get("operator"))
        .and_then(Value::as_object);
    let token = operator
        .and_then(|o| o.get("token"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let scopes = operator
        .and_then(|o| o.get("scopes"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    Ok(GatewayDevice {
        device_id,
        signing_key,
        public_key_raw,
        token,
        scopes,
    })
}

fn read_json(path: &Path) -> Result<Value, GatewayWsError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| GatewayWsError::DeviceStateUnavailable(format!("{}: {e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| GatewayWsError::DeviceStateUnavailable(format!("{}: {e}", path.display())))
}

fn required_str(value: &Value, key: &str) -> Result<String, GatewayWsError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| GatewayWsError::Missing(key.to_owned()))
}

/// Build the Ed25519 signing key from the device identity. Accepts either a raw
/// 32-byte seed (base64/base64url under `privateKeySeed`/`seed`) or a PKCS8 PEM
/// (`privateKeyPem`) from which the seed is extracted.
fn load_signing_key(identity: &Value) -> Result<SigningKey, GatewayWsError> {
    if let Some(seed_b64) = identity
        .get("privateKeySeed")
        .or_else(|| identity.get("seed"))
        .and_then(Value::as_str)
    {
        let seed = decode_b64_any(seed_b64)?;
        if seed.len() != 32 {
            return Err(GatewayWsError::DeviceStateUnavailable(format!(
                "ed25519 seed must be 32 bytes, got {}",
                seed.len()
            )));
        }
        let mut bytes = Zeroizing::new([0_u8; 32]);
        bytes.copy_from_slice(&seed);
        return Ok(SigningKey::from_bytes(&bytes));
    }
    if let Some(pem) = identity.get("privateKeyPem").and_then(Value::as_str) {
        let der = pem_to_der(pem)?;
        let seed = extract_pkcs8_ed25519_seed(&der)?;
        let mut bytes = Zeroizing::new([0_u8; 32]);
        bytes.copy_from_slice(&seed);
        return Ok(SigningKey::from_bytes(&bytes));
    }
    Err(GatewayWsError::Missing("privateKey".to_owned()))
}

fn decode_b64_any(text: &str) -> Result<Vec<u8>, GatewayWsError> {
    let trimmed = text.trim().trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| STANDARD.decode(text.trim()))
        .map_err(|_| GatewayWsError::DeviceStateUnavailable("device key b64 invalid".to_owned()))
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>, GatewayWsError> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .concat();
    STANDARD
        .decode(body.trim())
        .map_err(|_| GatewayWsError::DeviceStateUnavailable("device key PEM invalid".to_owned()))
}

fn extract_pkcs8_ed25519_seed(der: &[u8]) -> Result<[u8; 32], GatewayWsError> {
    if der.len() >= PKCS8_ED25519_PREFIX.len() + 32 && der.starts_with(PKCS8_ED25519_PREFIX) {
        let start = PKCS8_ED25519_PREFIX.len();
        let mut seed = [0_u8; 32];
        seed.copy_from_slice(&der[start..start + 32]);
        return Ok(seed);
    }
    Err(GatewayWsError::DeviceStateUnavailable(
        "unsupported PKCS8 ed25519 device key layout".to_owned(),
    ))
}

fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The connect-frame auth payload (the bytes the device signs). Mirrors
/// `buildDeviceAuthPayload` (`v3` layout).
#[allow(clippy::too_many_arguments)]
fn device_auth_payload(
    device_id: &str,
    role: &str,
    scopes: &[String],
    signed_at_ms: u128,
    token: &str,
    nonce: &str,
    platform: &str,
    device_family: &str,
) -> String {
    [
        "v3",
        device_id,
        "cli",
        "cli",
        role,
        &scopes.join(","),
        &signed_at_ms.to_string(),
        token,
        nonce,
        &normalize_device_metadata(platform),
        &normalize_device_metadata(device_family),
    ]
    .join("|")
}

fn normalize_device_metadata(value: &str) -> String {
    value.trim().replace('|', "-").chars().take(80).collect()
}

fn current_platform() -> &'static str {
    std::env::consts::OS
}

/// Build the signed `connect` request frame for the given challenge nonce.
fn build_connect_frame(
    connect_id: &str,
    nonce: &str,
    device: &GatewayDevice,
    extra_token: &str,
) -> Value {
    let role = "operator";
    let scopes: Vec<String> = if device.scopes.is_empty() {
        DEFAULT_SCOPES.iter().map(|s| (*s).to_owned()).collect()
    } else {
        device.scopes.clone()
    };
    let auth_token = if extra_token.trim().is_empty() {
        device.token.clone()
    } else {
        extra_token.trim().to_owned()
    };
    let signed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let platform = current_platform();
    let payload = device_auth_payload(
        &device.device_id,
        role,
        &scopes,
        signed_at_ms,
        &auth_token,
        nonce,
        platform,
        "",
    );
    let signature = device.signing_key.sign(payload.as_bytes());

    let auth = if auth_token.is_empty() {
        Value::Null
    } else {
        json!({
            "token": auth_token,
            "deviceToken": if device.token.is_empty() { Value::Null } else { json!(device.token) },
        })
    };

    json!({
        "type": "req",
        "id": connect_id,
        "method": "connect",
        "params": {
            "minProtocol": 4,
            "maxProtocol": 4,
            "client": { "id": "cli", "version": "codexini-openclaw", "platform": platform, "mode": "cli" },
            "auth": auth,
            "role": role,
            "scopes": scopes,
            "device": {
                "id": device.device_id,
                "publicKey": b64u(&device.public_key_raw),
                "signature": b64u(&signature.to_bytes()),
                "signedAt": signed_at_ms as u64,
                "nonce": nonce,
            },
        },
    })
}

/// Configuration for one gateway RPC.
#[derive(Debug, Clone)]
pub struct GatewayWsConfig {
    pub endpoint: String,
    pub state_dir: PathBuf,
    pub token: String,
    pub timeout: Duration,
}

/// Perform ONE gateway request/response over a fresh WS connection. Returns the
/// response payload on success. Bounded by `config.timeout`; every error path
/// degrades to a [`GatewayWsError`] (never panics, never hangs).
pub async fn request_gateway_ws(
    config: &GatewayWsConfig,
    method: &str,
    params: Value,
) -> Result<Value, GatewayWsError> {
    if config.endpoint.trim().is_empty() {
        return Err(GatewayWsError::Missing("endpoint".to_owned()));
    }
    // Load the device BEFORE connecting; a missing pairing is a clean error.
    let device = load_gateway_device(&config.state_dir)?;
    let timeout = config
        .timeout
        .clamp(Duration::from_secs(1), Duration::from_secs(300));

    let fut = run_exchange(config, &device, method, params);
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(GatewayWsError::Timeout),
    }
}

async fn run_exchange(
    config: &GatewayWsConfig,
    device: &GatewayDevice,
    method: &str,
    params: Value,
) -> Result<Value, GatewayWsError> {
    let request = config
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|e| GatewayWsError::ConnectFailed(e.to_string()))?;
    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| GatewayWsError::ConnectFailed(e.to_string()))?;
    let (mut write, mut read) = ws.split();

    let connect_id = format!("codexini-connect-{}", unique_suffix());
    let request_id = format!("codexini-req-{}", unique_suffix());
    let mut challenge_seen = false;
    let mut connected = false;

    while let Some(frame) = read.next().await {
        let msg = match frame {
            Ok(m) => m,
            Err(e) => {
                return Err(if connected {
                    GatewayWsError::RpcRejected(e.to_string())
                } else {
                    GatewayWsError::ConnectFailed(e.to_string())
                });
            }
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Close(_) => {
                return Err(if !challenge_seen {
                    GatewayWsError::ConnectChallengeMissing
                } else {
                    GatewayWsError::Closed
                });
            }
            // Ping/Pong/Frame: nothing to parse.
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };

        // connect.challenge -> sign + send connect frame.
        if value.get("event").and_then(Value::as_str) == Some("connect.challenge") {
            let nonce = value
                .get("payload")
                .and_then(|p| p.get("nonce"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| GatewayWsError::Protocol("challenge nonce missing".to_owned()))?;
            challenge_seen = true;
            let connect_frame = build_connect_frame(&connect_id, nonce, device, &config.token);
            write
                .send(Message::text(connect_frame.to_string()))
                .await
                .map_err(|e| GatewayWsError::ConnectFailed(e.to_string()))?;
            continue;
        }

        if value.get("type").and_then(Value::as_str) != Some("res") {
            continue;
        }
        let id = value.get("id").and_then(Value::as_str).unwrap_or("");

        // connect response.
        if id == connect_id {
            if value.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(GatewayWsError::ConnectFailed(error_message(&value)));
            }
            connected = true;
            let req = json!({
                "type": "req",
                "id": request_id,
                "method": method,
                "params": params,
            });
            write
                .send(Message::text(req.to_string()))
                .await
                .map_err(|e| GatewayWsError::RpcRejected(e.to_string()))?;
            continue;
        }

        // the one RPC response.
        if id == request_id {
            if value.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(value
                    .get("payload")
                    .cloned()
                    .or_else(|| value.get("result").cloned())
                    .unwrap_or(Value::Null));
            }
            return Err(GatewayWsError::RpcRejected(error_message(&value)));
        }
    }

    // Stream ended without a response.
    Err(if !challenge_seen {
        GatewayWsError::ConnectChallengeMissing
    } else {
        GatewayWsError::Closed
    })
}

fn error_message(frame: &Value) -> String {
    frame
        .get("error")
        .and_then(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .or_else(|| e.get("code").and_then(Value::as_str))
        })
        .unwrap_or("gateway error")
        .to_owned()
}

fn unique_suffix() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut rnd = [0_u8; 6];
    let _ = getrandom::getrandom(&mut rnd);
    format!("{now}-{}", b64u(&rnd))
}

/// Best-effort sanity check that a [`VerifyingKey`] can be reconstructed from
/// the device's raw public key (used only in tests / diagnostics).
pub fn verifying_key_from_raw(raw: &[u8; 32]) -> Option<VerifyingKey> {
    VerifyingKey::from_bytes(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_device(dir: &Path, identity: Value, auth: Value) {
        let id_dir = dir.join("identity");
        std::fs::create_dir_all(&id_dir).unwrap();
        let mut f = std::fs::File::create(id_dir.join("device.json")).unwrap();
        f.write_all(identity.to_string().as_bytes()).unwrap();
        let mut g = std::fs::File::create(id_dir.join("device-auth.json")).unwrap();
        g.write_all(auth.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn load_device_from_raw_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = [7_u8; 32];
        let identity = json!({
            "deviceId": "dev-1",
            "privateKeySeed": b64u(&seed),
        });
        let auth = json!({ "tokens": { "operator": { "token": "tok-abc", "scopes": ["operator.read"] } } });
        write_device(tmp.path(), identity, auth);

        let device = load_gateway_device(tmp.path()).unwrap();
        assert_eq!(device.device_id, "dev-1");
        assert_eq!(device.token, "tok-abc");
        assert_eq!(device.scopes, vec!["operator.read".to_owned()]);
        // Public key matches the seed-derived signing key.
        let expected = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        assert_eq!(device.public_key_raw, expected);
    }

    #[test]
    fn missing_device_state_is_clean_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = load_gateway_device(tmp.path()).unwrap_err();
        assert!(matches!(err, GatewayWsError::DeviceStateUnavailable(_)));
    }

    #[test]
    fn connect_frame_is_well_formed_and_signature_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = [3_u8; 32];
        let identity = json!({ "deviceId": "dev-9", "privateKeySeed": b64u(&seed) });
        let auth = json!({ "tokens": { "operator": { "token": "T", "scopes": [] } } });
        write_device(tmp.path(), identity, auth);
        let device = load_gateway_device(tmp.path()).unwrap();

        let frame = build_connect_frame("cid-1", "nonce-xyz", &device, "");
        assert_eq!(frame["method"], "connect");
        assert_eq!(frame["id"], "cid-1");
        assert_eq!(frame["params"]["device"]["nonce"], "nonce-xyz");
        // Default scopes filled in when device has none.
        assert_eq!(
            frame["params"]["scopes"].as_array().unwrap().len(),
            DEFAULT_SCOPES.len()
        );
        // The signature must verify over the reconstructed payload.
        let sig_b64u = frame["params"]["device"]["signature"].as_str().unwrap();
        let sig_bytes = URL_SAFE_NO_PAD.decode(sig_b64u).unwrap();
        let sig = ed25519_dalek::Signature::from_slice(&sig_bytes).unwrap();
        let signed_at = frame["params"]["device"]["signedAt"].as_u64().unwrap() as u128;
        let payload = device_auth_payload(
            "dev-9",
            "operator",
            &DEFAULT_SCOPES
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>(),
            signed_at,
            "T",
            "nonce-xyz",
            current_platform(),
            "",
        );
        let vk = verifying_key_from_raw(&device.public_key_raw).unwrap();
        assert!(vk.verify_strict(payload.as_bytes(), &sig).is_ok());
    }

    #[tokio::test]
    async fn request_unreachable_gateway_degrades_to_error() {
        let tmp = tempfile::tempdir().unwrap();
        let seed = [1_u8; 32];
        let identity = json!({ "deviceId": "d", "privateKeySeed": b64u(&seed) });
        let auth = json!({ "tokens": { "operator": { "token": "t", "scopes": [] } } });
        write_device(tmp.path(), identity, auth);

        // Port 9 (discard) is reliably not a WS gateway -> connect fails fast.
        let config = GatewayWsConfig {
            endpoint: "ws://127.0.0.1:9/".to_owned(),
            state_dir: tmp.path().to_path_buf(),
            token: String::new(),
            timeout: Duration::from_secs(2),
        };
        let err = request_gateway_ws(&config, "tasks.get", json!({}))
            .await
            .unwrap_err();
        // Must be a clean error (connect-failed or timeout), never a panic/hang.
        assert!(matches!(
            err,
            GatewayWsError::ConnectFailed(_) | GatewayWsError::Timeout
        ));
    }

    #[tokio::test]
    async fn request_without_device_state_errors_before_connect() {
        let tmp = tempfile::tempdir().unwrap();
        let config = GatewayWsConfig {
            endpoint: "ws://127.0.0.1:18789/".to_owned(),
            state_dir: tmp.path().to_path_buf(),
            token: String::new(),
            timeout: Duration::from_secs(2),
        };
        let err = request_gateway_ws(&config, "tasks.get", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayWsError::DeviceStateUnavailable(_)));
    }
}
