# Counterfactual Pools

Add a pool that never existed on mainnet and measure the taker flow it would've won.
This example launches a TaurusFi pool for SNDK (Sandisk, Backpack Securities) against USDC, and requotes every historical SNDK-USDC swap through Jupiter Metis with the new pool available.

## Usage
```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin counterfactual_pools
```

It replays a fixed range of slots `451,710,501`–`451,720,501` (2026-09-29 17:35–18:20 UTC) and prints how many requoted SNDK-USDC legs Metis sent through the TaurusFi pool, per direction, and how they priced against the original fill. Each leg is written to `new-pool.jsonl`.

## Methodology
The new SNDK-USDC market consists of three accounts, committed as `solana account --output json` dumps in [`fixtures/`](./fixtures). USDC settles from the vault every TaurusFi pool already shares, so it doesn't need a fixture.

| fixture | account | description |
|---|---|---|
| `pool.json` | `HxAgMQ…dU1a` | the pool, at TaurusFi's `["pair", SNDK, USDC]` address; its curve is copied from the live SPCX-USDC pool |
| `vault.json` | `9PMgNb…Y6GU` | the pool's SNDK vault, holding 1,000 SNDK |
| `oracle.json` | `ZPVuSa…WL6` | the price oracle the pool reads, at `["prices", 2]` |

- All three are injected at the first slot, and the pool is registered with Metis as a new market.
- TaurusFi pools don't store a price and instead read it from the oracle, so the oracle is rewritten every slot with Binance's SNDK-USDT perp price, hardcoded in [`oracle.rs`](./oracle.rs) for the fixed range.
- TaurusFi only quotes within an inventory band: with 100,000 SNDK in the vault the pool refused to buy SNDK, and with 200 or fewer it refused to sell. That's why `vault.json` holds 1,000 SNDK, which quotes both ways.
- SNDK flow is thin: there's around one SNDK-USDC swap every 2,500 slots in this range.

## Simulation Notes
- No reroute is committed the simulation state to prevent cascading feedback loops e.g. the pool's inventory isn't updated by the swaps routed through it.
- [`diagnostic.rs`](./diagnostic.rs) can ask Metis to quote the pool directly every 250 slots at $100, $1,000 and $10,000, which shows whether it's quotable in both directions regardless of flow. (It isn't wired in; its docstring shows how.)
- The server drops replacement notifications when the client has low bandwidth, and the stream carries an `original` replay for every swap, which the pair filter doesn't exclude. Compare the "requotes received" count printed against `rerouted`.
