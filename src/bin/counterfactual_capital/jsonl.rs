//! What each arm writes out: enough to re-render the table without re-running the range, and
//! enough to tell a flat curve from a broken one.

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
}

/// One arm of the ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArmRow {
    pub(crate) multiple: f64,
    pub(crate) vaults: Vec<VaultRow>,
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
    /// Share of buildable hops this arm's venue filled.
    pub(crate) fn fill_rate(&self) -> f64 {
        match self.built {
            0 => 0.0,
            built => self.scored as f64 / built as f64,
        }
    }

    /// Mean bps over the probes that scored, or `None` when none did — an arm that filled nothing
    /// has no mean, and reporting one as zero would read as "priced level with the market".
    pub(crate) fn mean_bps(&self) -> Option<f64> {
        match self.scored {
            0 => None,
            scored => Some(self.bps_total as f64 / scored as f64),
        }
    }
}
