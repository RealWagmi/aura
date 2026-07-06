//! `aura-server` — the unified call server.
//!
//! The host launches it (on `127.0.0.1` for LOCAL, on a VPS for REMOTE). It
//! holds the `XAI_API_KEY` + the engine + the host `Brief` + the in-call tools,
//! mints a per-call session secret, prints a **connection string**, and on the
//! client's Noise handshake bridges the tunnel audio into the SAME
//! `aura_engine::CallSession::run` (the engine never knew about transports).

use std::sync::Arc;

use aura_engine::inbox::{Inbox, InboxTask};
use aura_engine::{AmbientFeeder, CallSession};
use aura_hosts::{resolve_host, HostAdapter};
use aura_voice::compose::instructions_from_brief;
use aura_voice::{VoiceProvider, VoiceSessionConfig, XaiRealtimeProvider};

use aura_tunnel::{
    ConnectionString, IrohPreset, IrohServer, IrohTransport, SessionSecret, TunnelConfig,
    TunnelError, TunnelServer, TunnelTransport,
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
    (until='task_complete'). Call `end_voice_session` when they say goodbye.\n\n\
    STAY GROUNDED. You may speak freely from exactly two sources: the context \
    you were given (the background below plus what is said on this call) and \
    general knowledge for casual conversation. For ANYTHING project-specific \
    that is not in that context — code details, file contents, configs, \
    current task or repo status, logs, data that would need looking up — do \
    NOT answer from memory and do NOT improvise: route it to the worker \
    (`ask_worker_question` for lookups, `start_agent_task` for work) and relay \
    what comes back. If you are not sure whether you actually know, dispatch \
    instead of guessing — a short wait beats a confident wrong answer.";

/// Instruction budget in tokens.
const INSTRUCTION_BUDGET_TOKENS: u32 = 6_000;

/// The fixed default UDP port for the tunnel.
/// One predictable port the operator opens once at onboarding, rather than a
/// per-call random port. Overridable via `AURA_PORT`; the full `ip:port` bind
/// via `AURA_BIND`.
const DEFAULT_PORT: u16 = 47821;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // `--version` / `--help` must be handled BEFORE any call setup, so an
        // unrecognized flag never silently boots a call server.
        Some("-v" | "-V" | "--version") => {
            println!("aura-server {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Some("-h" | "--help") => {
            print_server_help();
            std::process::exit(0);
        }
        // Scheme 2 orchestrator helper: `aura-server inbox <cmd>` is the live
        // host chat session's side of the in-call dispatch inbox — a short-lived
        // subcommand that must NOT stand up a call.
        Some("inbox") => std::process::exit(run_inbox_cli(&args[2..]).await),
        // No args → normal launch (configured entirely via env vars).
        None => {}
        // ANY other argument — a stray flag OR a positional typo (e.g. `inbxo`) —
        // is a mistake. Do NOT fall through to launching a call: a booted server
        // would clobber a live call's `call-status.json` and could reap it. The
        // server takes no positional arguments; the only subcommand is `inbox`.
        Some(other) => {
            eprintln!(
                "aura-server: unrecognized argument {other:?}; the server takes no positional \
                 arguments (only the `inbox` subcommand). Try `aura-server --help`."
            );
            std::process::exit(2);
        }
    }
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

/// Short usage. The server takes no positional arguments — a call is configured
/// entirely via environment variables (the host launches it) — so this only
/// documents the flags, the `inbox` subcommand, and the key env knobs.
fn print_server_help() {
    println!(
        "aura-server {} — the unified voice-call server (holds the key, engine, context, tools).\n\n\
         The host launches it per call; you don't normally run it by hand. It is\n\
         configured via environment variables, not arguments.\n\n\
         Usage:\n  \
         aura-server                 launch a call server (config read from the environment)\n  \
         aura-server inbox <cmd>     orchestrator inbox helper: wait | done | stall | alive\n\n\
         Options:\n  \
         -V, --version   print the version and exit\n  \
         -h, --help      show this help and exit\n\n\
         Key environment variables:\n  \
         XAI_API_KEY           BYOK key (env / OS keychain / ./.env) — required\n  \
         AURA_PUBLIC_HOST      host clients dial (default 127.0.0.1 = LOCAL)\n  \
         AURA_PORT             UDP port (default 47821)\n  \
         AURA_TRANSPORT=iroh   NAT/CGNAT P2P transport (no open port needed)\n  \
         AURA_DISPATCH_MODEL   pin the in-call dispatch model\n  \
         AURA_FEEDER=1         opt in to the live ambient feeder\n\n\
         Connection string printed for the caller (single-use, ~120 s):\n  \
         direct:  aura://HOST:PORT#k=<secret>&c=<call>\n  \
         iroh:    aura://<node-id>#k=<secret>&c=<call>&t=iroh   (server behind NAT)",
        env!("CARGO_PKG_VERSION")
    );
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
        // Bind with startup self-healing so a busy port never needs manual
        // clearing (by the operator or the launching AI). `AURA_BIND` is the
        // advanced override and wins outright (no self-heal). Otherwise the
        // server reaps a stale server / hops to a free port on loopback — see
        // `bind_direct_udp`. Hopping is gated to loopback: for a REMOTE VPS the
        // firewall is opened for exactly ONE port, so a hop would make the server
        // unreachable (iroh has no such fixed-port constraint and never lands here).
        let server = if let Ok(explicit) = std::env::var("AURA_BIND") {
            TunnelServer::bind(&explicit).await?
        } else {
            let bind_ip = if is_loopback_host(&public_host) {
                "127.0.0.1"
            } else {
                "0.0.0.0"
            };
            bind_direct_udp(bind_ip, port, is_loopback_host(&public_host)).await?
        };
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

/// The orchestrator's side of the Scheme 2 coordination layer, exposed as
/// `aura-server inbox <cmd>`. The live host chat session (driven by its skill
/// watch-loop) calls these to consume voice-dispatched tasks and report results.
/// The inbox is the SAME `.aura/inbox/` the running call server posts to — both
/// are co-located in the project dir (the thin `aura-cli` is the only remote
/// part of a REMOTE call, so this file channel always works between them).
///
/// Subcommands:
///   * `wait [--timeout SECS]` — block until a task is pending (claiming it),
///     refreshing the liveness heartbeat each tick; prints the claimed task, or
///     `NO_TASK` on timeout (default 30s). This is the low-latency wake — a
///     sub-second tick, NOT a cron poll.
///   * `done <id> <speech...>`  — report a task finished (spoken back into the call).
///   * `stall <id> <speech...>` — report a task abandoned (aura then dispatches it directly).
///   * `alive`                  — refresh the heartbeat once (arm the orchestrator at call start).
async fn run_inbox_cli(args: &[String]) -> i32 {
    // Validate the subcommand FIRST — before creating `.aura/inbox/` — so a typo
    // or `--help` prints usage and exits WITHOUT mutating the filesystem.
    let sub = args.first().map(String::as_str);
    match sub {
        Some("wait" | "done" | "stall" | "alive") => {}
        Some("-h" | "--help") => {
            print_inbox_help();
            return 0;
        }
        other => {
            eprintln!(
                "aura-server inbox: unknown subcommand {other:?}; expected wait|done|stall|alive"
            );
            return 2;
        }
    }
    let inbox = match Inbox::open(std::path::Path::new(".")) {
        Ok(inbox) => inbox,
        Err(e) => {
            eprintln!("aura-server inbox: cannot open .aura/inbox ({e})");
            return 1;
        }
    };
    match sub {
        Some("wait") => inbox_wait(&inbox, &args[1..]).await,
        Some("done") => inbox_terminal(&inbox, &args[1..], false),
        Some("stall") => inbox_terminal(&inbox, &args[1..], true),
        Some("alive") => match inbox.touch_heartbeat() {
            Ok(()) => {
                // Print the ABSOLUTE inbox dir the loop is arming: it MUST match
                // the directory the running call server posts to (the same cwd),
                // so an operator can spot a cwd mismatch instead of silent no-ops.
                let shown = std::fs::canonicalize(inbox.dir())
                    .unwrap_or_else(|_| inbox.dir().to_path_buf());
                println!("ALIVE {}", shown.display());
                0
            }
            Err(e) => {
                eprintln!("aura-server inbox alive: {e}");
                1
            }
        },
        // Unreachable: the subcommand was validated above.
        _ => 2,
    }
}

/// Usage for the `inbox` subcommand family (the orchestrator's watch-loop side).
fn print_inbox_help() {
    println!(
        "aura-server inbox <cmd> — the live orchestrator's side of the in-call dispatch inbox.\n\n\
         Subcommands:\n  \
         wait [--timeout SECS]   block until a task is pending (claiming it), refreshing the\n                          \
         heartbeat each tick; prints the task, or NO_TASK on timeout (default 30s)\n  \
         done <id> <speech...>   report a task finished (spoken back into the call)\n  \
         stall <id> <speech...>  report a task abandoned (aura then dispatches it directly)\n  \
         alive                   refresh the heartbeat once and print the inbox directory"
    );
}

/// Block until a task is pending (claiming it), refreshing the heartbeat each
/// tick, or until `--timeout SECS` elapses. Prints the claimed task, else `NO_TASK`.
async fn inbox_wait(inbox: &Inbox, args: &[String]) -> i32 {
    /// Cap the wait so an absurd `--timeout` (e.g. `u64::MAX`) cannot overflow
    /// `Instant + Duration` (a panic) and no single `wait` blocks the loop for
    /// more than an hour — well beyond any live call.
    const MAX_WAIT_SECS: u64 = 3600;
    let timeout_secs = parse_timeout_secs(args)
        .unwrap_or(30)
        .clamp(1, MAX_WAIT_SECS);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        // Declare/refresh liveness so the running call server routes to us.
        let _ = inbox.touch_heartbeat();
        let mut claimed = None;
        for task in inbox.pending_tasks() {
            match inbox.claim(&task.id) {
                // Won the O_EXCL arbiter — this task is ours to execute.
                Ok(true) => {
                    claimed = Some(task);
                    break;
                }
                // Lost the race: aura recovered it (or another waiter took it)
                // between our pending read and the claim. Executing it anyway
                // would be a double execution — try the next pending task.
                Ok(false) => continue,
                Err(e) => {
                    eprintln!("aura-server inbox wait: claim failed ({e})");
                    return 1;
                }
            }
        }
        if let Some(task) = claimed {
            print_inbox_task(&task);
            return 0;
        }
        if tokio::time::Instant::now() >= deadline {
            println!("NO_TASK");
            return 0;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Print a claimed task in a format the orchestrator can act on (and that keeps
/// the id on its own line for the follow-up `done`/`stall` call).
///
/// Every field is flattened to a single physical line: an embedded newline in a
/// task field would otherwise forge extra `TASK`/`INTENT:`/`DONE` lines in the
/// orchestrator's stdout parse (a protocol-injection → wrong-`DONE` / stalled
/// call). The fields are already `redact_secrets`'d upstream; this closes the
/// structural (newline) channel.
fn print_inbox_task(task: &InboxTask) {
    println!("TASK {}", one_line(&task.id));
    println!("INTENT: {}", one_line(&task.user_intent));
    if !task.constraints.is_empty() {
        let joined = task
            .constraints
            .iter()
            .map(|c| one_line(c))
            .collect::<Vec<_>>()
            .join(" | ");
        println!("CONSTRAINTS: {joined}");
    }
    if !task.project.is_empty() {
        println!("PROJECT: {}", one_line(&task.project));
    }
}

/// Collapse any newline/carriage-return (and other ASCII control chars) in a task
/// field to a single space, so a field can never inject a forged protocol line
/// into the orchestrator's line-oriented stdout parse.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `done <id> <speech...>` / `stall <id> <speech...>`. The speech is
/// `redact_secrets`'d as defense in depth before it lands in the inbox (the call
/// server relays it into the call verbatim).
fn inbox_terminal(inbox: &Inbox, args: &[String], stall: bool) -> i32 {
    let verb = if stall { "stall" } else { "done" };
    let Some(id) = args.first() else {
        eprintln!("aura-server inbox: usage: inbox {verb} <id> <speech...>");
        return 2;
    };
    let speech = aura_core::redact_secrets(&args[1..].join(" "));
    let result = if stall {
        inbox.mark_stall(id, &speech)
    } else {
        inbox.mark_done(id, &speech)
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("aura-server inbox {verb}: write failed ({e})");
            1
        }
    }
}

/// Parse `--timeout SECS` / `--timeout=SECS` out of the args (first wins).
fn parse_timeout_secs(args: &[String]) -> Option<u64> {
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--timeout" {
            return it.next().and_then(|v| v.parse().ok());
        }
        if let Some(v) = arg.strip_prefix("--timeout=") {
            return v.parse().ok();
        }
    }
    None
}

/// Bind the direct UDP tunnel socket, self-healing a busy port at startup so
/// neither the operator nor the launching AI ever has to clear it by hand. The
/// two modes are deliberately asymmetric:
///
/// * **Loopback (`allow_hop = true`, LOCAL):** on a busy port, HOP to the next
///   free port (`port+1`, `port+2`, …). The connection string carries the
///   actually-bound port, so the client always dials the right one. A loopback
///   server NEVER reaps: hopping is free and always available, so killing a
///   process would be gratuitous — and it removes every "racing launchers
///   SIGTERM each other" / "kill a live local call" hazard.
/// * **REMOTE (`allow_hop = false`):** the firewall is opened for exactly ONE
///   port, so hopping would make the server unreachable. Only here do we reclaim
///   the port, and ONLY from a process `lsof` PROVES is currently holding it AND
///   [`pid_is_aura_server`] positively confirms is an `aura-server` (never our
///   own pid). A pid we cannot prove holds the port is never signalled. If the
///   port can't be reclaimed, fail with a clear, actionable error.
///
/// (The iroh transport has no fixed-port constraint and never reaches this path.)
async fn bind_direct_udp(
    bind_ip: &str,
    port: u16,
    allow_hop: bool,
) -> Result<TunnelServer, Box<dyn std::error::Error>> {
    use std::io::ErrorKind;
    /// How many consecutive ports to probe when hopping on loopback.
    const MAX_HOPS: u16 = 16;

    // (1) The requested port.
    match TunnelServer::bind(&format!("{bind_ip}:{port}")).await {
        Ok(server) => return Ok(server),
        Err(TunnelError::Io(e)) if e.kind() == ErrorKind::AddrInUse => {
            eprintln!("aura-server: UDP port {port} is in use; self-healing…");
        }
        Err(e) => return Err(e.into()),
    }

    // (2) Loopback → hop to the next free port. No process is ever reaped here.
    if allow_hop {
        let last = port.saturating_add(MAX_HOPS);
        for candidate in port.saturating_add(1)..=last {
            match TunnelServer::bind(&format!("{bind_ip}:{candidate}")).await {
                Ok(server) => {
                    eprintln!(
                        "aura-server: port {port} busy → bound free port {candidate} (loopback)."
                    );
                    return Ok(server);
                }
                Err(TunnelError::Io(e)) if e.kind() == ErrorKind::AddrInUse => continue,
                Err(e) => return Err(e.into()),
            }
        }
        return Err(format!("no free UDP port found in {port}..={last} on {bind_ip}").into());
    }

    // (3) REMOTE → reclaim the fixed port from a PROVEN aura-server holder, then
    // retry the SAME port. The reaped holder releases its socket during kernel
    // teardown — usually sub-10ms, but scheduling latency is nondeterministic and
    // UDP has no SO_REUSEADDR here — so retry a few times over a short bounded
    // window rather than a single shot.
    if reap_port_holder(port).await {
        const REBIND_ATTEMPTS: u32 = 5;
        for attempt in 1..=REBIND_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
            match TunnelServer::bind(&format!("{bind_ip}:{port}")).await {
                Ok(server) => {
                    eprintln!(
                        "aura-server: reclaimed the port from a stale server; rebound {port}."
                    );
                    return Ok(server);
                }
                Err(TunnelError::Io(e)) if e.kind() == ErrorKind::AddrInUse => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    Err(format!(
        "UDP port {port} on {bind_ip} is already in use and no stale aura-server holding it could \
         be reclaimed. Find the process holding it with `ss -lunp 'sport = :{port}'` (or \
         `lsof -iUDP:{port}`) and stop it, then relaunch on THIS same port (the firewall is opened \
         for it; only change AURA_PORT if you also re-open the firewall)."
    )
    .into())
}

/// Async wrapper over [`try_reap_port_holder`]: runs the blocking `ps`/`lsof`/
/// `kill` probing off the runtime and bounds it with a timeout, so a hung
/// `ps`/`lsof` can never stall server startup. Returns false on timeout/join
/// error (treated as "did not reap" — the caller then errors).
async fn reap_port_holder(busy_port: u16) -> bool {
    let work = tokio::task::spawn_blocking(move || try_reap_port_holder(busy_port));
    match tokio::time::timeout(std::time::Duration::from_secs(3), work).await {
        Ok(Ok(reaped)) => reaped,
        _ => false,
    }
}

/// Positively verify `pid` is an `aura-server` process. **Fail-safe:** ANY
/// uncertainty (tool absent, non-zero exit, empty/unrecognized output) returns
/// `false`, so we never signal a process we cannot confirm.
///
/// Portable across Linux and macOS (POSIX `ps`), with a Linux `/proc` fast-path
/// (a direct kernel read, cheaper and safer than `fork`+`exec`). The key
/// cross-OS trap: `ps -o comm=` prints the executable BASENAME on Linux but the
/// FULL PATH on macOS — so we always basename-normalize and compare exactly
/// (never substring). A `ps -o args=` argv[0] fallback covers a `prctl`-renamed
/// comm and Linux's historical 15-char comm truncation.
#[cfg(unix)]
fn pid_is_aura_server(pid: u32) -> bool {
    const TARGET: &str = "aura-server";
    // First 15 bytes tolerate Linux's historical TASK_COMM_LEN truncation. TARGET
    // is ASCII (every byte a char boundary), so byte-slicing is safe; for the
    // 11-char "aura-server" this equals the full name.
    let want15 = &TARGET[..TARGET.len().min(15)];
    let is_match = |argv0: &str| -> bool {
        // macOS `comm=` is a full path, Linux a basename → take the last component.
        let base = argv0.rsplit('/').next().unwrap_or(argv0);
        base == TARGET || base == want15
    };

    // Linux fast-path: read /proc/<pid>/comm directly (no subprocess). A match is
    // conclusive; a present-but-mismatched comm falls through to the `ps args`
    // check (catches a prctl-renamed comm) rather than killing.
    if cfg!(target_os = "linux") {
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            if is_match(comm.trim()) {
                return true;
            }
        }
    }

    // Portable path (Linux without /proc, and macOS): POSIX `ps`. `comm` may
    // legally contain spaces, so read the whole `-o comm=` line; for `-o args=`
    // the first whitespace token is argv[0].
    let field_matches = |field: &str, first_token: bool| -> bool {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", field])
            .output()
        else {
            return false; // ps absent → not verified
        };
        if !out.status.success() {
            return false; // dead pid / not visible
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.trim();
        if line.is_empty() {
            return false;
        }
        let argv0 = if first_token {
            line.split_whitespace().next().unwrap_or(line)
        } else {
            line
        };
        is_match(argv0)
    };
    field_matches("comm=", false) || field_matches("args=", true)
}

#[cfg(not(unix))]
fn pid_is_aura_server(_pid: u32) -> bool {
    false // Windows: no portable process-name introspection here → never verified
}

/// Best-effort: PIDs holding UDP `port`, via `lsof -t -iUDP:<port>` (portable to
/// Linux and macOS; `-t` prints PIDs only, de-duped across IPv4/IPv6). Empty when
/// `lsof` is absent (spawn error) or nothing holds the port. The exit code is
/// deliberately IGNORED — `lsof` returns 1 for both "nothing found" and generic
/// errors, so only non-empty stdout signals holders. Each PID is a CANDIDATE that
/// must still pass [`pid_is_aura_server`] before any kill.
#[cfg(unix)]
fn udp_port_holder_pids(port: u16) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("lsof")
        .arg("-t")
        .arg(format!("-iUDP:{port}"))
        .output()
    else {
        return Vec::new(); // lsof absent → no candidates
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

/// Best-effort: SIGTERM the `aura-server` currently holding `busy_port`.
/// Candidates come SOLELY from `lsof` (proof the pid actually holds the port);
/// an unproven recorded pid is never signalled, so we can't kill a live server
/// that has merely hopped to a different port. **Fail-safe:** never our own pid,
/// and only a pid that [`pid_is_aura_server`] positively confirms — every
/// uncertainty resolves to "do nothing". `SIGTERM` only (graceful; no `SIGKILL`
/// escalation — a wedged holder falls through to the error, safer than
/// force-killing a possibly mislabelled process). Returns true only if a kill was
/// issued.
#[cfg(unix)]
fn try_reap_port_holder(busy_port: u16) -> bool {
    let me = std::process::id();
    for pid in udp_port_holder_pids(busy_port) {
        if pid == me {
            continue; // never reap ourselves
        }
        if !pid_is_aura_server(pid) {
            continue; // fail-safe: only a positively-verified aura-server
        }
        eprintln!("aura-server: reclaiming port {busy_port} from stale server pid {pid}.");
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        return true;
    }
    false
}

#[cfg(not(unix))]
fn try_reap_port_holder(_busy_port: u16) -> bool {
    false // Windows: hard no-op (no portable process-name/port introspection here)
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

/// Minimal `.env` loader. Loads `KEY=VALUE` lines (quotes stripped) and never
/// overrides an already-set variable, from two files in order:
///   1. `./.env` in the current working directory (where the server was started);
///   2. a fixed user-global file — `$AURA_HOME/.env`, else
///      `${XDG_CONFIG_HOME:-~/.config}/aura/.env`.
///
/// The working-directory file wins (loaded first; the global never overrides a
/// key already set), and a real environment variable beats both. The global
/// file is what lets one onboarding-written key resolve no matter which
/// directory the host launches `aura-server` from. Keeps `XAI_API_KEY` out of
/// argv / shell history.
fn load_dotenv() {
    load_dotenv_file(std::path::Path::new(".env"));
    if let Some(dir) = global_config_dir() {
        load_dotenv_file(&dir.join(".env"));
    }
}

/// The fixed user-global aura config directory (where a CWD-independent `.env`
/// lives). Reads the environment, then defers to [`global_config_dir_from`].
fn global_config_dir() -> Option<std::path::PathBuf> {
    global_config_dir_from(
        std::env::var_os("AURA_HOME"),
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")),
    )
}

/// Pure resolution rule (no process-env access, so it is race-free to test):
/// `AURA_HOME` wins outright; else `XDG_CONFIG_HOME/aura`; else
/// `<home>/.config/aura`. Empty values are ignored.
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

/// Load one `.env` file if it exists; never override an already-set variable.
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
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn global_config_dir_precedence() {
        // AURA_HOME wins outright.
        assert_eq!(
            global_config_dir_from(
                Some(OsString::from("/srv/aura")),
                Some(OsString::from("/x")),
                Some(OsString::from("/home/u")),
            ),
            Some(PathBuf::from("/srv/aura"))
        );
        // else XDG_CONFIG_HOME/aura.
        assert_eq!(
            global_config_dir_from(
                None,
                Some(OsString::from("/x")),
                Some(OsString::from("/home/u")),
            ),
            Some(PathBuf::from("/x/aura"))
        );
        // else <home>/.config/aura.
        assert_eq!(
            global_config_dir_from(None, None, Some(OsString::from("/home/u"))),
            Some(PathBuf::from("/home/u/.config/aura"))
        );
        // empty values are skipped.
        assert_eq!(
            global_config_dir_from(Some(OsString::new()), None, Some(OsString::from("/home/u"))),
            Some(PathBuf::from("/home/u/.config/aura"))
        );
        // nothing resolvable → None.
        assert_eq!(global_config_dir_from(None, None, None), None);
    }

    #[tokio::test]
    async fn bind_hops_to_a_free_port_on_loopback() {
        // Occupy a loopback port, then bind_direct_udp on it with hop allowed: it
        // must land on a DIFFERENT, free port within the range (never reaping).
        let occupied = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let server = bind_direct_udp("127.0.0.1", port, true)
            .await
            .expect("hops to a free port");
        let got = server.local_addr().unwrap().port();
        assert_ne!(got, port, "must not reuse the occupied port");
        // `saturating_add` guards the assert itself against overflow when the OS
        // hands out a port near 65535 (e.g. macOS's high ephemeral range).
        assert!(
            got > port && got <= port.saturating_add(16),
            "hopped within range: {got}"
        );
    }

    #[tokio::test]
    async fn bind_refuses_to_hop_when_not_allowed() {
        // Same occupied port, but hop disallowed (REMOTE semantics). The occupier
        // is THIS test process, so the reaper finds it via lsof yet refuses to
        // signal our own pid → nothing is reaped and the bind fails, rather than
        // silently moving to another port (which the firewall would not have opened).
        let occupied = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = occupied.local_addr().unwrap().port();
        let result = bind_direct_udp("127.0.0.1", port, false).await;
        assert!(result.is_err(), "must not hop when hopping is disallowed");
    }

    #[test]
    fn reap_never_targets_self() {
        // Hold a UDP port ourselves; the reaper must find us via lsof (where
        // present) yet refuse to signal our own pid — so it never reaps and
        // returns false. Guards the exact self-kill bug the live-call smoke test
        // caught.
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        assert!(
            !try_reap_port_holder(port),
            "must never SIGTERM our own process"
        );
    }

    #[test]
    fn verify_rejects_a_non_aura_process() {
        // Fail-safe identity gate: THIS test process (a cargo test binary named
        // `aura_server-<hash>`, never exactly `aura-server`) must NOT verify as an
        // aura-server — so it would never be reaped. Guards the "kill the wrong
        // process" axis on whichever verification path this OS takes.
        assert!(!pid_is_aura_server(std::process::id()));
        // A pid that cannot exist is likewise never verified.
        assert!(!pid_is_aura_server(u32::MAX));
    }
}
