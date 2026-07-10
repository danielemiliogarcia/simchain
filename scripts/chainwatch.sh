#!/bin/bash

# Chainwatch - external Bitcoin regtest chain watcher for simchain
#
# Runs on the HOST, outside the docker stack, and talks to a node purely over
# its host-exposed JSON-RPC port (via curl) -- exactly like a real third-party
# observer would. It never mines and needs no access to docker.
#
# It prints every new block as it arrives, in chronological order (newest on
# the last line, like shell output). Because simchain simulates reorgs, it also
# detects them: when the tip rolls back or a block's hash changes it prints a
# REORG banner and the replaced range.
#
# Default target is node1 (localhost:18443), the non-mining full node exposed
# to the host, which always follows the canonical winning chain. RPC credentials
# are read from the repo .env (BTC_RPC_USER / BTC_RPC_PASS); node3 is not
# reachable from the host by design.

set -u

# Locate the repo root and .env relative to this script so it works from any
# cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Defaults (overridable by .env and CLI flags)
RPC_HOST="127.0.0.1"
RPC_PORT=""           # resolved from .env NODE1_RPC_PORT, else 18443
RPC_USER=""           # resolved from .env BTC_RPC_USER, else foo
RPC_PASSWORD=""       # resolved from .env BTC_RPC_PASS, else rpcpassword
INTERVAL=2            # Poll interval in seconds
ENV_FILE="$REPO_ROOT/.env"
MAX_REORG_DEPTH=100   # How far back to search for a reorg fork point

# Recent height -> block hash, used to notice when a block gets replaced.
declare -A SEEN
LAST=""               # Highest block height seen so far

# Colors
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'
BLUE='\033[0;34m'; BOLD='\033[1m'; NC='\033[0m'

print_info()    { echo -e "${BLUE}[INFO]${NC} $1"; }
print_success() { echo -e "${GREEN}[OK]${NC} $1"; }
print_warning() { echo -e "${YELLOW}[WARN]${NC} $1"; }
print_error()   { echo -e "${RED}[ERROR]${NC} $1"; }

# Read a single KEY=value from an env file without executing it.
env_get() {
    [ -f "$ENV_FILE" ] || return 0
    grep -E "^$1=" "$ENV_FILE" | tail -1 | cut -d= -f2-
}

# Fill any unset credential/port from .env, then fall back to project defaults.
load_env() {
    [ -z "$RPC_USER" ]     && RPC_USER="$(env_get BTC_RPC_USER)"
    [ -z "$RPC_PASSWORD" ] && RPC_PASSWORD="$(env_get BTC_RPC_PASS)"
    [ -z "$RPC_PORT" ]     && RPC_PORT="$(env_get NODE1_RPC_PORT)"
    RPC_USER="${RPC_USER:-foo}"
    RPC_PASSWORD="${RPC_PASSWORD:-rpcpassword}"
    RPC_PORT="${RPC_PORT:-18443}"
}

# JSON-RPC call over the host port. $1 = method, $2 = params JSON (default []).
rpc() {
    curl -s -m 10 --user "$RPC_USER:$RPC_PASSWORD" \
        --data-binary "{\"jsonrpc\":\"1.0\",\"id\":\"chainwatch\",\"method\":\"$1\",\"params\":${2:-[]}}" \
        -H 'content-type: text/plain;' "http://$RPC_HOST:$RPC_PORT/"
}

get_block_count() { rpc getblockcount | grep -oE '"result":[0-9]+' | grep -oE '[0-9]+'; }
get_block_hash()  { rpc getblockhash "[$1]" | grep -oE '"result":"[0-9a-f]{64}"' | grep -oE '[0-9a-f]{64}'; }

# Wait until the node's RPC answers.
wait_for_rpc() {
    local tries=0
    while true; do
        [ -n "$(get_block_count)" ] && { print_success "Connected to $RPC_HOST:$RPC_PORT"; return 0; }
        ((tries++))
        [ "$tries" -eq 1 ] && print_warning "Waiting for RPC at $RPC_HOST:$RPC_PORT (is the node up and the port exposed?)..."
        sleep "$INTERVAL"
    done
}

# Print one formatted block line for the given height. Records its hash in SEEN.
print_block() {
    local height="$1" hash info ntx btime clock
    hash="$(get_block_hash "$height")"
    [ -z "$hash" ] && return 1
    info="$(rpc getblock "[\"$hash\"]")"
    ntx="$(echo "$info"  | grep -oE '"nTx": *[0-9]+'  | grep -oE '[0-9]+')"
    btime="$(echo "$info" | grep -oE '"time": *[0-9]+' | grep -oE '[0-9]+' | head -1)"
    [ -n "$btime" ] && btime="$(date -d "@$btime" '+%H:%M:%S')" || btime="--:--:--"
    clock="$(date '+%H:%M:%S')"
    printf "${GREEN}[%s]${NC} block ${BOLD}#%s${NC}  %3s txs  %s  %s\n" \
        "$clock" "$height" "${ntx:-?}" "$btime" "$hash"
    SEEN[$height]="$hash"
}

# Drop SEEN entries older than the reorg window so the array stays small.
prune_seen() {
    local tip="$1" floor h
    floor=$((tip - MAX_REORG_DEPTH)); [ "$floor" -lt 0 ] && floor=0
    for h in "${!SEEN[@]}"; do
        [ "$h" -lt "$floor" ] && unset 'SEEN[$h]'
    done
}

