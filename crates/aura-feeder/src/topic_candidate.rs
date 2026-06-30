//! `TopicCandidate` — one element of the `Digest::topic_candidates` list
//! (Digest v2).
//!
//! The fast-tier subagent emits one of these per topic it can plausibly
//! identify in the rolling voice transcript. The opener-branch selector
//! (`crate::opener_branch::select_opener_branch`) then walks the list
//! sorted by confidence and picks one of the five archetypes based on
//! freshness x confidence x ambiguity.
//!
//! Schema is additive (no migration drama): every field has a sensible
//! default so a v1 Digest payload (which omits `topic_candidates`
//! entirely) still deserializes cleanly into a v2 `Digest` with an empty
//! candidate list.
//!
//! Note on `Eq`: this struct holds an `f32 confidence`, which does NOT
//! implement `Eq` (NaN is not equal to itself). We derive `PartialEq`
//! only; downstream `Digest` had to drop its `Eq` derive for the same
//! reason. Existing tests that called `assert_eq!` on a `Digest`
//! continue to work because `assert_eq!` requires only `PartialEq`.

use serde::{Deserialize, Serialize};

/// One candidate topic the fast-tier subagent extracted from the recent
/// voice transcript, with confidence + a verbatim quote anchor so the
/// opener can ground its phrasing.
///
/// Field semantics:
/// - `topic`: short clause naming what's being discussed. Same shape
///   as the legacy `Digest::active_topic` (3-10 words, present tense).
/// - `last_touched_ts`: when this topic was last mentioned, in unix
///   milliseconds (same convention as `Digest::generated_ms`).
/// - `confidence`: 0.0..1.0 score from the fast tier. The freshness
///   rules treat >=0.85 as "high" and <0.85 as "low".
/// - `verbatim_quote`: a >=20-char verbatim slice from the transcript
///   that grounds the topic (or empty when the fast tier can't find
///   one). The opener uses this to phrase branch A and C in the
///   user's own words.
/// - `project_id`: stable identifier for the project this topic is
///   associated with — used to detect "project-ambiguous" branch B.
///   Empty string means "no project assignment."
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TopicCandidate {
    /// Short clause naming what's being discussed (3-10 words).
    pub topic: String,
    /// Unix milliseconds when this topic was last touched.
    pub last_touched_ts: u64,
    /// 0.0..1.0 confidence from the fast-tier extractor.
    pub confidence: f32,
    /// Verbatim transcript quote (>=20 chars) or empty.
    pub verbatim_quote: String,
    /// Stable project identifier; empty if unassigned.
    pub project_id: String,
}

impl Default for TopicCandidate {
    /// Empty candidate. Useful in tests and as the zero value for
    /// `..Default::default()`-style struct construction.
    fn default() -> Self {
        Self {
            topic: String::new(),
            last_touched_ts: 0,
            confidence: 0.0,
            verbatim_quote: String::new(),
            project_id: String::new(),
        }
    }
}

impl TopicCandidate {
    /// Convenience constructor for tests. Production code paths
    /// build `TopicCandidate` via serde or the fast-tier parser.
    pub fn new(
        topic: impl Into<String>,
        last_touched_ts: u64,
        confidence: f32,
        verbatim_quote: impl Into<String>,
        project_id: impl Into<String>,
    ) -> Self {
        Self {
            topic: topic.into(),
            last_touched_ts,
            confidence,
            verbatim_quote: verbatim_quote.into(),
            project_id: project_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_zero_value() {
        let c = TopicCandidate::default();
        assert!(c.topic.is_empty());
        assert_eq!(c.last_touched_ts, 0);
        assert_eq!(c.confidence, 0.0);
        assert!(c.verbatim_quote.is_empty());
        assert!(c.project_id.is_empty());
    }

    #[test]
    fn round_trips_through_serde() {
        let c = TopicCandidate::new(
            "polishing the feeder",
            1_777_000_000_000,
            0.92,
            "let's polish the feeder before the demo",
            "aura-v0.1",
        );
        let json = serde_json::to_string(&c).expect("serializes");
        let parsed: TopicCandidate = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(parsed, c);
    }

    #[test]
    fn parses_when_fields_present() {
        let body = r#"{
            "topic": "X",
            "last_touched_ts": 42,
            "confidence": 0.5,
            "verbatim_quote": "hello",
            "project_id": "p1"
        }"#;
        let parsed: TopicCandidate = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.topic, "X");
        assert_eq!(parsed.last_touched_ts, 42);
        assert!((parsed.confidence - 0.5).abs() < f32::EPSILON);
        assert_eq!(parsed.verbatim_quote, "hello");
        assert_eq!(parsed.project_id, "p1");
    }
}
