#!/usr/bin/env bash
# aura-call-status — report the current aura call state so the launching host
# can MONITOR a call. Reads .aura/call-status.json (written by aura-server) and
# cross-checks the recorded server pid, so a crash is detected (pid gone while
# the state is still non-terminal) even if the status file is stale.
#
# Usage:
#   aura-call-status           Print the current state once and exit.
#   aura-call-status --wait    Poll every ~10 s until the call is over (ended /
#                              failed / dropped), printing a line each poll, then
#                              exit. This is the "monitor the call" step a host
#                              runs after relaying the connection string.
#
# Env: AURA_STATE_DIR   (root for .aura state; default = cwd). Resolved like
#                        the Rust binaries: process env, then ./.env, then the
#                        global aura .env — so the onboarding .env pin works
#                        here too,
#      AURA_STATUS_FILE (default $AURA_STATE_DIR/.aura/call-status.json),
#      AURA_RECAP_FILE  (default $AURA_STATE_DIR/.aura/hooks/aura-last-claude-result.json),
#      AURA_POLL_SECS   (default 10).
#
# Terminal states: ended (clean hang-up — caller Ctrl-C or the model's
# end_voice_session), failed (engine/provider error), dropped (server pid gone
# with no clean end — crash/kill). On a terminal state the call is over and the
# host should read the recap (if any) and tell the user.

set -uo pipefail

# Resolve the state root EXACTLY like the Rust binaries (aura-server /
# aura-inbox): process env first, then ./.env, then the global aura .env
# (AURA_HOME, else XDG_CONFIG_HOME, else ~/.config/aura). Home forms in the
# value ("~", "$HOME") are expanded, mirroring the Rust loader.
env_file_state_dir() {
    [ -f "$1" ] || return 1
    local v
    v=$(sed -n 's/^[[:space:]]*AURA_STATE_DIR[[:space:]]*=[[:space:]]*//p' "$1" | head -n 1)
    v="${v%\"}"; v="${v#\"}"; v="${v%\'}"; v="${v#\'}"
    [ -n "$v" ] || return 1
    printf '%s' "$v"
}
resolve_state_dir() {
    if [ -n "${AURA_STATE_DIR:-}" ]; then printf '%s' "$AURA_STATE_DIR"; return; fi
    local f v
    for f in ./.env "${AURA_HOME:-}/.env" "${XDG_CONFIG_HOME:-$HOME/.config}/aura/.env"; do
        [ "$f" = "/.env" ] && continue
        if v=$(env_file_state_dir "$f"); then printf '%s' "$v"; return; fi
    done
    printf '.'
}
STATE_DIR="$(resolve_state_dir)"
case "$STATE_DIR" in
    "~") STATE_DIR="$HOME" ;;
    "~/"*) STATE_DIR="$HOME/${STATE_DIR#\~/}" ;;
esac
STATE_DIR="${STATE_DIR//\$\{HOME\}/$HOME}"
STATE_DIR="${STATE_DIR//\$HOME/$HOME}"
STATUS_FILE="${AURA_STATUS_FILE:-$STATE_DIR/.aura/call-status.json}"
RECAP_FILE="${AURA_RECAP_FILE:-$STATE_DIR/.aura/hooks/aura-last-claude-result.json}"
POLL_SECS="${AURA_POLL_SECS:-10}"

TERMINAL=0

field() {
    sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"?([^\",}]*)\"?.*/\1/p" "$STATUS_FILE" 2>/dev/null | head -n 1
}

report_once() {
    TERMINAL=0
    if [ ! -f "$STATUS_FILE" ]; then
        echo "state=none (no call has started yet)"
        return
    fi
    local state pid call_id reason
    state=$(field state)
    pid=$(field pid)
    call_id=$(field call_id)
    reason=$(field reason)
    case "$state" in
        ended | failed)
            echo "state=$state call=${call_id:-?} reason=${reason:-?}"
            if [ -f "$RECAP_FILE" ]; then echo "recap=$RECAP_FILE"; else echo "recap=(none)"; fi
            TERMINAL=1
            ;;
        ringing | active)
            if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
                echo "state=$state call=${call_id:-?} (server pid $pid alive — call in progress)"
            else
                echo "state=dropped call=${call_id:-?} (server pid ${pid:-?} gone — crashed/killed, no clean end)"
                TERMINAL=1
            fi
            ;;
        *)
            echo "state=${state:-unknown} call=${call_id:-?}"
            ;;
    esac
}

if [ "${1:-}" = "--wait" ]; then
    echo "aura-call-status: monitoring every ${POLL_SECS}s until the call ends…" >&2
    while :; do
        report_once
        [ "$TERMINAL" -eq 1 ] && exit 0
        sleep "$POLL_SECS"
    done
else
    report_once
fi
