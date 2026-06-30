//! Voice session persistence — the per-session transcript, recap, and
//! its crash-safe on-disk lifecycle.
//!
//! Why this exists
//! ===============
//! A [`Session`] accumulates utterances and a bounded recap that a
//! later call (or a callback re-join) replays as context. Two
//! properties are load-bearing:
//!
//! - **Speech-safety in depth.** `record_*` and `build_recap` redact
//!   defensively, so even if an upstream layer forgot, nothing
//!   secret-bearing survives into the persisted recap. (This is also
//!   where the migration filter for the "past-tense
//!   fabrication" bug lives.)
//! - **Crash-safe atomic saves.** `save_session_atomic` writes to a
//!   uniquely-suffixed temp file (`pid + nanos + counter`, see
//!   `SESSION_TMP_COUNTER`) then renames into place, so a crash mid-write
//!   never corrupts the live file. `prune_sessions` only reaps temp
//!   orphans older than `TMP_PRUNE_MIN_AGE`, so it can't race a
//!   concurrent save by another process.
//!
//! Reads are kernel-bounded; writes go through the private-FS helpers
//! with symlink rejection so the session file stays `0o600` and can't
//! be redirected by a planted symlink.

use crate::private_fs::{reject_symlink_for_write, secure_dir, secure_file};
use crate::{redact_secrets, speech_safe_summary, CheckpointStore, TaskEnvelope};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Per-process counter feeding the unique tmp suffix below. Combined with
/// `pid + nanos` it removes collisions between concurrent saves of the
/// same session id from inside one process (two `save_session_atomic`
/// calls in the same nanosecond would otherwise share a name).
static SESSION_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How long an orphaned `<*.json.tmp.*>` file must sit on disk before
/// `prune_sessions` is willing to delete it. Anything younger could be a
/// concurrent save mid-write by another process; anything older is a
/// crash-orphan from a previous run that will never be renamed.
const TMP_PRUNE_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// Maximum size of a single session JSON file. A session is bounded by
/// `recap_max_chars` (default 1k) plus the envelope plus a handful of
/// utterances; 1 MiB is far above any plausible legitimate payload.
/// Same kernel-bounded read pattern as `aura-discord::read_reason`.
const MAX_SESSION_FILE_BYTES: u64 = 1024 * 1024;

/// Persistent state Aura carries across process invocations of the same
/// Claude Code conversation.
///
/// The identity binding is **conversation-scoped**, not folder-scoped: one
/// Aura session corresponds to one Claude Code session ID. Same folder, new
/// Claude conversation → fresh Aura state. Same Claude conversation
/// resumed → previous Aura state hydrated.
///
/// Every text field stored here passes through `speech_safe_summary` or
/// `redact_secrets` before write, so the recap can be spliced directly
/// into Grok's `instructions` without re-sanitizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub claude_session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub started_at_ms: u128,
    pub last_active_at_ms: u128,
    pub last_envelope: Option<TaskEnvelope>,
    pub last_user_utterance: Option<String>,
    pub last_assistant_utterance: Option<String>,
    pub last_attention_reason: Option<String>,
    /// Speech of the most recent N checkpoints captured at save time.
    pub recent_checkpoints: Vec<String>,
}

impl Session {
    pub fn new(claude_session_id: impl Into<String>, transcript_path: Option<PathBuf>) -> Self {
        let now = current_millis();
        Self {
            claude_session_id: claude_session_id.into(),
            transcript_path,
            started_at_ms: now,
            last_active_at_ms: now,
            last_envelope: None,
            last_user_utterance: None,
            last_assistant_utterance: None,
            last_attention_reason: None,
            recent_checkpoints: Vec::new(),
        }
    }

    pub fn touch(&mut self) {
        self.last_active_at_ms = current_millis();
    }

    pub fn record_envelope(&mut self, envelope: TaskEnvelope) {
        self.last_envelope = Some(envelope);
        self.touch();
    }

    pub fn record_user(&mut self, text: &str) {
        self.last_user_utterance = Some(speech_safe_summary(text));
        self.touch();
    }

