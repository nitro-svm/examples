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

use crate::utils::parse::{
    USDC_MINT, USDT_MINT, WSOL_MINT, derive_ata, extract_signer, get_titan_template_transaction,
    patch_titan_disable_positive_slippage_fee, patch_titan_single_venue,
};

use std::sync::LazyLock;

use anyhow::{Context, Result};
use simulator_api::{AccountModifications, ContinueParams};
use simulator_client::{
    CreateSession, ManagedBacktestSession, ManagedEvent, ManagedSessionError, backtest_ws_url,
};
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_transaction::versioned::VersionedTransaction;

mod action;
mod depth;
mod spread;
mod template;

use action::{ActionCoordinator, VenueProcessor};
use template::{
    titan_goonfiv2_sol_usdt_template_v3, titan_multi_sol_usdc_template_v3,
    titan_tessera_sol_usdc_template_v3,
};

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

// ── venue identifiers ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
#[repr(u8)]
pub enum TitanVenueDiscriminant {
    ZeroFi = 13,
    HumidiFi = 28,
    GoonFi = 35,
    BisonFi = 55,
    GoonFiV2 = 57,
    Tessera = 23,
}

impl TitanVenueDiscriminant {
    pub fn from_u8(disc: u8) -> Result<Self> {
        match disc {
            13 => Ok(Self::ZeroFi),
            23 => Ok(Self::Tessera),
            28 => Ok(Self::HumidiFi),
            35 => Ok(Self::GoonFi),
            55 => Ok(Self::BisonFi),
            57 => Ok(Self::GoonFiV2),
            other => anyhow::bail!("unknown venue discriminant: {other}"),
        }
    }

    /// Short lowercase venue name, used for default output filenames.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ZeroFi => "zerofi",
            Self::HumidiFi => "humidifi",
            Self::GoonFi => "goonfi",
            Self::BisonFi => "bisonfi",
            Self::GoonFiV2 => "goonfiv2",
            Self::Tessera => "tessera",
        }
    }

    pub fn get_program_id(self) -> Address {
        let program_id = match self {
            Self::ZeroFi => "ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY",
            Self::HumidiFi => "9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp",
            Self::GoonFi => "goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j",
            Self::BisonFi => "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi",
            Self::GoonFiV2 => "goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE",
            Self::Tessera => "TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH",
        };

        Address::from_str_const(program_id)
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

/// Pick the `(quote->SOL, SOL->quote)` template signature pair whose swap_route_v3 body
/// contains `venue`, keyed by the quote mint.
///
/// USDC: Tessera isn't co-listed with the prop venues in any SOL/USDC route, so it gets
/// its own single-venue template; every other venue uses the multi-venue bison template.
///
/// USDT: only one native SOL/USDT route exists on-chain (HumidiFi/GoonFiV2/BisonFi meshed);
/// `patch_titan_single_venue` isolates whichever of those `venue` names out of it. Requesting
/// a venue that route doesn't contain (e.g. Tessera, which has no on-chain SOL/USDT market)
/// surfaces later as a "venue not in swaps" error from the isolation step.
fn template_signatures(
    venue: TitanVenueDiscriminant,
    quote_mint: &str,
) -> (&'static str, &'static str) {
    if venue == TitanVenueDiscriminant::GoonFiV2 && quote_mint == USDT_MINT {
        return (
            titan_goonfiv2_sol_usdt_template_v3::USDT_TO_SOL,
            titan_goonfiv2_sol_usdt_template_v3::SOL_TO_USDT,
        );
    } else if venue == TitanVenueDiscriminant::Tessera && quote_mint == USDC_MINT {
        return (
            titan_tessera_sol_usdc_template_v3::USDC_TO_SOL,
            titan_tessera_sol_usdc_template_v3::SOL_TO_USDC,
        );
    } else {
        return (
            titan_multi_sol_usdc_template_v3::USDC_TO_SOL,
            titan_multi_sol_usdc_template_v3::SOL_TO_USDC,
        );
    }
}

async fn get_template(venue: TitanVenueDiscriminant, quote_mint: &str) -> Result<Template> {
    let (quote_to_sol_sig, sol_to_quote_sig) = template_signatures(venue, quote_mint);
    let quote_to_sol = get_titan_template_transaction(quote_to_sol_sig).await?;
    let sol_to_quote = get_titan_template_transaction(sol_to_quote_sig).await?;
    let quote_signer = extract_signer(&quote_to_sol)?;
    let base_signer = extract_signer(&sol_to_quote)?;
    let quote_receiver =
        derive_ata(&quote_signer, WSOL_MINT).context("derive q2b WSOL receiver")?;
    // b2q (SOL->quote) output lands in the base signer's ATA for the quote mint (USDC or USDT).
    let base_receiver =
        derive_ata(&base_signer, quote_mint).context("derive b2q quote receiver")?;

    let quote_to_base = patch_titan_disable_positive_slippage_fee(&patch_titan_single_venue(
        &quote_to_sol,
        &venue,
    )?)?;
    let base_to_quote = patch_titan_disable_positive_slippage_fee(&patch_titan_single_venue(
        &sol_to_quote,
        &venue,
    )?)?;

    Ok(Template {
        quote_to_base,
        base_to_quote,
        quote_mint: quote_mint.parse().context("parse quote mint")?,
        base_mint: Address::from_str_const(WSOL_MINT),
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

    let mut processors = Vec::with_capacity(config.venue_discriminants.len());
    for &disc in &config.venue_discriminants {
        let venue = TitanVenueDiscriminant::from_u8(disc)?;
        let name = venue.name();
        let program_id = config
            .enable_intra_block_inspection
            .then(|| venue.get_program_id());
        let template = get_template(venue, &config.quote_mint).await?;

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
        )?);
    }

    let coordinator = run_session(config, ActionCoordinator::new(processors)).await?;
    coordinator.finish()?;

    Ok(())
}
