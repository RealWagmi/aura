//! `aura-cli` — the thin voice client.
//!
//! It holds no engine, no `XAI_API_KEY`, no host adapter. It reads a connection
//! string (`AURA_CONNECT` or stdin — never argv), opens the Noise tunnel
//! to the server the host launched, and pumps cpal mic ↔ tunnel ↔ speaker. The
//! model, the chat context, the tools, and the key all live on the server.

use aura_audio::{AudioSettings, CpalTransport};
#[cfg(any(windows, target_os = "linux"))]
use aura_tunnel::TunnelControl;
use aura_tunnel::{
    ConnectionString, IrohEndpoint, IrohPreset, TransportKind, TunnelConfig, TunnelEndpoint,
    TunnelInputMode,
};
#[cfg(any(windows, target_os = "linux"))]
use std::sync::Arc;

#[cfg(windows)]
mod hotkey;

#[derive(Debug, Clone)]
enum InputMode {
    Voice,
    #[cfg(any(windows, target_os = "linux"))]
    TogglePushToTalk(PushToTalkGate),
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug, Clone)]
struct PushToTalkGate {
    state: Arc<std::sync::atomic::AtomicU64>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    failure_notify: Arc<tokio::sync::Notify>,
    label: String,
    #[cfg(target_os = "linux")]
    _control_socket: Arc<LinuxControlSocket>,
}

impl InputMode {
    fn from_mode(mode: TunnelInputMode, call_id: &str) -> Result<Self, Box<dyn std::error::Error>> {
        match mode {
            TunnelInputMode::Voice => Ok(Self::Voice),
            TunnelInputMode::PushToTalk => start_push_to_talk(call_id),
        }
    }

    fn local_mode_from_env() -> Result<TunnelInputMode, Box<dyn std::error::Error>> {
        let mode = std::env::var("AURA_INPUT_MODE").unwrap_or_else(|_| "voice".to_owned());
        match mode.trim().to_ascii_lowercase().as_str() {
            "" | "voice" | "vad" => Ok(TunnelInputMode::Voice),
            "push_to_talk" | "push-to-talk" | "ptt" => Ok(TunnelInputMode::PushToTalk),
            other => {
                Err(format!("AURA_INPUT_MODE must be voice or push_to_talk, got {other:?}").into())
            }
        }
    }
}

#[cfg(any(windows, target_os = "linux"))]
impl PushToTalkGate {
    fn press_count(&self) -> u64 {
        self.state.load(std::sync::atomic::Ordering::Acquire)
    }

    async fn wait_for_failure(&self) {
        if self.failed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.failure_notify.notified().await;
    }
}

#[cfg(windows)]
fn start_push_to_talk(_call_id: &str) -> Result<InputMode, Box<dyn std::error::Error>> {
    let raw = std::env::var("AURA_PUSH_TO_TALK_HOTKEY").unwrap_or_else(|_| "ctrl+space".to_owned());
    let allow_global = std::env::var("AURA_PUSH_TO_TALK_ALLOW_GLOBAL_HOTKEY").as_deref() == Ok("1");
    if allow_global {
        eprintln!(
            "aura: AURA_PUSH_TO_TALK_ALLOW_GLOBAL_HOTKEY=1 enables the PTT hotkey system-wide; keystrokes are observed even when Aura is not focused."
        );
    }
    let watcher = hotkey::start_push_to_talk_watcher(&raw, allow_global)?;
    Ok(InputMode::TogglePushToTalk(PushToTalkGate {
        state: watcher.presses,
        failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        failure_notify: Arc::new(tokio::sync::Notify::new()),
        label: watcher.label,
    }))
}

#[cfg(target_os = "linux")]
fn start_push_to_talk(call_id: &str) -> Result<InputMode, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let dir = push_to_talk_control_dir()?;
    let path = dir.join(format!(
        "aura-ptt-{}-{:016x}.sock",
        std::process::id(),
        stable_call_id_hash(call_id)
    ));
    remove_stale_control_socket(&path)?;
    let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|err| {
        format!(
            "failed to bind Linux push-to-talk control socket {}: {err}",
            path.display()
        )
    })?;
    let control_socket = Arc::new(LinuxControlSocket { path: path.clone() });
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let presses = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let task_presses = Arc::clone(&presses);
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_failed = Arc::clone(&failed);
    let failure_notify = Arc::new(tokio::sync::Notify::new());
    let task_failure_notify = Arc::clone(&failure_notify);
    let task_path = path.clone();
    let _listener_task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let connection_presses = Arc::clone(&task_presses);
                    tokio::spawn(async move {
                        use tokio::io::AsyncReadExt;

                        let mut command = Vec::with_capacity(8);
                        let read = tokio::time::timeout(
                            std::time::Duration::from_millis(500),
                            stream.take(8).read_to_end(&mut command),
                        )
                        .await;
                        if matches!(read, Ok(Ok(_))) && is_push_to_talk_toggle_command(&command) {
                            connection_presses.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        }
                    });
                }
                Err(err) => {
                    if is_transient_control_accept_error(&err) {
                        continue;
                    }
                    eprintln!(
                        "aura: Linux push-to-talk control socket {} stopped: {err}",
                        task_path.display()
                    );
                    task_failed.store(true, std::sync::atomic::Ordering::Release);
                    task_failure_notify.notify_one();
                    break;
                }
            }
        }
    });
    Ok(InputMode::TogglePushToTalk(PushToTalkGate {
        state: presses,
        failed,
        failure_notify,
        label: format!("aura-cli ptt-toggle ({})", path.display()),
        _control_socket: control_socket,
    }))
}

#[cfg(target_os = "linux")]
fn is_transient_control_accept_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

#[cfg(not(any(windows, target_os = "linux")))]
fn start_push_to_talk(_call_id: &str) -> Result<InputMode, Box<dyn std::error::Error>> {
    Err(
        "AURA_INPUT_MODE=push_to_talk currently supports Windows global hotkeys and Linux ptt-toggle only"
            .into(),
    )
}

