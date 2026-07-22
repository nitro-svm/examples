# Counterfactual Flow

Patching and re-simulating a single swap only shows how a change affects transactions already under a venue's control—it says nothing about flow the venue doesn't currently win. This example applies a quoting-parameter change to a venue mid-replay, then measures its effect on taker flow: every historical swap is re-quoted through Metis (simulated only, never committed), so legs touching the venue reveal whether the change would capture fills that actually went elsewhere.

## Methodology

The session registers a `ProgramExecuted` discovery filter on `--program-id`—the venue under test—and pauses immediately before each batch that invokes it, via `advance_to_discovery`. Once the batch containing the intended trigger transaction (a BPF Loader `Upgrade`, by default; see `discovery.rs`) is found, the example sends a custom transaction against the frozen chain state through `session.rpc()`. This is where the actual quoting-parameter change belongs—the shipped example only wires up the trigger and leaves the transaction itself as a `TODO`.

Concurrently, `reroute_order_flow` is enabled for the whole session, so every historical taker swap is re-quoted through Metis and the routed transaction is simulated in place of the original—never committed, so the replay itself is unaffected. Results stream in over `rerouteSubscribe`: each notification carries, per leg, the original quoted output, Metis's quoted output for the same intent, and the route Metis chose. Legs are kept only when Metis's route touches `--program-id`, and are tallied separately depending on whether the parameter change has been applied yet, so the two tallies printed at the end are a direct before/after comparison for that venue.

## Usage

```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin counterfactual_flow -- \
  --start-slot 433838452 \
  --end-slot 433838453 \
  --program-id <venue program id>
```

Per-leg comparisons are printed to stderr, tagged `before`/`after` the parameter change; the post-change transaction count is printed to stdout so it can be captured/piped.