    pub fn record_assistant(&mut self, text: &str) {
        self.last_assistant_utterance = Some(speech_safe_summary(text));
        self.touch();
    }

    pub fn record_attention_reason(&mut self, reason: &str) {
        self.last_attention_reason = Some(speech_safe_summary(reason));
        self.touch();
    }

    /// Snapshot the most recent N checkpoint speech lines from the store
    /// into the session for resume-time recap building.
    ///
    /// # Ingestion-lag caveat
    ///
    /// The `CheckpointStore` is fed by an asynchronous mpsc consumer
    /// task (see `aura-core::checkpoints`). `snapshot_checkpoints`
    /// reads the store synchronously, so if the consumer is behind on
    /// draining its inbox at the moment a session save fires, the
    /// snapshot silently misses the most recent worker progress events
    /// and the resume recap will be slightly stale.
    ///
    /// Callers must ensure checkpoint ingestion has caught up before
    /// calling — there is currently no flush primitive on the store.
    /// The right place to fix this is the consumer task itself: have
    /// `CheckpointStore` expose an async `flush()` (e.g. via a oneshot
    /// "watermark" message that the consumer ACKs once drained), and
    /// have `Session::save` await that before snapshotting. That
    /// restructure is out of scope here; this method's contract is
    /// "snapshot whatever the store has visible right now".
    pub fn snapshot_checkpoints(&mut self, store: &CheckpointStore, take: usize) {
        if take == 0 {
            self.recent_checkpoints.clear();
            return;
        }
        self.recent_checkpoints = store
            .recent(take)
            .into_iter()
            .map(|event| event.speech)
            .collect();
        self.touch();
    }

    /// Build a speech-safe recap to splice into Grok's `instructions` on
    /// resume. Truncated to at most `max_bytes` bytes at a UTF-8 char
    /// boundary so the instructions block stays both sane and panic-free.
    pub fn build_recap(&self, max_bytes: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(envelope) = &self.last_envelope {
            if !envelope.user_intent.trim().is_empty() {
                parts.push(format!("Active intent: {}.", envelope.user_intent));
            }
            if !envelope.constraints.is_empty() {
                let joined = envelope.constraints.join("; ");
                parts.push(format!("Standing constraints: {joined}."));
            }
        }
        if !self.recent_checkpoints.is_empty() {
            parts.push(format!(
                "Recent progress: {}.",
                self.recent_checkpoints.join(" ")
            ));
        }
        if let Some(text) = &self.last_user_utterance {
            parts.push(format!("Last user direction: {text}."));
        }
        if let Some(text) = &self.last_assistant_utterance {
            parts.push(format!("Last spoken: {text}."));
        }
        if let Some(reason) = &self.last_attention_reason {
            parts.push(format!("Open thread: {reason}."));
        }
        let mut recap = parts.join(" ");
        truncate_at_char_boundary(&mut recap, max_bytes);
        // Defensive redaction in case any path bypassed the writers above.
        redact_secrets(&recap)
    }
}

fn current_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn truncate_at_char_boundary(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
}

/// Compose `<dir>/<session_id>.json` for storage.
pub fn session_path(dir: &Path, claude_session_id: &str) -> PathBuf {
    let safe = sanitize_session_id(claude_session_id);
    dir.join(format!("{safe}.json"))
}

