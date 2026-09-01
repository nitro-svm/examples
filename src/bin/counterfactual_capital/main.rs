//! Counterfactual Capital: price a venue against the hops that already happened, with its own
//! inventory scaled up, and report how much of that flow more capital would have won.
//!
//! Sibling to `counterfactual_flow`, which changes what a venue *quotes*. This one changes what it
//! *holds*, and asks the question a desk allocates against: where does the next dollar of
//! committed inventory stop buying flow?

mod cli;
mod jsonl;
mod session;
mod vault;

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use clap::Parser;
use simulator_client::ManagedBacktestSession;
use solana_account::Account;
use solana_address::Address;
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use crate::{
    cli::{Cli, ConnectionArgs},
    jsonl::{ArmRow, VaultRow},
    session::Arm,
};

/// `decimals` in the SPL mint layout, after a 36-byte COption mint authority and an 8-byte supply.
const MINT_DECIMALS_OFFSET: usize = 44;

/// A vault, its baseline, and what its mint calls the units.
struct Vault {
    address: Address,
    account: Account,
    mint: Address,
    decimals: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Cli::parse();
    let spec = args.spec()?;
    let arms = args.arms()?;

    eprintln!(
        "[venue] {} via {}, pair {}",
        spec.venue,
        spec.aggregator.as_str(),
        spec.pair
    );
    eprintln!(
        "[range] {} + {} slots, {}",
        args.start_slot,
        args.slot_count,
        match args.no_replay {
            true => "executed",
            false => "replayed",
        }
    );

    // The control runs first because every other arm's override is a multiple of what it reads:
    // the venue's own inventory at the start slot, which only a session can serve.
    let (control, vaults) = run_control(&args, &spec).await?;
    report_baseline(&vaults);

    if args.dry_run {
        report_dry_run(&arms, &vaults);
        return Ok(());
    }

    let mut rows = vec![control];
    for multiple in arms.iter().copied().filter(|m| *m != 1.0) {
        rows.push(run_arm(&args, &spec, &vaults, multiple).await?);
    }
    rows.sort_by(|a, b| a.multiple.partial_cmp(&b.multiple).expect("finite multiples"));

    write_rows(&args, &rows)?;
    report(&rows, &vaults);
    Ok(())
}

/// The control arm, which posts nothing, plus the inventory every other arm scales from.
async fn run_control(args: &Cli, spec: &simulator_api::DirectFillParams) -> Result<(ArmRow, Vec<Vault>)> {
    let create = session::create_session(Arm {
        start_slot: args.start_slot,
        slot_count: args.slot_count,
        no_replay: args.no_replay,
        spec: spec.clone(),
        overrides: BTreeMap::new(),
    })?;
    let mut session = ManagedBacktestSession::start(
        args.conn.websocket_url(),
        args.conn.api_key.clone(),
        create,
    )
    .await?;

    let rpc_url = args.conn.rpc_url(&session.session_info().rpc_endpoint);
    let vaults = read_vaults(&args.conn, &rpc_url, &args.vaults).await?;

    eprintln!("[arm] 1x (control)");
    let stats = session::drive_to_completion(&mut session, args.slot_count).await?;
    Ok((row(1.0, &vaults, &vaults_at(&vaults, 1.0)?, stats), vaults))
}

/// One scaled arm.
async fn run_arm(
    args: &Cli,
    spec: &simulator_api::DirectFillParams,
    vaults: &[Vault],
    multiple: f64,
) -> Result<ArmRow> {
    let scaled = vaults_at(vaults, multiple)?;
    let overrides = vaults
        .iter()
        .zip(&scaled)
        .map(|(vault, scaled)| (vault.address, scaled.account.clone()))
        .collect::<BTreeMap<_, _>>();

    let create = session::create_session(Arm {
        start_slot: args.start_slot,
        slot_count: args.slot_count,
        no_replay: args.no_replay,
        spec: spec.clone(),
        overrides,
    })?;
    let mut session = ManagedBacktestSession::start(
        args.conn.websocket_url(),
        args.conn.api_key.clone(),
        create,
    )
    .await?;

    eprintln!("[arm] {}", format_multiple(multiple));
    let stats = session::drive_to_completion(&mut session, args.slot_count).await?;
    Ok(row(multiple, vaults, &scaled, stats))
}

