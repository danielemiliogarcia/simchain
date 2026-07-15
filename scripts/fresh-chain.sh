#!/bin/bash

# Fresh chain in one command: tear down the stack INCLUDING the chain volumes
# and bring it back up, so the bootstrap runs from block 0 again.
#
# Since the node datadirs live on named volumes, a plain `docker compose up`
# resumes the previous chain; this wrapper is the "I want a disposable chain
# like before" button (equivalent to `down -v` followed by `up -d`).
#
# Usage:
#   ./scripts/fresh-chain.sh                     # fresh basic stack
#   ./scripts/fresh-chain.sh --profile mempool   # fresh stack + explorer tools
#   ./scripts/fresh-chain.sh --profile all-tools # fresh stack + all-tools
#
# Extra arguments are passed to docker compose as global flags (profiles etc).
# The current chain is destroyed; snapshot it first if you care about it:
#   ./scripts/snapshot.sh save keep-me

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

compose() { docker compose -f "$REPO_ROOT/docker-compose.yml" --project-directory "$REPO_ROOT" "$@"; }

echo "[fresh-chain] wiping the stack and the chain volumes..."
compose --profile "*" down -v --remove-orphans

echo "[fresh-chain] starting a fresh chain..."
compose "$@" up -d

echo "[fresh-chain] done; bootstrap is running (watch it with:" \
     "docker compose logs -f btc-simnet-mining-controller)"
