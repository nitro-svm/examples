# Counterfactual Flow

Change a venue's parameters and measure how much taker flow it would have won or lost. Every historical swap is requoted through Jupiter Metis against the modified venue, and compared against a control run with no change.

The modified state is visible only to the router and nothing is committed, so other makers and taker flow stay as they were. This example shifts the venue's mid price; the same approach works for curve, capital or fee changes.

## Usage
```sh
export SIMULATOR_API_KEY=<key>
PROGRAM=BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi
POOL=8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo # BisonFi, SOL/USDC
PAIR=So11111111111111111111111111111111111111112,EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
RANGE="--start-slot 439649408 --slot-count 9999"

# 1. Record every state the pool held over the range.
cargo run --bin counterfactual_flow -- capture $RANGE --account $POOL --out capture.jsonl

# 2. Replay with the pool's price fields (byte offsets 839 and 895) moved down 0.4 bps, against a control.
cargo run --bin counterfactual_flow -- compare $RANGE --account $POOL --program-id $PROGRAM \
  --filter-pair $PAIR --capture capture.jsonl \
  --price-field 839 --price-field 895 --price-shift-bps -0.4 --out compare.jsonl
```

`compare` writes `compare-control.jsonl`, `compare-modified.jsonl` and `compare-report.jsonl`. Run `report <file>` on either arm to print its flow again without replaying.

## Results

BisonFi SOL/USDC, slots 439649408–439659407 (2026-08-24), legs routed through the pool:

| arm | SOL→USDC (sell) | USDC→SOL (buy) | total |
|---|---|---|---|
| control | 543 | 729 | 1,274 |
| −0.4 bps | 123 | 1,877 | 2,004 |
| −5 bps | 27 | 15,293 | 15,325 |

A lower price makes the pool's SOL cheap, so the router sends it buys and stops sending it sells. The total rises only because this pair's flow is mostly USDC→SOL. Whether that one-sided flow is profitable needs a markout, which this tool doesn't measure.
