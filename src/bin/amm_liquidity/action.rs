use anyhow::{Context, Result};
use serde::Deserialize;
use simulator_client::{
    ActionResultNotification, ActionSubscriptionHandle, BacktestSession, subscribe_actions,
};
use solana_account_decoder::UiAccount;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::resolve_url;

/// Read the SPL token `amount` from a returned `UiAccount` JSON value — one entry of
/// [`ActionResultNotification::accounts`]. Deserializes the account envelope and
/// decodes its data (base64/base58/zstd handled by [`UiAccount`]), then reads the
/// token `amount` as the little-endian `u64` at the SPL token account's `[64..72]`
/// byte range. Returns `None` for non-binary encodings (e.g. `jsonParsed`) or a
/// too-short account.
pub(crate) fn token_amount(account: &serde_json::Value) -> Option<u64> {
    let data = UiAccount::deserialize(account).ok()?.data.decode()?;
    let amount = data.get(64..72)?;
    Some(u64::from_le_bytes(amount.try_into().ok()?))
}

/// Label tagged on the spread [`ScheduledAction`](simulator_api::ScheduledAction).
pub(crate) const SPREAD_LABEL: &str = "spread";
/// Prefix for quote→base depth action labels; followed by the sweep size.
pub(crate) const DEPTH_Q2B_PREFIX: &str = "depth-q2b-";
/// Prefix for base→quote depth action labels; followed by the sweep size.
pub(crate) const DEPTH_B2Q_PREFIX: &str = "depth-b2q-";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DepthDirection {
    QuoteToBase,
    BaseToQuote,
}

pub(crate) enum Label {
    Spread,
    Depth(DepthDirection),
}

impl Label {
    /// Parse an action label. Spread actions are labelled [`SPREAD_LABEL`]; depth
    /// actions `{DEPTH_Q2B_PREFIX}{size}` / `{DEPTH_B2Q_PREFIX}{size}`, where
    /// `{size}` is the sweep size. Returns `None` for unknown labels or an
    /// unparseable size.
    pub(crate) fn parse(name: &str) -> Option<(Self, u64)> {
        if name == SPREAD_LABEL {
            return Some((Self::Spread, 0));
        }
        if let Some(size) = name.strip_prefix(DEPTH_Q2B_PREFIX) {
            return Some((Self::Depth(DepthDirection::QuoteToBase), size.parse().ok()?));
        }
        if let Some(size) = name.strip_prefix(DEPTH_B2Q_PREFIX) {
            return Some((Self::Depth(DepthDirection::BaseToQuote), size.parse().ok()?));
        }

        None
    }
}

/// Subscribe to the session's scheduled-action results before advancing.
///
/// The server runs the registered actions automatically each slot and streams
/// the results over `actionSubscribe`; the background task forwards each one over
/// the returned channel, in arrival order, so the caller can process events as
/// they stream in. After driving the session, call `handle.stop.send(true)` and
/// await `handle.join_handle`; that drains in-flight results and drops the sender,
/// which closes the channel and ends the consumer's `recv()` loop.
pub(crate) async fn subscribe_action_results(
    session: &BacktestSession,
    url: &str,
) -> Result<(
    ActionSubscriptionHandle,
    UnboundedReceiver<ActionResultNotification>,
)> {
    let rpc_endpoint = session
        .rpc_endpoint()
        .context("session has no rpc endpoint")?;
    let rpc_url = resolve_url(&format!("https://{}", url), rpc_endpoint);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = subscribe_actions(&rpc_url, move |result| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(result);
        }
    })
    .await?;

    Ok((handle, rx))
}
