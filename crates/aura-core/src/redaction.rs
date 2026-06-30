//! Secret redaction — the last line of defence before text leaves the
//! process.
//!
//! Why this exists
//! ===============
//! Worker output, history, config echoes, and voice summaries all pass
//! through here on their way to logs, providers, or the voice model. A
//! single leaked API key in any of those sinks is a real incident, so
//! redaction is centralised in one named, fixture-tested place rather
//! than re-implemented per call site. `redact_secrets` is idempotent
//! and safe to apply defensively even when an upstream layer already
//! ran it.
//!
//! Each pattern is a NAMED, separately-tested regex (see
//! `tests/redaction_fixtures.rs`) — even where a broad pattern would
//! subsume a narrower one — so a future refactor of the generic case
//! cannot silently drop coverage for a specific vendor's key shape.
//! Patterns favour false-negatives-that-look-like-secrets over
//! over-redaction of ordinary prose; the fixtures pin both directions.

use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

fn secret_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"\bxai-[A-Za-z0-9_-]{12,}\b").expect("valid xAI key regex"),
            Regex::new(r"\bsk-[A-Za-z0-9_-]{12,}\b").expect("valid OpenAI-like key regex"),
            // Anthropic `sk-ant-` keys. The generic `sk-` regex above
            // already covers the byte pattern implicitly, but a NAMED,
            // fixture-tested pattern is kept so future
            // refactors of the generic `sk-` regex cannot silently
            // remove Anthropic coverage. Conservative 20-char minimum;
            // real Anthropic keys are 40+ chars.
            Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{20,}\b").expect("valid Anthropic key regex"),
            // Stripe live/test secret keys — `sk_live_<24+>` or
            // `sk_test_<24+>`. Distinct charset from the OpenAI-style
            // `sk-` family (underscore-separated, base62 only).
            Regex::new(r"\bsk_(?:live|test)_[A-Za-z0-9]{24,}\b").expect("valid Stripe key regex"),
            // OpenSSH multi-line private key block. The body matches
            // greedily-non-greedily across newlines via `[\s\S]*?` (the
            // `regex` crate doesn't enable `s` mode by default; this
            // char class is the portable cross-engine spelling).
            Regex::new(
                r"-----BEGIN OPENSSH PRIVATE KEY-----[\s\S]*?-----END OPENSSH PRIVATE KEY-----",
            )
            .expect("valid OpenSSH private key block regex"),
            // Extended GitHub token family — covers personal access
            // (`ghp_`), OAuth (`gho_`), user (`ghu_`), server (`ghs_`),
            // and refresh (`ghr_`) tokens. The single-letter slot lets
            // one pattern cover all five rather than five near-duplicate
            // regexes. Underscores are valid GitHub token chars; dashes
            // are not.
            Regex::new(r"\bgh[osurp]_[A-Za-z0-9_]{30,}\b").expect("valid GitHub token regex"),
            Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{12,}\b").expect("valid GitHub PAT regex"),
            // AWS access key id — fixed 16-char base32-uppercase suffix
            // after the AKIA prefix. We don't try to also match the
            // 40-char secret access key here — it has no fixed shape and
            // matching free-form base64 would over-redact.
            Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("valid AWS access key regex"),
            // JSON Web Token — three base64url segments separated by
            // dots. The `eyJ` prefix is universal: `{"` base64url-encoded.
            // 10-char minimum per segment is conservative; real JWTs are
            // always longer.
            //
            // Trailing boundary uses a negative-lookahead-style char class
            // (`(?:$|[^A-Za-z0-9_.\-])`) instead of `\b`. `\b` is a word
            // boundary and `_`/`-` are NOT word characters in regex's
            // ASCII alphabet — `\b` would not match between `-` (last JWT
            // char) and end-of-string, leaving JWTs whose signature
            // happens to end in `-` or `_` (legal base64url chars)
            // un-redacted. The explicit char class catches every
            // non-base64url-and-non-dot byte plus the absolute end of
            // string, which is what we actually want.
            Regex::new(
                r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}($|[^A-Za-z0-9_.\-])",
            )
            .expect("valid JWT regex"),
            // Slack tokens — `xoxb-` (bot), `xoxa-` (legacy app), `xoxp-`
            // (user/personal), `xoxr-` (refresh), `xoxs-` (legacy session).
            // Minimum 10 chars after the prefix to avoid matching short
            // sentinel strings that happen to start with `xoxb-`.
            //
            // Same trailing-boundary fix as the JWT pattern above:
            // Slack tokens contain `-` and a token whose body ends with
            // `-` would not be matched by `\b`. The explicit char class
            // covers end-of-string plus every non-token-body byte.
            Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}($|[^A-Za-z0-9\-])")
                .expect("valid Slack token regex"),
            Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\b")
                .expect("valid bearer token regex"),
            Regex::new(r#"(?i)\b([A-Za-z0-9_-]*(?:api[_-]?key|token|secret|password|authorization|bearer)[A-Za-z0-9_-]*)\b\s*[:=]\s*["']?[^"'\s,;]+"#)
                .expect("valid assignment secret regex"),
        ]
    })
}

