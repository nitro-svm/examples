//! Run OKX's router (Pallas) against a backtest session and simulate the swaps it builds.

mod pallas;

use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::Parser;
use futures::{StreamExt as _, stream};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_with::{DisplayFromStr, serde_as};
use simulator_api::{
    AvailableRange, BacktestRequest, BacktestResponse, BundleKind, ContinueToParams,
    DiscoveryBatchEvent, DiscoveryFilter,
};
use simulator_client::{BacktestClient, BacktestSession, CreateSession, backtest_ws_url};
use solana_address::Address;
use solana_address_lookup_table_interface::state::AddressLookupTable;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_instruction::Instruction;
use solana_message::{AddressLookupTableAccount, VersionedMessage, v0};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::{config::RpcSimulateTransactionConfig, response::UiTransactionError};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use tokio::process::Child;

use backtest_example::utils::{connection::ConnectionArgs, parse::extract_signer};

use pallas::{Answer, Built, Endpoints};

const OKX_ROUTER: Address = Address::from_str_const("proVF4pMXVaYqmy4NjniPh4pqKNfMmsihgd4wdkCX3u");
const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
const CASES: &str = include_str!("cases.json");
const SESSION_WAIT_SECS: u16 = 900;
const STEP_TIMEOUT: Duration = Duration::from_secs(300);
const READY_TIMEOUT: Duration = Duration::from_secs(1800);
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(about = "Route swaps through OKX's Pallas against replayed mainnet state")]
struct Cli {
    #[command(flatten)]
    conn: ConnectionArgs,

    /// Pallas binary to start per case; unset, the example waits for one you start yourself.
    #[arg(long)]
    pallas_bin: Option<PathBuf>,

    #[command(flatten)]
    pallas: Endpoints,

    /// Run only the cases whose name or original signature is given.
    #[arg(long = "case")]
    cases: Vec<String>,

    /// Output directory, one subdirectory per case.
    #[arg(long, default_value = "pallas-out")]
    output: PathBuf,
}

#[serde_as]
#[derive(Deserialize)]
struct Case {
    name: String,
    #[serde_as(as = "DisplayFromStr")]
    signature: Signature,
    observed_error: String,
    route: String,
    quote_slot: u64,
    land_slot: u64,
    request: Box<RawValue>,
}

impl Case {
    fn is_named(&self, wanted: &str) -> bool {
        wanted == self.name || wanted == self.signature.to_string()
    }
}

fn load_cases() -> Result<Vec<Case>> {
    let cases: Vec<Case> = serde_json::from_str(CASES).context("parse cases.json")?;
    if let Some(case) = cases.iter().find(|case| case.land_slot < case.quote_slot) {
        bail!(
            "{} lands at slot {}, before its quote slot {}",
            case.name,
            case.land_slot,
            case.quote_slot
        );
    }
    Ok(cases)
}

enum Outcome {
    NoRoute {
        code: String,
        message: String,
    },
    Succeeded {
        units: Option<u64>,
    },
    Failed {
        error: UiTransactionError,
        failed: Option<String>,
    },
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRoute { code, message } => write!(f, "no route: {code} {message}"),
            Self::Succeeded { units: Some(units) } => write!(f, "succeeded ({units} CU)"),
            Self::Succeeded { units: None } => write!(f, "succeeded"),
            Self::Failed { error, failed } => {
                write!(f, "{error:?}")?;
                failed.iter().try_for_each(|line| write!(f, " — {line}"))
            }
        }
    }
}

const DEFAULT_ENV_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/bin/pallas/.env");

fn load_env_file() -> Result<()> {
    match std::env::var_os("PALLAS_ENV_FILE") {
        Some(path) => {
            dotenvy::from_path(&path).with_context(|| format!("load {}", path.display()))?;
        }
        None if Path::new(DEFAULT_ENV_FILE).exists() => {
            dotenvy::from_path(DEFAULT_ENV_FILE)
                .with_context(|| format!("load {DEFAULT_ENV_FILE}"))?;
        }
        None => {}
    }
    Ok(())
}

