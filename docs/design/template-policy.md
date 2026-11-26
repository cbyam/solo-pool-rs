# Template policy

**Status: observe mode built, enforce mode designed.** Phase 0 (the
classifier crate and the mainnet audit) merged in #147 and phase 1 (observe
mode) in #190, both on 2026-11-25. Observe mode is unreleased on `main` and
ships as an experimental feature in 1.1. Phase 2 (enforce mode) is design
only.

## Goal

Let the operator decide what goes into the blocks the pool mines, without
depending on which Bitcoin node sits behind it. Today the only way to keep
inscriptions, large OP_RETURNs and token protocols out of the pool's blocks is
to run Bitcoin Knots, whose relay policy filters them before they reach the
template. If Knots stops being maintained in its current form, that choice goes
with it. Moving the policy into the pool keeps it under the operator's control
whatever node they run.

The pool takes no stance of its own. The policy is off by default, so an
operator who wants the node's template as it is (most Umbrel installs) sees no
change.

## The narrow-only rule

The pool can remove transactions from what the node offers. It can never add a
transaction the node refused. This sets what "node-agnostic" means:

| Operator wants | Node | Policy |
|---|---|---|
| Filtered blocks | A permissive node (Bitcoin Core) | on |
| Everything the node accepts | Any | off |
| Filtered blocks, filtered relay | Knots (or another filtering node) | optional; it finds little to remove |

Relay stays the node's business. With the policy on, a Core node still relays
the transactions the pool declines to mine. An operator who also wants to stop
relaying them needs a filtering node.

## Rules

Each rule has the meaning and the default of a Knots option, so a Knots user
moving to Core can carry their policy across and get the same blocks. The keys
follow this config's own structure (snake_case, in one section) rather than
Knots' spelling; the table maps each one. The definitions and their test
vectors come from Bitcoin Roots (`src/policy/policy.cpp`, MIT licensed, on a
Core v31.1 base), which reimplements the Knots behaviour with the same option
names.

The pool implements only what Knots enforces and a Core v31.1 node does not.
Everything Core already rejects (non-standard scripts, the 80-byte tapscript
and P2WSH stack item limits, the 1,650-byte scriptSig limit, dust) never
reaches a Core template, so the pool does not repeat it. The set below comes
from comparing Roots' policy code with Core v31.1's.

| Key | Knots / Roots option | Core 31.1 | Rejects (Knots reason) |
|---|---|---|---|
| `datacarrier_size` | `-datacarriersize` (83) | same option, default 100,000 | OP_RETURN scripts totalling more than the limit (`datacarrier`) |
| `permit_multiple_op_return` | none: Knots always rejects | allows several | more than one OP_RETURN output (`multi-op-return`) |
| `accept_non_std_datacarrier` | `-acceptnonstddatacarrier` (0) | none | any data embedded in an executed script: `OP_FALSE OP_IF ... OP_ENDIF` envelopes (inscriptions), data pushes followed by `OP_DROP`, Olga, OPNet (`txn-datacarrier-nonstandard`) |
| `datacarrier_full_count` | `-datacarrierfullcount` (1) | none | OP_RETURN and embedded data together above `datacarrier_size` (`txn-datacarrier-exceeded`) |
| `max_script_size` | `-maxscriptsize` (1650) | none for witnesses | a witness stack serialising to more than the limit (`witness-size`); also output scripts and scriptSigs above it |
| `reject_tokens` | `-rejecttokens` (0) | none | Runes (`OP_RETURN OP_13` plus a push, `tokens-runes`) and Olga (data in P2WSH outputs, `tokens-olga`) |
| `reject_parasites` | `-rejectparasites` (1) | none | parasitic overlay protocols; in Roots' implementation, CAT-21 (`nLockTime == 21`, `parasite-cat21`) |
| `permit_bare_multisig` | `-permitbaremultisig` (0) | same option, default 1 | bare multisig outputs (Stamps, `bare-multisig`) |
| `permit_bare_pubkey` | `-permitbarepubkey` (0) | allowed | bare pay-to-pubkey outputs (`bare-pubkey`) |
| `permit_bare_datacarrier` | `-permitbaredatacarrier` (0) | allowed | transactions whose only outputs are data carriers or dust (`bare-datacarrier`) |

Knots 29.3 checks `-datacarriersize` per output, inside `IsStandard`, and
reports an oversize OP_RETURN as the generic `scriptpubkey`. The `datacarrier`
reason and the limit summed over all outputs are Roots' (and Core 30's). The
verdicts differ only when several OP_RETURN outputs are allowed, which Knots
never does.

Defaults in parentheses are from `bitcoind -help` on Knots 29.3. Where Core has
the same option, the rule follows Knots' meaning and default, not Core's.
`permit_multiple_op_return` has no Knots option; it exists so an operator can
choose Core's behaviour, and its default is Knots' fixed one. Knots'
`-datacarrier=0` is the same as `datacarrier_size = 0`.

With the defaults, two rules carry most of the weight against inscriptions:
`accept_non_std_datacarrier = false` declines any envelope at all, and
`max_script_size` declines any witness above 1,650 bytes whatever it contains.

A transaction whose parent in the template is rejected is rejected with it,
whatever its own content.

### The executed-script problem

The witness-size rule needs only the transaction. The embedded-data rules need
the script an input executes, and Knots finds it by looking up the coin the
input spends: the coin's scriptPubKey says whether the input is P2TR, P2WSH or
P2SH, and so which witness item or scriptSig push is the script. The pool has
the transaction but not the coin. Two ways to close that gap:

