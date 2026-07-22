use anyhow::{Context, Result};
use backtest_example::utils::types::TxWithMeta;
use serde::Deserialize;
use simulator_client::RerouteLegNotification;
use solana_address::Address;

/// BPF Upgradeable Loader — owns on-chain programs and applies their upgrades.
const BPF_LOADER_UPGRADEABLE: &str = "BPFLoaderUpgradeab1e11111111111111111111111";
// UpgradeableLoaderInstruction is a bincode-serialized enum; instruction data
// begins with the 4-byte little-endian variant tag. `Upgrade` is variant 3 and
// carries no fields, so its data is exactly these bytes.
const BPF_UPGRADE_DISCRIMINANT: [u8; 4] = [3, 0, 0, 0];

/// Returns true if the transaction issues a BPF Loader Upgradeable `Upgrade`
/// instruction — i.e. an on-chain program's executable was replaced.
pub fn is_program_upgrade(tx_with_meta: &TxWithMeta) -> bool {
    let tx = &tx_with_meta.transaction;
    let keys = tx.message.static_account_keys();
    // The loader is a program being invoked, so it is always a static key.
    let Some(loader_idx) = keys
        .iter()
        .position(|k| k.to_string() == BPF_LOADER_UPGRADEABLE)
    else {
        return false;
    };
    let loader_idx = loader_idx as u8;
    tx.message
        .instructions()
        .iter()
        .filter(|ix| ix.program_id_index == loader_idx)
        .any(|ix| ix.data.starts_with(&BPF_UPGRADE_DISCRIMINANT))
}

const PROGRAM_ID_TO_LABEL_URL: &str = "https://lite-api.jup.ag/swap/v1/program-id-to-label";

/// Resolve `program_id`'s Jupiter/Metis route label via `program-id-to-label`.
pub async fn resolve_venue_label(program_id: &Address) -> Result<String> {
    let labels: std::collections::HashMap<String, String> = reqwest::get(PROGRAM_ID_TO_LABEL_URL)
        .await
        .context("fetch program-id-to-label")?
        .json()
        .await
        .context("parse program-id-to-label response")?;
    labels
        .get(&program_id.to_string())
        .cloned()
        .with_context(|| {
            format!("{program_id} has no Jupiter/Metis label — is it a routable venue?")
        })
}

/// One hop of Metis's raw `routePlan` JSON, as carried in `route_plan`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoutePlanHop {
    swap_info: SwapInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapInfo {
    label: String,
}

/// Whether any hop of Metis's chosen route for `leg` carries `venue_label`.
/// Falls back to a substring check on `route_summary` when `route_plan` is
/// absent or fails to parse.
pub fn contains_venue(leg: &RerouteLegNotification, venue_label: &str) -> bool {
    let hops: Option<Vec<RoutePlanHop>> = leg
        .route_plan
        .as_deref()
        .and_then(|plan| serde_json::from_str(plan).ok());
    match hops {
        Some(hops) => hops.iter().any(|hop| hop.swap_info.label == venue_label),
        None => leg.route_summary.contains(venue_label),
    }
}
