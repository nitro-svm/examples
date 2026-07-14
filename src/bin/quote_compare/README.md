# Comparing Aggregator Quotes

"Would my router have beaten Jupiter on that trade?" is an easy question to ask and a hard one
to answer honestly. Comparing a live quote from your aggregator against a Jupiter fill that
happened ten seconds ago compares two different worlds: the pools moved in between, and any
edge you appear to have may just be the market drifting your way. The only fair comparison is
one where both routers see *byte-for-byte the same chain state*, including the state right
before the trade you're measuring against — not after it, since that trade's own impact would
flatter or punish whoever goes second.

This example gets that fairness by pausing the chain. It replays historical slots with a
discovery filter on the Jupiter V6 aggregator, and every time a batch containing a Jupiter swap
comes up, the simulator stops *immediately before any of its transactions execute*. Reads at
that moment reflect the chain up to `batch_index - 1` — the matched swaps have not run yet.
That is the frozen instant in which the real swap was priced, and it's where the example drops
in a Titan transaction for the same pair and the same input amount, simulates it, and records
what each router would have paid out.

Nothing is committed. After the comparison the session jumps straight to the next matching
batch.

## What you get

For each Jupiter swap found in the range, one CSV row with both sides of the comparison:

`slot, tx_sig, input_mint, output_mint, input_amount, jup_out, jup_quote, jup_venues, titan_out, titan_venues`

Both the realized output and the venue split are captured for each router, so a gap can be
traced back to *where* the routes diverged, not just how much they differed by.

## Pausing is the general primitive

The Jupiter-vs-Titan comparison is one use of the pause, not the only one. Any time the
simulator stops, you have a window in which the chain is held still and you can:

- read account state with `session.rpc()`, seeing the world exactly as the pending transactions
  will see it, and
- call `session.rpc().simulate_transaction(&your_tx)` to test *anything* against that state —
  your own routing, a backrun, a liquidation, a fill you're considering.

Swap the discovery filter's program ID and the same skeleton becomes a harness for whatever
protocol you care about.

## Run it

```bash
export SIMULATOR_API_KEY=your-key
cargo run --bin quote_compare -- \
  --start-slot 417811170 \
  --end-slot 417811175
```

Results go to `results.csv` (`--output`). `--program-id` selects the program to pause on; it
defaults to Jupiter V6.

One caveat if you adapt this: the Titan side is built by patching a template transaction's
input amount, and the signer's balance is topped up before each simulation and restored after,
so one comparison can't bleed into the next.
