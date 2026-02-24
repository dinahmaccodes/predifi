#!/usr/bin/env bash
# =============================================================================
# simulate_predictions.sh
#
# Simulate N predictions on a single PrediFi market pool deployed to a Stellar
# network (defaults to testnet / futurenet).
#
# Usage:
#   ./scripts/simulate_predictions.sh [OPTIONS]
#
# Options / environment variables:
#   PREDICTIONS   Number of predictions to simulate  (default: 100)
#   NETWORK       Stellar network passphrase alias    (default: testnet)
#   IDENTITY      Stellar CLI identity to use         (default: default)
#   TOKEN         Whitelisted token contract address  (required)
#   AMOUNT        Stake per prediction in stroops     (default: 1000000)
#
# Prerequisites:
#   - stellar CLI installed and configured
#   - A funded identity with enough XLM for fees
#   - The predifi-contract already built (runs build step automatically)
#   - A whitelisted token deployed and the TOKEN variable set
#
# Example:
#   TOKEN=CDLZFC... PREDICTIONS=50 ./scripts/simulate_predictions.sh
# =============================================================================

set -euo pipefail

# ── Defaults ──────────────────────────────────────────────────────────────────
PREDICTIONS="${PREDICTIONS:-100}"
NETWORK="${NETWORK:-testnet}"
IDENTITY="${IDENTITY:-default}"
AMOUNT="${AMOUNT:-1000000}"   # 1 XLM in stroops by default

# ── Derived paths ─────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTRACT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
WASM="${CONTRACT_DIR}/target/wasm32-unknown-unknown/release/predifi_contract.wasm"

log() { echo "[$(date -u '+%H:%M:%S')] $*"; }
err() { echo "[ERROR] $*" >&2; exit 1; }

# ── Validate prerequisites ────────────────────────────────────────────────────
command -v stellar >/dev/null 2>&1 || err "stellar CLI not found. Install from https://developers.stellar.org/docs/tools/developer-tools/cli/install"
[[ -n "${TOKEN:-}" ]] || err "TOKEN environment variable must be set to a whitelisted token contract address."

# ── Step 1: Build the contract ────────────────────────────────────────────────
log "Building predifi-contract (release)…"
cd "${CONTRACT_DIR}"
cargo build --target wasm32-unknown-unknown --release --quiet
log "Build complete: ${WASM}"

# ── Step 2: Deploy the contract ───────────────────────────────────────────────
log "Deploying contract to ${NETWORK}…"
CONTRACT_ID=$(stellar contract deploy \
    --wasm "${WASM}" \
    --network "${NETWORK}" \
    --source "${IDENTITY}" \
    2>&1)
log "Contract deployed: ${CONTRACT_ID}"

# ── Step 3: Retrieve the deployer address ─────────────────────────────────────
ADMIN_ADDR=$(stellar keys address "${IDENTITY}")
log "Admin address: ${ADMIN_ADDR}"

# ── Step 4: Initialize the contract ──────────────────────────────────────────
log "Initialising contract…"
stellar contract invoke \
    --id "${CONTRACT_ID}" \
    --network "${NETWORK}" \
    --source "${IDENTITY}" \
    -- init \
    --access_control "${ADMIN_ADDR}" \
    --treasury "${ADMIN_ADDR}" \
    --fee_bps 0 \
    --resolution_delay 0

# ── Step 5: Whitelist the token ───────────────────────────────────────────────
log "Whitelisting token ${TOKEN}…"
stellar contract invoke \
    --id "${CONTRACT_ID}" \
    --network "${NETWORK}" \
    --source "${IDENTITY}" \
    -- add_token_to_whitelist \
    --admin "${ADMIN_ADDR}" \
    --token "${TOKEN}"

# ── Step 6: Create a single market pool ──────────────────────────────────────
FUTURE_TIME=$(( $(date +%s) + 86400 ))  # 24 hours from now
log "Creating pool (end_time=${FUTURE_TIME})…"
POOL_ID=$(stellar contract invoke \
    --id "${CONTRACT_ID}" \
    --network "${NETWORK}" \
    --source "${IDENTITY}" \
    -- create_pool \
    --creator "${ADMIN_ADDR}" \
    --end_time "${FUTURE_TIME}" \
    --token "${TOKEN}" \
    --options_count 2 \
    --description "Stress test market" \
    --metadata_url "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi" \
    --initial_liquidity 0 \
    --category "stress" \
    2>&1)
log "Pool created: ID=${POOL_ID}"

# ── Step 7: Simulate predictions ─────────────────────────────────────────────
log "Simulating ${PREDICTIONS} predictions on pool ${POOL_ID}…"
START_TS=$(date +%s%N)

for i in $(seq 1 "${PREDICTIONS}"); do
    OUTCOME=$(( (i % 2) + 1 ))   # alternates between outcome 1 and 2

    stellar contract invoke \
        --id "${CONTRACT_ID}" \
        --network "${NETWORK}" \
        --source "${IDENTITY}" \
        -- place_prediction \
        --user "${ADMIN_ADDR}" \
        --pool_id "${POOL_ID}" \
        --amount "${AMOUNT}" \
        --outcome "${OUTCOME}" \
        --quiet

    if (( i % 10 == 0 )); then
        log "  Progress: ${i}/${PREDICTIONS} predictions submitted"
    fi
done

END_TS=$(date +%s%N)
ELAPSED_MS=$(( (END_TS - START_TS) / 1_000_000 ))

# ── Step 8: Report results ────────────────────────────────────────────────────
log "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
log "Simulation complete"
log "  Contract   : ${CONTRACT_ID}"
log "  Pool ID    : ${POOL_ID}"
log "  Predictions: ${PREDICTIONS}"
log "  Stake/pred : ${AMOUNT} stroops"
log "  Total stake: $(( PREDICTIONS * AMOUNT )) stroops"
log "  Elapsed    : ${ELAPSED_MS} ms"
log "  Avg/pred   : $(( ELAPSED_MS / PREDICTIONS )) ms"
log "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
log "Done. To resolve the pool and test claims, set the ledger time past"
log "end_time=${FUTURE_TIME} and call resolve_pool + claim_winnings."
