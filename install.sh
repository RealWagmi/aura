#!/usr/bin/env bash
#
# install.sh  - source-build installer for aura (Linux + macOS).
#
# aura = voice calls for AI chats. All-Rust. Two binaries:
#   * aura-cli    - the THIN CLIENT. Runs on YOUR machine (mic/speaker via
#                   cpal). Holds no key, no engine, no host. Reads the call
#                   connection string from $AURA_CONNECT (or one line on
#                   stdin)  - never from argv.
#   * aura-server  - the server the host/AI launches (127.0.0.1 for a LOCAL
#                   call, a VPS for a REMOTE call). Holds XAI_API_KEY + the
#                   engine + the chat context + the tools. Needs no audio
#                   package on any OS.
#
# This script builds the selected binaries from source with cargo and
# installs them to <prefix>/bin (default ~/.local/bin). It uses NO sudo: if
# a system package is required (Linux ALSA dev headers, client only) it
# prints the exact command for you to run instead of running it.
#
# Usage:
#   ./install.sh                 # build + install BOTH (default)
#   ./install.sh --client        # build + install aura-cli only
#   ./install.sh --server        # build + install aura-server only
#   ./install.sh --prefix DIR    # install under DIR/bin (default ~/.local)
#   ./install.sh --uninstall      # remove the installed binaries
#   ./install.sh -h | --help
#
set -euo pipefail

# --------------------------------------------------------------------------
# Constants and small helpers
# --------------------------------------------------------------------------

# Resolve the repo root = the directory that holds this script.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR"

DEFAULT_PREFIX="$HOME/.local"

# The AI-driven server setup lives in the onboarding guide; this installer only
# builds + installs the binaries. Public repo + raw onboarding URL.
REPO_URL="https://github.com/RealWagmi/aura"
ONBOARDING_DOC="docs/ONBOARDING.md"
ONBOARDING_URL="https://raw.githubusercontent.com/RealWagmi/aura/main/docs/ONBOARDING.md"

# ANSI styling, only when stdout is a terminal.
if [ -t 1 ]; then
  C_BOLD="$(printf '\033[1m')"
  C_DIM="$(printf '\033[2m')"
  C_RED="$(printf '\033[31m')"
  C_GREEN="$(printf '\033[32m')"
  C_YELLOW="$(printf '\033[33m')"
  C_RESET="$(printf '\033[0m')"
else
  C_BOLD=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_RESET=""
fi

info()  { printf '%s==>%s %s\n' "$C_BOLD" "$C_RESET" "$*"; }
note()  { printf '%s - %s%s\n' "$C_DIM" "$*" "$C_RESET"; }
ok()    { printf '%s  ok%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn()  { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
err()   { printf '%serror%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; }
die()   { err "$*"; exit 1; }

# Pointer for an AI agent: the full server setup is the onboarding guide, not
# this installer. Printed before building when the server is selected.
onboarding_banner() {
  cat <<EOF
${C_BOLD}================================================================================${C_RESET}
${C_BOLD}${C_RED}  *** AI AGENT: THIS INSTALLER ONLY BUILDS THE BINARIES. ***${C_RESET}
${C_BOLD}${C_RED}  *** MANDATORY: READ docs/ONBOARDING.md IN FULL, THEN FOLLOW EVERY STEP. ***${C_RESET}
${C_BOLD}================================================================================${C_RESET}
  The onboarding guide is written for YOU (the AI) and DRIVES the whole setup —
  the xAI key, the one-time firewall, installing the host skill, and a final
  self-check. The setup is NOT done when this build finishes. Read the guide top
  to bottom and execute every step IN ORDER; do not improvise, skip, or stop early.

      ${ONBOARDING_DOC}
      ${ONBOARDING_URL}
${C_BOLD}================================================================================${C_RESET}
EOF
  echo
}

# --------------------------------------------------------------------------
# Argument parsing
# --------------------------------------------------------------------------

usage() {
  cat <<EOF
${C_BOLD}aura installer${C_RESET}  - builds aura-cli / aura-server from source and installs them.

Usage: ./install.sh [options]

Selection (default = both):
  --client          Build + install only aura-cli (the thin client; needs a mic).
  --server          Build + install only aura-server (the host/AI side).

Options:
  --prefix DIR      Install into DIR/bin (default: ${DEFAULT_PREFIX}/bin).
  --uninstall       Remove the installed aura-cli / aura-server (honours
                    --client / --server / --prefix), then exit.
  -h, --help        Show this help and exit.

Notes:
  * No sudo is used. If a system package is missing the script prints the
    exact command for you to run.
  * aura-cli on Linux needs the ALSA dev headers (libasound2-dev / alsa-lib-devel
    / alsa-lib). aura-server needs no audio package. macOS needs none either.
  * Outputs land in ${DEFAULT_PREFIX}/bin by default; the script ensures that
    directory is on your PATH (appending to your shell rc if needed).
EOF
}

WANT_CLIENT=0
WANT_SERVER=0
EXPLICIT_SELECTION=0
DO_UNINSTALL=0
PREFIX="$DEFAULT_PREFIX"

while [ $# -gt 0 ]; do
  case "$1" in
    --client)    WANT_CLIENT=1; EXPLICIT_SELECTION=1 ;;
    --server)    WANT_SERVER=1; EXPLICIT_SELECTION=1 ;;
    --uninstall) DO_UNINSTALL=1 ;;
    --prefix)
      [ $# -ge 2 ] || die "--prefix requires a directory argument"
      PREFIX="$2"; shift ;;
    --prefix=*)  PREFIX="${1#--prefix=}" ;;
    -h|--help)   usage; exit 0 ;;
    --)          shift; break ;;
    *)           die "unknown argument: $1 (try --help)" ;;
  esac
  shift