- **Witness shape (start here).** After removing an annex (a last witness item
  starting `0x50`, when there are at least two items), a last item of 33+32m
  bytes (m ≤ 128) whose first byte is `0xc0` or `0xc1` is a taproot control
  block, and the item before it is the tapscript. Otherwise the last witness
  item is a P2WSH witness script, and the last scriptSig push is a P2SH redeem
  script. No lookups, so it costs nothing. The shape rule must recognise key
  spends explicitly: an ordinary P2WPKH public key parsed as a script can look
  like embedded data, and treating it as one would decline normal payments.
  The classifier's tests carry explicit vectors for this: P2WPKH spends and
  taproot key-path spends (with and without an annex) must pass every rule.
- **Coin lookup.** Exact, but needs `getrawtransaction` for each spent coin
  (which needs `txindex=1`; a default Umbrel node does not have it) and a cache
  keyed by txid. Add it only if the phase 0 audit shows the shape rule
  disagreeing with the exact answer often enough to matter.

## Mechanism

### Deprioritise, so the node refills the space

Removing transactions from a template after the fact leaves their space empty:
GBT returns only what the node picked. Instead, the pool tells the node to skip
them, and the node's own block builder fills the space with the next-best
transactions.

`prioritisetransaction <txid> 0 <delta>` adjusts the fee the node uses when it
builds blocks, without changing the transaction or the fee it pays. A large
negative delta gives the transaction a negative effective feerate:

- The Core v31.1 block builder takes transaction chunks from the highest
  feerate down and stops at the first one below `blockmintxfee`
  (`node/miner.cpp:298`), so a negative-feerate transaction is never selected.
- A child that pays for a deprioritised parent can only be selected together
  with that parent, so their combined negative feerate keeps the child out too.
- The coinbase value is the sum of the *actual* fees (`node/miner.cpp:265-270`),
  so the template stays correct without the pool adjusting anything.
- Knots and Roots have the same RPC.

The pool uses one distinctive delta (for example exactly −1,000,000,003 sats)
so it can tell its own adjustments from any the operator set by hand.

**Regtest result** (2026-10-16, a 40,000-weight block, 150 ordinary
transactions at 2-14 sat/vB, 12 unwanted 80-byte OP_RETURN transactions at 50
sat/vB, 3 children at 100 sat/vB paying for unwanted parents; only the 12
parents were deprioritised):

| | Core 31.1 | Knots 29.3 |
|---|---|---|
| Node template, untouched | 51 txs, 31,991 WU, 15 unwanted | 32 txs, 31,584 WU, 15 unwanted |
| Unwanted removed after the fact | 36 txs, 20,196 WU | 17 txs, 15,300 WU |
| Node template after deprioritising | 57 txs, 31,977 WU, 0 unwanted | 35 txs, 31,500 WU, 0 unwanted |

On both nodes the deprioritised template was full, held none of the children,
had a coinbase value equal to subsidy plus actual fees, passed proposal mode,
and was accepted by `submitblock`.

### Where the pool learns about transactions

- **ZMQ `rawtx`**, when the node publishes it: the pool classifies each
  transaction as it enters the mempool, so it is deprioritised before it can
  appear in a template.
- **The template itself**, as the fallback: a new unwanted transaction appears
  in one template, the backstop removes it there and deprioritises it, and it is
  gone from the next refresh on.
- **A sweep of the mempool** when enforce mode starts and after the node
  restarts. `rawtx` covers only new arrivals, and the template fallback learns
  the existing mempool one template at a time: with the deployed settings
  about 40% of weight is declinable, so without a sweep the first templates
  after turning enforce on run well short of full. The sweep reads
  `getrawmempool` and fetches and classifies each transaction it has not seen,
  off the engine task and with the batching rules under Latency.

`rawtx` has a cost at new tips. The node publishes every transaction of a
connected block on it as well (`BlockConnected` in
`zmq/zmqnotificationinterface.cpp` calls `NotifyTransaction` for each), a few
thousand messages that land at the moment the new-tip job matters most. The
pool skips those without classifying them: a transaction already in a block
needs no delta. The same publisher delays the node's `hashblock` notification
by seconds when its queue is busy (see `ZMQ_GRACE_SECS` in
`src/bitcoin/zmq.rs`), so enforce mode requires `zmq.poll_fallback = true`
and refuses to start without it.

### The backstop filter

The pool still filters every template it receives. Normally it finds nothing.
It catches what arrived between deprioritising and the GBT call, including the
GBT cache window below. When it removes something it must:

1. Remove every transaction that depends on a removed one. GBT lists parents
   before children, and `depends` holds 1-based indices of earlier entries;
   reject an index that is out of range or points forward.
2. Lower `coinbasevalue` by the removed transactions' `fee`, with a checked
   subtraction. Claiming 1 sat too much gets the block rejected with
   `bad-cb-amount`.
3. Recompute the witness commitment from the remaining wtxids (GBT's `hash`
   field; the coinbase counts as 32 zero bytes): double SHA-256 of the witness
   merkle root followed by a 32-byte zero reserved value, written as the script
   `6a24aa21a9ed` + commitment. This replaces `default_witness_commitment`,
   which `build_coinbase` copies as is today.

Weight and sigop limits cannot be broken by removing transactions.

### Proposal check

`getblocktemplate {"mode":"proposal","data":<block>}` has the node fully
validate a block without proof of work and returns `null` or a rejection
reason.

- Run it only on templates the pool changed. GBT already validates its own
  template before returning it (`node/miner.cpp:223`, on by default), so
  checking an untouched one repeats work the node just did.
