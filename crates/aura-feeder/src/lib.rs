//! `aura-feeder` — live ambient context fed into the call as it runs.
//!
//! Produces digests of the host chat the engine injects mid-call via
//! `VoiceSink::inject_system_context` (without triggering a response), plus
//! a cold-start opener so the model can greet with awareness of the chat.
//! Aligned with `aura_core::brief::Brief` so the two never double-budget
//! the instruction window.
//!
//! Pieces: [`tail_events`] (file follower over `.aura/history.jsonl`),
//! [`digest::ClaudeSubagent`] (subprocess), [`run_digest_cycle`] (window +
//! tick + emit). The [`Feeder`] facade wires the three together and
//! implements [`aura_engine::AmbientFeeder`] so the engine can pull rendered
//! digests as the call runs.

pub mod digest;
pub mod opener_branch;
pub mod topic_candidate;

pub use digest::{
    build_user_message, parse_digest_response, render_digest_for_inject, render_events_for_prompt,
    AnticipatedQA, ClaudeSubagent, Digest, DigestError, SubagentConfig, DIGEST_SYSTEM_PROMPT,
};
pub use opener_branch::{select_opener_branch, OpenerBranch, OpenerBranchInputs};
pub use topic_candidate::TopicCandidate;

use std::path::{Path, PathBuf};
use std::time::Duration;

// The feeder tails the on-disk history written by
// `aura_core::history::append_history_event`, so it MUST reason about the
// exact same shape: `aura_core::HistoryEvent { timestamp_ms: u128, kind,
// speech }`. Re-export it here so downstream code reads a single canonical
// type and the tailer's serde parse matches the writer byte-for-byte.
pub use aura_core::HistoryEvent;

use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep};

/// Default digest model for [`Feeder::start_for_call`] — a Claude Code CLI
/// alias (billed against the user's subscription, not the metered API), the
/// fast tier the digest schema was tuned for.
const DEFAULT_DIGEST_MODEL: &str = "sonnet";
/// Digests buffered between the cycle and the engine pull side in
/// [`Feeder::start_for_call`].
const DEFAULT_DIGESTS_BUFFER: usize = 16;

/// Tunables for the feeder.
#[derive(Debug, Clone)]
pub struct FeederConfig {
    /// Where to read events from. Defaults to `.aura/history.jsonl`.
    pub history_path: PathBuf,
    /// How often to poll the file for new bytes. Voice-turn cadence is
    /// ~3-8s, so 250ms is plenty fast and keeps idle CPU near zero.
    pub poll_interval: Duration,
    /// Channel buffer for parsed events.
    pub channel_buffer: usize,
}

