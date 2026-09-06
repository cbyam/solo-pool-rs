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
- **True solo.** `getblocktemplate` → your coinbase address. No shares database, no PPLNS, no operator fee.
- **Self-contained.** A single Rust binary plus your Bitcoin node. Cookie auth, ZMQ block notifications, RPC-poll fallback.
- **Observable.** A live HTML dashboard (hashrate history, per-worker table, network difficulty + estimated next-retarget move, probability) and a Prometheus endpoint.

---

## Does it actually find and pay blocks? (don't trust, verify)

The fair question for any young pool is *"how do I know a found block actually
gets submitted and pays my address?"* You don't have to take my word for it. The
proof is in the repo and runs on every change:

- **An end-to-end block-acceptance test runs on every PR and every push to main.** It boots a
  real `bitcoind -regtest`, launches the actual pool binary, connects over the
  live Stratum socket exactly as a miner would, grinds a real share that is also
  a valid block, submits it, and **asserts the node accepted it onto the chain
  and that the coinbase pays the pool's configured address**. This is the one
  path no unit test can fake. It guards the bugs that stay invisible until a
  block is genuinely found: prev-hash byte order, BIP34 height, merkle root,
  witness commitment, and the `submitblock` path itself. See
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
| ASIC extensions | SV1: `version-rolling` (BIP320), `minimum-difficulty`, `subscribe-extranonce`, `mining.configure`. SV2: extended channel with BIP320 version rolling |
| Auth | `mining.authorize` (any worker name accepted, solo pool) |
| Difficulty | Per-miner vardiff with configurable target share time, retarget interval, and max adjustment factor |
| Block template | `getblocktemplate` via Bitcoin RPC, ZMQ `hashblock` push (RPC poll fallback) |
| Coinbase | BIP34 height, configurable tag, SegWit witness commitment, reward to your address |
| Share validation | Header reconstruction, double-SHA256, meets-target check, duplicate detection, ntime drift check |
| Block submission | `submitblock` on valid block, immediate with latency logging |
| Security | Per-IP connection rate limiting, per-session share rate limiting (token bucket), invalid-share counting, message size limit with a short IP ban for oversize messages (rate limits refuse but never ban) |
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
docker run -d --name solo-pool-rs --network host \
  --group-add "$(stat -c %g "$HOME/.bitcoin/.cookie")" \
  -v "$PWD/config.toml:/app/config.toml:ro" \
  -v "$HOME/.bitcoin/.cookie:/home/solo-pool/.bitcoin/.cookie:ro" \
  ghcr.io/cbyam/solo-pool-rs:latest
```

Or with Compose, see [`docker-compose.yml`](docker-compose.yml):

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

Requires **Rust ≥ 1.75** and **libzmq** (`apt-get install libzmq3-dev pkg-config`).

```bash
git clone https://github.com/cbyam/solo-pool-rs
cd solo-pool-rs
cp config.toml.example config.toml   # edit coinbase_address + bitcoin_rpc
cargo build --release
./target/release/solo-pool-rs config.toml
```

Prebuilt Linux binaries are also attached to each [release](https://github.com/cbyam/solo-pool-rs/releases) (they need `libzmq5` on the host).

---

## Running as a systemd service

For a bare-metal install alongside your node, a hardened unit is provided at
[`packaging/systemd/solo-pool-rs.service`](packaging/systemd/solo-pool-rs.service).

```bash
# 1. Install the binary and config to standard locations
sudo install -Dm755 target/release/solo-pool-rs /usr/local/bin/solo-pool-rs
sudo install -Dm644 config.toml /etc/solo-pool-rs/config.toml      # then edit it
#    Set stats_db_path = "/var/lib/solo-pool-rs/pool_stats.sqlite" in the config.

# 2. Create a dedicated system user
sudo useradd --system --no-create-home --shell /usr/sbin/nologin solo-pool

# 3. Give it read access to bitcoind's RPC cookie (pick one):
#    a) add it to the group that can read your node's data dir, and set
#       rpccookieperms=group in bitcoin.conf. The group name is whatever you use
#       for node access: bitcoind's own group, or a shared one (e.g. `bitstack`
#       covering CLN/electrum/etc.). Substitute your group below:
sudo usermod -aG <node-group> solo-pool
#    b) or use explicit rpcuser/rpcpassword in config.toml (skip the cookie)

