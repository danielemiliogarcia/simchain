#!/bin/bash
# Convenience wrapper for the durable control-plane degradation job.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  ./scripts/degrade.sh start <node> <delay-ms> <loss-pct> <seconds>
  ./scripts/degrade.sh --help

Starts a timed P2P-only latency/loss job and waits for it to finish.

Arguments:
  start     Required confirmation that degradation should be started.
  node      node1, node2, node3, or btc-simnet-nodeN.
  delay-ms  Added one-way egress delay in milliseconds. Use 0 for loss-only.
  loss-pct  Egress packet loss percentage from 0 through 100. Use 0 for delay-only.
  seconds   Positive duration, with optional trailing s.
EOF
}

case "${1:-}" in
  -h|--help|help)
    usage
    exit 0
    ;;
  start)
    shift
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

if [[ $# -ne 4 ]]; then
  usage >&2
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
args=(degrade start --node "$node" --delay-ms "$delay" --loss-pct "$loss" --seconds "$seconds" --wait)

if command -v simchainctl >/dev/null 2>&1; then
  exec simchainctl "${args[@]}"
fi
exec cargo run --quiet --manifest-path "$REPO_ROOT/Cargo.toml" -p simchainctl -- "${args[@]}"
