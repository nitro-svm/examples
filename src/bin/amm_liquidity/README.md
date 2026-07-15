# Measuring Prop AMM Spread and Depth

Prop AMMs quote on demand instead of resting orders on a book. Spread and depth therefore have to be
measured: quote the venue at many sizes and read the prices back.

A mainnet RPC can do this for the current block only. It holds no historical state, and
`simulateTransaction` has no state override, so the signer must already hold every size being quoted.

This example simulates against a frozen historical slot and sets balances before each swap. Any size
can be quoted at any slot, and the whole sweep re-runs at each slot in a range.

## Methodology

**Spread** — a round trip against the frozen state (quote→base, then back base→quote). What the
round trip loses is the effective spread, `(size - final_out) / size * 10_000` bps.

**Depth** — a sweep doubling trade size each step, both directions, until price impact crosses
`--max-impact-bps` (default 1000) or output hits zero. Impact is measured against the spot rate
implied by the smallest size. Depth at 200 bps is the headline; the full curve goes to CSV.

Aggregator transactions fan out across venues, which would blend their curves, so a Titan template
transaction is patched down to one venue first. Select it with `--venue-disciminant` (55 = BisonFi,
13 = ZeroFi, 28 = HumidiFi, 35 = GoonFi, 57 = GoonFiV2 — see the `Venue` enum in the Titan IDL).

## Usage

```bash
export SIMULATOR_API_KEY=<key>
cargo run --bin amm_liquidity -- \
  --start-slot 422818048 --end-slot 422818148 \
  --measure-spread --measure-depth --venue-disciminant 55
```

Neither measurement runs unless asked for; results go to `spread.csv` / `depth.csv`
(`--spread-output`, `--depth-output`). Also: `--spread-size` (base native units), `--depth-min`
(smallest sweep size, quote native units), `--max-impact-bps`, `--quote-mint` / `--base-mint`
(default USDC, WSOL), and `--enable-intra-block-inspection` to measure inside blocks.
