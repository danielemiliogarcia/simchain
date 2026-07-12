#!/bin/bash

# Make one node's P2P link slower and/or lossy for a while, then restore it.
# Beginner-friendly wrapper around scripts/netem.sh: apply -> wait -> clear.
#
# Usage:
#   ./scripts/degrade.sh <node> <delay-ms> <loss-pct> <duration>
#
#   <node>      btc-simnet-node1 | btc-simnet-node2 | btc-simnet-node3
#   <delay-ms>  extra one-way delay on packets the node sends (0 = none)
#   <loss-pct>  percent of sent packets dropped (0 = none)
#   <duration>  30s = 30 seconds, 5b = until 5 new blocks are mined
#               (a bare number means seconds)
#
# Examples:
#   ./scripts/degrade.sh btc-simnet-node3 500 1 60s
#   ./scripts/degrade.sh btc-simnet-node3 2000 0 5b

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"
NETEM="$SCRIPT_DIR/netem.sh"

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
info() { echo -e "[degrade] $1"; }
ok()   { echo -e "${GREEN}[degrade]${NC} $1"; }
die()  { echo -e "${RED}[degrade] ERROR:${NC} $1" >&2; exit 1; }

usage() {
    sed -n '3,17p' "$0" | sed 's/^# \{0,1\}//'
}

env_get() {
    [ -f "$ENV_FILE" ] || return 0
    sed -n "s/^[[:space:]]*$1=//p" "$ENV_FILE" | tail -n 1 | sed 's/[[:space:]]*#.*$//; s/[[:space:]]*$//'
}

resolve() {
    local name="$1" default="$2" value
    value="${!name:-}"
    [ -n "$value" ] || value="$(env_get "$name")"
    printf '%s' "${value:-$default}"
}

BTC_RPC_USER="$(resolve BTC_RPC_USER foo)"
BTC_RPC_PASS="$(resolve BTC_RPC_PASS rpcpassword)"

node1_height() {
    docker exec btc-simnet-node1 bitcoin-cli -regtest \
        -rpcuser="$BTC_RPC_USER" -rpcpassword="$BTC_RPC_PASS" getblockcount
}

[ $# -eq 4 ] || { usage; exit 1; }
node="$1" delay="$2" loss="$3" duration="$4"

case "$node" in
    btc-simnet-node1|btc-simnet-node2|btc-simnet-node3) ;;
    *) die "node must be btc-simnet-node1, btc-simnet-node2, or btc-simnet-node3" ;;
esac
[[ "$delay" =~ ^[0-9]+$ ]] || die "<delay-ms> must be a non-negative integer"
[[ "$loss" =~ ^([0-9]|[1-9][0-9])([.][0-9]+)?$|^100([.]0+)?$ ]] \
    || die "<loss-pct> must be a number from 0 through 100"
[ "$delay" -gt 0 ] || [ "${loss%%.*}" -gt 0 ] || [[ "$loss" =~ ^0[.][0-9]*[1-9] ]] \
    || die "nothing to degrade: give a delay, a loss percentage, or both"

case "$duration" in
    *b) mode=blocks; amount="${duration%b}" ;;
    *s) mode=seconds; amount="${duration%s}" ;;
    *)  mode=seconds; amount="$duration" ;;
esac
[[ "$amount" =~ ^[1-9][0-9]*$ ]] \
    || die "<duration> must be a positive number of seconds (30s) or blocks (5b)"

restore() {
    local status=$?
    trap - EXIT
    info "restoring $node's P2P link"
    "$NETEM" clear "$node" || true
    exit "$status"
}

info "degrading $node: +${delay}ms one-way delay, ${loss}% packet loss"
"$NETEM" apply "$node" --delay-ms "$delay" --loss-pct "$loss"
trap restore EXIT
"$NETEM" status "$node"

if [ "$mode" = seconds ]; then
    info "holding the degraded link for ${amount}s (Ctrl+C restores early)"
    sleep "$amount"
else
    start_height="$(node1_height)" || die "could not query node1 height"
    target=$((start_height + amount))
    last="$start_height"
    info "holding until $amount new block(s) are mined (heights $((start_height + 1))..$target; needs mining running; Ctrl+C restores early)"
    while true; do
        height="$(node1_height)"
        if [ "$height" -ne "$last" ]; then
            info "block $((height - start_height))/$amount (height $height)"
            last="$height"
        fi
        [ "$height" -ge "$target" ] && break
        sleep 2
    done
fi

ok "observation window over"
