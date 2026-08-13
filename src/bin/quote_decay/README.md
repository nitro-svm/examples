# Measuring Quote Decay

A quote is priced against one block but lands several blocks later. In between, other trades move onchain liquidity, so the fill can come back smaller than promised. This example measures a specified router's stability and reliance on transient liquidity.

By default, it replays Jupiter swaps against the next 50 slots--route, size, and signer are held constant, only the landing slot varies. The resulting output-vs-slot curve shows how fast a quote becomes stale for a given pair.

## Background
![Quote Decay for Select Transactions](./Quote%20Decay%20for%20Select%20Transactions.png)

Some venues may adopt aggressive pricing strategies: they offer one price at quote time to capture taker flow from aggregators but actually use a worse price to execute the fill. The sample code generates data like the graph above, where the yellow line represents a venue that oscillates between prices for a SOL -> USDC swap.

## Methodology

**Phase 1 (at `--start-slot`)**: use a `ProgramExecuted` filter on Jupiter V6 to captures the first 5 swaps in the slot: transaction, signer, and the real onchain output.

**Phase 2 (`start_slot ..= +50`)**: step through the chain slot-by-slot and simulate each captured swap against that slot's historical state. This can be compared against the original baseline.

Implementation Details:
- The signer's input balance is set before each simulation and restored after, so swaps always have enough funds and don't cause subsequent transactions to diverge.
- `min_out` is patched to zero, otherwise the original slippage guard aborts the decayed swaps under measurement.
- `wSOL` is special-cased in the implementation since the swap can wrap and unwrap to SOL.

## Usage
```bash
export SIMULATOR_API_KEY=<key>
cargo run --bin quote_decay -- --start-slot 422818048
```

The output CSV (specified via `--output`) contains one row per swap + slot, with the original onchain output next to the simulated result. Use `--program-id` to test a different aggregator.
