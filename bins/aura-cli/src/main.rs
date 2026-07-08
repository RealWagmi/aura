//! `aura-cli` — the thin voice client.
//!
//! It holds no engine, no `XAI_API_KEY`, no host adapter. It reads a connection
//! string (`AURA_CONNECT` or stdin — never argv), opens the Noise tunnel
//! to the server the host launched, and pumps cpal mic ↔ tunnel ↔ speaker. The
//! model, the chat context, the tools, and the key all live on the server.

use aura_audio::{AudioSettings, CpalTransport};
use aura_tunnel::{
    ConnectionString, IrohEndpoint, IrohPreset, TransportKind, TunnelConfig, TunnelControl,
    TunnelEndpoint,
};
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
mod hotkey;

#[derive(Debug, Clone)]
enum InputMode {
    Voice,
    #[cfg(windows)]
    TogglePushToTalk(PushToTalkGate),
}

#[cfg(windows)]
#[derive(Debug, Clone)]
struct PushToTalkGate {
    state: Arc<std::sync::atomic::AtomicU64>,
    label: String,
}

impl InputMode {
    fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let mode = std::env::var("AURA_INPUT_MODE").unwrap_or_else(|_| "voice".to_owned());
        match mode.trim().to_ascii_lowercase().as_str() {
            "" | "voice" | "vad" => Ok(Self::Voice),
            "push_to_talk" | "push-to-talk" | "ptt" => start_push_to_talk(),
            other => {
                Err(format!("AURA_INPUT_MODE must be voice or push_to_talk, got {other:?}").into())
            }
        }
    }
}

#[cfg(windows)]
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

#[cfg(not(windows))]
fn start_push_to_talk() -> Result<InputMode, Box<dyn std::error::Error>> {
    Err("AURA_INPUT_MODE=push_to_talk currently supports Windows global hotkeys only".into())
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
                 Options:\n  \
                 -V, --version   print the version and exit\n  \
                 -h, --help      show this help and exit\n\n\
                 Environment:\n  \
                 AURA_CONNECT    the connection string — either form:\n                  \
                 direct: aura://HOST:PORT#k=...&c=...\n                  \
                 iroh:   aura://<node-id>#k=...&c=...&t=iroh (server behind NAT)\n  \
                 AURA_AEC        echo handling on the mic: on (default, AEC3 echo\n                  \
                 cancellation — speakers + barge-in work), gate (mute mic while\n                  \
                 the model speaks; no barge-in), off (raw mic; headsets only)\n  \
                 AURA_INPUT_MODE voice (default) or push_to_talk\n  \
                 AURA_PUSH_TO_TALK_HOTKEY Windows global toggle hotkey\n                  \
                 for push_to_talk mode (default ctrl+space)\n  \
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
            pump(tunnel).await?;
        }
        TransportKind::Iroh => {
            let tunnel = IrohEndpoint::connect_by_id(
                &conn.authority,
                &conn.secret,
                IrohPreset::Production,
                cfg,
            )
            .await?;
            pump(tunnel).await?;
        }
    }

    eprintln!("aura: call ended.");
    Ok(())
}

/// One audio-tunnel surface regardless of transport (direct Noise/UDP or iroh).
#[allow(async_fn_in_trait)]
trait VoiceTunnel {
    fn send_pcm24(&self, pcm: &[i16]);
    fn send_control(&self, control: TunnelControl);
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>>;
}

impl VoiceTunnel for TunnelEndpoint {
    fn send_pcm24(&self, pcm: &[i16]) {
        TunnelEndpoint::send_pcm24(self, pcm);
    }
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
async fn pump<T: VoiceTunnel>(mut tunnel: T) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("aura: tunnel up. Acquiring microphone and speaker…");
    let mut audio = CpalTransport::start(AudioSettings::default())?;
    let input_mode = InputMode::from_env()?;
    #[cfg(windows)]
    let mut ptt_recording = false;
    #[cfg(windows)]
    let mut ptt_seen_presses = match &input_mode {
        InputMode::Voice => 0,
        InputMode::TogglePushToTalk(gate) => gate.press_count(),
    };
    #[cfg(windows)]
    let mut ptt_frames_open = 0usize;
    #[cfg(windows)]
    let ptt_max_frames = push_to_talk_max_recording_frames()?;
    #[cfg(windows)]
    let mut ptt_warned_near_limit = false;
    #[cfg(windows)]
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
                    #[cfg(windows)]
                    InputMode::TogglePushToTalk(gate) => {
                        let presses = gate.press_count();
                        if presses != ptt_seen_presses {
                            let toggles = presses.saturating_sub(ptt_seen_presses);
                            ptt_seen_presses = presses;
                            for _ in 0..toggles {
                                if ptt_recording {
                                    ptt_recording = false;
                                    ptt_frames_open = 0;
                                    ptt_warned_near_limit = false;
                                    tunnel.send_control(TunnelControl::PttClose);
                                    eprintln!("aura: sent push-to-talk message.");
                                } else {
                                    ptt_recording = true;
                                    ptt_frames_open = 0;
                                    ptt_warned_near_limit = false;
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
                                    "aura: push-to-talk recording reached the limit. You can increase it with AURA_PUSH_TO_TALK_MAX_RECORDING_MS."
                                );
                                speak_push_to_talk_limit_warning();
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

#[cfg(windows)]
const DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS: u64 = 300_000;
#[cfg(windows)]
const PUSH_TO_TALK_FRAME_MS: u64 = 20;
#[cfg(windows)]
const PUSH_TO_TALK_LIMIT_WARNING_MS: u64 = 3_000;

#[cfg(windows)]
fn push_to_talk_max_recording_frames() -> Result<usize, Box<dyn std::error::Error>> {
    parse_push_to_talk_max_recording_ms(
        std::env::var("AURA_PUSH_TO_TALK_MAX_RECORDING_MS")
            .ok()
            .as_deref(),
    )
}

#[cfg(windows)]
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

#[cfg(windows)]
fn recording_ms_to_frames(ms: u64) -> Result<usize, Box<dyn std::error::Error>> {
    let frames = ms.div_ceil(PUSH_TO_TALK_FRAME_MS);
    usize::try_from(frames).map_err(|_| "AURA_PUSH_TO_TALK_MAX_RECORDING_MS is too large".into())
}

#[cfg(windows)]
fn push_to_talk_limit_warning_frame(max_frames: usize) -> usize {
    let warning_frames = recording_ms_to_frames(PUSH_TO_TALK_LIMIT_WARNING_MS).unwrap_or(150);
    max_frames.saturating_sub(warning_frames).max(1)
}

#[cfg(windows)]
fn speak_push_to_talk_limit_warning() {
    let script = "Add-Type -AssemblyName System.Speech; \
        $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
        $s.Speak('You reached the voice message limit. You can increase it in settings.')";
    let _ = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .spawn();
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

/// Minimal `.env` loader. Keep this in sync with `aura-server`: the client must
/// see settings such as `AURA_INPUT_MODE=push_to_talk`, or server/client mode can
/// split-brain.
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

#[cfg(all(test, windows))]
mod tests {
    use super::{
        global_config_dir_from, parse_push_to_talk_max_recording_ms,
        push_to_talk_limit_warning_frame, DEFAULT_PUSH_TO_TALK_MAX_RECORDING_MS,
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
}
