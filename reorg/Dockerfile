FROM rust:1-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:trixie-slim
COPY --from=builder /app/target/release/simchain-reorg /usr/local/bin/simchain-reorg
ENTRYPOINT ["simchain-reorg"]
