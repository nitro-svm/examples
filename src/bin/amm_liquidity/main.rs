//! Measures the bid-ask spread and depth of a single-venue prop AMM by simulating
//! against frozen historical chain state.
//!
//! ## Spread methodology
//!
//! Round-trip against the same frozen state.
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

use std::sync::LazyLock;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_api::{AccountModifications, ContinueParams};
use simulator_client::{
    CreateSession, ManagedBacktestSession, ManagedEvent, ManagedSessionError, backtest_ws_url,
};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

use backtest_example::utils::block::get_block_time;
use backtest_example::utils::parse::{
    SPCX_MINT, USDC_MINT, USDT_MINT, WSOL_MINT, derive_ata, extract_signer,
    get_titan_template_transaction, patch_titan_disable_positive_slippage_fee,
    patch_titan_single_venue,
};
use backtest_example::utils::price::{Ticker, get_historical_binance_price_usdc};
use backtest_example::utils::types::TitanVenueDiscriminant;

mod action;
mod depth;
mod spread;
mod template;

use action::{ActionCoordinator, VenueProcessor};
use template::{
    titan_goonfiv2_sol_usdt_template_v3, titan_multi_sol_usdc_template_v3,
    titan_multi_spcx_usdc_template_v3, titan_tessera_sol_usdc_template_v3,
};

/// Fallback USDC/base price for sizing the b2q depth sweep when we can't fetch a
/// real one (no tracked Binance ticker for the base mint, or the fetch fails).
const FALLBACK_BASE_PRICE_USDC: u64 = 100;

// ── configuration ──────────────────────────────────────────────────────────────

/// Everything needed to run a spread/depth measurement, decoupled from the CLI so
/// library callers can drive it directly. Field defaults match the CLI defaults.
#[derive(Clone, Debug)]
pub struct MeasurementConfig {
    /// Simulator host (no scheme); used for both the `wss://…/backtest` control
    /// plane and the `https://…` RPC data plane.
    pub url: String,
    /// Simulator API key.
    pub api_key: String,
    pub start_slot: u64,
    pub end_slot: u64,
    /// Explicit spread CSV path; only honored for single-venue runs. Multi-venue
    /// runs always auto-name per venue + slot range so files can't collide.
    pub spread_output: Option<String>,
    /// Explicit depth CSV path; single-venue only (see `spread_output`).
    pub depth_output: Option<String>,
    pub enable_intra_block_inspection: bool,
    pub quote_mint: String,
    pub base_mint: String,
    /// Spread measurement size in base native units.
    pub spread_size: u64,
    /// Smallest size for the depth sweep (quote-mint native units).
    pub depth_min: u64,
    /// Stop the depth sweep once price impact exceeds this many bps.
    pub max_impact_bps: u64,
    /// Titan venue discriminant(s) to isolate; all are measured in one session.
    pub venue_discriminants: Vec<u8>,
}

impl Default for MeasurementConfig {
    fn default() -> Self {
        Self {
            url: "staging.simulator.termina.technology".to_string(),
            api_key: String::new(),
            start_slot: 422_818_048,
            end_slot: 422_818_148,
            spread_output: None,
            depth_output: None,
            enable_intra_block_inspection: false,
            quote_mint: USDC_MINT.to_string(),
            base_mint: WSOL_MINT.to_string(),
            spread_size: 50_000_000_000,
            depth_min: 10_000_000,
            max_impact_bps: 1000,
            venue_discriminants: vec![TitanVenueDiscriminant::BisonFi as u8],
        }
    }
}

// ── template ─────────────────────────────────────────────────────────────────

pub(crate) struct Template {
    quote_to_base: VersionedTransaction,
    base_to_quote: VersionedTransaction,
    quote_mint: Address,
    base_mint: Address,
    quote_signer: Pubkey,
    base_signer: Pubkey,
    quote_receiver: Pubkey,
    base_receiver: Pubkey,
}

