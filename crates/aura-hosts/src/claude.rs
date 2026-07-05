//! Claude Code host adapter.
//!
//! Trigger: the deterministic slash command `/aura:aura-live`. Context: the
//! Claude Code session transcript at
//! `~/.claude/projects/<encoded cwd>/<session-uuid>.jsonl`, read into a
//! [`Brief`]. Reading is **fail-open**: a missing/empty transcript yields a
//! thin brief (host kind set, no messages) and the call still proceeds.
//!
//! ## Scope
//!
//! Read path: transcript → [`Brief`]. Dispatch: [`AgentRuntime::start_task`]
//! runs `claude -p` in the project dir when execution is enabled
//! (`ClaudeAdapter::executing`), giving the in-call voice model Claude Code's
//! full repo + tool access (read/edit/bash); with execution off it returns the
//! first-class [`TaskHandoffState::EnvelopePrepared`] handoff. Not yet wired:
//! the `.aura` callback delivery into the chat, the live transcript tailer /
//! `aura-needs-user-input` watcher, and the `notify`-based file watching —
//! `checkpoint_stream` keeps the trait default (`None`) until then.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use async_trait::async_trait;
use serde_json::{json, Value};

use aura_core::brief::{Brief, RecentMessage, MAX_CURRENT_FOCUS};
use aura_core::host::HostKind;
use aura_core::tools::{
    AgentContext, AgentRuntime, AgentStatus, AttentionAck, AttentionRequest, CancelAck, CancelMode,
    TaskEnvelope, TaskHandoffState, TaskResult,
};
use aura_core::{redact_secrets, speech_safe_summary, CallbackMode, ClaudeConfig};

use crate::{CallbackAck, HostAdapter, HostError, TriggerSource};

/// The slash command that triggers a Claude call (deterministic).
pub const CLAUDE_TRIGGER: &str = "/aura:aura-live";

/// Channel label stamped onto messages read from a Claude transcript.
const CLAUDE_CHANNEL: &str = "claude-code";

/// How many of the most recent transcript messages to carry into the brief.
/// Targets ~30 user/assistant pairs; `Brief::clamp` then enforces
/// the hard `MAX_RECENT_MESSAGES` ceiling.
const RECENT_MESSAGE_TARGET: usize = 60;

/// Largest transcript tail we read when building context (bytes). The active
/// session can be large; we only need the recent tail.
const MAX_TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;

// --- Transcript discovery -----------------------------------------------------

/// `~/.claude/projects/<cwd with '/' replaced by '-'>` — the directory Claude
/// Code stores this working directory's session transcripts in.
fn claude_project_dir(cwd: &Path) -> Option<PathBuf> {
    let cwd_str = cwd.to_str()?;
    let encoded = cwd_str.replace('/', "-");
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".claude/projects").join(encoded))
}

/// The most-recently-modified `.jsonl` in `dir`. Prefers a non-trivial
/// (>1 KiB) transcript, else falls back to the newest of any size.
fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut by_mtime: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        by_mtime.push((modified, meta.len(), path));
    }
    by_mtime.sort_by(|a, b| b.0.cmp(&a.0));
    by_mtime
        .iter()
        .find(|(_, len, _)| *len > 1024)
        .map(|(_, _, p)| p.clone())
        .or_else(|| by_mtime.into_iter().next().map(|(_, _, p)| p))
}

/// Read up to the last `MAX_TRANSCRIPT_TAIL_BYTES` of a transcript file as
/// text (lossy UTF-8). Returns `None` on any IO error.
fn read_transcript_tail_text(path: &Path) -> io::Result<String> {
    use io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(MAX_TRANSCRIPT_TAIL_BYTES);
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    // If we started mid-file, the first (partial) line is junk — drop it.
    if start > 0 {
        if let Some((_, rest)) = text.split_once('\n') {
            text = rest.to_owned();
        }
    }
    Ok(text)
}

// --- Transcript -> messages ---------------------------------------------------

