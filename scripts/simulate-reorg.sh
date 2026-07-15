#!/bin/bash
# Convenience wrapper for the first-party control-plane client.
# Usage: ./scripts/simulate-reorg.sh [depth] [empty]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

depth="${1:-${REORG_DEPTH:-3}}"
mode="${2:-}"
[[ "$depth" =~ ^[1-9][0-9]*$ ]] || { echo "depth must be a positive integer" >&2; exit 2; }
[[ -z "$mode" || "$mode" == "empty" ]] || { echo "second argument must be 'empty'" >&2; exit 2; }

node="${REORG_NODE:-node3}"
node="${node#btc-simnet-}"
args=(reorg --depth "$depth" --node "$node" --wait)
[[ "$mode" == "empty" ]] && args+=(--empty)
[[ "${REORG_ADDS_NEW_TXS:-0}" != "0" ]] && args+=(--adds-new-txs "${REORG_ADDS_NEW_TXS}")
[[ "${REORG_DOUBLE_SPEND_PCT:-0}" != "0" ]] && args+=(--double-spend-pct "${REORG_DOUBLE_SPEND_PCT}")

if command -v simchainctl >/dev/null 2>&1; then
  exec simchainctl "${args[@]}"
fi
exec cargo run --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p simchainctl -- "${args[@]}"