impl Default for FeederConfig {
    fn default() -> Self {
        Self {
            history_path: PathBuf::from(".aura/history.jsonl"),
            poll_interval: Duration::from_millis(250),
            channel_buffer: 64,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FeederError {
    #[error("history file io: {0}")]
    Io(#[from] std::io::Error),
    /// The `claude` subagent could not be spawned (e.g. the binary is not
    /// on `PATH`). The binary wiring surfaces this and passes `None` to
    /// the engine rather than failing the whole call.
    #[error("subagent spawn: {0}")]
    Subagent(#[from] DigestError),
}

/// Spawn a tail-follower on `path`. Each appended JSONL line is parsed
/// into a [`HistoryEvent`] and pushed into the returned receiver.
///
/// Lines that fail to parse are logged at WARN and skipped; the tailer
/// keeps running. The follower task exits when the receiver is dropped.
pub fn tail_events(config: FeederConfig) -> Result<mpsc::Receiver<HistoryEvent>, FeederError> {
    let (tx, rx) = mpsc::channel(config.channel_buffer);
    let path = config.history_path;
    let interval = config.poll_interval;

    tokio::spawn(async move {
        if let Err(err) = follow(&path, interval, tx).await {
            tracing::warn!("context-feeder tailer exited: {err}");
        }
    });

    Ok(rx)
}

async fn follow(
    path: &Path,
    interval: Duration,
    tx: mpsc::Sender<HistoryEvent>,
) -> Result<(), FeederError> {
    // Block until the file appears — the voice loop creates it lazily.
    loop {
        if path.exists() {
            break;
        }
        sleep(interval).await;
        if tx.is_closed() {
            return Ok(());
        }
    }

    let file = File::open(path).await?;
    // Start at end-of-file: we only care about events that happen
    // *during* the live session, not historical replay.
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::End(0)).await?;
    let mut current_inode = inode_of(path).await;

    let mut buf = String::new();
    loop {
        if tx.is_closed() {
            return Ok(());
        }

        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            // EOF — but the writer might have rotated or truncated
            // the file underneath us. Compare current stream position
            // against on-disk size + (on unix) inode. If the file
            // shrank or the inode changed, re-open from the start.
            if let Some(new_reader) = reopen_if_rotated(path, &mut reader, &mut current_inode).await
            {
                reader = new_reader;
                continue;
            }
            sleep(interval).await;
            continue;
        }

        let line = buf.trim();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<HistoryEvent>(line) {
            Ok(event) => {
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
            Err(err) => {
                tracing::warn!(target: "aura_feeder", "skip unparseable line: {err}");
            }
        }
    }
}

/// Inode of `path`, if obtainable on this platform. None on non-unix
/// or if the path doesn't exist (in which case the size-only check
/// still catches truncation).
async fn inode_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        tokio::fs::metadata(path).await.ok().map(|m| m.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// If the on-disk file shrank (truncate) or its inode changed
/// (move-then-create rotation), open a fresh reader from offset 0 and
/// return it. Otherwise return None so the caller keeps the existing
/// reader and just sleeps.
///
/// Uses the current stream position as the "expected size lower
/// bound" — anything smaller means the writer truncated. Inode change
/// is the cleaner signal on unix; we fall back to size-only on
/// platforms without inode metadata.
async fn reopen_if_rotated(
    path: &Path,
    reader: &mut BufReader<File>,
    current_inode: &mut Option<u64>,
) -> Option<BufReader<File>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let on_disk_size = metadata.len();
    let stream_pos = reader.stream_position().await.ok()?;
    let new_inode = inode_of(path).await;

    let truncated = on_disk_size < stream_pos;
    let inode_changed = matches!(
        (*current_inode, new_inode),
        (Some(old), Some(new)) if old != new
    );

    if !truncated && !inode_changed {
        return None;
    }

    let file = File::open(path).await.ok()?;
    let new_reader = BufReader::new(file);
    *current_inode = new_inode;
    tracing::info!(
        target: "aura_feeder",
        on_disk_size,
        stream_pos,
        truncated,
        inode_changed,
        "history file rotated; re-opening from start"
    );
    Some(new_reader)
}

/// Tunables for the digest cycle runner.
#[derive(Debug, Clone)]
pub struct CycleConfig {
    /// How often we ask the subagent for a fresh digest. Kept at 3s:
    /// Sonnet plays the back-room senior engineer role and needs to see
    /// fresh voice transcripts within a few seconds to be useful as a
    /// context oracle, not a 10-second-late summarizer. Sonnet 4.6 with
    /// prompt caching (the prefill arrives on the cached system-prompt
    /// prefix) keeps per-tick cost manageable at the faster cadence.
    pub interval: Duration,
    /// Max events held in the rolling window. Older events fall off
    /// the front. 30 keeps us inside Sonnet's input budget while still
    /// covering ~1-2 minutes of voice.
    pub window_size: usize,
}

impl Default for CycleConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3),
            window_size: 30,
        }
    }
}

