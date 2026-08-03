#!/bin/bash

# Full snapshot - save/restore the full simchain state PLUS the explorer's
# view of the chain (electrs index, mempool.space MariaDB).
#
# A plain ./scripts/snapshot.sh snapshot archives only the 3 node datadirs;
# on restore, the explorer stores are rebuilt by re-indexing node1's ACTIVE
# chain, which drops any stale/orphaned blocks a reorg had produced before
# the save (docs/SNAPSHOTS.md, "What survives a snapshot"). This script is a
# standalone companion -- it does not touch snapshot.sh or its snapshots --
# that additionally captures and restores those explorer stores, so a
# restored chain keeps the orphaned blocks the explorer had indexed.
#
# Usage:
#   ./scripts/full-snapshot.sh save <name>               stop stack, archive, resume
#   ./scripts/full-snapshot.sh restore <name> [--force] [compose-flags...]
#                                                        wipe volumes, unarchive, up
#   ./scripts/full-snapshot.sh list                      show saved full snapshots
#
# Same semantics as snapshot.sh (recorded service shape, --force guard rails,
# name validation) plus two extra stores captured/restored when their
# container was running at save time: the electrs index (container
# regtest-electrs, profiles electrs/mempool/all-tools) and the mempool.space
# MariaDB (container mempool-db, profiles mempool/all-tools).
#
# Full snapshots land in ./snapshots/full/ (override with FULL_SNAPSHOT_DIR),
# separate from plain snapshots' ./snapshots/ so the two formats never
# collide or get cross-restored by the wrong script.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"
SNAP_DIR="${FULL_SNAPSHOT_DIR:-$REPO_ROOT/snapshots/full}"

VOLUMES=(btc-simnet-node1-data btc-simnet-node2-data btc-simnet-node3-data)
HELPER_IMAGE=alpine:3
ELECTRS_CONTAINER=regtest-electrs
ELECTRS_DB_DIR=/tmp/electrs-db
MEMPOOL_DB_CONTAINER=mempool-db
MEMPOOL_DB_DIR=/var/lib/mysql

# All stop/start/down calls select every profile so optional and on-demand
# containers are cycled together with the nodes; services without containers
# are simply ignored. `up` is NOT profile-forced: restore starts the default
# stack unless the user passes profile flags.
compose()    { docker compose -f "$REPO_ROOT/docker-compose.yml" --project-directory "$REPO_ROOT" --profile "*" "$@"; }
compose_up() { docker compose -f "$REPO_ROOT/docker-compose.yml" --project-directory "$REPO_ROOT" "$@"; }

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "[full-snapshot] $1"; }
ok()    { echo -e "${GREEN}[full-snapshot]${NC} $1"; }
warn()  { echo -e "${YELLOW}[full-snapshot] WARNING:${NC} $1"; }
die()   { echo -e "${RED}[full-snapshot] ERROR:${NC} $1" >&2; exit 1; }

# Read a single KEY=value from .env without executing it.
env_get() {
    [ -f "$ENV_FILE" ] || return 0
    grep -E "^$1=" "$ENV_FILE" | tail -1 | cut -d= -f2-
}

# Resolve a setting with the same precedence docker compose uses:
# OS environment > .env > compose default.
resolve() { # resolve VAR default
    local from_env="${!1:-}"
    if [ -n "$from_env" ]; then echo "$from_env"; else
        local from_file; from_file="$(env_get "$1")"
        echo "${from_file:-$2}"
    fi
}

BTC_IMAGE="$(resolve BTC_IMAGE bitcoin/bitcoin:31.1)"
BTC_RPC_USER="$(resolve BTC_RPC_USER foo)"
BTC_RPC_PASS="$(resolve BTC_RPC_PASS rpcpassword)"
NODE2_WALLET_NAME="$(resolve NODE2_WALLET_NAME node2)"
NODE3_WALLET_NAME="$(resolve NODE3_WALLET_NAME node3)"
USER_ADDRESS="$(resolve USER_ADDRESS bcrt1q6rz28mcfaxtmd6v789l9rrlrusdprr9pz3cppk)"
NODE1_DISABLE_WALLET="$(resolve NODE1_DISABLE_WALLET 1)"

bcli() {
    docker exec btc-simnet-node1 bitcoin-cli -regtest \
        -rpcuser="$BTC_RPC_USER" -rpcpassword="$BTC_RPC_PASS" "$@"
}

