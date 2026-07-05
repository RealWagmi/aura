# aura — AI-driven server onboarding

**Audience: you, the AI model that is setting up an `aura-server`.** Read this
file in full, then execute the numbered steps **in order**. Do **not** improvise,
reorder, or skip steps. You proceed **autonomously** and report results to the
user in plain language; the one thing you always need from them is the
`XAI_API_KEY` (step 3). See **When to ask the user** below for the few exceptions.

`aura` gives an AI chat a real voice call: the user types "call me", the host
launches `aura-server` (it holds the xAI key, the engine, the chat context, and
the tools), the server mints a single-use session secret and prints a
**connection string**, and the user's thin client `aura-cli` (mic/speaker, via
cpal) connects over one Noise-encrypted UDP tunnel. There is **no broker and no relay** — in a remote setup the AI server is
itself the call endpoint.

## 🤖 AI agent checklist — follow IN ORDER, do NOT skip a step

You are the AI setting up the server. Read this whole file first, then execute:

1. **Detect OS + decide LOCAL vs REMOTE** — from the environment, not by asking → Step 0
2. **Clone the repo** into a stable working dir (`~/aura`) → Step 1
3. **Build + install the server** (`./install.sh --server`) — this also puts `aura-call` / `aura-call-status` on PATH → Step 2
4. **Get the `XAI_API_KEY`** — the ONE time you stop and ask the user → Step 3
5. **(REMOTE only) Open the one UDP port, once** (`scripts/aura-open-port.sh`); LOCAL → skip → Step 4
6. **Copy the skill** `skills/SKILL.md` into your framework's skills dir (`mkdir -p` first) → Step 5
7. **Set your framework to auto-approve** the orchestrator's tool calls + raise the turn/timeout budget → Step 5b
8. **Self-check** — verify binaries, key, port, skill, framework prep BEFORE you report done → Step 6
9. **Tell the user how to start a call** → Step 7

## 🚫 Common mistakes — DO NOT do these

- ❌ **Re-asking the user for things you can detect or decide.** Detect the OS; decide LOCAL vs REMOTE from where you are running; use the defaults (port 47821, `~/aura`, your own skills dir). Ask only for the items in "When to ask the user" below.
- ❌ **Starting before reading the whole guide**, or reordering / skipping steps.
- ❌ **Stopping after the build.** Built binaries ≠ a working call — you are done only after the Step 6 self-check passes.
- ❌ **Killing the build because it seems slow.** A first `cargo build --release` (it also fetches the pinned toolchain + crates) can take several minutes — give it a generous timeout and let it finish; do not kill or retry it.
- ❌ **Echoing, printing, or logging the `XAI_API_KEY` or the session secret**, or putting either on a command line (`argv` is visible in `ps`).
- ❌ **Opening the firewall per call.** The UDP port is opened ONCE at Step 4 and never touched again.
- ❌ **Asking the user where the skill goes.** You know your own skills directory — copy `SKILL.md` there yourself (`mkdir -p` the directory first).
- ❌ **Pasting the connection string anywhere public.** For REMOTE it goes only to the one user over the existing chat; it is single-use and ~120 s.
- ❌ **Continuing past a failed step.** On any failure: stop, tell the user exactly what failed and the fix, do not proceed.

## When to ask the user (the ONLY interactive points)

- **Proceed autonomously** — OS detection, the mode decision (when it is clear), cloning, building, installing, copying the skill, opening the port when you have root, and every verification. Report what you did in plain language; do not ask permission for these.
- **Ask the user — only these:**
  1. **The `XAI_API_KEY`** (Step 3) — BYOK; you cannot proceed without it.
  2. **LOCAL or REMOTE** (Step 0) — *only* if you genuinely cannot tell from the environment.
  3. **Run the `sudo` firewall commands** (Step 4) — only when you lack root; you print the exact commands and the user runs them.
