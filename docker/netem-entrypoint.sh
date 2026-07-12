#!/bin/bash

set -euo pipefail

die() { echo "[netem] ERROR: $1" >&2; exit 1; }

peer_ip() {
    local alias="${P2P_PEER_ALIAS:?P2P_PEER_ALIAS is required}"
    getent ahostsv4 "$alias" | awk 'NR == 1 { print $1 }' \
        || die "could not resolve $alias; is this node attached to btc-simnet-p2p?"
}

p2p_interface() {
    local ip route
    ip="$(peer_ip)" || exit 1
    route="$(ip -o route get "$ip")" || die "could not find a route to P2P peer $ip"
    awk '{ for (i = 1; i <= NF; i++) if ($i == "dev") { print $(i + 1); exit } }' <<<"$route"
}

case "${1:-}" in
    apply)
        [ $# -eq 3 ] || die "internal usage: netem-helper apply <delay-ms> <loss-pct>"
        interface="$(p2p_interface)"
        tc qdisc replace dev "$interface" root netem delay "${2}ms" loss "${3}%"
        echo "[netem] applied to $interface: delay ${2}ms, loss ${3}%"
        ;;
    clear)
        [ $# -eq 1 ] || die "internal usage: netem-helper clear"
        interface="$(p2p_interface)"
        if tc qdisc show dev "$interface" | grep -q 'qdisc netem'; then
            tc qdisc del dev "$interface" root
            echo "[netem] cleared from $interface"
        else
            echo "[netem] no netem qdisc is active on $interface"
        fi
        ;;
    status)
        [ $# -eq 1 ] || die "internal usage: netem-helper status"
        interface="$(p2p_interface)"
        echo "[netem] P2P interface: $interface"
        tc qdisc show dev "$interface"
        ;;
    *)
        die "internal usage: netem-helper {apply|clear|status}"
        ;;
esac
