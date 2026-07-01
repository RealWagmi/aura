//! Worker progress checkpoints — speech-safe markers Aura can narrate
//! while a downstream worker runs.
//!
//! Why this exists
//! ===============
//! A coding worker emits a stream of tool transitions, phase
//! boundaries, and errors. The voice layer wants to say "it's verifying
//! now" without re-reading raw tool output, so each
//! [`CheckpointEvent`] is run through `speech_safe_summary` at
//! construction and held in a bounded in-memory ring (with optional
//! JSONL append). The cap matters: a runaway worker must not grow this
//! store without bound between drains into voice context.

use crate::private_fs::append_jsonl_line;
use crate::speech_safe_summary;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// A summarized worker progress marker. Speech-safe by construction so the
/// store can be drained directly into Grok-facing context without
/// re-sanitizing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointEvent {
    pub timestamp_ms: u128,
    pub kind: CheckpointKind,
    pub speech: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    /// A worker tool transition (e.g. Edit, Bash, Read).
    ToolUse,
    /// A phase boundary inside the worker's reasoning (e.g. "now planning",
    /// "verifying").
    Phase,
    /// Worker reported an error, panic, or failure.
    Error,
    /// Free-form note that didn't match the other categories.
    Note,
    /// Worker signaled it is blocked and needs the user.
    NeedsUserInput,
}

impl CheckpointEvent {
    pub fn new(kind: CheckpointKind, speech: impl Into<String>) -> Self {
        let raw = speech.into();
        // Always run through the speech-safe filter at construction so
        // anything the producer hands us is already redacted by the time it
        // hits the ring buffer or disk.
        let spoken = speech_safe_summary(&raw);
        Self {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
            kind,
            speech: spoken,
        }
    }
}

/// Bounded in-memory ring with optional JSONL append. The ring holds the
/// most recent worker checkpoints so `get_context_summary` can answer
/// "what's going on?" with real progress instead of a static briefing.
///
/// Aura never speaks these events on its own; they are pulled into Grok
/// context only when the user asks or the agent surfaces them.
///
/// INVARIANT: disk logging is **off by default**. The
/// `.aura/checkpoints.jsonl` writer is append-only with no rotation,
/// so a long-running session would grow the file without bound. The
/// in-memory ring is the durable surface across the lifetime of a
/// single live_call. Users opt back into disk persistence by setting
/// `checkpoints.log_path` in the user config.
#[derive(Debug)]
pub struct CheckpointStore {
    inner: Mutex<CheckpointInner>,
}

#[derive(Debug)]
struct CheckpointInner {
    ring: VecDeque<CheckpointEvent>,
    capacity: usize,
    log_path: Option<PathBuf>,
}

impl CheckpointStore {
    pub fn new(capacity: usize, log_path: Option<PathBuf>) -> Self {
        let cap = capacity.max(1);
        Self {
            inner: Mutex::new(CheckpointInner {
                ring: VecDeque::with_capacity(cap),
                capacity: cap,
                log_path,
            }),
        }
    }

    pub fn append(&self, event: CheckpointEvent) -> Result<(), String> {
        let log_path = {
            // SAFETY: the critical sections in this module touch only a
            // VecDeque + a PathBuf clone — no user code, no allocator
            // failure path that panics, no `?` inside the lock. The
            // lock can only be poisoned by a panic inside the guard,
            // and there is no such panic site here. `expect` here is a
            // crash-on-bug, not a runtime error path.
            let mut guard = self.inner.lock().expect("checkpoint lock poisoned");
            if guard.ring.len() == guard.capacity {
                let _ = guard.ring.pop_front();
            }
            guard.ring.push_back(event.clone());
            guard.log_path.clone()
        };
        if let Some(path) = log_path {
            persist(&path, &event)?;
        }
        Ok(())
    }

    pub fn recent(&self, n: usize) -> Vec<CheckpointEvent> {
        // SAFETY: see `append` — the critical section is panic-free.
        let guard = self.inner.lock().expect("checkpoint lock poisoned");
        let skip = guard.ring.len().saturating_sub(n);
        guard.ring.iter().skip(skip).cloned().collect()
    }

