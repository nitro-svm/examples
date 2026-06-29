use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use simulator_api::{
    AccountModifications, ActionAnchor, ActionKind, DiscoveryFilter, ScheduledAction,
};
use solana_address::Address;

use backtest_example::utils::accounts::{make_native_account, make_token_account};
use backtest_example::utils::parse::{derive_ata, patch_titan_template_transaction};

use crate::Template;
use crate::action::{DEPTH_B2Q_PREFIX, DEPTH_Q2B_PREFIX, DepthDirection};

const ITERATIONS: usize = 10; // 20;

/// Lamports seeded into each signer for every depth action — covers fees/rent
/// and, for q2b, is the baseline the SOL output lands on top of (the output is
/// `post_lamports - DEPTH_SIGNER_LAMPORTS`). 1 SOL of headroom.
pub(crate) const DEPTH_SIGNER_LAMPORTS: u64 = 1_000_000_000;

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
    action_index: Option<u32>,
}

pub(crate) struct DepthStore {
    max_impact_bps: u64,
    max_depth: BTreeMap<DepthKey, Depth>,
    all_depth: BTreeMap<DepthKey, Vec<Depth>>,
    intra_block_inspection_enabled: bool,
}

impl Depth {
    /// Build a depth point from a swap's `size` and decoded `out_amount`. The
    /// caller decodes the output from the action's return account per direction
    /// (token balance for the USDC leg, native-lamport delta for the SOL leg),
    /// since the AMM unwraps WSOL to native lamports on the SOL side.
    pub(crate) fn new(size: u64, out_amount: u64) -> Self {
        Self {
            size,
            out_amount,
            price_impact_bps: 0.0,
        }
    }
}

impl DepthStore {
    pub(crate) fn new(max_impact_bps: u64, intra_block_inspection_enabled: bool) -> Self {
        Self {
            max_impact_bps,
            max_depth: BTreeMap::new(),
            all_depth: BTreeMap::new(),
            intra_block_inspection_enabled,
        }
    }

    pub(crate) fn add(
        &mut self,
        slot: u64,
        direction: DepthDirection,
        action_index: u32,
        depth: Depth,
    ) {
        let action_index = if self.intra_block_inspection_enabled {
            Some(action_index)
        } else {
            None
        };

        let key = DepthKey {
            slot,
            direction,
            action_index,
        };

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
        depths.retain(|d| d.out_amount > 0);
        depths.sort_by_key(|d| d.size);

        // Spot rate from the slope between the two smallest fills, not
        // out[0]/size[0]. The SOL leg's output is read as a native-lamport delta
        // (it unwraps from WSOL, so it can't be read as a token) and carries a
        // constant, size-independent offset from swap-internal rent flows. A
        // slope cancels any such additive offset; on a clean leg it equals
        // out[0]/size[0], so this matches the canonical spot there.
        let spot = match (depths.first(), depths.get(1)) {
            (Some(a), Some(b)) if b.size > a.size => {
                (b.out_amount as f64 - a.out_amount as f64) / (b.size as f64 - a.size as f64)
            }
            // Only one usable fill: fall back to its average rate.
            (Some(a), _) => a.out_amount as f64 / a.size as f64,
            _ => return,
        };

        let mut best: Option<Depth> = None;
        for d in depths {
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

    pub(crate) fn write_output(&self, filename: &str, quote: &str, base: &str) -> Result<()> {
        let f = std::fs::File::create(filename)?;
        let mut w = BufWriter::new(f);
        writeln!(w, "slot,in_mint,size,out_mint,out_amount,price_impact_bps")?;
        // `max_depth` is ordered by `DepthKey` (slot ascending, then direction), so
        // rows come out grouped per slot with quote→base before base→quote.
        for (key, depth) in &self.max_depth {
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

        eprintln!(
            "[done] wrote {} depth rows to {filename}",
            self.max_depth.len(),
        );
        Ok(())
    }
}

/// Geometric sweep of sizes through a single-venue template.
/// Doubles size each step, for up to `ITERATIONS` iterations.
/// Price impact is relative to the spot rate implied by the first (smallest) step.
pub(crate) fn get_depth_actions(
    template: &Template,
    start_size: u64,
    program_id: Option<Address>,
) -> Result<Vec<ScheduledAction>> {
    let Template {
        quote_to_base,
        base_to_quote,
        quote_signer,
        base_signer,
        base_receiver,
        quote_mint,
        base_mint,
        ..
    } = template;

    let anchor = if let Some(program_id) = program_id {
        ActionAnchor::AfterMatch {
            filter: DiscoveryFilter::ProgramExecuted(program_id),
        }
    } else {
        ActionAnchor::AfterSlot
    };

    // Pre-fund enough to cover all `ITERATIONS` doublings of start_size
    let max_size = start_size.saturating_mul(1 << ITERATIONS);
    let quote_mint = &quote_mint.to_string();
    let base_mint = &base_mint.to_string();

    // Inputs are each signer's ATA for the mint it spends (derived here).
    //   q2b (USDC→SOL): spend USDC from q2b_input. Titan unwraps the SOL output
    //     to native lamports (the receiver WSOL ATA reads 0), so the output is
    //     quote_signer's lamport delta over the seeded baseline — we return
    //     quote_signer. The reading carries a constant size-independent offset
    //     from swap-internal rent flows, which the slope-based spot in `flush`
    //     cancels.
    //   b2q (SOL→USDC): spend WSOL from b2q_input, receive USDC into
    //     base_receiver (zeroed first, so its post-balance is the swap output).
    let q2b_input = derive_ata(quote_signer, quote_mint).context("derive q2b USDC input")?;
    let b2q_input = derive_ata(base_signer, base_mint).context("derive b2q WSOL input")?;
    let b2q_output = base_receiver;

    // Fund the input ATA and seed each signer with native SOL for fees and rent.
    let q2b_overrides = AccountModifications(BTreeMap::from([
        (
            q2b_input,
            make_token_account(quote_signer, quote_mint, max_size)?,
        ),
        (*quote_signer, make_native_account(DEPTH_SIGNER_LAMPORTS)),
    ]));
    let b2q_overrides = AccountModifications(BTreeMap::from([
        (
            b2q_input,
            make_token_account(base_signer, base_mint, max_size)?,
        ),
        (*b2q_output, make_token_account(base_signer, quote_mint, 0)?),
        (*base_signer, make_native_account(DEPTH_SIGNER_LAMPORTS)),
    ]));

    let mut size = start_size;
    let mut actions = vec![];
    for _ in 0..ITERATIONS {
        let q2b_tx = STANDARD.encode(bincode::serialize(&patch_titan_template_transaction(
            quote_to_base,
            q2b_input,
            size,
        )?)?);
        let b2q_tx = STANDARD.encode(bincode::serialize(&patch_titan_template_transaction(
            base_to_quote,
            b2q_input,
            size,
        )?)?);

        actions.push(ScheduledAction {
            anchor: anchor.clone(),
            kind: ActionKind::Simulate,
            transactions: vec![q2b_tx],
            account_overrides: q2b_overrides.clone(),
            return_accounts: vec![*quote_signer],
            label: Some(format!("{DEPTH_Q2B_PREFIX}{size}")),
        });

        actions.push(ScheduledAction {
            anchor: anchor.clone(),
            kind: ActionKind::Simulate,
            transactions: vec![b2q_tx],
            account_overrides: b2q_overrides.clone(),
            return_accounts: vec![*b2q_output],
            label: Some(format!("{DEPTH_B2Q_PREFIX}{size}")),
        });

        size = size.saturating_mul(2);
    }

    Ok(actions)
}