fn vaults_at(vaults: &[Vault], multiple: f64) -> Result<Vec<vault::ScaledVault>> {
    vaults
        .iter()
        .map(|vault| {
            vault::scale(&vault.account, multiple)
                .with_context(|| format!("scaling vault {} by {multiple}x", vault.address))
        })
        .collect()
}

fn row(
    multiple: f64,
    vaults: &[Vault],
    scaled: &[vault::ScaledVault],
    stats: simulator_api::RerouteStatsReport,
) -> ArmRow {
    ArmRow {
        multiple,
        vaults: vaults
            .iter()
            .zip(scaled)
            .map(|(vault, scaled)| VaultRow {
                address: vault.address.to_string(),
                mint: vault.mint.to_string(),
                decimals: vault.decimals,
                before: scaled.before,
                after: scaled.after,
                native: scaled.native,
            })
            .collect(),
        matched: stats.direct_fill_matched,
        built: stats.direct_fill_built,
        scored: stats.direct_fill_scored,
        bps_total: stats.direct_fill_bps_total,
        rejections: stats.direct_fill_rejections,
        outcomes: stats.direct_fill_outcomes,
    }
}

/// The vaults plus the decimals their mints declare, so the capital column is readable.
async fn read_vaults(
    conn: &ConnectionArgs,
    rpc_url: &str,
    addresses: &[Address],
) -> Result<Vec<Vault>> {
    let _ = conn;
    let read = session::read_vaults(rpc_url, addresses).await?;
    let mints = read
        .iter()
        .map(|(address, account)| {
            let mint: [u8; 32] = account
                .data
                .get(0..32)
                .and_then(|slice| slice.try_into().ok())
                .with_context(|| format!("vault {address} is too short to name a mint"))?;
            Address::from(mint)
                .to_string()
                .parse::<Pubkey>()
                .map_err(anyhow::Error::new)
        })
        .collect::<Result<Vec<_>>>()?;

    let client = RpcClient::new(rpc_url.to_string());
    let mint_accounts = client
        .get_multiple_accounts(&mints)
        .await
        .context("reading the vaults' mints for their decimals")?;

    read.into_iter()
        .zip(mints)
        .zip(mint_accounts)
        .map(|(((address, account), mint), mint_account)| {
            let decimals = mint_account
                .and_then(|mint| mint.data.get(MINT_DECIMALS_OFFSET).copied())
                .with_context(|| format!("mint {mint} does not declare decimals"))?;
            Ok(Vault {
                address,
                account,
                mint: mint.to_string().parse()?,
                decimals,
            })
        })
        .collect()
}

fn format_multiple(multiple: f64) -> String {
    match multiple.fract() == 0.0 {
        true => format!("{multiple:.0}x"),
        false => format!("{multiple}x"),
    }
}

fn format_amount(base_units: u64, decimals: u8) -> String {
    let scaled = base_units as f64 / 10f64.powi(i32::from(decimals));
    match scaled >= 1000.0 {
        true => format!("{scaled:.0}"),
        false => format!("{scaled:.2}"),
    }
}

fn report_baseline(vaults: &[Vault]) {
    for vault in vaults {
        let amount = u64::from_le_bytes(
            vault.account.data[64..72]
                .try_into()
                .expect("a checked token account"),
        );
        eprintln!(
            "[baseline] {} holds {} of {}",
            short(&vault.address.to_string()),
            format_amount(amount, vault.decimals),
            short(&vault.mint.to_string())
        );
    }
}

fn report_dry_run(arms: &[f64], vaults: &[Vault]) {
    println!("arms that would run, and the inventory each would post:");
    for multiple in arms {
        let scaled = vaults_at(vaults, *multiple);
        let posted = match &scaled {
            Ok(scaled) => vaults
                .iter()
                .zip(scaled)
                .map(|(vault, scaled)| format_amount(scaled.after, vault.decimals))
                .collect::<Vec<_>>()
                .join(" / "),
            Err(error) => format!("would fail: {error}"),
        };
        println!("  {:>6}  {posted}", format_multiple(*multiple));
    }
}

fn short(address: &str) -> String {
    match address.len() > 10 {
        true => format!("{}..{}", &address[..6], &address[address.len() - 4..]),
        false => address.to_string(),
    }
}

fn write_rows(args: &Cli, rows: &[ArmRow]) -> Result<()> {
    let body = rows
        .iter()
        .map(|row| serde_json::to_string(row).map_err(anyhow::Error::new))
        .collect::<Result<Vec<_>>>()?
        .join("\n");
    std::fs::write(&args.out, format!("{body}\n"))
        .with_context(|| format!("writing {}", args.out.display()))?;
    eprintln!("[out] {} arms written to {}", rows.len(), args.out.display());
    Ok(())
}

