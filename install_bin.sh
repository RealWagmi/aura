#!/usr/bin/env bash
#
# install_bin.sh — prebuilt-binary installer for aura (Linux + macOS).
#
# This is the BINARY alternative to install.sh (which builds from source). It
# downloads a prebuilt archive for your OS+arch from the RealWagmi/aura GitHub
# Releases, verifies its SHA-256, and installs — with NO Rust toolchain and NO
# compile:
#   * aura-cli    — the thin client (mic/speaker).
#   * aura-server — the server the host/AI launches.
#   * aura-call / aura-call-status / aura-open-port — the on-PATH host helpers.
#   * SKILL.md    — the universal host skill (to ~/.local/share/aura/).
#
# Windows is NOT covered here — build it from source with install.ps1. If no
# prebuilt archive matches your platform, this script points you at install.sh.
#
# Usage:
#   ./install_bin.sh                 # install BOTH client + server (default)
#   ./install_bin.sh --client        # only aura-cli (the thin client)
#   ./install_bin.sh --server        # aura-server + helpers + the host skill
#   ./install_bin.sh --prefix DIR    # install under DIR/bin (default ~/.local)
#   ./install_bin.sh --uninstall     # remove what this script installed
#   ./install_bin.sh -h | --help
#
# Env:
#   AURA_VERSION   release tag to install (default: latest), e.g. v0.1.0
#
set -euo pipefail

REPO="RealWagmi/aura"
DEFAULT_PREFIX="$HOME/.local"
SHARE_DIR="$HOME/.local/share/aura"
ONBOARDING_DOC="docs/ONBOARDING_BIN.md"
ONBOARDING_URL="https://raw.githubusercontent.com/RealWagmi/aura/main/docs/ONBOARDING_BIN.md"
SOURCE_INSTALL_URL="https://raw.githubusercontent.com/RealWagmi/aura/main/install.sh"

# ANSI styling, only when stdout is a terminal.
if [ -t 1 ]; then
  C_BOLD="$(printf '\033[1m')"; C_DIM="$(printf '\033[2m')"; C_RED="$(printf '\033[31m')"
  C_GREEN="$(printf '\033[32m')"; C_YELLOW="$(printf '\033[33m')"; C_RESET="$(printf '\033[0m')"
else
  C_BOLD=""; C_DIM=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_RESET=""
fi
info()  { printf '%s==>%s %s\n' "$C_BOLD" "$C_RESET" "$*"; }
note()  { printf '%s - %s%s\n' "$C_DIM" "$*" "$C_RESET"; }
ok()    { printf '%s  ok%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn()  { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
err()   { printf '%serror%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; }
die()   { err "$*"; exit 1; }

# --------------------------------------------------------------------------
# Argument parsing
# --------------------------------------------------------------------------

usage() {
  cat <<EOF
${C_BOLD}aura binary installer${C_RESET} — downloads prebuilt aura binaries (Linux/macOS).

Usage: ./install_bin.sh [options]

Selection (default = both):
  --client          Install only aura-cli (the thin client; needs a mic).
  --server          Install aura-server + the host helpers + the skill.

Options:
  --prefix DIR      Install into DIR/bin (default: ${DEFAULT_PREFIX}/bin).
  --uninstall       Remove what this script installed, then exit.
  -h, --help        Show this help and exit.

Env:
  AURA_VERSION      Release tag to install (default: latest), e.g. v0.1.0

If no prebuilt archive matches your OS/arch, this prints the source installer
command (install.sh) so you can build from source instead. Windows is source
only (use install.ps1).
EOF
}

WANT_CLIENT=0
WANT_SERVER=0
EXPLICIT_SELECTION=0
DO_UNINSTALL=0
PREFIX="$DEFAULT_PREFIX"
ORIG_FLAGS=""   # forwarded to install.sh in the fallback message

while [ $# -gt 0 ]; do
  case "$1" in
    --client)    WANT_CLIENT=1; EXPLICIT_SELECTION=1; ORIG_FLAGS="--client" ;;
    --server)    WANT_SERVER=1; EXPLICIT_SELECTION=1; ORIG_FLAGS="--server" ;;
    --uninstall) DO_UNINSTALL=1 ;;
    --prefix)    [ $# -ge 2 ] || die "--prefix requires a directory argument"; PREFIX="$2"; shift ;;
    --prefix=*)  PREFIX="${1#--prefix=}" ;;
    -h|--help)   usage; exit 0 ;;
    *)           die "unknown argument: $1 (try --help)" ;;
  esac
  shift