/// Pick the `(spend-quote-get-base, spend-base-get-quote)` template signature pair whose
/// swap_route_v3 body contains `venue`, keyed by the quote and base mints.
///
/// USDC/SOL: Tessera isn't co-listed with the prop venues in any SOL/USDC route, so it
/// gets its own single-venue template; every other venue uses the multi-venue bison template.
///
/// USDT/SOL: only one native SOL/USDT route exists on-chain (HumidiFi/GoonFiV2/BisonFi meshed);
/// `patch_titan_single_venue` isolates whichever of those `venue` names out of it. Requesting
/// a venue that route doesn't contain (e.g. Tessera, which has no on-chain SOL/USDT market)
/// surfaces later as a "venue not in swaps" error from the isolation step.
///
/// USDC/SPCX: only GoonFiV2 and ZeroFi have an on-chain SPCX/USDC route.
///
/// Anything else falls back to the SOL/USDC bison template, which only makes sense when
/// base is actually WSOL.
fn template_signatures(
    venue: TitanVenueDiscriminant,
    quote_mint: &str,
    base_mint: &str,
) -> (&'static str, &'static str) {
    match (venue, quote_mint, base_mint) {
        (TitanVenueDiscriminant::GoonFiV2, USDT_MINT, WSOL_MINT) => (
            titan_goonfiv2_sol_usdt_template_v3::USDT_TO_SOL,
            titan_goonfiv2_sol_usdt_template_v3::SOL_TO_USDT,
        ),
        (
            TitanVenueDiscriminant::GoonFiV2 | TitanVenueDiscriminant::ZeroFi,
            USDC_MINT,
            SPCX_MINT,
        ) => (
            titan_multi_spcx_usdc_template_v3::USDC_TO_SPCX,
            titan_multi_spcx_usdc_template_v3::SPCX_TO_USDC,
        ),
        (TitanVenueDiscriminant::Tessera, USDC_MINT, WSOL_MINT) => (
            titan_tessera_sol_usdc_template_v3::USDC_TO_SOL,
            titan_tessera_sol_usdc_template_v3::SOL_TO_USDC,
        ),
        (_, _, _) => (
            titan_multi_sol_usdc_template_v3::USDC_TO_SOL,
            titan_multi_sol_usdc_template_v3::SOL_TO_USDC,
        ),
    }
}

