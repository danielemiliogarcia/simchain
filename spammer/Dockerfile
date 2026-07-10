FROM rust:1-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:trixie-slim
COPY --from=builder /app/target/release/simchain-spammer /usr/local/bin/simchain-spammer
ENTRYPOINT ["simchain-spammer"]
