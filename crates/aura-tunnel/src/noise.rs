//! Noise_NNpsk0 handshake + stateless transport.
//!
//! The per-call session secret is the **PSK** (position 0). NNpsk0 has no
//! static identity keys: a peer is authenticated purely by proving knowledge of
//! the shared secret, which is exactly the trust model — the secret was handed
//! to the client over the chat the user already trusts. A network attacker
//! without the PSK cannot complete the handshake as either side (the PSK is
//! mixed into the transcript), so the channel is mutually authenticated,
//! forward-secret, and MITM-resistant with no certificates.
//!
//! Transport is **stateless** (`snow` `*_stateless_transport_mode`): each frame
//! carries an explicit `u64` nonce, so frames decrypt correctly out-of-order /
//! after UDP loss. The state is immutable after the handshake, so it is shared
//! (`&self` read+write) across the send and receive tasks.

use snow::{HandshakeState, StatelessTransportState};

use crate::wire::{encode_input_mode, TunnelInputMode};

/// NNpsk0 with X25519 / ChaChaPoly / BLAKE2s.
pub const NOISE_PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// Authenticated application-protocol marker carried in both Noise handshake
/// messages. Incompatible transport framing must change this marker.
pub const PROTOCOL_MARKER: &[u8] = b"aura/direct/2";
/// AEAD tag length (ChaChaPoly) — transport ciphertext = plaintext + this.
pub const TAG_LEN: usize = 16;
/// Upper bound on a handshake message; NNpsk0 messages are well under this.
pub const MAX_HANDSHAKE_MSG: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    #[error("noise: {0}")]
    Snow(#[from] snow::Error),
    #[error("invalid Noise params (compile-time constant)")]
    Params,
    #[error("unsupported aura tunnel protocol version")]
    ProtocolVersion,
}

fn builder() -> Result<snow::Builder<'static>, NoiseError> {
    // The params string is a compile-time constant; parse cannot fail in
    // practice, but propagate the error rather than unwrap.
    let params: snow::params::NoiseParams = NOISE_PARAMS.parse().map_err(|_| NoiseError::Params)?;
    Ok(snow::Builder::new(params))
}

/// Build the initiator (client) handshake state with the session secret as PSK.
pub fn initiator(psk: &[u8]) -> Result<HandshakeState, NoiseError> {
    Ok(builder()?.psk(0, psk).build_initiator()?)
}

/// Build the responder (server) handshake state with the session secret as PSK.
pub fn responder(psk: &[u8]) -> Result<HandshakeState, NoiseError> {
    Ok(builder()?.psk(0, psk).build_responder()?)
}

/// Finalize a completed handshake into the stateless transport state.
pub fn finalize(hs: HandshakeState) -> Result<Transport, NoiseError> {
    Ok(Transport(hs.into_stateless_transport_mode()?))
}

/// The fixed byte length of the NNpsk0 first handshake message (deterministic
/// for the pattern). The server uses it to cheaply reject wrong-sized junk
/// datagrams BEFORE building an RNG-seeded responder (anti-amplification).
pub fn msg1_len(payload: &[u8]) -> usize {
    let fallback = 32 + payload.len() + TAG_LEN;
    match initiator(&[0u8; 32]) {
        Ok(mut hs) => {
            let mut buf = [0u8; MAX_HANDSHAKE_MSG];
            hs.write_message(payload, &mut buf).unwrap_or(fallback)
        }
        Err(_) => fallback,
    }
}

/// Build the authenticated application payload. Binding the server-selected
/// mode here prevents an edited connection-string fragment from silently
/// switching a PTT client into VAD mode or vice versa.
pub fn protocol_payload(mode: Option<TunnelInputMode>) -> Vec<u8> {
    let mode = mode.map(encode_input_mode).unwrap_or("unspecified");
    let mut out = Vec::with_capacity(PROTOCOL_MARKER.len() + 3 + mode.len());
    out.extend_from_slice(PROTOCOL_MARKER);
    out.extend_from_slice(b"|m=");
    out.extend_from_slice(mode.as_bytes());
    out
}

/// Verify the authenticated application payload from either handshake message.
pub fn verify_protocol_payload(
    payload: &[u8],
    expected_mode: Option<TunnelInputMode>,
) -> Result<(), NoiseError> {
    if payload == protocol_payload(expected_mode) {
        Ok(())
    } else {
        Err(NoiseError::ProtocolVersion)
    }
}

/// The post-handshake encrypt/decrypt surface. Immutable (`&self`) so it can be
/// shared across the UDP send and receive tasks; the nonce is supplied per
/// frame by the caller (the sender increments; the receiver uses the nonce
/// carried in the datagram).
pub struct Transport(StatelessTransportState);

