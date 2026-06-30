//! OpenClaw gateway state parser — pure data/parse layer (no network).
//!
//! Converts `tasks.get` and `sessions.get` JSON responses from the OpenClaw
//! gateway into the agent-agnostic [`HostMemoryCard`] so the runtime connector
//! can fetch those responses over the gateway WS and call
//! [`parse_openclaw_gateway_card`] to populate the card.
//!
//! # Design notes
//!
//! * This module is **network-free**.  All transport is the WS client's job;
//!   this module only holds types and pure parsing functions.
//! * OpenClaw exposes per-task reads (`tasks.get`) but a broad `tasks.list` may
//!   be unavailable.  That reality is modelled explicitly via the
//!   `tasks_list_available` flag on [`OpenClawGatewayState`] — the caller
//!   must never fabricate a full task list.
//! * Canonical slug: `openclaw` (Contract E).  Display label: "OpenClaw".
//! * Public types match the `HostMemoryCard` pattern used by the workspace-file
//!   fetcher: sections are `untrusted + redacted` and added to `card.metadata`
//!   under the key `"openclaw_gateway"`.

use aura_core::{
    HostMemoryCard, HostMemoryPriority, HostMemorySection, HostMemorySource, HostSessionIdentity,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Canonical identity constants (Contract E)
// ---------------------------------------------------------------------------

/// Canonical slug for OpenClaw (Contract E).  All worker-slug comparisons
/// and config keys use this value.
pub const OPENCLAW_SLUG: &str = "openclaw";

/// Display label for OpenClaw (Contract E).
pub const OPENCLAW_DISPLAY_LABEL: &str = "OpenClaw";

// ---------------------------------------------------------------------------
// Raw gateway response shapes (what the HTTP connector receives)
// ---------------------------------------------------------------------------

/// Raw response from a `tasks.get` call on the OpenClaw gateway.
///
/// Only the fields that are needed for panel display and card population are
/// modelled here.  Unknown keys are silently ignored via `#[serde(default)]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenClawTaskGetResponse {
    /// Unique task identifier assigned by the gateway.
    #[serde(default)]
    pub task_id: String,

    /// Human-readable title or intent of the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Current execution status (e.g. `"pending"`, `"running"`, `"done"`,
    /// `"failed"`, `"cancelled"`).
    #[serde(default)]
    pub status: String,

    /// ISO-8601 timestamp when the task was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,

    /// ISO-8601 timestamp when the task last changed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,

    /// Agent or sub-agent that owns this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// Free-form result or error message, present when `status` is terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,

    /// Catch-all for any extra fields returned by the gateway — kept so
    /// forward-compatibility is maintained without re-parsing.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

/// Raw response from a `sessions.get` call on the OpenClaw gateway.
///
/// Captures the fields relevant for brief population and panel display.
/// Unknown keys are silently ignored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenClawSessionGetResponse {
    /// Unique session identifier.
    #[serde(default)]
    pub session_id: String,

    /// Current session state (e.g. `"active"`, `"idle"`, `"closed"`).
    #[serde(default)]
    pub state: String,

    /// Agent identifier bound to this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// ISO-8601 creation timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,

    /// ISO-8601 last-activity timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<String>,

    /// Human-readable session summary or context digest (may be absent for
    /// fresh sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<String>,

    /// Catch-all for extra fields.
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
}

// ---------------------------------------------------------------------------
// Parsed gateway state (input to the card builder)
// ---------------------------------------------------------------------------

