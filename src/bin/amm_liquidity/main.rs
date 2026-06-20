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
//! Geometric sweep of trade sizes (2x each step) in both directions through
//! a single venue until price impact exceeds `--max-impact-bps` (default 1000 = 10%)
//! or the AMM returns 0 output. Price impact is measured relative to the
//! spot rate implied by the smallest sweep size.
//!
//! The industry-standard depth metric is reported at 200 bps (2%) of impact.
//! The full curve is written to the depth CSV so any threshold can be read off.

use backtest_example::utils::parse::{
    USDC_MINT, WSOL_MINT, derive_ata, extract_signer, get_titan_template_transaction,
    patch_titan_single_venue,
};
use backtest_example::utils::types::TxWithMeta;

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{
    BacktestClient, BacktestSession, Continue, CreateSession, DiscoveryStepResult,
};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

mod depth;
mod spread;

use depth::{DepthRecord, get_max_depth, write_depth_output};
use spread::{SpreadRecord, simulate_roundtrip_swap, write_spread_output};

#[repr(u8)]
enum TitanVenueDiscriminant {
    ZeroFi = 13,
    HumidiFi = 28,
    GoonFi = 35,
    BisonFi = 55,
    GoonFiV2 = 57,
}

impl TitanVenueDiscriminant {
    fn from_u8(disc: u8) -> Result<Self> {
        match disc {
            13 => Ok(Self::ZeroFi),
            28 => Ok(Self::HumidiFi),
            35 => Ok(Self::GoonFi),
            55 => Ok(Self::BisonFi),
            57 => Ok(Self::GoonFiV2),
            other => anyhow::bail!("unknown venue discriminant: {other}"),
        }
    }

    fn get_program_id(self) -> &'static str {
        match self {
            Self::ZeroFi => "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY",
            Self::HumidiFi => "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp",
            Self::GoonFi => "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j",
            Self::BisonFi => "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi",
            Self::GoonFiV2 => "goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE",
        }
    }
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

    #[arg(long, default_value_t = false)]
    pause_on_upgrade: bool,

    #[arg(long, default_value = USDC_MINT)]
    quote_mint: String,

    #[arg(long, default_value = WSOL_MINT)]
    base_mint: String,

    /// Spread measurement size in quote-mint native units.
    #[arg(long, default_value_t = 5_000_000_000)]
    spread_size: u64,

    /// Smallest size for the depth sweep (quote-mint native units).
    #[arg(long, default_value_t = 10_000_000)]
    depth_min: u64,

    /// Stop the depth sweep once price impact exceeds this many bps.
    #[arg(long, default_value_t = 1000)]
    max_impact_bps: u64,

    /// Titan Venue discriminant to isolate (55 = BisonFi, 13 = ZeroFi, 28 = HumidiFi,
    /// 35 = GoonFi, 57 = GoonFiV2). See the Venue enum in the Titan IDL.
    #[arg(long, default_value_t = TitanVenueDiscriminant::BisonFi as u8)]
    venue_disciminant: u8,
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
    // TODO: combine with multi-venue
    quote_to_base_single: VersionedTransaction,
    base_to_quote_single: VersionedTransaction,
}

async fn get_template(venue_disc: u8) -> Result<Template> {
    let usdc_to_sol = get_titan_template_transaction(titan_template_v3::USDC_TO_SOL).await?;
    let sol_to_usdc = get_titan_template_transaction(titan_template_v3::SOL_TO_USDC).await?;
    let quote_signer = extract_signer(&usdc_to_sol)?;
    let base_signer = extract_signer(&sol_to_usdc)?;
    let quote_ata = derive_ata(&quote_signer, USDC_MINT).context("derive quote ATA")?;
    let base_ata = derive_ata(&base_signer, WSOL_MINT).context("derive base ATA")?;

    let quote_to_base_single = patch_titan_single_venue(&usdc_to_sol, venue_disc)?;
    let base_to_quote_single = patch_titan_single_venue(&sol_to_usdc, venue_disc)?;

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
        if let Some(&prog_idx) = ix.accounts.get(1)
            && key_str.get(prog_idx as usize).map(|s| s.as_str()) == Some(program_id)
        {
            return true;
        }
    }
    false
}

// ── session runners ───────────────────────────────────────────────────────────

struct Measurement {
    spread: Option<SpreadRecord>,
    depth: Option<DepthRecord>,
}

async fn run_measurements(
    session: &BacktestSession,
    slot: u64,
    cli: &Cli,
    template: &Template,
) -> Result<Measurement> {
    // Spread
    let spread = match simulate_roundtrip_swap(session, cli.spread_size, template).await {
        Ok(out_amount) => (out_amount > 0).then(|| SpreadRecord {
            slot,
            input_amount: cli.spread_size,
            output_amount: out_amount,
            spread_bps: (cli.spread_size - out_amount) as f64 / cli.spread_size as f64 * 10_000.0,
        }),
        Err(e) => {
            eprint!("Failed to measure spread: {e}");
            None
        }
    };

    // Depth
    // let depth = match get_max_depth(session, template, cli.depth_min, cli.max_impact_bps).await {
    //     Ok((quote_to_base, base_to_quote)) => Some(DepthRecord {
    //         slot,
    //         quote_to_base,
    //         base_to_quote,
    //     }),
    //     Err(e) => {
    //         eprint!("Failed to measure depth: {e}");
    //         None
    //     }
    // };

    Ok(Measurement { spread, depth: None })
}

async fn run_discovery_session(
    client: BacktestClient,
    program_addr: Address,
    cli: &Cli,
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
    eprintln!("[ws] ready — scanning for {program_addr} batches");

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
                if !txs
                    .iter()
                    .any(|t| is_program_upgrade(&t.transaction, &program_addr.to_string()))
                {
                    continue;
                }

                match run_measurements(&session, slot, cli, template).await {
                    Ok(m) => {
                        spread_records.extend(m.spread);
                        depth_records.extend(m.depth);
                    }
                    Err(e) => eprintln!("[error] slot {slot}: {e:#}"),
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
    cli: &Cli,
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
            .advance(
                Continue::builder().advance_count(1).build(),
                timeout,
                |_| {},
            )
            .await?;

        let slot = result.last_slot.unwrap_or(0);
        match run_measurements(&session, slot, cli, template).await {
            Ok(m) => {
                spread_records.extend(m.spread);
                depth_records.extend(m.depth);
            }
            Err(e) => eprintln!("[error] slot {slot}: {e:#}"),
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
    let template = get_template(cli.venue_disciminant).await?;

    let client = BacktestClient::builder()
        .url(&cli.url)
        .api_key(cli.api_key.clone())
        .build();

    eprintln!("[ws] connecting to wss://{}/backtest", &cli.url);

    let spread_file = cli.spread_output.clone();
    let depth_file = cli.depth_output.clone();

    let (spread_records, depth_records) = if cli.pause_on_upgrade {
        let program_addr = TitanVenueDiscriminant::from_u8(cli.venue_disciminant)?.get_program_id();
        run_discovery_session(client, Pubkey::from_str(program_addr)?, &cli, &template).await?
    } else {
        run_regular_session(client, &cli, &template).await?
    };

    write_spread_output(
        &spread_file,
        &spread_records,
        &cli.quote_mint,
        &cli.base_mint,
    )?;
    eprintln!(
        "[done] wrote {} spread rows to {spread_file}",
        spread_records.len()
    );

    write_depth_output(&depth_file, &depth_records, &cli.quote_mint, &cli.base_mint)?;
    eprintln!(
        "[done] wrote {} depth rows to {depth_file}",
        depth_records.len()
    );

    Ok(())
}