done

# Default = both, when neither --client nor --server was given.
if [ "$EXPLICIT_SELECTION" -eq 0 ]; then
  WANT_CLIENT=1
  WANT_SERVER=1
fi

# Expand a leading ~ in --prefix (a quoted "~/x" is not expanded by the shell).
case "$PREFIX" in
  "~")    PREFIX="$HOME" ;;
  "~/"*)  PREFIX="$HOME/${PREFIX#"~/"}" ;;
esac
BIN_DIR="$PREFIX/bin"

# --------------------------------------------------------------------------
# OS detection
# --------------------------------------------------------------------------

UNAME_S="$(uname -s)"
case "$UNAME_S" in
  Linux)  OS="linux" ;;
  Darwin) OS="macos" ;;
  *)      die "unsupported OS '$UNAME_S'  - install.sh covers Linux and macOS; on Windows use install.ps1" ;;
esac

# --------------------------------------------------------------------------
# Uninstall path
# --------------------------------------------------------------------------

uninstall() {
  info "Uninstalling aura from $BIN_DIR"
  local removed=0 target name
  for name in $1; do
    target="$BIN_DIR/$name"
    if [ -e "$target" ]; then
      rm -f "$target"
      ok "removed $target"
      removed=1
    else
      note "not present: $target"
    fi
  done
  if [ "$removed" -eq 0 ]; then
    note "nothing to remove."
  fi
  echo
  note "PATH was not modified. If you no longer want '$BIN_DIR' on your PATH,"
  note "remove the line that mentions it (look for the 'aura installer' marker)"
  note "from your shell rc (e.g. ~/.bashrc, ~/.zshrc, or ~/.profile)."
}

# Build the list of binary names the user selected.
SELECTED=""
[ "$WANT_CLIENT" -eq 1 ] && SELECTED="$SELECTED aura-cli"
[ "$WANT_SERVER" -eq 1 ] && SELECTED="$SELECTED aura-server"
SELECTED="${SELECTED# }"

if [ "$DO_UNINSTALL" -eq 1 ]; then
  # The server ships on-PATH shell helpers (aura-call / aura-call-status /
  # aura-inbox); remove them too when uninstalling the server.
  REMOVE="$SELECTED"
  [ "$WANT_SERVER" -eq 1 ] && REMOVE="$REMOVE aura-call aura-call-status aura-inbox"
  uninstall "$REMOVE"
  exit 0
fi

# --------------------------------------------------------------------------
# Toolchain: ensure rustup + cargo
# --------------------------------------------------------------------------