/// The connector worker's view of the OpenClaw gateway at a point in time.
///
/// Construct this with the data you fetched, then call
/// [`parse_openclaw_gateway_card`] to get a populated [`HostMemoryCard`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpenClawGatewayState {
    /// Whether the gateway endpoint was reachable when this state was
    /// captured.  `false` means the connector got a transport-level error
    /// (timeout, connection refused, etc.) and all other fields may be stale
    /// or empty.
    pub gateway_reachable: bool,

    /// The single task fetched via `tasks.get`, if one was requested.
    ///
    /// `None` means no task was requested (not that no tasks exist).
    pub task: Option<OpenClawTaskGetResponse>,

    /// The session fetched via `sessions.get`, if one was requested.
    ///
    /// `None` means no session was requested (not that no sessions exist).
    pub session: Option<OpenClawSessionGetResponse>,

    /// **Always `false` until the gateway exposes a stable `tasks.list`
    /// endpoint.**  When `false` the host card and any UI derived from it
    /// MUST NOT imply that the task list is complete.  Only per-task state
    /// (from an explicit `tasks.get` call) is guaranteed accurate.
    pub tasks_list_available: bool,

    /// Optional quota information reported by the gateway (e.g. daily-minute
    /// usage for the OpenClaw beta plan).
    pub quota: Option<OpenClawQuota>,

    /// ISO-8601 timestamp when this state snapshot was captured.
    pub captured_at: Option<String>,
}

/// Quota information from the OpenClaw gateway (beta plan: 1 h/day).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenClawQuota {
    /// Minutes used in the current billing / reset period.
    #[serde(default)]
    pub used_minutes: u32,

    /// Total minutes available in the current period.
    #[serde(default)]
    pub total_minutes: u32,

    /// ISO-8601 timestamp when the quota resets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,

    /// Whether the plan is in beta (Contract E: "BETA · 1h/day" chip).
    #[serde(default)]
    pub beta: bool,
}

// ---------------------------------------------------------------------------
// App-panel status (ready-to-display summary for the UI)
// ---------------------------------------------------------------------------

/// Ready-to-display status snapshot for the OpenClaw host panel.
///
/// Populated by [`OpenClawGatewayState::panel_status`].  The connector worker
/// can serialise this to `.aura/openclaw-status.json` for the Swift UI to
/// read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenClawPanelStatus {
    /// Canonical slug — always `"openclaw"` (Contract E).
    pub slug: String,

    /// Display label — always `"OpenClaw"` (Contract E).
    pub display_label: String,

    /// Whether the gateway was reachable when this snapshot was captured.
    pub gateway_reachable: bool,

    /// Per-task status for the single task in scope, or `None` if no task
    /// was fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_status: Option<OpenClawTaskStatus>,

    /// **Explicit partial-list flag.**  `false` = only per-task data is
    /// available; the task list MUST NOT be shown as complete.  `true` =
    /// a full task list was fetched (reserved for when the gateway exposes
    /// `tasks.list`).
    pub tasks_list_available: bool,

    /// Quota snapshot, or `None` if quota data was not included in the
    /// gateway response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<OpenClawQuota>,

    /// ISO-8601 timestamp of the captured gateway state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
}

/// Per-task status for the OpenClaw panel dispatch tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenClawTaskStatus {
    /// Gateway task identifier.
    pub task_id: String,

    /// Human-readable task title (falls back to `task_id` if absent).
    pub title: String,

    /// Normalised status string (`"pending"`, `"running"`, `"done"`,
    /// `"failed"`, `"cancelled"`, or `"unknown"`).
    pub status: String,

    /// Agent / sub-agent that owns this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// Result or error excerpt (terminal states only, ≤ 200 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_excerpt: Option<String>,

    /// ISO-8601 last-updated timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Parse result
// ---------------------------------------------------------------------------

/// The result of [`parse_openclaw_gateway_card`].
#[derive(Clone, Debug)]
pub struct OpenClawGatewayParseResult {
    /// Populated [`HostMemoryCard`] ready to be handed to the voice model or
    /// written to the `.aura` state directory.
    pub card: HostMemoryCard,

    /// Ready-to-display panel status (can be written to
    /// `.aura/openclaw-status.json`).
    pub panel_status: OpenClawPanelStatus,