fn main() {
    // Environment mutation must finish before Tokio starts worker threads.
    load_dotenv();
    // Handle `--version` / `--help` before touching the mic or stdin.
    if let Some(code) = handle_cli_flags() {
        std::process::exit(code);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("aura-cli: failed to start async runtime: {err}");
            std::process::exit(1);
        }
    };
    if let Err(err) = runtime.block_on(run()) {
        eprintln!("aura-cli: {err}");
        std::process::exit(1);
    }
}

/// Early `-v`/`-V`/`--version` and `-h`/`--help` handling. Returns the exit code
/// to use, or `None` to proceed with a normal call. The connection string is
/// never read from argv (only `AURA_CONNECT`/stdin), so no other flags exist.
fn handle_cli_flags() -> Option<i32> {
    match std::env::args().nth(1).as_deref() {
        Some("ptt-toggle") => match send_push_to_talk_toggle() {
            Ok(()) => Some(0),
            Err(err) => {
                eprintln!("aura-cli: {err}");
                Some(1)
            }
        },
        Some("-v" | "-V" | "--version") => {
            println!("aura-cli {}", env!("CARGO_PKG_VERSION"));
            Some(0)
        }
        Some("-h" | "--help") => {
            println!(
                "aura-cli {} — the thin voice client (mic/speaker only; holds no key).\n\n\
                 Give the connection string via the AURA_CONNECT env var, or run with no\n\
                 arguments and paste it on the first line of stdin. The single-use secret is\n\
                 never taken from the command line.\n\n\
                 Commands:\n  \
                 aura-cli              start a call from AURA_CONNECT/stdin\n  \
                 aura-cli ptt-toggle   Linux: toggle the active push_to_talk call\n\n\
                 Options:\n  \
                 -V, --version   print the version and exit\n  \
                 -h, --help      show this help and exit\n\n\
                 Environment:\n  \
                 AURA_CONNECT    the connection string — either form:\n                  \
                 direct: aura://HOST:PORT#k=...&c=...&t=direct&m=voice\n                  \
                 iroh:   aura://<node-id>#k=...&c=...&t=iroh&m=ptt (server behind NAT)\n  \
                 AURA_AEC        echo handling on the mic: on (default, AEC3 echo\n                  \
                 cancellation — speakers + barge-in work), gate (mute mic while\n                  \
                 the model speaks; no barge-in), off (raw mic; headsets only)\n  \
                 AURA_INPUT_MODE legacy strings only; new connection strings carry\n                  \
                 the server-selected m=voice or m=ptt mode\n  \
                 AURA_PUSH_TO_TALK_HOTKEY Windows focus-scoped toggle hotkey\n                  \
                 AURA_PUSH_TO_TALK_ALLOW_GLOBAL_HOTKEY=1 explicitly allow system-wide PTT\n                  \
                 for push_to_talk mode (default ctrl+space; letters/numbers need a modifier)\n  \
                 AURA_PUSH_TO_TALK_MAX_RECORDING_MS max push_to_talk open-mic time\n                  \
                 in milliseconds (default 300000, about 5 minutes)",
                env!("CARGO_PKG_VERSION")
            );
            Some(0)
        }
        // No arguments → proceed to a normal call (connection string from
        // AURA_CONNECT / stdin).
        None => None,
        // Any other argument is a mistake. Most dangerously, a pasted connection
        // string (with its single-use secret) on argv would ALREADY have leaked to
        // `ps` and shell history — refuse loudly and point at the safe channels,
        // rather than silently ignoring it and dialing anyway. The argument is
        // NEVER echoed back: near-miss pastes (`AURA_CONNECT=aura://…`,
        // `--connect=aura://…`, a quote-wrapped string) still carry the secret,
        // and echoing would copy it into stderr/session logs a second time.
        Some(other) => {
            if other.contains("aura://") || other.contains("#k=") {
                eprintln!(
                    "aura-cli: refusing a connection string on the command line — its single-use \
                     secret would leak to `ps` and shell history. Pass it via the AURA_CONNECT env \
                     var, or run `aura-cli` with no arguments and paste it on the first line of stdin."
                );
            } else {
                eprintln!(
                    "aura-cli: unexpected argument (not shown; it may contain sensitive data); \
                     aura-cli takes no arguments (see `aura-cli --help`)."
                );
            }
            Some(2)
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    ensure_mic_permission()?;

    let raw = read_connection_string()?;
    let conn = ConnectionString::parse(&raw)?;
    let tunnel_input_mode = resolve_input_mode(conn.input_mode)?;
    #[cfg(any(windows, target_os = "linux"))]
    let ptt_max_frames = if tunnel_input_mode == TunnelInputMode::PushToTalk {
        Some(push_to_talk_max_recording_frames()?)
    } else {
        None
    };
    // Prepare every fallible local PTT component before dialing. A successful
    // tunnel handshake consumes the server's single-use connection string, so
    // an invalid hotkey or unsafe/unavailable Linux control directory must fail
    // before that handshake rather than strand the call.
    let input_mode = InputMode::from_mode(tunnel_input_mode, &conn.call_id)?;
    eprintln!(
        "aura: opening a secure tunnel to {} (call {})…",
        conn.authority, conn.call_id
    );
    let cfg = TunnelConfig {
        input_mode: Some(tunnel_input_mode),
        ..TunnelConfig::default()
    };

    // The connection string is self-describing: dial whichever transport the
    // server minted — direct Noise/UDP, or iroh QUIC for a NAT/CGNAT server.
    match conn.transport {
        TransportKind::Direct => {
            let tunnel = TunnelEndpoint::connect_client(&conn.authority, &conn.secret, cfg).await?;
            pump(
                tunnel,
                input_mode,
                #[cfg(any(windows, target_os = "linux"))]
                ptt_max_frames,
            )
            .await?;
        }
        TransportKind::Iroh => {
            let tunnel = IrohEndpoint::connect_by_id(
                &conn.authority,
                &conn.secret,
                IrohPreset::Production,
                cfg,
            )
            .await?;
            pump(
                tunnel,
                input_mode,
                #[cfg(any(windows, target_os = "linux"))]
                ptt_max_frames,
            )
            .await?;
        }
    }

    eprintln!("aura: call ended.");
    Ok(())
}

fn resolve_input_mode(
    connection_mode: Option<TunnelInputMode>,
) -> Result<TunnelInputMode, Box<dyn std::error::Error>> {
    resolve_input_mode_with(connection_mode, InputMode::local_mode_from_env)
}

fn resolve_input_mode_with<F>(
    connection_mode: Option<TunnelInputMode>,
    local_mode: F,
) -> Result<TunnelInputMode, Box<dyn std::error::Error>>
where
    F: FnOnce() -> Result<TunnelInputMode, Box<dyn std::error::Error>>,
{
    if let Some(mode) = connection_mode {
        reject_unsupported_input_mode(mode)?;
        return Ok(mode);
    }
    let local_mode = local_mode()?;
    reject_unsupported_input_mode(local_mode)?;
    Ok(local_mode)
}

fn reject_unsupported_input_mode(mode: TunnelInputMode) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(any(windows, target_os = "linux")))]
    if mode == TunnelInputMode::PushToTalk {
        return Err(
            "connection string requests push_to_talk, but aura-cli supports push_to_talk only on Windows and Linux"
                .into(),
        );
    }
    let _ = mode;
    Ok(())
}

