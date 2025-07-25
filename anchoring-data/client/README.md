## Data Anchor Rust Client Demo

Leverage the Data Anchor Rust SDK to anchor, retrieve, index, and prove data on Solana—all with simple, high‑level calls. This guide walks you through a full end‑to‑end example, from namespace initialization to cleanup.

### Installation

Add the client to your project via cargo add, or pin a specific version in your `Cargo.toml`:

```bash
cargo add data-anchor-client
```

In `Cargo.toml`:
```toml
[dependencies]
data-anchor-client = "0.1.x"
```

### Configuration

Clone this repo and create a `.env.local` in the same folder as `client.rs`:

```bash
cp examples/cli/.env.example .env.local
```

Edit `.env.local` with your own values with examples shown in `.env.example`:

### Quickstart

1. Install Dependencies

   ```bash
   cargo build
   ```
2. Export Environment Variables

   ```bash
   export $(cat .env.local | xargs)
   ```
3. Run the Client
   ```bash
   cargo run
   ```

### Core Methods

* `initialize_blober(fee, namespace, opts)` ⇒ `Vec<SuccessfulTransaction>`
  
  Sets up the on‑chain PDA for your namespace ([docs.termina.technology][4]).

* `upload_blob(data, fee, namespace, timeout)` ⇒ `Vec<SuccessfulTransaction>`
  
  Writes your data into Solana’s ledger history.

* `get_ledger_blobs_from_signatures(namespace, Vec<Signature>)` ⇒ `Vec<Vec<u8>>`
  
  Fetches raw blob bytes from Solana history without HTTP.

* `get_blobs(slot, namespace)` ⇒ `Vec<IndexerBlob>`
  
  HTTP RPC to list blobs by slot via our indexer.

* `get_slot_proof(slot, namespace)` ⇒ `SlotProof`
  
  Retrieves a Merkle‐style proof of inclusion.

* `close_blober(fee, namespace, opts)` ⇒ `Vec<SuccessfulTransaction>`
  Tears down the PDA and reclaims rent.

### Error Handling & Tips

* **Insufficient Balance**: ensure your payer has ≥896 160 lamports (\~0.001 SOL) for namespace init.
* **Empty Outcomes**: always assert `!outcomes.is_empty()` after `initialize_blober` and `upload_blob`.
* **Network**: default is Devnet - switch to Mainnet Beta by updating `DATA_ANCHOR_PROGRAM_ID` and `INDEXER_URL`.
* **Retry Logic**: wrap RPC/indexer calls in retries for production reliability.

## Further Resources

* **Deep dive CLI reference**: `data-anchor --help`
* **Developer docs**: [link](https://docs.termina.technology/documentation/network-extension-stack/modules/data-anchor)
* **Join the conversation**: [Termina's Twitter](https://x.com/Terminaxyz)

## Published Crates

The source code for the client and related components is published on [crates.io](https://crates.io), and visible to anyone who views them:

* [Client API](https://docs.rs/data-anchor-client/latest/data_anchor_client/)
* [CLI Documentation](https://docs.rs/data-anchor/latest/data_anchor/)
* [Blober Program](https://docs.rs/data-anchor-blober/latest/data_anchor_blober/)
* [Indexer API](https://docs.rs/data-anchor-api/latest/data_anchor_api/)
* [Proofs API](https://docs.rs/data-anchor-proofs/latest/data_anchor_proofs/)

*Happy anchoring!*