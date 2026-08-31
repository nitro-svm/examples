# Counterfactual Flow

Change a venue's quote and measure the taker flow it wins or loses.

The change is an **account override**: the venue's own captured state, modified, visible only to
the router. It re-quotes every historical swap against that state; nothing is committed, so other
venues and taker flow are unchanged.

## How it works

1. `capture` records the account's state at each slot it changed.
2. `run` posts those states back, optionally modified, and reports what the venue captured.
3. `compare` runs the null control and one arm in a single invocation and diffs them leg by leg.
4. `report` reads a run's output back and reports what crossed the venue, on L1 and after the
   re-quote, in swaps and in dollars.

Two modifications are supported:

- `--price-shift-bps` moves a stored price. Posted at the state's own slot.
- `--lag K` / `--lead K` posts slot `s∓K`'s state at slot `s` — the venue updating late or early.

`--setup-transactions` carries a time shift as the venue's own update transaction re-executed at
the shifted slot, instead of as bytes. Needed for `--lead` on a venue that stamps its last-update
slot, since a future snapshot is rejected. Requires a `--no-replay` capture.

## What is counted

`run` prints the size of that population on every run. It is the denominator every other count is
a share of:

```
[run] 19411 re-quoted legs seen
```

## Try it

```sh
export SIMULATOR_API_KEY=<key>
POOL=8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo             # BisonFi, SOL/USDC
PROGRAM=BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi
PAIR=So11111111111111111111111111111111111111112,EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
RANGE="--start-slot 439649408 --slot-count 9999"
ARGS="--account $POOL --filter-pair $PAIR --program-id $PROGRAM"

# 1. Capture.
cargo run --bin counterfactual_flow -- capture $RANGE --account $POOL --out capture.jsonl

# 2. Control: same bytes, unmodified, same slot.
cargo run --bin counterfactual_flow -- run $RANGE $ARGS \
  --capture capture.jsonl --lag 0 --out control.jsonl

# 3. The venue quoting 0.4 bps lower.
cargo run --bin counterfactual_flow -- run $RANGE $ARGS --capture capture.jsonl \
  --price-field 839 --price-field 895 --price-shift-bps -0.4 --out worse.jsonl
```

Step 1 ends on stdout with the row count, and with how many of those rows carry the update
transaction `--setup-transactions` would need:

```
captured <n> states (<n> with a transaction) to capture.jsonl
```

An account that never wrote in the range is an error, not an empty file
(`account <A> never appeared in [<start>, <end>]`).

