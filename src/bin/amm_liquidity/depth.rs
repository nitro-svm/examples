use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write as _};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use simulator_api::{
    AccountModifications, ActionAnchor, ActionKind, DiscoveryFilter, ScheduledAction,
};
use solana_address::Address;

use backtest_example::utils::accounts::{
    make_native_account, make_token_account, patch_real_token22_account,
};
use backtest_example::utils::parse::{WSOL_MINT, derive_ata, patch_titan_template_transaction};
use backtest_example::utils::types::TitanVenueDiscriminant;

use super::action::{DepthDirection, depth_b2q_label, depth_q2b_label};
use crate::Template;

const ITERATIONS: usize = 12;

/// 1 SOL of headroom seeded into each signer for every depth action.
/// (Output is `post_lamports - DEPTH_SIGNER_LAMPORTS`).
pub(crate) const DEPTH_SIGNER_LAMPORTS: u64 = 1_000_000_000;

/// Native SOL headroom for a real historical signer whose current balance may be too
/// low to cover incidental costs (tx fees, an ATA program's defensive rent top-up on
/// an already-initialized account) — independent of any token-account overrides.
const FEE_HEADROOM_LAMPORTS: u64 = 10_000_000;

#[derive(Clone, Copy)]
pub(crate) struct Depth {
    size: u64,
    out_amount: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DepthKey {
    slot: u64,
    direction: DepthDirection,
    action_index: Option<u32>,
}

/// Accumulates the geometric sweep per (slot, direction) until it is complete,
/// then streams the single best-depth row to a CSV. Only the in-flight sweeps
/// are held in memory; finished rows are flushed immediately, so a long session
/// doesn't retain every result.
pub(crate) struct DepthStore {
    max_impact_bps: u64,
    all_depth: BTreeMap<DepthKey, Vec<Depth>>,
    intra_block_inspection_enabled: bool,
    writer: BufWriter<File>,
    filename: String,
    quote_mint: String,
    base_mint: String,
    count: usize,
}

impl Depth {
    pub(crate) fn new(size: u64, out_amount: u64) -> Self {
        Self { size, out_amount }
    }
}

// `AfterSlot` pairs each transaction 1:1 with a slot;
// fan the single tx to one copy per slot so the action fires at all of them.
fn repeat_per_slot(tx: String, program_id: Option<Address>, slot_count: usize) -> Vec<String> {
    if program_id.is_some() {
        vec![tx]
    } else {
        vec![tx; slot_count]
    }
}

impl DepthStore {
    pub(crate) fn new(
        max_impact_bps: u64,
        intra_block_inspection_enabled: bool,
        filename: &str,
        quote_mint: &str,
        base_mint: &str,
    ) -> Result<Self> {
        let mut writer = BufWriter::new(File::create(filename)?);
        writeln!(
            writer,
            "slot,in_mint,size,out_mint,out_amount,price_impact_bps"
        )?;
        writer.flush()?;
        Ok(Self {
            max_impact_bps,
            all_depth: BTreeMap::new(),
            intra_block_inspection_enabled,
            writer,
            filename: filename.to_string(),
            quote_mint: quote_mint.to_string(),
            base_mint: base_mint.to_string(),
            count: 0,
        })
    }

    pub(crate) fn add(
        &mut self,
        slot: u64,
        direction: DepthDirection,
        action_index: u32,
        depth: Depth,
    ) -> Result<()> {
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
            self.flush(key)?;
        }
        Ok(())
    }

    /// Aggregate a completed sweep and write it out immediately.
    fn flush(&mut self, key: DepthKey) -> Result<()> {
        let Some(mut depths) = self.all_depth.remove(&key) else {
            return Ok(());
        };
        depths.retain(|d| d.out_amount > 0);
        depths.sort_by_key(|d| d.size);

        // Spot rate anchored at the smallest fill's average rate, so impact is
        // relative to the best sampled price and the first point is 0 by construction.
        let spot = match depths.first() {
            Some(a) => a.out_amount as f64 / a.size as f64,
            _ => return Ok(()),
        };

        let (in_mint, out_mint) = match key.direction {
            DepthDirection::QuoteToBase => (&self.quote_mint, &self.base_mint),
            DepthDirection::BaseToQuote => (&self.base_mint, &self.quote_mint),
        };

        // Emit every point of the sweep so the full liquidity curve is visible, up to the
        // first size whose price impact exceeds `max_impact_bps` (past that the venue is
        // effectively out of liquidity for this direction).
        for d in depths {
            let expected = spot * d.size as f64;
            let price_impact_bps = (expected - d.out_amount as f64) / expected * 10_000.0;

            if price_impact_bps > self.max_impact_bps as f64 {
                break;
            }

            writeln!(
                self.writer,
                "{},{},{},{},{},{:.2}",
                key.slot,
                in_mint,
                d.size,
                out_mint,
                d.out_amount,
                price_impact_bps.max(0.0),
            )?;
            self.count += 1;
        }
        self.writer.flush()?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        // Flush incomplete sweeps (e.g. a size failed mid-sweep) so the
        // successful smaller fills still make it into the CSV.
        for key in self.all_depth.keys().copied().collect::<Vec<_>>() {
            self.flush(key)?;
        }
        self.writer.flush()?;
        eprintln!(
            "[done] wrote {} depth rows to {}",
            self.count, self.filename
        );
        Ok(())
    }
}

