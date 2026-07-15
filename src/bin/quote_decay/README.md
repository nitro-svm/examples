# Measuring Quote Decay

A quote is only good for as long as the state it was priced against holds. Between the block a
quote is calculated and the block it lands in, others trade and the output shrinks. Slippage tolerances
absorb that gap, but they're usually set by guesswork — a live RPC can't rerun a swap against a
later block to show what it would really have paid.

Real Jupiter swaps are replayed unchanged against the next 50 slots; route, size, and
signer are held constant, only the landing slot varies. The resulting output-vs-slot curve shows how fast a quote becomes stale for a given pair.

## Methodology

**Phase 1 (at `--start-slot`)** — use a `ProgramExecuted` filter on Jupiter V6 to captures the first 5 swaps in the slot: transaction, signer, and the real onchain output.

**Phase 2 (`start_slot ..= +50`)** — step through the chain slot-by-slot and simulate each captured swap against that slot's historical state. This can be compared against the original baseline.

Three details keep the replay honest:

- The signer's input balance is set before each simulation and restored after, so swaps always have enough funds and won't cause subsequent transactions to diverge.
- `min_out` is patched to zero, otherwise the original slippage guard aborts the decayed
  swaps under measurement.
- `wSOL` is special-cased in the implementation since the swap can wrap and unwrap to SOL.

## Usage

```bash
export SIMULATOR_API_KEY=<key>
cargo run --bin quote_decay -- --start-slot 422818048
```

`results.csv` (`--output`) contains one row per swap + slot, with the original onchain output next to the simulated result.
Use `--program-id` to test a different aggregator.
