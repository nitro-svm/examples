# Reconstructing Prop AMM Liquidity Curves

Proprietary AMMs (BisonFi, ZeroFi, HumidiFi, GoonFi, …) don't publish an order book, and
most don't expose a quote endpoint either. From the outside, all you can see is the fills
that happened to route through them. That tells you what someone *got*, not what the venue
*would have given* at a size nobody traded — which is the number you actually need to
decide whether to route there, to market-make against them, or to size a position without
eating unexpected impact.

The only reliable way to get that number is to ask the AMM itself, but asking it on mainnet
means sending real transactions, and any size large enough to be interesting is also large
enough to move the market and cost real money. This example asks the same question against
a *frozen* historical slot instead. Chain state is held still, so you can push a 1 SOL trade,
a 2 SOL trade, a 4 SOL trade, and so on through the same venue, and every answer comes from
an identical starting book. Nothing is committed, nothing is paid, and you can do it as many
times as you like.

## What it measures

**Spread** — a round trip against the frozen state: swap quote→base, then swap that output
back base→quote. Whatever you lose on the round trip is the venue's effective spread.

```text
spread_bps = (size - final_out) / size * 10_000
```

**Depth** — a geometric sweep (each step doubles the trade size) run in both directions
until price impact crosses `--max-impact-bps` (default 1000 = 10%) or the AMM returns zero
output. Impact is measured against the spot rate implied by the smallest size in the sweep.
The headline number is depth at 200 bps, the industry convention, but the full curve is
written to CSV so you can read depth off at whatever threshold you care about.

Both are re-run at each slot in the range, so you get a time series: how the venue's spread
and depth actually breathed, block by block.

## Isolating a single venue

Real aggregator transactions fan out across several venues at once, which would blend their
curves together. To get a clean read, the example takes a Titan aggregator template
transaction and patches it down to a single venue before simulating. Pick which one with
`--venue-disciminant` (55 = BisonFi, 13 = ZeroFi, 28 = HumidiFi, 35 = GoonFi, 57 = GoonFiV2 —
see the `Venue` enum in the Titan IDL).

## Run it

```bash
export SIMULATOR_API_KEY=your-key
cargo run --bin amm_liquidity -- \
  --start-slot 422818048 \
  --end-slot 422818148 \
  --measure-spread \
  --measure-depth \
  --venue-disciminant 55
```

Neither measurement runs unless you ask for it. Results land in `spread.csv` and `depth.csv`
(`--spread-output` / `--depth-output`).

Other flags worth knowing:

- `--spread-size` — round-trip size in base-mint native units (default 50 SOL).
- `--depth-min` — smallest size in the depth sweep, in quote-mint native units.
- `--max-impact-bps` — where the sweep stops.
- `--quote-mint` / `--base-mint` — default to USDC and WSOL.
- `--enable-intra-block-inspection` — measure *inside* blocks rather than only at block
  boundaries.