async fn get_template(
    venue: TitanVenueDiscriminant,
    quote_mint: &str,
    base_mint: &str,
) -> Result<Template> {
    let (quote_to_base_sig, base_to_quote_sig) = template_signatures(venue, quote_mint, base_mint);
    let quote_to_base = get_titan_template_transaction(quote_to_base_sig).await?;
    let base_to_quote = get_titan_template_transaction(base_to_quote_sig).await?;
    let quote_signer = extract_signer(&quote_to_base)?;
    let base_signer = extract_signer(&base_to_quote)?;
    // q2b (quote->base) output lands in the quote signer's ATA for the base mint.
    let quote_receiver =
        derive_ata(&quote_signer, base_mint).context("derive q2b base receiver")?;
    // b2q (base->quote) output lands in the base signer's ATA for the quote mint (USDC or USDT).
    let base_receiver =
        derive_ata(&base_signer, quote_mint).context("derive b2q quote receiver")?;

    let quote_to_base = patch_titan_disable_positive_slippage_fee(&patch_titan_single_venue(
        &quote_to_base,
        &venue,
    )?)?;
    let base_to_quote = patch_titan_disable_positive_slippage_fee(&patch_titan_single_venue(
        &base_to_quote,
        &venue,
    )?)?;

    Ok(Template {
        quote_to_base,
        base_to_quote,
        quote_mint: quote_mint.parse().context("parse quote mint")?,
        base_mint: base_mint.parse().context("parse base mint")?,
        quote_signer,
        base_signer,
        quote_receiver,
        base_receiver,
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

/// Drive a single managed session to completion, routing every action result to
/// `action_processor` as it arrives.
///
/// The [`ManagedBacktestSession`] folds control-plane advancing and the action
/// subscription into one `next_event()` stream, and handles reconnect on both:
/// a dropped socket resumes via the server's `replayFromSlot` cursor (with
/// replayed events de-duplicated) rather than aborting the run. That replaces the
/// previous split of a manual advance loop plus a one-shot action subscription,
/// neither of which survived a websocket drop.
async fn run_session(
    config: &MeasurementConfig,
    mut action_processor: ActionCoordinator,
) -> Result<ActionCoordinator> {
    let actions = action_processor.get_actions()?;
    eprintln!("[dbg] registering {} actions", actions.len());

    let create = CreateSession::builder()
        .start_slot(config.start_slot)
        .end_slot(config.end_slot)
        .disconnect_timeout_secs(900u16)
        .capacity_wait_timeout_secs(900u16)
        .actions(actions)
        .build()
        .into_request()
        .context("building create-session request")?;

    let ws_url = backtest_ws_url(&config.url);
    let mut session = ManagedBacktestSession::start(ws_url, config.api_key.clone(), create)
        .await
        .context("starting managed session")?;

    // Actions are registered in the create request; this attaches the result
    // consumer that feeds `handle_action_result` below.
    session.subscribe_actions();

    let advance_count = config.end_slot - config.start_slot;
    loop {
        match session.next_event().await {
            // The server is ready for another `Continue`; advance the whole range.
            // Re-issued on each `ReadyForContinue` in case the range spans several.
            Ok(ManagedEvent::ReadyForContinue) => {
                let params = ContinueParams {
                    advance_count,
                    transactions: Vec::new(),
                    modify_account_states: AccountModifications(Default::default()),
                };
                session
                    .send_continue(params)
                    .await
                    .context("send_continue")?;
            }
            Ok(ManagedEvent::ActionResult(notification)) => {
                action_processor.handle_action_result(notification);
            }
            // Trailing action results are drained and delivered before `Completed`,
            // so by here every result has already been routed.
            Ok(ManagedEvent::Completed { .. }) => break,
            Ok(ManagedEvent::Error(e)) => {
                session.shutdown().await;
                anyhow::bail!("simulator error: {e}");
            }
            // Progress/among-others events we don't act on (no discovery pacing,
            // and we don't subscribe to tx/account-diff streams here).
            Ok(_) => {}
            Err(ManagedSessionError::Cancelled) => break,
            Err(e) => {
                session.shutdown().await;
                return Err(anyhow::anyhow!("session failed: {e}"));
            }
        }
    }

    session.shutdown().await;
    Ok(action_processor)
}

// ── public entry point ──────────────────────────────────────────────────────────

/// Run a full spread/depth measurement across every configured venue, streaming
/// results to their CSV files. This is exactly what the CLI binary does.
pub async fn run_measurement(config: &MeasurementConfig) -> Result<()> {
    eprintln!("[ws] connecting to {}", backtest_ws_url(&config.url));

    // Explicit spread_output/depth_output only apply to single-venue runs; multi-venue
    // runs always auto-name (per venue + slot range) so the files can't collide.
    let single_venue = config.venue_discriminants.len() == 1;

    // Base/USDC price at the start of the replay range, used to size the b2q depth
    // sweep to the same USD notional as q2b. Only fetched when we track a Binance
    // ticker for the configured base mint (e.g. WSOL, SPCX); if that ticker isn't
    // reachable (e.g. Binance's geo restrictions, or the pair isn't listed there)
    // or we don't track one at all, fall back to `FALLBACK_BASE_PRICE_USDC` rather
    // than aborting the run.
    let base_price_usdc = match Ticker::from_mint(&config.base_mint) {
        Some(ticker) => {
            let fetch = async {
                let start_block_time = get_block_time(config.start_slot).await?;
                get_historical_binance_price_usdc(ticker, start_block_time).await
            };
            match fetch.await {
                Ok(price) => {
                    eprintln!(
                        "[dbg] {} price at slot {}: {price}",
                        config.base_mint, config.start_slot
                    );
                    price
                }
                Err(e) => {
                    eprintln!(
                        "[warn] couldn't fetch {} price ({e}); falling back to {FALLBACK_BASE_PRICE_USDC} USDC/base for depth sweep sizing",
                        config.base_mint
                    );
                    FALLBACK_BASE_PRICE_USDC
                }
            }
        }
        None => FALLBACK_BASE_PRICE_USDC,
    };

    let mut processors = Vec::with_capacity(config.venue_discriminants.len());
    for &disc in &config.venue_discriminants {
        let venue = TitanVenueDiscriminant::from_u8(disc)?;
        let name = venue.name();
        let program_id = config
            .enable_intra_block_inspection
            .then(|| venue.get_program_id());
        let template = get_template(venue, &config.quote_mint, &config.base_mint).await?;

        let spread_file = config
            .spread_output
            .clone()
            .filter(|_| single_venue)
            .unwrap_or_else(|| {
                format!(
                    "spread_{name}_{}_{}.csv",
                    config.start_slot, config.end_slot
                )
            });
        let depth_file = config
            .depth_output
            .clone()
            .filter(|_| single_venue)
            .unwrap_or_else(|| {
                format!("depth_{name}_{}_{}.csv", config.start_slot, config.end_slot)
            });

        processors.push(VenueProcessor::new(
            venue,
            config.spread_size,
            config.depth_min,
            config.max_impact_bps,
            program_id,
            template,
            &spread_file,
            &depth_file,
            &config.quote_mint,
            &config.base_mint,
            config.start_slot,
            config.end_slot,
            base_price_usdc,
        )?);
    }

    let coordinator = run_session(config, ActionCoordinator::new(processors)).await?;
    coordinator.finish()?;

    Ok(())
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

    /// Spread CSV path. Defaults to `spread_<venue>.csv` so runs don't clobber each other.
    #[arg(long)]
    spread_output: Option<String>,

    /// Depth CSV path. Defaults to `depth_<venue>.csv` so runs don't clobber each other.
    #[arg(long)]
    depth_output: Option<String>,

    #[arg(long, default_value_t = false)]
    enable_intra_block_inspection: bool,

    #[arg(long, default_value = USDC_MINT)]
    quote_mint: String,

    #[arg(long, default_value = WSOL_MINT)]
    base_mint: String,

    /// Spread measurement size in base native units.
    #[arg(long, default_value_t = 50_000_000_000)]
    spread_size: u64,

    /// Smallest size for the depth sweep (quote-mint native units).
    #[arg(long, default_value_t = 1_000_000_000)]
    depth_min: u64,

    /// Stop the depth sweep once price impact exceeds this many bps.
    #[arg(long, default_value_t = 1000)]
    max_impact_bps: u64,

    /// Titan Venue discriminant(s) to isolate, comma-separated or repeated
    /// All listed venues are measured in the same simulator session.
    /// 55 = BisonFi, 13 = ZeroFi, 28 = HumidiFi, 57 = GoonFiV2, 23 = Tessera.
    /// See the Venue enum in the Titan IDL.
    #[arg(long = "venue-discriminant", value_delimiter = ',', default_value = "55", num_args = 1..)]
    venue_discriminants: Vec<u8>,
}

impl From<Cli> for MeasurementConfig {
    fn from(cli: Cli) -> Self {
        MeasurementConfig {
            url: cli.url,
            api_key: cli.api_key,
            start_slot: cli.start_slot,
            end_slot: cli.end_slot,
            spread_output: cli.spread_output,
            depth_output: cli.depth_output,
            enable_intra_block_inspection: cli.enable_intra_block_inspection,
            quote_mint: cli.quote_mint,
            base_mint: cli.base_mint,
            spread_size: cli.spread_size,
            depth_min: cli.depth_min,
            max_impact_bps: cli.max_impact_bps,
            venue_discriminants: cli.venue_discriminants,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();
    // Validate discriminants up front so a bad value fails before opening a session.
    for &disc in &cli.venue_discriminants {
        TitanVenueDiscriminant::from_u8(disc)?;
    }

    run_measurement(&MeasurementConfig::from(cli)).await
}
