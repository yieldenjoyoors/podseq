# Enshrined Bridge (L2 side)

`BridgeFactory.sol` is the L2 half of the podseq bridge: a genesis predeploy
that deploys — or adopts — one canonical `Bridge` ERC20 per bridged Sui coin
type. Two contracts are predeployed in genesis at fixed addresses:

| Address                                      | Contract        | Purpose                              |
| -------------------------------------------- | --------------- | ------------------------------------ |
| `0x4200000000000000000000000000000000000010` | `BridgeFactory` | token registry + deployer            |
| `0x4200000000000000000000000000000000000011` | `Bridge`        | canonical SUI token (stable address) |

- The sequencer relayer mints a coin's token when it observes the matching
  `bridge::Deposit` on Sui, creating the token first if it does not exist.
- Users call `initiateWithdrawal(bytes32 suiRecipient, uint256 amount)` to burn;
  the relayer then calls `bridge::withdraw` on Sui to release the locked coins.

The bridge is **enshrined**: it has no external relayer. The same operator that
runs the sequencer holds the factory `relayer` role (the minter on every token)
and the Sui `BridgeCap`, so both directions are relayed in-process.

## Why a factory

The Sui vault accepts deposits of any coin type. The factory is what makes that
safe instead of stranding value:

- **Permissionless creation**: anyone can call
  `createBridge(name, symbol, coinType)` for a coin type that has no token yet,
  so no operator gate stands between a deposit and its L2 representation.
- **One canonical token per coin type**: a duplicate `coinType` reverts, and
  `adoptBridge` (relayer-gated, verifies `coinType()` and `relayer()` on-chain)
  is the only way to register a pre-existing contract. Bridged liquidity is
  never split across two representations of the same asset.
- **Verifiable registry**: `tokenFor(coinType)` is the on-chain source of truth.
  The relayer only honors `WithdrawalInitiated` logs from factory-registered
  tokens, so a fake contract emitting forged burns can never drain the vault.

## Roles

| L2 role   | Holder            | Power                                               |
| --------- | ----------------- | --------------------------------------------------- |
| `relayer` | sequencer EVM key | factory: `setRelayer`; tokens: `mint`, `setRelayer` |

`createBridge` is callable by anyone once the factory is initialized. The
factory's `relayer` (not the creator) is set as the minter on each new token;
rotating the factory relayer applies to future tokens only, and existing tokens
rotate through their own `setRelayer`. `adoptBridge` is relayer-only.

Mint nonces (`lastMintedDepositNonce`) must strictly increase (with the first
mint at nonce 0), so relaying a Sui deposit twice reverts rather than
double-minting. Gaps are allowed: each coin's token mints only its own deposits,
a subsequence of the global nonce stream.

## Compile

With [Foundry](https://book.getfoundry.sh):

```sh
forge build
```

Or with `solc` directly:

```sh
solc --bin --abi src/BridgeFactory.sol -o build
```

## Test

```sh
forge test
```

The suites (`test/Bridge.t.sol`, `test/BridgeFactory.t.sol`) pin the
relayer-facing invariants: the first mint is nonce 0, mints only go forwards
(replays revert), only the relayer can mint/rotate, `initiateWithdrawal` burns
exactly the caller's balance, creation is permissionless, duplicates revert,
and the genesis-predeploy `initialize`/`adoptBridge` paths work and are
one-time/verified.

## Predeploy

Both contracts are **genesis predeploys**: their runtime bytecode is planted at
fixed addresses in the Reth genesis (`alloc`), so they exist from block 0 with
no autodeploy. Use the generator to inject them into a genesis file:

```sh
./solidity/scripts/gen-genesis.sh examples/reth-genesis.json
```

The committed `examples/reth-genesis.json` already contains them at
`0x4200000000000000000000000000000000000010` (factory) and
`0x4200000000000000000000000000000000000011` (SUI token).

**Genesis runs no constructor.** So both boot unconfigured
(`relayer == address(0)`). After the chain starts, configure them exactly once:

```sh
# 1. Fund the relayer's EVM address with L2 gas (the dev genesis only funds 0xf39F…).
cast send <RELAYER_EVM_ADDRESS> --value 1ether --private-key <GENESIS_ACCOUNT_PRIVATE_KEY>

# 2. Initialize the factory (sets the minter for all tokens).
cast send 0x4200000000000000000000000000000000000010 \
  "initialize(address)" <RELAYER_EVM_ADDRESS> \
  --private-key $(cat relayer.key)

# 3. Configure the predeployed SUI token.
cast send 0x4200000000000000000000000000000000000011 \
  "initialize(string,string,string,address)" \
  "Bridged SUI" "SUI" "0x2::sui::SUI" <RELAYER_EVM_ADDRESS> \
  --private-key $(cat relayer.key)

# 4. Adopt it as the canonical SUI token in the factory registry.
cast send 0x4200000000000000000000000000000000000010 \
  "adoptBridge(string,address)" \
  "0x2::sui::SUI" 0x4200000000000000000000000000000000000011 \
  --private-key $(cat relayer.key)
```

The `initialize` calls are callable by anyone but exactly once — the first
caller configures each contract — so on a fresh single-sequencer chain the
operator does them while the sequencer keeps producing blocks (the relayer
retries until then; see `docs/src/bridge.md`). `adoptBridge` is relayer-only.
The Sui side (`move/sources/bridge.move`) is initialized once after the package
is published: `bridge::initialize` shares
the `Vault` and transfers `BridgeCap` to the operator.

Other coin types need no bootstrap. The relayer creates each coin's token when
its first deposit arrives (name = coin type, symbol = struct name); anyone may
front-run it with `createBridge` for a friendlier display name.

## Wiring the relayer

The relayer runs inside the sequencer node (`crates/node/src/bridge.rs`). It needs:

- the Sui `Vault` and `BridgeCap` object IDs (from `initialize`),
- the L2 `BridgeFactory` and predeployed `Bridge` addresses (the table above),
- a funded EVM key for the factory `relayer` role.

See `[bridge]` in the node config (`podseq init config`).