/// Handle that lets the consumer signal the digest cycle to shut
/// down promptly — including pre-empting an in-flight `next_digest`
/// call so the Claude subprocess receives SIGKILL right away (via
/// `kill_on_drop` when the subagent value drops). Pre-fix the consumer
/// only had `JoinHandle::abort()` which yields at the next `.await`,
/// leaving the subprocess alive until its current read_line completes
/// — burning one full digest worth of quota on every shutdown.
pub struct Shutdown(tokio::sync::oneshot::Sender<()>);

impl Shutdown {
    /// Signal the cycle to exit. No-op if the cycle already exited
    /// (receiver dropped); the consumer doesn't need to know.
    pub fn cancel(self) {
        let _ = self.0.send(());
    }
}

/// Spawn the digest cycle. Reads events from `events_rx`, maintains a
/// rolling window, and on each tick (every `cycle.interval`) asks the
/// long-running `subagent` for a fresh digest. Non-empty digests are
/// pushed onto `digests_tx` for the injector to consume.
///
/// Empty windows are skipped — no point burning subagent quota on a
/// "nothing happened" cycle. Subagent errors are WARN-logged and the
/// runner moves on; the next tick gets a fresh chance.
///
/// The cycle exits promptly when `Shutdown::cancel()` fires — even
/// mid-`next_digest` — by dropping the subagent (whose `kill_on_drop`
/// SIGKILLs the subprocess immediately). It also exits if the subagent
/// dies, `events_rx` closes, or `digests_tx` is dropped.
pub fn run_digest_cycle(
    mut subagent: ClaudeSubagent,
    cycle: CycleConfig,
    mut events_rx: mpsc::Receiver<HistoryEvent>,
    digests_tx: mpsc::Sender<Digest>,
) -> (JoinHandle<()>, Shutdown) {
    // Clamp window_size to >= 1. With 0, the `len() >= window_size`
    // predicate below is true on the empty deque, the pop_front is a
    // no-op, and the deque grows unbounded for the whole call.
    let window_size = cycle.window_size.max(1);
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let mut window: std::collections::VecDeque<HistoryEvent> =
            std::collections::VecDeque::with_capacity(window_size);
        let mut tick = interval(cycle.interval);
        tick.tick().await;
        let mut cancel_rx = Box::pin(cancel_rx);

        loop {
            tokio::select! {
                biased;
                // Cancel arm comes first so a queued cancel pre-empts
                // any other ready future deterministically.
                _ = &mut cancel_rx => {
                    tracing::info!(target: "aura_feeder", "cycle cancelled by caller; subagent dropping (SIGKILL via kill_on_drop)");
                    drop(subagent);
                    return;
                }
                maybe_event = events_rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            if window.len() >= window_size {
                                window.pop_front();
                            }
                            window.push_back(event);
                        }
                        None => {
                            tracing::info!(target: "aura_feeder", "events channel closed; cycle exiting");
                            return;
                        }
                    }
                }
                _ = tick.tick() => {
                    if window.is_empty() {
                        continue;
                    }
                    if subagent.is_dead() {
                        tracing::warn!(target: "aura_feeder", "subagent process is dead; cycle exiting");
                        return;
                    }
                    let snapshot: Vec<HistoryEvent> = window.iter().cloned().collect();
                    // Wrap next_digest in a select so the cancel signal
                    // pre-empts the read_line await (which can sit at the
                    // configured read_timeout = 60s default). Without this
                    // inner select, abort-equivalent behavior would wait
                    // for the current digest to finish.
                    let result = tokio::select! {
                        biased;
                        _ = &mut cancel_rx => {
                            tracing::info!(target: "aura_feeder", "cycle cancelled mid-digest; subagent dropping");
                            drop(subagent);
                            return;
                        }
                        r = subagent.next_digest(&snapshot) => r,
                    };
                    match result {
                        Ok(digest) => {
                            if digest.is_empty() {
                                tracing::debug!(target: "aura_feeder", "digest empty; skip injection");
                                continue;
                            }
                            if digests_tx.send(digest).await.is_err() {
                                tracing::info!(target: "aura_feeder", "digest consumer gone; cycle exiting");
                                return;
                            }
                        }
                        Err(err) => {
                            tracing::warn!(target: "aura_feeder", "digest cycle skipped: {err}");
                            if subagent.is_dead() {
                                tracing::warn!(target: "aura_feeder", "subagent process died after error; cycle exiting now");
                                return;
                            }
                        }
                    }
                }
            }
        }
    });
    (handle, Shutdown(cancel_tx))
}

