# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, breaking changes bump the **minor** version and
everything else bumps the **patch** version.

## [Unreleased]

### Added
- Dashboard: **Settings page** — the payout address can now be changed at
  runtime from the dashboard. The network is **detected from the connected
  node** (`getblockchaininfo`) and shown read-only; a change is validated
  (the address must parse and belong to the node's chain), persisted in the
  stats SQLite (overriding config.toml at next boot), and applied immediately
  via a forced clean-job broadcast so connected miners switch payout without
  waiting for the next block. New config keys: `pool.network` (optional
  assertion — boot fails fast if set and the node reports a different chain)
  and `metrics.allow_runtime_settings` (default true; set false on an
  untrusted LAN, or front the dashboard with an authenticating proxy).

### Security
- Mining is **paused unless the payout address validates against the node's
  chain**. A wrong-network address (e.g. a `tb1…` testnet address while the
  node is on mainnet) still encodes a perfectly valid coinbase script, so the
  pool would previously have mined to a script the operator may not control;
  now no job is built until a valid address is saved (the pool still boots and
  the dashboard stays reachable to fix it — which also gives container
  platforms a clean first-run flow with a placeholder address).
- Dashboard: redesigned **console layout** — fixed left rail with scroll-spy
  navigation and status footer, hero hashrate zone with block odds, hairline
  KPI strip, and a non-mainnet network badge. Two color themes — carbon
  (near-black, amber accent; default) and Swiss light (porcelain, cobalt
  accent) — toggleable from the rail, seeded from the OS preference, and
  persisted per browser. The hashrate chart re-skins from the active theme's
  CSS variables without a server round trip.

## [0.4.2] - 2026-06-12

### Fixed
- **Critical: every found block was rejected by the node with
  `prev-blk-not-found`.** The coinbase/`mining.notify` prev-hash conversion
  applied a per-4-byte-word swap that `build_header` then undid, leaving the
  block header's `hashPrevBlock` in Bitcoin Core's *display* byte order instead
  of internal order. The header still produced valid proof-of-work, and share
  validation reconstructs the same header, so the bug was invisible at the
  share level — but no node would accept the assembled block, so a real
  block-find would have been archived to disk yet **rejected by the network and
  lost**. `stratum_prev_hash` now reverses the full 32 bytes (display →
  internal) before the per-word swap, yielding the canonical Stratum format;
  `build_header` recovers the correct internal bytes. Verified end to end on
  regtest: the pool now finds a block, submits it, and the node accepts it onto
  the chain (coinbase paying the configured address). The previous round-trip
  unit test only checked that the output changed; it now asserts the recovered
  header bytes equal the block's true internal prev-hash.
- BIP34 coinbase height encoding now matches Bitcoin Core's
  `CScript() << nHeight` for all heights: `OP_0` for 0 and `OP_1..OP_16` for
  heights 1–16, instead of always using the data-push form. The pool only ever
  emits these small-height encodings when mining the first 16 blocks of a fresh
  regtest / signet chain, where the old encoding was rejected with
  `bad-cb-height`; post-BIP34 mainnet heights are all >16 and were unaffected.
  Found while rehearsing the block-submission path on regtest.

## [0.4.1] - 2026-06-11

### Added
- Config: every value can now be overridden by an environment variable named
  `SOLO_POOL_<SECTION>__<KEY>` (double underscore between section and key),
  e.g. `SOLO_POOL_BITCOIN_RPC__URL`. Overrides are typed after the key in the
  config file (load fails loudly on a type mismatch); for keys absent from the
  file, booleans/numbers are inferred and surrounding double quotes force a
  string. Designed for container platforms (Umbrel, Start9, Compose) that
  configure apps through the environment.
- Packaging: the GHCR image is now multi-arch — `linux/amd64` + `linux/arm64`
  (each built on a native runner and merged into one manifest list), so the
  pool runs on Raspberry Pi–class hosts and arm64 servers. Release tarballs
  now include an `aarch64-unknown-linux-gnu` build alongside x86_64.

### Fixed
- Repeated `mining.authorize` calls on one connection no longer inflate the
  dashboard's per-worker `active_sessions` count (ghost-online accounting);
  re-authorizing the same name is now a no-op, and switching names releases
  the previous one.

### Security
- **Pre-auth DoS hardening** (the two High items from the June 2026 review):
  - A connection now has 10 seconds (was: the full 300 s idle timeout) to make
    protocol progress before authorizing a worker — covering the protocol
    auto-detect peek, the SV2 Noise handshake, and every pre-auth message — so
    silent connections can no longer pin the bounded global connection slots.
  - Worker-identity cardinality is now capped: one connection may authorize at
    most `security.max_authorizations_per_session` distinct names (default 8,
    0 disables; new config key). The per-session token bucket
    (`max_shares_per_sec`) now applies to **all** inbound messages, not just
    share submits, so authorize/configure floods are rate-limited too.
  - Offline workers idle > 24 h are evicted from the in-memory stats maps, and
    Prometheus series untouched for 24 h are dropped from the exporter, so
    attacker-minted worker names no longer leak memory or label series forever.
  - The persisted `worker_best_shares` table is bounded to the top 512 rows by
    difficulty (pruned at boot and periodically); an inflated table from an
    earlier run is trimmed before being loaded into RAM.

## [0.4.0] - 2026-06-11

### Changed
- **Packaging (Docker): the runtime image now runs as a non-root user
  (uid:gid `10001`, `solo-pool`) instead of root.** This is a breaking change
  for existing Docker / Compose deployers and needs two one-time migration
  steps:
  - Grant the container read access to bitcoind's cookie via the host node
    group — `docker run --group-add <gid>` or Compose `group_add` — and set
    `rpccookieperms=group` in `bitcoin.conf`.
  - `chown -R 10001:10001` any persisted `./data` volume so the SQLite stats DB
    and found-block archives stay writable.

  The default in-container cookie path also moves from `/root/.bitcoin/.cookie`
  to `/home/solo-pool/.bitcoin/.cookie`. The binary is byte-identical and needs
  no root (the stratum and dashboard ports are both >1024); only the image
  contract changes. The systemd unit already ran unprivileged and is unaffected.

### Fixed
- Packaging (Docker): the runtime image was missing `libsqlite3.so.0`, which
  `rusqlite` links — the binary failed at dynamic-link time before `main()`, so
  the published images never actually started. (Bare-metal/systemd installs were
  unaffected, linking the host's library.) The runtime stage now installs
  `libsqlite3-0` alongside `libzmq5`.
- Config: reject `extranonce1_size + extranonce2_size == 0` at startup. A zero
  total extranonce width underflowed the `usize` subtraction in SV2
  `OpenExtendedMiningChannel` channel setup; it now fails fast at load with a
  clear message instead of risking a runtime panic.

### Security
- Network: enforce `max_message_bytes` on the newline-terminated path of the
  SV1 line reader. A line whose terminating newline fell within a single read
  buffer (~8 KiB) was returned without the size check, so the configured limit
  could be exceeded; the cap is now applied on that path too.
- Metrics: bound the `pool_block_submissions_failed_total` `reason` label to a
  small fixed set of values instead of the `Debug`-formatted error. The error
  text carries node-influenced strings (RPC messages, rejection reasons) that
  would otherwise mint unbounded Prometheus label series.

## [0.3.2] - 2026-06-10

### Added
- Config: `pool.found_block_dir` (default `found-blocks`) — directory where the
  raw hex of every found block is archived before submission.

### Fixed
- Mining: a found block can no longer be silently lost when `submitblock`
  fails. The block hex is archived to disk before the first attempt, transient
  RPC failures are retried in-line and then by a background task (for up to two
  hours), and `duplicate`/`inconclusive` node responses are treated as success
  so retries are idempotent. Submission now also runs via `spawn_blocking`
  instead of blocking the session task on the synchronous RPC client.
- Network: a transient `accept()` error (e.g. `ECONNABORTED` from a peer reset
  mid-handshake, or `EMFILE` under fd exhaustion) no longer terminates the
  listener — and with it the whole pool. The accept loop now logs, backs off
  briefly, and continues.
- Security: the connection rate limiter no longer panics on `Instant`
  underflow when the pool starts within 60 seconds of host boot — previously
  this could kill the background pruner task (leaving the per-IP window map
  growing forever) or crash the accept loop on an incoming connection.

## [0.3.1] - 2026-06-09

### Added
- Config: `security.max_worker_name_len` (default 128) caps the accepted worker
  / SV2 user-identity name length.
- Dashboard: footer showing the running build version.

### Changed
- Packaging (systemd): the unit now waits for bitcoind's RPC to be ready before
  starting, via an `ExecStartPre` probe that loops `getblockchaininfo` until it
  succeeds (`After=`/`Wants=bitcoind.service`, `TimeoutStartSec` raised). This
  prevents the pool from exiting on a transient `getblocktemplate` failure when
  it starts against a node that is still warming up (RPC `-28`).
- Packaging (systemd): the readiness probe passes `-datadir` to `bitcoin-cli` so
  it does not read `~/.bitcoin`, which `ProtectHome` blocks (it would otherwise
  abort before reaching the RPC call).
- Packaging (systemd): moved `StartLimitIntervalSec`/`StartLimitBurst` from
  `[Service]` to `[Unit]`, where systemd actually honors them (they were
  silently ignored, producing a recurring journal warning).

### Fixed
- Logging: ANSI color escape sequences are no longer written to file logs
  (`log_dir`). `with_ansi(false)` was applied only to the stdout appenders, so
  on-disk logs at e.g. `/var/log/solo-pool-rs/` were cluttered with escape
  codes. Now disabled for both the JSON and plain file appenders.

### Security
- Network: bounded the SV1 line reader so an unauthenticated peer can no longer
  stream an endless newline-free line and exhaust memory before the size check —
  `max_message_bytes` is now enforced *during* the read.
- Network: validate worker / SV2 user-identity names (non-empty, length-capped,
  no control or whitespace characters) at `mining.authorize` /
  `OpenExtendedMiningChannel`. Bounds the per-worker stats maps and Prometheus
  label cardinality against untrusted input and blocks control-character
  injection into logs, metrics, and the dashboard.
- SV2: reject frames whose declared size exceeds the message limit before
  allocating and decrypting the body (previously up to ~16 MB per frame versus
  the 4 KB policy intent).
- Network: prune the per-IP connection-rate-limiter map so it cannot grow
  unbounded across spoofed / IPv6 source addresses.
- Network: bound the SV1 protocol-detect peek and the SV2 Noise handshake with
  the idle timeout, preventing slowloris connection-holding before the session
  loop's idle timeout engages.

## [0.3.0] - 2026-06-06

### Added
- Packaging: CI workflow, Docker image, and a systemd service unit.
- Dashboard: network hash rate and difficulty-change stats.
- Dashboard: embedded favicon, served at `/favicon.ico`.

### Fixed
- Jobs: seed the job-id high bits per process so jobs are not mislabeled as
  stale after a restart.

### Documentation
- Added a real security policy (supported versions, private reporting, scope).
- Clarified the crate description: Stratum V2 is implemented, not just planned.

## [0.2.0] - 2026-05-31

### Added
- Stratum V2 (Noise-encrypted) support via dual-stack SV1 + SV2 auto-detection
  on a single port.
- All-time and session-best hash rate tracking in stats and on the dashboard.
- Link from the dashboard to the raw `/metrics` endpoint.

### Changed
- Dashboard rework: worker rendering and stats mapping fixes; reject rate moved
  into the rejected card; best share keyed by vardiff difficulty.

[Unreleased]: https://github.com/cbyam/solo-pool-rs/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/cbyam/solo-pool-rs/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/cbyam/solo-pool-rs/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cbyam/solo-pool-rs/releases/tag/v0.2.0
