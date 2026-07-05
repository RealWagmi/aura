//! `OrchestratedRuntime` — the Scheme 2 dispatch decorator.
//!
//! It wraps the raw host [`AgentRuntime`] (the direct `claude -p` / worker
//! spawn) and changes ONLY dispatch: on [`start_task`](AgentRuntime::start_task)
//! it routes the task through the live orchestrator via the [`Inbox`],
//! blocking-waiting for the orchestrator's `DONE` (mirroring the wrapped
//! runtime, which also blocks for the full task while the model holds the call
//! with `pause_call_until`). Every other method delegates straight through.
//!
//! ## Fallback (a dead loop never strands a call)
//! Before touching the inbox it checks the orchestrator **heartbeat**. If no
//! live watch-loop is running (stale/absent heartbeat) it dispatches directly —
//! zero added latency, identical to the pre-Scheme-2 behaviour. If a loop IS
//! live but never **claims** the task within `claim_timeout`, or claims it but
//! never reports `DONE` within `work_timeout`, it also falls back to a direct
//! dispatch. So the inbox path is a best-effort fast lane over a runtime that
//! always still works on its own.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use aura_core::tools::{
    AgentContext, AgentRuntime, AgentStatus, AttentionAck, AttentionRequest, CancelAck, CancelMode,
    TaskEnvelope, TaskHandoffState, TaskResult,
};
use aura_core::CheckpointEvent;

use crate::inbox::{now_epoch, Inbox, InboxResult, InboxTask};

/// Timing knobs for the orchestrator fast lane. Defaults are tuned for a live
/// call: liveness is checked in seconds (not the minutes a cron would take), and
/// the work ceiling is generous because a real coding task can run for minutes
/// while the model keeps the call paused.
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// A heartbeat younger than this means a live watch-loop is consuming tasks.
    pub heartbeat_max_age: Duration,
    /// If the orchestrator is alive+idle yet does not `CLAIM` a posted task
    /// within this, treat the loop as wedged and recover the task (own it, then
    /// dispatch directly). Does not apply while the orchestrator holds an open
    /// claim on another task — aura waits for it to free up rather than racing a
    /// concurrent writer.
    pub claim_timeout: Duration,
    /// If a claimed task produces no `DONE` within this, treat it as stalled
    /// and dispatch directly.
    pub work_timeout: Duration,
    /// How often to re-check the inbox while waiting (a short, sub-second tick —
    /// not a cron poll).
    pub tick: Duration,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            heartbeat_max_age: Duration::from_secs(20),
            claim_timeout: Duration::from_secs(10),
            work_timeout: Duration::from_secs(30 * 60),
            tick: Duration::from_millis(150),
        }
    }
}

/// The terminal disposition of a posted task, from the caller's perspective.
enum WaitOutcome {
    /// The orchestrator finished the task — speak its result.
    Done(InboxResult),
    /// Safe to dispatch directly: the task was never claimed (and is now
    /// tombstoned) or the orchestrator explicitly handed it back (`STALL`). No
    /// orchestrator work is in flight for this id, so a direct worker cannot
    /// collide with it.
    DispatchDirectly,
    /// The orchestrator claimed this task (or is busy on another) past
    /// `work_timeout`. It may still be mutating the repo, so we must NOT spawn a
    /// second worker — hand back a non-executing result instead.
    OrchestratorBusy,
}

/// Wraps a raw [`AgentRuntime`] and routes dispatch through the live
/// orchestrator ([`Inbox`]), with a direct-dispatch fallback.
pub struct OrchestratedRuntime {
    inner: Arc<dyn AgentRuntime>,
    inbox: Inbox,
    cfg: OrchestratorConfig,
    /// Per-call monotonic task counter; combined with `id_prefix`/`pid` for
    /// global uniqueness.
    seq: AtomicU64,
    /// The server start epoch. Combined with `pid` so two servers sharing a cwd
    /// (e.g. a LOCAL loopback port-hop) started in the SAME second still mint
    /// disjoint task ids — a bare epoch at second granularity would collide.
    id_prefix: u64,
    /// This process's id — the per-process discriminator in the task id.
    pid: u32,
    /// The spoken intents of dispatches currently routed through the inbox,
    /// keyed by their sequence number. The wrapped runtime does NOT see inbox
    /// tasks (the orchestrator, not the inner runtime, executes them), so
    /// `status()`/`context()` would otherwise report `idle` while tasks are in
    /// flight. Dispatches can overlap (the engine spawns each onto a
    /// `JoinSet`), so this is a keyed set, not a single slot — one dispatch
    /// finishing must not erase another's entry.
    active: Mutex<Vec<(u64, String)>>,
}

