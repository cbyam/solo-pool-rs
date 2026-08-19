# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, breaking changes bump the **minor** version and
everything else bumps the **patch** version.

## [Unreleased]

### Security
- h2 bumped from 0.4.13 to 0.4.16 for RUSTSEC-2026-0258, unbounded processing
  of empty DATA frames sent by a peer. Reached transitively through hyper,
  which serves the dashboard and metrics endpoints; axum's server accepts
  cleartext HTTP/2, so a LAN client could exercise the flaw against port 9090.
  The stratum ports do not use hyper and were never affected. Lockfile-only.

## [0.6.5] - 2026-08-11

### Changed
- The README documents node-build choice after the August 2026 BIP110/RDTS
  split: which builds follow the majority chain, why RDTS-mandatory Knots
  builds (20260508 and later) must not be deployed while the minority chain is
  stalled, and the distinction that matters for data filtering: policy shapes
  your own templates on any build, only consensus enforcement strands a node.
- The E2E matrix no longer tests the RDTS-mandatory Knots build. The entry
  existed to pre-clear a node upgrade that is now off, and the coinbase-only
  test block never exercised RDTS rules anyway. Deployed Knots and current
  Core remain.

### Security
- serde_with bumped from 3.18.0 to 3.21.0 for GHSA-7gcf-g7xr-8hxj, a panic in
  KeyValueMap serialization on empty entries. Reached transitively through
  charming; nothing in this codebase or in charming uses KeyValueMap, so there
  was no live exposure. Lockfile-only.
- anyhow bumped to 1.0.104, which ships the fix for RUSTSEC-2026-0190, and the
  ignore entries for that advisory are dropped from audit.toml and deny.toml.
  cargo audit now runs with an empty ignore list.

## [0.6.4] - 2026-08-05

### Fixed
- A chain tip that moved without a ZMQ notification no longer leaves miners
  working a dead prevhash. Two defects combined here. ZMQ's `connect` is lazy
  and returns success for an endpoint with nothing listening (wrong port, a
  node started without `-zmqpubhashblock`, a firewall), after which the
  subscription blocks forever without ever reporting an error, so the polling
  fallback could not engage in the one case it existed for. Separately, the
  periodic 30-second template refresh never compared prevhashes, so a tip
  change first noticed there went out as `clean_jobs = false`, telling miners
  they could keep their current work. Shares against the retired job kept
  validating, and a block found on it would have been rejected by the node.
  The tip comparison now lives in the refresh itself, so correct invalidation
  no longer depends on ZMQ being healthy.
- Stratum messages split across TCP segments are no longer corrupted. The line
  reader consumed bytes before the newline arrived but cleared its buffer on
  entry, and the session loop races that read against job broadcasts, so the
  read is routinely cancelled mid-line. A `mining.submit` arriving in two
  segments with a `mining.notify` in the gap lost its first fragment, failed to
  parse, and disconnected the miner. A block-winning share arriving that way
  was lost until the miner reconnected.
- SV1 now rejects a `mining.submit` whose extranonce2 is not the width the
  session was given. On a width mismatch the coinbase splice silently left the
  extranonce region zeroed, producing a coinbase the miner could predict, so
  one share mined at the difficulty floor could be replayed under unlimited
  distinct extranonce2 values: identical hash, passing target check, and a new
  duplicate-detection key every time. The SV2 path already guarded this.
  Mis-sized firmware now also gets a named error instead of a stream of
  "low difficulty" rejects ending in a disconnect.
- Share duplicate-detection keys are built from the fields validation actually
  uses. When version rolling was not negotiated the submitted version bits are
  discarded before validation, so replaying one share under version bits 1, 2,
  3 and so on produced the identical header each time and a distinct key each
  time.
- Found-block hashes are reported in the byte order `bitcoin-cli` and block
  explorers use. The found-block log, the archive filename, the submission
  logs and the dashboard all showed the reversed form, so nothing an operator
  saw during a block find would have matched the node. Submission itself was
  unaffected.
- `[bitcoin_rpc] timeout_secs` is applied. It was parsed and then never passed
  to the RPC client, so the transport default of roughly 15 seconds governed
  every call including the inline `submitblock` attempts, and tightening the
  value did nothing. Values below 5 seconds are raised to that floor with a
  warning rather than refused, so a pool that had been running on a low, inert
  value keeps starting; the floor exists because the timeout now covers block
  submission and full-block validation can take seconds.
