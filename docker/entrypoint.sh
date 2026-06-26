#!/bin/sh
set -eu

# Generate the podseq TOML config from environment variables, then run the node.
# Optional fields are omitted unless the corresponding env var is set or the
# referenced file exists (keys), so the resulting config is always valid.

CONFIG_FILE="${PODSEQ_CONFIG:-/etc/podseq/podseq.toml}"
mkdir -p "$(dirname "$CONFIG_FILE")"

JWT_PATH="${PODSEQ_JWT_PATH:-/jwt/jwt.hex}"
ENGINE_URL="${PODSEQ_RETH_ENGINE_URL:-http://reth:8551}"

PUBLISHER="${WALRUS_PUBLISHER_URL:?WALRUS_PUBLISHER_URL is required}"
AGGREGATOR="${WALRUS_AGGREGATOR_URL:?WALRUS_AGGREGATOR_URL is required}"
EPOCHS="${WALRUS_EPOCHS:-1}"

SUI_RPC="${SUI_RPC_URL:?SUI_RPC_URL is required}"

MODE="${PODSEQ_MODE:-sequencer}"
BLOCK_TIME_MS="${BLOCK_TIME_MS:-2000}"
FEE_RECIPIENT="${FEE_RECIPIENT:-0x0000000000000000000000000000000000000000}"

SIGNER_KEY="${PODSEQ_SIGNER_KEY_PATH:-/secrets/sui.key}"

{
    printf '[reth]\n'
    printf 'engine_url = "%s"\n' "$ENGINE_URL"
    printf 'jwt_path = "%s"\n\n' "$JWT_PATH"

    printf '[walrus]\n'
    printf 'publisher_url = "%s"\n' "$PUBLISHER"
    printf 'aggregator_url = "%s"\n' "$AGGREGATOR"
    printf 'epochs = %s\n' "$EPOCHS"
    if [ -n "${WALRUS_PUBLISHER_AUTH_TOKEN:-}" ]; then
        printf 'publisher_auth_token = "%s"\n' "$WALRUS_PUBLISHER_AUTH_TOKEN"
    fi
    printf '\n'

    printf '[sui]\n'
    printf 'rpc_url = "%s"\n' "$SUI_RPC"
    [ -n "${SUI_SETTLEMENT_PACKAGE_ID:-}" ] && printf 'settlement_package_id = "%s"\n' "$SUI_SETTLEMENT_PACKAGE_ID"
    [ -n "${SUI_SETTLER_CAP_ID:-}" ] && printf 'settler_cap_id = "%s"\n' "$SUI_SETTLER_CAP_ID"
    [ -n "${SUI_REGISTRY_ID:-}" ] && printf 'registry_id = "%s"\n' "$SUI_REGISTRY_ID"

    if [ -f "$SIGNER_KEY" ]; then
        printf '\n[signer]\n'
        printf 'key_path = "%s"\n' "$SIGNER_KEY"
    fi

    printf '\n[sequencer]\n'
    printf 'block_time_ms = %s\n' "$BLOCK_TIME_MS"
    printf 'fee_recipient = "%s"\n' "$FEE_RECIPIENT"
    [ -n "${GENESIS_HASH:-}" ] && printf 'genesis_hash = "%s"\n' "$GENESIS_HASH"

    printf '\nmode = "%s"\n' "$MODE"
} > "$CONFIG_FILE"

exec podseq start --config "$CONFIG_FILE"
