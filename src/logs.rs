use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Context, Result};
use simulator_client::{LogSubscriptionHandle, subscribe_program_logs};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use tokio::{sync::watch, task::JoinHandle};

use super::{Stats, Transaction};
use crate::utils::fetch_balance_changes;

// ── Log subscription ───────────────────────────────────────────────────────────

/// Subscribe to logs mentioning `program`.
///
/// Each notification spawns a concurrent `getTransaction` task. All records are
/// held in memory and written to `log_file` in arrival order once the task exits.
///
/// Returns `(JoinHandle, stop_tx)`. To shut down cleanly:
///   1. `stop_tx.send(true)` — signals the task to drain remaining buffered
///      notifications, then await all in-flight `getTransaction` tasks and write.
///   2. `await` the handle.
///   3. Close the session only after the handle resolves.
pub async fn subscribe_logs(
    rpc_endpoint: &str,
    program: &str,
    log_file: PathBuf,
    stats: Arc<Mutex<Stats>>,
) -> Result<(JoinHandle<()>, watch::Sender<bool>)> {
    let rpc = Arc::new(RpcClient::new_with_commitment(
        rpc_endpoint.to_string(),
        CommitmentConfig::confirmed(),
    ));

    // Shared state collected by per-notification callbacks.
    let records: Arc<Mutex<Vec<(usize, Transaction)>>> = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::new(AtomicUsize::new(0));

    let rpc_cb = rpc.clone();
    let records_cb = records.clone();
    let counter_cb = counter.clone();
    let program_cb = program.to_string();

    let LogSubscriptionHandle { join_handle: sub_handle, stop } = subscribe_program_logs(
        rpc_endpoint,
        program,
        CommitmentConfig::confirmed(),
        move |notification| {
            // Capture arrival index before spawning so records can be sorted later.
            let idx = counter_cb.fetch_add(1, Ordering::Relaxed);
            let rpc = rpc_cb.clone();
            let records = records_cb.clone();
            let program = program_cb.clone();

            async move {
                let slot = notification.context.slot;
                let signature = notification.value.signature.clone();
                let err_str = notification.value.err.as_ref().map(|e| e.to_string());
                let success = err_str.is_none();

                println!(
                    "[{program}] slot={slot} {} sig={signature}",
                    if success { "ok" } else { "FAIL" }
                );

                let (sol_changes, token_changes) =
                    fetch_balance_changes(&rpc, &signature).await.unwrap_or_default();

                let tx = Transaction {
                    slot,
                    signature,
                    success,
                    err: err_str,
                    logs: notification.value.logs,
                    sol_changes,
                    token_changes,
                };

                records.lock().unwrap().push((idx, tx));
            }
        },
    )
    .await
    .context("subscribe to program logs")?;

    eprintln!("[sub] subscribed — will write logs to {log_file:?} on completion");

    // Wrap the subscription handle in an outer task that writes the log file
    // and accumulates stats once all callbacks have completed.
    let join_handle = tokio::spawn(async move {
        sub_handle.await.ok();

        let mut records = records.lock().unwrap().drain(..).collect::<Vec<_>>();
        records.sort_by_key(|(idx, _)| *idx);

        eprintln!("[sub] waiting for {} getTransaction tasks...", records.len());

        match File::create(&log_file) {
            Ok(file) => {
                let mut writer = BufWriter::new(file);
                for (_, rec) in &records {
                    write_tx_record(&mut writer, rec);
                }
                eprintln!("[sub] wrote {} records to {log_file:?}", records.len());
            }
            Err(e) => eprintln!("[sub] failed to create {log_file:?}: {e}"),
        }

        let mut s = stats.lock().unwrap();
        for (_, tx) in &records {
            s.total += 1;
            if tx.success {
                s.successes += 1;
            } else {
                s.failures += 1;
            }

            for account in &tx.sol_changes {
                *s.sol_net.entry(account.pubkey.clone()).or_default() += account.delta();
            }

            for account in &tx.token_changes {
                let entry = s
                    .token_net
                    .entry((account.pubkey.clone(), account.mint.clone()))
                    .or_insert((0, account.decimals));
                entry.0 += account.delta();
            }
        }

        eprintln!("[sub] done");
    });

    Ok((join_handle, stop))
}

// ── Log writing ────────────────────────────────────────────────────────────────

fn write_tx_record(writer: &mut impl Write, tx: &Transaction) {
    let status = if tx.success { "OK" } else { "FAIL" };
    let _ = writeln!(writer, "slot={} sig={} status={}", tx.slot, tx.signature, status);
    if let Some(e) = &tx.err {
        let _ = writeln!(writer, "  err={e}");
    }
    for log in &tx.logs {
        let _ = writeln!(writer, "  {log}");
    }

    if !tx.sol_changes.is_empty() || !tx.token_changes.is_empty() {
        let _ = writeln!(writer, "  ---");
    }
    for account in &tx.sol_changes {
        let _ = writeln!(
            writer,
            "  sol  {}  {:.9} → {:.9} SOL  (Δ {:+.9})",
            account.pubkey,
            account.pre_lamports as f64 / 1e9,
            account.post_lamports as f64 / 1e9,
            account.delta() as f64 / 1e9,
        );
    }
    for account in &tx.token_changes {
        let _ = writeln!(
            writer,
            "  spl  {}  {}  {:.prec$} → {:.prec$}  (Δ {:+.prec$})  owner={}",
            account.pubkey,
            account.mint,
            account.to_ui(account.pre_amount),
            account.to_ui(account.post_amount),
            account.to_ui(account.delta().unsigned_abs()),
            account.owner,
            prec = account.decimals as usize,
        );
    }
    let _ = writeln!(writer); // blank line between transactions
}
