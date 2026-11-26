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
PLAN=src/bin/counterfactual_capital/<plan_name>.json
RANGE="--start-slot 460919623 --slot-count 2"
```

### Plan
`--plan` holds the venue template and says where the venue's capital and curve live in bytes. Neither half can be derived from the other. The template is one venue instruction that you serialized from a landed swap that routed through the venue. The server rewrites the amount and the taker accounts in it for each probe.

The `venueTemplate` object has the format in the [venue template reference](https://docs.termina.technology/documentation/cli/file-formats#venue-templates). `sim` reads a JSON list of templates from `--venue-templates`. The plan holds one object. The plan also accepts the old `directFill` key for the same object.

```jsonc
{
  "venueTemplate": {
    "label": "temp-sol-usdc",    // names the template on every row and in the stats
    "mode": "ExactIn",           // which hops match; ExactIn writes the input amount
    "inputMint": "So11111111111111111111111111111111111111112",   // the mint the matched hops sell
    "outputMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // the mint the matched hops buy
    "instruction": {
      "programId": "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi",
      "accounts": [
        // The role accounts come first. The server rewrites their addresses for each probe,
        // so each holds the zero address as a placeholder.
        { "address": "11111111111111111111111111111111", "signer": true },    // 0: taker
        { "address": "11111111111111111111111111111111", "writable": true }, // 1: taker's input token account
        { "address": "11111111111111111111111111111111", "writable": true }, // 2: taker's output token account
        // The venue's own accounts follow, in program order.
        { "address": "FQmFVQ7i8GCLqwE7EriUA4WQCvyCKxAeY4XUr55ZD6D7", "writable": true }, // state
        { "address": "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp", "writable": true }, // vault 0
        { "address": "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j", "writable": true }  // vault 1
      ],
      "data": { "data": "AAAAAAAAAAAAAAAAAAAAAA==", "encoding": "base64" }
    },
    "amountOffset": 8,           // byte offset of the 8-byte little-endian amount in the data
    "taker": {
      "user": 0,                 // account slots the server points at each probe's taker
      "inputTokenAccount": 1,
      "outputTokenAccount": 2
    }
    // Optional: "resolve" refreshes the instruction at each probe pass, and "taker.derived"
    // names accounts the venue derives from the taker.
  },
  "inventory": {
    "vaults": ["9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp", "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j"], // in the order the state mirrors them
    "state": {
      "account": "FQmFVQ7i8GCLqwE7EriUA4WQCvyCKxAeY4XUr55ZD6D7",
      "discriminator": "temp",                   // asserted before a byte is written
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

The state, vault and venue addresses in this sample are placeholders. Replace them with the venue's real accounts. With placeholder vaults, a run stops at `mint ... does not declare decimals`.

A `ladders` entry reads: the tier count is `width` bytes at `count`; tier *i* sits at `entries + stride*i` as a `width`-byte price then a `width`-byte size. To find the offsets, diff the state account across slots — of the fields that move, prices change and sizes do not. Omit `state` for a constant-product pool, whose vaults are its curve.

### Validate
Check the plan before opening a session. This catches what otherwise costs a replay per arm to discover: an account outside the template's instruction, one the venue writes to but the plan marks read-only, and a ladder running past the end of the account.
```sh
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN --dry-run
```

### Run
Replay the range once per arm, scaled, and report what each would have filled. Every arm is a full replay plus one for the reference pass. The 2-slot `RANGE` above only shows that the commands work. A real measurement needs a long range, and a long range takes minutes per arm. If the venue's state does not move in a short range, `capture::require_changes` refuses the plan.
```sh
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN \
  --multiple 1 --multiple 100 --scale all --scale vaults --scale ladder \
  --out decompose.jsonl
```

### Reuse a capture
`--capture` records the venue's trajectory the first time and reads it back afterwards, which saves the reference pass on every later run over the same range. The recording is refused if it names a different account than the plan.
```sh
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN \
  --multiple 1000 --scale ladder --capture venue.jsonl --out wider.jsonl
```

## Results

For the a sample venue's SOL/USDC market in the range `439649408–439659407` from 2026-09-01, over 56,960 buildable hops:

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
