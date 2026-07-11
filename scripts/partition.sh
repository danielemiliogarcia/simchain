#!/bin/bash

# Isolate one miner from the P2P mesh while retaining RPC control access.
#
# Usage:
#   ./scripts/partition.sh run <miner-node> [--main-blocks N] [--isolated-blocks N] [--keep-spammer]
#   ./scripts/partition.sh disconnect <miner-node>
#   ./scripts/partition.sh heal <miner-node>
#   ./scripts/partition.sh status

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"
COMPOSE_FILE="$REPO_ROOT/docker-compose.yml"
P2P_NETWORK=btc-simnet-p2p
NODES=(btc-simnet-node1 btc-simnet-node2 btc-simnet-node3)

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info() { echo -e "[partition] $1"; }
ok()   { echo -e "${GREEN}[partition]${NC} $1"; }
warn() { echo -e "${YELLOW}[partition] WARNING:${NC} $1"; }
die()  { echo -e "${RED}[partition] ERROR:${NC} $1" >&2; exit 1; }

compose() {
    docker compose -f "$COMPOSE_FILE" --project-directory "$REPO_ROOT" "$@"
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
NODE2_WALLET_NAME="$(resolve NODE2_WALLET_NAME node2)"
NODE3_WALLET_NAME="$(resolve NODE3_WALLET_NAME node3)"
DEFAULT_MAIN_BLOCKS="$(resolve PARTITION_MAIN_BLOCKS 3)"
DEFAULT_ISOLATED_BLOCKS="$(resolve PARTITION_ISOLATED_BLOCKS 4)"
CONVERGENCE_TIMEOUT="$(resolve PARTITION_CONVERGENCE_TIMEOUT_SECS 60)"
PEER_TIMEOUT="$(resolve PARTITION_PEER_TIMEOUT_SECS 15)"

validate_miner() {
    case "$1" in
        btc-simnet-node2|btc-simnet-node3) ;;
        *) die "miner node must be btc-simnet-node2 or btc-simnet-node3" ;;
    esac
}

positive_integer() {
    [[ "$1" =~ ^[1-9][0-9]*$ ]]
}

container_running() {
    [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null || true)" = true ]
}

require_nodes() {
    local node
    docker network inspect "$P2P_NETWORK" >/dev/null 2>&1 \
        || die "$P2P_NETWORK does not exist; recreate the stack with the new two-network topology"
    for node in "${NODES[@]}"; do
        container_running "$node" || die "$node is not running"
    done
}

network_attached() {
    [ "$(docker inspect -f '{{if index .NetworkSettings.Networks "btc-simnet-p2p"}}yes{{else}}no{{end}}' "$1" 2>/dev/null || true)" = yes ]
}

alias_for() {
    case "$1" in
        btc-simnet-node1) echo node1-p2p ;;
        btc-simnet-node2) echo node2-p2p ;;
        btc-simnet-node3) echo node3-p2p ;;
        *) return 1 ;;
    esac
}

wallet_for() {
    case "$1" in
        btc-simnet-node2) echo "$NODE2_WALLET_NAME" ;;
        btc-simnet-node3) echo "$NODE3_WALLET_NAME" ;;
        *) return 1 ;;
    esac
}

bcli() {
    local node="$1"; shift
    docker exec "$node" bitcoin-cli -regtest \
        -rpcuser="$BTC_RPC_USER" -rpcpassword="$BTC_RPC_PASS" "$@"
}

control_rpc_height() {
    local target="$1"
    docker exec btc-simnet-node1 bitcoin-cli -regtest \
        -rpcconnect="$target" -rpcport=18443 \
        -rpcuser="$BTC_RPC_USER" -rpcpassword="$BTC_RPC_PASS" getblockcount
}

p2p_peer_count() {
    local peers
    peers="$(bcli "$1" getpeerinfo)" || return 1
    grep -Ec '"addr": "(172\.30\.0\.|node[123]-p2p:)' <<<"$peers" || true
}

disconnect_current_peers() {
    local node="$1" id
    while read -r id; do
        [ -n "$id" ] || continue
        bcli "$node" -named disconnectnode nodeid="$id" >/dev/null 2>&1 || true
    done < <(bcli "$node" getpeerinfo | sed -n 's/^[[:space:]]*"id": \([0-9][0-9]*\),/\1/p')
}

