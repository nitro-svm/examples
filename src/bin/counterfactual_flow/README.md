# Rerouting Order Flow

Patching and re-simulating a single swap only shows how a change affects transactions already under a router's control—it says nothing about flow it doesn't currently win. This example reroutes all historical taker flow through Metis, revealing whether an updated quoting strategy would capture fills that actually went elsewhere.

## Methodology

With `--reroute-metis`, the session extracts the underlying intent from every historical taker swap (e.g. "10 SOL -> USDC" or "$1K USDT -> USD1"), submits it to Jupiter Metis for a fresh quote, and simulates the resulting route in place of the original one. Metis may route the same intent through different venues than the original transaction did, which lets a venue estimate how many fills it would win from Metis under a new quoting strategy. The simulation is never committed, so the replay itself is unaffected, but simulated swaps compose within a block.

Results are read from `rerouteSubscribe`, a per-swap stream delivered alongside the replay. Each notification carries, per leg: the original quoted output, Metis's quoted output for the same intent, and the route Metis chose. Where available, it also carries the realized output of both the original and rerouted transaction after execution. The session summary additionally reports a server-side funnel—how many detected swaps were re-quoted, simulated, and executed without reverting.

`--program-id` names the venue under evaluation—an AMM or pool program, not Jupiter's own aggregator address. When set, every count is tailored to legs whose Metis-chosen route touched that venue, answering "how many fills would this venue win under Metis." Omit it to see every rerouted leg regardless of venue.

## Usage

```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin counterfactual_flow -- \
  --start-slot 433838452 \
  --end-slot 433838453 \
  --reroute-metis \
  --program-id <venue>
```

Per-swap comparisons are printed to stderr; the (venue-filtered, if `--program-id` is set) count of rerouted transactions is printed to stdout so it can be captured/piped. Running without `--reroute-metis` replays the range with rerouting disabled, so nothing is reported.
