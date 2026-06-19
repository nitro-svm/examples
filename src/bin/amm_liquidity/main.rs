//! Measures the bid-ask spread and depth of a single-venue prop AMM by simulating
//! against frozen historical chain state.
//!
//! ## Spread methodology
//!
//! Round-trip (quote→base then base→quote) against the same frozen state.
//! ```text
//! spread_bps = (size - final_out) / size * 10_000
//! ```
//!
//! ## Depth methodology
//!
//! Geometric sweep of trade sizes (2× each step) in both directions through
//! the single venue only (other swap entries are stripped from the tx).
//! Stops when price impact exceeds `--max-impact-bps` (default 1000 = 10%)
//! or the AMM returns 0 output. Price impact is measured relative to the
//! spot rate implied by the smallest sweep size.
//!
//! The industry-standard depth metric is reported at 200 bps (2%) of impact.
//! The full curve is written to the depth CSV so any threshold can be read off.

use backtest_example::utils::parse::{
    USDC_MINT, WSOL_MINT, derive_ata, extract_signer,
    get_titan_template_transaction, parse_titan_sim_result,
    patch_titan_single_venue, patch_titan_template_transaction,
};
use backtest_example::utils::{accounts::set_account_balance, types::TxWithMeta};

use std::io::{BufWriter, Write as _};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{BacktestClient, BacktestSession, Continue, CreateSession, DiscoveryStepResult};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

const PROGRAM_ID: &str = "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi";

#[repr(u8)]
enum TitanVenueDiscriminant {
    ZeroFi = 13,
    HumidiFi = 28,
    GoonFi = 35,
    BisonFi = 55,
    GoonFiV2 = 57,
}

mod titan_template_v3 {
    pub const SOL_TO_USDC: &str =
        "24RysBDMt3gavdURB1H835C9KBC5ovsAdQ9AhdJ3HwccX9dvk29mNQkeUAKqUfHEC8UeqecoGkPqCKe2TViVF45Y";
    pub const USDC_TO_SOL: &str =
        "2RtLqCUeYBVhRppiJ2DFZoyVcwuJtPWprauRFfocynoiREYrGeJoqbpLM8bKsJkSoYpgr4oLnYEwCvrpDpiEZZV8";
}

#[derive(Parser)]
#[command(about = "Measure spread and depth for a single prop AMM venue across blocks")]
struct Cli {
    #[arg(long, default_value = "staging.simulator.termina.technology")]
    url: String,

    #[arg(long, env = "SIMULATOR_API_KEY")]
    api_key: String,

    #[arg(long, default_value_t = 422_818_048)]
    start_slot: u64,

    #[arg(long, default_value_t = 422_818_148)]
    end_slot: u64,

    #[arg(long, default_value = "spread.csv")]
    spread_output: String,

    #[arg(long, default_value = "depth.csv")]
    depth_output: String,

    #[arg(long, default_value = "")]
    program_id: String,

    #[arg(long, default_value = USDC_MINT)]
    quote_mint: String,

    #[arg(long, default_value = WSOL_MINT)]
    base_mint: String,

    /// Spread measurement size in quote-mint native units.
    #[arg(long, default_value_t = 5_000_000_000)]
    size: u64,

    /// Smallest size for the depth sweep (quote-mint native units).
    #[arg(long, default_value_t = 10_000_000)]
    depth_start_size: u64,

    /// Stop the depth sweep once price impact exceeds this many bps.
    #[arg(long, default_value_t = 1000)]
    max_impact_bps: u64,

    /// Titan Venue discriminant to isolate (55 = BisonFi, 13 = ZeroFi, 28 = HumidiFi,
    /// 35 = GoonFi, 57 = GoonFiV2). See the Venue enum in the Titan IDL.
    #[arg(long, default_value_t = TitanVenueDiscriminant::BisonFi as u8)]
    venue_disc: u8,
}

// ── output records ───────────────────────────────────────────────────────────

struct SpreadRecord {
    slot: u64,
    quote_mint: String,
    base_mint: String,
    input_amount: u64,
    output_amount: u64,
    spread_bps: f64,
}

struct DepthRecord {
    slot: u64,
    direction: &'static str,
    size: u64,
    out_amount: u64,
    price_impact_bps: f64,
}

