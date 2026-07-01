# In-call dispatch — Scheme 1 + Scheme 2 (accepted design)

When the voice model (Grok) is asked to *do* something mid-call, aura dispatches
work back to the host agent. This doc fixes the two accepted schemes and the
per-host reality (researched against the actual frameworks, not guessed).

## The problem

The current dispatch spawns a **fresh** worker per task (Claude → `claude -p`) that
has the right *tool access* but (a) uses the machine's **default model** — not the
model the user was chatting with — and (b) has **no live conversation context**
(only the Brief + the spoken intent). We want the executor to match the chat
model and, ideally, to *be* the live session (full context), while heavy work is
delegated asynchronously.

## Scheme 1 — match the dispatch model (per host)

Make the dispatched sub-agent use the **host session's model**, plus a config
override.

| Host | Extract model from | Pass to dispatch |
|---|---|---|
| **Claude** | transcript JSONL assistant events (`message.model`) | `claude -p --model <m>` |
| **Codex** | rollout JSONL (if it records the model) | the existing `CodexAdapter.model` field → RPC `params["model"]` |
| **OpenClaw** | n/a | inherent — the consult runs in the live session (its model) |
| **Hermes** | n/a | no per-call model knob — bake it into the configured `worker_command` |

Plus an optional `dispatch_model` config per host (None → default) as a fallback/pin.

## Scheme 2 — live orchestrator + async sub-agents over a heyarp-style coordination layer

The live chat session acts as an **orchestrator**: it watches an inbox, and for
each task **triages** — answer directly from chat context, or delegate
asynchronously to a sub-agent (Scheme-1 model-matched) and relay the result.
Everything is logged in the real chat (context continuity).

### Coordination layer (universal — the heyarp blueprint, proven in Hermes)
- **Inbox = append-only files** under `.aura/inbox/` — task / result / status.
- **Line protocol:** `NEW` (task to dispatch) / `DONE` (terminal) / `STALL` (dead sub-agent).
- **Reliability:** guard-before-action (read live state first), resume-from-state
  (a re-dispatched sub-agent recovers from ids, never restarts), append-only dedup
  (`SEEN` / `DISPATCHED` with `id<TAB>epoch`, latest wins), health-check (STALL after N).
- **Background-wait + notify** for long sub-agent tasks.
- **Latency: NOT cron.** heyarp used a ~1-min cron because it was an *unattended*
  bot; aura dispatches on demand, so we use each host's real-time ingress + a
  **blocking wait** (returns instantly on a new task), never a poll.

### Real-time ingress + orchestrator feasibility (per host — source-backed)

| Host | Real-time ingress (not cron) | Live orchestrator? | Async sub-agent |
|---|---|---|---|
| **Claude** | the session self-loops (blocking `--wait` on the inbox) + `claude -p` spawn | ✅ yes (the call-monitor step already blocks on `aura-call-status --wait`) | `claude -p` per task |
| **Codex** | `codex app-server` JSON-RPC (`thread/start`→`turn/start`→`turn/completed`) — aura already drives it | ⚠️ partial — drive the user's live thread (aura currently opens its own coordinator thread) | turns / `codex exec` |
| **OpenClaw** | gateway websocket (`:18789`) + `sessions_send` / `openclaw agent --message` — aura already on the gateway | ✅ yes — `sessions_send` into the live session key | `sessions_send` to isolated agents |
| **Hermes** | **no external inject into a live session** (docs show no programmatic `send`/RPC into a running chat; sessions are addressable only by chat commands). Real-time = **spawn a worker** (aura already does) | ❌ no → ephemeral spawn | `delegate_task` + `background=true` / `process poll,wait,log` |

Sources: Codex app-server (`docs/exec.md` / app-server) — and aura's own `codex.rs`
already implements `thread/start`/`turn/start`/`turn/completed`. OpenClaw gateway
`:18789` + `sessions_send` — and aura's `openclaw/*` already speaks the gateway ws.
Hermes docs (`/docs/user-guide/messaging`, `/features/tools`) — `delegate_task`,
`execute_code`, `background=true`+`process`, cron-with-gateway-notifier, but **no**
external live-session inject.

### Executor choice per host
- **Claude / OpenClaw** — full **live orchestrator** (self-loop / `sessions_send` into the live session).
- **Codex** — orchestrator by driving the user's live thread (target the live thread, not a fresh coordinator).
- **Hermes** — **ephemeral spawn** (`delegate_task` / worker) + Scheme 1 model. No live-chat context (matches heyarp's own choice).
- **Liveness fallback (all):** if the orchestrator doesn't claim a task within a timeout, aura-server spawns the sub-agent directly (Scheme-1) so a dead loop never strands a call.

## Implementation order
1. **Scheme 1 — Claude** (extract transcript `message.model` → `--model`). ← foundation, lowest risk.
2. **Scheme 1 — Codex** (extract rollout model → the existing `model` field).
3. **Coordination layer** in `aura-engine` (inbox files + NEW/DONE/STALL + dedup/guard/resume + blocking-wait).
4. **Scheme 2 orchestrator** per host: Claude self-loop → OpenClaw `sessions_send` → Codex live-thread → Hermes ephemeral (spawn + Scheme 1).
5. Liveness fallback + the skill wiring (auto-approve, the watch-loop step).

## Open implementation-time checks (not blockers)
- Codex: can `turn/start` target the user's **live** thread (not only a fresh one)? (`docs/` app-server + `codex exec --json`.)
- Hermes: any CLI `send` into a running session? (`/docs/reference/cli-commands`.) If not → ephemeral spawn stays.
- Auto-approval for unattended loops: Codex `approval_policy`/`--dangerously-bypass-approvals-and-sandbox`; OpenClaw sandbox / `main` session.