# Seed the recent chain window without printing it, so the first reorg after
# startup can still find an accurate fork point.
seed_recent_chain() {
    local tip="$1" floor h hash
    floor=$((tip - MAX_REORG_DEPTH)); [ "$floor" -lt 0 ] && floor=0
    for ((h = floor; h <= tip; h++)); do
        hash="$(get_block_hash "$h")"
        [ -n "$hash" ] && SEEN[$h]="$hash"
    done
}

# Walk down from `start` to the highest height whose recorded hash still matches
# the node's current hash: the fork point (last common block). Prints it.
find_fork() {
    local start="$1" h cur
    local floor=$((start - MAX_REORG_DEPTH)); [ "$floor" -lt 1 ] && floor=1
    for ((h = start; h >= floor; h--)); do
        [ -z "${SEEN[$h]:-}" ] && continue
        cur="$(get_block_hash "$h")"
        [ "$cur" = "${SEEN[$h]}" ] && { echo "$h"; return 0; }
    done
    echo "$floor"   # fell off the window; treat the floor as the fork
}

cleanup() { echo; print_warning "Stopping chainwatch..."; exit 0; }

usage() {
    cat <<EOF
Usage: $0 [OPTIONS]

Watch a simchain regtest node from the host over RPC and print every new block;
highlight reorgs. Does not mine and does not use docker. RPC credentials default
to the repo .env (BTC_RPC_USER / BTC_RPC_PASS).

Options:
  -H, --host HOST        RPC host (default: 127.0.0.1)
  -P, --port PORT        RPC port (default: .env NODE1_RPC_PORT or 18443)
  -u, --user USER        RPC username (default: .env BTC_RPC_USER or foo)
  -p, --password PASS    RPC password (default: .env BTC_RPC_PASS or rpcpassword)
  -i, --interval SECS    Poll interval in seconds (default: 2)
  -e, --env FILE         Path to the env file (default: <repo root>/.env)
  -h, --help             Show this help

Examples:
  $0                     # watch node1 on localhost:18443 (the user endpoint)
  $0 -P 28443            # watch node2 (its host-mapped port)
  $0 -i 1                # poll every second
EOF
}

main() {
    while [[ $# -gt 0 ]]; do
        case $1 in
            -H|--host)     RPC_HOST="$2";     shift 2 ;;
            -P|--port)     RPC_PORT="$2";     shift 2 ;;
            -u|--user)     RPC_USER="$2";     shift 2 ;;
            -p|--password) RPC_PASSWORD="$2"; shift 2 ;;
            -i|--interval) INTERVAL="$2";     shift 2 ;;
            -e|--env)      ENV_FILE="$2";     shift 2 ;;
            -h|--help)     usage; exit 0 ;;
            *) print_error "Unknown option: $1"; usage; exit 1 ;;
        esac
    done

    if ! [[ "$INTERVAL" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
        print_error "Invalid interval: $INTERVAL (must be a positive number)"
        exit 1
    fi
    command -v curl >/dev/null || { print_error "curl is required but not found"; exit 1; }

    load_env

    print_info "chainwatch - watching $RPC_HOST:$RPC_PORT every ${INTERVAL}s as RPC user '$RPC_USER' (Ctrl+C to stop)"
    echo
    wait_for_rpc

    # Seed a recent hash window so reorg detection has a real fork search
    # range, while still only printing blocks from now on.
    local tip
    tip="$(get_block_count)"
    seed_recent_chain "$tip"
    print_info "Chain at height #$tip; watching for new blocks..."
    print_block "$tip"
    LAST="$tip"

    while true; do
        sleep "$INTERVAL"

        tip="$(get_block_count)"
        if [ -z "$tip" ]; then
            print_warning "RPC unreachable, waiting to reconnect..."
            wait_for_rpc
            continue
        fi

        # Nothing changed since last poll.
        if [ "$tip" -eq "$LAST" ] && [ "$(get_block_hash "$tip")" = "${SEEN[$tip]:-}" ]; then
            continue
        fi

        # If the block at min(LAST,tip) still has the hash we recorded, the chain
        # only grew; otherwise a reorg rewrote history at or below that height.
        local base=$LAST
        [ "$tip" -lt "$base" ] && base=$tip
        local base_hash
        base_hash="$(get_block_hash "$base")"

        if [ "$base_hash" = "${SEEN[$base]:-}" ] && [ "$tip" -gt "$LAST" ]; then
            # Pure extension: print the new blocks in order.
            local h
            for ((h = LAST + 1; h <= tip; h++)); do
                print_block "$h"
            done
        else
            # Reorg: find the fork point and report the replaced range.
            local fork replaced_from h
            fork="$(find_fork "$base")"
            replaced_from=$((fork + 1))
            echo
            print_warning "REORG detected: blocks #${replaced_from}..#${LAST} replaced; forked at #${fork}, new tip #${tip}"
            for ((h = tip + 1; h <= LAST; h++)); do unset 'SEEN[$h]'; done
            for ((h = replaced_from; h <= tip; h++)); do print_block "$h"; done
            echo
        fi

        LAST="$tip"
        prune_seen "$tip"
    done
}

trap cleanup SIGINT SIGTERM
main "$@"
