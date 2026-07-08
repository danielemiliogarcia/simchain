#!/bin/bash
set -e


# PUID/PGID (not UID/GID: bash pre-sets those as readonly shell variables,
# so values passed with -e UID=... never reach this script)
if [ -n "${PUID+x}" ] && [ "${PUID}" != "0" ]; then
  usermod -u "$PUID" bitcoin
fi

if [ -n "${PGID+x}" ] && [ "${PGID}" != "0" ]; then
  groupmod -g "$PGID" bitcoin
fi

echo "$0: assuming uid:gid for bitcoin:bitcoin of $(id -u bitcoin):$(id -g bitcoin)"

if [ $(echo "$1" | cut -c1) = "-" ]; then
  echo "$0: assuming arguments for bitcoind"
  set -- bitcoind "$@"
fi

if [ $(echo "$1" | cut -c1) = "-" ] || [ "$1" = "bitcoind" ]; then
  mkdir -p "$BITCOIN_DATA"
  chmod 700 "$BITCOIN_DATA"
  # Fix permissions for home dir.
  chown -R bitcoin:bitcoin "$(getent passwd bitcoin | cut -d: -f6)"
  # Fix permissions for bitcoin data dir.
  chown -R bitcoin:bitcoin "$BITCOIN_DATA"

  echo "$0: setting data directory to $BITCOIN_DATA"

  set -- "$@" -datadir="$BITCOIN_DATA"
fi

if [ "$(id -u)" != "$(id -u bitcoin)" ]; then
  if [ "$1" = "bitcoind" ] || [ "$1" = "bitcoin-cli" ] || [ "$1" = "bitcoin-tx" ]; then
    exec gosu bitcoin "$@"
  fi
fi

echo
exec "$@"
