//! The connection string and the on-wire datagram framing.
//!
//! Connection string: `aura://<host>:<port>#k=<base64url-32B>&c=<call_id>`. The
//! secret rides in the **fragment** (`#…`) — by convention off the request
//! line / logs — and the client takes the whole string from `AURA_CONNECT` /
//! stdin, never argv.
//!
//! Datagrams (one UDP packet each): a 1-byte tag then a body.
//! - `0x01` handshake: body = a raw Noise handshake message.
//! - `0x02` transport: body = `[nonce: u64 BE][ciphertext]`. The explicit
//!   per-packet nonce lets the receiver decrypt out-of-order / past loss
//!   (snow stateless transport), which is what makes Noise-over-UDP work.

use crate::session::{SecretError, SessionSecret};

/// Datagram tag: a Noise handshake message.
pub const TAG_HANDSHAKE: u8 = 0x01;
/// Datagram tag: an encrypted transport frame (`[nonce u64 BE][ciphertext]`).
pub const TAG_TRANSPORT: u8 = 0x02;
/// `aura://` scheme prefix.
const SCHEME: &str = "aura://";
const CONTROL_PREFIX: &[u8] = b"AURA_CTRL_1";
const CONTROL_PTT_OPEN: u8 = 1;
const CONTROL_PTT_CLOSE: u8 = 2;
const CONTROL_PTT_CANCEL: u8 = 3;

/// Authenticated in-band control events carried over the same encrypted tunnel
/// as audio. They ride the SAME jitter buffer as audio on the receive side so
/// delivery order matches send order (a control that overtakes the audio sent
/// before it would commit a turn missing its trailing frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelControl {
    PttOpen,
    PttClose,
    /// Abandon the open turn WITHOUT committing it: the client discarded a
    /// too-short recording, so the server must drop the already-streamed
    /// frames from the provider input buffer (a plain `PttClose` would commit
    /// them and answer a message the user explicitly discarded).
    PttCancel,
}

/// What the server can receive from the client side of the tunnel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelInput {
    Audio(Vec<i16>),
    Control(TunnelControl),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConnError {
    #[error("connection string must start with `aura://`")]
    Scheme,
    #[error("connection string is missing the `#k=…&c=…` fragment")]
    Fragment,
    #[error("connection string is missing host:port")]
    Authority,
    #[error("connection string is missing the `k=` secret")]
    MissingSecret,
    #[error("connection string is missing the `c=` call id")]
    MissingCallId,
    #[error("session secret: {0}")]
    Secret(#[from] SecretError),
}

/// Which transport the connection string selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Direct Noise/UDP: `authority` is `host:port`.
    Direct,
    /// iroh QUIC P2P: `authority` is the server's `EndpointId` (base32); the
    /// client resolves its addresses via iroh discovery.
    Iroh,
}

/// Input mode selected by the server and carried in the connection string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelInputMode {
    Voice,
    PushToTalk,
}

/// A parsed connection string. For [`TransportKind::Direct`] `authority` is the
/// `host:port` to dial; for [`TransportKind::Iroh`] it is the server's
/// `EndpointId`.
#[derive(Debug)]
pub struct ConnectionString {
    pub authority: String,
    pub call_id: String,
    pub secret: SessionSecret,
    pub transport: TransportKind,
    pub input_mode: Option<TunnelInputMode>,
}

impl ConnectionString {
    /// Build a DIRECT (Noise/UDP) connection string for `host:port`.
    pub fn format_direct(
        authority: &str,
        call_id: &str,
        secret: &SessionSecret,
        input_mode: TunnelInputMode,
    ) -> String {
        format!(
            "{SCHEME}{authority}#k={}&c={}&t=direct&m={}",
            secret.to_base64url(),
            call_id,
            encode_input_mode(input_mode)
        )
    }

    /// Build an IROH connection string for a server `EndpointId`. The client
    /// resolves the server's addresses via iroh discovery (no host:port needed).
    pub fn format_iroh(
        endpoint_id: &str,
        call_id: &str,
        secret: &SessionSecret,
        input_mode: TunnelInputMode,
    ) -> String {
        format!(
            "{SCHEME}{endpoint_id}#k={}&c={}&t=iroh&m={}",
            secret.to_base64url(),
            call_id,
            encode_input_mode(input_mode)
        )
    }

