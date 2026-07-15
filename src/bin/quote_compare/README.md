# Comparing Aggregator Quotes

Comparing a router's quote today against a fill from last week isn't a fair benchmark. An apples-to-apples comparison prices both routers against the same state to measure the output, slippage, and venue splits.


## Methodology
The example replays historical slots with a discovery filter for the specified router's program (by default, this is Jupiter). When a transaction batch containing one of the router's swap occurs, the simulator stops before any of its transactions execute. It then prices a different router's transaction (by default, this is Titan) for the same pair and input amount and records what each router paid out. The simulator then jumps to the next matching batch and repeats.

## Output

The output is structured as CSV file. Each row records both routers' output and their venue splits, so a gap in output can be traced back to the venues where the routes diverged.

## Usage
```sh
export SIMULATOR_API_KEY=<key>
cargo run --bin quote_compare -- --start-slot 417811170 --end-slot 417811175
```

Additional Flags:
- `--output` directs results to the specified file
- `--program-id` picks the program to pause on (default is Jupiter)

Note the Titan side is built by patching a template transaction's input amount, and the signer's balance is topped up before each simulation and restored after, so comparisons don't affect each other.
