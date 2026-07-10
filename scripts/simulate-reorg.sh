#!/bin/bash
# Convenience wrapper: one-shot reorg of the last N blocks (default 3).
# Usage: ./scripts/simulate-reorg.sh [depth]
# The simnet must be running (docker compose up) with some blocks mined.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

exec docker compose \
  -f "$REPO_ROOT/docker-compose.yml" \
  --project-directory "$REPO_ROOT" \
  run --no-deps --rm btc-simnet-reorg "$@"
