#!/usr/bin/env bash
set -euo pipefail

# aura-open-port.sh — open the aura server's UDP port on a REMOTE (VPS) host.
#
# This is a ONE-TIME onboarding step, NOT a per-call action. A REMOTE aura-server
# listens on a single fixed UDP port (default 47821); the thin client (aura-cli)
# dials that port over the Noise/UDP tunnel. Open it once and leave it open.
#
# Usage:
#   scripts/aura-open-port.sh [PORT]
#
# Behaviour:
#   * Detects the ACTIVE firewall front-end, in priority order:
#         ufw -> firewalld -> nftables -> iptables
#   * Opens UDP PORT idempotently.
#   * If run as root (EUID 0): performs the change and prints that it opened the
#     port (so the AI can relay that to the user).
#   * If NOT root: prints the EXACT sudo commands for the user to run, then
#     exits 0 (this is the documented no-root path; it is not a failure).
#   * ALWAYS prints a final caveat that a cloud provider security group / NAT
#     (a filter OUTSIDE this VM) must ALSO allow UDP PORT and cannot be opened
#     from inside the machine.

readonly DEFAULT_PORT=47821

usage() {
    cat <<'EOF'
Usage: aura-open-port.sh [PORT]

Open the aura server's UDP port (default 47821) on a REMOTE/VPS host.
This is a one-time onboarding step, not a per-call action.

Arguments:
  PORT        UDP port to open (default 47821).

Options:
  -h, --help  Show this help and exit.
EOF
}

# ---- parse arguments --------------------------------------------------------
PORT="${DEFAULT_PORT}"
case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
    "")
        ;;
    *)
        PORT="$1"
        ;;
esac

# Validate the port: integer in 1..65535.
case "${PORT}" in
    ''|*[!0-9]*)
        echo "error: PORT must be a positive integer, got: ${PORT}" >&2
        exit 1
        ;;
esac
if [ "${PORT}" -lt 1 ] || [ "${PORT}" -gt 65535 ]; then
    echo "error: PORT must be in the range 1-65535, got: ${PORT}" >&2
    exit 1
fi

is_root() {
    [ "$(id -u)" -eq 0 ]
}

# have CMD — true if CMD is on PATH.
have() {
    command -v "$1" >/dev/null 2>&1
}

# Print the trailing caveat that applies regardless of which front-end ran or
# whether we had root. The cloud security group / NAT is outside this VM.
print_cloud_caveat() {
    cat <<EOF

NOTE: Opening the firewall INSIDE this machine is only half the story. For a
client to reach this server from another machine, the port must also be reachable
from OUTSIDE this machine:

  - Cloud host (AWS / GCP / Azure / Hetzner / DigitalOcean / ...): add an inbound
    UDP ${PORT} rule in the provider's SECURITY GROUP / firewall / network ACL.
    That filter is outside the VM and cannot be changed from in here.

  - Home / office router (this PC on a LAN behind NAT): the OS firewall alone is
    NOT enough. On the router, add a PORT-FORWARDING rule: WAN UDP ${PORT} -> this
    PC's LAN IP, port ${PORT}. Then dial the router's PUBLIC (WAN) IP as
    AURA_PUBLIC_HOST, not this PC's private LAN address; if the WAN IP changes, use
    dynamic DNS.
    CGNAT: if your ISP gives no real public IP (carrier-grade NAT), port-forwarding
    is impossible and inbound calls cannot reach you. Check by comparing the
    router's WAN IP with the output of:  curl -fsS ifconfig.me
    If they differ (or the WAN IP is itself private, e.g. 100.64.x / 10.x /
    192.168.x), you are behind CGNAT and the direct transport cannot reach you.
    Use the iroh transport (AURA_TRANSPORT=iroh; it hole-punches with a blind-relay
    fallback and needs no open port), or host the server on a VPS, or put both ends
    on a VPN/overlay (WireGuard / Tailscale), or use LOCAL calls only.
EOF
}

# ---- ufw --------------------------------------------------------------------
# Active only when `ufw status` reports "Status: active".
ufw_is_active() {
    have ufw || return 1
    ufw status 2>/dev/null | head -n 1 | grep -qi 'Status: active'
}

ufw_open_root() {
    # `ufw allow` is idempotent: re-adding an existing rule is a no-op ("Skipping").
    ufw allow "${PORT}/udp"
    echo "Opened UDP ${PORT} via ufw on this machine."
}

ufw_instructions() {
    cat <<EOF
Detected ufw (active). To open UDP ${PORT}, run as root:

    sudo ufw allow ${PORT}/udp

(Re-running is safe; ufw skips a rule that already exists.)
EOF
}

# ---- firewalld --------------------------------------------------------------
# Active when firewall-cmd exists and the daemon is running.
firewalld_is_active() {
    have firewall-cmd || return 1
    firewall-cmd --state >/dev/null 2>&1
}

firewalld_open_root() {
    # --add-port is idempotent (already-present returns ALREADY_ENABLED, which
    # --permanent treats as success). Apply to both runtime and permanent.
    firewall-cmd --permanent --add-port="${PORT}/udp" >/dev/null
    firewall-cmd --add-port="${PORT}/udp" >/dev/null 2>&1 || true
    firewall-cmd --reload >/dev/null
    echo "Opened UDP ${PORT} via firewalld on this machine (runtime + permanent)."
}

firewalld_instructions() {
    cat <<EOF
Detected firewalld (running). To open UDP ${PORT}, run as root:

    sudo firewall-cmd --permanent --add-port=${PORT}/udp
    sudo firewall-cmd --reload

(Re-running is safe; an already-present port is left as-is.)
EOF
}