    /// Parse a connection string from `AURA_CONNECT` / stdin.
    pub fn parse(s: &str) -> Result<Self, ConnError> {
        let s = s.trim();
        let rest = s.strip_prefix(SCHEME).ok_or(ConnError::Scheme)?;
        let (authority, fragment) = rest.split_once('#').ok_or(ConnError::Fragment)?;
        if authority.is_empty() {
            return Err(ConnError::Authority);
        }
        let mut secret_b64: Option<&str> = None;
        let mut call_id: Option<&str> = None;
        let mut transport = TransportKind::Direct; // legacy strings (no `t=`) are direct
        let mut input_mode = None; // legacy strings fall back to local env.
        for pair in fragment.split('&') {
            if let Some(v) = pair.strip_prefix("k=") {
                secret_b64 = Some(v);
            } else if let Some(v) = pair.strip_prefix("c=") {
                call_id = Some(v);
            } else if let Some(v) = pair.strip_prefix("t=") {
                transport = match v {
                    "iroh" => TransportKind::Iroh,
                    _ => TransportKind::Direct,
                };
            } else if let Some(v) = pair.strip_prefix("m=") {
                input_mode = parse_input_mode_tag(v);
            }
        }
        let secret = SessionSecret::from_base64url(secret_b64.ok_or(ConnError::MissingSecret)?)?;
        let call_id = call_id.ok_or(ConnError::MissingCallId)?;
        if call_id.is_empty() {
            return Err(ConnError::MissingCallId);
        }
        Ok(Self {
            authority: authority.to_owned(),
            call_id: call_id.to_owned(),
            secret,
            transport,
            input_mode,
        })
    }
}

pub fn encode_input_mode(mode: TunnelInputMode) -> &'static str {
    match mode {
        TunnelInputMode::Voice => "voice",
        TunnelInputMode::PushToTalk => "ptt",
    }
}

fn parse_input_mode_tag(raw: &str) -> Option<TunnelInputMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "voice" | "vad" => Some(TunnelInputMode::Voice),
        "push_to_talk" | "push-to-talk" | "ptt" => Some(TunnelInputMode::PushToTalk),
        _ => None,
    }
}

/// Encode a transport datagram: `0x02 || nonce(8 BE) || ciphertext`.
pub fn encode_transport(nonce: u64, ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8 + ciphertext.len());
    out.push(TAG_TRANSPORT);
    out.extend_from_slice(&nonce.to_be_bytes());
    out.extend_from_slice(ciphertext);
    out
}

/// Decode a transport datagram body (after the tag), returning `(nonce,
/// ciphertext)`. Returns `None` if too short to hold a nonce.
pub fn decode_transport(body: &[u8]) -> Option<(u64, &[u8])> {
    if body.len() < 8 {
        return None;
    }
    let mut n = [0u8; 8];
    n.copy_from_slice(&body[..8]);
    Some((u64::from_be_bytes(n), &body[8..]))
}

pub fn encode_tunnel_control(control: TunnelControl) -> Vec<u8> {
    let code = match control {
        TunnelControl::PttOpen => CONTROL_PTT_OPEN,
        TunnelControl::PttClose => CONTROL_PTT_CLOSE,
        TunnelControl::PttCancel => CONTROL_PTT_CANCEL,
    };
    let mut out = Vec::with_capacity(CONTROL_PREFIX.len() + 1);
    out.extend_from_slice(CONTROL_PREFIX);
    out.push(code);
    out
}

pub fn decode_tunnel_control(body: &[u8]) -> Option<TunnelControl> {
    let code = *body.strip_prefix(CONTROL_PREFIX)?.first()?;
    match code {
        CONTROL_PTT_OPEN => Some(TunnelControl::PttOpen),
        CONTROL_PTT_CLOSE => Some(TunnelControl::PttClose),
        CONTROL_PTT_CANCEL => Some(TunnelControl::PttCancel),
        _ => None,
    }
}

