# Single build for all simchain Rust tools. One builder stage compiles the
# whole workspace against the committed Cargo.lock, then one final stage per
# tool copies out its release binary. Compose selects which binary an image
# contains via `target:` in docker-compose.yml.
#
# This replaces the previous three independent Dockerfiles (one per tool, each
# with its own `COPY . .` context) that resolved three separate dependency
# graphs. The workspace is compiled once here.

# ---- shared builder --------------------------------------------------------
FROM rust:1-trixie AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY crates ./crates
# --locked fails the build if Cargo.lock is stale, which is the whole point of
# committing the lockfile: every image is built from the exact pinned
# dependency set, so two builds of the same commit ship identical dependencies.
RUN cargo build --release --workspace --locked

# ---- mining-controller -----------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS mining-controller
COPY --from=builder /app/target/release/simchain-mining-controller /simchain-mining-controller
ENTRYPOINT ["/simchain-mining-controller"]

# ---- spammer ---------------------------------------------------------------
FROM debian:trixie-slim AS spammer
COPY --from=builder /app/target/release/simchain-spammer /usr/local/bin/simchain-spammer
ENTRYPOINT ["simchain-spammer"]

# ---- reorg -----------------------------------------------------------------
FROM debian:trixie-slim AS reorg
COPY --from=builder /app/target/release/simchain-reorg /usr/local/bin/simchain-reorg
ENTRYPOINT ["simchain-reorg"]

# ---- scenario-engine -------------------------------------------------------
# Compatibility HTTP client only. Scenario execution belongs to the control
# plane, so this image deliberately contains no Docker CLI or orchestration
# backend.
FROM gcr.io/distroless/cc-debian12 AS scenario-engine
COPY --from=builder /app/target/release/simchain-scenario-engine /simchain-scenario-engine
ENTRYPOINT ["/simchain-scenario-engine"]

# ---- control-plane ---------------------------------------------------------
# Mining, spam, and reorg paths use worker APIs/Bitcoin RPC. The transitional
# boot/lifecycle adapter still needs Compose until Phase 7 removes it and this
# image's Docker CLI/socket access. Debian is required because the builder
# links against glibc.
FROM debian:trixie-slim AS control-plane
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates docker-cli docker-compose \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/simchain-control-plane /usr/local/bin/simchain-control-plane
ENTRYPOINT ["simchain-control-plane"]

# ---- network-agent ---------------------------------------------------------
# Runs as root with Docker granting only CAP_NET_ADMIN. It shares a Bitcoin
# node's network namespace and owns only P2P-interface nft/tc state.
FROM debian:trixie-slim AS network-agent
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends iproute2 nftables \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/simchain-network-agent /usr/local/bin/simchain-network-agent
ENTRYPOINT ["simchain-network-agent"]

# ---- simchainctl -----------------------------------------------------------
# Thin HTTP client; deliberately contains neither Docker CLI nor Bitcoin RPC
# orchestration logic.
FROM gcr.io/distroless/cc-debian12:nonroot AS simchainctl
COPY --from=builder /app/target/release/simchainctl /simchainctl
ENTRYPOINT ["/simchainctl"]
