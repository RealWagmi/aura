//! Scheme 2 coordination layer — the in-call dispatch inbox.
//!
//! When the voice model asks the host to *do* something mid-call, aura does NOT
//! immediately spawn a fresh cold worker. It posts the task to an append-only
//! inbox under `<cwd>/.aura/inbox/` that the **live host chat session** (the
//! orchestrator, driven by its skill watch-loop) is watching. The orchestrator
//! triages each task — answer from its live context, or delegate to a
//! model-matched sub-agent — and writes the result back. aura blocking-waits for
//! that result and speaks it into the call. This decouples the executor from a
//! cold `claude -p`: the work runs in (or is dispatched by) the SAME session the
//! developer was chatting with, so it carries the real conversation context and
//! the right model.
//!
//! ## Files (all append-only JSONL — one JSON object per line)
//! * `tasks.jsonl`   — aura appends `NEW`; the orchestrator reads.
//! * `results.jsonl` — the orchestrator appends `CLAIM` / `DONE` / `STALL`; aura reads.
//! * `orchestrator.alive` — the orchestrator rewrites this while its watch-loop
//!   is running; a fresh mtime is the **heartbeat** aura guards on. No live loop
//!   (stale/absent heartbeat) → aura skips the inbox and dispatches directly, so
//!   nothing is slower when the orchestrator is off.
//!
//! ## Reliability (the heyarp blueprint, proven in Hermes)
//! Append-only + dedup by task id (the latest line for an id wins),
//! guard-before-action (the heartbeat check), and a liveness fallback (an
//! unclaimed/stalled task is dispatched directly by the caller so a dead loop
//! never strands a call). Latency is a short bounded wait tick (sub-second) —
//! NOT a cron poll.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Wire `kind` value: a task aura posts for the orchestrator to pick up.
pub const KIND_NEW: &str = "NEW";
/// Wire `kind` value: the orchestrator has taken ownership of a task.
pub const KIND_CLAIM: &str = "CLAIM";
/// Wire `kind` value: the orchestrator finished a task (terminal, success).
pub const KIND_DONE: &str = "DONE";
/// Wire `kind` value: the orchestrator (or aura) gave up on a task (terminal).
pub const KIND_STALL: &str = "STALL";

const TASKS_FILE: &str = "tasks.jsonl";
const RESULTS_FILE: &str = "results.jsonl";
const HEARTBEAT_FILE: &str = "orchestrator.alive";

/// Seconds since the Unix epoch (0 on a pre-epoch clock — never panics).
pub(crate) fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A task aura posts for the live orchestrator to pick up (a `NEW` record).
/// Text fields are already `redact_secrets`'d by the time they reach here (the
/// [`TaskEnvelope`](aura_core::tools::TaskEnvelope) constructor redacts), so the
/// inbox file never holds raw secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxTask {
    pub id: String,
    pub epoch: u64,
    pub user_intent: String,
    pub constraints: Vec<String>,
    pub project: String,
}

/// A terminal result the orchestrator writes back (`DONE` or `STALL`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxResult {
    pub id: String,
    pub epoch: u64,
    /// [`KIND_DONE`] or [`KIND_STALL`].
    pub kind: String,
    /// The speech-safe update to relay into the call.
    pub speech: String,
}

impl InboxResult {
    /// True when this result is a genuine completion (not a stall).
    pub fn is_done(&self) -> bool {
        self.kind == KIND_DONE
    }
}

/// The coordination inbox rooted at `<root>/.aura/inbox/`.
#[derive(Clone, Debug)]
pub struct Inbox {
    dir: PathBuf,
}

