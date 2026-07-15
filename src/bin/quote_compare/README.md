# Comparing Aggregator Quotes

Comparing a router's quote today against a Jupiter fill from last week proves nothing. The pools
moved in between, so the difference is mostly drift. A fair comparison prices both routers against
the same state: the state immediately before the Jupiter swap executed. It has to be *before*,
because the swap moves the pools it touches, which would penalize whichever router quotes second.
No RPC serves that state.

This example pauses the chain there instead. It replays historical slots with a discovery filter on
Jupiter V6. When a batch containing a Jupiter swap comes up, the simulator stops before any of its
transactions execute, so reads reflect the chain up to `batch_index - 1`. It then prices a Titan
transaction for the same pair and input amount, records what each router paid out, and jumps to the
next matching batch.

## Output

One CSV row per Jupiter swap. Each row records both routers' output and their venue splits, so a gap
in output can be traced back to the venues where the routes diverged.

`slot, tx_sig, input_mint, output_mint, input_amount, jup_out, jup_quote, jup_venues, titan_out, titan_venues`

## Generalizing the Pause

Quote comparison is one use of the pause. While the simulator is stopped, the chain is frozen.
`session.rpc()` reads the same account state the pending transactions will see, and
`session.rpc().simulate_transaction(&tx)` prices any transaction against it — a competing route, a
backrun, a liquidation. Change the discovery filter's program ID to apply this to another protocol.

## Usage

```bash
export SIMULATOR_API_KEY=<key>
cargo run --bin quote_compare -- --start-slot 417811170 --end-slot 417811175
```

Results go to `results.csv` (`--output`); `--program-id` picks the program to pause on (default
Jupiter V6). Note the Titan side is built by patching a template transaction's input amount, and the
signer's balance is topped up before each simulation and restored after, so comparisons can't bleed
into each other.
