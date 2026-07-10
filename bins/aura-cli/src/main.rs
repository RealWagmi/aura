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
    label: String,
}

impl InputMode {
    fn from_mode(mode: TunnelInputMode) -> Result<Self, Box<dyn std::error::Error>> {
        match mode {
            TunnelInputMode::Voice => Ok(Self::Voice),
            TunnelInputMode::PushToTalk => start_push_to_talk(),
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
}

#[cfg(windows)]
fn start_push_to_talk() -> Result<InputMode, Box<dyn std::error::Error>> {
    let raw = std::env::var("AURA_PUSH_TO_TALK_HOTKEY").unwrap_or_else(|_| "ctrl+space".to_owned());
    let watcher = hotkey::start_push_to_talk_watcher(&raw)?;
    Ok(InputMode::TogglePushToTalk(PushToTalkGate {
        state: watcher.presses,
        label: watcher.label,
    }))
}

#[cfg(target_os = "linux")]
fn start_push_to_talk() -> Result<InputMode, Box<dyn std::error::Error>> {
    let path = push_to_talk_control_path();
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|err| {
        format!(
            "failed to bind Linux push-to-talk control socket {}: {err}",
            path.display()
        )
    })?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let presses = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let task_presses = Arc::clone(&presses);
    let task_path = path.clone();
    let _listener_task = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((_stream, _addr)) => {
                    task_presses.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                Err(err) => {
                    eprintln!(
                        "aura: Linux push-to-talk control socket {} stopped: {err}",
                        task_path.display()
                    );
                    break;
                }
            }
        }
    });
    Ok(InputMode::TogglePushToTalk(PushToTalkGate {
        state: presses,
        label: format!("aura-cli ptt-toggle ({})", path.display()),
    }))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn start_push_to_talk() -> Result<InputMode, Box<dyn std::error::Error>> {
    Err(
        "AURA_INPUT_MODE=push_to_talk currently supports Windows global hotkeys and Linux ptt-toggle only"
            .into(),
    )
}

