# Stable surface

**Status: draft for the 1.0 release candidate.** Until 1.0 ships this document
describes intent, not a promise. The "Before this is final" section at the end
lists what still has to change in the code or the docs for every statement
here to be true.

## What 1.0 means

1.0 is a stability promise. Feature completeness plays no part in it. From 1.0 the project
follows SemVer in the usual direction: an incompatible change to anything in
"What is covered" bumps the major version, a new capability bumps the minor
version, and everything else bumps the patch version. Before 1.0 the rule was
inverted (breaking changes bumped the minor), so 0.x history is not evidence of
anything below; the promise starts at the 1.0 tag.

"Incompatible" means: a config file, a miner, a script, a Prometheus scrape, a
compose file, or a stats database that worked with 1.x stops working, or
silently means something different, with 1.y where y > x. Adding a config key
with a default, a JSON field, a metric, or a Stratum extension is compatible.
Removing, renaming, retyping, or changing the meaning of one is not.

Everything under "What is covered" is verifiable: a config key can be checked
against `config.toml.example`, a wire behaviour against the regtest end-to-end
test and the unit tests, a route against `curl`. Where a statement here is
not yet backed by a test, it is called out.

## What is covered

### 1. Configuration file

The pool reads one TOML file. Its path is the first positional argument, or the
value of `--config <path>`, or `config.toml` in the working directory when no
argument is given. There is no search path. There are no other command-line
flags.

Promised for every key listed below: its section and name, its type, its
meaning, and its validation rule. A key that is required stays required only
until a default is added for it; a default is never removed. Default values
shipped in `config.toml.example` change only in a major release. Unknown keys
are ignored; a future minor release may log a warning for them but will not
refuse to start.

`~` at the start of a path value expands to the home directory in
`cookie_path`, `authority_key_file`, and `log_dir`.

| Section | Key | Type | Meaning and validation |
|---|---|---|---|
| `[pool]` | `listen_addr` | string, required | TCP bind for Stratum V1 and V2 on one port. |
| | `coinbase_address` | string, required | Payout address. Validated against the node's chain at boot; an address for the wrong chain is not fatal, the pool starts with mining paused. See §6. |
| | `coinbase_tag` | string, required | Text placed in the coinbase scriptSig. Length checked so the scriptSig stays within 100 bytes after the height push and the extranonce. |
| | `initial_difficulty` | integer, required | Share difficulty a new connection starts at. Not clamped to the vardiff bounds. |
| | `extranonce1_size` | integer, required | Bytes of pool-assigned extranonce. At least 1. |
| | `extranonce2_size` | integer, required | Bytes of miner-rolled extranonce. The two sizes sum to between 1 and 32. |
| | `max_connections` | integer, required | Global cap on open miner connections. |
| | `idle_timeout_secs` | integer, required | Disconnect an authorized session after this long with no inbound message. |
| | `found_block_dir` | string, default `found-blocks` | Directory for the found-block archive. See §8. |
| | `network` | string, optional | If set, one of `mainnet`, `testnet`, `testnet4`, `signet`, `regtest`. A mismatch with the node's chain is fatal at boot. |
| `[sv2]` | `enabled` | bool | Accept Stratum V2 on the shared port. The whole section may be omitted (SV2 on). If the section header is present the key is required. |
| | `persist_authority_key` | bool, default true | Keep the Noise authority key across restarts. |
| | `authority_key_file` | string, default `sv2-authority.key` | Where the key lives. Created on first start. Must be non-empty when persistence is on. |
| | `cert_validity_secs` | integer, default 31536000 | Validity window of the per-connection Noise certificate. |
| `[bitcoin_rpc]` | `url` | string, required | Node RPC endpoint. |
| | `cookie_path` | string, optional | Cookie file. Default `~/.bitcoin/.cookie`. If readable it wins over user and password. |
| | `user`, `password` | string, optional | Used only when the cookie is not readable. |
| | `timeout_secs` | integer, required | RPC timeout. Values under 5 are raised to 5 with a warning, not rejected. |
| `[zmq]` | `hashblock_endpoint` | string, required | ZMQ `hashblock` publisher of the node. |
| | `poll_fallback` | bool, required | Poll the tip over RPC when ZMQ is silent. |
| | `poll_interval_ms` | integer, required | Poll cadence. |
| `[vardiff]` | `target_share_time_secs` | integer, at least 1 | Seconds per share the retarget aims for. |
| | `retarget_interval_secs` | integer, at least 1 | How often a session is retargeted. Evaluated on inbound traffic, never on a timer. |
| | `min_difficulty` | integer, at least 1 | Floor. Shares are accepted against this value, never against the session's current difficulty. See §5. |
| | `max_difficulty` | integer, at least `min_difficulty` | Ceiling. Equal to the floor is allowed and pins every miner. |
| | `max_retarget_factor` | float, at least 1.0 | Largest ratio one retarget may move by. |
| `[security]` | `max_connections_per_ip` | integer, required | New connections per source address per sliding minute. Exceeding it refuses the connection. It never bans. |
| | `max_shares_per_sec` | integer, required | Token bucket on every inbound message per session. Exceeding it disconnects. It never bans. Zero rejects every message; there is no "disabled" value. |
| | `ban_duration_secs` | integer, required | Length of a ban. Zero disables banning. |
| | `max_invalid_shares` | integer, required | Invalid shares before a session is disconnected. Zero disables. |
| | `max_message_bytes` | integer, required | Longest Stratum V1 line, and the basis of the V2 frame cap. Exceeding it bans. |
| | `max_worker_name_len` | integer, default 128 | Longest accepted worker name. |
| | `max_authorizations_per_session` | integer, default 8 | Distinct worker names one connection may authorize. Zero disables. |
| `[metrics]` | `prometheus_addr` | string, required | HTTP bind for the dashboard and `/metrics`. Empty disables both. |
| | `stats_db_path` | string, optional | SQLite file for persistent state. Omitted or empty disables persistence; the pool still runs. |
| | `allow_runtime_settings` | bool, default true | Whether the two mutating HTTP routes are enabled. |
| | `allowed_hosts` | list of strings, default empty | Extra `Host` names accepted on mutating routes. Bare hostnames only, optional port. |
| `[logging]` | `level` | string, required | A `tracing` filter. An unparseable value falls back to `info`. |
| | `json` | bool, required | JSON lines instead of human-readable output. |
| | `log_dir` | string, optional | Directory for daily-rotated files. Unset or empty logs to stdout. |
| | `log_max_files` | integer, default 14 | Daily files kept when `log_dir` is set; the oldest is deleted as a new one opens. At least 1. |

