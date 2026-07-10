//! `XaiRealtimeProvider` — the xAI [`VoiceProvider`]: DIRECT
//! `wss://api.x.ai/v1/realtime`, BYOK, host-pinned.
//!
//! [`VoiceProvider::connect`] resolves the key, builds the xAI-flavored
//! `session.update` (+ optional cold-start), and hands off to the shared
//! [`crate::realtime_ws::connect_realtime`] plumbing, which host-pins the URL,
//! opens the WS with a `Bearer` header, sends the handshake as ONE batched
//! flush, and returns the **split** sink/stream pair (the mic-pump and
//! event-loop tasks can't share `&mut self` to one WS). The engine never sees
//! provider JSON.

use async_trait::async_trait;
use zeroize::Zeroizing;

use crate::realtime_ws::connect_realtime;
use crate::{byok, wire};
use crate::{AudioCaps, VoiceError, VoiceProvider, VoiceSessionConfig, VoiceSink, VoiceStream};

/// Default voice id used when a session config doesn't override it.
pub const DEFAULT_VOICE: &str = "eve";

/// Barge-in `conversation.item.truncate` toggle for xAI. **ON by default** —
/// live-verified 2026-07-07 that xAI attaches `item_id` to output-audio deltas
/// and documents/confirms `conversation.item.truncate`/`.truncated`, so
/// syncing the heard position stops the model repeating a line after a
/// barge-in (the earlier "off, no item_id" belief was wrong). Set
/// `AURA_XAI_TRUNCATE=0` (or `off`/`false`/`no`) to disable — it is best-effort
/// and a too-long `audio_end_ms` only draws a benign, logged `error` event
/// (never a reconnect), so disabling is for debugging only.
pub const XAI_TRUNCATE_ENV: &str = "AURA_XAI_TRUNCATE";

/// Decide whether xAI barge-in truncate is enabled from the env value. Default
/// (unset) is ON; only an explicit falsey value disables it. Pure so it is
/// unit-testable without touching the process environment.
fn xai_truncate_enabled(env_value: Option<&str>) -> bool {
    match env_value {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        ),
        None => true,
    }
}

/// Resolve the BYOK `XAI_API_KEY` (env → OS keychain, service `"aura"`, entry
/// `"XAI_API_KEY"`), wrapped in [`Zeroizing`]. See [`byok::resolve_key`] for
/// the full resolution/soft-fail contract.
pub fn resolve_xai_key() -> Result<Zeroizing<String>, VoiceError> {
    byok::resolve_key("XAI_API_KEY", "xAI")
}

/// The xAI Grok realtime voice provider.
#[derive(Debug, Clone)]
pub struct XaiRealtimeProvider {
    model: String,
    voice: String,
}

impl XaiRealtimeProvider {
    /// Default provider: current Grok voice model + default voice.
    pub fn new() -> Self {
        Self {
            model: wire::DEFAULT_MODEL.to_owned(),
            voice: DEFAULT_VOICE.to_owned(),
        }
    }

    /// Override model and/or default voice.
    pub fn with_model_and_voice(model: impl Into<String>, voice: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            voice: voice.into(),
        }
    }
}

