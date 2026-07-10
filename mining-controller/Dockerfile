FROM rust:1-slim-trixie as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /app/target/release/simchain-mining-controller /simchain-mining-controller
ENTRYPOINT ["/simchain-mining-controller"]
