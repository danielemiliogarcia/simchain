FROM rust:latest
WORKDIR /app
COPY . .
RUN cargo build --release

# TODO use a builder and copy only the binary into an slim container

ENTRYPOINT ["/app/target/release/simchain-spammer"]
