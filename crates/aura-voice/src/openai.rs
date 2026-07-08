//! `OpenAiRealtimeProvider` — the OpenAI [`VoiceProvider`]: DIRECT
//! `wss://api.openai.com/v1/realtime` (GA protocol; the beta protocol was
//! removed upstream), BYOK, host-pinned.
//!
//! Same shape as `xai.rs`: resolve the key, build the GA `session.update`
//! (+ optional cold-start), hand off to the shared
//! [`crate::realtime_ws::connect_realtime`] plumbing. Provider-specific facts
//! worth knowing at this seam:
//!
//! - **Audio**: GA supports exactly PCM16 mono @ 24 kHz — our fixed contract,
//!   no resampling anywhere.
//! - **Input transcription** is an async ASR sidecar (whisper-1) configured in
//!   `session.update`; the AUDIO path stays direct speech-to-speech — the
//!   sidecar only produces the transcript TEXT for recap/digests.
//! - **No WS resumption**: a dropped socket cannot be resumed; the engine's
//!   reconnect-with-digest (fresh session + conversation digest in the
//!   instructions) is exactly the documented recovery pattern.
//! - **Session cap** is 60 minutes (xAI: 30); at the cap the reconnect path
//!   carries the call over into a fresh session.

use async_trait::async_trait;
use zeroize::Zeroizing;

use crate::realtime_ws::connect_realtime;
use crate::{byok, wire};
use crate::{AudioCaps, VoiceError, VoiceProvider, VoiceSessionConfig, VoiceSink, VoiceStream};

/// Default voice id used when a session config doesn't override it. `marin` is
/// one of the two voices OpenAI recommends for `gpt-realtime-2.1`.
pub const DEFAULT_VOICE: &str = "marin";

/// Resolve the BYOK `OPENAI_API_KEY` (env → OS keychain, service `"aura"`,
/// entry `"OPENAI_API_KEY"`), wrapped in [`Zeroizing`]. See
/// [`byok::resolve_key`] for the full resolution/soft-fail contract.
pub fn resolve_openai_key() -> Result<Zeroizing<String>, VoiceError> {
    byok::resolve_key("OPENAI_API_KEY", "OpenAI")
}

/// The OpenAI realtime voice provider (`gpt-realtime-2.1` family).
#[derive(Debug, Clone)]
pub struct OpenAiRealtimeProvider {
    model: String,
    voice: String,
}

impl OpenAiRealtimeProvider {
    /// Default provider: current OpenAI realtime model + default voice.
    pub fn new() -> Self {
        Self {
            model: wire::OPENAI_DEFAULT_MODEL.to_owned(),
            voice: DEFAULT_VOICE.to_owned(),
        }
    }

    /// Override model (e.g. [`wire::OPENAI_MINI_MODEL`]) and/or default voice.
    pub fn with_model_and_voice(model: impl Into<String>, voice: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            voice: voice.into(),
        }
    }
}

impl Default for OpenAiRealtimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VoiceProvider for OpenAiRealtimeProvider {
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
        let key = resolve_openai_key()?;
        let url = wire::openai_realtime_url(&self.model);
        let mut frames = vec![wire::openai_session_update_event(cfg, &self.model)];
        if cfg.cold_start_kick {
            let (user_msg, response_create) = wire::cold_start_kick_events();
            frames.push(user_msg);
            frames.push(response_create);
        }
        // truncate_enabled: GA documents (and on WS effectively requires)
        // `conversation.item.truncate` on barge-in — without it the model's
        // conversation state keeps the unheard tail of a cancelled response.
        connect_realtime(
            &url,
            wire::DIRECT_OPENAI_ALLOWED_HOST,
            key,
            frames,
            true,
            cfg.manual_turn_detection,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_metadata() {
        let p = OpenAiRealtimeProvider::new();
        assert_eq!(p.model_id(), wire::OPENAI_DEFAULT_MODEL);
        assert_eq!(p.default_voice(), DEFAULT_VOICE);
        let caps = p.audio_caps();
        assert!(caps.server_vad);
        assert_eq!(caps.input_sample_rate_hz, 24_000);
        assert_eq!(caps.output_sample_rate_hz, 24_000);
    }

    #[test]
    fn mini_model_override() {
        let p = OpenAiRealtimeProvider::with_model_and_voice(wire::OPENAI_MINI_MODEL, "cedar");
        assert_eq!(p.model_id(), wire::OPENAI_MINI_MODEL);
        assert_eq!(p.default_voice(), "cedar");
    }

    /// Mirrors the xAI key test; touches ONLY `OPENAI_API_KEY` (each provider's
    /// env-mutating test owns exactly one var, so they can't race each other in
    /// the multi-threaded runner).
    #[test]
    fn resolve_key_order_and_failure() {
        let saved = std::env::var("OPENAI_API_KEY").ok();
        // SAFETY: serialized within this single test; no other thread reads the
        // var concurrently (this is the only test that touches it).
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        unsafe { std::env::set_var("OPENAI_API_KEY", "  sk-openai-env  ") };
        let got = resolve_openai_key().expect("env-present should resolve");
        assert_eq!(got.as_str(), "sk-openai-env");

        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        match resolve_openai_key() {
            Err(VoiceError::MissingKey(msg)) => {
                assert!(msg.contains("OPENAI_API_KEY"));
                assert!(msg.contains(byok::KEYCHAIN_SERVICE));
            }
            // A populated real keychain on a dev box is the only other valid
            // outcome; never an empty key, never a different error.
            Ok(k) => assert!(!k.trim().is_empty()),
            Err(other) => panic!("expected MissingKey, got {other:?}"),
        }

        match saved {
            Some(v) => unsafe { std::env::set_var("OPENAI_API_KEY", v) },
            None => unsafe { std::env::remove_var("OPENAI_API_KEY") },
        }
    }
}
