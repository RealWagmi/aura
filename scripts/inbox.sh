#!/usr/bin/env bash
# aura-inbox - the live host session's side of the Scheme 2 in-call dispatch
# inbox. During a call, the SAME chat session that launched the call acts as an
# orchestrator: it loops on `aura-inbox wait`, and for each spoken task it either
# answers directly or delegates to a sub-agent, then reports the result with
# `aura-inbox done`. The running call server (aura-server) posts tasks to and
# reads results from the SAME `.aura/inbox/` — rooted at AURA_STATE_DIR when
# set (persist it in the aura .env so both sides converge regardless of cwd;
# essential on hosts whose exec tool starts every command in a fresh cwd),
# else the current directory.
#
# This is a thin wrapper over `aura-server inbox …`; `install.sh` installs it on
# PATH as `aura-inbox` (together with the server). The host skill calls it.
#
# Usage (run from the project dir the call server was launched in):
#   aura-inbox wait [--timeout SECS]   block until a task is pending (claiming
#                                      it); prints the task, or NO_TASK on timeout
#                                      (default 30s). Refreshes the liveness
#                                      heartbeat so the call server routes to you.
#   aura-inbox done <id> <speech...>   report a task finished; <speech> is spoken
#                                      back into the call.
#   aura-inbox stall <id> <speech...>  report a task abandoned; the call server
#                                      then dispatches it directly instead.
#   aura-inbox alive                   refresh the heartbeat once (arm the
#                                      orchestrator at call start).
#
# Env overrides honoured:
#   AURA_SERVER_BIN   explicit path to aura-server (else PATH, else ~/.local/bin)
#
# Portable to Linux and macOS (bash 3.2).

set -euo pipefail

die() { printf 'aura-inbox: %s\n' "$1" >&2; exit 1; }

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

exec "$server_bin" inbox "$@"
