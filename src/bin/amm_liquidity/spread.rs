use std::collections::BTreeMap;
use std::io::{BufWriter, Write as _};

use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use simulator_api::{AccountModifications, ActionAnchor, ActionKind, ScheduledAction};
use simulator_client::ActionResultNotification;

use backtest_example::utils::accounts::make_token_account;
use backtest_example::utils::parse::patch_titan_template_transaction;

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
    pub(crate) fn new(n: &ActionResultNotification, input: u64) -> Option<Self> {
        let output = n.accounts.first()?.as_ref().and_then(token_amount)?;
        Some(Self {
            slot: n.slot,
            input_amount: input,
            output_amount: output,
            spread_bps: (input as f64 - output as f64) / input as f64 * 10_000.0,
        })
    }
}

pub(crate) fn get_spread_action(template: &Template, size: u64) -> Result<ScheduledAction> {
    let Template {
        quote_ata,
        quote_signer,
        quote_mint,
        // TODO(@ygao): use circular arb tx
        quote_to_base,
        ..
    } = template;
    let overrides = AccountModifications(BTreeMap::from([(
        *quote_ata,
        make_token_account(quote_signer, &quote_mint.to_string(), size)?,
    )]));

    let action = ScheduledAction {
        anchor: ActionAnchor::AfterSlot,
        kind: ActionKind::Simulate,
        transactions: vec![STANDARD.encode(bincode::serialize(
            &patch_titan_template_transaction(quote_to_base, *quote_ata, size)?,
        )?)],
        account_overrides: overrides,
        return_accounts: vec![*quote_ata],
        label: Some(SPREAD_LABEL.to_string()),
    };

    Ok(action)
}

pub(crate) fn write_spread_output(
    filename: &str,
    records: &[Spread],
    quote_mint: &str,
    base_mint: &str,
) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(
        w,
        "slot,quote_mint,base_mint,input_amount,output_amount,spread_bps"
    )?;
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{}",
            r.slot, quote_mint, base_mint, r.input_amount, r.output_amount, r.spread_bps,
        )?;
    }
    Ok(())
}
