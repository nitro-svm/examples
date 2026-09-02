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
RANGE="--start-slot 439649408 --slot-count 9999"
ARGS="--account $POOL --filter-pair $PAIR --program-id $PROGRAM"
```

### Capture
Record the pool's account every time it changes, so the next step can override it with the counterfactual state.
```sh
cargo run --bin counterfactual_flow -- capture $RANGE --account $POOL --out capture.jsonl
```

### Run
Replay the range with a modified version of the pool, that's only visible to the router.

Shift oracle updates by -0.4 bps. Since there's no IDL, use byte offsets (839, 895) to apply the update.
```sh
cargo run --bin counterfactual_flow -- run $RANGE $ARGS \
  --capture capture.jsonl \
  --price-field 839 --price-field 895 --price-shift-bps -0.4 --out worse.jsonl
```

### Compare
Instead of running a control and experiment as two separate sessions: use `compare` to do this automatically.

```sh
cargo run --bin counterfactual_flow -- compare $RANGE $ARGS --capture capture.jsonl \
  --price-field 839 --price-field 895 --price-shift-bps -0.4 \
  --report compare-report.jsonl
```

### Report
Read a run's output and report what crossed the venue, on L1 and after the requote, in swaps and in dollars.

```sh
cargo run --bin counterfactual_flow -- report reroute.jsonl
```

## Results

For the BisonFi SOL/USDC pool in the range `439649408–439659407` from 2026-08-24:

| arm | SOL→USDC (sell) | USDC→SOL (buy) | total |
|---|---|---|---|
| control | 543 | 729 | 1,274 |
| 0.4 bps lower | 123 | 1,877 | 2,004 |
| 5 bps lower | 27 | 15,293 | 15,325 |

Shifting the price down makes the pool's SOL cheap, so the router starts sending buys and stops sending sells. 
- At −0.4 bps, the sell side drops to 0.23x and the buy side grows to 2.6x.
- At −5 bps, the sell side is effectively gone at 0.05x and the buy side is 21x. 
- The totals rise only because this pair is mostly USDC→SOL. What the pool gains is one-sided flow--whether it's profitable would need a markout, which this tool doesn't measure.
