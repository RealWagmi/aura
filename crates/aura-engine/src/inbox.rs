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
//! ## Files
//! * `tasks.jsonl`   — append-only; aura appends `NEW`, the orchestrator reads.
//! * `results.jsonl` — append-only; the orchestrator appends `CLAIM`/`DONE`/`STALL`, aura reads.
//! * `claims/<id>`   — a per-task **claim marker** created with `O_EXCL`
//!   (`create_new`). This is the atomic ownership arbiter: exactly one of the
//!   orchestrator (taking the task) and aura (giving up on it) can create it, so
//!   a `CLAIM` and a give-up `STALL` can never both act on the same id. A
//!   just-appended `CLAIM`/`STALL` line is only observability; the marker is the
//!   truth.
//! * `orchestrator.alive` — the orchestrator rewrites this while its watch-loop
//!   is running; a fresh mtime is the **heartbeat** aura guards on. No live loop
//!   (stale/absent heartbeat) → aura recovers a posted task itself and dispatches
//!   directly, so nothing is slower when the orchestrator is off.
//!
//! ## Reliability
//! Append-only logs + an O_EXCL claim marker for mutual exclusion,
//! guard-before-action (the heartbeat check), and a liveness fallback (an
//! unclaimed/stalled task is recovered and dispatched directly by the caller so a
//! dead loop never strands a call). Growth is bounded by resetting the inbox once
//! at call start ([`open_for_call`](Inbox::open_for_call)) — never by a
//! read-modify-rewrite that could race the live server's concurrent appends.
//! Latency is a short bounded wait tick (sub-second) — NOT a cron poll.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use aura_core::append_private_jsonl_line;

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
const CLAIMS_DIR: &str = "claims";

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
    /// Open (creating if needed) the inbox under `<root>/.aura/inbox/` WITHOUT
    /// mutating any existing content. This is the safe form for the `aura-inbox`
    /// CLI, which the live orchestrator invokes many times per call concurrently
    /// with the server's appends: it must never rewrite the logs. Growth-bounding
    /// is the server's job, done once via [`open_for_call`](Self::open_for_call).
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let dir = root.join(".aura").join("inbox");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Open the inbox for a fresh call, clearing PROVABLY-DEAD leftovers.
    ///
    /// The files are deleted only when no id referenced by `tasks.jsonl` /
    /// `results.jsonl` / `claims/` embeds a pid of a LIVE process — i.e. every
    /// call that ever wrote here has exited, so the state is provably a prior
    /// (ended) call's leftovers. That bounds growth AND prevents a dead call's
    /// un-tombstoned `NEW` from being resurrected, WITHOUT the hazard a blind
    /// reset had: two servers can legitimately share a cwd (the LOCAL loopback
    /// port-hop), and wiping a CONCURRENT live call's claim markers would void
    /// its O_EXCL arbiter and double-execute its in-flight task. If any live
    /// pid is referenced, everything is left untouched (a live sibling exists;
    /// per-call growth is a few hundred bytes — negligible). Correctness for
    /// leftovers that survive (until every writer is dead) is handled at read
    /// time: [`pending_tasks`](Self::pending_tasks) skips dead-pid tasks and
    /// [`has_open_claim_other_than`](Self::has_open_claim_other_than) skips
    /// dead-pid markers. The heartbeat file is always left intact: it is owned
    /// by the host's persistent watch-loop, not by any one call.
    ///
    /// On non-Unix targets pid liveness cannot be probed cheaply, so this
    /// falls back to the unconditional reset (concurrent same-cwd servers are
    /// not a supported topology there).
    pub fn open_for_call(root: &Path) -> std::io::Result<Self> {
        let dir = root.join(".aura").join("inbox");
        fs::create_dir_all(&dir)?;
        let inbox = Self { dir };
        #[cfg(unix)]
        let can_reset = !inbox.references_live_pid();
        #[cfg(not(unix))]
        let can_reset = true;
        if can_reset {
            let _ = fs::remove_file(inbox.tasks_path());
            let _ = fs::remove_file(inbox.results_path());
            let _ = fs::remove_dir_all(inbox.claims_dir());
        }
        fs::create_dir_all(inbox.claims_dir())?;
        Ok(inbox)
    }

    /// Does any id in the inbox (tasks, results, claim markers) embed the pid
    /// of a live process? Unparseable ids contribute nothing (they never block
    /// cleanup — our own ids always parse; junk should not pin the logs).
    #[cfg(unix)]
    fn references_live_pid(&self) -> bool {
        let mut ids: Vec<String> = Vec::new();
        for path in [self.tasks_path(), self.results_path()] {
            let text = fs::read_to_string(path).unwrap_or_default();
            for line in text.lines() {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        ids.push(id.to_owned());
                    }
                }
            }
        }
        if let Ok(entries) = fs::read_dir(self.claims_dir()) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_owned());
                }
            }
        }
        ids.iter().filter_map(|id| task_pid(id)).any(pid_is_alive)
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

    fn claims_dir(&self) -> PathBuf {
        self.dir.join(CLAIMS_DIR)
    }

    /// The claim-marker path for `id`, or `None` if `id` is unsafe as a file name
    /// (empty, or containing a path separator / `..` / `:`). A `None` id can
    /// never be owned — [`try_own`](Self::try_own) treats it as un-ownable — so
    /// a hostile task id from a tampered `tasks.jsonl` cannot escape the
    /// `claims/` dir. `:` is rejected because on Windows a drive-relative
    /// component like `C:evil` REPLACES the joined base path entirely.
    fn marker_path(&self, id: &str) -> Option<PathBuf> {
        if id.is_empty()
            || id.contains('/')
            || id.contains('\\')
            || id.contains(':')
            || id.contains("..")
        {
            return None;
        }
        Some(self.claims_dir().join(id))
    }

    /// Append one JSON object as a single private (`0o600`, `O_NOFOLLOW`) JSONL
    /// line — the same private-fs guarantee the rest of `.aura` state uses, so
    /// redacted intents/speech never land in a world-readable or symlink-followed
    /// file. `serde_json` never emits an embedded newline, so one line write is an
    /// atomic append on POSIX for line-sized payloads.
    fn append_line(path: &Path, value: &Value) -> std::io::Result<()> {
        append_private_jsonl_line(path, value, "aura inbox").map_err(std::io::Error::other)
    }

    /// Atomically create the claim marker for `id`. Returns `Ok(true)` iff THIS
    /// call created it (we now own the task), `Ok(false)` if it already existed
    /// (someone else owns it). `O_EXCL` (`create_new`) makes the create a
    /// cross-process compare-and-set: the kernel guarantees exactly one creator.
    fn create_marker(path: &Path) -> std::io::Result<bool> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Attempt to take exclusive ownership of `id` (create its `O_EXCL` marker).
    /// `Ok(true)` = we own it now; `Ok(false)` = already owned (or an unsafe id).
    /// This is the atomic arbiter both the orchestrator (before working) and aura
    /// (before giving up) call, so a `CLAIM` and a give-up `STALL` can never both
    /// take effect for one id.
    pub fn try_own(&self, id: &str) -> std::io::Result<bool> {
        let Some(path) = self.marker_path(id) else {
            return Ok(false);
        };
        fs::create_dir_all(self.claims_dir())?;
        Self::create_marker(&path)
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

    /// Has `id` been claimed yet? Checks the atomic marker (a cheap stat), so it
    /// is race-free with respect to the `CLAIM` line's append.
    pub fn is_claimed(&self, id: &str) -> bool {
        self.marker_path(id)
            .map(|p| fs::symlink_metadata(p).is_ok())
            .unwrap_or(false)
    }

    /// Does the orchestrator currently hold an OPEN claim on some task OTHER than
    /// `id` — i.e. a claim marker whose id has no terminal result yet? A live
    /// single-threaded orchestrator that is mid-task shows up here even when its
    /// heartbeat has gone stale (it only refreshes the heartbeat while blocked in
    /// `wait`). aura uses this to avoid spawning a concurrent direct writer while
    /// the orchestrator is busy editing the repo for another task.
    ///
    /// Markers whose id embeds a provably-DEAD server pid are skipped: they are
    /// a prior (ended) call's leftovers awaiting cleanup, not live work — they
    /// must not make a fresh call read as "orchestrator busy" forever.
    pub fn has_open_claim_other_than(&self, id: &str) -> bool {
        let Ok(entries) = fs::read_dir(self.claims_dir()) else {
            return false;
        };
        let results = fs::read_to_string(self.results_path()).unwrap_or_default();
        let terminal = terminal_ids(&results);
        for entry in entries.flatten() {
            if let Some(marker) = entry.file_name().to_str() {
                if marker == id || terminal.contains(marker) {
                    continue;
                }
                if matches!(task_pid(marker), Some(pid) if !pid_is_alive(pid)) {
                    continue; // dead call's leftover, not live work
                }
                return true;
            }
        }
        false
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
    /// heartbeat — which is why aura also consults
    /// [`has_open_claim_other_than`](Self::has_open_claim_other_than) before
    /// treating a stale heartbeat as "dead".
    pub fn touch_heartbeat(&self) -> std::io::Result<()> {
        Self::write_heartbeat(&self.heartbeat_path(), now_epoch())
    }

    /// Overwrite the single-value heartbeat file privately (`0o600`). The
    /// heartbeat is a mutable value, not an append log, so it is written to a
    /// fresh private temp then renamed — it never grows and never loosens to
    /// world-readable.
    fn write_heartbeat(path: &Path, epoch: u64) -> std::io::Result<()> {
        let tmp = path.with_extension("alive.tmp");
        let _ = fs::remove_file(&tmp);
        append_private_jsonl_line(&tmp, &epoch, "aura heartbeat").map_err(std::io::Error::other)?;
        fs::rename(&tmp, path)
    }

    /// Take ownership of a task: create the atomic marker, then append a
    /// `CLAIM` line for observability. Returns `Ok(true)` iff THIS call won the
    /// O_EXCL arbiter — the caller must NOT execute the task on `Ok(false)`
    /// (someone else owns it: aura may already be running it directly, so
    /// executing anyway would be the double-execution the marker exists to
    /// prevent). If the marker was won but the observability append fails, the
    /// marker is removed again (best-effort) before the error returns, so the
    /// task drops back to pending instead of being claimed-but-never-executed.
    pub fn claim(&self, id: &str) -> std::io::Result<bool> {
        if !self.try_own(id)? {
            return Ok(false);
        }
        let appended = Self::append_line(
            &self.results_path(),
            &json!({ "kind": KIND_CLAIM, "id": id, "epoch": now_epoch() }),
        );
        if let Err(err) = appended {
            // Undo the won marker so the task is not black-holed: without a
            // TASK line printed the orchestrator will never work on it, and
            // with the marker left in place nobody else could either.
            if let Some(path) = self.marker_path(id) {
                let _ = fs::remove_file(path);
            }
            return Err(err);
        }
        Ok(true)
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
    /// `CLAIM`/`DONE`/`STALL` line AND no claim marker yet, de-duplicated (a
    /// repeated id keeps the first), in posting order. The marker filter closes
    /// the brief window between a marker being created and its `CLAIM` line being
    /// appended, so a task can never be handed to two `wait` callers.
    ///
    /// Tasks whose id embeds a provably-DEAD server pid are skipped: their call
    /// is gone, nobody is waiting for the result, and executing them would be
    /// the resurrection bug the per-call cleanup exists to prevent.
    pub fn pending_tasks(&self) -> Vec<InboxTask> {
        let tasks = fs::read_to_string(self.tasks_path()).unwrap_or_default();
        let results = fs::read_to_string(self.results_path()).unwrap_or_default();
        pending_tasks_in(&tasks, &results)
            .into_iter()
            .filter(|task| !self.is_claimed(&task.id))
            .filter(|task| !matches!(task_pid(&task.id), Some(pid) if !pid_is_alive(pid)))
            .collect()
    }
}

// --- pid liveness (scopes leftovers to their originating call) ---------------

/// Extract the server pid embedded in a task id (`vt-<epoch:x>-<pid:x>-<n>`),
/// or `None` for any other shape. Foreign/malformed ids yield `None` — callers
/// treat that conservatively (a `None` never blocks work, and never pins the
/// logs against cleanup).
fn task_pid(id: &str) -> Option<u32> {
    let mut parts = id.split('-');
    if parts.next() != Some("vt") {
        return None;
    }
    let _epoch = parts.next()?;
    let pid_hex = parts.next()?;
    let _seq = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    u32::from_str_radix(pid_hex, 16).ok()
}

/// Is the process with this pid alive? Uses `kill(pid, 0)` on Unix (EPERM
/// still means "exists"). Guards pid 0 / out-of-range values (never signals a
/// process group). On non-Unix targets liveness cannot be probed cheaply, so
/// everything reads as alive (conservative: keep data, never double-execute).
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 performs only the permission/existence check; no signal
    // is delivered. pid is validated positive above, so this can never target
    // a process group or "every process" (-1).
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    true
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

/// The set of ids with a terminal (`DONE`/`STALL`) result in a `results.jsonl`
/// body.
fn terminal_ids(results_text: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in results_text.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(kind, KIND_DONE | KIND_STALL) {
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                set.insert(id.to_owned());
            }
        }
    }
    set
}

