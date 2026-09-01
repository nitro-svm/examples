# Examples

This repo contains starter code for various use cases of the simulation API.

Every example rests on the same primitive: a backtest session replays historical Solana slots and
can freeze the chain at any point—-including partway through a block--then simulate against
that state with arbitrary balances and bytecode. They answer questions such as:
- What would this prop AMM have quoted at 10x the size?
- Would my router have won that trade?
- Does my new bytecode break real traffic?

These examples address real use cases and feature requests, but the protocols used are just placeholders and don't represent actual customers.

## [Counterfactual Flow](./src/bin/counterfactual_flow)

When developing a quoting strategy, it's difficult to determine a taker's theoretical size and response to a parameter change. This example rewrites the venue's own oracle, curve, or fee account — re-priced at each state's own slot, or the same state posted early or late — re-quotes the historical order flow through Jupiter Metis to determine the impact of the change. Jupiter's flow by default; the other aggregators on request (DFlow, Titan, OKX). More details in the example's README.

## [Counterfactual Capital](./src/bin/counterfactual_capital)

A venue that already knows its historical quotes were uncompetitive learns nothing from being shown them again; the useful question is what would have to change. This example scales a venue's inventory and its quoting curve as account overrides, rebuilds every historical swap on the pair against that state, and reports the flow each version would have filled and at what margin. Capital and curve are separate knobs because they fail in different places: the vaults decide whether a fill can be paid, the curve decides whether it is offered at all, and on a venue that quotes from a curve only one of them moves the answer.

## [Measuring Spread and Depth](./src/bin/amm_liquidity)

Prop AMMs price dynamically via a liquidity curve rather than resting orders on a book, so spread and depth can only be measured by quoting the venue at a range of sizes. 
This example sweeps sizes for the specified pair slot-by-slot and reports the derived spread and depth curve for the venue of interest.

## [Comparing Quotes](./src/bin/quote_compare)

Benchmarking a live quote against a historical fill isn't an apples-to-apples comparison since pools and prices have moved in between. This example pauses the chain immediately before each real Jupiter swap and prices a competing route against the same state, recording what each one would have paid out and the venues that were used.
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

The hosted examples default `--url` to `simulator.termina.technology`. To see that
deployment's supported slot ranges:
```sh
curl https://simulator.termina.technology/available-ranges | jq
```
or
```sh
curl -fsSL https://cli.simulator.termina.technology/install.sh | bash
sim ranges
```
