# solo-pool-rs

[![GitHub](https://img.shields.io/github/stars/cbyam/solo-pool-rs)](https://github.com/cbyam/solo-pool-rs)

A solo Bitcoin mining pool written in Rust, targeting ASIC miners via **Stratum V1** with a clean migration path to **Stratum V2** (SRI).

> **Loto mining**: 100% of the block reward goes to your configured address. No fees, no payout splits, no external dependencies beyond a Bitcoin Knots node.

---

## Features

| Category | Detail |
|---|---|
| Protocol | Stratum V1 (JSON-RPC over TCP) |
| ASIC extensions | `version-rolling` (BIP320), `minimum-difficulty`, `subscribe-extranonce`, `mining.configure` |
| Auth | `mining.authorize` (any worker name accepted — solo pool) |
| Difficulty | Per-miner vardiff with configurable target share time, retarget interval, and max adjustment factor |
| Block template | `getblocktemplate` via Bitcoin Knots RPC, ZMQ `hashblock` push (RPC poll fallback) |
| Coinbase | BIP34 height, configurable tag, SegWit witness commitment, reward to your address |
| Share validation | Header reconstruction, double-SHA256, meets-target check, duplicate detection, ntime drift check |
| Block submission | `submitblock` on valid block, immediate with latency logging |
| Security | Per-IP connection rate limiting, per-session share rate limiting (token bucket), invalid share counting, IP ban list with TTL, message size limit |
| Metrics | Prometheus endpoint (`/metrics`) — hashrate, share counts, block finds, connected miners |
| Logging | Structured JSON or human-readable via `tracing` |
| SV2 | Migration shim documented in `src/protocol/sv2_stub.rs` |

---

## Requirements

- **Rust** ≥ 1.75.0
- **Bitcoin Knots** (or Bitcoin Core) with:
  - RPC enabled
  - Cookie auth (default) or explicit `rpcuser`/`rpcpassword`
  - ZMQ enabled (recommended) — see below

---

## Bitcoin Knots / Core configuration (`bitcoin.conf`)

```ini
# Required: RPC
server=1
# Cookie auth is on by default — no rpcuser/rpcpassword needed

# Recommended: ZMQ for instant block notifications
zmqpubhashblock=tcp://127.0.0.1:28332
zmqpubrawtx=tcp://127.0.0.1:28333

# Allow RPC from localhost (default)
rpcbind=127.0.0.1
rpcallowip=127.0.0.1
```

---

## Installation

```bash
git clone https://github.com/cbyam/solo-pool-rs
cd solo-pool-rs
cp config.toml.example config.toml   # edit coinbase_address + bitcoin_rpc settings
cargo build --release
./target/release/solo-pool-rs config.toml
```

---

## Configuration

All settings live in `config.toml`. The most important ones:

```toml
[pool]
coinbase_address = "bc1qyouraddresshere"   # ← YOUR address
initial_difficulty = 4096                  # ~1 TH/s at 15s/share; vardiff ramps from here

[bitcoin_rpc]
url = "http://127.0.0.1:8332"
cookie_path = "~/.bitcoin/.cookie"        # default Bitcoin Knots location

[zmq]
hashblock_endpoint = "tcp://127.0.0.1:28332"
poll_fallback = true                       # falls back if ZMQ unreachable
```

See `config.toml` for the full annotated reference.

---

## Pointing your ASICs at the pool

Configure your ASIC firmware (Braiins OS, stock firmware, etc.):

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://<your-server-ip>:3335` |
| Worker | anything (e.g. `rig1.worker1`) |
| Password | anything (ignored) |

### Version-rolling (BIP320)

Most modern ASICs and firmware (Braiins OS, LuxOS, etc.) will auto-negotiate `mining.configure` and enable version-rolling. No extra configuration needed — the pool advertises mask `1fffe000`.

---

## Metrics

If `prometheus_addr` is set (default `0.0.0.0:9090`), visit:

```
http://<server>:9090/metrics
```

Key metrics:

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
ASICs (SV1)
    │ TCP :3333
    ▼
network/server.rs        — accept loop, IP limits, connection cap
    │ tokio::spawn
    ▼
network/session.rs       — per-miner state machine (subscribe→auth→submit loop)
    │                      vardiff, extension negotiation, share validation
    ▼
mining/validator.rs      — header reconstruction, SHA256d, target comparison
mining/vardiff.rs        — per-session difficulty management
    │
    ▼
mining/engine.rs         — current job store, job history, broadcast channel
    │
    ▼
bitcoin/template.rs      — GBT → StratumJob (coinbase, merkle branch, job ID)
bitcoin/rpc.rs           — Bitcoin Knots RPC (cookie auth, getblocktemplate, submitblock)
bitcoin/zmq.rs           — ZMQ hashblock listener + RPC poll fallback
```

---

## Stratum V2 migration path

When your ASICs support SV2 (or you run a firmware that does):

1. Uncomment the SRI crates in `Cargo.toml`
2. Implement `StratumFrontend` for an SV2 session (see `src/protocol/sv2_stub.rs`)
3. Optionally, run the [SRI Translator Proxy](https://github.com/stratum-mining/sv2-apps/tree/main/miner-apps/translator) to bridge legacy SV1 ASICs to the SV2 listener side-by-side

The mining engine, validator, vardiff, and template code require **no changes** — they're protocol-agnostic.

---

## Development

```bash
# Run tests
cargo test

# Run with debug logging
RUST_LOG=debug cargo run -- config.toml

# Check for issues
cargo clippy -- -D warnings
cargo fmt --check
```

---

## License

MIT OR Apache-2.0
