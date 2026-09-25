# TODO

Backlog of known work, grouped by whether it has to land before 1.0. The 1.0
release is a stability promise, not a feature milestone: it names the surface
that will not change incompatibly and starts the SemVer clock on it. Anything
that could force a breaking change to that surface belongs in the first group;
everything else can ship after 1.0 as a minor or patch release. Line references
are as of the review that raised each item and may drift.

## Before 1.0 (rc checklist)

Gates, in the order they can be closed:

- [x] **Cut 0.6.7** with the dashboard work merged 2026-09-06 (round effort,
  persisted found blocks, node health, chart downtime gap, BIP110 card removal)
  and move the community-store pin to it. Releases have since reached 0.6.10.
- [x] **Write the stable-surface doc.** Drafted as `docs/stable-surface.md`:
  config keys and their meaning, Stratum V1/V2 behaviour on the wire, the
  HTTP JSON, metric names and labels, files on disk, the packaging contract
  Umbrel depends on, and the deferred stances (TLS, fees/multi-coin/cloud
  non-goals, RPC failover). Ships as the headline of the rc once its open
  items close (next gate).
- [x] **Close the stable-surface "Before this is final" list.** Each item was
  a place where the code and the draft disagreed; all were settled in the code
  or the doc for 0.6.11. The list stays in the doc for anything that drifts
  before the rc.
- [x] **Submit the official Umbrel app-store PR.** Opened 2026-09-06 as
  getumbrel/umbrel-apps#6064 with 0.6.8 pinned, after a test install in an
  umbrelOS guest. Review is pending and outside our control.
- [x] **Decide the RPC failover config shape, without building it.** Decided in
  the stable-surface Stances: a future list key sits beside `url`, and the
  single-URL form is never removed in 1.x.
- [x] **Fail loudly when the stats store will not open.** Done for 0.6.11 as
  an alarm rather than a boot error, so a stats problem never stops mining:
  an open failure or a failed write logs at error level and shows as a red
  "Stats not saving" pill, `/stats` `stats_store_error`, and
  `pool_stats_store_ok 0`, until a later write succeeds.
- [ ] **Prod soak.** Two months of continuous mainnet mining with no
  correctness bug on the share or block path, started 2026-09-05 on 0.6.6.
  Patch-day restarts do not reset it; a change to share validation or block
  submission does. Earliest completion around 2026-11-05.
- [ ] **Tag v1.0.0-rc.1** once the doc ships and the Umbrel PR is open;
  **1.0.0** after store acceptance and a quiet end to the soak.

Explicitly not a gate: feature completeness. Fees, multi-coin and cloud are out
of scope by decision, and 1.0 puts that in writing.

## Next: notifications (planned for 0.6.12, not a 1.0 gate)

The operator is not always watching the dashboard. When a miner drops at 2 am,
the pool should say so. This adds a config section that 1.0 will promise, so
the shape gets its own review before it ships.

- **Channels**, each off until its address is set, several at once allowed:
  - **Webhook** (first): JSON POST with `title`, `message`, `body` (same
    text, so Gotify and the Apprise API accept it as is), `priority`, `event`,
    `worker`, `ts`. Covers Home Assistant (and its phone push or SMS),
    Gotify, Node-RED, n8n, and Apprise (email, SMS gateways, and the rest).
    Confirm the Gotify and Apprise shapes against both before relying on it.
  - **Email** (second): native SMTP with `lettre` over the rustls stack the
    binary already carries. High-priority events only by default.
  - **ntfy** (third): plain-text POST with Title, Priority and Tags headers.
    The quickest phone push; self-hosted or ntfy.sh.
  - **Heartbeat** (fourth): GET a dead-man's-switch URL every few minutes
    (healthchecks.io, Uptime Kuma push monitor), the only way to hear that
    the pool host itself is down.
  - SMS is reached through the webhook (Home Assistant, Apprise) or email.
    A native Twilio adapter only if someone asks; carrier email-to-text
    gateways are being retired, so do not document them as a path.
  - Discord and Telegram: small adapters, not planned unless requested; the
    webhook reaches both through Apprise.
