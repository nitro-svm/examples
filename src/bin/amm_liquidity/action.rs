use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;
use simulator_api::ScheduledAction;
use simulator_client::ActionResultNotification;
use solana_account_decoder::UiAccount;
use solana_address::Address;

use crate::utils::accounts::native_seed_lamports;

use super::Template;
use super::TitanVenueDiscriminant;
use super::depth::{DEPTH_SIGNER_LAMPORTS, Depth, DepthStore, get_depth_actions};
use super::spread::{Spread, SpreadStore, get_spread_action};

/// Read the SPL token `amount` from a returned `UiAccount` JSON value.
pub(crate) fn token_amount(account: &serde_json::Value) -> Option<u64> {
    let data = UiAccount::deserialize(account).ok()?.data.decode()?;
    let amount = data.get(64..72)?;
    Some(u64::from_le_bytes(amount.try_into().ok()?))
}

/// Read the native lamport balance from a returned `UiAccount` JSON value.
pub(crate) fn native_lamports(account: &serde_json::Value) -> Option<u64> {
    Some(UiAccount::deserialize(account).ok()?.lamports)
}

// Action labels are `{venue_disc}-{kind}` so one session can measure several venues at
// once and route each result back to the venue that produced it.
const SPREAD_LABEL: &str = "spread";
const DEPTH_Q2B_PREFIX: &str = "depth-q2b-";
const DEPTH_B2Q_PREFIX: &str = "depth-b2q-";

pub(crate) fn spread_label(venue: TitanVenueDiscriminant) -> String {
    format!("{}-{SPREAD_LABEL}", venue as u8)
}
pub(crate) fn depth_q2b_label(venue: TitanVenueDiscriminant, size: u64) -> String {
    format!("{}-{DEPTH_Q2B_PREFIX}{size}", venue as u8)
}
pub(crate) fn depth_b2q_label(venue: TitanVenueDiscriminant, size: u64) -> String {
    format!("{}-{DEPTH_B2Q_PREFIX}{size}", venue as u8)
}

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
    /// Parse a `{venue}-{kind}` label into `(venue_disc, label, sweep_size)`.
    pub(crate) fn parse(name: &str) -> Option<(TitanVenueDiscriminant, Self, u64)> {
        let (venue, rest) = name.split_once('-')?;
        let discriminant: u8 = venue.parse().ok()?;
        let venue = TitanVenueDiscriminant::from_u8(discriminant).ok()?;
        if rest == SPREAD_LABEL {
            return Some((venue, Self::Spread, 0));
        }
        if let Some(size) = rest.strip_prefix(DEPTH_Q2B_PREFIX) {
            return Some((
                venue,
                Self::Depth(DepthDirection::QuoteToBase),
                size.parse().ok()?,
            ));
        }
        if let Some(size) = rest.strip_prefix(DEPTH_B2Q_PREFIX) {
            return Some((
                venue,
                Self::Depth(DepthDirection::BaseToQuote),
                size.parse().ok()?,
            ));
        }

        None
    }
}

/// Spread + depth measurement for a single venue:
/// owns its patched template, output stores, and intra-block program filter.
pub(crate) struct VenueProcessor {
    venue: TitanVenueDiscriminant,
    spread_size: u64,
    depth_min: u64,
    program_id: Option<Address>,
    template: Template,
    spread_records: SpreadStore,
    depth_records: DepthStore,
}

