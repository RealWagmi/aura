#!/usr/bin/env bash
# aura-call - launch aura-server for ONE voice call, print the single-use
# connection string on stdout, and leave the server running to accept the call.
#
# This is the one launch helper for every host. `install.sh` installs it on PATH
# as `aura-call` (together with the server); the host skill calls it.
#
# Behaviour:
#   - aura-server prints its connection line on STDERR, then blocks to accept the
#     client and run the whole call. This helper lifts that ONE line onto its own
#     STDOUT and returns immediately, while the server keeps running detached.
#   - The session secret is printed exactly once (inside the AURA_CONNECT=...
#     line) for the caller to relay or run. It is NEVER written to disk and NEVER
#     placed on argv. aura-server reads its BYOK key (XAI_API_KEY or
#     OPENAI_API_KEY) from its own environment (else the OS keychain, else a
#     ./.env file); this script never reads or echoes it.
#
# Usage:
#   aura-call local                          LOCAL loopback call (binds 127.0.0.1)
#   aura-call remote                          REMOTE call; the SERVER auto-picks
#                                            direct (if AURA_PUBLIC_HOST is set in
#                                            the aura .env) or iroh otherwise
#   aura-call remote <PUBLIC_HOST>           REMOTE, forcing direct UDP to this
#                                            reachable host (needs an open port)
# Options:
#   --host <claude|codex|hermes|openclaw>    select the host adapter (default: claude)
#   -h, --help                               show this help
#
# REMOTE transport selection lives in the SERVER (it robustly loads the aura
# .env), so this launcher only signals a remote call (AURA_REMOTE=1) and forwards
# an optional explicit host:
#   - `aura-call remote`                -> the server picks direct if a reachable
#     AURA_PUBLIC_HOST is configured (env or the aura .env), else iroh (P2P
#     hole-punch + blind relay fallback; NAT/CGNAT-safe).
#   - `aura-call remote <PUBLIC_HOST>`  -> exports AURA_PUBLIC_HOST, so the server
#     uses direct Noise/UDP to that host.
#   - AURA_TRANSPORT=iroh|direct in the environment overrides on the server.
#
# Env overrides honoured:
#   AURA_SERVER_BIN   explicit path to aura-server (else PATH, else ~/.local/bin)
#   AURA_PORT         fixed UDP port (default 47821); read by aura-server itself
#   AURA_TRANSPORT    force the REMOTE transport on the server: 'iroh' (NAT/CGNAT
#                     QUIC, hole-punch + blind relay fallback; the connection
#                     string carries the node id instead of host:port) or
#                     'direct' (Noise/UDP; needs a reachable public host). Unset =
#                     the server auto-selects from AURA_PUBLIC_HOST reachability.
#   AURA_CALL_LOG     append the server's MID-CALL stderr (reconnects, dispatch
#                     inbox path, truncate markers, call end) to this file
#                     instead of discarding it — for diagnostics/experiments.
#                     The connection line is consumed before the log starts, so
#                     the session secret never reaches the file.
#   (Voice selection - AURA_VOICE_PROVIDER / AURA_VOICE_MODEL / the BYOK keys -
#   is read by aura-server itself from the environment / .env; prefix this
#   helper to switch per call.)
#
# Portable to Linux and macOS (bash 3.2, BSD tools; no setsid, no GNU timeout).

set -euo pipefail

prog=aura-call

die() {
  printf '%s: %s\n' "$prog" "$1" >&2
  exit 1
}

usage() {
  cat >&2 <<'EOF'
Usage:
  aura-call local                       Start a LOCAL (loopback) call on 127.0.0.1
  aura-call remote                      Start a REMOTE call; the server auto-picks
                                        direct (AURA_PUBLIC_HOST set) or iroh
  aura-call remote <PUBLIC_HOST>        REMOTE, forcing direct UDP to PUBLIC_HOST
                                        (needs an open port)
Options:
  --host <claude|codex|hermes|openclaw> Select the host adapter (default: claude)
  -h, --help                            Show this help and exit
Prints one line on stdout (the single-use connection string, ~120 s validity):
  direct: AURA_CONNECT='aura://HOST:PORT#k=...&c=...' aura-cli
  iroh:   AURA_CONNECT='aura://<node-id>#k=...&c=...&t=iroh' aura-cli
EOF
}

mode=""
public_host=""
host_kind=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --host) shift; [ "$#" -gt 0 ] || die "--host needs a value"; host_kind="$1" ;;
    --host=*) host_kind="${1#--host=}" ;;
    local|remote)
      [ -z "$mode" ] || die "mode given more than once"
      mode="$1" ;;
    -*) die "unknown option: $1" ;;
    *)
      if [ "$mode" = "remote" ] && [ -z "$public_host" ]; then
        public_host="$1"
      else
        die "unexpected argument: $1"
      fi ;;
  esac
  shift
done

[ -n "$mode" ] || { usage; exit 2; }