- **Never** sign in, paste the key, or change anything outside this machine on the user's behalf.

## What you are building toward

- **`aura-server`** — runs where the host/AI runs (on `127.0.0.1` for a LOCAL
  call, on a VPS for a REMOTE call). Holds `XAI_API_KEY` + engine + context +
  tools. Does **not** use the microphone, so it needs **no audio library**.
- **`aura-cli`** — the thin client on the **user's own machine** (it has the mic
  and speakers). Holds no key, no engine, no context. On Linux it needs the ALSA
  dev headers; on macOS/Windows it needs nothing extra.

This onboarding sets up the **server** side. The user installs `aura-cli` on
their own machine separately (see the repository `README.md` / `install.sh
--client`); for a LOCAL call on the same machine, `install.sh` (default = both)
installs `aura-cli` too.

## Hard rules (never violate)

- **Never echo, print, or log the `XAI_API_KEY` or the session secret.** Never
  place either on a command line (`argv`) — anything on a command line is
  visible in `ps`. The key lives only in the environment, the OS keychain, or a
  `chmod 600` `./.env` file. The session secret travels only inside the
  connection string via the `AURA_CONNECT` environment variable or stdin.
- **One predictable UDP port, opened once.** The tunnel uses a fixed port
  (default **47821**). For a REMOTE server you open it **once** at onboarding
  (step 4), **never per call**.
- **English only.** All output and any file you write is English.
- **No silent failure.** If a step fails, stop, tell the user exactly what
  failed and how to fix it, and do not continue to a later step.

---

## Step 0 — Preconditions: detect OS and architecture

Determine where you are running and confirm the basics.

```bash
uname -s    # Linux | Darwin (macOS)
uname -m    # x86_64 | aarch64 / arm64
```

Decide the **call mode** for this server up front:

- **REMOTE** — this server runs on a VPS / remote host the user reaches over the
  network. Steps 1–6 apply, including the firewall step (step 4).
- **LOCAL** — this server runs on the user's own machine and the client connects
  over loopback (`127.0.0.1`). Steps 1–6 apply **except step 4** (no firewall:
  loopback is never exposed).

A quick heuristic: if you are on a cloud VM / SSH host that the user is *not*
sitting at, treat it as **REMOTE**. If the host is the user's personal
workstation, treat it as **LOCAL**. When unsure, ask the user one question:
"Is this server on your own machine (local) or on a remote/VPS host?"

Confirm the build prerequisites exist (you install them in step 2 if missing):

```bash
command -v git    || echo "git missing — install it (e.g. apt/dnf/brew install git)"
command -v cargo  || echo "cargo missing — rustup will be installed in step 2"
command -v curl   || echo "curl missing — install it for downloads / IP lookup"
```

---

## Step 1 — Get the repository into a working directory

> **Already ran the one-line installer** (`curl … | bash -s -- --server`)? It
> cloned the repo to `~/aura` and built the server for you — Steps 1–2 are done.
> `cd ~/aura` and continue from **Step 3**. Otherwise, do Steps 1–2 below.

Pick a stable working directory the server will run from (this is also where the
`./.env` key file and any `.aura/` callback files live). `~/aura` is a good
default.

```bash
# If you already have the repo, skip the clone and just cd into it.
git clone https://github.com/RealWagmi/aura "$HOME/aura"
cd "$HOME/aura"
```

If `git` is unavailable and you only have a release tarball/zip, unpack it into
`$HOME/aura` and `cd` there instead. Verify you are at the repository root:

```bash
test -f Cargo.toml && test -f install.sh && echo "at repo root: OK"
```

If `install.sh` is not present at the root, you are in the wrong directory — fix
that before continuing.

---

## Step 2 — Build and install the server

The repository pins the Rust toolchain via `rust-toolchain.toml`
(Rust **1.92.0**); `rustup` selects it automatically — do not install a
different toolchain.