fn main() -> Result<()> {
    load_env_file()?;
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let cli = Cli::parse();
    let cases = load_cases()?;
    let unknown: Vec<&String> = cli
        .cases
        .iter()
        .filter(|wanted| !cases.iter().any(|case| case.is_named(wanted)))
        .collect();
    if !unknown.is_empty() {
        bail!("no case matches {unknown:?}");
    }
    let selected = cases.iter().filter(|case| {
        cli.cases.is_empty() || cli.cases.iter().any(|wanted| case.is_named(wanted))
    });

    let client = BacktestClient::builder()
        .url(backtest_ws_url(&cli.conn.url))
        .api_key(cli.conn.api_key.clone())
        .build();
    let http = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?;
    let ranges = client.available_ranges().await?;

    let outcomes: Vec<(&Case, String)> = stream::iter(selected)
        .then(|case| {
            let (cli, client, http, ranges) = (&cli, &client, &http, &ranges);
            async move {
                eprintln!("\n=== {} ({})", case.name, case.signature);
                eprintln!("our run routed it {}", case.route);
                let outcome = match run_case(cli, client, http, ranges, case).await {
                    Ok(outcome) => outcome.to_string(),
                    Err(error) => format!("example error: {error:#}"),
                };
                eprintln!("--> {outcome}");
                (case, outcome)
            }
        })
        .collect()
        .await;

    println!("\n{:<28} {:<48} this run", "case", "observed in our run");
    for (case, outcome) in outcomes {
        println!("{:<28} {:<48} {outcome}", case.name, case.observed_error);
    }
    Ok(())
}

async fn run_case(
    cli: &Cli,
    client: &BacktestClient,
    http: &reqwest::Client,
    ranges: &[AvailableRange],
    case: &Case,
) -> Result<Outcome> {
    let dir = cli.output.join(&case.name);
    std::fs::create_dir_all(&dir)?;
    let start_slot = session_start(ranges, case)?;
    let mut session = client
        .create_session(
            CreateSession::builder()
                .start_slot(start_slot)
                .end_slot(case.land_slot)
                .disconnect_timeout_secs(SESSION_WAIT_SECS)
                .capacity_wait_timeout_secs(SESSION_WAIT_SECS)
                .discoveries(vec![DiscoveryFilter::ProgramExecuted(OKX_ROUTER)])
                .build(),
        )
        .await?;
    let outcome = drive_case(cli, http, &mut session, start_slot, case, &dir).await;
    let _ = session.close(Some(CLOSE_TIMEOUT)).await;
    outcome
}

async fn drive_case(
    cli: &Cli,
    http: &reqwest::Client,
    session: &mut BacktestSession,
    start_slot: u64,
    case: &Case,
    dir: &Path,
) -> Result<Outcome> {
    session
        .ensure_ready(Some(Duration::from_secs(SESSION_WAIT_SECS.into())))
        .await?;
    let mut replay = Replay {
        session,
        at: Position::block_start(start_slot),
        original: case.signature,
        found: None,
    };
    replay
        .continue_to(Position::block_start(case.quote_slot))
        .await?;
    let rpc_url = replay
        .session
        .rpc_endpoint()
        .context("session has no RPC endpoint")?
        .to_string();
    eprintln!("session paused at slot {}; RPC {rpc_url}", case.quote_slot);

    let built = match quote(cli, http, &rpc_url, case, dir).await? {
        Answer::Rejected { code, message } => return Ok(Outcome::NoRoute { code, message }),
        Answer::Built(built) => built,
    };
    eprintln!("pallas quoted {} out", built.quoted_out);

    let fee_payer = replay.advance_to_original(case.land_slot).await?;
    eprintln!(
        "paused before the original at slot {} batch {}",
        replay.at.slot, replay.at.batch
    );
    simulate(replay.session.rpc(), &fee_payer, built, dir).await
}

