use std::{str::FromStr, sync::Arc, time::Duration};

use data_anchor_client::{BloberIdentifier, DataAnchorClient, FeeStrategy};
use serde_json::json;
use solana_cli_config::Config;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting client example...");

    // ─── Load env ─────────────────────────────────────────────────────────────────
    println!("Reading environment variables...");
    let program_id = Pubkey::from_str(&std::env::var("DATA_ANCHOR_PROGRAM_ID")?)?;
    let namespace = std::env::var("DATA_ANCHOR_NAMESPACE")?;
    let payer_path = std::env::var("PAYER_KEYPAIR_PATH")?;
    let indexer_url = std::env::var("DATA_ANCHOR_INDEXER_URL")?;
    let indexer_token = std::env::var("DATA_ANCHOR_INDEXER_API_TOKEN").ok();

    println!("Environment variables loaded:");
    println!("Program ID: {}", program_id);
    println!("Namespace: {}", namespace);
    println!("Keypair path: {}", payer_path);

    // Load the Solana keypair that will pay for transactions and own the data
    println!("Loading keypair...");
    let keypair_bytes = std::fs::read(&payer_path)?;
    let json_str = String::from_utf8(keypair_bytes)?;
    let json_array: Vec<u8> = serde_json::from_str(&json_str)?;
    let mut keypair_bytes_array = [0u8; 64];
    keypair_bytes_array.copy_from_slice(&json_array);
    let payer = Arc::new(Keypair::from_bytes(&keypair_bytes_array)?);
    println!("Keypair loaded: {}", payer.pubkey());

    // Build DataAnchor client
    println!("Building DataAnchor client...");
    let config = Config::load(solana_cli_config::CONFIG_FILE.as_ref().unwrap())?;
    println!("Using RPC URL: {}", config.json_rpc_url);
    let client = DataAnchorClient::builder()
        .payer(payer.clone())
        .program_id(program_id)
        .indexer_from_url(&indexer_url, indexer_token)
        .await?
        .build_with_config(config)
        .await?;
    println!("Client built successfully!");

    // ─── 1. Initialize blober ─────────────────────────────────────────────────────
    // Create the on-chain storage container (PDA) for our namespace
    println!("\n1. Initializing Data Anchor blober");
    match client
        .initialize_blober(FeeStrategy::default(), namespace.clone().into(), None)
        .await
    {
        Ok(_) => println!("Blober initialized for namespace '{}'", namespace),
        Err(e) => {
            if e.to_string().contains("AccountExists")
                || e.to_string().contains("Account already exists")
            {
                println!(
                    "Blober already exists for namespace '{}', continuing...",
                    namespace
                );
            } else {
                return Err(e.into());
            }
        }
    }

    // ─── 2. Write dynamic JSON blob (can be skipped if you want to upload a static blob) ───────────────────────────────
    // Build a JSON file with current rewards metrics
    println!("\n2. Create rewards payload");
    let payload_json = json!({
        "epoch": 1042,
        "timestamp": "2025-07-25T17:09:00+08:00",
        "location": "Zug, Switzerland",
        "devices": [
            { "device_id": "sensor-001", "data_points": 340, "co2_ppm": 417, "reward": "0.03" },
            { "device_id": "sensor-002", "data_points": 327, "co2_ppm": 419, "reward": "0.02" }
        ],
        "total_reward": "0.05",
        "proof": "mock_zk_proof_here"
    });
    let payload = serde_json::to_string(&payload_json)?.into_bytes();
    println!(
        "  payload: {}",
        serde_json::to_string_pretty(&payload_json)?
    );

    // ─── 3. Upload blob ───────────────────────────────────────────────────────────
    // Send the payload on-chain, capture its signature and ledger slot
    println!("\n3. Uploading blob");
    let (outcomes, _blob_addr) = client
        .upload_blob(
            &payload,
            FeeStrategy::default(),
            &namespace,
            Some(Duration::from_secs(10)),
        )
        .await?;
    if outcomes.is_empty() {
        panic!("No upload outcomes returned");
    }
    let slot = outcomes[0].slot;
    let sigs: Vec<_> = outcomes.iter().map(|o| o.signature).collect();
    println!("  signature: {:?}", sigs[0]);
    println!("  slot:      {}", slot);

    // ─── 4. Fetch from ledger by signature ────────────────────────────────────────
    // Retrieve via ledger and decode back to JSON
    println!("\n4. Fetching from ledger (by signature)");
    let recovered = client
        .get_ledger_blobs_from_signatures(
            BloberIdentifier::Namespace(namespace.clone()),
            sigs.clone(),
        )
        .await?;
    assert_eq!(recovered, payload);

    // Verify the retrieved data matches what we uploaded
    let recovered_json: serde_json::Value = serde_json::from_slice(&recovered)?;
    println!("  fetched data matches original:");
    println!("{}", serde_json::to_string_pretty(&recovered_json)?);

    // ─── 5. Query via indexer by slot ─────────────────────────────────────────────
    // Query the indexer for metadata & raw data at that slot
    println!("\n5. Fetching blob via indexer");
    let blobs = client
        .get_blobs(slot, BloberIdentifier::Namespace(namespace.clone()))
        .await?;
    if let Some(blobs) = blobs {
        println!("Indexer returned {} blob(s)", blobs.len());
    } else {
        println!("Indexer returned no blobs");
    }

    // ─── 6. Fetch proofs from indexer ─────────────────────────────────────────────
    // Fetch a cryptographic proof for on-chain audit at that slot
    println!("\n6. Fetching proofs for slot {}", slot);
    let proof = client
        .get_proof(slot, BloberIdentifier::Namespace(namespace.clone()))
        .await?;
    if proof.is_some() {
        println!("Proof retrieved successfully");
    } else {
        println!("No proof available yet");
    }

    // ─── 7. Close blober ─────────────────────────────────────────────────────────
    // Tear down the on-chain storage account to reclaim rent
    println!("\n7. Closing Data Anchor blober");
    client
        .close_blober(
            FeeStrategy::default(),
            BloberIdentifier::Namespace(namespace.clone()),
            None,
        )
        .await?;
    println!("Blober closed");

    println!(
        "\nYou've successfully initialized a namespace, uploaded & verified a blob, queried the indexer, fetched a proof, and closed the namespace."
    );
    Ok(())
}
