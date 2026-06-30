//! Speech-safety filter — shapes text into something Aura can say out
//! loud.
//!
//! Why this exists
//! ===============
//! The voice model speaks its context verbatim. File paths, line
//! numbers, code fences, raw URLs, and opaque ids are noise (or worse,
//! leak structure) when read aloud, and unredacted secrets must never
//! reach the audio path at all. `speech_safe_summary` runs the secret
//! redaction from [`crate::redaction`] AND strips/neutralises those
//! speech-hostile tokens, so callers can drain its output straight into
//! Grok-facing context without a second sanitising pass.
//!
//! This is the textual half of "Aura cannot speak slashes / paths /
//! line numbers" — the rule the rest of the crate (history,
//! checkpoints, session recaps) leans on by routing producer text
//! through here at construction time.

use crate::redaction::{contains_secret, redact_secrets};
use regex::Regex;
use std::sync::OnceLock;

fn code_block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```.*?```").expect("valid code block regex"))
}

fn path_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b[\w./-]+\.(rs|ts|tsx|js|jsx|py|go|java|kt|swift|toml|json|ya?ml)(:\d+)?\b")
            .expect("valid path regex")
    })
}

fn line_number_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bline\s+\d+\b|:\d+(:\d+)?\b").expect("valid line regex"))
}

fn url_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"https?://\S+").expect("valid URL regex"))
}

fn issue_ref_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|\s)#[a-z0-9_-]+").expect("valid issue ref regex"))
}

fn opaque_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:[a-f0-9]{8,}|[A-Za-z0-9]{4,}(?:[-_][A-Za-z0-9]{3,}){2,})\b")
            .expect("valid opaque id regex")
    })
}

fn spoken_symbol_word_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:hash|hashtag|hyphen|slash|underscore)\s+[#A-Za-z0-9_./:-]{2,}")
            .expect("valid spoken symbol word regex")
    })
}

fn noisy_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed.starts_with("diff --git")
        || trimmed.starts_with("index ")
        || trimmed.starts_with("@@")
        || trimmed.starts_with("+++")
        || trimmed.starts_with("---")
        || trimmed.starts_with('+')
        || trimmed.starts_with('-')
        || trimmed.starts_with("at ")
        || trimmed.contains("stack backtrace")
}

/// Convert raw worker output into a voice-safe spoken summary.
///
/// SPEECH SAFETY (kept):
/// - Redacts secrets (API keys, tokens, etc.).
/// - Strips code fences (replaced with " code omitted ").
/// - Replaces file paths with "a project file" (Aura cannot speak slashes
///   or extensions aloud cleanly).
/// - Replaces line numbers with "a specific line" (Aura cannot speak
///   raw digits cleanly).
/// - Drops noisy lines: stack traces, diff hunks, +/- diff context,
///   "at frame" lines, blank lines.
///
/// SUBSTANCE PRESERVATION:
/// A prior version of this function did pattern-matching on Claude's
/// stdout and substituted three CANNED phrases — "I found a failure
/// that needs attention", "The latest check is passing", "I updated
/// the relevant project files" — that REPLACED the actual content
/// whenever a keyword pattern matched. This caused past-tense
/// fabrication: Aura kept saying
/// "I updated the project files" verbatim even when Claude's stdout
/// had nothing to do with file modifications, because the speech
/// filter was putting those exact words in the system note before
/// Aura ever saw Claude's real output.
///
/// With the canned-phrase substitution gone, the actual cleaned content
/// flows through to Aura's voice context and she can paraphrase real
/// substance instead of reciting hardcoded templates. Length cap is
/// 800 bytes so 4-8 substantive sentences from Claude's
/// structured-briefing output survive (paired with the
/// `--append-system-prompt` change that asks Claude for
/// a richer handoff format).
pub fn speech_safe_summary(raw: &str) -> String {
    const MAX_BYTES: usize = 800;
    const TRUNCATE_TARGET: usize = 797;

    let had_secret = contains_secret(raw);
    let redacted = redact_secrets(raw);
    let without_blocks = code_block_regex().replace_all(&redacted, " code omitted ");

    let mut useful_lines = Vec::new();
    for line in without_blocks.lines() {
        if noisy_line(line) {
            continue;
        }
        let mut cleaned = line.trim().replace(['{', '}', '`'], "");
        cleaned = cleaned.replace("=>", " to ");
        cleaned = url_regex().replace_all(&cleaned, "a link").into_owned();
        cleaned = path_regex()
            .replace_all(&cleaned, "a project file")
            .into_owned();
        cleaned = line_number_regex()
            .replace_all(&cleaned, "a specific line")
            .into_owned();
        cleaned = spoken_symbol_word_regex()
            .replace_all(&cleaned, "an internal reference")
            .into_owned();
        cleaned = issue_ref_regex()
            .replace_all(&cleaned, " an issue")
            .into_owned();
        cleaned = opaque_id_regex()
            .replace_all(&cleaned, "an internal id")
            .into_owned();
        if !cleaned.trim().is_empty() {
            useful_lines.push(cleaned);
        }
    }

    let mut spoken = if useful_lines.is_empty() {
        "I have a concise update ready.".to_owned()
    } else {
        useful_lines.join(". ")
    };

    if had_secret {
        spoken.push_str(" I redacted sensitive values before speaking.");
    }

    if spoken.len() > MAX_BYTES {
        truncate_at_char_boundary(&mut spoken, TRUNCATE_TARGET);
        spoken.push_str("...");
    }
    spoken
}

