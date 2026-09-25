# solo-pool-rs

[![CI](https://github.com/cbyam/solo-pool-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/cbyam/solo-pool-rs/actions/workflows/ci.yml)
[![E2E block acceptance](https://github.com/cbyam/solo-pool-rs/actions/workflows/e2e.yml/badge.svg)](https://github.com/cbyam/solo-pool-rs/actions/workflows/e2e.yml)
[![Release](https://github.com/cbyam/solo-pool-rs/actions/workflows/release.yml/badge.svg)](https://github.com/cbyam/solo-pool-rs/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Stars](https://img.shields.io/github/stars/cbyam/solo-pool-rs?style=social)](https://github.com/cbyam/solo-pool-rs)

**A solo Bitcoin mining pool that speaks both Stratum V1 and Noise-encrypted Stratum V2 on a single port.** Point any ASIC (or a Bitaxe / NerdQAxe++) at it and **100% of the block reward goes to your address**. One Rust binary, one Bitcoin node, no fees, no payout splits, no accounts.

> **Lottery / solo mining**: every block your miners find pays out entirely to the coinbase address you configure. The pool only coordinates work; it never takes a cut.

![solo-pool-rs dashboard](docs/dashboard.png)

---

## Why this pool

If you already run an established solo-mining daemon, the honest case isn't that
this is more battle-tested. It's younger, and the [verification
section](#does-it-actually-find-and-pay-blocks-dont-trust-verify) is how you
check the part that has to be correct. The case is that it does things the older
solo daemons don't:

- **SV1 + SV2 on one port.** The protocol is auto-detected from the first byte of each connection. Legacy SV1 ASICs and modern Noise-encrypted SV2 firmware (e.g. NerdQAxe++) share the *same* host:port. No proxy, no second listener.
- **True solo.** `getblocktemplate` → your coinbase address. No payout share accounting, no PPLNS, no operator fee.
- **Self-contained.** A single Rust binary plus your Bitcoin node. Cookie auth, ZMQ block notifications, an always-on RPC poll as a safety net.
- **Observable.** A live HTML dashboard (hashrate history, per-worker table, network difficulty + estimated next-retarget move, probability) and a Prometheus endpoint.

---

## Does it actually find and pay blocks? (don't trust, verify)

The fair question for any young pool is *"how do I know a found block actually
gets submitted and pays my address?"* You don't have to take my word for it. The
proof is in the repo and runs on every change:

- **An end-to-end block-acceptance test runs on every PR and every push to main.** It boots a
  real `bitcoind -regtest`, launches the actual pool binary, connects over the
  live Stratum socket exactly as a miner would (once over SV1, once over
  Noise-encrypted SV2), grinds a real share that is also a valid block, submits
  it, and **asserts the node accepted it onto the chain and that the coinbase
  pays the pool's configured address**. This is the one path no unit test can
  fake. It guards the bugs that stay invisible until a block is genuinely
  found: prev-hash byte order, BIP34 height, merkle root, witness commitment,
  and the `submitblock` path itself. See
  [`tests/block_acceptance.rs`](tests/block_acceptance.rs) and the
  [`e2e.yml`](.github/workflows/e2e.yml) workflow. A green badge above means the
  full `getblocktemplate` → coinbase → `submitblock` pipeline passed on the
  latest commit.
- **Every PR goes through CI before merge** (fmt, clippy, tests, release
  build). See [`ci.yml`](.github/workflows/ci.yml).
- **Run it yourself.** The regtest harness is one command (see
  [Development](#development)); on regtest you can mine real blocks through the
  pool in seconds. If something breaks there, that's exactly the bug report I
  want pre-1.0. Open an issue.

Short track record is fair to weigh. But the coverage is public, reproducible,
and exercised on every commit, so you can check it rather than trust it.

---

## Features

| Category | Detail |
|---|---|
| Protocol | Stratum V1 (JSON-RPC over TCP) **and** Stratum V2 (Extended Channel, Noise-encrypted), auto-detected per connection on one port |
| ASIC extensions | SV1: `mining.configure` with `version-rolling` (BIP320, mask `1fffe000`), `mining.suggest_difficulty` as a starting difficulty. SV2: extended channel with BIP320 version rolling |
| Auth | `mining.authorize` accepts any worker name (non-empty, no whitespace, up to 128 bytes); the password is ignored |
| Difficulty | Per-miner vardiff with configurable target share time, retarget interval, and max adjustment factor |
| Block template | `getblocktemplate` via Bitcoin RPC, ZMQ `hashblock` push plus an always-on RPC tip poll |
| Coinbase | BIP34 height, configurable tag, SegWit witness commitment, reward to your address |
| Share validation | Header reconstruction, double-SHA256, check against the vardiff floor, duplicate detection, ntime window check |
| Block submission | `submitblock` as soon as a share solves a block, with the raw block archived in parallel. Transient RPC failures are retried inline, then in the background, and an unconfirmed block is resubmitted on the next start |
| Security | Per-IP connection rate limiting, per-session message rate limiting (token bucket), invalid-share counting, message size limit with a short IP ban for oversize messages (rate limits refuse or disconnect but never ban) |
| Metrics | Prometheus endpoint (`/metrics`): hashrate, share counts, block finds, connected miners |
| Logging | Structured JSON or human-readable via `tracing` |

---

## Quick start (Docker)

```bash
# 1. Get a config and set your address + node details
curl -O https://raw.githubusercontent.com/cbyam/solo-pool-rs/main/config.toml.example
mv config.toml.example config.toml
$EDITOR config.toml          # set coinbase_address + bitcoin_rpc

# 2. Run (host networking lets it reach bitcoind's RPC + ZMQ on localhost).
#    The image runs as the non-root user uid:gid 10001, so it reads the cookie
#    via a supplementary group: pass your node group's GID (find it with
#    `stat -c %g "$HOME/.bitcoin/.cookie"`) and set rpccookieperms=group in
#    bitcoin.conf so the cookie is group-readable.
mkdir -p data && sudo chown 10001:10001 data   # persistent state, owned by the image's uid
docker run -d --name solo-pool-rs --network host \
  --group-add "$(stat -c %g "$HOME/.bitcoin/.cookie")" \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  -v "$HOME/.bitcoin/.cookie:/home/solo-pool/.bitcoin/.cookie:ro" \
  -v "$PWD/data:/app/data" \
  ghcr.io/cbyam/solo-pool-rs:latest
```

In `config.toml`, point the three files that must outlive the container at
that volume: `stats_db_path = "data/pool_stats.sqlite"`,
`found_block_dir = "data/found-blocks"`, and under `[sv2]`
`authority_key_file = "data/sv2-authority.key"`. With the example's relative
defaults they land in `/app` and vanish when the container is re-created.

bitcoind writes a new cookie file every time it starts, and a single-file
mount keeps pointing at the old one. After restarting the node, restart the
container too (`docker restart solo-pool-rs`) so it picks up the new cookie.

Or with Compose, see [`docker-compose.yml`](docker-compose.yml). Before the
first start, create `./data` owned by uid 10001 as above and replace the
placeholder GID under `group_add` with your node group's GID:

```bash
docker compose up -d
```

Then open the dashboard at `http://<host>:9090/`.

> **Upgrading from ≤ 0.3.x:** the image now runs as a non-root user (uid:gid
> `10001`) instead of root. Two one-time changes are needed: grant cookie access
> via the node group as shown above (`--group-add` / Compose `group_add`), and if
> you persist data with `-v ./data:/app/data`, make that host dir writable by the
> new uid: `sudo chown -R 10001:10001 ./data`. See [CHANGELOG.md](CHANGELOG.md).

---

## Quick start (from source)

Requires **Rust ≥ 1.90**, a C/C++ toolchain, `pkg-config`, and the SQLite
headers (`apt-get install build-essential pkg-config libsqlite3-dev`). libzmq
is compiled into the binary, so no ZMQ package is needed.

```bash
git clone https://github.com/cbyam/solo-pool-rs
cd solo-pool-rs
cp config.toml.example config.toml   # edit coinbase_address + bitcoin_rpc
cargo build --release
./target/release/solo-pool-rs config.toml
```

The only argument is the config path (`--config <path>` also works). With no
argument the pool reads `config.toml` from the working directory.

Prebuilt Linux binaries are also attached to each [release](https://github.com/cbyam/solo-pool-rs/releases). They link the system SQLite and C++ runtime (`libsqlite3-0` and `libstdc++6`, present on most distributions).

---

## Running as a systemd service

For a bare-metal install alongside your node, a hardened unit is provided at
[`packaging/systemd/solo-pool-rs.service`](packaging/systemd/solo-pool-rs.service).

```bash
# 1. Build and install the binary. The install script copies the built binary
#    to /usr/local/lib/solo-pool-rs/<version>/ and swaps a symlink at
#    /usr/local/bin/solo-pool-rs, so a stray `cargo build` never changes what
#    the service runs.
cargo build --release && sudo packaging/install.sh

# 2. Create a dedicated system user
sudo useradd --system --no-create-home --shell /usr/sbin/nologin solo-pool

# 3. Install the config, readable by the service user only (it may hold an
#    RPC password), then edit it.
sudo install -Dm640 -g solo-pool config.toml /etc/solo-pool-rs/config.toml
#    The unit runs under ProtectSystem=strict with /var/lib/solo-pool-rs as its
#    writable state directory, so set these three paths in the config:
#      stats_db_path      = "/var/lib/solo-pool-rs/pool_stats.sqlite"
#      found_block_dir    = "/var/lib/solo-pool-rs/found-blocks"
#      authority_key_file = "/var/lib/solo-pool-rs/sv2-authority.key"   # under [sv2]

# 4. Give it read access to bitcoind's RPC cookie (pick one):
#    a) add it to the group that can read your node's data dir, set
#       rpccookieperms=group in bitcoin.conf, and set cookie_path to the
#       cookie's real location. The service user has no home directory, so the
#       default ~/.bitcoin/.cookie does not resolve. The group name is whatever
#       you use for node access: bitcoind's own group, or a shared one (e.g.
#       `bitstack` covering CLN/electrum/etc.). Substitute your group below:
sudo usermod -aG <node-group> solo-pool
#         cookie_path = "/path/to/bitcoin/datadir/.cookie"   # under [bitcoin_rpc]
#    b) or use explicit rpcuser/rpcpassword in config.toml (skip the cookie)

# 5. Install the unit and start it
sudo install -Dm644 packaging/systemd/solo-pool-rs.service \
  /etc/systemd/system/solo-pool-rs.service
sudo systemctl daemon-reload
sudo systemctl enable --now solo-pool-rs
journalctl -u solo-pool-rs -f

# Upgrading later: build, install, restart.
cargo build --release && sudo packaging/install.sh && sudo systemctl restart solo-pool-rs
```

To roll back, `sudo packaging/install.sh --rollback` relinks the newest
installed version other than the active one (by version sort, so a leftover
pre-release build such as `0.6.10-dev` outranks `0.6.9`). Check what is
installed with `packaging/install.sh --list`, then restart the service.

The unit starts after `bitcoind.service` but does not wait for its RPC to
answer; a node that is still loading makes the pool exit. systemd retries
every 5 s but gives up after three starts within a minute and leaves the unit
failed, so a node that takes longer than about 15 s to load at boot leaves the
pool down. If that bites, uncomment the `ExecStartPre` readiness probe and
`TimeoutStartSec` in the unit.

Logs go to the journal by default (`log_dir` empty). For files instead, set
`log_dir = "/var/log/solo-pool-rs"`: the unit's `LogsDirectory=` creates that
directory with the right owner, and `log_max_files` (default 14) bounds how
many daily files are kept.

---

## Bitcoin node configuration (`bitcoin.conf`)

```ini
# Required: RPC
server=1
# Cookie auth is on by default; no rpcuser/rpcpassword needed

# Recommended: ZMQ for instant block notifications
zmqpubhashblock=tcp://127.0.0.1:28332

# Allow RPC from localhost (default)
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
```

### Which node build: BIP110/RDTS

The pool copies its block template from the node, so it mines whatever chain
the node follows. Node choice started deciding real money in August 2026:
BIP110 (RDTS) entered mandatory signaling at block 961,632 with under 3% miner
support, and enforcing nodes split onto a minority chain that stalled two
blocks later.

- **Knots 29.3.knots20260507 and earlier**: RDTS off by default. Follows the
  majority chain. This is what the pool is deployed and tested against.
- **Knots 29.3.knots20260508 and later**: RDTS enforcement is mandatory. These
  builds reject the majority chain at 961,632 and follow the stalled minority
  chain. **Do not point the pool at one**: every block found there is worthless
  on the majority chain.
- **Bitcoin Core**: does not implement BIP110. Follows the majority chain.
  Tested in CI as a second target.

If you run Knots for its stricter data-carrier policy, none of this takes that
away. Policy shapes what enters your node's mempool and therefore what goes
into the templates your pool mines, so a non-enforcing Knots still mines
blocks without the data you filter. Only consensus enforcement, rejecting
other miners' blocks, is what strands a node.

As of August 2026 Knots has not announced dropping mandatory enforcement, so
hold node upgrades at 20260507 until a release without it exists. You can
verify which side your node is on yourself: a stranded node shows a frozen tip
height (compare `getblockcount`, or the dashboard's Chain tip card, against any
block explorer).

---

## Configuration

All settings live in `config.toml`. Start from
[`config.toml.example`](config.toml.example), the fully annotated reference:
most keys have no built-in default, so a file holding only the lines below
will not load. These are the ones you will almost certainly change:

```toml
[pool]
listen_addr = "0.0.0.0:3333"
coinbase_address = "bc1qyouraddresshere"   # ← YOUR address
initial_difficulty = 4096                  # ~1 TH/s at 15s/share; vardiff ramps from here

[sv2]
enabled = true                             # accept SV2 on the same port (false = SV1 only)

[bitcoin_rpc]
url = "http://127.0.0.1:8332"
cookie_path = "~/.bitcoin/.cookie"         # default Bitcoin location

[zmq]
hashblock_endpoint = "tcp://127.0.0.1:28332"
poll_fallback = true                       # always-on RPC tip poll alongside ZMQ
```

### Environment overrides

Every scalar value (string, number, boolean) can also be overridden by an
environment variable named `SOLO_POOL_<SECTION>__<KEY>` (double underscore
between section and key), so container platforms can inject
deployment-specific settings without editing the file. List values such as
`[metrics] allowed_hosts` cannot be set this way.

```bash
SOLO_POOL_POOL__COINBASE_ADDRESS=bc1q...        # [pool] coinbase_address
SOLO_POOL_BITCOIN_RPC__URL=http://10.0.0.5:8332 # [bitcoin_rpc] url
SOLO_POOL_BITCOIN_RPC__USER=umbrel              # [bitcoin_rpc] user
SOLO_POOL_SV2__ENABLED=false                    # [sv2] enabled
```

Setting any `SOLO_POOL_SV2__*` variable requires `enabled` to be present
under `[sv2]` in the file (or set through `SOLO_POOL_SV2__ENABLED`).

### Difficulty and small / large miners

`[vardiff]` automatically tracks each miner's hashrate, but it works within a
configured floor and ceiling (`min_difficulty` / `max_difficulty`). The default
floor of **4096** suits roughly **1 TH/s and up** (a Bitaxe, Avalon Nano, or
larger) at the 15 s target share time. Two cases to know about:

- **Low-hashrate devices** (USB sticks, NerdMiner-class lottery miners, ~sub-0.3 TH/s)
  will be pinned at the floor and submit shares slowly, or for very tiny
  devices almost never. This is purely cosmetic: **share difficulty has no
  payout effect in solo mining** (you're paid on blocks, 100%, regardless), so
  such a device still finds and submits a real block normally; it just shows
  little or no hashrate on the dashboard. If you want better telemetry for small
  hardware, lower `min_difficulty`.
- **Fast machines**: the default ceiling of 65536 fits up to about 19 TH/s per
  connection at the 15 s target. Anything faster sits at the ceiling and
  submits shares faster than the target, so raise `max_difficulty` to match.
  Vardiff is per connection, so a farm of small devices needs no change.

Shares are accepted against the floor on every connection, whatever
difficulty vardiff has assigned, so firmware that ignores difficulty changes
keeps mining.

Miners that send `mining.suggest_difficulty` (e.g. AxeOS's "pool difficulty"
field) are honored as a **starting** difficulty, clamped to this floor/ceiling;
vardiff takes over from there. The floor is never crossed, so a suggestion can't
push a miner below the configured share-rate floor.

---

## Pointing your miners at the pool

### Stratum V1 (most ASICs)

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://<your-server-ip>:3333` |
| Worker | any name without spaces (e.g. `rig1.worker1`) |
| Password | anything (ignored) |

Modern firmware (Braiins OS, LuxOS, stock AxeOS) auto-negotiates `mining.configure` and enables BIP320 version-rolling. The pool grants the part of the miner's requested mask that falls within `1fffe000`.

### Stratum V2 (e.g. NerdQAxe++)

SV2 firmware connects to the **same host and port** as SV1. The protocol is auto-detected, so there is no separate listener.

On a NerdQAxe++ (AxeOS ≥ v1.0.37):

| Field | Value |
|---|---|
| Stratum | select **Stratum V2** |
| Encryption | **on** (Noise); authority pubkey optional, see below |
| Host / Port | `<your-server-ip>` : `3333` (same as SV1) |
| Worker | any name without spaces (used as the SV2 `user_identity`) |
| Channel type | **extended** (the firmware default). "Standard" is refused, see below |

The connection is secured with the SV2 **Noise** handshake (pool = responder); the device then opens an **Extended Channel** and is served `NewExtendedMiningJob` + `SetNewPrevHash` from the same `getblocktemplate` pipeline as SV1. Set `enabled = false` under `[sv2]` to refuse SV2 and serve SV1 only.

**Extended channels only.** A miner whose `SetupConnection` sets `REQUIRES_STANDARD_JOBS` (the Bitaxe "standard" channel setting) or `REQUIRES_WORK_SELECTION` is refused with `unsupported-feature-flags`. Bitaxe firmware discards that error payload, so the device log shows only a `msg_type=0x02` reject and a fallback to the next pool. Switch the channel type back to extended. Standard channels are not planned: the extended channel gives the pool full control of the coinbase and lets the device roll version and extranonce itself, which is all a solo miner needs.

**Pool identity (optional pinning).** The pool signs each connection's Noise certificate with a persistent authority key and prints the base58check public key at startup (also shown in the dashboard's Connect modal, and at `GET /api/info`). Miners that support it can pin this key to cryptographically verify they are talking to your pool; miners that leave it unset connect exactly the same, encrypted but without identity verification. The key file (`[sv2] authority_key_file`, default `sv2-authority.key`) is created on first start; `persist_authority_key = false` reverts to a fresh key per process. Both the accept and reject paths are covered by tests that run a real handshake against a pinning SRI initiator, including wrong-key and expired-certificate cases.

---

## Dashboard & metrics

`prometheus_addr` (the example uses `0.0.0.0:9090`) serves the dashboard and
the Prometheus endpoint from one HTTP server. An empty value turns both off.

| Route | Description |
|---|---|
| `GET /` | HTML dashboard: hashrate chart, workers, network difficulty + estimated next-retarget move, probability, market card, uptime (auto-refreshes) |
| `GET /stats` | JSON snapshot of current pool state |
| `GET /history` | JSON hashrate history (`?since=<unix-ts>` for increments) |
| `GET /chart` | Hashrate chart as an ECharts option spec (`?window=36h\|1w\|1m\|6m`) |
| `GET /api/info` | Pool version, stratum port, SV2 status and authority pubkey, network, payout address |
| `GET/POST /api/settings` | Read or change the payout address at runtime (POST is on by default and subject to the host checks below; `[metrics] allow_runtime_settings = false` turns it off) |
| `POST /api/reset-best-hashrate` | Clear the all-time best-hashrate watermark (same guards as the settings POST) |
| `GET /metrics` | Prometheus text exposition |

The two `POST` routes (`/api/settings` and `/api/reset-best-hashrate`) refuse
requests whose `Host` header is a public DNS name, or whose `Origin` names a
different site. This stops DNS rebinding and cross-site form posts from
changing the payout address through a browser on the LAN. IP addresses,
`localhost`, single-label names and local-only suffixes (`.localhost`,
`.local`, `.lan`, `.home`, `.home.arpa`, `.internal`, `.intranet`,
`.private`) work without configuration. If you reach the
dashboard by any other name, such as a Tailscale name, add it to
`[metrics] allowed_hosts`.

Key Prometheus metrics:

| Metric | Description |
|---|---|
| `pool_connected_miners` | Current live connections |
| `pool_shares_accepted_total{worker}` | Valid shares since the process started |
| `pool_shares_rejected_total{reason,worker}` | Rejected shares by reason |
| `pool_blocks_found_total` | 🏆 Blocks found and accepted by the node |
| `pool_hashrate_estimated_hps{worker}` | Per-worker estimated H/s |
| `pool_job_height` | Current template block height |

The full list is in [`docs/stable-surface.md`](docs/stable-surface.md). Any
series not written for 24 hours is dropped from the exposition, which bounds
the `worker` label; a scraper sees an idle worker's series disappear and a
counter such as `pool_blocks_found_total` restart from zero after a quiet
day.

---

## Development

```bash
cargo test                        # run unit + integration tests
SOLO_POOL_LOGGING__LEVEL=debug cargo run -- config.toml
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check

# End-to-end block-acceptance test: boots a real bitcoind -regtest, mines a
# block through the pool over SV1 and over SV2, and requires the node to accept
# it (needs bitcoind + bitcoin-cli on PATH or via $BITCOIND / $BITCOIN_CLI).
cargo test --release --test block_acceptance -- --ignored --nocapture
```

CI runs fmt, clippy, tests, and a release build on every PR and every push to
main; the separate E2E workflow runs the block-acceptance test on the same
triggers and once a week.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, the checks a PR must pass,
commit/PR conventions, a map of the source tree, and how releases are cut.

## Versioning

Versioning follows [SemVer](https://semver.org/). While pre-1.0, breaking
changes bump the **minor** version and everything else bumps the **patch**
version. What 1.0 will promise, and what it will not, is drafted in
[docs/stable-surface.md](docs/stable-surface.md). Changes are recorded in
[CHANGELOG.md](CHANGELOG.md).

---

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option.