/// Kills and awaits a Pallas this example started, so the next case can bind its ports.
async fn quote(
    cli: &Cli,
    http: &reqwest::Client,
    rpc_url: &str,
    case: &Case,
    dir: &Path,
) -> Result<Answer> {
    let mut child = match &cli.pallas_bin {
        Some(binary) => {
            let log = dir.join("pallas.log");
            eprintln!("starting {} (log: {})", binary.display(), log.display());
            Some(pallas::spawn(
                binary,
                rpc_url,
                &cli.pallas,
                &dir.join("pallas-data"),
                &log,
            )?)
        }
        None => {
            eprintln!("start Pallas with this environment, then it is picked up once ready:");
            for (key, value) in pallas::frozen_chain_env(rpc_url) {
                eprintln!("  {key}={value}");
            }
            pallas::wait_restarted(http, &cli.pallas).await?;
            None
        }
    };
    let answer = request_quote(http, &cli.pallas, child.as_mut(), case, dir).await;
    if let Some(child) = &mut child {
        child.kill().await?;
    }
    answer
}

async fn request_quote(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    child: Option<&mut Child>,
    case: &Case,
    dir: &Path,
) -> Result<Answer> {
    pallas::wait_ready(http, endpoints, child, READY_TIMEOUT).await?;
    std::fs::write(dir.join("request.json"), case.request.get())?;
    let (status, body) = pallas::swap_instruction(http, endpoints, &case.request).await?;
    std::fs::write(dir.join("response.json"), &body)?;
    pallas::decode(status, &body)
}

async fn simulate(
    rpc: &RpcClient,
    fee_payer: &Address,
    built: Built,
    dir: &Path,
) -> Result<Outcome> {
    let tables = fetch_lookup_tables(rpc, &built.lookup_tables).await?;
    let instructions: Vec<_> = std::iter::once(ComputeBudgetInstruction::set_compute_unit_limit(
        MAX_COMPUTE_UNIT_LIMIT,
    ))
    .chain(
        built
            .instructions
            .into_iter()
            .filter(|ix| !sets_compute_unit_limit(ix)),
    )
    .collect();
    let blockhash = rpc.get_latest_blockhash().await?;
    let message = v0::Message::try_compile(fee_payer, &instructions, &tables, blockhash)?;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); usize::from(message.header.num_required_signatures)],
        message: VersionedMessage::V0(message),
    };
    std::fs::write(
        dir.join("transaction.b64"),
        BASE64.encode(wincode::serialize(&tx)?),
    )?;

    let simulated = rpc
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                ..RpcSimulateTransactionConfig::default()
            },
        )
        .await?
        .value;
    std::fs::write(
        dir.join("simulation.json"),
        serde_json::to_vec_pretty(&simulated)?,
    )?;

    Ok(match simulated.err {
        None => Outcome::Succeeded {
            units: simulated.units_consumed,
        },
        Some(error) => Outcome::Failed {
            error,
            failed: simulated
                .logs
                .unwrap_or_default()
                .into_iter()
                .rev()
                .find(|line| line.contains(" failed: ")),
        },
    })
}

fn sets_compute_unit_limit(ix: &Instruction) -> bool {
    ix.program_id == solana_compute_budget_interface::id()
        && ix.data.first()
            == ComputeBudgetInstruction::set_compute_unit_limit(0)
                .data
                .first()
}

/// A session can only start where a bundle does.
fn session_start(ranges: &[AvailableRange], case: &Case) -> Result<u64> {
    ranges
        .iter()
        .filter(|range| {
            range.kind.is_none_or(|kind| kind == BundleKind::Blocks)
                && range.bundle_start_slot <= case.quote_slot
                && range
                    .max_bundle_end_slot
                    .is_none_or(|end| case.land_slot <= end)
        })
        .map(|range| range.bundle_start_slot)
        .max()
        .with_context(|| {
            format!(
                "no available range covers slots {}..={}",
                case.quote_slot, case.land_slot
            )
        })
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    slot: u64,
    batch: u32,
}

impl Position {
    fn block_start(slot: u64) -> Self {
        Self { slot, batch: 0 }
    }
}

#[derive(Clone, Copy)]
struct OriginalBatch {
    at: Position,
    fee_payer: Address,
}

/// The server announces each matching batch once, ahead of execution, so every response is
/// checked.
struct Replay<'a> {
    session: &'a mut BacktestSession,
    at: Position,
    original: Signature,
    found: Option<OriginalBatch>,
}

