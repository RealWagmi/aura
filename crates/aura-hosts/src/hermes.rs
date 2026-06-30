//! Hermes (Codexini) host adapter.
//!
//! Trigger: the LLM skill `codexini-call` (the model catches NL; triggers are
//! deliberately non-unified). Context: built on `rusqlite` — open
//! `~/.hermes/profiles/<active>/
//! state.db` READ-ONLY and pick the live conversation via **burst-clone
//! ranking** (`dialog_count >= 12 AND span_sec >= 5 ORDER BY span_sec DESC`):
//! a naive newest-by-timestamp picks a per-minute clone snapshot and yields an
//! amnesiac brief. Dispatch: a host-side worker subprocess (the
//! `PROGRESS:`/`SUMMARY:` line protocol). Callback: composed back to the
//! gateway/Telegram (hosted; a stub here).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags};

use aura_core::brief::{Brief, RecentMessage, MAX_CURRENT_FOCUS, MAX_SOUL_SUMMARY};
use aura_core::host::HostKind;
use aura_core::tools::{
    AgentContext, AgentRuntime, AgentStatus, AttentionAck, AttentionRequest, CancelAck, CancelMode,
    TaskEnvelope, TaskHandoffState, TaskResult,
};
use aura_core::{redact_secrets, speech_safe_summary, CallbackMode};

use crate::{CallbackAck, HostAdapter, HostError, TriggerSource};

/// The LLM skill that triggers a Hermes call.
pub const HERMES_TRIGGER_SKILL: &str = "codexini-call";
/// Channel label stamped onto messages read from the Hermes session store.
const HERMES_CHANNEL: &str = "session_store";
/// Burst-clone ranking thresholds (the load-bearing gotcha).
const MIN_DIALOG: i64 = 12;
const MIN_SPAN_SEC: f64 = 5.0;
/// Newest transcript messages carried into the brief.
const RECENT_MESSAGE_TARGET: usize = 200;
/// SQLite busy_timeout while a live writer runs in parallel.
const BUSY_TIMEOUT: Duration = Duration::from_millis(2000);
/// Wall-clock ceiling for a dispatched Hermes worker.
const HERMES_WORKER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

// --- Home / profile resolution -----------------------------------------------