**Environment overrides.** Any scalar key can be set with
`SOLO_POOL_<SECTION>__<KEY>` (upper case, double underscore between section
and key). The value is parsed as the key's TOML type when the key exists in
the file and inferred otherwise; wrapping the value in double quotes forces a
string. A `SOLO_POOL_` variable without `__` is a fatal boot error. The
scheme cannot set a list value (`allowed_hosts`) and cannot unset an optional
key. Those two limits are part of the surface until a later release lifts
them, which would be a compatible change.

**Precedence for the payout address.** A value saved from the dashboard is
stored in the stats database and wins over `coinbase_address` in the file on
every later start, unless it is invalid for the node's current chain, in
which case the file value applies and the mismatch is logged. Editing the
file does not undo a dashboard save; the dashboard does.

### 2. Stratum V1

Plain JSON-RPC lines over TCP, `\n` or `\r\n` terminated.

- **Methods:** `mining.subscribe`, `mining.authorize`, `mining.submit`,
  `mining.configure`, `mining.suggest_difficulty`. Any other method receives
  error 20. Blank and whitespace-only lines are ignored and cost nothing
  against the rate limit. A line that is not valid JSON closes the
  connection. The request `id` may be any JSON value and is echoed verbatim;
  notifications carry `"id": null`.
- **Subscribe result:**
  `[[["mining.set_difficulty", sid], ["mining.notify", sid]], extranonce1_hex, extranonce2_size]`
  where `sid` is a 16-hex-character session id and the sizes come from
  `[pool]`. The pool-assigned extranonce1 is unique per connection for the
  life of the process.
- **Authorize:** any worker name is accepted subject to: non-empty, at most
  `max_worker_name_len` bytes, no whitespace or control characters. The
  password is ignored. There is no `address.worker` convention; the payout
  address comes only from configuration. Authorizing before subscribing
  returns error 25. Distinct names per connection are capped by
  `max_authorizations_per_session`; exceeding it disconnects.
