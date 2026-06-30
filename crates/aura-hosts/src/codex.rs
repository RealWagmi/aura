//! Codex host adapter.
//!
//! Trigger: launcher/env `AURA_AGENT=codex` (NOT a slash command).
//! Context: the dated rollout JSONL at `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`
//! (`CODEX_HOME` override) — there is NO `state_5.sqlite` (that was a bug).
//! Dispatch: spawn `codex app-server` and drive it over JSON-RPC — `thread/start`
//! then `turn/start`, awaiting the `turn/completed` notification so the engine
//! speaks Codex's result (like the Claude adapter blocks on `claude -p`). Cancel
//! maps to `turn/interrupt`.
//!
//! Scope: `CodexClient`, rollout discovery/parsing, and the dispatch surface —
//! trimmed of the coordinator / multi-worker machinery the `HostAdapter`
//! doesn't need.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, oneshot, Mutex};

use aura_core::brief::{Brief, RecentMessage, MAX_CURRENT_FOCUS};
use aura_core::host::HostKind;
use aura_core::tools::{
    AgentContext, AgentRuntime, AgentStatus, AttentionAck, AttentionRequest, CancelAck, CancelMode,
    TaskEnvelope, TaskHandoffState, TaskResult,
};
use aura_core::{redact_secrets, speech_safe_summary, CallbackMode};

use crate::{CallbackAck, HostAdapter, HostError, TriggerSource};

/// Env var + value that selects Codex as the active host.
pub const CODEX_TRIGGER_VAR: &str = "AURA_AGENT";
pub const CODEX_TRIGGER_VALUE: &str = "codex";
/// Channel label stamped onto messages read from a Codex rollout.
const CODEX_CHANNEL: &str = "codex-app-server";
/// Newest transcript messages carried into the brief (~30 pairs).
const RECENT_MESSAGE_TARGET: usize = 60;
/// Bounded tail read of a (possibly huge) rollout file.
const MAX_ROLLOUT_TAIL_BYTES: u64 = 512 * 1024;
/// Wall-clock ceiling for one dispatched Codex turn.
const CODEX_TURN_TIMEOUT: Duration = Duration::from_secs(20 * 60);

// =============================================================================
// CodexClient — JSON-RPC over a `codex app-server` child.
// =============================================================================

#[derive(Debug, thiserror::Error)]
pub enum CodexClientError {
    #[error("codex app-server io: {0}")]
    Io(#[from] std::io::Error),
    #[error("codex app-server response channel closed")]
    ResponseClosed,
    #[error("codex app-server request failed: {0}")]
    Request(String),
    #[error("codex app-server response missing result")]
    MissingResult,
}

type PendingMap = HashMap<u64, oneshot::Sender<Value>>;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_APP_SERVER_LINE_BYTES: usize = 1024 * 1024;

/// Drives a `codex app-server` child over its stdio JSON-RPC channel:
/// line-framed JSON, requests multiplexed by id, notifications fanned out on a
/// broadcast channel.
pub struct CodexClient {
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    notifications: broadcast::Sender<Value>,
    next_id: AtomicU64,
    _child: Mutex<Child>,
}

impl CodexClient {
    pub async fn spawn(app_server_bin: &Path) -> Result<Arc<Self>, CodexClientError> {
        let mut child = Command::new(app_server_bin)
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| {
            CodexClientError::Request("codex app-server stdin unavailable".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            CodexClientError::Request("codex app-server stdout unavailable".to_owned())
        })?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (notifications, _) = broadcast::channel(256);
        spawn_reader(stdout, pending.clone(), notifications.clone());
        let client = Arc::new(Self {
            stdin: Mutex::new(stdin),
            pending,
            notifications,
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        });
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "aura",
                        "title": "Aura Codex Integration",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notifications.subscribe()
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, CodexClientError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let payload = json!({ "id": id, "method": method, "params": params });
        if let Err(err) = self.write_line(&payload).await {
            let _ = self.pending.lock().await.remove(&id);
            return Err(err);
        }
        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return Err(CodexClientError::ResponseClosed),
            Err(_) => {
                let _ = self.pending.lock().await.remove(&id);
                return Err(CodexClientError::Request(format!(
                    "{method} timed out after {}s",
                    REQUEST_TIMEOUT.as_secs()
                )));
            }
        };
        if let Some(error) = response.get("error") {
            return Err(CodexClientError::Request(error.to_string()));
        }
        response
            .get("result")
            .cloned()
            .ok_or(CodexClientError::MissingResult)
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), CodexClientError> {
        self.write_line(&json!({ "method": method, "params": params }))
            .await
    }

