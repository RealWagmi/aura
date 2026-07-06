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
identical everywhere; only per-host details differ (host-select, context source,
the dispatch fallback executor, recap target) — see the **Per-host specifics**
table at the end. For a host not listed, follow the same flow and pick the
nearest adapter.

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
# REMOTE behind NAT (AURA_TRANSPORT=iroh was set at onboarding) — no public IP needed:
conn="$(aura-call remote --host <kind>)"
```

`<kind>` = `claude` | `codex` | `hermes` | `openclaw` (the host you are — see the
table). `conn` holds one line; the secret is the `#k=` fragment, single-use, ~120 s.
With iroh the string carries a node id (`&t=iroh`) instead of `HOST:PORT` — hand it
to the user exactly the same way:

```
AURA_CONNECT='aura://HOST:PORT#k=<secret>&c=<call_id>' aura-cli          # direct
AURA_CONNECT='aura://<node-id>#k=<secret>&c=<call_id>&t=iroh' aura-cli   # iroh
```

The server needs `XAI_API_KEY` (env / OS keychain / `./.env`); if it is missing,
`aura-call` exits non-zero — ask the user for their xAI key (BYOK). REMOTE assumes
the UDP port was opened once at onboarding — never open it per call.

## Step 3 — Connect

- **LOCAL** — run it *for* the user so the mic opens now, **in the background** so
  you stay free to orchestrate (Step 4 runs concurrently). The client runs for the
  whole call and exits by itself when the call ends:
  ```bash
  eval "$conn" &   # AURA_CONNECT='aura://127.0.0.1:PORT#k=…' aura-cli — backgrounded
  ```
  In an agent runtime, launch it with your background/non-blocking mode — do **not**
  block your turn on it, or the Step 4 loop can never run and every dispatch silently
  falls back to a cold worker.
- **REMOTE** — send the `$conn` line to the user (that one user only); they run it
  on **their own** machine, or run `aura-cli` and paste it on stdin. It expires in
  ~120 s — if it lapses, start a fresh call. (You are not running the client here,
  so you are already free to start the Step 4 loop.)

## Step 4 — Orchestrate the call (and monitor it)

While the call is live, **THIS session is its orchestrator** — running
concurrently with the client (backgrounded on LOCAL; on the user's machine for
REMOTE). The voice model posts each spoken task to an inbox that you drain in
real time, so the work runs with *your* live context and model — not a cold
worker. Run this loop yourself (one command, then act, then repeat) until the
call ends:

1. `aura-inbox alive` — once, to arm the orchestrator (route dispatch to you).
   Run every `aura-inbox` command from the **call's project directory** (the same
   dir you launched the call in) — the inbox lives in `./.aura/inbox` there.
2. `aura-inbox wait --timeout 20` — blocks up to 20 s for the next task,
   refreshing your liveness. It prints `NO_TASK`, or:
   ```
   TASK <id>
   INTENT: <what the user asked for>
   CONSTRAINTS: <…>   PROJECT: <…>
   ```
3. On **`NO_TASK`** → run `aura-call-status`; if it shows `ended` / `failed` /
   `dropped`, the call is over — go to Step 5. Otherwise return to (2).
4. On **`TASK <id>`** → handle `INTENT` the way you'd handle the same request
   typed in this chat: do it yourself (read/edit/bash in this repo) when you can,
   or delegate a heavy/long job to a sub-agent. Then report it — spoken back into
   the call, one short speech-safe sentence (no code / paths / line numbers):
   ```bash
   aura-inbox done <id> "Updated the config; tests pass."
   ```
   Return to (2). (Use `aura-inbox stall <id> "<why>"` to hand a task back — aura
   then runs it directly instead.)

`aura-server` exits when the call ends for ANY reason. Terminal verdicts:
`ended` (clean hang-up / the model's `end_voice_session`), `failed` (provider
error), `dropped` (server pid gone — a crash, detected via the recorded pid, so
you always learn the call ended).

**Hermes / messenger-gateway hosts (Telegram etc.): never run the loop in the
foreground.** Hermes' default `busy_input_mode=interrupt` kills an in-flight
foreground tool the moment the user sends a message, and foreground `terminal`
timeouts are hard-capped at 600 s — a blocking `aura-inbox wait` loop dies on
the first user turn (dispatches then all cold-fallback). Instead run each wait
in the background with completion notification:
`terminal(command="aura-inbox wait --timeout 300", background=true,
notify_on_complete=true)` — the `[IMPORTANT: Background process ... completed]`
notification re-enters as a new turn when you are idle (CLI and gateway alike);
read its output, handle the TASK (or `NO_TASK` → check `aura-call-status`),
`aura-inbox done <id> "..."`, and re-arm the next background wait.

**OpenClaw:** its exec tool auto-backgrounds any command that runs past
`yieldMs` (default 10 s) and wakes you with a system event on exit
(`tools.exec.notifyOnExit`, on by default) — so `aura-inbox wait --timeout 300`
naturally becomes a background wait: act on the exit notification, then re-arm.
Incoming user messages do NOT kill the in-flight run (default queue mode
`steer` injects them at the next model boundary). Do not re-poll in a tight
loop — OpenClaw's own docs forbid emulating scheduling with sleep/poll loops.

**Can't hold a live loop at all?** — skip the loop and just monitor:
`aura-call-status --wait`. Dispatches still execute: when no orchestrator is
draining the inbox, aura spawns each task directly — you only lose the
live-context edge, nothing breaks. (On Hermes the direct fallback needs
`AURA_HERMES_WORKER` set at launch — see the per-host table.)

