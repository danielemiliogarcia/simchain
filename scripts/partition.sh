#!/bin/bash
# Compatibility wrapper for the durable control-plane partition job.
# Usage: ./scripts/partition.sh run <miner-node> [--main-blocks N] [--isolated-blocks N]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ "${1:-}" != run || $# -lt 2 ]]; then
  echo "usage: $0 run <miner-node> [--main-blocks N] [--isolated-blocks N]" >&2
  echo "raw disconnect/heal commands were replaced by TTL-healed control-plane jobs" >&2
  exit 2
fi
node="${2#btc-simnet-}"
shift 2
args=(partition --node "$node" --wait "$@")

if command -v simchainctl >/dev/null 2>&1; then
  exec simchainctl "${args[@]}"
fi
exec cargo run --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p simchainctl -- "${args[@]}"
