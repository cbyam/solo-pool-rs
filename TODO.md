# TODO

Backlog of known work, ordered by priority. Most items come from the June 2026
full-codebase security/performance review; PR #6 fixed the top three findings
(block-loss on `submitblock` failure, fatal `accept()` errors, `Instant`
underflow panic). Line references are as of that review and may drift.

## High — pre-auth DoS hardening

- [x] **Cap attacker-controlled worker-name growth.** Fixed: per-session cap on
  distinct authorized identities (`max_authorizations_per_session`, default 8),
  token bucket extended to all inbound messages, 24 h TTL eviction of offline
  workers from the in-memory maps, `PrometheusBuilder::idle_timeout` (24 h),
  and `worker_best_shares` bounded to the top 512 rows (pruned at boot +
  periodically).
- [x] **Dedicated handshake timeout for protocol auto-detect.** Fixed with a
  10 s pre-auth deadline covering the first-byte peek, the SV2 Noise handshake,
  and both session loops until a worker authorizes / a channel opens.

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
- [x] Fix ghost-online accounting: repeated `mining.authorize` increments
  `active_sessions` per call but disconnect decrements once, for the last name
  only. (Fixed alongside the authorization cap: same-name re-auth is a no-op,
  switching names releases the previous one.)
- [ ] Hot-path cleanups: recompute hashrate windows only on accepted shares
  (today: 4 full deque scans per inbound message); move per-share hex/format
  allocations inside `debug!` so they're skipped when disabled; reuse a scratch
  buffer instead of cloning `coinbase_template` per share.

## Planned features

- [x] **v0.4.0: non-root Docker image** (shipped in v0.4.0, 2026-06-11).
- [ ] **SV2 identity pinning:** optional persistent Noise authority keypair via
  config instead of the per-process ephemeral key (deferred in the
  `protocol/sv2/noise.rs` docstring; today no miner verifies pool identity).
- [ ] **SV1-over-TLS (`stratum+ssl://`).** SV2 is Noise-encrypted but the SV1
  path is still plaintext — credentials/work travel in the clear on untrusted
  LANs/WANs. Add an optional TLS listener (config-supplied cert/key, hot-reload
  on SIGHUP) so legacy ASICs can connect encrypted without a stunnel sidecar.
  Keep it on a separate port from the auto-detect listener (TLS ClientHello vs.
  the SV1/SV2 first-byte peek don't co-exist cleanly on one socket).
- [ ] **Multi-node Bitcoin RPC failover.** Today a single `bitcoin_rpc.url`; if
  that node restarts (see the near-daily needrestart sweep) or crashes, template
  refresh stalls until it returns. Accept a list of node endpoints and fail over
  on connect error / RPC error / stale tip, preferring the highest-tip healthy
  node. Stays within the single-binary, zero-ops thesis (no external HA layer).
- [ ] Dependency refresh when convenient: `rusqlite` 0.29 and
  `metrics-exporter-prometheus` 0.15 are a few majors behind (no advisories,
  just aging).
