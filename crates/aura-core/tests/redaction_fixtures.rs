//! Fixture corpus for the three redaction patterns:
//!
//!   1. Stripe `sk_live_…` / `sk_test_…` secret keys
//!   2. OpenSSH multi-line `-----BEGIN/END OPENSSH PRIVATE KEY-----` blocks
//!   3. Anthropic `sk-ant-…` explicit named pattern
//!
//! Each pattern gets a positive-match fixture (the secret must be redacted)
//! and a negative-match fixture (a too-short / wrong-shape lookalike must
//! NOT trigger redaction). The negatives guard against regex loosening
//! that would accidentally swallow user speech transcripts — voice users
//! say "sk live secret" and "openssh key" all the time.

use aura_core::redaction::{contains_secret, redact_secrets};

// ── Stripe keys ──────────────────────────────────────────────────────────

#[test]
fn redacts_stripe_live_secret_key() {
    // Real Stripe shape: sk_live_<24+ base62>. 24 chars is the documented
    // minimum the prefix carries before the key id.
    // Built piecewise so GitHub's secret-scanner does not flag this
    // intentional fixture. The runtime value is still a real Stripe-shaped
    // key, so the redactor's regex (\bsk_(?:live|test)_[A-Za-z0-9]{24,}\b)
    // matches it normally.
    let stripe_key = concat!("sk", "_live_", "AbCdEfGhIjKlMnOpQrStUvWx");
    let raw = format!("STRIPE_SECRET_KEY={stripe_key} # leaked!");
    let redacted = redact_secrets(&raw);
    assert!(
        !redacted.contains(stripe_key),
        "Stripe live key must be redacted, got {redacted:?}"
    );
    assert!(redacted.contains("[REDACTED_SECRET]"));
    assert!(
        contains_secret(&raw),
        "contains_secret must flag Stripe key"
    );
}

#[test]
fn redacts_stripe_test_secret_key() {
    // See note above on piecewise construction.
    let stripe_key = concat!("sk", "_test_", "1234567890ABCDEFGHIJKLMN");
    let raw = format!("export STRIPE_TEST={stripe_key}");
    let redacted = redact_secrets(&raw);
    assert!(
        !redacted.contains(stripe_key),
        "Stripe test key must be redacted, got {redacted:?}"
    );
    assert!(redacted.contains("[REDACTED_SECRET]"));
}

#[test]
fn does_not_redact_stripe_short_lookalike() {
    // Too short (< 24 chars after the prefix) — must pass through.
    // The user might say "use sk_live_short for testing" and we don't
    // want to swallow the phrase.
    let benign = "the sk_live_short example is just a placeholder";
    let redacted = redact_secrets(benign);
    assert_eq!(
        redacted, benign,
        "short sk_live_ lookalike must not be redacted: {redacted:?}"
    );
    assert!(!contains_secret(benign));
}

#[test]
fn does_not_redact_stripe_wrong_environment_prefix() {
    // `sk_dev_` is not a real Stripe environment — Stripe only ships
    // `sk_live_` and `sk_test_`. Anything else must pass through.
    let benign = "use sk_dev_AbCdEfGhIjKlMnOpQrStUvWx in staging";
    let redacted = redact_secrets(benign);
    assert_eq!(
        redacted, benign,
        "non-live/test sk_ lookalike must not match Stripe regex: {redacted:?}"
    );
}

// ── OpenSSH private key blocks ───────────────────────────────────────────

#[test]
fn redacts_openssh_private_key_block_across_newlines() {
    // Realistic multi-line OpenSSH block. The `[\s\S]*?` body class is
    // what lets the pattern straddle newlines without depending on the
    // `(?s)` flag.
    let body = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
                b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
                QyNTUxOQAAACDxC9PiVqLBjwULFx9V7ZAxA5x/yYPdJrJDz1YgVk0RegAAAJg7l5kqO5eZ\n\
                KgAAAAtzc2gtZWQyNTUxOQAAACDxC9PiVqLBjwULFx9V7ZAxA5x/yYPdJrJDz1YgVk0Reg\n\
                -----END OPENSSH PRIVATE KEY-----";
    let raw = format!("here is my key:\n{body}\nthat was bad of me to commit");
    let redacted = redact_secrets(&raw);

    assert!(
        !redacted.contains("b3BlbnNzaC1rZXktdjEAAAAABG5vbmU"),
        "OpenSSH key body must be redacted, got {redacted:?}"
    );
    assert!(
        !redacted.contains("-----BEGIN OPENSSH PRIVATE KEY-----"),
        "BEGIN marker must be part of the redacted span, got {redacted:?}"
    );
    assert!(
        !redacted.contains("-----END OPENSSH PRIVATE KEY-----"),
        "END marker must be part of the redacted span, got {redacted:?}"
    );
    assert!(redacted.contains("[REDACTED_SECRET]"));
    // Surrounding context must survive.
    assert!(
        redacted.contains("that was bad of me to commit"),
        "trailing context must survive redaction: {redacted:?}"
    );
    assert!(
        redacted.contains("here is my key:"),
        "leading context must survive redaction: {redacted:?}"
    );
}

#[test]
fn does_not_redact_openssh_phrase_without_begin_end_markers() {
    // User speech can mention "openssh private key" without producing
    // a key. The pattern requires both BEGIN and END markers, so the
    // phrase passes through.
    let benign = "I should rotate my openssh private key tomorrow";
    let redacted = redact_secrets(benign);
    assert_eq!(
        redacted, benign,
        "phrase without BEGIN/END markers must pass through: {redacted:?}"
    );
    assert!(!contains_secret(benign));
}

#[test]
fn does_not_redact_openssh_with_missing_end_marker() {
    // BEGIN without END = malformed; pattern shouldn't catch it.
    let benign = "header only: -----BEGIN OPENSSH PRIVATE KEY----- and that's it";
    let redacted = redact_secrets(benign);
    assert_eq!(
        redacted, benign,
        "BEGIN without END must not match OpenSSH block: {redacted:?}"
    );
}

// ── Anthropic sk-ant- keys ───────────────────────────────────────────────

#[test]
fn redacts_anthropic_sk_ant_key() {
    // Real Anthropic keys are 40+ chars after the prefix; 20 is the
    // conservative minimum the explicit named pattern uses.
    // Piecewise construction — see Stripe-key note. The runtime value still
    // matches the Anthropic regex; only the source-code literal is split.
    let anthropic_key = concat!(
        "sk",
        "-ant-api03-",
        "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_AbCdEfGh"
    );
    let raw = format!("ANTHROPIC_API_KEY={anthropic_key}");
    let redacted = redact_secrets(&raw);
    assert!(
        !redacted.contains(anthropic_key),
        "Anthropic sk-ant- key must be redacted, got {redacted:?}"
    );
    assert!(redacted.contains("[REDACTED_SECRET]"));
    assert!(
        contains_secret(&raw),
        "contains_secret must flag Anthropic key"
    );
}

#[test]
fn does_not_redact_anthropic_short_lookalike() {
    // `sk-ant-short` has <20 chars after the prefix. Neither the explicit
    // Anthropic regex nor the generic `sk-` regex (which needs 12+ chars
    // after `sk-`) should match. Negative test guarantees voice
    // transcripts saying "sk-ant- short example" survive.
    let benign = "the sk-ant-short prefix is the Anthropic family marker";
    let redacted = redact_secrets(benign);
    assert_eq!(
        redacted, benign,
        "short sk-ant- lookalike must not be redacted: {redacted:?}"
    );
    assert!(!contains_secret(benign));
}