/// Live ambient-context feeder facade. Owns the tailer + digest cycle and
/// hands rendered digests to the engine via [`aura_engine::AmbientFeeder`].
///
/// Construct with [`Feeder::start`], which wires `tail_events` ->
/// `ClaudeSubagent` -> `run_digest_cycle` and keeps the digests receiver.
/// On drop the cycle's [`Shutdown`] fires so the Claude subprocess is
/// SIGKILLed promptly (no lingering quota burn).
pub struct Feeder {
    rx: tokio::sync::Mutex<mpsc::Receiver<Digest>>,
    _handle: JoinHandle<()>,
    shutdown: Option<Shutdown>,
}

impl Feeder {
    /// Wire the full feeder pipeline and start producing digests.
    ///
    /// `feeder` configures the file tailer, `subagent` the `claude`
    /// subprocess, and `cycle` the digest cadence + window. The
    /// `digests_buffer` bounds the channel between the cycle and the
    /// engine pull side (clamped to at least 1).
    ///
    /// Returns `Err` if the `claude` subagent cannot be spawned (e.g. the
    /// binary is not on `PATH`). The binary wiring surfaces that and
    /// passes `None` to the engine instead of panicking — ambient context
    /// is best-effort, the call still proceeds without it.
    pub async fn start(
        feeder: FeederConfig,
        subagent: SubagentConfig,
        cycle: CycleConfig,
        digests_buffer: usize,
    ) -> Result<Self, FeederError> {
        let events_rx = tail_events(feeder)?;
        let subagent = ClaudeSubagent::spawn(&subagent).await?;
        let (digests_tx, digests_rx) = mpsc::channel(digests_buffer.max(1));
        let (handle, shutdown) = run_digest_cycle(subagent, cycle, events_rx, digests_tx);
        Ok(Self {
            rx: tokio::sync::Mutex::new(digests_rx),
            _handle: handle,
            shutdown: Some(shutdown),
        })
    }

    /// Batteries-included constructor for the binaries (LOCAL `aura-cli` /
    /// REMOTE `aura-server`): tail `<cwd>/.aura/history.jsonl`, run the digest
    /// subagent with the standard flags and no MCP servers, at the default
    /// cadence/window.
    ///
    /// The digest subagent is a `claude` CLI process billed against the user's
    /// Claude Code subscription (not the metered API), running the
    /// [`DIGEST_SYSTEM_PROMPT`] on the fast tier the schema was tuned for.
    /// A minimal `<cwd>/.aura/feeder-mcp.json` (`{"mcpServers":{}}`)
    /// is written so `--strict-mcp-config` has a valid empty config — the
    /// digest needs no tools (`--tools ""` already disables the built-ins).
    ///
    /// Returns `Err` if the `.aura` dir can't be created or the `claude` binary
    /// is not spawnable; the caller degrades to `None` (the call proceeds with
    /// the startup brief but no live ambient deltas).
    pub async fn start_for_call(cwd: &Path) -> Result<Self, FeederError> {
        let aura_dir = cwd.join(".aura");
        std::fs::create_dir_all(&aura_dir)?;
        let mcp_path = aura_dir.join("feeder-mcp.json");
        std::fs::write(&mcp_path, r#"{"mcpServers":{}}"#)?;
        let feeder = FeederConfig {
            history_path: aura_dir.join("history.jsonl"),
            ..FeederConfig::default()
        };
        let subagent = SubagentConfig {
            extra_args: ClaudeSubagent::standard_args(
                DEFAULT_DIGEST_MODEL,
                DIGEST_SYSTEM_PROMPT,
                &mcp_path.to_string_lossy(),
            ),
            ..SubagentConfig::default()
        };
        Feeder::start(
            feeder,
            subagent,
            CycleConfig::default(),
            DEFAULT_DIGESTS_BUFFER,
        )
        .await
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        // Fire the cancel so the cycle drops its subagent (kill_on_drop
        // SIGKILLs the subprocess) instead of lingering until the next
        // read_line completes.
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.cancel();
        }
    }
}