/// One audio-tunnel surface regardless of transport (direct Noise/UDP or iroh).
#[allow(async_fn_in_trait)]
trait VoiceTunnel {
    fn send_pcm24(&self, pcm: &[i16]);
    #[cfg(any(windows, target_os = "linux"))]
    fn send_control(&self, control: TunnelControl);
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>>;
}

impl VoiceTunnel for TunnelEndpoint {
    fn send_pcm24(&self, pcm: &[i16]) {
        TunnelEndpoint::send_pcm24(self, pcm);
    }
    #[cfg(any(windows, target_os = "linux"))]
    fn send_control(&self, control: TunnelControl) {
        TunnelEndpoint::send_control(self, control);
    }
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
        TunnelEndpoint::recv_pcm24(self).await
    }
}

impl VoiceTunnel for IrohEndpoint {
    fn send_pcm24(&self, pcm: &[i16]) {
        IrohEndpoint::send_pcm24(self, pcm);
    }
    #[cfg(any(windows, target_os = "linux"))]
    fn send_control(&self, control: TunnelControl) {
        IrohEndpoint::send_control(self, control);
    }
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
        IrohEndpoint::recv_pcm24(self).await
    }
}

/// Pump mic → tunnel and tunnel → speaker until either side closes. `select!`
/// drops the losing branch's borrow before the handler runs, so the two `&mut`
/// endpoints don't conflict (the discipline the engine's loop relies on).
async fn pump<T: VoiceTunnel>(
    mut tunnel: T,
    input_mode: InputMode,
    #[cfg(any(windows, target_os = "linux"))] ptt_max_frames: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("aura: tunnel up. Acquiring microphone and speaker…");
    let mut audio = CpalTransport::start(AudioSettings::default())?;
    #[cfg(any(windows, target_os = "linux"))]
    let mut ptt_recording = false;
    #[cfg(any(windows, target_os = "linux"))]
    let mut ptt_seen_presses = match &input_mode {
        InputMode::Voice => 0,
        InputMode::TogglePushToTalk(gate) => gate.press_count(),
    };
    #[cfg(any(windows, target_os = "linux"))]
    let mut ptt_frames_open = 0usize;
    #[cfg(any(windows, target_os = "linux"))]
    let ptt_max_frames = ptt_max_frames.unwrap_or(usize::MAX);
    #[cfg(any(windows, target_os = "linux"))]
    let mut ptt_warned_near_limit = false;
    #[cfg(any(windows, target_os = "linux"))]
    if let InputMode::TogglePushToTalk(gate) = &input_mode {
        eprintln!(
            "aura: push-to-talk is enabled. Press {} to start talking, then press {} again to send.",
            gate.label, gate.label
        );
    }
    eprintln!("aura: on the call — speak when you hear Aura. Ctrl-C to hang up.");
    loop {
        tokio::select! {
            _ = wait_ptt_control_failure(&input_mode) => {
                return Err("Push-to-talk control failed; ending the call rather than leaving microphone control unavailable".into());
            }
            mic = audio.recv_pcm24() => match mic {
                Some(frame) => match &input_mode {
                    InputMode::Voice => tunnel.send_pcm24(&frame),
                    #[cfg(any(windows, target_os = "linux"))]
                    InputMode::TogglePushToTalk(gate) => {
                        let presses = gate.press_count();
                        if presses != ptt_seen_presses {
                            let toggles = presses.saturating_sub(ptt_seen_presses);
                            ptt_seen_presses = presses;
                            match resolve_ptt_toggles(
                                ptt_recording,
                                toggles,
                                ptt_frames_open,
                                PUSH_TO_TALK_MIN_RECORDING_FRAMES,
                            ) {
                                PttBatchAction::None => {}
                                PttBatchAction::Start => {
                                    ptt_recording = true;
                                    ptt_frames_open = 0;
                                    ptt_warned_near_limit = false;
                                    tunnel.send_control(TunnelControl::PttOpen);
                                    audio.clear_playout();
                                    eprintln!("aura: recording push-to-talk message.");
                                }
                                PttBatchAction::Send => {
                                    ptt_recording = false;
                                    ptt_frames_open = 0;
                                    ptt_warned_near_limit = false;
                                    tunnel.send_control(TunnelControl::PttClose);
                                    eprintln!("aura: sent push-to-talk message.");
                                }
                                PttBatchAction::DiscardTooShort => {
                                    // Frames were already streamed live; tell
                                    // the server to drop them, or they would
                                    // prefix the next committed turn. (From
                                    // idle — open+close in one batch — nothing
                                    // was streamed and no control ever sent.)
                                    if ptt_recording {
                                        tunnel.send_control(TunnelControl::PttCancel);
                                    }
                                    ptt_recording = false;
                                    ptt_frames_open = 0;
                                    ptt_warned_near_limit = false;
                                    eprintln!("aura: push-to-talk message was too short; discarded.");
                                }
                                PttBatchAction::SendThenRestart => {
                                    ptt_frames_open = 0;
                                    ptt_warned_near_limit = false;
                                    tunnel.send_control(TunnelControl::PttClose);
                                    eprintln!("aura: sent push-to-talk message.");
                                    tunnel.send_control(TunnelControl::PttOpen);
                                    audio.clear_playout();
                                    eprintln!("aura: recording push-to-talk message.");
                                }
                                PttBatchAction::DiscardThenRestart => {
                                    ptt_frames_open = 0;
                                    ptt_warned_near_limit = false;
                                    tunnel.send_control(TunnelControl::PttCancel);
                                    eprintln!("aura: push-to-talk message was too short; discarded.");
                                    tunnel.send_control(TunnelControl::PttOpen);
                                    audio.clear_playout();
                                    eprintln!("aura: recording push-to-talk message.");
                                }
                            }
                        }

                        if ptt_recording {
                            tunnel.send_pcm24(&frame);
                            ptt_frames_open = ptt_frames_open.saturating_add(1);
                            if !ptt_warned_near_limit
                                && ptt_frames_open >= push_to_talk_limit_warning_frame(ptt_max_frames)
                            {
                                ptt_warned_near_limit = true;
                                eprintln!(
                                    "aura: push-to-talk recording is near the limit; sending in about 3 seconds. You can increase it with AURA_PUSH_TO_TALK_MAX_RECORDING_MS."
                                );
                                signal_push_to_talk_limit_warning();
                            }
                            if ptt_frames_open >= ptt_max_frames {
                                ptt_recording = false;
                                ptt_frames_open = 0;
                                ptt_warned_near_limit = false;
                                tunnel.send_control(TunnelControl::PttClose);
                                eprintln!("aura: sent push-to-talk message.");
                            }
                        }
                    }
                },
                None => break, // mic/device closed
            },
            net = tunnel.recv_pcm24() => match net {
                Some(frame) => audio.send_pcm24(&frame),
                None => break, // tunnel closed (hang-up / peer gone)
            },
        }
    }
    Ok(())
}