fn write_spread_output(filename: &str, records: &[SpreadRecord]) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(w, "slot,quote_mint,base_mint,input_amount,output_amount,spread_bps")?;
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{}",
            r.slot, r.quote_mint, r.base_mint, r.input_amount, r.output_amount, r.spread_bps,
        )?;
    }
    Ok(())
}

fn write_depth_output(filename: &str, records: &[DepthRecord]) -> Result<()> {
    let f = std::fs::File::create(filename)?;
    let mut w = BufWriter::new(f);
    writeln!(w, "slot,direction,size,out_amount,price_impact_bps")?;
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{:.2}",
            r.slot, r.direction, r.size, r.out_amount, r.price_impact_bps,
        )?;
    }
    Ok(())
}

// ── template ─────────────────────────────────────────────────────────────────

struct Template {
    // Full multi-venue templates (used for spread round-trip)
    quote_to_base: VersionedTransaction,
    base_to_quote: VersionedTransaction,
    quote_mint: Address,
    base_mint: Address,
    quote_signer: Pubkey,
    base_signer: Pubkey,
    quote_ata: Pubkey,
    base_ata: Pubkey,
    // Single-venue templates (used for depth sweep); None if venue not found in template
    quote_to_base_single: Option<VersionedTransaction>,
    base_to_quote_single: Option<VersionedTransaction>,
}

async fn get_template(venue_disc: u8) -> Result<Template> {
    let usdc_to_sol = get_titan_template_transaction(titan_template_v3::USDC_TO_SOL).await?;
    let sol_to_usdc = get_titan_template_transaction(titan_template_v3::SOL_TO_USDC).await?;
    let quote_signer = extract_signer(&usdc_to_sol)?;
    let base_signer = extract_signer(&sol_to_usdc)?;
    let quote_ata = derive_ata(&quote_signer, USDC_MINT).context("derive quote ATA")?;
    let base_ata = derive_ata(&base_signer, WSOL_MINT).context("derive base ATA")?;

    let quote_to_base_single = match patch_titan_single_venue(&usdc_to_sol, venue_disc) {
        Ok(tx) => { eprintln!("[template] single-venue quote->base patch OK (venue {venue_disc})"); Some(tx) }
        Err(e) => { eprintln!("[template] single-venue quote->base patch failed: {e}"); None }
    };
    let base_to_quote_single = match patch_titan_single_venue(&sol_to_usdc, venue_disc) {
        Ok(tx) => { eprintln!("[template] single-venue base->quote patch OK (venue {venue_disc})"); Some(tx) }
        Err(e) => { eprintln!("[template] single-venue base->quote patch failed: {e}"); None }
    };

    Ok(Template {
        quote_to_base: usdc_to_sol,
        base_to_quote: sol_to_usdc,
        quote_mint: Address::from_str_const(USDC_MINT),
        base_mint: Address::from_str_const(WSOL_MINT),
        quote_signer,
        base_signer,
        quote_ata,
        base_ata,
        quote_to_base_single,
        base_to_quote_single,
    })
}

// ── simulation helpers ────────────────────────────────────────────────────────

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

async fn simulate_roundtrip_swap(
    session: &BacktestSession,
    size: u64,
    template: &Template,
) -> Result<u64> {
    let (Some(q2b_tpl), Some(b2q_tpl)) = (&template.quote_to_base_single, &template.base_to_quote_single) else {
        return Ok(0);
    };

    let quote_mint = template.quote_mint.to_string();
    let base_mint = template.base_mint.to_string();

    let orig_quote = set_account_balance(&session, &template.quote_signer, &quote_mint, size, true).await?;
    let q2b = patch_titan_template_transaction(q2b_tpl, template.quote_ata, size)?;
    let intermediate = simulate_single_swap(session, &q2b).await?;

    if intermediate == 0 {
        set_account_balance(&session, &template.quote_signer, &quote_mint, orig_quote, true).await?;
        return Ok(0);
    }

    let orig_base = set_account_balance(&session, &template.base_signer, &base_mint, intermediate, true).await?;
    let b2q = patch_titan_template_transaction(b2q_tpl, template.base_ata, intermediate)?;
    let final_out = simulate_single_swap(session, &b2q).await?;

    let (r1, r2) = tokio::join!(
        set_account_balance(&session, &template.quote_signer, &quote_mint, orig_quote, true),
        set_account_balance(&session, &template.base_signer, &base_mint, orig_base, true),
    );
    r1?;
    r2?;

    Ok(final_out)
}

