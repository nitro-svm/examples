# Examples

This repo contains starter code for various use cases of the simulation API.

Every example rests on the same primitive: a backtest session replays historical Solana slots and
can freeze the chain at any point—-including partway through a block--then simulate against
that state with arbitrary balances and bytecode. They answer question such as:
- What would this prop AMM have quoted at 10x the size?
- Would my router have won that trade?
- Does my new bytecode break real traffic?

These examples address real use cases and feature requests, but the protocols used are just placeholders and don't represent actual customers.

## [Rerouting Order Flow](./src/bin/reroute_liquidity)

Patching and re-simulating a single swap only shows how a change affects transactions already under a router's control—it says nothing about flow it doesn't currently win. This example extracts the intent behind every historical taker swap, re-quotes it through Metis, and simulates the result to see whether an updated quoting strategy would capture fills that actually went elsewhere.

## [Measuring Spread and Depth](./src/bin/amm_liquidity)

Prop AMMs price dynamically via a liquidity curve rather than resting orders on a book, so spread and depth can only be measured by quoting the venue at a range of sizes. 
This example sweeps sizes for the specified pair slot-by-slot and reports the derived spread and depth curve for the venue of interest.

## [Comparing Quotes](./src/bin/quote_compare)

Benchmarking a live quote against a historical fill isn't an apples-to-apples comparison since pools and prices have moved in between. This example pauses the chain immediately before each real Jupiter swap 
and prices a competing route against the same state, recording what each one would have paid out and the venues that were used.
This is useful for applications evaluating which router to integrate and also allows routing teams to benchmark directly against others or their own past quotes.

## [Measuring Quote Decay](./src/bin/quote_decay)

There's always latency between quote and execution, but for a retail user swapping from a UI it can run upwards of 50 slots. Always, but especially in these cases, the swap needs to avoid transient liquidity and spoofing games that may disappear when the transaction actually lands onchain. 
This example captures swaps for the specified router and replays each one against the next 50 slots to calculate empirically how fast output decays with landing latency.
This helps applications gauge the router with the most stable routes, and for routers to understand which venues have the most stable quotes.

## [Capture Regressions](./src/bin/regression_test)

Devnet doesn't reveal what a program upgrade does to real users. This example replays historical
mainnet slots with custom program logic, runs the real transactions from those blocks against it, and reports every failure, log line, and token balance change--a regression suite whose inputs are real mainnet traffic.

## Notes
Please see the [documentation](https://docs.termina.technology/documentation) for a comprehensive overview of the API interface and client libraries.

To see all supported slot ranges:
```sh
curl https://simulator.termina.technology/available-ranges | jq
```
or
```sh
curl -fsSL https://cli.simulator.termina.technology/install.sh | bash
sim ranges
```
