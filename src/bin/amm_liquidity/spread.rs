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
use solana_pubkey::Pubkey;

use backtest_example::utils::types::TitanVenueDiscriminant;
use backtest_example::utils::accounts::{make_native_account, make_token_account, make_token_ledger_account};
use backtest_example::utils::parse::{
    TITAN_PROGRAM, add_token_ledger, derive_ata, patch_titan_template_transaction,
    repoint_titan_static_account,
};

use crate::Template;
use super::action::spread_label;
use super::depth::DEPTH_SIGNER_LAMPORTS;

pub(crate) struct Spread {
    slot: u64,
    input_amount: u64,
    output_amount: u64,
    spread_bps: f64,
}

impl Spread {
    /// Build a [`Spread`] from a spread action result.
    pub(crate) fn new(slot: u64, input: u64, output: u64) -> Self {
        Self {
            slot,
            input_amount: input,
            output_amount: output,
            spread_bps: (input as f64 - output as f64) / input as f64 * 10_000.0,
        }
    }
}

/// Streams spread rows to a CSV as they arrive, so a long session doesn't hold
/// every record in memory. The header is written on construction, each row is
/// flushed on [`push`](SpreadStore::push), and [`finish`](SpreadStore::finish)
/// flushes any tail and reports the total.
pub(crate) struct SpreadStore {
    writer: BufWriter<File>,
    filename: String,
    quote_mint: String,
    base_mint: String,
    count: usize,
}

impl SpreadStore {
    pub(crate) fn new(filename: &str, quote_mint: &str, base_mint: &str) -> Result<Self> {
        let mut writer = BufWriter::new(File::create(filename)?);
        writeln!(
            writer,
            "slot,quote_mint,base_mint,input_amount,output_amount,spread_bps"
        )?;
        writer.flush()?;
        Ok(Self {
            writer,
            filename: filename.to_string(),
            quote_mint: quote_mint.to_string(),
            base_mint: base_mint.to_string(),
            count: 0,
        })
    }

    /// Write (and flush) a single spread row as soon as it is received.
    pub(crate) fn push(&mut self, spread: Spread) -> Result<()> {
        writeln!(
            self.writer,
            "{},{},{},{},{},{}",
            spread.slot,
            self.quote_mint,
            self.base_mint,
            spread.input_amount,
            spread.output_amount,
            spread.spread_bps,
        )?;
        self.writer.flush()?;
        self.count += 1;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.writer.flush()?;
        eprintln!(
            "[done] wrote {} spread rows to {}",
            self.count, self.filename
        );
        Ok(())
    }
}

/// Build a round-trip action: SOL -> USDC -> SOL through one venue in two txs.
///
/// Use SOL -> USDC -> SOL instead of USDC -> SOL -> USDC so that the intermediate
/// asset is an ATA and can leverage the Titan token ledger.
/// This allows leg 1's intermediate amount to be used by leg 2 dynamically.
///
///   hop1 (SOL→USDC): spend `size` WSOL, deposit USDC into `intermediate`.
///   hop2 (USDC→SOL): read input = `balance(intermediate) − ledger.amount` (= X)
///                    via the token ledger, output native SOL to quote_signer.
///
/// Final SOL (`Y`) is quote_signer's lamport delta over the seeded baseline;
/// spread = (size − Y) / size.
pub(crate) fn get_spread_action(
    template: &Template,
    size: u64,
    program_id: Option<Address>,
    venue: TitanVenueDiscriminant,
) -> Result<ScheduledAction> {
    let Template {
        quote_to_base, // (hop2)
        base_to_quote, // (hop1)
        quote_signer,
        base_signer,
        quote_mint,
        base_mint,
        ..
    } = template;

    // For an `AfterSlot` anchor, `transactions[i]` fires at `slots[i]`, and a
    // slot repeated across entries collects them, in order, into one sequence.
    let anchor = if let Some(program_id) = program_id {
        ActionAnchor::AfterMatch {
            filter: DiscoveryFilter::ProgramExecuted(program_id),
        }
    } else {
        let slots = (start_slot..=end_slot)
            .flat_map(|slot| [slot, slot])
            .collect();
        ActionAnchor::AfterSlot { slots }
    };

    let quote = &quote_mint.to_string();
    let base = &base_mint.to_string();
    let output = quote_signer;

    // Intermediate USDC account, shared across hops (hop1 deposits, hop2 spends):
    // quote_signer's USDC ATA, which is already hop2's input account.
    let intermediate = derive_ata(quote_signer, quote).context("derive intermediate USDC")?;
    // hop1's WSOL input, funded with the swept `size`.
    let input = derive_ata(base_signer, base).context("derive hop1 WSOL input")?;
    // Ledger snapshotting `intermediate` at 0, so hop2's input = what hop1 deposits.
    let titan: Pubkey = TITAN_PROGRAM.parse().context("parse titan program")?;
    let (ledger, _) = Pubkey::find_program_address(&[b"spread-ledger"], &titan);

    // hop1: SOL -> USDC, output repointed from base_signer's USDC ATA to `intermediate`.
    let hop1 = repoint_titan_static_account(
        &patch_titan_template_transaction(base_to_quote, input, size)?,
        4, // output_token_account
        intermediate,
    )?;
    // hop2: USDC→SOL, input read from `intermediate` ledger account
    // (the 0 here is ignored once a real ledger is supplied).
    let hop2 = add_token_ledger(
        &patch_titan_template_transaction(quote_to_base, intermediate, 0)?,
        ledger,
    )?;

    let hop1 = STANDARD.encode(bincode::serialize(&hop1)?);
    let hop2 = STANDARD.encode(bincode::serialize(&hop2)?);
    let transactions = if program_id.is_some() {
        vec![hop1, hop2]
    } else {
        (start_slot..=end_slot)
            .flat_map(|_| [hop1.clone(), hop2.clone()])
            .collect()
    };

    let account_overrides = AccountModifications(BTreeMap::from([
        (input, make_token_account(base_signer, base, size)?),
        (intermediate, make_token_account(quote_signer, quote, 0)?),
        (ledger, make_token_ledger_account(&intermediate, 0)),
        (*quote_signer, make_native_account(DEPTH_SIGNER_LAMPORTS)),
    ]));

    let action = ScheduledAction {
        anchor,
        kind: ActionKind::Simulate,
        transactions,
        account_overrides,
        return_accounts: vec![*output],
        label: Some(spread_label(venue)),
    };

    Ok(action)
}
