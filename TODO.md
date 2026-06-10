# TODO

Backlog of known work, ordered by priority. Most items come from the June 2026
full-codebase security/performance review; PR #6 fixed the top three findings
(block-loss on `submitblock` failure, fatal `accept()` errors, `Instant`
underflow panic). Line references are as of that review and may drift.

## High — pre-auth DoS hardening

- [ ] **Cap attacker-controlled worker-name growth.** Worker names are
  length-validated but unbounded in *cardinality*: `mining.authorize` is not
  rate-limited, every distinct name permanently inserts into six stats maps and
  mints never-freed Prometheus label series, and `update_worker_hashrate` does
  an O(n) sum per inbound message (cost grows quadratically). Accepted shares
  for new names also add never-pruned `worker_best_shares` SQLite rows reloaded
  into RAM at boot. Fix set: cap authorizations per session, TTL-evict offline
  workers from the in-memory maps, rate-limit non-submit messages with the
  existing token bucket, set `PrometheusBuilder::idle_timeout`, prune stale
  SQLite rows. (`src/network/session.rs`, `src/stats.rs`, `src/metrics/mod.rs`)
- [ ] **Dedicated handshake timeout for protocol auto-detect.** The first-byte
  peek reuses `idle_timeout_secs` (300 s), so a silent connection pins one of
  the 256 global slots for 5 minutes; ~7 IPs at 8 conns/min can lock out all
  miners. Use a short 5–10 s deadline for the peek only.
  (`src/network/server.rs`, spawn block)

## Medium

- [ ] **Move remaining blocking I/O off the async runtime.** `submit_block` is
  done (PR #6); still direct: `getblocktemplate` in `TemplateEngine::refresh`,
  SQLite best-share writes on the share-accept path (behind a sync mutex —
  funnel through a dedicated writer thread, enable WAL + `synchronous=NORMAL`),
  and the dashboard `/history` + `/chart` SQLite scans (contend with the share
  path on the same connection mutex; wrap in `spawn_blocking`).
- [ ] **Harden the duplicate-share set.** 4096-entry FIFO allows bounded replay
  (evict-then-resubmit inflates share/hashrate stats); shares are also inserted
  *before* validation, so invalid shares occupy slots and later identical
  submits are misreported as `duplicate`. Scope dedup to live jobs and insert
  only after validation passes. (`src/mining/validator.rs`)
- [ ] **Credit background-retrier block acceptance to dashboard stats.** A block
  accepted by the PR #6 background retrier updates Prometheus counters but not
  the dashboard block list / pool stats (needs session context plumbing).

## Low

- [ ] Monotonic guard on pool best-share/best-hashrate SQLite `UPDATE`s
  (`WHERE ?1 > ...`), matching the per-worker variant; also make the
  best-hashrate in-memory update a CAS. (`src/stats.rs`)
- [ ] Fix ghost-online accounting: repeated `mining.authorize` increments
  `active_sessions` per call but disconnect decrements once, for the last name
  only. (`src/network/session.rs`, `src/stats.rs`)
- [ ] Enforce `max_message_bytes` on the newline-found path of
  `read_line_bounded` (currently accepts up to BufReader capacity ~8 KiB).
  (`src/network/session.rs`)
- [ ] Hot-path cleanups: recompute hashrate windows only on accepted shares
  (today: 4 full deque scans per inbound message); move per-share hex/format
  allocations inside `debug!` so they're skipped when disabled; reuse a scratch
  buffer instead of cloning `coinbase_template` per share.
- [ ] Use a small static reason enum for the `block_submission_failure` metric
  label instead of `Debug`-formatted errors (label cardinality).
  (`src/metrics/mod.rs`)
- [ ] Reject SV2 `OpenExtendedMiningChannel` when `extranonce_total == 0` to
  avoid a `usize` underflow on misconfigured extranonce sizes.
  (`src/protocol/sv2/mod.rs`)

## Planned features

- [ ] **v0.4.0: non-root Docker image** (upgrade-breaking; deliberately deferred
  from v0.3.1).
- [ ] **SV2 identity pinning:** optional persistent Noise authority keypair via
  config instead of the per-process ephemeral key (deferred in the
  `protocol/sv2/noise.rs` docstring; today no miner verifies pool identity).
- [ ] Dependency refresh when convenient: `rusqlite` 0.29 and
  `metrics-exporter-prometheus` 0.15 are a few majors behind (no advisories,
  just aging).