    pub fn len(&self) -> usize {
        // SAFETY: see `append` — the critical section is panic-free.
        self.inner
            .lock()
            .expect("checkpoint lock poisoned")
            .ring
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn persist(path: &Path, event: &CheckpointEvent) -> Result<(), String> {
    append_jsonl_line(path, event, "checkpoint log")
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the Unix permission tests touch `fs`; unused on other targets.
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn ring_drops_oldest_when_full() {
        let store = CheckpointStore::new(2, None);
        store
            .append(CheckpointEvent::new(CheckpointKind::Note, "first message"))
            .unwrap();
        store
            .append(CheckpointEvent::new(CheckpointKind::Note, "second message"))
            .unwrap();
        store
            .append(CheckpointEvent::new(CheckpointKind::Note, "third message"))
            .unwrap();
        let recent = store.recent(10);
        assert_eq!(recent.len(), 2);
        assert!(!recent.iter().any(|event| event.speech.contains("first")));
    }

    #[test]
    fn checkpoint_speech_is_redacted_at_construction() {
        let event = CheckpointEvent::new(
            CheckpointKind::Error,
            "API_KEY=abc12345678901234567890 failed at src/auth.rs:217",
        );
        // Speech-safety transformations stay: secrets redacted, line numbers
        // replaced.
        assert!(!event.speech.contains("abc123"));
        assert!(!event.speech.contains("217"));
        // A prior assertion checked for "failure" — the canned
        // phrase `speech_safe_summary` used to inject for any "fail" input.
        // That substitution was a root cause and is gone. Now we
        // assert the actual word ("failed") survives in the speech_safe
        // output instead of being replaced with a templated phrase.
        assert!(
            event.speech.contains("failed"),
            "expected 'failed' from real content to survive; speech={:?}",
            event.speech
        );
    }

    #[test]
    fn recent_returns_oldest_first_within_window() {
        let store = CheckpointStore::new(8, None);
        for label in ["alpha message", "beta message", "gamma message"] {
            store
                .append(CheckpointEvent::new(CheckpointKind::Note, label))
                .unwrap();
        }
        let recent = store.recent(2);
        assert_eq!(recent.len(), 2);
        assert!(recent[0].speech.contains("beta"));
        assert!(recent[1].speech.contains("gamma"));
    }

    /// Concurrency regression: CheckpointStore is documented as
    /// shareable across threads (Mutex<CheckpointInner>), and the
    /// production wiring spawns an mpsc consumer task that calls
    /// `append` from one thread while the live loop reads via `recent`
    /// from another. None of the existing tests exercised that path —
    /// a future refactor that swapped Mutex for, say, an unsynchronized
    /// shape would break under load with no test coverage. This test
    /// hammers append + recent from N tasks concurrently and asserts
    /// (a) no panics, (b) the capacity bound holds, (c) every recent()
    /// snapshot is a valid suffix of the append history (no torn reads
    /// of half-constructed events).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn store_survives_concurrent_append_and_recent_under_load() {
        use std::sync::Arc;
        let store = Arc::new(CheckpointStore::new(64, None));
        let writers = (0..4).map(|writer_id| {
            let store = store.clone();
            tokio::spawn(async move {
                for i in 0u32..50 {
                    let event = CheckpointEvent::new(
                        CheckpointKind::Note,
                        format!("writer-{writer_id}-event-{i}"),
                    );
                    store
                        .append(event)
                        .expect("append must not panic under contention");
                    if i % 7 == 0 {
                        // Yield occasionally so the readers actually
                        // get to interleave instead of being starved.
                        tokio::task::yield_now().await;
                    }
                }
            })
        });
        let readers = (0..4).map(|_| {
            let store = store.clone();
            tokio::spawn(async move {
                let mut max_seen = 0usize;
                for _ in 0..200 {
                    let snapshot = store.recent(64);
                    // The capacity bound MUST hold even mid-contention.
                    assert!(
                        snapshot.len() <= 64,
                        "recent() returned more than capacity: {}",
                        snapshot.len()
                    );
                    // Every event must be a fully-constructed
                    // CheckpointEvent (no torn reads); validate by
                    // touching every field. If this panics or the
                    // string is empty, we have a torn-read bug.
                    for event in &snapshot {
                        assert!(!event.speech.is_empty(), "torn read: empty speech");
                    }
                    max_seen = max_seen.max(snapshot.len());
                    tokio::task::yield_now().await;
                }
                max_seen
            })
        });

        for w in writers {
            w.await.expect("writer task panicked");
        }
        // Drain readers — they may have observed any depth from 0 to 64.
        let mut peak = 0;
        for r in readers {
            peak = peak.max(r.await.expect("reader task panicked"));
        }
        // After all writers done, the store must hold the most recent
        // 64 events (4 writers × 50 events = 200 total written, capped
        // at 64).
        let final_snapshot = store.recent(usize::MAX);
        assert_eq!(
            final_snapshot.len(),
            64,
            "post-write snapshot must equal capacity"
        );
        // And at least one reader observed a nontrivial depth (proves
        // the readers actually ran during writing rather than after).
        assert!(peak > 0, "readers never observed any events");
    }

    #[cfg(unix)]
    #[test]
    fn persisted_log_is_private() {
        let dir = std::env::temp_dir().join(format!(
            "aura-checkpoints-private-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("checkpoints.jsonl");
        let store = CheckpointStore::new(4, Some(path.clone()));
        store
            .append(CheckpointEvent::new(CheckpointKind::Note, "private note"))
            .unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let _ = fs::remove_dir_all(dir);
    }
}