- A panic inside a miner session no longer leaks its connection slot. The
  global connection budget was released by straight-line code after the session
  returned, so an unwinding task kept its slot forever and `max_connections`
  ratcheted downward until the pool refused every miner and only a restart
  recovered.
- A share against a job that aged out of the history window no longer counts
  toward the invalid-share disconnect counter. A job explicitly retired by a
  clean job was already exempt, and an aged-out job returns the same error to
  the miner, but only the latter counted: a miner legitimately continuing on a
  non-clean job was disconnected after `max_invalid_shares` submissions.
- The windowed hashrate estimate no longer divides by the gap between shares,
  which could make it arbitrarily large. It measured elapsed time from the
  oldest share inside the window, so a session that submitted a few shares
  close together and then went quiet collapsed the time base: two shares 155
  microseconds apart in the ten-minute window produce a reading of roughly
  2.3e17 H/s. Because the all-time best-hashrate watermark is a monotonic
  maximum written to SQLite, one such reading stood forever and no honest
  measurement could ever reach it again. On the developer's own pool the stored
  watermark was 814 times the highest rate observed across 17,614 recorded
  samples.

  The estimate now divides accumulated share work by the period actually
  observed: the whole window once the session is that old, and the session's
  age before then. A fixed observation period cannot collapse, so the figure is
  bounded by the work genuinely proven and can only read high if the shares were
  really submitted. Nothing is reported during the first 30 seconds of a
  session, when there is not yet a meaningful time base.

  Expect dashboard hashrate readings to change, most visibly on the short
  windows. The new figures are the correct ones.
- Added a `POST /api/reset-best-hashrate` endpoint to clear the all-time
  best-hashrate watermark, gated behind `[metrics] allow_runtime_settings` like
  the other mutating routes. Needed because the watermark only ever moves up, so
  a value recorded from the estimator fault above cannot otherwise be cleared.
  The reset is deliberately manual: silently rewriting an all-time record is not
  something an upgrade should do on the operator's behalf. Best-share records
  are untouched, being proof of work actually performed.
- Repeated `mining.suggest_difficulty` messages can no longer postpone
  retargeting indefinitely. Each one reset the retarget window, so a client
  suggesting more often than `retarget_interval_secs` could pin itself at the
  difficulty floor for the life of the session. The first suggestion still gets
  a fresh window and later ones still set the value.
- A malformed transaction id in a block template is reported as a recoverable
  error instead of aborting the template engine's task, which previously left
  the pool serving one frozen job for the rest of the process lifetime.
- Configurations that would make every found block invalid are rejected at
  boot. The coinbase scriptSig has a 100-byte consensus limit and nothing
  checked it, so an over-long `coinbase_tag` left the pool looking healthy,
  accepting shares, until a block was finally found and rejected with
  `bad-cb-length`. The total extranonce width is also bounded at 32 bytes,
  which SV2 channel setup requires, and `extranonce1_size` must be non-zero so
  sessions do not all search identical space. These three refuse only
  configurations that could never produce a valid block or a working SV2
  channel, so no working deployment is affected.

### Changed
- The RPC tip poll now runs continuously alongside ZMQ rather than only after a
  ZMQ failure, because a subscription cannot be proven live from the pool's
  side. It signals only when the tip hash actually changes, so with healthy ZMQ
  it costs one `getbestblockhash` per `poll_interval_ms` and nothing else.
- `pool_rpc_fallback_used_total` now counts tip changes observed while ZMQ was
  silent, rather than the listener returning an error, and a warning naming the
  relevant config key is logged alongside it. A silently dead subscription is
  visible instead of invisible. Alerting on this metric may begin firing on
  deployments where ZMQ was never delivering.

## [0.6.3] - 2026-08-05

### Added
- BIP110/RDTS card on the dashboard: shows whether the node's block templates
  set version bit 4, read live from the current template. The pool copies the
  block version from the node, so the card reflects the node's stance, not
  anything the pool decides. It exists so an operator can see which side of
  the RDTS deployment their node is on, and, once enforcement starts, whether
  they are still connected to a node that followed a minority split. Expected
  to be removed once the deployment settles.

