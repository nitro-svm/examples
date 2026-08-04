# Counterfactual Flow

Ask what would have happened to your fills if your quotes had been different.

You give us a change to your venue — a wider spread, a faster oracle, a deeper curve — and we
replay a real slice of mainnet with that change in place. Every swap that had involved the venue
gets re-quoted against your modified venue, competing against every other venue exactly as it was
originally. The output is how much of that flow you would have won instead of lost, or lost 
instead of won.

Nothing is sent to chain. The replayed history is untouched, so other makers quote and takers
trade exactly as they did; the only thing that differs is you.

## The question this is built for

Makers can measure what they filled. What they can't measure is what they *would* have filled at a
different price — the orders that went to someone else are invisible, and you cannot A/B test a
market by quoting worse for a week.

The most valuable version is a pricing question: **how much wider can I quote before I start 
losing flow?** If your fills hold as you widen, you've been leaving money on the table — you were 
the best price by more than you needed to be. If they fall off a cliff, you're priced right at 
the edge and your margin is defended by a hair.

The same machinery answers the mirror question — what does being slower cost me — which is the
experiment worked through below, because staleness is easy to construct from a venue's own history
without knowing anything about how it prices.

## How it works

Your change is expressed as an **account override**: the bytes of your oracle, curve, or fee
account, rewritten per slot. A requote is generated against that changed state, and the transaction
is simulated against it as well, everything stays untouched for the rest of the chain.

For this example rather than inventing account contents, the workflow reuses the venue's own history,
so every injected state is existing L1 state:
3. `compare` runs the null control and one arm in a single invocation and diffs them leg by leg.

1. `capture` records what the account actually held at each slot it changed.
2. `run`/`compare` post those states on a shifted schedule. `--lag K` posts slot `s-K`'s state at
   slot `s` — your venue updating K slots slower than it really did.

Any bytes you can serialize work the same way, including a state that never existed on chain — the
curve you're about to deploy, or your current curve with the spread widened. The schedule contract
is raw account bytes.

> A comparable modification would be using a lookahead (`--lead K`) variant to see how faster
> price updating would have affected the given venue.
> **Some venues will not read a price update that is higher than the current slot, so more modification
> would be needed to run it for those venues (BisonFi does not work with `--lead` in this example)**

Four bad commands are rejected before a session is opened, so they fail in under a second
instead of after a replay:

```
--capture is required with --lag/--lead/--price-shift-bps
--setup-transactions carries a shift; pass --lag or --lead
--price-field moves a price; pass --price-shift-bps
--price-shift-bps cannot be carried by --setup-transactions: the setup replays the venue's own captured update, which re-prices itself. Drop one of the two.
```

The third is only checked when no `--lag`/`--lead` is given. `--lag 2 --price-field 839` builds a
pure time shift and never touches the offset; the missing `[price]` line is the only sign.

## What is counted

The client only ever sees swaps that were **re-quoted**. Every number below is over that
population — 19,411 legs on the range used here, not the 21,326 the pair traded on L1.
`--filter-pair` narrows it further, the server re-quotes Jupiter's flow alone unless
`--reroute-venues` names more, and arbitrage cycles stay out unless `--circular-arbs` admits them.

`run` prints the size of that population on every run. It is the denominator every other count is
a share of:

```
[run] 19411 re-quoted legs seen
```

`won` and `lost` are not absolute flow. On the null control — byte-identical state posted at each
state's own slot — the venue keeps 194 of the 501 legs it held on L1: it "loses" 61% of them with
nothing changed. An override reaches the router immediately while an ordinary account view trails
the replay by several blocks, so any overridden pool is fresher than its competitors and the
router re-sorts around it. Read every arm as a difference against `--lag 0`, never as a level.
The tool repeats this on every venue report.

## Try it

These are the exact commands behind the result below.

```sh
export SIMULATOR_API_KEY=<key>
POOL=8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo             # BisonFi, SOL/USDC
PROGRAM=BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi
PAIR=So11111111111111111111111111111111111111112,EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v

# 1. Record what the account really held across the range.
cargo run --bin counterfactual_flow -- capture \
  --start-slot 433838452 --slot-count 9999 \
  --account $POOL --out capture.jsonl

# 2. The venue as it ran, and the venue updating 10 slots (~4s) slower.
cargo run --bin counterfactual_flow -- run \
  --start-slot 433838452 --slot-count 9999 \
  --account $POOL --filter-pair $PAIR --program-id $PROGRAM \
  --out baseline.jsonl

cargo run --bin counterfactual_flow -- run \
  --start-slot 433838452 --slot-count 9999 \
  --account $POOL --filter-pair $PAIR --program-id $PROGRAM \
  --capture capture.jsonl --lag 10 \
  --out lagged.jsonl
```

The range is slots **433838452–433848451** (10,000 slots, ~60-70 minutes of mainnet). An account-state
bundle is published for exactly this range, so the sessions replay recorded state instead of
executing — a run takes minutes, not hours. `--slot-count` must stay `9999`: replay resolves the
bundle by exact range match.