- **Config**, all flat scalars in `[notify]` so every key works through the
  `SOLO_POOL_NOTIFY__*` environment overrides (Umbrel):
  `webhook_url`, `ntfy_url`, `smtp_host`, `smtp_port`, `smtp_user`,
  `smtp_password`, `email_from`, `email_to`, `email_min_priority` (default
  high), `offline_after_secs` (default 600), `heartbeat_url`.
- **Events and priority**: block found (urgent); miner offline past the grace
  period, miner rejecting at red, node stale, stats not saving (high); miner
  degraded, rejecting at amber (normal); a recovery for each (low).
- **Behaviour**: one message per state change, never repeats while a state
  holds; events that land together are bundled (a switch taking four miners
  down sends one message); the grace period keeps pool restarts and miner
  reboots quiet. Quiet hours are left to the phone app.
- **Secrets**: URLs and passwords get the same treatment as the RPC password
  (readable-config warning at boot, redaction in logs; a Telegram token, if
  that adapter ever lands, sits in the URL).
- **Send test** button on the Settings page, behind the same Host/Origin
  guards as the other mutating routes.

## After 1.0 (no promised surface changes)

### Correctness and hardening

- [ ] **SV2 spec conformance set.** Each is small on its own; group them and
  test against the NerdQAxe++ before merging, since they touch the handshake and
  channel-open path that hardware actually uses. Held until after the 0.6.11
  soak, which already carries the SRI v1.12 port on the same path.
  - `OpenExtendedMiningChannel.max_target` is used once to clamp the initial
    target and then discarded, so later vardiff `SetTarget` messages can hand a
    device an easier target than it declared it would accept. Store it on the
    session and clamp every retarget. The open-time clamp also adjusts the wire
    target without adjusting `session.difficulty`, so the first retarget derives
    from a value the device was never assigned.
  - A repeat `OpenExtendedMiningChannel` on an open channel re-allocates the
    channel id and extranonce prefix without closing the previous one, leaving
    in-flight shares validating against a prefix the device no longer has.
  - `SubmitSharesExtended.channel_id` is decoded and ignored. Harmless while a
    session serves one channel, but it stops being harmless combined with the
    item above.

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

- [x] **Pool-wide metrics no longer expire** (0.6.11): a 30 s tick touches
  every series without a `worker` label, so the 24 h idle timeout only bounds
  worker series; `pool_worker_online` and
  `pool_worker_last_share_timestamp_seconds` added for offline alerting.
- [x] **Descriptive wrong-extranonce2 reply on SV1** (0.6.11): code 20 with
  "Wrong extranonce2 size: got N bytes, expected M".
- [x] **Absolute pre-auth deadline** (v0.6.6): measured from connect, so blank
  keepalive lines cannot hold a connection slot without authorizing.
- [x] **Dependency refresh**: `rusqlite` 0.40 and the metrics trio (0.24 /
  0.18 / 0.20, which must move together) are current.
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
  rollback). The install directory is named from `Cargo.toml`, so a test build
  is installed under a temporary pre-release version (e.g. `0.6.11-dev.2`,
  never committed) to keep it from overwriting a release's directory.
  Pre-release directories sort above their release for `--rollback`, so delete
  them after the soak.
- [x] **v0.4.0: non-root Docker image**.
- [x] **SV2 identity pinning** (v0.6.0): persistent Noise authority key
  (`[sv2] authority_key_file`), pubkey logged at boot and shown in the Connect
  modal + `GET /api/info`; verified on a NerdQAxe++. The bitaxe/nerdqaxe
  firmware checks only the Schnorr signature, never the validity window;
  upstream enforcement-toggle PRs: bitaxeorg/ESP-Miner#1796,
  shufps/ESP-Miner-NerdQAxePlus#656.
