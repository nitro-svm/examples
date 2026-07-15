# Measuring Spread and Depth

Prop AMMs don't quote via resting orders on an orderbook, so spread and depth have to be
measured by simulating the venue at differing sizes and reading the output.

A mainnet RPC can only support this analysis for the current block, since it doesn't have historical state. It also lacks support for `simulateTransaction` account overrides, which means the signer must already hold every size being quoted.

This example iterates through the specified time range and samples the liquidity curve for the specified venue at every slot.

## Methodology

**Spread**: a round trip against the frozen state (quote->base then back to base->quote). 
What the round trip loses is the effective spread: `(size - final_out) / size * 10_000` bps.

**Depth**: a sweep that doubles trade size each step until price impact crosses
`--max-impact-bps` (default 1000) or output is zero. 
Impact is measured against the spot rate implied by the smallest size. The full curve in both directions (quote->base and base->quote) is recorded in the output CSV.

Both measurements use a Titan swap transaction as a starting template but patch it to force the trade through a single venue of interest. Select the venue with `--venue-disciminant` (e.g. 55 = BisonFi,
13 = ZeroFi, 57 = GoonFiV2; see the `Venue` enum in the Titan IDL).

## Usage

```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin amm_liquidity -- \
  --start-slot 422818048 \
  --end-slot 422818148 \
  --venue-disciminant 55
```

Additonal Flags: 
- `--spread-size` (base native units), 
- `--depth-min` (smallest sweep size, quote native units), 
- `--max-impact-bps`, 
- `--quote-mint` and `--base-mint` (default USDC, WSOL), 
- `--enable-intra-block-inspection` to measure inside blocks
