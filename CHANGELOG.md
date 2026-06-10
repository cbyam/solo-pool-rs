# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, breaking changes bump the **minor** version and
everything else bumps the **patch** version.

## [Unreleased]

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

[Unreleased]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cbyam/solo-pool-rs/releases/tag/v0.2.0
