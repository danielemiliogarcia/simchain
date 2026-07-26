#!/bin/bash
# Rebuild the disposable electrs index after a rollback-only chain rewind.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"
ELECTRS_CONTAINER=regtest-electrs
NODE1_CONTAINER=btc-simnet-node1
TIMEOUT_SECS=300

info() { printf '[explorer-recovery] %s\n' "$1"; }
die() { printf '[explorer-recovery] ERROR: %s\n' "$1" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: ./scripts/recover-explorer.sh

Recreates an existing Simchain electrs container, discards its stale
container-local index, and waits until its indexed tip exactly matches node1.
If the selected Compose profile did not create electrs, this is a no-op.
EOF
}

case "${1:-}" in
    "") ;;
    -h|--help|help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
esac

if ! docker inspect "$ELECTRS_CONTAINER" >/dev/null 2>&1; then
    info "electrs is not part of the selected Compose profile; nothing to recover"
    exit 0
fi

electrs_running="$(docker inspect -f '{{.State.Running}}' "$ELECTRS_CONTAINER")"
electrs_exit_code="$(docker inspect -f '{{.State.ExitCode}}' "$ELECTRS_CONTAINER")"
mempool_running="$(docker inspect -f '{{.State.Running}}' mempool-web 2>/dev/null || true)"
if [ "$electrs_running" != "true" ] && [ "$electrs_exit_code" = "0" ] \
    && [ "$mempool_running" != "true" ]; then
    info "electrs was stopped cleanly and no mempool frontend is active; nothing to recover"
    exit 0
fi

[ "$(docker inspect -f '{{.State.Running}}' "$NODE1_CONTAINER" 2>/dev/null || true)" = "true" ] \
    || die "node1 is not running"

env_get() {
    [ -f "$ENV_FILE" ] || return 0
    sed -n "s/^$1=//p" "$ENV_FILE" | tail -1
}

resolve() {
    local from_process="${!1:-}"
    local from_file=""
    if [ -n "$from_process" ]; then
        printf '%s\n' "$from_process"
        return
    fi
    from_file="$(env_get "$1")"
    printf '%s\n' "${from_file:-$2}"
}

electrs_port="$(resolve ELECTRS_HTTP_PORT 3000)"
[[ "$electrs_port" =~ ^[0-9]+$ ]] || die "ELECTRS_HTTP_PORT must be numeric"

info "recreating electrs with an empty disposable index"
docker compose -f "$REPO_ROOT/docker-compose.yml" --project-directory "$REPO_ROOT" \
    up -d --force-recreate electrs

node_rpc() {
    docker exec "$NODE1_CONTAINER" sh -c \
        'bitcoin-cli -regtest -rpcuser="$BTC_RPC_USER" -rpcpassword="$BTC_RPC_PASS" "$@"' \
        sh "$@"
}

deadline=$((SECONDS + TIMEOUT_SECS))
while (( SECONDS < deadline )); do
    node_height="$(node_rpc getblockcount 2>/dev/null || true)"
    node_hash="$(node_rpc getbestblockhash 2>/dev/null || true)"
    indexed_height="$(curl -fsS --max-time 2 "http://127.0.0.1:${electrs_port}/blocks/tip/height" 2>/dev/null || true)"
    indexed_hash="$(curl -fsS --max-time 2 "http://127.0.0.1:${electrs_port}/blocks/tip/hash" 2>/dev/null || true)"
    if [ -n "$node_height" ] && [ "$indexed_height" = "$node_height" ] \
        && [ -n "$node_hash" ] && [ "$indexed_hash" = "$node_hash" ]; then
        info "electrs synchronized with node1 at height $node_height ($node_hash)"
        exit 0
    fi
    sleep 1
done

die "electrs did not synchronize with node1 within ${TIMEOUT_SECS}s; inspect with: docker compose logs electrs"