    async fn write_line(&self, value: &Value) -> Result<(), CodexClientError> {
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(value.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }
}

fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    pending: Arc<Mutex<PendingMap>>,
    notifications: broadcast::Sender<Value>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut chunk = [0_u8; 4096];
        let mut line = Vec::with_capacity(4096);
        let mut dropping_oversized_line = false;
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => {
                    if !line.is_empty() && !dropping_oversized_line {
                        handle_app_server_line(&line, &pending, &notifications).await;
                    }
                    break;
                }
                Ok(n) => {
                    for byte in &chunk[..n] {
                        if *byte == b'\n' {
                            if !dropping_oversized_line && !line.is_empty() {
                                handle_app_server_line(&line, &pending, &notifications).await;
                            }
                            line.clear();
                            dropping_oversized_line = false;
                            continue;
                        }
                        if dropping_oversized_line {
                            continue;
                        }
                        if line.len() >= MAX_APP_SERVER_LINE_BYTES {
                            line.clear();
                            dropping_oversized_line = true;
                            continue;
                        }
                        line.push(*byte);
                    }
                }
                Err(err) => {
                    tracing::warn!(target: "aura_codex", "codex app-server stdout closed: {err}");
                    break;
                }
            }
        }
        let drained = pending.lock().await.drain().count();
        if drained > 0 {
            tracing::warn!(target: "aura_codex", drained, "codex reader exited with pending requests");
        }
    });
}

async fn handle_app_server_line(
    raw: &[u8],
    pending: &Arc<Mutex<PendingMap>>,
    notifications: &broadcast::Sender<Value>,
) {
    let line = String::from_utf8_lossy(raw);
    let Ok(value) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    if value.get("id").is_some() {
        let Some(id) = value.get("id").and_then(Value::as_u64) else {
            return;
        };
        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(value);
        }
        return;
    }
    if value.get("method").and_then(Value::as_str).is_some() {
        let _ = notifications.send(value);
    }
}

// =============================================================================
// Rollout reading: ~/.codex/sessions/**/rollout-*.jsonl -> Brief
// =============================================================================

/// `$CODEX_HOME/sessions` (else `$HOME/.codex/sessions`), if it exists.
fn codex_sessions_root() -> Option<PathBuf> {
    std::env::var("CODEX_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".codex"))
        })
        .map(|home| home.join("sessions"))
        .filter(|path| path.is_dir())
}

/// Recursively collect `(mtime, path)` for every `.jsonl` under `dir` (depth ≤ 6).
fn collect_rollouts(dir: &Path, depth: usize, out: &mut Vec<(SystemTime, PathBuf)>) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, depth + 1, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        out.push((modified, path));
    }
}