disconnect_node() {
    local node="$1"
    if ! network_attached "$node"; then
        info "$node is already detached from $P2P_NETWORK"
        return 1
    fi
    docker network disconnect "$P2P_NETWORK" "$node"
    # Docker removes the route immediately, but established TCP sessions can
    # remain visible to Core until their next failed write or keepalive.
    disconnect_current_peers "$node"
    ok "$node disconnected from $P2P_NETWORK; RPC/control remains attached"
}

trigger_reconnect() {
    local target="$1" main_miner target_alias main_alias
    target_alias="$(alias_for "$target")"
    if [ "$target" = btc-simnet-node3 ]; then
        main_miner=btc-simnet-node2
    else
        main_miner=btc-simnet-node3
    fi
    main_alias="$(alias_for "$main_miner")"

    bcli btc-simnet-node1 addnode "$target_alias:18444" onetry >/dev/null 2>&1 || true
    bcli "$main_miner" addnode "$target_alias:18444" onetry >/dev/null 2>&1 || true
    bcli "$target" addnode node1-p2p:18444 onetry >/dev/null 2>&1 || true
    bcli "$target" addnode "$main_alias:18444" onetry >/dev/null 2>&1 || true
}

heal_node() {
    local node="$1" alias
    if network_attached "$node"; then
        info "$node is already attached to $P2P_NETWORK"
        return 1
    fi
    alias="$(alias_for "$node")"
    docker network connect --alias "$alias" "$P2P_NETWORK" "$node"
    trigger_reconnect "$node"
    ok "$node reconnected to $P2P_NETWORK as $alias"
}

wait_for_split() {
    local target="$1" main_miner="$2" start=$SECONDS target_peers main_peers
    while (( SECONDS - start < PEER_TIMEOUT )); do
        target_peers="$(p2p_peer_count "$target")"
        main_peers="$(p2p_peer_count "$main_miner")"
        if [ "$target_peers" -eq 0 ] && [ "$main_peers" -ge 1 ]; then
            return
        fi
        sleep 1
    done
    die "P2P split did not settle within ${PEER_TIMEOUT}s"
}

mine_blocks() {
    local node="$1" blocks="$2" wallet address
    wallet="$(wallet_for "$node")"
    address="$(bcli "$node" -rpcwallet="$wallet" getnewaddress)" \
        || die "could not get a mining address from wallet '$wallet' on $node"
    bcli "$node" -rpcwallet="$wallet" generatetoaddress "$blocks" "$address" >/dev/null
    info "$node mined $blocks block(s)"
}

wait_for_convergence() {
    local start=$SECONDS h1 h2 h3
    while (( SECONDS - start < CONVERGENCE_TIMEOUT )); do
        h1="$(bcli btc-simnet-node1 getbestblockhash)"
        h2="$(bcli btc-simnet-node2 getbestblockhash)"
        h3="$(bcli btc-simnet-node3 getbestblockhash)"
        if [ "$h1" = "$h2" ] && [ "$h1" = "$h3" ]; then
            ok "all nodes converged at height $(bcli btc-simnet-node1 getblockcount), tip $h1"
            return
        fi
        sleep 1
    done
    die "nodes did not converge within ${CONVERGENCE_TIMEOUT}s"
}

cmd_disconnect() {
    local target="$1"
    validate_miner "$target"
    require_nodes
    disconnect_node "$target" || true
}

cmd_heal() {
    local target="$1"
    validate_miner "$target"
    require_nodes
    heal_node "$target" || true
}

cmd_status() {
    local node attached peers height hash
    printf '%-24s %-10s %-7s %-8s %s\n' NODE P2P PEERS HEIGHT BEST_BLOCK_HASH
    for node in "${NODES[@]}"; do
        if ! container_running "$node"; then
            printf '%-24s %-10s %-7s %-8s %s\n' "$node" unavailable - - -
            continue
        fi
        if network_attached "$node"; then attached=attached; else attached=detached; fi
        peers="$(p2p_peer_count "$node" 2>/dev/null || echo error)"
        height="$(bcli "$node" getblockcount 2>/dev/null || echo error)"
        hash="$(bcli "$node" getbestblockhash 2>/dev/null || echo error)"
        printf '%-24s %-10s %-7s %-8s %s\n' "$node" "$attached" "$peers" "$height" "$hash"
    done
}