impl Replay<'_> {
    /// Already-reached targets are skipped: the server refuses to step to one.
    async fn continue_to(&mut self, to: Position) -> Result<()> {
        if to <= self.at {
            return Ok(());
        }
        self.session
            .send(
                &BacktestRequest::ContinueTo(ContinueToParams {
                    slot: to.slot,
                    batch_index: Some(to.batch),
                }),
                Some(STEP_TIMEOUT),
            )
            .await?;
        loop {
            match self.next_response().await? {
                BacktestResponse::Paused(paused) => {
                    self.at = Position {
                        slot: paused.slot,
                        batch: paused.batch_index.unwrap_or(0),
                    };
                    return Ok(());
                }
                BacktestResponse::Completed { .. } => {
                    bail!("session ended before slot {}", to.slot)
                }
                _ => {}
            }
        }
    }

    /// Steps to the land slot first so that the server's discovery scan reaches the original.
    async fn advance_to_original(&mut self, land_slot: u64) -> Result<Address> {
        self.continue_to(Position::block_start(land_slot)).await?;
        loop {
            if let Some(found) = self.found {
                self.continue_to(found.at).await?;
                return Ok(found.fee_payer);
            }
            if let BacktestResponse::Completed { .. } = self.next_response().await? {
                bail!("session ended before {} ran", self.original);
            }
        }
    }

    async fn next_response(&mut self) -> Result<BacktestResponse> {
        let response = self
            .session
            .next_response(Some(STEP_TIMEOUT))
            .await?
            .context("session closed")?;
        match &response {
            BacktestResponse::Error(err) => bail!("session error: {err:?}"),
            BacktestResponse::DiscoveryBatch(event) if self.found.is_none() => {
                self.found = find_original(event, &self.original)?;
            }
            _ => {}
        }
        Ok(response)
    }
}

fn find_original(
    event: &DiscoveryBatchEvent,
    original: &Signature,
) -> Result<Option<OriginalBatch>> {
    for encoded in &event.transactions {
        // A `TxWithMeta` starts with its transaction; unlike bincode, wincode reads v1 transactions.
        let tx: VersionedTransaction =
            wincode::deserialize(&encoded.decode()?).context("decode a discovered transaction")?;
        if tx.signatures.first() == Some(original) {
            return Ok(Some(OriginalBatch {
                at: Position {
                    slot: event.slot,
                    batch: event.batch_index,
                },
                fee_payer: extract_signer(&tx)?,
            }));
        }
    }
    Ok(None)
}

async fn fetch_lookup_tables(
    rpc: &RpcClient,
    addresses: &[Address],
) -> Result<Vec<AddressLookupTableAccount>> {
    let accounts = rpc.get_multiple_accounts(addresses).await?;
    addresses
        .iter()
        .zip(accounts)
        .map(|(&key, account)| {
            let account = account.with_context(|| format!("lookup table {key} not found"))?;
            let table = AddressLookupTable::deserialize(&account.data)
                .with_context(|| format!("lookup table {key}"))?;
            Ok(AddressLookupTableAccount {
                key,
                addresses: table.addresses.to_vec(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_cases_parse_and_are_quoted_before_they_land() {
        assert!(!load_cases().expect("cases load").is_empty());
    }

    fn range(start: u64, end: u64, kind: BundleKind) -> AvailableRange {
        AvailableRange {
            bundle_start_slot: start,
            max_bundle_end_slot: Some(end),
            kind: Some(kind),
            ..AvailableRange::default()
        }
    }

    fn case(quote_slot: u64, land_slot: u64) -> Case {
        let mut case = load_cases().expect("cases load").remove(0);
        case.quote_slot = quote_slot;
        case.land_slot = land_slot;
        case
    }

    #[test]
    fn a_session_starts_on_the_latest_bundle_covering_quote_and_land() {
        let ranges = [
            range(100, 199, BundleKind::Blocks),
            range(150, 299, BundleKind::Blocks),
            range(170, 299, BundleKind::AccountState),
            range(160, 165, BundleKind::Blocks),
        ];
        assert_eq!(
            session_start(&ranges, &case(180, 190)).expect("covered"),
            150
        );
    }

    #[test]
    fn no_bundle_covering_both_slots_is_an_error() {
        let ranges = [range(100, 150, BundleKind::Blocks)];
        assert!(session_start(&ranges, &case(120, 160)).is_err());
    }
}