/// Pure core of [`Inbox::pending_tasks`]: `NEW` tasks whose id has no
/// `CLAIM`/`DONE`/`STALL` line yet, first occurrence kept, posting order
/// preserved. (The [`Inbox`] wrapper additionally drops any id with a claim
/// marker.)
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
    fn try_own_is_an_atomic_compare_and_set() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        // Exactly one of two racing owners wins the same id.
        assert!(inbox.try_own("vt-1").unwrap(), "first owner wins");
        assert!(!inbox.try_own("vt-1").unwrap(), "second owner loses");
        assert!(inbox.is_claimed("vt-1"));
        // Unsafe ids can never be owned (no path escape) — including the
        // Windows drive-relative shape `C:evil`.
        assert!(!inbox.try_own("../escape").unwrap());
        assert!(!inbox.is_claimed("../escape"));
        assert!(!inbox.try_own("C:evil").unwrap());
        assert!(!inbox.is_claimed("C:evil"));
    }

    #[test]
    fn claim_reports_a_lost_arbiter_race() {
        // Regression (second-pass review): claim() used to discard the try_own
        // verdict, so the loser of the O_EXCL race still printed TASK and
        // executed — the double execution the marker exists to prevent.
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.post_task(&sample_task("vt-1")).unwrap();
        // aura's recovery wins the marker first…
        assert!(inbox.try_own("vt-1").unwrap());
        // …so the orchestrator's claim must report the loss.
        assert!(
            !inbox.claim("vt-1").unwrap(),
            "claim() must surface a lost race, not swallow it"
        );
        // The winner path still works and reports ownership.
        inbox.post_task(&sample_task("vt-2")).unwrap();
        assert!(inbox.claim("vt-2").unwrap());
    }

    #[test]
    fn open_claim_tracking_excludes_terminal_and_self() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        // Only its own claim exists → self is never counted.
        inbox.claim("vt-a").unwrap();
        assert!(!inbox.has_open_claim_other_than("vt-a"));
        // A second open claim shows up from the other task's perspective.
        inbox.claim("vt-b").unwrap();
        assert!(inbox.has_open_claim_other_than("vt-a"));
        assert!(inbox.has_open_claim_other_than("vt-b"));
        // Resolving vt-b drops it from the open set.
        inbox.mark_done("vt-b", "done").unwrap();
        assert!(!inbox.has_open_claim_other_than("vt-a"));
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
    fn open_for_call_resets_but_open_preserves() {
        let tmp = tempdir().unwrap();
        {
            let inbox = Inbox::open(tmp.path()).unwrap();
            inbox.post_task(&sample_task("vt-done")).unwrap();
            inbox.post_task(&sample_task("vt-orphan")).unwrap(); // never claimed
            inbox.claim("vt-done").unwrap();
            inbox.mark_done("vt-done", "finished").unwrap();
        }
        // A plain re-open PRESERVES state (CLI-safe, never rewrites the logs).
        let reopened = Inbox::open(tmp.path()).unwrap();
        assert!(reopened.result_for("vt-done").is_some());
        assert_eq!(reopened.pending_tasks().len(), 1, "orphan still pending");
        // open_for_call RESETS: a new call starts from a clean inbox, so a prior
        // call's orphan NEW can never be resurrected.
        let fresh = Inbox::open_for_call(tmp.path()).unwrap();
        assert_eq!(fresh.result_for("vt-done"), None);
        assert!(fresh.pending_tasks().is_empty(), "reset clears orphans");
        assert!(!fresh.is_claimed("vt-done"), "reset clears claim markers");
    }

    #[test]
    fn task_pid_parses_only_our_id_shape() {
        assert_eq!(task_pid("vt-1a2b3c-abc-7"), Some(0xabc));
        assert_eq!(task_pid("vt-1-2-3"), Some(2));
        // Foreign / malformed shapes yield None (treated conservatively).
        assert_eq!(task_pid("vt-1"), None);
        assert_eq!(task_pid("vt-1-2-3-4"), None);
        assert_eq!(task_pid("worker-42"), None);
        assert_eq!(task_pid("vt-x-zz-1"), None);
    }

    /// A task id embedding a pid that provably cannot be alive (far above any
    /// real pid_max) vs one embedding THIS test process's pid.
    #[cfg(unix)]
    fn dead_pid_id(n: u32) -> String {
        format!("vt-1-{:x}-{n}", 0x7fff_fff0u32)
    }
    #[cfg(unix)]
    fn live_pid_id(n: u32) -> String {
        format!("vt-1-{:x}-{n}", std::process::id())
    }

    #[cfg(unix)]
    #[test]
    fn dead_call_leftovers_are_invisible_but_live_ones_survive() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        let dead = dead_pid_id(1);
        let live = live_pid_id(2);
        inbox.post_task(&sample_task(&dead)).unwrap();
        inbox.post_task(&sample_task(&live)).unwrap();
        // pending: the dead call's orphan is filtered (no resurrection); the
        // live call's task is served.
        let pending = inbox.pending_tasks();
        assert_eq!(pending.len(), 1, "dead-pid orphan must not be pending");
        assert_eq!(pending[0].id, live);
        // busy: an open claim from a dead call must not read as a busy
        // orchestrator, but a live call's open claim must.
        inbox.try_own(&dead).unwrap();
        assert!(!inbox.has_open_claim_other_than("x"));
        inbox.try_own(&live).unwrap();
        assert!(inbox.has_open_claim_other_than("x"));
    }

    #[cfg(unix)]
    #[test]
    fn open_for_call_never_wipes_a_live_siblings_state() {
        // Regression (second-pass review): the blind reset deleted a CONCURRENT
        // same-cwd server's live claims (the loopback port-hop topology),
        // voiding its O_EXCL arbiter → double execution. Cleanup must happen
        // only when every referenced pid is provably dead.
        let tmp = tempdir().unwrap();
        let live = live_pid_id(1);
        {
            let inbox = Inbox::open(tmp.path()).unwrap();
            inbox.post_task(&sample_task(&live)).unwrap();
            inbox.claim(&live).unwrap();
        }
        // A "second server" opens for a fresh call in the same cwd: the live
        // sibling's task + claim must SURVIVE.
        let fresh = Inbox::open_for_call(tmp.path()).unwrap();
        assert!(
            fresh.is_claimed(&live),
            "a live sibling's claim marker must survive open_for_call"
        );
        assert!(
            fresh.has_open_claim_other_than("x"),
            "the live sibling still reads as busy"
        );
        // Once only dead pids are referenced, the same open resets everything.
        let tmp2 = tempdir().unwrap();
        let dead = dead_pid_id(1);
        {
            let inbox = Inbox::open(tmp2.path()).unwrap();
            inbox.post_task(&sample_task(&dead)).unwrap();
            inbox.claim(&dead).unwrap();
        }
        let cleaned = Inbox::open_for_call(tmp2.path()).unwrap();
        assert!(!cleaned.is_claimed(&dead), "dead leftovers are cleared");
        assert!(cleaned.pending_tasks().is_empty());
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