/// Geometric sweep of sizes through a single-venue template.
/// Doubles size each step, stops at max_impact_bps or zero output.
/// Price impact is relative to the spot rate implied by the first (smallest) step.
async fn sweep_depth(
    session: &BacktestSession,
    single_venue_tx: &VersionedTransaction,
    in_ata: Pubkey,
    in_mint: &str,
    signer: &Pubkey,
    start_size: u64,
    max_impact_bps: u64,
    direction: &'static str,
    slot: u64,
) -> Result<Option<DepthRecord>> {
    let mut last: Option<DepthRecord> = None;
    let mut spot_rate: Option<f64> = None;
    let mut size = start_size;

    // Pre-fund enough to cover all 20 doublings of start_size
    let original_balance = set_account_balance(session, signer, in_mint, start_size.saturating_mul(1 << 20), true).await?;

    for _ in 0..20 {
        let tx = patch_titan_template_transaction(single_venue_tx, in_ata, size)?;
        let result = session.rpc().simulate_transaction(&tx).await?;
        let swap = parse_titan_sim_result(&result.value);

        if swap.out_amount == 0 {
            eprintln!("  [depth {direction}] size={size} → zero output, stopping");
            static DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if direction == "base_to_quote" && !DUMPED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!("  [DEPTHDUMP] err={:?}", result.value.err);
                for l in result.value.logs.as_deref().unwrap_or(&[]) {
                    eprintln!("  [DEPTHDUMP] {l}");
                }
            }
            break;
        }

        let rate = swap.out_amount as f64 / size as f64;
        let spot = *spot_rate.get_or_insert(rate);
        let expected = spot * size as f64;
        let impact_bps = (expected - swap.out_amount as f64) / expected * 10_000.0;

        eprintln!("  [depth {direction}] size={size} out={} impact={impact_bps:.1}bps venues={:?}", swap.out_amount, swap.venues);

        if impact_bps > max_impact_bps as f64 {
            break;
        }
        last = Some(DepthRecord { slot, direction, size, out_amount: swap.out_amount, price_impact_bps: impact_bps });
        size = size.saturating_mul(2);
    }

    set_account_balance(session, signer, in_mint, original_balance, true).await?;

    Ok(last)
}

async fn get_depth(
    session: &BacktestSession, 
    template: &Template, 
    slot: u64, 
    depth_start_size: u64, 
    max_impact_bps: u64,
) -> Result<Vec<DepthRecord>> {
    let mut records: Vec<DepthRecord> = vec![];

    // quote -> base
    if let Some(q2b) = &template.quote_to_base_single {
        if let Some(row) = sweep_depth(
            session,
            q2b,
            template.quote_ata,
            &template.quote_mint.to_string(),
            &template.quote_signer,
            depth_start_size,
            max_impact_bps,
            "quote_to_base",
            slot,
        )
        .await? {
            records.push(row);
        }
    }

    // base -> quote
    if let Some(b2q) = &template.base_to_quote_single {
        if let Some(row) = sweep_depth(
            session,
            b2q,
            template.base_ata,
            &template.base_mint.to_string(),
            &template.base_signer,
            depth_start_size,
            max_impact_bps,
            "base_to_quote",
            slot,
        )
        .await? {
            records.push(row);
        }
    }

    Ok(records)
}

// ── BPF upgrade detection ─────────────────────────────────────────────────────

const BPF_UPGRADEABLE_LOADER: &str = "BPFLoaderUpgradeab1e11111111111111111111111";

fn is_program_upgrade(tx: &VersionedTransaction, program_id: &str) -> bool {
    let keys = tx.message.static_account_keys();
    let key_str: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    let Some(loader_idx) = key_str.iter().position(|k| k == BPF_UPGRADEABLE_LOADER) else {
        return false;
    };
    for ix in tx.message.instructions() {
        if ix.program_id_index as usize != loader_idx {
            continue;
        }
        if ix.data.get(..4) != Some(&[3, 0, 0, 0]) {
            continue;
        }
        if let Some(&prog_idx) = ix.accounts.get(1) {
            if key_str.get(prog_idx as usize).map(|s| s.as_str()) == Some(program_id) {
                return true;
            }
        }
    }
    false
}

// ── session runners ───────────────────────────────────────────────────────────

