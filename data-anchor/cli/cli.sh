#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

# ─── Load env ─────────────────────────────────────────────────────────────────
if [ -f .env.local ]; then
  set -a; source .env.local; set +a
else
  echo ".env.local not found, please create one with required variables. Example can be found in examples/cli/env.example" >&2
  exit 1
fi

# ─── 1. Initialize blober ─────────────────────────────────────────────────────
# Create the on-chain storage container (PDA) for our namespace
echo -e "\n1. Initializing Data Anchor blober"
data-anchor blober initialize

# ─── 2. Write dynamic JSON blob (can be skipped if you want to upload a static blob) ───────────────────────────────
# Build a JSON file with current rewards metrics
echo -e "\n2. Create rewards payload to tmp.json"
DATA_FILE=tmp.json
cat <<EOF > "$DATA_FILE"
{
  "epoch": 1042,
  "location": "Zug, Switzerland",
  "timestamp": "2025-07-25T17:09:00+08:00",
  "devices": [
    { "device_id": "sensor-001", "data_points": 340, "co2_ppm": 417, "reward": "0.03" },
    { "device_id": "sensor-002", "data_points": 327, "co2_ppm": 419, "reward": "0.02" }
  ],
  "total_reward": "0.05",
  "proof": "mock_zk_proof_here"
}
EOF
echo "  payload: $(cat "$DATA_FILE")"

# ─── 3. Upload blob ───────────────────────────────────────────────────────────
# Send the payload on-chain, capture its signature and ledger slot
echo -e "\n3. Uploading blob"
OUT=$(data-anchor blob upload --data-path "$DATA_FILE")
SIG=$(echo "$OUT" | jq -r .signatures[0])
SLOT=$(echo "$OUT" | jq -r .slot)
echo "  signature: $SIG"
echo "  slot:      $SLOT"

# ─── 4. Fetch from ledger by signature ────────────────────────────────────────
# Retrieve via ledger and decode hex back to JSON
echo -e "\n4. Fetching from ledger (by signature)"
data-anchor blob fetch "$SIG" --output text | xxd -r -p > fetched.json
echo "  fetched.json:"
cat fetched.json

# ─── 5. Query via indexer by slot ─────────────────────────────────────────────
# Query the indexer for metadata & raw data at that slot
echo -e "\n5. Fetching blob via indexer"
data-anchor indexer blobs "$SLOT" | jq .

# ─── 6. Fetch proofs from indexer ─────────────────────────────────────────────
# Fetch a cryptographic proof for on-chain audit at that slot
echo -e "\n6. Fetching proofs for slot $SLOT"
data-anchor indexer proofs "$SLOT"

# ─── 7. Close blober ─────────────────────────────────────────────────────────
# Tear down the on-chain storage account to reclaim rent
echo -e "\n7. Closing Data Anchor blober"
data-anchor --program-id $DATA_ANCHOR_PROGRAM_ID --namespace termina blober close

echo -e "\nAll done! You've successfully initialized a namespace → uploaded & verified a blob → queried the indexer → fetched a proof → closed the namespace. Try swapping in your own data and running on Mainnet!"