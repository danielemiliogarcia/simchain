#!/bin/bash
# Convenience wrapper: one-shot reorg of the last N blocks (default 3).
# Usage: ./simulate-reorg.sh [depth]
# The simnet must be running (docker compose up) with some blocks mined.
exec docker compose run --rm btc-simnet-reorg "$@"
