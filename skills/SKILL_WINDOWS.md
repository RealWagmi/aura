---
name: voice-call-windows
description: >-
  Complete Windows Aura install and call skill. Use this instead of the universal
  Aura skill when the user is on Windows, installs with install.ps1, or asks
  about PowerShell, PATH, .env, WASAPI audio, Git Bash, WSL, or Windows
  differences.
trigger: >-
  The user is installing or running Aura on Windows, or asks for Windows-specific
  setup details.
allowed-tools: PowerShell
---

# Aura on Windows

This is the standalone Windows skill. When the machine is Windows, future AI
agents should use this file as the source of truth instead of combining it with
`skills/SKILL.md`.

Never start a voice call unless the user explicitly asks for one. Never print,
echo, or log `XAI_API_KEY` or the `AURA_CONNECT` secret.

## What is different on Windows

- Use PowerShell commands for install and setup.
- Windows builds from source. There are no Windows prebuilt binaries in
  `install_bin.sh`.
- Audio uses WASAPI. No ALSA package or extra audio package is needed.
- Rust is selected from `rust-toolchain.toml`; this repo pins Rust `1.92.0`.
- Native Rust on Windows may need Microsoft C++ Build Tools. If linking fails
  with `link.exe` or MSVC errors, install "Desktop development with C++".
- The default install prefix is `%USERPROFILE%\.local`.
- Installed binaries go to `%USERPROFILE%\.local\bin`.
- `install.ps1` adds `%USERPROFILE%\.local\bin` to the user PATH. Already-open
  terminals may need restart before they see it.
- `aura-cli.exe` and `aura-server.exe` are Windows executables.
- The helper commands `aura-call`, `aura-call-status`, and `aura-inbox` are
  POSIX shell scripts copied next to the binaries. On Windows they need Git
  Bash, WSL, or MSYS to run.

## Install

From the repo root:

```powershell
Set-Location C:\New\WagmiV3\aura
.\install.ps1
```

Install only the client:

```powershell
.\install.ps1 -Client
```

Install only the server:

```powershell
.\install.ps1 -Server
```

Install to a custom prefix:

```powershell
.\install.ps1 -Prefix "$env:USERPROFILE\.local"
```

Uninstall:

```powershell
.\install.ps1 -Uninstall
```

## Expected installed files

Default location:

```text
%USERPROFILE%\.local\bin\aura-cli.exe
%USERPROFILE%\.local\bin\aura-server.exe
%USERPROFILE%\.local\bin\aura-call
%USERPROFILE%\.local\bin\aura-call-status
%USERPROFILE%\.local\bin\aura-inbox
```

Check from PowerShell:

```powershell
Get-Command aura-cli.exe
Get-Command aura-server.exe
```

If these commands are not found after install, open a new terminal and try
again. The installer updates the user PATH, but the current IDE terminal may not
reload it automatically.

## Local .env

For a repo-local setup, the file is:

```text
C:\New\WagmiV3\aura\.env
```

Minimum required value:

```env
XAI_API_KEY=
```

