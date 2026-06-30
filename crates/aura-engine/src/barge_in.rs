//! Barge-in decision — the carrying logic for `speech_started_decision`.
//!
//! Pure and unit-tested. The engine calls it when the provider's server-VAD
//! reports the user started speaking, then applies the returned actions as a
//! UNIT: `cancel` + the suppress-after-cancel guard must move together, or
//! late deltas of the cancelled response refill the playout queue and the
//! speech overlaps ("no cancel without guard").
//!
//! The decision table:
//! - open speaker + echo risk, audio queued → SUPPRESS (the speaker can
//!   self-trigger the VAD with the model's own voice; echo risk is NOT set for
//!   headsets, so AirPods stay interruptible).
//! - audio queued → clear playout + cancel + mark speaking.
//! - response active, no queue → cancel + mark speaking.
//! - otherwise → just mark speaking.

/// The actions the engine should take on a `UserSpeechStarted` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechStartedDecision {
    /// Clear the local playout queue (drop audio already buffered to play).
    pub clear_local_playback: bool,
    /// Send `response.cancel` to the provider. Carried together with the
    /// engine's `suppress` flag — never one without the other.
    pub cancel_active_response: bool,
    /// Record that the user is now speaking.
    pub mark_user_speaking: bool,
    /// Structured log tag for this decision (no secrets).
    pub log_type: &'static str,
}

/// Decide what to do when the user starts speaking. See module docs for the
/// table. `playback_ms` is the engine's current playout-queue depth,
/// `assistant_response_active` whether a model response is in flight, and
/// `open_speaker_echo_risk` whether an open speaker could echo the model's
/// voice back into the mic (false for headsets).
pub fn speech_started_decision(
    playback_ms: u64,
    assistant_response_active: bool,
    open_speaker_echo_risk: bool,
) -> SpeechStartedDecision {
    if playback_ms > 0 && open_speaker_echo_risk {
        return SpeechStartedDecision {
            clear_local_playback: false,
            cancel_active_response: false,
            mark_user_speaking: false,
            log_type: "ws.speaker_echo_suppressed",
        };
    }
    if playback_ms > 0 {
        // Queued playback means the model is audibly speaking even if the
        // provider already sent response.done. Cancel as well; it's harmless
        // after done and stops late deltas from refilling the queue.
        return SpeechStartedDecision {
            clear_local_playback: true,
            cancel_active_response: true,
            mark_user_speaking: true,
            log_type: "ws.barge_in_during_playback",
        };
    }
    if assistant_response_active {
        return SpeechStartedDecision {
            clear_local_playback: false,
            cancel_active_response: true,
            mark_user_speaking: true,
            log_type: "ws.barge_in_before_playback",
        };
    }
    SpeechStartedDecision {
        clear_local_playback: false,
        cancel_active_response: false,
        mark_user_speaking: true,
        log_type: "ws.user_speech_started",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_risk_with_playback_suppresses_everything() {
        let d = speech_started_decision(120, true, true);
        assert!(!d.clear_local_playback);
        assert!(!d.cancel_active_response);
        assert!(!d.mark_user_speaking);
        assert_eq!(d.log_type, "ws.speaker_echo_suppressed");
    }

    #[test]
    fn queued_playback_clears_and_cancels() {
        // No echo risk (headset): barge-in is honored even mid-playback.
        let d = speech_started_decision(200, true, false);
        assert!(d.clear_local_playback);
        assert!(d.cancel_active_response);
        assert!(d.mark_user_speaking);
    }

    #[test]
    fn queued_playback_after_response_done_still_cancels() {
        let d = speech_started_decision(80, false, false);
        assert!(d.clear_local_playback);
        assert!(d.cancel_active_response);
    }

    #[test]
    fn active_response_no_queue_cancels_without_clear() {
        let d = speech_started_decision(0, true, false);
        assert!(!d.clear_local_playback);
        assert!(d.cancel_active_response);
        assert!(d.mark_user_speaking);
    }

    #[test]
    fn idle_just_marks_speaking() {
        let d = speech_started_decision(0, false, false);
        assert!(!d.clear_local_playback);
        assert!(!d.cancel_active_response);
        assert!(d.mark_user_speaking);
        assert_eq!(d.log_type, "ws.user_speech_started");
    }
}
