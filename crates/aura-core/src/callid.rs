//! `CallId` — the opaque, validated identifier for a single call.
//!
//! Lives in `aura-core` so the engine, the REMOTE server, and the host
//! adapters all agree on one type. This type only validates and carries the
//! value; **minting** (random generation) lives in `aura-server`, which owns
//! the CSPRNG.
//!
//! A `CallId` appears in the connection string
//! (`aura://<host>:<port>#k=<secret>&c=<call_id>`) and in logs. It is therefore
//! constrained to a small URL-safe alphabet and a bounded length so it can
//! never smuggle path traversal, query injection, or unbounded input into any
//! of those sinks.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum length of a call id. Generous for a base64url/hex token while
/// still bounding anything that reaches a URL, a filename, or a log line.
pub const MAX_CALL_ID_LEN: usize = 64;
/// Minimum length — a real minted id is much longer; this only rejects
/// empty/degenerate input.
pub const MIN_CALL_ID_LEN: usize = 8;

/// A validated call identifier.
///
/// Construct via [`CallId::new`] / [`FromStr`]; both enforce the
/// [`is_valid_char`] alphabet and the length bounds. The inner string is
/// never exposed mutably, so a `CallId` is valid by construction for its
/// whole lifetime.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CallId(String);

impl CallId {
    /// Validate `value` and wrap it. Rejects empty, over-long, and
    /// non-URL-safe input.
    pub fn new(value: impl Into<String>) -> Result<Self, CallIdError> {
        let value = value.into();
        if value.len() < MIN_CALL_ID_LEN {
            return Err(CallIdError::TooShort {
                len: value.len(),
                min: MIN_CALL_ID_LEN,
            });
        }
        if value.len() > MAX_CALL_ID_LEN {
            return Err(CallIdError::TooLong {
                len: value.len(),
                max: MAX_CALL_ID_LEN,
            });
        }
        if let Some(bad) = value.chars().find(|c| !is_valid_char(*c)) {
            return Err(CallIdError::IllegalChar(bad));
        }
        Ok(Self(value))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned string.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// The id alphabet: ASCII alphanumerics plus `-` and `_` (base64url-safe,
/// also safe as a path segment and in a log line).
fn is_valid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CallId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for CallId {
    type Err = CallIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for CallId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CallId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Reasons a string is not a valid [`CallId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallIdError {
    #[error("call id too short ({len} < {min})")]
    TooShort { len: usize, min: usize },
    #[error("call id too long ({len} > {max})")]
    TooLong { len: usize, max: usize },
    #[error("call id contains illegal character {0:?}")]
    IllegalChar(char),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_url_safe_id() {
        let id = CallId::new("aB3-_xY9zZ").expect("valid");
        assert_eq!(id.as_str(), "aB3-_xY9zZ");
        assert_eq!(id.to_string(), "aB3-_xY9zZ");
    }

    #[test]
    fn rejects_empty_and_short() {
        assert!(matches!(CallId::new(""), Err(CallIdError::TooShort { .. })));
        assert!(matches!(
            CallId::new("short"),
            Err(CallIdError::TooShort { .. })
        ));
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(MAX_CALL_ID_LEN + 1);
        assert!(matches!(
            CallId::new(long),
            Err(CallIdError::TooLong { .. })
        ));
    }

    #[test]
    fn rejects_path_traversal_and_query_chars() {
        for bad in [
            "../../etc/passwd",
            "id/with/slash",
            "idquery?token=1",
            "id with spaces!",
        ] {
            assert!(
                matches!(CallId::new(bad), Err(CallIdError::IllegalChar(_))),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn serde_round_trip_and_rejects_bad_json() {
        let id = CallId::new("call-abc12345").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"call-abc12345\"");
        let back: CallId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
        assert!(serde_json::from_str::<CallId>("\"bad/id\"").is_err());
    }

    #[test]
    fn from_str_matches_new() {
        let a: CallId = "call-abc12345".parse().unwrap();
        let b = CallId::new("call-abc12345").unwrap();
        assert_eq!(a, b);
    }
}