## Step 5 — After the call: summarize, don't paste

The host callback delivers the raw in-call **transcript** — both `[developer]`
and `[aura]` lines (the model's own transcript events; not speech-to-text),
already redacted. Its location is per-host (see the table). **Read it and write a
short summary into the chat** — key points, decisions, follow-ups. **Do not paste
the raw transcript.** If there is no recap (call never connected, or empty), say so.

## Per-host specifics (examples)

The **In-call dispatch** column is the direct-spawn **fallback** — what aura runs
when no orchestrator is draining the inbox (Step 4). With the Step 4 loop running,
you handle dispatch yourself (answer live, or delegate to that same executor).

| Host | `--host` | Context source | In-call dispatch (fallback) | Recap delivered to |
|---|---|---|---|---|
| **Claude Code** | `claude` (default) | transcript JSONL `~/.claude/projects/<cwd>/` | `claude -p` in the repo (model matched to your chat) | `.aura/hooks/aura-last-claude-result.json` (`compact_summary`) |
| **Codex** | `codex` (or auto `AURA_AGENT=codex`) | rollout JSONL `~/.codex/sessions/` | `codex app-server` (`turn/start`) | app-server note, prefixed `Aura voice callback:` |
| **Hermes** | `hermes` (**required**) | `~/.hermes/profiles/<active>/state.db` | worker subprocess from `AURA_HERMES_WORKER` — recommended value `hermes -z` (oneshot: answer-only stdout, approvals bypassed); the intent is appended as the last arg, the `SUMMARY:` line (else last stdout line) is spoken. Unset = no fallback: the Step 4 loop is the only executor | `.aura/aura-last-call-recap.md` (full redacted transcript — read + summarize it) |
| **OpenClaw** | `openclaw` (or auto via identity env) | host-brief, else workspace fetcher | `openclaw_agent_consult` (gated) | `.aura/aura-last-call-recap.md` (full redacted transcript — read + summarize it); plus a runtime-inbox `tool_result` frame when the gateway inbox is configured (AES-GCM when keyed) |

Claude is the default with no `--host`; Codex/OpenClaw also auto-resolve from
their own env; **Hermes has no ambient signal — `--host hermes` is mandatory.**
Passing `--host` is always safe.

### Delegating heavy work (Step 4, per host)

In the Step 4 loop, do anything you can quickly from live context **inline**.
Delegate only **long/heavy** jobs to a sub-agent so the call stays responsive —
then relay the result with `aura-inbox done`. Use your host's native async spawn,
and match the sub-agent's model to this chat (or pin it — see below):

| Host | Delegate a heavy job with |
|---|---|
| **Claude** | `claude -p "<task>" --model <this chat's model>` (run it in the background; report on completion) |
| **Codex** | `codex exec "<task>"` (headless), or a fresh app-server `turn/start` |
| **OpenClaw** | `sessions_send` / `openclaw agent --message "<task>"` to an isolated agent |
| **Hermes** | `delegate_task` (always async — its `background` param is deprecated/ignored; the result re-enters as a new message when the subagent finishes), then relay |

**Pinning the dispatch model** *(optional)* — for **Claude**, the direct-spawn
fallback already matches the model to your chat (auto-detected from the
transcript). **Codex does NOT auto-match** — without a pin it runs on the
app-server's default model. To force a specific model on either, launch the
server with `AURA_DISPATCH_MODEL=<model>` in the environment (e.g.
`AURA_DISPATCH_MODEL=claude-opus-4-8 aura-call local --host claude`). Only Claude
and Codex have a per-call model knob; OpenClaw/Hermes ignore it.

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
| no `aura-server` / `aura-call` / `aura-inbox` | `install.sh --server` (needs no audio package; installs all three helpers) |
| `XAI_API_KEY` not found | the server exits with a clear error → ask the user for their xAI key |
| the model interrupts/answers ITSELF on open speakers (echo loop) | the client cancels echo by default (AEC3). If the user disabled it or runs an old client: relaunch `aura-cli` without `AURA_AEC=off`, or set `AURA_AEC=gate` (mutes the mic while the model speaks — no barge-in), or suggest headphones. |
| Step 4 loop stalls — `aura-inbox`/edits/dispatch hang, dispatches all cold-fallback | your framework is prompting for tool approval the user can't give mid-call. Set it to **auto-approve** the orchestrator's tool calls (Claude: allow-rules `Bash(aura-inbox:*)`, `Bash(aura-call-status:*)`, `Bash(aura-call:*)` — the loop runs ALL of these helpers, the `:` before `*` is required, and `--permission-mode acceptEdits` covers edits but NOT Bash, so the allow-rules are needed; Hermes: `hermes config set approvals.cron_mode approve`; OpenClaw: `openclaw config set tools.exec.ask off`) — one-time, see onboarding Step 5b. |
| REMOTE launch fails: `Address already in use` | LOCAL self-heals by hopping to a free port (it never kills anything). On a VPS it can't hop (the firewall is opened for one port); it reclaims the port only from a stale `aura-server` that `lsof` proves is holding it. If it still fails, kill whatever holds the port — find it with `ss -lunp 'sport = :<port>'` (or `lsof -iUDP:<port>`) — then relaunch on the **same** port. Use `pkill -x aura-server`, never `pkill -f <path>` (that also matches your own shell and kills it). |

Full server setup (build, key, the one-time firewall, installing this skill) is in **`docs/ONBOARDING.md`**.