/// Strip path separators and other unsafe characters from a Claude
/// session id before using it as a filename. The id is normally a UUID,
/// but we don't want to trust the input blindly.
fn sanitize_session_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Canned phrases were removed from `speech_safe_summary` (see
/// `speech.rs` for the full root-cause writeup). Sessions persisted
/// before that change have these strings baked into `recent_checkpoints`
/// and `last_attention_reason`. Without migration, an existing user
/// resuming after the fix would still hear Aura recite the canned
/// phrases on her opener until the checkpoint window cycles them out.
/// This list IS the migration filter — entries matching are dropped.
const STALE_CANNED_CHECKPOINT_PHRASES: &[&str] = &[
    "I found a failure that needs attention.",
    "The latest check is passing.",
    "I updated the relevant project files.",
];

/// Returns true if `entry` is a stale canned checkpoint that should be
/// dropped on load. Matches both the raw canned phrase and the
/// `write_needs_input_hook` wrapper format
/// (`"Claude finished the dispatched task. Summary: <canned>"`).
fn is_stale_canned_checkpoint(entry: &str) -> bool {
    let trimmed = entry.trim();
    for canned in STALE_CANNED_CHECKPOINT_PHRASES {
        if trimmed == *canned {
            return true;
        }
    }
    // Hook-wrapper format: drop when the substance after "Summary: " is
    // EXACTLY one of the canned phrases. Real rich summaries (which also
    // wear this wrapper but carry substantive content after the prefix)
    // are kept.
    if let Some(idx) = trimmed.rfind("Summary: ") {
        let substance = trimmed[idx + "Summary: ".len()..].trim();
        for canned in STALE_CANNED_CHECKPOINT_PHRASES {
            if substance == *canned {
                return true;
            }
        }
    }
    false
}

pub fn load_session(path: &Path) -> Result<Option<Session>, String> {
    let file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("failed to read session {}: {err}", path.display())),
    };
    // Kernel-bounded read so a corrupt or hostile session file (the
    // sessions dir is local but writeable by anything with the user's
    // uid; an attack tool could swap a 64 GB blob into place) cannot
    // OOM the bot.
    let mut buf = Vec::with_capacity(8 * 1024);
    file.take(MAX_SESSION_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|err| format!("failed to read session {}: {err}", path.display()))?;
    if buf.len() as u64 > MAX_SESSION_FILE_BYTES {
        return Err(format!(
            "session {} exceeds {} byte cap (corruption or attack); refusing to load",
            path.display(),
            MAX_SESSION_FILE_BYTES
        ));
    }
    let raw = std::str::from_utf8(&buf)
        .map_err(|err| format!("session {} is not valid UTF-8: {err}", path.display()))?;
    let mut session: Session = serde_json::from_str(raw)
        .map_err(|err| format!("invalid session at {}: {err}", path.display()))?;

    // Migrate away from the canned-phrase residue. Drop
    // recent_checkpoints whose substance is one of the three retired
    // canned phrases, and clear last_attention_reason if it equals one
    // exactly. Substantive content is preserved.
    session
        .recent_checkpoints
        .retain(|entry| !is_stale_canned_checkpoint(entry));
    if let Some(reason) = session.last_attention_reason.as_deref() {
        if STALE_CANNED_CHECKPOINT_PHRASES.contains(&reason.trim()) {
            session.last_attention_reason = None;
        }
    }

    Ok(Some(session))
}

/// Atomic save: write to a unique `<basename>.tmp.<pid>.<nanos>.<counter>`
/// file in the same directory as `path`, then rename onto `path`. Rename
/// is atomic on Unix, so a crash mid-write never leaves a half-written
/// file in the canonical location.
///
/// The unique suffix means concurrent saves of the same session id never
/// share a tmp filename: each writer gets its own file, each rename is
/// independent, and whichever rename completes last wins (a clean atomic
/// publish — both payloads were valid, by definition).
pub fn save_session_atomic(path: &Path, session: &Session) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        // `secure_dir` itself ensures the directory exists; it only
        // chmods to 0o700 when it had to create the dir, so a
        // user-managed parent (e.g. `.` for `history.path = "x.jsonl"`)
        // keeps its original mode.
        secure_dir(parent)?;
    }
    // Refuse to publish onto a pre-planted symlink at the final
    // session path. Without this, `ln -s ~/.ssh/config
    // .aura/sessions/<id>.json` would route the rename through the
    // link, clobbering the target. The tmp path is non-symlinky by
    // construction (unique pid+nanos+counter suffix) so we only check
    // the final publish target.
    reject_symlink_for_write(path)?;
    let tmp_path = unique_tmp_path(path);
    write_then_rename(&tmp_path, path, session).inspect_err(|_| {
        // Best-effort: don't let a stale tmp accumulate when the write or
        // rename fails partway through. Ignore the cleanup error; the
        // original failure is what the caller needs to see.
        let _ = fs::remove_file(&tmp_path);
    })
}

