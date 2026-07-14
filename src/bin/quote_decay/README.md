# Measuring Quote Decay

A quote is only good for as long as the state it was priced against stays put. Between the
moment an aggregator hands you a route and the moment your transaction actually lands, other
people trade, pools move, and the output you were promised quietly shrinks. That gap is why
slippage tolerances exist — but "how much slippage should I allow?" is usually answered with
a guess, because nobody can rerun the same swap against a later block to see what it would
have paid out.

That is exactly what this example does. It takes real Jupiter swaps, freezes them, and replays
each one unchanged against the next 50 slots of chain state. The input amount, the route, the
signer, everything is held constant — the only thing that varies is *when* the swap lands. The
resulting curve of output-vs-slot is your quote decay: an empirical, per-pair answer to how
fast a quote goes stale.

Use it to set slippage tolerances from data rather than folklore, to decide whether a slower
(cheaper) landing strategy actually costs you anything, or to compare how quickly different
pairs and route shapes rot.

## How it works

**Phase 1 — collect (at `--start-slot`).** Open a session with a `ProgramExecuted` discovery
filter on Jupiter V6 and capture the first 5 swaps in the slot: the transaction itself, its
signer, and the input/output amounts that really happened on-chain. Those on-chain amounts are
the baseline every later simulation is compared against.

**Phase 2 — replay (`start_slot ..= start_slot + 50`).** Step forward one slot at a time and,
at each slot, re-simulate every captured swap against that slot's frozen state, recording the
output amount. Both sessions are created concurrently so phase 2 is warm the moment phase 1
finishes.

A few things the example has to do to make the replay honest, which are worth stealing if you
write your own:

- The signer's input balance is set before each simulation and restored afterwards, so a swap
  never runs short of funds and never contaminates the next one.
- The transaction's `min_out` is patched to zero. Otherwise the original slippage guard would
  abort exactly the decayed swaps you're trying to measure.
- WSOL is handled specially. If the swap wraps its own SOL, native lamports are set and the
  transaction creates its own ATA; on the output side, `CloseAccount` may not run under
  simulation, so the code reads the wSOL ATA when the native gain comes back as zero.

## Run it

```bash
export SIMULATOR_API_KEY=your-key
cargo run --bin quote_decay -- --start-slot 422818048
```

Results go to `results.csv` (`--output`), one row per (swap, slot) with the original on-chain
output alongside the simulated one, so decay is a subtraction away. Point `--program-id` at a
different aggregator to measure it instead.