If `cargo`/`rustup` is missing, install Rust first (non-interactive):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

Now build and install **only the server** with the bundled installer. The
server pulls **no audio library**, so you do **not** need ALSA/`libasound2-dev`
to build it:

```bash
./install.sh --server
```

This builds `aura-server` in release mode and installs it to `~/.local/bin`
(override with `--prefix DIR`). The installer adds `~/.local/bin` to your `PATH`
via your shell rc if it is not already there and tells you to restart the shell
or `source` the rc. `install.sh` is **idempotent** — safe to re-run; it rebuilds
and overwrites.

Make `aura-server` available in the current shell, then confirm:

```bash
export PATH="$HOME/.local/bin:$PATH"   # if the installer just added it
command -v aura-server && echo "aura-server installed: OK"
```

If the build fails, surface the exact `cargo` error to the user and stop. The
most common cause on a fresh box is a missing C linker — install `build-essential`
(Debian/Ubuntu) / `gcc` + `make` (Fedora/RHEL via `dnf`, Arch via `pacman`) and
re-run `./install.sh --server`.

---

## Step 3 — Obtain the `XAI_API_KEY` (get it from the user)

`aura` is **BYOK** (bring your own key). The server resolves the key in this
order (first non-empty value wins):

1. the environment variable `XAI_API_KEY`;
2. a `.env` file (`KEY=VALUE` lines), loaded into the environment without
   overriding an already-set variable — first `./.env` in the directory the
   server starts in, then a fixed user-global `$AURA_HOME/.env` (else
   `${XDG_CONFIG_HOME:-~/.config}/aura/.env`);
3. the OS keychain (service `aura`, entry `XAI_API_KEY`).

The key is sent **only** to `api.x.ai` (host-pinned); the server refuses to send
it anywhere else.

**Ask the user for their xAI API key now** — this is where you stop for input.
Store it **without ever echoing or logging it**, and choose the location by how
the server runs.

**Recommended — a user-global `~/.config/aura/.env`.** The host (e.g. Claude
Code) launches the server from whatever directory it is in, so a key tied to one
project directory may not be found. The global file is read **regardless of the
launch directory**, so it is the safe default. Create it owner-only *first*, then
have the user paste the key so it never appears in your transcript or shell history:

```bash
umask 077
mkdir -p ~/.config/aura
printf 'XAI_API_KEY=' > ~/.config/aura/.env && chmod 600 ~/.config/aura/.env
# The user pastes the key — read from stdin, never argv, never printed:
( read -rs k; printf '%s\n' "$k" >> ~/.config/aura/.env; unset k )
echo "key written to ~/.config/aura/.env"   # confirmation only; the key is never printed
```

Verify it exists and is owner-only, **without printing its contents**:

```bash
f=~/.config/aura/.env
test -f "$f" && [ "$(stat -c '%a' "$f" 2>/dev/null || stat -f '%Lp' "$f")" = "600" ] \
  && echo "$f present and 0600: OK"
```

> `stat -c` is GNU/Linux; `stat -f '%Lp'` is macOS/BSD — the line tries both.

**Alternatives.** For a dedicated server you always start from one fixed directory,
a `chmod 600 ./.env` in that directory works the same way (same commands, `.env`
instead of `~/.config/aura/.env`). Or store the secret in the **OS keychain**
(service `aura`, entry `XAI_API_KEY`) on macOS (`security`) or Windows (Credential
Manager) — directory-independent, nothing on disk. On Linux the keychain backend
is the kernel keyring (keyutils), which does not reliably persist for a
long-running server, so prefer `.env` there.

**Never** run `echo "$XAI_API_KEY"`, never put the key on a command line, and
never write it anywhere but a `0600` `.env` (or the keychain).

---

## Step 4 — Decide on the UDP port, then (REMOTE only) open it once