ensure_rust() {
  # If cargo is already reachable we are done  - rustup will honour the
  # rust-toolchain.toml pin (1.92.0) automatically inside the repo.
  if command -v cargo >/dev/null 2>&1; then
    ok "cargo found: $(command -v cargo)"
    return 0
  fi

  # cargo might be installed but just not on PATH yet (fresh rustup).
  if [ -f "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
    if command -v cargo >/dev/null 2>&1; then
      ok "cargo found via ~/.cargo/env"
      return 0
    fi
  fi

  info "Rust toolchain (cargo) not found  - installing via rustup."
  command -v curl >/dev/null 2>&1 || die "curl is required to install rustup; please install curl and re-run."

  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    || die "rustup installation failed."

  # shellcheck disable=SC1091
  . "$HOME/.cargo/env" 2>/dev/null || true
  command -v cargo >/dev/null 2>&1 \
    || die "cargo still not on PATH after rustup install; open a new shell and re-run, or run: source \$HOME/.cargo/env"
  ok "rustup installed; cargo is now available."
}

# --------------------------------------------------------------------------
# Linux: ensure the ALSA dev headers for the CLIENT build (no sudo).
# --------------------------------------------------------------------------

# Detect the system package manager and the matching ALSA dev package.
# Sets globals: PKG_MGR, ALSA_PKG, ALSA_INSTALL_CMD.
detect_linux_pkg() {
  if command -v apt-get >/dev/null 2>&1; then
    PKG_MGR="apt"
    ALSA_PKG="libasound2-dev"
    ALSA_INSTALL_CMD="sudo apt-get install -y libasound2-dev"
  elif command -v dnf >/dev/null 2>&1; then
    PKG_MGR="dnf"
    ALSA_PKG="alsa-lib-devel"
    ALSA_INSTALL_CMD="sudo dnf install -y alsa-lib-devel"
  elif command -v pacman >/dev/null 2>&1; then
    PKG_MGR="pacman"
    ALSA_PKG="alsa-lib"
    ALSA_INSTALL_CMD="sudo pacman -S --needed alsa-lib"
  elif command -v zypper >/dev/null 2>&1; then
    PKG_MGR="zypper"
    ALSA_PKG="alsa-lib-devel"
    ALSA_INSTALL_CMD="sudo zypper install -y alsa-lib-devel"
  else
    PKG_MGR=""
    ALSA_PKG=""
    ALSA_INSTALL_CMD=""
  fi
}

# Return 0 if the ALSA development headers appear to be present.
alsa_dev_present() {
  # pkg-config is the most reliable signal when available.
  if command -v pkg-config >/dev/null 2>&1; then
    if pkg-config --exists alsa 2>/dev/null; then
      return 0
    fi
  fi
  # Fall back to the canonical header location.
  if [ -f /usr/include/alsa/asoundlib.h ]; then
    return 0
  fi
  return 1
}

# Ensure the ALSA dev headers exist (client + Linux only). Never runs sudo;
# on a miss it prints the exact command and aborts so the user can fix it.
ensure_alsa_for_client() {
  [ "$OS" = "linux" ]      || return 0
  [ "$WANT_CLIENT" -eq 1 ] || return 0

  if alsa_dev_present; then
    ok "ALSA development headers found (needed by aura-cli / cpal)."
    return 0
  fi

  detect_linux_pkg
  err "aura-cli needs the ALSA development headers, which were not found."
  if [ -n "$ALSA_INSTALL_CMD" ]; then
    cat >&2 <<EOF

  Install them with your package manager ($PKG_MGR), then re-run this script:

      $ALSA_INSTALL_CMD

EOF
  else
    cat >&2 <<EOF

  Your package manager was not recognised. Install the ALSA development
  package for your distribution (it provides <alsa/asoundlib.h>), e.g.:

      Debian/Ubuntu : sudo apt-get install -y libasound2-dev
      Fedora/RHEL   : sudo dnf install -y alsa-lib-devel
      Arch          : sudo pacman -S --needed alsa-lib
      openSUSE      : sudo zypper install -y alsa-lib-devel

  Then re-run this script.

EOF
  fi
  note "(This is required only for the CLIENT. To install just the server now, run: ./install.sh --server)"
  exit 1
}

# --------------------------------------------------------------------------
# Build
# --------------------------------------------------------------------------

build_one() {
  local pkg="$1"
  info "Building $pkg (release)..."
  ( cd "$REPO_ROOT" && cargo build --release -p "$pkg" ) \
    || die "cargo build failed for $pkg."
  local artifact="$REPO_ROOT/target/release/$pkg"
  [ -x "$artifact" ] || die "expected build artifact missing: $artifact"
  ok "built $pkg"
}

install_one() {
  local pkg="$1"
  local artifact="$REPO_ROOT/target/release/$pkg"
  install -d "$BIN_DIR" 2>/dev/null || mkdir -p "$BIN_DIR"
  # Copy then chmod (portable; `install` flags differ on BSD/macOS).
  cp -f "$artifact" "$BIN_DIR/$pkg"
  chmod 0755 "$BIN_DIR/$pkg"
  ok "installed $BIN_DIR/$pkg"
}

# The server-side launch helpers are shell scripts (not built artifacts). Install
# them on PATH next to the server so the host skill's `aura-call` /
# `aura-call-status` / `aura-inbox` commands resolve. A client-only install does
# not need them.
install_helpers() {
  install -d "$BIN_DIR" 2>/dev/null || mkdir -p "$BIN_DIR"
  cp -f "$REPO_ROOT/scripts/launch-call.sh" "$BIN_DIR/aura-call"
  cp -f "$REPO_ROOT/scripts/call-status.sh" "$BIN_DIR/aura-call-status"
  cp -f "$REPO_ROOT/scripts/inbox.sh" "$BIN_DIR/aura-inbox"
  chmod 0755 "$BIN_DIR/aura-call" "$BIN_DIR/aura-call-status" "$BIN_DIR/aura-inbox"
  ok "installed $BIN_DIR/aura-call, $BIN_DIR/aura-call-status and $BIN_DIR/aura-inbox (host call helpers)"
}

# --------------------------------------------------------------------------
# PATH wiring (append to shell rc only if missing).
# --------------------------------------------------------------------------

# Return 0 if BIN_DIR is already on the current PATH.
bindir_on_path() {
  case ":$PATH:" in
    *":$BIN_DIR:"*) return 0 ;;
    *) return 1 ;;
  esac
}

