FROM debian:bullseye-slim

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
  && apt-get install -y curl gnupg gosu

# Clean up apt cache
RUN apt-get clean \
  && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*

# Set environment variables for Bitcoin Core
ARG BITCOIN_VERSION
ENV BITCOIN_DATA=/home/bitcoin/.bitcoin
ENV PATH=/opt/bitcoin-${BITCOIN_VERSION}/bin:$PATH

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
  echo "$PLATFORM" > /tmp/bitcoin-platform

# Download Bitcoin Core, server is failing, use mirror
RUN set -eux; \
  TARGETPLATFORM="$(cat /tmp/bitcoin-platform)"; \
  curl -SLO https://bitcoincore.org/bin/bitcoin-core-${BITCOIN_VERSION}/bitcoin-${BITCOIN_VERSION}-${TARGETPLATFORM}.tar.gz \
  && curl -SLO https://bitcoincore.org/bin/bitcoin-core-${BITCOIN_VERSION}/SHA256SUMS \
  && curl -SLO https://bitcoincore.org/bin/bitcoin-core-${BITCOIN_VERSION}/SHA256SUMS.asc

# Download Bitcoin Core (this mirror currently only provide up to version 27.0)
# RUN curl -SLO https://bitcoin.org/bin/bitcoin-core-${BITCOIN_VERSION}/bitcoin-${BITCOIN_VERSION}-${TARGETPLATFORM}.tar.gz \
#   && curl -SLO https://bitcoin.org/bin/bitcoin-core-${BITCOIN_VERSION}/SHA256SUMS \
#   && curl -SLO https://bitcoin.org/bin/bitcoin-core-${BITCOIN_VERSION}/SHA256SUMS.asc

# Verify SHA256SUMS is signed by the Bitcoin Core release signers: import
# the builder keys from the bitcoin-core/guix.sigs repo (no flaky
# keyservers) and check the detached signature. gpg exits non-zero (failing
# the build) unless at least one good signature is found; signatures from
# builders whose key is not in the repo only produce warnings.
RUN set -eux; \
  curl -SL https://github.com/bitcoin-core/guix.sigs/archive/refs/heads/main.tar.gz \
    | tar -xz -C /tmp guix.sigs-main/builder-keys; \
  gpg --batch --import /tmp/guix.sigs-main/builder-keys/*.gpg; \
  gpg --batch --verify SHA256SUMS.asc SHA256SUMS; \
  rm -rf /tmp/guix.sigs-main /root/.gnupg

# Check the SHA256SUM of the downloaded tarball
RUN set -eux; \
  TARGETPLATFORM="$(cat /tmp/bitcoin-platform)"; \
  grep " bitcoin-${BITCOIN_VERSION}-${TARGETPLATFORM}.tar.gz" SHA256SUMS

# Validate the SHA256 checksum
RUN set -eux; \
  TARGETPLATFORM="$(cat /tmp/bitcoin-platform)"; \
  grep " bitcoin-${BITCOIN_VERSION}-${TARGETPLATFORM}.tar.gz" SHA256SUMS | sha256sum -c -


# Extract the Bitcoin Core binaries
RUN tar -xzf *.tar.gz -C /opt

# Cleanup
RUN rm *.tar.gz *.asc \
  && rm -rf /opt/bitcoin-${BITCOIN_VERSION}/bin/bitcoin-qt

# Copy the entrypoint script
COPY docker-entrypoint.sh /entrypoint.sh

# REGTEST VERSION, WE DONT WANT A VOLUME
# Define a volume for the Bitcoin data directory
# VOLUME ["/home/bitcoin/.bitcoin"]

# Expose necessary ports
# EXPOSE 8332 8333 18332 18333 18443 18444 38333 38332

# Set the entrypoint to the custom script
ENTRYPOINT ["/entrypoint.sh"]

RUN bitcoind -version
# Verify installation
RUN bitcoind -version | grep "Bitcoin Core daemon version v${BITCOIN_VERSION}"

# Set the default command to start bitcoind
CMD ["bitcoind"]

