---
name: voice-call-windows
description: >-
  Windows companion to the universal Aura voice-call skill. Read it ALONGSIDE
  skills/SKILL.md when the host is Windows — SKILL.md owns the call flow; this
  file adds only the Windows-only differences (install.ps1, PowerShell client
  launch, running the POSIX helpers under Git Bash/WSL, WASAPI audio, PATH).
trigger: >-
  The host is on Windows, or the user asks about install.ps1, PowerShell, PATH,
  WASAPI audio, Git Bash / WSL, or any Windows-specific Aura difference.
allowed-tools: Bash
---

# Aura on Windows — companion to `skills/SKILL.md`

This is **not** a standalone skill. The whole call flow — Step 1 (LOCAL/REMOTE),
Step 2 (launch + connection string), Step 3 (connect), Step 4 (the `aura-inbox`
orchestration loop), Step 5 (recap), the per-host table, and the rules — lives in
`skills/SKILL.md` and applies **unchanged** on Windows. The one catch: the
`aura-call` / `aura-inbox` / `aura-call-status` helpers are POSIX shell scripts,
so you run that flow in **Git Bash, WSL, or MSYS**, not plain PowerShell. Below
are only the Windows deltas.

Never start a call unless the user asks. Never echo/print/log `XAI_API_KEY` or the
`AURA_CONNECT` secret.

## Install (PowerShell, from source)

Windows builds from source — there are no prebuilt Windows binaries. From the
cloned repo root, run `install.ps1`:

```powershell
.\install.ps1            # both binaries + helpers (default)
.\install.ps1 -Server    # server only    .\install.ps1 -Client   # client only
.\install.ps1 -Prefix "$env:USERPROFILE\.local"   # custom prefix (default is this)
.\install.ps1 -Uninstall
```

- Binaries land in `%USERPROFILE%\.local\bin` (`aura-cli.exe`, `aura-server.exe`)
  next to the POSIX helpers (`aura-call`, `aura-call-status`, `aura-inbox`).
- `install.ps1` adds that dir to the **user PATH**; an already-open terminal must
  be reopened before it sees the commands.
- Building may need the **Microsoft C++ Build Tools** — if linking fails with
  `link.exe` / MSVC errors, install the "Desktop development with C++" workload.
  Rust `1.92.0` is auto-selected from `rust-toolchain.toml`.

Confirm from PowerShell:

```powershell
Get-Command aura-cli.exe; Get-Command aura-server.exe
```

## Run the call flow (helpers) in a POSIX shell

`aura-call`, `aura-inbox`, and `aura-call-status` are POSIX scripts. Run the
**entire `SKILL.md` flow** — the server launch and the Step 4 inbox loop — in Git
Bash / WSL / MSYS, exactly as written there:

```bash
aura-call local --host <kind>      # or: aura-call remote --host <kind>
aura-inbox alive
aura-inbox wait --timeout 20
aura-inbox done <id> "..."
```

In plain PowerShell you can only invoke the `.exe`s directly (e.g.
`aura-cli.exe --help`), not the helper scripts.

## Launch the client from PowerShell

The connection string carries the session secret — pass it via the environment
or stdin, **never as an argument**:

```powershell
$env:AURA_CONNECT = 'aura://HOST:PORT#k=...&c=...'
aura-cli.exe
```

Or run `aura-cli.exe` with no args and paste the string on stdin when prompted.
(You may also launch the client from Git Bash the same way `SKILL.md` shows.)

## Audio (WASAPI)

- Audio uses **WASAPI** — no ALSA or extra audio package is needed.
- Windows must allow it: turn on **"Allow desktop apps to access your
  microphone"**; `aura-cli.exe` then shows as using the mic (the Voice Recorder
  app does **not** need to be enabled).
- Echo cancellation (AEC3) is on by default. If the user disables it, note that
  `AURA_AEC=off` has been seen to fail on Windows with
  `Access is denied. (0x80070005)` — prefer `AURA_AEC=gate` (mic muted while Aura
  speaks; no barge-in) or headphones over `off` unless the client log proves
  `off` starts.

## Key + config