Point `--account`/`--program-id` at your own venue and `--filter-pair` at the book you quote to run
it for yourself. `--filter-pair` takes the two mints in either order and only limits how much gets
re-quoted; it never decides who wins. Candidate pool addresses to capture and override are the
per-hop `ammKey`s in any baseline run's `routePlan`.

## What we measured

The 10,000-slot range above, against **BisonFi** (`8FnX3xo2yYw3EUE6w3nQA4GfXGS9wpK6oj3veJpbFzLo`),
a SOL/USDC prop AMM. The pair carries **14,513** re-quotable swap legs in that window — every
SOL↔USDC swap a supported router placed, not just the ones this venue won.

**Fresh, the venue won 1,516 of them. Ten slots slower, it won 2.**

| | legs won |
|---|---|
| the venue as it really ran | 1,516 |
| the same venue, 4s slower | 2 |

Ten slots of staleness costs this venue **99.9%** of the flow it captures. That is one run per
side; the router is not deterministic, so see [below](#reading-the-output-and-what-not-to-claim)
for how many runs a smaller difference would need.

### Against what actually happened on chain

The numbers above are counterfactual. Here they are next to L1, which you can check against your own
books:

| | legs |
|---|---|
| SOL/USDC legs re-quoted | 14,513 |
| **…that routed through the venue on chain** | **2,595** |
| re-quoted fresh → router picks the venue | 1,516 |
| re-quoted 10 slots stale → router picks the venue | 2 |

Two things to read here, and the first one bounds what any of this can say.

**This only sees aggregator flow.** Swaps are detected by decoding router instructions — Jupiter,
OKX, Titan and DFlow. A taker hitting your pool directly, through an unsupported aggregator, or via
private flow never enters the sample at all, so the totals here are a slice of your book, not your
book.

**The baseline is close to history, but it is not a replay of it.** The venue held 2,595 legs on
chain and the fresh re-quote gives it 1,516 — 58%. It is not the same 58%, either: it keeps 660 of
the legs it really won and takes 856 it did not, because the re-quote runs against a different
router than the one that originally placed each swap, with its own market coverage and quote timing.
Read the baseline as *"what this router would do"*, not *"what happened"*.

**Which is why the comparison is baseline-vs-modified, never modified-vs-L1.** Both arms run through
the same router with the same disagreements, so those cancel; what remains is your change. 1,516 → 2
is not a haircut, it is elimination.

## What it means for a maker

That is why it takes a counterfactual to see. The cost of a quoting decision is invisible in your own
fill history, because the orders you didn't win left no trace there. This reconstructs them.

**The same tool can answer a pricing question.** The experiment above asks what being slower would cost
you. You can instead ask what quoting *wider* would cost you: put a worse price in the override and see how
much flow you keep.

That is a trade — a wider spread wins fewer fills but earns more on each one — and either answer is
worth having:

- **Flow drops immediately.** You are priced at the edge. You win by a hair, and there is no room to
  charge more without losing the business.
- **Flow barely moves.** You were winning by far more than you needed to. Your competitors were not
  close to your price, so the extra tightness bought you nothing — that is margin you were giving
  away.

## Reading the output, and what not to claim

To avoid uncertain results due to nondeterminism, you should run multiple runs per group to distinguish
signal from noise. This gives you more meaningful results without lying to you.

To find out what N is enough, we ran ten baselines and ten lagged runs over this venue and split each
group of ten against *itself*, asking how often two arms of N separate cleanly — meaning one has more
flow wins than the other. Any separation there is a false positive, because nothing was changed
between them. Both groups give the same answer:

| runs per side | false positives, baseline group | false positives, lagged group |
|---|---|---|
| 1 | **100%** | 87% |
| 2 | 33% | 22% |
| 3 | 10% | 4.5% |
| 4 | 2.9% | 1.0% |
| **5** | **0.8%** | **0.0%** |
| 6 | 0.1% * | 0.1% * |
| 7 | <0.05% * | <0.05% * |
| 8–10 | <0.05% * | <0.05% * |

Rows 1–5 are exact: every way of splitting ten runs into two disjoint arms of N. Rows marked `*`
are estimated by resampling, since two disjoint arms of six would need twelve runs.

So a single run is not a precise measurement of *how much* flow moved, even though the example above
separates so far — 1,516 against 2 — that one run per side already carries the direction. If you ever
see a minor difference between the baseline and the override, run both sides several times to take
run-to-run variance out of the answer.

**Is more than five worth it?** Barely. Going 5 → 6 takes you from ~0.8% to ~0.1%, and past six the
curve is flat at effectively zero — you are paying runs for nothing.

> **PS You can run all of them in parallel so that it doesn't take any more time than a single run**

What is trustworthy is a **direction that holds across every run in each group**. If the groups
overlap at all, you don't have a result yet.

**Read routing, not price.** On a contested pair the quote barely moves when you lose a leg — a
competitor fills it at nearly the same price — so a per-leg price delta stays buried in noise while
the flow you win collapses. Flow won is the signal.

**These are routing decisions, not settled PnL.** Re-quoted swaps are simulated and never
committed. The result says where the flow would have gone, not what it would have earned.