done

if [ "$EXPLICIT_SELECTION" -eq 0 ]; then WANT_CLIENT=1; WANT_SERVER=1; fi

case "$PREFIX" in
  "~")   PREFIX="$HOME" ;;
  "~/"*) PREFIX="$HOME/${PREFIX#"~/"}" ;;
esac
BIN_DIR="$PREFIX/bin"

# --------------------------------------------------------------------------
# OS + arch -> release target triple
# --------------------------------------------------------------------------

detect_triple() {
  local os arch
  case "$(uname -s)" in
    Linux)  os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *)      TRIPLE=""; return 0 ;;   # Windows / other -> source only
  esac
  case "$(uname -m)" in
    x86_64|amd64)   arch="x86_64" ;;
    aarch64|arm64)  arch="aarch64" ;;
    *)              TRIPLE=""; return 0 ;;
  esac
  TRIPLE="${arch}-${os}"
}

# Pointer to the source installer (we keep the two paths separate; we never run
# install.sh automatically — we tell the user how to).
source_fallback() {
  warn "no prebuilt binary is available for this platform${TRIPLE:+ ($TRIPLE)}."
  note "Build from source instead (needs a Rust toolchain; install.sh sets it up):"
  note "    curl -fsSL ${SOURCE_INSTALL_URL} | bash -s -- ${ORIG_FLAGS:-}"
  note "  (Windows: use install.ps1.)"
  exit 1
}

# --------------------------------------------------------------------------
# Uninstall
# --------------------------------------------------------------------------

ITEMS_CLIENT="aura-cli"
ITEMS_SERVER="aura-server aura-call aura-call-status aura-open-port"

if [ "$DO_UNINSTALL" -eq 1 ]; then
  info "Uninstalling aura from $BIN_DIR"
  removed=0
  set +u
  for name in \
    $([ "$WANT_CLIENT" -eq 1 ] && echo "$ITEMS_CLIENT") \
    $([ "$WANT_SERVER" -eq 1 ] && echo "$ITEMS_SERVER"); do
    if [ -e "$BIN_DIR/$name" ]; then rm -f "$BIN_DIR/$name"; ok "removed $BIN_DIR/$name"; removed=1; fi
  done
  set -u
  if [ "$WANT_SERVER" -eq 1 ] && [ -e "$SHARE_DIR/SKILL.md" ]; then
    rm -f "$SHARE_DIR/SKILL.md"; rmdir "$SHARE_DIR" 2>/dev/null || true; ok "removed $SHARE_DIR/SKILL.md"
  fi
  [ "$removed" -eq 0 ] && note "nothing to remove."
  note "PATH was not modified."
  exit 0
fi

# --------------------------------------------------------------------------
# Tooling checks
# --------------------------------------------------------------------------

command -v curl >/dev/null 2>&1 || die "curl is required to download the release."
command -v tar  >/dev/null 2>&1 || die "tar is required to unpack the release."
if command -v sha256sum >/dev/null 2>&1; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "need sha256sum or shasum to verify the download."
fi

# --------------------------------------------------------------------------
# Download + verify + extract
# --------------------------------------------------------------------------

release_base() {
  if [ -n "${AURA_VERSION:-}" ]; then
    printf 'https://github.com/%s/releases/download/%s' "$REPO" "$AURA_VERSION"
  else
    printf 'https://github.com/%s/releases/latest/download' "$REPO"
  fi
}

download_and_verify() {
  local url="$(release_base)/aura-${TRIPLE}.tar.gz"
  TMP="$(mktemp -d "${TMPDIR:-/tmp}/aura-bin.XXXXXX")"
  trap 'rm -rf "$TMP"' EXIT
  info "Downloading aura-${TRIPLE}${AURA_VERSION:+ @ $AURA_VERSION}"
  note "$url"
  curl -fsSL "$url"          -o "$TMP/a.tgz"    || source_fallback
  curl -fsSL "$url.sha256"   -o "$TMP/a.sha256" || die "release is missing a .sha256 checksum for aura-${TRIPLE}."
  local actual expected
  actual="$(sha256_of "$TMP/a.tgz")"
  expected="$(tr -d '[:space:]' < "$TMP/a.sha256")"
  [ -n "$expected" ] || die "empty checksum file."
  if [ "$actual" != "$expected" ]; then
    die "SHA-256 mismatch — refusing to install.
  expected: $expected
  actual:   $actual"
  fi
  ok "checksum verified (sha256 $actual)"
  mkdir -p "$TMP/x"
  tar -xzf "$TMP/a.tgz" -C "$TMP/x" || die "failed to unpack the archive."
}

