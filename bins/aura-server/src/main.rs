//! `aura-server` — the unified call server.
//!
//! The host launches it (on `127.0.0.1` for LOCAL, on a VPS for REMOTE). It
//! holds the `XAI_API_KEY` + the engine + the host `Brief` + the in-call tools,
//! mints a per-call session secret, prints a **connection string**, and on the
//! client's Noise handshake bridges the tunnel audio into the SAME
//! `aura_engine::CallSession::run` (the engine never knew about transports).

use std::sync::Arc;

use aura_engine::{AmbientFeeder, CallSession};
use aura_hosts::{resolve_host, HostAdapter};
use aura_voice::compose::instructions_from_brief;
use aura_voice::{VoiceProvider, VoiceSessionConfig, XaiRealtimeProvider};

use aura_tunnel::{
    ConnectionString, IrohPreset, IrohServer, IrohTransport, SessionSecret, TunnelConfig,
    TunnelServer, TunnelTransport,
};

/// Base persona prepended to the composed chat context.
const PERSONA: &str = "You are Aura, the developer's voice companion. THE CALL IS \
    ALREADY LIVE — you are connected and talking WITH the developer right now, \
    this very moment. Speak naturally, warmly, and concisely — a phone-style \
    conversation, not a document. The recent coding-chat context below is \
    BACKGROUND (so you don't have to ask \"what are we working on?\"); it may even \
    describe how THIS very call was set up — launching a server, a connection \
    string or link. That setup is already DONE. So NEVER offer to start a call, \
    to \"bring up/launch a server\", to send a link or connection string, and \
    never ask \"ready?\" as if you were still connecting — you are ALREADY on the \
    call, mid-conversation; just keep talking. Never read code, file paths, line \
    numbers, URLs, or stack traces aloud — paraphrase instead. If the developer \
    interrupts you, stop talking immediately and listen.\n\n\
    You have TOOLS — use them whenever the developer gives a command, exactly \
    like they would in a text chat. `start_agent_task` dispatches coding work \
    to their coding agent, which runs in the current repository with full \
    read/edit/bash access (use it for \"look at X\", \"fix Y\", \"run the \
    tests\", \"change Z\"). `ask_worker_question` asks it a read-only question \
    about the codebase. Tell the developer briefly what you're doing, dispatch, \
    then paraphrase the result when it comes back. If you dispatch a long task \
    and there's nothing to discuss meanwhile, call `pause_call_until` \
    (until='task_complete'). Call `end_voice_session` when they say goodbye.";

/// Instruction budget in tokens.
const INSTRUCTION_BUDGET_TOKENS: u32 = 6_000;

/// The fixed default UDP port for the tunnel.
/// One predictable port the operator opens once at onboarding, rather than a
/// per-call random port. Overridable via `AURA_PORT`; the full `ip:port` bind
/// via `AURA_BIND`.
const DEFAULT_PORT: u16 = 47821;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("aura-server: {err}");
        std::process::exit(1);
    }
    // Single-call server: terminate promptly once the call (and its post-call
    // summary, already delivered inside `run`) is done. iroh spawns background
    // tasks / OS threads (relay, magicsock) that can keep the process alive after
    // `run()` returns, so exit explicitly instead of waiting on them.
    std::process::exit(0);
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    load_dotenv();
    // Fail fast if the BYOK key is absent (before binding / minting).
    aura_voice::xai::resolve_xai_key()?;

    let cwd = std::env::current_dir()?;
    // Resolve the host this server was launched for: the launching
    // chat/session is the server's identity. Claude is the default.
    let host: Arc<dyn HostAdapter> = resolve_host(&cwd);
    eprintln!("aura-server: host = {}.", host.kind().as_str());
    // Fail-open: a thin/empty brief still composes valid instructions and dials.
    let brief = host.read_context().await.unwrap_or_default();
    let msg_count = brief.context.recent_messages_verbatim.len();
    eprintln!("aura-server: composed context from {msg_count} recent message(s).");

    let provider = XaiRealtimeProvider::new();
    let cfg = VoiceSessionConfig {
        instructions: instructions_from_brief(PERSONA, &brief, INSTRUCTION_BUDGET_TOKENS),
        voice: provider.default_voice().to_owned(),
        tools: aura_core::local_function_schemas(),
        latency_target_ms: 800,
        temperature: Some(0.5),
        end_of_turn_timeout_ms: None,
        output_speed: None,
        cold_start_kick: true,
    };

    let call_id = mint_call_id();
    let secret = SessionSecret::generate()?;
    write_status("ringing", &call_id, None);

    // Transport selection. Default `direct` = Noise/UDP (needs a
    // reachable host:port). `AURA_TRANSPORT=iroh` = QUIC P2P for a NAT/CGNAT
    // server (holepunch + blind relay fallback; no firewall port to open).
    let use_iroh = std::env::var("AURA_TRANSPORT")
        .map(|v| v.trim().eq_ignore_ascii_case("iroh"))
        .unwrap_or(false);

    let transport: Box<dyn aura_engine::AudioTransport> = if use_iroh {
        // The connection string carries the server's EndpointId; the client
        // resolves the address via iroh discovery. No `AURA_PUBLIC_HOST`/port.
        let server = IrohServer::bind(IrohPreset::Production).await?;
        let endpoint_id = server.endpoint_id().to_string();
        let conn = ConnectionString::format_iroh(&endpoint_id, &call_id, &secret);
        eprintln!("aura-server: iroh transport; endpoint {endpoint_id}; call {call_id}.");
        eprintln!("aura-server: GIVE THE CALLER THIS CONNECTION STRING (single-use, valid ~120s):");
        eprintln!("    AURA_CONNECT='{conn}' aura-cli");
        let endpoint = server.accept(&secret, TunnelConfig::default()).await?;
        eprintln!("aura-server: client connected; bridging to the model.");
        Box::new(IrohTransport::new(endpoint))
    } else {
        // Direct Noise/UDP on a FIXED, predictable port (the firewall is opened
        // once for THIS port at onboarding). `AURA_PUBLIC_HOST` is what the
        // client dials and selects the bind interface: loopback-only for a LOCAL
        // call, all-interfaces for a REMOTE VPS. `AURA_BIND` (full `ip:port`) is
        // an advanced override that wins outright.
        let public_host =
            std::env::var("AURA_PUBLIC_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
        let port: u16 = match std::env::var("AURA_PORT") {
            Ok(v) => v
                .trim()
                .parse()
                .map_err(|_| format!("AURA_PORT must be a port number 1..=65535, got {v:?}"))?,
            Err(_) => DEFAULT_PORT,
        };
        let bind = std::env::var("AURA_BIND").unwrap_or_else(|_| {
            if is_loopback_host(&public_host) {
                format!("127.0.0.1:{port}")
            } else {
                format!("0.0.0.0:{port}")
            }
        });
        let server = TunnelServer::bind(&bind).await?;
        let local = server.local_addr()?;
        let authority = format!("{public_host}:{}", local.port());
        let conn = ConnectionString::format_direct(&authority, &call_id, &secret);
        eprintln!("aura-server: tunnel UDP bound on {local}; call {call_id}.");
        if !is_loopback_host(&public_host) {
            eprintln!(
                "aura-server: REMOTE — clients reach {authority} over UDP; ensure port {} is open \
                 (open it once at onboarding via scripts/aura-open-port.sh).",
                local.port()
            );
        }
        eprintln!("aura-server: GIVE THE CALLER THIS CONNECTION STRING (single-use, valid ~120s):");
        eprintln!("    AURA_CONNECT='{conn}' aura-cli");
        let endpoint = server.accept(&secret, TunnelConfig::default()).await?;
        eprintln!("aura-server: client connected; bridging to the model.");
        Box::new(TunnelTransport::new(endpoint))
    };
    // The caller completed the handshake — the call is live.
    write_status("active", &call_id, None);

    let provider: Arc<dyn VoiceProvider> = Arc::new(provider);
    // Live ambient context — opt-in via `AURA_FEEDER`.
    let feeder = maybe_start_feeder(&cwd).await;

    // Run the call. On ANY end (caller hung up, model called `end_voice_session`,
    // provider fatal) the engine returns and we record the terminal state; the
    // post-call summary was already delivered inside `run`. `main` then exits.
    let outcome = match CallSession::run(transport, provider, host, feeder, cfg).await {
        Ok(o) => o,
        Err(e) => {
            write_status("failed", &call_id, Some(&e.to_string()));
            return Err(e.into());
        }
    };
    write_status("ended", &call_id, Some(&format!("{:?}", outcome.reason)));
    eprintln!("aura-server: call ended ({:?}).", outcome.reason);
    Ok(())
}

