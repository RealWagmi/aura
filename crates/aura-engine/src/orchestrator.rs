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
use std::sync::Arc;
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
    /// If the orchestrator does not `CLAIM` a posted task within this, treat it
    /// as unavailable and dispatch directly.
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
    /// The orchestrator CLAIMED the task but did not finish within `work_timeout`.
    /// It may still be mutating the repo, so we must NOT spawn a second worker —
    /// hand back a non-executing result instead.
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
        }
    }

    /// A globally-unique task id (`vt-<epoch>-<pid>-<n>`).
    fn next_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        format!("vt-{:x}-{:x}-{n}", self.id_prefix, self.pid)
    }

    /// Block until the orchestrator reports a terminal result for `id`, or a
    /// timeout fires. See [`WaitOutcome`] for the dispositions. Both timeout
    /// paths write a `STALL` tombstone so the task drops out of the inbox's
    /// pending set and a slow orchestrator loop cannot later re-claim and re-run
    /// a task the caller has already moved past.
    async fn await_result(&self, id: &str) -> WaitOutcome {
        let start = tokio::time::Instant::now();
        let mut claimed = false;
        loop {
            if let Some(res) = self.inbox.result_for(id) {
                // A DONE is a completion; a STALL here can only be the
                // orchestrator's own explicit hand-back (our tombstones return
                // before looping) → safe to dispatch directly.
                return if res.is_done() {
                    WaitOutcome::Done(res)
                } else {
                    WaitOutcome::DispatchDirectly
                };
            }
            if !claimed {
                if self.inbox.is_claimed(id) {
                    claimed = true;
                } else if start.elapsed() >= self.cfg.claim_timeout {
                    // Never picked up. Tombstone it so a slow orchestrator loop
                    // that later runs `aura-inbox wait` cannot re-claim and
                    // RE-RUN a task we are about to dispatch directly.
                    let _ = self.inbox.mark_stall(id, "not claimed in time");
                    return WaitOutcome::DispatchDirectly;
                }
            }
            if claimed && start.elapsed() >= self.cfg.work_timeout {
                // Claimed but overran. The live session may STILL be mutating the
                // repo, so we must not spawn a second writer — tombstone and hand
                // back a non-executing result.
                let _ = self.inbox.mark_stall(id, "orchestrator work timed out");
                return WaitOutcome::OrchestratorBusy;
            }
            tokio::time::sleep(self.cfg.tick).await;
        }
    }
}

#[async_trait]
impl AgentRuntime for OrchestratedRuntime {
    async fn status(&self) -> AgentStatus {
        self.inner.status().await
    }

    async fn context(&self) -> AgentContext {
        self.inner.context().await
    }

    async fn start_task(&self, envelope: TaskEnvelope) -> TaskResult {
        // Guard: no live orchestrator loop → dispatch directly, no added latency.
        if !self.inbox.orchestrator_alive(self.cfg.heartbeat_max_age) {
            return self.inner.start_task(envelope).await;
        }
        let id = self.next_id();
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
        match self.await_result(&id).await {
            // A genuine completion from the live session — speak it back.
            WaitOutcome::Done(res) => TaskResult {
                task_id: id,
                handoff_state: TaskHandoffState::Accepted,
                speech_update: res.speech,
                envelope,
            },
            // Never claimed / explicitly handed back → run it directly. The id is
            // tombstoned, so no orchestrator worker is in flight to collide with.
            WaitOutcome::DispatchDirectly => self.inner.start_task(envelope).await,
            // Claimed but overran — the live session may still be editing the
            // repo. Do NOT dispatch a second (mutating) worker; hand back a
            // non-executing result and let the live session finish and report in
            // its own chat.
            WaitOutcome::OrchestratorBusy => TaskResult {
                task_id: id,
                handoff_state: TaskHandoffState::EnvelopePrepared,
                speech_update: "Your live session is still working on that one — \
                                I'll let it finish; it'll report back in the chat."
                    .to_owned(),
                envelope,
            },
        }
    }

    async fn pause_or_cancel(&self, task_id: &str, mode: CancelMode) -> CancelAck {
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
            claim_timeout: Duration::from_millis(300),
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
            fast_cfg(),
        );
        let result = runtime.start_task(envelope()).await;
        assert_eq!(result.speech_update, "ran directly");
        assert!(*fell_back.lock().unwrap());
        // The task MUST be tombstoned (STALL) so a slow orchestrator loop cannot
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
        let a = rt.next_id();
        let b = rt.next_id();
        assert_ne!(a, b, "ids from one instance are distinct (seq)");
        let pid_hex = format!("{:x}", std::process::id());
        assert!(a.contains(&pid_hex), "id must embed the pid: {a}");
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
