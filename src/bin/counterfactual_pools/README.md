# Counterfactual Pools

Add a pool that never existed on mainnet and measure the taker flow it would have won. This example launches a TaurusFi pool for SNDK (Sandisk, Backpack Securities) against USDC, and requotes every historical SNDK/USDC swap through Jupiter Metis with the new pool available.

## How it works
The pool lives in three accounts, committed as `solana account --output json` dumps in [`fixtures/`](./fixtures):

| fixture | account | |
|---|---|---|
| `pool.json` | `HxAgMQ…dU1a` | the pool, at TaurusFi's `["pair", SNDK, USDC]` address; its curve is copied from the live SPCX/USDC pool |
| `vault.json` | `9PMgNb…Y6GU` | the pool's SNDK vault, holding 1,000 SNDK |
| `oracle.json` | `ZPVuSa…WL6` | the price oracle the pool reads, at `["prices", 2]` |

All three are posted at the first slot, and the pool is offered to Metis as an extra market. TaurusFi pools don't store a price; they read it from the oracle. So the oracle is rewritten every slot with Binance's SNDKUSDT price, hardcoded in [`oracle.rs`](./oracle.rs) for the fixed range, and a fresh update time. USDC settles from the vault every TaurusFi pool already shares.

Nothing is committed, so other pools and taker flow stay as they were. The pool's inventory isn't updated by the swaps routed through it either.

## Usage
```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin counterfactual_pools
```

It replays slots 451,710,501–451,720,501 (2026-09-29 17:35–18:20 UTC) and prints:
- how many requoted SNDK/USDC legs Metis sent through the pool, per direction, and how they priced against the original fill. Each leg is written to `new-pool.jsonl`.
- the router's own quotes of the pool, probed every 250 slots at $100, $1,000 and $10,000, which show it's quotable whether or not flow arrives.

## Notes
- TaurusFi only quotes within an inventory band: with 100,000 SNDK in the vault the pool refused to buy SNDK, and with 200 or fewer it refused to sell. 1,000 quotes both ways.
- SNDK flow is thin: about one SNDK/USDC swap every 2,500 slots in this range.
- The server can drop replacement notifications when the subscriber lags, and the stream also carries an `original` replay for every swap, which the pair filter doesn't exclude. Compare the `requote` count printed against `rerouted`.