async fn wait_ptt_control_failure(input_mode: &InputMode) {
    match input_mode {
        InputMode::TogglePushToTalk(gate) => gate.wait_for_failure().await,
        InputMode::Voice => std::future::pending().await,
    }
}

#[cfg(any(windows, target_os = "linux"))]
const DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS: u64 = 300_000;
#[cfg(any(windows, target_os = "linux"))]
const PUSH_TO_TALK_FRAME_MS: u64 = 20;
#[cfg(any(windows, target_os = "linux"))]
const PUSH_TO_TALK_LIMIT_WARNING_MS: u64 = 3_000;
#[cfg(any(windows, target_os = "linux"))]
// Keep this aligned with the engine's 2,400-sample manual-turn minimum:
// five 20 ms client frames are 100 ms at 24 kHz.
const PUSH_TO_TALK_MIN_RECORDING_FRAMES: usize = 5;

#[cfg(any(windows, target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PttBatchAction {
    None,
    Start,
    Send,
    DiscardTooShort,
    /// Two-plus presses landed in one poll gap while recording: the user
    /// closed the in-flight turn (long enough to send) and reopened. The close
    /// must not be collapsed away — a lost `PttClose` leaves the turn open and
    /// the sent-message feedback never prints.
    SendThenRestart,
    /// Same batch shape, but the in-flight turn was under the minimum:
    /// discard it (cancel server-side) and keep recording the fresh turn.
    DiscardThenRestart,
}

