//! Crypto for the OpenClaw runtime-inbox callback channel.
//!
//! Implements the AES-256-GCM tool-envelope wire format of OpenClaw's
//! `runtime-inbox-client.js` (`encryptAesGcmEnvelope` / `decryptAesGcmEnvelope`):
//! the wire is `base64url(iv(12) || ciphertext || tag(16))` and the key is a
//! 32-byte (AES-256) key delivered as base64url. The per-call key arrives on a
//! `call_started` frame (`key_b64u_ref`) and is wrapped in [`Zeroizing`] so the
//! plaintext key bytes are wiped on drop.
//!
//! This module is transport-free: it only encrypts/decrypts and base64url-codes.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use zeroize::Zeroizing;

/// AES-GCM nonce length (the wire prefix), in bytes.
pub const IV_LEN: usize = 12;
/// AES-GCM authentication-tag length (the wire suffix), in bytes.
pub const TAG_LEN: usize = 16;
/// AES-256 key length, in bytes.
pub const KEY_LEN: usize = 32;

/// Errors from the runtime-inbox envelope crypto. Mirrors the JS error codes.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The key was not exactly [`KEY_LEN`] bytes after base64url decode.
    #[error("tool_envelope_key_invalid:{0}")]
    KeyInvalid(usize),
    /// The wire payload was shorter than `IV_LEN + TAG_LEN`.
    #[error("tool_envelope_too_short")]
    TooShort,
    /// The base64url wire payload could not be decoded.
    #[error("tool_envelope_b64_invalid")]
    Base64Invalid,
    /// AES-256-GCM authentication failed (wrong key or tampered ciphertext).
    #[error("tool_envelope_decrypt_failed")]
    DecryptFailed,
    /// AES-256-GCM encryption failed (should not happen for valid keys).
    #[error("tool_envelope_encrypt_failed")]
    EncryptFailed,
}

/// Encode bytes as base64url without padding (matching Node's `"base64url"`).
pub fn b64u_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a base64url (no-pad) string. Tolerates trailing `=` padding too.
pub fn b64u_decode(text: &str) -> Result<Vec<u8>, CryptoError> {
    // `URL_SAFE_NO_PAD` rejects `=`; strip any padding first so we accept both
    // the padded and unpadded forms a host might emit.
    let trimmed = text.trim_end_matches('=');
    URL_SAFE_NO_PAD
        .decode(trimmed)
        .map_err(|_| CryptoError::Base64Invalid)
}

/// Decode a base64url key and verify it is exactly 32 bytes (AES-256),
/// returning it wrapped in [`Zeroizing`] so it is wiped on drop.
pub fn decode_key(key_b64u: &str) -> Result<Zeroizing<[u8; KEY_LEN]>, CryptoError> {
    let raw = b64u_decode(key_b64u)?;
    if raw.len() != KEY_LEN {
        return Err(CryptoError::KeyInvalid(raw.len()));
    }
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    key.copy_from_slice(&raw);
    Ok(key)
}

/// Encrypt `plaintext` under the 32-byte `key`, returning the base64url
/// `iv(12) || ciphertext || tag(16)` wire string. The 12-byte nonce is drawn
/// from the OS CSPRNG via `getrandom`.
pub fn encrypt_aes_gcm(plaintext: &[u8], key: &[u8; KEY_LEN]) -> Result<String, CryptoError> {
    let cipher = Aes256Gcm::new(key.into());
    let mut iv = [0_u8; IV_LEN];
    getrandom::getrandom(&mut iv).map_err(|_| CryptoError::EncryptFailed)?;
    let nonce = Nonce::from_slice(&iv);
    // `aes_gcm` appends the 16-byte tag to the ciphertext, matching the JS
    // `Buffer.concat([iv, ciphertext, tag])` layout once we prefix `iv`.
    let ct_and_tag = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: &[],
            },
        )
        .map_err(|_| CryptoError::EncryptFailed)?;
    let mut wire = Vec::with_capacity(IV_LEN + ct_and_tag.len());
    wire.extend_from_slice(&iv);
    wire.extend_from_slice(&ct_and_tag);
    Ok(b64u_encode(&wire))
}

/// Decrypt a base64url `iv(12) || ciphertext || tag(16)` wire string under the
/// 32-byte `key`, returning the plaintext bytes.
pub fn decrypt_aes_gcm(wire_b64u: &str, key: &[u8; KEY_LEN]) -> Result<Vec<u8>, CryptoError> {
    let raw = b64u_decode(wire_b64u)?;
    if raw.len() < IV_LEN + TAG_LEN {
        return Err(CryptoError::TooShort);
    }
    let (iv, ct_and_tag) = raw.split_at(IV_LEN);
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Nonce::from_slice(iv);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ct_and_tag,
                aad: &[],
            },
        )
        .map_err(|_| CryptoError::DecryptFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; KEY_LEN] {
        let mut k = [0_u8; KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        k
    }

    #[test]
    fn round_trip_recovers_plaintext() {
        let key = test_key();
        let msg = b"the openclaw consult result, redacted and speech-safe";
        let wire = encrypt_aes_gcm(msg, &key).unwrap();
        let back = decrypt_aes_gcm(&wire, &key).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn round_trip_via_decoded_b64u_key() {
        let key = test_key();
        let key_b64u = b64u_encode(&key);
        let decoded = decode_key(&key_b64u).unwrap();
        let wire = encrypt_aes_gcm(b"hello", &decoded).unwrap();
        let back = decrypt_aes_gcm(&wire, &decoded).unwrap();
        assert_eq!(back, b"hello");
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let key = test_key();
        let mut other = test_key();
        other[0] ^= 0xFF;
        let wire = encrypt_aes_gcm(b"secret", &key).unwrap();
        let err = decrypt_aes_gcm(&wire, &other).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptFailed));
    }

    #[test]
    fn too_short_payload_is_rejected() {
        let key = test_key();
        // 11 bytes < IV_LEN + TAG_LEN.
        let short = b64u_encode(&[0_u8; 11]);
        let err = decrypt_aes_gcm(&short, &key).unwrap_err();
        assert!(matches!(err, CryptoError::TooShort));
    }

    #[test]
    fn key_must_be_32_bytes() {
        let short_key_b64u = b64u_encode(&[0_u8; 16]);
        let err = decode_key(&short_key_b64u).unwrap_err();
        assert!(matches!(err, CryptoError::KeyInvalid(16)));
    }

    #[test]
    fn nonce_is_randomized_per_encrypt() {
        let key = test_key();
        let a = encrypt_aes_gcm(b"same", &key).unwrap();
        let b = encrypt_aes_gcm(b"same", &key).unwrap();
        // With a random 12-byte nonce, two ciphertexts of the same plaintext
        // must differ.
        assert_ne!(a, b);
    }

    #[test]
    fn b64u_decode_tolerates_padding() {
        let bytes = [1_u8, 2, 3, 4, 5];
        let padded = base64::engine::general_purpose::URL_SAFE.encode(bytes);
        assert!(padded.contains('='));
        assert_eq!(b64u_decode(&padded).unwrap(), bytes);
    }
}