/// Removes ITS OWN dispatch's `active` entry when the inbox wait ends, on
/// every return path. Keyed by sequence number so overlapping dispatches never
/// clobber each other's status.
struct ActiveGuard<'a> {
    slot: &'a Mutex<Vec<(u64, String)>>,
    key: u64,
}

impl Drop for ActiveGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.slot.lock() {
            entries.retain(|(key, _)| *key != self.key);
        }
    }
}

impl OrchestratedRuntime {
    /// Wrap `inner`, routing dispatch through `inbox` under `cfg`.
    pub fn new(inner: Arc<dyn AgentRuntime>, inbox: Inbox, cfg: OrchestratorConfig) -> Self {
        Self {
            inner,
            inbox,
            cfg,
            seq: AtomicU64::new(1),
            id_prefix: now_epoch(),
            pid: std::process::id(),
            active: Mutex::new(Vec::new()),
        }
    }

    /// A globally-unique task id (`vt-<epoch>-<pid>-<n>`) plus its sequence
    /// number (the `active`-tracking key).
    fn next_id(&self) -> (u64, String) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        (n, format!("vt-{:x}-{:x}-{n}", self.id_prefix, self.pid))
    }

    /// The most recently started still-in-flight inbox dispatch, if any.
    fn latest_active(&self) -> Option<String> {
        self.active
            .lock()
            .ok()
            .and_then(|entries| entries.last().map(|(_, intent)| intent.clone()))
    }

    /// Block until the orchestrator reports a terminal result for `id`, or the
    /// wait resolves to one of the [`WaitOutcome`] dispositions.
    ///
    /// Ownership is decided by the inbox's O_EXCL claim marker, not by a race
    /// between two log appends: whenever aura wants to give up and dispatch
    /// directly it FIRST [`try_own`](Inbox::try_own)s the id. If it wins the
    /// marker the orchestrator can no longer claim the task, so exactly one
    /// executor runs it (no double execution — even if the follow-up `STALL`
    /// line write fails). If it loses, the orchestrator just claimed it and we
    /// keep waiting for the result.
    ///
    /// While the orchestrator is demonstrably busy — its heartbeat is fresh, or
    /// it holds an open claim on another task ([`has_open_claim_other_than`]) —
    /// aura keeps waiting rather than spawning a concurrent direct writer that
    /// could collide with the live session's edits. It only recovers a task
    /// itself once the orchestrator is neither alive nor busy, or a bounded
    /// ceiling elapses.
    ///
    /// [`has_open_claim_other_than`]: Inbox::has_open_claim_other_than
    async fn await_result(&self, id: &str) -> WaitOutcome {
        let start = tokio::time::Instant::now();
        loop {
            if let Some(res) = self.inbox.result_for(id) {
                // A DONE is a completion; a STALL for this id can only be the
                // orchestrator's own explicit hand-back (our own tombstone path
                // returns immediately, never loops back to read it) → safe to
                // dispatch directly.
                return if res.is_done() {
                    WaitOutcome::Done(res)
                } else {
                    WaitOutcome::DispatchDirectly
                };
            }

            if self.inbox.is_claimed(id) {
                // The orchestrator owns this task. Wait for its DONE up to the
                // work ceiling; on overrun, it may still be editing the repo, so
                // we must NOT spawn a second writer — hand back a non-executing
                // result instead. Deliberately NO tombstone here: a STALL line
                // would put the id into the terminal set and erase the
                // open-claim "busy" signal, letting the NEXT dispatch run a
                // direct cold worker concurrently with the still-working live
                // session. The claim stays open; when the session eventually
                // finishes it reports in its own chat (its late DONE is simply
                // never read). If it died mid-work instead, later dispatches
                // keep queueing behind the open claim and hand back honestly
                // after their own ceilings — degraded, but never two writers.
                if start.elapsed() >= self.cfg.work_timeout {
                    return WaitOutcome::OrchestratorBusy;
                }
            } else {
                let alive = self.inbox.orchestrator_alive(self.cfg.heartbeat_max_age);
                let busy_elsewhere = self.inbox.has_open_claim_other_than(id);
                if !alive && !busy_elsewhere {
                    // No live watch-loop and not mid another task → recover the
                    // task ourselves: atomically own it (so a late loop cannot
                    // re-run it), tombstone for observability, dispatch directly.
                    match self.inbox.try_own(id) {
                        Ok(true) => {
                            self.tombstone(id, "orchestrator offline");
                            return WaitOutcome::DispatchDirectly;
                        }
                        // Lost the race — the orchestrator just claimed it. Loop;
                        // the next iteration sees `is_claimed` and waits for DONE.
                        Ok(false) => {}
                        // Inbox FS is broken; fail open like `post_task` does.
                        Err(e) => {
                            eprintln!("aura-engine: inbox claim failed for {id}: {e}");
                            return WaitOutcome::DispatchDirectly;
                        }
                    }
                } else if alive && !busy_elsewhere && start.elapsed() >= self.cfg.claim_timeout {
                    // Alive and idle, yet never claimed within the window — a
                    // wedged loop. Recover it (same atomic-own guard).
                    match self.inbox.try_own(id) {
                        Ok(true) => {
                            self.tombstone(id, "not claimed in time");
                            return WaitOutcome::DispatchDirectly;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            eprintln!("aura-engine: inbox claim failed for {id}: {e}");
                            return WaitOutcome::DispatchDirectly;
                        }
                    }
                } else if busy_elsewhere && start.elapsed() >= self.cfg.work_timeout {
                    // The orchestrator has been busy on another task past the work
                    // ceiling and still has not reached ours. Don't strand the
                    // call forever and don't double-write — hand back.
                    return WaitOutcome::OrchestratorBusy;
                }
                // Otherwise (busy within the ceiling, or alive within the claim
                // window) keep waiting for the orchestrator to pick this task up.
            }
            tokio::time::sleep(self.cfg.tick).await;
        }
    }

    /// Write the recovery STALL tombstone for an id we just WON via `try_own`,
    /// retrying once on a transient failure. Correctness does not depend on the
    /// line landing (the marker already blocks re-claiming); the retry only
    /// shrinks the window where the id reads as an OPEN claim — which would
    /// make later dispatches see a phantom "busy" — and the residual
    /// (persistent write failure) is logged loudly.
    fn tombstone(&self, id: &str, reason: &str) {
        for attempt in 1..=2 {
            match self.inbox.mark_stall(id, reason) {
                Ok(()) => return,
                Err(e) if attempt == 1 => {
                    eprintln!("aura-engine: inbox stall write failed for {id} (retrying): {e}");
                }
                Err(e) => {
                    eprintln!(
                        "aura-engine: inbox stall write failed for {id} twice ({e}); the open \
                         marker may read as a busy orchestrator until the call ends"
                    );
                }
            }
        }
    }
}

