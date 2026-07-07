<#
.SYNOPSIS
    aura — Windows source-build installer (PowerShell).

.DESCRIPTION
    Builds aura from source with cargo and installs the selected binaries into
    a per-user bin directory (default %USERPROFILE%\.local\bin). This mirrors
    install.sh. There are NO prebuilt binaries — everything is compiled locally.

      aura-cli    — the THIN CLIENT. Runs on the user's machine (it owns the
                    mic/speaker via WASAPI). Holds NO key, NO engine, NO host.
      aura-server — the SERVER the host/AI launches (127.0.0.1 for a LOCAL call,
                    a VPS for a REMOTE one). Holds the API key (xAI or OpenAI) + the engine + the
                    chat context + the tools, mints a per-call session secret,
                    prints a connection string.

    Windows audio is WASAPI: NO extra audio package is needed on either side.
    rustup honors rust-toolchain.toml automatically (Rust 1.92.0).

.PARAMETER Client
    Build and install aura-cli only.

.PARAMETER Server
    Build and install aura-server only.

.PARAMETER Prefix
    Install prefix. Binaries go to <Prefix>\bin. Default: %USERPROFILE%\.local

.PARAMETER Uninstall
    Remove the installed binaries from <Prefix>\bin.

.PARAMETER Help
    Show usage and exit.

.EXAMPLE
    .\install.ps1
    Build and install both binaries.

.EXAMPLE
    .\install.ps1 -Client
    Build and install only the thin client (aura-cli).
#>

