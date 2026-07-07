# aura — AI-driven server onboarding (prebuilt binaries)

**Audience: you, the AI model that is setting up an `aura-server`.** This is the
**binary** onboarding for **Linux / macOS** — it downloads prebuilt binaries, so
there is **no Rust toolchain, no clone, and no compile**. (Windows, or an
unlisted CPU arch, uses the source guide instead: `docs/ONBOARDING.md`.) Read
this file in full, then execute the steps in order. You proceed autonomously and
report results; the one thing you always need from the user is the `XAI_API_KEY`.

`aura` gives an AI chat a real voice call: the user types "call me", the host
launches `aura-server` (it holds the xAI key, the engine, the chat context, and
the tools), the server mints a single-use session secret and prints a
**connection string**, and the user's thin client `aura-cli` (mic/speaker) dials
in over one Noise-encrypted UDP tunnel. **No broker, no relay** — the AI server
is itself the call endpoint.

## 🤖 AI agent checklist — follow IN ORDER, do NOT skip a step

1. **Detect OS + arch + decide LOCAL vs REMOTE** — from the environment → Step 0
2. **Install the binaries** (`install_bin.sh --server`) — downloads + verifies + puts on PATH → Step 1
3. **Get the `XAI_API_KEY`** — the ONE time you stop and ask the user → Step 2
4. **(REMOTE only) NAT check first**, then open the UDP port once (`aura-open-port`) — or, behind NAT, set `AURA_TRANSPORT=iroh` and open nothing; LOCAL → skip → Step 3
5. **Drop the host skill** into your framework's skills dir → Step 4
6. **Set your framework to auto-approve** the orchestrator's tool calls + raise the budget → Step 4b
7. **Self-check** — verify binaries, key, port, skill, framework prep BEFORE you report done → Step 5
8. **Tell the user how to start a call** → Step 6

## 🚫 Common mistakes — DO NOT do these

- ❌ **Re-asking the user for things you can detect or decide** (OS, arch, mode when clear, the skills dir, defaults). Ask only for the items in "When to ask the user".
- ❌ **Stopping after the install.** Installed binaries ≠ a working call — you are done only after the Step 5 self-check passes.
- ❌ **Echoing, printing, or logging the `XAI_API_KEY` or the session secret**, or putting either on a command line (`argv` is visible in `ps`).
- ❌ **Opening the firewall per call.** The UDP port is opened ONCE at Step 3.
- ❌ **Ignoring a checksum failure.** If `install_bin.sh` reports a SHA-256 mismatch, STOP — do not work around it.
- ❌ **Continuing past a failed step.** On any failure: stop, tell the user what failed and the fix, do not proceed.

## When to ask the user (the ONLY interactive points)

- **Proceed autonomously:** OS/arch detection, the mode decision (when clear), the install, dropping the skill, opening the port when you have root, and every verification.
- **Ask the user — only these:** (1) the **`XAI_API_KEY`** (Step 2); (2) **LOCAL or REMOTE** (Step 0) *only* if you cannot tell from the environment; (3) to run the **`sudo` firewall command** (Step 3) *only* when you lack root.
- **Never** sign in, paste the key, or change anything outside this machine on the user's behalf.

---

## Step 0 — Preconditions: OS, arch, mode

```bash
uname -s    # Linux | Darwin (macOS)   — Windows/other → use docs/ONBOARDING.md
uname -m    # x86_64 | aarch64 / arm64
```

Decide the **call mode** now:
- **REMOTE** — this server runs on a VPS / remote host the user reaches over the network (Steps 1–5 apply, incl. the firewall Step 3).
- **LOCAL** — this server runs on the user's own machine; the client connects over loopback (Steps 1–5 apply **except** Step 3).

Heuristic: a cloud VM / SSH host the user is not sitting at → **REMOTE**; the user's own workstation → **LOCAL**. Only if you truly can't tell, ask one question.

---

## Step 1 — Install the binaries (no toolchain, no compile)

```bash
curl -fsSL https://raw.githubusercontent.com/RealWagmi/aura/main/install_bin.sh | bash -s -- --server
```

