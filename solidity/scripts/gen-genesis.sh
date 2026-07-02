#!/usr/bin/env bash
# Injects the Bridge runtime bytecode as a genesis predeploy at a fixed address.
#
# Genesis `alloc` plants runtime bytecode with NO constructor execution, so the
# contract boots unconfigured. The operator calls `initialize(name, symbol,
# coinType, relayer)` once after the chain starts (see solidity/README.md).
#
# Usage:
#   ./solidity/scripts/gen-genesis.sh <base-genesis.json> [predeploy-address]
#   ./solidity/scripts/gen-genesis.sh examples/reth-genesis.json
#
# Requires: forge (built), jq.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE="${1:-$ROOT/examples/reth-genesis.json}"
ADDRESS="${2:-0x4200000000000000000000000000000000000010}"

artifact="$ROOT/solidity/out/Bridge.sol/Bridge.json"
if [ ! -f "$artifact" ]; then
  echo "Building Bridge.sol…" >&2
  (cd "$ROOT/solidity" && forge build)
fi

# Foundry stores deployed runtime bytecode under `.deployedBytecode.object`.
code="$(jq -r '.deployedBytecode.object' "$artifact" | tr -d '\n')"
case "$code" in
  0x60*) ;;  # sanity: EVM bytecode starts with PUSH1 0x80 0x60…
  *) echo "error: unexpected deployedBytecode (got '${code:0:10}…')" >&2; exit 1;;
esac

jq --arg code "$code" --arg addr "$ADDRESS" \
  '.alloc[$addr] = { "balance": "0x0", "code": $code }' "$BASE"
echo "Injected Bridge predeploy at $ADDRESS" >&2