/// The `session_meta` first-line `payload.cwd` of a rollout.
fn rollout_cwd(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let first = text.lines().find(|l| !l.trim().is_empty())?;
    let value: Value = serde_json::from_str(first).ok()?;
    value
        .get("payload")
        .and_then(|p| p.get("cwd"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

/// Canonicalize-compare two paths (falls back to literal equality).
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Pick the newest rollout whose `cwd` matches, else the newest overall.
fn resolve_rollout(cwd: &Path) -> Option<PathBuf> {
    let root = codex_sessions_root()?;
    let mut files = Vec::new();
    collect_rollouts(&root, 0, &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    files
        .iter()
        .find(|(_, p)| rollout_cwd(p).is_some_and(|c| same_path(&c, cwd)))
        .map(|(_, p)| p.clone())
        .or_else(|| files.into_iter().next().map(|(_, p)| p))
}

/// Read the last `MAX_ROLLOUT_TAIL_BYTES` of a rollout as text (lossy UTF-8).
fn read_rollout_tail(path: &Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_ROLLOUT_TAIL_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        if let Some((_, rest)) = text.split_once('\n') {
            text = rest.to_owned();
        }
    }
    Ok(text)
}

/// Pull the message text out of a Codex `message` payload (handles `text`
/// string, `content` string, or `content[]` blocks).
fn codex_message_text(payload: &Value) -> Option<String> {
    if let Some(text) = payload.get("text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    let content = payload.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let text = content
        .as_array()?
        .iter()
        .filter_map(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .or_else(|| part.get("content").and_then(Value::as_str))
        })
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

/// Build a `RecentMessage` from a `message` payload (user/assistant only).
fn message_from_payload(payload: &Value, ts_iso: Option<String>) -> Option<RecentMessage> {
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let role = payload.get("role").and_then(Value::as_str)?;
    if !matches!(role, "user" | "assistant") {
        return None;
    }
    let text = codex_message_text(payload)?;
    if text.trim().is_empty() {
        return None;
    }
    Some(RecentMessage {
        role: role.to_owned(),
        text: redact_secrets(&text),
        channel: CODEX_CHANNEL.to_owned(),
        ts_iso,
    })
}

/// Parse a rollout's tail into chronological messages. Handles `message` lines
/// and `compacted` lines (whose `payload.replacement_history[]` holds messages).
fn parse_rollout(text: &str) -> Vec<RecentMessage> {
    let mut messages = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if value.get("type").and_then(Value::as_str) == Some("compacted") {
            if let Some(hist) = value
                .get("payload")
                .and_then(|p| p.get("replacement_history"))
                .and_then(Value::as_array)
            {
                for item in hist {
                    if let Some(m) = message_from_payload(item, None) {
                        messages.push(m);
                    }
                }
            }
            continue;
        }
        if let Some(payload) = value.get("payload") {
            if let Some(m) = message_from_payload(payload, ts) {
                messages.push(m);
            }
        }
    }
    let drop = messages.len().saturating_sub(RECENT_MESSAGE_TARGET);
    if drop > 0 {
        messages.drain(0..drop);
    }
    messages
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

// =============================================================================
// CodexAdapter
// =============================================================================

/// The Codex host adapter.
pub struct CodexAdapter {
    cwd: PathBuf,
    app_server_bin: PathBuf,
    model: Option<String>,
    /// Cached app-server client + coordinator thread id (spawned lazily).
    thread: Mutex<Option<(Arc<CodexClient>, String)>>,
    /// The in-flight `(thread_id, turn_id)` for cancellation.
    active: StdMutex<Option<(String, String)>>,
    last_envelope: StdMutex<Option<TaskEnvelope>>,
    task_counter: AtomicU64,
}

impl CodexAdapter {
    /// Adapter for a working directory using the default `codex` CLI on PATH.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            app_server_bin: PathBuf::from("codex"),
            model: None,
            thread: Mutex::new(None),
            active: StdMutex::new(None),
            last_envelope: StdMutex::new(None),
            task_counter: AtomicU64::new(1),
        }
    }

    /// Override the `codex` binary and/or the worker model.
    pub fn with_binary_and_model(
        cwd: impl Into<PathBuf>,
        app_server_bin: impl Into<PathBuf>,
        model: Option<String>,
    ) -> Self {
        let mut a = Self::new(cwd);
        a.app_server_bin = app_server_bin.into();
        a.model = model;
        a
    }

    /// Spawn (once) the app-server and start a coordinator thread, caching both.
    async fn ensure_thread(&self) -> Result<(Arc<CodexClient>, String), CodexClientError> {
        let mut guard = self.thread.lock().await;
        if let Some((client, thread_id)) = guard.as_ref() {
            return Ok((client.clone(), thread_id.clone()));
        }
        let client = CodexClient::spawn(&self.app_server_bin).await?;
        let result = client
            .request(
                "thread/start",
                json!({ "cwd": self.cwd.to_string_lossy(), "serviceName": "aura" }),
            )
            .await?;
        let thread_id = result
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CodexClientError::Request("thread/start returned no thread id".to_owned())
            })?
            .to_owned();
        *guard = Some((client.clone(), thread_id.clone()));
        Ok((client, thread_id))
    }
}

