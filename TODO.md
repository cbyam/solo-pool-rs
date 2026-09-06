# TODO

Backlog of known work, grouped by whether it has to land before 1.0. The 1.0
release is a stability promise, not a feature milestone: it names the surface
that will not change incompatibly and starts the SemVer clock on it. Anything
that could force a breaking change to that surface belongs in the first group;
everything else can ship after 1.0 as a minor or patch release. Line references
are as of the review that raised each item and may drift.

## Before 1.0 (rc checklist)

Gates, in the order they can be closed:

- [ ] **Cut 0.6.7** with the dashboard work merged 2026-09-06 (round effort,
  persisted found blocks, node health, chart downtime gap, BIP110 card removal)
  and move the community-store pin to it.
- [ ] **Write the stable-surface doc.** Names what the promise covers: config
  file keys and their meaning, Stratum V1/V2 behaviour on the wire, the compose
  and environment contract Umbrel depends on, the stats database migrating
  forward across versions, and the `/stats` and `/api/*` JSON the dashboard
  reads. Names what it excludes: dashboard layout and copy, log text, metric
  names, internal module structure. Records the deferred stances (TLS,
  fees/multi-coin/cloud non-goals, RPC failover) so no pending decision can
  force a config break later. Ships as the headline of the rc.
- [ ] **Submit the official Umbrel app-store PR.** Test install on real Umbrel
  hardware, produce the 1440×900 gallery PNGs (the 256 SVG icon exists), open
  the PR against getumbrel/umbrel-apps. Review time is outside our control, so
  this goes in before the rc, not after.
- [ ] **Decide the RPC failover config shape, without building it.** Today a
  single `bitcoin_rpc.url`. If failover ever lands as a list of endpoints, that
  is a config-shape change. Either commit in the stable-surface doc that a
  future list key sits beside `url` and `url` keeps working, or defer the
  feature there in writing. See the feature entry under "After 1.0".
- [ ] **Fail loudly when the stats store will not open.** A locked file or
  transient I/O error at boot leaves `store = None` behind a single `warn!`.
  The database now holds the found-block list and the running round as well as
  the all-time bests, so a silent open failure means the dashboard forgets a
  found block on the next restart with no visible signal. Make it a boot error
  or a red pill on the dashboard, not a log line.
- [ ] **Prod soak.** Two months of continuous mainnet mining with no
  correctness bug on the share or block path, started 2026-09-05 on 0.6.6.
  Patch-day restarts do not reset it; a change to share validation or block
  submission does. Earliest completion around 2026-11-05.
- [ ] **Tag v1.0.0-rc.1** once the doc ships and the Umbrel PR is open;
  **1.0.0** after store acceptance and a quiet end to the soak.

Explicitly not a gate: feature completeness. Fees, multi-coin and cloud are out
of scope by decision, and 1.0 puts that in writing.

## After 1.0 (no promised surface changes)

### Correctness and hardening

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
- [ ] **Absolute pre-auth deadline.** The 10 s handshake deadline re-anchors on
  every inbound line, including the blank keepalives the loop tolerates, so a
  client sending one newline every 9 s holds a bounded global connection slot
  indefinitely without ever authorizing. Add a deadline that does not move.
- [ ] Scope the Prometheus `idle_timeout(MetricKindMask::ALL, 24h)` to the
  worker-labelled series. It currently expires rarely-written globals too:
  `pool_connected_miners` disappears when one miner stays connected without
  churn for 24 h, and `pool_blocks_found_total` vanishes (and restarts from 0)
  24 h after a block, while the dashboard count, now persisted, does not.
- [ ] The SV1 rejection for a wrong-length extranonce2 reaches the miner as
  `[20, "Unknown problem", null]`, because `PoolError::BadExtranonceSize` is not
  mapped in `to_stratum_error` and falls through to the generic arm. Operator
  signal is fine (clear log line, `bad_extranonce` metric label), but the point
  of that guard was diagnosability for mis-sized firmware, and the miner's own
  log currently says nothing useful. Stratum V1 has no dedicated code, so keep
  code 20 and send a descriptive message.

