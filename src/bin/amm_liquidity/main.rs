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

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::DiscoveryFilter;
use simulator_client::{BacktestClient, Continue, CreateSession, DiscoveryStepResult};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

mod action;
mod depth;
mod spread;

use action::subscribe_action_results;
use depth::{DepthRecord, write_depth_output};
use spread::{SpreadRecord, write_spread_output};

use crate::action::Label;
use crate::depth::get_depth_actions;
use crate::spread::get_spread_action;

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

static BPF_UPGRADEABLE_LOADER: LazyLock<Pubkey> = LazyLock::new(|| {
    "BPFLoaderUpgradeab1e11111111111111111111111"
        .parse()
        .expect("valid BPF upgradeable loader pubkey")
});

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

    let mut actions = vec![get_spread_action(template, cli.spread_size)?];
    actions.extend(get_depth_actions(template, cli.depth_min)?);

    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(program_addr)])
                .actions(actions)
                .build(),
        )
        .await?;

    eprintln!("[ws] session: {}", session.session_id().unwrap_or("?"));
    session.ensure_ready(Some(Duration::from_secs(600))).await?;
    eprintln!("[ws] ready — scanning for {program_addr} batches");

    // Stream scheduled-action results while we scan for upgrade batches. Discovery
    // correlates results with the upgrade slots it finds, so it buffers the events
    // and processes them after the scan (once `upgrade_slots` is complete).
    let (sub, mut rx) = subscribe_action_results(&session, &cli.url).await?;
    // Slots where the program was upgraded; use to filter action results below.
    let mut upgrade_slots: Vec<u64> = Vec::new();

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
                    .any(|t| is_program_upgrade(&t.transaction, &program_addr))
                {
                    continue;
                }

                upgrade_slots.push(slot);
            }
            DiscoveryStepResult::Completed => {
                eprintln!("[done] session completed; total pauses: {pause_count}");
                break;
            }
        }
    }

    // Stop the subscription and drain the buffered results (the dropped sender
    // closes the channel, ending the loop).
    sub.stop.send(true).ok();
    sub.join_handle.await.ok();
    let mut notifications = Vec::new();
    while let Some(n) = rx.recv().await {
        notifications.push(n);
    }
    let _ = session.close(Some(Duration::from_secs(10))).await;
    eprintln!(
        "[actions] collected {} action results across {} upgrade slots",
        notifications.len(),
        upgrade_slots.len()
    );

    // TODO(@ygao): process `notifications` (filtered to `upgrade_slots`) into records.
    let _ = (&mut spread_records, &mut depth_records);
    Ok((spread_records, depth_records))
}

async fn run_regular_session(
    client: BacktestClient,
    cli: &Cli,
    template: &Template,
) -> Result<(Vec<SpreadRecord>, Vec<DepthRecord>)> {
    let timeout = Some(Duration::from_secs(120));

    let mut actions = vec![get_spread_action(template, cli.spread_size)?];
    actions.extend(get_depth_actions(template, cli.depth_min)?);

    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(cli.start_slot)
                .end_slot(cli.end_slot)
                .disconnect_timeout_secs(900u16)
                .capacity_wait_timeout_secs(900u16)
                .actions(actions)
                .build(),
        )
        .await?;

    session.ensure_ready(Some(Duration::from_secs(600))).await?;

    // Subscribe before advancing, then process action results as they stream in,
    // concurrently with the advance loop.
    let (sub, rx) = subscribe_action_results(&session, &cli.url).await?;
    let spread_size = cli.spread_size;
    let max_impact_bps = cli.max_impact_bps;
    let processor = tokio::spawn(async move {
        let mut rx = rx;
        let mut spread_records: Vec<SpreadRecord> = Vec::new();
        let mut depth_records: BTreeMap<u64, DepthRecord> = BTreeMap::new();
        while let Some(notification) = rx.recv().await {
            let slot = notification.slot;
            let Some((label, size)) = notification.label.as_deref().and_then(Label::parse) else {
                continue;
            };

            match label {
                Label::Spread => {
                    if let Some(spread) = SpreadRecord::new(&notification, spread_size) {
                        spread_records.push(spread);
                    } else {
                        eprintln!("Unable to parse spread notification for slot {slot}");
                    }
                }
                Label::Depth(_direction) => {
                    // TODO(@ygao): accumulate depth sweep points per slot/direction
                    // and reduce each sweep to its deepest point within
                    // `max_impact_bps`, inserting into `depth_records`.
                    let _ = (size, max_impact_bps);
                }
            }
        }
        (
            spread_records,
            depth_records.into_values().collect::<Vec<_>>(),
        )
    });

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
    let (spread_records, depth_records) = processor.await.context("action processor panicked")?;
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
