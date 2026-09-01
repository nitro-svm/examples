//! Recording an account's real trajectory: the subscribe / collect / drain lifecycle both
//! counterfactuals need before they can replay a modified one.

use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use simulator_api::{AccountData, BinaryEncoding, EncodedBinary};
use simulator_client::{
    AccountDiffSubscriptionHandle, RoutedAccountDiffNotification, subscribe_account_diffs_many,
};
use solana_account::Account;
use solana_address::Address;

/// A running capture. The subscription writes into `state` until [`Collected::finish`] takes it.
pub struct Collected<T> {
    state: Arc<Mutex<T>>,
    handle: AccountDiffSubscriptionHandle,
}

/// Subscribe to every account whose trajectory the run needs, folding each diff into `initial`.
///
/// Start this before the range advances: the subscription treats a dropped websocket as a fatal
/// completeness error rather than reconnecting, because a missed change would ride on as a stale
/// override at every later slot with nothing downstream able to tell.
pub async fn collect_account_diffs<T, F>(
    rpc_url: &str,
    accounts: &[Address],
    initial: T,
    record: F,
) -> Result<Collected<T>>
where
    T: Send + 'static,
    F: Fn(&mut T, RoutedAccountDiffNotification) + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(initial));
    let sink = state.clone();
    let record = Arc::new(record);
    let handle = subscribe_account_diffs_many(
        rpc_url,
        accounts.iter().map(Address::to_string),
        move |routed| {
            let sink = sink.clone();
            let record = record.clone();
            async move {
                let mut state = sink.lock().expect("the capture sink is never poisoned");
                record(&mut state, routed);
            }
        },
    )
    .await
    .context("subscribing to the accounts under capture")?;
    Ok(Collected { state, handle })
}

impl<T: Default> Collected<T> {
    /// Stop the subscription, wait for it to drain, and take what it collected.
    pub async fn finish(self) -> Result<T> {
        self.handle.stop.send(true).ok();
        self.handle
            .join_handle
            .await
            .context("the account subscription task panicked")?
            .context("the account subscription ended early, so the capture is incomplete")?;
        Ok(std::mem::take(
            &mut *self
                .state
                .lock()
                .expect("the capture sink is never poisoned"),
        ))
    }
}

/// One account state as it stood after a change, as a capture file records it.
///
/// A capture is JSONL, one row per change, so a sweep can be replayed against a trajectory without
/// paying for the reference replay again — and so two runs can be compared against byte-identical
/// state.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRow {
    pub slot: u64,
    /// Which account this state belongs to. Absent in single-account captures, where the file
    /// itself names the account. Base58, as [`AccountData`] spells an address.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,
    pub account: AccountData,
    /// The transaction that produced this state, when the diff named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// That transaction, base64-encoded — the wire bytes a setup transaction replays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
}

/// An account as an override carries it. The inverse of [`AccountData::to_account`], which the
/// library has but its counterpart does not.
pub fn account_data(account: &Account) -> AccountData {
    AccountData {
        space: account.data.len() as u64,
        data: EncodedBinary::from_bytes(&account.data, BinaryEncoding::Base64),
        executable: account.executable,
        lamports: account.lamports,
        owner: account.owner,
    }
}

pub fn write_capture(path: &Path, rows: &[CaptureRow]) -> Result<()> {
    let mut out = BufWriter::new(
        File::create(path).with_context(|| format!("writing capture {}", path.display()))?,
    );
    for row in rows {
        writeln!(out, "{}", serde_json::to_string(row)?)?;
    }
    out.flush()?;
    Ok(())
}

pub fn load_capture(path: &Path) -> Result<Vec<CaptureRow>> {
    let input = BufReader::new(
        File::open(path).with_context(|| format!("opening capture {}", path.display()))?,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: Option<Address>) -> CaptureRow {
        CaptureRow {
            slot: 7,
            address,
            account: account_data(&Account::default()),
            signature: None,
            transaction: None,
        }
    }

    #[test]
    fn a_row_without_an_account_omits_the_key_rather_than_writing_null() {
        let written = serde_json::to_string(&row(None)).expect("serializing a row");
        assert!(!written.contains("address"), "{written}");
    }

    /// A line as `counterfactual_flow` wrote them before the row carried an account.
    #[test]
    fn a_capture_written_before_rows_named_their_account_still_reads() {
        let line = r#"{"slot":439649408,"account":{"data":{"data":"","encoding":"base64"},
            "executable":false,"lamports":1,"owner":"11111111111111111111111111111111","space":0}}"#;
        let read: CaptureRow = serde_json::from_str(line).expect("reading a row with no address");
        assert_eq!(read.address, None);
        assert_eq!(read.slot, 439649408);
    }

    /// `rent_epoch` is the one field an override cannot carry, so a capture cannot either.
    #[test]
    fn an_account_survives_the_round_trip_apart_from_its_rent_epoch() {
        let original = Account {
            lamports: 42,
            data: vec![1, 2, 3],
            owner: Address::from([9; 32]),
            executable: true,
            rent_epoch: 5,
        };
        let read = account_data(&original).to_account().expect("decoding");

        assert_eq!(read.rent_epoch, 0, "AccountData has no field for it");
        assert_eq!(
            read,
            Account {
                rent_epoch: 0,
                ..original
            }
        );
    }
}
