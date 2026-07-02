# Enshrined Bridge (L2 side)

`Bridge.sol` is the L2 half of the podseq bridge. One instance is deployed per
bridged Sui coin type and behaves as an ERC20:

- The sequencer relayer mints when it observes a `bridge::Deposit` on Sui.
- Users call `initiateWithdrawal(bytes32 suiRecipient, uint256 amount)` to burn;
  the relayer then calls `bridge::withdraw` on Sui to release the locked coins.

The bridge is **enshrined**: it has no external relayer. The same operator that
runs the sequencer holds the L2 `relayer` role and the Sui `BridgeCap`, so both
directions are relayed in-process.

## Roles

| L2 role   | Holder            | Power                                           |
| --------- | ----------------- | ----------------------------------------------- |
| `relayer` | sequencer EVM key | `mint`, `markWithdrawalProcessed`, `setRelayer` |

Mint nonces (`lastMintedDepositNonce`) must strictly increase (with the first
mint at nonce 0), so relaying a Sui deposit twice reverts rather than
double-minting. Gaps are allowed: in a multi-coin vault each coin's relayer mints
only its own deposits, a subsequence of the global nonce stream.

## Compile

With [Foundry](https://book.getfoundry.sh):

```sh
forge build
```

Or with `solc` directly:

```sh
solc --bin --abi src/Bridge.sol -o build
```

## Test

```sh
forge test
```

The suite (`test/Bridge.t.sol`) pins the relayer-facing invariants: the first
mint is nonce 0, mints only go forwards (replays revert), only the relayer can
mint/rotate, and `initiateWithdrawal` burns exactly the caller's balance.

## Predeploy

`Bridge` is a **genesis predeploy**: its runtime bytecode is planted at a fixed
address in the Reth genesis (`alloc`), so it exists from block 0 with no
autodeploy. Use the generator to inject it into a genesis file:

```sh
./solidity/scripts/gen-genesis.sh examples/reth-genesis.json
```

The committed `examples/reth-genesis.json` already contains it at
`0x4200000000000000000000000000000000000010`.

**Genesis runs no constructor.** So the contract boots unconfigured (no metadata,
`relayer == address(0)`). After the chain starts, configure it exactly once:

```sh
# 1. Fund the relayer's EVM address with L2 gas (the dev genesis only funds 0xf39F…).
cast send <RELAYER_EVM_ADDRESS> --value 1ether --private-key <GENESIS_ACCOUNT_PRIVATE_KEY>

# 2. Initialize (genesis runs no constructor, so this configures metadata + relayer).
cast send 0x4200000000000000000000000000000000000010 \
  "initialize(string,string,string,address)" \
  "Bridged USDSui" "USDS" "0x2::sui::SUI" <RELAYER_EVM_ADDRESS> \
  --private-key $(cat relayer.key)
```

`initialize` is callable by anyone but exactly once — the first caller sets the
initial `relayer`. On a fresh single-sequencer chain the operator does this
after starting the node: the sequencer keeps producing blocks while the relayer
waits, retrying until `initialize` has confirmed (see `docs/src/bridge.md`). The
Sui side (`move/sources/bridge.move`) is initialized once after the package is
published: `bridge::initialize` shares
the `Vault` and transfers `BridgeCap` to the operator.

## Wiring the relayer

The relayer runs inside the sequencer node (`crates/node/src/bridge.rs`). It needs:

- the Sui `Vault` and `BridgeCap` object IDs (from `initialize`),
- the L2 `Bridge` contract address (the predeploy above),
- a funded EVM key for the L2 `relayer` role.

See `[bridge]` in the node config (`podseq init config`).
