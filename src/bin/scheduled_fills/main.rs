//! Execute provider-built swap transactions at their quote slot and the slots after it.
//!
//! Each input sample is a quote a provider returned at `rpc_slot`, along with the
//! transaction it built. One `ScheduledAction` per sample fires that transaction after
//! `rpc_slot ..= rpc_slot + window`, and the `ActionResult`
//! notifications it produces are the fills, keyed back to the sample by the action's label.

use backtest_example::utils::accounts::{
    empty_token_override, native_override, native_seed_lamports, token_override,
};
use backtest_example::utils::connection::ConnectionArgs;
use backtest_example::utils::parse::{
    WSOL_MINT, derive_ata, extract_signer, set_compute_unit_limit,
};
use backtest_example::utils::session::drive_to_completion;

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, BufWriter, Write as _};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::Parser;
use serde::Deserialize;
use simulator_api::{AccountModifications, ActionAnchor, ActionKind, ScheduledAction};
use simulator_client::{CreateSession, ManagedBacktestSession, ManagedEvent, backtest_ws_url};
use solana_account_decoder::UiAccount;
use solana_address::Address;
use solana_transaction::versioned::VersionedTransaction;

#[derive(Parser)]
#[command(about = "Execute provider quotes at and after their quote slot")]
struct Cli {
    #[command(flatten)]
    conn: ConnectionArgs,
    /// JSONL of quote samples exported from the parquet (see README).
    #[arg(long)]
    input: String,
    /// First slot of the session; samples quoted before it are skipped.
    #[arg(long)]
    start_slot: u64,
    /// Last slot of the session; samples whose window runs past it are skipped.
    #[arg(long)]
    end_slot: u64,
    /// Number of slots after the quote slot to also execute at.
    #[arg(long, default_value_t = 10)]
    window: u64,
    #[arg(long, default_value = "fills.csv")]
    output: String,
    /// Compute unit limit set on every transaction. Titan's transactions set none, so they would
    /// otherwise run on the 200k default; other providers' own limits are replaced.
    #[arg(long, default_value_t = 1_400_000)]
    cu_limit: u32,
    /// Execute every historical transaction instead of rebuilding state from recorded
    /// account deltas (the default). Much slower; for ranges with only `transactions` data.
    #[arg(long)]
    replay_transactions: bool,
}

/// One row of the quote-sample export.
#[derive(Deserialize)]
struct Sample {
    sample_id: String,
    provider: String,
    input_mint: String,
    output_mint: String,
    in_amount: u64,
    out_amount: u64,
    /// Null for providers that don't report one.
    min_out_amount: Option<u64>,
    /// The sampler's slot when the quote was requested, shared by every provider for a
    /// `sample_id`. Only some providers report their own context slot, so it's the common anchor.
    rpc_slot: u64,
    transaction: String,
}

struct Fill {
    sample_id: String,
    provider: String,
    input_mint: String,
    output_mint: String,
    in_amount: u64,
    quoted_out: u64,
    min_out: Option<u64>,
    quote_slot: u64,
    exec_slot: u64,
    filled_out: Option<u64>,
    units_consumed: Option<u64>,
    error: Option<String>,
    /// Last program log lines of a failed run, which name the program and reason behind `error`.
    log_tail: Option<String>,
}

// ── input ────────────────────────────────────────────────────────────────────

fn read_samples(cli: &Cli) -> Result<Vec<Sample>> {
    let file = std::fs::File::open(&cli.input).with_context(|| format!("open {}", cli.input))?;
    let mut samples = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let sample: Sample = serde_json::from_str(&line).context("parse sample")?;
        let slot = sample.rpc_slot;
        if slot >= cli.start_slot && slot + cli.window <= cli.end_slot {
            samples.push(sample);
        }
    }
    Ok(samples)
}

/// Provider transactions arrive base64-encoded; fall back to base58 for the ones that aren't.
fn decode_transaction(encoded: &str) -> Result<VersionedTransaction> {
    let bytes = STANDARD
        .decode(encoded)
        .or_else(|_| bs58::decode(encoded).into_vec())
        .context("transaction is neither base64 nor base58")?;
    bincode::deserialize(&bytes).context("deserialize transaction")
}

// ── actions ──────────────────────────────────────────────────────────────────

