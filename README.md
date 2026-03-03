# backtest-example-rust

A minimal Rust example of interacting with the backtest API using only
standard external crates.

## Running
```bash
# Show available ranges
curl https://<host>/available-ranges | jq
```

```bash
# Run the sim
export SIMULATOR_API_KEY=your-key
cargo run -- \
  --url wss://<host>/backtest \
  --api-key local-dev-key \
  --start-slot 399834992 \
  --end-slot 399834999
```

1. Connect to the backtest WebSocket endpoint
2. Create a session over a range of historical Solana slots (if the requested range isn't in the cache, it'll take ~90s to fetch and prepare)
3. Query the simulated chain state via HTTP JSON-RPC (`getSlot`, `getLatestBlockhash`, `getAccountInfo`)
4. Advance through each block one at a time
5. Close the session

## Notes
The API key can also be supplied via the `SIMULATOR_API_KEY` environment variable:

All arguments have defaults so a bare `cargo run` will attempt to connect to
`ws://localhost:8900/backtest` with the key `local-dev-key`.

HTTP JSON-RPC calls are standard Solana RPC format, posted to the
`rpcEndpoint` from the `SessionCreated` response. This endpoint is unauthenticated.