/// Truncate `text` to at most `max_bytes` bytes without splitting a UTF-8
/// scalar. `String::truncate` panics if the byte index falls inside a
/// multi-byte char; spoken summaries can include em-dashes, smart quotes, or
/// transcribed accented letters, so we walk back to the nearest boundary.
fn truncate_at_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_speak_raw_code_or_line_numbers() {
        let spoken = speech_safe_summary(
            "error at src/auth/login.rs:217\n```rust\nfn main() { println!(\"x\"); }\n```",
        );
        // Speech-safety transformations stay: no raw line numbers, no
        // braces, no code body, no full file path with slashes.
        assert!(!spoken.contains("217"));
        assert!(!spoken.contains('{'));
        assert!(!spoken.contains("fn main"));
        assert!(!spoken.contains("src/auth/login.rs"));
        // Path got replaced with the speech-safe placeholder.
        assert!(spoken.contains("a project file") || spoken.contains("a specific line"));
        // The actual word "error" survives — a prior version substituted it
        // with a canned "failure" phrase, which is exactly the fabrication
        // pattern this guards against.
        assert!(spoken.contains("error"));
    }

    #[test]
    fn redacts_secret_before_speech() {
        let spoken = speech_safe_summary("API_KEY=abc12345678901234567890 failed");
        assert!(!spoken.contains("abc123"));
        assert!(spoken.contains("redacted"));
    }

    #[test]
    fn truncate_does_not_split_multibyte_char() {
        // Em-dashes ("\u{2014}") are 3 bytes in UTF-8. Pad enough to push
        // the 800-byte cap into a multi-byte scalar; the truncate logic
        // walks back to the nearest char boundary so String::truncate
        // doesn't panic.
        let raw = "\u{2014}".repeat(400);
        let spoken = speech_safe_summary(&raw);
        assert!(spoken.is_char_boundary(spoken.len()));
        assert!(spoken.ends_with("..."));
    }

    /// Regression: speech_safe_summary used to substitute
    /// canned phrases for any input matching keyword patterns
    /// (fail/error/panic became a "failure needs attention" sentence,
    /// pass/green became "the latest check is passing", and diff/modified/
    /// created became "I updated the relevant project files"). The canned
    /// phrases REPLACED the actual content — the prior function only
    /// returned real lines when NO pattern matched. That caused
    /// past-tense fabrication: Aura
    /// kept reciting the canned phrases verbatim because the speech filter
    /// was putting those words in her mouth before she ever saw Claude's
    /// real output. This test pins the current behavior: actual content passes
    /// through, canned phrases never appear.
    #[test]
    fn substance_passes_through_without_canned_phrase_substitution() {
        let raw = "I refactored the auth module and tightened token rotation. \
                   Tests pass on the suite. Created a new helper for session resume. \
                   Next step: wire rotation into the resume path.";
        let spoken = speech_safe_summary(raw);
        // Actual substance survives.
        assert!(
            spoken.contains("refactored"),
            "expected 'refactored' to survive; got: {spoken:?}"
        );
        assert!(
            spoken.contains("auth module"),
            "expected 'auth module' to survive; got: {spoken:?}"
        );
        assert!(
            spoken.contains("rotation"),
            "expected 'rotation' to survive; got: {spoken:?}"
        );
        assert!(
            spoken.contains("session resume") || spoken.contains("resume path"),
            "expected 'session resume' detail to survive; got: {spoken:?}"
        );
        // The OLD canned phrases must NOT appear. Each of the three
        // pattern triggers is present in the input ('pass' from 'Tests pass',
        // 'created' literally, plus implicit 'modified' meaning is what
        // 'refactored' would have triggered) — under the old function this
        // input would have produced "The latest check is passing. I updated
        // the relevant project files." with all real content discarded.
        assert!(
            !spoken.contains("I updated the relevant project files"),
            "canned 'I updated the relevant project files' phrase leaked; got: {spoken:?}"
        );
        assert!(
            !spoken.contains("The latest check is passing"),
            "canned 'The latest check is passing' phrase leaked; got: {spoken:?}"
        );
        assert!(
            !spoken.contains("I found a failure that needs attention"),
            "canned 'I found a failure that needs attention' phrase leaked; got: {spoken:?}"
        );
    }

    /// With the canned-phrase substitution removed, Aura's
    /// upstream system prompt asks Claude for 4-8 sentence structured
    /// briefings. The 800-byte cap is chosen so a
    /// reasonable structured briefing survives without being truncated.
    /// This test pins that a representative briefing fits.
    #[test]
    fn structured_briefing_fits_within_cap() {
        let briefing = "I refactored the dispatch handler in the live loop module. \
                        Modified the auth-token rotation path and added a regression test. \
                        Findings: the prior race condition between cancel and dispatch is closed; \
                        the test count is unchanged at one fifty-six. \
                        Blocker: the new test is gated on a feature flag the developer has not enabled. \
                        Next step: confirm whether to flip the flag or land the test as ignored.";
        let spoken = speech_safe_summary(briefing);
        assert!(
            !spoken.ends_with("..."),
            "structured briefing should fit under cap; got truncated: {spoken:?}"
        );
        assert!(
            spoken.len() <= 800,
            "spoken length {} exceeds 800-byte cap",
            spoken.len()
        );
        // Headline + finding + blocker + next-step all survive.
        assert!(spoken.contains("refactored"));
        assert!(spoken.contains("Findings") || spoken.contains("findings"));
        assert!(spoken.contains("Blocker") || spoken.contains("blocker"));
        assert!(spoken.contains("Next step") || spoken.contains("next step"));
    }

    #[test]
    fn strips_spoken_symbol_noise_and_opaque_references() {
        let spoken = speech_safe_summary(
            "Use hash abcdef1234567890 then open https://example.test/a-b#frag. \
             The project id is codexini-openclaw-v01-build and issue #123.",
        );

        assert!(!spoken.to_lowercase().contains("hash abcdef"));
        assert!(!spoken.contains("https://"));
        assert!(!spoken.contains("codexini-openclaw-v01-build"));
        assert!(!spoken.contains("#123"));
        assert!(spoken.contains("an internal reference"));
        assert!(spoken.contains("a link"));
    }
}
