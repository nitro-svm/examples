# Regression Tests

Devnet isn't an accurate picture of reality since it lacks the liquidity, MEV bots, or account state that are present on mainnet, so tests on the network don't provide much signal about what logic changes do to real users.

This example can run a baseline as well as an experimental test with custom programs and accounts against the same historical time range. It can copmare the two runs to determine how the changed states affected metrics like transaction success rate and token balances.

## Output

The output prints success or failure, the error, full logs (to `--log-file`), and token balance deltas for each transaction.
It also includes total transactions, successes, failures, and net token P&L per account, sorted by magnitude.

## Usage
```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin regression_test -- --start-slot 123 --end-slot 456 --log-file baseline.txt

cargo run --bin regression_test -- --start-slot 123 --end-slot 456 \
  --program-id addr1234 --program-so path/to/program.so --log-file experiment.txt
```
