---
name: aura
description: >-
  Place an Aura voice call so the user can talk to this chat by voice — direct
  audio, no speech-to-text. Use when the user asks to be called or to talk by
  voice ("call me", "let's talk", "voice", "switch to voice"). Launches
  aura-server, relays the single-use connection string, connects (local) or
  sends it (remote), monitors the call, then summarizes the recap.
trigger: >-
  The user asks for a voice call — "call me", "let's talk", "voice call",
  "switch to voice", or any explicit call intent. Never start a call unprompted.
allowed-tools: Bash
---

# Aura voice call — universal host skill

One skill for every host (Claude Code, Codex, Hermes, OpenClaw, …). The flow is
identical everywhere; only four details differ per host (host-select, context
source, dispatch, recap target) — see the **Per-host specifics** table at the
end. For a host not listed, follow the same flow and pick the nearest adapter.

**Two binaries, one Noise-encrypted UDP tunnel — no broker:**

- **`aura-server`** — *you* launch it (via the `aura-call` helper, on PATH from
  `install.sh`). Holds the key, engine, this chat's context, and tools; mints a
  single-use secret and prints a connection string. LOCAL → binds `127.0.0.1`;
  REMOTE → binds the VPS.
- **`aura-cli`** — the thin client on the **user's** machine (it has the mic).
  Holds no key/engine/context. Takes the connection string from `AURA_CONNECT`
  or stdin — never argv.

## Step 1 — LOCAL or REMOTE?

- `command -v aura-cli` **succeeds** → the client is on this machine, so you are
  likely on the user's own machine. **Ask once:** local (loopback, on-device) or
  remote.
- `aura-cli` **not found** → REMOTE: the user connects from their own machine. If
  they have no client, point them at `README.md` / `install.sh --client` first.

## Step 2 — Launch the server, capture the connection string

Always go through **`aura-call`**: it runs `aura-server`, prints exactly the one
`AURA_CONNECT=… aura-cli` line on stdout, and returns while the server keeps
running. Pass `--host <kind>` so host resolution is deterministic:

```bash
# LOCAL (binds 127.0.0.1):
conn="$(aura-call local --host <kind>)"
# REMOTE (clients dial the VPS public IP):
conn="$(aura-call remote <VPS_PUBLIC_IP> --host <kind>)"
```

`<kind>` = `claude` | `codex` | `hermes` | `openclaw` (the host you are — see the
table). `conn` holds one line; the secret is the `#k=` fragment, single-use, ~120 s:

```
AURA_CONNECT='aura://HOST:PORT#k=<secret>&c=<call_id>' aura-cli
```

The server needs `XAI_API_KEY` (env / OS keychain / `./.env`); if it is missing,
`aura-call` exits non-zero — ask the user for their xAI key (BYOK). REMOTE assumes
the UDP port was opened once at onboarding — never open it per call.

## Step 3 — Connect

- **LOCAL** — run it *for* the user so the mic opens now:
  ```bash
  eval "$conn"     # AURA_CONNECT='aura://127.0.0.1:PORT#k=…' aura-cli
  ```
- **REMOTE** — send the `$conn` line to the user (that one user only); they run it
  on **their own** machine, or run `aura-cli` and paste it on stdin. It expires in
  ~120 s — if it lapses, start a fresh call.

## Step 4 — Monitor the call

```bash
aura-call-status --wait    # one line per ~10 s; returns when the call is over
```

`aura-server` exits when the call ends for ANY reason. Terminal verdicts:
`ended` (clean hang-up / the model's `end_voice_session`), `failed` (provider
error), `dropped` (server pid gone — a crash, detected via the recorded pid, so
you always learn the call ended).

## Step 5 — After the call: summarize, don't paste

The host callback delivers the raw in-call **transcript** — both `[developer]`
and `[aura]` lines (the model's own transcript events; not speech-to-text),
already redacted. Its location is per-host (see the table). **Read it and write a
short summary into the chat** — key points, decisions, follow-ups. **Do not paste
the raw transcript.** If there is no recap (call never connected, or empty), say so.

## Per-host specifics (examples)

| Host | `--host` | Context source | In-call dispatch | Recap delivered to |
|---|---|---|---|---|
| **Claude Code** | `claude` (default) | transcript JSONL `~/.claude/projects/<cwd>/` | `claude -p` in the repo | `.aura/hooks/aura-last-claude-result.json` (`compact_summary`) |
| **Codex** | `codex` (or auto `AURA_AGENT=codex`) | rollout JSONL `~/.codex/sessions/` | `codex app-server` (`turn/start`) | app-server note, prefixed `Aura voice callback:` |
| **Hermes** | `hermes` (**required**) | `~/.hermes/profiles/<active>/state.db` | Hermes worker (`PROGRESS:` / `SUMMARY:`) | result-message in the conversation |
| **OpenClaw** | `openclaw` (or auto via identity env) | host-brief, else workspace fetcher | `openclaw_agent_consult` (gated) | runtime-inbox `tool_result` (AES-GCM when keyed) |

Claude is the default with no `--host`; Codex/OpenClaw also auto-resolve from
their own env; **Hermes has no ambient signal — `--host hermes` is mandatory.**
Passing `--host` is always safe.

## Rules — never break

- 🔒 The session secret and `XAI_API_KEY` travel **only** via the `AURA_CONNECT`
  env var or stdin — **never argv** (`ps` exposes argv). Never echo/print/log the key.
- 🔒 The REMOTE connection string goes to **one** user over the existing chat,
  never anywhere public. Single-use, ~120 s.
- 🗣 Step 5 delivers a **summary**, never the raw transcript.
- 🚫 Never start a call unprompted.

## If something is missing

| Symptom | Fix |
|---|---|
| no `aura-cli` (user side) | `install.sh --client` (Linux needs ALSA dev headers; macOS/Windows none) |
| no `aura-server` / `aura-call` | `install.sh --server` (needs no audio package) |
| `XAI_API_KEY` not found | the server exits with a clear error → ask the user for their xAI key |

Full server setup (build, key, the one-time firewall, installing this skill) is in **`docs/ONBOARDING.md`**.
