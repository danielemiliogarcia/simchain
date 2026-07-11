FROM debian:trixie-slim

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends bash iproute2 libc-bin \
    && rm -rf /var/lib/apt/lists/*

COPY docker/netem-entrypoint.sh /usr/local/bin/netem-helper
ENTRYPOINT ["netem-helper"]