/// Fund the signer with `in_amount` of the input mint and zero the output side, so the
/// returned post-execution balance *is* the fill. A wSOL input is funded both as native SOL
/// and as a wSOL ATA, since providers differ on whether the transaction wraps its own SOL.
async fn build_action(
    sample: &Sample,
    window: u64,
    cu_limit: u32,
    label: String,
) -> Result<ScheduledAction> {
    let mut tx = decode_transaction(&sample.transaction)?;
    set_compute_unit_limit(&mut tx, cu_limit)?;
    let signer = extract_signer(&tx)?;
    let mut overrides = BTreeMap::new();

    if sample.input_mint == WSOL_MINT {
        let (addr, data) = native_override(&signer, sample.in_amount)?;
        overrides.insert(addr, data);
    } else if sample.output_mint != WSOL_MINT {
        // Neither side is SOL: the signer still pays fees and rent.
        let (addr, data) = native_override(&signer, 0)?;
        overrides.insert(addr, data);
    }
    let (in_addr, in_data) = token_override(&signer, &sample.input_mint, sample.in_amount).await?;
    overrides.insert(in_addr, in_data);

    let (out_addr, out_data) = if sample.output_mint == WSOL_MINT {
        native_override(&signer, 0)?
    } else {
        empty_token_override(&signer, &sample.output_mint).await?
    };
    overrides.insert(out_addr, out_data);

    let mut return_accounts = vec![out_addr];
    if sample.output_mint == WSOL_MINT {
        let wsol_ata: Address = derive_ata(&signer, WSOL_MINT)
            .context("derive wSOL ATA")?
            .to_string()
            .parse()?;
        return_accounts.push(wsol_ata);
    }

    let slots: Vec<u64> =
        (sample.rpc_slot..=sample.rpc_slot + window).collect();
    // The provider's own min_out is kept: a fill that would breach it should fail, as it would onchain.
    let encoded = STANDARD.encode(bincode::serialize(&tx)?);
    Ok(ScheduledAction {
        anchor: ActionAnchor::AfterSlot {
            slots: slots.clone(),
        },
        kind: ActionKind::Simulate,
        transactions: vec![encoded; slots.len()],
        account_overrides: AccountModifications(overrides),
        feeds_reroute: false,
        return_accounts,
        label: Some(label),
    })
}

// ── results ──────────────────────────────────────────────────────────────────

/// Rent-exempt minimum of a token account, subtracted when a wSOL output stays wrapped.
const ATA_RENT_EXEMPT: u64 = 2_039_280;

fn token_amount(account: &serde_json::Value) -> Option<u64> {
    let data = UiAccount::deserialize(account).ok()?.data.decode()?;
    Some(u64::from_le_bytes(data.get(64..72)?.try_into().ok()?))
}

fn lamports(account: &serde_json::Value) -> Option<u64> {
    Some(UiAccount::deserialize(account).ok()?.lamports)
}

/// The fill, read against the zeroed baseline `build_action` set on the output side.
fn read_fill(output_mint: &str, accounts: &[Option<serde_json::Value>]) -> u64 {
    let account = |i: usize| accounts.get(i).and_then(|a| a.as_ref());
    if output_mint != WSOL_MINT {
        return account(0).and_then(token_amount).unwrap_or(0);
    }
    // Unwrapped to native SOL if the transaction closed its wSOL ATA, otherwise still in the ATA.
    let native = account(0)
        .and_then(lamports)
        .map(|l| l.saturating_sub(native_seed_lamports(0)))
        .unwrap_or(0);
    if native > 0 {
        native
    } else {
        account(1)
            .and_then(lamports)
            .map(|l| l.saturating_sub(ATA_RENT_EXEMPT))
            .unwrap_or(0)
    }
}

