# FAQ

Answers to questions operators have asked, and to each warning the dashboard
can show. Config keys refer to `config.toml`; the annotated reference is
[`config.toml.example`](../config.toml.example).

## My Bitaxe falls back to its backup pool on SV2

The device is set to a standard channel. The pool serves extended channels
only, so it refuses a `SetupConnection` that requires standard jobs, with
`unsupported-feature-flags`. Bitaxe firmware discards the reason, so its log
shows only a `msg_type=0x02` reject before it moves to the next pool.

Set the channel type back to extended, the firmware default. SV2 details are
in the [README](../README.md#stratum-v2-eg-nerdqaxe).

## My small miner shows little or no hashrate

Every connection is kept between `[vardiff] min_difficulty` and
`max_difficulty`. A device too slow to reach one share per 15 s at the floor
sits there and submits less often. That changes nothing about payout: a
block pays in full whichever device finds it.

What can bite is `[pool] idle_timeout_secs` (default 300). A miner that sends
nothing for that long is disconnected, and it reconnects on its own. A
device's share interval at the floor is `min_difficulty × 2^32 / hashrate`
seconds. At the example floor of 512 that stays under 300 s down to about
10 GH/s. For anything slower, lower `min_difficulty` or raise
`idle_timeout_secs` above that interval.

## A miner is marked degraded

The status light turns amber when a connected miner with an established
share rate goes quiet for five times its usual interval (at least two
minutes). The pool has heard nothing from it, so look at the device: its own
web page, temperature, fan, or network link. A miner that overheats and
throttles often shows up here first.

## A miner is marked rejecting

The dashboard judges each miner's rejects over the last hour, in two kinds.

Stale shares are work that arrived after a new block made its job obsolete.
Some are unavoidable, because a device needs a moment to switch jobs, so they
are judged as a rate once the miner has sent about 200 shares in the hour:
amber at 1%, red at 2%. Under 0.5% is healthy on a LAN. A miner well above
its neighbours on the same hardware is slow to receive or switch jobs; check
Wi-Fi, a busy switch port, or a firmware difference.

Every other reject is a device fault that healthy hardware never produces.
The first one turns the miner amber; three or more that also make up 1% of
its shares turn it red.

| Reject | Usual cause |
|---|---|
| Invalid | failing chip or hashboard, or an overclock the device cannot hold |
| Duplicate | firmware bug |
| Low diff | firmware ignoring the difficulty it agreed to at connect |
| Bad extranonce | firmware or config mismatch; the miner's log says "Wrong extranonce2 size: got N bytes, expected M" |

Five invalid, duplicate or bad-extranonce shares on one connection also
disconnect it (`[security] max_invalid_shares`).

The flag clears on its own once a clean hour has passed. The worker row then
shows "recovered" with the age of the last fault for a day.

## Stats not saving

The stats database (`[metrics] stats_db_path`) failed to open at startup, or
a write to it failed. The pool keeps mining without it. What is at risk is
the found-block list, the current round and the best shares, which will not
survive a restart until this is fixed. The hover text on the pill carries
the database's own error.

Common causes:

- The service user cannot write the file or its directory. Under the
  systemd unit the path must be in `/var/lib/solo-pool-rs`; in Docker it
  must be in the `data/` volume, owned by uid 10001.
- The disk is full.
- Another program holds a write lock, such as an open `sqlite3` session.
  The pool waits up to 5 seconds for a lock before a write fails.

A write failure clears by itself once a later write succeeds. A failure at
startup stays until you fix the cause and restart the pool.

## Node stale

The pool has not built a job from a fresh block template for 90 seconds
(amber) or 5 minutes (red). Miners keep working on the old template, so a
new block on the network goes unnoticed until the node answers again. The
Network section shows the error the node returned.

- bitcoind is stopped, restarting, or still loading after a restart.
  `bitcoin-cli getblockchaininfo` on the pool host shows which.
- The RPC credentials changed. The pool re-reads the cookie file when it
  changes, but a Docker container with the cookie mounted as a single file
  keeps the old one: restart the container after restarting the node.

## Mining paused

The payout address is not valid for the network the node is on, for example
a testnet address with a mainnet node. The pool builds no jobs until that is
fixed, so miners receive no work. Set a valid address in Settings or in
`[pool] coinbase_address`. An address saved from Settings takes precedence
over the config file on later starts.

## Pinning the pool's identity on an SV2 miner

The pool's authority public key is printed at startup, shown in the
dashboard's Connect dialog, and returned by `GET /api/info` as
`sv2_authority_pubkey`. Enter it on the miner as the pool (authority) public
key. The miner then refuses any server that cannot prove it holds the
matching private key.

The key lives in `[sv2] authority_key_file`, so keep that file in persistent
storage (`data/` in Docker, `/var/lib/solo-pool-rs` under systemd). Deleting
it, or setting `persist_authority_key = false`, gives the pool a new
identity, and pinned miners refuse to connect until they are given the new
key. Miners without a pinned key connect either way.

## What are my odds of finding a block

The Block odds card shows your chance per day and per month and the expected
wait. The expected wait is the network hashrate divided by yours, times ten
minutes. At 30 TH/s against a 940 EH/s network that is about 600 years on
average. Each block is an independent draw, so a find can come at any time,
and time already spent mining does not bring the next one closer.

## Which version am I running

`solo-pool-rs --version`, the dashboard footer, or `version` in
`GET /api/info`.
