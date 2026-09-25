# Contributing to solo-pool-rs

Thanks for your interest in improving solo-pool-rs! This guide covers how to set
up, the conventions the project follows, what a pull request needs to pass, how
the source is laid out, and how releases are cut.

## Reporting issues

- **Bugs and feature requests:** open a GitHub issue. Include your version
  (the release tag, the dashboard footer, or the `starting version=` line in
  the log), relevant config (redact your address/credentials), and log
  excerpts.
- **Security vulnerabilities:** do **not** open a public issue. Follow the
  private reporting process in [SECURITY.md](SECURITY.md).

## Development setup

You need a Rust toolchain, a C/C++ compiler, `pkg-config`, and the SQLite
headers. `rusqlite` links the system SQLite; libzmq is compiled from source by
`zmq-sys` and linked statically, so no ZMQ package is needed.

```bash
# Debian/Ubuntu
sudo apt-get install -y build-essential pkg-config libsqlite3-dev

# build & test
cargo build
cargo test --all
SOLO_POOL_LOGGING__LEVEL=debug cargo run -- config.toml
```

The log filter comes from `[logging] level` in the config (`RUST_LOG` is not
read), so the environment override above is the quick way to raise it.

The minimum supported Rust version (MSRV) is **1.90** (edition 2021). CI builds
on the latest stable only, so the MSRV is not tested.

## Before you open a PR

CI runs the following on every pull request, with `RUSTFLAGS=-D warnings`. All
must pass, so run them locally first:

```bash
cargo fmt --all -- --check                               # formatting
cargo clippy --all-targets --all-features -- -D warnings  # lints
cargo test --all                                         # tests
cargo build --release                                    # release build
```

The E2E workflow also runs the regtest block-acceptance test on every pull
request (see the Development section of the [README](README.md#development)
for the local command), and `cargo audit` / `cargo deny` run when a PR touches
`Cargo.toml`, `Cargo.lock`, or `deny.toml`.

## Commit messages

The project uses [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <summary>
```

Types in use here include `feat`, `fix`, `docs`, `test`, `chore`, `ci`,
`build`, `deps`, and `release`. Examples from the history:

- `fix(jobs): seed job-id high bits per process to avoid post-restart stale mislabel`
- `feat(dashboard): embed favicon, serve at /favicon.ico`
- `docs: real security policy (supported versions, private reporting, scope)`

Keep the summary in the imperative mood and explain the *why* in the body when
it isn't obvious.

## Pull request workflow

1. Branch off `main` (e.g. `fix/stale-share-label`, `feat/sv2-noise`, or
   `chore/dependabot-group-exclusions`).
2. Make your change, keeping commits focused.
3. Add an entry under the `[Unreleased]` section of
   [CHANGELOG.md](CHANGELOG.md) describing the change (Added / Changed / Fixed).
4. Push your branch and open a PR against `main`. Describe what changed and how
   you tested it.
5. Make sure CI is green. Once reviewed and approved, the PR is merged into
   `main`.

Releases are cut separately from merges. See [Releasing](#releasing) below.

## Architecture

```
ASIC / Bitaxe (SV1 or SV2)
    │ TCP :3333  (protocol auto-detected from first byte)
    ▼
network/server.rs        - accept loop, IP limits, connection cap
    │
    ├── SV1 ──▶ network/session.rs   - subscribe→auth→submit state machine
    │           protocol/sv1.rs        vardiff, extension negotiation, message codec
    └── SV2 ──▶ protocol/sv2/         - Noise handshake, extended channel,
                                        NewExtendedMiningJob / SetNewPrevHash
    │
    ▼
mining/validator.rs      - header reconstruction, SHA256d, target comparison
mining/vardiff.rs        - per-session difficulty management
mining/engine.rs         - current job store, job history, broadcast channel,
                           found-block submit, archive, retry and boot replay
    │
    ▼
bitcoin/template.rs      - GBT → StratumJob (coinbase, merkle branch, job ID)
bitcoin/rpc.rs           - Bitcoin RPC (cookie auth, getblocktemplate, submitblock)
bitcoin/zmq.rs           - ZMQ hashblock listener + always-on RPC tip poll

network/dashboard.rs     - HTTP (prometheus_addr): dashboard, /stats JSON, /api/*, /metrics
stats.rs                 - in-memory pool stats and the SQLite stats store
metrics/mod.rs           - Prometheus metric names and recorder
security/mod.rs          - ban list, rate limits, worker-name checks
config.rs, settings.rs   - config file + env overrides; runtime payout address
```

The mining engine, validator, vardiff, and template code are protocol-agnostic;
SV1 and SV2 share the same job pipeline.

## Releasing

Changes are recorded in [CHANGELOG.md](CHANGELOG.md) under `[Unreleased]` as
they merge. To cut a release (e.g. `v0.3.1`):

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
- **`docker.yml`** builds and pushes a multi-arch (amd64 + arm64) image
  tagged `X.Y.Z`, `X.Y` and `latest` (no `v` prefix; `v0.6.7` becomes
  `ghcr.io/cbyam/solo-pool-rs:0.6.7`). Every push to `main` also publishes
  `:edge`.

Both workflows treat a pre-release tag such as `v1.0.0-rc.1` like any other:
`:latest` moves to it and the GitHub Release is not marked as a pre-release.
Only the `X.Y` tag is skipped.

So the only manual steps are the changelog promotion, the version bump, and the
tag push. CI produces the artifacts and the GitHub Release. After the image is
published, bump the pin in the
[community app store](https://github.com/cbyam/umbrel-app-store) to the new
version and its index digest.

## License

By contributing, you agree that your contributions will be dual-licensed under
the same terms as the project: [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
