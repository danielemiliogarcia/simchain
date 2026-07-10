#!/bin/bash
# Build the local bitcoin node image (optional, by default the compose file
# pulls ${BTC_IMAGE:-bitcoin/bitcoin:31.1} from the registry).
set -e

# Locate the repo root from this script so it works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"

# Resolve BITCOIN_VERSION from:
# 1. the environment passed to this script
# 2. the repo .env
# 3. the hard default
if [ -f "$ENV_FILE" ]; then
  ENV_VERSION="$(grep -E '^BITCOIN_VERSION=' "$ENV_FILE" | tail -n1 | cut -d= -f2 | tr -d '[:space:]')"
fi
BITCOIN_VERSION="${BITCOIN_VERSION:-${ENV_VERSION:-31.1}}"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  cat <<EOF
Usage: $0

Build the local simchain bitcoind Docker image.

Version resolution order:
  1. BITCOIN_VERSION from the environment
  2. BITCOIN_VERSION in $ENV_FILE
  3. default 31.1

Example:
  BITCOIN_VERSION=31.1 $0
EOF
  exit 0
fi

docker build \
  --build-arg UID="$(id -u)" \
  --build-arg GID="$(id -g)" \
  --build-arg BITCOIN_VERSION="$BITCOIN_VERSION" \
  -t "simchainbitcoinnode:$BITCOIN_VERSION" \
  -f "$REPO_ROOT/docker/bitcoin-node.Dockerfile" \
  "$REPO_ROOT"

echo
echo "Built simchainbitcoinnode:$BITCOIN_VERSION"
echo "To use it, set in your .env:  BTC_IMAGE=simchainbitcoinnode:$BITCOIN_VERSION"
