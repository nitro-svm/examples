use std::io::{BufWriter, Write as _};

use anyhow::Result;
use simulator_client::BacktestSession;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

use backtest_example::utils::accounts::set_account_balance;
use backtest_example::utils::parse::{parse_titan_sim_result, patch_titan_template_transaction};

use crate::Template;

pub(crate) struct Depth {
    size: u64,
    out_amount: u64,
    price_impact_bps: f64,
}

pub(crate) struct DepthRecord {
    pub(crate) slot: u64,
    pub(crate) quote_to_base: Depth,
    pub(crate) base_to_quote: Depth,
}

/// Geometric sweep of sizes through a single-venue template.
/// Doubles size each step, stops at max_impact_bps or zero output.
/// Price impact is relative to the spot rate implied by the first (smallest) step.
async fn sweep_liquidity_curve(
    session: &BacktestSession,
    single_venue_tx: &VersionedTransaction,
    in_ata: Pubkey,
    in_mint: &str,
    signer: &Pubkey,
    start_size: u64,
    max_impact_bps: u64,
) -> Result<Depth> {
    let mut last = Depth {
        size: 0,
        out_amount: 0,
        price_impact_bps: 0.0,
    };
    let mut spot_rate: Option<f64> = None;
    let mut size = start_size;

    // Pre-fund enough to cover all 20 doublings of start_size
    let original_balance = set_account_balance(
        session,
        signer,
        in_mint,
        start_size.saturating_mul(1 << 20),
        true,
    )
    .await?;

    for _ in 0..20 {
        let tx = patch_titan_template_transaction(single_venue_tx, in_ata, size)?;
        let result = session.rpc().simulate_transaction(&tx).await?;
        let swap = parse_titan_sim_result(&result.value);

        if swap.out_amount == 0 {
            eprintln!("  [depth {in_mint}] size={size} -> zero output, stopping");
        }

        let rate = swap.out_amount as f64 / size as f64;
        let spot = *spot_rate.get_or_insert(rate);
        let expected = spot * size as f64;
        let impact_bps = (expected - swap.out_amount as f64) / expected * 10_000.0;

        eprintln!(
            "  [depth {in_mint}] size={size} out={} impact={impact_bps:.1}bps venues={:?}",
            swap.out_amount, swap.venues
        );

        if impact_bps > max_impact_bps as f64 {
            break;
        }
        last = Depth {
            size,
            out_amount: swap.out_amount,
            price_impact_bps: impact_bps,
        };
        size = size.saturating_mul(2);
    }

    set_account_balance(session, signer, in_mint, original_balance, true).await?;

    Ok(last)
}

pub(crate) async fn get_max_depth(
    session: &BacktestSession,
    template: &Template,
    depth_start_size: u64,
    max_impact_bps: u64,
) -> Result<(Depth, Depth)> {
    // Hoist these out of the `join!` args: the futures borrow them, so they must
    // outlive the joined awaits rather than being dropped at the end of the arg expr.
    let quote_mint = template.quote_mint.to_string();
    let base_mint = template.base_mint.to_string();

    let (q2b, b2q) = tokio::join!(
        // quote -> base
        sweep_liquidity_curve(
            session,
            &template.quote_to_base,
            template.quote_ata,
            &quote_mint,
            &template.quote_signer,
            depth_start_size,
            max_impact_bps,
        ),
        // base -> quote
        sweep_liquidity_curve(
            session,
            &template.base_to_quote,
            template.base_ata,
            &base_mint,
            &template.base_signer,
            depth_start_size,
            max_impact_bps,
        ),
    );

    Ok((q2b?, b2q?))
}

pub(crate) fn write_depth_output(
    filename: &str,
    records: &[DepthRecord],
    quote: &str,
    base: &str,
) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(w, "slot,in_mint,size,out_mint,out_amount,price_impact_bps")?;
    for r in records {
        let q2b = &r.quote_to_base;
        writeln!(
            w,
            "{},{},{},{},{},{:.2}",
            r.slot, quote, q2b.size, base, q2b.out_amount, q2b.price_impact_bps,
        )?;

        let b2q = &r.base_to_quote;
        writeln!(
            w,
            "{},{}, {},{},{},{:.2}",
            r.slot, base, b2q.size, quote, b2q.out_amount, b2q.price_impact_bps,
        )?;
    }
    Ok(())
}
