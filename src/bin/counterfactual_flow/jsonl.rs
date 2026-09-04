//! The JSONL row types and the encoding they read and write.

use std::{fs, io, io::BufRead, path::Path};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use simulator_api::RerouteStatsReport;
use simulator_client::RerouteNotification;
use solana_message::{
    Message, MessageHeader, VersionedMessage,
    compiled_instruction::CompiledInstruction,
    v0::{self, MessageAddressTableLookup},
};
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use solana_transaction::versioned::{TransactionVersion, VersionedTransaction};
use solana_transaction_status::{EncodedTransaction, EncodedTransactionWithStatusMeta, UiMessage};

/// The version a writer stamps and a reader accepts.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// The first line of a recording. Every line after it is a
/// [`simulator_client::RerouteNotification`] written verbatim, so a reader cannot drift from the
/// wire type. `kind` lets a reader tell a frame from a notification by content, not position.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunHeader {
    pub(crate) format_version: u32,
    pub(crate) kind: HeaderKind,
    pub(crate) start_slot: u64,
    pub(crate) end_slot: u64,
    /// The venue the run was about, which a report with no selector measures.
    #[serde(default)]
    pub(crate) program_id: Option<String>,
    #[serde(default)]
    pub(crate) label: Option<String>,
    /// The re-price the arm applied. `None` posts no override at all.
    #[serde(default)]
    pub(crate) price_shift_bps: Option<f64>,
    /// Anchor slots the arm posts at; far below the range means the venue barely moved.
    pub(crate) override_slots: usize,
    /// `logs` and `routedTransaction` were emptied rather than kept, so a reader reports them
    /// absent by choice rather than inferring a run that streamed nothing.
    pub(crate) slim: bool,
    /// Serialized as `rerouteVenues`, the key already written into recorded runs.
    #[serde(rename = "rerouteVenues")]
    pub(crate) reroute_aggregators: Option<String>,
    pub(crate) filter_pairs: Vec<String>,
    pub(crate) circular_arbs: bool,
    pub(crate) detect_failed_l1_swaps: bool,
    pub(crate) replay_account_state: bool,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) enum HeaderKind {
    CounterfactualFlowRun,
}

/// The run's funnel. A trailer because it is only known at the end, so a run that died mid-range
/// writes none and its totals are never read as covering the whole range.
///
/// Named field by field rather than flattening [`RerouteStatsReport`]: a counter reaching this
/// struct is a decision. Leave one out unless it says something about the venue under test.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunSummary {
    pub(crate) kind: SummaryKind,
    pub(crate) swaps_detected: u64,
    /// Excluded before quoting: an arbitrage cycle, a pair the filter does not name. Without it
    /// the gap to `swaps_rerouted` reads as routing failures.
    pub(crate) swaps_filtered: u64,
    pub(crate) swaps_rerouted: u64,
    pub(crate) swaps_simulated: u64,
    pub(crate) swaps_succeeded: u64,
    pub(crate) requote_failures: u64,
    /// Failed to post any state, leaving those slots on an older override.
    pub(crate) override_setup_failures: u64,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SummaryKind {
    CounterfactualFlowSummary,
}

impl RunSummary {
    pub(crate) fn from_report(report: &RerouteStatsReport) -> Self {
        Self {
            kind: SummaryKind::CounterfactualFlowSummary,
            swaps_detected: report.swaps_detected,
            swaps_filtered: report.swaps_filtered,
            swaps_rerouted: report.swaps_rerouted,
            swaps_simulated: report.swaps_simulated,
            swaps_succeeded: report.swaps_succeeded,
            requote_failures: report.requote_failures,
            override_setup_failures: report.override_setup_failures,
        }
    }
}

/// One line of the `compare` report JSONL.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JoinedLeg {
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

/// Everything one recording holds. A run that died mid-range writes no header or trailer, which
/// a reader has to be able to say rather than reporting a partial range as a whole one.
pub(crate) struct Recording {
    pub(crate) header: Option<RunHeader>,
    pub(crate) notifications: Vec<RerouteNotification>,
    pub(crate) summary: Option<RunSummary>,
}

/// Rows are classified by content, so a concatenated or headless file still reads. An
/// unrecognized line is an error, since dropping one would understate every total.
pub(crate) fn read_recording(path: &Path) -> Result<Recording> {
    let input = io::BufReader::new(
        fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
    );
    let mut recording = Recording {
        header: None,
        notifications: Vec::new(),
        summary: None,
    };
    for (index, line) in input.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let at = || format!("{} line {}", path.display(), index + 1);
        // Only frames carry `kind`, so this decides the grammar before anything parses the line.
        if !line.contains("\"kind\"") {
            recording
                .notifications
                .push(serde_json::from_str(&line).with_context(at)?);
            continue;
        }
        // A frame that will not parse is version skew, not a notification.
        match serde_json::from_str::<RunHeader>(&line) {
            Ok(header) => recording.header = Some(header),
            Err(header_error) => match serde_json::from_str::<RunSummary>(&line) {
                Ok(summary) => recording.summary = Some(summary),
                Err(_) => {
                    return Err(anyhow::Error::new(header_error).context(format!(
                        "{}: unreadable header or trailer; this build reads format version \
                         {FORMAT_VERSION}",
                        at()
                    )));
                }
            },
        }
    }
    Ok(recording)
}

/// Rebuild the signed wire encoding from the JSON the transaction subscription pushes. A v0
/// message with no lookups is indistinguishable from legacy, so `version` decides — but lookups
/// exist only in v0, so their presence outranks a missing or stale `version`.
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
