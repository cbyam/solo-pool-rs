# ── Builder ──────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder

# tmq → zmq → libzmq: needed to build and link.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libzmq3-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Pre-build dependencies against a stub main so `cargo build` is cached across
# source-only changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Build the real binary.
COPY src ./src
RUN touch src/main.rs \
    && cargo build --release \
    && strip target/release/solo-pool-rs

# ── Runtime ──────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        libzmq5 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/solo-pool-rs /usr/local/bin/solo-pool-rs

WORKDIR /app
# So a `~/.bitcoin/.cookie` cookie_path (the Bitcoin default) expands to
# /root/.bitcoin/.cookie inside the container — mount your host cookie there.
ENV HOME=/root

# Stratum (SV1 + SV2) and the dashboard/metrics HTTP port.
EXPOSE 3333 9090

# Mount your config at /app/config.toml (see docker-compose.yml).
ENTRYPOINT ["solo-pool-rs"]
CMD ["/app/config.toml"]
