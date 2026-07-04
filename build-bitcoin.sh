#!/bin/bash
# Build the local bitcoin node image (optional, by default the compose file
# pulls ${BTC_IMAGE:-bitcoin/bitcoin:29.0} from the registry).
set -e

# Take BITCOIN_VERSION from .env if present, else default
if [ -f .env ]; then
  VERSION=$(grep -E '^BITCOIN_VERSION=' .env | tail -n1 | cut -d= -f2 | tr -d '[:space:]')
fi
VERSION=${VERSION:-29.0}

docker build --build-arg UID=$(id -u) --build-arg GID=$(id -g) --build-arg BITCOIN_VERSION=$VERSION -t simchainbitcoinnode:$VERSION .

echo
echo "Built simchainbitcoinnode:$VERSION"
echo "To use it, set in your .env:  BTC_IMAGE=simchainbitcoinnode:$VERSION"