#[derive(Clone, Copy)]
enum Role {
    User,
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Pull plain message text out of one transcript JSONL event. `User` content
/// can be a bare string or an array of blocks; `Assistant` content is an array
/// where only typed `text` blocks are kept (tool_use / thinking skipped).
fn extract_message_text(value: &Value, role: Role) -> Option<String> {
    let content_opt = value.get("message").and_then(|m| m.get("content"));
    let text = match role {
        Role::User => {
            let content = content_opt.or_else(|| value.get("content"))?;
            match content {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .iter()
                    .filter_map(|v| {
                        v.get("text")
                            .and_then(Value::as_str)
                            .or_else(|| v.as_str())
                            .map(str::to_owned)
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => return None,
            }
        }
        Role::Assistant => {
            let arr = content_opt?.as_array()?;
            let parts: Vec<&str> = arr
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .filter(|t| !t.trim().is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            parts.join("\n")
        }
    };
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Parse a transcript's tail into chronological [`RecentMessage`]s (newest
/// `RECENT_MESSAGE_TARGET` kept). Every text is `redact_secrets`'d as defense
/// in depth. `ts_iso` is carried from the event's `timestamp` when present.
fn parse_transcript_messages(text: &str) -> Vec<RecentMessage> {
    let mut messages: Vec<RecentMessage> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let role = match value.get("type").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let Some(text) = extract_message_text(&value, role) else {
            continue;
        };
        let ts_iso = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_owned);
        messages.push(RecentMessage {
            role: role.as_str().to_owned(),
            text: redact_secrets(&text),
            channel: CLAUDE_CHANNEL.to_owned(),
            ts_iso,
        });
    }
    let drop = messages.len().saturating_sub(RECENT_MESSAGE_TARGET);
    if drop > 0 {
        messages.drain(0..drop);
    }
    messages
}

/// Extract the model the host Claude session is using — the `message.model` of
/// the most recent assistant event in the transcript. Lets the in-call dispatch
/// run `claude -p` with the SAME model the developer was chatting with (Scheme 1,
/// see docs/DISPATCH.md) instead of the CLI default. Returns None if the
/// transcript is unreadable or records no model.
fn latest_transcript_model(path: &Path) -> Option<String> {
    let text = read_transcript_tail_text(path).ok()?;
    let mut model: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        // Cheap pre-filter before the JSON parse.
        if line.is_empty() || !line.contains("\"model\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(m) = value
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .filter(|m| is_plausible_model_id(m))
        {
            model = Some(m.to_owned());
        }
    }
    model
}

/// Is `s` a plausible model id to pass to `claude -p --model`? Real ids are
/// short slug-like tokens (`claude-sonnet-4-5`, `gpt-4o`, a date-suffixed
/// variant). This REJECTS the placeholders that appear in real transcripts —
/// most importantly `<synthetic>` (Claude Code's marker for synthesized
/// assistant turns), which is NOT a dispatchable model and would make the worker
/// fail. Guard: non-empty, no whitespace, no angle brackets, and only the
/// characters a model slug uses (`[A-Za-z0-9._:-]`).
fn is_plausible_model_id(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

// --- Adapter -----------------------------------------------------------------

/// The Claude Code host adapter.
#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    cwd: PathBuf,
    transcript_path: Option<PathBuf>,
    transcripts_dir: Option<PathBuf>,
    hooks_dir: Option<PathBuf>,
    execution: ClaudeExecutionOptions,
    last_envelope: Arc<Mutex<Option<TaskEnvelope>>>,
    task_counter: Arc<AtomicU64>,
}

impl ClaudeAdapter {
    /// Build from a working directory and the resolved [`ClaudeConfig`].
    pub fn with_config(cwd: impl Into<PathBuf>, config: &ClaudeConfig) -> Self {
        Self {
            cwd: cwd.into(),
            transcript_path: config.transcript_path.clone(),
            transcripts_dir: config.transcripts_dir.clone(),
            hooks_dir: config.hooks_dir.clone(),
            execution: ClaudeExecutionOptions::from(config),
            last_envelope: Arc::new(Mutex::new(None)),
            task_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Build for a working directory with task execution ENABLED — the in-call
    /// dispatch path: `start_task` runs `claude -p` in `cwd`, giving the voice
    /// model Claude Code's repo + tool access (read/edit/bash).
    pub fn executing(cwd: impl Into<PathBuf>) -> Self {
        Self::executing_with_dispatch_model(cwd, None)
    }

    /// Like [`executing`](Self::executing) but pinning the in-call dispatch model
    /// (`ClaudeConfig::dispatch_model`). `None` keeps the per-dispatch transcript
    /// auto-detection (Scheme 1); `Some(m)` forces `claude -p --model <m>`. The
    /// server threads this from the `AURA_DISPATCH_MODEL` env at
    /// [`build_host`](crate::build_host).
    pub fn executing_with_dispatch_model(
        cwd: impl Into<PathBuf>,
        dispatch_model: Option<String>,
    ) -> Self {
        let cwd = cwd.into();
        // Default the hooks dir to `<cwd>/.aura/hooks` so the post-call summary
        // (and in-call dispatch callbacks) are actually written somewhere the
        // Claude skill / Stop hook can read them — the recap file
        // `aura-last-claude-result.json`. Without this the default config leaves
        // `hooks_dir = None` and `deliver_callback` no-ops, silently dropping the
        // recap. `.aura/` is gitignored. A loaded config can still override it.
        let config = ClaudeConfig {
            execute_tasks: true,
            hooks_dir: Some(cwd.join(".aura").join("hooks")),
            dispatch_model,
            ..ClaudeConfig::default()
        };
        let mut adapter = Self::with_config(cwd, &config);
        // Pin the developer's chat transcript AS OF call start. Model auto-detect
        // otherwise re-scans newest-by-mtime on every dispatch — and an in-call
        // `claude -p` worker writes its OWN transcript, which would then be the
        // newest, making detection self-referential (the worker's model, not the
        // developer's). Capturing it once here freezes the reference to the
        // session that launched the call. (An explicit configured path wins and
        // is left as-is.)
        if adapter.transcript_path.is_none() {
            adapter.transcript_path = adapter.resolve_transcript();
        }
        adapter
    }

    /// Build with default config for a working directory (transcript is then
    /// auto-discovered under `~/.claude/projects/<encoded cwd>`).
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_config(cwd, &ClaudeConfig::default())
    }

    /// Resolve which transcript file to read: an explicit configured path, else
    /// the newest `.jsonl` in the configured dir, else the newest in the
    /// auto-discovered project dir for `cwd`.
    fn resolve_transcript(&self) -> Option<PathBuf> {
        if let Some(path) = &self.transcript_path {
            return Some(path.clone());
        }
        if let Some(dir) = &self.transcripts_dir {
            if let Some(found) = newest_jsonl(dir) {
                return Some(found);
            }
        }
        let dir = claude_project_dir(&self.cwd)?;
        newest_jsonl(&dir)
    }
}

#[async_trait]
impl AgentRuntime for ClaudeAdapter {
    async fn status(&self) -> AgentStatus {
        let active_task = self
            .last_envelope
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|e| e.user_intent)
            .filter(|i| !i.is_empty());
        AgentStatus {
            state: if active_task.is_some() {
                "agent_working".to_owned()
            } else {
                "idle".to_owned()
            },
            active_task,
            summary: "Claude Code adapter ready.".to_owned(),
        }
    }

    async fn context(&self) -> AgentContext {
        let project = self.cwd.to_string_lossy().into_owned();
        // Derive a speech-safe briefing from the most recent assistant turn.
        let brief = self.read_context().await.unwrap_or_default();
        let speech_briefing = brief
            .context
            .recent_messages_verbatim
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| speech_safe_summary(&m.text))
            .unwrap_or_else(|| "Claude session attached; no transcript captured yet.".to_owned());
        AgentContext {
            project,
            active_task: self
                .last_envelope
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .map(|e| e.user_intent),
            speech_briefing,
            recent_changes: Vec::new(),
        }
    }

    async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
        if let Ok(mut last) = self.last_envelope.lock() {
            *last = Some(envelope.clone());
        }
        let id = self.task_counter.fetch_add(1, Ordering::SeqCst);
        let task_id = format!("claude-{id}");

        // When execution is disabled, hand off the prepared envelope without
        // running Claude (the first-class `EnvelopePrepared` state).
        if !self.execution.execute_tasks {
            return TaskResult {
                task_id,
                handoff_state: TaskHandoffState::EnvelopePrepared,
                speech_update: "I prepared the task and kept your constraints, but task \
                                execution is turned off."
                    .to_owned(),
                envelope,
            };
        }

        // Run `claude -p` in the project dir (full repo + tool access) on a
        // blocking thread so the long subprocess doesn't stall the async
        // runtime. The summary it returns is spoken back to the user.
        // Scheme 1: dispatch `claude -p` with the SAME model the developer's chat
        // session is using (read from the transcript), not the CLI default.
        let mut exec = self.execution.clone();
        if exec.model.is_none() {
            exec.model = self
                .resolve_transcript()
                .and_then(|p| latest_transcript_model(&p));
        }
        let cwd = self.cwd.clone();
        let env_for_run = envelope.clone();
        let run = tokio::task::spawn_blocking(move || run_claude_task(&exec, &cwd, &env_for_run))
            .await
            .unwrap_or_else(|_| ClaudeRunResult {
                success: false,
                summary: "The task runner crashed before completing; nothing was confirmed."
                    .to_owned(),
            });
        TaskResult {
            task_id,
            // It ran (Accepted) regardless of the child's exit; `summary`
            // carries success or the failure explanation for the model to speak.
            handoff_state: TaskHandoffState::Accepted,
            speech_update: redact_secrets(&run.summary),
            envelope,
        }
    }

    async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
        let speech_update = match mode {
            CancelMode::Pause => "Paused. The task state is preserved.",
            CancelMode::Cancel => {
                "Cancellation requested. I will not roll back files without approval."
            }
            CancelMode::StopAfterCurrentStep => "I will stop after the current step.",
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
impl HostAdapter for ClaudeAdapter {
    fn kind(&self) -> HostKind {
        HostKind::Claude
    }

    async fn detect(&self) -> bool {
        // Claude is "present" if its per-project transcript dir exists, or the
        // top-level ~/.claude/projects exists, or a transcript is configured.
        if self.transcript_path.is_some() {
            return true;
        }
        if let Some(dir) = claude_project_dir(&self.cwd) {
            if dir.is_dir() {
                return true;
            }
            if let Some(projects) = dir.parent() {
                return projects.is_dir();
            }
        }
        false
    }

    fn trigger_source(&self) -> TriggerSource {
        TriggerSource::SlashCommand {
            command: CLAUDE_TRIGGER.to_owned(),
        }
    }

    async fn read_context(&self) -> Result<Brief, HostError> {
        // Fail-open: a thin/empty context yields a thin brief, never an error.
        let mut brief = Brief {
            host_kind: Some(HostKind::Claude.as_str().to_owned()),
            ..Brief::default()
        };
        if let Some(path) = self.resolve_transcript() {
            if let Ok(text) = read_transcript_tail_text(&path) {
                let messages = parse_transcript_messages(&text);
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

    async fn deliver_call_summary(&self, transcript: &str) -> Result<CallbackAck, HostError> {
        // The post-call recap is a TEXT transcript the host READS (then writes a
        // summary into the chat), NOT something Aura speaks — so it must NOT pass
        // through `speech_safe_summary`'s 800-byte spoken cap (that truncated the
        // conversation mid-sentence). Deliver the FULL redacted transcript
        // (capped generously at `CALL_SUMMARY_MAX_CHARS`) to the hooks file. The
        // default `HostAdapter::deliver_call_summary` routes through
        // `deliver_callback` (spoken-capped); this override bypasses that.
        let Some(dir) = &self.hooks_dir else {
            return Ok(CallbackAck {
                delivered: false,
                detail: "no hooks directory configured for call-summary delivery".to_owned(),
            });
        };
        let recap = redact_secrets(transcript);
        let recap = recap.trim();
        if recap.is_empty() {
            return Ok(CallbackAck {
                delivered: false,
                detail: "empty call; nothing to recap".to_owned(),
            });
        }
        let capped: String = recap.chars().take(crate::CALL_SUMMARY_MAX_CHARS).collect();
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| HostError::Callback(e.to_string()))?;
        let payload = json!({
            "event": "aura_voice_call_summary",
            "task_id": "voice-call-summary",
            "compact_summary": format!(
                "Voice call transcript (developer + Aura) — summarize this for the chat:\n{capped}"
            ),
            "tool_name": "claude",
        });
        let body = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned());
        tokio::fs::write(dir.join("aura-last-claude-result.json"), body)
            .await
            .map_err(|e| HostError::Callback(e.to_string()))?;
        Ok(CallbackAck {
            delivered: true,
            detail: "wrote full call transcript to hooks dir".to_owned(),
        })
    }

    async fn deliver_callback(&self, result: &TaskResult) -> Result<CallbackAck, HostError> {
        // Write a speech-safe result file into the hooks dir if configured;
        // the Claude `.aura` Stop hook surfaces it back in the chat.
        let Some(dir) = &self.hooks_dir else {
            return Ok(CallbackAck {
                delivered: false,
                detail: "no hooks directory configured for callback delivery".to_owned(),
            });
        };
        // Async fs (not blocking std::fs) — this runs on the engine's runtime.
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| HostError::Callback(e.to_string()))?;
        // All callback text passes redact_secrets + speech_safe_summary.
        let payload = json!({
            "event": "aura_claude_task_result",
            "task_id": result.task_id,
            "compact_summary": speech_safe_summary(&redact_secrets(&result.speech_update)),
            "tool_name": "claude",
        });
        let body = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned());
        tokio::fs::write(dir.join("aura-last-claude-result.json"), body)
            .await
            .map_err(|e| HostError::Callback(e.to_string()))?;
        Ok(CallbackAck {
            delivered: true,
            detail: "wrote callback result to hooks dir".to_owned(),
        })
    }
}

// --- Task execution: `claude -p` subprocess -----------------------------------

/// Wall-clock ceiling for a single dispatched Claude run. A real coding task
/// can take many minutes; this is generous but bounds a hung subprocess.
const CLAUDE_TASK_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Poll cadence while waiting on the child.
const CLAUDE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The `claude` CLI execution knobs, derived from [`ClaudeConfig`].
#[derive(Debug, Clone)]
struct ClaudeExecutionOptions {
    execute_tasks: bool,
    cli_path: Option<PathBuf>,
    permission_mode: String,
    allowed_tools: Vec<String>,
    max_budget_usd: Option<String>,
    /// Model to pass to `claude -p --model` (Scheme 1: match the host chat
    /// session's model). None → resolved from the transcript at dispatch time;
    /// if still None, the `claude` CLI default is used.
    model: Option<String>,
}

impl From<&ClaudeConfig> for ClaudeExecutionOptions {
    fn from(config: &ClaudeConfig) -> Self {
        Self {
            execute_tasks: config.execute_tasks,
            cli_path: config.cli_path.clone(),
            permission_mode: config.permission_mode.clone(),
            allowed_tools: config.allowed_tools.clone(),
            max_budget_usd: config.max_budget_usd.clone(),
            // A config override (`dispatch_model`) pins the model outright; when
            // unset it stays `None` and is resolved per-dispatch from the live
            // transcript (Scheme 1), else the `claude` CLI default.
            model: config.dispatch_model.clone(),
        }
    }
}

impl ClaudeExecutionOptions {
    fn cli_display(&self) -> &std::ffi::OsStr {
        self.cli_path
            .as_deref()
            .map(Path::as_os_str)
            .unwrap_or_else(|| std::ffi::OsStr::new("claude"))
    }
}

struct ClaudeRunResult {
    /// Whether the child exited successfully. Read by the callback/hook
    /// delivery path (`deliver_callback`); the summary already conveys outcome.
    #[allow(dead_code)]
    success: bool,
    summary: String,
}

/// The voice-relayed task prompt handed to `claude -p`. Drops `safety_mode`
/// from the prompt (it confused the executor); the spoken approval already
/// happened upstream.
fn task_prompt(envelope: &TaskEnvelope) -> String {
    let constraint_line = if envelope.constraints.is_empty() {
        String::new()
    } else {
        format!(
            "\n\nConstraints to honor: {}",
            envelope.constraints.join("; ")
        )
    };
    format!(
        "The developer just asked, via voice (relayed through Aura): {}{}\n\nThis work is \
         already approved end-to-end. You have full authority to read, edit, run bash, and apply \
         changes — that is exactly what you were dispatched to do. When you're done, return a \
         short one-paragraph summary suitable for being spoken back via voice (no code blocks, \
         no file paths, no line numbers, no stack traces). Callback policy: {}.",
        envelope.user_intent, constraint_line, envelope.callback_mode
    )
}

/// System prompt appended to the dispatched Claude run so its final message is
/// a rich, voice-friendly handoff Aura can paraphrase.
const CLAUDE_APPEND_SYSTEM_PROMPT: &str =
    "You are Claude Code being invoked by Aura, the developer's voice agent. \
     Do the approved coding work only and preserve constraints exactly. \
     Your final response is your handoff back to Aura — she'll paraphrase it (NOT read it \
     verbatim) over voice to the developer. Make it RICH and STRUCTURED so she has real \
     substance to deliver: (1) WHAT YOU DID — concrete actions taken; name the modules/areas \
     in plain language, avoid raw file paths or line numbers. (2) FINDINGS — concrete results, \
     decisions, things confirmed or ruled out. (3) NEXT STEP or BLOCKER — one specific thing \
     the developer should decide, do, or be aware of. Format: 4-8 short, voice-friendly \
     sentences. NO code blocks, NO file paths with slashes, NO line numbers, NO stack traces, \
     NO error messages with secrets — Aura cannot speak those. If you investigated: include the \
     actual takeaways inline, not a 'see the docs' pointer. If you couldn't complete the task: \
     say what blocked you and what you'd need to proceed.";

/// Build and run the `claude -p` command for `envelope` in `cwd`.
fn run_claude_task(
    exec: &ClaudeExecutionOptions,
    cwd: &Path,
    envelope: &TaskEnvelope,
) -> ClaudeRunResult {
    let prompt = task_prompt(envelope);
    let mut command = Command::new(exec.cli_display());
    command
        .current_dir(cwd)
        .arg("-p")
        .arg("--output-format")
        .arg("text")
        .arg("--permission-mode")
        .arg(&exec.permission_mode)
        .arg("--append-system-prompt")
        .arg(CLAUDE_APPEND_SYSTEM_PROMPT);
    // Scheme 1: match the host chat session's model when we could resolve it.
    if let Some(model) = &exec.model {
        command.arg("--model").arg(model);
    }
    if !exec.allowed_tools.is_empty() {
        command
            .arg("--allowedTools")
            .arg(exec.allowed_tools.join(","));
    }
    if let Some(max_budget) = &exec.max_budget_usd {
        command.arg("--max-budget-usd").arg(max_budget);
    }
    command.arg(prompt);
    run_command_with_timeout(command, CLAUDE_TASK_TIMEOUT)
}

/// Spawn `command` with piped stdout/stderr under a wall-clock `timeout`,
/// draining both pipes on helper threads so a chatty child can't deadlock.
fn run_command_with_timeout(mut command: Command, timeout: Duration) -> ClaudeRunResult {
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ClaudeRunResult {
                success: false,
                summary: format!("Claude Code could not be started: {err}"),
            };
        }
    };