- Serialize the coinbase **with** its witness (one 32-byte zero item) for the
  proposal. The pool's blocks omit it today because `submitblock` adds it
  (`rpc/mining.cpp:1081` in Core v31.1, `UpdateUncommittedBlockStructures`).
  Proposal mode does not, so without it every proposal with a witness
  commitment fails `bad-witness-nonce-size`.
- On failure, stop handing out that template and raise an alert. The fallback
  is a setting: `coinbase_only` (always valid, keeps the policy, earns no fees;
  the recommended default) or `unfiltered` (the node's own template: keeps the
  fees, includes the declined transactions).

### Version bits

The pool mines the `version` GBT returns, unchanged (`src/bitcoin/template.rs`,
`version: gbt.version`). Deprioritising changes which transactions the node
picks, not the header, so the version and any deployment signalling stay the
node's under this design. The deployed node's templates carry `0x20000000`
(`template_version` in `/stats`).

If a later design has the pool set the version itself, two rules apply:

- Keep bits 13 to 28 clear. BIP 320 reserves them for miners to roll. The
  validator substitutes the rolled bits into the base version
  (`resolve_version` in `src/mining/validator.rs`, `(base & !mask) | (bits &
  mask)`), so a set bit there does not break validation. It does not survive
  either: inside the negotiated mask the miner's bit replaces it, so a signal
  placed there can be missing from a found block. Firmware that ORs its rolled
  bits into the version and reports them with XOR (AxeForge before v1.3.2;
  ESP-Miner until bitaxeorg/ESP-Miner#2041 ships) breaks outright: against a
  base with a bit set in that range, its shares fail validation.
- Signalling becomes a pool setting. Any signal bit is an explicit config
  value, outside the rolling range. The dashboard does not show the version
  today (`template_version` is in `/stats` only), so this needs a new
  dashboard element for the version and the signal. Take the bit from the
  deployment's BIP, never from memory.

### Delta bookkeeping

The node clears a delta only when a block confirms its transaction or a
transaction that conflicts with it (`removeForBlock` and the `removeConflicts`
it calls, `txmempool.cpp:704-743` in Knots 29.3). A delta for a transaction
that expires, is evicted, or is replaced in the mempool stays on record, and
deltas survive restarts in `mempool.dat`. The replacement case was confirmed
on regtest on 2026-11-14 on Core 31.1 and Knots 29.3: after the replacement the
entry shows `in_mempool: false` and stays until the original confirms. So:

- A periodic cleanup lists `getprioritisedtransactions` and cancels, with the
  opposite delta, every entry carrying the pool's delta whose `in_mempool` is
  false.
- At startup with the policy not in `enforce`, the pool cancels every entry
  carrying its delta, so switching the policy off leaves nothing behind.

The regtest run confirmed both behaviours on Core 31.1 and Knots 29.3.

Deltas add up, and the pool's record of what it set can drift from the
node's:

- **Doubled deltas.** The `rawtx` path and the backstop can both hit one
  transaction, and a restarted pool no longer knows what it set before. Two
  calls leave −2,000,000,006, which no longer matches the distinctive value,
  so a cleanup that matches it exactly would skip that entry for good. The
  pool keeps one set of txids it has deprioritised and never calls twice for
  one txid, and the cleanup treats any non-zero multiple of the distinctive
  delta as the pool's and cancels the whole amount.
- **Reconcile from the node.** The node's `getprioritisedtransactions` is the
  record, not the pool's memory. The pool rebuilds its set from it at start
  and whenever it sees the node restart (the RPC connection drops, or
  `uptime` goes backwards).
- **A node restart can drop the deltas.** They survive only in `mempool.dat`;
  a node run with `-persistmempool=0`, or one whose file failed to load,
  comes back without them. Without the reconcile step, the pool would think
  a transaction rebroadcast after the restart is deprioritised, find it in a
  template, and raise the ignored-delta warning on a healthy node. After a
  reconcile, a txid missing from the node's list is classified again, not
  counted as ignored.
- **The boot cleanup runs with the policy off**, so it runs on installs that
  never used the policy. It must stay silent when the RPC fails or the node
  lacks it, or healthy installs get a boot warning again (rule 6 in the
  review guide). Safer still, it runs only if a marker in the stats database
  says enforce was on at some point; with no stats database it runs and
  stays silent on failure.

### Checking that the node honours the deltas

The pool does not trust its node to apply the deltas; it checks. A
transaction the pool deprioritised that is still in a template fetched more
than GBT's cache window (5 seconds, gotcha 2) after the delta was set means
the node is ignoring it. The backstop filter still removes the transaction,
so the block stays within policy, but its space goes unfilled and the block
earns less than it should.

Each such catch is counted (`pool_template_delta_ignored_total`). When they
persist across refreshes, the pool logs a warning and the dashboard shows
it: the node is not honouring deprioritisation, and on Knots or Roots the
usual cause is `blockprioritysize` above 0. The same signal catches a future
node release that changes how deltas, GBT caching or the block builder's
feerate cutoff work.

### What the delta changes besides block building

A delta changes the fee the node's mempool uses everywhere, not only in the
block builder. Peers never see it and the transaction is unchanged, so RBF
and relay work as before for everyone else. On the operator's node, a delta
this large has three side effects. All three were confirmed on regtest on
2026-11-14, on Core 31.1 and Knots 29.3 (results below):

- **Replacement gets easier.** The node compares a replacement against the
  modified fees of the transactions it replaces, so it accepts a
  replacement of a deprioritised transaction that pays less than its real
  fee. Peers refuse that replacement, so the node's mempool can differ from
  theirs by a few transactions. Mining is unaffected: the replacement has a
  new txid, and the pool classifies it like any other transaction. A bumped
  unwanted transaction is deprioritised again. The replaced transaction's
  delta is not cleared (see "Delta bookkeeping"), so the cleanup handles it
  like an evicted one. A replaced transaction stays in the compact-block
  extra pool when the replacement arrived from a peer, so a block that mines
  the original costs no extra round trip.
- **Deprioritised transactions are evicted first.** Mempool trimming ranks
  by modified feerate, so a full mempool drops them and their descendants
  before anything else. Evicted transactions do not go into the extra pool.
  If another pool then mines one, the node lacks it and fetches it while
  validating that block: one more round trip in compact-block
  reconstruction, which delays the new tip and the clean job sent to miners.
  The delta has to stay far below zero all the same: one sized to leave the
  modified fee near zero lets a paying child pull its parent back into the
  template (see the results).
- **A returning transaction is refused.** A delta outlives eviction and
  replacement. Until the cleanup cancels it, a rebroadcast of that
  transaction fails the node's minimum fee check and is not relayed. For
  that window the node filters at relay, against "relay stays the node's
  business" in the narrow-only rule. Cancelling the delta makes the
  transaction acceptable again at once, so the cleanup interval is the
  window. If it matters, the cleanup runs on each mempool removal (ZMQ
  `sequence` reports them) rather than on a timer.

**Deltas are node-global.** Anything else that builds blocks from the same
node gets the filtered template too: a second pool, a DATUM Gateway, a test
miner pointed at the node's GBT. The operator's explorer differs the other
way (see Visibility). The enforce documentation says so, and the dashboard
tile names the node the deltas are on.

**Regtest results** (2026-11-14). Three nodes: the operator's node with a
5 MB mempool and the deltas, a peer with no deltas that mines, and a relay
that carries the same delta and forwards replacements to the operator's node
over P2P. Every transaction is P2WPKH. Each check ran twice: with the
design's delta of −1,000,000,003 sats, and with a delta equal to minus the
transaction's fee, so the modified fee is exactly 0.

| Check | Core 31.1 | Knots 29.3 |
|---|---|---|
| 2.7 sat/vB replacement of a deprioritised 90.9 sat/vB transaction | Accepted; the peer refuses it (`insufficient fee`) | Same |
| Mempools 20 s later | Operator has the replacement, peer the original | Same |
| Delta on the replaced transaction | Kept (`in_mempool: false`) until the original confirms | Same |
| Peer mines the original, replacement had arrived by P2P | 0 transactions requested, 1 from the extra pool | Same |
| Full mempool, 5 sat/vB filler: deprioritised 50 sat/vB parent with a 100 sat/vB child, and a lone 50 sat/vB transaction | All three evicted first; filler stays | Same |
| Peer mines all three | 3 requested; 29 ms from compact block to reconstruction on localhost, against 0.01 ms for a control block | 3 requested; 18 ms against 0.01 ms |
| Rebroadcast while the delta stands | Refused, `mempool min fee not met` | Refused, `min relay fee not met` |
| Rebroadcast after cancelling the delta | Accepted | Accepted |

With the modified fee at exactly 0, both nodes put the parent and its paying
child into `getblocktemplate`: the package feerate is the child's fee over
both sizes, well above `blockmintxfee`. The same child kept the parent from
being evicted. The lone transaction was still evicted first and still
refused on return (`0 < 720` on Core, `0 < 141` on Knots), so a near-zero delta gives up the policy and
buys nothing. The localhost timings only show that the extra round trip
happens. On a real network it costs one round trip to the peer that sent
the block.

A replacement sent by RPC is a special case: its replaced transaction
reaches the extra pool only if a peer relays the original again and the
node rejects it, which happened in some runs and not others.

The probe is `examples/delta_probe.rs`. It starts its own regtest nodes on
ports 18700 to 18721 and writes `results.json` to the workdir:

```
cargo run --release --example delta_probe -- \
    --bitcoind <path> --workdir <empty dir> --mode big|zero [--node-arg <arg>]...
```

**Roots** (v31.1-roots.6, 2026-11-14) behaves like Core and Knots on every
check above: replacement, eviction first with the same extra round trip, and
refusal on return (`min relay fee not met`). Its delta also survives a
mempool replacement. The difference is the template (gotcha 1): on a stock
Roots node, with its priority area on, a deprioritised transaction was in
`getblocktemplate` in all three runs with the large delta (the lone one in
two runs, the parent and its child in the third). With
`--node-arg -blockprioritysize=0`, none was, in three runs of three. Before
enforce ships, the enforce-mode E2E test covers these on every node in the
matrix, Roots included.

## Gotchas found on regtest

1. **The coin-age priority area ignores fee deltas.** With Knots
   `blockprioritysize=100000`, deprioritising changed nothing: the template kept
   all 15 unwanted transactions, and a block mined from it included all 12
   parents. Knots' `prioritisetransaction` also takes a priority delta; adding
   `-1e17` closed the gap. Both Knots 29.3 and Roots turn the priority area on
   by default (`blockprioritysize` 100,000 bytes), so a stock node of either
   has this hole; the deployed node avoids it only because its config sets
   `blockprioritysize=0`. Roots has no priority delta at all (the argument is a
   dummy that must be zero). Rule: on Knots the pool sends both deltas; for any
   node the documentation recommends `blockprioritysize=0`.

   A stock Roots node is the case the deltas cannot close: its priority area
   is on and there is no priority delta to send. Confirmed on v31.1-roots.6
   (see the Roots results above): a deprioritised transaction reached the
   template in every run on a stock node and in none with
   `blockprioritysize=0`. The policy still holds,
   because the backstop removes what the priority area picks, but that space
   is left empty. Documentation alone cannot fix this. The pool's app on
   Umbrel cannot change the node's config, and a missed line in the docs
   fails silently, so the pool detects the problem at runtime (see "Checking
   that the node honours the deltas").
2. **GBT caches its template.** After a mempool change, Core reuses the cached
   template for up to 5 seconds unless the tip moves (`rpc/mining.cpp:863` in
   v31.1). A just-deprioritised transaction can appear during that window, which
   is why the backstop filter is required.
3. **The proposal check caught a real bug.** The demo's block builder padded
   the txid list for an odd-length merkle level in place, so the block's
   transaction count came out one too high. Proposal mode rejected it before
   anything was mined.

## Latency

Time on the wire decides a block race, so the policy keeps off the paths where
it matters. Measured 2026-10-16 on mainnet with Knots 29.3 and a 1.9 MB
template:

| Step | Wall time | Holds `cs_main` |
|---|---|---|
| `getblocktemplate` (already every 30 s and on each new tip) | 107-113 ms | yes |
| Parsing every template transaction (pure Python; Rust is faster) | 9.4 ms | no |
| Proposal check on a full block | 130-132 ms | yes |
| `prioritisetransaction`, per call | short | yes |

`submitblock` needs `cs_main` too, so anything holding it can delay a found
block. Rules:

1. Nothing new runs between a share meeting the network target and
   `submitblock`. The found block is built from the job's transaction list, as
   today.
2. Never wait on a proposal check to submit a block or to send new-tip jobs.
   A failed check stops a template from being handed out; it never stops a
   submission.
3. Batch deprioritise calls, and hold them for a second or two after a new tip,
   while the template refresh and propagation matter most.

The node's lock is one queue; the pool has two of its own:

- **The RPC client.** `bitcoincore-rpc`'s `SimpleHttpTransport` keeps one
  socket behind a `Mutex` and holds it for the whole request, so every call
  through the one `RpcClient` in `src/bitcoin/rpc.rs` is serialized. Today
  `submit_block` shares that client with the template fetch, the one-second
  tip poll and the 30-second network-stats poll, so a found block can already
  wait up to one `getblocktemplate` (about 100 to 140 ms on the deployed node,
  measured 2026-11-14) in the roughly 0.4% of the time one is in flight.
  Policy calls (the sweep, deltas, cleanup, proposal checks) must not join
  that queue: they get an `RpcClient` of their own. Submission getting its
  own client too is a separate, smaller change.
- **The engine task.** `TemplateEngine::run` in `src/mining/engine.rs` is one
  sequential loop: `refresh` fetches the template, builds the job and
  broadcasts it in line, and a new-tip signal waits until a running refresh
  finishes. Classification of the template's transactions stays in that path
  (pure Rust, a few ms). Everything that calls the node runs on a separate
  policy task fed by a channel: deltas, the sweep, the cleanup and the
  proposal check. The engine never awaits it.
- **What miners get at a new tip.** If the new template is clean, which is
  the normal case once the deltas are set, the job goes out at once, as
  today. If the backstop changed it, the proposal check has not returned
  yet, so the engine sends a coinbase-only job with `clean_jobs = true` at
  once and replaces it with the filtered template (`clean_jobs = false`)
  when the check passes, about 130 ms later. Miners are never left on the
  old tip while the check runs. In the first enforce release, which has no
  rewrite (see Decided), the coinbase-only job instead stays until a refetch
  after the 5-second cache window returns a clean template.

Propagation does not suffer. Peers rebuild a compact block from their own
mempools. Core peers hold every transaction, including the ones the pool
declined. Knots peers lack the declined ones, so leaving them out saves those
peers a round trip. The transactions that fill the freed space are ordinary
low-fee ones that nearly every mempool holds.

## Configuration

A new `[template_policy]` section, every key a flat scalar so the
`SOLO_POOL_TEMPLATE_POLICY__*` environment overrides work on Umbrel. Adding it
after 1.0 is a compatible change (a new section with defaults).

The section ships **experimental**: its keys, metrics and `/stats` fields may
change or go away in a minor release, the pool logs a warning at boot while
`mode` is not `off`, and it becomes stable only after enforce mode has run on
prod. The experimental clause is in `docs/stable-surface.md` ("Experimental
features"), on main since before 1.0.0-rc.1. When the section ships, it gets a
row in that table, in the same release.

```toml
[template_policy]
mode = "off"                          # off | observe | enforce
datacarrier_size = 83                 # bytes; Knots -datacarriersize
permit_multiple_op_return = false     # Knots has no option and always rejects
accept_non_std_datacarrier = false    # Knots -acceptnonstddatacarrier
datacarrier_full_count = true         # Knots -datacarrierfullcount
max_script_size = 1650                # bytes; Knots -maxscriptsize
reject_tokens = false                 # Knots -rejecttokens
reject_parasites = true               # Knots -rejectparasites
permit_bare_multisig = false          # Knots -permitbaremultisig
permit_bare_pubkey = false            # Knots -permitbarepubkey
permit_bare_datacarrier = false       # Knots -permitbaredatacarrier
verify_with_proposal = true           # only on templates the pool changed
on_proposal_failure = "coinbase_only" # coinbase_only | unfiltered
```

Defaults for the rule keys follow Knots, so that turning `mode` on without
other changes gives Knots-like blocks. `mode = "observe"` runs every rule and
records the results but mines the node's template unchanged.

### Later rules

Knots 29.3 has more policy options. Whether one can move into the pool depends
on what it needs to know:

- **The transaction alone.** `-acceptunknownwitness` (default 1, so it
  rejects nothing unless turned off). A candidate, named the same way.
- **The coins being spent.** `-bytespersigopstrict` and `-maxtxlegacysigops`
  count sigops in P2SH and witness scripts, which needs the spent coins. They
  wait for the coin-lookup mode.
- **Mempool or chain state.** `-spkreuse`, `-minrelaycoinblocks`,
  `-minrelaymaturity` and `-permitephemeral` judge a transaction against other
  transactions or coin ages. The pool sees one template, not the mempool, so
  these stay with the node.
- **Re-ranking rather than rejecting.** `-datacarriercost` makes data bytes
  count as more than one vbyte when ranking. The deprioritise mechanism can
  express this with a fee delta proportional to a transaction's data bytes
  instead of a fixed large one.

## Visibility

An operator choosing a policy should see what it costs.

- Metrics per rule: transactions declined, weight declined, fees declined.
  Proposed names: `pool_template_declined_txs_total{rule}`,
  `pool_template_declined_weight_total{rule}`,
  `pool_template_declined_fees_sats_total{rule}`, plus
  `pool_template_proposal_failures_total` and
  `pool_template_delta_ignored_total`. They stay experimental with the rest
  of the section.
- Each declined transaction is counted once. One transaction can match
  several rules (a Runes transaction with a large OP_RETURN trips both
  `reject_tokens` and `datacarrier_size`), so it is credited to the first
  rule it matches, in the order of the rules table. A transaction removed
  only because its parent was declined is labelled `rule="descendant"`. An
  unlabelled total sits beside the per-rule series, so a sum over `rule`
  never has to be trusted for the total.
- Fees declined are an upper bound. The true cost is the difference from the
  template the node would have built without the deltas, and in enforce mode
  the pool never sees that template. Observe mode on the operator's own node
  is the measurement.
- A dashboard tile with the template being mined: transaction count, total
  fees, and what the policy declined from it (count, weight, fees), plus the
  active rules and the totals since start. In enforce mode this is the only
  view of the operator's real template, so it must not rely on any other
  tool.
- **Mempool explorers diverge in enforce mode.** A self-hosted mempool
  explorer builds its projected blocks from the node's mempool itself and
  does not call GBT. `getrawmempool true` reports `fees.base` and
  `fees.modified` for each transaction. The mempool explorer (mempool/mempool
  v3.3.1, the one on the deployed host) uses the base fee: it sets each
  transaction's fee from `mempoolEntry.fees.base`
  (`backend/src/api/bitcoin/bitcoin-api.ts:391`, shared by its `none` and
  `electrum` backends). So it shows what the rest of the network is likely to
  mine, which is still the right view for fee estimates, and not the pool's
  template. Observe mode sets no deltas, so the explorer and the pool agree
  until enforce.

  Checked on regtest on 2026-11-14 (Knots 29.3, a throwaway backend with
  `BACKEND: none`, mempool filled to two projected blocks with 5 sat/vB
  transactions). Two 50 sat/vB transactions were deprioritised, one after
  the explorer had seen it and one the moment it entered the mempool. Both
  stayed in projected block 0, and `/api/v1/tx` reported their real fee
  (7,050 sats) while the node reported `fees.modified` of −9.99992953 BTC
  and left both out of `getblocktemplate`. A delta cannot be set before the
  explorer first sees a transaction: one set before broadcast makes the node
  refuse it (see "A returning transaction is refused").

  With its block audit on (`MEMPOOL.AUDIT`, which needs its database), the
  explorer will then list the declined transactions in a block the pool
  finds as censored, "missing, without excuse" in `audit.ts`, for any paying
  1 sat/vB or more, and score the block lower. That follows from the code;
  the audit itself was not run. The dashboard's template tile above is the
  operator's view of what the pool declined, and the documentation for
  enforce mode should say why the explorer disagrees.
- `/stats` fields carrying the same.

## Rollout

**Phase 0 (merged in #147 on 2026-11-25; until 1.0 it stayed on a branch,
so the released binary and the soak were untouched).**
- This document.
- The classifier as a standalone module with the Roots test vectors ported.
- An audit tool that replays recent mainnet blocks. `getblock <hash> 3`
  includes the spent coins, so for each block it reports what each rule would
  have removed and what it would have cost, and compares the witness-shape rule
  with the exact coin lookup. This answers "how much does the policy cost" with
  data and tests the rules on real inscription traffic.

**Phase 0 results (2026-10-16).** The classifier and audit tool were then on
branch `feat/template-policy-classifier` (#147). Over mainnet blocks 971,566 to
971,710 (144 blocks, about 680,000 transactions):

| Settings | Txs declined | Weight declined | Fees declined | Per-block weight (median, max) |
|---|---|---|---|---|
| Knots 29.3 defaults | 1.44% | 3.12% | 3.74% | 2.1%, 30.2% |
| Deployed node (`datacarrier_size = 42`, `reject_tokens = true`) | 61.05% | 42.94% | 15.24% | 36.3%, 99.2% |

Exact and shape mode disagreed on no transaction, so the coin-lookup mode is
not needed for these rules. Under Knots' defaults most of the declined weight
is embedded data; with `reject_tokens` most of it is Runes, which arrive in
bursts (8% to 87% of a block's transactions in a seven-block sample). The fee
figures are what the declined transactions paid, an upper bound on the cost:
deprioritising lets the node refill their space. Anyone can rerun the audit
with `cargo run --release -p tx-datacarrier-policy --example template_audit`.

**Full difficulty period (2026-11-08).** Over mainnet blocks 973,069 to
975,084 (2,016 blocks, about 8.5 million transactions; the deployed-settings
run ended one block later):

| Settings | Txs declined | Weight declined | Fees declined | Per-block weight (median, max) |
|---|---|---|---|---|
| Knots 29.3 defaults | 2.36% | 6.39% | 1.98% | 1.3%, 99.2% |
| Deployed node (`datacarrier_size = 42`, `reject_tokens = true`) | 59.43% | 39.69% | 7.90% | 33.1%, 99.7% |

With the deployed settings, the transaction and weight shares stay close to
the 144-block window and the fee share halved (15.24% to 7.90%). Under Knots'
defaults the weight share doubled and a block appeared that is 99.2%
declinable, so even default settings can leave a template nearly empty
before the refill; enforce mode's regtest test should include that case.
Most of the default-settings weight is `txn-datacarrier-nonstandard` (5.19%);
with `reject_tokens`, Runes alone are 56.47% of transactions and 31.89% of
weight.

The longer window found a shape-mode bug: a taproot key spend with an annex
(`[signature, annex]`) was read as a witness script, so the annex counted as
embedded data. Two mainnet transactions showed it (57da10d1..., carrying a
16-byte BitVMX annex, and 52bc40e5..., a 43 kB annex). Both are nonstandard
at the node anyway, since Core and Knots refuse any annex, so neither reaches
a mempool template. With the fix, exact and shape mode disagree on one
transaction in each run, 496b7fdb..., which spends a coin with a 4,908-byte
bare scriptPubKey. Exact mode declines it under `script-size`; shape mode
cannot see the size of a coin it does not have. A coin like that is declined
when it is created (`scriptpubkey-size`, 132 outputs in the window), so the
gap is one transaction in 8.5 million.

**Phase 1, the first release after 1.0: observe mode** (built in #190,
unreleased). The classifier is
already in its own crate (see Library crate below), so the pool uses it as a
dependency from the start. The pool classifies and records;
templates are unchanged. Run it behind the deployed Knots node
first: Knots has already filtered, so anything the pool would still decline
points to a rule that is stricter than Knots or wrong.

That run only catches over-declining. A rule the pool applies too loosely
declines nothing behind Knots, because Knots already removed those
transactions, so observe mode there cannot see it. The evidence against
under-declining is the differential test (22 crafted vectors at and past each
limit) and the mainnet audit. Observe mode behind a Core node shows the
declines at their real size but has no node verdict to compare with.

**Phase 2, the release after: enforce mode.** Deprioritising, the mempool
sweep, delta bookkeeping and the ignored-delta check. The backstop detects a
dirty template but does not rewrite it; the rewrite and the proposal check
come in a later release (see Decided). Before it ships, a regtest
test in the E2E matrix (Core and Knots) builds an inscription, a Runes
transaction, a large OP_RETURN, a CAT-21 transaction and a child of a declined
parent, then checks that the template holds none of them, that the proposal
passes, and that a block mined from it is accepted. It extends the existing
block-submit rehearsal.

Also before enforce ships:

- **Rerun the audit over a full difficulty period** (2,016 blocks, about two
  weeks). Done 2026-11-08; see the results above. Exact and shape mode
  disagree on one transaction, which only a coin lookup can catch; whether
  that keeps the coin-lookup mode unplanned is an open question below.
- **Node versions in the E2E matrix.** The mechanism depends on how Core and
  Knots implement `prioritisetransaction`, GBT caching and the block
  builder's feerate cutoff. A new Core or Knots major joins the matrix, and
  passes, before the documentation claims support for it. Between releases,
  the runtime check above catches a node that stops honouring the deltas.

- **The delta side effects** (replacement, eviction, refusal on return) are
  confirmed on regtest, and the explorer check under Visibility is done.
  Done for Core 31.1 and Knots 29.3 on 2026-11-14 (see "What the delta
  changes besides block building"). The explorer check is done too (see
  Visibility), and Roots on 2026-11-14.

A change in those internals is a fee risk, not a correctness risk: the
backstop keeps declined transactions out and the proposal check rejects an
invalid block, so what breaks is the refill.

## Library crate

The classifier is useful outside the pool: to another pool, a Template
Provider, a mempool explorer, or a sidecar that applies the same rules to a
plain Core or Roots node. It depends only on the `bitcoin` crate. It was
extracted on 2026-11-14, on the phase 0 branch and ahead of phase 1, so no
live code moved later.

- **Layout.** The repository is a Cargo workspace with the crate at
  `crates/tx-datacarrier-policy/`; the mainnet audit and the differential test
  moved with it. The pool depends on it by path since observe mode was wired
  in (#190). The
  regtest delta probe (`examples/delta_probe.rs`) tests node behaviour for
  enforce mode and stays with the pool. A separate repository waits until
  someone outside the pool uses the crate.
- **API.** `Rules` for the settings, `check` taking a transaction and, when
  available, its spent coins (exact mode with them, shape mode without), and
  `Rule` with `Rule::ALL`. `Rule` and `Rules` are `#[non_exhaustive]`, so a
  new rule or setting is not a breaking change; outside the crate, `Rules` is
  built by changing fields of `Rules::default()`.
- **rust-bitcoin.** The API takes `bitcoin` types, so a rust-bitcoin major
  bump is a breaking release of the crate. The README says so and the crate
  follows rust-bitcoin majors.
- **Name.** `tx-datacarrier-policy`: descriptive and neutral, without
  "knots" or "roots", which would suggest an affiliation. The README maps each
  rule to the Knots, Roots and Core options and versions it follows, as the
  rules table here does.
- **Differential test.** The mainnet audit shows what the rules do on real
  traffic; it does not show they still match the node after a Knots or Roots
  release changes a default. A regtest test sends crafted transactions (one
  per rule, at and just past each limit) through the node's
  `testmempoolaccept` and through the crate, and fails on any disagreement.
  It runs in the E2E matrix against Knots and Roots. This test is the reason
  to trust the crate. It is `tests/policy_differential.rs` in the crate, and
  runs in the E2E workflow on the Knots entry.
- **Publishing.** crates.io at 0.x, versioned apart from the pool. Its own 1.0
  waits for outside use, and does not follow the pool's. Not published yet;
  the crate is 0.1.0 in the workspace.

### Differential test results

First run, 2026-11-14, Knots 29.3: 22 vectors under three settings (Knots'
defaults; the deployed node's `datacarriersize=42` and `rejecttokens=1`; and
a permissive set with `acceptnonstddatacarrier`, `permitbaremultisig`,
`permitbarepubkey` and `permitbaredatacarrier` on and `rejectparasites` off).
Exact and shape mode agreed with the node on every vector, including each
boundary: OP_RETURN scripts of 42/43, 83/84 and 1,650/1,651 bytes, a tapscript
envelope counted at exactly 83 and 84 bytes, and a tapscript witness of
1,650 and 1,651 bytes. The one difference was the reject reason for an
oversize OP_RETURN (`scriptpubkey`, see the note under Rules); the test maps
it to `datacarrier` only when the classifier says `datacarrier`. Changing the
deployed settings in the test to 43 bytes fails it, as it should.

Roots v31.1-roots.6, same day: every verdict agrees under all three
settings, and Roots reports `datacarrier` itself, so the `scriptpubkey`
mapping is not used. Its option names and defaults match Knots 29.3's;
`-datacarriersize` is summed over all outputs ("in aggregate"), as the
classifier does.

Not covered, because Core's own rules refuse the transaction first at these
settings: `scriptsig-size` and `scriptcheck-size` (Core's 1,650-byte scriptSig
limit) and `script-size` (spending a coin with a scriptPubKey over 1,650
bytes means spending a non-standard output).

## Decided

- **Keys** (2026-10-16): Knots' meaning and defaults, this config's structure.
  snake_case names in `[template_policy]`, mapped to the Knots, Roots and Core
  options in the rules table. An operator who wants their node's own settings
  (for example `reject_tokens = true`, as on the deployed node) sets them here.
- **Experimental status** (2026-10-18): `[template_policy]` ships
  experimental, observe mode first, in a release after 1.0.
- **Detecting an ignored delta** (2026-10-18): the pool checks behaviour
  instead of reading node config. A deprioritised transaction still in a
  template after the cache window is counted and warned about. This replaces
  the open question on reading `blockprioritysize`.
- **Attribution** (2026-10-18): each declined transaction counts once, under
  the first matching rule; descendants under `descendant`; plus a total.
- **Before enforce** (2026-10-18): a 2,016-block audit, and every supported
  node major in the E2E matrix.
- **Crate name** (2026-11-14): `tx-datacarrier-policy`, under `crates/`.
- **Delta size** (2026-11-14): the delta stays far below zero. Sizing it to
  leave the modified fee near zero, to soften eviction, lets a paying child
  pull its parent back into the template on Core and Knots.
- **First enforce release without the template rewrite** (2026-11-15).
  Enforce ships first with deltas, the sweep, bookkeeping and the
  ignored-delta check, and no backstop rewrite. On a dirty template the pool
  keeps the current job, or at a new tip sends a coinbase-only job, then
  refetches after the 5-second cache window (gotcha 2). Once the
  ignored-delta signal persists (a stock Roots node, gotcha 1), it refuses
  enforce rather than mining coinbase-only for good. The rewrite (dependant
  removal, fee subtraction, a recomputed witness commitment) and the
  proposal check follow once that has run on prod. Until then the pool adds
  no consensus-critical code and copies the node's witness commitment as
  today.

## Open questions

- **The coin-lookup mode.** Phase 0 found no disagreement over 144 blocks.
  Over 2,016 blocks, after the annex fix, shape mode misses one transaction
  in 8.5 million: a spend of a coin whose scriptPubKey exceeds
  `max_script_size`. Leaning: not planned, since that coin is declined when
  created and the miss is that rare; it stays an option if a later rule (the
  sigop limits) needs the spent coins.
- **SV2 Template Distribution as a feed.** Bitcoin Core's IPC mining interface
  and the `sv2-tp` Template Provider deliver templates over SV2 instead of GBT.
  The pool already speaks SV2 to miners. Open: whether Knots ships the IPC
  interface, how `RequestTransactionData` fits the backstop filter, and whether
  the proposal check and `prioritisetransaction` still need an RPC connection
  beside it.
- **A pool-built block.** This design never assembles a block from the mempool;
  the node's builder does, under the node's consensus checks. A builder of the
  pool's own would take on what the node handles now: ancestor packages, weight
  and sigop limits counting the coinbase, the witness commitment, the BIP 34
  height, and lock-time finality against median time past. It would need the
  proposal check on every template, with the node's GBT as the fallback. Not
  planned.
