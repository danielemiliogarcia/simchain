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
# The opt-in orchestrator invokes compose and repo helper scripts through the
# host Docker socket, so its runtime includes the Docker CLI and Compose v2.
FROM debian:trixie-slim AS scenario-engine
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates docker-cli docker-compose \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/simchain-scenario-engine /usr/local/bin/simchain-scenario-engine
ENTRYPOINT ["simchain-scenario-engine"]

# ---- control-plane ---------------------------------------------------------
# The transitional control-plane backend rewrites .env and recreates tool
# services through the host Docker socket, so like the scenario engine its
# runtime includes the Docker CLI and Compose v2 (Debian base: the builder
# links against glibc, so an Alpine/docker:cli base would not run it).
FROM debian:trixie-slim AS control-plane
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates docker-cli docker-compose \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/simchain-control-plane /usr/local/bin/simchain-control-plane
ENTRYPOINT ["simchain-control-plane"]

# ---- simchainctl -----------------------------------------------------------
# Thin HTTP client; deliberately contains neither Docker CLI nor Bitcoin RPC
# orchestration logic.
FROM gcr.io/distroless/cc-debian12:nonroot AS simchainctl
COPY --from=builder /app/target/release/simchainctl /simchainctl
ENTRYPOINT ["/simchainctl"]