#[cfg(any(windows, target_os = "linux"))]
fn resolve_ptt_toggles(
    recording: bool,
    toggles: u64,
    recorded_frames: usize,
    min_frames: usize,
) -> PttBatchAction {
    if toggles == 0 {
        return PttBatchAction::None;
    }
    let net_recording = recording ^ (toggles % 2 == 1);
    match (recording, net_recording) {
        // Opened and closed within one batch: zero frames were streamed in
        // between, so there is nothing to send or cancel.
        (false, false) => PttBatchAction::DiscardTooShort,
        (false, true) => PttBatchAction::Start,
        (true, false) if recorded_frames >= min_frames => PttBatchAction::Send,
        (true, false) => PttBatchAction::DiscardTooShort,
        (true, true) if recorded_frames >= min_frames => PttBatchAction::SendThenRestart,
        (true, true) => PttBatchAction::DiscardThenRestart,
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn push_to_talk_max_recording_frames() -> Result<usize, Box<dyn std::error::Error>> {
    parse_push_to_talk_max_recording_ms(
        std::env::var("AURA_PUSH_TO_TALK_MAX_RECORDING_MS")
            .ok()
            .as_deref(),
    )
}

#[cfg(any(windows, target_os = "linux"))]
fn parse_push_to_talk_max_recording_ms(
    raw: Option<&str>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let Some(raw) = raw else {
        return recording_ms_to_frames(DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return recording_ms_to_frames(DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS);
    }
    let ms = trimmed.parse::<u64>().map_err(|_| {
        format!("AURA_PUSH_TO_TALK_MAX_RECORDING_MS must be a positive number, got {raw:?}")
    })?;
    let minimum_ms = PUSH_TO_TALK_MIN_RECORDING_FRAMES as u64 * PUSH_TO_TALK_FRAME_MS;
    if ms < minimum_ms {
        return Err(
            format!("AURA_PUSH_TO_TALK_MAX_RECORDING_MS must be at least {minimum_ms} ms").into(),
        );
    }
    recording_ms_to_frames(ms)
}

#[cfg(any(windows, target_os = "linux"))]
fn recording_ms_to_frames(ms: u64) -> Result<usize, Box<dyn std::error::Error>> {
    let frames = ms.div_ceil(PUSH_TO_TALK_FRAME_MS);
    usize::try_from(frames).map_err(|_| "AURA_PUSH_TO_TALK_MAX_RECORDING_MS is too large".into())
}

#[cfg(any(windows, target_os = "linux"))]
fn push_to_talk_limit_warning_frame(max_frames: usize) -> usize {
    let warning_frames = recording_ms_to_frames(PUSH_TO_TALK_LIMIT_WARNING_MS).unwrap_or(150);
    max_frames.saturating_sub(warning_frames).max(1)
}

#[cfg(any(windows, target_os = "linux"))]
fn signal_push_to_talk_limit_warning() {
    eprint!("\x07");
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct LinuxControlSocket {
    path: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl Drop for LinuxControlSocket {
    fn drop(&mut self) {
        let _ = remove_owned_control_socket(&self.path);
    }
}

#[cfg(target_os = "linux")]
fn stable_call_id_hash(call_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call_id.hash(&mut hasher);
    hasher.finish()
}

#[cfg(target_os = "linux")]
fn is_push_to_talk_toggle_command(command: &[u8]) -> bool {
    command == b"toggle\n"
}

#[cfg(target_os = "linux")]
fn push_to_talk_control_dir() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let (private_base, dir) = select_push_to_talk_control_dir(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("HOME"),
    )?;
    let relative = dir.strip_prefix(&private_base).map_err(|_| {
        format!(
            "Linux push-to-talk runtime directory {} is outside its private base {}",
            dir.display(),
            private_base.display()
        )
    })?;
    create_private_directory_tree(&private_base, relative)?;
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn create_private_directory_tree(
    base: &std::path::Path,
    relative: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::{CString, OsStr};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    fn component_name(component: &OsStr) -> Result<CString, Box<dyn std::error::Error>> {
        CString::new(component.as_bytes())
            .map_err(|_| "Linux push-to-talk directory contains a NUL byte".into())
    }

    fn open_at(parent: i32, name: &CString) -> std::io::Result<OwnedFd> {
        // SAFETY: `name` is NUL-terminated, `parent` is a live directory fd,
        // and a successful return transfers ownership of the new fd.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: `openat` returned a fresh owned file descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    fn inspect_private_dir(
        fd: i32,
        label: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `stat` points to writable storage and `fd` remains live.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: a successful `fstat` initialized the structure.
        let stat = unsafe { stat.assume_init() };
        let uid = unsafe { libc::geteuid() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || stat.st_uid != uid
            || stat.st_mode & 0o022 != 0
        {
            return Err(format!(
                "refusing unsafe Linux push-to-talk directory {} (must be owned by the current user and not writable by group or others)",
                label.display()
            )
            .into());
        }
        Ok(())
    }

    let root = CString::new("/").expect("static path");
    let dot = CString::new(".").expect("static path");
    let mut current = if base.is_absolute() {
        open_at(libc::AT_FDCWD, &root)?
    } else {
        open_at(libc::AT_FDCWD, &dot)?
    };
    let mut traversed = std::path::PathBuf::new();
    for component in base.components() {
        use std::path::Component;
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(format!(
                    "refusing unsafe Linux push-to-talk base path {}",
                    base.display()
                )
                .into())
            }
        };
        traversed.push(name);
        current = open_at(current.as_raw_fd(), &component_name(name)?).map_err(|err| {
            format!(
                "failed no-symlink traversal of Linux push-to-talk base {}: {err}",
                base.display()
            )
        })?;
    }
    inspect_private_dir(current.as_raw_fd(), base)?;

    let mut final_fd = None;
    for component in relative.components() {
        use std::path::Component;
        let name = match component {
            Component::Normal(name) => name,
            _ => {
                return Err(format!(
                    "refusing unsafe Linux push-to-talk relative path {}",
                    relative.display()
                )
                .into())
            }
        };
        let name_c = component_name(name)?;
        let next = match open_at(current.as_raw_fd(), &name_c) {
            Ok(fd) => fd,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // SAFETY: both arguments are valid and the parent fd is live.
                let result = unsafe { libc::mkdirat(current.as_raw_fd(), name_c.as_ptr(), 0o700) };
                if result != 0 {
                    let mkdir_err = std::io::Error::last_os_error();
                    if mkdir_err.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(mkdir_err.into());
                    }
                }
                open_at(current.as_raw_fd(), &name_c)?
            }
            Err(err) => return Err(err.into()),
        };
        traversed.push(name);
        inspect_private_dir(next.as_raw_fd(), &traversed)?;
        current = next;
        final_fd = Some(current.as_raw_fd());
    }
    if let Some(fd) = final_fd {
        // SAFETY: `fd` is the live final directory descriptor.
        if unsafe { libc::fchmod(fd, 0o700) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn select_push_to_talk_control_dir(
    xdg_runtime_dir: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<(std::path::PathBuf, std::path::PathBuf), Box<dyn std::error::Error>> {
    if let Some(runtime_dir) = xdg_runtime_dir.filter(|value| !value.is_empty()) {
        let base = std::path::PathBuf::from(runtime_dir);
        let dir = base.join("aura");
        return Ok((base, dir));
    }
    if let Some(home) = home.filter(|value| !value.is_empty()) {
        // Do not use a predictable shared-temp path here. Another local user
        // could create `/tmp/aura-$UID` first and deny PTT startup. The home
        // directory remains a stable, user-private rendezvous point for the
        // separate `aura-cli ptt-toggle` process.
        let base = std::path::PathBuf::from(home);
        let dir = base.join(".cache").join("aura").join("runtime");
        return Ok((base, dir));
    }
    Err(
        "Linux push-to-talk needs XDG_RUNTIME_DIR or HOME; refusing an unsafe shared temporary-directory fallback"
            .into(),
    )
}

#[cfg(target_os = "linux")]
fn owned_control_socket(path: &std::path::Path) -> Result<bool, Box<dyn std::error::Error>> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() })
}

#[cfg(target_os = "linux")]
fn remove_owned_control_socket(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    if owned_control_socket(path)? {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_stale_control_socket(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(());
    }
    if !owned_control_socket(path)? {
        return Err(format!(
            "refusing to replace Linux push-to-talk path {} because it is not an owned Unix socket",
            path.display()
        )
        .into());
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(format!(
            "refusing to replace live Linux push-to-talk socket {}",
            path.display()
        )
        .into());
    }
    remove_owned_control_socket(path)
}

#[cfg(target_os = "linux")]
fn send_push_to_talk_toggle() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let dir = push_to_talk_control_dir()?;
    let mut active = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_candidate = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("aura-ptt-") && name.ends_with(".sock"));
        if !is_candidate || !owned_control_socket(&path)? {
            continue;
        }
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            active.push(path);
        } else {
            remove_owned_control_socket(&path)?;
        }
    }
    let path = match active.as_slice() {
        [path] => path,
        [] => {
            return Err(
                "no active Linux push-to-talk call found; start a push_to_talk call first".into(),
            );
        }
        _ => {
            return Err(
                "multiple Linux push-to-talk calls are active; close all but the call you want to control"
                    .into(),
            );
        }
    };
    let mut stream = std::os::unix::net::UnixStream::connect(path).map_err(|err| {
        format!(
            "failed to reach Linux push-to-talk control socket {}: {err}",
            path.display()
        )
    })?;
    stream.write_all(b"toggle\n")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn send_push_to_talk_toggle() -> Result<(), Box<dyn std::error::Error>> {
    Err(
        "`aura-cli ptt-toggle` is supported only on Linux; Windows uses AURA_PUSH_TO_TALK_HOTKEY"
            .into(),
    )
}

/// Read the connection string from `AURA_CONNECT`, else one line from stdin.
/// Never argv — the secret would otherwise be visible in `ps`.
fn read_connection_string() -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(v) = std::env::var("AURA_CONNECT") {
        let v = v.trim().to_owned();
        if !v.is_empty() {
            return Ok(v);
        }
    }
    eprintln!("aura: paste the connection string (aura://…) and press Enter:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim().to_owned();
    if line.is_empty() {
        return Err("no connection string (set AURA_CONNECT or paste it on stdin)".into());
    }
    Ok(line)
}

/// Ensure the process may capture the microphone.
///
/// - **Linux / Windows:** no programmatic prompt for a plain binary; an
///   access-denied surfaces when the cpal stream is built. No-op.
/// - **macOS:** the real TCC prompt requires a signed `.app` with an
///   `NSMicrophoneUsageDescription`; until then this is a
///   no-op and the OS surfaces the error at stream build.
fn ensure_mic_permission() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Minimal `.env` loader for client-side settings only.
///
/// Do not load `AURA_CONNECT` from cwd/global `.env`: the connection string is a
/// live microphone routing secret and must come only from the real process env
/// or stdin. A target repository must not be able to redirect `aura-cli` or
/// change microphone/PTT controls by planting `.env`. PTT settings are accepted
/// only from the real process environment or the trusted user-global config.
fn load_dotenv() {
    load_dotenv_file(std::path::Path::new(".env"), false);
    if let Some(dir) = global_config_dir() {
        load_dotenv_file(&dir.join(".env"), true);
    }
}

fn global_config_dir() -> Option<std::path::PathBuf> {
    global_config_dir_from(
        std::env::var_os("AURA_HOME"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )
}

fn global_config_dir_from(
    aura_home: Option<std::ffi::OsString>,
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    if let Some(h) = aura_home.filter(|s| !s.is_empty()) {
        return Some(std::path::PathBuf::from(h));
    }
    if let Some(x) = xdg_config_home.filter(|s| !s.is_empty()) {
        return Some(std::path::PathBuf::from(x).join("aura"));
    }
    home.filter(|s| !s.is_empty())
        .map(|h| std::path::PathBuf::from(h).join(".config").join("aura"))
}

fn load_dotenv_file(path: &std::path::Path, trusted_user_config: bool) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !is_cli_dotenv_key_allowed(key, trusted_user_config) {
            continue;
        }
        let mut value = value.trim();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = &value[1..value.len() - 1];
        }
        if value.is_empty() {
            continue;
        }
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, expand_home(value));
        }
    }
}

