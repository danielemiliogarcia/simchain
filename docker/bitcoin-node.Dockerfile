FROM debian:trixie-slim

# Set default UID and GID
ARG UID=101
ARG GID=101
RUN echo "UID: ${UID}"
RUN echo "GID: ${GID}"

# Create bitcoin user and group with specified UID and GID
RUN groupadd --gid ${GID} bitcoin \
  && useradd --create-home --no-log-init -u ${UID} -g ${GID} bitcoin

# Install dependencies
RUN apt-get update -y \
  && apt-get install -y --no-install-recommends ca-certificates curl gnupg gosu \
  && apt-get clean \
  && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Set environment variables for Bitcoin Core
ARG BITCOIN_VERSION
ENV BITCOIN_DATA=/home/bitcoin/.bitcoin
ENV PATH=/opt/bitcoin-${BITCOIN_VERSION}/bin:$PATH
LABEL Name="simchain_bitcoin_node"
LABEL Version="${BITCOIN_VERSION}"

# The bitcoin.org mirror currently only provides versions through 27.0. To
# use that fallback, replace the DOWNLOAD_BASE assignment below with:
# DOWNLOAD_BASE="https://bitcoin.org/bin/bitcoin-core-${BITCOIN_VERSION}";

# Verify SHA256SUMS is signed by the Bitcoin Core release signers: import
# the builder keys from the bitcoin-core/guix.sigs repo (no flaky
# keyservers) and check the detached signature. gpg exits non-zero (failing
# the build) unless at least one good signature is found; signatures from
# builders whose key is not in the repo only produce warnings. Keep download,
# verification, extraction and cleanup in one layer so archives and keys are
# never retained in the final image history.
RUN set -eux; \
  ARCH="$(uname -m)"; \
  case "$ARCH" in \
    x86_64) PLATFORM="x86_64-linux-gnu" ;; \
    aarch64) PLATFORM="aarch64-linux-gnu" ;; \
    armv7l) PLATFORM="arm-linux-gnueabihf" ;; \
    riscv64) PLATFORM="riscv64-linux-gnu" ;; \
    ppc64|ppc64le) PLATFORM="powerpc64-linux-gnu" ;; \
    *) echo "Unsupported architecture: $ARCH" && exit 1 ;; \
  esac; \
  DOWNLOAD_BASE="https://bitcoincore.org/bin/bitcoin-core-${BITCOIN_VERSION}"; \
  curl -SLO "${DOWNLOAD_BASE}/bitcoin-${BITCOIN_VERSION}-${PLATFORM}.tar.gz"; \
  curl -SLO "${DOWNLOAD_BASE}/SHA256SUMS"; \
  curl -SLO "${DOWNLOAD_BASE}/SHA256SUMS.asc"; \
  curl -SL https://github.com/bitcoin-core/guix.sigs/archive/refs/heads/main.tar.gz \
    | tar -xz -C /tmp guix.sigs-main/builder-keys; \
  gpg --batch --import /tmp/guix.sigs-main/builder-keys/*.gpg; \
  gpg --batch --verify SHA256SUMS.asc SHA256SUMS; \
  grep " bitcoin-${BITCOIN_VERSION}-${PLATFORM}.tar.gz" SHA256SUMS | sha256sum -c -; \
  tar -xzf "bitcoin-${BITCOIN_VERSION}-${PLATFORM}.tar.gz" -C /opt; \
  rm -f "bitcoin-${BITCOIN_VERSION}-${PLATFORM}.tar.gz" SHA256SUMS SHA256SUMS.asc; \
  rm -rf /tmp/guix.sigs-main /root/.gnupg /opt/bitcoin-${BITCOIN_VERSION}/bin/bitcoin-qt

# Copy the entrypoint script
COPY docker/entrypoint.sh /entrypoint.sh

# REGTEST VERSION, WE DONT WANT A VOLUME
# Define a volume for the Bitcoin data directory
# VOLUME ["/home/bitcoin/.bitcoin"]

# Expose necessary ports
# EXPOSE 8332 8333 18332 18333 18443 18444 38333 38332

# Set the entrypoint to the custom script
ENTRYPOINT ["/entrypoint.sh"]

# Verify installation
RUN bitcoind -version | grep "Bitcoin Core daemon version v${BITCOIN_VERSION}"

# Set the default command to start bitcoind
CMD ["bitcoind"]
