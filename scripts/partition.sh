#!/bin/bash
# Convenience wrapper for the durable control-plane partition job.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  ./scripts/partition.sh start <miner-node> [--main-blocks N] [--isolated-blocks N]
  ./scripts/partition.sh --help

Starts a TTL-healed control-plane partition job and waits for it to finish.

Arguments:
  start       Required confirmation that a partition should be started.
  miner-node  node2, node3, btc-simnet-node2, or btc-simnet-node3.

The legacy confirmation word "run" is still accepted as an alias for "start".
EOF
}

case "${1:-}" in
  -h|--help|help)
    usage
    exit 0
    ;;
  start|run)
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

if [[ $# -lt 2 ]]; then
  usage >&2
  exit 2
fi
node="${2#btc-simnet-}"
shift 2
args=(partition start --node "$node" --wait "$@")

if command -v simchainctl >/dev/null 2>&1; then
  exec simchainctl "${args[@]}"
fi
exec cargo run --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p simchainctl -- "${args[@]}"
