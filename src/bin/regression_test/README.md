# Regression Tests

Devnet doesn't have real liquidity, real MEV bots, or the weird account state your program
will actually meet in production, so "it passed on devnet" tells you very little about what a
program upgrade does to real users. The honest test is to run the new bytecode against traffic
that really happened — but on mainnet that experiment costs money and can't be undone.

This example runs it safely. It replays a range of historical mainnet slots, optionally
*swapping in your compiled `.so` in place of the deployed program* before the first slot, and
then lets every real transaction in those blocks execute against your version. Every failure,
every log line, and every lamport and token balance change is captured. Diff a run with your
new bytecode against a baseline run without it and you have a regression test whose inputs are
real mainnet traffic rather than fixtures you invented.

It's also the smallest end-to-end tour of the session lifecycle in this repo — connect, create,
subscribe to logs, advance, close — so it's the natural place to start if you're building
something new against the simulator.

## What it captures

Per transaction: success or failure, the error, the full log stream (written to `--log-file`),
and both SOL and SPL-token balance deltas. At the end it prints a summary — totals, successes,
failures, and net SOL and token P&L per account, sorted by magnitude.

Point `--program-id` at a program to filter the log subscription down to just that program's
output.

## Run it

List the available slot ranges first — a range that isn't cached takes ~90s to fetch and prepare.

```bash
curl https://staging.simulator.termina.technology/available-ranges | jq
```

Baseline run (real deployed bytecode, no substitution):

```bash
export SIMULATOR_API_KEY=your-key
cargo run --bin regression_test -- \
  --url staging.simulator.termina.technology \
  --start-slot 123 \
  --end-slot 456
```

Same slots, but with your build swapped in and its logs streamed to a file:

```bash
export SIMULATOR_API_KEY=your-key
cargo run --bin regression_test -- \
  --url staging.simulator.termina.technology \
  --start-slot 123 \
  --end-slot 456 \
  --program-id addr1234 \
  --program-so path/to/program/bytecode \
  --log-file test.txt
```

Dump the currently deployed bytecode to compare against with
`solana program dump <addr> program.so --url mainnet-beta`.

## What happens under the hood

1. Connect to the backtest WebSocket endpoint.
2. Create a session over the slot range (uncached ranges take ~90s to prepare).
3. Query the simulated chain state over HTTP JSON-RPC (`getSlot`, `getLatestBlockhash`,
   `getAccountInfo`).
4. Advance through each block one at a time, collecting logs and balance changes.
5. Close the session.

## Notes

The API key can also be supplied via the `SIMULATOR_API_KEY` environment variable rather than
`--api-key`.

JSON-RPC calls are standard Solana RPC format, posted to the `rpcEndpoint` returned in the
`SessionCreated` response. That endpoint is unauthenticated.
