# Regression Tests

Devnet has no liquidity, MEV bots, or account state that programs actually meet, so tests on the
network have little signal about what an upgrade does to real users.

This binary runs an honest test against historical mainnet slots with a custom program
swapped in for the deployed program. Diff the two runs to compare metrics like transaction success rates and token balance deltas.

## Output

The output prints success or failure, the error, full logs (to `--log-file`), and token balance deltas for each transaction. 
It also includes totals, successes, failures, and net token P&L per account, sorted by magnitude.

## Usage

Check which slot ranges are available first.
```sh
curl https://staging.simulator.termina.technology/available-ranges | jq
```
or
```sh
sim ranges
```

Then run the same slots with the baseline and new build.
```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin regression_test -- --start-slot 123 --end-slot 456 --log-file baseline.txt

cargo run --bin regression_test -- --start-slot 123 --end-slot 456 \
  --program-id addr1234 --program-so path/to/program.so --log-file experiment.txt
```