    /// Diagnostic messages from the parse pass (non-fatal; for logs only).
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Core parse function
// ---------------------------------------------------------------------------

/// Parse OpenClaw gateway state into a populated [`HostMemoryCard`].
///
/// # Arguments
///
/// * `identity` — the [`HostSessionIdentity`] for the session (use
///   [`HostSessionIdentity::openclaw`] for new sessions).
/// * `state` — the gateway state snapshot captured by the connector.
/// * `generated_at_ms` — wall-clock milliseconds since the Unix epoch when
///   the snapshot was taken (for [`HostMemoryCard::generated_at_ms`]).
///
/// # Returns
///
/// An [`OpenClawGatewayParseResult`] containing the card, panel status, and
/// any non-fatal warnings.  The function never panics.
///
/// # Connector contract
///
/// The connector worker must:
/// 1. POST the bearer token to `tasks.get` and `sessions.get` (separately).
/// 2. Populate [`OpenClawGatewayState`] with the parsed responses.
/// 3. Call this function with the state.
/// 4. Write `result.panel_status` to `.aura/openclaw-status.json`.
/// 5. Merge `result.card` into the live-state feed (Contract A/B) or use it
///    as the prefill card for the voice model.
pub fn parse_openclaw_gateway_card(
    identity: HostSessionIdentity,
    state: &OpenClawGatewayState,
    generated_at_ms: u64,
) -> OpenClawGatewayParseResult {
    let mut card = HostMemoryCard::new(identity, generated_at_ms);
    let mut warnings: Vec<String> = Vec::new();

    // --- Gateway reachability ---
    if !state.gateway_reachable {
        warnings.push("openclaw.gateway: not reachable — card is empty".to_owned());
        let panel_status = build_panel_status(state, None);
        card.metadata.insert(
            "openclaw_gateway".to_owned(),
            gateway_metadata(state, &warnings),
        );
        return OpenClawGatewayParseResult {
            card,
            panel_status,
            warnings,
        };
    }

    // --- Session section ---
    let task_status = if let Some(session) = &state.session {
        push_session_section(&mut card, session, &mut warnings);
        // Task section (only when gateway reachable)
        state
            .task
            .as_ref()
            .map(|task| push_task_section(&mut card, task, &mut warnings))
    } else {
        // No session, but maybe a standalone task (valid path)
        if let Some(task) = &state.task {
            Some(push_task_section(&mut card, task, &mut warnings))
        } else {
            warnings.push(
                "openclaw.gateway: no session or task data in state — card has no sections"
                    .to_owned(),
            );
            None
        }
    };

    // --- Partial-list note section ---
    if !state.tasks_list_available {
        push_partial_list_note(&mut card);
    }

    // --- Quota section ---
    if let Some(quota) = &state.quota {
        push_quota_section(&mut card, quota);
    }

    let panel_status = build_panel_status(state, task_status);

    card.metadata.insert(
        "openclaw_gateway".to_owned(),
        gateway_metadata(state, &warnings),
    );

    OpenClawGatewayParseResult {
        card,
        panel_status,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Section builders (private helpers)
// ---------------------------------------------------------------------------

fn push_session_section(
    card: &mut HostMemoryCard,
    session: &OpenClawSessionGetResponse,
    warnings: &mut Vec<String>,
) {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("session_id: {}", session.session_id));
    lines.push(format!("state: {}", session.state));
    if let Some(agent) = &session.agent_id {
        lines.push(format!("agent_id: {agent}"));
    }
    if let Some(ts) = &session.created_at {
        lines.push(format!("created_at: {ts}"));
    }
    if let Some(ts) = &session.last_active_at {
        lines.push(format!("last_active_at: {ts}"));
    }
    if let Some(summary) = &session.context_summary {
        let excerpt = truncate_to_boundary(summary.trim(), 400);
        lines.push(format!("context_summary: {excerpt}"));
    }
    if session.session_id.is_empty() {
        warnings.push("openclaw.session: session_id is empty".to_owned());
    }
    let text = lines.join("\n");
    card.memory.push(HostMemorySection::untrusted(
        "openclaw.gateway.session",
        "OpenClaw gateway session state",
        HostMemorySource::TaskState,
        HostMemoryPriority::High,
        text,
    ));
}

/// Push a task section and return the [`OpenClawTaskStatus`] for the panel.
fn push_task_section(
    card: &mut HostMemoryCard,
    task: &OpenClawTaskGetResponse,
    warnings: &mut Vec<String>,
) -> OpenClawTaskStatus {
    if task.task_id.is_empty() {
        warnings.push("openclaw.task: task_id is empty".to_owned());
    }

    let title = task
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&task.task_id)
        .to_owned();

    let status = normalise_task_status(&task.status);

    let result_excerpt = task.result.as_deref().map(|r| {
        let trimmed = r.trim();
        truncate_to_boundary(trimmed, 200)
    });

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("task_id: {}", task.task_id));
    lines.push(format!("title: {title}"));
    lines.push(format!("status: {status}"));
    if let Some(agent) = &task.agent_id {
        lines.push(format!("agent_id: {agent}"));
    }
    if let Some(ts) = &task.created_at {
        lines.push(format!("created_at: {ts}"));
    }
    if let Some(ts) = &task.updated_at {
        lines.push(format!("updated_at: {ts}"));
    }
    if let Some(excerpt) = &result_excerpt {
        lines.push(format!("result_excerpt: {excerpt}"));
    }

