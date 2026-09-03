#!/usr/bin/env bash
# Injects the bridge contracts as genesis predeploys at fixed addresses:
#
#   0x4200000000000000000000000000000000000010  BridgeFactory
#   0x4200000000000000000000000000000000000011  Bridge (canonical SUI token)
#
# Genesis `alloc` plants runtime bytecode with NO constructor execution, so both
# boot unconfigured. After the chain starts the operator calls, once:
#   BridgeFactory.initialize(relayer), Bridge.initialize(name, symbol,
#   coinType, relayer), then BridgeFactory.adoptBridge(coinType, token)
# (see solidity/README.md). Other coin types get tokens at runtime through the
# factory's permissionless createBridge.
#
# Usage:
#   ./solidity/scripts/gen-genesis.sh <base-genesis.json> [factory-address] [bridge-address]
#   ./solidity/scripts/gen-genesis.sh examples/reth-genesis.json
#
# Requires: forge (built), jq.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE="${1:-$ROOT/examples/reth-genesis.json}"
FACTORY_ADDR="${2:-0x4200000000000000000000000000000000000010}"
BRIDGE_ADDR="${3:-0x4200000000000000000000000000000000000011}"

runtime_code() {
  local artifact="$1"
  if [ ! -f "$artifact" ]; then
    echo "Building solidity…" >&2
    (cd "$ROOT/solidity" && forge build)
  fi
  # Foundry stores deployed runtime bytecode under `.deployedBytecode.object`.
  local code
  code="$(jq -r '.deployedBytecode.object' "$artifact" | tr -d '\n')"
  case "$code" in
    0x60*) ;;  # sanity: EVM bytecode starts with PUSH1 0x80 0x60…
    *) echo "error: unexpected deployedBytecode in $artifact (got '${code:0:10}…')" >&2; exit 1;;
  esac
  printf '%s' "$code"
}

FACTORY_CODE="$(runtime_code "$ROOT/solidity/out/BridgeFactory.sol/BridgeFactory.json")"
BRIDGE_CODE="$(runtime_code "$ROOT/solidity/out/Bridge.sol/Bridge.json")"

jq --arg factory "$FACTORY_CODE" --arg factory_addr "$FACTORY_ADDR" \
   --arg bridge "$BRIDGE_CODE" --arg bridge_addr "$BRIDGE_ADDR" '
  .alloc[$factory_addr] = { "balance": "0x0", "code": $factory } |
  .alloc[$bridge_addr]  = { "balance": "0x0", "code": $bridge }
' "$BASE"

echo "Injected BridgeFactory at $FACTORY_ADDR and Bridge at $BRIDGE_ADDR" >&2