async fn run_measurements(
    session: &BacktestSession,
    slot: u64,
    cli: &Cli,
    template: &Template,
    spread_records: &mut Vec<SpreadRecord>,
    depth_records: &mut Vec<DepthRecord>,
) -> Result<()> {
    // Spread
    let out_amount = simulate_roundtrip_swap(session, cli.size, template).await?;
    if out_amount > 0 {
        spread_records.push(SpreadRecord {
            slot,
            quote_mint: template.quote_mint.to_string(),
            base_mint: template.base_mint.to_string(),
            input_amount: cli.size,
            output_amount: out_amount,
            spread_bps: (cli.size - out_amount) as f64 / cli.size as f64 * 10_000.0,
        });
    }

    // Depth
    let mut records = get_depth(session, template, slot, cli.depth_start_size, cli.max_impact_bps).await?;
    depth_records.append(&mut records);

    Ok(())
}

async fn run_discovery_session(
    client: BacktestClient,
    program_addr: Address,
    cli: Cli,
    template: &Template,
) -> Result<(Vec<SpreadRecord>, Vec<DepthRecord>)> {
    let mut spread_records = Vec::new();
    let mut depth_records = Vec::new();
    let mut pause_count = 0u64;
    let timeout = Some(Duration::from_secs(120));

    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(program_addr)])
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));
    session.ensure_ready(Some(Duration::from_secs(600))).await?;
    eprintln!("[ws] ready — scanning for {} batches", cli.program_id);

    loop {
        match session.advance_to_discovery(timeout).await? {
            DiscoveryStepResult::Paused(pause) => {
                pause_count += 1;
                let slot = pause.paused.slot;
                let batch = pause.paused.batch_index.unwrap_or(0);
                eprintln!("[pause #{pause_count}] slot={slot} batch={batch}");

                let txs: Vec<TxWithMeta> = pause
                    .discovery
                    .transactions
                    .iter()
                    .filter_map(|bin| {
                        let bytes = bin.decode().ok()?;
                        bincode::deserialize(&bytes).ok()
                    })
                    .collect();
                if !txs.iter().any(|t| is_program_upgrade(&t.transaction, &cli.program_id)) {
                    continue;
                }

                if let Err(e) = run_measurements(&session, slot, &cli, template, &mut spread_records, &mut depth_records).await {
                    eprintln!("[error] slot {slot}: {e:#}");
                }
            }
            DiscoveryStepResult::Completed => {
                eprintln!("[done] session completed; total pauses: {pause_count}");
                break;
            }
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;
    Ok((spread_records, depth_records))
}

async fn run_regular_session(
    client: BacktestClient,
    cli: Cli,
    template: &Template,
) -> Result<(Vec<SpreadRecord>, Vec<DepthRecord>)> {
    let mut spread_records = Vec::new();
    let mut depth_records = Vec::new();
    let timeout = Some(Duration::from_secs(120));

    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .build(),
        )
        .await?;

    session.ensure_ready(Some(Duration::from_secs(600))).await?;

    loop {
        let result = session
            .advance(Continue::builder().advance_count(1).build(), timeout, |_| {})
            .await?;

        let slot = result.last_slot.unwrap_or(0);
        if let Err(e) = run_measurements(&session, slot, &cli, template, &mut spread_records, &mut depth_records).await {
            eprintln!("[error] slot {slot}: {e:#}");
        }

        if result.completed {
            break;
        }
    }

    let _ = session.close(Some(Duration::from_secs(10))).await;
    Ok((spread_records, depth_records))
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();
    let template = get_template(cli.venue_disc).await?;

    let client = BacktestClient::builder()
        .url(format!("wss://{}/backtest", &cli.url))
        .api_key(cli.api_key.clone())
        .build();

    eprintln!("[ws] connecting to wss://{}/backtest", &cli.url);

    let spread_output = cli.spread_output.clone();
    let depth_output = cli.depth_output.clone();

    let program_addr = cli.program_id.parse::<Address>().ok();
    let (spread_records, depth_records) = if let Some(program_addr) = program_addr {
        run_discovery_session(client, program_addr, cli, &template).await?
    } else {
        run_regular_session(client, cli, &template).await?
    };

    write_spread_output(&spread_output, &spread_records)?;
    eprintln!("[done] wrote {} spread rows to {spread_output}", spread_records.len());

    write_depth_output(&depth_output, &depth_records)?;
    eprintln!("[done] wrote {} depth rows to {depth_output}", depth_records.len());

    Ok(())
}