Key storage and per-call provider/model switching follow `SKILL.md` / onboarding
unchanged. The user-global aura `.env` on Windows resolves to
`%USERPROFILE%\.config\aura\.env` (in Git Bash, `~/.config/aura/.env` is the same
file); the OS keychain equivalent is **Windows Credential Manager**. Paste the key
only into `.env`, the process environment, or Credential Manager — never echo it.

Useful Windows-visible env values:

```env
AURA_PORT=47821
AURA_PUBLIC_HOST=127.0.0.1
AURA_TRANSPORT=direct
AURA_AEC=on
AURA_STATE_DIR=.
AURA_FEEDER=false
AURA_END_OF_TURN_TIMEOUT_MS=2000
AURA_INPUT_MODE=voice
AURA_PUSH_TO_TALK_HOTKEY=ctrl+space
AURA_PUSH_TO_TALK_MAX_RECORDING_MS=300000
```

Do not add an empty `AURA_BIND=` line. If `AURA_BIND` exists, the server treats
it as an explicit bind override.

`AURA_END_OF_TURN_TIMEOUT_MS` controls how much silence Aura waits before it
decides the user's turn is finished and starts answering in `voice` mode. Use
`1500`, `2000`, or `2500` when Aura interrupts too quickly. The provider clamps
the value to `300..3000` ms in `voice` mode. In `push_to_talk` mode, Aura uses
manual hotkey turn commit and ignores this value.

`AURA_INPUT_MODE` is set on the server. The connection string carries the mode
to `aura-cli.exe` as `m=voice` or `m=ptt`, and the client follows it
automatically:

```env
AURA_INPUT_MODE=voice
AURA_INPUT_MODE=push_to_talk
```

In Windows `push_to_talk` mode, the user presses the global hotkey once to start
sending mic audio, speaks, then presses the hotkey again to commit the turn and
ask Aura to answer. The default hotkey is:

```env
AURA_PUSH_TO_TALK_HOTKEY=ctrl+space
```

Set `AURA_PUSH_TO_TALK_HOTKEY` before starting `aura-cli.exe`; changing `.env`
does not affect an already-running client. `AURA_PUSH_TO_TALK_MAX_RECORDING_MS`
is a client safety cap for an accidentally open mic. The default is `300000`
ms, about five minutes. Three seconds before the cap, Aura warns that the voice
message limit is near. Very short push-to-talk taps are discarded instead of
sent; the client prints that the message was too short.

`aura-cli.exe` deliberately ignores `AURA_CONNECT` from `.env` files. The
connection string must come from the real process environment or stdin, so a
target repository cannot redirect the user's microphone by planting a local
`.env`.

For a local call on the same Windows machine:

- `AURA_PUBLIC_HOST=127.0.0.1`
- `AURA_PORT=47821`
- `AURA_TRANSPORT=direct`
- Windows Firewall usually does not need a public inbound rule for loopback.
- Aura creates a local `.aura/` runtime folder in `AURA_STATE_DIR` or the server
  working directory. If that folder is inside a git worktree, Aura best-effort
  adds `.aura/` to `.git/info/exclude`, never to tracked `.gitignore`.
- In default `voice` mode, the user does not press a button. When `aura-cli.exe`
  logs `speak when you hear Aura`, the mic is open and the user can speak
  normally.

## Common Windows problems

- `cargo` / `rustup` not found → install Rust with rustup, then open a new terminal.
- Linker / MSVC error → install the C++ Build Tools ("Desktop development with C++").
- `aura-cli.exe` not found after install → reopen the terminal so the user PATH reloads.
- `aura-call` won't run in PowerShell → it is a POSIX script; use Git Bash / WSL / MSYS.
- `XAI_API_KEY` not found → check `.env` in the server's launch dir, or set the
  user-global `%USERPROFILE%\.config\aura\.env`.
- Aura interrupts too quickly → set `AURA_END_OF_TURN_TIMEOUT_MS` to a larger
  value, usually `2000` or `2500`, then restart Aura.
- User thinks they need Voice Recorder → no. Aura uses `aura-cli.exe`; the
  important Windows setting is "Allow desktop apps to access your microphone".