fn is_cli_dotenv_key_allowed(key: &str, trusted_user_config: bool) -> bool {
    key == "AURA_AEC"
        || (trusted_user_config
            && matches!(
                key,
                "AURA_INPUT_MODE"
                    | "AURA_PUSH_TO_TALK_HOTKEY"
                    | "AURA_PUSH_TO_TALK_MAX_RECORDING_MS"
                    | "AURA_PUSH_TO_TALK_ALLOW_GLOBAL_HOTKEY"
            ))
}

fn expand_home(value: &str) -> String {
    let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) else {
        return value.to_owned();
    };
    let home = home.to_string_lossy();
    let mut v = value.replace("${HOME}", &home).replace("$HOME", &home);
    if v == "~" {
        v = home.into_owned();
    } else if let Some(rest) = v.strip_prefix("~/") {
        v = format!("{home}/{rest}");
    }
    v
}

#[cfg(test)]
mod cross_platform_tests {
    use super::{reject_unsupported_input_mode, resolve_input_mode_with};
    use aura_tunnel::TunnelInputMode;

    #[test]
    fn ptt_mode_is_rejected_only_where_unsupported() {
        #[cfg(any(windows, target_os = "linux"))]
        assert!(reject_unsupported_input_mode(TunnelInputMode::PushToTalk).is_ok());
        #[cfg(not(any(windows, target_os = "linux")))]
        assert!(reject_unsupported_input_mode(TunnelInputMode::PushToTalk).is_err());
        assert!(reject_unsupported_input_mode(TunnelInputMode::Voice).is_ok());
    }