check_name() {
    [[ "${1:-}" =~ ^[A-Za-z0-9._-]+$ ]] \
        || die "snapshot name must match [A-Za-z0-9._-]+ (got '${1:-}')"
}

node1_running() {
    [ "$(docker inspect -f '{{.State.Running}}' btc-simnet-node1 2>/dev/null)" = "true" ]
}

container_exists() {
    docker inspect -f '{{.Id}}' "$1" >/dev/null 2>&1
}

# Extract a value from the one-key-per-line metadata json (no jq dependency).
meta_get() { # meta_get FILE KEY
    sed -n "s/^  \"$2\": \"\{0,1\}\([^\",]*\)\"\{0,1\},\{0,1\}\$/\1/p" "$1"
}

wait_node1_healthy() {
    info "waiting for node1 to be healthy..."
    for _ in $(seq 1 60); do
        if [ "$(docker inspect -f '{{.State.Health.Status}}' btc-simnet-node1 2>/dev/null)" = "healthy" ]; then
            return 0
        fi
        sleep 1
    done
    die "node1 did not become healthy within 60s"
}

cmd_save() {
    local name="$1"
    check_name "$name"
    local tar_file="$SNAP_DIR/$name.tar.gz" meta_file="$SNAP_DIR/$name.json"
    local electrs_file="$SNAP_DIR/$name.electrs.tar" mempool_db_file="$SNAP_DIR/$name.mempool-db.tar"
    [ -e "$tar_file" ] || [ -e "$meta_file" ] \
        && die "snapshot '$name' already exists ($tar_file); pick another name or delete it first"
    node1_running \
        || die "node1 is not running; snapshots are taken from a live stack (docker compose up -d)"

    local height hash
    height="$(bcli getblockcount)" || die "could not query node1 height (check RPC credentials)"
    hash="$(bcli getbestblockhash)"
    info "saving full snapshot '$name' at height $height"

    # Remember what is running: `start` must target exactly these services --
    # starting the whole profile trips over containers that were never created
    # (e.g. the mempool stack when only the basic profile is up). The same
    # list also tells us which explorer stores actually have data to capture.
    local running_services
    running_services="$(compose ps --services --status running)"
    local has_electrs=false has_mempool_db=false
    echo "$running_services" | grep -qx electrs && has_electrs=true
    echo "$running_services" | grep -qx mempool-db && has_mempool_db=true

    info "stopping the stack (clean shutdown flushes chainstate, wallets and mempool.dat)..."
    compose stop

    mkdir -p "$SNAP_DIR"
    info "archiving node datadirs..."
    docker run --rm \
        -v "${VOLUMES[0]}:/snap/node1:ro" \
        -v "${VOLUMES[1]}:/snap/node2:ro" \
        -v "${VOLUMES[2]}:/snap/node3:ro" \
        -v "$SNAP_DIR:/out" \
        "$HELPER_IMAGE" \
        tar czf "/out/$name.tar.gz" --numeric-owner -C /snap node1 node2 node3

    if $has_electrs; then
        info "archiving electrs index..."
        docker cp -a "$ELECTRS_CONTAINER:$ELECTRS_DB_DIR" - > "$electrs_file" \
            || die "failed to archive electrs store from $ELECTRS_CONTAINER"
    fi
    if $has_mempool_db; then
        info "archiving mempool.space database..."
        docker cp -a "$MEMPOOL_DB_CONTAINER:$MEMPOOL_DB_DIR" - > "$mempool_db_file" \
            || die "failed to archive mempool.space database from $MEMPOOL_DB_CONTAINER"
    fi

    # shellcheck disable=SC2086 # flatten the newline-separated list to one line
    local services_flat; services_flat="$(echo $running_services)"
    cat > "$meta_file" <<EOF
{
  "name": "$name",
  "created": "$(date -Is)",
  "height": $height,
  "best_block_hash": "$hash",
  "btc_image": "$BTC_IMAGE",
  "node2_wallet": "$NODE2_WALLET_NAME",
  "node3_wallet": "$NODE3_WALLET_NAME",
  "user_address": "$USER_ADDRESS",
  "node1_disable_wallet": "$NODE1_DISABLE_WALLET",
  "services": "$services_flat",
  "electrs": $has_electrs,
  "mempool_db": $has_mempool_db
}
EOF

    info "resuming the stack..."
    # shellcheck disable=SC2086 # service names, word splitting wanted
    [ -n "$running_services" ] && compose start $running_services
    ok "full snapshot '$name' saved: $(du -h "$tar_file" | cut -f1) at height $height ($tar_file)"
}