    let stdout_reader = {
        let handle = child.stdout.take();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut h) = handle {
                let _ = h.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stderr_reader = {
        let handle = child.stderr.take();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut h) = handle {
                let _ = h.read_to_end(&mut buf);
            }
            buf
        })
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                thread::sleep(CLAUDE_POLL_INTERVAL);
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return ClaudeRunResult {
                    success: false,
                    summary: format!("Claude Code run could not be monitored: {err}"),
                };
            }
        }
    };

    let stdout_bytes = stdout_reader.join().unwrap_or_default();
    let stderr_bytes = stderr_reader.join().unwrap_or_default();

    match status {
        Some(status) => {
            let stdout = String::from_utf8_lossy(&stdout_bytes);
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            let combined = if status.success() || stderr.trim().is_empty() {
                stdout.trim().to_owned()
            } else {
                stderr.trim().to_owned()
            };
            ClaudeRunResult {
                success: status.success(),
                summary: if combined.is_empty() {
                    "Claude Code finished without a visible message.".to_owned()
                } else {
                    combined
                },
            }
        }
        None => {
            let secs = timeout.as_secs();
            let span = if secs >= 60 {
                format!("{} minutes", secs / 60)
            } else {
                format!("{secs} seconds")
            };
            ClaudeRunResult {
                success: false,
                summary: format!(
                    "Claude Code timed out after {span} and was stopped. The task may have been \
                     stuck; nothing was confirmed complete."
                ),
            }
        }
    }
}