/// Resolve the Hermes home dir: `$CODEXINI_HERMES_ROOT`, else `$HOME/.hermes`.
/// (A multi-ancestor `.hermes` walk for nested installs is a follow-up;
/// the env override + `$HOME/.hermes` covers the standard install.)
fn resolve_hermes_home(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(home) = explicit {
        return Some(home.to_path_buf());
    }
    if let Ok(root) = std::env::var("CODEXINI_HERMES_ROOT") {
        let p = PathBuf::from(root.trim());
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let hermes = PathBuf::from(home).join(".hermes");
    hermes.is_dir().then_some(hermes)
}

/// The active profile name (explicit, else the trimmed `<home>/active_profile`).
fn resolve_profile(home: &Path, explicit: Option<&str>) -> Option<String> {
    if let Some(p) = explicit {
        return Some(p.to_owned());
    }
    let raw = std::fs::read_to_string(home.join("active_profile")).ok()?;
    let trimmed = raw.trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

struct ProfilePaths {
    base: PathBuf,
    db: PathBuf,
}

fn profile_paths(home: &Path, profile: &str) -> ProfilePaths {
    let base = home.join("profiles").join(profile);
    let db = base.join("state.db");
    ProfilePaths { base, db }
}

// --- Read path: burst-clone ranking + transcript -----------------------------

struct SelectedSession {
    id: String,
}

/// Open `state.db` READ-ONLY with a busy_timeout (a live writer runs alongside).
fn open_state_db_read_only(db: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let _ = conn.pragma_update(None, "query_only", "ON");
    Ok(conn)
}

/// Burst-clone ranking: the session with the longest real conversation span,
/// NOT the most recent per-minute clone snapshot.
fn select_session_via_db(conn: &Connection) -> Option<SelectedSession> {
    let sql = "WITH per_session AS (
            SELECT m.session_id AS id,
                   SUM(CASE WHEN m.role IN ('user','assistant') THEN 1 ELSE 0 END) AS dialog_count,
                   MAX(m.timestamp) - MIN(m.timestamp) AS span_sec,
                   MAX(m.timestamp) AS last_ts
            FROM messages m
            WHERE m.role IN ('user','assistant')
            GROUP BY m.session_id
        )
        SELECT id FROM per_session
        WHERE dialog_count >= ?1 AND span_sec >= ?2
        ORDER BY span_sec DESC, last_ts DESC
        LIMIT 1";
    conn.query_row(sql, rusqlite::params![MIN_DIALOG, MIN_SPAN_SEC], |row| {
        row.get::<_, String>(0)
    })
    .ok()
    .map(|id| SelectedSession { id })
}

/// Read a session's ordered user/assistant transcript from the DB. Uses a bound
/// param (no JS-style manual quote-escaping → injection-safe).
fn read_transcript_via_db(conn: &Connection, session_id: &str) -> Vec<(String, String)> {
    let sql = "SELECT role, content FROM messages
               WHERE session_id = ?1 AND role IN ('user','assistant')
               ORDER BY timestamp ASC, id ASC";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    });
    match rows {
        Ok(rows) => rows
            .filter_map(Result::ok)
            .filter(|(_, content)| !content.trim().is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Read SOUL.md + MEMORY.md (first base that yields content wins), redacted and
/// capped to `MAX_SOUL_SUMMARY`.
fn gather_soul_summary(profile_base: &Path) -> String {
    let mut bases = vec![profile_base.to_path_buf(), profile_base.join("home")];
    if let Some(home) = std::env::var_os("HOME") {
        bases.push(PathBuf::from(home));
    }
    let mut out = String::new();
    for base in bases {
        for name in ["SOUL.md", "MEMORY.md"] {
            if let Ok(text) = std::fs::read_to_string(base.join(name)) {
                let text = text.trim();
                if !text.is_empty() {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    out.push_str(text);
                }
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    let redacted = redact_secrets(&out);
    truncate_chars(&redacted, MAX_SOUL_SUMMARY)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        s.chars().take(max).collect()
    }
}

// --- Adapter -----------------------------------------------------------------

/// The Hermes host adapter.
pub struct HermesAdapter {
    cwd: PathBuf,
    home: Option<PathBuf>,
    profile: Option<String>,
    /// Optional host-side worker command (`cmd` + args). When unset, `start_task`
    /// hands off the prepared envelope without executing.
    worker_command: Option<Vec<String>>,
    last_envelope: Mutex<Option<TaskEnvelope>>,
    task_counter: AtomicU64,
}

impl HermesAdapter {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            home: None,
            profile: None,
            worker_command: None,
            last_envelope: Mutex::new(None),
            task_counter: AtomicU64::new(1),
        }
    }

    /// Override the Hermes home, active profile, and/or worker command.
    pub fn configured(
        cwd: impl Into<PathBuf>,
        home: Option<PathBuf>,
        profile: Option<String>,
        worker_command: Option<Vec<String>>,
    ) -> Self {
        let mut a = Self::new(cwd);
        a.home = home;
        a.profile = profile;
        a.worker_command = worker_command;
        a
    }

    fn resolved_home(&self) -> Option<PathBuf> {
        resolve_hermes_home(self.home.as_deref())
    }
}

#[async_trait]
impl AgentRuntime for HermesAdapter {
    async fn status(&self) -> AgentStatus {
        let active = self
            .last_envelope
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|e| e.user_intent)
            .filter(|i| !i.is_empty());
        AgentStatus {
            state: if active.is_some() {
                "agent_working"
            } else {
                "idle"
            }
            .to_owned(),
            active_task: active,
            summary: "Hermes adapter ready.".to_owned(),
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
            speech_briefing: "Hermes session attached.".to_owned(),
            recent_changes: Vec::new(),
        }
    }

    async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
        if let Ok(mut last) = self.last_envelope.lock() {
            *last = Some(envelope.clone());
        }
        let id = self.task_counter.fetch_add(1, Ordering::SeqCst);
        let task_id = format!("hermes-{id}");

        let Some(command) = self.worker_command.clone().filter(|c| !c.is_empty()) else {
            return TaskResult {
                task_id,
                handoff_state: TaskHandoffState::EnvelopePrepared,
                speech_update: "I prepared the task, but no Hermes worker command is configured \
                                to run it."
                    .to_owned(),
                envelope,
            };
        };

        let cwd = self.cwd.clone();
        let task_id_for_run = task_id.clone();
        let intent = envelope.user_intent.clone();
        let run = tokio::task::spawn_blocking(move || {
            run_hermes_worker(&command, &cwd, &task_id_for_run, &intent)
        })
        .await
        .unwrap_or_else(|_| "The Hermes worker crashed before completing.".to_owned());

        TaskResult {
            task_id,
            handoff_state: TaskHandoffState::Accepted,
            speech_update: redact_secrets(&speech_safe_summary(&run)),
            envelope,
        }
    }

    async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
        CancelAck {
            task_id: task_id.to_owned(),
            mode,
            speech_update: "Hermes task cancellation is delivered through the worker registry."
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

#[async_trait]
impl HostAdapter for HermesAdapter {
    fn kind(&self) -> HostKind {
        HostKind::Hermes
    }

    async fn detect(&self) -> bool {
        let Some(home) = self.resolved_home() else {
            return false;
        };
        // Present when there's a usable active profile, or a profiles/ dir.
        if let Some(profile) = resolve_profile(&home, self.profile.as_deref()) {
            if home.join("profiles").join(&profile).is_dir() {
                return true;
            }
        }
        home.join("profiles").is_dir()
    }

    fn trigger_source(&self) -> TriggerSource {
        TriggerSource::LlmSkill {
            skill: HERMES_TRIGGER_SKILL.to_owned(),
        }
    }

    async fn read_context(&self) -> Result<Brief, HostError> {
        // Fail-open throughout: a thin/missing store yields a thin brief, Ok.
        let mut brief = Brief {
            host_kind: Some(HostKind::Hermes.as_str().to_owned()),
            ..Brief::default()
        };

        let Some(home) = self.resolved_home() else {
            brief.clamp();
            return Ok(brief);
        };
        let Some(profile) = resolve_profile(&home, self.profile.as_deref()) else {
            brief.clamp();
            return Ok(brief);
        };
        let paths = profile_paths(&home, &profile);

        brief.user.soul_summary = gather_soul_summary(&paths.base);

        if let Ok(conn) = open_state_db_read_only(&paths.db) {
            if let Some(selected) = select_session_via_db(&conn) {
                let dialog = read_transcript_via_db(&conn, &selected.id);
                let mut messages: Vec<RecentMessage> = dialog
                    .into_iter()
                    .map(|(role, content)| RecentMessage {
                        role: if role == "assistant" {
                            "hermes"
                        } else {
                            "user"
                        }
                        .to_owned(),
                        text: redact_secrets(&content),
                        channel: HERMES_CHANNEL.to_owned(),
                        ts_iso: None,
                    })
                    .collect();
                let drop = messages.len().saturating_sub(RECENT_MESSAGE_TARGET);
                if drop > 0 {
                    messages.drain(0..drop);
                }
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

    async fn deliver_callback(&self, _result: &TaskResult) -> Result<CallbackAck, HostError> {
        // The Hermes callback is composed back to the gateway/Telegram over the
        // runtime-inbox WS (a hosted concern). Local delivery is not wired here.
        Ok(CallbackAck {
            delivered: false,
            detail: "hermes callback delivery is routed through the hosted gateway".to_owned(),
        })
    }
}

/// Spawn the host-side worker, capture its output, and extract the
/// `SUMMARY:` line (else the last non-empty line) as the spoken result.
fn run_hermes_worker(command: &[String], cwd: &Path, task_id: &str, intent: &str) -> String {
    let (bin, args) = command.split_first().expect("non-empty worker command");
    let output = Command::new(bin)
        .args(args)
        .arg(intent)
        .current_dir(cwd)
        .env("CODEXINI_TASK_ID", task_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            // Bound the run; full streaming of PROGRESS:/registry updates is
            // a follow-up — for now capture output and extract the SUMMARY.
            let start = std::time::Instant::now();
            loop {
                if let Some(status) = child.try_wait().ok().flatten() {
                    let out = child.wait_with_output()?;
                    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
                    if !status.success() {
                        combined.push_str(&String::from_utf8_lossy(&out.stderr));
                    }
                    return Ok(combined);
                }
                if start.elapsed() >= HERMES_WORKER_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(String::from("The Hermes worker timed out and was stopped."));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    match output {
        Ok(text) => extract_summary(&text),
        Err(err) => format!("The Hermes worker could not be started: {err}"),
    }
}

/// `SUMMARY: ...` line if present, else the last non-empty line.
fn extract_summary(text: &str) -> String {
    for line in text.lines().rev() {
        if let Some(rest) = line.trim().strip_prefix("SUMMARY:") {
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_owned();
            }
        }
    }
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("The Hermes worker finished with no output.")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fixture state.db with a "clone" snapshot session (recent but
    /// tiny span) and a real long-span conversation; ranking must pick the real one.
    fn write_state_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT, role TEXT, content TEXT, timestamp REAL
            );",
        )
        .unwrap();
        // Real conversation: 14 turns spanning 600s, older.
        for i in 0..14 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1,?2,?3,?4)",
                rusqlite::params![
                    "real",
                    role,
                    format!("real message {i}"),
                    1000.0 + (i as f64) * 46.0
                ],
            )
            .unwrap();
        }
        // Clone snapshot: 13 turns but all within 1 second, NEWER timestamps.
        for i in 0..13 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp) VALUES (?1,?2,?3,?4)",
                rusqlite::params![
                    "clone",
                    role,
                    format!("clone {i}"),
                    9000.0 + (i as f64) * 0.05
                ],
            )
            .unwrap();
        }
    }

    #[test]
    fn burst_clone_ranking_picks_real_conversation_not_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("state.db");
        write_state_db(&db);
        let conn = open_state_db_read_only(&db).unwrap();
        let selected = select_session_via_db(&conn).expect("a session is selected");
        // The clone has a newer last_ts but span_sec < 5 → excluded; the real
        // conversation (span 600s) wins.
        assert_eq!(selected.id, "real");
        let dialog = read_transcript_via_db(&conn, "real");
        assert_eq!(dialog.len(), 14);
        assert_eq!(dialog[0].0, "user");
    }

    #[tokio::test]
    async fn read_context_maps_hermes_store_into_brief() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join(".hermes");
        let profile_base = home.join("profiles").join("default");
        std::fs::create_dir_all(&profile_base).unwrap();
        std::fs::write(home.join("active_profile"), "default\n").unwrap();
        write_state_db(&profile_base.join("state.db"));
        std::fs::write(profile_base.join("SOUL.md"), "I value clarity.").unwrap();

        let adapter = HermesAdapter::configured(tmp.path(), Some(home), None, None);
        let brief = adapter.read_context().await.unwrap();
        assert_eq!(brief.host_kind.as_deref(), Some("hermes"));
        assert_eq!(brief.context.recent_messages_verbatim.len(), 14);
        // assistant role is mapped to "hermes".
        assert!(brief
            .context
            .recent_messages_verbatim
            .iter()
            .any(|m| m.role == "hermes"));
        assert!(brief.user.soul_summary.contains("I value clarity."));
    }

    #[tokio::test]
    async fn read_context_fail_open_without_store() {
        let tmp = tempfile::tempdir().unwrap();
        let adapter =
            HermesAdapter::configured(tmp.path(), Some(tmp.path().join("nonexistent")), None, None);
        let brief = adapter.read_context().await.unwrap();
        assert_eq!(brief.host_kind.as_deref(), Some("hermes"));
        assert!(brief.context.recent_messages_verbatim.is_empty());
    }

    #[test]
    fn extract_summary_prefers_summary_line() {
        assert_eq!(
            extract_summary("PROGRESS: 50% halfway\nSUMMARY: shipped the fix\n"),
            "shipped the fix"
        );
        assert_eq!(extract_summary("just one line\n"), "just one line");
    }

    #[test]
    fn trigger_is_llm_skill() {
        let adapter = HermesAdapter::new("/tmp/x");
        assert_eq!(
            adapter.trigger_source(),
            TriggerSource::LlmSkill {
                skill: "codexini-call".to_owned()
            }
        );
        assert_eq!(adapter.kind(), HostKind::Hermes);
    }
}