/// The table, and every guard that separates a real curve from a broken one.
fn report(rows: &[ArmRow], vaults: &[Vault]) {
    let Some(control) = rows.iter().find(|row| row.multiple == 1.0) else {
        return;
    };

    println!();
    println!(
        "[run] {} buildable hops — the denominator below",
        control.built
    );
    println!();
    let header = vaults
        .iter()
        .map(|vault| short(&vault.mint.to_string()))
        .collect::<Vec<_>>()
        .join(" / ");
    println!("  {:>12}  {:>28}  {:>9}", "multiple", header, "fill rate");
    for row in rows {
        let capital = row
            .vaults
            .iter()
            .map(|vault| format_amount(vault.after, vault.decimals))
            .collect::<Vec<_>>()
            .join(" / ");
        let label = match row.multiple == 1.0 {
            true => format!("{} (control)", format_multiple(row.multiple)),
            false => format_multiple(row.multiple),
        };
        println!(
            "  {:>12}  {capital:>28}  {:>8.1}%",
            label,
            row.fill_rate() * 100.0
        );
    }
    println!();

    report_bps(rows, control);
    report_guards(rows, control);
}

/// bps sits below the table, with the caveat travelling next to it: the scored population differs
/// between arms, so the column is not like-for-like and a falling mean is not a worse price.
fn report_bps(rows: &[ArmRow], control: &ArmRow) {
    let lines = rows
        .iter()
        .filter_map(|row| {
            row.mean_bps()
                .map(|bps| format!("{} {bps:+.1}", format_multiple(row.multiple)))
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        println!("No arm scored a probe, so there is no price to report.");
        return;
    }
    println!("Mean bps against what each hop actually filled at: {}.", lines.join(", "));
    if rows.iter().any(|row| row.scored != control.scored) {
        println!(
            "Read across arms with care: each arm scores a different set of hops — a higher arm \
             fills hops a lower one could not — and the marginal fill is usually the worst-priced \
             one, so the mean can fall while the flow won rises."
        );
    }
}

fn report_guards(rows: &[ArmRow], control: &ArmRow) {
    // Detection does not read overrides, so the admitted population is a property of the range.
    // Drift means the arms are not comparable and every difference below is partly noise.
    if let Some(drift) = rows
        .iter()
        .find(|row| row.matched != control.matched || row.built != control.built)
    {
        println!(
            "[warn] the {} arm admitted {}/{} hops against the control's {}/{}; detection does not \
             read overrides, so the arms are not measuring one population",
            format_multiple(drift.multiple),
            drift.matched,
            drift.built,
            control.matched,
            control.built
        );
    }

    // A venue that prices off state this run did not override answers EVERY arm identically. Two
    // adjacent arms matching is the opposite signal — the curve has saturated — so this only fires
    // when nothing anywhere moved.
    if rows.len() > 1 && rows.iter().all(|row| row.outcomes == control.outcomes) {
        println!(
            "[warn] every arm's outcomes are identical to the control's: this venue does not price \
             off the accounts --vault named. Scaling them changes nothing, so the table above is \
             not a capital curve. See the README on venues that quote from oracle or signed state."
        );
    }

    // The falsification arm. Less inventory that does not cost fills means the lever is inert,
    // and every arm above the control is then unattributable too.
    if let Some(down) = rows.iter().find(|row| row.multiple < 1.0)
        && down.scored >= control.scored
    {
        println!(
            "[warn] the {} arm filled {} against the control's {} — cutting inventory did not cost \
             fills, so the lever is not live and no arm above the control is attributable",
            format_multiple(down.multiple),
            down.scored,
            control.scored
        );
    }

    // Titan writes a zero minimum-out, so a route can succeed and deliver nothing. The scorer
    // counts that as a fill at exactly -10000 bps, which drags an aggregate toward -100%.
    if let Some(empty) = rows
        .iter()
        .find(|row| row.mean_bps().is_some_and(|bps| (bps + 10_000.0).abs() < 1.0))
    {
        println!(
            "[warn] the {} arm's mean bps is -10000, the signature of routes that succeeded and \
             delivered nothing; the venue's mint ordering in the spec is probably reversed",
            format_multiple(empty.multiple)
        );
    }
}

#[cfg(test)]
mod tests;
