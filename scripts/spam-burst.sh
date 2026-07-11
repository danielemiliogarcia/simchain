#!/bin/bash

# Run the scenario engine's one-shot spam_burst action manually.
# Usage: ./scripts/spam-burst.sh <miner-node> --txs N [--outputs-per-tx M]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/docker-compose.yml"

die() { echo "[spam-burst] ERROR: $1" >&2; exit 1; }
positive_integer() { [[ "$1" =~ ^[1-9][0-9]*$ ]]; }
nonnegative_integer() { [[ "$1" =~ ^[0-9]+$ ]]; }

[ $# -ge 1 ] || die "usage: $0 <miner-node> --txs N [--outputs-per-tx M]"
NODE="$1"
shift
case "$NODE" in
    btc-simnet-node2|btc-simnet-node3) ;;
    *) die "miner node must be btc-simnet-node2 or btc-simnet-node3" ;;
esac

TXS=""
OUTPUTS=0
while [ $# -gt 0 ]; do
    case "$1" in
        --txs) [ $# -ge 2 ] || die "--txs requires a value"; TXS="$2"; shift 2 ;;
        --outputs-per-tx) [ $# -ge 2 ] || die "--outputs-per-tx requires a value"; OUTPUTS="$2"; shift 2 ;;
        *) die "unknown option: $1" ;;
    esac
done
positive_integer "$TXS" || die "--txs must be a positive integer"
nonnegative_integer "$OUTPUTS" || die "--outputs-per-tx must be a non-negative integer"

SCENARIO_PATH="$(mktemp "$REPO_ROOT/.spam-burst.XXXXXX.yml")"
trap 'rm -f "$SCENARIO_PATH"' EXIT
cat >"$SCENARIO_PATH" <<EOF
version: 1
steps:
  - type: spam_burst
    node: $NODE
    txs: $TXS
    outputs_per_tx: $OUTPUTS
EOF

SCENARIO_FILE="/workspace/$(basename "$SCENARIO_PATH")" docker compose \
    -f "$COMPOSE_FILE" --project-directory "$REPO_ROOT" --profile scenario \
    run --rm btc-simnet-scenario
