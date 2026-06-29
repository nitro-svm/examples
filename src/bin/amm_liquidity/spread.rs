use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use simulator_api::{
    AccountModifications, ActionAnchor, ActionKind, DiscoveryFilter, ScheduledAction,
};
use solana_address::Address;
use solana_pubkey::Pubkey;

use backtest_example::utils::accounts::{
    make_native_account, make_token_account, make_token_ledger_account,
};
use backtest_example::utils::parse::{
    TITAN_PROGRAM, add_token_ledger, derive_ata, patch_titan_template_transaction,
    repoint_titan_static_account,
};

use crate::Template;
use crate::action::SPREAD_LABEL;
use crate::depth::DEPTH_SIGNER_LAMPORTS;

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

pub(crate) struct SpreadStore {
    records: Vec<Spread>,
}

impl SpreadStore {
    pub(crate) fn new() -> Self {
        Self { records: vec![] }
    }

    pub(crate) fn push(&mut self, spread: Spread) {
        self.records.push(spread);
    }

    pub(crate) fn write_output(
        &self,
        filename: &str,
        quote_mint: &str,
        base_mint: &str,
    ) -> Result<()> {
        let f = std::fs::File::create(filename)?;
        let mut w = BufWriter::new(f);
        writeln!(
            w,
            "slot,quote_mint,base_mint,input_amount,output_amount,spread_bps"
        )?;
        for r in &self.records {
            writeln!(
                w,
                "{},{},{},{},{},{}",
                r.slot, quote_mint, base_mint, r.input_amount, r.output_amount, r.spread_bps,
            )?;
        }

        eprintln!(
            "[done] wrote {} spread rows to {filename}",
            self.records.len()
        );
        Ok(())
    }
}

/// Build a round-trip spread action: SOL → USDC → SOL through one venue, in two
/// threaded transactions. `size` is the SOL input (lamports). Putting USDC in the
/// middle keeps the intermediate as a real token account (the SOL leg would
/// otherwise unwrap to native), so a Titan token ledger can carry the dynamic
/// intermediate amount from hop1 into hop2 — letting hop2's input be exactly
/// hop1's output without knowing it at registration time.
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
) -> Result<ScheduledAction> {
    let Template {
        quote_to_base, // USDC→SOL  (hop2)
        base_to_quote, // SOL→USDC  (hop1)
        quote_signer,
        base_signer,
        quote_mint, // USDC
        base_mint,  // WSOL
        ..
    } = template;

    let anchor = if let Some(program_id) = program_id {
        ActionAnchor::AfterMatch {
            filter: DiscoveryFilter::ProgramExecuted(program_id),
        }
    } else {
        ActionAnchor::AfterSlot
    };

    let usdc = &quote_mint.to_string();
    let wsol = &base_mint.to_string();

    // Intermediate USDC account, shared across hops (hop1 deposits, hop2 spends):
    // quote_signer's USDC ATA, which is already hop2's input account.
    let intermediate = derive_ata(quote_signer, usdc).context("derive intermediate USDC")?;
    // hop1's WSOL input, funded with the swept `size`.
    let hop1_input = derive_ata(base_signer, wsol).context("derive hop1 WSOL input")?;
    // Ledger snapshotting `intermediate` at 0, so hop2's input = what hop1 deposits.
    let titan: Pubkey = TITAN_PROGRAM.parse().context("parse titan program")?;
    let (ledger, _) = Pubkey::find_program_address(&[b"spread-ledger"], &titan);

    // hop1: SOL→USDC, output repointed from base_signer's USDC ATA to `intermediate`.
    let hop1 = repoint_titan_static_account(
        &patch_titan_template_transaction(base_to_quote, hop1_input, size)?,
        4, // output_token_account
        intermediate,
    )?;
    // hop2: USDC→SOL, input = `intermediate`, amount read from the ledger (the 0
    // here is ignored once a real ledger is supplied).
    let hop2 = add_token_ledger(
        &patch_titan_template_transaction(quote_to_base, intermediate, 0)?,
        ledger,
    )?;

    let transactions = vec![
        STANDARD.encode(bincode::serialize(&hop1)?),
        STANDARD.encode(bincode::serialize(&hop2)?),
    ];

    let account_overrides = AccountModifications(BTreeMap::from([
        (hop1_input, make_token_account(base_signer, wsol, size)?),
        (intermediate, make_token_account(quote_signer, usdc, 0)?),
        (ledger, make_token_ledger_account(&intermediate, 0)),
        (*quote_signer, make_native_account(DEPTH_SIGNER_LAMPORTS)),
    ]));

    let action = ScheduledAction {
        anchor,
        kind: ActionKind::Simulate,
        transactions,
        account_overrides,
        return_accounts: vec![*quote_signer],
        label: Some(SPREAD_LABEL.to_string()),
    };

    Ok(action)
}
