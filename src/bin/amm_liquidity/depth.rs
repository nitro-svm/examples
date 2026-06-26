use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};

use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use simulator_api::{AccountModifications, ActionAnchor, ActionKind, ScheduledAction};
use simulator_client::ActionResultNotification;

use backtest_example::utils::accounts::{make_native_account, make_token_account};
use backtest_example::utils::parse::patch_titan_template_transaction;

use crate::Template;
use crate::action::{DEPTH_B2Q_PREFIX, DEPTH_Q2B_PREFIX, DepthDirection, token_amount};

const ITERATIONS: usize = 20;

#[derive(Clone, Copy)]
pub(crate) struct Depth {
    size: u64,
    out_amount: u64,
    price_impact_bps: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DepthKey {
    slot: u64,
    direction: DepthDirection,
}

pub(crate) struct DepthRecords {
    max_impact_bps: u64,
    max_depth: BTreeMap<DepthKey, Depth>,
    all_depth: BTreeMap<DepthKey, Vec<Depth>>,
}

impl DepthRecords {
    fn new(max_impact_bps: u64) -> Self {
        Self {
            max_impact_bps,
            max_depth: BTreeMap::new(),
            all_depth: BTreeMap::new(),
        }
    }

    fn add(&mut self, slot: u64, direction: DepthDirection, depth: Depth) {
        let key = DepthKey { slot, direction };
        let depths = self.all_depth.entry(key).or_default();
        depths.push(depth);

        if depths.len() == ITERATIONS {
            self.flush(key);
        }
    }

    fn flush(&mut self, key: DepthKey) {
        let Some(mut depths) = self.all_depth.remove(&key) else {
            return;
        };
        depths.sort_by_key(|d| d.size);

        let mut spot_rate: Option<f64> = None;
        let mut best: Option<Depth> = None;
        for d in depths {
            let rate = d.out_amount as f64 / d.size as f64;
            let spot = *spot_rate.get_or_insert(rate);
            let expected = spot * d.size as f64;
            let price_impact_bps = (expected - d.out_amount as f64) / expected * 10_000.0;

            if price_impact_bps > self.max_impact_bps as f64 {
                break;
            }

            best = Some(Depth {
                price_impact_bps,
                ..d
            });
        }

        if let Some(best) = best {
            self.max_depth.insert(key, best);
        }
    }
}

impl Depth {
    pub(crate) fn new(n: &ActionResultNotification, size: u64) -> Option<Self> {
        let out_amount = n.accounts.first()?.as_ref().and_then(token_amount)?;

        Some(Self {
            size,
            out_amount,
            price_impact_bps: 0.0,
        })
    }
}

/// Geometric sweep of sizes through a single-venue template.
/// Doubles size each step, for up to `ITERATIONS` iterations.
/// Price impact is relative to the spot rate implied by the first (smallest) step.
pub(crate) fn get_depth_actions(
    template: &Template,
    start_size: u64,
) -> Result<Vec<ScheduledAction>> {
    let Template {
        quote_to_base,
        base_to_quote,
        quote_signer,
        base_signer,
        quote_ata,
        base_ata,
        quote_mint,
        ..
    } = template;

    // Pre-fund enough to cover all `ITERATIONS` doublings of start_size
    let max_size = start_size.saturating_mul(1 << ITERATIONS);
    let quote_mint = &quote_mint.to_string();
    let overrides = AccountModifications(BTreeMap::from([
        (
            *quote_ata,
            make_token_account(quote_signer, quote_mint, max_size)?,
        ),
        (*base_signer, make_native_account(max_size)),
    ]));

    let mut size = start_size;
    let mut actions = vec![];
    for _ in 0..ITERATIONS {
        let q2b_tx = STANDARD.encode(bincode::serialize(&patch_titan_template_transaction(
            quote_to_base,
            *quote_ata,
            size,
        )?)?);
        let b2q_tx = STANDARD.encode(bincode::serialize(&patch_titan_template_transaction(
            base_to_quote,
            *base_ata,
            size,
        )?)?);

        actions.push(ScheduledAction {
            anchor: ActionAnchor::AfterSlot,
            kind: ActionKind::Simulate,
            transactions: vec![q2b_tx],
            account_overrides: overrides.clone(),
            return_accounts: vec![*quote_ata],
            label: Some(format!("{DEPTH_Q2B_PREFIX}{size}")),
        });

        actions.push(ScheduledAction {
            anchor: ActionAnchor::AfterSlot,
            kind: ActionKind::Simulate,
            transactions: vec![b2q_tx],
            account_overrides: overrides.clone(),
            return_accounts: vec![*base_signer],
            label: Some(format!("{DEPTH_B2Q_PREFIX}{size}")),
        });

        size = size.saturating_mul(2);
    }

    Ok(actions)
}

pub(crate) fn write_depth_output(
    filename: &str,
    records: &DepthRecords,
    quote: &str,
    base: &str,
) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(w, "slot,in_mint,size,out_mint,out_amount,price_impact_bps")?;
    // `max_depth` is ordered by `DepthKey` (slot ascending, then direction), so
    // rows come out grouped per slot with quote→base before base→quote.
    for (key, depth) in &records.max_depth {
        let (in_mint, out_mint) = match key.direction {
            DepthDirection::QuoteToBase => (quote, base),
            DepthDirection::BaseToQuote => (base, quote),
        };
        writeln!(
            w,
            "{},{},{},{},{},{:.2}",
            key.slot, in_mint, depth.size, out_mint, depth.out_amount, depth.price_impact_bps,
        )?;
    }
    Ok(())
}
