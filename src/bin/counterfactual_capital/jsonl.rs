//! What each arm writes out: the vaults it posted, the ladder tiers it quoted, and its own totals.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One vault as an arm posted it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VaultRow {
    pub(crate) address: String,
    pub(crate) mint: String,
    pub(crate) decimals: u8,
    /// Base units the venue held at the start slot.
    pub(crate) before: u64,
    /// Base units this arm posted.
    pub(crate) after: u64,
    pub(crate) native: bool,
    /// Lamports the override carried: rent, plus the amount itself on a wrapped-SOL vault.
    pub(crate) lamports: u64,
}

/// One ladder tier as an arm posted it, in the venue's own raw units.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TierRow {
    pub(crate) side: usize,
    pub(crate) tier: usize,
    pub(crate) price_before: String,
    pub(crate) price_after: String,
    pub(crate) size_before: String,
    pub(crate) size_after: String,
}

/// One arm of the sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArmRow {
    pub(crate) multiple: f64,
    #[serde(default)]
    pub(crate) tighten_bps: f64,
    /// What the multiple rewrote: `all`, `vaults` or `ladder`, each of which reads it differently.
    #[serde(default)]
    pub(crate) scale: String,
    /// Whether this arm posted an override at all; the one arm that does not is the control.
    #[serde(default)]
    pub(crate) frozen: bool,
    pub(crate) vaults: Vec<VaultRow>,
    #[serde(default)]
    pub(crate) tiers: Vec<TierRow>,
    /// The deepest tier this arm quotes, in the base mint's raw units: the ceiling on one trade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ceiling: Option<String>,
    /// Hops whose pair the book named.
    pub(crate) matched: u64,
    /// Of those, the ones the encoder could build. The denominator for `fill_rate`.
    pub(crate) built: u64,
    /// Probes that filled and could be scored against the hop they replace.
    pub(crate) scored: u64,
    /// Sum of per-probe bps; divide by `scored` for the mean.
    pub(crate) bps_total: i64,
    pub(crate) rejections: BTreeMap<String, u64>,
    pub(crate) outcomes: BTreeMap<String, u64>,
}

impl ArmRow {
    pub(crate) fn fill_rate(&self) -> f64 {
        match self.built {
            0 => 0.0,
            built => self.scored as f64 / built as f64,
        }
    }

    /// `None`, not zero, when nothing scored: zero would read as priced level with the market.
    pub(crate) fn mean_bps(&self) -> Option<f64> {
        match self.scored {
            0 => None,
            scored => Some(self.bps_total as f64 / scored as f64),
        }
    }

    pub(crate) fn outcome(&self, key: &str) -> u64 {
        self.outcomes.get(key).copied().unwrap_or(0)
    }

    /// Every outcome that is not a fill, largest first.
    pub(crate) fn refusals(&self) -> Vec<(&str, u64)> {
        let mut refusals = self
            .outcomes
            .iter()
            .filter(|(key, _)| key.as_str() != FILLED)
            .map(|(key, count)| (key.as_str(), *count))
            .collect::<Vec<_>>();
        refusals.sort_by_key(|(key, count)| (std::cmp::Reverse(*count), *key));
        refusals
    }
}

/// The outcome key a probe that filled is recorded under.
pub(crate) const FILLED: &str = "filled";
