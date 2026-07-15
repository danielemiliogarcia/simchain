#!/bin/bash
# Compatibility wrapper for the durable control-plane degradation job.
# Usage: ./scripts/degrade.sh <node> <delay-ms> <loss-pct> <seconds>

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ $# -ne 4 ]]; then
  echo "usage: $0 <node> <delay-ms> <loss-pct> <seconds>" >&2
  exit 2
fi
node="${1#btc-simnet-}"
delay="$2"
loss="$3"
seconds="${4%s}"
[[ "$seconds" =~ ^[1-9][0-9]*$ ]] || {
  echo "duration must be a positive number of seconds" >&2
  exit 2
}
args=(degrade --node "$node" --delay-ms "$delay" --loss-pct "$loss" --seconds "$seconds" --wait)

if command -v simchainctl >/dev/null 2>&1; then
  exec simchainctl "${args[@]}"
fi
exec cargo run --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p simchainctl -- "${args[@]}"