- **After authorize:** the pool sends `mining.set_difficulty` then
  `mining.notify` with `clean_jobs = true`. Every later job is sent the same
  way. `clean_jobs` is true on a new chain tip (however detected), on the
  first job after boot, and after a payout-address change; it is false for
  the periodic ntime refresh.
- **Configure:** `version-rolling` is granted with mask `1fffe000` when the
  miner's requested mask intersects it and the intersection satisfies the
  miner's `min-bit-count`; otherwise it is refused. The reply always carries
  `minimum-difficulty: true` with the session's current difficulty as the
  value; a value requested by the miner is not applied. `subscribe-extranonce`
  is acknowledged; see "Before this is final" for what that currently means.
- **Suggest difficulty:** `[n]` or bare `n`, integer or float, finite and at
  least 1. Applied once as the starting difficulty, clamped to the vardiff
  floor and ceiling. Vardiff takes over from there.
- **Submit:** the reply is `true` for an accepted share, otherwise an error.
  A share is judged against `min_difficulty`. A share whose extranonce2
  length differs from the advertised size is rejected with error 20 and
  counts as invalid. An unknown or superseded job id returns 21 and does not
  count as invalid. An `ntime` outside `[template min time, now + 7200 s]`
  returns 20.
- **Error codes:** `[code, message, null]` with 20 Unknown problem, 21 Job
  not found, 22 Duplicate share, 23 Low difficulty share, 24 Unauthorized
  worker, 25 Not subscribed. Codes and their meanings are promised; message
  text is not.

### 3. Stratum V2

- **Transport:** Noise `NX` with secp256k1 EllSwift, ChaChaPoly, SHA256. The
  pool is the responder. Plaintext V2 is not accepted; a connection whose
  first byte is not `{` and which does not complete the handshake is dropped.
- **Identity:** the pool signs each connection's certificate with a
  persistent authority key and publishes the base58check public key in the
  boot log, the dashboard Connect dialog, and `GET /api/info`. Miners may pin
  it; miners that do not connect exactly the same way. The certificate is
  valid from the handshake for `cert_validity_secs`.
- **SetupConnection:** mining sub-protocol only, version 2 only. A
  `min_version` above 2 gets `protocol-version-mismatch`; another sub-protocol
  gets `unsupported-protocol`; `REQUIRES_STANDARD_JOBS` or
  `REQUIRES_WORK_SELECTION` gets `unsupported-feature-flags` naming the
  offending flags. Version rolling is always available with mask
  `1fffe000`; the mask is not negotiated. `SetupConnectionSuccess.flags` is 0.
- **Channels:** extended channels only. `OpenExtendedMiningChannel` grants
  the requested `min_extranonce_size` verbatim (at least 1) and keeps the rest
  of `extranonce1_size + extranonce2_size` as the pool prefix. A request
  larger than the total gets `unsupported-extranonce-size` and the connection
  stays open. The initial target is the session difficulty, clamped so it is
  never easier than the device's `max_target`. On open the pool sends
  `NewExtendedMiningJob` (future) then `SetNewPrevHash`. Retargets arrive as
  `SetTarget`. `OpenStandardMiningChannel`, `UpdateChannel`, `CloseChannel`
  and every other message are ignored.
- **Submit:** `SubmitSharesExtended`. Rejections use the string codes
  `bad-extranonce-size`, `stale-job`, `block-submit-failed`, `stale`,
  `duplicate`, `low_difficulty`, `invalid`. An unmapped job id counts as
  invalid on V2.

### 4. One port, connection policy

- The first byte of a connection decides the protocol: `{` is Stratum V1,
  anything else is Stratum V2 when `[sv2] enabled`, otherwise the connection
  is dropped without a reply.
- Order of checks on accept: global `max_connections`, ban list, per-IP rate
  limit. Refusals are silent drops.
- A connection has 10 seconds from accept to reach an authorized V1 worker or
  an open V2 channel, covering the first-byte peek, the Noise handshake, and
  every message before that point. After that `idle_timeout_secs` applies,
  measured from the last inbound message.
- What bans: an oversize V1 line, or a V2 frame larger than
  `max_message_bytes + 1024`. Nothing else bans. Rate limits refuse or
  disconnect. Invalid-share overflow disconnects.

### 5. Difficulty and credit

