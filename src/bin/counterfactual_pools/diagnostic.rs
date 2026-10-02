//! Jupiter Metis diagnostic probes: 
//!     - ask the simulation's Metis instance to quote a pool on-demand, independent of historical taker flow
//!     - useful to check what Metis thinks the pool's prices are, even in a window with no mainnet SNDK swaps
//! 
//! To enable, add the probe to the session and attach a subscriber:
//! ```ignore
//! let create = CreateSession::builder()
//!     // ...
//!     .diagnostic_probes(vec![diagnostic::params(POOL)?])
//!     .build();
//! 
//! // after the session starts:
//! let probes = Arc::new(Mutex::new(diagnostic::Probes::default()));
//! let sink = probes.clone();
//! let handle = subscribe_diagnostics(&session.session_info().rpc_endpoint, move |sample| {
//!     diagnostic::record(&mut sink.lock().expect("probes"), sample);
//!     ready(())
//! })
//! .await?;
//! 
//! // after the session completes:
//! handle.stop.send(true).ok();
//! handle.join_handle.await??;
//! diagnostic::print(&probes.lock().expect("probes"));
//! ```

use std::collections::BTreeMap;

use anyhow::Result;
use simulator_api::{ActionAnchor, DiagnosticProbeParams};
use simulator_client::{DiagnosticNotification, reroute_report::short_mint};
use solana_address::Address;

/// Slots between probes.
const PROBE_EVERY: u64 = 250;
/// Trade sizes, in USD, the router quotes the pool at.
const PROBE_USD: [u32; 3] = [100, 1_000, 10_000];

/// The router's direct quotes of one pool in one direction.
#[derive(Default)]
pub struct Probe {
    quoted: u64,
    failed: u64,
    last_error: Option<String>,
    /// Output per unit of input at the smallest size quoted.
    last_rate: Option<f64>,
}

/// Probe results keyed by pool, then `(input, output)` mint.
pub type Probes = BTreeMap<String, BTreeMap<(String, String), Probe>>;

/// Probe `pool` every [`PROBE_EVERY`] slots at each of [`PROBE_USD`].
pub fn params(pool: Address) -> Result<DiagnosticProbeParams> {
    Ok(DiagnosticProbeParams {
        anchor: ActionAnchor::AfterEverySlot {
            every_n_slots: PROBE_EVERY.try_into()?,
        },
        markets: vec![pool],
        usd_values: PROBE_USD.to_vec(),
        label: None,
    })
}

/// Add one diagnostic probe's result to the total:
///     - a `swapInfo` means Metis was able to quote against the pool
///     - a `quoteFailure` means Metis failed to quote against it
pub fn record(probes: &mut Probes, sample: DiagnosticNotification) {
    let pool = probes.entry(sample.market).or_default();
    for result in sample
        .results
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = |value: &serde_json::Value, key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        if let Some(info) = result.get("swapInfo") {
            let probe = pool
                .entry((text(info, "inputMint"), text(info, "outputMint")))
                .or_default();
            probe.quoted += 1;
            let amount = |key| text(info, key).parse::<f64>().ok();
            if let (Some(input), Some(output)) = (amount("inAmount"), amount("outAmount")) {
                probe.last_rate = Some(output / input);
            }
        } else if let Some(failure) = result.get("quoteFailure") {
            let probe = pool
                .entry((text(failure, "input_mint"), text(failure, "output_mint")))
                .or_default();
            probe.failed += 1;
            probe.last_error = Some(text(failure, "error"));
        }
    }
}

pub fn print(probes: &Probes) {
    for (pool, directions) in probes {
        println!("router quotes of {pool}:");
        for ((input, output), probe) in directions {
            println!(
                "  {}->{}: {} quoted, {} failed{}{}",
                short_mint(input),
                short_mint(output),
                probe.quoted,
                probe.failed,
                probe
                    .last_rate
                    .map(|rate| format!(", rate {rate:.6}"))
                    .unwrap_or_default(),
                probe
                    .last_error
                    .as_ref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            );
        }
    }
}