[CmdletBinding()]
param(
    [switch]$Client,
    [switch]$Server,
    [string]$Prefix,
    [switch]$Uninstall,
    [Alias('h')]
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# --- console helpers --------------------------------------------------------

function Write-Info  { param([string]$Message) Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Note  { param([string]$Message) Write-Host "    $Message" -ForegroundColor DarkGray }
function Write-Ok    { param([string]$Message) Write-Host "ok  $Message" -ForegroundColor Green }
function Write-Warn  { param([string]$Message) Write-Host "warn $Message" -ForegroundColor Yellow }
function Die {
    param([string]$Message)
    Write-Host "error $Message" -ForegroundColor Red
    exit 1
}

# Pointer for an AI agent: the full server setup is the onboarding guide, not
# this installer. Shown before building when the server is selected.
function Show-OnboardingBanner {
    Write-Host ''
    Write-Host '================================================================================' -ForegroundColor Cyan
    Write-Host '  *** AI AGENT: THIS INSTALLER ONLY BUILDS THE BINARIES. ***' -ForegroundColor Red
    Write-Host '  *** MANDATORY: READ docs/ONBOARDING.md IN FULL, THEN FOLLOW EVERY STEP. ***' -ForegroundColor Red
    Write-Host '================================================================================' -ForegroundColor Cyan
    Write-Note 'The onboarding guide is written for YOU (the AI) and DRIVES the whole setup —'
    Write-Note 'the xAI key, the one-time firewall, installing the host skill, and a final'
    Write-Note 'self-check. The setup is NOT done when this build finishes. Read it top to'
    Write-Note 'bottom and execute every step IN ORDER; do not improvise, skip, or stop early.'
    Write-Note '    https://raw.githubusercontent.com/RealWagmi/aura/main/docs/ONBOARDING.md'
    Write-Host '================================================================================' -ForegroundColor Cyan
    Write-Host ''
}

function Show-Help {
@'
aura installer (Windows / PowerShell) — builds from source, no prebuilt binaries.

Usage:
    .\install.ps1 [options]

Options:
    -Client          Build and install aura-cli only (the thin client).
    -Server          Build and install aura-server only.
                     (default, with neither flag: build and install BOTH)
    -Prefix DIR      Install prefix; binaries go to DIR\bin.
                     Default: %USERPROFILE%\.local
    -Uninstall       Remove installed aura binaries from <Prefix>\bin.
    -Help, -h        Show this help and exit.

What gets installed:
    aura-cli.exe     The CLIENT (mic/speaker via WASAPI). No key, no engine.
    aura-server.exe  The SERVER the host/AI launches. Holds the API key + engine.

Windows audio is WASAPI: no extra audio package is required on either side.
The Rust toolchain is pinned by rust-toolchain.toml (1.92.0); rustup selects it.
Building native Rust on Windows requires the MSVC C++ build tools; if `cargo
build` fails for that reason this script prints how to install them.
'@ | Write-Host
}

if ($Help) {
    Show-Help
    exit 0
}

# --- resolve repo root (this script lives at the workspace root) ------------

$RepoRoot = $PSScriptRoot
if ([string]::IsNullOrEmpty($RepoRoot)) {
    $RepoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
}
# If we are not inside an aura checkout (e.g. run via `irm ... | iex`), clone the
# repo and build from there — this makes the one-line installer self-contained.
$haveCheckout = (-not [string]::IsNullOrEmpty($RepoRoot)) -and (Test-Path -LiteralPath (Join-Path $RepoRoot 'Cargo.toml'))
if (-not $haveCheckout) {
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        Die "git is required to fetch the aura source. Install Git for Windows and re-run, or clone manually: git clone https://github.com/RealWagmi/aura"
    }
    $dest = if ($env:AURA_SRC_DIR) { $env:AURA_SRC_DIR } else { Join-Path $env:USERPROFILE 'aura' }
    if (Test-Path -LiteralPath (Join-Path $dest 'Cargo.toml')) {
        Write-Info "Using the existing aura checkout at $dest"
        # Re-running the installer must UPDATE, not silently rebuild the old
        # checkout. Pull only when it is a clean git clone (never clobber local
        # edits); on any failure fall back to building the checkout as-is.
        if (Test-Path -LiteralPath (Join-Path $dest '.git')) {
            $dirty = git -C $dest status --porcelain 2>$null
            if ([string]::IsNullOrWhiteSpace(($dirty -join ''))) {
                Write-Info 'Updating the checkout (git pull --ff-only)'
                git -C $dest pull --ff-only
                if ($LASTEXITCODE -ne 0) { Write-Warn 'git pull failed; building the existing checkout as-is.' }
            } else {
                Write-Warn "local changes in $dest - skipping git pull; building the checkout as-is."
            }
        }
    } else {
        Write-Info "Fetching the aura source into $dest (git clone)"
        git clone --depth 1 https://github.com/RealWagmi/aura $dest
        if ($LASTEXITCODE -ne 0) { Die "git clone failed" }
    }
    $RepoRoot = $dest
    Set-Location -LiteralPath $RepoRoot
}

# --- resolve install prefix / bin dir ---------------------------------------

if ([string]::IsNullOrWhiteSpace($Prefix)) {
    $Prefix = Join-Path $env:USERPROFILE '.local'
}
$BinDir = Join-Path $Prefix 'bin'

# --- selection: default = both ----------------------------------------------

$BuildClient = $Client.IsPresent
$BuildServer = $Server.IsPresent
if (-not $BuildClient -and -not $BuildServer) {
    $BuildClient = $true
    $BuildServer = $true
}

# Each binary as (package, exe-file-name).
$Targets = @()
if ($BuildClient) { $Targets += , @('aura-cli',    'aura-cli.exe') }
if ($BuildServer) { $Targets += , @('aura-server', 'aura-server.exe') }

# --- uninstall path ---------------------------------------------------------

if ($Uninstall) {
    Write-Info "Uninstalling aura binaries from '$BinDir'"
    $removed = 0
    foreach ($t in $Targets) {
        $exe = Join-Path $BinDir $t[1]
        if (Test-Path -LiteralPath $exe) {
            Remove-Item -LiteralPath $exe -Force
            Write-Ok "removed $exe"
            $removed++
        } else {
            Write-Note "not present: $exe"
        }
    }
    if ($BuildServer) {
        foreach ($h in @('aura-call', 'aura-call-status', 'aura-inbox')) {
            $hp = Join-Path $BinDir $h
            if (Test-Path -LiteralPath $hp) { Remove-Item -LiteralPath $hp -Force; Write-Ok "removed $hp"; $removed++ }
        }
    }
    if ($removed -eq 0) {
        Write-Note 'Nothing to remove.'
    }
    Write-Note "PATH entry '$BinDir' (if added) was left in place; remove it manually if you want it gone."
    exit 0
}

# --- update vs fresh install -------------------------------------------------

# A previous aura binary in BinDir means this run replaces an existing install
# (used for the post-install skill notice).
$IsUpdate = (Test-Path -LiteralPath (Join-Path $BinDir 'aura-server.exe')) -or
            (Test-Path -LiteralPath (Join-Path $BinDir 'aura-cli.exe'))
if ($IsUpdate) { Write-Info "Existing aura install detected in $BinDir - this run is an UPDATE." }

# --- onboarding pointer (server = the AI-driven side) -----------------------

if ($BuildServer) { Show-OnboardingBanner }

# --- ensure rustup / cargo --------------------------------------------------

function Test-Command {
    param([string]$Name)
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Install-Rustup {
    Write-Info 'rustup/cargo not found — installing the Rust toolchain via rustup'

    $arch = $env:PROCESSOR_ARCHITECTURE
    switch ($arch) {
        'AMD64' { $rustupUrl = 'https://win.rustup.rs/x86_64' }
        'ARM64' { $rustupUrl = 'https://win.rustup.rs/aarch64' }
        'x86'   { $rustupUrl = 'https://win.rustup.rs/i686' }
        default { $rustupUrl = 'https://win.rustup.rs/x86_64' }
    }

    $installer = Join-Path ([System.IO.Path]::GetTempPath()) 'rustup-init.exe'
    Write-Note "downloading $rustupUrl"
    try {
        # Force a modern TLS for older Windows PowerShell builds.
        try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch { }
        Invoke-WebRequest -Uri $rustupUrl -OutFile $installer -UseBasicParsing
    } catch {
        Die "failed to download rustup-init.exe: $($_.Exception.Message)"
    }

    Write-Note 'running rustup-init.exe -y (default profile)'
    & $installer -y --no-modify-path
    if ($LASTEXITCODE -ne 0) {
        Die "rustup-init.exe exited with code $LASTEXITCODE"
    }
    Remove-Item -LiteralPath $installer -Force -ErrorAction SilentlyContinue

    # Make cargo available in THIS session without a restart.
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path -LiteralPath $cargoBin) {
        $env:PATH = "$cargoBin;$env:PATH"
    }

    if (-not (Test-Command 'cargo')) {
        Die "cargo is still not on PATH after installing rustup. Open a new terminal and re-run, or add '$cargoBin' to PATH."
    }
    Write-Ok 'Rust toolchain installed.'
}

if (-not (Test-Command 'cargo')) {
    # cargo may be installed but just not on PATH yet in this session.
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path -LiteralPath (Join-Path $cargoBin 'cargo.exe')) {
        $env:PATH = "$cargoBin;$env:PATH"
    }
}
if (-not (Test-Command 'cargo')) {
    Install-Rustup
} else {
    Write-Ok "found cargo: $((Get-Command cargo).Source)"
}