impl Inbox {
    /// Open (creating if needed) the inbox under `<root>/.aura/inbox/`, then
    /// compact away resolved history (best-effort) so the logs don't grow
    /// unboundedly across calls.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let dir = root.join(".aura").join("inbox");
        fs::create_dir_all(&dir)?;
        let inbox = Self { dir };
        inbox.compact();
        Ok(inbox)
    }

    /// The inbox directory (for helpers that need to show it to the operator).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn tasks_path(&self) -> PathBuf {
        self.dir.join(TASKS_FILE)
    }

    fn results_path(&self) -> PathBuf {
        self.dir.join(RESULTS_FILE)
    }

    fn heartbeat_path(&self) -> PathBuf {
        self.dir.join(HEARTBEAT_FILE)
    }

    /// Append one JSON object as a single line. `serde_json` never emits an
    /// embedded newline, so one `write_all` of `line + '\n'` is an atomic append
    /// on POSIX for line-sized payloads.
    fn append_line(path: &Path, value: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(line.as_bytes())
    }

    /// Drop every line whose task id already has a terminal (`DONE`/`STALL`)
    /// result, leaving only in-flight/unresolved tasks. Bounds both logs at
    /// O(in-flight) instead of O(all-tasks-ever), so the per-poll read+parse
    /// stays cheap over the life of a busy repo. Best-effort: any IO error is
    /// swallowed and the logs are left intact (fail-open). Called once on
    /// [`open`], before this process posts anything — a fresh server opens the
    /// inbox per call, so prior calls' resolved lines are provably dead. (A
    /// concurrent LOCAL server sharing the cwd could in principle lose an append
    /// racing the rewrite; that is the same narrow same-cwd edge the task-id pid
    /// discriminator already scopes, and it only degrades that call to a direct
    /// dispatch, never corrupts state.)
    fn compact(&self) {
        let results = fs::read_to_string(self.results_path()).unwrap_or_default();
        if results.is_empty() {
            return; // no results → nothing resolved → nothing to drop
        }
        let mut terminal: HashSet<String> = HashSet::new();
        for line in results.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let kind = value
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(kind, KIND_DONE | KIND_STALL) {
                if let Some(id) = value.get("id").and_then(Value::as_str) {
                    terminal.insert(id.to_owned());
                }
            }
        }
        if terminal.is_empty() {
            return; // in-flight only — keep everything
        }
        // Keep only well-formed lines whose id is NOT terminal.
        let retain = |text: &str| -> String {
            text.lines()
                .filter_map(|line| {
                    let line = line.trim();
                    let value = serde_json::from_str::<Value>(line).ok()?;
                    let id = value.get("id").and_then(Value::as_str)?;
                    (!terminal.contains(id)).then(|| format!("{line}\n"))
                })
                .collect()
        };
        let tasks = fs::read_to_string(self.tasks_path()).unwrap_or_default();
        let _ = Self::rewrite_atomic(&self.tasks_path(), &retain(&tasks));
        let _ = Self::rewrite_atomic(&self.results_path(), &retain(&results));
    }

    /// Replace a file's contents atomically (write a sibling temp, then rename).
    fn rewrite_atomic(path: &Path, content: &str) -> std::io::Result<()> {
        let tmp = path.with_extension("compact.tmp");
        fs::write(&tmp, content)?;
        fs::rename(&tmp, path)
    }

    // --- aura (caller) side --------------------------------------------------

    /// Post a task for the orchestrator to pick up (append `NEW`).
    pub fn post_task(&self, task: &InboxTask) -> std::io::Result<()> {
        Self::append_line(
            &self.tasks_path(),
            &json!({
                "kind": KIND_NEW,
                "id": task.id,
                "epoch": task.epoch,
                "user_intent": task.user_intent,
                "constraints": task.constraints,
                "project": task.project,
            }),
        )
    }

    /// The latest terminal (`DONE`/`STALL`) result for `id`, if any.
    pub fn result_for(&self, id: &str) -> Option<InboxResult> {
        let text = fs::read_to_string(self.results_path()).ok()?;
        latest_result_in(&text, id)
    }

    /// Has the orchestrator taken ownership of `id` yet (a `CLAIM`/`DONE`/`STALL`
    /// line exists)?
    pub fn is_claimed(&self, id: &str) -> bool {
        fs::read_to_string(self.results_path())
            .map(|text| is_claimed_in(&text, id))
            .unwrap_or(false)
    }

    /// Is a live orchestrator watch-loop running (heartbeat younger than
    /// `max_age`)? The guard aura checks BEFORE routing through the inbox: a
    /// stale/absent heartbeat means no loop is consuming, so the caller should
    /// dispatch directly and add zero latency.
    pub fn orchestrator_alive(&self, max_age: Duration) -> bool {
        let Ok(meta) = fs::metadata(self.heartbeat_path()) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        // A future mtime (clock skew) reads as age 0 → alive.
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        age <= max_age
    }

    // --- orchestrator side ---------------------------------------------------

    /// Refresh the liveness heartbeat. The orchestrator's `aura-inbox wait` loop
    /// touches it each tick while blocked waiting for a task; it does NOT refresh
    /// during the work window between claiming a task and reporting it, so a task
    /// dispatched while the orchestrator is busy on a long task may see a stale
    /// heartbeat and take the direct fallback — which is correct, since a
    /// single-threaded orchestrator cannot service a second task concurrently.
    pub fn touch_heartbeat(&self) -> std::io::Result<()> {
        fs::write(self.heartbeat_path(), now_epoch().to_string())
    }

    /// Take ownership of a task (append `CLAIM`). Idempotent on the wire — a
    /// duplicate CLAIM is harmless (dedup is latest-wins).
    pub fn claim(&self, id: &str) -> std::io::Result<()> {
        Self::append_line(
            &self.results_path(),
            &json!({ "kind": KIND_CLAIM, "id": id, "epoch": now_epoch() }),
        )
    }

    /// Report a task finished (append `DONE`) with the speech-safe update.
    pub fn mark_done(&self, id: &str, speech: &str) -> std::io::Result<()> {
        self.append_result(KIND_DONE, id, speech)
    }

    /// Report a task abandoned (append `STALL`) with a speech-safe reason.
    pub fn mark_stall(&self, id: &str, speech: &str) -> std::io::Result<()> {
        self.append_result(KIND_STALL, id, speech)
    }

    fn append_result(&self, kind: &str, id: &str, speech: &str) -> std::io::Result<()> {
        Self::append_line(
            &self.results_path(),
            &json!({ "kind": kind, "id": id, "epoch": now_epoch(), "speech": speech }),
        )
    }

    /// Tasks still awaiting the orchestrator: every `NEW` whose id has no
    /// `CLAIM`/`DONE`/`STALL` yet, de-duplicated (a repeated id keeps the first),
    /// in posting order. This is what the orchestrator's watch-loop reads.
    pub fn pending_tasks(&self) -> Vec<InboxTask> {
        let tasks = fs::read_to_string(self.tasks_path()).unwrap_or_default();
        let results = fs::read_to_string(self.results_path()).unwrap_or_default();
        pending_tasks_in(&tasks, &results)
    }
}