#[tokio::main]
async fn main() {
    load_dotenv();
    // Handle `--version` / `--help` before touching the mic or stdin.
    if let Some(code) = handle_cli_flags() {
        std::process::exit(code);
    }
    if let Err(err) = run().await {
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
                 AURA_INPUT_MODE voice (default) or push_to_talk\n  \
                 AURA_PUSH_TO_TALK_HOTKEY Windows global toggle hotkey\n                  \
                 for push_to_talk mode (default ctrl+space)\n  \
                 AURA_PUSH_TO_TALK_CONTROL_PATH Linux socket path used by\n                  \
                 `aura-cli ptt-toggle` (default: $XDG_RUNTIME_DIR/aura-ptt.sock,\n                  \
                 then the OS temp dir)\n  \
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
    let input_mode = resolve_input_mode(conn.input_mode)?;
    eprintln!(
        "aura: opening a secure tunnel to {} (call {})…",
        conn.authority, conn.call_id
    );
    let cfg = TunnelConfig::default();

    // The connection string is self-describing: dial whichever transport the
    // server minted — direct Noise/UDP, or iroh QUIC for a NAT/CGNAT server.
    match conn.transport {
        TransportKind::Direct => {
            let tunnel = TunnelEndpoint::connect_client(&conn.authority, &conn.secret, cfg).await?;
            pump(tunnel, input_mode).await?;
        }
        TransportKind::Iroh => {
            let tunnel = IrohEndpoint::connect_by_id(
                &conn.authority,
                &conn.secret,
                IrohPreset::Production,
                cfg,
            )
            .await?;
            pump(tunnel, input_mode).await?;
        }
    }

    eprintln!("aura: call ended.");
    Ok(())
}

fn resolve_input_mode(
    connection_mode: Option<TunnelInputMode>,
) -> Result<TunnelInputMode, Box<dyn std::error::Error>> {
    let local_mode = InputMode::local_mode_from_env()?;
    if let Some(mode) = connection_mode {
        reject_unsupported_input_mode(mode)?;
        if mode != local_mode {
            eprintln!("aura: connection string input mode wins over local AURA_INPUT_MODE.");
        }
        Ok(mode)
    } else {
        Ok(local_mode)
    }
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
    tunnel_input_mode: TunnelInputMode,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("aura: tunnel up. Acquiring microphone and speaker…");
    let mut audio = CpalTransport::start(AudioSettings::default())?;
    let input_mode = InputMode::from_mode(tunnel_input_mode)?;
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
    let ptt_max_frames = push_to_talk_max_recording_frames()?;
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

#[cfg(any(windows, target_os = "linux"))]
const DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS: u64 = 300_000;
#[cfg(any(windows, target_os = "linux"))]
const PUSH_TO_TALK_FRAME_MS: u64 = 20;
#[cfg(any(windows, target_os = "linux"))]
const PUSH_TO_TALK_LIMIT_WARNING_MS: u64 = 3_000;
#[cfg(any(windows, target_os = "linux"))]
const PUSH_TO_TALK_MIN_RECORDING_FRAMES: usize = 10;

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
    if ms == 0 {
        return Err("AURA_PUSH_TO_TALK_MAX_RECORDING_MS must be greater than 0".into());
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
fn push_to_talk_control_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("AURA_PUSH_TO_TALK_CONTROL_PATH").filter(|s| !s.is_empty())
    {
        return std::path::PathBuf::from(path);
    }
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return std::path::PathBuf::from(runtime_dir).join("aura-ptt.sock");
    }
    std::env::temp_dir().join("aura-ptt.sock")
}

#[cfg(target_os = "linux")]
fn send_push_to_talk_toggle() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    let path = push_to_talk_control_path();
    let mut stream = std::os::unix::net::UnixStream::connect(&path).map_err(|err| {
        format!(
            "failed to reach Linux push-to-talk control socket {}: {err}. Start an Aura call with AURA_INPUT_MODE=push_to_talk first.",
            path.display()
        )
    })?;
    stream.write_all(b"toggle\n")?;
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
/// or stdin. A target repository must not be able to redirect `aura-cli` by
/// planting `.env`.
fn load_dotenv() {
    load_dotenv_file(std::path::Path::new(".env"));
    if let Some(dir) = global_config_dir() {
        load_dotenv_file(&dir.join(".env"));
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

fn load_dotenv_file(path: &std::path::Path) {
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
        if !is_cli_dotenv_key_allowed(key) {
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

fn is_cli_dotenv_key_allowed(key: &str) -> bool {
    matches!(
        key,
        "AURA_AEC"
            | "AURA_INPUT_MODE"
            | "AURA_PUSH_TO_TALK_HOTKEY"
            | "AURA_PUSH_TO_TALK_CONTROL_PATH"
            | "AURA_PUSH_TO_TALK_MAX_RECORDING_MS"
    )
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
    use super::reject_unsupported_input_mode;
    use aura_tunnel::TunnelInputMode;

    #[test]
    fn ptt_mode_is_rejected_only_where_unsupported() {
        #[cfg(any(windows, target_os = "linux"))]
        assert!(reject_unsupported_input_mode(TunnelInputMode::PushToTalk).is_ok());
        #[cfg(not(any(windows, target_os = "linux")))]
        assert!(reject_unsupported_input_mode(TunnelInputMode::PushToTalk).is_err());
        assert!(reject_unsupported_input_mode(TunnelInputMode::Voice).is_ok());
    }
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::{
        global_config_dir_from, is_cli_dotenv_key_allowed, load_dotenv_file,
        parse_push_to_talk_max_recording_ms, push_to_talk_limit_warning_frame, resolve_ptt_toggles,
        PttBatchAction, DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS,
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
        assert!(is_cli_dotenv_key_allowed("AURA_INPUT_MODE"));
        assert!(is_cli_dotenv_key_allowed("AURA_PUSH_TO_TALK_HOTKEY"));
        assert!(is_cli_dotenv_key_allowed(
            "AURA_PUSH_TO_TALK_MAX_RECORDING_MS"
        ));
        assert!(is_cli_dotenv_key_allowed("AURA_AEC"));
        assert!(!is_cli_dotenv_key_allowed("AURA_CONNECT"));
        assert!(!is_cli_dotenv_key_allowed("XAI_API_KEY"));
        assert!(!is_cli_dotenv_key_allowed("OPENAI_API_KEY"));
        assert!(!is_cli_dotenv_key_allowed("AURA_REALTIME_URL"));
    }

    #[test]
    fn cli_dotenv_ignores_aura_connect_from_file() {
        unsafe {
            std::env::remove_var("AURA_CONNECT");
            std::env::remove_var("AURA_INPUT_MODE");
        }
        let tmp = std::env::temp_dir().join(format!("aura-cli-dotenv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir(&tmp).unwrap();
        let env_file = tmp.join(".env");
        std::fs::write(
            &env_file,
            "AURA_CONNECT=aura://attacker.invalid#k=bad&c=call-bad\nAURA_INPUT_MODE=push_to_talk\n",
        )
        .unwrap();

        load_dotenv_file(&env_file);

        assert!(std::env::var_os("AURA_CONNECT").is_none());
        assert_eq!(std::env::var("AURA_INPUT_MODE").unwrap(), "push_to_talk");
        unsafe {
            std::env::remove_var("AURA_INPUT_MODE");
        }
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
        assert!(parse_push_to_talk_max_recording_ms(Some("abc")).is_err());
    }

    #[test]
    fn ptt_toggle_batch_resolves_to_one_action() {
        assert_eq!(resolve_ptt_toggles(false, 1, 0, 10), PttBatchAction::Start);
        assert_eq!(resolve_ptt_toggles(true, 1, 10, 10), PttBatchAction::Send);
        assert_eq!(
            resolve_ptt_toggles(true, 1, 9, 10),
            PttBatchAction::DiscardTooShort
        );
        assert_eq!(
            resolve_ptt_toggles(false, 2, 0, 10),
            PttBatchAction::DiscardTooShort
        );
        // Three presses from idle: the intermediate open+close pair streamed
        // zero frames, so a single Start is the whole batch.
        assert_eq!(resolve_ptt_toggles(false, 3, 0, 10), PttBatchAction::Start);
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
