# Scheduled Fills

A quote is only as good as the fill it turns into. This example takes swap transactions that providers built at quote time and executes each one at its quote slot and the slots after it, recording what the swap would actually have paid out by provider, pair, and size.

## Methodology

Each sample is a provider quote: the slot it was requested at (`rpc_slot`, shared by every provider for a `sample_id`; only some providers report their own context slot), the quoted and minimum output, and the transaction the provider returned.

- One `ScheduledAction` per sample runs its transaction after `rpc_slot ..= rpc_slot + --window` (10 by default), and every run comes back as an `ActionResult` notification.
- Each action is labeled with its sample's row index, which maps each notification back to its provider, pair, and amount. `sample_id` is shared by every provider's quote for the same pair, size, and slot, so it is kept as a CSV column for comparing providers rather than used as the key.
- The signer is funded with the input amount and its output account is zeroed before each run, so the returned output balance *is* the fill. wSOL inputs are funded both as native SOL and as a wSOL ATA, since providers differ on whether the transaction wraps its own SOL.
- The provider's `min_out` is left in place: a run that would breach it fails and is recorded with its error, as it would have failed onchain.

## Usage

Export the successful samples from the parquet to JSONL:

```python
import polars as pl

(
    pl.scan_parquet("part-20261001T195941UTC.parquet")
    .filter(pl.col("error").is_null() & pl.col("rpc_error").is_null() & (pl.col("http_status") == 200))
    .filter(pl.col("transaction").is_not_null() & pl.col("rpc_slot").is_not_null())
    .select("sample_id", "provider", "input_mint", "output_mint", "in_amount", "out_amount",
            "min_out_amount", "rpc_slot", "transaction")
    .collect()
    .write_ndjson("samples.jsonl")
)
```

Then run it over a slot range the deployment serves (`sim ranges`):

```bash
export SIMULATOR_API_KEY=<key>
export SOLANA_RPC_URL=<helius url>  # optional; mint lookups fall back to the public mainnet RPC
cargo run --bin scheduled_fills -- \
  --input samples.jsonl --start-slot <start> --end-slot <end> --window 10
```

Samples quoted before `--start-slot`, or whose window runs past `--end-slot`, are skipped. The output CSV (`--output`, default `fills.csv`) has one row per sample + execution slot, with `quoted_out`, `min_out`, `filled_out`, `slot_offset`, and `error` side by side.