Useful visible defaults:

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
```

Do not add an empty `AURA_BIND=` line. In this project, if `AURA_BIND` exists,
the server treats it as an explicit bind override.

`AURA_END_OF_TURN_TIMEOUT_MS` controls how much silence Aura waits before it
decides the user's turn is finished and starts answering. Use it when Aura
interrupts the user too quickly. Practical values:

```env
AURA_END_OF_TURN_TIMEOUT_MS=1500
AURA_END_OF_TURN_TIMEOUT_MS=2000
AURA_END_OF_TURN_TIMEOUT_MS=2500
```

The provider clamps the value to `300..3000` ms in `voice` mode. When
`AURA_INPUT_MODE=push_to_talk`, Aura ignores `AURA_END_OF_TURN_TIMEOUT_MS` and
uses `0` automatically. Set it before starting `aura-server`; changing `.env`
does not affect an already-running call.

`AURA_INPUT_MODE` controls how the client sends the user's speech:

```env
AURA_INPUT_MODE=voice
AURA_INPUT_MODE=push_to_talk
```

In `voice` mode, Aura listens normally and uses voice/silence detection. In
Windows `push_to_talk` mode, the user presses the global hotkey once to start
recording, speaks, then presses the hotkey again to send the recorded voice to
Aura. The default hotkey is:

```env
AURA_PUSH_TO_TALK_HOTKEY=ctrl+space
```

The hotkey is global on Windows, so it works while Cursor, a browser, or another
app has focus. Set these values before starting `aura-cli`; changing `.env` does
not affect an already-running client.

Do not print, echo, or log `XAI_API_KEY`. Paste it only into `.env`, the process
environment, or Windows Credential Manager.

## Running the client from PowerShell

Do not put the connection string after `aura-cli.exe` as an argument. Put it in
the environment variable, then run the client:

```powershell
$env:AURA_CONNECT = 'aura://HOST:PORT#k=...&c=...'
aura-cli.exe
```

Or run:

```powershell
aura-cli.exe
```

Then paste the connection string into stdin when prompted.

## Running call helpers on Windows

The helper commands are shell scripts. Use Git Bash, WSL, or MSYS for these:

```bash
aura-call local --host codex
aura-inbox alive
aura-inbox wait --timeout 20
aura-call-status
```

In PowerShell, call the Windows executables directly for basic checks:

```powershell
aura-cli.exe --help
aura-server.exe --help
```

## Windows call flow

Use this section when the user asks for a voice call on Windows.

1. Decide local or remote.

   Local means `aura-cli.exe` and `aura-server.exe` run on this Windows machine.
   Remote means the server runs somewhere else, and this Windows machine runs
   only `aura-cli.exe`.

2. Check installed commands from PowerShell.

   ```powershell
   Get-Command aura-cli.exe
   Get-Command aura-server.exe
   ```

   If they are missing, run `.\install.ps1` from the repo root or open a new
   terminal if install already finished.

3. Confirm the key location without printing the key.

   For repo-local setup:

   ```powershell
   Test-Path C:\New\WagmiV3\aura\.env
   ```

   The file must contain `XAI_API_KEY=<real key>` before a real call can work.
   Without it, build and install can continue, but the voice server cannot
   bridge to xAI.

4. Launch the server.

   The helper `aura-call` is a POSIX shell script, so run this part in Git Bash,
   WSL, or MSYS, not plain PowerShell:

   ```bash
   aura-call local --host codex
   ```

   For a remote server with a public host:

   ```bash
   aura-call remote <PUBLIC_HOST_OR_IP> --host codex
   ```

   For iroh transport behind NAT:

   ```bash
   AURA_TRANSPORT=iroh aura-call remote --host codex
   ```

   The command prints an `AURA_CONNECT=... aura-cli` line. Treat that line as a
   secret. It is single-use and short-lived.

5. Connect the Windows client.

   In PowerShell, convert the printed connection line to this form:

   ```powershell
   $env:AURA_CONNECT = 'aura://HOST:PORT#k=...&c=...'
   aura-cli.exe
   ```

   Do not pass the connection string as a command-line argument.

6. Monitor the call.

   Use Git Bash, WSL, or MSYS for the helper loop:

   ```bash
   aura-inbox alive
   aura-inbox wait --timeout 20
   ```

   `aura-inbox alive` prints `ALIVE <dir>`. That directory must match the
   intended `AURA_STATE_DIR`. If it does not, stop and fix `AURA_STATE_DIR`
   before starting a new call.

   `aura-inbox wait --timeout 20` can print:

   - `NO_TASK`: wait again.
   - `CALL_ENDED state=<ended|failed|dropped>`: stop the loop and summarize.
   - `TASK <id>`: handle the task, then report back:

   ```bash
   aura-inbox done <id> "Done."
   ```

7. After the call, summarize the result in chat. Do not paste raw transcripts or
   secrets.

## Known-good local PowerShell launch

This was verified on Windows with `aura-server.exe` and `aura-cli.exe` installed
in `%USERPROFILE%\.local\bin`. To discuss a specific repository, launch
`aura-server.exe` with that repository as the working directory. The server's
working directory is the project context for the call.

Example repository used during local testing:

```text
C:\New\WagmiV3\heyanon-github-analyzer
```

Use the Aura repo `.env` to load `XAI_API_KEY`, start the server from the target
repo, capture the one-time connection string internally, and pass it directly to
`aura-cli.exe`. Do not print the connection string.

```powershell
$repo = 'C:\New\WagmiV3\heyanon-github-analyzer'
$envFile = 'C:\New\WagmiV3\aura\.env'

Get-Content -LiteralPath $envFile | ForEach-Object {
    $line = $_.Trim()
    if ($line.Length -eq 0 -or $line.StartsWith('#')) { return }
    $idx = $line.IndexOf('=')
    if ($idx -le 0) { return }
    $name = $line.Substring(0, $idx).Trim()
    $value = $line.Substring($idx + 1)
    if ($name -and $value) { Set-Item -Path "Env:$name" -Value $value }
}

