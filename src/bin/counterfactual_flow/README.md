# Counterfactual Flow

This example updates a venue's quoting strategy by applying a parameter change, then measures its effect on taker flow. Every historical swap is re-quoted through Jupiter Metis, so new routes reveal whether the venue's parameter change would've captured fills that were routed elsewhere historically.

## Methodology

The code registers a filter on a specified venue via `--program-id` so that the session can pause on a matching event (e.g. a BPF Loader `Upgrade`) and allow the client to replace it with a custom parameter change. The example leaves the parameter itself as a `TODO`, but this can be an oracle update, spread widening, or fee change.

The session requotes all historical order flow through Metis and simulates the resulting swap in place of the original. It doesn't commit the new swap to avoid noisy feedback loops and holds other maker quotes and taker flow constant--since price discovery for "blue-chip assets" is primarily on perp CEXs.

Results are streamed over `rerouteSubscribe` and carry the new route, the new output, and the original output. The example code filters for swaps that are routed through the venue specified in `--program-id`.

## Usage

```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin counterfactual_flow -- \
  --start-slot 433838452 \
  --end-slot 433838453 \
  --program-id <venue program id>
```

Per-leg comparisons are printed to stderr, tagged `before` or `after` the parameter change. The post-change transaction count is printed to stdout so it can be captured.