# 4. Install the unit and start it
sudo install -Dm644 packaging/systemd/solo-pool-rs.service \
  /etc/systemd/system/solo-pool-rs.service
sudo systemctl daemon-reload
sudo systemctl enable --now solo-pool-rs
journalctl -u solo-pool-rs -f
```

Logs go to the journal by default (`log_dir` empty); set `log_dir` plus
`LogsDirectory=` in the unit for file logging instead.

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

All settings live in `config.toml`. The essentials:

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
poll_fallback = true                       # falls back if ZMQ unreachable
```

See [`config.toml.example`](config.toml.example) for the fully annotated reference.

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
- **Large miners / farms** can raise `max_difficulty` so vardiff can settle them
  at a higher target instead of submitting shares faster than the 15 s goal.

Miners that send `mining.suggest_difficulty` (e.g. AxeOS's "pool difficulty"
field) are honored as a **starting** difficulty, clamped to this floor/ceiling;
vardiff takes over from there. The floor is never crossed, so a suggestion can't
push a miner below the configured share-rate floor.

Every value can also be overridden by an environment variable named
`SOLO_POOL_<SECTION>__<KEY>` (double underscore between section and key), so
container platforms can inject deployment-specific settings without editing
the file:

```bash
SOLO_POOL_POOL__COINBASE_ADDRESS=bc1q...        # [pool] coinbase_address
SOLO_POOL_BITCOIN_RPC__URL=http://10.0.0.5:8332 # [bitcoin_rpc] url
SOLO_POOL_BITCOIN_RPC__USER=umbrel              # [bitcoin_rpc] user
SOLO_POOL_SV2__ENABLED=false                    # [sv2] enabled
```

---

## Pointing your miners at the pool

### Stratum V1 (most ASICs)

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://<your-server-ip>:3333` |
| Worker | anything (e.g. `rig1.worker1`) |
| Password | anything (ignored) |

Modern firmware (Braiins OS, LuxOS, stock AxeOS) auto-negotiates `mining.configure` and enables BIP320 version-rolling; the pool advertises mask `1fffe000`.

### Stratum V2 (e.g. NerdQAxe++)

SV2 firmware connects to the **same host and port** as SV1. The protocol is auto-detected, so there is no separate listener.

On a NerdQAxe++ (AxeOS ≥ v1.0.37):

| Field | Value |
|---|---|
| Stratum | select **Stratum V2** |
| Encryption | **on** (Noise); authority pubkey optional, see below |
| Host / Port | `<your-server-ip>` : `3333` (same as SV1) |
| Worker | anything (used as the SV2 `user_identity`) |

The connection is secured with the SV2 **Noise** handshake (pool = responder); the device then opens an **Extended Channel** and is served `NewExtendedMiningJob` + `SetNewPrevHash` from the same `getblocktemplate` pipeline as SV1. Set `enabled = false` under `[sv2]` to refuse SV2 and serve SV1 only.

**Pool identity (optional pinning).** The pool signs each connection's Noise certificate with a persistent authority key and prints the base58check public key at startup (also shown in the dashboard's Connect modal, and at `GET /api/info`). Miners that support it can pin this key to cryptographically verify they are talking to your pool; miners that leave it unset connect exactly the same, encrypted but without identity verification. The key file (`[sv2] authority_key_file`, default `sv2-authority.key`) is created on first start; `persist_authority_key = false` reverts to a fresh key per process. Both the accept and reject paths are covered by tests that run a real handshake against a pinning SRI initiator, including wrong-key and expired-certificate cases.

---

## Dashboard & metrics

With `prometheus_addr` set (default `0.0.0.0:9090`), an HTTP server exposes:

| Route | Description |
|---|---|
| `GET /` | HTML dashboard: hashrate chart, workers, network difficulty + estimated next-retarget move, probability, market card, uptime (auto-refreshes) |
| `GET /stats` | JSON snapshot of current pool state |
| `GET /history` | JSON hashrate history (`?since=<unix-ts>` for increments) |
| `GET /chart` | Hashrate chart as an ECharts option spec (`?window=36h\|1w\|1m\|6m`) |
| `GET /api/info` | Pool version, stratum port, SV2 status and authority pubkey |
| `GET/POST /api/settings` | Read or change the payout address at runtime (POST requires `[metrics] allow_runtime_settings = true` and a local `Host`, see below) |
| `GET /metrics` | Prometheus text exposition |

