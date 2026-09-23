# Counterfactual Flow

This example updates a venue's parameters then measures the effect on taker flow. Every historical swap is requoted through Jupiter Metis, so new routes reveal whether the parameter change would've captured fills that were routed elsewhere historically.

## Methodology
The session reroutes all historical order flow through Metis and simulates the resulting swap in place of the original. It doesn't commit the new swap to avoid noisy feedback loops and holds other maker quotes and taker flow constant.

The parameter update can be a liquidity curve change, capital deployment, or fee change, but this code tests the effect of shifting down the mid price. See [results](./README.md#results).

## Usage
### Setup 
Set the environment with the venue and time range of interest.
```sh
export SIMULATOR_API_KEY=<key>
PROGRAM=BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi
POOL=8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo # BisonFi, SOL/USDC
PAIR=So11111111111111111111111111111111111111112,EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
RANGE="--start-slot 449059373 --slot-count 49999"
ARGS="--account $POOL --filter-pair $PAIR --program-id $PROGRAM"
```
The range is the one the recorded `.adlt` covers, and replay matches its start slot exactly: keep
`--start-slot` as it is and dial `--slot-count` down to anything up to `49999` to run a shorter
slice. These are bash idioms — `$RANGE` is one argument in fish and clap will refuse it.

Ranges above slot **447120000** need a simulator that implements transaction v1, which the
`enable_tx_v1` feature activated there. A v1 transaction carries its budget in
`message.transactionConfig` rather than in ComputeBudget instructions, so an older build diverges
from the recorded chain on the first one it meets.

### Capture
Record the pool's account every time it changes, so the next step can override it with the counterfactual state.
```sh
cargo run --bin counterfactual_flow -- capture $RANGE --account $POOL --out capture.jsonl
```

### Run
Replay the range with a modified version of the pool, that's only visible to the router. Pass the
same `--account` the capture was taken for; a mismatch is refused.

Shift oracle updates by -0.4 bps. Since there's no IDL, use byte offsets (839, 898) to apply the update.

The offsets are the two places this pool stores its price, established by byte-churn census over a
captured range: every byte a shift rewrites is a byte that moves on its own in live traffic. The
field at 898 is narrow — it holds tens of thousands of units, so one unit is on the order of
0.1 bps and a shift of that size quantises upward (over this range -0.4 bps lands on -0.462). Read
the realised shift off the capture rather than assuming the requested one.

Re-census the offsets whenever the range or the program changes: `reprice` only declines to write
an offset holding a non-positive value, so a drifted offset that still reads positive is written
without complaint. Offset 895 was used until it was found to be three bytes low — it moves the
price correctly but also rewrites three frozen bytes on every state it touches.
```sh
cargo run --bin counterfactual_flow -- run $RANGE $ARGS \
  --capture capture.jsonl \
  --price-field 839 --price-field 898 --price-shift-bps -0.4 --out worse.jsonl
```

### Compare
Instead of running a control and experiment as two separate sessions: use `compare` to do this automatically.

```sh
cargo run --bin counterfactual_flow -- compare $RANGE $ARGS --capture capture.jsonl \
  --price-field 839 --price-field 898 --price-shift-bps -0.4 \
  --out compare.jsonl
```
Both arms and the per-leg report are named from `--out`: `compare-control.jsonl`,
`compare-modified.jsonl`, `compare-report.jsonl`.

### Report
Read a run's output and report what crossed the venue, on L1 and after the requote, in swaps and in dollars.

```sh
cargo run --bin counterfactual_flow -- report reroute.jsonl
```

Pass `--against` to read a run beside its control, which is what `compare` prints at the end of a
session. It reads the two files only, so the comparison re-renders in seconds instead of costing
another replay.

```sh
cargo run --bin counterfactual_flow -- report modified.jsonl --against control.jsonl
```

## Results

Measured on the BisonFi SOL/USDC pool over `449059373–449109372` (2026-09-22), the full 50,000
slots, with the `839`/`898` offsets.

Re-quoted swaps, by direction:

| arm | SOL→USDC (sell) | USDC→SOL (buy) | all pairs |
|---|---|---|---|
| control | 3,760 | 2,865 | 6,896 |
| 0.4 bps lower | 2,893 | 5,995 | 9,552 |
| 5 bps lower | 169 | 14,351 | 15,629 |

Volume on L1 and after the re-quote. The change is measured against the control, not against L1:
most of the gap between those two columns is the router's own work, so reading one arm alone
credits the price shift with it.

| arm | on L1 | re-quoted | venue share | the change alone |
|---|---|---|---|---|
| control | $2.93M | $6.95M | 44.2% | — |
| 0.4 bps lower | $2.99M | $8.09M | 48.7% | +$1.13M (1.16x) |
| 5 bps lower | $3.10M | $11.82M | 56.8% | +$4.87M (1.70x) |

The L1 column drifts between arms, and that drift is noise rather than signal: the run is not
reproducible leg for leg. Two control runs over the same 10,000 slots, same arm and no shift at
all, came out 1.6% apart on L1 swaps (766 vs 754) and 3.7% apart on re-quoted volume. The spread
across the three arms above is 0.2%, well inside that. Read a difference as real only when it
clears the noise floor — which is why the 5 bps arm is legible at 2,000 slots and the 0.4 bps arm
is not.

Shifting the price down makes the pool's SOL cheap, so the router starts sending buys and stops
sending sells.
- At −0.4 bps, the sell side falls to 0.77x and the buy side grows to 2.1x.
- At −5 bps, the sell side is all but gone at 0.04x and the buy side is 5.0x.
- What the pool gains is one-sided flow — whether that is profitable needs a markout, which this
  tool doesn't measure.
- Give a small shift the whole range. Over 500–2,000 slots the −0.4 bps margin changes sign
  between runs; only the full 50,000 separates it from noise. The −5 bps effect is legible far
  sooner.
