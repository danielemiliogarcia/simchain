FROM rust:1-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:trixie-slim
COPY --from=builder /app/target/release/simchain-mining-controller /usr/local/bin/simchain-mining-controller
ENTRYPOINT ["simchain-mining-controller"]