pub fn contains_secret(input: &str) -> bool {
    secret_patterns()
        .iter()
        .any(|pattern| pattern.is_match(input))
}

pub fn redact_secrets(input: &str) -> String {
    if !contains_secret(input) {
        return input.to_owned();
    }
    let mut output = input.to_owned();
    for pattern in secret_patterns() {
        output = pattern
            .replace_all(&output, |caps: &regex::Captures<'_>| {
                // The named-assignment pattern uses group 1 to carry the
                // key name (e.g. `API_KEY`); reformat as
                // `KEY=[REDACTED_SECRET]` to keep the assignment shape.
                //
                // The JWT and Slack patterns use group 1 to carry the
                // trailing terminating byte (a non-token-body char or
                // empty string at end-of-input). We must preserve that
                // byte in the output — otherwise a trailing space or
                // newline in the original gets consumed, mangling the
                // surrounding text. Distinguish "key name" (alphabetic
                // identifier, length ≥ 2) from "trailing terminator"
                // (single non-word byte or empty) by character.
                if let Some(grp) = caps.get(1) {
                    let s = grp.as_str();
                    let is_assignment_key = s.len() >= 2
                        && s.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                    if is_assignment_key {
                        return format!("{s}=[REDACTED_SECRET]");
                    }
                    // Trailing terminator (e.g. ` `, `\n`, `,`) — keep it.
                    return format!("[REDACTED_SECRET]{s}");
                }
                "[REDACTED_SECRET]".to_owned()
            })
            .into_owned();
    }
    output
}

/// The fixed mask substituted for the session-secret value by [`log_safe`].
const SESSION_SECRET_MASK: &str = "[redacted-session-secret]";

/// The per-call session secret (the Noise PSK) rides in the connection string
/// `aura://<host>:<port>#k=<base64url-32B>&c=<call_id>&t=<transport>` as the
/// `k=` value — a base64url-encoded 32-byte secret (`[A-Za-z0-9_-]`, ~43
/// chars). It is single-use and short-lived and travels only via the
/// `AURA_CONNECT` env var or stdin, never argv or a log; but as defense in
/// depth `log_safe` masks it if a connection string ever reaches a log line.
/// Only the `k=` value is secret — the `c=` (call id) and `t=` (transport) are
/// not, so they are left intact. The pattern fires only where the `k=` key sits
/// in a fragment/query position (`#k=` / `&k=` / `?k=`), so an unrelated
/// `k=...` elsewhere is left alone.
fn connection_secret_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // The base64url session secret: 32 bytes ≈ 43 chars; `{20,}` bounds it
        // well above any short benign `k=` value without over-reaching.
        const SECRET: &str = r"[A-Za-z0-9_-]{20,}";
        vec![
            // `#k=<secret>` (fragment) or `&k=`/`?k=` (query). Group 1 carries
            // the `k=` key + its anchor so it is re-emitted verbatim; only the
            // secret value is masked. The `[#?&]`/`^` anchor ensures `k=` only
            // matches as a standalone fragment/query key, never the tail of a
            // longer identifier.
            Regex::new(&format!(r"((?:[#?&]|^)k=){SECRET}"))
                .expect("valid connection-secret regex"),
        ]
    })
}

/// Defensive `Authorization: Bearer <token>` masker for the single-segment
/// (non-JWT) bearer values `redact_secrets` does NOT already cover.
///
/// `redact_secrets`'s bearer regex only matches *dotted* (JWT-shaped)
/// bearers, and its generic assignment regex matches the `Authorization`
/// keyword + `:` and then *consumes the literal `Bearer` word* as the
/// value (it stops at the first whitespace), leaving the real token
/// exposed AND erasing the `Bearer` marker. We therefore run this guard
/// *before* `redact_secrets`, so the `Bearer ` prefix is still present to
/// anchor on; it masks the whole opaque value regardless of internal
/// shape. Group 1 carries the `Bearer ` prefix so it survives.
fn bearer_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)(\bBearer\s+)[A-Za-z0-9._~+/=-]{12,}").expect("valid opaque-bearer regex")
    })
}