impl Transport {
    /// Encrypt `plaintext` under `nonce`; returns `ciphertext || tag`.
    pub fn encrypt(&self, nonce: u64, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; plaintext.len() + TAG_LEN];
        let n = self.0.write_message(nonce, plaintext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Decrypt a frame sent under `nonce`. Fails (AEAD) on tamper/wrong key.
    pub fn decrypt(&self, nonce: u64, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; ciphertext.len()];
        let n = self.0.read_message(nonce, ciphertext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a full NNpsk0 handshake in memory (the message order endpoint.rs
    /// runs over UDP): `-> e` then `<- e, ee`.
    fn handshake(psk_i: &[u8], psk_r: &[u8]) -> Result<(Transport, Transport), NoiseError> {
        let mut ini = initiator(psk_i)?;
        let mut res = responder(psk_r)?;
        let mut a = [0u8; MAX_HANDSHAKE_MSG];
        let mut b = [0u8; MAX_HANDSHAKE_MSG];
        let marker = protocol_payload(None);
        let n1 = ini.write_message(&marker, &mut a)?; // msg1
        let p1 = res.read_message(&a[..n1], &mut b)?;
        verify_protocol_payload(&b[..p1], None)?;
        let n2 = res.write_message(&marker, &mut a)?; // msg2
        let p2 = ini.read_message(&a[..n2], &mut b)?;
        verify_protocol_payload(&b[..p2], None)?;
        assert!(ini.is_handshake_finished() && res.is_handshake_finished());
        Ok((finalize(ini)?, finalize(res)?))
    }

    #[test]
    fn matching_psk_handshakes_and_frames_round_trip() {
        let psk = [7u8; 32];
        let (client, server) = handshake(&psk, &psk).unwrap();
        // Client → server, possibly out of order: encrypt n=0 and n=1, decrypt 1 then 0.
        let c0 = client.encrypt(0, b"frame-zero").unwrap();
        let c1 = client.encrypt(1, b"frame-one").unwrap();
        assert_eq!(server.decrypt(1, &c1).unwrap(), b"frame-one");
        assert_eq!(server.decrypt(0, &c0).unwrap(), b"frame-zero");
        // Server → client direction too.
        let s = server.encrypt(0, b"model-audio").unwrap();
        assert_eq!(client.decrypt(0, &s).unwrap(), b"model-audio");
    }

    #[test]
    fn wrong_psk_fails_the_handshake() {
        let mut ini = initiator(&[1u8; 32]).unwrap();
        let mut res = responder(&[2u8; 32]).unwrap();
        let mut a = [0u8; MAX_HANDSHAKE_MSG];
        let mut b = [0u8; MAX_HANDSHAKE_MSG];
        let n1 = ini.write_message(&protocol_payload(None), &mut a).unwrap();
        // The responder must reject msg1 authenticated under a different PSK.
        assert!(res.read_message(&a[..n1], &mut b).is_err());
    }

    #[test]
    fn authenticated_protocol_mismatch_is_rejected() {
        let psk = [4u8; 32];
        let mut ini = initiator(&psk).unwrap();
        let mut res = responder(&psk).unwrap();
        let mut message = [0u8; MAX_HANDSHAKE_MSG];
        let mut payload = [0u8; MAX_HANDSHAKE_MSG];

        let n1 = ini.write_message(b"aura/direct/0", &mut message).unwrap();
        let p1 = res.read_message(&message[..n1], &mut payload).unwrap();
        assert!(matches!(
            verify_protocol_payload(&payload[..p1], None),
            Err(NoiseError::ProtocolVersion)
        ));

        let mut ini = initiator(&psk).unwrap();
        let mut legacy_res = responder(&psk).unwrap();
        let n1 = ini
            .write_message(&protocol_payload(None), &mut message)
            .unwrap();
        legacy_res
            .read_message(&message[..n1], &mut payload)
            .unwrap();
        let n2 = legacy_res.write_message(&[], &mut message).unwrap();
        let p2 = ini.read_message(&message[..n2], &mut payload).unwrap();
        assert!(matches!(
            verify_protocol_payload(&payload[..p2], None),
            Err(NoiseError::ProtocolVersion)
        ));
    }

    #[test]
    fn authenticated_input_mode_mismatch_is_rejected() {
        let psk = [8u8; 32];
        let mut ini = initiator(&psk).unwrap();
        let mut res = responder(&psk).unwrap();
        let mut message = [0u8; MAX_HANDSHAKE_MSG];
        let mut payload = [0u8; MAX_HANDSHAKE_MSG];
        let n1 = ini
            .write_message(
                &protocol_payload(Some(TunnelInputMode::PushToTalk)),
                &mut message,
            )
            .unwrap();
        let p1 = res.read_message(&message[..n1], &mut payload).unwrap();
        assert!(matches!(
            verify_protocol_payload(&payload[..p1], Some(TunnelInputMode::Voice)),
            Err(NoiseError::ProtocolVersion)
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let psk = [9u8; 32];
        let (client, server) = handshake(&psk, &psk).unwrap();
        let mut ct = client.encrypt(0, b"hello").unwrap();
        ct[0] ^= 0xff;
        assert!(server.decrypt(0, &ct).is_err());
        // Wrong nonce also fails AEAD.
        let ct2 = client.encrypt(5, b"hello").unwrap();
        assert!(server.decrypt(6, &ct2).is_err());
    }
}