    card.memory.push(HostMemorySection::untrusted(
        "openclaw.gateway.task",
        "OpenClaw gateway task status",
        HostMemorySource::TaskState,
        HostMemoryPriority::High,
        lines.join("\n"),
    ));

    OpenClawTaskStatus {
        task_id: task.task_id.clone(),
        title,
        status,
        agent_id: task.agent_id.clone(),
        result_excerpt,
        updated_at: task.updated_at.clone(),
    }
}

/// Push a short note explaining that the task list is partial (not a full
/// list view).  This prevents the voice model and any downstream consumer
/// from fabricating a complete list.
fn push_partial_list_note(card: &mut HostMemoryCard) {
    card.memory.push(HostMemorySection::untrusted(
        "openclaw.gateway.partial_list_note",
        "OpenClaw gateway task list availability",
        HostMemorySource::Other,
        HostMemoryPriority::Low,
        "tasks_list_available: false — only per-task state is available via tasks.get; \
         a full task list has not been fetched and must not be fabricated.",
    ));
}

fn push_quota_section(card: &mut HostMemoryCard, quota: &OpenClawQuota) {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("used_minutes: {}", quota.used_minutes));
    lines.push(format!("total_minutes: {}", quota.total_minutes));
    if let Some(ts) = &quota.resets_at {
        lines.push(format!("resets_at: {ts}"));
    }
    lines.push(format!(
        "beta: {}",
        if quota.beta { "true" } else { "false" }
    ));
    card.memory.push(HostMemorySection::untrusted(
        "openclaw.gateway.quota",
        "OpenClaw gateway quota",
        HostMemorySource::Other,
        HostMemoryPriority::Low,
        lines.join("\n"),
    ));
}

// ---------------------------------------------------------------------------
// Panel status builder
// ---------------------------------------------------------------------------

fn build_panel_status(
    state: &OpenClawGatewayState,
    task_status: Option<OpenClawTaskStatus>,
) -> OpenClawPanelStatus {
    OpenClawPanelStatus {
        slug: OPENCLAW_SLUG.to_owned(),
        display_label: OPENCLAW_DISPLAY_LABEL.to_owned(),
        gateway_reachable: state.gateway_reachable,
        task_status,
        tasks_list_available: state.tasks_list_available,
        quota: state.quota.clone(),
        captured_at: state.captured_at.clone(),
    }
}

// ---------------------------------------------------------------------------
// Metadata block for card.metadata["openclaw_gateway"]
// ---------------------------------------------------------------------------

