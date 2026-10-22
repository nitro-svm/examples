use std::{
    fs::File,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use reqwest::{StatusCode, header::CONTENT_TYPE};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_with::{DisplayFromStr, serde_as};
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};
use tokio::process::{Child, Command};

const READY_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(clap::Args)]
pub struct Endpoints {
    /// Pallas's business API (`PORT`), which serves `/swap-instruction`.
    #[arg(long = "pallas-url", default_value = "http://127.0.0.1:18081/")]
    pub api: url::Url,

    /// Pallas's ops API (`METRICS_PORT`), which serves `/ready`.
    #[arg(long = "pallas-ops-url", default_value = "http://127.0.0.1:19101/")]
    pub ops: url::Url,
}

/// Pallas inherits the OKX credentials (`OKX_API_KEY`, `OKX_SECRET_KEY`, `OKX_PASSPHRASE`) from
/// this process.
pub fn spawn(
    binary: &Path,
    rpc_url: &str,
    endpoints: &Endpoints,
    data_dir: &Path,
    log: &Path,
) -> Result<Child> {
    let port = |url: &url::Url| {
        url.port_or_known_default()
            .context("pallas url has no port")
    };
    std::fs::create_dir_all(data_dir)?;
    let stdout = File::create(log).with_context(|| format!("create {}", log.display()))?;
    let stderr = stdout.try_clone()?;
    Command::new(binary)
        .envs(frozen_chain_env(rpc_url))
        .env("HOST", "127.0.0.1")
        .env("PORT", port(&endpoints.api)?.to_string())
        .env("METRICS_ENABLED", "true")
        .env("METRICS_BIND", "127.0.0.1")
        .env("METRICS_PORT", port(&endpoints.ops)?.to_string())
        .env("PALLAS_DATA_DIR", data_dir)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))
}

