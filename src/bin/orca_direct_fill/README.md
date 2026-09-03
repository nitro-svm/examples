# Direct Fill Against Your Own Pool

A venue that isn't actively tuning quotes still wants to know what its passive pools left on the table. This example takes every SOL/USDC fill that landed somewhere other than Orca and prices it against Orca's own pool at the same moment, then reports how much flow was winnable and by how much.

## Methodology

Four steps, in this order:

1. Open a one-slot session at the range and read the pool through it, deriving the venue's account
   run from what it holds. The tick-array window has to sit where the price *was*.
2. Close that session, so it stops holding capacity.
3. Open the run with that market as its direct-fill book. The session rebuilds every SOL/USDC hop
   it finds through the pool, at the hop's own size and against the state the hop executed on, and
   compares the two payouts in bps of the realized fill.
4. Report the census the session returns.

This is a direct fill, not a counterfactual: no router runs and nothing is rerouted. Same state,
same size, only the filling venue differs.

## Usage

```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin orca_direct_fill -- --url ws://<host> \
  --pool Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE \
  --start-slot 438196108 --end-slot 438206107
```

```
=== SOL/USDC direct fill vs Orca ===
hops matched : 20712
probes built : 20686
not built    : {"folded_route": 26}

outcomes:
  filled                         20686  (100.0%)

scored 20686 of 20686 fills, mean -5.7 bps vs the venue that traded
Orca would have paid 5.7 bps less than the winning venue on average.
```

A run also writes a row per probe server-side, in the session's `direct_fill_probes` table, which
carries the size and direction the census aggregates away.

## The venue market spec

`orca-sol-usdc.json` is the spec `sim run --direct-fill` reads: which router encodes the hop, the
venue to pin it to, and the venue's own account run — the 16 accounts ending at the Whirlpools
program, which Titan forwards to the venue untouched.

It is derived from the pool account, not harvested from a landed transaction. `orca_market_spec`
reads the pool's mints, vaults and tick spacing, then derives the tick arrays and oracle as PDAs:

```sh
cargo run --bin orca_market_spec -- --url ws://<host> \
  --pool Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE --start-slot <start> \
  > src/bin/orca_direct_fill/orca-sol-usdc.json

sim run --url ws://<host>/backtest --api-key <key> \
  --start-slot <start> --end-slot <end> \
  --reroute-order-flow --reroute-requote false \
  --direct-fill src/bin/orca_direct_fill/orca-sol-usdc.json
```

`--start-slot` reads the pool through a one-slot session positioned there, so the window sits where
the price actually was.
