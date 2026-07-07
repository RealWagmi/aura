//! `aura-cli` — the thin voice client.
//!
//! It holds no engine, no `XAI_API_KEY`, no host adapter. It reads a connection
//! string (`AURA_CONNECT` or stdin — never argv), opens the Noise tunnel
//! to the server the host launched, and pumps cpal mic ↔ tunnel ↔ speaker. The
//! model, the chat context, the tools, and the key all live on the server.

use aura_audio::{AudioSettings, CpalTransport};
use aura_tunnel::{
    ConnectionString, IrohEndpoint, IrohPreset, TransportKind, TunnelConfig, TunnelEndpoint,
};
use std::sync::Arc;

#[cfg(windows)]
mod hotkey;

#[derive(Debug, Clone)]
enum InputMode {
    Voice,
    TogglePushToTalk(PushToTalkGate),
}

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
                 for push_to_talk mode (default ctrl+space)",
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
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>>;
}

impl VoiceTunnel for TunnelEndpoint {
    fn send_pcm24(&self, pcm: &[i16]) {
        TunnelEndpoint::send_pcm24(self, pcm);
    }
    async fn recv_pcm24(&mut self) -> Option<Vec<i16>> {
        TunnelEndpoint::recv_pcm24(self).await
    }
}

impl VoiceTunnel for IrohEndpoint {
    fn send_pcm24(&self, pcm: &[i16]) {
        IrohEndpoint::send_pcm24(self, pcm);
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
    let mut ptt_recording = false;
    let mut ptt_seen_presses = match &input_mode {
        InputMode::Voice => 0,
        InputMode::TogglePushToTalk(gate) => gate.press_count(),
    };
    let mut ptt_buffer: Vec<Vec<i16>> = Vec::new();
    const MAX_PUSH_TO_TALK_BUFFER_FRAMES: usize = 1_500;
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
                    InputMode::TogglePushToTalk(gate) => {
                        let presses = gate.press_count();
                        if presses != ptt_seen_presses {
                            let toggles = presses.saturating_sub(ptt_seen_presses);
                            ptt_seen_presses = presses;
                            for _ in 0..toggles {
                                if ptt_recording {
                                    ptt_recording = false;
                                    for buffered in ptt_buffer.drain(..) {
                                        tunnel.send_pcm24(&buffered);
                                    }
                                    eprintln!("aura: sent push-to-talk message.");
                                } else {
                                    ptt_buffer.clear();
                                    ptt_recording = true;
                                    audio.clear_playout();
                                    eprintln!("aura: recording push-to-talk message.");
                                }
                            }
                        }

                        if ptt_recording {
                            if ptt_buffer.len() >= MAX_PUSH_TO_TALK_BUFFER_FRAMES {
                                ptt_buffer.remove(0);
                            }
                            ptt_buffer.push(frame.clone());
                        }
                        tunnel.send_pcm24(&vec![0; frame.len()]);
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
