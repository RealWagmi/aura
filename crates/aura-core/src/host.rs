//! Host-embedding contract: the memory card and tool manifest Aura
//! hands to (or receives from) an embedding host such as Hermes or
//! OpenClaw.
//!
//! Why this exists
//! ===============
//! When Aura runs *inside* another agent platform, that host owns the
//! conversation and feeds Aura its working context. These types are the
//! wire shape of that handoff: a [`HostMemoryCard`] carries the host's
//! identity, privacy posture, layered memory sections, and the set of
//! tools the host is willing to expose. Keeping the shape here — in
//! product-logic `aura-core`, with no transport or host-specific deps —
//! lets every embedding (and its tests, see `tests/host_contract.rs`)
//! agree on one schema rather than each side inventing its own.
//!
//! Trust-boundary defaults
//! - [`HostPrivacy::default`] is fail-safe: redaction ON, privacy
//!   filter ON, and the voice model NOT permitted to disable the
//!   filter. A host must opt *out*, never opt in by silence.
//! - [`HostMemorySection::untrusted`] marks a section as both
//!   `untrusted` and `redacted`, because host-supplied memory is
//!   attacker-influenced input until proven otherwise — the convenience
//!   constructor makes the safe choice the easy one.
//!
//! The schema is versioned via [`HostMemoryCard::CURRENT_SCHEMA_VERSION`];
//! optional fields use `skip_serializing_if` so older and newer hosts
//! can exchange cards without tripping over absent keys.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Which host (AI chat) Aura is attached to.
///
/// Covers the four chat hosts a `HostAdapter` implements (Claude/Codex/
/// Hermes/OpenClaw) plus `Other`. `Hermes`/`OpenClaw` also identify the
/// embedding host in a [`HostSessionIdentity`] memory card; `Claude`/`Codex`
/// are added for the `aura-hosts::HostAdapter::kind` contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    Claude,
    Codex,
    Hermes,
    OpenClaw,
    Other,
}

impl HostKind {
    /// The snake_case wire token for this host, matching the serde
    /// representation (`open_claw` for [`HostKind::OpenClaw`]).
    pub fn as_str(self) -> &'static str {
        match self {
            HostKind::Claude => "claude",
            HostKind::Codex => "codex",
            HostKind::Hermes => "hermes",
            HostKind::OpenClaw => "open_claw",
            HostKind::Other => "other",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HostSessionIdentity {
    pub host: HostKind,
    pub principal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_session_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl HostSessionIdentity {
    pub fn openclaw(
        principal_id: impl Into<String>,
        agent_id: impl Into<String>,
        session_key: impl Into<String>,
    ) -> Self {
        Self {
            host: HostKind::OpenClaw,
            principal_id: principal_id.into(),
            agent_id: Some(agent_id.into()),
            session_id: None,
            session_key: Some(session_key.into()),
            requester_session_key: None,
            channel: None,
            reply_to: None,
            account_id: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HostPrivacy {
    pub redact_secrets: bool,
    pub privacy_filter_enabled: bool,
    pub voice_can_disable_privacy_filter: bool,
}

impl Default for HostPrivacy {
    fn default() -> Self {
        Self {
            redact_secrets: true,
            privacy_filter_enabled: true,
            voice_can_disable_privacy_filter: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMemorySource {
    AgentSystemPrompt,
    Preferences,
    Configuration,
    Skill,
    RoutineActivity,
    Scheduler,
    Interest,
    Notes,
    Soul,
    MessageTail,
    LongTermMemory,
    DailyNote,
    SessionTail,
    ActiveMemory,
    ToolRecall,
    TaskState,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMemoryPriority {
    Critical,
    High,
    Medium,
    Low,
    Ambient,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HostMemorySection {
    pub id: String,
    pub label: String,
    pub source: HostMemorySource,
    pub priority: HostMemoryPriority,
    pub text: String,
    pub untrusted: bool,
    pub redacted: bool,
}

impl HostMemorySection {
    pub fn untrusted(
        id: impl Into<String>,
        label: impl Into<String>,
        source: HostMemorySource,
        priority: HostMemoryPriority,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            source,
            priority,
            text: text.into(),
            untrusted: true,
            redacted: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HostToolDescriptor {
    pub name: String,
    pub description: String,
    pub read_only: bool,
    pub destructive: bool,
    pub requires_confirmation: bool,
}

impl HostToolDescriptor {
    pub fn read_only(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            read_only: true,
            destructive: false,
            requires_confirmation: false,
        }
    }

    pub fn confirmed_action(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            read_only: false,
            destructive: true,
            requires_confirmation: true,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolManifest {
    #[serde(default)]
    pub tools: Vec<HostToolDescriptor>,
}

impl ToolManifest {
    pub fn new(tools: Vec<HostToolDescriptor>) -> Self {
        Self { tools }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HostMemoryCard {
    pub schema_version: u16,
    pub identity: HostSessionIdentity,
    pub generated_at_ms: u64,
    pub privacy: HostPrivacy,
    #[serde(default)]
    pub memory: Vec<HostMemorySection>,
    pub tools: ToolManifest,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl HostMemoryCard {
    pub const CURRENT_SCHEMA_VERSION: u16 = 1;

    pub fn new(identity: HostSessionIdentity, generated_at_ms: u64) -> Self {
        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            identity,
            generated_at_ms,
            privacy: HostPrivacy::default(),
            memory: Vec::new(),
            tools: ToolManifest::default(),
            metadata: BTreeMap::new(),
        }
    }
}
