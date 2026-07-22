use anyhow::{Context, Result};
use serde::Deserialize;
use simulator_api::ScheduledAction;
use simulator_client::{
    ActionResultNotification, ActionSubscriptionHandle, BacktestSession, subscribe_actions,
};
use solana_account_decoder::UiAccount;
use solana_address::Address;
use tokio::sync::mpsc::UnboundedReceiver;

use backtest_example::utils::accounts::native_seed_lamports;

use crate::Template;
use crate::depth::{DEPTH_SIGNER_LAMPORTS, Depth, DepthStore, get_depth_actions};
use crate::resolve_url;
use crate::spread::{Spread, SpreadStore, get_spread_action};

/// Read the SPL token `amount` from a returned `UiAccount` JSON value.
/// Deserializes the account envelope and decodes its data, then reads the
/// token `amount` at the SPL token account's `[64..72]` byte range.
pub(crate) fn token_amount(account: &serde_json::Value) -> Option<u64> {
    let data = UiAccount::deserialize(account).ok()?.data.decode()?;
    let amount = data.get(64..72)?;
    Some(u64::from_le_bytes(amount.try_into().ok()?))
}

/// Read the native lamport balance from a returned `UiAccount` JSON value.
pub(crate) fn native_lamports(account: &serde_json::Value) -> Option<u64> {
    Some(UiAccount::deserialize(account).ok()?.lamports)
}

/// Label tagged on the spread [`ScheduledAction`](simulator_api::ScheduledAction).
pub(crate) const SPREAD_LABEL: &str = "spread";
/// Prefixs for depth action labels; followed by the sweep size.
pub(crate) const DEPTH_Q2B_PREFIX: &str = "depth-q2b-";
pub(crate) const DEPTH_B2Q_PREFIX: &str = "depth-b2q-";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DepthDirection {
    QuoteToBase,
    BaseToQuote,
}

pub(crate) enum Label {
    Spread,
    Depth(DepthDirection),
}

impl Label {
    pub(crate) fn parse(name: &str) -> Option<(Self, u64)> {
        if name == SPREAD_LABEL {
            return Some((Self::Spread, 0));
        }
        if let Some(size) = name.strip_prefix(DEPTH_Q2B_PREFIX) {
            return Some((Self::Depth(DepthDirection::QuoteToBase), size.parse().ok()?));
        }
        if let Some(size) = name.strip_prefix(DEPTH_B2Q_PREFIX) {
            return Some((Self::Depth(DepthDirection::BaseToQuote), size.parse().ok()?));
        }

        None
    }
}

pub(crate) async fn subscribe_action_results(
    session: &BacktestSession,
    url: &str,
) -> Result<(
    ActionSubscriptionHandle,
    UnboundedReceiver<ActionResultNotification>,
)> {
    let rpc_endpoint = session
        .rpc_endpoint()
        .context("session has no rpc endpoint")?;
    let rpc_url = resolve_url(&format!("https://{}", url), rpc_endpoint);
    eprintln!("[dbg] rpc_endpoint={rpc_endpoint:?} -> rpc_url={rpc_url}");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = subscribe_actions(&rpc_url, move |result| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(result);
        }
    })
    .await?;

    Ok((handle, rx))
}

pub(crate) struct ActionProcessor {
    spread_size: u64,
    depth_min: u64,
    program_id: Option<Address>,
    spread_records: SpreadStore,
    depth_records: DepthStore,
}

impl ActionProcessor {
    pub(crate) fn new(
        spread_size: u64,
        depth_min: u64,
        max_impact_bps: u64,
        program_id: Option<Address>,
    ) -> Self {
        let intra_block_inspection_enabled = program_id.is_some();

        Self {
            spread_size,
            depth_min,
            program_id,
            spread_records: SpreadStore::new(),
            depth_records: DepthStore::new(max_impact_bps, intra_block_inspection_enabled),
        }
    }

    /// Build the spread + depth scheduled actions for this run, borrowing the
    /// template (the server runs them automatically; no ownership needed here).
    /// `start_slot`/`end_slot` bound the `AfterSlot`-anchored actions, which must
    /// enumerate every slot they should fire at.
    pub(crate) fn get_actions(
        &self,
        template: &Template,
        start_slot: u64,
        end_slot: u64,
    ) -> Result<Vec<ScheduledAction>> {
        let mut actions = vec![get_spread_action(
            template,
            self.spread_size,
            self.program_id,
            start_slot,
            end_slot,
        )?];
        actions.extend(get_depth_actions(
            template,
            self.depth_min,
            self.program_id,
            start_slot,
            end_slot,
        )?);
        Ok(actions)
    }

    /// Consume scheduled-action results from the subscription channel until it closes,
    /// parsing each into spread/depth records. Runs concurrently with the advance loop;
    /// returns once the subscription's sender is dropped and the channel drains.
    pub(crate) async fn parse_events(
        mut self,
        mut rx: UnboundedReceiver<ActionResultNotification>,
    ) -> Self {
        while let Some(notification) = rx.recv().await {
            let ActionResultNotification {
                slot,
                accounts,
                label,
                transaction_outcomes,
                action_index,
                ..
            } = notification;

            if let Some(err) = transaction_outcomes.first().and_then(|o| o.err.as_ref()) {
                eprintln!("Action transaction failed for slot {slot} (label={label:?}): {err}");
                continue;
            }

            let Some((label, depth_size)) = label.as_deref().and_then(Label::parse) else {
                continue;
            };

            match label {
                Label::Spread => {
                    // Round-trip SOL->USDC->SOL: the final output is unwrapped to native SOL
                    // (subtract the full seeded balance).
                    let out_amount = accounts
                        .first()
                        .and_then(|a| a.as_ref())
                        .and_then(native_lamports)
                        .map(|l| l.saturating_sub(native_seed_lamports(DEPTH_SIGNER_LAMPORTS)))
                        .unwrap_or(0);
                    let spread = Spread::new(slot, self.spread_size, out_amount);
                    self.spread_records.push(spread);
                }
                Label::Depth(direction) => {
                    let out_account = accounts.first().and_then(|a| a.as_ref());
                    let out_amount = match direction {
                        // q2b's SOL output is unwrapped to native SOL
                        // (subtract the full seeded balance).
                        DepthDirection::QuoteToBase => out_account
                            .and_then(native_lamports)
                            .map(|l| l.saturating_sub(native_seed_lamports(DEPTH_SIGNER_LAMPORTS)))
                            .unwrap_or(0),
                        // b2q's USDC output is in a receiver ATA with no baseline balance.
                        DepthDirection::BaseToQuote => {
                            out_account.and_then(token_amount).unwrap_or(0)
                        }
                    };
                    let depth = Depth::new(depth_size, out_amount);
                    self.depth_records.add(slot, direction, action_index, depth);
                }
            }
        }

        self
    }

    pub(crate) fn write_output(
        &self,
        spread_file: &str,
        depth_file: &str,
        quote_mint: &str,
        base_mint: &str,
    ) -> Result<()> {
        self.spread_records
            .write_output(spread_file, quote_mint, base_mint)?;
        self.depth_records
            .write_output(depth_file, quote_mint, base_mint)?;

        Ok(())
    }
}
