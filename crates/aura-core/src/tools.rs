//! Tool dispatch + the voice-approval trust boundary.
//!
//! Why this exists
//! ===============
//! [`ToolRouter`] is the in-process surface the realtime model calls to
//! act: query status, dispatch a worker task, ask the worker a
//! question, request attention, or hang up. Two responsibilities make
//! this module security-sensitive:
//!
//! - **Speech-safety on the way out.** Every response (status, context,
//!   task result, cancel/attention) is run through the redaction +
//!   speech filter before it reaches the model, so the router can't
//!   echo a secret or a raw path back into voice.
//! - **Approval can't be forged by the model.** Destructive actions
//!   (`start_agent_task`, destructive `pause_or_cancel_task`) require a
//!   one-time approval token that is minted from `/dev/urandom` and
//!   stored only as a SHA-256 hash *bound to the spoken intent* (see
//!   `approval_hash`). The model cannot manufacture approval by
//!   string-matching the intent, and a token minted for one intent
//!   fails `consume_*` against any other. `SafetyConfig.require_voice_approval
//!   = false` is rejected at construction — there is no "approval off"
//!   mode to slip through.
//!
//! Every tool the model is offered is handled: session-control tools
//! (`end_voice_session`, `pause_call_until`) are handled in the engine, and
//! the rest route through [`ToolRouter::handle`]. The ambient feeder is
//! inject-only, so there is no model-facing feeder-lookup tool.

