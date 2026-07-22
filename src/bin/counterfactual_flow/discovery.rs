use backtest_example::utils::types::TxWithMeta;

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
