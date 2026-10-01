# Counterfactual Pools

Add a pool that never existed on mainnet and measure the taker flow it would have won. Every historical swap is requoted through Jupiter Metis with the new pool available, and the report counts the legs the router sent through it and how much better they priced than the original fill.

The pool's accounts are posted at the first slot and offered to Metis as an extra market. Nothing is committed, so other pools and taker flow stay as they were.

The pool's program must already be deployed at the start slot, and Metis must support it: programs can't be overridden, and the router can only quote pool types it knows.

## Usage
Dump every account the pool needs (the pool itself, its vaults, any oracle or config) with `solana account <address> --output json`, for example from a pool created on devnet or a local validator with the same program.

```sh
export SIMULATOR_API_KEY=<key>
POOL=<new pool address>
PAIR=So11111111111111111111111111111111111111112,EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
RANGE="--start-slot 439649408 --slot-count 9999"

cargo run --bin counterfactual_new_pool -- $RANGE --pool $POOL --filter-pair $PAIR \
  --account pool.json --account vault_a.json --account vault_b.json
```

It prints how many requoted legs went through the pool, and per direction the split routes and mean bps against the original fill. Each of those legs is written to `new-pool.jsonl`.

The pool's state is never updated by the swaps routed through it, so its price stays where it starts. Read a large leg count over a long range with that in mind.