# ---- nftables ---------------------------------------------------------------
# Used when `nft` is present (and ufw/firewalld did not claim the host).
nft_is_present() {
    have nft
}

nft_open_root() {
    # Ensure an inet table + an input chain exist, then add the accept rule only
    # if it is not already present (so re-running stays idempotent).
    nft add table inet aura 2>/dev/null || true
    nft add chain inet aura input '{ type filter hook input priority 0 ; }' 2>/dev/null || true
    if nft list chain inet aura input 2>/dev/null | grep -q "udp dport ${PORT} accept"; then
        echo "UDP ${PORT} is already allowed via nftables on this machine (no change)."
    else
        nft add rule inet aura input udp dport "${PORT}" accept
        echo "Opened UDP ${PORT} via nftables on this machine."
    fi
}

nft_instructions() {
    cat <<EOF
Detected nftables (nft). To open UDP ${PORT}, run as root:

    sudo nft add table inet aura
    sudo nft add chain inet aura input '{ type filter hook input priority 0 ; }'
    sudo nft add rule inet aura input udp dport ${PORT} accept

(The table/chain commands are safe to repeat; add the rule only once.)
EOF
}

# ---- iptables (fallback) ----------------------------------------------------
iptables_is_present() {
    have iptables
}

iptables_open_root() {
    # -C checks for the rule; if absent (-C fails) we -I insert it. Idempotent.
    if iptables -C INPUT -p udp --dport "${PORT}" -j ACCEPT 2>/dev/null; then
        echo "UDP ${PORT} is already allowed via iptables on this machine (no change)."
    else
        iptables -I INPUT -p udp --dport "${PORT}" -j ACCEPT
        echo "Opened UDP ${PORT} via iptables on this machine."
        echo "NOTE: iptables rules are not persistent across reboot by default."
        echo "      Persist them with your distro's tool (e.g. iptables-save /"
        echo "      netfilter-persistent save) so the port stays open."
    fi

    # Mirror the rule for IPv6 if ip6tables is available (the client may dial
    # over IPv6). Same -C/-I idempotent logic, same non-persistence caveat.
    if have ip6tables; then
        if ip6tables -C INPUT -p udp --dport "${PORT}" -j ACCEPT 2>/dev/null; then
            echo "UDP ${PORT} is already allowed via ip6tables on this machine (no change)."
        else
            ip6tables -I INPUT -p udp --dport "${PORT}" -j ACCEPT
            echo "Opened UDP ${PORT} via ip6tables on this machine."
            echo "NOTE: ip6tables rules are not persistent across reboot by default."
            echo "      Persist them with your distro's tool (e.g. ip6tables-save /"
            echo "      netfilter-persistent save) so the port stays open."
        fi
    fi
}

iptables_instructions() {
    cat <<EOF
Detected iptables. To open UDP ${PORT}, run as root:

    sudo iptables -I INPUT -p udp --dport ${PORT} -j ACCEPT
    sudo ip6tables -I INPUT -p udp --dport ${PORT} -j ACCEPT

(The ip6tables line applies if the client may dial over IPv6.)

Then persist it across reboots with your distro's tool, e.g.:

    sudo netfilter-persistent save      # Debian/Ubuntu (iptables-persistent)
    sudo service iptables save          # RHEL/CentOS

(Check first with: sudo iptables -C INPUT -p udp --dport ${PORT} -j ACCEPT)
EOF
}

# ---- front-end selection ----------------------------------------------------
# Decide once which front-end owns this host, then either act (root) or print
# the matching instructions (non-root).
select_frontend() {
    if ufw_is_active; then
        echo "ufw"
    elif firewalld_is_active; then
        echo "firewalld"
    elif nft_is_present; then
        echo "nftables"
    elif iptables_is_present; then
        echo "iptables"
    else
        echo "none"
    fi
}

main() {
    local frontend
    frontend="$(select_frontend)"

    if [ "${frontend}" = "none" ]; then
        echo "No supported firewall front-end (ufw/firewalld/nftables/iptables) was found." >&2
        echo "If this host has no host firewall, UDP ${PORT} may already be reachable." >&2
        echo "Otherwise, open inbound UDP ${PORT} using whatever tool your system uses." >&2
        print_cloud_caveat
        # No front-end to act on is not an error of this script; the cloud
        # caveat still matters, so exit 0 with guidance.
        exit 0
    fi

    echo "Active firewall front-end: ${frontend}"

    if is_root; then
        case "${frontend}" in
            ufw)       ufw_open_root ;;
            firewalld) firewalld_open_root ;;
            nftables)  nft_open_root ;;
            iptables)  iptables_open_root ;;
        esac
        print_cloud_caveat
        exit 0
    fi

    # Not root: print exact sudo commands and exit 0 (documented no-root path).
    echo "Not running as root — no change was made. Run the commands below to open the port:"
    echo
    case "${frontend}" in
        ufw)       ufw_instructions ;;
        firewalld) firewalld_instructions ;;
        nftables)  nft_instructions ;;
        iptables)  iptables_instructions ;;
    esac
    # `ufw_is_active` reads `ufw status`, which needs root, so a real ufw box can
    # be misdetected as nftables/iptables here. If ufw is installed at all, also
    # offer its idiomatic command (idempotent; non-fatal) when it was not the
    # selected front-end.
    if [ "${frontend}" != "ufw" ] && have ufw; then
        echo
        echo "If ufw is your active firewall, the idiomatic command is instead:"
        echo
        echo "    sudo ufw allow ${PORT}/udp"
    fi
    print_cloud_caveat
    exit 0
}

main