Steps 2 and 3 take the default venue set (Jupiter alone), which is the population every number
in [Results](#results) is measured over.

### A time shift carried as the venue's own transaction

`--setup-transactions` re-executes the update transaction that produced each captured state, at
the shifted slot, so a venue that stamps its own last-update slot accepts a `--lead`. Replay
records no transaction to re-execute, so the capture needs `--no-replay`.

```sh
# 1. Capture with the update transactions attached.
cargo run --bin counterfactual_flow -- capture $RANGE --no-replay \
  --account $POOL --out capture-tx.jsonl

# 2. The venue updating 2 slots sooner.
cargo run --bin counterfactual_flow -- run $RANGE $ARGS --capture capture-tx.jsonl \
  --lead 2 --setup-transactions --out lead2.jsonl
```

### Picking a range

`--start-slot` and `--slot-count` are not free. `--slot-count N` covers the inclusive range
`[start, start + N]`, and replay resolves the recorded account-state bundle on that range, 
so make sure to build account bundles if you want fast replay. List what is available via:

```sh
curl -s https://simulator.termina.technology/available-ranges | jq
```

The 10,000-slot range used here replays in 5–8 minutes, and `--setup-transactions` costs about
20% more. `compare` runs two sessions back to back, so double it. A session waits up to 15
minutes for capacity before failing, so a slow start is not a hang.

### Finding the pool

`POOL` is the venue's own pool for the pair. Read it out of the router's chosen routes on a probe
run — no `--capture`, so nothing is overridden:

```sh
cargo run --bin counterfactual_flow -- run $RANGE --program-id $PROGRAM \
  --filter-pair $PAIR --account $PROGRAM --out probe.jsonl

jq -r '.legs[].routePlan // empty' probe.jsonl \
  | jq -r '.[] | select(.swapInfo.label == "BisonFi") | .swapInfo.ammKey' \
  | sort | uniq -c | sort -rn
```

The run resolves `--program-id` to the label to match on and prints it:

```
[jup] resolved venue label: "BisonFi"
```

`--account` is required by the parser but unused without `--capture`, so the program id stands in
for it.

## Reading the output

`run` writes everything to stderr; only `capture` and `compare` put a summary on stdout. Every
replayed slot emits a `[slot] <n>` line, so redirect stderr to a file rather than reading it live.

```
[jup] resolved venue label: "BisonFi"
[price] moved 2/2 --price-field(s) by -0.4 bps over <n> states, <n> writes
[null shift] <n> anchor slots, carried as account bytes
[slot] 439649408
...
[run] reroute: <n> detected -> <n> rerouted -> <n> simulated -> <n> succeeded | <n> requote-fail
[run] <n> re-quoted legs seen
[run] venue on L1: legs=<n> | after re-quote: legs=<n> transactions=<n> (held=<n> won=<n> lost=<n> split=<n>) | legs where metis quoted higher=<n>
[run] won/lost read only as a difference against the `--lag 0` control, which itself reports the venue losing most of its L1 legs with nothing changed — an absolute lost is not attributable
[run]   So1111..1112->EPjFWd..Dt1v: L1=<n> after=<n> (held=<n> won=<n> lost=<n> split=<n>) improved=<n>
[run]   EPjFWd..Dt1v->So1111..1112: L1=<n> after=<n> (held=<n> won=<n> lost=<n> split=<n>) improved=<n>
```

- **detected** — swaps found in the range on the admitted venues, before
  `--filter-pair`. Filter misses, arbitrage cycles and swaps the router could not quote all drop out
  between here and **rerouted**
- **rerouted** — swaps re-quoted through the router and queued for simulation.
- **simulated** — routed transactions simulated against the frozen state.
- **succeeded** — simulations that ran without reverting. Everything below counts quotes, not fills.
- **requote-fail** — detected swaps the router could not route at all.
- **re-quoted legs seen** — the population. Every count below is a share of it.
- **venue on L1 legs** — legs whose original L1 route ran through `--program-id`, matched on the
  program id.
- **after re-quote legs** — legs whose re-quoted route names the venue's label.
- **transactions** — original transactions with at least one leg on the venue after the re-quote.
- **held** — legs the venue had on both sides.
- **won** = after − held. Legs the re-quote took from another venue.
- **lost** = L1 − held. Legs the venue had on a recovered L1 route and the re-quote sent to a different venue.
- **split** — legs where the venue took less than the whole route on one side or the other.
- **improved** (`legs where metis quoted higher` on the totals line) — legs on the venue after the
  re-quote whose quote beat the original's.

The per-direction lines carry the same fields for one `(input, output)` mint pair, busiest first.
A direction appears only where the venue was on one side of it, so the lines do not add up to the
direction's whole flow. Mints show as their symbol where the range knows one, and otherwise
abbreviated to the first six and last four characters (`USD1tt..EmuB`), so grepping either form
for a full mint finds nothing.

Lines that appear only when they apply:

- `[jup] resolved venue label: ...` — with `--program-id`. Nothing under `venue on L1` exists
  without it.
- `[price] ...` — with `--price-shift-bps`. Reports offsets written against offsets asked for,
  and warns per offset that held no positive value in any state — probably padding.
- `[<label>] <n> anchor slots, carried as account bytes` — with a schedule. `<label>` is `lag K`,
  `lead K`, or `null shift` for `--lag 0` and for a re-price with no time shift; the carrier reads
  `setup transactions` for the other one. `<n>` is how many slots the arm actually posts at: far
  below the range means the venue barely moved during it.
- `[setup] <n>/<n> captured states have no transaction and are skipped` — `--setup-transactions`
  on a capture that did not resolve every transaction.
- the whole `venue on L1` block, including the per-direction lines — with `--program-id`, and only
  when at least one leg had the venue on one side.
- `[run] <n> of the venue's legs were split routes it only partly held; held/won/lost count
  participation, not share` — when `split=` is non-zero.
- `[run] <n> legs carried no recoverable L1 route; their before-side is unknown` — those legs are
  in neither `L1` nor `held`, so each one lands in `won` whether the venue took it from anyone or
  not.
- `[run] <n>/<n> scheduled actions failed and posted no state; those slots kept the previous
  override in force` — the denominator is the anchor-slot count above. A failed action leaves 
  the venue on an older shifted state, not on its unmodified one, and the arm is diluted 
  towards that older shift rather than towards baseline.

## `report`

What crossed the venue, on L1 and after the re-quote, read back from a run's output:

```sh
cargo run --bin counterfactual_flow -- report reroute-out.jsonl
```

## `compare`

`compare` runs both arms itself and joins them by leg. Its reference arm is the **no override reroute** —
the same `--capture`. So `--capture` is required, and so is one of `--lag`/`--lead`/`--price-shift-bps`:

```sh
cargo run --bin counterfactual_flow -- compare $RANGE $ARGS --capture capture.jsonl \
  --price-field 839 --price-field 895 --price-shift-bps -0.4 \
  --report compare-report.jsonl
```

The two sessions run back to back, announced by `[control] running the reroute (no override)...` 
and `[modified] running with <label>...`. Each prints its own block from
[Reading the output](#reading-the-output), tagged `[control]` and `[modified]` instead of `[run]`.
Then, on stdout:

```
=== null shift vs the control ===
legs matched: <n> (<n> moved), <n> legs excluded where the control quoted zero
|delta| bps: median <x.xxx> | mean <x.xxx> | p90 <x.xxx>
venue legs captured: control <n> -> modified <n>
report written to compare-report.jsonl
```

- The header names the arm's time shift, so a pure `--price-shift-bps` arm reads `null shift`;
  `--lag 3` reads `lag 3`.
- **legs matched** — legs present in both runs. **moved** counts a non-zero delta; deltas are
  fractional bps, because router jitter routinely moves a quote by well under one.
- The `excluded` clause appears only when the control quoted zero out on a matched leg, where a
  delta is undefined.
- **venue legs captured** appears only with `--program-id`. It is `after re-quote legs` on both
  sides, and carries the same run-to-run spread as any other leg count.
- Report rows carry `shift`, `originalSignature`, `legIndex`, both mints, `amount`,
  `originalQuotedOut`, `baseQuotedOut` (the control), `quotedOut` and `deltaBps`.
- `--out` is inherited from `run` and unused here: `compare` writes only `--report`.

The per-leg deltas are a matched pair over the same legs and are the sturdier number; the leg
counts are two separate sessions. The tool says as much on exit:

```
[note] single-run deltas include router noise; repeat both runs before attributing
```

## Results

Range 439649408–439659407, staging, 2026-08-24. All five arms run 3 times;
each is read against the control. ~19,360 re-quoted legs per arm.

Legs on the venue, per direction. The first row is what it held on L1, before any re-quote:

| arm | carrier | SOL→USDC (sell) | USDC→SOL (buy) | total |
|---|---|---|---|---|
| L1, of the re-quoted | — | 180 | 385 | 565 |
| control | none | 543 | 729 | 1,274 |
| 0.4 bps lower | bytes | 123 | 1,877 | 2,004 |
| 5 bps lower | bytes | 27 | 15,293 | 15,325 |
| `--lag 10` | setup tx | 830 | 532 | 1,366 |
| `--lead 10` | setup tx | 748 | 661 | 1,413 |

The L1 row is measured per arm and lands at 560–568 every time. It should: the override changes
what the router picks, not what the original did. An L1 row that moves with the arm means
something is counting the treatment into the baseline.

It is not the venue's whole L1 footprint: only re-quoted swaps are reported, so the row counts L1
legs among those. On this range the venue was on 841 L1 SOL/USDC legs — 561 re-quoted, 271
skipped, 9 the router could not quote. Read every row as a share of the re-quoted population: the
ratios hold because both sides are over it, but the levels understate L1 by a third.

**Quoting lower is not winning.** 0.4 bps — inside the venue's own 0.7–1.4 bps spread — costs 77%
of the sell side while multiplying the buy side by 2.6. At 5 bps the sell side is gone (−95%) and
the buy side is 21x. The totals read as a large gain because this pair is mostly USDC→SOL; the
gain is flow that arrives because the venue is mispriced in the taker's favour.

**A late price and an early one both gain flow.** `--lag 10` and `--lead 10` are +18% and +22%
over their control, in both directions. A shifted price is wrong in a random direction and the
router routes to it exactly when the error favours the taker.

Caveats on the two time arms: 10.5% (control), 21.3% (lag 10) and 11.0% (lead 10) of scheduled
setup transactions failed and left no override, those slots ran a exactly as control.

The control itself holds only 160 of 564 L1 legs — it "loses" 72% with byte-identical state at the
same slot, but wins elsewhere. `won`/`lost` are therefore not absolute flow.
Only the difference against the control carries meaning.

A few legs per arm land on mints outside the pair: `--filter-pair` restricts which swaps
are re-quoted, not which mints a route may cross - a different route might omit the pair if a different
hop is more profitable.