/// Is this decrypted frame payload a control frame? Used by the outbound queue
/// to never evict a control on overflow (dropping audio degrades quality;
/// dropping a `PttClose` strands the whole turn).
pub(crate) fn is_tunnel_control(body: &[u8]) -> bool {
    decode_tunnel_control(body).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_connection_string_round_trips() {
        let secret = SessionSecret::generate().unwrap();
        let s = ConnectionString::format_direct(
            "203.0.113.7:9443",
            "call-abc12345",
            &secret,
            TunnelInputMode::Voice,
        );
        assert!(s.starts_with("aura://203.0.113.7:9443#k="));
        assert!(s.contains("&t=direct"));
        let parsed = ConnectionString::parse(&s).unwrap();
        assert_eq!(parsed.authority, "203.0.113.7:9443");
        assert_eq!(parsed.call_id, "call-abc12345");
        assert_eq!(parsed.secret.as_bytes(), secret.as_bytes());
        assert_eq!(parsed.transport, TransportKind::Direct);
        assert_eq!(parsed.input_mode, Some(TunnelInputMode::Voice));
    }

    #[test]
    fn iroh_connection_string_round_trips() {
        let secret = SessionSecret::generate().unwrap();
        // An opaque EndpointId-like authority (base32; no host:port).
        let id = "ci6ej5hsqs4u4xx7m4t4i7s2yqd3kq4gqf6h2c4xk7h6w7a";
        let s = ConnectionString::format_iroh(
            id,
            "call-iroh0001",
            &secret,
            TunnelInputMode::PushToTalk,
        );
        assert!(s.contains("&t=iroh"));
        let parsed = ConnectionString::parse(&s).unwrap();
        assert_eq!(parsed.authority, id);
        assert_eq!(parsed.call_id, "call-iroh0001");
        assert_eq!(parsed.secret.as_bytes(), secret.as_bytes());
        assert_eq!(parsed.transport, TransportKind::Iroh);
        assert_eq!(parsed.input_mode, Some(TunnelInputMode::PushToTalk));
    }

    #[test]
    fn legacy_string_without_tag_parses_as_direct() {
        let secret = SessionSecret::generate().unwrap();
        let s = format!("aura://h:1#k={}&c=call-x", secret.to_base64url());
        assert_eq!(
            ConnectionString::parse(&s).unwrap().transport,
            TransportKind::Direct
        );
        assert_eq!(ConnectionString::parse(&s).unwrap().input_mode, None);
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(matches!(
            ConnectionString::parse("http://x#k=a&c=b"),
            Err(ConnError::Scheme)
        ));
        assert!(matches!(
            ConnectionString::parse("aura://host:1"),
            Err(ConnError::Fragment)
        ));
        assert!(matches!(
            ConnectionString::parse("aura://#k=a&c=b"),
            Err(ConnError::Authority)
        ));
        assert!(matches!(
            ConnectionString::parse("aura://h:1#c=call-x"),
            Err(ConnError::MissingSecret)
        ));
        let good_secret = SessionSecret::generate().unwrap().to_base64url();
        assert!(matches!(
            ConnectionString::parse(&format!("aura://h:1#k={good_secret}")),
            Err(ConnError::MissingCallId)
        ));
    }

    #[test]
    fn transport_framing_round_trips() {
        let ct = b"ciphertext-bytes";
        let dg = encode_transport(0x0102_0304_0506_0708, ct);
        assert_eq!(dg[0], TAG_TRANSPORT);
        let (nonce, body) = decode_transport(&dg[1..]).unwrap();
        assert_eq!(nonce, 0x0102_0304_0506_0708);
        assert_eq!(body, ct);
        assert!(decode_transport(&[0, 1, 2]).is_none(), "too short → None");
    }

    #[test]
    fn tunnel_control_round_trips() {
        assert_eq!(
            decode_tunnel_control(&encode_tunnel_control(TunnelControl::PttOpen)),
            Some(TunnelControl::PttOpen)
        );
        assert_eq!(
            decode_tunnel_control(&encode_tunnel_control(TunnelControl::PttClose)),
            Some(TunnelControl::PttClose)
        );
        assert_eq!(
            decode_tunnel_control(&encode_tunnel_control(TunnelControl::PttCancel)),
            Some(TunnelControl::PttCancel)
        );
        assert_eq!(decode_tunnel_control(b"not-control"), None);
    }
}