### Performance

- [ ] **Move remaining blocking I/O off the async runtime.** `submit_block` is
  done (PR #6); the ZMQ tip poll and the network-stats RPCs are done (v0.6.4).
  Still direct: `getblocktemplate` in `TemplateEngine::refresh`
  (`engine.rs:210`), SQLite best-share writes on the share-accept path (behind a
  sync mutex, and one of them holds a DashMap write guard across the fsync:
  funnel through a dedicated writer thread, enable WAL + `synchronous=NORMAL`),
  and the dashboard `/history` + `/chart` SQLite scans (contend with the share
  path on the same connection mutex; wrap in `spawn_blocking`).
- [ ] Hot-path cleanups: recompute hashrate windows only on accepted shares
  (today: 4 full deque scans per inbound message); move per-share hex/format
  allocations inside `debug!` so they're skipped when disabled; reuse a scratch
  buffer instead of cloning `coinbase_template` per share.
- [ ] Consider whether `JOB_HISTORY_DEPTH` (8, about four minutes at the ntime
  cadence) should grow. Deliberately left alone in v0.6.4: each entry pins a
  whole `StratumJob` including the raw transaction data, so on mainnet this is
  megabytes per entry and tripling it would cost 50-100 MB on single-board
  hardware. The disconnects that motivated it came from the accounting, which is
  fixed, so this is now a rejected-share-rate question rather than a correctness
  one.

### Upkeep

- [ ] Keep the E2E node matrix current. It pins the deployed Knots build and a
  recent Core. The RDTS-mandatory Knots entry was dropped after the August 2026
  split (PR #49); re-add a candidate entry when a deployable Knots release
  without mandatory RDTS exists, so the matrix keeps answering "does this still
  work after the next upgrade" rather than only "did it work before the last
  one".
- [ ] Dependency refresh when convenient: `rusqlite` 0.29 and
  `metrics-exporter-prometheus` 0.15 are a few majors behind (no advisories,
  just aging). The metrics trio (`metrics`, `metrics-exporter-prometheus`,
  `metrics-util`) must move together, and the bump needs a Zlib licence allow
  in `deny.toml` for `foldhash`.

### Features, deferred by decision

The stance on each is recorded here and, from 1.0, in the stable-surface doc.
Build only on request.

- [ ] **Multi-node Bitcoin RPC failover.** Today a single `bitcoin_rpc.url`; if
  that node restarts (see the patch-day needrestart sweep) or crashes, template
  refresh stalls until it returns, which the dashboard's node LED now shows.
  Accept a list of node endpoints and fail over on connect error / RPC error /
  stale tip, preferring the highest-tip healthy node. Stays within the
  single-binary, zero-ops thesis (no external HA layer). The config shape is
  decided before 1.0 (see above); the feature is not.
- [ ] **SV1-over-TLS (`stratum+ssl://`).** Decision (2026-06-15): not building
  it. The target audience is the self-hosted solo crowd on a trusted LAN, where
  the value is marginal: solo mining has no account password to leak, and TLS
  would only hide the payout address and hashrate from a passive on-path
  observer. SV2 (Noise) already encrypts the modern firmware path, so this is
  purely for legacy SV1 devices over an untrusted network (a shrinking niche),
  and client support for `stratum+ssl` is spotty (cgminer/Avalon yes;
  AxeOS/ESP-Miner version-dependent). Revisit only if a real user asks for it.

  Design notes for when/if that happens, so it doesn't become a support burden:
  - **rustls / `tokio-rustls`, not OpenSSL**: keeps the pure-Rust single-binary
    and arm64/musl cross-compile story intact.
  - **Separate `tls_port`** (e.g. 3334), not the auto-detect port. The detector
    is binary (`first[0] == b'{'` → SV1, else → SV2, `server.rs`); a TLS
    ClientHello (`0x16`) lands in the "else → SV2" bucket and collides with the
    Noise handshake's arbitrary first byte, so TLS cannot share that socket.
  - The TLS listener wraps TCP, does the handshake, then feeds the decrypted
    stream into the same auto-detect + session path, so SV1 and SV2 both work
    over TLS for free. Only refactor needed: make `session::run` generic over
    `impl AsyncRead + AsyncWrite + Unpin + Send` (`tokio::io::split` instead of
    `TcpStream::into_split`).
  - **Cert UX is the real problem, not the crypto.** Default to a pool-generated
    self-signed cert (`rcgen`) written to the data dir on first boot, so the
    user manages nothing. Stratum-over-TLS clients generally don't verify the
    cert anyway (opportunistic encryption), which still defeats passive
    eavesdropping. Optional `cert_path`/`key_path` override (+ SIGHUP reload) for
    anyone wanting a CA-signed cert. Skip ACME/Let's Encrypt (needs a public
    domain + inbound reachability, impractical behind home NAT). Ship opt-in,
    off by default; document as "encryption for SV1 miners over untrusted
    networks, not needed on a trusted LAN."

## Findings disproved by testing

Two findings from the August 2026 audit were disproved and should not be
re-raised. On Linux `Instant` holds a signed timespec, so `Instant - Duration`
does not panic on underflow (verified by subtracting `u64::MAX/2` seconds); the
guards in `security/mod.rs` and `vardiff.rs` are portability hygiene for
platforms where `Instant` is an unsigned counter, not fixes for a live bug. And
dashmap 5.5.3 does not park readers behind queued writers, so a read during
iteration of the same map does not deadlock (verified with a probe that pinned a
shard guard, confirmed a writer was blocked on it, then completed the read).

## Shipped

Kept for the record; the changelog has the detail.

- [x] **Empty `log_dir` logs to stdout** (PR #90): an empty or whitespace-only
  value counts as unset, and `[logging] log_max_files` (default 14) bounds
  file retention. The systemd unit ships with `LogsDirectory=` enabled.
- [x] **Cap attacker-controlled worker-name growth**: per-session cap on
  distinct authorized identities (`max_authorizations_per_session`, default 8),
  token bucket on all inbound messages, 24 h TTL eviction of offline workers,
  `PrometheusBuilder::idle_timeout` (24 h), `worker_best_shares` bounded to the
  top 512 rows.
- [x] **Dedicated handshake timeout for protocol auto-detect**: 10 s pre-auth
  deadline covering the first-byte peek, the SV2 Noise handshake, and both
  session loops until a worker authorizes / a channel opens.
- [x] **Harden the duplicate-share set** (v0.6.0): shares recorded for dedup
  only after validation; per-session set clears on every clean-job broadcast.
- [x] **Credit background-retrier block acceptance to dashboard stats**
  (v0.6.0): retry success mirrors the inline-success stats update.
- [x] Monotonic guard on pool best-share/best-hashrate SQLite `UPDATE`s
  (v0.6.0); best-hashrate in-memory update is a CAS.
- [x] Ghost-online accounting: same-name re-auth is a no-op, switching names
  releases the previous one.
- [x] **Vardiff floor credit** (PR #80): a share is credited at the threshold it
  cleared (current assignment, previous assignment, or the floor), never at
  the session difficulty and never at its hash difficulty. This closed the
  "how to credit shares accepted at the floor" design question.
- [x] **Deliberate deploys**: `packaging/install.sh` copies the binary to
  `/usr/local/lib/solo-pool-rs/<version>/` with an atomic symlink swap,
  `--rollback` and `--list`. The live host is cut over (0.6.6 active, 0.6.4 as
  rollback). Note the install directory is named from `Cargo.toml`, so installing
  an unreleased main build overwrites the current version's directory; cut a
  release first.
- [x] **v0.4.0: non-root Docker image**.
- [x] **SV2 identity pinning** (v0.6.0): persistent Noise authority key
  (`[sv2] authority_key_file`), pubkey logged at boot and shown in the Connect
  modal + `GET /api/info`; verified on a NerdQAxe++. The bitaxe/nerdqaxe
  firmware checks only the Schnorr signature, never the validity window;
  upstream enforcement-toggle PRs: bitaxeorg/ESP-Miner#1796,
  shufps/ESP-Miner-NerdQAxePlus#656.
