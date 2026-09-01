//! The plan file: the venue's routing identity, the accounts its capital and curve live in, and the checks a plan must pass before a run.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use simulator_api::DirectFillParams;
use solana_address::Address;

/// [`Address`]'s own `Serialize` is the 32-byte array, so a hand-written plan needs base58 explicitly.
mod base58 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
    use solana_address::Address;

    fn parse<E: Error>(spelling: &str) -> Result<Address, E> {
        spelling
            .parse()
            .map_err(|_| E::custom(format!("{spelling} is not a base58 address")))
    }

    pub(super) fn serialize<S: Serializer>(address: &Address, out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(&address.to_string())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Address, D::Error> {
        parse(&String::deserialize(input)?)
    }

    pub(super) mod list {
        use super::{Address, Deserialize, Deserializer, Serialize, Serializer, parse};

        pub(crate) fn serialize<S: Serializer>(
            addresses: &[Address],
            out: S,
        ) -> Result<S::Ok, S::Error> {
            addresses
                .iter()
                .map(Address::to_string)
                .collect::<Vec<_>>()
                .serialize(out)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            input: D,
        ) -> Result<Vec<Address>, D::Error> {
            Vec::<String>::deserialize(input)?
                .iter()
                .map(|spelling| parse(spelling))
                .collect()
        }
    }
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StateLayout {
    #[serde(with = "base58")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Inventory {
    /// The token accounts the venue settles from, in the order the state account mirrors them.
    #[serde(with = "base58::list")]
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
    pub(crate) direct_fill: DirectFillParams,
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
                .market
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped plan is the worked example the README points at.
    #[test]
    fn the_shipped_tempest_plan_parses_and_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/bin/counterfactual_capital/tempest-sol-usdc.json");
        let plan = Plan::read(Path::new(path)).expect("the shipped plan is valid");
        assert_eq!(plan.inventory.vaults.len(), 2);
        let state = plan.inventory.state.expect("Tempest quotes from a ladder");
        assert_eq!(state.len, 2385);
        assert_eq!(state.ladders.len(), 2);
        assert_eq!(state.balance_mirrors, vec![2321, 2329]);
        assert_eq!(state.discriminator.as_deref(), Some("tempest1"));
    }

    fn tempest() -> Plan {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/bin/counterfactual_capital/tempest-sol-usdc.json");
        Plan::read(Path::new(path)).expect("the shipped plan is valid")
    }

    #[test]
    fn every_overridden_account_is_writable_in_the_route() {
        let plan = tempest();
        for account in plan.overridden() {
            let entry = plan
                .direct_fill
                .market
                .accounts
                .iter()
                .find(|entry| entry.address == account)
                .expect("named in the run");
            assert!(entry.writable, "{account} must be writable");
        }
    }

    #[test]
    fn a_vault_outside_the_account_run_is_refused() {
        let mut plan = tempest();
        plan.inventory.vaults[0] = "11111111111111111111111111111111".parse().expect("an address");
        let error = plan.validate().expect_err("an unloaded vault must be refused");
        assert!(error.to_string().contains("never loads it"), "{error}");
    }

    #[test]
    fn a_mirror_count_that_does_not_match_the_vaults_is_refused() {
        let mut plan = tempest();
        plan.inventory.state.as_mut().expect("a ladder").balance_mirrors = vec![2321];
        let error = plan.validate().expect_err("a partial mirror list must be refused");
        assert!(error.to_string().contains("one per vault"), "{error}");
    }

    #[test]
    fn a_tier_count_large_enough_to_overflow_the_bounds_check_is_refused() {
        let mut plan = tempest();
        plan.inventory.state.as_mut().expect("a ladder").max_tiers = usize::MAX;
        let error = plan.validate().expect_err("an overflowing bound must be refused");
        assert!(error.to_string().contains("runs past the account's"), "{error}");
    }

    #[test]
    fn a_tier_count_offset_outside_the_account_is_refused() {
        let mut plan = tempest();
        plan.inventory.state.as_mut().expect("a ladder").ladders[0].count = 2_380;
        let error = plan.validate().expect_err("an out-of-bounds count offset must be refused");
        assert!(error.to_string().contains("does not fit"), "{error}");
    }

    #[test]
    fn a_ladder_running_past_the_account_is_refused() {
        let mut plan = tempest();
        plan.inventory.state.as_mut().expect("a ladder").ladders[0].entries = 2300;
        let error = plan.validate().expect_err("an out-of-bounds ladder must be refused");
        assert!(error.to_string().contains("past the account's"), "{error}");
    }
}