/// THE single redaction funnel for anything entering `tracing`.
///
/// Only through `log_safe` may a field reach the log
/// sink. It composes [`redact_secrets`] (the credential/key deny-list)
/// with connection-string-secret masking and an opaque-`Bearer` guard, so a single
/// call site cannot accidentally leak a secret a future pattern would
/// have caught. It is total: it never panics and, when a token-shaped
/// value sits in a token-marked position, it always masks (never leaks
/// on doubt).
///
/// Deny-list categories that must NEVER hit logs:
/// - API key / `Authorization: Bearer` (xAI / vendor keys),
/// - the per-call session secret / Noise PSK and the connection string,
/// - PCM / Opus / audio frames and realtime text deltas (call content),
/// - `Brief` contents (log a [`content_fingerprint`] instead).
///
/// The pattern-matchable categories (keys, bearers, the session secret) are
/// masked here directly. The bulk-content categories (audio, text
/// deltas, SDP/ICE, Brief) have no safe textual shadow, so callers MUST
/// NOT pass them through `log_safe` at all — they log a
/// [`content_fingerprint`] of those payloads instead. `log_safe` is the
/// last-resort net for the former; [`content_fingerprint`] is the
/// contract for the latter.
pub fn log_safe(input: &str) -> String {
    // Order matters; each step is justified:
    //
    // 1. Opaque `Bearer <token>` masking runs FIRST. `redact_secrets`'s
    //    generic assignment regex matches the `Authorization` keyword +
    //    `:` and then consumes the literal `Bearer` word as its "value"
    //    (it stops at the first space), which both erases the `Bearer`
    //    marker AND leaves the real token exposed. Anchoring on `Bearer `
    //    before that sweep masks the whole opaque value.
    // 2. `redact_secrets` is the credential/key deny-list catch-all. It
    //    legitimately owns the generic `key=`/`secret=`/`token=` assignment
    //    shapes.
    // 3. Connection-string secret masking. The session secret rides as the
    //    `k=` value in `aura://…#k=<secret>&c=…`; that fragment key is not a
    //    secret-assignment keyword, so `redact_secrets` does not catch it.
    //    Running this AFTER `redact_secrets` keeps the dedicated mask from
    //    being re-grabbed as an assignment value; mask the `k=` value and
    //    leave `c=`/`t=` intact.

    // 1. Opaque `Bearer <token>` (single-segment, non-JWT) carriers.
    let bearer_masked = bearer_pattern()
        .replace_all(input, |caps: &regex::Captures<'_>| {
            let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("Bearer ");
            format!("{prefix}[REDACTED_SECRET]")
        })
        .into_owned();

    // 2. The credential/key deny-list catch-all.
    let mut output = redact_secrets(&bearer_masked);

    // 3. The connection-string `k=` secret that `redact_secrets` does not
    //    own. Re-emit the captured `k=` key; replace only the secret value.
    for pattern in connection_secret_patterns() {
        output = pattern
            .replace_all(&output, |caps: &regex::Captures<'_>| {
                let prefix = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                format!("{prefix}{SESSION_SECRET_MASK}{suffix}")
            })
            .into_owned();
    }

    output
}