# rust-toolchain.toml pins 1.92.0; rustup downloads it on first build. Nudge it
# now so the (large) toolchain fetch is a clear, separate step.
if (Test-Command 'rustup') {
    Write-Info 'Ensuring the pinned toolchain (rust-toolchain.toml -> 1.92.0) is present'
    Push-Location $RepoRoot
    try {
        & rustup show > $null 2>&1
    } finally {
        Pop-Location
    }
}

# --- build ------------------------------------------------------------------

$cargoArgs = @('build', '--release')
foreach ($t in $Targets) {
    $cargoArgs += @('-p', $t[0])
}

$pkgList = ($Targets | ForEach-Object { $_[0] }) -join ', '
Write-Info "Building (release): $pkgList"
Write-Note "cargo $($cargoArgs -join ' ')"

Push-Location $RepoRoot
try {
    & cargo @cargoArgs
    $buildExit = $LASTEXITCODE
} finally {
    Pop-Location
}

if ($buildExit -ne 0) {
    Write-Host ''
    Write-Warn 'cargo build failed.'
    Write-Note 'If the error mentions a missing linker (link.exe), MSVC C++ build'
    Write-Note 'tools, or "Microsoft Visual C++", install the Microsoft C++ build'
    Write-Note 'tools — the Rust MSVC toolchain needs them to link:'
    Write-Note '    https://visualstudio.microsoft.com/visual-cpp-build-tools/'
    Write-Note '  In the installer, select the "Desktop development with C++" workload'
    Write-Note '  (it provides link.exe and the Windows SDK), then re-run this script.'
    Die "build failed (exit $buildExit)"
}
Write-Ok 'build succeeded'

# --- install ----------------------------------------------------------------

if (-not (Test-Path -LiteralPath $BinDir)) {
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
}

$ReleaseDir = Join-Path $RepoRoot 'target\release'
$installed = @()
foreach ($t in $Targets) {
    $src = Join-Path $ReleaseDir $t[1]
    if (-not (Test-Path -LiteralPath $src)) {
        Die "built binary not found: $src (expected after a successful build)"
    }
    $dst = Join-Path $BinDir $t[1]
    Copy-Item -LiteralPath $src -Destination $dst -Force
    Write-Ok "installed $dst"
    $installed += $dst
}