#[async_trait::async_trait]
impl aura_engine::AmbientFeeder for Feeder {
    async fn next_digest(&self) -> Option<String> {
        loop {
            // recv() yields None when the cycle closed the channel
            // (subagent died / cancelled / consumer gone) -> propagate
            // None so the engine stops pulling. Use the async Mutex
            // because recv is async and the trait takes `&self`.
            let digest = self.rx.lock().await.recv().await?;
            if !digest.is_empty() {
                return Some(render_digest_for_inject(&digest));
            }
            // Defensive: the cycle already skips empty digests, but if one
            // slips through we loop rather than inject a "(no new context)"
            // sentinel.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_engine::AmbientFeeder;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    fn async_cycle_timeout() -> Duration {
        Duration::from_secs(10)
    }

    /// Pin the digest-cycle cadence at 3s. The Sonnet back-room oracle
    /// role needs to see fresh voice transcripts within a few seconds —
    /// a regression to 10s would silently degrade the feeder back to
    /// "10-second-late summarizer" territory. This test fails fast if
    /// anyone bumps the default without thinking it through.
    #[test]
    fn cycle_default_interval_is_three_seconds() {
        let cfg = CycleConfig::default();
        assert_eq!(
            cfg.interval,
            Duration::from_secs(3),
            "default poll interval drifted from 3s — see Phase 3 rationale"
        );
        assert_eq!(cfg.window_size, 30);
    }

    #[test]
    fn parses_real_history_line() {
        // The shape written by aura_core::append_history_event: three
        // fields, no extra columns. The tailer reuses aura_core's struct.
        let line = r#"{"timestamp_ms":1777195807203,"kind":"assistant","speech":"Got it"}"#;
        let event: HistoryEvent = serde_json::from_str(line).unwrap();
        assert_eq!(event.timestamp_ms, 1777195807203);
        assert_eq!(event.kind, "assistant");
        assert_eq!(event.speech, "Got it");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tails_appended_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");

        // Pre-create empty file so tailer attaches immediately.
        File::create(&path).await.unwrap();

        let mut rx = tail_events(FeederConfig {
            history_path: path.clone(),
            poll_interval: Duration::from_millis(20),
            channel_buffer: 8,
        })
        .unwrap();

        // Give the follower a tick to attach.
        sleep(Duration::from_millis(60)).await;

        let mut writer = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        writer
            .write_all(b"{\"timestamp_ms\":1,\"kind\":\"user\",\"speech\":\"hello\"}\n")
            .await
            .unwrap();
        writer
            .write_all(b"{\"timestamp_ms\":2,\"kind\":\"assistant\",\"speech\":\"hi\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first event timed out")
            .expect("channel closed");
        assert_eq!(first.timestamp_ms, 1);
        assert_eq!(first.speech, "hello");

        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("second event timed out")
            .expect("channel closed");
        assert_eq!(second.kind, "assistant");
        assert_eq!(second.speech, "hi");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn skips_garbage_lines_without_dying() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        File::create(&path).await.unwrap();

        let mut rx = tail_events(FeederConfig {
            history_path: path.clone(),
            poll_interval: Duration::from_millis(20),
            channel_buffer: 8,
        })
        .unwrap();
        sleep(Duration::from_millis(60)).await;

        let mut writer = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        writer.write_all(b"this is not json\n").await.unwrap();
        writer
            .write_all(b"{\"timestamp_ms\":42,\"kind\":\"user\",\"speech\":\"after garbage\"}\n")
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event timed out")
            .expect("channel closed");
        assert_eq!(event.timestamp_ms, 42);
        assert_eq!(event.speech, "after garbage");
    }

    /// Regression: previously the tailer opened the file once and seeked
    /// to end. After a truncate (`> file`) or rotation (move-then-create),
    /// the file handle either pointed at an unlinked inode (no new data
    /// forever) or had a stream position past the new EOF. Now follow()
    /// re-stats on every EOF tick and re-opens from offset 0 when size
    /// shrinks or the inode changes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovers_from_log_rotation_via_truncate() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        File::create(&path).await.unwrap();

        let mut rx = tail_events(FeederConfig {
            history_path: path.clone(),
            poll_interval: Duration::from_millis(15),
            channel_buffer: 8,
        })
        .unwrap();
        sleep(Duration::from_millis(40)).await;

        // Phase 1: write a normal event, confirm it reaches the tailer.
        {
            let mut writer = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await
                .unwrap();
            writer
                .write_all(b"{\"timestamp_ms\":1,\"kind\":\"user\",\"speech\":\"before\"}\n")
                .await
                .unwrap();
            writer.flush().await.unwrap();
        }
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first event timed out")
            .expect("channel closed");
        assert_eq!(first.speech, "before");

        // Phase 2: truncate the file (logrotate copytruncate / `> file`).
        // Pre-fix the tailer's stream position (~bytes-of-the-line) is
        // now past the new EOF, so subsequent appends are invisible.
        tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .await
            .unwrap();
        sleep(Duration::from_millis(60)).await;

        // Phase 3: write a new event after truncation. The tailer must
        // pick it up — that's the recovery contract.
        {
            let mut writer = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await
                .unwrap();
            writer
                .write_all(b"{\"timestamp_ms\":2,\"kind\":\"user\",\"speech\":\"after\"}\n")
                .await
                .unwrap();
            writer.flush().await.unwrap();
        }
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("post-truncation event timed out — tailer didn't recover")
            .expect("channel closed");
        assert_eq!(second.speech, "after");
        assert_eq!(second.timestamp_ms, 2);
    }

