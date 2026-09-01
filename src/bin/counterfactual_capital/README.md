# Counterfactual Capital

Scale a venue's inventory and its quoting curve, and measure the flow it would have filled.

The change is an **account override**: the venue's own captured state, scaled, visible only to the
direct-fill probe. Every historical swap on the pair is rebuilt through the venue and priced against
that state; nothing is committed, so other venues and taker flow are unchanged.

## How it works

1. The range is replayed once with nothing overridden, subscribed to the venue's accounts, and
   every change to them is recorded.
2. Each arm posts those states back, scaled, and reports what the venue filled and at what margin.
3. The `1x` arm posts them unscaled, so it must reproduce the reference pass exactly. The run
   prints both and warns if they differ.

Three modifications are supported:

- `--multiple K --scale vaults` scales the vault balances and the copies the venue keeps of them.
- `--multiple K --scale ladder` scales every ladder tier's size — the quote ceiling.
- `--tighten-bps D` moves each tier's price toward the other side by `D` bps of its own price,
  never past the midpoint.

`--scale all` is the default and does vaults and ladder together. Every flag is repeatable and
crossed with the others, so one run covers the decomposition. `--multiple` must include `1`.

## The plan file

`--plan` is one JSON document naming the venue's routing identity and where its capital and curve
live in bytes. The two halves come from different places and neither can be derived from the other.

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

A `ladders` entry reads: the tier count is `width` bytes at `count`; tier *i* sits at
`entries + stride*i` as a `width`-byte price then a `width`-byte size. The count is read, never
assumed — outside `1..=maxTiers` the run refuses rather than scaling whatever integer is there.
Omit `state` for a constant-product pool, whose vaults are its curve.

Nothing derives the account run. A router's IDL declares only the accounts the *router* touches;
the venue's run arrives as `remaining_accounts` and is forwarded to the CPI untouched. Take it from
the venue, or from a landed swap that routed through it. Two traps, each worth a replay:

- The run ends with the venue's own program id. The runtime resolves a CPI callee by finding it in
  the calling instruction's account list; omit it and every route fails before pricing anything.
- An account the venue writes to but the run marks read-only reverts at execution, not at build.

To find the ladder offsets, diff the state account across slots. Nearly everything is constant; of
what moves, the prices change and the sizes do not. A block of same-stride fields where one column
moves and its neighbour never does is a price/size ladder, and the mirrors are the fields that track
the vaults exactly.

## What is counted

Every arm reports the same denominator, and it is a property of the range, not of the override:

```
[run] <n> hops matched the venue's pair, <n> were buildable — the denominator below
```

Detection does not read overrides, so both counts must be identical across arms. The run warns if
they are not.

## Try it

```sh
export SIMULATOR_API_KEY=<key>
PLAN=src/bin/counterfactual_capital/tempest-sol-usdc.json
RANGE="--start-slot 439649408 --slot-count 9999"

# Validate the plan without opening a session.
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN --dry-run

# The decomposition: which knob the venue is actually short of.
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN \
  --multiple 1 --multiple 100 --scale all --scale vaults --scale ladder \
  --out decompose.jsonl

# The size response of one knob.
cargo run --bin counterfactual_capital -- $RANGE --plan $PLAN \
  --multiple 0.1 --multiple 1 --multiple 2 --multiple 10 --multiple 100 \
  --out sweep.jsonl
```

Run `--dry-run` first. It checks everything that otherwise costs a replay per arm to discover: an
account outside the venue's run (never loaded, so scaling it changes nothing), one the venue writes
to but the run marks read-only, a ladder that runs past the end of the account, and an arm matrix
entered by accident.

Each arm is a full replay, plus one for the reference pass. The 10,000-slot range above replays in
4–6 minutes per arm. A session waits up to 15 minutes for capacity before failing, so a slow start
is not a hang. `sim ranges` lists what exists.

## Reading the output

Everything goes to stderr except one number; every replayed slot emits a `[slot] <n>` line, so
redirect stderr to a file rather than reading it live. Rows are written to `--out` as each arm
lands, so a failure on the last one does not discard the ones before it.

```
[capture] <n> slots carry a change to the venue's state — one override each
[baseline] <addr> holds <x> of <mint>
[arm] 1x — <n> overrides
[run] <n> hops matched the venue's pair, <n> were buildable — the denominator below

             arm             vaults posted      max trade      won   filled   mean bps      vs 1x
              1x                 <x> / <x>            <x>      <n>     <x>%       <±x>          -
            100x                 <x> / <x>            <x>      <n>     <x>%       <±x>       <±x>

Why the rest were not filled:
              1x  reverted_exec_ix_Custom_5 <n>, reverted_exec_ix_Custom_18 <n>

[diag] reference: <±x> bps, 1x arm following the same trajectory: <±x> — gap <±x> bps
```

- **max trade** — the deepest tier the arm quotes: the ceiling on any single fill. Below the flow's
  trade sizes, capital is not the constraint.
- **won** — buildable hops the venue filled. The like-for-like number across arms.
- **mean bps** — margin against what each hop actually filled at. Positive beat the venue that traded.
- **vs 1x** — the same against the control arm.
- **Custom_5** — the venue refused to quote: the trade was larger than its deepest tier.
- **Custom_18** — the venue quoted and could not pay: the output vault was short.
- The `[diag]` line is the self-test. A `1x` arm rewrites the venue with its own bytes, so a gap
  means the capture missed changes and every arm inherits the same hole.

Read `won` and `mean bps` together. Each arm's mean is over the hops *that arm* filled, and a larger
arm fills hops a smaller one refused — a venue refuses what its curve prices worst, so the mean
falls as the population grows even where nothing about the pricing changed. The run says so whenever
the arms score different populations.

Lines that appear only when they apply:

- `[warn] every arm's outcomes are identical to the control's` — with the remedy depending on which
  knob was turned. Under `--scale vaults` against a ladder venue this is the finding, not a fault.
- `[warn] the <x> arm filled <n> against the control's <n>` — a sub-1 arm that did not lose fills.
  The lever is not connected and no arm above the control is attributable.
- `[warn] the <x> arm's mean bps is -10000` — routes that succeeded and delivered nothing. The
  venue's mint ordering in the plan is reversed.
- `[warn] <n> of <n> captured slots could not be scaled and were skipped` — the previous override
  stayed in force at those slots.

Last on stdout is the smallest multiple that filled everything it was offered, or `0`.

## Results

Range 439649408–439659407, 2026-09-01. Tempest SOL/USDC. 58,453 hops matched, 56,960 buildable.

| arm | max trade | won | filled | mean bps | vs 1x | refused |
|---|---|---|---|---|---|---|
| 1x | 14.92 | 55,600 | 97.6% | −1.2 | — | Custom_5 928, Custom_18 432 |
| 100x ladder | 1492 | 55,663 | 97.7% | +5.9 | **+7.1** | Custom_18 1,297 |
| 100x all | 1492 | 52,402 | 92.0% | −2.0 | −0.8 | Custom_5 4,558 |
| 100x vaults | 14.92 | 37,065 | 65.1% | +2.4 | +3.6 | Custom_5 19,895 |

**Widening the ladder is the whole gain.** 100x on tier sizes wins +7.1 bps and is the only arm that
improves both what the venue fills and what it earns.