/// Truncate to at most `max` chars on a char boundary.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(dir: &Path) -> PathBuf {
        // Minimal Claude Code transcript: two user turns + two assistant turns,
        // plus a thinking/tool line that must be skipped.
        let lines = [
            json!({"type":"user","timestamp":"2026-06-29T10:00:00Z","message":{"content":"refactor the auth module"}}),
            json!({"type":"assistant","timestamp":"2026-06-29T10:00:05Z","message":{"content":[{"type":"text","text":"On it — reading the auth module now."},{"type":"tool_use","name":"Read","input":{"file_path":"src/auth.rs"}}]}}),
            json!({"type":"assistant","timestamp":"2026-06-29T10:00:30Z","message":{"content":[{"type":"thinking","thinking":"secret reasoning"}]}}),
            json!({"type":"user","timestamp":"2026-06-29T10:01:00Z","message":{"content":[{"type":"text","text":"also add tests"}]}}),
            json!({"type":"assistant","timestamp":"2026-06-29T10:01:10Z","message":{"content":[{"type":"text","text":"Added tests; all green."}]}}),
        ];
        let path = dir.join("session-abc.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[tokio::test]
    async fn read_context_maps_transcript_into_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path());
        let cfg = ClaudeConfig {
            transcript_path: Some(path),
            ..ClaudeConfig::default()
        };
        let adapter = ClaudeAdapter::with_config(tmp.path(), &cfg);

        let brief = adapter.read_context().await.expect("fail-open Ok");
        assert_eq!(brief.host_kind.as_deref(), Some("claude"));
        let msgs = &brief.context.recent_messages_verbatim;
        // 2 user + 2 assistant text turns; the thinking-only assistant line is
        // skipped (no text block).
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].text, "refactor the auth module");
        assert_eq!(msgs[0].channel, "claude-code");
        assert_eq!(msgs[0].ts_iso.as_deref(), Some("2026-06-29T10:00:00Z"));
        assert!(msgs.iter().any(|m| m.text == "Added tests; all green."));
        // current_focus is the last user message.
        assert_eq!(brief.context.current_focus, "also add tests");
        // Brief is valid (clamped, within caps).
        assert!(brief.validate().ok);
    }

    #[tokio::test]
    async fn read_context_is_fail_open_without_transcript() {
        // No transcript anywhere → thin brief, Ok, call still proceeds.
        let tmp = tempfile::tempdir().unwrap();
        let adapter = ClaudeAdapter::new(tmp.path().join("nonexistent/cwd"));
        let brief = adapter.read_context().await.expect("fail-open Ok");
        assert_eq!(brief.host_kind.as_deref(), Some("claude"));
        assert!(brief.context.recent_messages_verbatim.is_empty());
        assert!(brief.context.current_focus.is_empty());
    }

    #[test]
    fn project_dir_encodes_cwd_like_claude_cli() {
        let dir = claude_project_dir(Path::new("/media/stas/proj")).unwrap();
        let s = dir.to_string_lossy();
        assert!(s.contains(".claude/projects"));
        assert!(s.ends_with("-media-stas-proj"));
    }

    #[test]
    fn trigger_is_the_slash_command() {
        let adapter = ClaudeAdapter::new("/tmp/x");
        assert_eq!(
            adapter.trigger_source(),
            TriggerSource::SlashCommand {
                command: "/aura:aura-live".to_owned()
            }
        );
        assert_eq!(adapter.kind(), HostKind::Claude);
    }

    #[tokio::test]
    async fn start_task_prepares_envelope_without_executing() {
        let adapter = ClaudeAdapter::new("/tmp/x");
        let envelope = TaskEnvelope::new(
            "do the thing",
            vec![],
            "proj",
            CallbackMode::SpeakImmediately,
            "approval-token",
        );
        let result = adapter.start_task(envelope).await;
        assert_eq!(result.handoff_state, TaskHandoffState::EnvelopePrepared);
        assert!(!result.accepted());
        assert!(result.task_id.starts_with("claude-"));
    }

    #[test]
    fn latest_transcript_model_reads_most_recent_assistant_model() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"hi"}}}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"claude-sonnet-5","content":[{{"type":"text","text":"a"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"claude-opus-4-8","content":[{{"type":"text","text":"b"}}]}}}}"#
        )
        .unwrap();
        f.flush().unwrap();
        // Most recent assistant model wins.
        assert_eq!(
            latest_transcript_model(&path).as_deref(),
            Some("claude-opus-4-8")
        );
    }

    #[test]
    fn latest_transcript_model_skips_synthetic_placeholder() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // A real model, then a `<synthetic>` placeholder as the NEWEST assistant
        // turn. The placeholder must be skipped, keeping the last REAL model —
        // otherwise `claude -p --model '<synthetic>'` would fail the dispatch.
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"claude-sonnet-5","content":[{{"type":"text","text":"a"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"<synthetic>","content":[{{"type":"text","text":"b"}}]}}}}"#
        )
        .unwrap();
        f.flush().unwrap();
        assert_eq!(
            latest_transcript_model(&path).as_deref(),
            Some("claude-sonnet-5"),
            "synthetic placeholder must not be picked as the dispatch model"
        );
    }

    #[test]
    fn plausible_model_id_filters_placeholders() {
        assert!(is_plausible_model_id("claude-sonnet-5"));
        assert!(is_plausible_model_id("claude-opus-4-8"));
        assert!(is_plausible_model_id("gpt-4o-realtime-2025-06-01"));
        // Placeholders / junk are rejected.
        assert!(!is_plausible_model_id("<synthetic>"));
        assert!(!is_plausible_model_id(""));
        assert!(!is_plausible_model_id("  "));
        assert!(!is_plausible_model_id("has space"));
    }

    #[test]
    fn latest_transcript_model_none_when_absent() {
        use std::io::Write as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"hi"}}}}"#).unwrap();
        f.flush().unwrap();
        // No model recorded → None; missing file → None (never panics).
        assert_eq!(latest_transcript_model(&path), None);
        assert_eq!(
            latest_transcript_model(&tmp.path().join("nope.jsonl")),
            None
        );
    }

    #[test]
    fn dispatch_model_config_pins_the_execution_model() {
        // Unset → no pin; the model is resolved from the transcript at dispatch
        // time (`start_task` only auto-detects when `model.is_none()`).
        let exec = ClaudeExecutionOptions::from(&ClaudeConfig::default());
        assert_eq!(exec.model, None);
        // Set → pinned outright, so it wins over transcript auto-detection.
        let cfg = ClaudeConfig {
            dispatch_model: Some("claude-opus-4-8".to_owned()),
            ..ClaudeConfig::default()
        };
        let exec = ClaudeExecutionOptions::from(&cfg);
        assert_eq!(exec.model.as_deref(), Some("claude-opus-4-8"));
    }

    #[test]
    fn executing_constructor_threads_the_dispatch_model_pin() {
        // No pin → transcript auto-detection (model stays None at construction).
        let none = ClaudeAdapter::executing("/tmp/aura-dm-test");
        assert_eq!(none.execution.model, None);
        // Pinned → forced regardless of the chat session's model.
        let pinned = ClaudeAdapter::executing_with_dispatch_model(
            "/tmp/aura-dm-test",
            Some("claude-opus-4-8".to_owned()),
        );
        assert_eq!(pinned.execution.model.as_deref(), Some("claude-opus-4-8"));
    }
}