# --------------------------------------------------------------------------
# Install
# --------------------------------------------------------------------------

install_file() {  # install_file <name> [+x]
  local name="$1"
  [ -f "$TMP/x/$name" ] || die "archive is missing '$name' (corrupt release?)."
  install -d "$BIN_DIR" 2>/dev/null || mkdir -p "$BIN_DIR"
  cp -f "$TMP/x/$name" "$BIN_DIR/$name"
  chmod 0755 "$BIN_DIR/$name"
  ok "installed $BIN_DIR/$name"
}

# PATH wiring (append to shell rc only if missing) — same policy as install.sh.
ensure_on_path() {
  case ":$PATH:" in *":$BIN_DIR:"*) ok "'$BIN_DIR' is already on your PATH."; return 0 ;; esac
  local rc marker="# added by the aura installer" line="export PATH=\"$BIN_DIR:\$PATH\""
  case "${SHELL:-}" in *zsh) rc="$HOME/.zshrc" ;; *bash) rc="$HOME/.bashrc" ;; *) rc="$HOME/.profile" ;; esac
  if [ -f "$rc" ] && grep -Fq "$BIN_DIR" "$rc" 2>/dev/null; then :; else
    printf '\n%s\n%s\n' "$marker" "$line" >> "$rc" 2>/dev/null || warn "could not update $rc"
  fi
  warn "'$BIN_DIR' was not on your PATH — added it to $rc"
  note "Open a new terminal, or run:  export PATH=\"$BIN_DIR:\$PATH\""
}

onboarding_banner() {
  cat <<EOF
${C_BOLD}================================================================================${C_RESET}
${C_BOLD}${C_RED}  *** AI AGENT: BINARIES ARE INSTALLED, BUT SETUP IS NOT DONE. ***${C_RESET}
${C_BOLD}${C_RED}  *** MANDATORY: READ ${ONBOARDING_DOC} IN FULL, THEN FOLLOW EVERY STEP. ***${C_RESET}
${C_BOLD}================================================================================${C_RESET}
  The binary onboarding is written for YOU (the AI): the xAI key, the one-time
  firewall, dropping the host skill, and a final self-check. Execute it IN ORDER.
      ${ONBOARDING_URL}
${C_BOLD}================================================================================${C_RESET}
EOF
  echo
}

main() {
  [ "$WANT_SERVER" -eq 1 ] && onboarding_banner

  info "aura binary installer — prefix: $PREFIX, selecting:$([ "$WANT_CLIENT" -eq 1 ] && printf ' aura-cli')$([ "$WANT_SERVER" -eq 1 ] && printf ' aura-server')"

  detect_triple
  [ -n "$TRIPLE" ] || source_fallback

  download_and_verify

  [ "$WANT_CLIENT" -eq 1 ] && install_file aura-cli
  if [ "$WANT_SERVER" -eq 1 ]; then
    install_file aura-server
    install_file aura-call
    install_file aura-call-status
    install_file aura-open-port
    mkdir -p "$SHARE_DIR"
    cp -f "$TMP/x/SKILL.md" "$SHARE_DIR/SKILL.md"
    ok "installed $SHARE_DIR/SKILL.md (the host skill)"
  fi

  ensure_on_path

  echo
  info "Done. Installed into $BIN_DIR."
  echo
  if [ "$WANT_CLIENT" -eq 1 ]; then
    cat <<EOF
${C_BOLD}Placing a call (aura-cli, the client):${C_RESET}
  When the AI hands you a connection string, run it via the environment (never
  on the command line):
      AURA_CONNECT='aura://HOST:PORT#k=...&c=...' aura-cli
EOF
    echo
  fi
  if [ "$WANT_SERVER" -eq 1 ]; then
    cat <<EOF
${C_BOLD}NEXT — do not stop here.${C_RESET} The binaries + helpers are installed; the server
  is NOT yet set up. Continue with ${ONBOARDING_DOC} ${C_BOLD}in this same session${C_RESET}:
      * store the xAI key (BYOK)
      * (REMOTE only) open the one UDP port, once (run: aura-open-port)
      * drop the host skill: cp $SHARE_DIR/SKILL.md into your skills dir
      * run the self-check
  The aura-call / aura-call-status helpers are on your PATH.
      ${ONBOARDING_URL}
EOF
    echo
  fi
  ok "all set."
}

main