/// Does this notification belong to `(thread_id, turn_id)`?
fn notification_matches_turn(params: &Value, thread_id: &str, turn_id: &str) -> bool {
    if turn_id.is_empty() || !thread_id_matches(params, thread_id) {
        return false;
    }
    params
        .get("turnId")
        .and_then(Value::as_str)
        .map(|id| id == turn_id)
        .unwrap_or_else(|| {
            params
                .get("turn")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
                == Some(turn_id)
        })
}

fn thread_id_matches(params: &Value, thread_id: &str) -> bool {
    [
        params.get("threadId").and_then(Value::as_str),
        params.get("thread_id").and_then(Value::as_str),
        params
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .any(|id| id == thread_id)
}

/// Await a turn to completion, accumulating the assistant's streamed message.
/// Returns the assembled text (empty if the turn produced none).
async fn await_turn_result(
    client: &CodexClient,
    thread_id: &str,
    turn_id: &str,
    timeout: Duration,
) -> String {
    let mut notifications = client.subscribe();
    let mut text = String::new();
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return text;
        }
        match tokio::time::timeout(remaining, notifications.recv()).await {
            Ok(Ok(value)) => {
                let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                if !notification_matches_turn(&params, thread_id, turn_id) {
                    continue;
                }
                match method {
                    "item/agentMessage/delta" => {
                        if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                            text.push_str(delta);
                        }
                    }
                    "turn/completed" | "turn/failed" => return text,
                    _ => {}
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => return text,
            Err(_) => return text,
        }
    }
}

/// The voice-relayed task prompt handed to a Codex turn.
fn render_codex_prompt(envelope: &TaskEnvelope) -> String {
    let constraints = if envelope.constraints.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nConstraints to honor: {}",
            envelope.constraints.join("; ")
        )
    };
    format!(
        "The developer just asked, via voice (relayed through Aura): {}{}\n\nThis work is already \
         approved. Do it directly in this thread with full read/edit/run access. When done, reply \
         with a short voice-friendly paragraph summarizing what you did and the result — no code \
         blocks, file paths, line numbers, or stack traces (Aura speaks this aloud). If it's a \
         read-only question, just answer it. Callback policy: {}.",
        envelope.user_intent, constraints, envelope.callback_mode
    )
}

#[async_trait]
impl AgentRuntime for CodexAdapter {
    async fn status(&self) -> AgentStatus {
        let active = self.active.lock().ok().and_then(|g| g.clone()).is_some();
        AgentStatus {
            state: if active { "agent_working" } else { "idle" }.to_owned(),
            active_task: self
                .last_envelope
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .map(|e| e.user_intent)
                .filter(|i| !i.is_empty()),
            summary: "Codex adapter ready.".to_owned(),
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
            speech_briefing: "Codex session attached.".to_owned(),
            recent_changes: Vec::new(),
        }
    }

    async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
        if let Ok(mut last) = self.last_envelope.lock() {
            *last = Some(envelope.clone());
        }
        let id = self.task_counter.fetch_add(1, Ordering::SeqCst);
        let task_id = format!("codex-{id}");

