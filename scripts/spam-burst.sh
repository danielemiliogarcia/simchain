#!/bin/bash
# Convenience wrapper for `simchainctl spam burst`.
# Usage: ./scripts/spam-burst.sh <miner-node> --txs N [--data-bytes B | --outputs-per-tx M]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

[[ $# -ge 1 ]] || { echo "usage: $0 <miner-node> --txs N [--data-bytes B | --outputs-per-tx M]" >&2; exit 2; }
node="${1#btc-simnet-}"
shift
args=(spam burst --node "$node" --wait "$@")

if command -v simchainctl >/dev/null 2>&1; then
  exec simchainctl "${args[@]}"
fi
exec cargo run --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p simchainctl -- "${args[@]}"
