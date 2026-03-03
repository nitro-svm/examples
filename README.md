# backtest-example

A minimal Rust example of interacting with the Nitro backtest API using only
standard external crates.

## Running

```bash
cargo run -- \
  --url wss://<host>/backtest \
  --api-key local-dev-key \
  --start-slot 300000000 \
  --end-slot 300000010
```

1. Connect to the backtest WebSocket endpoint
2. Create a session over a range of historical Solana slots
3. Query the simulated chain state via HTTP JSON-RPC (`getSlot`, `getLatestBlockhash`, `getAccountInfo`)
4. Advance through each block one at a time
5. Close the session

## Notes
The API key can also be supplied via the `SIMULATOR_API_KEY` environment variable:

```bash
export SIMULATOR_API_KEY=your-key
cargo run -- --start-slot 300000000 --end-slot 300000010
```

All arguments have defaults so a bare `cargo run` will attempt to connect to
`ws://localhost:8900/backtest` with the key `local-dev-key`.

HTTP JSON-RPC calls are standard Solana RPC format, posted to the
`rpcEndpoint` from the `SessionCreated` response with an `X-API-Key` header.
