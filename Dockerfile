# Stage 1: build the release binary.
FROM rust:1-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# Stage 2: minimal runtime.
FROM debian:bookworm-slim
# ca-certificates: rustls needs the system CA store for HTTPS to providers.
# curl: used by the compose healthcheck.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --create-home app
COPY --from=builder /build/target/release/big-brother /usr/local/bin/big-brother
USER app
EXPOSE 8787
CMD ["big-brother", "/app/config/config.toml"]
