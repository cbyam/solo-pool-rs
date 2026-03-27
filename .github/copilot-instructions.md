# GitHub Copilot instructions for solo-pool-rs

## Purpose
This file is the workspace-level instruction set for Copilot-style AI agents navigating the `solo-pool-rs` repository. It is meant to be concise, authoritative, and to complement rather than duplicate the existing `README.md`.

## Workflow for tasks
1. Use `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check` as first validation tools.
2. Read the `README.md` architecture section (Stratum V1, mining engine, validator, vardiff, bitcoin RPC/ZMQ) before modifying related code.
3. Prefer preserving behavior and compatibility with Bitcoin Knots/Bitcoin Core RPC+ZMQ config.
4. For protocol changes, keep core validator/engine paths independent of networking (`src/mining/*`, `src/bitcoin/*`, `src/protocol/*`, `src/network/*`).

## Key conventions
- Rust edition 2021. Minimum toolchain: `rustc >= 1.75.0`.
- Configuration is in `config.toml` and `config.toml.example`.
- Logging is via `tracing` + structured output.
- Metrics are Prometheus at `/metrics` as described in the README.
- Stratum V2 migration path is stubbed at `src/protocol/sv2_stub.rs`.

## Quick pointers
- Build: `cargo build --release`
- Run: `RUST_LOG=debug cargo run -- config.toml`
- Tests: `cargo test`
- Lints: `cargo clippy -- -D warnings`

## Link, don't embed
- Rather than copy-paste full config or architecture text, always refer to README sections:
  - `README.md`: sections "Architecture", "Installation", "Development".

## Example prompts for the agent
- "Implement a new per-worker metric `pool_worker_rejected_shares_total` in the metrics module and wire it to share acceptance logic." 
- "Add a command-line option to disable ZMQ fallback and configure `bitcoin/zmq.rs` accordingly." 
- "Inspect and refactor `mining/vardiff.rs` to remove duplicate code paths while keeping behaviour unchanged."

## Next agent customization lanes
- `create-agent` for `solo_pool_rs_maintenance` (routine bugfix + tests)
- `create-hook` for `cmd:verify-config` (check config validity before app startup)