- A session starts at `initial_difficulty` (or a valid suggestion), and is
  retargeted every `retarget_interval_secs` toward one share per
  `target_share_time_secs`, moving by at most `max_retarget_factor` per step,
  clamped to the floor and ceiling, and only announced when the change exceeds
  5%. A window with no shares halves the difficulty, floored at
  `min_difficulty`.
- Shares are accepted against `min_difficulty`, so firmware that ignores
  difficulty changes keeps mining.
- Each accepted share is credited at the threshold it actually cleared: the
  current assignment, else the previous assignment, else the floor. Hashrate,
  the round effort, and the per-worker share counts all use this credited
  value, never the hash's own difficulty.

### 6. Payout and block submission

- Every block pays the full coinbase to `coinbase_address`. There is no fee,
  no split, no second output for the pool.
- If no valid address exists for the node's chain, no jobs are built and
  connected miners receive nothing until one is saved. The dashboard shows
  "Mining paused".
- A found block is written to the archive in parallel with the first
  `submitblock` call, never before it. Submission is retried inline three
  times with backoff, then in the background every 10 seconds for two hours.
  A block still unconfirmed when the process stops is resubmitted on the next
  boot; the node answering "duplicate" counts as success.

### 7. HTTP interface

Served on `prometheus_addr`. No authentication. Intended for a trusted LAN.

| Route | Promise |
|---|---|
| `GET /` | The dashboard. Its markup, styling and copy are not covered. |
| `GET /stats` | JSON. Every field listed below keeps its name, type and meaning. New fields may appear. |
| `GET /history?since=<unix>` | JSON array of `{ts, hps}`. |
| `GET /chart?window=36h\|1w\|1m\|6m` | An ECharts option object. Only the `series[0].data` array of `[ms, hps or null]` pairs is covered; the rest of the object is not. |
| `GET /api/info` | `version`, `stratum_port`, `sv2_enabled`, `sv2_authority_pubkey` (or null), `network`, `coinbase_address`. |
| `GET /api/settings` | `coinbase_address`, `network`, `address_valid`, `persisted`, `editable`. |
| `POST /api/settings` | Body `{"coinbase_address": "..."}`. 200 `{ok, persisted}`; 422 `{error}` on an invalid address; 403 `{error}` when disabled or the origin check fails. |
| `POST /api/reset-best-hashrate` | 200 `{ok, best_hashrate_hps}`; 403 as above. |
| `GET /metrics` | Prometheus text exposition; 503 when the recorder is not installed. |

`/stats` fields: `shares_accepted`, `shares_rejected`, `blocks_found`,
`connected_miners`, `current_height`, `current_coinbase_value`,
`current_block_transaction_count`, `template_version`,
`best_share_difficulty`, `session_best_share_difficulty`,
`best_hashrate_hps`, `total_hashrate_60s`, `total_hashrate_10m`,
`total_hashrate_3h`, `total_hashrate_24h`, `network_hashrate_hps`,
`network_difficulty`, `est_difficulty_change_pct`, `worker_hashrates[]`,
`worker_states[]`, `uptime_secs`, `session_best_hashrate_hps`,
`last_block_worker`, `last_block_hash`, `last_block_ts`, `round_work`,
`round_start_ts`, `found_blocks[]`, `template_age_secs` (nullable),
`template_error` (nullable). Each `worker_states[]` entry: `worker`,
`protocol`, `online`, `current_vardiff`, `shares_accepted`,
`shares_rejected`, `shares_stale`, `reject_reasons{}`,
`best_share_difficulty`, `active_sessions`, `connected_ts`,
`last_submit_ts`, and the four `hashrate_*_hps` values. Each `found_blocks[]`
entry: `height`, `hash`, `worker`, `ts`, `round_work`, `network_difficulty`.
Timestamps are unix seconds; hashrates are H/s as floats.

**Mutating routes** (`POST /api/settings`, `POST /api/reset-best-hashrate`)
require `allow_runtime_settings`, a `Host` header, and either a local host
name or one listed in `allowed_hosts`. Local means: an IP literal,
`localhost`, a single-label name, or a name ending in `.localhost`, `.local`,
`.lan`, `.home`, `.home.arpa`, `.internal`, `.intranet`, `.private`. If an
`Origin` header is present its authority must equal the `Host` value;
`Origin: null` is refused. Host comparison ignores case, port, and a trailing
dot.