A LOCAL call binds loopback (`127.0.0.1`) and needs **no** open port — loopback is
never exposed and never touches the firewall. Only a REMOTE call needs the UDP
port reachable. **Use the mode you already decided in Step 0 — do not re-ask the
user:**

- **LOCAL → skip 4a and 4b entirely.** Open no port. Tell the user once: "No
  firewall port was opened — local calls don't need one. To allow remote calls
  later, run `scripts/aura-open-port.sh` once (and, on a cloud host, also add an
  inbound UDP 47821 rule in your provider's security group / NAT)." Continue to
  Step 5.
- **REMOTE → open the port once** via 4a + 4b below. It is a one-time action; you
  never touch the firewall again on later calls.

**4a. Find the server's public address** and set `AURA_PUBLIC_HOST` to it. This
is both what the client dials and what tells the server to bind all interfaces
(a non-loopback value) instead of loopback-only:

```bash
PUBLIC_IP="$(curl -fsS ifconfig.me || curl -fsS https://api.ipify.org)"
echo "public IP = ${PUBLIC_IP}"
# You will pass AURA_PUBLIC_HOST=${PUBLIC_IP} when you launch the server (step 7).
```

If the host has a DNS name the user prefers, use that string instead of the IP.

**4b. Open the UDP port once** with the bundled script. The default port is
**47821**; pass a different one only if the user overrides `AURA_PORT`.

```bash
scripts/aura-open-port.sh            # opens UDP 47821 (or:  scripts/aura-open-port.sh <PORT>)
```

Behaviour of the script (do not re-implement it — just run it and relay its
output):

- It detects the **active** firewall front-end in priority order
  (`ufw` → `firewalld` → `nftables` → `iptables`) and opens **UDP `PORT`**
  idempotently.
- **If it runs as root**, it performs the change and prints that it opened the
  port. **Tell the user**: "I opened UDP `PORT` on the server firewall."
- **If it is not root**, it does **not** fail: it prints the **exact commands**
  (with `sudo`) for the user to run. **Relay those commands verbatim** to the
  user and ask them to run them.
- It **always** prints a final note about filters **outside** this machine that
  must also allow inbound UDP `PORT` (the script cannot change them). **Relay it to
  the user.** Two cases:
  - **Cloud host:** add an inbound UDP `PORT` rule in the provider's security group
    / firewall (AWS, GCP, Azure NSG, DigitalOcean, Hetzner, ...).
  - **Home / office router (server on a LAN behind NAT):** the OS firewall is
    **not** enough — the user must add a **port-forwarding** rule on the router
    (WAN UDP `PORT` → this PC's LAN IP) and use the router's **public WAN IP** as
    `AURA_PUBLIC_HOST` (dynamic DNS if it changes). **CGNAT:** if the ISP gives no
    real public IP (compare the router's WAN IP with `curl -fsS ifconfig.me`),
    inbound is impossible for the direct transport — so advise the **iroh
    transport** (`AURA_TRANSPORT=iroh`, which hole-punches and can fall back to a
    blind encrypted relay, needing no open port), or a VPS, a VPN/overlay
    (WireGuard / Tailscale), or LOCAL-only calls.

Opening the port is a **one-time** onboarding action. You never touch the
firewall again on later calls.

---

## Step 5 — Install the host skill (one universal skill)

The skill is a single self-contained file, `skills/SKILL.md`. **Copy it into the
directory your framework scans for skills** — you know that path for your own
runtime. The file is identical for every framework; the per-host details
(context source, dispatch, callback) are examples **inside** it, so one file
serves every host.

```bash
# Claude Code (the default) scans ~/.claude/skills/<name>/:
mkdir -p ~/.claude/skills/aura && cp skills/SKILL.md ~/.claude/skills/aura/SKILL.md
```

**Create the directory first** (`mkdir -p`) — otherwise the `cp` (or a `curl -o`
if you fetch the file instead) fails with `No such file or directory`.

For another runtime, substitute its skills directory — e.g.
`~/.codex/skills/aura/`, `~/.hermes/skills/aura/`, `~/.openclaw/skills/aura/`, or
wherever your host scans. The file you copy is the same in every case.

The `aura-call`, `aura-call-status`, and `aura-inbox` helpers the skill uses are
**already on your `PATH`** — `install.sh` installed them next to `aura-server` in
step 2 (the server install ships them; a client-only install does not). Confirm:

```bash
command -v aura-call && command -v aura-call-status && command -v aura-inbox && echo "helpers on PATH: OK"
```

Reload your host (e.g. restart Claude Code) so it picks up the new skill. The
skill is what lets the user say "call me" (or use the host's command) to have the
host launch `aura-server` (via `aura-call --host <kind>`) and relay the connection
string. The server resolves the host adapter automatically where it can — Claude
is the default; Codex and OpenClaw also auto-resolve from their own launch env;
Hermes needs `--host hermes`, which `aura-call` passes for you.

---

## Step 5b — Let the orchestrator run unattended (auto-approve + budget)

During a call the **user is on the voice line, not watching the chat**. So the
host session that orchestrates the call — the skill's Step 4 `aura-inbox`
watch-loop, plus any edits/bash and delegated sub-agents — must run its tool calls
**without blocking on an approval prompt**, and the session must be allowed to run
long enough for a whole call. If tool calls need manual approval, the loop
silently freezes on a confirmation nobody can give mid-call, and every dispatch
falls back to a cold worker. Configure your framework **once**:

**1. Auto-approve the orchestrator's tool calls.**

- **Claude Code** — aura already runs the dispatched `claude -p` sub-agent in
  `acceptEdits`; for the live orchestrating session, add Bash **allow-rules** for
  the on-PATH helpers: `Bash(aura-inbox:*)`, `Bash(aura-call:*)`,
  `Bash(aura-call-status:*)` (note the `:` before `*` — that is the Claude Code
  rule syntax; `Bash(aura-inbox*)` does NOT match). Run the session under
  `--permission-mode acceptEdits` so its file edits don't prompt — but
  `acceptEdits` does **not** auto-approve Bash, so those allow-rules are required
  (not an alternative) or `aura-inbox wait` freezes on a prompt nobody can answer
  mid-call. For a fully unattended loop, `--dangerously-skip-permissions` bypasses
  all prompts instead.
- **Codex** — set the approval policy so the loop's commands don't block, e.g.
  `--dangerously-bypass-approvals-and-sandbox` (or the equivalent `approval_policy`
  setting). *Verify the exact knob for your Codex version.*
- **Hermes** — the dispatch runs as an unattended worker (`delegate_task`), so
  auto-approve its commands:
  ```
  hermes config set approvals.cron_mode approve
  ```
- **OpenClaw** — auto-approve exec so the consult is not gated:
  ```
  openclaw config set tools.exec.security full
  openclaw config set tools.exec.ask off
  ```

**2. Raise the time / turn budget** so a multi-minute call's watch-loop is not
cut off mid-way.

- **Hermes**:
  ```
  hermes config set agent.gateway_timeout 1800
  hermes config set delegation.child_timeout_seconds 1800
  hermes config set max_turns 200
  ```
- **Claude Code / Codex / OpenClaw** — make sure the session/turn cap and any
  sub-agent timeout are high enough for a full call (no single command; raise
  whatever limit your runtime enforces).

> The **Hermes** and **OpenClaw** commands above are the proven settings from the
> heyarp Hermes/OpenClaw setup, and map to aura's dispatch (Hermes = an unattended
> `delegate_task` worker; OpenClaw = a gateway consult). aura's Hermes/OpenClaw
> paths are not yet live-verified, so confirm the exact config keys against your
> installed framework version. Claude Code is the live-verified default.

---

## Step 6 — Self-check (gate before you report "done")

Confirm everything is wired **without** dialing the model. Run the three checks,
then gate completion on the checklist at the end — do **not** report success
until every box is checked.

**6a. Binaries on `PATH`.** The server must be installed; the client is only
required on the same machine for a LOCAL call.

```bash
command -v aura-server && echo "server: OK" || echo "server MISSING — redo step 2"
# The on-PATH host helpers the skill drives (call + status + the orchestrator inbox):
for h in aura-call aura-call-status aura-inbox; do
  command -v "$h" >/dev/null && echo "$h: OK" || echo "$h MISSING — redo step 2"
done
# LOCAL only (client lives on the user's machine for REMOTE):
command -v aura-cli && echo "client present (LOCAL ready)" || echo "client not here (expected for REMOTE)"
```

**6b. The key resolves.** Run the server's startup just long enough to prove the
key is found, then stop it before it waits for a caller. The server resolves the
key **before** it binds or mints anything; if the key is missing it exits
non-zero immediately with a clear message.

```bash
# From the server working directory (where ./.env lives).
# It prints "host = ..." / "composed context ..." and then the connection-string
# line on stderr. Reaching that line proves the key resolved. We stop it there.
timeout 8s aura-server 2>&1 | grep -m1 -E 'AURA_CONNECT=|composed context|host =' \
  && echo "key resolved + server starts: OK" \
  || echo "server did NOT start — check the XAI_API_KEY (step 3)"
```

If instead you see `no BYOK xAI key found` (or `aura-server: ... key ...`), the
key is not resolving — revisit step 3 (is `./.env` in *this* directory, is the
line `XAI_API_KEY=...`, is the env var unset so `./.env` is consulted?). The
connection string printed during this probe is throwaway (single-use, ~120 s) —
it is never used and expires harmlessly.

**6c. (REMOTE) Reachability reminder.** You cannot fully verify external UDP
reachability from inside the VM. Remind the user that step 4 must be complete on
**both** the OS firewall and the cloud security group / NAT.

**✅ Self-check — do NOT report "done" until every box is checked:**

- [ ] `aura-server` is on `PATH` (6a) — and `aura-cli` too if this is a LOCAL host
- [ ] `aura-call`, `aura-call-status`, and `aura-inbox` are on `PATH` (Step 2)
- [ ] the `XAI_API_KEY` resolves — the server reaches the connection-string line (6b)
- [ ] (REMOTE) UDP 47821 was opened once — OS firewall **and** cloud SG / NAT (Step 4)
- [ ] `skills/SKILL.md` was copied into your skills directory (Step 5)
- [ ] your framework is set to **auto-approve** the orchestrator's tool calls and has a high enough turn/timeout budget for a full call (Step 5b)
- [ ] you did NOT re-ask the user beyond the key (and at most the one mode question), and did NOT stop at the build

Any unchecked box → go back to that step and fix it. Only when **all** boxes are
checked do you continue to Step 7 and tell the user how to start a call.

---

## Step 7 — Tell the user how to start a call

Onboarding is done. Explain to the user, in plain language, what happens next.
The host skill (step 5) launches `aura-server` for them on each call; they do
not normally run it by hand. For reference, this is what a call does:

**LOCAL call (server on the user's own machine):**

1. The host launches the server on loopback (`AURA_PUBLIC_HOST=127.0.0.1`, the
   default).
2. The server prints, on stderr, exactly:
   `    AURA_CONNECT='aura://127.0.0.1:47821#k=...&c=...' aura-cli`
3. The user (or the skill, on their behalf) runs that line — `aura-cli` opens the
   mic and the call begins.

**REMOTE call (server on the VPS):**

1. The host launches the server with `AURA_PUBLIC_HOST=<the VPS public IP from
   step 4>` (the port was already opened once at onboarding).
2. The server prints the connection string on stderr.
3. The skill **sends the connection string to the user over the chat**.
4. On **their own** machine the user runs:
   `AURA_CONNECT='aura://<vps-ip>:47821#k=...&c=...' aura-cli`
   (they install `aura-cli` once via `install.sh --client` — see the README).

Tell the user plainly: *"To start a call, say 'call me' (or use the host's call
command) in your chat. I'll launch the voice server; you'll either be connected
automatically (local) or I'll send you a one-line `AURA_CONNECT=... aura-cli`
command to paste on your machine (remote)."*

The connection string's secret is **single-use** and valid for about **120
seconds** — if it expires, just start another call. After the call ends, the
server posts a short recap of the conversation back into the chat through the
host callback (for Claude this is a file under `.aura/` in the repo that the
skill reads and summarizes).

### Optional server knobs (environment)

The server is env-driven (no config file is loaded); the skill launches it and
`aura-call` passes the environment through. All optional:

- `AURA_DISPATCH_MODEL=<model>` — pin the in-call dispatch model for delegated
  work. By default **Claude** auto-matches the dispatched sub-agent to the live
  chat session's model (read from the transcript); **Codex** does NOT auto-match —
  without a pin it runs on the app-server's default model. Set this to force a
  specific model on either, e.g. `AURA_DISPATCH_MODEL=claude-opus-4-8`. Only Claude
  and Codex have a per-call model knob; OpenClaw/Hermes ignore it.
- `AURA_PORT` / `AURA_PUBLIC_HOST` / `AURA_TRANSPORT=iroh` — UDP port, the public
  host clients dial, and the optional P2P transport (all covered above).
- `AURA_FEEDER=1` — opt in to the live ambient-context feeder (needs `claude` on `PATH`).

---

## Troubleshooting

- **Client can't reach a REMOTE server / call never connects.** The UDP port is
  almost always blocked **outside** the VM by the cloud **security group / NAT**.
  The in-VM firewall (step 4b) is necessary but **not sufficient** — add an
  inbound **UDP 47821** rule in the cloud provider console. Confirm
  `AURA_PUBLIC_HOST` is the *reachable* public IP/DNS name, not a private/loopback
  address.
- **`aura-server` exits immediately with a key error
  (`no BYOK xAI key found`).** The `XAI_API_KEY` did not resolve. Check, in
  order: is the env var exported in *this* shell? is there a `./.env` with a
  `XAI_API_KEY=...` line in the server's working directory? is the env var unset
  so `./.env` is even consulted (a set-but-empty env var does not fall through to
  `./.env`)? Re-do step 3. Never print the key to debug it.
- **Build error mentioning `alsa` / `libasound` / `asound`.** That dependency is
  **client-only** — only `aura-cli` pulls cpal/ALSA. If you are building the
  **server**, you do not need it; make sure you ran `./install.sh --server`
  (not the default which also builds the client). If you *are* building the
  client on Linux, install the ALSA dev headers: `libasound2-dev`
  (Debian/Ubuntu, `apt`), `alsa-lib-devel` (Fedora/RHEL, `dnf`), `alsa-lib`
  (Arch, `pacman`), `alsa-lib-devel` (openSUSE, `zypper`). macOS and Windows
  need no extra audio package.
- **`cargo`/`rustup` not found.** Install Rust as in step 2
  (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`,
  then `source "$HOME/.cargo/env"`); the pin file selects 1.92.0 automatically.
- **`aura-server` / `aura-cli` not found after install.** `~/.local/bin` is not
  on `PATH` yet. Restart the shell or `source ~/.bashrc` (or `~/.zshrc` /
  `~/.profile`), or run with the full path `~/.local/bin/aura-server`.
- **Connection string expired.** It is single-use and lasts ~120 s. Start a new
  call to mint a fresh one.
- **`scripts/aura-open-port.sh` printed `sudo` commands.** That means it ran
  without root and could not change the firewall itself — relay those exact
  commands to the user to run, then re-verify reachability.