/// Build a per-call unique tmp filename next to `path`. Format is
/// `<basename>.tmp.<pid>.<nanos>.<counter>` so collisions between two
/// concurrent saves of the same session id (same process or two
/// processes) are ruled out by construction. `prune_sessions` matches
/// the `<*.json.tmp.*>` shape so abandoned tmps are still swept.
fn unique_tmp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let base = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let counter = SESSION_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("{base}.tmp.{pid}.{nanos}.{counter}"))
}

/// Returns true when `name` matches the `<*.json.tmp.*>` shape produced
/// by `unique_tmp_path` — i.e. it is one of our session-save tmp files
/// and not an unrelated `.json` payload.
fn is_session_tmp_name(name: &OsStr) -> bool {
    let Some(s) = name.to_str() else {
        return false;
    };
    // Match `<stem>.json.tmp.<suffix>` — the `.json.tmp.` substring plus
    // a non-empty suffix is enough; we need not parse pid/nanos/counter
    // back out, only recognise the pattern.
    if let Some(idx) = s.find(".json.tmp.") {
        // Anything after `.json.tmp.` (length 10) is the unique suffix.
        s.len() > idx + ".json.tmp.".len()
    } else {
        false
    }
}

fn write_then_rename(tmp_path: &Path, final_path: &Path, session: &Session) -> Result<(), String> {
    let mut options = OpenOptions::new();
    // `create_new` fails if the file already exists. Combined with the
    // unique-per-call suffix this is fail-fast on the (vanishingly
    // unlikely) collision rather than silently truncating someone else's
    // in-flight tmp.
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(tmp_path)
        .map_err(|err| format!("failed to open session tmp {}: {err}", tmp_path.display()))?;
    let raw = serde_json::to_string_pretty(session)
        .map_err(|err| format!("failed to serialize session: {err}"))?;
    file.write_all(raw.as_bytes())
        .map_err(|err| format!("failed to write session tmp {}: {err}", tmp_path.display()))?;
    file.write_all(b"\n").map_err(|err| {
        format!(
            "failed to terminate session tmp {}: {err}",
            tmp_path.display()
        )
    })?;
    drop(file);
    secure_file(tmp_path)?;
    fs::rename(tmp_path, final_path)
        .map_err(|err| format!("failed to publish session {}: {err}", final_path.display()))
}

/// Find the active Claude Code session in `transcripts_dir` by picking the
/// most-recently-modified `.jsonl`. Returns the derived session id (file
/// stem) and the absolute transcript path.
pub fn detect_active_claude_session(transcripts_dir: &Path) -> Option<(String, PathBuf)> {
    let entries = fs::read_dir(transcripts_dir).ok()?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(current, _)| modified > *current) {
            best = Some((modified, path));
        }
    }
    let (_, path) = best?;
    let id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_owned)?;
    Some((id, path))
}