# Mode -> bind interface. AURA_PUBLIC_HOST is what the client dials AND it selects
# the bind interface inside aura-server (loopback-only for local, all interfaces
# for a real remote host).
case "$mode" in
  local)
    export AURA_PUBLIC_HOST="127.0.0.1" ;;
  remote)
    # Signal a REMOTE call and let the SERVER resolve the transport: direct if a
    # reachable AURA_PUBLIC_HOST is configured (the host arg below, or one
    # persisted in the aura .env), else iroh (NAT/CGNAT-safe P2P). An explicit
    # AURA_TRANSPORT still overrides on the server; reject a typo early here.
    case "${AURA_TRANSPORT:-}" in
      ""|iroh|direct) : ;;
      *) die "AURA_TRANSPORT must be 'iroh' or 'direct', got '${AURA_TRANSPORT}'" ;;
    esac
    export AURA_REMOTE=1
    if [ -n "$public_host" ]; then
      case "$public_host" in
        127.*|localhost|::1|0.0.0.0)
          die "remote mode needs a real public host, not a loopback/wildcard ($public_host)" ;;
      esac
      export AURA_PUBLIC_HOST="$public_host"
    fi ;;
esac

# Host adapter selection (default: Claude). AURA_HOST is the explicit override the
# server's registry honours above every other signal.
if [ -n "$host_kind" ]; then
  case "$host_kind" in
    claude|codex|hermes|openclaw) export AURA_HOST="$host_kind" ;;
    *) die "unknown --host: $host_kind (expected claude|codex|hermes|openclaw)" ;;
  esac
fi

# Resolve the server binary: explicit override, then PATH, then ~/.local/bin.
server_bin="${AURA_SERVER_BIN:-}"
if [ -z "$server_bin" ]; then
  if command -v aura-server >/dev/null 2>&1; then
    server_bin="$(command -v aura-server)"
  elif [ -x "$HOME/.local/bin/aura-server" ]; then
    server_bin="$HOME/.local/bin/aura-server"
  else
    die "aura-server not found on PATH or in ~/.local/bin (build it with ./install.sh --server)"
  fi
fi
[ -x "$server_bin" ] || die "aura-server is not executable: $server_bin"

# The server's stderr (where the connection line is printed) flows through a FIFO.
# We read it until the connection line, then hand the read end to a DETACHED
# drainer that keeps it open for the server's whole life. This is essential:
# aura-server keeps logging to stderr after it prints the connection line (e.g.
# "client connected" at handshake), and Rust's eprintln! PANICS on a broken pipe
# (EPIPE) - so abandoning the read end would kill the call at the moment the
# client connects. The FIFO is unlinked immediately; the secret never hits disk.
fifo_dir="$(mktemp -d "${TMPDIR:-/tmp}/aura-call.XXXXXX")" || die "could not create a temp dir"
fifo="$fifo_dir/stderr"
mkfifo "$fifo" || die "could not create the stderr pipe"

# Launch the server detached (nohup + disown so a SIGHUP on our exit can't reach
# it), stdout discarded, stderr to the FIFO. It survives this helper's exit.
nohup "$server_bin" >/dev/null 2>"$fifo" &
disown 2>/dev/null || true

# Open the read end (this unblocks the server's open-for-write), then unlink the
# path - the open fds keep working and nothing about the call is left on disk.
exec 3<"$fifo"
rm -rf "$fifo_dir" 2>/dev/null || true

emitted=0
while IFS= read -r line; do
  # Trim leading whitespace; aura-server indents the connection line.
  trimmed="${line#"${line%%[![:space:]]*}"}"
  case "$trimmed" in
    "AURA_CONNECT='aura://"*"' aura-cli"*)
      printf '%s\n' "$trimmed"   # the one connection line -> our stdout, once
      emitted=1
      break ;;
    *)
      # Forward the server's startup diagnostics so they stay visible; never the
      # connection line (handled above), never anything we synthesize.
      [ -n "$line" ] && printf '%s\n' "$line" >&2 ;;
  esac
done <&3

if [ "$emitted" -ne 1 ]; then
  # The FIFO reached EOF without a connection line: the server exited early.
  die "aura-server exited before printing a connection string (check the API key (XAI_API_KEY / OPENAI_API_KEY), the host config, and that UDP ${AURA_PORT:-47821} is free)"
fi

# Hand fd 3 to a detached drainer so the live server never sees a closed stderr
# pipe. `cat` reads to EOF (server exit) then exits. The drained text is
# discarded unless AURA_CALL_LOG names a file to append it to (the connection
# line was already consumed above, so no secret can reach the log).
#
# The drainer must NEVER die while the server lives — a dead drainer closes the
# last read end and the server's next eprintln! EPIPE-panics, killing the call.
# So: (1) probe-open the log target and fall back to /dev/null if it cannot be
# opened (a typo'd path must not become a call-killer); (2) if the logging cat
# still fails MID-call (disk full, file removed), a second cat keeps draining
# to /dev/null for the rest of the call.
drain_log="${AURA_CALL_LOG:-/dev/null}"
# (2>/dev/null comes FIRST: redirections apply left to right, and the probe's
# own failure message must be suppressed, not printed.)
if ! : 2>/dev/null >>"$drain_log"; then
  printf '%s: warning: AURA_CALL_LOG=%s is not writable; discarding the server log\n' \
    "$prog" "$drain_log" >&2
  drain_log=/dev/null
fi
# The wrapper's OWN stdout/stderr must be redirected AT SPAWN: it inherits this
# helper's stdout otherwise, and a caller using `conn="$(aura-call ...)"` would
# block forever on the command substitution (the pipe's write end stays open
# for the server's whole life). The inner cat re-redirects to the log itself.
nohup sh -c 'cat >>"$1" 2>/dev/null; exec cat >/dev/null 2>&1' drain "$drain_log" \
  <&3 >/dev/null 2>&1 &
disown 2>/dev/null || true

exit 0
