//! The JSONL row types and the encoding they read and write, split out so `main` reads as the counterfactual and not its plumbing.

use std::{
    fs,
    io::{self, BufRead, Write},
    path::Path,
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use simulator_api::AccountData;
use simulator_client::RerouteLegNotification;
use solana_message::{
    Message, MessageHeader, VersionedMessage,
    compiled_instruction::CompiledInstruction,
    v0::{self, MessageAddressTableLookup},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::{TransactionVersion, VersionedTransaction};
use solana_transaction_status::{EncodedTransaction, EncodedTransactionWithStatusMeta, UiMessage};

/// One line of the `capture` JSONL.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureRow {
    pub(crate) slot: u64,
    pub(crate) account: AccountData,
    /// The transaction that produced this state, when the diff named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) signature: Option<String>,
    /// That transaction, base64-encoded — the wire bytes a setup transaction replays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) transaction: Option<String>,
}

/// One line of the `run` reroute-notification JSONL.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RerouteRow<'a> {
    pub(crate) slot: u64,
    pub(crate) original_signature: &'a str,
    pub(crate) legs: Vec<RerouteLegRow<'a>>,
    pub(crate) err: Option<&'a str>,
    pub(crate) compute_units: u64,
    pub(crate) realized_out: Option<u64>,
    pub(crate) original_realized_out: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RerouteLegRow<'a> {
    input_mint: &'a str,
    output_mint: &'a str,
    amount: u64,
    original_quoted_out: u64,
    metis_quoted_out: u64,
    /// Metis's chosen route. The per-hop `ammKey`s are how you find the pool to capture
    /// and override.
    route_plan: Option<&'a str>,
    /// The route the original took on L1, so a reader can see what the re-quote displaced.
    original_route_plan: Option<&'a str>,
}

impl<'a> From<&'a RerouteLegNotification> for RerouteLegRow<'a> {
    fn from(leg: &'a RerouteLegNotification) -> Self {
        Self {
            input_mint: &leg.input_mint,
            output_mint: &leg.output_mint,
            amount: leg.amount,
            original_quoted_out: leg.original_quoted_out,
            metis_quoted_out: leg.metis_quoted_out,
            route_plan: leg.route_plan.as_deref(),
            original_route_plan: leg.original_route_plan.as_deref(),
        }
    }
}

/// One line of the `compare` report JSONL.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JoinedLeg {
    pub(crate) shift: i64,
    pub(crate) original_signature: String,
    pub(crate) leg_index: usize,
    pub(crate) input_mint: String,
    pub(crate) output_mint: String,
    pub(crate) amount: u64,
    pub(crate) original_quoted_out: u64,
    pub(crate) base_quoted_out: u64,
    pub(crate) quoted_out: u64,
    pub(crate) delta_bps: f64,
}

pub(crate) fn write_capture(path: &Path, rows: &[CaptureRow]) -> Result<()> {
    let mut out = io::BufWriter::new(fs::File::create(path)?);
    for row in rows {
        writeln!(out, "{}", serde_json::to_string(row)?)?;
    }
    out.flush()?;
    Ok(())
}

pub(crate) fn load_capture_rows(path: &Path) -> Result<Vec<CaptureRow>> {
    let input = io::BufReader::new(
        fs::File::open(path).with_context(|| format!("opening capture {}", path.display()))?,
    );
    input
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(&line?)
                .with_context(|| format!("parsing {} line {}", path.display(), index + 1))
        })
        .collect()
}

/// Rebuild the signed wire encoding from the JSON the transaction subscription pushes, with the
/// signature the account diffs name it by. Legacy and v0 serialize differently, and a v0 message
/// with no lookups is indistinguishable from legacy, so `version` decides — except that address
/// table lookups only exist in v0, so their presence outranks a missing or stale `version`.
pub(crate) fn wire_transaction(
    encoded: &EncodedTransactionWithStatusMeta,
) -> Option<(String, String)> {
    let EncodedTransaction::Json(ui) = &encoded.transaction else {
        return None;
    };
    let UiMessage::Raw(raw) = &ui.message else {
        return None;
    };
    let header = MessageHeader {
        num_required_signatures: raw.header.num_required_signatures,
        num_readonly_signed_accounts: raw.header.num_readonly_signed_accounts,
        num_readonly_unsigned_accounts: raw.header.num_readonly_unsigned_accounts,
    };
    let account_keys = raw
        .account_keys
        .iter()
        .map(|key| key.parse().ok())
        .collect::<Option<Vec<Pubkey>>>()?;
    let recent_blockhash = raw.recent_blockhash.parse().ok()?;
    let instructions = raw
        .instructions
        .iter()
        .map(|ix| {
            Some(CompiledInstruction {
                program_id_index: ix.program_id_index,
                accounts: ix.accounts.clone(),
                data: bs58::decode(&ix.data).into_vec().ok()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let address_table_lookups = raw
        .address_table_lookups
        .iter()
        .flatten()
        .map(|lookup| {
            Some(MessageAddressTableLookup {
                account_key: lookup.account_key.parse().ok()?,
                writable_indexes: lookup.writable_indexes.clone(),
                readonly_indexes: lookup.readonly_indexes.clone(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let versioned = matches!(encoded.version, Some(TransactionVersion::Number(0)))
        || !address_table_lookups.is_empty();
    let message = match versioned {
        true => VersionedMessage::V0(v0::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
            address_table_lookups,
        }),
        false => VersionedMessage::Legacy(Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
        }),
    };
    let transaction = VersionedTransaction {
        signatures: ui
            .signatures
            .iter()
            .map(|signature| signature.parse().ok())
            .collect::<Option<Vec<Signature>>>()?,
        message,
    };
    Some((
        ui.signatures.first()?.clone(),
        BASE64.encode(bincode::serialize(&transaction).ok()?),
    ))
}