// --- pure protocol helpers (unit-tested) -------------------------------------

/// Parse one `tasks.jsonl` line into an [`InboxTask`] (only `NEW` records).
fn parse_task_line(line: &str) -> Option<InboxTask> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("kind").and_then(Value::as_str) != Some(KIND_NEW) {
        return None;
    }
    let id = value.get("id")?.as_str()?.to_owned();
    if id.is_empty() {
        return None;
    }
    Some(InboxTask {
        id,
        epoch: value.get("epoch").and_then(Value::as_u64).unwrap_or(0),
        user_intent: value
            .get("user_intent")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        constraints: value
            .get("constraints")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        project: value
            .get("project")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

/// The latest `DONE`/`STALL` result for `id` in a `results.jsonl` body
/// (later lines win — the append-only dedup rule).
fn latest_result_in(text: &str, id: &str) -> Option<InboxResult> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind != KIND_DONE && kind != KIND_STALL {
            continue;
        }
        if value.get("id").and_then(Value::as_str) != Some(id) {
            continue;
        }
        found = Some(InboxResult {
            id: id.to_owned(),
            epoch: value.get("epoch").and_then(Value::as_u64).unwrap_or(0),
            kind: kind.to_owned(),
            speech: value
                .get("speech")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    found
}

/// Does any `CLAIM`/`DONE`/`STALL` line reference `id`?
fn is_claimed_in(text: &str, id: &str) -> bool {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let owns = matches!(kind, KIND_CLAIM | KIND_DONE | KIND_STALL);
        if owns && value.get("id").and_then(Value::as_str) == Some(id) {
            return true;
        }
    }
    false
}

/// Pure core of [`Inbox::pending_tasks`]: `NEW` tasks whose id has no
/// `CLAIM`/`DONE`/`STALL` yet, first occurrence kept, posting order preserved.
fn pending_tasks_in(tasks_text: &str, results_text: &str) -> Vec<InboxTask> {
    let mut handled: HashSet<String> = HashSet::new();
    for line in results_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            handled.insert(id.to_owned());
        }
    }
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in tasks_text.lines() {
        let Some(task) = parse_task_line(line) else {
            continue;
        };
        if handled.contains(&task.id) || !seen.insert(task.id.clone()) {
            continue;
        }
        out.push(task);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_task(id: &str) -> InboxTask {
        InboxTask {
            id: id.to_owned(),
            epoch: 100,
            user_intent: "update the config".to_owned(),
            constraints: vec!["keep tests green".to_owned()],
            project: "aura".to_owned(),
        }
    }

    #[test]
    fn post_then_pending_roundtrips() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.post_task(&sample_task("vt-1")).unwrap();
        inbox.post_task(&sample_task("vt-2")).unwrap();
        let pending = inbox.pending_tasks();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, "vt-1");
        assert_eq!(pending[0].user_intent, "update the config");
        assert_eq!(pending[0].constraints, vec!["keep tests green".to_owned()]);
        assert_eq!(pending[1].id, "vt-2");
    }

    #[test]
    fn claimed_task_drops_out_of_pending() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.post_task(&sample_task("vt-1")).unwrap();
        inbox.post_task(&sample_task("vt-2")).unwrap();
        inbox.claim("vt-1").unwrap();
        let pending = inbox.pending_tasks();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "vt-2");
        assert!(inbox.is_claimed("vt-1"));
        assert!(!inbox.is_claimed("vt-2"));
    }

    #[test]
    fn done_result_is_read_back_latest_wins() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.claim("vt-1").unwrap();
        inbox.mark_done("vt-1", "first summary").unwrap();
        inbox.mark_done("vt-1", "final summary").unwrap();
        let res = inbox.result_for("vt-1").expect("a terminal result");
        assert_eq!(res.kind, KIND_DONE);
        assert!(res.is_done());
        assert_eq!(res.speech, "final summary");
        // No result for an unknown id.
        assert_eq!(inbox.result_for("vt-unknown"), None);
    }

    #[test]
    fn stall_is_terminal_but_not_done() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.mark_stall("vt-9", "worker died").unwrap();
        let res = inbox.result_for("vt-9").unwrap();
        assert_eq!(res.kind, KIND_STALL);
        assert!(!res.is_done());
        assert_eq!(res.speech, "worker died");
    }

    #[test]
    fn heartbeat_freshness_gates_liveness() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        // No heartbeat file yet → not alive.
        assert!(!inbox.orchestrator_alive(Duration::from_secs(30)));
        // A fresh heartbeat is alive within a generous window.
        inbox.touch_heartbeat().unwrap();
        assert!(inbox.orchestrator_alive(Duration::from_secs(30)));
    }

    #[test]
    fn junk_and_partial_lines_are_ignored() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        // A valid task, then garbage, then a non-NEW record.
        inbox.post_task(&sample_task("vt-1")).unwrap();
        fs::write(
            inbox.dir().join(TASKS_FILE),
            "{\"kind\":\"NEW\",\"id\":\"vt-1\",\"epoch\":100,\"user_intent\":\"update the config\",\"constraints\":[\"keep tests green\"],\"project\":\"aura\"}\n\
             not json at all\n\
             {\"kind\":\"OTHER\",\"id\":\"vt-x\"}\n\
             {\"kind\":\"NEW\",\"id\":\"\"}\n",
        )
        .unwrap();
        let pending = inbox.pending_tasks();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "vt-1");
    }

    #[test]
    fn compact_drops_resolved_tasks_on_open() {
        let tmp = tempdir().unwrap();
        {
            let inbox = Inbox::open(tmp.path()).unwrap();
            inbox.post_task(&sample_task("vt-done")).unwrap();
            inbox.post_task(&sample_task("vt-live")).unwrap();
            inbox.claim("vt-done").unwrap();
            inbox.mark_done("vt-done", "finished").unwrap();
            inbox.claim("vt-live").unwrap(); // in-flight: claimed, not terminal
        }
        // Re-open → compaction runs: the resolved task is dropped, the in-flight
        // one is retained.
        let reopened = Inbox::open(tmp.path()).unwrap();
        assert_eq!(
            reopened.result_for("vt-done"),
            None,
            "resolved task's records are compacted away"
        );
        assert!(
            reopened.is_claimed("vt-live"),
            "an in-flight task's CLAIM survives compaction"
        );
        let tasks = fs::read_to_string(reopened.dir().join(TASKS_FILE)).unwrap();
        assert!(
            !tasks.contains("vt-done"),
            "resolved NEW dropped from tasks"
        );
        assert!(tasks.contains("vt-live"), "in-flight NEW retained");
    }

    #[test]
    fn duplicate_new_id_kept_once() {
        assert_eq!(
            pending_tasks_in(
                "{\"kind\":\"NEW\",\"id\":\"vt-1\",\"user_intent\":\"a\"}\n\
                 {\"kind\":\"NEW\",\"id\":\"vt-1\",\"user_intent\":\"a-again\"}\n",
                "",
            )
            .len(),
            1
        );
    }
}
