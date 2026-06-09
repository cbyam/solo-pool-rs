# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, breaking changes bump the **minor** version and
everything else bumps the **patch** version.

## [Unreleased]

### Fixed
- Logging: ANSI color escape sequences are no longer written to file logs
  (`log_dir`). `with_ansi(false)` was applied only to the stdout appenders, so
  on-disk logs at e.g. `/var/log/solo-pool-rs/` were cluttered with escape
  codes. Now disabled for both the JSON and plain file appenders.

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

[Unreleased]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cbyam/solo-pool-rs/releases/tag/v0.2.0