/// Best-effort write of the call's lifecycle state to `.aura/call-status.json`
/// so the launching host can MONITOR the call (poll this file ~every 10 s) and
/// still get a verdict if the server crashes (the recorded `pid` going away
/// while the state is non-terminal = a drop). States: `ringing` (server up,
/// waiting for the caller) → `active` (caller connected, call in progress) →
/// `ended` / `failed` (+ `reason`). Hand-rolled JSON (no serde dep); `reason` is
/// sanitized so it can't break the JSON.
fn write_status(state: &str, call_id: &str, reason: Option<&str>) {
    let reason = reason
        .unwrap_or("")
        .replace('\\', "/")
        .replace('"', "'")
        .replace(['\n', '\r'], " ");
    let body = format!(
        "{{\"call_id\":\"{call_id}\",\"pid\":{},\"state\":\"{state}\",\"reason\":\"{reason}\"}}\n",
        std::process::id()
    );
    let _ = std::fs::create_dir_all(".aura");
    let _ = std::fs::write(".aura/call-status.json", body);
}

/// Is `host` a loopback name/address? A LOCAL call then binds loopback-only so
/// the tunnel port is never exposed beyond the machine; a REMOTE host binds all
/// interfaces. Conservative: anything not clearly loopback is treated as remote.
fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// A short, opaque, never-reused call id (4 random bytes as hex).
fn mint_call_id() -> String {
    let mut b = [0u8; 4];
    let _ = getrandom::getrandom(&mut b);
    format!("call-{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3])
}

/// Start the live ambient-context feeder if opted in via `AURA_FEEDER`
/// (`1`/`true`/`on`/`yes`). Best-effort: if `claude` is not on `PATH` the call
/// proceeds with the startup brief but no live deltas.
async fn maybe_start_feeder(cwd: &std::path::Path) -> Option<Arc<dyn AmbientFeeder>> {
    let enabled = std::env::var("AURA_FEEDER")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "on" || v == "yes"
        })
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    match aura_feeder::Feeder::start_for_call(cwd).await {
        Ok(feeder) => {
            eprintln!("aura-server: live ambient feeder on.");
            Some(Arc::new(feeder) as Arc<dyn AmbientFeeder>)
        }
        Err(err) => {
            eprintln!("aura-server: ambient feeder unavailable ({err}); continuing without it.");
            None
        }
    }
}

/// Minimal `.env` loader: `KEY=VALUE` from `./.env`, quotes stripped, never
/// overriding an already-set var. Keeps `XAI_API_KEY` out of argv/shell history.
fn load_dotenv() {
    let Ok(content) = std::fs::read_to_string(".env") else {
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
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}
