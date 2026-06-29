use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use simulator_api::{
    AccountModifications, ActionAnchor, ActionKind, DiscoveryFilter, ScheduledAction,
};
use solana_address::Address;

use backtest_example::utils::accounts::make_token_account;
use backtest_example::utils::parse::{derive_ata, patch_titan_template_transaction};

use crate::Template;
use crate::action::{SPREAD_LABEL, token_amount};

pub(crate) struct Spread {
    slot: u64,
    input_amount: u64,
    output_amount: u64,
    spread_bps: f64,
}

impl Spread {
    /// Build a [`Spread`] from a spread action result.
    pub(crate) fn new(slot: u64, accounts: &[Option<serde_json::Value>], input: u64) -> Self {
        let output = if let Some(amount) = accounts
            .first()
            .and_then(|a| a.as_ref())
            .and_then(token_amount)
        {
            amount
        } else {
            0
        };

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

pub(crate) fn get_spread_action(
    template: &Template,
    size: u64,
    program_id: Option<Address>,
) -> Result<ScheduledAction> {
    let Template {
        quote_signer,
        quote_mint,
        // TODO(@ygao): use circular arb tx
        quote_to_base,
        ..
    } = template;

    let anchor = if let Some(program_id) = program_id {
        ActionAnchor::AfterMatch {
            filter: DiscoveryFilter::ProgramExecuted(program_id),
        }
    } else {
        ActionAnchor::AfterSlot
    };

    let quote_input =
        derive_ata(quote_signer, &quote_mint.to_string()).context("derive q2b USDC input")?;
    let transactions =
        vec![
            STANDARD.encode(bincode::serialize(&patch_titan_template_transaction(
                quote_to_base,
                quote_input,
                size,
            )?)?),
        ];

    let account_overrides = AccountModifications(BTreeMap::from([(
        quote_input,
        make_token_account(quote_signer, &quote_mint.to_string(), size)?,
    )]));

    let action = ScheduledAction {
        anchor,
        kind: ActionKind::Simulate,
        transactions,
        account_overrides,
        return_accounts: vec![quote_input],
        label: Some(SPREAD_LABEL.to_string()),
    };

    Ok(action)
}