impl VenueProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        venue: TitanVenueDiscriminant,
        spread_size: u64,
        depth_min: u64,
        max_impact_bps: u64,
        program_id: Option<Address>,
        template: Template,
        spread_file: &str,
        depth_file: &str,
        quote_mint: &str,
        base_mint: &str,
    ) -> Result<Self> {
        let intra_block_inspection_enabled = program_id.is_some();

        Ok(Self {
            venue,
            spread_size,
            depth_min,
            program_id,
            template,
            spread_records: SpreadStore::new(spread_file, quote_mint, base_mint)?,
            depth_records: DepthStore::new(
                max_impact_bps,
                intra_block_inspection_enabled,
                depth_file,
                quote_mint,
                base_mint,
            )?,
        })
    }

    /// Build this venue's spread + depth scheduled actions, tagged with its discriminant.
    fn get_actions(&self) -> Result<Vec<ScheduledAction>> {
        // let mut actions = vec![get_spread_action(
        //     &self.template,
        //     self.spread_size,
        //     self.program_id,
        //     self.venue,
        // )?];
        let mut actions = vec![];
        actions.extend(get_depth_actions(
            &self.template,
            self.depth_min,
            self.program_id,
            self.venue,
        )?);
        Ok(actions)
    }

    /// Record one parsed action result into this venue's spread/depth stores.
    ///
    /// Output is read from the returned output account balance.
    /// `patch_titan_template_transaction` zeroes `fee_centi_bps`, so no Titan routing fee is taken.
    fn record(
        &mut self,
        slot: u64,
        label: Label,
        size: u64,
        action_index: u32,
        accounts: &[Option<serde_json::Value>],
    ) {
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
                if let Err(e) = self.spread_records.push(spread) {
                    eprintln!(
                        "failed to write spread row for venue {} slot {slot}: {e}",
                        self.venue as u8
                    );
                }
            }
            Label::Depth(direction) => {
                let out_account = accounts.first().and_then(|a| a.as_ref());
                let out_amount = match direction {
                    // q2b's SOL output is unwrapped to native SOL (subtract the seeded balance).
                    DepthDirection::QuoteToBase => out_account
                        .and_then(native_lamports)
                        .map(|l| l.saturating_sub(native_seed_lamports(DEPTH_SIGNER_LAMPORTS)))
                        .unwrap_or(0),
                    // b2q's USDC output is in a receiver ATA with no baseline balance.
                    DepthDirection::BaseToQuote => out_account.and_then(token_amount).unwrap_or(0),
                };
                let depth = Depth::new(size, out_amount);
                if let Err(e) = self.depth_records.add(slot, direction, action_index, depth) {
                    eprintln!(
                        "failed to write depth row for venue {} slot {slot}: {e}",
                        self.venue as u8
                    );
                }
            }
        }
    }

    fn finish(self) -> Result<()> {
        self.spread_records.finish()?;
        self.depth_records.finish()?;
        Ok(())
    }
}

/// Routes action results from a shared session to the right [`VenueProcessor`] by the
/// venue discriminant encoded in each action label.
pub(crate) struct ActionCoordinator {
    venues: HashMap<TitanVenueDiscriminant, VenueProcessor>,
}

impl ActionCoordinator {
    pub(crate) fn new(venues: Vec<VenueProcessor>) -> Self {
        Self {
            venues: venues.into_iter().map(|v| (v.venue, v)).collect(),
        }
    }

    /// Every venue's spread + depth actions, registered together in one session.
    pub(crate) fn get_actions(&self) -> Result<Vec<ScheduledAction>> {
        let mut actions = Vec::new();
        for venue in self.venues.values() {
            actions.extend(venue.get_actions()?);
        }
        Ok(actions)
    }

    /// Route one scheduled-action result to its venue's stores, keyed by the
    /// discriminant encoded in the action label. Called inline by the managed
    /// session's event loop as each `ManagedEvent::ActionResult` arrives.
    pub(crate) fn handle_action_result(&mut self, notification: ActionResultNotification) {
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
            return;
        }

        let Some((venue, label, size)) = label.as_deref().and_then(Label::parse) else {
            return;
        };

        match self.venues.get_mut(&venue) {
            Some(processor) => processor.record(slot, label, size, action_index, &accounts),
            None => eprintln!(
                "result for unregistered venue {} at slot {slot}",
                venue as u8
            ),
        }
    }

    /// Flush any buffered tail and report totals for every venue. Rows are already
    /// written incrementally as results arrive, so this only finalizes the files.
    pub(crate) fn finish(self) -> Result<()> {
        for venue in self.venues.into_values() {
            venue.finish()?;
        }
        Ok(())
    }
}
