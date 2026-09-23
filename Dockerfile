# Both stages are pinned by the multi-arch index digest, so a build is
# reproducible and a base image update arrives as a reviewable Dependabot PR
# (see .github/dependabot.yml) rather than silently on the next rebuild. Keep
# the builder and runtime on the same Debian release: the binary links
# against the builder's glibc, libstdc++ and libsqlite3.

# ── Builder ──────────────────────────────────────────────────────────────────
FROM rust:1-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546 AS builder

# rusqlite links the system SQLite, found through pkg-config. No system libzmq
# is needed: zmq-sys compiles libzmq from source and links it statically.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libsqlite3-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Pre-build dependencies against stub sources so `cargo build` is cached
# across source-only changes. The package has a library target as well as
# the binary, so both files must exist or Cargo stops at target resolution.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && : > src/lib.rs \
    && cargo build --release \
    && rm -rf src

# Build the real binary.
COPY src ./src
RUN touch src/main.rs src/lib.rs \
    && cargo build --release \
    && strip target/release/solo-pool-rs

# ── Runtime ──────────────────────────────────────────────────────────────────
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a

# Runtime shared libs the binary links: libsqlite3 (rusqlite stats DB) and
# libstdc++ (the libzmq that zmq-sys builds into the binary is C++). Without
# either the dynamic linker fails before main(). libzmq itself is not
# installed: nothing links it, and an unused package still collects
# advisories.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libsqlite3-0 libstdc++6 ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Run as a dedicated unprivileged user — the binary needs no root: the stratum
# and dashboard ports are >1024, and bitcoind's RPC cookie is read via a
# supplementary group supplied at run time (see docker-compose.yml / README).
# uid/gid 10001 is fixed so a host-side `chown` of the data volume stays valid
# across image rebuilds.
RUN groupadd --gid 10001 solo-pool \
    && useradd --uid 10001 --gid 10001 --create-home --home-dir /home/solo-pool \
        --shell /usr/sbin/nologin solo-pool

COPY --from=builder /build/target/release/solo-pool-rs /usr/local/bin/solo-pool-rs

# /app is the working dir, so relative paths in config.toml (stats_db_path,
# found_block_dir) resolve under it; /app/data is the persistence mount point.
# Both are owned by the runtime user so the SQLite DB and found-block archives
# are writable.
RUN mkdir -p /app/data && chown -R solo-pool:solo-pool /app
WORKDIR /app

# The default `~/.bitcoin/.cookie` cookie_path now expands under the non-root
# home — mount your host cookie at /home/solo-pool/.bitcoin/.cookie.
ENV HOME=/home/solo-pool

USER solo-pool

# Stratum (SV1 + SV2) and the dashboard/metrics HTTP port.
EXPOSE 3333 9090

# Mount your config at /app/config.toml (see docker-compose.yml).
ENTRYPOINT ["solo-pool-rs"]
CMD ["/app/config.toml"]