$logDir = Join-Path $env:TEMP ("aura-local-" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $logDir -Force | Out-Null
$serverErr = Join-Path $logDir 'server.err.log'
$clientErr = Join-Path $logDir 'client.err.log'

$server = Start-Process -FilePath 'aura-server.exe' -WorkingDirectory $repo `
    -PassThru -WindowStyle Hidden -RedirectStandardError $serverErr

$conn = $null
$deadline = (Get-Date).AddSeconds(30)
while ((Get-Date) -lt $deadline -and -not $conn) {
    Start-Sleep -Milliseconds 500
    $text = Get-Content -LiteralPath $serverErr -Raw -ErrorAction SilentlyContinue
    $m = [regex]::Match($text, "AURA_CONNECT='([^']+)'\s+aura-cli")
    if ($m.Success) { $conn = $m.Groups[1].Value }
}
if (-not $conn) { throw "server did not produce a connection string; see $logDir" }

$env:AURA_CONNECT = $conn
$client = Start-Process -FilePath 'aura-cli.exe' -WorkingDirectory $repo `
    -PassThru -WindowStyle Hidden -RedirectStandardError $clientErr
```

Stop/restart all Aura processes:

```powershell
Get-Process aura-cli,aura-server -ErrorAction SilentlyContinue | Stop-Process -Force
```

## Local Windows call notes

For a local call on the same Windows machine:

- `AURA_PUBLIC_HOST=127.0.0.1`
- `AURA_PORT=47821`
- `AURA_TRANSPORT=direct`
- Windows Firewall usually does not need a public inbound rule for loopback.
- The real xAI voice call still requires `XAI_API_KEY`.
- The connection string is not stable config. It is a single-use per-call secret
  from `aura-server`, passed to `aura-cli.exe` through `AURA_CONNECT` or stdin.
- Aura creates a local `.aura/` runtime folder in `AURA_STATE_DIR` or the
  server working directory. If that folder is inside a git worktree, Aura
  best-effort adds `.aura` to that directory's `.gitignore`.
- `AURA_END_OF_TURN_TIMEOUT_MS` must be set before launch if the user wants a
  longer pause before Aura answers.
- In default `voice` mode, the user does not press a button. When `aura-cli`
  logs `speak when you hear Aura`, the mic is open and the user can speak
  normally.
- In `AURA_INPUT_MODE=push_to_talk`, the user presses the global hotkey once to
  start recording, speaks, then presses the same hotkey again to send the voice
  message to Aura.
- Windows microphone settings should show `aura-cli.exe` under desktop apps as
  currently using the microphone. `Voice Recorder` does not need to be enabled.
- Verified local audio device log shape:

  ```text
  [aura-audio] input  device  : Microphone (Realtek(R) Audio)
  [aura-audio] output device  : Speakers (Realtek(R) Audio)
  [aura-audio] echo cancellation ON (AEC3; noise suppression 'medium')
  ```
- If the user hears Aura and Aura responds, the basic xAI, tunnel, speaker, and
  microphone path works.
- Large response lag can still happen from model/network latency, first-call
  warmup, Realtek mic/speaker behavior, or echo cancellation.
- On the tested Windows machine, restarting with `AURA_AEC=off` failed with
  `Access is denied. (0x80070005)`. Do not assume `AURA_AEC=off` is a safe
  Windows workaround unless the client log proves it starts successfully.

For remote/VPS calls from Windows:

- The Windows computer usually runs only `aura-cli.exe`.
- The server runs on the VPS and sends the `AURA_CONNECT=... aura-cli` line.
- In PowerShell, convert that line to `$env:AURA_CONNECT = '...'` then run
  `aura-cli.exe`.

## Common Windows problems

- `cargo` not found: install Rust with rustup, then open a new terminal.
- Rust version mismatch: run `rustup default 1.92.0` or let the repo
  `rust-toolchain.toml` select it from inside the repo.
- Linker/MSVC error: install Microsoft C++ Build Tools with the "Desktop
  development with C++" workload.
- `aura-cli.exe` not found after install: restart the terminal so user PATH is
  reloaded.
- `aura-call` does not run in PowerShell: use Git Bash, WSL, or MSYS because it
  is a POSIX shell script.
- `XAI_API_KEY` not found: check the `.env` file in the server launch directory
  or set a user-global Aura env file.
- User hears Aura but speech has large lag: first confirm the Windows input
  meter moves for the exact device in the `aura-cli` log, then try headphones
  and restart normal mode. Avoid `AURA_AEC=off` if it produces
  `Access is denied. (0x80070005)`.
- Aura interrupts the user too quickly: set `AURA_END_OF_TURN_TIMEOUT_MS` to a
  larger value, usually `2000` or `2500`, then restart Aura.
- User thinks they need Voice Recorder: no. Aura uses `aura-cli.exe`, and the
  important Windows setting is "Allow desktop apps to access your microphone".

## Windows test fixes verified locally

During Windows validation, these code/test portability fixes were needed:

- `aura-core` config tests should build JSON with `serde_json` instead of manual
  path interpolation, because Windows backslashes must be escaped in JSON.
- The live hardware audio smoke test should be opt-in on Windows with
  `AURA_LIVE_AUDIO_TEST=1`, because WASAPI/device drivers can crash a normal
  `cargo test` run even when non-hardware audio tests are fine.
- `aura-feeder` tests should use native PowerShell `.ps1` fake Claude scripts on
  Windows and Bash `.sh` fake Claude scripts on Unix. Do not skip these tests on
  Windows; the PowerShell stubs keep full feeder coverage.

Verified commands:

```powershell
cargo test -p aura-feeder --lib
cargo test
```
