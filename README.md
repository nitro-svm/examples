# backtest-example-rust

A minimal Rust example of interacting with the backtest API using only
standard external crates.

## Running
List the available slot ranges.
```bash
curl https://staging.simulator.termina.technology/available-ranges | jq
```

Run a baseline simulation.
```bash
export SIMULATOR_API_KEY=your-key
cargo run -- \
  --url staging.example.com \
  --start-slot 123 \
  --end-slot 456
```

Run a simulation while replacing and streaming logs for a program.
```bash
export SIMULATOR_API_KEY=your-key
cargo run -- \
  --url staging.simulator.termina.technology \
  --start-slot 123 \
  --end-slot 456 \
  --program-id addr1234 \
  --program-so path/to/program/bytecode
  --log-file test.txt
```

1. Connect to the backtest WebSocket endpoint
2. Create a session over a range of historical Solana slots (if the requested range isn't in the cache, it'll take ~90s to fetch and prepare)
3. Query the simulated chain state via HTTP JSON-RPC (`getSlot`, `getLatestBlockhash`, `getAccountInfo`)
4. Advance through each block one at a time
5. Close the session

## Notes
The API key can also be supplied via the `SIMULATOR_API_KEY` environment variable:

HTTP JSON-RPC calls are standard Solana RPC format, posted to the `rpcEndpoint` from the `SessionCreated` response. This endpoint is unauthenticated.