impl Default for XaiRealtimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VoiceProvider for XaiRealtimeProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn default_voice(&self) -> &str {
        &self.voice
    }

    fn audio_caps(&self) -> AudioCaps {
        AudioCaps {
            server_vad: true,
            input_sample_rate_hz: 24_000,
            output_sample_rate_hz: 24_000,
        }
    }

    async fn connect(
        &self,
        cfg: &VoiceSessionConfig,
    ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError> {
        self.connect_with_manual_turn_detection(cfg, false).await
    }

    fn supports_manual_turn_detection(&self) -> bool {
        true
    }

    async fn connect_with_manual_turn_detection(
        &self,
        cfg: &VoiceSessionConfig,
        manual_turn_detection: bool,
    ) -> Result<(Box<dyn VoiceSink>, Box<dyn VoiceStream>), VoiceError> {
        let key = resolve_xai_key()?;
        let url = wire::xai_realtime_url(&self.model);
        let mut frames = vec![wire::xai_session_update_event_with_mode(
            cfg,
            manual_turn_detection,
        )];
        if cfg.cold_start_kick {
            let (user_msg, response_create) = wire::cold_start_kick_events();
            frames.push(user_msg);
            frames.push(response_create);
        }
        let truncate_enabled =
            xai_truncate_enabled(std::env::var(XAI_TRUNCATE_ENV).ok().as_deref());
        connect_realtime(
            &url,
            wire::DIRECT_XAI_ALLOWED_HOST,
            key,
            frames,
            truncate_enabled,
            manual_turn_detection,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_metadata() {
        let p = XaiRealtimeProvider::new();
        assert_eq!(p.model_id(), wire::DEFAULT_MODEL);
        assert_eq!(p.default_voice(), DEFAULT_VOICE);
        let caps = p.audio_caps();
        assert!(caps.server_vad);
        assert_eq!(caps.input_sample_rate_hz, 24_000);
        assert_eq!(caps.output_sample_rate_hz, 24_000);
        assert!(p.supports_manual_turn_detection());
    }

    #[test]
    fn truncate_defaults_on_and_only_falsey_disables() {
        // Default (unset) → ON.
        assert!(xai_truncate_enabled(None));
        // Any non-falsey value → ON (including the old opt-in "1").
        assert!(xai_truncate_enabled(Some("1")));
        assert!(xai_truncate_enabled(Some("on")));
        assert!(xai_truncate_enabled(Some("yes")));
        assert!(xai_truncate_enabled(Some("garbage")));
        // Only explicit falsey values (any case / surrounding space) → OFF.
        assert!(!xai_truncate_enabled(Some("0")));
        assert!(!xai_truncate_enabled(Some(" off ")));
        assert!(!xai_truncate_enabled(Some("FALSE")));
        assert!(!xai_truncate_enabled(Some("No")));
    }

    /// All `resolve_xai_key` cases in ONE test: `XAI_API_KEY` is process-global,
    /// so the env-mutating assertions must be serialized (the multi-threaded test
    /// runner would otherwise race). The keychain probe must stay soft (no hang,
    /// no panic) even on a headless CI box with no secret service — so the
    /// "absent" path is deterministic without ever populating a real keychain.
    #[test]
    fn resolve_key_order_and_failure() {
        // Snapshot and clear so the assertions are independent of the ambient env.
        let saved = std::env::var("XAI_API_KEY").ok();
        // SAFETY: serialized within this single test; no other thread reads the
        // var concurrently (this is the only test that touches it).
        unsafe { std::env::remove_var("XAI_API_KEY") };

        // env present (with surrounding whitespace) → trimmed env key wins.
        unsafe { std::env::set_var("XAI_API_KEY", "  sk-from-env  ") };
        let got = resolve_xai_key().expect("env-present should resolve");
        assert_eq!(got.as_str(), "sk-from-env");

        // env empty / whitespace-only → NOT used as a key.
        unsafe { std::env::set_var("XAI_API_KEY", "   ") };
        let empty = resolve_xai_key();
        // Either the keychain has a real entry (desktop dev box) or it doesn't
        // (headless CI). Whichever, the whitespace env value must never surface.
        if let Ok(k) = &empty {
            assert_ne!(k.as_str(), "   ");
            assert!(!k.trim().is_empty());
        }

        // env absent + keychain absent/unavailable → typed MissingKey error.
        // On a headless box the keychain probe fails soft to "absent", so the
        // error path is deterministic; it must not hang or panic.
        unsafe { std::env::remove_var("XAI_API_KEY") };
        match resolve_xai_key() {
            Err(VoiceError::MissingKey(msg)) => {
                assert!(msg.contains("XAI_API_KEY"));
                assert!(msg.contains(byok::KEYCHAIN_SERVICE));
            }
            // A populated real keychain on a dev box is the only other valid
            // outcome; never an empty key, never a different error.
            Ok(k) => assert!(!k.trim().is_empty()),
            Err(other) => panic!("expected MissingKey, got {other:?}"),
        }

        // Restore the ambient env for any later code in the process.
        match saved {
            Some(v) => unsafe { std::env::set_var("XAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("XAI_API_KEY") },
        }
    }
}
