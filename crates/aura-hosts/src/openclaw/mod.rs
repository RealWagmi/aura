//! OpenClaw host adapter.
//!
//! Trigger: the gateway tool `codexini_start_call` at the local gateway
//! endpoint (`ws://127.0.0.1:18789`) — a different method from the OUTBOUND
//! dispatch, which is the single `openclaw_agent_consult` tool on
//! `talk.client.toolCall` (do not conflate).
//!
//! Context: TWO read paths (host-brief preferred). PATH A deserializes a
//! host-composed `codexini-host-brief-v2` (from a `CODEXINI_OPENCLAW_HOST_BRIEF_*`
//! env source) straight into a [`Brief`], validates fail-OPEN, derives
//! `setup.workflows` when absent, and normalizes the callback task / call
//! intent. PATH B (fall-back) scrapes the local workspace via
//! [`OpenClawMemoryFetcher`] and projects the card sections onto a [`Brief`].
//! Both are fail-open: a thin/empty store yields a thin `Brief`, never `Err`.
//!
//! Dispatch: EXACTLY ONE `openclaw_agent_consult` frame, built by
//! [`dispatch::build_openclaw_consult_dispatch`] (the security gate runs first),
//! sent over the gateway WS (`talk.client.toolCall` accepts ONLY this tool
//! name upstream). Callback: an AES-256-GCM `tool_result` frame over the
//! runtime-inbox WS — an AURA-CUSTOM channel (no first-party OpenClaw
//! equivalent; upstream's nearest seam, `talk.session.submitToolResult`, is
//! realtime-relay-only). Both WS legs degrade gracefully when unreachable; the
//! post-call recap additionally always lands in `.aura/aura-last-call-recap.md`.

pub mod card;
pub mod crypto;
pub mod dispatch;
pub mod fetcher;
pub mod gateway;
pub mod gateway_ws;
pub mod inbox_ws;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::net::TcpStream;

use aura_core::brief::{
    Brief, CronJob, CronRun, RecentMessage, RecentTask, Setup, User, MAX_CURRENT_FOCUS,
};
use aura_core::host::{HostKind, HostSessionIdentity};
use aura_core::tools::{
    AgentContext, AgentRuntime, AgentStatus, AttentionAck, AttentionRequest, CancelAck, CancelMode,
    TaskEnvelope, TaskHandoffState, TaskResult,
};
use aura_core::{redact_secrets, speech_safe_summary, CallbackMode};

use crate::{CallbackAck, HostAdapter, HostError, TriggerSource};

pub use dispatch::{
    build_openclaw_consult_dispatch, reject_direct_overrides, ConsultDispatch, ConsultExtras,
    DispatchError, FORBIDDEN_DIRECT_FIELDS,
};
pub use fetcher::{OpenClawMemoryCoverage, OpenClawMemoryFetcher};
pub use gateway::{parse_openclaw_gateway_card, OpenClawGatewayState};

/// Default OpenClaw gateway endpoint (the INBOUND trigger + OUTBOUND dispatch
/// transport).
pub const DEFAULT_GATEWAY_ENDPOINT: &str = "ws://127.0.0.1:18789";
/// The gateway tool method that starts a call FROM an OpenClaw session.
pub const OPENCLAW_TRIGGER_METHOD: &str = "codexini_start_call";
/// Host/gateway TCP host:port probed by [`detect`](OpenClawAdapter::detect).
const GATEWAY_PROBE_ADDR: &str = "127.0.0.1:18789";
/// Newest verbatim messages carried into the brief on PATH B.
const RECENT_MESSAGE_TARGET: usize = 250;
/// Channel label stamped onto messages read from the workspace store.
const OPENCLAW_CHANNEL: &str = "openclaw";
/// Wall-clock ceiling for one gateway/inbox WS exchange.
const WS_TIMEOUT: Duration = Duration::from_secs(20);
/// Short timeout for the TCP detect probe.
const DETECT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

// =============================================================================
// Identity resolution (OpenClaw's real spawn-env vars, source-verified)
// =============================================================================

