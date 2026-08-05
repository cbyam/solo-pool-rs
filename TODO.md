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
  done (PR #6); the ZMQ tip poll and the network-stats RPCs are done (v0.6.4).
  Still direct: `getblocktemplate` in `TemplateEngine::refresh`
  (`engine.rs:210`), SQLite best-share writes on the share-accept path (behind a
  sync mutex, and one of them holds a DashMap write guard across the fsync:
  funnel through a dedicated writer thread, enable WAL + `synchronous=NORMAL`),
  and the dashboard `/history` + `/chart` SQLite scans (contend with the share
  path on the same connection mutex; wrap in `spawn_blocking`).
- [x] **Harden the duplicate-share set** (shipped in v0.6.0, 2026-07-02):
  shares are recorded for dedup only after validation passes, and the
  per-session set clears on every clean-job broadcast (live-jobs scoping); the
  4096 FIFO cap remains as a memory backstop only.
- [x] **Credit background-retrier block acceptance to dashboard stats**
  (shipped in v0.6.0, 2026-07-02): worker + `PoolStats` are threaded through
  `submit_found_block` into the resubmit task; retry success now mirrors the
  inline-success stats update.

## Low

- [x] Monotonic guard on pool best-share/best-hashrate SQLite `UPDATE`s
  (`WHERE ?1 > ...`), matching the per-worker variant; best-hashrate in-memory
  update is now a CAS. (shipped in v0.6.0, 2026-07-02)
- [x] Fix ghost-online accounting: repeated `mining.authorize` increments
  `active_sessions` per call but disconnect decrements once, for the last name
  only. (Fixed alongside the authorization cap: same-name re-auth is a no-op,
  switching names releases the previous one.)
- [ ] Hot-path cleanups: recompute hashrate windows only on accepted shares
  (today: 4 full deque scans per inbound message); move per-share hex/format
  allocations inside `debug!` so they're skipped when disabled; reuse a scratch
  buffer instead of cloning `coinbase_template` per share.

## From the August 2026 audit (remainder after v0.6.4)

v0.6.4 shipped the findings that could lose a block, lose a share, or stop the
pool. What is left is listed here. Line references are as of that audit.

Two findings from that audit were **disproved** by testing and should not be
re-raised. On Linux `Instant` holds a signed timespec, so `Instant - Duration`
does not panic on underflow (verified by subtracting `u64::MAX/2` seconds); the
guards in `security/mod.rs` and `vardiff.rs` are portability hygiene for
platforms where `Instant` is an unsigned counter, not fixes for a live bug. And
dashmap 5.5.3 does not park readers behind queued writers, so a read during
iteration of the same map does not deadlock (verified with a probe that pinned a
shard guard, confirmed a writer was blocked on it, then completed the read).

### Medium

- [ ] **SV2 spec conformance set.** Each is small on its own; group them and
  test against the NerdQAxe++ before merging, since they touch the handshake and
  channel-open path that hardware actually uses.
  - `OpenExtendedMiningChannel.max_target` is used once to clamp the initial
    target and then discarded, so later vardiff `SetTarget` messages can hand a
    device an easier target than it declared it would accept. Store it on the
    session and clamp every retarget. The open-time clamp also adjusts the wire
    target without adjusting `session.difficulty`, so the first retarget derives
    from a value the device was never assigned.
  - `SetupConnection` rejects only `min_version > 2`; a client offering
    `max_version = 1` gets `SetupConnectionSuccess { used_version: 1 }` and then
    is spoken to in v2. Add the `max_version < 2` rejection.
  - The Noise decoder resizes its buffer to the attacker-declared frame size
    before the `max_frame` check runs, so a peer can force a ~16 MB allocation
    per connection before being dropped. Compare `writable_len()` first.
  - A repeat `OpenExtendedMiningChannel` on an open channel re-allocates the
    channel id and extranonce prefix without closing the previous one, leaving
    in-flight shares validating against a prefix the device no longer has.
  - `SubmitSharesExtended.channel_id` is decoded and ignored. Harmless while a
    session serves one channel, but it stops being harmless combined with the
    item above.
- [ ] **Decide how to credit shares accepted at the vardiff floor.** Shares are
  validated against `min_difficulty` (the Avalon/cgminer accommodation) but
  credited at `session.difficulty`. For firmware that ignores `set_difficulty`
  this ratchets difficulty upward and inflates hashrate. Crediting the floor
  instead would undercount every compliant miner by the same ratio, so neither
  constant is right: the effective threshold has to be inferred from observed
  share difficulties. This is a design decision, not a patch.
- [ ] **Absolute pre-auth deadline.** The 10 s handshake deadline re-anchors on
  every inbound line, including the blank keepalives the loop tolerates, so a
  client sending one newline every 9 s holds a bounded global connection slot
  indefinitely without ever authorizing. Add a deadline that does not move.
- [ ] **Fail loudly when the stats store will not open.** A locked file or
  transient I/O error at boot leaves `store = None` behind a single `warn!`, and
  every all-time best earned during that run is lost at the next restart with no
  dashboard-visible signal. Worker best shares are required to survive restarts,
  so degrading silently is the wrong default.

### Low

- [ ] Scope the Prometheus `idle_timeout(MetricKindMask::ALL, 24h)` to the
  worker-labelled series. It currently expires rarely-written globals too:
  `pool_connected_miners` disappears when one miner stays connected without
  churn for 24 h, and `pool_blocks_found_total` vanishes (and restarts from 0)
  24 h after a block.
- [ ] The SV1 rejection for a wrong-length extranonce2 reaches the miner as
  `[20, "Unknown problem", null]`, because `PoolError::BadExtranonceSize` is not
  mapped in `to_stratum_error` and falls through to the generic arm. Operator
  signal is fine (clear log line, `bad_extranonce` metric label), but the point
  of that guard was diagnosability for mis-sized firmware, and the miner's own
  log currently says nothing useful. Stratum V1 has no dedicated code, so keep
  code 20 and send a descriptive message.
- [ ] `log_dir = ""` writes a rotating log file into the working directory
  instead of logging to stdout as the config comment promises, because an empty
  string is `Some("")` rather than `None` (`main.rs:207`). Treat empty or
  whitespace-only as unset.
- [ ] Consider whether `JOB_HISTORY_DEPTH` (8, about four minutes at the ntime
  cadence) should grow. Deliberately left alone in v0.6.4: each entry pins a
  whole `StratumJob` including the raw transaction data, so on mainnet this is
  megabytes per entry and tripling it would cost 50-100 MB on single-board
  hardware. The disconnects that motivated it came from the accounting, which is
  fixed, so this is now a rejected-share-rate question rather than a correctness
  one.

### Deployment

- [ ] **Make deploys deliberate.** `/usr/local/bin/solo-pool-rs` is a symlink
  into `target/release/`, so any `cargo build` in the working tree changes what
  the service runs on its next restart, with no version pinning and no rollback.
  A validation build during the v0.6.4 work armed an unintended deploy this way.
  `packaging/install.sh` now does the right thing (copy to
  `/usr/local/lib/solo-pool-rs/<version>/`, atomic symlink swap, `--rollback`
  and `--list`), but the live host is still on the old symlink. Cut over with:

      cargo build --release && sudo packaging/install.sh && systemctl restart solo-pool-rs

  Until that runs, treat any `cargo build` in this tree as arming a deploy. The
  scheduled local E2E already avoids it by building into a private
  `CARGO_TARGET_DIR`; do not remove that override before the cutover.

## Planned features

- [x] **v0.4.0: non-root Docker image** (shipped in v0.4.0, 2026-06-11).
- [x] **SV2 identity pinning** (shipped in v0.6.0, 2026-07-02): persistent
  Noise authority key (`[sv2] authority_key_file`, cookie-style
  create-on-first-start), pubkey logged at boot + shown in the dashboard
  Connect modal + `GET /api/info`; `persist_authority_key = false` opts out,
  `cert_validity_secs` configurable. Verified on a NerdQAxe++: pinned key
  verifies and mines, wrong key rejected. Note: the bitaxe/nerdqaxe firmware
  checks only the Schnorr signature, never the validity window (no wall
  clock); upstream enforcement-toggle PRs: bitaxeorg/ESP-Miner#1796,
  shufps/ESP-Miner-NerdQAxePlus#656.
- [ ] **SV1-over-TLS (`stratum+ssl://`) — DEFERRED, build only on request.**
  Decision (2026-06-15): not building it. The target audience is the
  self-hosted *solo* crowd on a trusted LAN, where the value is marginal — solo
  mining has no account password to leak; TLS would only hide the payout address
  and hashrate from a passive on-path observer. SV2 (Noise) already encrypts the
  modern firmware path, so this is purely for legacy SV1 devices over an
  untrusted network (a shrinking niche), and client support for `stratum+ssl` is
  spotty (cgminer/Avalon yes; AxeOS/ESP-Miner version-dependent). Revisit only
  if a real user asks for it.

  Design notes for when/if that happens, so it doesn't become a support burden:
  - **rustls / `tokio-rustls`, not OpenSSL** — keeps the pure-Rust single-binary
    and arm64/musl cross-compile story intact.
  - **Separate `tls_port`** (e.g. 3334), *not* the auto-detect port. The detector
    is binary (`first[0] == b'{'` → SV1, else → SV2, `server.rs`); a TLS
    ClientHello (`0x16`) lands in the "else → SV2" bucket and collides with the
    Noise handshake's arbitrary first byte, so TLS cannot share that socket.
  - The TLS listener wraps TCP, does the handshake, then feeds the **decrypted**
    stream into the *same* auto-detect + session path — so SV1 and SV2 both work
    over TLS for free. Only refactor needed: make `session::run` generic over
    `impl AsyncRead + AsyncWrite + Unpin + Send` (`tokio::io::split` instead of
    `TcpStream::into_split`).
  - **Cert UX is the real problem, not the crypto.** Default to a pool-generated
    **self-signed cert** (`rcgen`) written to the data dir on first boot, so the
    user manages nothing — Stratum-over-TLS clients generally don't verify the
    cert anyway (opportunistic encryption), which still defeats passive
    eavesdropping. Optional `cert_path`/`key_path` override (+ SIGHUP reload) for
    anyone wanting a CA-signed cert. Skip ACME/Let's Encrypt (needs a public
    domain + inbound reachability — impractical behind home NAT). Ship opt-in,
    off by default; document as "encryption for SV1 miners over untrusted
    networks, not needed on a trusted LAN."
- [ ] **Multi-node Bitcoin RPC failover.** Today a single `bitcoin_rpc.url`; if
  that node restarts (see the near-daily needrestart sweep) or crashes, template
  refresh stalls until it returns. Accept a list of node endpoints and fail over
  on connect error / RPC error / stale tip, preferring the highest-tip healthy
  node. Stays within the single-binary, zero-ops thesis (no external HA layer).
- [ ] Dependency refresh when convenient: `rusqlite` 0.29 and
  `metrics-exporter-prometheus` 0.15 are a few majors behind (no advisories,
  just aging).
