//! Defensive opener branch selector.
//!
//! Given a Digest snapshot, picks one of 5 opener archetypes:
//!   A — fresh (<2h) AND confidence >= 0.85 AND unambiguous project
//!   B — fresh AND high-conf AND project-ambiguous (top1/top2 within 20%)
//!   C — stale (>12h) AND high-conf
//!   D — fresh BUT low-conf
//!   E — empty memory OR feeder unreachable
//!
//! This module ships the selector only; wiring into the realtime prompt
//! comes later.
//!
//! The thresholds are self-contained in `OpenerBranchInputs::default()`.

use crate::digest::Digest;
use crate::topic_candidate::TopicCandidate;

/// One of the five opener archetypes. The voice loop will map each
/// variant to the literal opener text the realtime model receives via
/// `instructions`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum OpenerBranch {
    /// Fresh (<=2h) AND confidence >= 0.85 AND unambiguous project.
    /// "Sounds like you've been deep in {topic} — keep going or switch?"
    A,
    /// Fresh AND high-conf AND project-ambiguous (top1/top2 within 20%).
    /// "Last bit I caught was {A} and {B} — which one's on top?"
    B,
    /// Stale (>12h) AND high-conf.
    /// "Last time around you were on {topic} — where'd that land?"
    C,
    /// Fresh BUT low-conf.
    /// "I've been half-listening through the chat — what do you want a
    /// voice on right now?"
    D,
    /// Empty memory OR feeder unreachable.
    /// "Quick way to talk through anything you'd normally type."
    E,
}

/// Tunables for `select_opener_branch`. Default thresholds:
/// - fresh window = 2h
/// - stale window = 12h
/// - high-confidence threshold = 0.85
/// - ambiguity threshold = 0.20 (top1 and top2 confidences within
///   20% of top1 -> "ambiguous")
///
/// The `feeder_unreachable` flag forces branch E even when
/// `topic_candidates` is non-empty — the caller signals this when the
/// digest is stale beyond a sanity bound, the subagent crashed, the
/// snapshot timestamp is suspiciously zero, etc. The Digest itself
/// can't know this (it has no notion of "I'm broken"), so we push the
/// flag in via inputs rather than baking it onto the struct.
#[derive(Debug, Clone, Copy)]
pub struct OpenerBranchInputs {
    /// Wall-clock now (unix milliseconds). Caller supplies it so the
    /// selector stays deterministic and testable.
    pub now_ms: u64,
    /// Anything with `age_ms <= fresh_window_ms` counts as fresh.
    /// Default: 2h.
    pub fresh_window_ms: u64,
    /// Anything with `age_ms > stale_window_ms` counts as stale (note
    /// the strict `>`). Default: 12h.
    pub stale_window_ms: u64,
    /// Lower bound for "high confidence." Default: 0.85.
    pub high_conf_threshold: f32,
    /// Relative ambiguity: if `(top.confidence - second.confidence) <
    /// ambiguity_threshold * top.confidence`, branch B is picked over
    /// branch A. Default: 0.20.
    pub ambiguity_threshold: f32,
    /// Force branch E when the caller knows the feeder is unreachable
    /// (subagent crashed, snapshot too stale to trust, etc.).
    pub feeder_unreachable: bool,
}

impl Default for OpenerBranchInputs {
    fn default() -> Self {
        Self {
            now_ms: 0,
            fresh_window_ms: 2 * 3600 * 1000,
            stale_window_ms: 12 * 3600 * 1000,
            high_conf_threshold: 0.85,
            ambiguity_threshold: 0.20,
            feeder_unreachable: false,
        }
    }
}

/// Walk the decision matrix in order: feeder-unreachable / empty -> E,
/// then freshness x confidence x ambiguity -> A/B/C/D.
///
/// Total: never panics. NaN confidences sort to the back. Multiple
/// candidates with identical confidence are tolerated; the first one
/// in the original order wins as "top" after a stable sort.
pub fn select_opener_branch(digest: &Digest, inputs: OpenerBranchInputs) -> OpenerBranch {
    // 1. Hard-override: feeder explicitly unreachable.
    if inputs.feeder_unreachable {
        return OpenerBranch::E;
    }
    // 2. Empty memory: nothing to ground the opener.
    if digest.topic_candidates.is_empty() {
        return OpenerBranch::E;
    }

    // 3. Sort candidates by confidence descending (stable, NaN sinks).
    let mut sorted: Vec<&TopicCandidate> = digest.topic_candidates.iter().collect();
    sorted.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top = sorted[0];

    // 4. Compute age. saturating_sub guards against clock skew where
    //    last_touched_ts > now_ms (we treat that as "age = 0", i.e.,
    //    very fresh — defensively, future-stamped data should not be
    //    misclassified as stale).
    let age_ms = inputs.now_ms.saturating_sub(top.last_touched_ts);

    let high_conf = top.confidence >= inputs.high_conf_threshold;
    let fresh = age_ms <= inputs.fresh_window_ms;
    let stale = age_ms > inputs.stale_window_ms;

    // 5. Ambiguity: if there is a second candidate AND
    //    (top.confidence - second.confidence) < ambiguity_threshold *
    //    top.confidence, the field is "ambiguous." A single candidate
    //    is never ambiguous.
    let ambiguous = if let Some(second) = sorted.get(1) {
        // Guard against negative top.confidence (clamped at 0 in
        // practice but be defensive). When top.confidence <= 0, fall
        // back to absolute-difference < threshold.
        let margin = top.confidence - second.confidence;
        let bar = if top.confidence > 0.0 {
            inputs.ambiguity_threshold * top.confidence
        } else {
            inputs.ambiguity_threshold
        };
        margin < bar
    } else {
        false
    };

    // 6. Decision matrix.
    if fresh && high_conf && !ambiguous {
        OpenerBranch::A
    } else if fresh && high_conf && ambiguous {
        OpenerBranch::B
    } else if stale && high_conf {
        OpenerBranch::C
    } else if fresh && !high_conf {
        OpenerBranch::D
    } else {
        // Fallback: stale + low-conf, or anything in the gap window
        // (fresh_window < age <= stale_window) -> D. Low-confidence
        // framing is forgiving and never claims memory the model
        // doesn't have.
        OpenerBranch::D
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_digest() -> Digest {
        Digest {
            recent_facts: vec![],
            active_topic: String::new(),
            suggested_directions: vec![],
            generated_ms: 0,
            proactive_facts: vec![],
            anticipated_questions: vec![],
            flagged_issues: vec![],
            alternatives_to_surface: vec![],
            needs_research: None,
            topic_candidates: vec![],
            snapshot_ms: 0,
            source_principal_id: String::new(),
        }
    }

    #[test]
    fn empty_candidates_is_branch_e() {
        let d = empty_digest();
        assert_eq!(
            select_opener_branch(&d, OpenerBranchInputs::default()),
            OpenerBranch::E
        );
    }

    #[test]
    fn feeder_unreachable_overrides_everything() {
        let mut d = empty_digest();
        d.topic_candidates.push(TopicCandidate::new(
            "X",
            0,
            0.99,
            "verbatim quote of at least twenty chars",
            "p",
        ));
        let inputs = OpenerBranchInputs {
            now_ms: 1_000,
            feeder_unreachable: true,
            ..OpenerBranchInputs::default()
        };
        assert_eq!(select_opener_branch(&d, inputs), OpenerBranch::E);
    }
}
