## Data Anchor CLI End‑to‑End Demo

A turnkey bash script (`cli.sh`) that walks Solana developers through storing, verifying, indexing, and reclaiming on‑chain data with Data Anchor—all in one go. This demo targets data‑intensive use cases (AI agents, DePIN networks, IoT telemetry, etc.) where cost and verifiability matter.


### Prerequisites

* **1. Solana CLI**
  Install via the official installer script:

  ```bash
  curl --proto '=https' --tlsv1.2 -sSfL https://solana-install.solana.workers.dev | bash
  ```

* **2. Data Anchor CLI**
  Download and install the Data Anchor tool:

  ```bash
  curl -sSf https://data-anchor.termina.technology/install.sh
  ```
* **3. Verify installation**
  Run to list available commands and options:

  ```bash
  data-anchor --help
  ```
* **4. Funded wallet**
  
  Make sure your Solana keypair has SOL on Devnet or Mainnet before running the demo.


### Optional Prerequisites

* **jq**, **xxd**: for JSON parsing & hex decoding

### Configuration

Clone this repo and create a `.env.local` in the same folder as `cli.sh`:

```bash
cp examples/cli/.env.example .env.local
```

Edit `.env.local` with your own values with examples shown in `.env.example`:


## Quickstart

1. Make the demo script executable:

   ```bash
   chmod +x cli.sh
   ```
2. Run the end‑to‑end flow:

   ```bash
   ./cli.sh
   ```

## Demo Steps

Below is a high‑level summary of what `cli.sh` does:

1. **Load environment**
   Exports your `.env.local` variables or aborts with a helpful error.

2. **Initialize blober**
   Creates the on‑chain “blober” (Program‑Derived Address) for your namespace.

3. **Build payload**
   Writes a sample JSON blob (`tmp.json`) containing reward metrics from IoT sensors.

4. **Upload blob**
   Anchors your JSON on‑chain, returning a signature and slot.

5. **Fetch & decode**
   Retrieves the blob by signature, decodes hex → JSON, and pretty‑prints it.

6. **Indexer query**
   Calls `get_blobs_by_slot` on the indexer to fetch metadata and raw data.

7. **Fetch proof**
   Retrieves a cryptographic proof for your blob’s existence at that slot.

8. **Close blober**
   Tears down the PDA to reclaim rent, cleaning up on‑chain state.


### Error Handling & Validation

* **Missing `.env.local`**
  Script exits with instructions if environment variables aren’t set.

* **Insufficient balance**
  If your payer has < rent‑exempt SOL, the script will bail at initialization.
  *Solution*: `solana airdrop 1 <your-key>` (Devnet) or fund your Mainnet wallet.

* **Empty fetch**
  Fetch & decode checks for non‑empty data and aborts if something went wrong.

* **Indexer mismatch**
  Compares indexer‑returned data to on‑chain decoded bytes and errors on discrepancy.


### Customization

* **Static blob**: skip step 2 and point `--data-path` at your own JSON.
* **Alternate payload**: swap in any JSON shape—AI metrics, DePIN session logs, etc.
* **Mainnet usage**: update `.env.local` to Mainnet program ID and indexer URL, fund your wallet, then re‑run.


## Program IDs

Use the correct on‑chain program for your network:

* **Solana Mainnet**: `9i2MEc7s38jLGoEkbFszuTJCL1w3Uorg7qjPjfN8Tv5Z`
* **Solana Devnet**: `2RWsr92iL39YCLiZu7dZ5hron4oexEMbgWDg35v5U5tH`


## Further Resources

* **Deep dive CLI reference**: `data-anchor --help`
* **Developer docs**: [link](https://docs.termina.technology/documentation/network-extension-stack/modules/data-anchor)
* **Join the conversation**: [Termina's Twitter](https://x.com/Terminaxyz)


## Published Crates

The source code for the CLI and related components is published on [crates.io](https://crates.io), and visible to anyone who views them:

* [CLI Documentation](https://docs.rs/data-anchor/latest/data_anchor/)
* [Client API](https://docs.rs/data-anchor-client/latest/data_anchor_client/)
* [Blober Program](https://docs.rs/data-anchor-blober/latest/data_anchor_blober/)
* [Indexer API](https://docs.rs/data-anchor-api/latest/data_anchor_api/)
* [Proofs API](https://docs.rs/data-anchor-proofs/latest/data_anchor_proofs/)

*Happy anchoring!*