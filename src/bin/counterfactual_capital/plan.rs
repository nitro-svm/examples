//! The plan file: the venue's routing identity, the accounts its capital and curve live in, and the checks a plan must pass before a run.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use simulator_api::DirectFillTemplate;
use solana_address::Address;

/// Where one ladder lives inside the venue's state account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LadderLayout {
    /// Byte offset of the tier count, read at `width` bytes.
    pub(crate) count: usize,
    /// Byte offset of tier 0. Each tier is `price` then `size`, each `width` bytes.
    pub(crate) entries: usize,
    /// Bytes from one tier to the next.
    pub(crate) stride: usize,
    /// Bytes per field. Both the count and each field are read at this width.
    pub(crate) width: usize,
}

/// The venue's state account: the ladders it quotes from, and the balances it mirrors.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StateLayout {
    /// Base58: [`Address`]'s own `Serialize` is the 32-byte array, which no hand-written plan uses.
    #[serde_as(as = "DisplayFromStr")]
    pub(crate) account: Address,
    /// Leading bytes the account must open with, checked before anything is written. Optional, but
    /// the only cheap defence against a plan aimed at the wrong account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) discriminator: Option<String>,
    /// Exact size the account must be, checked the same way and for the same reason.
    pub(crate) len: usize,
    /// Upper bound on a credible tier count, so a plan pointed at the wrong offset fails instead
    /// of scaling whatever integer it lands on.
    pub(crate) max_tiers: usize,
    /// Offsets of the u64 copies the venue keeps of each vault's balance, in vault order. Empty
    /// for a venue that keeps none.
    #[serde(default)]
    pub(crate) balance_mirrors: Vec<usize>,
    pub(crate) ladders: Vec<LadderLayout>,
}

/// Where a venue's capital lives.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Inventory {
    /// The token accounts the venue settles from, in the order the state account mirrors them.
    #[serde_as(as = "Vec<DisplayFromStr>")]
    pub(crate) vaults: Vec<Address>,
    /// Absent for a venue whose vaults *are* its curve, as a constant-product pool is. Present for
    /// one quoting from an explicit ladder, where scaling the vaults alone changes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) state: Option<StateLayout>,
}

/// A venue, described completely enough to run the counterfactual.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Plan {
    pub(crate) direct_fill: DirectFillTemplate,
    pub(crate) inventory: Inventory,
}

impl Plan {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading the plan at {}", path.display()))?;
        let plan: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parsing the plan at {}", path.display()))?;
        plan.validate()?;
        Ok(plan)
    }

    /// Every account this plan writes to, in the order arms post them.
    pub(crate) fn overridden(&self) -> Vec<Address> {
        self.inventory
            .vaults
            .iter()
            .copied()
            .chain(self.inventory.state.as_ref().map(|state| state.account))
            .collect()
    }

    /// Checks that each cost a whole replay to discover otherwise.
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.inventory.vaults.is_empty(),
            "a plan must name at least one vault, or there is no capital to scale"
        );
        for account in self.overridden() {
            let named = self
                .direct_fill
                .instruction
                .accounts
                .iter()
                .find(|entry| entry.address == account)
                .with_context(|| {
                    format!(
                        "{account} is not in the venue's account run, so a probe never loads it \
                         and scaling it would change nothing"
                    )
                })?;
            ensure!(
                named.writable,
                "{account} is read-only in the venue's account run; a venue that writes to it \
                 reverts at execution, so the run is mis-specified"
            );
        }
        let Some(state) = &self.inventory.state else {
            return Ok(());
        };
        ensure!(
            state.balance_mirrors.is_empty()
                || state.balance_mirrors.len() == self.inventory.vaults.len(),
            "the plan names {} balance mirrors for {} vaults; a mirror belongs to exactly one \
             vault, so name one per vault or none at all",
            state.balance_mirrors.len(),
            self.inventory.vaults.len()
        );
        ensure!(
            !state.ladders.is_empty(),
            "a state layout with no ladders describes nothing to scale; drop it and the vaults \
             alone will be scaled"
        );
        for (side, ladder) in state.ladders.iter().enumerate() {
            ensure!(
                ladder.width > 0 && ladder.width <= 16,
                "ladder {side} declares a {}-byte field width; 1..=16 is readable as an integer",
                ladder.width
            );
            ensure!(
                ladder.stride >= ladder.width * 2,
                "ladder {side}'s stride of {} cannot hold a {}-byte price and size",
                ladder.stride,
                ladder.width
            );
            // `max_tiers` is unbounded plan JSON: an unchecked multiply would wrap past the bound.
            ladder
                .stride
                .checked_mul(state.max_tiers)
                .and_then(|span| ladder.entries.checked_add(span))
                .filter(|end| *end <= state.len)
                .with_context(|| {
                    format!(
                        "ladder {side} runs past the account's {} bytes at {} tiers of {} from {}",
                        state.len, state.max_tiers, ladder.stride, ladder.entries
                    )
                })?;
            ensure!(
                ladder
                    .count
                    .checked_add(ladder.width)
                    .is_some_and(|end| end <= state.len),
                "ladder {side}'s tier count at byte {} does not fit in the account's {} bytes",
                ladder.count,
                state.len
            );
        }
        Ok(())
    }
}