/// Drop session files whose underlying Claude transcript no longer exists,
/// or that have been idle longer than `max_age_days`. Also sweeps
/// corrupt session files older than `max_age_days` so they don't
/// accumulate forever (a today-corrupt file is preserved in case a fix
/// is incoming). Returns the number of files removed. Errors are
/// swallowed (best-effort).
pub fn prune_sessions(dir: &Path, max_age_days: u64) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = current_millis();
    let max_age_ms = (max_age_days as u128).saturating_mul(86_400_000);
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // Sweep `<*.json.tmp.*>` files left by a save that crashed between
        // open() and rename(). Only delete when older than `TMP_PRUNE_MIN_AGE`
        // so an in-flight save by a concurrent process is never yanked
        // mid-write — anything younger is plausibly still being written.
        if let Some(name) = path.file_name() {
            if is_session_tmp_name(name) {
                let mtime_age = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| SystemTime::now().duration_since(t).ok());
                if matches!(mtime_age, Some(age) if age >= TMP_PRUNE_MIN_AGE) {
                    let _ = fs::remove_file(&path);
                }
                continue;
            }
        }
        let ext = path.extension().and_then(|ext| ext.to_str());
        if ext != Some("json") {
            continue;
        }
        let session = match load_session(&path) {
            Ok(Some(session)) => session,
            Ok(None) => continue,
            Err(_) => {
                // Corrupt session file: load_session returns Err on bad
                // JSON. Without this branch the file would accumulate
                // forever. Use the on-disk mtime as a stale-clock since
                // we cannot read `last_active_at_ms` from the corrupt
                // payload. Only delete if we'd otherwise miss the
                // chance — leave today's corrupt file in case a fix is
                // landing.
                if max_age_days == 0 {
                    continue;
                }
                let mtime_ms = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(now);
                if now.saturating_sub(mtime_ms) > max_age_ms && fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
                continue;
            }
        };
        let transcript_gone = match &session.transcript_path {
            // `Path::exists()` flattens "stat says NotFound" and
            // "stat itself failed (NFS down, EPERM on the parent,
            // intermittent FUSE mount)" to the same `false`. That can
            // permanently delete a still-valid session over a transient
            // IO blip. `try_exists()` separates the two; treating
            // "we can't tell" as "still exists, don't prune" is the
            // safer default — a stale session lingers another sweep,
            // a wrongly-pruned one is gone forever.
            Some(transcript) => !transcript.try_exists().unwrap_or(true),
            None => false,
        };
        let stale = max_age_days > 0 && now.saturating_sub(session.last_active_at_ms) > max_age_ms;
        if (transcript_gone || stale) && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallbackMode, CheckpointEvent, CheckpointKind};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{label}-{nanos}"))
    }

    #[test]
    fn round_trip_atomic_save_and_load() {
        let dir = unique_dir("aura-session-roundtrip");
        let path = session_path(&dir, "550e8400-e29b-41d4-a716-446655440000");
        let mut session = Session::new(
            "550e8400-e29b-41d4-a716-446655440000",
            Some(PathBuf::from("/tmp/aura-fake.jsonl")),
        );
        session.record_envelope(TaskEnvelope::new(
            "migrate auth",
            vec!["do not touch production config".to_owned()],
            "aura",
            CallbackMode::PingFirst,
            "approval-token",
        ));
        session.record_user("Let's keep going on the auth migration.");
        save_session_atomic(&path, &session).unwrap();

        let loaded = load_session(&path).unwrap().expect("session present");
        assert_eq!(loaded.claude_session_id, session.claude_session_id);
        assert_eq!(loaded.last_envelope.unwrap().user_intent, "migrate auth");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn build_recap_is_truncated_safely() {
        let mut session = Session::new("test", None);
        // Em-dashes are 3 bytes; pack the recap so the cap lands inside one.
        session.record_user(&"\u{2014}".repeat(400));
        let recap = session.build_recap(120);
        assert!(recap.len() <= 120);
        assert!(recap.is_char_boundary(recap.len()));
    }

    #[test]
    fn build_recap_includes_envelope_and_progress() {
        let store = CheckpointStore::new(8, None);
        store
            .append(CheckpointEvent::new(
                CheckpointKind::Phase,
                "now verifying the migration helper",
            ))
            .unwrap();
        let mut session = Session::new("conv-1", None);
        session.record_envelope(TaskEnvelope::new(
            "migrate auth",
            vec!["do not touch production config".to_owned()],
            "aura",
            CallbackMode::PingFirst,
            "approval",
        ));
        session.snapshot_checkpoints(&store, 5);

        let recap = session.build_recap(1000);
        assert!(recap.contains("migrate auth"));
        assert!(recap.contains("do not touch production config"));
        assert!(recap.contains("Recent progress"));
    }

    #[test]
    fn detect_active_claude_session_picks_most_recent() {
        let dir = unique_dir("aura-session-detect");
        fs::create_dir_all(&dir).unwrap();
        let older = dir.join("conv-old.jsonl");
        fs::write(&older, "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = dir.join("conv-new.jsonl");
        fs::write(&newer, "{}\n").unwrap();
        // A non-jsonl file must be ignored.
        fs::write(dir.join("notes.txt"), "hi").unwrap();

        let (id, path) = detect_active_claude_session(&dir).expect("a session");
        assert_eq!(id, "conv-new");
        assert_eq!(path, newer);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_drops_sessions_whose_transcript_is_gone() {
        let dir = unique_dir("aura-session-prune");
        fs::create_dir_all(&dir).unwrap();
        let path = session_path(&dir, "ghost");
        let session = Session::new("ghost", Some(PathBuf::from("/tmp/does-not-exist.jsonl")));
        save_session_atomic(&path, &session).unwrap();
        assert!(path.exists());

        let removed = prune_sessions(&dir, 0);
        assert_eq!(removed, 1);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn prune_keeps_session_when_transcript_stat_is_inconclusive() {
        // Regression: the old code used `Path::exists()`, which collapses
        // "file gone" and "stat failed (NFS down, EPERM, FUSE timeout)"
        // into the same `false` and would permanently delete a still-
        // valid session over a transient IO blip. `try_exists()`
        // surfaces the failure; `unwrap_or(true)` keeps the session.
        //
        // We synthesize the stat failure by pointing the transcript path
        // *through* an existing regular file, so the kernel returns
        // `ENOTDIR` while traversing — same shape `try_exists()` would
        // see on a flapping mount. No platform-specific permission
        // gymnastics needed.
        let dir = unique_dir("aura-session-prune-stat-fail");
        fs::create_dir_all(&dir).unwrap();
        // Regular file pretending to be a directory in the path.
        let blocker = dir.join("blocker.file");
        fs::write(&blocker, b"x").unwrap();
        let unreachable_transcript = blocker.join("subdir").join("transcript.jsonl");
        // Sanity check: try_exists must actually return Err here, not
        // Ok(false). Otherwise the test is exercising the wrong branch
        // and the regression could come back unnoticed.
        assert!(
            unreachable_transcript.try_exists().is_err(),
            "test setup expected ENOTDIR-style stat failure"
        );

        let session_file = session_path(&dir, "stat-fail");
        let session = Session::new("stat-fail", Some(unreachable_transcript));
        save_session_atomic(&session_file, &session).unwrap();
        assert!(session_file.exists());

        // max_age_days = 0 means age-based pruning is OFF, so the only
        // way the session could be removed is the transcript-gone path.
        // With `try_exists() == Err`, the new code must keep the file.
        let removed = prune_sessions(&dir, 0);
        assert_eq!(
            removed, 0,
            "session pruned despite inconclusive transcript stat"
        );
        assert!(
            session_file.exists(),
            "session file gone despite inconclusive transcript stat"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_saves_of_same_session_publish_one_valid_payload() {
        // Two saves of the same session id must not collide on a shared
        // tmp filename; whichever rename completes last wins, and the
        // final file must deserialize as one of the two payloads (never
        // a half-written corruption).
        let dir = unique_dir("aura-session-concurrent");
        let path = session_path(&dir, "concurrent-test");

        let mut session_a = Session::new("concurrent-test", None);
        session_a.record_user("payload A: alpha bravo charlie");
        let mut session_b = Session::new("concurrent-test", None);
        session_b.record_user("payload B: zulu yankee xray");

        let path_a = path.clone();
        let path_b = path.clone();
        let task_a = tokio::spawn(async move { save_session_atomic(&path_a, &session_a) });
        let task_b = tokio::spawn(async move { save_session_atomic(&path_b, &session_b) });

        task_a.await.unwrap().unwrap();
        task_b.await.unwrap().unwrap();

        let loaded = load_session(&path)
            .expect("final file deserializes")
            .expect("session present");
        let utterance = loaded.last_user_utterance.unwrap_or_default();
        assert!(
            utterance.contains("payload A") || utterance.contains("payload B"),
            "final session should match one of the two writers, got {utterance:?}"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unique_tmp_path_is_distinct_per_call_and_matches_sweep_pattern() {
        let final_path = PathBuf::from("/tmp/aura-tmp-test/conv-1.json");
        let a = unique_tmp_path(&final_path);
        let b = unique_tmp_path(&final_path);
        assert_ne!(a, b, "two calls must produce distinct tmp filenames");
        // Both must be detectable by the prune-sweep predicate so
        // crash-orphans are eventually collected.
        assert!(is_session_tmp_name(a.file_name().unwrap()));
        assert!(is_session_tmp_name(b.file_name().unwrap()));
        // A canonical `<id>.json` file must NOT match — otherwise the
        // sweep would delete real sessions.
        assert!(!is_session_tmp_name(OsStr::new("conv-1.json")));
    }

    #[test]
    fn prune_keeps_recent_tmp_files_and_only_sweeps_aged_ones() {
        // A young tmp from a concurrent process must survive a prune;
        // only crash-orphans older than TMP_PRUNE_MIN_AGE may be deleted.
        let dir = unique_dir("aura-session-tmp-prune");
        fs::create_dir_all(&dir).unwrap();
        let young_tmp = dir.join("conv-young.json.tmp.123.456.0");
        fs::write(&young_tmp, b"{}").unwrap();

        let removed = prune_sessions(&dir, 0);
        assert_eq!(removed, 0, "prune_sessions reports session removals only");
        assert!(young_tmp.exists(), "young tmp must survive a prune");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_session_file_is_private() {
        let dir = unique_dir("aura-session-private");
        let path = session_path(&dir, "private-test");
        save_session_atomic(&path, &Session::new("private-test", None)).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let _ = fs::remove_dir_all(dir);
    }

    /// Regression for the corrupt-file branch in prune_sessions
    /// (session.rs:404-426). When `load_session` returns Err on bad
    /// JSON, prune uses the on-disk mtime as a stale clock — but only
    /// deletes when `max_age_days > 0`. A young corrupt file MUST be
    /// preserved (a fix may be incoming); an aged corrupt file MUST be
    /// swept (otherwise garbage accumulates forever). Uses the stable
    /// `std::fs::FileTimes` API to backdate mtime without shelling out.
    #[test]
    fn prune_drops_aged_corrupt_session_files() {
        let dir = unique_dir("aura-prune-corrupt");
        fs::create_dir_all(&dir).unwrap();

        // Two corrupt sessions: one freshly written, one with mtime
        // backdated 7 days. Both load_session() returns Err on; only
        // the aged one should get swept.
        let young = dir.join("young.json");
        let old = dir.join("old.json");
        fs::write(&young, b"this is not valid json {{{").unwrap();
        fs::write(&old, b"also not json").unwrap();

        let seven_days_ago = SystemTime::now() - std::time::Duration::from_secs(7 * 86_400);
        let times = std::fs::FileTimes::new().set_modified(seven_days_ago);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_times(times)
            .unwrap();

        // Sanity: load_session must reject both as corrupt before we
        // assert prune behavior.
        assert!(load_session(&young).is_err(), "young must be corrupt");
        assert!(load_session(&old).is_err(), "old must be corrupt");

        // max_age_days = 1: aged corrupt sweep fires; young preserved.
        let removed = prune_sessions(&dir, 1);
        assert_eq!(removed, 1, "exactly one corrupt file should be swept");
        assert!(young.exists(), "today's corrupt file MUST be preserved");
        assert!(!old.exists(), "aged corrupt file MUST be swept");

        let _ = fs::remove_dir_all(dir);
    }

    /// Contract: prune with max_age_days = 0 means "age-based pruning
    /// is OFF" — even an aged corrupt file must be left alone. The
    /// early-return guard above protects this case; without it the
    /// corrupt branch would happily delete files at max_age_days=0.
    #[test]
    fn prune_keeps_corrupt_files_when_max_age_days_is_zero() {
        let dir = unique_dir("aura-prune-corrupt-noop");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.json");
        fs::write(&path, b"not json at all").unwrap();

        // Even with a deeply backdated mtime, max_age_days=0 must NOT
        // delete. The early-return is the contract.
        let year_ago = SystemTime::now() - std::time::Duration::from_secs(365 * 86_400);
        let times = std::fs::FileTimes::new().set_modified(year_ago);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(times)
            .unwrap();

        let removed = prune_sessions(&dir, 0);
        assert_eq!(removed, 0, "max_age_days=0 must skip aged corrupt sweep");
        assert!(path.exists(), "file must survive when pruning is OFF");

        let _ = fs::remove_dir_all(dir);
    }

    /// Regression test for the canned-phrase migration.
    /// Sessions persisted before the speech.rs canned-phrase removal
    /// have entries like "I found a failure that needs
    /// attention." baked into recent_checkpoints from the old speech
    /// filter. After the fix, those phrases no longer correspond to any
    /// real ground truth — but a returning user resuming an old session
    /// would still hear Aura recite them on her opener until the
    /// checkpoint window cycles them out. load_session migrates these
    /// out at deserialize time. This test pins both the raw-phrase form
    /// and the hook-wrapper form ("Claude finished the dispatched task.
    /// Summary: <canned>"), and asserts substantive content survives
    /// (including substantive content wearing the same wrapper).
    #[test]
    fn load_session_drops_stale_canned_checkpoints() {
        let dir = unique_dir("aura-session-canned-migration");
        let path = session_path(&dir, "770e8400-e29b-41d4-a716-446655440042");
        let mut session = Session::new(
            "770e8400-e29b-41d4-a716-446655440042",
            Some(PathBuf::from("/tmp/aura-fake-migration.jsonl")),
        );
        session.recent_checkpoints = vec![
            // Raw canned phrases — must drop.
            "I found a failure that needs attention.".to_owned(),
            "The latest check is passing.".to_owned(),
            "I updated the relevant project files.".to_owned(),
            // Hook-wrapper format with canned substance — must drop.
            "Claude finished the dispatched task. Summary: I found a failure that needs attention.".to_owned(),
            "Claude stopped on the dispatched task. Summary: The latest check is passing.".to_owned(),
            // Real substantive content — MUST survive (raw form).
            "I refactored the auth module and tightened token rotation.".to_owned(),
            // Real substantive content with hook wrapper — MUST survive.
            "Claude finished the dispatched task. Summary: I refactored the auth module and added a regression test for token rotation.".to_owned(),
        ];
        // Stale canned attention reason — must be cleared.
        session.last_attention_reason = Some("I found a failure that needs attention.".to_owned());
        save_session_atomic(&path, &session).unwrap();

        let loaded = load_session(&path).unwrap().expect("session present");
        assert_eq!(
            loaded.recent_checkpoints.len(),
            2,
            "expected 2 substantive entries to survive; got: {:?}",
            loaded.recent_checkpoints
        );
        assert!(
            loaded
                .recent_checkpoints
                .iter()
                .any(|entry| entry.contains("refactored the auth module and tightened")),
            "raw substantive entry must survive; got: {:?}",
            loaded.recent_checkpoints
        );
        assert!(
            loaded
                .recent_checkpoints
                .iter()
                .any(|entry| entry.contains("regression test for token rotation")),
            "wrapped substantive entry must survive; got: {:?}",
            loaded.recent_checkpoints
        );
        // None of the canned phrases or their wrappers may survive.
        for entry in &loaded.recent_checkpoints {
            assert!(
                !entry.contains("found a failure that needs attention"),
                "canned 'failure' phrase leaked through migration: {entry:?}"
            );
            assert!(
                !entry.contains("latest check is passing"),
                "canned 'passing' phrase leaked through migration: {entry:?}"
            );
            assert!(
                !entry.contains("updated the relevant project files"),
                "canned 'updated files' phrase leaked through migration: {entry:?}"
            );
        }
        assert!(
            loaded.last_attention_reason.is_none(),
            "stale canned attention reason should have been cleared; got: {:?}",
            loaded.last_attention_reason
        );

        let _ = fs::remove_dir_all(dir);
    }
}
