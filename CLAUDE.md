# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> **Language policy:** all code, comments, and docs are written in **English**.

## What this is

**aura** — voice calls for AI chats: a user types "call me" in a chat with an AI agent → gets a realtime voice call with a model that already knows the conversation context. Voice is a realtime audio-native model (xAI Grok voice), **direct audio, no STT/TTS**. Two modes: **LOCAL** (mic ↔ model on one device; the server runs on `127.0.0.1`) and **REMOTE** (a native thin client ↔ Rust server on a VPS over a dedicated **Noise (NNpsk0) tunnel over UDP**; the server bridges to the model). The per-call session secret travels over the same chat/gateway the user already uses; no domain, no TLS cert, no NAT-traversal servers. **No intermediary broker.** All-Rust, cross-platform (Linux/Windows/macOS).

## Design rationale (decisions and rejected options)

**Locked decisions and why:**
- **Voice = xAI Grok voice realtime** with the user's key, behind a swappable `VoiceProvider` — direct audio in/out without STT/TTS requires a realtime audio-native model.
- **Direct audio, no STT/TTS** — explicit requirement; speech-to-text is unacceptable.
- **No intermediary broker** — in REMOTE the AI server is itself the call endpoint, not a third party. xAI is the model endpoint (the brain), not a relay.
- **Unified client/server; LOCAL and REMOTE are one path.** The host ALWAYS launches an `aura-server` (on `127.0.0.1` for LOCAL, on a VPS for REMOTE) that holds `XAI_API_KEY` + the engine + the Brief + tools, mints a per-call session secret, and prints a **connection string**. The client (`aura-cli`, always a thin cpal↔tunnel terminal) connects with that string. LOCAL = the server on loopback. This makes the launching chat/session the server's identity, and gives one protocol for both modes. The transport over the tunnel is **Noise_NNpsk0** (the session secret IS the PSK → mutual-auth + forward-secret + anti-MITM, no certs, no domain, no NAT-traversal servers); the secret travels over the same chat/gateway the user already uses. The user need NOT own the server — a call is a feature/protocol.
- **Two binaries:** `aura-server` (the host launches it; holds key/engine/context/tools; the engine's only transport is an `AudioTransport`) and `aura-cli` (always a thin client: cpal mic/speaker ↔ tunnel; no engine/key/host). `aura-audio` provides cpal for the client.
- **Context loop closes both ways:** Brief (chat→call) at start; a **post-call summary** (call→chat) at the end — the in-call inline transcript (the realtime model's own transcript events, NOT STT) is posted into the chat via `HostAdapter::deliver_call_summary` for the host to summarize.
- **Hosts:** Claude Code, Codex, Hermes, OpenClaw; priority is Codex + Claude.
- **Optional 2nd REMOTE transport (iroh).** For a server behind NAT/CGNAT with no openable port, `AURA_TRANSPORT=iroh` selects an iroh QUIC P2P transport (hole-punching + blind relay fallback) instead of direct Noise/UDP. Noise_NNpsk0 still runs INSIDE the iroh stream — the relay only ever sees ciphertext and is used for fallback only. Fixed at onboarding; the connection string is self-describing (`&t=direct|iroh`).

**Explicitly rejected (and why):**
- **Any broker (e.g. Cloudflare)** — a third party between user and AI; the goal is to remove it. (Relaxed only to a *blind encrypted relay* for the optional iroh NAT fallback.)
- **STT/TTS sandwich** — unacceptable; direct audio is required.
- **Any third-party SFU / media relay** — the AI's own server is the call endpoint; no relay sits in the media path (the optional iroh blind relay is fallback-only and sees ciphertext).
- **GPU / self-hosted voice model in v1** — Rust here is the orchestrator, the "brain" = xAI via the key; self-hosting is possible later through the swappable `VoiceProvider`, but not in v1.
- **Fail-closed brief gate** — a thin/empty context must NOT block the call; the Brief is fail-open.

**Terminology (two distinct "brains"):**
- **The Rust software (our core)** — the orchestrator: captures audio, runs the call, reads the chat context, talks to host agents. It does not "think" or speak itself.
- **The voice neural net** — hears speech and answers with voice. This is xAI via the user's key; the model runs at xAI. It is the only content egress and the essence of the product — not a relay.

## Architecture

The single seam between LOCAL and REMOTE is the **`AudioTransport`** trait (PCM16 mono LE @ 24 kHz frames), defined in `aura-engine`. Everything below the "24k PCM in/out" boundary is shared code. 7 lib crates + 2 binaries:

| Crate | Role |
|---|---|
| `aura-core` | shared I/O-free types: config, redaction, speech-filter, history, private_fs (0o600), `ToolRouter` (voice-approval boundary), `Brief`, `HostMemoryCard`, `CallId` |
| `aura-voice` | `VoiceProvider`/`VoiceSink`/`VoiceStream` + xAI realtime impl; wire protocol, `compose_instructions_by_priority`, host-pinning, barge-in |
| `aura-audio` | client-side cpal mic/speaker + `CpalTransport` (inherent 24k frame I/O; no `aura-engine` dep, so the thin client stays light) + the anti-echo stage (`aec::EchoStage`, sonora AEC3) on the mic uplink |
| `aura-tunnel` | the REMOTE transport: Noise_NNpsk0 over UDP + jitter + 20 ms pacer + session-secret + connection-string + `TunnelTransport`; optional iroh QUIC backend (`iroh` feature) and optional Opus codec (`opus` feature) |
| `aura-engine` | **defines the `AudioTransport` trait**; mode-agnostic call engine: event-loop, barge-in, in-call dispatch (`JoinSet`), reconnect, post-call summary |
| `aura-hosts` | `HostAdapter` trait + 4 implementations (Claude/Codex/Hermes/OpenClaw): trigger, read context → Brief, dispatch, callback, post-call summary |
| `aura-feeder` | live ambient context (opt-in; needs `claude` on `PATH`) |
| **bin** `aura-cli` | the thin client: cpal ↔ tunnel; deps `aura-audio` + `aura-tunnel` only (no engine/key/host) |
| **bin** `aura-server` | the server the host launches (local/VPS): holds key/engine/host/tools, mints the session secret, terminates the tunnel, drives `CallSession::run` |

## Critical invariants (easy to break, expensive to fix)

- **The realtime model id is always current.** The voice path uses xAI `grok-voice-think-fast-1.0` (and a dormant OpenAI seam `gpt-realtime-2`). Never write stale ids (`gpt-4o-realtime`, `grok-2-realtime`, anything with `-2024-`) — not in code, docs, or comments. Don't quote model characteristics from memory.
- **BYOK only, key host-pinned.** `XAI_API_KEY` from the environment (then OS keychain), wrapped in `Zeroizing`. Before building `Authorization: Bearer`, a validator refuses to send the key to any host ≠ `api.x.ai`. Secrets live only in env/keychain, NEVER in config/logs/URL/argv. This is the core anti-exfiltration guard.
- **No intermediary broker.** In REMOTE the AI server is itself the call endpoint; the connection string points to the server itself. xAI is the model endpoint (the brain), not a relay. (The optional iroh relay is blind — ciphertext only — and fallback-only.)
- **The session secret never hits argv.** The client reads the connection string from `AURA_CONNECT` or stdin only — never the command line (it would be visible in `ps`).
- **Direct audio, no STT/TTS.** The user transcript arrives as inline realtime-WS events, not a separate STT.
- **`VoiceConnection` is split into `VoiceSink` + `VoiceStream`.** Two tasks (mic-pump and event-loop) can't hold `&mut self` to one WS; `connect()` returns a pair over `SplitSink`/`SplitStream`.
- **A `Reframer{carry}` is mandatory before the audio pipeline.** The pipeline is lossy on arbitrary-length frames (truncates / silence-pads); only exact 480/960-sample frames are safe.
- **Echo cancellation lives in the CLIENT; its far-end tap lives at playout-pop time.** The provider VAD is server-side and can't cancel echo (it never sees the speaker signal) — only the client holds both signals. `aura-audio::aec::EchoStage` (pure-Rust `sonora` AEC3, git-pinned PAST the `dec6a07` panic fix — never the crates.io 0.1.0; BSD-3-Clause → `THIRD_PARTY_LICENSES.md` must ship with binaries) processes the mic uplink in strict 10 ms/240-sample frames (the APM only `debug_assert`s lengths — enforce by construction). The `FarEndTap` mirrors samples in `pop_or_silence`, NEVER at `push_pcm_24k` (the playback queue buffers up to 30 s; a push-time reference leads the acoustic echo far beyond AEC3's delay window). Degradation ladder, never a crashed call: AEC on (default) → warmup/APM-error → half-duplex gate → `AURA_AEC=off`.
- **Brief is fail-OPEN.** Thin/empty context does NOT block the call; validation downgrades fields, the call always proceeds.
- **Voice-approval boundary for dispatch.** In-call tasks launch only through `ToolRouter` with a single-use approval token bound to the spoken intent; `require_voice_approval=false` is rejected at construction. Barge-in: `cancel` + the `suppress_canceled_response` guard are one block; all tool calls (incl. control tools) are gated on `suppress`, so a barged-over response can't pause/hang up.
- **Client/server split is at the binary level.** `aura-cli` (client) = cpal (`aura-audio`) + `aura-tunnel`, no engine/key. `aura-server` = engine/key/host + `aura-tunnel`, no `aura-audio`. The engine's transport is always an `AudioTransport` (LOCAL = loopback); the client is a native cpal terminal.
- **Production quality.** No `todo!()`/`unimplemented!()`/stubs/simplifications in final code; in the audio path, no `unwrap`/`panic`.

## Current state

All code is complete and gates are green (`cargo build`/`test --workspace`/`clippy --workspace --all-targets -- -D warnings`/`fmt`, zero Cyrillic, no stubs). The pieces:

- **`aura-tunnel`** — REMOTE transport. `noise` (NNpsk0 via `snow`, stateless per-packet nonce), `session` (32-byte `SessionSecret`, Zeroizing, base64url), `wire` (`aura://host:port#k=…&c=…&t=…` + datagram framing), `endpoint` (`TunnelServer::bind/accept` responder + `TunnelEndpoint::connect_client` initiator over UDP with handshake retransmit; jitter + 20 ms pacer; tasks aborted on drop), `transport` (`TunnelTransport: AudioTransport`, `server` feature), `jitter`/`reframe`. Optional `iroh` feature: `iroh_transport` (`IrohServer`/`IrohEndpoint`/`IrohTransport` + `connect_by_id`) — iroh QUIC P2P (Noise handshake over a bi-stream, audio over QUIC datagrams), selected by `AURA_TRANSPORT=iroh`. Loopback tests: handshake + PCM round-trip both ways; wrong-secret / tamper / wrong-nonce rejected.
- **`aura-voice`** — `wire` (xAI DIRECT protocol, host-pin, balance classify), `compose` (priority packer + `Brief`→instructions), `XaiRealtimeProvider` (split sink/stream, BYOK+Zeroizing, rustls `ring` provider installed before connect), dormant OpenAI seam.
- **`aura-engine`** — `AudioTransport` seam + `CallSession::run`: single-task event loop, barge-in, reconnect with bounded backoff, pause/resume. The universal chat-callback seam delivers a completed `start_agent_task` dispatch into the host chat via `HostAdapter::deliver_callback` on a detached task (a slow host sink never stalls the audio loop); the `AmbientFeeder` seam injects feeder digests. `InCallTranscript` records both sides (developer input transcript + Aura output transcript) and on call-end the engine posts the recap via `HostAdapter::deliver_call_summary` (best-effort, redacted).
- **`aura-hosts`** — `HostAdapter` + 4 adapters: `ClaudeAdapter` (transcript→`Brief`, slash trigger, `.aura` file callback; `deliver_call_summary` writes the full redacted transcript for the host to summarize), `CodexAdapter` (app-server JSON-RPC, rollout JSONL→`Brief`), `HermesAdapter` (rusqlite read-only state.db, burst-clone ranking, worker `PROGRESS:`/`SUMMARY:` from `AURA_HERMES_WORKER`, recap file `.aura/aura-last-call-recap.md`), `OpenClawAdapter` (host-brief / workspace-fetcher→`Brief`, single `openclaw_agent_consult` dispatch behind the 18-field `reject_direct_overrides` gate, AES-256-GCM runtime-inbox callback). `registry::resolve_host(cwd)` picks the adapter by explicit launch signal (`AURA_HOST` → `AURA_AGENT=codex` → OpenClaw identity env → Claude default). All callback text passes `redact_secrets` + `speech_safe_summary`.
- **`aura-core`** — config/redaction/speech/history/private_fs/host/tools/session/checkpoints + `CallId` + `brief` (fail-open) + `log_safe`/`content_fingerprint`. `HostKind` = Claude/Codex/Hermes/OpenClaw/Other.
- **`aura-audio`** — cpal mic/speaker + `CpalTransport` with inherent 24k frame I/O. Needs `libasound2-dev` on Linux. The mic uplink passes through `aec::EchoStage` (WebRTC AEC3 via pure-Rust `sonora`, + noise suppression mapped from `AudioSettings`): far-end reference from the `FarEndTap` at playout pop, warmup gate while converging, permanent half-duplex-gate fallback on APM error, `AURA_AEC=on|gate|off`. Verified by an in-crate ERLE test (>=15 dB echo attenuation at 24 kHz; double-talk survives).
- **`aura-feeder`** — live ambient context (`tail_events` + `run_digest_cycle` + `ClaudeSubagent`); `Feeder` impls `aura_engine::AmbientFeeder`. Opt-in (`AURA_FEEDER`), degrades to `None` if `claude` is not on `PATH`.
- **`bins/aura-cli`** — the thin client. Reads the connection string from `AURA_CONNECT`/stdin, dials the transport the string names (direct or iroh), pumps cpal mic ↔ tunnel ↔ speaker.
- **`bins/aura-server`** — the unified server. `resolve_host` → `read_context` → compose → `XaiRealtimeProvider`; mints the `SessionSecret`, prints the connection string, accepts the tunnel handshake, drives `CallSession::run`; opt-in feeder; post-call summary on end; writes `.aura/call-status.json` (ringing/active/ended/failed) for monitoring.

**Resolved:** xAI realtime accepts PCM16 mono @ 24 kHz in BOTH directions (confirmed live). LOCAL e2e is live-verified. **iroh REMOTE is live-verified** (2026-07-07: a real Hermes/Telegram-hosted call connected over `&t=iroh` across machines behind NAT — voice both ways + an in-call dispatch executed). `.aura` state (call-status, inbox, recap files, hooks) roots at `AURA_STATE_DIR` when set (else each process's cwd) — the fix for messenger hosts whose exec tool gives every command a fresh cwd; server, engine, adapters, and the `aura-inbox`/`aura-call-status` helpers all resolve it identically (the Rust side loads `.env` first with `~`/`$HOME` expansion; `call-status.sh` parses the same `.env` files itself).

**Remaining (needs the user / real endpoints):**
- A real two-machine REMOTE call over direct real-VPS UDP (the iroh variant is verified; the direct-port path is not yet).
- Per-host live runs: Codex (`codex app-server`), Hermes (`~/.hermes` store + worker), OpenClaw (live gateway WS, Ed25519 connect-challenge). All pure logic is unit-tested and the WS legs degrade gracefully (bounded timeout → `HostError`, never panic/hang) when the gateway is absent.
- Per-host trigger wiring (the host launches `aura-server` and relays the connection string to the user). `skills/SKILL.md` is one universal skill (with per-host examples for Claude/Codex/Hermes/OpenClaw) the AI drops into its own skills dir; the `aura-call`/`aura-call-status` helpers go on PATH via `install.sh` with the server.
- `deliver_call_summary` for Codex/OpenClaw still routes through the spoken-callback cap; apply the Claude-style full-transcript override when they are live-tested (Hermes got it after its first live call: `.aura/aura-last-call-recap.md`). An empty transcript now still delivers a minimal "call ended, nothing captured" note.
- Optional Opus codec on lossy links; per-OS signing; load test.
- Live speaker-echo validation of the client AEC stage on real hardware (macOS especially — the reported self-barge-in machine); the DSP itself is verified by the in-crate ERLE test.

## Commands

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

The toolchain is pinned via `rust-toolchain.toml` (1.92.0). CI (`.github/workflows/ci.yml`) runs Linux/Windows/macOS with clippy `-D warnings` + fmt. The base build is pure Rust (no C toolchain); the optional Opus codec (`aura-tunnel` `opus` feature) is the only piece that needs cmake/libopus, gated behind the feature.
