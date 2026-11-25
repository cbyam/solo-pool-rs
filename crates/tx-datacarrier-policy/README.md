# tx-datacarrier-policy

Bitcoin Knots' data-carrier and token transaction policy, as a Rust library.
`check` tells whether a transaction passes the policy rules Knots enforces and
Bitcoin Core v31.1 does not, so a Core node plus these rules gives Knots-like
blocks. Each setting keeps the meaning and the default of its Knots option.

```rust
use tx_datacarrier_policy::{check, Rules};

let mut rules = Rules::default(); // Knots 29.3's defaults
rules.reject_tokens = true;
match check(&tx, &rules, Some(&spent_coins)) {
    Ok(()) => { /* passes */ }
    Err(rule) => println!("declined: {}", rule.reason()),
}
```

`spent_coins` are the outputs the transaction's inputs spend, in input order.
With them, the script each input executes is found exactly, as Knots does.
Pass `None` without them and it is inferred from the witness shape; over the
2,016 mainnet blocks the solo-pool-rs audit replayed, the two modes disagreed
on one transaction in 8.5 million.

## Rules

| `Rules` field | Knots / Roots option (default) | Core 31.1 | Declines (`Rule::reason`) |
|---|---|---|---|
| `datacarrier_size` | `-datacarriersize` (83) | same option, default 100,000 | OP_RETURN scripts over the limit (`datacarrier`; Knots 29 reports `scriptpubkey`) |
| `permit_multiple_op_return` | none: always rejected | allowed | more than one OP_RETURN output (`multi-op-return`) |
| `accept_non_std_datacarrier` | `-acceptnonstddatacarrier` (0) | none | data embedded in an executed script: envelopes, push/`OP_DROP`, Olga, OPNet (`txn-datacarrier-nonstandard`) |
| `datacarrier_full_count` | `-datacarrierfullcount` (1) | none | OP_RETURN and embedded data together over `datacarrier_size` (`txn-datacarrier-exceeded`) |
| `max_script_size` | `-maxscriptsize` (1650) | none for witnesses | witness stacks, output scripts, scriptSigs, redeem scripts and spent scripts over the limit (`witness-size`, `scriptpubkey-size`, `scriptsig-size`, `scriptcheck-size`, `script-size`) |
| `reject_tokens` | `-rejecttokens` (0) | none | Runes (`tokens-runes`) and Olga (`tokens-olga`) |
| `reject_parasites` | `-rejectparasites` (1) | none | CAT-21, `nLockTime == 21` (`parasite-cat21`) |
| `permit_bare_multisig` | `-permitbaremultisig` (0) | same option, default 1 | bare multisig outputs (`bare-multisig`) |
| `permit_bare_pubkey` | `-permitbarepubkey` (0) | allowed | bare pay-to-pubkey outputs (`bare-pubkey`) |
| `permit_bare_datacarrier` | `-permitbaredatacarrier` (0) | allowed | transactions whose only outputs are data carriers or dust (`bare-datacarrier`) |

Defaults follow Knots 29.3 (`bitcoind -help`). Where Core has the same option,
the field follows Knots' meaning and default, not Core's. Checks Core already
applies (non-standard scripts, dust, the 80-byte stack item limits) are left
to the node.

## How it is tested

- Unit tests with test vectors ported from Bitcoin Roots.
- `tests/policy_differential.rs` (ignored by default) builds transactions at
  and just past each limit and checks the verdict and reject reason against a
  Knots or Roots node's `testmempoolaccept`, under three settings:

  ```text
  BITCOIND=/path/to/knots/bitcoind \
      cargo test --release -p tx-datacarrier-policy --test policy_differential -- --ignored --nocapture
  ```

- `examples/template_audit.rs` replays recent mainnet blocks through the rules
  and reports what each would have declined:

  ```text
  cargo run --release -p tx-datacarrier-policy --example template_audit -- \
      --url http://127.0.0.1:8332 --cookie ~/.bitcoin/.cookie --blocks 144
  ```

## Versioning

0.x, versioned apart from solo-pool-rs. `Rules` and `Rule` are
`#[non_exhaustive]`, so a new setting or rule is not a breaking change. The
API takes rust-bitcoin types, so a rust-bitcoin major bump is a breaking
release of this crate; it follows rust-bitcoin majors (0.32 today).

## Origin and license

The logic is a port of Bitcoin Roots' `src/policy/policy.cpp` and the
data-carrier checks in its `src/validation.cpp` (MIT licensed, Copyright The
Bitcoin Core developers), which reimplement the Knots behaviour on a Core
v31.1 base. This crate is not affiliated with Bitcoin Knots or Bitcoin Roots.

Licensed under either of Apache License 2.0 or MIT, at your option.