pub fn frozen_chain_env(rpc_url: &str) -> [(&'static str, &str); 6] {
    [
        ("RPC_URL", rpc_url),
        ("GEYSER_MODE", "rpc-only"),
        // A paused session's slot never advances, which Pallas otherwise treats as a stalled feed.
        ("PALLAS_SLOT_STALE_SEC", "86400"),
        ("GRPC_CONSISTENCY_CHECK_INTERVAL_SECS", "0"),
        ("GEYSER_GRPC_RESET_ON_GAP", "0"),
        ("PALLAS_PROTOCOL_CU_HUB_ENABLED", "false"),
    ]
}

async fn is_ready(http: &reqwest::Client, endpoints: &Endpoints) -> Result<bool> {
    let url = endpoints.ops.join("ready")?;
    Ok(match http.get(url).send().await {
        Ok(response) => response
            .json::<serde_json::Value>()
            .await
            .is_ok_and(|body| body["ready"] == true),
        Err(_) => false,
    })
}

pub async fn wait_ready(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    mut spawned: Option<&mut Child>,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(child) = spawned.as_deref_mut()
            && let Some(status) = child.try_wait()?
        {
            bail!("pallas exited during startup ({status}); see its log");
        }
        if is_ready(http, endpoints).await? {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("pallas not ready at {} after {timeout:?}", endpoints.ops);
        }
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// A Pallas left over from the previous case is still priced against that case's session.
pub async fn wait_restarted(http: &reqwest::Client, endpoints: &Endpoints) -> Result<()> {
    while is_ready(http, endpoints).await? {
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
    Ok(())
}

pub async fn swap_instruction(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    request: &RawValue,
) -> Result<(StatusCode, String)> {
    let response = http
        .post(endpoints.api.join("swap-instruction")?)
        .header(CONTENT_TYPE, "application/json")
        .body(request.get().to_owned())
        .send()
        .await?;
    let status = response.status();
    Ok((status, response.text().await?))
}

#[derive(Debug, PartialEq)]
pub enum Answer {
    Rejected { code: String, message: String },
    Built(Built),
}

#[derive(Debug, PartialEq)]
pub struct Built {
    pub instructions: Vec<Instruction>,
    pub lookup_tables: Vec<Address>,
    pub quoted_out: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    error_code: String,
    #[serde(default)]
    message: String,
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    #[serde(default)]
    compute_budget_instructions: Vec<WireInstruction>,
    #[serde(default)]
    setup_instructions: Vec<WireInstruction>,
    swap_instruction: WireInstruction,
    cleanup_instruction: Option<WireInstruction>,
    #[serde(default)]
    other_instructions: Vec<WireInstruction>,
    #[serde_as(as = "Vec<DisplayFromStr>")]
    #[serde(default)]
    address_lookup_table_addresses: Vec<Address>,
    router_result: RouterResult,
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouterResult {
    #[serde_as(as = "DisplayFromStr")]
    to_token_amount: u64,
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireInstruction {
    #[serde_as(as = "DisplayFromStr")]
    program_id: Address,
    accounts: Vec<WireAccount>,
    data: String,
}

#[serde_as]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAccount {
    #[serde_as(as = "DisplayFromStr")]
    pubkey: Address,
    #[serde(default)]
    is_signer: bool,
    #[serde(default)]
    is_writable: bool,
}

impl TryFrom<WireInstruction> for Instruction {
    type Error = anyhow::Error;

    fn try_from(wire: WireInstruction) -> Result<Self> {
        let accounts = wire
            .accounts
            .into_iter()
            .map(|account| AccountMeta {
                pubkey: account.pubkey,
                is_signer: account.is_signer,
                is_writable: account.is_writable,
            })
            .collect();
        Ok(Instruction {
            program_id: wire.program_id,
            accounts,
            data: BASE64.decode(wire.data).context("instruction data")?,
        })
    }
}

pub fn decode(status: StatusCode, body: &str) -> Result<Answer> {
    if let Ok(ErrorBody {
        error_code,
        message,
    }) = serde_json::from_str(body)
    {
        return Ok(Answer::Rejected {
            code: error_code,
            message,
        });
    }
    if !status.is_success() {
        bail!("pallas answered HTTP {status}: {body}");
    }
    let wire: Wire = serde_json::from_str(body).context("parse /swap-instruction answer")?;
    let instructions = wire
        .compute_budget_instructions
        .into_iter()
        .chain(wire.setup_instructions)
        .chain([wire.swap_instruction])
        .chain(wire.cleanup_instruction)
        .chain(wire.other_instructions)
        .map(Instruction::try_from)
        .collect::<Result<_>>()?;
    Ok(Answer::Built(Built {
        instructions,
        lookup_tables: wire.address_lookup_table_addresses,
        quoted_out: wire.router_result.to_token_amount,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn program(n: u8) -> Address {
        Address::new_from_array([n; 32])
    }

    fn wire_ix(n: u8) -> serde_json::Value {
        json!({
            "programId": program(n).to_string(),
            "accounts": [{ "pubkey": program(100 + n).to_string(), "isWritable": true }],
            "data": BASE64.encode([n]),
        })
    }

    #[test]
    fn a_business_error_on_http_200_is_a_rejection() {
        let body = json!({ "errorCode": "82000", "message": "no route" }).to_string();
        assert_eq!(
            decode(StatusCode::OK, &body).expect("decodes"),
            Answer::Rejected {
                code: "82000".to_string(),
                message: "no route".to_string(),
            }
        );
    }

    #[test]
    fn a_built_swap_keeps_execution_order() {
        let body = json!({
            "computeBudgetInstructions": [wire_ix(1)],
            "setupInstructions": [wire_ix(2)],
            "swapInstruction": wire_ix(3),
            "cleanupInstruction": wire_ix(4),
            "otherInstructions": [wire_ix(5)],
            "addressLookupTableAddresses": [program(9).to_string()],
            "routerResult": { "toTokenAmount": "42" },
        })
        .to_string();
        let Answer::Built(built) = decode(StatusCode::OK, &body).expect("decodes") else {
            panic!("expected a built swap");
        };
        let order: Vec<Address> = built.instructions.iter().map(|ix| ix.program_id).collect();
        assert_eq!(order, (1..=5).map(program).collect::<Vec<_>>());
        assert_eq!(built.instructions[2].data, vec![3]);
        assert!(built.instructions[2].accounts[0].is_writable);
        assert_eq!(built.lookup_tables, vec![program(9)]);
        assert_eq!(built.quoted_out, 42);
    }

    #[test]
    fn a_failed_status_without_an_error_body_is_an_error() {
        assert!(decode(StatusCode::BAD_GATEWAY, "upstream down").is_err());
    }
}