#[async_trait]
impl AgentRuntime for OrchestratedRuntime {
    async fn status(&self) -> AgentStatus {
        // Report an in-flight inbox dispatch: the inner runtime never saw it, so
        // forwarding blindly would claim `idle` while a task runs. With
        // overlapping dispatches the most recent one is reported.
        if let Some(intent) = self.latest_active() {
            return AgentStatus {
                state: "working".to_owned(),
                active_task: Some(intent.clone()),
                summary: format!("Live chat session is handling: {intent}"),
            };
        }
        self.inner.status().await
    }

    async fn context(&self) -> AgentContext {
        let mut ctx = self.inner.context().await;
        if ctx.active_task.is_none() {
            if let Some(intent) = self.latest_active() {
                ctx.active_task = Some(intent);
            }
        }
        ctx
    }

    async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
        // Guard: dispatch directly (zero added latency) ONLY when there is neither
        // a live watch-loop nor an open claim. If the orchestrator holds an open
        // claim it is busy mid-task; routing through the inbox lets us serialize
        // behind it instead of spawning a concurrent, colliding direct writer.
        let alive = self.inbox.orchestrator_alive(self.cfg.heartbeat_max_age);
        let busy = self.inbox.has_open_claim_other_than("");
        if !alive && !busy {
            return self.inner.start_task(envelope).await;
        }
        let (key, id) = self.next_id();
        let task = InboxTask {
            id: id.clone(),
            epoch: now_epoch(),
            user_intent: envelope.user_intent.clone(),
            constraints: envelope.constraints.clone(),
            project: envelope.project.clone(),
        };
        // If the inbox is unwritable, fall back rather than drop the task.
        if self.inbox.post_task(&task).is_err() {
            return self.inner.start_task(envelope).await;
        }
        // Track this dispatch as active for the duration of the wait; the
        // keyed guard removes exactly this entry on every return path, so
        // overlapping dispatches never clobber each other's status.
        if let Ok(mut entries) = self.active.lock() {
            entries.push((key, envelope.user_intent.clone()));
        }
        let _active = ActiveGuard {
            slot: &self.active,
            key,
        };
        match self.await_result(&id).await {
            // A genuine completion from the live session — speak it back.
            WaitOutcome::Done(res) => TaskResult {
                task_id: id,
                handoff_state: TaskHandoffState::Accepted,
                speech_update: res.speech,
                envelope,
            },
            // Never claimed / explicitly handed back → run it directly. The id is
            // owned+tombstoned by us, so no orchestrator worker is in flight to
            // collide with.
            WaitOutcome::DispatchDirectly => self.inner.start_task(envelope).await,
            // Claimed/busy but overran — the live session may still be editing the
            // repo. Do NOT dispatch a second (mutating) worker; hand back a
            // non-executing result. The speech is honest: it does NOT claim the
            // task finished, and points the user at the chat.
            WaitOutcome::OrchestratorBusy => TaskResult {
                task_id: id,
                handoff_state: TaskHandoffState::EnvelopePrepared,
                speech_update: "That one's still working in your live chat session — it's \
                                taking longer than a call turn, so it'll post the result \
                                in the chat when it lands."
                    .to_owned(),
                envelope,
            },
        }
    }

    async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
        // An inbox dispatch (`vt-*`) runs inside the live chat session, not the
        // inner runtime — the inner never saw this id, so forwarding would
        // fabricate a success ack for a task it cannot touch. Be honest: we can't
        // force-stop the session's work from the call.
        if task_id.starts_with("vt-") {
            return CancelAck {
                task_id: task_id.to_owned(),
                mode,
                speech_update: "That task is running in your live chat session — I can't \
                                stop it from the call. Tell the session to halt in the chat \
                                if you need it stopped."
                    .to_owned(),
            };
        }
        self.inner.pause_or_cancel(task_id, mode).await
    }

    async fn request_attention(&self, request: AttentionRequest) -> AttentionAck {
        self.inner.request_attention(request).await
    }

    fn checkpoint_stream(&self) -> Option<mpsc::UnboundedReceiver<CheckpointEvent>> {
        self.inner.checkpoint_stream()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// A stand-in runtime that records whether the direct fallback ran and
    /// returns a distinguishable result.
    struct RecordingRuntime {
        fell_back: Arc<Mutex<bool>>,
    }

    #[async_trait]
    impl AgentRuntime for RecordingRuntime {
        async fn status(&self) -> AgentStatus {
            AgentStatus {
                state: "idle".to_owned(),
                active_task: None,
                summary: String::new(),
            }
        }
        async fn context(&self) -> AgentContext {
            AgentContext {
                project: String::new(),
                active_task: None,
                speech_briefing: String::new(),
                recent_changes: Vec::new(),
            }
        }
        async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
            *self.fell_back.lock().unwrap() = true;
            TaskResult {
                task_id: "direct".to_owned(),
                handoff_state: TaskHandoffState::Accepted,
                speech_update: "ran directly".to_owned(),
                envelope,
            }
        }
        async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
            CancelAck {
                task_id: task_id.to_owned(),
                mode,
                speech_update: String::new(),
            }
        }
        async fn request_attention(&self, _request: AttentionRequest) -> AttentionAck {
            AttentionAck {
                speech_update: String::new(),
                requires_ack: false,
            }
        }
    }

    fn envelope() -> TaskEnvelope {
        TaskEnvelope::new(
            "do the thing",
            vec![],
            "aura",
            aura_core::CallbackMode::default(),
            String::new(),
        )
    }

    fn fast_cfg() -> OrchestratorConfig {
        OrchestratorConfig {
            heartbeat_max_age: Duration::from_secs(30),
            // Generous claim window so a background "orchestrator" sim reliably
            // wins the claim before aura recovers the task, even on a loaded CI
            // box. Tests that WANT the timeout to fire override this to a short
            // value (there is no competing claimant there, so it stays fast).
            claim_timeout: Duration::from_secs(2),
            work_timeout: Duration::from_secs(5),
            tick: Duration::from_millis(20),
        }
    }

    #[tokio::test]
    async fn dispatches_directly_without_a_heartbeat() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        let fell_back = Arc::new(Mutex::new(false));
        let runtime = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: fell_back.clone(),
            }),
            inbox,
            fast_cfg(),
        );
        // No heartbeat → the inner runtime runs immediately.
        let result = runtime.start_task(envelope()).await;
        assert_eq!(result.speech_update, "ran directly");
        assert!(*fell_back.lock().unwrap());
    }

    #[tokio::test]
    async fn falls_back_and_tombstones_when_alive_but_task_never_claimed() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.touch_heartbeat().unwrap(); // alive, but nothing consumes tasks
        let fell_back = Arc::new(Mutex::new(false));
        let runtime = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: fell_back.clone(),
            }),
            inbox.clone(),
            // Short claim window: nothing competes to claim, so the timeout fires
            // deterministically and fast.
            OrchestratorConfig {
                claim_timeout: Duration::from_millis(200),
                ..fast_cfg()
            },
        );
        let result = runtime.start_task(envelope()).await;
        assert_eq!(result.speech_update, "ran directly");
        assert!(*fell_back.lock().unwrap());
        // The task MUST be owned+tombstoned so a slow orchestrator loop cannot
        // later re-claim and re-run it (the double-execution bug). No pending.
        assert!(
            inbox.pending_tasks().is_empty(),
            "an un-claimed, fallen-back task must not stay pending"
        );
    }

    #[tokio::test]
    async fn returns_orchestrator_result_when_it_completes() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.touch_heartbeat().unwrap();
        let fell_back = Arc::new(Mutex::new(false));
        let runtime = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: fell_back.clone(),
            }),
            inbox.clone(),
            fast_cfg(),
        );
        // Simulate the live orchestrator: claim + finish the (only) posted task
        // shortly after it is posted.
        let worker = inbox.clone();
        let sim = tokio::spawn(async move {
            for _ in 0..100 {
                if let Some(task) = worker.pending_tasks().into_iter().next() {
                    worker.claim(&task.id).unwrap();
                    worker
                        .mark_done(&task.id, "orchestrator handled it")
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let result = runtime.start_task(envelope()).await;
        sim.await.unwrap();
        assert_eq!(result.speech_update, "orchestrator handled it");
        assert!(!*fell_back.lock().unwrap(), "should not have fallen back");
    }

    #[tokio::test]
    async fn hands_back_without_a_second_writer_when_claimed_task_overruns() {
        // The orchestrator CLAIMED the task but overran work_timeout — it may
        // still be editing the repo, so aura must NOT spawn a second (mutating)
        // worker. It returns a non-executing hand-off instead of falling back.
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.touch_heartbeat().unwrap();
        let fell_back = Arc::new(Mutex::new(false));
        let cfg = OrchestratorConfig {
            work_timeout: Duration::from_millis(200),
            ..fast_cfg()
        };
        let runtime = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: fell_back.clone(),
            }),
            inbox.clone(),
            cfg,
        );
        // Claim the task but never finish it.
        let worker = inbox.clone();
        let sim = tokio::spawn(async move {
            for _ in 0..100 {
                if let Some(task) = worker.pending_tasks().into_iter().next() {
                    worker.claim(&task.id).unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let result = runtime.start_task(envelope()).await;
        sim.await.unwrap();
        assert!(
            !*fell_back.lock().unwrap(),
            "must NOT run a second direct writer while the orchestrator holds the claim"
        );
        assert_eq!(result.handoff_state, TaskHandoffState::EnvelopePrepared);
        assert!(result.speech_update.contains("still working"));
    }

    #[tokio::test]
    async fn cancel_of_an_inbox_task_is_honest_not_a_fabricated_ack() {
        // A `vt-*` task runs in the live chat session; the inner runtime never saw
        // it, so cancel must be honest rather than forward a canned success.
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        let runtime = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: Arc::new(Mutex::new(false)),
            }),
            inbox,
            fast_cfg(),
        );
        let ack = runtime
            .pause_or_cancel("vt-deadbeef-1-1", CancelMode::Cancel)
            .await;
        assert!(
            ack.speech_update.to_lowercase().contains("chat session"),
            "cancel ack must point the user at the chat, got: {}",
            ack.speech_update
        );
        // A non-inbox id still forwards to the inner runtime unchanged.
        let inner_ack = runtime
            .pause_or_cancel("direct-42", CancelMode::Cancel)
            .await;
        assert_eq!(inner_ack.task_id, "direct-42");
    }

    #[test]
    fn task_ids_carry_the_pid_and_are_unique() {
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        let rt = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: Arc::new(Mutex::new(false)),
            }),
            inbox,
            fast_cfg(),
        );
        let (ka, a) = rt.next_id();
        let (kb, b) = rt.next_id();
        assert_ne!(a, b, "ids from one instance are distinct (seq)");
        assert_ne!(ka, kb, "tracking keys are distinct");
        let pid_hex = format!("{:x}", std::process::id());
        assert!(a.contains(&pid_hex), "id must embed the pid: {a}");
    }

    #[tokio::test]
    async fn overrun_keeps_the_claim_open_no_stall_tombstone() {
        // Regression (second-pass review): a work-timeout STALL on a CLAIMED
        // task would put it into the terminal set, erasing the open-claim
        // "busy" signal — the next dispatch would then run a direct cold
        // worker concurrently with the still-working live session. The claim
        // must stay open after the busy hand-back.
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.touch_heartbeat().unwrap();
        let cfg = OrchestratorConfig {
            work_timeout: Duration::from_millis(200),
            ..fast_cfg()
        };
        let runtime = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: Arc::new(Mutex::new(false)),
            }),
            inbox.clone(),
            cfg,
        );
        let worker = inbox.clone();
        let sim = tokio::spawn(async move {
            for _ in 0..100 {
                if let Some(task) = worker.pending_tasks().into_iter().next() {
                    assert!(worker.claim(&task.id).unwrap(), "sim wins the claim");
                    return task.id;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("task never appeared");
        });
        let result = runtime.start_task(envelope()).await;
        let id = sim.await.unwrap();
        assert_eq!(result.handoff_state, TaskHandoffState::EnvelopePrepared);
        assert_eq!(
            inbox.result_for(&id),
            None,
            "no STALL tombstone may be written for a claimed-overrun task"
        );
        assert!(inbox.is_claimed(&id), "the claim marker stays open");
        assert!(
            inbox.has_open_claim_other_than("someone-else"),
            "the open claim must keep reading as busy for later dispatches"
        );
    }

    #[tokio::test]
    async fn overlapping_dispatches_keep_status_working_until_both_done() {
        // Regression (second-pass review): a single `active` slot was clobbered
        // by concurrent dispatches — the short dispatch's completion cleared
        // the flag while the long one was still in flight.
        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.touch_heartbeat().unwrap();
        let runtime = Arc::new(OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: Arc::new(Mutex::new(false)),
            }),
            inbox.clone(),
            OrchestratorConfig {
                work_timeout: Duration::from_secs(10),
                ..fast_cfg()
            },
        ));
        // Dispatch A: claimed but not finished until the very end.
        let rt_a = runtime.clone();
        let a = tokio::spawn(async move { rt_a.start_task(envelope()).await });
        // Wait until A is posted + claim it (so it sits in the claimed wait).
        let id_a = loop {
            if let Some(task) = inbox.pending_tasks().into_iter().next() {
                assert!(inbox.claim(&task.id).unwrap());
                break task.id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        // Dispatch B: completes quickly.
        let rt_b = runtime.clone();
        let b = tokio::spawn(async move { rt_b.start_task(envelope()).await });
        let id_b = loop {
            if let Some(task) = inbox.pending_tasks().into_iter().next() {
                assert!(inbox.claim(&task.id).unwrap());
                break task.id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        inbox.mark_done(&id_b, "b finished").unwrap();
        let result_b = b.await.unwrap();
        assert_eq!(result_b.speech_update, "b finished");
        // B is done, A is still in flight: status must still be `working`.
        let status = runtime.status().await;
        assert_eq!(
            status.state, "working",
            "finishing dispatch B must not erase dispatch A's active status"
        );
        // Finish A; status returns to the inner runtime's (idle).
        inbox.mark_done(&id_a, "a finished").unwrap();
        let result_a = a.await.unwrap();
        assert_eq!(result_a.speech_update, "a finished");
        assert_eq!(runtime.status().await.state, "idle");
    }

    /// Full dispatch-chain e2e: a `start_agent_task` tool call flows through the
    /// REAL `ToolRouter` voice-approval boundary → the REAL `OrchestratedRuntime`
    /// → the REAL on-disk `Inbox`; a background task plays the live orchestrator
    /// (`claim` + `done`, refreshing the heartbeat exactly like `aura-inbox wait`);
    /// the spoken-back result IS the orchestrator's text and the direct fallback
    /// never runs. This is the Scheme 2 path a live LOCAL call exercises, minus
    /// only the xAI voice/audio leg.
    #[tokio::test]
    async fn end_to_end_dispatch_through_router_reaches_live_orchestrator() {
        use aura_core::config::SafetyConfig;
        use aura_core::tools::{ToolCall, ToolRouter};
        use aura_core::CallbackMode;

        let tmp = tempdir().unwrap();
        let inbox = Inbox::open(tmp.path()).unwrap();
        inbox.touch_heartbeat().unwrap(); // arm the orchestrator before dispatch

        let fell_back = Arc::new(Mutex::new(false));
        let orchestrated = OrchestratedRuntime::new(
            Arc::new(RecordingRuntime {
                fell_back: fell_back.clone(),
            }),
            inbox.clone(),
            OrchestratorConfig {
                work_timeout: Duration::from_secs(10),
                ..fast_cfg()
            },
        );
        let router = ToolRouter::with_safety(
            Arc::new(orchestrated),
            CallbackMode::SpeakImmediately,
            SafetyConfig::default(),
        );

        // Background live orchestrator: drain the inbox and report done, keeping
        // the heartbeat fresh — exactly what the skill's `aura-inbox` loop does.
        let worker = inbox.clone();
        let drainer = tokio::spawn(async move {
            for _ in 0..200 {
                let _ = worker.touch_heartbeat();
                if let Some(task) = worker.pending_tasks().into_iter().next() {
                    worker.claim(&task.id).unwrap();
                    worker
                        .mark_done(
                            &task.id,
                            &format!("orchestrator handled it: {}", task.user_intent),
                        )
                        .unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        // Mint the voice-approval token exactly as the engine's dispatch path does.
        let intent = "update the config";
        let token = router.issue_task_approval(intent).unwrap();
        let resp = router
            .handle(ToolCall {
                name: "start_agent_task".to_owned(),
                arguments: serde_json::json!({
                    "user_intent": intent,
                    "_local_voice_approval_id": token,
                }),
            })
            .await
            .expect("dispatch succeeds");
        drainer.await.unwrap();

        assert!(
            resp.speech.contains("orchestrator handled it"),
            "spoken result should be the orchestrator's DONE text, got: {}",
            resp.speech
        );
        assert!(
            !*fell_back.lock().unwrap(),
            "must not have used the direct fallback"
        );
    }
}
