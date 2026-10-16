# Template policy

**Status: design, not built.** Nothing here ships before 1.0. The code freeze
holds the pool binary until then, so the phase 0 work below stays on a branch
until 1.0 is tagged.

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

Each rule mirrors a Knots option, so a Knots user moving to Core can carry
their policy across and get the same blocks. The definitions and their test
vectors come from Bitcoin Roots (`src/policy/policy.cpp`, MIT licensed, on a
Core v31.1 base), which reimplements the Knots behaviour.

| Key | Knots option | Rejects |
|---|---|---|
| `datacarriersize` | `-datacarriersize` | data-carrier bytes above the limit, counting every OP_RETURN script in the transaction |
| `datacarrierfullcount` | `-datacarrierfullcount` | also counts data embedded in the script being executed toward the same limit: `OP_FALSE OP_IF ... OP_ENDIF` envelopes (inscriptions) and data pushes followed by `OP_DROP` |
| `rejecttokens` | `-rejecttokens` | Runes (`OP_RETURN OP_13`) and Olga (data in P2WSH outputs) |
| `rejectparasites` | `-rejectparasites` | parasitic overlay protocols; in Roots' implementation, CAT-21 (`nLockTime == 21`) |
| `permitbaremultisig` | `-permitbaremultisig` | bare multisig outputs when false (Stamps) |

A transaction whose parent in the template is rejected is rejected with it,
whatever its own content.

### The executed-script problem

Knots finds the script an input executes by looking up the coin it spends: the
coin's scriptPubKey says whether the input is P2TR, P2WSH or P2SH, and so which
witness item or scriptSig push is the script. The pool has the transaction but
not the coin. Two ways to close that gap:

- **Witness shape (start here).** After removing an annex (a last witness item
  starting `0x50`, when there are at least two items), a last item of 33+32m
  bytes (m ≤ 128) whose first byte is `0xc0` or `0xc1` is a taproot control
  block, and the item before it is the tapscript. Otherwise the last witness
  item is a P2WSH witness script, and the last scriptSig push is a P2SH redeem
  script. No lookups, so it costs nothing. An unusual witness can be misread.
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

### Delta bookkeeping

The node clears a delta when its transaction confirms or is replaced by a
conflicting one (`txmempool.cpp:398,420` in v31.1). A delta for a transaction
that expires or is evicted stays on record, and deltas survive restarts in
`mempool.dat`. So:

- A periodic cleanup lists `getprioritisedtransactions` and cancels, with the
  opposite delta, every entry carrying the pool's delta whose `in_mempool` is
  false.
- At startup with the policy not in `enforce`, the pool cancels every entry
  carrying its delta, so switching the policy off leaves nothing behind.

The regtest run confirmed both behaviours on Core 31.1 and Knots 29.3.

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

Propagation does not suffer. Peers rebuild a compact block from their own
mempools. Core peers hold every transaction, including the ones the pool
declined. Knots peers lack the declined ones, so leaving them out saves those
peers a round trip. The transactions that fill the freed space are ordinary
low-fee ones that nearly every mempool holds.

## Configuration

A new `[template_policy]` section, every key a flat scalar so the
`SOLO_POOL_TEMPLATE_POLICY__*` environment overrides work on Umbrel. Adding it
after 1.0 is a compatible change (a new section with defaults).

```toml
[template_policy]
mode = "off"                     # off | observe | enforce
datacarriersize = 83             # bytes; Knots' default
datacarrierfullcount = true
rejecttokens = false
rejectparasites = true
permitbaremultisig = false
verify_with_proposal = true      # only on templates the pool changed
on_proposal_failure = "coinbase_only"   # coinbase_only | unfiltered
```

Defaults for the rule keys follow Knots, so that turning `mode` on without
other changes gives Knots-like blocks. `mode = "observe"` runs every rule and
records the results but mines the node's template unchanged.

## Visibility

An operator choosing a policy should see what it costs.

- Metrics per rule: transactions declined, weight declined, fees declined.
  Proposed names: `pool_template_declined_txs_total{rule}`,
  `pool_template_declined_weight_total{rule}`,
  `pool_template_declined_fees_sats_total{rule}`, plus
  `pool_template_proposal_failures_total`. They join the stable surface when
  they ship.
- A dashboard tile with the active rules and "fees declined" for the current
  template and since start.
- `/stats` fields carrying the same.

## Rollout

**Phase 0, now (on a branch, so the released binary and the soak are
untouched).**
- This document.
- The classifier as a standalone module with the Roots test vectors ported.
- An audit tool that replays recent mainnet blocks. `getblock <hash> 3`
  includes the spent coins, so for each block it reports what each rule would
  have removed and what it would have cost, and compares the witness-shape rule
  with the exact coin lookup. This answers "how much does the policy cost" with
  data and tests the rules on real inscription traffic.

**Phase 1, the first release after 1.0: observe mode.** The pool classifies
and records; templates are unchanged. Run it behind the deployed Knots node
first: Knots has already filtered, so anything the pool would still decline
points to a rule that is stricter than Knots or wrong.

**Phase 2, the release after: enforce mode.** Deprioritising, the backstop
filter, the proposal check and delta bookkeeping. Before it ships, a regtest
test in the E2E matrix (Core and Knots) builds an inscription, a Runes
transaction, a large OP_RETURN, a CAT-21 transaction and a child of a declined
parent, then checks that the template holds none of them, that the proposal
passes, and that a block mined from it is accepted. It extends the existing
block-submit rehearsal.

## Open questions

- **Key spelling.** Knots' names verbatim (`datacarriersize`) are the
  vocabulary Knots users know; snake_case (`datacarrier_size`) matches the
  rest of the config. This draft uses the Knots spelling.
- **Default for `rejecttokens`.** Knots defaults it off; the deployed node runs
  it on. The draft follows Knots.
- **The coin-lookup mode,** if phase 0 shows the shape rule is not good enough.
- **Detecting the priority area.** The pool may not be able to read
  `blockprioritysize` over RPC. If it cannot, documentation carries the
  recommendation and the pool always sends the priority delta to Knots.