### Removed
- The unused `[zmq] rawtx_endpoint` config field. The pool only consumes
  `hashblock` notifications; the raw-transaction stream was never read.
  Existing configs that still set the key keep working (unknown keys are
  ignored), and `zmqpubrawtx` is no longer suggested in the docs.

### Fixed
- The per-worker Prometheus hashrate gauge (`pool_hashrate_estimated_hps`) no
  longer freezes at its last value when a miner disconnects. A background task
  re-pushes the same decayed 10-minute estimate the dashboard shows, so the
  gauge reads zero within ten minutes of a worker going offline instead of
  holding its last live value until the exporter's 24-hour idle timeout.
  Grafana graphs built on the gauge now match the dashboard.
- SV2 setup rejections now answer with a spec-compliant `SetupConnection.Error`
  before disconnecting, covering all three spec error codes:
  `unsupported-protocol` for non-mining sub-protocols (such as a Job
  Declarator Client), `protocol-version-mismatch` for version ranges the pool
  cannot serve, and `unsupported-feature-flags` for devices that require
  standard jobs or work selection (the pool serves extended channels only and
  does no job declaration; such a device previously stalled at channel open).
  Previously the connection was closed without a reply, which a conformant
  client could not distinguish from a network fault. Malformed payloads still
  disconnect immediately.

## [0.6.2] - 2026-07-10

### Added
- Market card: 24-hour price change shown as a green/red percentage under the
  BTC price, and the price digits pulse green or red on each tick by
  direction (fetched from CoinGecko every 60 seconds, with the 24h-change
  field on the same request). A quote-currency selector (USD, EUR, GBP, CAD,
  AUD, CHF, JPY) persists in the browser like the theme choice.
- Mobile layout: below 880px the nav collapses into a hamburger drawer
  (brand left, menu button right; links, Connect/Settings, Raw metrics,
  Theme, and Uptime inside). The workers table now scrolls horizontally
  inside its panel instead of stretching the whole page, and on phones the
  Mode, Vardiff, 3h/24h hashrate, Best share, and Uptime columns are hidden
  so the essentials fit without scrolling.

### Fixed
- **Offline workers no longer freeze the pool hashrate.** Windowed hashrate
  estimates are only recomputed while a miner's session is delivering traffic,
  so a disconnected worker's last values stayed in the dashboard totals
  unchanged until eviction (up to 24 hours). Offline workers' estimates now
  decay linearly out of each window: the 60s figure reads zero after a minute
  offline, 10m after ten minutes, 3h after three hours, 24h after a day. The
  decayed values feed the dashboard totals, the per-worker rows, the hashrate
  history chart, and the best-hashrate watermark. Online workers and
  reconnects behave as before.
- **Dead miners are detected in about a minute instead of 15-20 minutes.**
  A powered-off miner sends no TCP FIN, and two intended defenses were not
  working: the accept path claimed to enable TCP keepalive but only set
  TCP_NODELAY, and the per-session idle timeout was reset by every job
  broadcast (the timeout future is recreated each `select!` iteration), so it
  never fired for a dead peer still being sent jobs. Detection was left to the
  kernel's retransmission budget on job writes (15-20 minutes with Linux
  defaults). Accepted sockets now get real TCP keepalive (probe after 60s
  idle, every 10s, 3 tries) plus `TCP_USER_TIMEOUT` (90s, Linux) bounding
  unacked job writes, and the idle timeout in both session loops (SV1 and SV2)
  anchors on the last inbound message, so `idle_timeout_secs` now works as
  documented. Note for very small hardware: the broken timeout also never
  disconnected legitimate miners whose share interval exceeds
  `idle_timeout_secs` (default 300). If your vardiff floor puts a miner's
  expected share pace above that, raise `idle_timeout_secs`.

## [0.6.1] - 2026-07-05

### Added
- **Per-reason reject breakdown on the dashboard.** Each worker's rejected
  shares are now counted by reason (stale, duplicate, low difficulty, unknown
  job, bad extranonce, invalid) in the in-memory stats and exposed via
  `/api/stats` (`worker_states[].reject_reasons`). The Rejects tile shows the
  pool-wide breakdown next to the stale rate, and hovering a worker's rejected
  count in the workers table shows that worker's breakdown. Counters reset on
  restart.
