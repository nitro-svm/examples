# Counterfactual Capital

This example scales a venue's inventory and its quoting curve, then measures the flow it would have filled. Every historical swap on the pair is rebuilt through the venue and priced against the scaled state, so the fills reveal which of the two the venue was actually short of.

## Methodology
The session replays the range once with nothing overridden, recording the venue's own state at every slot it changed. Each arm posts those states back scaled, priced against the same historical flow. Nothing is committed, so other venues and taker flow are held constant.

Capital and curve are separate knobs because they fail in different places: the vaults decide whether a fill can be paid, the curve decides whether it's offered at all. On a venue that quotes from a ladder, only one of them moves the answer. See [results](#results).

- `--multiple K --scale vaults` — the vault balances, and the copies the venue keeps of them.
- `--multiple K --scale ladder` — every ladder tier's size, which is the quote ceiling.
- `--tighten-bps D` — each tier's price, toward the other side by `D` bps, never past the midpoint.

`--scale all` is the default. Every flag is repeatable and crossed with the others, so one run covers the decomposition; `--multiple` must include `1`, the control.

## Usage
### Setup
Set the environment with the plan and time range of interest.
```sh
export SIMULATOR_API_KEY=<key>
PLAN=src/bin/counterfactual_capital/tempest-sol-usdc.json
RANGE="--start-slot 439649408 --slot-count 9999"
```

### Plan
`--plan` names the venue's routing identity and where its capital and curve live in bytes. Neither half can be derived from the other: a router's IDL declares only the accounts the *router* touches, while the venue's own account run arrives as `remaining_accounts`. Take it from a landed swap that routed through the venue.

```jsonc
{
  "directFill": {
    "aggregator": "titan",       // the router whose encoder builds the replacement hop
    "venue": "Tempest",          // as that router's IDL names it, or its ordinal
    "pair": ["So111...112", "EPjFW...Dt1v"],
    "slippageBps": 50,
    "market": {
      "mints": ["So111...112", "EPjFW...Dt1v"],  // the VENUE's ordering; a direction byte indexes it
      "accounts": [{ "address": "...", "writable": true }]   // its account run, in program order
    }
  },
  "inventory": {
    "vaults": ["4kHHme...", "6vNWbf..."],        // in the order the state mirrors them
    "state": {
      "account": "FQmFVQ...",
      "discriminator": "tempest1",               // asserted before a byte is written
      "len": 2385,                               // likewise
      "maxTiers": 32,
      "balanceMirrors": [2321, 2329],            // the venue's own copies of each vault balance
      "ladders": [
        { "count": 209,  "entries": 225,  "stride": 32, "width": 16 },
        { "count": 1265, "entries": 1281, "stride": 32, "width": 16 }
      ]
    }
  }
}
```

A `ladders` entry reads: the tier count is `width` bytes at `count`; tier *i* sits at `entries + stride*i` as a `width`-byte price then a `width`-byte size. To find the offsets, diff the state account across slots — of the fields that move, prices change and sizes do not. Omit `state` for a constant-product pool, whose vaults are its curve.

### Validate
Check the plan before opening a session. This catches what otherwise costs a replay per arm to discover: an account outside the venue's run, one the venue writes to but the plan marks read-only, and a ladder running past the end of the account.
```sh
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN --dry-run
```

### Run
Replay the range once per arm, scaled, and report what each would have filled. Every arm is a full replay plus one for the reference pass; the range above takes 4–6 minutes per arm.
```sh
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN \
  --multiple 1 --multiple 100 --scale all --scale vaults --scale ladder \
  --out decompose.jsonl
```

### Reuse a capture
`--capture` records the venue's trajectory the first time and reads it back afterwards, which saves the reference pass on every later run over the same range. The recording is refused if it names a different account than the plan.
```sh
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN \
  --multiple 1000 --scale ladder --capture tempest.jsonl --out wider.jsonl
```

## Results

For the Tempest SOL/USDC market in the range `439649408–439659407` from 2026-09-01, over 56,960 buildable hops:

| arm | max trade | won | filled | mean bps | vs 1x | refused |
|---|---|---|---|---|---|---|
| 1x | 14.92 | 55,600 | 97.6% | −1.2 | — | Custom_5 928, Custom_18 432 |
| 100x ladder | 1492 | 55,663 | 97.7% | +5.9 | **+7.1** | Custom_18 1,297 |
| 100x all | 1492 | 52,402 | 92.0% | −2.0 | −0.8 | Custom_5 4,558 |
| 100x vaults | 14.92 | 37,065 | 65.1% | +2.4 | +3.6 | Custom_5 19,895 |

Widening the ladder is the whole gain: it's the only arm that improves both what the venue fills and what it earns. Adding inventory on top gives back 3,200 fills, and inventory alone costs a third of them.
- `Custom_5` is the venue refusing to quote, because the trade was larger than its deepest tier. Raising inventory alone makes that *worse* (928 → 19,895) while the ceiling never moves, so the venue derives its cap from the ratio of inventory to quoted size.
- `Custom_18` is the venue quoting and then failing to pay, because the output vault was short.
- Read `won` and `mean bps` together. Each arm's mean is over the hops *that arm* filled, and a venue refuses what its curve prices worst — so the mean falls as the population grows even where nothing about the pricing changed. `won` is the like-for-like number.