This downloads the prebuilt archive for your OS+arch from the GitHub Release,
**verifies its SHA-256**, and installs onto your `PATH` (`~/.local/bin`):
`aura-server`, the helpers `aura-call` / `aura-call-status` / `aura-open-port`,
and the host skill to `~/.local/share/aura/SKILL.md`. (`--client` installs only
`aura-cli`; no flag installs both.) Pin a version with `AURA_VERSION=v0.1.0`.

Make PATH current, then confirm:
```bash
export PATH="$HOME/.local/bin:$PATH"
command -v aura-server && command -v aura-call && echo "installed: OK"
```

If it reports **no prebuilt binary for your platform** (Windows, or an unusual
arch), it prints the source-install command — follow `docs/ONBOARDING.md` instead.

---

## Step 2 — Obtain the `XAI_API_KEY` (get it from the user)

`aura` is **BYOK**. The server resolves the key in order: env `XAI_API_KEY` → a
`.env` file (`./.env`, then a user-global `$AURA_HOME/.env`, else
`${XDG_CONFIG_HOME:-~/.config}/aura/.env`) → the OS keychain. The key is sent
**only** to `api.x.ai` (host-pinned).

**Recommended — a user-global `~/.config/aura/.env`** (found no matter which
directory the host launches the server from). Create it owner-only *first*, then
have the user paste the key so it never appears in your transcript:

```bash
umask 077
mkdir -p ~/.config/aura
printf 'XAI_API_KEY=' > ~/.config/aura/.env && chmod 600 ~/.config/aura/.env
( read -rs k; printf '%s\n' "$k" >> ~/.config/aura/.env; unset k )
echo "key written to ~/.config/aura/.env"   # confirmation only; the key is never printed
```

**Never** run `echo "$XAI_API_KEY"`, never put the key on a command line, and
never write it anywhere but a `0600` `.env` (or the keychain). On macOS/Windows
you may instead store it in the OS keychain (service `aura`, entry `XAI_API_KEY`).

---

## Step 3 — (REMOTE only) NAT check, then open the port OR choose iroh

A LOCAL call binds loopback and needs **no** open port — **skip this step**.
For a REMOTE server, first determine whether the machine is directly reachable
— do NOT open a firewall port before knowing this:

**3a. NAT check (do not skip).** Compare the machine's interface IPs with its
public IP:
```bash
PUBLIC_IP="$(curl -fsS ifconfig.me || curl -fsS https://api.ipify.org)"; echo "public IP = $PUBLIC_IP"
LOCAL_IPS="$( (ip -4 addr show 2>/dev/null || ifconfig 2>/dev/null) | awk '/inet /{sub("/.*","",$2); print $2}' )"
case " $(echo $LOCAL_IPS) " in
  *" $PUBLIC_IP "*) echo "DIRECT: the public IP is on an interface" ;;
  *)                echo "BEHIND NAT: no interface has $PUBLIC_IP (local: $(echo $LOCAL_IPS))" ;;
esac
```
- **DIRECT** (typical VPS) → continue with 3b + 3c below; `AURA_PUBLIC_HOST=$PUBLIC_IP`.
- **BEHIND NAT** (home server, VM with a private 10.x/192.168.x address) —
  opening the OS firewall would be useless; pick ONE of:
  1. **iroh transport (recommended — zero network config).** Persist it next to
     the key and **skip 3b/3c entirely**:
     ```bash
     printf 'AURA_TRANSPORT=iroh\n' >> ~/.config/aura/.env
     ```
     iroh hole-punches through NAT (blind encrypted relay as fallback); the
     connection string then carries a node id instead of `host:port`
     (`aura://<node-id>#k=...&t=iroh`), and `aura-call remote` needs **no**
     public host argument. No port is ever opened.
  2. **Router port-forwarding** — only if the user controls the router AND has a
     real public IP on it: forward WAN UDP 47821 → this machine's LAN IP, use
     the router's WAN IP as `AURA_PUBLIC_HOST`, then do 3b + 3c. **CGNAT** (the
     router's WAN address is itself private / differs from `$PUBLIC_IP`) makes
     inbound impossible — use option 1.

