#!/bin/bash
VERSION=29.0
docker build --build-arg UID=$(id -u) --build-arg GID=$(id -g) --build-arg BITCOIN_VERSION=$VERSION -t simchainbitcoinnode:$VERSION .