cmd_restore() {
    local name="$1"; shift
    check_name "$name"
    local force=false up_args=()
    for arg in "$@"; do
        if [ "$arg" = "--force" ]; then force=true; else up_args+=("$arg"); fi
    done

    local tar_file="$SNAP_DIR/$name.tar.gz" meta_file="$SNAP_DIR/$name.json"
    local electrs_file="$SNAP_DIR/$name.electrs.tar" mempool_db_file="$SNAP_DIR/$name.mempool-db.tar"
    [ -f "$tar_file" ] || die "no such snapshot: $tar_file (see: full-snapshot.sh list)"
    [ -f "$meta_file" ] || die "snapshot metadata missing: $meta_file"

    local m_height m_hash m_image m_w2 m_w3 m_addr m_n1w m_services m_electrs m_mempool_db
    m_height="$(meta_get "$meta_file" height)"
    m_hash="$(meta_get "$meta_file" best_block_hash)"
    m_image="$(meta_get "$meta_file" btc_image)"
    m_w2="$(meta_get "$meta_file" node2_wallet)"
    m_w3="$(meta_get "$meta_file" node3_wallet)"
    m_addr="$(meta_get "$meta_file" user_address)"
    m_n1w="$(meta_get "$meta_file" node1_disable_wallet)"
    m_services="$(meta_get "$meta_file" services)"
    m_electrs="$(meta_get "$meta_file" electrs)"
    m_mempool_db="$(meta_get "$meta_file" mempool_db)"

    # Guard rails: a snapshot only makes sense under the environment it was
    # taken with. Image/wallet mismatches break the stack, so they abort;
    # a changed user address silently strands the chain's user funding on
    # the old address, so it warns as loudly as possible.
    local fatal=()
    [ "$m_image" = "$BTC_IMAGE" ] \
        || fatal+=("BTC_IMAGE differs: snapshot '$m_image' vs current '$BTC_IMAGE' (datadir upgrades are one-way)")
    [ "$m_w2" = "$NODE2_WALLET_NAME" ] \
        || fatal+=("NODE2_WALLET_NAME differs: snapshot '$m_w2' vs current '$NODE2_WALLET_NAME' (miners would start from an empty wallet)")
    [ "$m_w3" = "$NODE3_WALLET_NAME" ] \
        || fatal+=("NODE3_WALLET_NAME differs: snapshot '$m_w3' vs current '$NODE3_WALLET_NAME' (miners would start from an empty wallet)")
    if [ "${#fatal[@]}" -gt 0 ]; then
        for f in "${fatal[@]}"; do
            if $force; then warn "$f -- proceeding (--force)"; else echo -e "${RED}[full-snapshot] MISMATCH:${NC} $f" >&2; fi
        done
        $force || die "environment does not match the snapshot; fix .env or pass --force"
    fi
    if [ "$m_addr" != "$USER_ADDRESS" ]; then
        warn "USER_ADDRESS differs from the snapshot!"
        warn "  snapshot funded: $m_addr"
        warn "  current .env:    $USER_ADDRESS"
        warn "the restored chain's user funds belong to the SNAPSHOT address; only keys for it can spend them"
    fi
    # Warn only: flipping the flag hides/reveals a node1 wallet but corrupts
    # nothing. Old snapshots predate the field; skip the check when absent.
    if [ -n "$m_n1w" ] && [ "$m_n1w" != "$NODE1_DISABLE_WALLET" ]; then
        warn "NODE1_DISABLE_WALLET differs: snapshot '$m_n1w' vs current '$NODE1_DISABLE_WALLET'"
        if [ "$NODE1_DISABLE_WALLET" != "0" ]; then
            warn "any node1 wallet stored in the snapshot stays invisible until NODE1_DISABLE_WALLET=0 (nothing is lost)"
        fi
    fi

    info "restoring '$name' (height $m_height, $m_image)"
    info "tearing down the stack (volumes are recreated from the archive)..."
    compose down --remove-orphans
    for v in "${VOLUMES[@]}"; do
        docker volume rm "$v" >/dev/null 2>&1 || true
    done

    # Restore the stack in the shape it had at save time: the metadata lists
    # the services that were running, and naming them explicitly makes
    # compose activate their profiles automatically -- no --profile flags
    # needed. User-passed compose flags override the recorded shape (they
    # are compose GLOBALS, e.g. --profile all-tools, so they go before the
    # subcommand); old snapshots without the field fall back to the default
    # services.
    local services=()
    if [ "${#up_args[@]}" -eq 0 ] && [ -n "$m_services" ]; then
        # shellcheck disable=SC2206 # service names, word splitting wanted
        services=($m_services)
        info "restoring the saved service shape: ${services[*]}"
    fi

    # `create` (not `up`) first: compose recreates the volumes with its own
    # labels (avoiding "created outside of compose" warnings on every later
    # command) but nothing starts until the datadirs are unarchived.
    compose_up ${up_args[0]+"${up_args[@]}"} create --quiet-pull ${services[0]+"${services[@]}"} >/dev/null 2>&1 \
        || compose_up ${up_args[0]+"${up_args[@]}"} create ${services[0]+"${services[@]}"}

    info "unarchiving node datadirs..."
    docker run --rm \
        -v "${VOLUMES[0]}:/snap/node1" \
        -v "${VOLUMES[1]}:/snap/node2" \
        -v "${VOLUMES[2]}:/snap/node3" \
        -v "$SNAP_DIR:/in:ro" \
        "$HELPER_IMAGE" \
        tar xzf "/in/$name.tar.gz" --numeric-owner -C /snap

    # Explorer stores have no named volume: they live in each container's own
    # filesystem, so they are streamed back in with docker cp BEFORE the
    # container starts (create leaves it stopped with an empty writable
    # layer). If the recorded store was captured but the restore shape
    # doesn't include that container (--profile override), skip it loudly
    # instead of failing the whole restore.
    if [ "$m_electrs" = "true" ]; then
        if container_exists "$ELECTRS_CONTAINER"; then
            info "restoring electrs index..."
            docker cp -a - "$ELECTRS_CONTAINER:$(dirname "$ELECTRS_DB_DIR")" < "$electrs_file" \
                || die "failed to restore electrs store into $ELECTRS_CONTAINER"
        else
            warn "snapshot captured an electrs store but $ELECTRS_CONTAINER is not in the restore shape; skipping (pass --profile electrs, mempool or all-tools to include it)"
        fi
    fi
    if [ "$m_mempool_db" = "true" ]; then
        if container_exists "$MEMPOOL_DB_CONTAINER"; then
            info "restoring mempool.space database..."
            docker cp -a - "$MEMPOOL_DB_CONTAINER:$(dirname "$MEMPOOL_DB_DIR")" < "$mempool_db_file" \
                || die "failed to restore mempool.space database into $MEMPOOL_DB_CONTAINER"
        else
            warn "snapshot captured a mempool.space database but $MEMPOOL_DB_CONTAINER is not in the restore shape; skipping (pass --profile mempool or all-tools to include it)"
        fi
    fi

    info "starting the stack..."
    compose_up ${up_args[0]+"${up_args[@]}"} up -d ${services[0]+"${services[@]}"}
    wait_node1_healthy

    # The controller may already be mining on top, so the height can only be
    # >= the saved one; the saved block hash must match exactly.
    local height hash
    height="$(bcli getblockcount)"
    [ "$height" -ge "$m_height" ] \
        || die "restored height $height is below the snapshot height $m_height"
    hash="$(bcli getblockhash "$m_height")"
    [ "$hash" = "$m_hash" ] \
        || die "block hash at height $m_height does not match the snapshot (got $hash, want $m_hash)"
    ok "full snapshot '$name' restored; chain resumed at height $height"
}

cmd_list() {
    [ -d "$SNAP_DIR" ] || { info "no snapshots yet ($SNAP_DIR is empty)"; return 0; }
    local found=false
    printf '%-20s %-8s %-28s %s\n' NAME HEIGHT CREATED IMAGE
    for meta_file in "$SNAP_DIR"/*.json; do
        [ -f "$meta_file" ] || continue
        found=true
        printf '%-20s %-8s %-28s %s\n' \
            "$(meta_get "$meta_file" name)" \
            "$(meta_get "$meta_file" height)" \
            "$(meta_get "$meta_file" created)" \
            "$(meta_get "$meta_file" btc_image)"
    done
    $found || info "no snapshots yet in $SNAP_DIR"
}

case "${1:-}" in
    save)    [ $# -eq 2 ] || die "usage: full-snapshot.sh save <name>"; cmd_save "$2" ;;
    restore) [ $# -ge 2 ] || die "usage: full-snapshot.sh restore <name> [--force] [compose-up-args...]"
             shift; cmd_restore "$@" ;;
    list)    cmd_list ;;
    *)       sed -n '3,28p' "$0" | sed 's/^# \{0,1\}//'; exit 1 ;;
esac