cmd_run() {
    local target="$1"; shift
    local main_blocks="$DEFAULT_MAIN_BLOCKS" isolated_blocks="$DEFAULT_ISOLATED_BLOCKS"
    local keep_spammer=false main_miner controller_running=false spammer_running=false split_active=false

    while [ $# -gt 0 ]; do
        case "$1" in
            --main-blocks) [ $# -ge 2 ] || die "--main-blocks requires a value"; main_blocks="$2"; shift 2 ;;
            --isolated-blocks) [ $# -ge 2 ] || die "--isolated-blocks requires a value"; isolated_blocks="$2"; shift 2 ;;
            --keep-spammer) keep_spammer=true; shift ;;
            *) die "unknown run option: $1" ;;
        esac
    done

    validate_miner "$target"
    positive_integer "$main_blocks" || die "--main-blocks must be a positive integer"
    positive_integer "$isolated_blocks" || die "--isolated-blocks must be a positive integer"
    [ "$main_blocks" -ne "$isolated_blocks" ] \
        || die "main and isolated block counts must differ to guarantee a deterministic winner"
    positive_integer "$CONVERGENCE_TIMEOUT" || die "PARTITION_CONVERGENCE_TIMEOUT_SECS must be a positive integer"
    positive_integer "$PEER_TIMEOUT" || die "PARTITION_PEER_TIMEOUT_SECS must be a positive integer"
    require_nodes

    local node
    for node in "${NODES[@]}"; do
        network_attached "$node" || die "$node is already detached; heal it before starting a partition run"
    done

    local height
    height="$(bcli btc-simnet-node1 getblockcount)" || die "could not query node1 height"
    [ "$height" -ge 204 ] || die "bootstrap is incomplete (node1 height $height, need at least 204)"

    if [ "$target" = btc-simnet-node3 ]; then main_miner=btc-simnet-node2; else main_miner=btc-simnet-node3; fi
    container_running btc-simnet-mining-controller && controller_running=true
    container_running btc-simnet-spammer && spammer_running=true

    cleanup() {
        local status=$?
        trap - EXIT
        if $split_active; then
            warn "healing $target during cleanup"
            heal_node "$target" >/dev/null 2>&1 || true
        fi
        if $controller_running; then compose start btc-simnet-mining-controller >/dev/null || true; fi
        if $spammer_running && ! $keep_spammer; then compose start btc-simnet-spammer >/dev/null || true; fi
        exit "$status"
    }
    trap cleanup EXIT

    if $controller_running; then
        info "stopping mining controller"
        compose stop btc-simnet-mining-controller >/dev/null
    fi
    if $spammer_running && ! $keep_spammer; then
        info "stopping spammer"
        compose stop btc-simnet-spammer >/dev/null
    fi

    disconnect_node "$target"
    split_active=true
    control_rpc_height "$target" >/dev/null \
        || die "target RPC is not reachable over btc-simnet-control after the split"
    wait_for_split "$target" "$main_miner"
    ok "P2P split verified; $main_miner remains connected to node1"

    mine_blocks "$main_miner" "$main_blocks"
    mine_blocks "$target" "$isolated_blocks"

    heal_node "$target"
    split_active=false
    wait_for_convergence

    if $controller_running; then
        compose start btc-simnet-mining-controller >/dev/null
        controller_running=false
        info "restarted mining controller"
    fi
    if $spammer_running && ! $keep_spammer; then
        compose start btc-simnet-spammer >/dev/null
        spammer_running=false
        info "restarted spammer"
    fi
    trap - EXIT
}

usage() {
    sed -n '3,9p' "$0" | sed 's/^# \{0,1\}//'
}

case "${1:-}" in
    run)        [ $# -ge 2 ] || { usage; exit 1; }; target="$2"; shift 2; cmd_run "$target" "$@" ;;
    disconnect) [ $# -eq 2 ] || { usage; exit 1; }; cmd_disconnect "$2" ;;
    heal)       [ $# -eq 2 ] || { usage; exit 1; }; cmd_heal "$2" ;;
    status)     [ $# -eq 1 ] || { usage; exit 1; }; cmd_status ;;
    *)          usage; exit 1 ;;
esac
