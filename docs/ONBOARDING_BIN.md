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
4. **(REMOTE only) Open the one UDP port, once** (`aura-open-port`); LOCAL → skip → Step 3
5. **Drop the host skill** into your framework's skills dir → Step 4
6. **Self-check** — verify binaries, key, port, skill BEFORE you report done → Step 5
7. **Tell the user how to start a call** → Step 6

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

## Step 3 — (REMOTE only) Open the one UDP port, once

A LOCAL call binds loopback and needs **no** open port — **skip this step**. For
a REMOTE server, open the fixed UDP port (default **47821**) **once**:

**3a.** Find the public address and use it as `AURA_PUBLIC_HOST` when launching:
```bash
PUBLIC_IP="$(curl -fsS ifconfig.me || curl -fsS https://api.ipify.org)"; echo "public IP = $PUBLIC_IP"
```

**3b.** Open the port with the bundled helper (now on your PATH):
```bash
aura-open-port            # opens UDP 47821 (or: aura-open-port <PORT>)
```
If it runs as root it opens the port and says so; if not, it prints the exact
`sudo` commands — relay them to the user. It also always prints a note about
filters **outside** the machine (cloud security group / NAT / home router
port-forwarding; CGNAT → use `AURA_TRANSPORT=iroh`, a VPS, or a VPN overlay) —
relay it. Opening the port is a **one-time** action; never per call.

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

## Step 5 — Self-check (gate before you report "done")

Prove the wiring **without** dialing the model:

```bash
command -v aura-server && echo "server: OK"
command -v aura-call && command -v aura-call-status && echo "helpers: OK"
# Key resolves? The server reaches the connection-string line, then we stop it.
timeout 8s aura-server 2>&1 | grep -m1 -E 'AURA_CONNECT=|composed context|host =' \
  && echo "key resolved + server starts: OK" \
  || echo "server did NOT start — check the XAI_API_KEY (Step 2)"
```

**✅ Do NOT report "done" until every box is checked:**
- [ ] `aura-server` (and, for a LOCAL host, `aura-cli`) on `PATH`
- [ ] `aura-call` + `aura-call-status` on `PATH`
- [ ] the `XAI_API_KEY` resolves (server reaches the connection-string line)
- [ ] (REMOTE) UDP 47821 opened once — OS firewall **and** cloud SG / NAT
- [ ] `SKILL.md` copied into your skills directory
- [ ] you did NOT re-ask the user beyond the key, and did NOT stop at the install

Any unchecked box → go back to that step. Only when all pass, continue to Step 6.

---

## Step 6 — Tell the user how to start a call

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
- **Client can't reach a REMOTE server.** The UDP port is almost always blocked **outside** the VM by the cloud security group / NAT — add an inbound UDP 47821 rule there. Confirm `AURA_PUBLIC_HOST` is the reachable public IP/DNS.
- **`aura-server` / `aura-cli` not found after install.** `~/.local/bin` is not on `PATH` yet — restart the shell or `source` your rc, or use the full path.