fn gateway_metadata(state: &OpenClawGatewayState, warnings: &[String]) -> Value {
    json!({
        "host": OPENCLAW_SLUG,
        "display_label": OPENCLAW_DISPLAY_LABEL,
        "gateway_reachable": state.gateway_reachable,
        "tasks_list_available": state.tasks_list_available,
        "has_task": state.task.is_some(),
        "has_session": state.session.is_some(),
        "has_quota": state.quota.is_some(),
        "warnings": warnings,
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Normalise a raw gateway status string to a known set of values.
/// Falls back to `"unknown"` rather than propagating unexpected strings.
fn normalise_task_status(raw: &str) -> String {
    match raw.to_ascii_lowercase().as_str() {
        "pending" | "queued" => "pending".to_owned(),
        "running" | "in_progress" | "active" => "running".to_owned(),
        "done" | "completed" | "succeeded" | "success" => "done".to_owned(),
        "failed" | "error" => "failed".to_owned(),
        "cancelled" | "canceled" => "cancelled".to_owned(),
        "" => "unknown".to_owned(),
        other => other.to_owned(),
    }
}

fn truncate_to_boundary(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let suffix = "…";
    let avail = max_bytes.saturating_sub(suffix.len());
    let mut end = avail;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &input[..end])
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::HostSessionIdentity;

    fn test_identity() -> HostSessionIdentity {
        HostSessionIdentity::openclaw("principal-test", "agent-main", "sess-abc")
    }

    fn minimal_task() -> OpenClawTaskGetResponse {
        OpenClawTaskGetResponse {
            task_id: "task-001".to_owned(),
            title: Some("Refactor auth module".to_owned()),
            status: "running".to_owned(),
            created_at: Some("2026-05-25T10:00:00Z".to_owned()),
            updated_at: Some("2026-05-25T10:05:00Z".to_owned()),
            agent_id: Some("agent-main".to_owned()),
            result: None,
            extra: Default::default(),
        }
    }

    fn minimal_session() -> OpenClawSessionGetResponse {
        OpenClawSessionGetResponse {
            session_id: "sess-abc".to_owned(),
            state: "active".to_owned(),
            agent_id: Some("agent-main".to_owned()),
            created_at: Some("2026-05-25T09:00:00Z".to_owned()),
            last_active_at: Some("2026-05-25T10:05:00Z".to_owned()),
            context_summary: Some("Working on auth refactor".to_owned()),
            extra: Default::default(),
        }
    }

    // ------------------------------------------------------------------
    // tasks.get + sessions.get → card (the primary happy path)
    // ------------------------------------------------------------------

    #[test]
    fn parse_task_and_session_populates_card_sections() {
        let state = OpenClawGatewayState {
            gateway_reachable: true,
            task: Some(minimal_task()),
            session: Some(minimal_session()),
            tasks_list_available: false,
            quota: None,
            captured_at: Some("2026-05-25T10:05:30Z".to_owned()),
        };

        let result = parse_openclaw_gateway_card(test_identity(), &state, 1_700_000_000_000);

        // Card must have at least session, task, and partial-list note sections.
        assert!(
            result
                .card
                .memory
                .iter()
                .any(|s| s.id == "openclaw.gateway.session"),
            "missing session section"
        );
        assert!(
            result
                .card
                .memory
                .iter()
                .any(|s| s.id == "openclaw.gateway.task"),
            "missing task section"
        );
        assert!(
            result
                .card
                .memory
                .iter()
                .any(|s| s.id == "openclaw.gateway.partial_list_note"),
            "missing partial-list note"
        );

        // All sections must be untrusted + redacted.
        assert!(result.card.memory.iter().all(|s| s.untrusted && s.redacted));

        // Panel status basics.
        assert!(result.panel_status.gateway_reachable);
        assert!(!result.panel_status.tasks_list_available);
        assert_eq!(result.panel_status.slug, "openclaw");
        assert_eq!(result.panel_status.display_label, "OpenClaw");

        let ts = result.panel_status.task_status.as_ref().unwrap();
        assert_eq!(ts.task_id, "task-001");
        assert_eq!(ts.title, "Refactor auth module");
        assert_eq!(ts.status, "running");

        // Metadata key present.
        assert!(result.card.metadata.contains_key("openclaw_gateway"));
        assert!(
            result.warnings.is_empty(),
            "unexpected warnings: {:?}",
            result.warnings
        );
    }

    // ------------------------------------------------------------------
    // Partial / no-list path
    // ------------------------------------------------------------------

    #[test]
    fn partial_no_list_flag_is_explicit_in_card_and_panel() {
        let state = OpenClawGatewayState {
            gateway_reachable: true,
            task: Some(minimal_task()),
            session: None,
            tasks_list_available: false, // the key invariant
            quota: None,
            captured_at: None,
        };

        let result = parse_openclaw_gateway_card(test_identity(), &state, 0);

        // The partial-list note section must be present.
        let note = result
            .card
            .memory
            .iter()
            .find(|s| s.id == "openclaw.gateway.partial_list_note")
            .expect("partial_list_note section missing");
        assert!(
            note.text.contains("tasks_list_available: false"),
            "note text should explain partial state"
        );
        assert!(note.text.contains("must not be fabricated"));

        // Panel status also carries the flag.
        assert!(!result.panel_status.tasks_list_available);

        // Metadata also carries the flag.
        let meta = &result.card.metadata["openclaw_gateway"];
        assert_eq!(meta["tasks_list_available"], serde_json::json!(false));
        assert_eq!(meta["has_task"], serde_json::json!(true));
    }

    #[test]
    fn when_tasks_list_available_no_partial_note_is_added() {
        let state = OpenClawGatewayState {
            gateway_reachable: true,
            task: Some(minimal_task()),
            session: Some(minimal_session()),
            tasks_list_available: true, // list is available
            quota: None,
            captured_at: None,
        };

        let result = parse_openclaw_gateway_card(test_identity(), &state, 0);

        assert!(
            !result
                .card
                .memory
                .iter()
                .any(|s| s.id == "openclaw.gateway.partial_list_note"),
            "partial_list_note should NOT be present when list is available"
        );
        assert!(result.panel_status.tasks_list_available);
    }

    // ------------------------------------------------------------------
    // Gateway unreachable
    // ------------------------------------------------------------------

    #[test]
    fn unreachable_gateway_produces_empty_card_with_warning() {
        let state = OpenClawGatewayState {
            gateway_reachable: false,
            task: None,
            session: None,
            tasks_list_available: false,
            quota: None,
            captured_at: None,
        };

        let result = parse_openclaw_gateway_card(test_identity(), &state, 0);

        assert!(
            result.card.memory.is_empty(),
            "card must be empty when gateway is unreachable"
        );
        assert!(!result.panel_status.gateway_reachable);
        assert!(
            result.warnings.iter().any(|w| w.contains("not reachable")),
            "expected 'not reachable' warning"
        );
        let meta = &result.card.metadata["openclaw_gateway"];
        assert_eq!(meta["gateway_reachable"], serde_json::json!(false));
    }

    // ------------------------------------------------------------------
    // Quota section
    // ------------------------------------------------------------------

    #[test]
    fn quota_section_is_added_when_present() {
        let state = OpenClawGatewayState {
            gateway_reachable: true,
            task: Some(minimal_task()),
            session: Some(minimal_session()),
            tasks_list_available: false,
            quota: Some(OpenClawQuota {
                used_minutes: 20,
                total_minutes: 60,
                resets_at: Some("2026-05-26T00:00:00Z".to_owned()),
                beta: true,
            }),
            captured_at: None,
        };

        let result = parse_openclaw_gateway_card(test_identity(), &state, 0);

        let quota_section = result
            .card
            .memory
            .iter()
            .find(|s| s.id == "openclaw.gateway.quota")
            .expect("quota section missing");
        assert!(quota_section.text.contains("used_minutes: 20"));
        assert!(quota_section.text.contains("total_minutes: 60"));
        assert!(quota_section.text.contains("beta: true"));

        assert!(result.panel_status.quota.is_some());
        let q = result.panel_status.quota.as_ref().unwrap();
        assert_eq!(q.used_minutes, 20);
        assert!(q.beta);
    }

    // ------------------------------------------------------------------
    // Task status normalisation
    // ------------------------------------------------------------------

    #[test]
    fn task_status_normalised_to_canonical_values() {
        let cases = [
            ("running", "running"),
            ("in_progress", "running"),
            ("active", "running"),
            ("done", "done"),
            ("completed", "done"),
            ("succeeded", "done"),
            ("failed", "failed"),
            ("error", "failed"),
            ("cancelled", "cancelled"),
            ("canceled", "cancelled"),
            ("pending", "pending"),
            ("queued", "pending"),
            ("", "unknown"),
            ("custom_state", "custom_state"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalise_task_status(input),
                expected,
                "normalise_task_status({input:?}) should be {expected:?}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Task-only state (no session)
    // ------------------------------------------------------------------

    #[test]
    fn task_only_state_produces_card_without_session_section() {
        let state = OpenClawGatewayState {
            gateway_reachable: true,
            task: Some(OpenClawTaskGetResponse {
                task_id: "task-999".to_owned(),
                title: None,
                status: "done".to_owned(),
                result: Some("All tests pass.".to_owned()),
                agent_id: None,
                created_at: None,
                updated_at: None,
                extra: Default::default(),
            }),
            session: None,
            tasks_list_available: false,
            quota: None,
            captured_at: None,
        };

        let result = parse_openclaw_gateway_card(test_identity(), &state, 0);

        assert!(
            !result
                .card
                .memory
                .iter()
                .any(|s| s.id == "openclaw.gateway.session"),
            "session section should not be present"
        );
        assert!(
            result
                .card
                .memory
                .iter()
                .any(|s| s.id == "openclaw.gateway.task"),
            "task section should be present"
        );

        let ts = result.panel_status.task_status.as_ref().unwrap();
        // Title falls back to task_id when absent.
        assert_eq!(ts.title, "task-999");
        assert_eq!(ts.status, "done");
        assert_eq!(ts.result_excerpt.as_deref(), Some("All tests pass."));
    }

    // ------------------------------------------------------------------
    // Slug + display label are always canonical
    // ------------------------------------------------------------------

    #[test]
    fn panel_status_always_has_canonical_slug_and_label() {
        let state = OpenClawGatewayState {
            gateway_reachable: false,
            ..Default::default()
        };
        let result = parse_openclaw_gateway_card(test_identity(), &state, 0);
        assert_eq!(result.panel_status.slug, OPENCLAW_SLUG);
        assert_eq!(result.panel_status.display_label, OPENCLAW_DISPLAY_LABEL);
        assert_eq!(OPENCLAW_SLUG, "openclaw");
        assert_eq!(OPENCLAW_DISPLAY_LABEL, "OpenClaw");
    }

    // ------------------------------------------------------------------
    // Serialisation round-trips
    // ------------------------------------------------------------------

    #[test]
    fn panel_status_serialises_and_deserialises_cleanly() {
        let state = OpenClawGatewayState {
            gateway_reachable: true,
            task: Some(minimal_task()),
            session: Some(minimal_session()),
            tasks_list_available: false,
            quota: Some(OpenClawQuota {
                used_minutes: 5,
                total_minutes: 60,
                resets_at: None,
                beta: true,
            }),
            captured_at: Some("2026-05-25T10:00:00Z".to_owned()),
        };
        let result = parse_openclaw_gateway_card(test_identity(), &state, 1_000);

        let json = serde_json::to_string(&result.panel_status).expect("serialise panel_status");
        let back: OpenClawPanelStatus =
            serde_json::from_str(&json).expect("deserialise panel_status");

        assert_eq!(back.slug, result.panel_status.slug);
        assert_eq!(
            back.tasks_list_available,
            result.panel_status.tasks_list_available
        );
        assert!(back.quota.as_ref().map(|q| q.beta).unwrap_or(false));
    }
}