/// Content fingerprint for bulk payloads that must NEVER be logged raw
/// (deny-list): `Brief` contents, audio (PCM/Opus) frames,
/// realtime text deltas, SDP. Callers log this fingerprint — a
/// `sha256:<first-16-hex>+len=<byte-len>` summary — so they can correlate
/// and size a payload across logs without ever exposing its content.
///
/// Deterministic (same input → same output) and one-way: the 16-hex
/// prefix is 64 bits of a SHA-256 digest, which reveals neither the
/// content nor enough of the digest to invert it; `len` is the UTF-8
/// byte length.
pub fn content_fingerprint(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut hex = String::with_capacity(16);
    // First 8 bytes → 16 hex chars.
    for b in digest.iter().take(8) {
        hex.push(char::from(HEX_LOWER[(b >> 4) as usize]));
        hex.push(char::from(HEX_LOWER[(b & 0x0f) as usize]));
    }
    format!("sha256:{hex}+len={}", input.len())
}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_xai_key() {
        let raw = "use xai-FAKEKEYFORTESTINGONLY1234567890";
        let redacted = redact_secrets(raw);
        assert!(!redacted.contains("FAKEKEY"));
        assert!(redacted.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn redacts_named_assignment() {
        let redacted = redact_secrets("API_KEY=abc12345678901234567890");
        assert!(!redacted.contains("abc123"));
        assert!(redacted.contains("API_KEY=[REDACTED_SECRET]"));
    }

    #[test]
    fn redacts_aura_token_assignment_and_bearer() {
        let redacted = redact_secrets(
            "AURA_TOKEN=abc.defghijklmnopqrstuvwxyz123456 Authorization: Bearer abcdefghijklmnopqrstuvwxyz.12345678901234567890",
        );
        assert!(!redacted.contains("defghijklmnopqrstuvwxyz"));
        assert!(!redacted.contains("12345678901234567890"));
        assert!(redacted.contains("AURA_TOKEN=[REDACTED_SECRET]"));
        assert!(redacted.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn redacts_aws_access_key_id() {
        // AKIA + 16 base32-uppercase chars is the documented shape.
        let raw = "creds: AKIAIOSFODNN7EXAMPLE were leaked";
        let redacted = redact_secrets(raw);
        assert!(
            !redacted.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS access key id must be redacted"
        );
        assert!(redacted.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn redacts_jwt_tokens() {
        // Minimal-shape JWT: header.payload.signature, base64url chars.
        // The pattern requires at least 10 chars AFTER the `eyJ`
        // literal in the header segment, so the header below is 13
        // chars total — same shape a real JWT header carries.
        let jwt = "eyJhbGciOiJIUzI1.eyJzdWIiOjEyMw.SflKxwRJSMeKKF";
        let raw = format!("Authorization: Bearer {jwt}");
        let redacted = redact_secrets(&raw);
        assert!(!redacted.contains(jwt), "JWT body must be redacted");
        assert!(redacted.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn redacts_extended_github_token_family() {
        // The `gh[osurp]_` family — covers OAuth, server, user, refresh,
        // PAT. The original `ghp_` test still passes because the new
        // pattern is a strict superset.
        for prefix in ["gho_", "ghs_", "ghu_", "ghr_", "ghp_"] {
            let token = format!("{prefix}AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
            let raw = format!("set GH_TOKEN={token} now");
            let redacted = redact_secrets(&raw);
            assert!(
                !redacted.contains(&token),
                "{prefix} token must be redacted, got {redacted:?}"
            );
            assert!(redacted.contains("[REDACTED_SECRET]"));
        }
    }

    #[test]
    fn redacts_jwt_ending_in_base64url_dash_or_underscore() {
        // `\b` is a word boundary; `-`/`_` are not word characters in
        // the ASCII regex sense — a `\b`-terminated JWT pattern would
        // miss a signature ending in `-`/`_` (both legal base64url).
        // The explicit terminator class `($|[^A-Za-z0-9_.\-])` covers
        // both.
        let jwt_dash =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5-";
        let redacted = redact_secrets(jwt_dash);
        assert!(
            !redacted.contains(jwt_dash),
            "JWT ending in `-` must be redacted, got {redacted:?}"
        );
        assert!(redacted.contains("[REDACTED_SECRET]"));

        let jwt_underscore =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw_";
        let redacted = redact_secrets(jwt_underscore);
        assert!(
            !redacted.contains(jwt_underscore),
            "JWT ending in `_` must be redacted, got {redacted:?}"
        );

        // Round-trip: token followed by a real terminator (space) must
        // preserve the terminator in the output.
        let with_trailer = format!("auth: {jwt_dash} more");
        let redacted = redact_secrets(&with_trailer);
        assert!(!redacted.contains(jwt_dash));
        assert!(
            redacted.contains(" more"),
            "trailing whitespace + word must survive redaction, got {redacted:?}"
        );
    }

    #[test]
    fn redacts_slack_token_ending_in_dash() {
        // Same word-boundary gotcha as JWT — Slack tokens contain `-` and
        // can plausibly end with one. The explicit terminator class on
        // the Slack pattern guards against that.
        let token = "xoxb-1234567890-abcdEFGHij-";
        let raw = format!("token={token}");
        let redacted = redact_secrets(&raw);
        assert!(
            !redacted.contains(token),
            "Slack token ending in `-` must be redacted, got {redacted:?}"
        );
    }

    #[test]
    fn redacts_slack_tokens() {
        let token = "xoxb-1234567890-AAAAAAAAA";
        let raw = format!("slack-token={token}");
        let redacted = redact_secrets(&raw);
        assert!(
            !redacted.contains("xoxb-1234567890-AAAAAAAAA"),
            "Slack bot token must be redacted, got {redacted:?}"
        );
    }

    /// Negative coverage: secret-shaped PREFIXES must NOT trigger
    /// redaction when they're embedded in benign English. A regex
    /// loosening that started swallowing user speech would be a
    /// real production regression — voice transcripts contain words
    /// like "AKIA team" or "xoxb-greeting" all the time.
    #[test]
    fn does_not_redact_benign_prefix_words() {
        // Each input is shaped like a secret prefix but lacks the
        // required suffix length / charset. The redactor must leave
        // them intact so the user's spoken words survive.
        let benign_inputs = [
            // AKIA needs 16 base32-upper chars; "team" is 4 lowercase.
            "the AKIA team is on call",
            // xoxb-* needs 10+ trailing chars; "greeting" alone is fine.
            "xoxb-greeting from the channel",
            // gh*_ needs 20+ chars; "tokens" is too short.
            "github_pat tokens are useful",
            // sk- needs 20+ chars; "show" is 4 chars.
            "sk- show me the menu please",
            // xai- needs 30+ chars; "talk" is 4 chars.
            "xai- talk about the weather",
        ];
        for input in benign_inputs {
            let redacted = redact_secrets(input);
            assert_eq!(
                redacted, input,
                "benign prefix-shaped input must pass through unchanged: {input:?} -> {redacted:?}"
            );
            assert!(
                !redacted.contains("[REDACTED"),
                "no redaction marker should appear for benign input {input:?}"
            );
        }
    }

    /// Negative coverage: contains_secret must be a tight predicate —
    /// a `false` from this gates skipping the more expensive
    /// redact_secrets pass. False positives here cost CPU; false
    /// negatives leak. We want the same tight bound on benign inputs.
    #[test]
    fn contains_secret_says_no_for_benign_prefix_words() {
        for input in [
            "the AKIA team",
            "xoxb-greeting",
            "sk- show me",
            "xai- talk",
            "github_pat is short",
        ] {
            assert!(
                !contains_secret(input),
                "contains_secret false-positive on benign input {input:?}"
            );
        }
    }

    // --- log_safe / content_fingerprint ---------------------------------

    /// A real-shaped session secret: base64url of 32 bytes (43 chars, no
    /// padding, alphabet `[A-Za-z0-9_-]`), matching
    /// `SessionSecret::to_base64url`.
    fn sample_session_secret() -> String {
        format!("{}{}", "aB3-_xY9".repeat(5), "cD2") // 8*5 + 3 = 43 chars
    }

    #[test]
    fn masks_session_secret_in_connection_string() {
        let secret = sample_session_secret();
        let line = format!(
            "AURA_CONNECT='aura://203.0.113.7:47821#k={secret}&c=call-abc12345&t=direct' aura-cli"
        );
        let safe = log_safe(&line);
        assert!(
            !safe.contains(&secret),
            "session secret must be masked in the #k= fragment, got {safe:?}"
        );
        assert!(
            safe.contains("#k=[redacted-session-secret]"),
            "got {safe:?}"
        );
        // The call id, transport, host, and the command are NOT secrets — they
        // must survive so the line stays useful in a log.
        assert!(safe.contains("c=call-abc12345"));
        assert!(safe.contains("t=direct"));
        assert!(safe.contains("203.0.113.7:47821"));
        assert!(safe.contains("aura-cli"));
    }

    #[test]
    fn masks_session_secret_in_query_position() {
        // `?k=` / `&k=` carriers are masked too (defense in depth); the call
        // id alongside is left intact.
        let secret = sample_session_secret();
        let url = format!("aura://h:1?k={secret}&c=call-x");
        let safe = log_safe(&url);
        assert!(!safe.contains(&secret), "got {safe:?}");
        assert!(safe.contains("k=[redacted-session-secret]"), "got {safe:?}");
        assert!(safe.contains("c=call-x"));
    }

    #[test]
    fn masks_opaque_bearer_token() {
        // A single-segment (non-JWT) opaque bearer. redact_secrets's
        // dotted-bearer regex does NOT catch this, so log_safe's bearer
        // guard must.
        let raw = "Authorization: Bearer 0a1b2c3d4e5f60718293a4b5c6d7e8f9aabbccdd";
        let safe = log_safe(raw);
        assert!(
            !safe.contains("0a1b2c3d4e5f60718293a4b5c6d7e8f9aabbccdd"),
            "opaque Bearer value must be masked, got {safe:?}"
        );
        assert!(safe.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn log_safe_composes_redact_secrets_for_api_keys() {
        // log_safe must still catch everything redact_secrets does.
        let raw = "key is xai-FAKEKEYFORTESTINGONLY1234567890 ok";
        let safe = log_safe(raw);
        assert!(!safe.contains("FAKEKEY"));
        assert!(safe.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn log_safe_leaves_ordinary_text_untouched() {
        // No secrets, no token-shaped values: must pass through verbatim.
        let inputs = [
            "the user asked to call back at 3pm tomorrow",
            "version 1.92.0 of the toolchain is pinned",
            "see https://aura.example/docs for details",
            // Ordinary dotted text (not a secret-marked value) stays intact.
            "commit abc123.def456 is on the feature branch",
        ];
        for input in inputs {
            let safe = log_safe(input);
            assert_eq!(
                safe, input,
                "ordinary text must pass through unchanged: {input:?} -> {safe:?}"
            );
        }
    }

    #[test]
    fn log_safe_does_not_mask_unmarked_secret_shaped_blob() {
        // A secret-SHAPED base64url blob that is NOT in a `k=` fragment/query
        // position must NOT be masked — we only fire where the `k=` marker
        // proves it is the connection-string secret.
        let secret = sample_session_secret();
        let bare = format!("the digest {secret} appeared in the manifest");
        let safe = log_safe(&bare);
        assert_eq!(
            safe, bare,
            "unmarked secret-shaped blob must not be masked, got {safe:?}"
        );
    }

    #[test]
    fn content_fingerprint_is_deterministic() {
        let a = content_fingerprint("the brief contents go here");
        let b = content_fingerprint("the brief contents go here");
        assert_eq!(a, b, "fingerprint must be deterministic");
    }

    #[test]
    fn content_fingerprint_reveals_neither_content_nor_full_digest() {
        let secret = "the user's private brief: meet at the old mill at dawn";
        let fp = content_fingerprint(secret);
        // No substring of the content leaks.
        assert!(!fp.contains("mill"));
        assert!(!fp.contains("brief"));
        assert!(!fp.contains("dawn"));
        // Shape: `sha256:<16 hex>+len=<n>`.
        assert!(fp.starts_with("sha256:"), "got {fp:?}");
        assert!(fp.contains("+len="), "got {fp:?}");
        let hexpart = fp
            .strip_prefix("sha256:")
            .and_then(|s| s.split("+len=").next())
            .expect("fingerprint has the documented shape");
        assert_eq!(hexpart.len(), 16, "exactly 16 hex chars, got {hexpart:?}");
        assert!(
            hexpart.chars().all(|c| c.is_ascii_hexdigit()),
            "hex prefix only, got {hexpart:?}"
        );
        // Length component is the UTF-8 byte length.
        assert!(fp.ends_with(&format!("+len={}", secret.len())));
    }

    #[test]
    fn content_fingerprint_differs_for_different_content() {
        assert_ne!(
            content_fingerprint("brief A"),
            content_fingerprint("brief B"),
        );
    }

    #[test]
    fn content_fingerprint_known_vector() {
        // Pin a stable vector so a future refactor can't silently change
        // the digest derivation. SHA-256("") first 8 bytes are
        // e3b0c44298fc1c14.
        assert_eq!(content_fingerprint(""), "sha256:e3b0c44298fc1c14+len=0");
    }

    #[test]
    fn log_safe_masks_secret_in_realistic_log_line() {
        // A realistic record: the connection line the server prints for the
        // caller. The `k=` secret must be masked; the call id must survive.
        let secret = sample_session_secret();
        let line = format!(
            "server: give the caller: AURA_CONNECT='aura://h:47821#k={secret}&c=call-x&t=direct' aura-cli"
        );
        let safe = log_safe(&line);
        assert!(
            !safe.contains(&secret),
            "the connection-string secret must be masked, got {safe:?}"
        );
        assert_eq!(safe.matches("[redacted-session-secret]").count(), 1);
        assert!(
            safe.contains("c=call-x"),
            "the call id must survive, got {safe:?}"
        );
    }
}