/// Resolve the OpenClaw session identity from the environment.
///
/// Upstream OpenClaw injects the `OPENCLAW_MCP_*` family into CLI-backend agent
/// runs (`src/agents/cli-runner/prepare.ts`) and only `OPENCLAW_SHELL=exec` +
/// `OPENCLAW_CHANNEL_CONTEXT` (`{sender:{id},chat:{id}}`) into exec-tool
/// spawns (`src/agents/bash-tools.exec.ts`). The bare legacy names
/// (`OPENCLAW_SESSION_KEY` etc.) do not exist upstream but are kept as a
/// fallback for custom launcher shims. Fail-open: missing fields stay `None`.
fn resolve_identity_from_env() -> HostSessionIdentity {
    let get = |k: &str| {
        std::env::var(k)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    // MCP family first (the real upstream names), then the legacy shim names.
    let pick = |mcp: &str, legacy: &str| get(mcp).or_else(|| get(legacy));
    let session_key = pick("OPENCLAW_MCP_SESSION_KEY", "OPENCLAW_SESSION_KEY");
    let agent_id = pick("OPENCLAW_MCP_AGENT_ID", "OPENCLAW_AGENT_ID");
    let account_id = pick("OPENCLAW_MCP_ACCOUNT_ID", "OPENCLAW_ACCOUNT_ID");
    let channel = pick("OPENCLAW_MCP_MESSAGE_CHANNEL", "OPENCLAW_CHANNEL");
    // Reply target: no upstream env carries it directly; exec spawns provide
    // the chat id inside OPENCLAW_CHANNEL_CONTEXT. Legacy shim name last.
    let reply_to = channel_context_chat_id().or_else(|| get("OPENCLAW_REPLY_TARGET"));
    let principal = account_id.clone().unwrap_or_default();

    HostSessionIdentity {
        host: HostKind::OpenClaw,
        principal_id: principal,
        agent_id,
        session_id: None,
        session_key,
        requester_session_key: None,
        channel,
        reply_to,
        account_id,
    }
}

/// The chat id from `OPENCLAW_CHANNEL_CONTEXT` (`{"sender":{"id":..},"chat":{"id":..}}`)
/// — the narrow context OpenClaw's exec tool injects into spawned processes.
fn channel_context_chat_id() -> Option<String> {
    let raw = std::env::var("OPENCLAW_CHANNEL_CONTEXT").ok()?;
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    let chat = value.get("chat")?.get("id")?;
    match chat {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn resolve_call_id_from_env() -> Option<String> {
    std::env::var("CODEXINI_CALL_ID")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

// =============================================================================
// PATH A: host-brief composition
// =============================================================================

/// Read the host-composed `codexini-host-brief-v2` JSON, if any. Source order
/// mirrors `readHostBrief`: env inline JSON, then env FILE (≤ 512 KB).
fn read_host_brief_json() -> Option<Value> {
    if let Ok(inline) = std::env::var("CODEXINI_OPENCLAW_HOST_BRIEF_JSON") {
        if !inline.trim().is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(inline.trim()) {
                if value.is_object() {
                    return Some(value);
                }
            }
        }
    }
    if let Ok(path) = std::env::var("CODEXINI_OPENCLAW_HOST_BRIEF_FILE") {
        if !path.trim().is_empty() {
            if let Ok(meta) = std::fs::metadata(path.trim()) {
                if meta.is_file() && meta.len() <= 512 * 1024 {
                    if let Ok(text) = std::fs::read_to_string(path.trim()) {
                        if let Ok(value) = serde_json::from_str::<Value>(&text) {
                            if value.is_object() {
                                return Some(value);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// PATH A: build a [`Brief`] from a host-composed v2 brief value. Fail-open:
/// validation records `missing[]` but never blocks. Derives workflows when
/// `setup.workflows` is empty; normalizes call_intent + callback_task; trims
/// oldest recent messages to keep the serialized brief under the byte cap.
fn brief_from_host_brief(raw: &Value) -> Brief {
    // Redact + clamp every string, then deserialize tolerantly into the Brief
    // shape (every field defaults — unknown keys are ignored).
    let cleaned = card::clean_json_value(raw, 4000);
    let validation = card::validate_openclaw_pro_host_brief(&cleaned);

    let mut brief: Brief = serde_json::from_value(cleaned.clone()).unwrap_or_default();
    brief.v = 2;
    brief.host_kind = Some(HostKind::OpenClaw.as_str().to_owned());

    // Normalize call_intent: explicit value, else default by callback presence.
    let normalized_callback = cleaned
        .get("callback_task")
        .and_then(card::normalize_callback_task);
    let call_intent = brief
        .call_intent
        .as_deref()
        .map(card::normalize_call_intent)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if normalized_callback.is_some() {
                "outbound_callback".to_owned()
            } else {
                "inbound".to_owned()
            }
        });
    brief.call_intent = Some(call_intent);

    // Derive setup.workflows when absent. OpenClaw has no workflows store, so we
    // honestly synthesize from crons + routines + skills. The Brief schema has
    // no `workflows` field; we fold the synthesized entries into setup.skills so
    // the composer still sees the standing work (deduped against existing).
    let setup = brief.setup.get_or_insert_with(Setup::default);
    if setup.skills.is_empty() || setup_workflows_absent(&cleaned) {
        let crons = cleaned
            .get("context")
            .and_then(|c| c.get("cron_jobs"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let regular = cleaned
            .get("setup")
            .and_then(|s| s.get("regular_activity"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let existing_skills = setup.skills.clone();
        let workflows = card::derive_workflows(&crons, &regular, &existing_skills);
        for wf in workflows {
            let entry = if wf.detail.is_empty() {
                wf.name
            } else {
                format!("{}: {}", wf.name, wf.detail)
            };
            if !setup.skills.iter().any(|s| s == &entry) {
                setup.skills.push(entry);
            }
        }
    }

    // Record the validator's missing[] for observability (never blocks).
    if !validation.ok {
        brief.onboarding_needle = Some(format!(
            "host_brief_missing:{}",
            validation.missing.join(",")
        ));
    }

    // Shrink to the byte cap by trimming the oldest recent messages first.
    shrink_brief(&mut brief, card::MAX_BRIEF_BYTES);
    brief.clamp();
    brief
}

fn setup_workflows_absent(cleaned: &Value) -> bool {
    cleaned
        .get("setup")
        .and_then(|s| s.get("workflows"))
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true)
}

/// Trim the oldest recent messages until the serialized brief fits `max_bytes`.
fn shrink_brief(brief: &mut Brief, max_bytes: usize) {
    while brief.serialized_size() > max_bytes && !brief.context.recent_messages_verbatim.is_empty()
    {
        brief.context.recent_messages_verbatim.remove(0);
    }
}

// =============================================================================
// PATH B: workspace-card projection
// =============================================================================

/// Project a fetched [`OpenClawMemoryFetcher`] card onto a [`Brief`], matching
/// sections by their `openclaw.*` id prefix. Then read the raw message log for
/// the verbatim-message target and derive workflows.
fn brief_from_workspace(cwd: &Path, identity: &HostSessionIdentity) -> Brief {
    let mut brief = Brief {
        host_kind: Some(HostKind::OpenClaw.as_str().to_owned()),
        ..Brief::default()
    };
    let mut setup = Setup::default();
    let mut user = User::default();

    let report = OpenClawMemoryFetcher::new(cwd.to_path_buf(), identity.clone()).fetch();
    let mut cron_text = String::new();
    let mut routine_text = String::new();
    let mut skill_summaries: Vec<String> = Vec::new();
    let mut notes_chunks: Vec<String> = Vec::new();

    for section in &report.card.memory {
        let id = section.id.as_str();
        let text = section.text.trim();
        if text.is_empty() {
            continue;
        }
        if id.starts_with("openclaw.system_prompt") {
            setup.system_prompt_summary = Some(card::clamp_clean(text, 900));
        } else if id.starts_with("openclaw.preferences") || id.starts_with("openclaw.configuration")
        {
            setup.preferences = Some(card::clamp_clean(text, 900));
        } else if id.starts_with("openclaw.skill") {
            skill_summaries.push(card::clamp_clean(text, 200));
        } else if id.starts_with("openclaw.routine") {
            routine_text.push_str(text);
            routine_text.push('\n');
        } else if id.starts_with("openclaw.scheduler") {
            cron_text.push_str(text);
            cron_text.push('\n');
        } else if id.starts_with("openclaw.interests") {
            for item in card::extract_line_items(text, 100) {
                user.interests.push(item);
            }
        } else if id.starts_with("openclaw.notes") {
            notes_chunks.push(card::clamp_clean(text, 320));
        } else if id.starts_with("openclaw.soul") {
            let soul = card::clamp_clean(text, 4000);
            user.soul_summary = soul.clone();
        }
    }

    if !skill_summaries.is_empty() {
        setup.skills = skill_summaries.clone();
    }
    if !routine_text.trim().is_empty() {
        let regular = card::clamp_clean(&routine_text, 700);
        // Stash routines into preferences-adjacent notes summary if no notes.
        if notes_chunks.is_empty() {
            notes_chunks.push(regular);
        }
    }
    if !notes_chunks.is_empty() {
        let joined = notes_chunks
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ");
        // Notes summary has no direct Brief field; fold into preferences when
        // preferences is empty so the composer still sees it.
        if setup.preferences.is_none() {
            setup.preferences = Some(joined);
        }
    }

    // Cron jobs -> context.cron_jobs.
    let crons = parse_cron_jobs(&cron_text);
    let cron_values: Vec<Value> = crons
        .iter()
        .filter_map(|c| serde_json::to_value(c).ok())
        .collect();
    brief.context.cron_jobs = crons;

    // Tasks (TASKS.md is in the card too, but the structured tasks come from
    // the workspace TASKS.md / tasks/ files; project the task-state sections).
    let tasks = parse_tasks_from_card(&report);
    brief.context.recent_tasks = tasks;

    // Raw verbatim messages (parse up to RECENT_MESSAGE_TARGET).
    let messages = read_verbatim_messages(cwd);
    if let Some(last_user) = messages.iter().rev().find(|m| m.role == "user") {
        brief.context.current_focus = truncate_chars(&last_user.text, MAX_CURRENT_FOCUS);
    }
    brief.context.recent_messages_verbatim = messages;

    // Derive workflows from crons + routines + skills; fold into skills.
    let workflows = card::derive_workflows(&cron_values, &routine_text, &skill_summaries);
    for wf in workflows {
        let entry = if wf.detail.is_empty() {
            wf.name
        } else {
            format!("{}: {}", wf.name, wf.detail)
        };
        if !setup.skills.iter().any(|s| s == &entry) {
            setup.skills.push(entry);
        }
    }

    brief.user = user;
    if setup_has_content(&setup) {
        brief.setup = Some(setup);
    }
    brief.clamp();
    brief
}

fn setup_has_content(setup: &Setup) -> bool {
    setup.system_prompt_summary.is_some() || setup.preferences.is_some() || !setup.skills.is_empty()
}

/// Parse the scheduler section text into [`CronJob`]s. Accepts a JSON array /
/// `{crons|jobs}` object, else best-effort line items (mirrors `parseCronJobs`).
fn parse_cron_jobs(text: &str) -> Vec<CronJob> {
    if let Ok(parsed) = serde_json::from_str::<Value>(text.trim()) {
        let raw_items = parsed
            .as_array()
            .cloned()
            .or_else(|| parsed.get("crons").and_then(Value::as_array).cloned())
            .or_else(|| parsed.get("jobs").and_then(Value::as_array).cloned());
        if let Some(items) = raw_items {
            let mut out = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let job = CronJob {
                    id: pick(obj, &["id", "name"], 80)
                        .unwrap_or_else(|| format!("cron-{}", index + 1)),
                    purpose: pick(obj, &["purpose", "intent", "description", "summary"], 220)
                        .unwrap_or_default(),
                    schedule: pick(obj, &["schedule", "cron", "expression"], 80)
                        .unwrap_or_default(),
                    tz: pick(obj, &["tz", "timezone"], 60),
                    next_run_iso: pick(obj, &["next_run_iso", "nextRunAt", "next"], 80),
                    last_status: pick(obj, &["last_status", "status"], 60),
                    last_summary: pick(obj, &["last_summary", "summary"], 220),
                    last_runs: obj
                        .get("last_runs")
                        .and_then(Value::as_array)
                        .map(|runs| {
                            runs.iter()
                                .rev()
                                .take(3)
                                .rev()
                                .map(|run| CronRun {
                                    ts_iso: run
                                        .get("ts_iso")
                                        .or_else(|| run.get("last_run_iso"))
                                        .or_else(|| run.get("time"))
                                        .and_then(Value::as_str)
                                        .map(|s| card::clamp_clean(s, 80)),
                                    status: run
                                        .get("status")
                                        .and_then(Value::as_str)
                                        .map(|s| card::clamp_clean(s, 60)),
                                    summary: run
                                        .get("summary")
                                        .and_then(Value::as_str)
                                        .map(|s| card::clamp_clean(s, 180)),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    intent: None,
                };
                if !job.purpose.is_empty() || !job.schedule.is_empty() {
                    out.push(job);
                }
            }
            return out;
        }
    }
    // Fall back to line items as purpose-only crons.
    card::extract_line_items(text, 200)
        .into_iter()
        .enumerate()
        .map(|(index, line)| CronJob {
            id: format!("cron-{}", index + 1),
            purpose: line,
            ..CronJob::default()
        })
        .collect()
}

fn pick(obj: &Map<String, Value>, keys: &[&str], max: usize) -> Option<String> {
    for key in keys {
        if let Some(s) = obj.get(*key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return Some(card::clamp_clean(s, max));
            }
        }
    }
    None
}

/// Project the card's task-state sections (TASKS.md / tasks/*) into
/// [`RecentTask`]s (best-effort line items; structured task JSON is rare).
fn parse_tasks_from_card(report: &fetcher::OpenClawMemoryFetchReport) -> Vec<RecentTask> {
    let mut out = Vec::new();
    for section in &report.card.memory {
        if !section.id.starts_with("openclaw.task") {
            continue;
        }
        for (index, line) in card::extract_line_items(&section.text, 50)
            .into_iter()
            .enumerate()
        {
            out.push(RecentTask {
                task_id: format!("task-{}", out.len() + index + 1),
                intent: line,
                ..RecentTask::default()
            });
        }
    }
    out
}

/// Read the raw message log for the verbatim-message target (up to 250).
fn read_verbatim_messages(cwd: &Path) -> Vec<RecentMessage> {
    for candidate in fetcher::MESSAGE_CANDIDATES {
        let path = cwd.join(candidate);
        // Bound the read like PATH A (512 KB): skip an oversized log rather than
        // pulling it wholesale into memory (fail-open).
        if std::fs::metadata(&path)
            .map(|m| m.len() > 512 * 1024)
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed = fetcher::parse_jsonl_messages(&text, RECENT_MESSAGE_TARGET);
        if parsed.is_empty() {
            continue;
        }
        return parsed
            .into_iter()
            .map(|m| RecentMessage {
                role: m.role,
                text: redact_secrets(&m.text),
                channel: if m.channel.is_empty() {
                    OPENCLAW_CHANNEL.to_owned()
                } else {
                    m.channel
                },
                ts_iso: m.ts_iso,
            })
            .collect();
    }
    Vec::new()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

// =============================================================================
// OpenClawAdapter
// =============================================================================

/// The OpenClaw host adapter.
pub struct OpenClawAdapter {
    /// The OpenClaw workspace root (where the fetcher reads files).
    cwd: PathBuf,
    /// Resolved session identity (env, then explicit override).
    identity: HostSessionIdentity,
    /// The gateway WS endpoint (dispatch + detect probe).
    gateway_endpoint: String,
    /// The paired-device state directory (Ed25519 connect key).
    state_dir: Option<PathBuf>,
    /// The runtime-inbox WS endpoint for the callback leg.
    inbox_endpoint: Option<String>,
    /// The Codexini call id (binds dispatch + callback to this call).
    call_id: Option<String>,
    /// The most recent envelope (for status/context).
    last_envelope: Mutex<Option<TaskEnvelope>>,
    /// The most recent gateway snapshot (for status()).
    last_gateway: Mutex<Option<OpenClawGatewayState>>,
    task_counter: AtomicU64,
}

impl OpenClawAdapter {
    /// Adapter for a workspace root, resolving identity + call id from the env
    /// and using the default gateway endpoint.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            identity: resolve_identity_from_env(),
            gateway_endpoint: DEFAULT_GATEWAY_ENDPOINT.to_owned(),
            state_dir: std::env::var("OPENCLAW_STATE_DIR").ok().map(PathBuf::from),
            inbox_endpoint: std::env::var("CODEXINI_RUNTIME_INBOX_WS")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            call_id: resolve_call_id_from_env(),
            last_envelope: Mutex::new(None),
            last_gateway: Mutex::new(None),
            task_counter: AtomicU64::new(1),
        }
    }

    /// Fully configure the adapter (mirrors `HermesAdapter::configured`).
    pub fn configured(
        cwd: impl Into<PathBuf>,
        identity: Option<HostSessionIdentity>,
        gateway_endpoint: Option<String>,
        state_dir: Option<PathBuf>,
        inbox_endpoint: Option<String>,
        call_id: Option<String>,
    ) -> Self {
        let mut a = Self::new(cwd);
        if let Some(id) = identity {
            a.identity = id;
        }
        if let Some(ep) = gateway_endpoint {
            a.gateway_endpoint = ep;
        }
        if state_dir.is_some() {
            a.state_dir = state_dir;
        }
        if inbox_endpoint.is_some() {
            a.inbox_endpoint = inbox_endpoint;
        }
        if call_id.is_some() {
            a.call_id = call_id;
        }
        a
    }

    /// The resolved session identity.
    pub fn identity(&self) -> &HostSessionIdentity {
        &self.identity
    }

    fn gateway_config(&self) -> Option<gateway_ws::GatewayWsConfig> {
        let state_dir = self.state_dir.clone()?;
        Some(gateway_ws::GatewayWsConfig {
            endpoint: self.gateway_endpoint.clone(),
            state_dir,
            token: std::env::var("OPENCLAW_GATEWAY_TOKEN").unwrap_or_default(),
            timeout: WS_TIMEOUT,
        })
    }
}

// =============================================================================
// AgentRuntime (the dispatch surface)
// =============================================================================

#[async_trait]
impl AgentRuntime for OpenClawAdapter {
    async fn status(&self) -> AgentStatus {
        let active = self
            .last_envelope
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|e| e.user_intent)
            .filter(|i| !i.is_empty());
        // Fold the last gateway snapshot's task status into the summary.
        let summary = self
            .last_gateway
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|state| state.task)
            .map(|task| {
                speech_safe_summary(&format!(
                    "OpenClaw task {} is {}.",
                    task.task_id, task.status
                ))
            })
            .unwrap_or_else(|| "OpenClaw adapter ready.".to_owned());
        AgentStatus {
            state: if active.is_some() {
                "agent_working"
            } else {
                "idle"
            }
            .to_owned(),
            active_task: active,
            summary,
        }
    }

    async fn context(&self) -> AgentContext {
        AgentContext {
            project: self.cwd.to_string_lossy().into_owned(),
            active_task: self
                .last_envelope
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .map(|e| e.user_intent),
            speech_briefing: "OpenClaw session attached.".to_owned(),
            recent_changes: Vec::new(),
        }
    }

    async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
        if let Ok(mut last) = self.last_envelope.lock() {
            *last = Some(envelope.clone());
        }
        let id = self.task_counter.fetch_add(1, Ordering::SeqCst);
        let task_id = format!("openclaw-{id}");

        // The envelope was already approved through ToolRouter; reuse its
        // redacted user_intent as the consult question. Build the ONE allowed
        // consult frame — the security gate runs first.
        //
        // Trust model (why the gate's `extra_input_keys` is empty here): the
        // voice path supplies ONLY the approved question. The tool name is
        // hardcoded to `openclaw_agent_consult`, and `session_key`/`account_id`/
        // `agent_id`/`channel`/`reply_target` come from the trusted env-resolved
        // `self.identity` — none of it is model/voice-controlled. The
        // `extra_input_keys` map is the untrusted-override channel that
        // `reject_direct_overrides` scans; it is empty because the voice path
        // never lets the model inject raw keys. The gate is therefore
        // defense-in-depth for any FUTURE caller that wires model/voice JSON
        // into `extra_input_keys` — keep that the SOLE untrusted channel
        // (`ConsultExtras` is typed and must stay caller-trusted).
        let call_id = self.call_id.clone().unwrap_or_else(|| task_id.clone());
        let dispatch = match build_openclaw_consult_dispatch(
            &self.identity,
            &call_id,
            &envelope.user_intent,
            &ConsultExtras::default(),
            &Map::new(),
        ) {
            Ok(d) => d,
            Err(e) => {
                return TaskResult {
                    task_id,
                    handoff_state: TaskHandoffState::Rejected,
                    speech_update: speech_safe_summary(&redact_secrets(&format!(
                        "I could not dispatch that to OpenClaw: {e}"
                    ))),
                    envelope,
                };
            }
        };

        // Send the consult over the gateway WS. Unreachable/timeout/error all
        // degrade to a non-accepted handoff with a speech-safe note — never an
        // Err or panic.
        let Some(config) = self.gateway_config() else {
            return TaskResult {
                task_id,
                handoff_state: TaskHandoffState::EnvelopePrepared,
                speech_update: "I prepared the consult, but no OpenClaw gateway device is \
                                configured to send it."
                    .to_owned(),
                envelope,
            };
        };
        match gateway_ws::request_gateway_ws(&config, &dispatch.method, dispatch.params).await {
            Ok(payload) => {
                let reply = payload
                    .get("reply")
                    .or_else(|| payload.get("summary"))
                    .or_else(|| payload.get("result"))
                    .and_then(Value::as_str)
                    .unwrap_or("OpenClaw accepted the consult and will report when done.");
                TaskResult {
                    task_id,
                    handoff_state: TaskHandoffState::Accepted,
                    speech_update: speech_safe_summary(&redact_secrets(reply)),
                    envelope,
                }
            }
            Err(e) => TaskResult {
                task_id,
                handoff_state: TaskHandoffState::Rejected,
                speech_update: speech_safe_summary(&redact_secrets(&format!(
                    "I reached OpenClaw but it did not accept the consult: {e}"
                ))),
                envelope,
            },
        }
    }

    async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
        // OpenClaw permits only the single consult frame; there is no
        // turn-interrupt wire op. This is a speech-only acknowledgement.
        CancelAck {
            task_id: task_id.to_owned(),
            mode,
            speech_update: "I can't interrupt the OpenClaw session mid-run; it will report when \
                            it's done."
                .to_owned(),
        }
    }

    async fn request_attention(&self, request: AttentionRequest) -> AttentionAck {
        match request.callback_mode {
            CallbackMode::PingFirst => AttentionAck {
                speech_update: "Hey, you there?".to_owned(),
                requires_ack: true,
            },
            CallbackMode::SpeakImmediately => AttentionAck {
                speech_update: speech_safe_summary(&request.reason),
                requires_ack: false,
            },
            CallbackMode::SilentNotification => AttentionAck {
                speech_update: "Silent callback recorded locally.".to_owned(),
                requires_ack: false,
            },
        }
    }
}

// =============================================================================
// HostAdapter (identity / detect / trigger / read_context / callback)
// =============================================================================

#[async_trait]
impl HostAdapter for OpenClawAdapter {
    fn kind(&self) -> HostKind {
        HostKind::OpenClaw
    }

    async fn detect(&self) -> bool {
        // Fail-soft: present when env identity is set, OR a paired device-state
        // dir exists AND a TCP connect to the gateway succeeds. Never panic.
        if self.identity.session_key.is_some() && self.identity.account_id.is_some() {
            return true;
        }
        let device_present = self
            .state_dir
            .as_ref()
            .map(|d| d.join("identity").join("device.json").is_file())
            .unwrap_or(false);
        if !device_present {
            return false;
        }
        matches!(
            tokio::time::timeout(DETECT_PROBE_TIMEOUT, TcpStream::connect(GATEWAY_PROBE_ADDR))
                .await,
            Ok(Ok(_))
        )
    }

    fn trigger_source(&self) -> TriggerSource {
        TriggerSource::GatewayTool {
            method: OPENCLAW_TRIGGER_METHOD.to_owned(),
            endpoint: self.gateway_endpoint.clone(),
        }
    }

    async fn read_context(&self) -> Result<Brief, HostError> {
        // PATH A (preferred): a host-composed v2 brief from the environment.
        if let Some(raw) = read_host_brief_json() {
            return Ok(brief_from_host_brief(&raw));
        }
        // PATH B (fall-back): scrape the local workspace.
        Ok(brief_from_workspace(&self.cwd, &self.identity))
    }

    async fn deliver_callback(&self, result: &TaskResult) -> Result<CallbackAck, HostError> {
        // Build the result object: speech-safe text + normalized links/log.
        let summary = speech_safe_summary(&redact_secrets(&result.speech_update));
        let mut result_obj = Map::new();
        result_obj.insert("task_id".to_owned(), Value::String(result.task_id.clone()));
        result_obj.insert(
            "status".to_owned(),
            Value::String(match result.handoff_state {
                TaskHandoffState::Accepted => "completed".to_owned(),
                TaskHandoffState::EnvelopePrepared => "queued".to_owned(),
                TaskHandoffState::Rejected => "failed".to_owned(),
            }),
        );
        result_obj.insert("summary".to_owned(), Value::String(summary.clone()));
        result_obj.insert("reply".to_owned(), Value::String(summary));
        let result_value = Value::Object(result_obj);

        // No inbox endpoint / no per-call key => fail-open (no sink configured).
        let Some(endpoint) = self.inbox_endpoint.clone() else {
            return Ok(CallbackAck {
                delivered: false,
                detail: "no callback sink configured".to_owned(),
            });
        };
        let tool_call_id = self
            .call_id
            .clone()
            .unwrap_or_else(|| result.task_id.clone());
        let key_b64u = std::env::var("CODEXINI_TOOL_ENVELOPE_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());

        let target = inbox_ws::InboxTarget {
            endpoint,
            tool_call_id,
            key_b64u,
            timeout: WS_TIMEOUT,
        };
        match inbox_ws::send_tool_result(&target, &result_value).await {
            Ok(()) => Ok(CallbackAck {
                delivered: true,
                detail: "delivered tool_result over the runtime-inbox WS".to_owned(),
            }),
            Err(e) => Err(HostError::Callback(e.to_string())),
        }
    }

    async fn deliver_call_summary(&self, transcript: &str) -> Result<CallbackAck, HostError> {
        // The trait default routes the recap through `deliver_callback`, which
        // needs a configured runtime-inbox WS — absent one (the common local
        // case), a whole call's transcript silently disappears. Write the full
        // redacted transcript to a local file ALWAYS (the skill's Step 5 reads
        // and summarizes it), and additionally push the speech-capped frame
        // over the runtime inbox when it is configured.
        let recap = redact_secrets(transcript);
        let recap = recap.trim();
        if recap.is_empty() {
            return Ok(CallbackAck {
                delivered: false,
                detail: "empty call; nothing to recap".to_owned(),
            });
        }
        let capped: String = recap.chars().take(crate::CALL_SUMMARY_MAX_CHARS).collect();
        let dir = self.cwd.join(".aura");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| HostError::Callback(e.to_string()))?;
        let path = dir.join("aura-last-call-recap.md");
        let body = format!(
            "# Voice call transcript (developer + Aura) - summarize this for the chat\n\n{capped}\n"
        );
        tokio::fs::write(&path, body)
            .await
            .map_err(|e| HostError::Callback(e.to_string()))?;

        // Best-effort hosted delivery on top of the file (never fails the ack:
        // the file already landed).
        let mut detail = format!("wrote the call transcript to {}", path.display());
        if self.inbox_endpoint.is_some() {
            let result = TaskResult {
                task_id: "voice-call-summary".to_owned(),
                handoff_state: TaskHandoffState::Accepted,
                speech_update: format!(
                    "Voice call transcript (developer + Aura) — summarize this for the chat:\n{capped}"
                ),
                envelope: TaskEnvelope::new(
                    "voice call summary",
                    Vec::new(),
                    "aura",
                    CallbackMode::default(),
                    String::new(),
                ),
            };
            match self.deliver_callback(&result).await {
                Ok(ack) if ack.delivered => {
                    detail.push_str("; also delivered over the runtime-inbox WS")
                }
                Ok(_) => {}
                Err(e) => detail.push_str(&format!("; runtime-inbox delivery failed ({e})")),
            }
        }
        Ok(CallbackAck {
            delivered: true,
            detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Mutex as StdMutex, OnceLock};

    /// Serialize tests that mutate process-global env (host-brief env vars).
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Run an async body on a fresh current-thread runtime. Used by the
    /// env-mutating tests so the `env_lock` guard is held across a synchronous
    /// `block_on` (not across an `.await`), keeping them serialized without
    /// tripping `clippy::await_holding_lock`.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    fn full_identity() -> HostSessionIdentity {
        let mut id = HostSessionIdentity::openclaw("acct-1", "agent-1", "sess-1");
        id.account_id = Some("acct-1".to_owned());
        id.channel = Some("telegram".to_owned());
        id.reply_to = Some("chat-1".to_owned());
        id
    }

    #[test]
    fn trigger_and_kind() {
        let a =
            OpenClawAdapter::configured("/tmp/x", Some(full_identity()), None, None, None, None);
        assert_eq!(a.kind(), HostKind::OpenClaw);
        assert_eq!(
            a.trigger_source(),
            TriggerSource::GatewayTool {
                method: "codexini_start_call".to_owned(),
                endpoint: "ws://127.0.0.1:18789".to_owned(),
            }
        );
    }

    #[test]
    fn read_context_path_a_from_env_json() {
        let _guard = env_lock();
        let host_brief = json!({
            "v": 2,
            "host_kind": "openclaw",
            "user": { "name": "Stas", "soul_summary": "builds voice infra", "interests": ["rust"] },
            "context": {
                "current_focus": "ship the openclaw adapter",
                "recent_messages_verbatim": [
                    { "role": "user", "text": "call me about the adapter" },
                    { "role": "openclaw", "text": "on it" }
                ]
            },
            "setup": {
                "preferences": "Be concise, warm, and never read code aloud.",
                "skills": ["research: deep dives"]
            },
            "call_intent": "outbound"
        });
        std::env::set_var("CODEXINI_OPENCLAW_HOST_BRIEF_JSON", host_brief.to_string());

        let adapter = OpenClawAdapter::configured(
            "/tmp/does-not-matter",
            Some(full_identity()),
            None,
            None,
            None,
            None,
        );
        let brief = block_on(adapter.read_context()).unwrap();
        std::env::remove_var("CODEXINI_OPENCLAW_HOST_BRIEF_JSON");

        assert_eq!(brief.host_kind.as_deref(), Some("open_claw"));
        assert_eq!(brief.user.name, "Stas");
        assert_eq!(brief.context.current_focus, "ship the openclaw adapter");
        assert_eq!(brief.context.recent_messages_verbatim.len(), 2);
        assert_eq!(brief.call_intent.as_deref(), Some("outbound"));
        // Secrets cleaned; setup carried through.
        assert!(brief.setup.is_some());
    }

    #[test]
    fn read_context_path_a_redacts_secrets() {
        let _guard = env_lock();
        let host_brief = json!({
            "v": 2, "host_kind": "openclaw",
            "user": { "name": "Stas", "soul_summary": "x", "interests": [] },
            "context": {
                "current_focus": "key is xai-FAKEKEYFORTESTINGONLY1234567890",
                "recent_messages_verbatim": []
            }
        });
        std::env::set_var("CODEXINI_OPENCLAW_HOST_BRIEF_JSON", host_brief.to_string());
        let adapter = OpenClawAdapter::new("/tmp/x");
        let brief = block_on(adapter.read_context()).unwrap();
        std::env::remove_var("CODEXINI_OPENCLAW_HOST_BRIEF_JSON");
        assert!(!brief.context.current_focus.contains("FAKEKEY"));
    }

    #[test]
    fn read_context_path_b_from_workspace() {
        let _guard = env_lock();
        // No host-brief env -> falls back to the workspace scraper.
        std::env::remove_var("CODEXINI_OPENCLAW_HOST_BRIEF_JSON");
        std::env::remove_var("CODEXINI_OPENCLAW_HOST_BRIEF_FILE");

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write(
            dir,
            "SYSTEM_PROMPT.md",
            "You are OpenClaw, a calm assistant.",
        );
        write(
            dir,
            "PREFERENCES.md",
            "Be concise and warm; never read code.",
        );
        write(dir, "soul.md", "I value craft and clarity.");
        write(dir, "INTERESTS.md", "- rust\n- audio");
        write(dir, "CRON.md", "[{\"id\":\"c1\",\"purpose\":\"backup\",\"schedule\":\"0 2 * * *\",\"last_summary\":\"ran ok\"}]");
        write(
            dir,
            "messages.jsonl",
            "{\"role\":\"user\",\"content\":\"hi openclaw\"}\n{\"role\":\"assistant\",\"content\":\"hello\"}\n{\"role\":\"user\",\"content\":\"what's next?\"}\n",
        );
        write(
            dir,
            "skills/research/SKILL.md",
            "# Research\ndescription: deep web dives",
        );

        let adapter =
            OpenClawAdapter::configured(dir, Some(full_identity()), None, None, None, None);
        let brief = block_on(adapter.read_context()).unwrap();
        assert_eq!(brief.host_kind.as_deref(), Some("open_claw"));
        assert!(brief.user.soul_summary.contains("craft"));
        assert!(!brief.context.recent_messages_verbatim.is_empty());
        assert_eq!(brief.context.current_focus, "what's next?");
        assert!(!brief.context.cron_jobs.is_empty());
        assert!(brief
            .setup
            .as_ref()
            .map(|s| !s.skills.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn read_context_fail_open_on_empty_workspace() {
        let _guard = env_lock();
        std::env::remove_var("CODEXINI_OPENCLAW_HOST_BRIEF_JSON");
        std::env::remove_var("CODEXINI_OPENCLAW_HOST_BRIEF_FILE");
        let tmp = tempfile::tempdir().unwrap();
        let adapter =
            OpenClawAdapter::configured(tmp.path(), Some(full_identity()), None, None, None, None);
        let brief = block_on(adapter.read_context()).unwrap();
        assert_eq!(brief.host_kind.as_deref(), Some("open_claw"));
        assert!(brief.context.recent_messages_verbatim.is_empty());
    }

    #[test]
    fn identity_prefers_upstream_mcp_names_over_legacy_shim_names() {
        let _guard = env_lock();
        let all = [
            "OPENCLAW_MCP_SESSION_KEY",
            "OPENCLAW_MCP_AGENT_ID",
            "OPENCLAW_MCP_ACCOUNT_ID",
            "OPENCLAW_MCP_MESSAGE_CHANNEL",
            "OPENCLAW_CHANNEL_CONTEXT",
            "OPENCLAW_SESSION_KEY",
            "OPENCLAW_ACCOUNT_ID",
            "OPENCLAW_AGENT_ID",
            "OPENCLAW_CHANNEL",
            "OPENCLAW_REPLY_TARGET",
        ];
        for k in all {
            std::env::remove_var(k);
        }
        // Upstream MCP names win over the legacy shim names.
        std::env::set_var("OPENCLAW_MCP_SESSION_KEY", "mcp-sess");
        std::env::set_var("OPENCLAW_SESSION_KEY", "legacy-sess");
        std::env::set_var("OPENCLAW_MCP_ACCOUNT_ID", "mcp-acct");
        std::env::set_var("OPENCLAW_MCP_MESSAGE_CHANNEL", "telegram");
        // Exec-spawn channel context supplies the reply target (chat id).
        std::env::set_var(
            "OPENCLAW_CHANNEL_CONTEXT",
            r#"{"sender":{"id":"u1"},"chat":{"id":12345}}"#,
        );
        let id = resolve_identity_from_env();
        for k in all {
            std::env::remove_var(k);
        }
        assert_eq!(id.session_key.as_deref(), Some("mcp-sess"));
        assert_eq!(id.account_id.as_deref(), Some("mcp-acct"));
        assert_eq!(id.channel.as_deref(), Some("telegram"));
        assert_eq!(id.reply_to.as_deref(), Some("12345"));
    }

    #[test]
    fn legacy_shim_names_still_resolve_when_mcp_absent() {
        let _guard = env_lock();
        for k in [
            "OPENCLAW_MCP_SESSION_KEY",
            "OPENCLAW_MCP_ACCOUNT_ID",
            "OPENCLAW_CHANNEL_CONTEXT",
        ] {
            std::env::remove_var(k);
        }
        std::env::set_var("OPENCLAW_SESSION_KEY", "legacy-sess");
        std::env::set_var("OPENCLAW_ACCOUNT_ID", "legacy-acct");
        std::env::set_var("OPENCLAW_REPLY_TARGET", "chat-9");
        let id = resolve_identity_from_env();
        for k in [
            "OPENCLAW_SESSION_KEY",
            "OPENCLAW_ACCOUNT_ID",
            "OPENCLAW_REPLY_TARGET",
        ] {
            std::env::remove_var(k);
        }
        assert_eq!(id.session_key.as_deref(), Some("legacy-sess"));
        assert_eq!(id.account_id.as_deref(), Some("legacy-acct"));
        assert_eq!(id.reply_to.as_deref(), Some("chat-9"));
    }

    #[test]
    fn detect_is_false_on_clean_tempdir() {
        let _guard = env_lock();
        // Clear env identity so detect() relies on device-state + TCP probe.
        for k in [
            "OPENCLAW_SESSION_KEY",
            "OPENCLAW_ACCOUNT_ID",
            "OPENCLAW_AGENT_ID",
            "OPENCLAW_CHANNEL",
            "OPENCLAW_REPLY_TARGET",
        ] {
            std::env::remove_var(k);
        }
        let tmp = tempfile::tempdir().unwrap();
        let blank = HostSessionIdentity {
            host: HostKind::OpenClaw,
            principal_id: String::new(),
            agent_id: None,
            session_id: None,
            session_key: None,
            requester_session_key: None,
            channel: None,
            reply_to: None,
            account_id: None,
        };
        let adapter = OpenClawAdapter::configured(
            tmp.path(),
            Some(blank),
            None,
            Some(tmp.path().join("state")),
            None,
            None,
        );
        assert!(!block_on(adapter.detect()));
    }

    #[tokio::test]
    async fn start_task_without_gateway_device_prepares_envelope() {
        let envelope = TaskEnvelope::new(
            "summarize the latest CI failure",
            vec![],
            "aura",
            CallbackMode::PingFirst,
            "approval-1",
        );
        let adapter = OpenClawAdapter::configured(
            "/tmp/x",
            Some(full_identity()),
            None,
            None, // no state_dir -> no gateway config
            None,
            Some("call-xyz".to_owned()),
        );
        let result = adapter.start_task(envelope).await;
        // No gateway device -> EnvelopePrepared, never a panic.
        assert_eq!(result.handoff_state, TaskHandoffState::EnvelopePrepared);
        assert!(!result.accepted());
    }

    #[tokio::test]
    async fn start_task_rejects_when_identity_incomplete() {
        // Missing channel/reply -> the consult builder rejects -> Rejected.
        let mut id = HostSessionIdentity::openclaw("acct", "agent", "sess");
        id.account_id = Some("acct".to_owned());
        // channel + reply_to left None.
        let adapter = OpenClawAdapter::configured(
            "/tmp/x",
            Some(id),
            None,
            Some("/tmp/state".into()),
            None,
            Some("call".to_owned()),
        );
        let envelope = TaskEnvelope::new("do it", vec![], "aura", CallbackMode::PingFirst, "appr");
        let result = adapter.start_task(envelope).await;
        assert_eq!(result.handoff_state, TaskHandoffState::Rejected);
    }

    #[tokio::test]
    async fn deliver_callback_without_sink_is_fail_open() {
        let adapter = OpenClawAdapter::configured(
            "/tmp/x",
            Some(full_identity()),
            None,
            None,
            None, // no inbox endpoint
            None,
        );
        let envelope = TaskEnvelope::new("intent", vec![], "aura", CallbackMode::PingFirst, "appr");
        let result = TaskResult {
            task_id: "openclaw-1".to_owned(),
            handoff_state: TaskHandoffState::Accepted,
            speech_update: "all done".to_owned(),
            envelope,
        };
        let ack = adapter.deliver_callback(&result).await.unwrap();
        assert!(!ack.delivered);
        assert_eq!(ack.detail, "no callback sink configured");
    }

    #[tokio::test]
    async fn deliver_callback_to_unreachable_inbox_is_host_error() {
        let adapter = OpenClawAdapter::configured(
            "/tmp/x",
            Some(full_identity()),
            None,
            None,
            Some("ws://127.0.0.1:9/".to_owned()),
            Some("call".to_owned()),
        );
        let envelope = TaskEnvelope::new("intent", vec![], "aura", CallbackMode::PingFirst, "appr");
        let result = TaskResult {
            task_id: "openclaw-1".to_owned(),
            handoff_state: TaskHandoffState::Accepted,
            speech_update: "done".to_owned(),
            envelope,
        };
        let err = adapter.deliver_callback(&result).await.unwrap_err();
        assert!(matches!(err, HostError::Callback(_)));
    }

    #[tokio::test]
    async fn host_adapter_is_object_safe() {
        // The engine holds `Arc<dyn HostAdapter>`; ensure OpenClawAdapter fits.
        fn accepts(_: &dyn HostAdapter) {}
        let a = OpenClawAdapter::new("/tmp/x");
        accepts(&a);
    }
}
