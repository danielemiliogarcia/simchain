#!/bin/bash

# Apply P2P-only egress latency and packet loss to a running simchain node.
#
# Usage:
#   ./scripts/netem.sh apply <node> --delay-ms N [--loss-pct P]
#   ./scripts/netem.sh clear <node>
#   ./scripts/netem.sh status <node>

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/docker-compose.yml"

die() { echo "[netem] ERROR: $1" >&2; exit 1; }

compose() {
    docker compose -f "$COMPOSE_FILE" --project-directory "$REPO_ROOT" --profile partition "$@"
}

service_for() {
    case "$1" in
        btc-simnet-node1) echo btc-simnet-netem-node1 ;;
        btc-simnet-node2) echo btc-simnet-netem-node2 ;;
        btc-simnet-node3) echo btc-simnet-netem-node3 ;;
        *) die "node must be btc-simnet-node1, btc-simnet-node2, or btc-simnet-node3" ;;
    esac
}

require_running() {
    [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true)" = true ] \
        || die "$1 is not running"
}

run_helper() {
    local node="$1"; shift
    require_running "$node"
    compose run --rm --no-deps "$(service_for "$node")" "$@"
}

valid_loss() {
    [[ "$1" =~ ^([0-9]|[1-9][0-9])([.][0-9]+)?$|^100([.]0+)?$ ]]
}

cmd_apply() {
    local node="$1"; shift
    local delay="" loss=0
    while [ $# -gt 0 ]; do
        case "$1" in
            --delay-ms) [ $# -ge 2 ] || die "--delay-ms requires a value"; delay="$2"; shift 2 ;;
            --loss-pct) [ $# -ge 2 ] || die "--loss-pct requires a value"; loss="$2"; shift 2 ;;
            *) die "unknown apply option: $1" ;;
        esac
    done
    [[ "$delay" =~ ^[0-9]+$ ]] || die "--delay-ms must be a non-negative integer"
    valid_loss "$loss" || die "--loss-pct must be a number from 0 through 100"
    run_helper "$node" apply "$delay" "$loss"
}

usage() {
    sed -n '3,8p' "$0" | sed 's/^# \{0,1\}//'
}

case "${1:-}" in
    apply)  [ $# -ge 2 ] || { usage; exit 1; }; node="$2"; shift 2; cmd_apply "$node" "$@" ;;
    clear)  [ $# -eq 2 ] || { usage; exit 1; }; run_helper "$2" clear ;;
    status) [ $# -eq 2 ] || { usage; exit 1; }; run_helper "$2" status ;;
    *)      usage; exit 1 ;;
esac
