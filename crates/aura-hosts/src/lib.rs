//! `aura-hosts` — the `HostAdapter` trait and one implementation per host.
//!
//! A host is the AI chat the user typed "call me" into. Each host differs
//! in how the call is triggered, how its context is read into a [`Brief`],
//! how in-call tasks are dispatched, and how callbacks are delivered —
//! these are deliberately NOT unified (a universal trigger would break at
//! least two hosts).
//!
//! [`HostAdapter`] is a **supertrait of [`aura_core::tools::AgentRuntime`]**:
//! the dispatch surface (`status` / `start_task` / `pause_or_cancel` /
//! `request_attention` / `checkpoint_stream`) is the
//! `AgentRuntime` surface, and `HostAdapter` adds only host identity, detection,
//! the (non-unified) trigger, reading the host store into a [`Brief`], and
//! callback delivery.
//!
//! Implementations:
//! - `claude` — slash `/aura:aura-live`, transcript digest from
//!   `~/.claude/projects/.../<session>.jsonl`, `.aura` file protocol.
//! - `codex`, `hermes`, `openclaw`.

use async_trait::async_trait;

use aura_core::brief::Brief;
use aura_core::host::HostKind;
use aura_core::tools::{AgentRuntime, TaskEnvelope, TaskHandoffState, TaskResult};
use aura_core::{redact_secrets, CallbackMode};

/// Cap on the post-call transcript posted into the chat (chars). Generous: it
/// carries BOTH sides of the conversation and the host summarizes it; delivered
/// once, not spoken. Truncation keeps the start — long calls should raise this.
pub(crate) const CALL_SUMMARY_MAX_CHARS: usize = 8_000;

pub mod claude;
pub mod codex;
pub mod hermes;
pub mod openclaw;
pub mod registry;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use hermes::HermesAdapter;
pub use openclaw::OpenClawAdapter;
pub use registry::{build_host, resolve_host};

/// How a host triggers a call. Deliberately NOT unified across hosts — each
/// host's mechanism is native to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TriggerSource {
    /// Claude Code: a deterministic slash command, e.g. `/aura:aura-live`.
    SlashCommand { command: String },
    /// Codex: a launcher/env var, e.g. `AURA_AGENT=codex`.
    LauncherEnv { var: String },
    /// Hermes: an LLM skill the model invokes from natural language, e.g.
    /// `codexini-call`.
    LlmSkill { skill: String },
    /// OpenClaw: a gateway tool method at a local endpoint, e.g.
    /// `codexini_start_call` @ `ws://127.0.0.1:18789`.
    GatewayTool { method: String, endpoint: String },
}

/// Acknowledgement that an in-call task result was delivered back into the
/// host chat. The `detail` string is already speech/secret-safe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackAck {
    /// Whether the host accepted the callback for delivery.
    pub delivered: bool,
    /// Speech-safe note about the delivery outcome.
    pub detail: String,
}

/// Errors a host adapter can surface.
///
/// Reading context is fail-open at the brief level: a thin/empty store yields
/// a thin [`Brief`], never an error and never a blocked call. `read_context`
/// errors are therefore reserved for "could not locate/read the host store at
/// all", not "the context turned out to be thin".
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The host is not present on this machine.
    #[error("host not detected: {0}")]
    NotDetected(String),
    /// An I/O error reading the host store.
    #[error("host io error: {0}")]
    Io(String),
    /// Delivering a callback into the host chat failed.
    #[error("callback delivery failed: {0}")]
    Callback(String),
}

/// One AI chat host (Claude / Codex / Hermes / OpenClaw).
///
/// Extends [`AgentRuntime`] (the dispatch surface) with the
/// host-facing concerns the engine and the binaries need: identity,
/// detection, the native trigger, reading context into a [`Brief`], and
/// delivering callbacks.
#[async_trait]
pub trait HostAdapter: AgentRuntime {
    /// Which host this is.
    fn kind(&self) -> HostKind;

    /// Whether this host is present on the machine.
    async fn detect(&self) -> bool;

    /// How this host triggers a call. Not unified across hosts.
    fn trigger_source(&self) -> TriggerSource;

    /// Read the host's store/transcript into a [`Brief`]. Fail-open: thin or
    /// empty context yields a thin `Brief` (which still composes valid
    /// instructions and dials), never a blocked call.
    async fn read_context(&self) -> Result<Brief, HostError>;

    /// Deliver an in-call task result back into the host chat. The result's
    /// speech text is expected to already be redacted/speech-safe.
    async fn deliver_callback(&self, result: &TaskResult) -> Result<CallbackAck, HostError>;

    /// Post a recap of a finished voice call back into the host chat, closing
    /// the context loop call→chat. `transcript` is the call's
    /// inline transcript (the realtime model's own transcript events — NOT a
    /// separate STT). The default delivers the redacted, capped recap through
    /// the host's existing callback channel; a host MAY override to summarize it
    /// with its own model first. Empty transcript → nothing delivered (fail-open).
    async fn deliver_call_summary(&self, transcript: &str) -> Result<CallbackAck, HostError> {
        let recap = redact_secrets(transcript);
        let recap = recap.trim();
        if recap.is_empty() {
            return Ok(CallbackAck {
                delivered: false,
                detail: "empty call; nothing to recap".to_owned(),
            });
        }
        let capped: String = recap.chars().take(CALL_SUMMARY_MAX_CHARS).collect();
        let result = TaskResult {
            task_id: "voice-call-summary".to_owned(),
            handoff_state: TaskHandoffState::Accepted,
            speech_update: format!(
                "Voice call transcript (developer + Aura) — summarize this for the chat:\n{capped}"
            ),
            envelope: TaskEnvelope::new(
                "voice call summary",
                Vec::new(),
                String::new(),
                CallbackMode::default(),
                String::new(),
            ),
        };
        self.deliver_callback(&result).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_source_variants_construct() {
        let t = TriggerSource::SlashCommand {
            command: "/aura:aura-live".into(),
        };
        assert_eq!(
            t,
            TriggerSource::SlashCommand {
                command: "/aura:aura-live".into()
            }
        );
    }

    #[test]
    fn host_adapter_is_object_safe() {
        // Compile-time assertion: `dyn HostAdapter` must be usable as a trait
        // object (the engine holds `Arc<dyn HostAdapter>`).
        fn _accepts(_: &dyn HostAdapter) {}
        let _ = HostKind::Claude;
    }

    #[tokio::test]
    async fn call_summary_default_redacts_and_delivers_via_callback() {
        use aura_core::config::ClaudeConfig;
        let dir = tempfile::tempdir().unwrap();
        let cfg = ClaudeConfig {
            hooks_dir: Some(dir.path().to_path_buf()),
            ..ClaudeConfig::default()
        };
        let host = ClaudeAdapter::with_config(dir.path(), &cfg);

        // Empty transcript → nothing delivered (fail-open).
        let empty = host.deliver_call_summary("   ").await.unwrap();
        assert!(!empty.delivered);

        // A real recap is delivered through the host's callback channel, with
        // secrets redacted before they reach the chat.
        let ack = host
            .deliver_call_summary("we shipped the parser; my key is sk-secret-1234567890")
            .await
            .unwrap();
        assert!(ack.delivered, "recap should be delivered: {}", ack.detail);
        let written =
            std::fs::read_to_string(dir.path().join("aura-last-claude-result.json")).unwrap();
        assert!(written.contains("Voice call transcript"));
        assert!(written.contains("we shipped the parser"));
        assert!(
            !written.contains("sk-secret-1234567890"),
            "secret must be redacted"
        );
    }
}
