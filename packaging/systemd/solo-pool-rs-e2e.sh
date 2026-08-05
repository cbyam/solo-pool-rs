#!/usr/bin/env bash
#
# Scheduled local block-acceptance run.
#
# Why this exists alongside the GitHub workflow: CI proves the code is portable,
# this proves it still works against *your* node. Consensus behaviour lives in
# bitcoind, not in this repo, so a Knots upgrade or a rule activation (BIP110 /
# RDTS) can invalidate block construction without a single line here changing.
# CI would stay green through that. This would not.
#
# Runs entirely in regtest against a throwaway datadir. It never touches mainnet
# state, the production config, or the stats database.
#
# IMPORTANT: builds into a private CARGO_TARGET_DIR. /usr/local/bin/solo-pool-rs
# is a symlink into the repo's target/release, so building there would silently
# replace the binary the live pool starts on its next restart. Do not remove the
# override without fixing that first.

set -euo pipefail

REPO="${REPO:-/opt/solo-pool-rs}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/var/tmp/solo-pool-rs-e2e-target}"
export CARGO_TERM_COLOR=never

cd "$REPO"

# Pin to the node the pool is actually deployed against, not whatever is on PATH,
# so the result says something about production rather than about this host.
export BITCOIND="${BITCOIND:-$(command -v bitcoind)}"
export BITCOIN_CLI="${BITCOIN_CLI:-$(command -v bitcoin-cli)}"

echo "repo            : $REPO ($(git -C "$REPO" rev-parse --short HEAD))"
echo "target dir      : $CARGO_TARGET_DIR"
echo "node under test : $("$BITCOIND" -version | head -1)"

# --ignored: the block-acceptance test is opt-in because it needs a real node.
cargo test --release --test block_acceptance -- --ignored --nocapture