**Metrics.** These names and label sets are promised; adding a metric is
compatible, removing or relabelling one is not. Histogram bucket boundaries
are not covered.

| Metric | Type | Labels |
|---|---|---|
| `pool_connected_miners` | gauge | |
| `pool_connections_refused_total` | counter | `reason` |
| `pool_bans_total` | counter | `reason` |
| `pool_shares_accepted_total` | counter | `worker` |
| `pool_shares_rejected_total` | counter | `reason`, `worker` |
| `pool_worker_shares_rejected_total` | counter | `worker` |
| `pool_share_difficulty` | histogram | `worker` |
| `pool_share_validation_duration_ms` | histogram | |
| `pool_connection_duration_secs` | histogram | `worker` |
| `pool_miner_disconnects_total` | counter | `reason`, `worker` |
| `pool_block_submissions_success_total` | counter | |
| `pool_block_submissions_failed_total` | counter | `reason` |
| `pool_blocks_found_total` | counter | |
| `pool_job_broadcasts_total` | counter | |
| `pool_job_broadcast_miners` | gauge | |
| `pool_job_height` | gauge | |
| `pool_rpc_fallback_used_total` | counter | |
| `pool_worker_difficulty` | gauge | `worker` |
| `pool_vardiff_change_ratio` | histogram | |
| `pool_hashrate_estimated_hps` | gauge | `worker` |

Any series not written for 24 hours is dropped from the exposition. This
bounds the cardinality of the `worker` label and is part of the surface.

### 8. Files on disk

- **Stats database** at `stats_db_path`, SQLite, owner-only permissions.
  Promise: a database written by any 1.x opens under any later 1.y with its
  data intact; schema changes are additive and applied in place on open. No
  promise is made for opening a database under an older version. Contents:
  all-time best share and best hashrate, the current round's work and start,
  every block found (hash, height, worker, time, and the round it closed),
  six months of ten-minute hashrate samples, per-worker all-time best shares
  (top 512), and the dashboard-saved payout address. Deleting it loses
  exactly that list.
- **Found-block archive** at `found_block_dir`: `block_<height>_<hash>.hex`
  holding the raw block, moved to `submitted/` once the node has it and to
  `rejected/` if the node rejects it outright on replay. A file still at the
  top level on boot is resubmitted. The names and the three locations are
  promised.
- **SV2 authority key** at `authority_key_file`: one line, base58check
  secp256k1 secret in the SRI key-utils format, created owner-only on first
  start. A corrupt file is a fatal boot error. Deleting it changes the pool's
  public identity; pinned miners then refuse to connect until re-pinned.
- **Log files** at `log_dir`: `solo-pool-rs.log.YYYY-MM-DD`, daily rotation,
  the newest `log_max_files` kept. Safe to delete.

### 9. Packaging

- **Docker image** `ghcr.io/cbyam/solo-pool-rs`. Runs as uid and gid 10001,
  home `/home/solo-pool`, working directory `/app`, `/app/data` writable by
  that user, config at `/app/config.toml` by default (the single command
  argument), ports 3333 and 9090 exposed, `libzmq5` and SQLite present.
  Tags: `edge` for every push to main; for each release `X.Y.Z`, `X.Y`, and
  `latest`. Platforms `linux/amd64` and `linux/arm64` in one manifest.
- **Umbrel** relies on: the positional config path, the env-override scheme
  for `bitcoin_rpc.url`, `bitcoin_rpc.user`, `bitcoin_rpc.password`,
  `zmq.hashblock_endpoint` and `sv2.authority_key_file`, a writable
  `/app/data`, running as an arbitrary uid via `user:`, the dashboard on
  9090, and Stratum on 3333. All of these are covered.