use crate::{
    checkpoints::CheckpointStore, redact_secrets, speech_safe_summary, CallbackMode,
    CheckpointEvent, SafetyConfig,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::sync::mpsc;

pub type TaskId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub state: String,
    pub active_task: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContext {
    pub project: String,
    pub active_task: Option<String>,
    pub speech_briefing: String,
    pub recent_changes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEnvelope {
    pub user_intent: String,
    pub constraints: Vec<String>,
    pub project: String,
    pub callback_mode: CallbackMode,
    pub safety_mode: String,
    pub voice_approval_id: String,
}

impl TaskEnvelope {
    pub fn new(
        user_intent: impl Into<String>,
        constraints: Vec<String>,
        project: impl Into<String>,
        callback_mode: CallbackMode,
        voice_approval_id: impl Into<String>,
    ) -> Self {
        Self {
            user_intent: redact_secrets(&user_intent.into()),
            constraints: constraints
                .into_iter()
                .map(|constraint| redact_secrets(&constraint))
                .collect(),
            project: redact_secrets(&project.into()),
            callback_mode,
            safety_mode: "voice_model_never_edits_files".to_owned(),
            voice_approval_id: redact_secrets(&voice_approval_id.into()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskHandoffState {
    #[default]
    Accepted,
    EnvelopePrepared,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResult {
    pub task_id: TaskId,
    #[serde(default)]
    pub handoff_state: TaskHandoffState,
    pub speech_update: String,
    pub envelope: TaskEnvelope,
}

impl TaskResult {
    /// True ONLY when the agent is actually executing the task
    /// (`Accepted`). `EnvelopePrepared` (handoff-only — Claude not
    /// started, e.g. when `claude.execute_tasks=false`) and `Rejected`
    /// both return false. The wire-format `accepted` field has always
    /// meant "the task is being executed" — callers reading just that
    /// bool would otherwise mis-report envelope-only handoffs as
    /// running tasks. Use `handoff_state` for the richer distinction.
    /// Computed from `handoff_state` so state and bool can never
    /// disagree.
    pub fn accepted(&self) -> bool {
        matches!(self.handoff_state, TaskHandoffState::Accepted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    Pause,
    Cancel,
    StopAfterCurrentStep,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelAck {
    pub task_id: TaskId,
    pub mode: CancelMode,
    pub speech_update: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRequest {
    pub task_id: Option<TaskId>,
    pub reason: String,
    pub callback_mode: CallbackMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionAck {
    pub speech_update: String,
    pub requires_ack: bool,
}

#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn status(&self) -> AgentStatus;
    async fn context(&self) -> AgentContext;
    async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult;
    async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck;
    async fn request_attention(&self, request: AttentionRequest) -> AttentionAck;

    /// Optional stream of speech-safe progress events the worker emits while
    /// a task is active. Aura drains this into a `CheckpointStore` so that
    /// `get_context_summary` can answer "what's going on?" with real
    /// progress instead of a static briefing. The default returns `None`,
    /// meaning the adapter has no live progress to report.
    ///
    /// Adapters may only hand out the receiver once; subsequent calls are
    /// expected to return `None`.
    fn checkpoint_stream(&self) -> Option<mpsc::UnboundedReceiver<CheckpointEvent>> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResponse {
    pub name: String,
    pub content: Value,
    pub speech: String,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments for {tool}: {message}")]
    InvalidArguments { tool: String, message: String },
}

// Voice-approval mediation is a local trust boundary. The model can ask
// for a tool call, but it must not be able to manufacture the local-only
// approval fields by choosing matching strings. Tokens are random,
// one-time handles stored in this process and mapped to a redacted hash
// of the spoken approval text.

fn approval_hash(value: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(redact_secrets(value).as_bytes());
    let digest = hash.finalize();
    hex_digest(&digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);

fn random_local_token(prefix: &str) -> String {
    let mut bytes = [0_u8; 32];
    let filled = File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        let mut hash = Sha256::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        hash.update(now.to_le_bytes());
        hash.update(std::process::id().to_le_bytes());
        hash.update(TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
        bytes.copy_from_slice(&hash.finalize());
    }
    format!("{prefix}-{}", hex_digest(&bytes))
}

pub struct ToolRouter {
    agent: Arc<dyn AgentRuntime>,
    default_callback_mode: CallbackMode,
    safety: SafetyConfig,
    checkpoints: Option<Arc<CheckpointStore>>,
    checkpoints_to_speak: usize,
    allow_read_only_worker_queries: bool,
    task_approvals: Mutex<HashMap<String, String>>,
    cancel_approvals: Mutex<HashMap<String, String>>,
}

impl ToolRouter {
    pub fn with_safety(
        agent: Arc<dyn AgentRuntime>,
        default_callback_mode: CallbackMode,
        safety: SafetyConfig,
    ) -> Self {
        // Voice-approval is non-optional. The 5 `else` branches that used to
        // produce "voice-approval-disabled-by-config" were never reached — no
        // caller in the workspace ever set this to false, the default is true,
        // and the V1 contract requires the user to have spoken approval before
        // any task dispatch. Enforce the invariant at construction so the
        // runtime arms can collapse.
        assert!(
            safety.require_voice_approval,
            "SafetyConfig.require_voice_approval=false is not supported; \
             voice approval is non-negotiable for the V1 dispatch contract"
        );
        Self {
            agent,
            default_callback_mode,
            safety,
            checkpoints: None,
            checkpoints_to_speak: 0,
            allow_read_only_worker_queries: true,
            task_approvals: Mutex::new(HashMap::new()),
            cancel_approvals: Mutex::new(HashMap::new()),
        }
    }

    /// Attach a checkpoint store. When attached, `get_context_summary` will
    /// weave up to `recent` recent checkpoint speech lines into the
    /// briefing so the user-facing answer reflects real worker progress.
    pub fn with_checkpoint_store(mut self, store: Arc<CheckpointStore>, recent: usize) -> Self {
        self.checkpoints = Some(store);
        self.checkpoints_to_speak = recent;
        self
    }

    pub fn without_read_only_worker_queries(mut self) -> Self {
        self.allow_read_only_worker_queries = false;
        self
    }

    pub fn issue_task_approval(&self, user_intent: &str) -> Result<String, ToolError> {
        let token = random_local_token("task");
        let mut approvals =
            self.task_approvals
                .lock()
                .map_err(|_| ToolError::InvalidArguments {
                    tool: "start_agent_task".to_owned(),
                    message: "local approval store is unavailable".to_owned(),
                })?;
        approvals.insert(token.clone(), approval_hash(user_intent));
        Ok(token)
    }

    pub fn issue_cancel_confirmation(&self, task_id: &str) -> Result<String, ToolError> {
        let token = random_local_token("cancel");
        let mut approvals =
            self.cancel_approvals
                .lock()
                .map_err(|_| ToolError::InvalidArguments {
                    tool: "pause_or_cancel_task".to_owned(),
                    message: "local cancel store is unavailable".to_owned(),
                })?;
        approvals.insert(token.clone(), approval_hash(task_id));
        Ok(token)
    }

    pub fn validate_task_approval_args(&self, tool: &str, args: &Value) -> Result<(), ToolError> {
        let user_intent =
            string_arg(tool, args, "user_intent").or_else(|_| string_arg(tool, args, "intent"))?;
        let local_token = string_arg(tool, args, "_local_voice_approval_id")?;
        if local_token.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                tool: tool.to_owned(),
                message: "_local_voice_approval_id cannot be empty".to_owned(),
            });
        }
        self.consume_task(&local_token, &user_intent)
    }

    pub async fn handle(&self, call: ToolCall) -> Result<ToolResponse, ToolError> {
        match call.name.as_str() {
            "get_agent_status" => {
                let status = self.agent.status().await;
                let sanitized = sanitize_status(status);
                Ok(ToolResponse {
                    name: call.name,
                    speech: sanitized.summary.clone(),
                    content: serde_json::to_value(sanitized).expect("status serializes"),
                })
            }
            "get_context_summary" => {
                let context = self.agent.context().await;
                let sanitized = sanitize_context(context);
                let merged = merge_checkpoints_into_context(
                    sanitized,
                    self.checkpoints.as_deref(),
                    self.checkpoints_to_speak,
                );
                let speech = merged.speech_briefing.clone();
                Ok(ToolResponse {
                    name: call.name,
                    speech,
                    content: serde_json::to_value(merged).expect("context serializes"),
                })
            }
            "start_agent_task" => {
                let envelope = self.envelope_from_args(&call.name, &call.arguments)?;
                let result = self.agent.start_task(envelope).await;
                let speech = sanitize_speech(&result.speech_update);
                let content = serde_json::to_value(sanitize_task_result(result))
                    .expect("task result serializes");
                Ok(ToolResponse {
                    name: call.name,
                    speech,
                    content,
                })
            }
            // `ask_worker_question` is the worker-agnostic canonical name;
            // `ask_claude_question` stays as a backward-compatible alias so
            // existing prompt examples, dispatch logs, and downstream
            // consumers continue to work. Both names route through the exact
            // same handler; the only visible difference is the published tool
            // schema (see `tools.rs::tool_schema`), where the canonical name
            // ships first.
            "ask_worker_question" | "ask_claude_question" => {
                if !self.allow_read_only_worker_queries {
                    return Err(ToolError::InvalidArguments {
                        tool: call.name,
                        message: "read-only worker questions are disabled in this mode; answer from live context or wait for the context feeder".to_owned(),
                    });
                }
                // Thin alias: rewrap the question into a read-only
                // start_agent_task with an explicit "QUERY" prefix so
                // the active coding worker knows to answer rather than build. Aura still
                // sees a normal task envelope going out, the same
                // dispatch ack flow, and the same async result. The
                // distinction is purely semantic at the prompt layer
                // for Aura, and at the user_intent text for the worker.
                //
                // Voice approval still applies — querying production
                // code state is read-only but it still consumes a
                // worker turn and shows up in checkpoints. We validate
                // the approval against the QUESTION text (not the
                // QUERY-prefixed intent) so the developer's vocal
                // request is what gets hashed end-to-end.
                let question = string_arg(&call.name, &call.arguments, "question")?;
                if question.trim().is_empty() {
                    return Err(ToolError::InvalidArguments {
                        tool: call.name,
                        message: "question must not be empty".to_owned(),
                    });
                }
                let local_token =
                    string_arg(&call.name, &call.arguments, "_local_voice_approval_id")?;
                let approved_text = optional_string(&call.arguments, "_local_approved_user_intent")
                    .unwrap_or_else(|| question.clone());
                self.consume_task(&local_token, &approved_text)?;
                let intent = format!(
                    "QUERY (read-only — do not modify any files): {question}\n\n\
                     Answer in one or two short sentences. If the answer requires running a \
                     command, run it but do not edit code. Just report the answer."
                );
                let project = optional_string(&call.arguments, "project")
                    .unwrap_or_else(|| "current project".to_owned());
                let callback_mode = if let Some(raw) =
                    optional_string(&call.arguments, "callback_mode")
                {
                    raw.parse::<CallbackMode>()
                        .map_err(|message| ToolError::InvalidArguments {
                            tool: call.name.clone(),
                            message,
                        })?
                } else {
                    self.default_callback_mode
                };
                let envelope =
                    TaskEnvelope::new(intent, vec![], project, callback_mode, local_token);
                let result = self.agent.start_task(envelope).await;
                let speech = sanitize_speech(&result.speech_update);
                let content = serde_json::to_value(sanitize_task_result(result))
                    .expect("task result serializes");
                Ok(ToolResponse {
                    name: call.name,
                    speech,
                    content,
                })
            }
            "pause_or_cancel_task" => {
                let task_id = string_arg(&call.name, &call.arguments, "task_id")?;
                let mode = match optional_string(&call.arguments, "mode")
                    .unwrap_or_else(|| "pause".to_owned())
                    .as_str()
                {
                    "pause" => CancelMode::Pause,
                    "cancel" => CancelMode::Cancel,
                    "stop_after_current_step" => CancelMode::StopAfterCurrentStep,
                    other => {
                        return Err(ToolError::InvalidArguments {
                            tool: call.name,
                            message: format!("unknown cancel mode: {other}"),
                        })
                    }
                };
                if self.safety.require_cancel_confirmation && mode == CancelMode::Cancel {
                    if let Some(token) = optional_string(&call.arguments, "_local_cancel_token") {
                        let approved_text =
                            optional_string(&call.arguments, "_local_cancel_approved_text")
                                .unwrap_or_else(|| task_id.clone());
                        self.consume_cancel(&token, &approved_text)?;
                    } else {
                        let speech = "I need your confirmation before cancelling.";
                        return Ok(ToolResponse {
                            name: call.name,
                            speech: speech.to_owned(),
                            content: json!({
                                "pending_confirmation": true,
                                "task_id": redact_secrets(&task_id),
                                "mode": "cancel",
                                "speech_update": speech
                            }),
                        });
                    }
                }
                let ack = self.agent.pause_or_cancel(&task_id, mode).await;
                let speech = sanitize_speech(&ack.speech_update);
                let content =
                    serde_json::to_value(sanitize_cancel_ack(ack)).expect("cancel ack serializes");
                Ok(ToolResponse {
                    name: call.name,
                    speech,
                    content,
                })
            }
            "end_voice_session" => {
                // Speech-safe sign-off. The CLI live loop watches for this
                // tool name on the wire and treats the response as a signal
                // to clear playback and tear the WebSocket down — so the
                // model itself can hang the call up the moment the user
                // says "bye", "stop", "that's all", etc.
                let raw = optional_string(&call.arguments, "farewell")
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "Bye.".to_owned());
                let speech = sanitize_speech(&raw);
                Ok(ToolResponse {
                    name: call.name,
                    speech: speech.clone(),
                    content: json!({
                        "ended": true,
                        "speech_update": speech,
                    }),
                })
            }
            "request_user_attention" => {
                let reason = optional_string(&call.arguments, "reason")
                    .unwrap_or_else(|| "The agent needs your input.".to_owned());
                let task_id = optional_string(&call.arguments, "task_id");
                let request = AttentionRequest {
                    task_id,
                    reason,
                    callback_mode: self.default_callback_mode,
                };
                let ack = self.agent.request_attention(request).await;
                let speech = sanitize_speech(&ack.speech_update);
                let content = serde_json::to_value(AttentionAck {
                    speech_update: speech.clone(),
                    requires_ack: ack.requires_ack,
                })
                .expect("attention ack serializes");
                Ok(ToolResponse {
                    name: call.name,
                    speech,
                    content,
                })
            }
            other => Err(ToolError::UnknownTool(other.to_owned())),
        }
    }

    fn envelope_from_args(&self, tool: &str, args: &Value) -> Result<TaskEnvelope, ToolError> {
        let user_intent =
            string_arg(tool, args, "user_intent").or_else(|_| string_arg(tool, args, "intent"))?;
        let project =
            optional_string(args, "project").unwrap_or_else(|| "current project".to_owned());
        let constraints = args
            .get("constraints")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let callback_mode = if let Some(raw) = optional_string(args, "callback_mode") {
            raw.parse::<CallbackMode>()
                .map_err(|message| ToolError::InvalidArguments {
                    tool: tool.to_owned(),
                    message,
                })?
        } else {
            self.default_callback_mode
        };
        let local_token = string_arg(tool, args, "_local_voice_approval_id")?;
        self.validate_task_approval_args(tool, args)?;
        Ok(TaskEnvelope::new(
            user_intent,
            constraints,
            project,
            callback_mode,
            local_token,
        ))
    }

    fn consume_task(&self, token: &str, approved_text: &str) -> Result<(), ToolError> {
        let mut approvals =
            self.task_approvals
                .lock()
                .map_err(|_| ToolError::InvalidArguments {
                    tool: "start_agent_task".to_owned(),
                    message: "local approval store is unavailable".to_owned(),
                })?;
        match approvals.remove(token) {
            Some(expected) if expected == approval_hash(approved_text) => Ok(()),
            _ => Err(ToolError::InvalidArguments {
                tool: "start_agent_task".to_owned(),
                message: "local approval token does not match this task".to_owned(),
            }),
        }
    }

    fn consume_cancel(&self, token: &str, task_id: &str) -> Result<(), ToolError> {
        let mut approvals =
            self.cancel_approvals
                .lock()
                .map_err(|_| ToolError::InvalidArguments {
                    tool: "pause_or_cancel_task".to_owned(),
                    message: "local cancel store is unavailable".to_owned(),
                })?;
        match approvals.remove(token) {
            Some(expected) if expected == approval_hash(task_id) => Ok(()),
            _ => Err(ToolError::InvalidArguments {
                tool: "pause_or_cancel_task".to_owned(),
                message: "local cancel token does not match this task".to_owned(),
            }),
        }
    }
}

fn optional_string(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn string_arg(tool: &str, args: &Value, key: &str) -> Result<String, ToolError> {
    optional_string(args, key).ok_or_else(|| ToolError::InvalidArguments {
        tool: tool.to_owned(),
        message: format!("missing string argument `{key}`"),
    })
}

pub fn local_function_schemas(feeder_enabled: bool) -> Value {
    local_function_schemas_with_options(true, feeder_enabled)
}

pub fn local_function_schemas_without_read_only_worker_query(feeder_enabled: bool) -> Value {
    local_function_schemas_with_options(false, feeder_enabled)
}

fn local_function_schemas_with_options(
    include_read_only_worker_query: bool,
    feeder_enabled: bool,
) -> Value {
    // The ambient feeder (opt-in via `AURA_FEEDER`) is inject-only — it pushes
    // context digests into the call; the model never calls a tool for it. So its
    // guidance is woven into these two descriptions ONLY when the feeder is
    // actually enabled. With the feeder off (the default) the model must NOT be
    // told to "wait for the feeder digest" or that repo facts "arrive from the
    // feeder" — that source will never appear, so the guidance is feeder-free.
    let context_desc = format!(
        "Return speech-safe worker/coordinator status, including active worker progress and ETA status when available. Use this for dispatched-worker status, active-task recap, coordinator state, or 'how much longer' questions only when the user is not also asking for an edit/write/update. It does not read files, does not fetch repo/source facts, {}",
        if feeder_enabled {
            "does not query the feeder, and does not start a coding worker; exact source snippets and repo context should arrive from the feeder digest."
        } else {
            "and does not start a coding worker."
        }
    );
    let task_desc = format!(
        "Request a downstream coding task after the user has clearly approved worker execution. Use for code edits, docs updates, repo commands, verification, shipping, combined lookup+update requests, or explicit deep passes. If the user says to dispatch, run it, set up the task, set up the task for the worker, fix this, or have Codex/the worker handle it, this is the tool to call before speaking. Do not use for exact wording/source lookup/onboarding greeting questions, read-only repo inspection, code/source wiring explanations, \"look in the code\", \"pull that up\", \"pull this data from the code\", token/context-size lookup, or requests for file references/key snippets{}",
        if feeder_enabled {
            "; wait for feeder digest unless the user explicitly asks to dispatch Codex, run a task, update files, verify with tests, or do a deep pass. If the user says to pull/check/read something \"with the feeder\" or \"through the feeder\", that is not worker approval. Aura validates the approval locally."
        } else {
            " — these are read-only and do not need a coding worker; dispatch only when the user explicitly asks to dispatch Codex, run a task, update files, verify with tests, or do a deep pass. Aura validates the approval locally."
        }
    );
    let mut tools = vec![
        json!({"type":"function","name":"get_agent_status","description":"Return current coding agent status, including active worker progress and ETA bands when the agent provides them.","parameters":{"type":"object","properties":{}}}),
        json!({"type":"function","name":"get_context_summary","description": context_desc,"parameters":{"type":"object","properties":{}}}),
        // NOTE: there is no `request_feeder_lookup` tool — the ambient feeder is
        // inject-only (it pushes digests; the model never pulls). A lookup tool
        // the engine cannot route would only error, so none is published.
        json!({"type":"function","name":"start_agent_task","description": task_desc,"parameters":{"type":"object","properties":{"user_intent":{"type":"string"},"constraints":{"type":"array","items":{"type":"string"}},"project":{"type":"string"},"callback_mode":{"type":"string","description":"How to deliver the result when it lands. Valid values: 'ping_first' (default — ping then speak), 'speak_immediately' (just speak when it lands), 'silent_notification' (queue silently), 'hangup' (alias for ping_first)."}},"required":["user_intent"]}}),
    ];
    if include_read_only_worker_query {
        // V2 step 1: publish the worker-agnostic canonical name FIRST
        // so models prefer it; keep the legacy `ask_claude_question`
        // as a documented alias in second position. The router accepts
        // both. New prompt examples should reference
        // `ask_worker_question`; legacy examples remain valid.
        tools.push(json!({"type":"function","name":"ask_worker_question","description":"Ask the active coding worker (Claude or Codex, depending on agent mode) a quick read-only question about repo state. Worker-agnostic canonical name; supersedes ask_claude_question.","parameters":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}}));
        tools.push(json!({"type":"function","name":"ask_claude_question","description":"Legacy alias of ask_worker_question. Same handler, same semantics; kept for backward compatibility with older prompts and dispatch logs. Prefer ask_worker_question in new code.","parameters":{"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}}));
    }
    tools.extend([
        json!({"type":"function","name":"pause_or_cancel_task","description":"Request safe pause or cancellation of a downstream task. Destructive cancel returns a local confirmation request first.","parameters":{"type":"object","properties":{"task_id":{"type":"string"},"mode":{"type":"string","enum":["pause","cancel","stop_after_current_step"]}},"required":["task_id"]}}),
        json!({"type":"function","name":"request_user_attention","description":"Ask the user for attention when blocked or finished.","parameters":{"type":"object","properties":{"task_id":{"type":"string"},"reason":{"type":"string"}}}}),
        json!({"type":"function","name":"end_voice_session","description":"Hang up the live voice call immediately. Call this when the user says bye, goodbye, stop, hang up, end the call, that's all, or any clear sign-off.","parameters":{"type":"object","properties":{"farewell":{"type":"string"}}}}),
        // Engine-handled (session control), NOT routed through ToolRouter — no
        // voice-approval boundary (it starts no coding work).
        json!({"type":"function","name":"pause_call_until","description":"Pause the live voice call to save cost while a long dispatched task runs: the realtime session is collapsed (no tokens burned) while the task keeps running, then the call resumes automatically when the condition is met and you report the result. Use this right after dispatching a long task when there is nothing to discuss in the meantime. Conditions: 'task_complete' (resume when the task you just dispatched finishes — the usual choice), 'timeout' (resume after `seconds`), 'event' (resume on a named external signal).","parameters":{"type":"object","properties":{"until":{"type":"string","enum":["task_complete","timeout","event"]},"seconds":{"type":"integer","description":"Seconds to pause when until='timeout'."},"event":{"type":"string","description":"Event name when until='event'."}},"required":["until"]}}),
    ]);
    Value::Array(tools)
}

fn sanitize_speech(raw: &str) -> String {
    speech_safe_summary(raw)
}

fn sanitize_status(status: AgentStatus) -> AgentStatus {
    AgentStatus {
        state: redact_secrets(&status.state),
        active_task: status.active_task.map(|task| sanitize_speech(&task)),
        summary: sanitize_speech(&status.summary),
    }
}

fn sanitize_context(context: AgentContext) -> AgentContext {
    AgentContext {
        project: redact_secrets(&context.project),
        active_task: context.active_task.map(|task| sanitize_speech(&task)),
        speech_briefing: sanitize_speech(&context.speech_briefing),
        recent_changes: context
            .recent_changes
            .into_iter()
            .map(|change| sanitize_speech(&change))
            .collect(),
    }
}

fn merge_checkpoints_into_context(
    mut context: AgentContext,
    store: Option<&CheckpointStore>,
    take: usize,
) -> AgentContext {
    let Some(store) = store else { return context };
    if take == 0 || store.is_empty() {
        return context;
    }
    let recent = store.recent(take);
    if recent.is_empty() {
        return context;
    }
    // Append the most recent worker progress lines to the briefing so the
    // spoken answer reflects real activity. Each event was already
    // speech-safe at construction; we keep only their `speech` field.
    let progress: Vec<String> = recent.into_iter().map(|event| event.speech).collect();
    let progress_blob = progress.join(" ");
    if !progress_blob.trim().is_empty() {
        context.speech_briefing = if context.speech_briefing.trim().is_empty() {
            format!("Recent worker progress: {progress_blob}")
        } else {
            format!(
                "{} Recent worker progress: {progress_blob}",
                context.speech_briefing
            )
        };
    }
    // Also surface the events as recent_changes so structured consumers
    // (not just speech) can inspect them.
    context.recent_changes.extend(progress);
    context
}

#[derive(Debug, Serialize)]
struct ProviderTaskResult {
    task_id: TaskId,
    accepted: bool,
    handoff_state: TaskHandoffState,
    speech_update: String,
}

fn sanitize_task_result(result: TaskResult) -> ProviderTaskResult {
    ProviderTaskResult {
        task_id: redact_secrets(&result.task_id),
        accepted: result.accepted(),
        handoff_state: result.handoff_state,
        speech_update: sanitize_speech(&result.speech_update),
    }
}

fn sanitize_cancel_ack(ack: CancelAck) -> CancelAck {
    CancelAck {
        task_id: redact_secrets(&ack.task_id),
        mode: ack.mode,
        speech_update: sanitize_speech(&ack.speech_update),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    struct TestAgent {
        last_envelope: Mutex<Option<TaskEnvelope>>,
    }

    #[async_trait]
    impl AgentRuntime for TestAgent {
        async fn status(&self) -> AgentStatus {
            AgentStatus {
                state: "idle".to_owned(),
                active_task: None,
                summary: "No task is running.".to_owned(),
            }
        }

        async fn context(&self) -> AgentContext {
            AgentContext {
                project: "test".to_owned(),
                active_task: None,
                speech_briefing: "Tests passed.".to_owned(),
                recent_changes: vec![],
            }
        }

        async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
            *self.last_envelope.lock().await = Some(envelope.clone());
            TaskResult {
                task_id: "task-1".to_owned(),
                handoff_state: TaskHandoffState::Accepted,
                speech_update: "On it. I will keep the task safe.".to_owned(),
                envelope,
            }
        }

        async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
            CancelAck {
                task_id: task_id.to_owned(),
                mode,
                speech_update: "Paused. The coding task is preserved.".to_owned(),
            }
        }

        async fn request_attention(&self, request: AttentionRequest) -> AttentionAck {
            AttentionAck {
                speech_update: match request.callback_mode {
                    CallbackMode::PingFirst => "Hey, you there?".to_owned(),
                    CallbackMode::SpeakImmediately => request.reason,
                    CallbackMode::SilentNotification => "Notification sent.".to_owned(),
                },
                requires_ack: request.callback_mode == CallbackMode::PingFirst,
            }
        }
    }

    #[tokio::test]
    async fn routes_start_task_and_redacts_constraints() {
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router = ToolRouter::with_safety(
            agent.clone(),
            CallbackMode::PingFirst,
            SafetyConfig::default(),
        );
        let approval = router.issue_task_approval("migrate auth").unwrap();
        let response = router
            .handle(ToolCall {
                name: "start_agent_task".to_owned(),
                arguments: json!({
                    "user_intent":"migrate auth",
                    "constraints":["never store xai-FAKEKEYFORTESTINGONLY1234567890"],
                    "project":"aura",
                    "_local_voice_approval_id":approval
                }),
            })
            .await
            .unwrap();

        assert!(response.speech.contains("On it"));
        let envelope = agent.last_envelope.lock().await.clone().unwrap();
        assert!(!envelope.constraints[0].contains("xai-71"));
        assert_eq!(envelope.safety_mode, "voice_model_never_edits_files");
        assert!(!envelope.voice_approval_id.is_empty());
        assert!(!response.content.to_string().contains("voice_approval_id"));
        assert!(!response.content.to_string().contains("migrate auth"));
    }

    #[tokio::test]
    async fn rejects_task_without_voice_approval() {
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router =
            ToolRouter::with_safety(agent, CallbackMode::PingFirst, SafetyConfig::default());
        let error = router
            .handle(ToolCall {
                name: "start_agent_task".to_owned(),
                arguments: json!({"user_intent":"migrate auth"}),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("_local_voice_approval_id"));
    }

    #[tokio::test]
    async fn cancel_first_returns_pending_confirmation() {
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router =
            ToolRouter::with_safety(agent, CallbackMode::PingFirst, SafetyConfig::default());
        let response = router
            .handle(ToolCall {
                name: "pause_or_cancel_task".to_owned(),
                arguments: json!({"task_id":"task-1","mode":"cancel"}),
            })
            .await
            .unwrap();
        assert_eq!(response.content["pending_confirmation"], true);
        assert!(response.speech.contains("confirmation"));
    }

    #[tokio::test]
    async fn cancel_requires_local_confirmation_token() {
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router =
            ToolRouter::with_safety(agent, CallbackMode::PingFirst, SafetyConfig::default());
        let token = router.issue_cancel_confirmation("task-1").unwrap();
        let response = router
            .handle(ToolCall {
                name: "pause_or_cancel_task".to_owned(),
                arguments: json!({
                    "task_id":"task-1",
                    "mode":"cancel",
                    "_local_cancel_token": token
                }),
            })
            .await
            .unwrap();
        assert!(!response.content.to_string().contains("_local_cancel_token"));
        assert_eq!(response.content["mode"], "cancel");
    }

    #[tokio::test]
    async fn local_task_approval_is_intent_bound() {
        // The token IS approval_hash(user_intent), so misuse on a different
        // intent fails (hash mismatch), and a garbage literal fails the same
        // way.
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router =
            ToolRouter::with_safety(agent, CallbackMode::PingFirst, SafetyConfig::default());
        let approval = router.issue_task_approval("migrate auth").unwrap();
        let error = router
            .handle(ToolCall {
                name: "start_agent_task".to_owned(),
                arguments: json!({
                    "user_intent":"rewrite billing",
                    "_local_voice_approval_id":approval
                }),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not match"));
        let bogus = router
            .handle(ToolCall {
                name: "start_agent_task".to_owned(),
                arguments: json!({
                    "user_intent":"migrate auth",
                    "_local_voice_approval_id":"bogus-token"
                }),
            })
            .await
            .unwrap_err();
        assert!(bogus.to_string().contains("does not match"));
    }

    #[tokio::test]
    async fn context_summary_weaves_recent_checkpoints() {
        use crate::CheckpointKind;

        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let store = Arc::new(CheckpointStore::new(8, None));
        store
            .append(CheckpointEvent::new(
                CheckpointKind::ToolUse,
                "Edit on the migration helper",
            ))
            .unwrap();
        store
            .append(CheckpointEvent::new(
                CheckpointKind::Phase,
                "now verifying the new column",
            ))
            .unwrap();

        let router =
            ToolRouter::with_safety(agent, CallbackMode::PingFirst, SafetyConfig::default())
                .with_checkpoint_store(store, 5);
        let response = router
            .handle(ToolCall {
                name: "get_context_summary".to_owned(),
                arguments: json!({}),
            })
            .await
            .unwrap();
        // Both static briefing and accumulated progress must appear.
        assert!(response.speech.contains("Tests passed") || response.speech.contains("passing"));
        assert!(response.speech.contains("Recent worker progress"));
        assert!(response.content["recent_changes"].as_array().unwrap().len() >= 2);
    }

    #[tokio::test]
    async fn sanitizes_context_content_before_provider_output() {
        struct SecretContextAgent;

        #[async_trait]
        impl AgentRuntime for SecretContextAgent {
            async fn status(&self) -> AgentStatus {
                AgentStatus {
                    state: "idle".to_owned(),
                    active_task: None,
                    summary: "idle".to_owned(),
                }
            }

            async fn context(&self) -> AgentContext {
                AgentContext {
                    project: "aura".to_owned(),
                    active_task: Some("fix src/auth.rs:217".to_owned()),
                    speech_briefing: "API_KEY=abc12345678901234567890 failed at src/auth.rs:217"
                        .to_owned(),
                    recent_changes: vec!["xai-FAKEKEYFORTESTINGONLY1234567890".to_owned()],
                }
            }

            async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
                TaskResult {
                    task_id: "unused".to_owned(),
                    handoff_state: TaskHandoffState::Accepted,
                    speech_update: "unused".to_owned(),
                    envelope,
                }
            }

            async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
                CancelAck {
                    task_id: task_id.to_owned(),
                    mode,
                    speech_update: "unused".to_owned(),
                }
            }

            async fn request_attention(&self, _request: AttentionRequest) -> AttentionAck {
                AttentionAck {
                    speech_update: "unused".to_owned(),
                    requires_ack: false,
                }
            }
        }

        let router = ToolRouter::with_safety(
            Arc::new(SecretContextAgent),
            CallbackMode::PingFirst,
            SafetyConfig::default(),
        );
        let response = router
            .handle(ToolCall {
                name: "get_context_summary".to_owned(),
                arguments: json!({}),
            })
            .await
            .unwrap();
        let serialized = response.content.to_string();
        assert!(!serialized.contains("abc123"));
        assert!(!serialized.contains("FAKEKEY"));
        assert!(!serialized.contains("217"));
    }

    #[tokio::test]
    async fn ask_claude_question_routes_through_start_task_with_query_prefix() {
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router = ToolRouter::with_safety(
            agent.clone(),
            CallbackMode::PingFirst,
            SafetyConfig::default(),
        );
        // ask_claude_question still requires a voice approval (read-only
        // doesn't bypass the safety check — every dispatch consumes a worker
        // turn). Reuse the start_agent_task approval flow.
        let approval = router
            .issue_task_approval("How many tests are passing?")
            .unwrap();
        let response = router
            .handle(ToolCall {
                name: "ask_claude_question".to_owned(),
                arguments: json!({
                    "question": "How many tests are passing?",
                    "_local_voice_approval_id": approval
                }),
            })
            .await
            .unwrap();

        assert_eq!(response.name, "ask_claude_question");
        assert!(response.speech.contains("On it"));
        let envelope = agent.last_envelope.lock().await.clone().unwrap();
        // The user_intent must carry the QUERY prefix so Claude knows to
        // answer rather than build.
        assert!(
            envelope.user_intent.starts_with("QUERY"),
            "expected user_intent to start with QUERY, got: {}",
            envelope.user_intent
        );
        assert!(envelope.user_intent.contains("How many tests are passing?"));
        // Read-only intent should be visible.
        assert!(envelope.user_intent.contains("read-only"));
    }

    #[tokio::test]
    async fn ask_worker_question_canonical_alias_routes_identically() {
        // V2 step 1: `ask_worker_question` is the new worker-agnostic
        // canonical name and must produce the same envelope as the
        // legacy `ask_claude_question` alias.
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router = ToolRouter::with_safety(
            agent.clone(),
            CallbackMode::PingFirst,
            SafetyConfig::default(),
        );
        let approval = router
            .issue_task_approval("Where does CodexAdapter live?")
            .unwrap();
        let response = router
            .handle(ToolCall {
                name: "ask_worker_question".to_owned(),
                arguments: json!({
                    "question": "Where does CodexAdapter live?",
                    "_local_voice_approval_id": approval
                }),
            })
            .await
            .unwrap();
        // Response.name echoes the *called* name so logs preserve the
        // distinction between legacy + canonical without losing
        // semantics — matches the existing `ask_claude_question`
        // behaviour.
        assert_eq!(response.name, "ask_worker_question");
        let envelope = agent.last_envelope.lock().await.clone().unwrap();
        assert!(
            envelope.user_intent.starts_with("QUERY"),
            "expected user_intent to start with QUERY, got: {}",
            envelope.user_intent
        );
        assert!(envelope
            .user_intent
            .contains("Where does CodexAdapter live?"));
        assert!(envelope.user_intent.contains("read-only"));
    }

    #[tokio::test]
    async fn local_function_schemas_publish_both_worker_question_names() {
        // The schema published to Grok must surface the new canonical
        // FIRST so models pick it preferentially, with the legacy
        // alias retained for backward compatibility.
        let schemas = local_function_schemas(false);
        let names: Vec<&str> = schemas
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.get("name").and_then(|v| v.as_str()))
            .collect();
        let canonical_idx = names.iter().position(|n| *n == "ask_worker_question");
        let legacy_idx = names.iter().position(|n| *n == "ask_claude_question");
        let canonical_idx = canonical_idx.expect("ask_worker_question must be published");
        let legacy_idx = legacy_idx.expect("ask_claude_question must remain published");
        assert!(
            canonical_idx < legacy_idx,
            "ask_worker_question must be listed before ask_claude_question \
             so models prefer the canonical name (canonical={}, legacy={})",
            canonical_idx,
            legacy_idx
        );
    }

    #[tokio::test]
    async fn ask_claude_question_rejects_empty_question() {
        let agent = Arc::new(TestAgent {
            last_envelope: Mutex::new(None),
        });
        let router =
            ToolRouter::with_safety(agent, CallbackMode::PingFirst, SafetyConfig::default());
        let approval = router.issue_task_approval("noop").unwrap();
        let err = router
            .handle(ToolCall {
                name: "ask_claude_question".to_owned(),
                arguments: json!({
                    "question": "   ",
                    "_local_voice_approval_id": approval
                }),
            })
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidArguments { tool, message } => {
                assert_eq!(tool, "ask_claude_question");
                assert!(message.contains("must not be empty"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn local_function_schemas_includes_ask_claude_question() {
        let schemas = local_function_schemas(false);
        let arr = schemas.as_array().expect("schemas is array");
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|s| s.get("name").and_then(Value::as_str))
            .collect();
        assert!(
            names.contains(&"ask_claude_question"),
            "ask_claude_question missing from schemas: {:?}",
            names
        );
        // ensure required parameter is exactly "question"
        let entry = arr
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("ask_claude_question"))
            .unwrap();
        let required = entry["parameters"]["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "question");
    }

    #[test]
    fn local_function_schemas_keep_exact_wording_out_of_worker_tools() {
        // Feeder ON: the feeder-referencing guidance is present.
        let schemas = local_function_schemas_without_read_only_worker_query(true);
        let arr = schemas.as_array().expect("schemas is array");

        let context_description = arr
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("get_context_summary"))
            .and_then(|s| s.get("description"))
            .and_then(Value::as_str)
            .expect("get_context_summary description");
        assert!(context_description.contains("worker/coordinator status"));
        assert!(context_description.contains("active-task recap"));
        assert!(context_description.contains("not also asking for an edit/write/update"));
        assert!(context_description.contains("does not fetch repo/source facts"));
        assert!(context_description.contains("does not query the feeder"));
        assert!(context_description.contains("repo context should arrive from the feeder digest"));
        assert!(context_description.contains("does not start a coding worker"));

        // The ambient feeder is inject-only: there is no
        // model-facing `request_feeder_lookup` tool — publishing one would offer
        // the model a tool the engine has no handler for (it would only error).
        assert!(
            arr.iter()
                .all(|s| s.get("name").and_then(Value::as_str) != Some("request_feeder_lookup")),
            "request_feeder_lookup must not be published (no handler exists)"
        );

        let task_description = arr
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("start_agent_task"))
            .and_then(|s| s.get("description"))
            .and_then(Value::as_str)
            .expect("start_agent_task description");
        assert!(task_description.contains("code edits"));
        assert!(task_description.contains("docs updates"));
        assert!(task_description.contains("combined lookup+update requests"));
        assert!(task_description.contains("set up the task for the worker"));
        assert!(task_description.contains("Do not use for exact wording"));
        assert!(task_description.contains("read-only repo inspection"));
        assert!(task_description.contains("code/source wiring explanations"));
        assert!(task_description.contains("pull that up"));
        assert!(task_description.contains("pull this data from the code"));
        assert!(task_description.contains("token/context-size lookup"));
        assert!(task_description.contains("\"with the feeder\""));
        assert!(task_description.contains("requests for file references/key snippets"));
        assert!(task_description.contains("wait for feeder digest"));
    }

    #[test]
    fn feeder_disabled_schemas_carry_no_feeder_wording() {
        // Feeder OFF (the default): no tool description may mention the feeder —
        // the model must not be told to wait for a digest that never arrives.
        let schemas = local_function_schemas(false);
        let arr = schemas.as_array().expect("schemas is array");
        for tool in arr {
            let desc = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert!(
                !desc.to_ascii_lowercase().contains("feeder"),
                "feeder-off schema must not mention the feeder, but {:?} does: {desc}",
                tool.get("name").and_then(Value::as_str).unwrap_or("?")
            );
        }
        // The feeder-free guidance still gates dispatch correctly.
        let task_description = arr
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("start_agent_task"))
            .and_then(|s| s.get("description"))
            .and_then(Value::as_str)
            .expect("start_agent_task description");
        assert!(task_description.contains("do not need a coding worker"));
        assert!(task_description.contains("dispatch only when the user explicitly asks"));
        let context_description = arr
            .iter()
            .find(|s| s.get("name").and_then(Value::as_str) == Some("get_context_summary"))
            .and_then(|s| s.get("description"))
            .and_then(Value::as_str)
            .expect("get_context_summary description");
        assert!(context_description.contains("does not start a coding worker"));
    }

    #[test]
    fn task_result_accepted_matches_handoff_state_only_for_accepted() {
        // The wire-format `accepted` field has
        // always meant "the task is actually being executed". The
        // EnvelopePrepared state — used when claude.execute_tasks=false
        // returns a handoff envelope WITHOUT starting Claude — must
        // round-trip as accepted=false. Anything else lets callers
        // reading just the bool report envelope-only handoffs as
        // running tasks (semantic regression).
        let envelope = TaskEnvelope::new(
            "noop intent",
            vec![],
            "aura",
            CallbackMode::PingFirst,
            "test-approval",
        );
        // Accepted -> accepted=true
        let accepted = TaskResult {
            task_id: "task-accepted".to_owned(),
            handoff_state: TaskHandoffState::Accepted,
            speech_update: "On it.".to_owned(),
            envelope: envelope.clone(),
        };
        assert!(accepted.accepted());
        let wire = sanitize_task_result(accepted);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["accepted"], true);
        assert_eq!(json["handoff_state"], "accepted");

        // EnvelopePrepared -> accepted=false (the regression Codex
        // flagged: prior collapse to derived method made this true).
        let prepared = TaskResult {
            task_id: "task-envelope".to_owned(),
            handoff_state: TaskHandoffState::EnvelopePrepared,
            speech_update: "Envelope prepared.".to_owned(),
            envelope: envelope.clone(),
        };
        assert!(!prepared.accepted());
        let wire = sanitize_task_result(prepared);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["accepted"], false);
        assert_eq!(json["handoff_state"], "envelope_prepared");

        // Rejected -> accepted=false
        let rejected = TaskResult {
            task_id: "task-rejected".to_owned(),
            handoff_state: TaskHandoffState::Rejected,
            speech_update: "Rejected.".to_owned(),
            envelope,
        };
        assert!(!rejected.accepted());
        let wire = sanitize_task_result(rejected);
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["accepted"], false);
        assert_eq!(json["handoff_state"], "rejected");
    }
}