**3b. (DIRECT only)** Use the public address found above as `AURA_PUBLIC_HOST`
when launching.

**3c. (DIRECT only)** Open the port with the bundled helper (now on your PATH):
```bash
aura-open-port            # opens UDP 47821 (or: aura-open-port <PORT>)
```
If it runs as root it opens the port and says so; if not, it prints the exact
`sudo` commands — relay them to the user. It also always prints a note about
filters **outside** the machine (cloud security group etc.) — relay it. Opening
the port is a **one-time** action; never per call.

---

## Step 4 — Drop the host skill into your skills directory

The universal skill was installed to `~/.local/share/aura/SKILL.md`. Copy it into
the directory **your framework** scans (you know that path). **`mkdir -p` first**,
or the `cp` fails with `No such file or directory`:

```bash
# Claude Code (the default) scans ~/.claude/skills/<name>/:
mkdir -p ~/.claude/skills/aura && cp ~/.local/share/aura/SKILL.md ~/.claude/skills/aura/SKILL.md
```
For another runtime substitute its skills dir (e.g. `~/.codex/skills/aura/`,
`~/.hermes/skills/aura/`, `~/.openclaw/skills/aura/`). The file is the same
everywhere; the per-host details are examples inside it. Reload your host so it
picks up the skill.

---

## Step 4b — Let the orchestrator run unattended (auto-approve + budget)

During a call the user is on the **voice line, not the chat**, so the host session
that orchestrates it (the skill's Step 4 `aura-inbox` watch-loop + any edits/bash
and delegated sub-agents) must run its tool calls **without an approval prompt**,
and be allowed to run long enough for a whole call. Otherwise the loop freezes on
a confirmation nobody can give and every dispatch cold-falls-back. Set it **once**:

- **Claude Code** — add Bash allow-rules for the on-PATH helpers: `Bash(aura-inbox:*)`,
  `Bash(aura-call:*)`, `Bash(aura-call-status:*)` (the `:` before `*` is required —
  `Bash(aura-inbox*)` does not match). `--permission-mode acceptEdits` covers file
  edits but NOT Bash, so the allow-rules are required, not an alternative (or use
  `--dangerously-skip-permissions` to bypass all prompts). aura already runs the
  dispatched `claude -p` in `acceptEdits`.
- **Codex** — bypass approvals for the loop, e.g. `--dangerously-bypass-approvals-and-sandbox`
  (verify the exact knob for your version).
