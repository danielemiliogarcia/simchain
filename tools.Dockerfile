# Single build for all three simchain Rust tools. One builder stage compiles
# the whole workspace against the committed Cargo.lock, then three tiny final
# stages each copy out one release binary. Compose selects which binary an
# image contains via `target:` in docker-compose.yml.
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
