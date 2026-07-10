//! `aura-core` — Aura's product-logic spine.
//!
//! This crate holds the parts of Aura that must be correct regardless
//! of which transport, voice provider, or worker backend is wired up:
//! configuration, conversation history, speech-safety + secret
//! redaction, voice session persistence, the tool-dispatch trust
//! boundary, and the host-embedding contract. It is deliberately free
//! of provider and host specifics so those concerns can change without
//! touching the rules that keep secrets out of voice and approvals
//! unforgeable.
//!
//! Module map:
//! - [`config`] — typed settings, pinned defaults, safe load/save.
//! - [`history`] / [`checkpoints`] — speech-safe append-only records.
//! - [`redaction`] / [`speech`] — the two-stage "safe to log / safe to
//!   say" filters every outbound string passes through.
//! - [`session`] — crash-safe per-session transcript + recap.
//! - [`tools`] — the [`tools::ToolRouter`] dispatch surface and the
//!   voice-approval trust boundary.
//! - [`host`] — the memory-card / tool-manifest contract for embedding
//!   hosts (Hermes, OpenClaw).
//! - `private_fs` (internal) — the shared `0o600`/`0o700`, TOCTOU-safe
//!   filesystem primitives the persisting modules build on.
//! - [`callid`] — the validated `CallId` shared by the engine, the
//!   REMOTE server, and the host adapters.

pub mod brief;
pub mod callid;
pub mod checkpoints;
pub mod config;
pub mod history;
pub mod host;
mod private_fs;
pub mod redaction;
pub mod session;
pub mod speech;
pub mod tools;

pub use brief::{
    Brief, BriefIssue, BriefReport, CallbackTask, Context, CronJob, CronRun, OtherAgent, Profile,
    RecentMessage, RecentTask, Setup, User,
};
pub use callid::{CallId, CallIdError};
pub use checkpoints::{CheckpointEvent, CheckpointKind, CheckpointStore};
pub use config::{
    apply_claude_hot_interval_override, apply_codex_hot_interval_override, default_config_path,
    load_or_default, save_default_config, trusted_operator_executable, AgentHealth, AgentTransport,
    AuraConfig, BridgeConfig, CallbackMode, CheckpointConfig, ClaudeConfig, CodexConfig,
    ConnectionProfile, ConnectionProfilesConfig, DiscordConfig, HistoryConfig, ProviderConfig,
    SafetyConfig, SessionConfig, HOT_INTERVAL_CEIL_MS, HOT_INTERVAL_FLOOR_MS,
};
pub use history::{append_history_event, load_history, HistoryEvent};
pub use host::{
    HostKind, HostMemoryCard, HostMemoryPriority, HostMemorySection, HostMemorySource, HostPrivacy,
    HostSessionIdentity, HostToolDescriptor, ToolManifest,
};
pub use private_fs::{append_private_jsonl_line, write_private_contents};
pub use redaction::{contains_secret, content_fingerprint, log_safe, redact_secrets};
pub use session::{
    detect_active_claude_session, load_session, prune_sessions, save_session_atomic, session_path,
    Session,
};
pub use speech::speech_safe_summary;
pub use tools::{
    local_function_schemas, local_function_schemas_without_read_only_worker_query, AgentContext,
    AgentRuntime, AgentStatus, AttentionAck, AttentionRequest, CancelAck, CancelMode, TaskEnvelope,
    TaskHandoffState, TaskResult, ToolCall, ToolResponse, ToolRouter,
};
