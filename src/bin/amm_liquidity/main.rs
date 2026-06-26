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

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_client::{BacktestClient, Continue, CreateSession};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

mod action;
mod depth;
mod spread;

use action::{ActionProcessor, subscribe_action_results};
use depth::DepthStore;
use spread::SpreadStore;

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

    fn get_program_id(self) -> Address {
        let program_id = match self {
            Self::ZeroFi => "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY",
            Self::HumidiFi => "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp",
            Self::GoonFi => "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j",
            Self::BisonFi => "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi",
            Self::GoonFiV2 => "goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE",
        };

        Address::from_str_const(program_id)
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
    enable_intra_block_inspection: bool,

    #[arg(long, default_value = USDC_MINT)]
    quote_mint: String,

    #[arg(long, default_value = WSOL_MINT)]
    base_mint: String,

    #[arg(long, default_value_t = false)]
    measure_spread: bool,

    #[arg(long, default_value_t = false)]
    measure_depth: bool,

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
    quote_to_base: VersionedTransaction,
    base_to_quote: VersionedTransaction,
    quote_mint: Address,
    base_mint: Address,
    quote_signer: Pubkey,
    base_signer: Pubkey,
    quote_ata: Pubkey,
    base_ata: Pubkey,
}

async fn get_template(venue_disc: u8) -> Result<Template> {
    let usdc_to_sol = get_titan_template_transaction(titan_template_v3::USDC_TO_SOL).await?;
    let sol_to_usdc = get_titan_template_transaction(titan_template_v3::SOL_TO_USDC).await?;
    let quote_signer = extract_signer(&usdc_to_sol)?;
    let base_signer = extract_signer(&sol_to_usdc)?;
    let quote_ata = derive_ata(&quote_signer, USDC_MINT).context("derive quote ATA")?;
    let base_ata = derive_ata(&base_signer, WSOL_MINT).context("derive base ATA")?;

    let quote_to_base = patch_titan_single_venue(&usdc_to_sol, venue_disc)?;
    let base_to_quote = patch_titan_single_venue(&sol_to_usdc, venue_disc)?;

    Ok(Template {
        quote_to_base,
        base_to_quote,
        quote_mint: Address::from_str_const(USDC_MINT),
        base_mint: Address::from_str_const(WSOL_MINT),
        quote_signer,
        base_signer,
        quote_ata,
        base_ata,
    })
}

// ── BPF upgrade detection ─────────────────────────────────────────────────────

#[allow(dead_code)]
static BPF_UPGRADEABLE_LOADER: LazyLock<Pubkey> = LazyLock::new(|| {
    "BPFLoaderUpgradeab1e11111111111111111111111"
        .parse()
        .expect("valid BPF upgradeable loader pubkey")
});

#[allow(dead_code)]
fn is_program_upgrade(tx: &VersionedTransaction, program_id: &Pubkey) -> bool {
    let keys = tx.message.static_account_keys();
    let Some(loader_idx) = keys.iter().position(|k| k == &*BPF_UPGRADEABLE_LOADER) else {
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
            && keys.get(prog_idx as usize) == Some(program_id)
        {
            return true;
        }
    }
    false
}

// ── session runners ───────────────────────────────────────────────────────────

/// Resolve a possibly-relative session endpoint against the base HTTP URL.
fn resolve_url(base: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    format!("{base}/{}", endpoint.trim_start_matches('/'))
}

async fn run_session(
    client: BacktestClient,
    cli: &Cli,
    template: &Template,
    program_id: Option<Address>,
) -> Result<(SpreadStore, DepthStore)> {
    let timeout = Some(Duration::from_secs(120));
    let action_processor = ActionProcessor::new(
        cli.spread_size,
        cli.depth_min,
        cli.max_impact_bps,
        program_id,
    );

    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .actions(action_processor.get_actions(template)?)
                .build(),
        )
        .await?;

    session.ensure_ready(Some(Duration::from_secs(600))).await?;

    // Subscribe before advancing, then process action results as they stream in,
    // concurrently with the advance loop.
    let (sub, rx) = subscribe_action_results(&session, &cli.url).await?;
    let handle = tokio::spawn(action_processor.parse_events(rx));

    loop {
        let result = session
            .advance(
                Continue::builder()
                    .advance_count(cli.end_slot - cli.start_slot)
                    .build(),
                timeout,
                |_| {},
            )
            .await?;

        if result.completed {
            break;
        }
    }

    // Stop the subscription; dropping its sender closes the channel, which ends
    // the processor's `recv()` loop after it drains the tail results.
    sub.stop.send(true).ok();
    sub.join_handle.await.ok();
    let (spread_records, depth_records) = handle.await.context("action processor panicked")?;
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

    let program_id = if cli.enable_intra_block_inspection {
        Some(TitanVenueDiscriminant::from_u8(cli.venue_disciminant)?.get_program_id())
    } else {
        None
    };

    let (spread_records, depth_records) = run_session(client, &cli, &template, program_id).await?;
    spread_records.write_output(&spread_file, &cli.quote_mint, &cli.base_mint)?;

    depth_records.write_output(&depth_file, &cli.quote_mint, &cli.base_mint)?;

    Ok(())
}
