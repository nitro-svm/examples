# Examples

This repo contains starter code for various use cases of the simulation API.

Every example rests on the same primitive: a backtest session replays historical Solana slots
and lets you *freeze* the chain at any point, then simulate whatever transactions you like
against that frozen state. Nothing is committed and nothing costs money, so questions that are
prohibitively expensive or simply unanswerable on mainnet — what would this AMM have quoted at
10x the size? would my router have won that trade? does my new bytecode break real traffic? —
become things you can just measure.

The protocols used are just examples and aren't necessarily reflective of actual integrations.

## [Reconstructing Prop AMM Liquidity Curves](./src/bin/amm_liquidity)

Prop AMMs don't publish an order book or a quote endpoint, so their real spread and depth are
invisible from the outside. This example sweeps trade sizes through a single venue against
frozen state — doubling the size each step, in both directions — and reconstructs the venue's
spread and depth curve slot by slot, without moving the market or spending anything.

## [Comparing Aggregator Quotes](./src/bin/quote_compare)

Comparing a live quote against a fill that already happened isn't a fair fight; the pools moved
in between. This example pauses the chain immediately *before* each real Jupiter swap executes
and prices a competing route (Titan) against that identical state, recording what each router
would have paid out and which venues each one used.

## [Measuring Quote Decay](./src/bin/quote_decay)

A quote goes stale the moment it's issued, which is why slippage tolerances exist — usually set
by guesswork. This example captures real Jupiter swaps and replays each one unchanged against
the next 50 slots, so you can see empirically how fast output decays with landing latency and
size your tolerances from data.

## [Regression Tests](./src/bin/regression_test)

Devnet won't tell you what a program upgrade does to real users. This example replays historical
mainnet slots with your compiled `.so` swapped in for the deployed program, runs the real
transactions from those blocks against it, and reports every failure, log line, and SOL/token
balance change — a regression suite whose inputs are real mainnet traffic. It's also the
simplest end-to-end walkthrough of the session lifecycle, so start here.

## Notes

The API key can be supplied via the `SIMULATOR_API_KEY` environment variable rather than
`--api-key`.

List the slot ranges available to simulate over — a range that isn't cached takes ~90s to fetch
and prepare:

```bash
curl https://staging.simulator.termina.technology/available-ranges | jq
```

HTTP JSON-RPC calls are standard Solana RPC format, posted to the `rpcEndpoint` from the
`SessionCreated` response. That endpoint is unauthenticated.