# Server-side launch helpers (POSIX shell scripts, not built artifacts). Copy
# them on PATH next to the server so the host skill's `aura-call` /
# `aura-call-status` commands resolve. They require a POSIX shell (git-bash /
# WSL / MSYS) to run on Windows.
if ($BuildServer) {
    foreach ($h in @(@('launch-call.sh', 'aura-call'), @('call-status.sh', 'aura-call-status'), @('inbox.sh', 'aura-inbox'))) {
        $hsrc = Join-Path $RepoRoot (Join-Path 'scripts' $h[0])
        $hdst = Join-Path $BinDir $h[1]
        Copy-Item -LiteralPath $hsrc -Destination $hdst -Force
        Write-Ok "installed $hdst (host call helper)"
        $installed += $hdst
    }
}

# --- PATH: persist BinDir into the USER environment if absent ---------------

function Test-OnUserPath {
    param([string]$Dir)
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    if ([string]::IsNullOrEmpty($userPath)) { return $false }
    $normTarget = $Dir.TrimEnd('\')
    foreach ($p in $userPath.Split(';')) {
        if ([string]::IsNullOrWhiteSpace($p)) { continue }
        if ($p.TrimEnd('\') -ieq $normTarget) { return $true }
    }
    return $false
}

$pathChanged = $false
if (Test-OnUserPath $BinDir) {
    Write-Note "'$BinDir' is already on the user PATH."
} else {
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    if ([string]::IsNullOrEmpty($userPath)) {
        $newPath = $BinDir
    } else {
        $newPath = ($userPath.TrimEnd(';')) + ';' + $BinDir
    }
    [Environment]::SetEnvironmentVariable('PATH', $newPath, 'User')
    # Also make it usable in the CURRENT session.
    $env:PATH = "$env:PATH;$BinDir"
    $pathChanged = $true
    Write-Ok "added '$BinDir' to the user PATH (persisted, User scope)"
}

# --- final summary ----------------------------------------------------------

Write-Host ''
Write-Info 'Done.'
foreach ($exe in $installed) {
    Write-Host "    $exe"
}
Write-Host ''

if ($pathChanged) {
    Write-Note 'PATH was updated. Open a NEW terminal (or restart your shell) so'
    Write-Note 'the change takes effect; already-open terminals will not see it.'
    Write-Host ''
}

if ($BuildClient) {
    Write-Note 'aura-cli is the CLIENT. It reads the connection string from the'
    Write-Note 'AURA_CONNECT environment variable (never from the command line), or'
    Write-Note 'from one line on stdin. To join a call:'
    Write-Note '    $env:AURA_CONNECT = ''aura://HOST:PORT#k=...&c=...''; aura-cli'
    Write-Note '  or run `aura-cli` and paste the connection string when prompted.'
}
if ($BuildServer) {
    Write-Host 'NEXT — do not stop here.' -ForegroundColor Cyan
    Write-Note 'The binaries + call helpers are installed; the server is NOT yet set'
    Write-Note 'up. Continue with docs/ONBOARDING.md in this same session:'
    Write-Note '    * store the xAI key (BYOK)                  - onboarding step 3'
    Write-Note '    * (REMOTE only) open the one UDP port, once - onboarding step 4'
    Write-Note '    * drop the host skill (skills/SKILL.md)     - onboarding step 5'
    Write-Note 'The aura-call / aura-call-status / aura-inbox helpers are on your PATH'
    Write-Note '(POSIX shell; run the host under git-bash/WSL on Windows). The host launches'
    Write-Note 'aura-server per call; it needs XAI_API_KEY or OPENAI_API_KEY in its'
    Write-Note 'environment (BYOK; never on the command line). It prints the'
    Write-Note '`AURA_CONNECT=... aura-cli` line to STDERR for the caller to use.'
    Write-Note '    https://raw.githubusercontent.com/RealWagmi/aura/main/docs/ONBOARDING.md'
    if ($IsUpdate) {
        Write-Host ''
        Write-Host 'UPDATE detected - refresh the host skill.' -ForegroundColor Yellow
        Write-Note '*** AI AGENT: the skill may have changed in this release, and the copy in'
        Write-Note 'YOUR skills directory is NOT updated automatically. Re-copy it NOW: ***'
        Write-Note "    Copy-Item '$RepoRoot\skills\SKILL.md' `"$env:USERPROFILE\.claude\skills\voice-call\SKILL.md`""
        Write-Note '(substitute your framework''s skills dir if not Claude Code), then re-read it'
        Write-Note 'before the next call. The onboarding config (key, port) needs NO redo.'
    }
}
