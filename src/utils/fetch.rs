use anyhow::{Context, Result, anyhow};
use simulator_client::balances::{BalanceChanges, balance_changes};
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::config::RpcTransactionConfig;
use solana_signature::Signature;
use solana_transaction_status::{
    EncodedTransaction, UiAddressTableLookup, UiLoadedAddresses, UiMessage, UiTransactionEncoding,
    option_serializer::OptionSerializer,
};

use super::parse::MAX_SUPPORTED_TRANSACTION_VERSION;

/// Call `getTransaction` for `signature` and extract per-account balance changes.
pub async fn fetch_balance_changes(rpc: &RpcClient, signature: &str) -> Result<BalanceChanges> {
    let parsed: Signature = signature.parse().context("parse signature")?;
    let config = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(MAX_SUPPORTED_TRANSACTION_VERSION),
        min_context_slot: None,
    };

    let mut confirmed = rpc
        .get_transaction_with_config(&parsed, config)
        .await
        .context("getTransaction")?;

    // The session returns an empty `loadedAddresses` for a transaction that uses lookup tables,
    // so the account keys would not line up with the balances.
    let lookups = match &confirmed.transaction.transaction {
        EncodedTransaction::Json(ui) => match &ui.message {
            UiMessage::Raw(raw) => raw.address_table_lookups.as_deref().unwrap_or_default(),
            UiMessage::Parsed(_) => &[],
        },
        _ => &[],
    };
    if !lookups.is_empty() {
        let loaded = resolve_lookups(rpc, lookups)
            .await
            .context("resolve address table lookups")?;
        if let Some(meta) = confirmed.transaction.meta.as_mut() {
            meta.loaded_addresses = OptionSerializer::Some(loaded);
        }
    }

    balance_changes(&confirmed.transaction).context("read balance changes")
}

/// The addresses `lookups` load: every writable one first, then every readonly one.
async fn resolve_lookups(
    rpc: &RpcClient,
    lookups: &[UiAddressTableLookup],
) -> Result<UiLoadedAddresses> {
    let keys = lookups
        .iter()
        .map(|lookup| lookup.account_key.parse())
        .collect::<Result<Vec<Pubkey>, _>>()
        .context("parse lookup table key")?;
    let accounts = rpc
        .get_multiple_accounts(&keys)
        .await
        .context("fetch lookup tables")?;

    let pick = |table: &[Pubkey], indexes: &[u8]| {
        indexes
            .iter()
            .map(|&index| {
                table
                    .get(usize::from(index))
                    .map(ToString::to_string)
                    .ok_or_else(|| anyhow!("lookup index {index} is outside the table"))
            })
            .collect::<Result<Vec<_>>>()
    };
    lookups.iter().zip(keys).zip(accounts).try_fold(
        UiLoadedAddresses {
            writable: Vec::new(),
            readonly: Vec::new(),
        },
        |mut loaded, ((lookup, key), account)| -> Result<_> {
            let account = account.with_context(|| format!("lookup table {key} not found"))?;
            let table = AddressLookupTable::deserialize(&account.data)
                .with_context(|| format!("decode lookup table {key}"))?;
            loaded
                .writable
                .extend(pick(&table.addresses, &lookup.writable_indexes)?);
            loaded
                .readonly
                .extend(pick(&table.addresses, &lookup.readonly_indexes)?);
            Ok(loaded)
        },
    )
}