/// Runs the session, writing each fill to `out` as it arrives. Returns the number of rows written.
async fn run(cli: &Cli, samples: &[Sample], out: &mut FillWriter) -> Result<usize> {
    // `sample_id` is shared by every provider's quote for the same pair, size, and slot, so each
    // action is labeled with its sample's index instead.
    let mut actions = Vec::with_capacity(samples.len());
    for (i, sample) in samples.iter().enumerate() {
        match build_action(sample, cli.window, cli.cu_limit, i.to_string()).await {
            Ok(action) => actions.push(action),
            Err(e) => eprintln!("[skip] {} {}: {e:#}", sample.provider, sample.sample_id),
        }
        if (i + 1) % 500 == 0 {
            eprintln!("[build] {}/{} actions", i + 1, samples.len());
        }
    }
    eprintln!("[session] registering {} actions", actions.len());

    let create = CreateSession::builder()
        .start_slot(cli.start_slot)
        .end_slot(cli.end_slot)
        .disconnect_timeout_secs(900u16)
        .capacity_wait_timeout_secs(900u16)
        .replay_account_state(!cli.replay_transactions)
        .actions(actions)
        .build()
        .into_request()
        .context("building create-session request")?;
    let ws_url = backtest_ws_url(&cli.conn.url);
    let mut session = ManagedBacktestSession::start(ws_url, cli.conn.api_key.clone(), create)
        .await
        .context("starting managed session")?;
    session.subscribe_actions();

    let mut rows = 0;
    let mut write_err = None;
    let slot_count = cli.end_slot - cli.start_slot;
    let mut next_progress = cli.start_slot + 1_000;
    let result = drive_to_completion(&mut session, slot_count, |event| {
        let notification = match event {
            ManagedEvent::ActionResult(notification) => notification,
            ManagedEvent::Slot(slot) if slot >= next_progress => {
                eprintln!(
                    "[slot] {slot} ({}/{slot_count}), {rows} rows written",
                    slot - cli.start_slot
                );
                next_progress = slot - slot % 1_000 + 1_000;
                return;
            }
            _ => return,
        };
        let Some(sample) = notification
            .label
            .as_deref()
            .and_then(|label| label.parse::<usize>().ok())
            .and_then(|i| samples.get(i))
        else {
            return;
        };
        let outcome = notification.transaction_outcomes.first();
        let error = outcome.and_then(|o| o.err.clone());
        let log_tail = outcome
            .filter(|o| o.err.is_some())
            .map(|o| o.logs[o.logs.len().saturating_sub(LOG_TAIL_LINES)..].join(" | "));
        let filled_out = error
            .is_none()
            .then(|| read_fill(&sample.output_mint, &notification.accounts));
        eprintln!(
            "  {} {} slot={} quoted={} filled={:?}",
            sample.provider, sample.sample_id, notification.slot, sample.out_amount, filled_out
        );
        let fill = Fill {
            sample_id: sample.sample_id.clone(),
            provider: sample.provider.clone(),
            input_mint: sample.input_mint.clone(),
            output_mint: sample.output_mint.clone(),
            in_amount: sample.in_amount,
            quoted_out: sample.out_amount,
            min_out: sample.min_out_amount,
            quote_slot: sample.rpc_slot,
            exec_slot: notification.slot,
            filled_out,
            units_consumed: outcome.map(|o| o.units_consumed),
            error,
            log_tail,
        };
        match out.write(&fill) {
            Ok(()) => rows += 1,
            Err(e) => {
                write_err.get_or_insert(e);
            }
        }
    })
    .await;
    session.shutdown().await;
    result?;
    if let Some(e) = write_err {
        return Err(e.context("writing fills"));
    }
    Ok(rows)
}

// ── output ───────────────────────────────────────────────────────────────────

/// Log lines kept per failed run.
const LOG_TAIL_LINES: usize = 6;

/// Quoted so commas in error strings and logs don't break the CSV.
fn csv_quoted(field: Option<&str>) -> String {
    field
        .map(|f| format!("\"{}\"", f.replace('"', "'")))
        .unwrap_or_default()
}

/// CSV of fills, flushed per row so a session that dies partway still leaves its rows on disk.
struct FillWriter(BufWriter<std::fs::File>);

impl FillWriter {
    fn create(path: &str) -> Result<Self> {
        let file = std::fs::File::create(path).with_context(|| format!("create {path}"))?;
        let mut w = BufWriter::new(file);
        writeln!(
            w,
            "sample_id,provider,input_mint,output_mint,in_amount,quoted_out,min_out,quote_slot,exec_slot,slot_offset,filled_out,units_consumed,error,log_tail"
        )?;
        w.flush()?;
        Ok(Self(w))
    }

    fn write(&mut self, f: &Fill) -> Result<()> {
        writeln!(
            self.0,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            f.sample_id,
            f.provider,
            f.input_mint,
            f.output_mint,
            f.in_amount,
            f.quoted_out,
            f.min_out.map(|v| v.to_string()).unwrap_or_default(),
            f.quote_slot,
            f.exec_slot,
            f.exec_slot - f.quote_slot,
            f.filled_out.map(|v| v.to_string()).unwrap_or_default(),
            f.units_consumed.map(|v| v.to_string()).unwrap_or_default(),
            csv_quoted(f.error.as_deref()),
            csv_quoted(f.log_tail.as_deref()),
        )?;
        self.0.flush()?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let cli = Cli::parse();

    let samples = read_samples(&cli)?;
    eprintln!(
        "[input] {} samples quoted in {}..={}",
        samples.len(),
        cli.start_slot,
        cli.end_slot - cli.window
    );
    if samples.is_empty() {
        return Ok(());
    }

    let mut out = FillWriter::create(&cli.output)?;
    let rows = run(&cli, &samples, &mut out).await?;
    eprintln!("[done] wrote {} rows to {}", rows, cli.output);
    Ok(())
}