# Populate the named array (passed by name) with the shell rc files to update,
# chosen from the user's login shell. Using an array keeps rc paths that contain
# spaces intact (a space-joined string would word-split them).
rc_files_for_shell() {
  local primary
  local login_shell="${SHELL:-}"
  case "$login_shell" in
    *zsh)  primary="$HOME/.zshrc" ;;
    *bash) primary="$HOME/.bashrc" ;;
    *)     primary="$HOME/.profile" ;;
  esac
  # Always also keep ~/.profile in the loop for login-shell coverage, but
  # avoid duplicating it when it is already the primary target.
  if [ "$primary" = "$HOME/.profile" ]; then
    rc_files=("$primary")
  else
    rc_files=("$primary" "$HOME/.profile")
  fi
}

ensure_on_path() {
  if bindir_on_path; then
    ok "'$BIN_DIR' is already on your PATH."
    return 0
  fi

  local marker="# added by the aura installer"
  local line="export PATH=\"$BIN_DIR:\$PATH\""
  local touched=""
  local rc
  local -a rc_files=()
  rc_files_for_shell

  for rc in "${rc_files[@]}"; do
    # Skip if this rc already references BIN_DIR (idempotent re-runs).
    if [ -f "$rc" ] && grep -Fq "$BIN_DIR" "$rc" 2>/dev/null; then
      continue
    fi
    {
      printf '\n%s\n' "$marker"
      printf '%s\n' "$line"
    } >> "$rc" || die "could not update $rc"
    touched="$touched $rc"
  done

  if [ -n "$touched" ]; then
    warn "'$BIN_DIR' was not on your PATH  - added it to:${touched}"
    note "Open a new terminal, or run:  export PATH=\"$BIN_DIR:\$PATH\""
  else
    warn "'$BIN_DIR' is not on your PATH and your shell rc already mentions it."
    note "Open a new terminal, or run:  export PATH=\"$BIN_DIR:\$PATH\""
  fi
}

# --------------------------------------------------------------------------
# Source bootstrap (when run standalone, e.g. `curl | bash`)
# --------------------------------------------------------------------------

# If we are not inside an aura checkout, clone it and build from there. This is
# what makes the one-line `curl ... | bash` installer self-contained.
ensure_repo() {
  if [ -f "$REPO_ROOT/Cargo.toml" ]; then
    return 0   # already inside an aura checkout
  fi
  command -v git >/dev/null 2>&1 \
    || die "git is required to fetch the aura source. Install git and re-run, or clone manually: git clone $REPO_URL"
  local dest="${AURA_SRC_DIR:-$HOME/aura}"
  if [ -f "$dest/Cargo.toml" ]; then
    info "Using the existing aura checkout at $dest"
    # Re-running the installer must UPDATE, not silently rebuild the old
    # checkout. Pull only when it is a clean git clone (never clobber local
    # edits); on any failure fall back to building the checkout as-is.
    if [ -d "$dest/.git" ]; then
      if [ -z "$(git -C "$dest" status --porcelain 2>/dev/null)" ]; then
        info "Updating the checkout (git pull --ff-only)"
        git -C "$dest" pull --ff-only \
          || warn "git pull failed; building the existing checkout as-is."
      else
        warn "local changes in $dest — skipping git pull; building the checkout as-is."
      fi
    fi
  else
    info "Fetching the aura source into $dest (git clone)"
    git clone --depth 1 "$REPO_URL" "$dest" || die "git clone failed: $REPO_URL"
  fi
  REPO_ROOT="$dest"
  cd "$REPO_ROOT" || die "could not enter $REPO_ROOT"
  ok "aura source ready at $REPO_ROOT"
}

# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------

main() {
  # The server is the AI-driven side: show the MANDATORY onboarding notice FIRST,
  # before anything else, so an AI agent cannot miss it.
  [ "$WANT_SERVER" -eq 1 ] && onboarding_banner

  info "aura installer  - target: $OS, prefix: $PREFIX, selecting:$([ "$WANT_CLIENT" -eq 1 ] && printf ' aura-cli')$([ "$WANT_SERVER" -eq 1 ] && printf ' aura-server')"

  # UPDATE vs fresh install: a previous aura binary in BIN_DIR means this run
  # replaces an existing install (used for the post-install skill notice).
  IS_UPDATE=0
  if [ -e "$BIN_DIR/aura-server" ] || [ -e "$BIN_DIR/aura-cli" ]; then
    IS_UPDATE=1
    info "Existing aura install detected in $BIN_DIR — this run is an UPDATE."
  fi

  # Ensure we have the source: an existing checkout, or clone it (curl | bash).
  ensure_repo

  ensure_rust
  ensure_alsa_for_client

  local pkg
  for pkg in $SELECTED; do
    build_one "$pkg"
  done
  for pkg in $SELECTED; do
    install_one "$pkg"
  done
  # The server ships the on-PATH call helpers (aura-call / aura-call-status / aura-inbox).
  [ "$WANT_SERVER" -eq 1 ] && install_helpers

  ensure_on_path

  # ----- Final summary -----
  echo
  info "Done. Installed into $BIN_DIR:"
  for pkg in $SELECTED; do
    printf '      %s\n' "$BIN_DIR/$pkg"
  done
  if [ "$WANT_SERVER" -eq 1 ]; then
    printf '      %s\n' "$BIN_DIR/aura-call"
    printf '      %s\n' "$BIN_DIR/aura-call-status"
    printf '      %s\n' "$BIN_DIR/aura-inbox"
  fi
  echo

  if [ "$WANT_CLIENT" -eq 1 ]; then
    cat <<EOF
${C_BOLD}Next  - placing a call (aura-cli, the client):${C_RESET}
  When the AI hands you a connection string, run it like this (the secret
  travels in the environment, never on the command line):

      AURA_CONNECT='aura://HOST:PORT#k=...&c=...' aura-cli

  Or just run 'aura-cli' and paste the connection string when prompted.
EOF
    echo
  fi

  if [ "$WANT_SERVER" -eq 1 ]; then
    cat <<EOF
${C_BOLD}NEXT — do not stop here.${C_RESET} The binaries + call helpers are installed; the
  server is NOT yet set up. Continue with ${ONBOARDING_DOC} ${C_BOLD}in this same session${C_RESET}:
      * store the xAI key (BYOK)                         — onboarding step 3
      * (REMOTE only) open the one UDP port, once        — onboarding step 4
      * drop the host skill (skills/SKILL.md) in place   — onboarding step 5
  The aura-call / aura-call-status / aura-inbox helpers are already on your PATH
  (the skill uses them). The host/AI launches aura-server per call; it needs
  XAI_API_KEY in its environment (or the OS keychain, or a ./.env in its working dir).
      ${ONBOARDING_URL}
EOF
    echo
    if [ "$IS_UPDATE" -eq 1 ]; then
      cat <<EOF
${C_BOLD}${C_YELLOW}UPDATE detected — refresh the host skill.${C_RESET}
  *** AI AGENT: the skill may have changed in this release, and the copy in
  YOUR skills directory is NOT updated automatically. Re-copy it NOW: ***
      cp $REPO_ROOT/skills/SKILL.md ~/.claude/skills/aura/SKILL.md
  (substitute your framework's skills dir if not Claude Code), then re-read it
  before the next call. The onboarding config (key, port) needs NO redo.
EOF
      echo
    fi
  fi

  ok "all set."
}

main