        let (client, thread_id) = match self.ensure_thread().await {
            Ok(x) => x,
            Err(e) => {
                return TaskResult {
                    task_id,
                    handoff_state: TaskHandoffState::Rejected,
                    speech_update: speech_safe_summary(&format!("Codex is not ready: {e}")),
                    envelope,
                };
            }
        };

        let mut params = json!({
            "threadId": thread_id,
            "cwd": self.cwd.to_string_lossy(),
            "input": [{ "type": "text", "text": render_codex_prompt(&envelope) }],
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        let result = match client.request("turn/start", params).await {
            Ok(r) => r,
            Err(e) => {
                return TaskResult {
                    task_id,
                    handoff_state: TaskHandoffState::Rejected,
                    speech_update: speech_safe_summary(&format!(
                        "Codex could not start that task: {e}"
                    )),
                    envelope,
                };
            }
        };
        let Some(turn_id) = result
            .get("turn")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
        else {
            return TaskResult {
                task_id,
                handoff_state: TaskHandoffState::Rejected,
                speech_update:
                    "Codex started without a turn id; rejecting to avoid mixing results.".to_owned(),
                envelope,
            };
        };

        if let Ok(mut active) = self.active.lock() {
            *active = Some((thread_id.clone(), turn_id.clone()));
        }
        let raw = await_turn_result(&client, &thread_id, &turn_id, CODEX_TURN_TIMEOUT).await;
        if let Ok(mut active) = self.active.lock() {
            *active = None;
        }
        let summary = if raw.trim().is_empty() {
            format!("Codex finished the task: {}", envelope.user_intent)
        } else {
            speech_safe_summary(&raw)
        };
        TaskResult {
            task_id,
            handoff_state: TaskHandoffState::Accepted,
            speech_update: redact_secrets(&summary),
            envelope,
        }
    }

    async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
        let speech_update = match mode {
            CancelMode::Cancel => {
                let target = self.active.lock().ok().and_then(|g| g.clone());
                let client = self.thread.lock().await.as_ref().map(|(c, _)| c.clone());
                match (client, target) {
                    (Some(client), Some((thread_id, turn_id))) => {
                        match client
                            .request(
                                "turn/interrupt",
                                json!({ "threadId": thread_id, "turnId": turn_id }),
                            )
                            .await
                        {
                            Ok(_) => "Cancellation requested for the active Codex turn.",
                            Err(_) => "Codex did not accept the cancellation request.",
                        }
                    }
                    _ => "There is no active Codex task to cancel.",
                }
            }
            CancelMode::Pause | CancelMode::StopAfterCurrentStep => {
                "Codex only supports cancel; I can cancel the turn instead."
            }
        };
        CancelAck {
            task_id: task_id.to_owned(),
            mode,
            speech_update: speech_update.to_owned(),
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

#[async_trait]
impl HostAdapter for CodexAdapter {
    fn kind(&self) -> HostKind {
        HostKind::Codex
    }

    async fn detect(&self) -> bool {
        std::env::var(CODEX_TRIGGER_VAR).ok().as_deref() == Some(CODEX_TRIGGER_VALUE)
            || codex_sessions_root().is_some()
    }

    fn trigger_source(&self) -> TriggerSource {
        TriggerSource::LauncherEnv {
            var: CODEX_TRIGGER_VAR.to_owned(),
        }
    }

    async fn read_context(&self) -> Result<Brief, HostError> {
        let mut brief = Brief {
            host_kind: Some(HostKind::Codex.as_str().to_owned()),
            ..Brief::default()
        };
        if let Some(path) = resolve_rollout(&self.cwd) {
            if let Ok(text) = read_rollout_tail(&path) {
                let messages = parse_rollout(&text);
                if let Some(last_user) = messages.iter().rev().find(|m| m.role == "user") {
                    brief.context.current_focus =
                        truncate_chars(&last_user.text, MAX_CURRENT_FOCUS);
                }
                brief.context.recent_messages_verbatim = messages;
            }
        }
        brief.clamp();
        Ok(brief)
    }

    async fn deliver_callback(&self, result: &TaskResult) -> Result<CallbackAck, HostError> {
        // Post a visible note back into the coordinator thread.
        let client_and_thread = self.thread.lock().await.clone();
        let Some((client, thread_id)) = client_and_thread else {
            return Ok(CallbackAck {
                delivered: false,
                detail: "no codex thread available for callback delivery".to_owned(),
            });
        };
        // All callback text passes redact_secrets + speech_safe_summary.
        let note = format!(
            "Aura voice callback: {}",
            speech_safe_summary(&redact_secrets(&result.speech_update))
        );
        client
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "cwd": self.cwd.to_string_lossy(),
                    "input": [{ "type": "text", "text": note }],
                }),
            )
            .await
            .map_err(|e| HostError::Callback(e.to_string()))?;
        Ok(CallbackAck {
            delivered: true,
            detail: "posted callback into codex coordinator thread".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_rollout(dir: &Path, cwd: &str) -> PathBuf {
        let lines = [
            json!({"timestamp":"2026-06-29T10:00:00Z","payload":{"type":"session_meta","cwd":cwd,"id":"thread-1"}}),
            json!({"timestamp":"2026-06-29T10:00:05Z","payload":{"type":"message","role":"user","content":"refactor the planner"}}),
            json!({"timestamp":"2026-06-29T10:00:10Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done — planner split into modules."}]}}),
            json!({"timestamp":"2026-06-29T10:00:20Z","payload":{"type":"reasoning","text":"hidden"}}),
            json!({"timestamp":"2026-06-29T10:01:00Z","payload":{"type":"message","role":"user","content":"now add tests"}}),
        ];
        let dir = dir.join("2026/06/29");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-thread-1.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn parse_rollout_keeps_user_assistant_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_rollout(tmp.path(), "/repo/x");
        let text = std::fs::read_to_string(&path).unwrap();
        let msgs = parse_rollout(&text);
        assert_eq!(msgs.len(), 3); // 2 user + 1 assistant; reasoning line skipped
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].text, "refactor the planner");
        assert_eq!(msgs[0].channel, "codex-app-server");
        assert!(msgs
            .iter()
            .any(|m| m.text == "Done — planner split into modules."));
    }

    #[tokio::test]
    async fn read_context_maps_rollout_into_brief() {
        let tmp = tempfile::tempdir().unwrap();
        // Point CODEX_HOME at our fixture and match the rollout cwd to self.cwd.
        let codex_home = tmp.path().join("home");
        std::fs::create_dir_all(codex_home.join("sessions")).unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        write_rollout(&codex_home.join("sessions"), cwd.to_str().unwrap());
        std::env::set_var("CODEX_HOME", &codex_home);

        let adapter = CodexAdapter::new(&cwd);
        let brief = adapter.read_context().await.unwrap();
        assert_eq!(brief.host_kind.as_deref(), Some("codex"));
        assert!(!brief.context.recent_messages_verbatim.is_empty());
        assert_eq!(brief.context.current_focus, "now add tests");

        std::env::remove_var("CODEX_HOME");
    }

    #[test]
    fn trigger_is_launcher_env() {
        let adapter = CodexAdapter::new("/tmp/x");
        assert_eq!(
            adapter.trigger_source(),
            TriggerSource::LauncherEnv {
                var: "AURA_AGENT".to_owned()
            }
        );
        assert_eq!(adapter.kind(), HostKind::Codex);
    }

    #[test]
    fn notification_matching() {
        let p = json!({"threadId":"t1","turnId":"u1"});
        assert!(notification_matches_turn(&p, "t1", "u1"));
        assert!(!notification_matches_turn(&p, "t1", "u2"));
        assert!(!notification_matches_turn(&p, "tX", "u1"));
    }
}
