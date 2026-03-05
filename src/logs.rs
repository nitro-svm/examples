use std::{
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use solana_client::nonblocking::{pubsub_client::PubsubClient, rpc_client::RpcClient};
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

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
) -> Result<(JoinHandle<()>, tokio::sync::watch::Sender<bool>)> {
    let ws_url = http_url_to_ws(rpc_endpoint)?;
    let program = program.to_string();
    let rpc_endpoint = rpc_endpoint.to_string();

    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        eprintln!("[sub] connecting to {ws_url}");

        let rpc = Arc::new(RpcClient::new_with_commitment(
            rpc_endpoint.clone(),
            CommitmentConfig::confirmed(),
        ));

        let client = match PubsubClient::new(&ws_url).await {
            Ok(c) => c,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("connect failed: {e}")));
                return;
            }
        };

        let (mut stream, _unsubscribe) = match client
            .logs_subscribe(
                RpcTransactionLogsFilter::Mentions(vec![program.clone()]),
                RpcTransactionLogsConfig {
                    commitment: Some(CommitmentConfig::confirmed()),
                },
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("logs_subscribe failed: {e}")));
                return;
            }
        };

        let _ = ready_tx.send(Ok(()));
        eprintln!("[sub] subscribed — will write logs to {log_file:?} on completion");

        // Each spawned task returns (arrival_index, TxRecord),
        // so we can sort after all concurrent fetches complete.
        let mut tasks: Vec<JoinHandle<(usize, Transaction)>> = Vec::new();

        // Spawn a fetch task for one notification.
        let spawn_task = |resp: solana_client::rpc_response::Response<
            solana_client::rpc_response::RpcLogsResponse,
        >,
                              idx: usize| {
            let rpc = Arc::clone(&rpc);
            let program = program.clone();
            tokio::spawn(async move {
                let slot = resp.context.slot;
                let signature = resp.value.signature.clone();
                let err_str = resp.value.err.as_ref().map(|e| e.to_string());
                let success = err_str.is_none();

                println!(
                    "[{program}] slot={slot} {} sig={signature}",
                    if success { "ok" } else { "FAIL" }
                );

                let (sol_changes, token_changes) =
                    fetch_balance_changes(&rpc, &signature).await.unwrap_or_default();

                (
                    idx,
                    Transaction {
                        slot,
                        signature,
                        success,
                        err: err_str,
                        logs: resp.value.logs,
                        sol_changes,
                        token_changes,
                    },
                )
            })
        };

        // Main receive loop.
        loop {
            if *stop_rx.borrow() {
                eprintln!("[sub] stop signaled — draining buffered notifications...");
                loop {
                    match timeout(Duration::from_secs(30), stream.next()).await {
                        Ok(Some(resp)) => {
                            let idx = tasks.len();
                            tasks.push(spawn_task(resp, idx));
                        }
                        _ => break,
                    }
                }
                break;
            }

            let resp = tokio::select! {
                r = stream.next() => r,
                _ = stop_rx.changed() => continue,
            };

            match resp {
                Some(resp) => {
                    let idx = tasks.len();
                    tasks.push(spawn_task(resp, idx));
                }
                None => break,
            }
        }

        // Await all in-flight getTransaction tasks.
        eprintln!("[sub] waiting for {} getTransaction tasks...", tasks.len());
        let mut records: Vec<(usize, Transaction)> = Vec::with_capacity(tasks.len());
        for task in tasks {
            match task.await {
                Ok(rec) => records.push(rec),
                Err(e) => eprintln!("[sub] fetch task panicked: {e}"),
            }
        }

        // Restore arrival order.
        records.sort_by_key(|(idx, _)| *idx);

        // Write all records to file at once.
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

        // Accumulate stats from collected records.
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

    ready_rx
        .await
        .context("subscribe_logs task dropped before signaling ready")?
        .map_err(|e| anyhow!("{e}"))?;

    Ok((handle, stop_tx))
}

/// http://host/path → ws://host/path  |  https → wss
fn http_url_to_ws(url: &str) -> Result<String> {
    let mut parsed = url::Url::parse(url).context("invalid RPC endpoint URL")?;
    let scheme = match parsed.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => bail!("unexpected scheme: {other}"),
    };
    parsed
        .set_scheme(scheme)
        .map_err(|()| anyhow!("failed to set WebSocket scheme"))?;
    Ok(parsed.to_string())
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