    #[test]
    fn authoritative_connection_mode_does_not_parse_local_config() {
        let mode = resolve_input_mode_with(Some(TunnelInputMode::Voice), || {
            panic!("local AURA_INPUT_MODE must not be parsed")
        })
        .expect("connection mode");
        assert_eq!(mode, TunnelInputMode::Voice);
    }
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::{
        global_config_dir_from, is_cli_dotenv_key_allowed, load_dotenv_file,
        parse_push_to_talk_max_recording_ms, push_to_talk_limit_warning_frame, resolve_ptt_toggles,
        PttBatchAction, DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS, PUSH_TO_TALK_MIN_RECORDING_FRAMES,
    };
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn global_config_dir_precedence_matches_server() {
        assert_eq!(
            global_config_dir_from(
                Some(OsString::from("/srv/aura")),
                Some(OsString::from("/x")),
                Some(OsString::from("/home/u")),
            ),
            Some(PathBuf::from("/srv/aura"))
        );
        assert_eq!(
            global_config_dir_from(
                None,
                Some(OsString::from("/x")),
                Some(OsString::from("/home/u"))
            ),
            Some(PathBuf::from("/x/aura"))
        );
        assert_eq!(
            global_config_dir_from(None, None, Some(OsString::from("/home/u"))),
            Some(PathBuf::from("/home/u/.config/aura"))
        );
    }

    #[test]
    fn cli_dotenv_allowlist_blocks_connection_string() {
        assert!(!is_cli_dotenv_key_allowed("AURA_INPUT_MODE", false));
        assert!(!is_cli_dotenv_key_allowed(
            "AURA_PUSH_TO_TALK_HOTKEY",
            false
        ));
        assert!(!is_cli_dotenv_key_allowed(
            "AURA_PUSH_TO_TALK_MAX_RECORDING_MS",
            false
        ));
        assert!(!is_cli_dotenv_key_allowed(
            "AURA_PUSH_TO_TALK_CONTROL_PATH",
            true
        ));
        assert!(is_cli_dotenv_key_allowed("AURA_INPUT_MODE", true));
        assert!(is_cli_dotenv_key_allowed("AURA_PUSH_TO_TALK_HOTKEY", true));
        assert!(is_cli_dotenv_key_allowed("AURA_AEC", false));
        assert!(!is_cli_dotenv_key_allowed("AURA_CONNECT", true));
        assert!(!is_cli_dotenv_key_allowed("XAI_API_KEY", true));
        assert!(!is_cli_dotenv_key_allowed("OPENAI_API_KEY", true));
        assert!(!is_cli_dotenv_key_allowed("AURA_REALTIME_URL", true));
    }