/// Geometric sweep of sizes through a single-venue template.
/// Doubles size each step, for up to `ITERATIONS` iterations.
/// Price impact is relative to the spot rate implied by the first (smallest) step.
#[allow(clippy::too_many_arguments)]
pub(crate) fn get_depth_actions(
    template: &Template,
    start_size: u64,
    program_id: Option<Address>,
    venue: TitanVenueDiscriminant,
    start_slot: u64,
    end_slot: u64,
    base_price_usdc: u64,
) -> Result<Vec<ScheduledAction>> {
    let Template {
        quote_to_base,
        base_to_quote,
        quote_signer,
        base_signer,
        quote_receiver,
        base_receiver,
        quote_mint,
        base_mint,
        quote_token_program,
        base_token_program,
        base_account: quote_receiver_real,
        ..
    } = template;

    // For an `AfterSlot` anchor, `transactions[i]` fires at `slots[i]`,
    // so "fire every slot in the replay range" means listing every
    // slot explicitly, paired with a repeat of the same transaction.
    let all_slots: Vec<u64> = (start_slot..=end_slot).collect();
    let anchor = if let Some(program_id) = program_id {
        ActionAnchor::AfterMatch {
            filter: DiscoveryFilter::ProgramExecuted(program_id),
        }
    } else {
        ActionAnchor::AfterSlot {
            slots: all_slots.clone(),
        }
    };

    let is_native_base = base_mint.to_string() == WSOL_MINT;

    // q2b sweeps USDC-native sizes; b2q sweeps the base-native amount of equal USD
    // value (per `base_price_usdc`) so both directions trade roughly the same
    // notional at each step.
    let q2b_start = start_size;
    let b2q_start = if is_native_base {
        // SOL is 9 decimals
        start_size.saturating_mul(1_000) / base_price_usdc
    } else {
        // other tokens are 6 decimals
        start_size / base_price_usdc
    };

    // Pre-fund enough to cover all `ITERATIONS` doublings of each leg's start size.
    let q2b_max = q2b_start.saturating_mul(1 << ITERATIONS);
    let b2q_max = b2q_start.saturating_mul(1 << ITERATIONS);
    let quote_mint = &quote_mint.to_string();
    let base_mint = &base_mint.to_string();
    let quote_token_program = &quote_token_program.to_string();
    let base_token_program = &base_token_program.to_string();

    // Inputs are each signer's ATA for the mint it spends (derived here).
    //   q2b (quote->base): swap quote from q2b_input to base in q2b_output.
    //   b2q (base->quote): swap base from b2q_input ATA to USDC in b2q_output ATA.
    let q2b_input = derive_ata(quote_signer, quote_mint).context("derive q2b USDC input")?;
    let q2b_output = if is_native_base {
        *quote_signer
    } else {
        *quote_receiver
    };
    let b2q_input = derive_ata(base_signer, base_mint).context("derive b2q base input")?;
    let b2q_output = base_receiver;

    // Fund the input ATA and seed each signer with native SOL for fees and rent.
    // When a real q2b_output already exists on-chain, patch its balance in place instead of
    // synthesizing a bare account — a synthetic account can't replicate Token-2022 extensions
    // (e.g. TransferHookAccount) that the mint may require.
    let q2b_output_account = if is_native_base {
        make_native_account(DEPTH_SIGNER_LAMPORTS)
    } else if let Some(real) = quote_receiver_real {
        patch_real_token22_account(real, 0)?
    } else {
        make_token_account(quote_signer, base_mint, 0, base_token_program)?
    };
    let mut q2b_overrides_map = BTreeMap::from([
        (
            q2b_input,
            make_token_account(quote_signer, quote_mint, q2b_max, quote_token_program)?,
        ),
        (q2b_output, q2b_output_account),
    ]);
    if !is_native_base {
        // quote_signer's real balance may be too drained to cover the ATA program's
        // defensive rent top-up on an already-initialized Token-2022 output account.
        q2b_overrides_map.insert(*quote_signer, make_native_account(FEE_HEADROOM_LAMPORTS));
    }
    let q2b_overrides = AccountModifications(q2b_overrides_map);
    let b2q_overrides = AccountModifications(BTreeMap::from([
        (
            b2q_input,
            make_token_account(base_signer, base_mint, b2q_max, base_token_program)?,
        ),
        (
            *b2q_output,
            make_token_account(base_signer, quote_mint, 0, quote_token_program)?,
        ),
    ]));

    let mut q2b_size = q2b_start;
    let mut b2q_size = b2q_start;
    let mut actions = vec![];
    for _ in 0..ITERATIONS {
        let q2b_tx = STANDARD.encode(bincode::serialize(&patch_titan_template_transaction(
            quote_to_base,
            q2b_input,
            q2b_size,
        )?)?);
        let b2q_tx = STANDARD.encode(bincode::serialize(&patch_titan_template_transaction(
            base_to_quote,
            b2q_input,
            b2q_size,
        )?)?);

        actions.push(ScheduledAction {
            anchor: anchor.clone(),
            kind: ActionKind::Simulate,
            transactions: repeat_per_slot(q2b_tx, program_id, all_slots.len()),
            account_overrides: q2b_overrides.clone(),
            return_accounts: vec![q2b_output],
            label: Some(depth_q2b_label(venue, q2b_size)),
        });

        actions.push(ScheduledAction {
            anchor: anchor.clone(),
            kind: ActionKind::Simulate,
            transactions: repeat_per_slot(b2q_tx, program_id, all_slots.len()),
            account_overrides: b2q_overrides.clone(),
            return_accounts: vec![*b2q_output],
            label: Some(depth_b2q_label(venue, b2q_size)),
        });

        q2b_size = q2b_size.saturating_mul(2);
        b2q_size = b2q_size.saturating_mul(2);
    }

    Ok(actions)
}