The two `POST` routes (`/api/settings` and `/api/reset-best-hashrate`) refuse
requests whose `Host` header is a public DNS name, or whose `Origin` names a
different site. This stops DNS rebinding and cross-site form posts from
changing the payout address through a browser on the LAN. IP addresses,
`localhost`, single-label names and local-only suffixes (`.local`, `.lan`,
`.home.arpa`, `.internal`) work without configuration. If you reach the
dashboard by any other name, such as a Tailscale name, add it to
`[metrics] allowed_hosts`.

Key Prometheus metrics:

| Metric | Description |
|---|---|
| `pool_connected_miners` | Current live connections |
| `pool_shares_accepted_total` | Lifetime valid shares |
| `pool_shares_rejected_total{reason}` | Rejected shares by reason |
| `pool_blocks_found_total` | 🏆 Blocks found and submitted |
| `pool_hashrate_estimated_hps{worker}` | Per-worker estimated H/s |
| `pool_job_height` | Current template block height |

---

## Architecture

```
ASIC / Bitaxe (SV1 or SV2)
    │ TCP :3333  (protocol auto-detected from first byte)
    ▼
network/server.rs        - accept loop, IP limits, connection cap
    │
    ├── SV1 ──▶ network/session.rs   - subscribe→auth→submit state machine
    │                                  vardiff, extension negotiation
    └── SV2 ──▶ protocol/sv2/         - Noise handshake, extended channel,
                                        NewExtendedMiningJob / SetNewPrevHash
    │
    ▼
mining/validator.rs      - header reconstruction, SHA256d, target comparison
mining/vardiff.rs        - per-session difficulty management
mining/engine.rs         - current job store, job history, broadcast channel
    │
    ▼
bitcoin/template.rs      - GBT → StratumJob (coinbase, merkle branch, job ID)
bitcoin/rpc.rs           - Bitcoin RPC (cookie auth, getblocktemplate, submitblock)
bitcoin/zmq.rs           - ZMQ hashblock listener + RPC poll fallback

network/dashboard.rs     - HTTP :9090 - dashboard, /stats JSON, /metrics
```

The mining engine, validator, vardiff, and template code are protocol-agnostic; SV1 and SV2 share the same job pipeline.

---

## Development

```bash
cargo test                        # run unit + integration tests
RUST_LOG=debug cargo run -- config.toml
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check

# End-to-end block-acceptance test: boots a real bitcoind -regtest, mines a
# block through the pool, and requires the node to accept it (needs bitcoind +
# bitcoin-cli on PATH or via $BITCOIND / $BITCOIN_CLI).
cargo test --release --test block_acceptance -- --ignored --nocapture
```

CI runs fmt, clippy, tests, and a release build on every PR and every push to
main; the separate E2E workflow runs the block-acceptance test on the same
triggers.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, the checks a PR must pass, and
commit/PR conventions.

## Releasing

Versioning follows [SemVer](https://semver.org/). While pre-1.0, breaking
changes bump the **minor** version and everything else bumps the **patch**
version. Changes are recorded in [CHANGELOG.md](CHANGELOG.md) under
`[Unreleased]` as they merge.

To cut a release (e.g. `v0.3.1`):

```bash
# 1. Promote the changelog: rename [Unreleased] -> [0.3.1] - <date>, add a fresh
#    empty [Unreleased], and update the compare links at the bottom.

# 2. Bump the version in Cargo.toml, then refresh Cargo.lock.
#    edit Cargo.toml:  version = "0.3.1"
cargo build

# 3. Commit the version bump + changelog together.
git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "release: v0.3.1 - <one-line summary>"

# 4. Tag and push. The tag is what triggers the release automation.
git tag -a v0.3.1 -m "v0.3.1"
git push && git push origin v0.3.1
```

Pushing a `v*` tag triggers two workflows automatically:

- **`release.yml`** builds Linux x86_64 and aarch64 binaries on native runners,
  packages a tarball per architecture (binary + `config.toml.example` + README),
  and publishes a GitHub Release with auto-generated notes.
- **`docker.yml`** builds and pushes a multi-arch (amd64 + arm64) image to
  `ghcr.io/cbyam/solo-pool-rs:<tag>`.

So the only manual steps are the changelog promotion, the version bump, and the
tag push. CI produces the artifacts and the GitHub Release.

---

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option.