- **Hermes** — `hermes config set approvals.cron_mode approve` (the dispatch is an
  unattended `delegate_task` worker), plus raise the budget:
  `hermes config set agent.gateway_timeout 1800` · `hermes config set delegation.child_timeout_seconds 1800` · `hermes config set max_turns 200`.
  Also enable aura's direct-dispatch fallback: `printf 'AURA_HERMES_WORKER=hermes -z\n' >> ~/.config/aura/.env`
  (`hermes -z` = oneshot mode: answer-only stdout, approvals bypassed; aura appends
  the task as the last argument). Run the in-call watch-loop via
  `terminal(background=true, notify_on_complete=true)` — Hermes kills foreground
  tools on any incoming user message (see the skill's Step 4).
- **OpenClaw** — `openclaw config set tools.exec.security full` · `openclaw config set tools.exec.ask off`
  (already the gateway-host defaults; shortcut: `openclaw exec-policy preset yolo`, then `openclaw gateway restart`).

> All Hermes/OpenClaw commands and config keys above are source-verified against
> the upstream repositories (hermes-agent, openclaw); aura's Hermes/OpenClaw call
> paths themselves are newer than Claude's — report anything that misbehaves.
> Claude Code is the live-verified default. Also make sure the session/turn cap on
> any host is high enough for a multi-minute call.

---

## Step 5 — Self-check (gate before you report "done")

Prove the wiring **without** dialing the model:

```bash
command -v aura-server && echo "server: OK"
command -v aura-call && command -v aura-call-status && command -v aura-inbox && echo "helpers: OK"
# Key resolves? The server reaches the connection-string line, then we stop it.
timeout 8s aura-server 2>&1 | grep -m1 -E 'AURA_CONNECT=|composed context|host =' \
  && echo "key resolved + server starts: OK" \
  || echo "server did NOT start — check the XAI_API_KEY (Step 2)"
# State-dir agreement: run BOTH and compare the two absolute paths:
timeout 8s aura-server 2>&1 | grep -m1 'in-call dispatch inbox at'
aura-inbox alive   # prints: ALIVE <absolute inbox dir>
```

If your framework's exec tool starts every command in a fresh/implicit cwd
(Hermes, OpenClaw and other messenger gateways), pin the state root ONCE so the
server, the helpers, and the recap files can never drift apart:
```bash
mkdir -p ~/.config/aura
printf 'AURA_STATE_DIR=%s\n' "$HOME" >> ~/.config/aura/.env
```

**✅ Do NOT report "done" until every box is checked:**
- [ ] `aura-server` (and, for a LOCAL host, `aura-cli`) on `PATH`
- [ ] `aura-call` + `aura-call-status` + `aura-inbox` on `PATH`
- [ ] the `XAI_API_KEY` resolves (server reaches the connection-string line)
- [ ] `aura-inbox alive` prints the SAME directory the server run above logged
      (`in-call dispatch inbox at …`) — else set `AURA_STATE_DIR` (see above)
- [ ] (REMOTE) UDP 47821 opened once — OS firewall **and** cloud SG / NAT
- [ ] `SKILL.md` copied into your skills directory
- [ ] framework set to **auto-approve** the orchestrator's tool calls + budget raised (Step 4b)
- [ ] you did NOT re-ask the user beyond the key, and did NOT stop at the install

Any unchecked box → go back to that step. Only when all pass, continue to Step 6.

---

## Step 6 — Tell the user how to start a call

**And a rule for YOU, the agent:** when the user later asks for a call, OPEN
AND FOLLOW the skill you installed in Step 4 — do not run the call flow from
onboarding memory. Onboarding taught you to INSTALL; the skill teaches you to
CALL (they are different procedures, and the skill's Step 4.1 check is what
prevents the most common silent failure).

Explain in plain language: *"To start a call, say 'call me' (or use the host's
command) in your chat. I'll launch the voice server; you'll either be connected
automatically (local) or I'll send you a one-line `AURA_CONNECT=... aura-cli`
command to paste on your machine (remote)."* The connection string's secret is
single-use and valid ~120 s. After the call the server posts a short recap of the
conversation back into the chat.

For a REMOTE call the user needs `aura-cli` on **their own** machine — they get it
the same way, client-only:
```bash
curl -fsSL https://raw.githubusercontent.com/RealWagmi/aura/main/install_bin.sh | bash -s -- --client
```

---

## Troubleshooting

- **"no prebuilt binary for this platform".** Your OS/arch isn't published (Windows, or an unusual arch). Build from source: `docs/ONBOARDING.md` (or the command the installer printed).
- **SHA-256 mismatch.** The installer aborts on purpose — do not work around it. Re-run (a partial download), and if it persists, report it; do not install an unverified binary.
- **`aura-server` exits immediately with `no BYOK xAI key found`.** The key didn't resolve — re-do Step 2 (is `~/.config/aura/.env` present with a `XAI_API_KEY=...` line and `0600`?). Never print the key to debug it.
- **Client can't reach a REMOTE server.** The UDP port is almost always blocked **outside** the VM by the cloud security group / NAT — add an inbound UDP 47821 rule there. Confirm `AURA_PUBLIC_HOST` is the reachable public IP/DNS. If the server sits behind NAT/CGNAT (Step 3a says BEHIND NAT), switch to `AURA_TRANSPORT=iroh` instead of fighting the firewall.
- **`aura-server` / `aura-cli` not found after install.** `~/.local/bin` is not on `PATH` yet — restart the shell or `source` your rc, or use the full path.
