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
/// carries BOTH sides of the conversation and the host summarizes it (in
/// chunks when large — the skill's Step 5); delivered once, not spoken.
pub(crate) const CALL_SUMMARY_MAX_CHARS: usize = 24_000;

/// Cap an over-long recap by keeping the START and the END with an elision
/// marker between them — never head-only truncation: on a long call the tail
/// carries the decisions/follow-ups, which are exactly what the summary needs.
/// The head keeps roughly a third of the budget, the tail the rest.
pub(crate) fn cap_recap(recap: &str) -> String {
    let total = recap.chars().count();
    if total <= CALL_SUMMARY_MAX_CHARS {
        return recap.to_owned();
    }
    let head_budget = CALL_SUMMARY_MAX_CHARS / 3;
    let tail_budget = CALL_SUMMARY_MAX_CHARS - head_budget;
    let head: String = recap.chars().take(head_budget).collect();
    let tail: String = recap.chars().skip(total - tail_budget).collect();
    let elided = total - head_budget - tail_budget;
    format!("{head}\n[... {elided} characters of the middle elided ...]\n{tail}")
}

/// The `.aura` state root for host-side artifacts (recap files, hooks):
/// `AURA_STATE_DIR` when set (the server loads `.env` into the process env at
/// startup, so a pinned value is visible here), else the adapter's own `cwd`.
/// Keeps recap locations in lockstep with the engine inbox and
/// `call-status.json`, which resolve the same variable.
pub(crate) fn state_root_or(cwd: &std::path::Path) -> std::path::PathBuf {
    match std::env::var("AURA_STATE_DIR") {
        Ok(v) if !v.trim().is_empty() => std::path::PathBuf::from(v.trim()),
        _ => cwd.to_path_buf(),
    }
}

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
        let capped = cap_recap(recap);
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
    fn cap_recap_keeps_head_and_tail_of_long_calls() {
        // Short recap: untouched.
        assert_eq!(cap_recap("short call"), "short call");
        // Long recap: both the opening AND the ending survive, with an elision
        // marker between — head-only truncation would lose the decisions at
        // the end of a long call.
        let long: String = (0..CALL_SUMMARY_MAX_CHARS + 10_000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let capped = cap_recap(&long);
        assert!(capped.chars().count() < long.chars().count());
        assert!(capped.starts_with(&long[..100]), "head preserved");
        assert!(
            capped.ends_with(&long[long.len() - 100..]),
            "tail preserved"
        );
        assert!(capped.contains("elided"), "elision marker present");
    }

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
