#!/bin/bash
# Convenience wrapper for the first-party control-plane client.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  ./scripts/simulate-reorg.sh start [depth] [empty]
  ./scripts/simulate-reorg.sh --help

Starts a control-plane reorg job and waits for it to finish.

Arguments:
  start   Required confirmation that a reorg should be started.
  depth   Positive number of tip blocks to replace. Defaults to REORG_DEPTH or 3.
  empty   Optional chaos mode: mine empty replacement blocks.

Environment:
  REORG_NODE                  Replacement-chain miner, default node3.
  REORG_ADDS_NEW_TXS          Extra fresh txs for non-empty replacement blocks.
  REORG_DOUBLE_SPEND_PCT      Percentage of eligible orphaned txs to conflict.
  SIMCHAIN_CONTROL_URL        Control-plane URL for simchainctl.
  SIMCHAIN_CONTROL_TOKEN      Control-plane bearer token for simchainctl.
EOF
}

if [[ $# -eq 0 ]]; then
  usage >&2
  exit 2
fi

case "$1" in
  -h|--help|help)
    usage
    exit 0
    ;;
  start)
    shift
    ;;
  *)
    echo "first argument must be 'start' (or --help)" >&2
    usage >&2
    exit 2
    ;;
esac

[[ $# -le 2 ]] || { echo "too many arguments" >&2; usage >&2; exit 2; }

depth="${1:-${REORG_DEPTH:-3}}"
mode="${2:-}"
[[ "$depth" =~ ^[1-9][0-9]*$ ]] || { echo "depth must be a positive integer" >&2; exit 2; }
[[ -z "$mode" || "$mode" == "empty" ]] || { echo "second argument must be 'empty'" >&2; exit 2; }

node="${REORG_NODE:-node3}"
node="${node#btc-simnet-}"
args=(reorg start --depth "$depth" --node "$node" --wait)
[[ "$mode" == "empty" ]] && args+=(--empty)
[[ "${REORG_ADDS_NEW_TXS:-0}" != "0" ]] && args+=(--adds-new-txs "${REORG_ADDS_NEW_TXS}")
[[ "${REORG_DOUBLE_SPEND_PCT:-0}" != "0" ]] && args+=(--double-spend-pct "${REORG_DOUBLE_SPEND_PCT}")

if command -v simchainctl >/dev/null 2>&1; then
  exec simchainctl "${args[@]}"
fi
exec cargo run --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p simchainctl -- "${args[@]}"