    #[test]
    fn cli_dotenv_ignores_aura_connect_from_file() {
        unsafe {
            std::env::remove_var("AURA_CONNECT");
            std::env::remove_var("AURA_INPUT_MODE");
            std::env::remove_var("AURA_PUSH_TO_TALK_HOTKEY");
            std::env::remove_var("AURA_PUSH_TO_TALK_CONTROL_PATH");
            std::env::remove_var("AURA_PUSH_TO_TALK_MAX_RECORDING_MS");
        }
        let tmp = std::env::temp_dir().join(format!("aura-cli-dotenv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir(&tmp).unwrap();
        let env_file = tmp.join(".env");
        std::fs::write(
            &env_file,
            "AURA_CONNECT=aura://attacker.invalid#k=bad&c=call-bad\nAURA_INPUT_MODE=push_to_talk\nAURA_PUSH_TO_TALK_HOTKEY=a\nAURA_PUSH_TO_TALK_CONTROL_PATH=$HOME/.ssh/config\nAURA_PUSH_TO_TALK_MAX_RECORDING_MS=1\n",
        )
        .unwrap();

        load_dotenv_file(&env_file, false);

        assert!(std::env::var_os("AURA_CONNECT").is_none());
        assert!(std::env::var_os("AURA_INPUT_MODE").is_none());
        assert!(std::env::var_os("AURA_PUSH_TO_TALK_HOTKEY").is_none());
        assert!(std::env::var_os("AURA_PUSH_TO_TALK_CONTROL_PATH").is_none());
        assert!(std::env::var_os("AURA_PUSH_TO_TALK_MAX_RECORDING_MS").is_none());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn push_to_talk_max_recording_defaults_to_five_minutes() {
        assert_eq!(DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS, 300_000);
        assert_eq!(parse_push_to_talk_max_recording_ms(None).unwrap(), 15_000);
        assert_eq!(
            parse_push_to_talk_max_recording_ms(Some("")).unwrap(),
            15_000
        );
    }

    #[test]
    fn push_to_talk_max_recording_accepts_milliseconds() {
        assert_eq!(
            parse_push_to_talk_max_recording_ms(Some("1000")).unwrap(),
            50
        );
        assert_eq!(
            parse_push_to_talk_max_recording_ms(Some("1001")).unwrap(),
            51
        );
    }

    #[test]
    fn push_to_talk_limit_warning_is_three_seconds_before_cap() {
        assert_eq!(push_to_talk_limit_warning_frame(152), 2);
    }

    #[test]
    fn push_to_talk_max_recording_rejects_invalid_values() {
        assert!(parse_push_to_talk_max_recording_ms(Some("0")).is_err());
        assert!(parse_push_to_talk_max_recording_ms(Some("99")).is_err());
        assert_eq!(
            parse_push_to_talk_max_recording_ms(Some("100")).unwrap(),
            PUSH_TO_TALK_MIN_RECORDING_FRAMES
        );
        assert!(parse_push_to_talk_max_recording_ms(Some("abc")).is_err());
    }

    #[test]
    fn ptt_toggle_batch_resolves_to_one_action() {
        let min = PUSH_TO_TALK_MIN_RECORDING_FRAMES;
        assert_eq!(min, 5, "client minimum must remain 100 ms");
        assert_eq!(resolve_ptt_toggles(false, 1, 0, min), PttBatchAction::Start);
        assert_eq!(resolve_ptt_toggles(true, 1, min, min), PttBatchAction::Send);
        assert_eq!(
            resolve_ptt_toggles(true, 1, min - 1, min),
            PttBatchAction::DiscardTooShort
        );
        assert_eq!(
            resolve_ptt_toggles(false, 2, 0, min),
            PttBatchAction::DiscardTooShort
        );
        // Three presses from idle: the intermediate open+close pair streamed
        // zero frames, so a single Start is the whole batch.
        assert_eq!(resolve_ptt_toggles(false, 3, 0, min), PttBatchAction::Start);
    }

    #[test]
    fn ptt_double_press_while_recording_preserves_the_close() {
        // Two presses in one poll gap while recording is close+reopen — the
        // in-flight turn must be SENT (or cancelled), never collapsed to None:
        // a swallowed PttClose leaves the turn open until the safety cap.
        assert_eq!(
            resolve_ptt_toggles(true, 2, 20, 10),
            PttBatchAction::SendThenRestart
        );
        assert_eq!(
            resolve_ptt_toggles(true, 2, 9, 10),
            PttBatchAction::DiscardThenRestart
        );
        assert_eq!(resolve_ptt_toggles(true, 3, 20, 10), PttBatchAction::Send);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::{
        create_private_directory_tree, is_push_to_talk_toggle_command,
        is_transient_control_accept_error, remove_stale_control_socket,
        select_push_to_talk_control_dir,
    };
    use std::ffi::OsString;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn control_socket_requires_exact_toggle_command() {
        assert!(is_push_to_talk_toggle_command(b"toggle\n"));
        assert!(!is_push_to_talk_toggle_command(b""));
        assert!(!is_push_to_talk_toggle_command(b"toggle"));
        assert!(!is_push_to_talk_toggle_command(b"toggle\nextra"));
    }

    #[test]
    fn control_accept_retries_only_transient_errors() {
        assert!(is_transient_control_accept_error(&std::io::Error::from(
            std::io::ErrorKind::Interrupted
        )));
        assert!(!is_transient_control_accept_error(&std::io::Error::from(
            std::io::ErrorKind::OutOfMemory
        )));
    }

    #[test]
    fn control_dir_prefers_xdg_runtime_and_falls_back_to_home_cache() {
        let (base, dir) = select_push_to_talk_control_dir(
            Some(OsString::from("/run/user/1000")),
            Some(OsString::from("/home/alice")),
        )
        .expect("XDG runtime path");
        assert_eq!(base, std::path::PathBuf::from("/run/user/1000"));
        assert_eq!(dir, base.join("aura"));

        let (base, dir) =
            select_push_to_talk_control_dir(None, Some(OsString::from("/home/alice")))
                .expect("home cache path");
        assert_eq!(base, std::path::PathBuf::from("/home/alice"));
        assert_eq!(dir, base.join(".cache/aura/runtime"));
    }

    #[test]
    fn control_dir_never_falls_back_to_shared_temp() {
        let err = select_push_to_talk_control_dir(None, None).expect_err("missing private base");
        assert!(err
            .to_string()
            .contains("unsafe shared temporary-directory"));
    }

    #[test]
    fn stale_cleanup_never_removes_a_regular_file() {
        let dir = std::env::temp_dir().join(format!(
            "aura-cli-socket-safety-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("test directory");
        let path = dir.join("aura-ptt-test.sock");
        std::fs::write(&path, b"keep me").expect("test file");

        assert!(remove_stale_control_socket(&path).is_err());
        assert_eq!(std::fs::read(&path).expect("file preserved"), b"keep me");
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn private_directory_tree_is_created_with_private_final_mode() {
        let base =
            std::env::temp_dir().join(format!("aura-cli-private-tree-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir(&base).expect("test base");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("private base");

        create_private_directory_tree(&base, std::path::Path::new(".cache/aura/runtime"))
            .expect("safe descriptor-relative creation");
        let runtime = base.join(".cache/aura/runtime");
        let mode = std::fs::symlink_metadata(&runtime)
            .expect("runtime metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        std::fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn private_directory_tree_rejects_intermediate_symlink() {
        let base =
            std::env::temp_dir().join(format!("aura-cli-symlink-tree-test-{}", std::process::id()));
        let target = std::env::temp_dir().join(format!(
            "aura-cli-symlink-target-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&target);
        std::fs::create_dir(&base).expect("test base");
        std::fs::create_dir(&target).expect("symlink target");
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            .expect("private base");
        symlink(&target, base.join(".cache")).expect("intermediate symlink");

        let err = create_private_directory_tree(&base, std::path::Path::new(".cache/aura/runtime"))
            .expect_err("intermediate symlink must be rejected");
        assert!(
            err.to_string().contains("Not a directory")
                || err.to_string().contains("Too many levels")
                || err.to_string().contains("os error")
        );
        assert!(!target.join("aura/runtime").exists());
        std::fs::remove_dir_all(&base).expect("cleanup base");
        std::fs::remove_dir_all(&target).expect("cleanup target");
    }
}
