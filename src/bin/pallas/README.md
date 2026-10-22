# Running Pallas Alongside a Session

This example runs OKX's router (Pallas) against a backtest session, so it routes on replayed
mainnet state, and simulates each swap it builds where the original swap landed. The bundled
[`cases.json`](./cases.json) holds two OKX-routed swaps from 2026-09-24 whose re-routed
replacements fail:

| Case | Failure |
|---|---|
| `min-return-not-reached` | OKX router `MinReturnNotReached` (6010) |
| `byreal-panic` | ByReal panic in `swap.rs` (`ProgramFailedToComplete`) |

## Methodology

For each case, the example:

1. pauses a session at the case's quote slot and points Pallas at the session's RPC;
2. sends Pallas the case's `/swap-instruction` request;
3. advances the session to just before the original swap;
4. simulates Pallas's instructions there, paid by the original's fee payer, with a 1.4M
   compute-unit limit.

Pallas runs against the paused session with:

| Variable | Value |
|---|---|
| `RPC_URL` | the session's RPC URL |
| `GEYSER_MODE` | `rpc-only` |
| `PALLAS_SLOT_STALE_SEC` | `86400` |
| `GRPC_CONSISTENCY_CHECK_INTERVAL_SECS` | `0` |
| `GEYSER_GRPC_RESET_ON_GAP` | `0` |
| `PALLAS_PROTOCOL_CU_HUB_ENABLED` | `false` |

## Output

Each case gets a directory holding the request, Pallas's response, the simulated transaction, the
simulation result with its logs, and Pallas's log. A final table sets each result beside the error
from our run.

## Usage

Copy [`.env.example`](./.env.example) to `.env` in this directory and fill it in, or point
`PALLAS_ENV_FILE` at another file. Then:

```sh
cargo run --release --bin pallas -- \
  --pallas-bin /path/to/pallas
```

That runs every case; to run one, name it:

```sh
cargo run --release --bin pallas -- \
  --pallas-bin /path/to/pallas \
  --case byreal-panic
```

Without `--pallas-bin`, start Pallas yourself for each case with the environment the example
prints. To test other swaps, add entries to `cases.json` in the same shape.

Additional flags:
- `--case <name or signature>` runs only that case
- `--pallas-url` / `--pallas-ops-url` set Pallas's `PORT` and `METRICS_PORT` addresses (defaults
  `127.0.0.1:18081` and `127.0.0.1:19101`)
- `--output` sets the output directory (default `pallas-out`)
