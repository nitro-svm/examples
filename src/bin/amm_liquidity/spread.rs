use std::io::{BufWriter, Write as _};

use anyhow::{Context, Result};
use simulator_client::BacktestSession;
use solana_transaction::versioned::VersionedTransaction;

use backtest_example::utils::accounts::set_account_balance;
use backtest_example::utils::parse::{parse_titan_sim_result, patch_titan_template_transaction};

use crate::Template;

pub(crate) struct SpreadRecord {
    pub(crate) slot: u64,
    pub(crate) input_amount: u64,
    pub(crate) output_amount: u64,
    pub(crate) spread_bps: f64,
}

async fn simulate_single_swap(session: &BacktestSession, tx: &VersionedTransaction) -> Result<u64> {
    let result = session
        .rpc()
        .simulate_transaction(tx)
        .await
        .context("simulate titan tx failed")?;
    let swap = parse_titan_sim_result(&result.value);
    let err = result
        .value
        .logs
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|l| l.contains("Error Message:"))
        .cloned();
    eprintln!(
        "    titan: out={} venues={:?} err={:?}",
        swap.out_amount, swap.venues, err
    );
    if swap.out_amount == 0 {
        static DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("    [LOGDUMP] err={:?}", result.value.err);
            for l in result.value.logs.as_deref().unwrap_or(&[]) {
                eprintln!("    [LOGDUMP] {l}");
            }
        }
    }
    Ok(swap.out_amount)
}

pub(crate) async fn simulate_roundtrip_swap(
    session: &BacktestSession,
    size: u64,
    template: &Template,
) -> Result<u64> {
    let q2b_tpl = &template.quote_to_base;
    let b2q_tpl = &template.base_to_quote;

    let quote_mint = template.quote_mint.to_string();
    let base_mint = template.base_mint.to_string();

    let original_quote =
        set_account_balance(session, &template.quote_signer, &quote_mint, size, true).await?;
    let q2b = patch_titan_template_transaction(q2b_tpl, template.quote_ata, size)?;
    let intermediate = simulate_single_swap(session, &q2b).await?;

    let original_base = set_account_balance(
        session,
        &template.base_signer,
        &base_mint,
        intermediate,
        true,
    )
    .await?;
    let b2q = patch_titan_template_transaction(b2q_tpl, template.base_ata, intermediate)?;
    let final_out = simulate_single_swap(session, &b2q).await?;

    let (r1, r2) = tokio::join!(
        set_account_balance(
            session,
            &template.quote_signer,
            &quote_mint,
            original_quote,
            true
        ),
        set_account_balance(
            session,
            &template.base_signer,
            &base_mint,
            original_base,
            true
        ),
    );
    r1?;
    r2?;

    Ok(final_out)
}

pub(crate) fn write_spread_output(
    filename: &str,
    records: &[SpreadRecord],
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