    /// Spawn a stub subagent, retrying past the ETXTBSY ("text file busy")
    /// window. This is the classic multi-threaded fork+exec race: while one
    /// test writes its stub script (a brief write fd), another test's
    /// `Command::spawn` forks and the child inherits that write fd, so exec'ing
    /// the just-written script fails with `ExecutableFileBusy`. The window is
    /// sub-second, so a short bounded retry always wins. (Production spawns a
    /// real `claude` binary, never a freshly written file, so it cannot hit
    /// this.)
    async fn spawn_stub_retrying(cfg: &digest::SubagentConfig) -> ClaudeSubagent {
        for _ in 0..40 {
            match ClaudeSubagent::spawn(cfg).await {
                Ok(agent) => return agent,
                Err(digest::DigestError::Io(e))
                    if e.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                {
                    sleep(Duration::from_millis(25)).await;
                }
                Err(e) => panic!("stub subagent spawn failed: {e}"),
            }
        }
        panic!("stub subagent spawn kept hitting ETXTBSY after retries");
    }

    /// Build a stub-claude bash script that emits one valid digest per
    /// stdin line. Used by the cycle tests to keep them self-contained
    /// (no real `claude` install required).
    async fn stub_subagent() -> ClaudeSubagent {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake_claude.sh");
        // Loop forever — one user line in, one digest out. Exits when
        // stdin closes (i.e. when the subagent is dropped).
        let script = r#"#!/usr/bin/env bash
while read -r _line; do
  echo '{"type":"system","subtype":"init"}'
  echo '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"{\"recent_facts\":[\"test fact\"],\"active_topic\":\"test topic\",\"suggested_directions\":[]}"}]}}'
  echo '{"type":"result","subtype":"success","is_error":false,"result":""}'
done
"#;
        tokio::fs::write(&path, script).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&path).await.unwrap().permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(&path, perms).await.unwrap();
        }
        // Leak the tempdir so the script outlives the test (cleaned up
        // on process exit). Otherwise the script vanishes when this fn
        // returns and the spawned subprocess can't re-read it.
        std::mem::forget(dir);
        let cfg = digest::SubagentConfig {
            claude_binary: path,
            ..Default::default()
        };
        spawn_stub_retrying(&cfg).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cycle_exits_when_events_channel_closes() {
        let (events_tx, events_rx) = mpsc::channel::<HistoryEvent>(4);
        let (digests_tx, mut digests_rx) = mpsc::channel::<Digest>(4);
        let subagent = stub_subagent().await;

        let (handle, _shutdown) = run_digest_cycle(
            subagent,
            CycleConfig {
                interval: Duration::from_secs(60),
                window_size: 4,
            },
            events_rx,
            digests_tx,
        );

        // Close the events channel: cycle should observe None and exit.
        drop(events_tx);

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cycle did not exit after events channel closed")
            .expect("cycle task panicked");

        assert!(digests_rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cycle_emits_digest_when_events_present() {
        let (events_tx, events_rx) = mpsc::channel::<HistoryEvent>(4);
        let (digests_tx, mut digests_rx) = mpsc::channel::<Digest>(4);
        let subagent = stub_subagent().await;

        let (_handle, _shutdown) = run_digest_cycle(
            subagent,
            CycleConfig {
                interval: Duration::from_millis(80),
                window_size: 4,
            },
            events_rx,
            digests_tx,
        );

        events_tx
            .send(HistoryEvent {
                timestamp_ms: 1,
                kind: "user".into(),
                speech: "hello".into(),
            })
            .await
            .unwrap();

        // Wait long enough for at least one tick after first-tick burn.
        let digest = tokio::time::timeout(async_cycle_timeout(), digests_rx.recv())
            .await
            .expect("digest never emitted")
            .expect("channel closed");
        assert_eq!(digest.recent_facts, vec!["test fact"]);
        assert_eq!(digest.active_topic, "test topic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cycle_skips_when_window_empty_at_tick() {
        let (events_tx, events_rx) = mpsc::channel::<HistoryEvent>(4);
        let (digests_tx, mut digests_rx) = mpsc::channel::<Digest>(4);
        let subagent = stub_subagent().await;

        let (_handle, _shutdown) = run_digest_cycle(
            subagent,
            CycleConfig {
                interval: Duration::from_millis(50),
                window_size: 4,
            },
            events_rx,
            digests_tx,
        );

        sleep(Duration::from_millis(250)).await;

        // No event ever fed in -> no digest emitted, no panic.
        assert!(digests_rx.try_recv().is_err());

        drop(events_tx);
    }

    /// Regression: previously the consumer's only shutdown lever was
    /// `JoinHandle::abort()`, which yields at the next .await. While
    /// next_digest awaits read_line, the Claude subprocess kept
    /// running until that line arrived (or stdin closed) — burning
    /// one full digest worth of quota on every shutdown. The new
    /// `Shutdown` channel pre-empts the in-flight read via the inner
    /// tokio::select, drops the subagent (kill_on_drop fires SIGKILL),
    /// and exits in well under a second.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_preempts_in_flight_digest_promptly() {
        // Stub `claude` that reads input then sleeps long enough to
        // simulate a stalled subprocess. Pre-fix, abort had to wait
        // for this sleep; post-fix, drop(subagent) ends it via SIGKILL.
        let stub_dir = tempdir().unwrap();
        let stub_path = stub_dir.path().join("fake_claude_slow.sh");
        let script = r#"#!/usr/bin/env bash
read -r _line
echo '{"type":"system","subtype":"init"}'
sleep 30
"#;
        tokio::fs::File::create(&stub_path).await.unwrap();
        tokio::fs::write(&stub_path, script).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&stub_path).await.unwrap().permissions();
            perms.set_mode(0o755);
            tokio::fs::set_permissions(&stub_path, perms).await.unwrap();
        }
        let subagent = spawn_stub_retrying(&digest::SubagentConfig {
            claude_binary: stub_path,
            // Long enough that the subagent would wait on its own,
            // forcing the test to rely on the cancel path.
            read_timeout: Duration::from_secs(30),
            ..Default::default()
        })
        .await;

        let (events_tx, events_rx) = mpsc::channel::<HistoryEvent>(4);
        let (digests_tx, _digests_rx) = mpsc::channel::<Digest>(4);
        let (handle, shutdown) = run_digest_cycle(
            subagent,
            CycleConfig {
                interval: Duration::from_millis(80),
                window_size: 4,
            },
            events_rx,
            digests_tx,
        );

        // Push one event and wait long enough for the cycle to call
        // next_digest (which will get stuck on the script's `sleep 30`).
        events_tx
            .send(HistoryEvent {
                timestamp_ms: 1,
                kind: "user".into(),
                speech: "hello".into(),
            })
            .await
            .unwrap();
        sleep(Duration::from_millis(250)).await;

        // Trigger shutdown. The cycle must exit promptly — no waiting
        // for the script's 30s sleep, and no waiting for read_timeout.
        let started = std::time::Instant::now();
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("cycle did not exit within 2s of cancel — abort race not fixed")
            .expect("cycle task panicked");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took {elapsed:?}; expected sub-second cancel pre-emption"
        );
    }

    /// New `Feeder`/`AmbientFeeder` coverage: a non-empty Digest pushed
    /// onto the digests channel surfaces from `next_digest` as the
    /// rendered injection text; an empty Digest is skipped (the loop
    /// pulls the next item instead of returning the sentinel).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feeder_next_digest_renders_nonempty_and_skips_empty() {
        let (digests_tx, digests_rx) = mpsc::channel::<Digest>(4);
        // Build a Feeder directly around a hand-driven channel so the
        // test never needs a real `claude` binary. The cycle handle is a
        // no-op task; only the receiver + render path are exercised.
        let feeder = Feeder {
            rx: tokio::sync::Mutex::new(digests_rx),
            _handle: tokio::spawn(async {}),
            shutdown: None,
        };

        // Empty digest first — must be skipped, not returned as a
        // "(no new context)" sentinel.
        digests_tx
            .send(Digest {
                generated_ms: 1,
                ..Digest::default()
            })
            .await
            .unwrap();
        // Then a non-empty digest — must surface as rendered text.
        digests_tx
            .send(Digest {
                recent_facts: vec!["user is on AirPods".into()],
                active_topic: "audio device check".into(),
                generated_ms: 2,
                ..Digest::default()
            })
            .await
            .unwrap();

        let rendered = tokio::time::timeout(Duration::from_secs(2), feeder.next_digest())
            .await
            .expect("next_digest timed out")
            .expect("feeder returned None unexpectedly");
        assert!(rendered.starts_with("[ambient context update]"));
        assert!(rendered.contains("user is on AirPods"));
        assert!(rendered.contains("Active topic: audio device check"));
        assert!(
            !rendered.contains("(no new context)"),
            "empty digest leaked through as a sentinel: {rendered:?}"
        );
    }

    /// When the digests channel closes (cycle exited), `next_digest`
    /// returns None so the engine stops pulling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn feeder_next_digest_returns_none_on_channel_close() {
        let (digests_tx, digests_rx) = mpsc::channel::<Digest>(4);
        let feeder = Feeder {
            rx: tokio::sync::Mutex::new(digests_rx),
            _handle: tokio::spawn(async {}),
            shutdown: None,
        };
        drop(digests_tx);
        let out = tokio::time::timeout(Duration::from_secs(2), feeder.next_digest())
            .await
            .expect("next_digest timed out");
        assert!(out.is_none(), "closed channel must yield None");
    }
}