- Hide/Show toggle on the hashrate chart panel. The choice persists in the
  browser (localStorage, like the theme), and while hidden the dashboard skips
  the periodic chart fetch; expanding re-fetches immediately.

### Changed
- New-block indication on the Chain tip card: the height now pulses in the
  accent color (two beats), replacing the green background flash.

### Fixed
- Per-worker rejected counts previously missed `job_not_found` rejects (SV1
  and SV2) and `bad_extranonce` rejects (SV2); only the pool-level counter
  recorded them. All reject paths now increment the worker counter, matching
  the Prometheus `pool_shares_rejected_total` metric.

## [0.6.0] - 2026-07-02

### Added
- **SV2 pool identity (authority key pinning).** The Noise authority key now
  persists across restarts (`[sv2] authority_key_file`, created on first start
  with owner-only permissions, same pattern as bitcoind's `.cookie`), so the
  pool keeps a stable identity that miners can pin. The base58check public key
  (SRI `key-utils` format) is logged at startup and shown in the dashboard's
  Connect modal with a copy button, and returned by `GET /api/info` as
  `sv2_authority_pubkey`. Set it as the pool/authority public key on an SV2
  miner to cryptographically verify the pool; miners that do not pin connect
  exactly as before. `[sv2] persist_authority_key = false` opts out (fresh key
  per process, the previous behavior).
- `[sv2] cert_validity_secs` — validity window of the per-connection
  certificate (default one year). Short values are useful for testing how a
  verifying miner handles certificate expiry and clock skew.
- Tests: full Noise handshakes against a pinning SRI initiator (correct key
  accepted, wrong authority key rejected, expired certificate rejected, no-pin
  still connects) plus authority-key-file round-trip/permission checks.

### Changed
- **Upgrade note (breaking for read-only deployments):** with SV2 enabled the
  pool now creates `sv2-authority.key` (relative to its working directory) on
  first start and **fails at boot if it cannot**. Deployments with a read-only
  working directory, such as the shipped systemd unit with
  `ProtectSystem=strict`, must set `[sv2] authority_key_file` to a writable
  path (e.g. `/var/lib/solo-pool-rs/sv2-authority.key`) or set
  `persist_authority_key = false`. Docker users who want the pool identity to
  survive container re-creates should point it into the data volume
  (`authority_key_file = "data/sv2-authority.key"`).

### Fixed
- Dashboard: the Connect modal **Copy buttons now actually copy** when the
  dashboard is served over plain HTTP (the usual LAN case).
  `navigator.clipboard` only exists in secure contexts, so the old code
  selected the text and showed "Copied" without copying. Insecure contexts now
  fall back to `document.execCommand('copy')`, and if even that fails the
  button says "Copy manually" and leaves the text selected.
- **Duplicate-share tracking** no longer misreports or permits bounded replay:
  a share is recorded for dedup only after it validates (invalid submissions
  previously occupied slots, so a later identical valid submit was wrongly
  rejected as `duplicate`), and the per-session set is cleared on every
  clean-job broadcast, scoping replay protection to live jobs instead of FIFO
  eviction (evict-then-resubmit could inflate share/hashrate stats).
- Pool **best-share / best-hashrate writes are monotonic end to end**: the
  SQLite `UPDATE`s now carry a `?1 > ...` guard (matching the per-worker
  variant) and the in-memory best-hashrate update is a CAS loop, so racing
  writers can no longer regress a recorded best value.
- A block accepted by the **background submit retrier** (inline attempts
  failed, e.g. while bitcoind restarts) now updates the dashboard block count
  and last-block panel, not just the Prometheus counters.

## [0.5.1] - 2026-06-15

### Added
- Dashboard: **Connect** modal (rail nav) — shows the exact
  `stratum+tcp://<host>:<port>` to point a miner at (host derived from the
  browser, port from the pool config), with a copy button, a note that SV1/SV2
  are auto-detected on the one port, the read-only payout address (link to
  Settings), per-firmware quick-start hints, and build/network/license info.
  Backed by a new read-only `GET /api/info` endpoint. Removes the
  "where do I point my miner?" onboarding gap, especially for Umbrel installs.
- `mining.suggest_difficulty` is now honored as a **starting** difficulty,
  clamped to the configured vardiff floor/ceiling (vardiff takes over from
  there). Lets miners that send it (e.g. AxeOS's "pool difficulty" field) cold-
  start near their settled difficulty instead of bursting low-diff shares from
  the floor. A suggestion can never push a miner below the floor.

### Changed
- Dashboard: the per-worker **degraded (amber) status LED** is now evaluated
  *relative to each worker's own established cadence* rather than a flat
  no-share timeout. Low-hashrate, never-submitted, and just-connected miners
  (whose natural share interval is long or not yet established) are no longer
  falsely flagged; a worker is amber only after going silent well past its own
  expected share interval.

### Documentation
- README: added a "Difficulty and small / large miners" note explaining the
  `min_difficulty` floor, that share difficulty has no payout effect in solo,
  and how to tune for small or large hardware.

## [0.5.0] - 2026-06-14

### Added
- Dashboard: **Settings page** — the payout address can now be changed at
  runtime from the dashboard. The network is **detected from the connected
  node** (`getblockchaininfo`, including testnet4) and shown read-only; a change
  is validated (the address must parse and belong to the node's chain),
  persisted in the stats SQLite (overriding config.toml at next boot), and
  applied immediately via a forced clean-job broadcast so connected miners
  switch payout without waiting for the next block. New config keys:
  `pool.network` (optional assertion — boot fails fast if set and the node
  reports a different chain) and `metrics.allow_runtime_settings` (default true;
  set false on an untrusted LAN, or front the dashboard with an authenticating
  proxy).
- Dashboard: redesigned **console layout** — fixed left rail with scroll-spy
  navigation and status footer, hero hashrate zone with block odds, hairline
  KPI strip, and a non-mainnet network badge. Two color themes — carbon
  (near-black, amber accent; default) and Swiss light (porcelain, cobalt
  accent) — toggleable from the rail, seeded from the OS preference, and
  persisted per browser. The hashrate chart re-skins from the active theme's
  CSS variables without a server round trip. Settings opens in a centered modal.
- Dashboard: **theme-aware brand logo** — a pickaxe-and-Bitcoin-block SVG mark
  with dark and light variants that follow the active theme (replacing the
  earlier glyph).
- Dashboard: **per-worker status LEDs** — an online (green) / offline (grey)
  indicator replaces the Online/Offline text, with a third **degraded (amber)**
  state when a worker is connected but hasn't landed an accepted share in over
  two minutes (the same signal as the "degraded" KPI). The status column is
  centered and the worker columns are reordered to Worker · Status · Mode.
- Dashboard: **rail connectivity LED** — green while `/stats` refreshes are
  landing, grey when they stall, so a frozen dashboard is visible at a glance.

### Changed
- Dashboard: the Market card drops the redundant raw-difficulty readout (the
  same value is already shown on the Network card).

### Security
- Mining is **paused unless the payout address validates against the node's
  chain**. A wrong-network address (e.g. a `tb1…` testnet address while the
  node is on mainnet) still encodes a perfectly valid coinbase script, so the
  pool would previously have mined to a script the operator may not control;
  now no job is built until a valid address is saved (the pool still boots and
  the dashboard stays reachable to fix it — which also gives container
  platforms a clean first-run flow with a placeholder address).
- Stratum: the miner-disconnect metric's `reason` label is now a bounded value
  instead of the raw parser error, which embedded attacker-controlled bytes
  (including line/column). Malformed input could otherwise mint unbounded
  Prometheus time series — a metrics-bloat / amplification vector.

### Fixed
- Stratum: blank / whitespace-only lines (a common firmware keepalive) are now
  ignored instead of being parsed as empty JSON and dropping the connection.

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

[Unreleased]: https://github.com/cbyam/solo-pool-rs/compare/v0.6.5...HEAD
[0.6.5]: https://github.com/cbyam/solo-pool-rs/compare/v0.6.4...v0.6.5
[0.6.4]: https://github.com/cbyam/solo-pool-rs/compare/v0.6.3...v0.6.4
[0.6.3]: https://github.com/cbyam/solo-pool-rs/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/cbyam/solo-pool-rs/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/cbyam/solo-pool-rs/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/cbyam/solo-pool-rs/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/cbyam/solo-pool-rs/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/cbyam/solo-pool-rs/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/cbyam/solo-pool-rs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/cbyam/solo-pool-rs/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cbyam/solo-pool-rs/releases/tag/v0.2.0