- **Release tarballs** `solo-pool-rs-<tag>-<target>.tar.gz` for
  `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, each holding the
  binary, `config.toml.example`, and `README.md`. Dynamically linked against
  `libzmq5`.
- **systemd unit** at `packaging/systemd/solo-pool-rs.service`: user
  `solo-pool`, config at `/etc/solo-pool-rs/config.toml`, state under
  `/var/lib/solo-pool-rs`, binary at `/usr/local/bin/solo-pool-rs`.
  `packaging/install.sh` places versioned binaries under
  `/usr/local/lib/solo-pool-rs/<version>/` and swaps that symlink.

### 10. Process behaviour

- Starts, checks the node's chain over RPC, and exits with status 1 if the
  node cannot be reached at boot. Once running, a node outage is logged and
  surfaced on the dashboard; the process stays up and resumes when the node
  answers.
- Handles SIGTERM and Ctrl-C. On shutdown it stops accepting, writes a final
  stats snapshot, and waits up to five seconds for an in-flight block
  submission before exiting 0.
- Exits 1 on any configuration or boot error. No other exit codes are
  defined.

## What is not covered

- The dashboard: HTML, CSS, layout, wording, refresh cadence, and the shape
  of the `/chart` option object beyond its data array.
- Log lines: text, fields, levels, and which events log.
- Internal constants: the 30-second ntime refresh, the eight-job history
  depth, the broadcast buffer, retry backoff timings. They may change in a
  minor release with a changelog note.
- The Rust crate API. The library target exists for tests and fuzzing.
- Histogram bucket boundaries.
- The Docker base image and the minimum supported Rust version. MSRV bumps
  are minor releases.
- The Umbrel gallery, icon, and manifest metadata.

## Stances

These are decisions, recorded so no pending choice can force a breaking change
later.

- **No fees, no multi-coin, no hosted or cloud mode.** The pool is one binary
  beside one node paying one address. That scope is the product.
- **No TLS for Stratum V1.** Deferred since June 2026. If it is ever built it
  arrives as a separate `tls_port` key beside the existing listener, so the
  shared port and its first-byte detection are unaffected.
- **No multi-node RPC failover yet.** If it is built, `[bitcoin_rpc]` gains a
  list key beside `url` and `url` keeps working on its own. The single-URL
  form is never removed in 1.x.
- **No standard SV2 channels and no work selection.** Extended channels only;
  the flags are refused explicitly rather than half-served.
- **No dashboard authentication.** The HTTP interface is for a trusted LAN
  and is documented as such; the mutating routes are guarded against
  cross-site and rebinding attacks. A hostile LAN is out of scope.
- **Knots upgrade hold.** While Knots ships mandatory RDTS enforcement, the
  documented node builds are non-enforcing Knots or Bitcoin Core. This is
  operational guidance in the README, not a code surface.

## Change classification after 1.0

| Change | Version |
|---|---|
| Remove, rename, or retype a config key; change a default | major |
| Change a Stratum error code's meaning, a subscribe result shape, or a V2 rejection code | major |
| Remove or relabel a metric; remove or retype a `/stats` field; change a route's status codes | major |
| A stats database that a newer version cannot open, or that it opens and misreads | major |
| Change the archive file naming or the image uid, workdir, or default config path | major |
| Add a config key with a default, a `/stats` field, a metric, a route, a Stratum extension | minor |
| Change an internal constant, a log line, dashboard layout, MSRV | minor |
| Fix a bug that made behaviour differ from this document | patch |

A key or field slated for removal is first deprecated in a minor release with
a boot-time warning and keeps working until the next major.

## Before this is final

Each item is a place where the code or the shipped docs currently disagree
with the text above. They are tracked in `TODO.md` under "Before 1.0".

1. `subscribe-extranonce` is acknowledged in the `mining.configure` reply but
   the pool never sends `mining.set_extranonce`. Either send it on extranonce
   changes or stop acknowledging the extension; the README feature table
   currently lists it as supported.
2. `minimum-difficulty` is listed as a supported extension in the README, but
   a value requested by the miner is discarded. The text above documents the
   actual behaviour; the README should match.
3. `pool_zmq_reconnects_total` is declared and never written. Emit it or drop
   it before the metric table is frozen.
4. The config example says the per-IP rate limit bans, and that
   `max_shares_per_sec` is a limit "before banning". Neither bans since
   0.6.6.
5. `[sv2] enabled` is required when the section header is present, while the
   example presents the whole section as defaulted. Give the key a default of
   true.
6. The environment-override docs say every value can be overridden;
   `allowed_hosts` cannot, and optional keys cannot be unset.
7. The binary has no `--version` or `--help`; `CONTRIBUTING.md` tells
   reporters to run `--version`. Add the flags or fix the text.
8. The MSRV of 1.75 is declared in `Cargo.toml` and the README but CI builds
   on stable only, so the claim is not verified.
9. The stats-store open failure is a warning, not an error, so a locked file
    silently disables the persistence promised in §8.
