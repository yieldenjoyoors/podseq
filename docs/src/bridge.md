# Enshrined Bridge

Podseq ships a trust-minimized bridge between Sui and the L2. Users lock Sui
coins (USDSui, SUI, or any coin type) in a vault and receive a matching ERC20 on
L2; burning on L2 releases the original coins on Sui. There is **no external
relayer**: the sequencer process itself relays both directions, holding the Sui
`BridgeCap` and the L2 `relayer` role.

```text
            Sui                                   L2 (EVM)
┌──────────────────────────┐         ┌──────────────────────────┐
│ bridge::Vault (shared)   │         │ BridgeFactory (predeploy)│
│  + BridgeCap (sequencer) │         │  ├─ Bridge@0x…11 (SUI)   │
└────────────┬─────────────┘         │  └─ Bridge per coin…     │
             │                       │     + relayer (sequencer)│
   deposit<T>(coin, l2_recipient)                 │
             │ deposit_nonce (Table)    initiateWithdrawal(sui_recipient, amt)
             ▼                                    │ WithdrawalInitiated (logs)
        ┌─────────────  sequencer relayer (in-process)  ─────────────┐
        │  reads deposits by nonce ──► mints the coin's token (mint) │
        │  factory: createBridge ──► one canonical ERC20 per type    │
        │  reads L2 burn logs ──► bridge::withdraw<T> on Sui         │
        └────────────────────────────────────────────────────────────┘
```

## Sui side: `bridge.move`

`bridge::Vault` is a shared object holding one locked `Coin<T>` per coin type in a
`Bag`, plus a `Table<u64, DepositRecord>` with one entry per deposit nonce. The
sequencer owns `BridgeCap`.

| Entrypoint    | Caller      | Effect                                                            |
| ------------- | ----------- | ----------------------------------------------------------------- |
| `initialize`  | operator    | shares `Vault`, transfers `BridgeCap`                             |
| `deposit<T>`  | anyone      | locks `Coin<T>`, records `DepositRecord`, emits `Deposit`         |
| `withdraw<T>` | `BridgeCap` | splits and sends `Coin<T>` to a Sui recipient, emits `Withdrawal` |

Deposits are generic over `T` and every coin type is accepted: the L2 side can
always produce a matching token on demand (see the factory below), so a deposit
never locks value that cannot be represented and later burned for release. Each
coin type is tracked independently in the vault's `Bag` and in the global nonce
stream, so one vault serves every bridged asset.

The Sui gRPC read API exposes no event query, so deposits are also written to the
`deposits` table and read by nonce (one `get_object` per nonce), exactly like the
settlement registry. See [Settlement Contract](./contract.md).

## L2 side: `BridgeFactory` + `Bridge.sol`

`BridgeFactory` is the genesis predeploy at
`0x4200000000000000000000000000000000000010`, and the canonical SUI `Bridge`
token is predeployed right next to it at
`0x4200000000000000000000000000000000000011`, so the flagship asset has a
stable, integration-friendly address. The factory deploys every other token
and keeps the on-chain registry `tokenFor(coinType)`:

- **Permissionless**: anyone can call `createBridge(name, symbol, coinType)` for
  a coin type that has no token yet. This is what makes "any coin" safe: no
  operator gate stands between a deposit and its L2 representation.
- **One canonical token per coin type**: a duplicate `coinType` reverts, so
  bridged liquidity is never split across two representations of the same asset.
- **Adoption for predeploys**: `adoptBridge(coinType, token)` (factory relayer
  only) registers the genesis-planted `Bridge` as canonical for its coin type
  after `Bridge.initialize` configures it, verifying on-chain that the token's
  `coinType()` and `relayer()` match.
- The factory's `relayer` (the sequencer's EVM key) is set as the minter on
  every token it deploys.

Each `Bridge` mints only against strictly increasing Sui deposit nonces, so a
retried relay is a safe no-op (gaps are legal: each token mints the subsequence
of the global nonce stream that belongs to its coin). Users call
`initiateWithdrawal(bytes32 suiRecipient, uint256 amount)` to burn; the relayer
watches `WithdrawalInitiated` logs and calls `bridge::withdraw` on Sui.

See `solidity/README.md` for compile and predeploy instructions.

## Rust: `podseq_sui::bridge` and the relayer

- `crates/sui/src/bridge.rs` — Sui vault client: reads the whole vault state
  in one fetch (`vault_status`: nonces, deposits-table UID), reads single
  deposits by nonce (`deposit_at`), and submits transactions
  (`BridgeClient::withdraw`, `initialize`).
- `crates/node/src/bridge.rs` — the in-process relayer (`BridgeRelayer`):
  - Sui deposits → resolves the deposit's coin type to its canonical token,
    creating it through the factory when none exists yet, then signs and sends
    `mint`;
  - L2 `WithdrawalInitiated` logs → `bridge::withdraw` on Sui, with the coin
    type taken from the emitting token.

Cursors and the token registry are persisted to `bridge_cursors.json` in one
write, so a restart resumes without re-minting, re-releasing, or rescanning.
The full factory history is indexed only on first start, in bounded chunks
(`createBridge` is permissionless, so the token count is unbounded).

## Configuration

Generate the relayer's EVM key with the keyring (it writes a 32-byte
secp256k1 scalar; the printed address must be the factory's `relayer`):

```sh
podseq keyring generate-evm-key --out relayer.key
podseq keyring list   # shows signer, p2p, and bridge relayer keys
```

```toml
[bridge]
enabled             = true
cap_id              = "0x..."   # BridgeCap; auto-created on first start if unset
vault_id            = "0x..."   # Vault; auto-created on first start if unset
l2_relayer_key_path  = "relayer.key"   # from `keyring generate-evm-key`
poll_interval_ms    = 2000
```

Both sides are auto-configured on first start — the same UX as settlement:

- **Sui**: the `bridge` module ships in the settlement package (already
  published by settlement's auto-deploy). If `cap_id`/`vault_id` are unset, the
  sequencer calls `bridge::initialize` and persists the created object IDs back
  to the config. No `package_id` to set (it reuses settlement's).
- **L2**: Reth is the source of truth —
  - the `BridgeFactory` is a genesis predeploy at the fixed address
    `0x4200000000000000000000000000000000000010`, and the canonical SUI
    `Bridge` is predeployed at
    `0x4200000000000000000000000000000000000011` (see `solidity/`),
  - the chain id is read from Reth via `eth_chainId`,
  - mint and `createBridge` gas are fixed constants,
  - tokens are created on demand; no per-coin configuration exists.

Two keys, two roles (both required when `enabled`):

- `signer.key_path` (the Sui suiprivkey, `sequencer.key`) — owns `BridgeCap`,
  signs `bridge::initialize` and `bridge::withdraw`.
- `bridge.l2_relayer_key_path` (a secp256k1 EVM key, `relayer.key`) — is the
  factory's `relayer` (set once via `BridgeFactory.initialize` after chain
  start) and signs `mint` and `createBridge`. Fund it with L2 gas.

The bridge is disabled by default; it only runs in sequencer mode.

## Bootstrap: initialize the L2 contracts once

Both predeploys are planted by genesis with **no constructor execution**, so
they boot unconfigured (`initialized == false`, `relayer == address(0)`). Three
one-time calls after the chain is producing blocks.

First, **fund the relayer's EVM address with L2 gas** — it pays for every call
below plus every later `mint` and `createBridge`. The dev genesis only funds
the hardhat account (`0xf39F…`), so send it some native token:

```sh
# RELAYER is the address printed by `podseq keyring generate-evm-key`
# (or the `bridge relayer key loaded` log line). Fund it from the genesis account.
cast send <RELAYER_EVM_ADDRESS> --value 1ether \
  --private-key <GENESIS_ACCOUNT_PRIVATE_KEY>
```

Then bring up the factory and adopt the predeployed SUI token:

```sh
# 1. Initialize the factory (sets the minter for all tokens).
cast send 0x4200000000000000000000000000000000000010 \
  "initialize(address)" <RELAYER_EVM_ADDRESS> \
  --private-key $(cat relayer.key)

# 2. Configure the predeployed SUI token.
cast send 0x4200000000000000000000000000000000000011 \
  "initialize(string,string,string,address)" \
  "Bridged SUI" "SUI" "0x2::sui::SUI" <RELAYER_EVM_ADDRESS> \
  --private-key $(cat relayer.key)

# 3. Adopt it as the canonical SUI token in the factory registry.
cast send 0x4200000000000000000000000000000000000010 \
  "adoptBridge(string,address)" \
  "0x2::sui::SUI" 0x4200000000000000000000000000000000000011 \
  --private-key $(cat relayer.key)
```

(If `cast send` errors `gas required exceeds allowance (0)`, the relayer address
isn't funded — run the funding step above.)

The `initialize` calls are callable by anyone but exactly once — the first
caller configures each contract — so on a fresh chain the operator does them
before anyone else can. `adoptBridge` is relayer-gated. Other coin types need no
bootstrap: the relayer creates their tokens when their first deposits arrive,
and anyone may front-run it with `createBridge(name, symbol, coinType)` for a
friendlier display name. Skipping step 3 does not break bridging (the relayer
would create a fresh SUI token through the factory on the first deposit), but
the predeployed fixed address would go unused — do the adopt while the chain is
young.

### The sequencer does NOT halt while waiting

The relayer is **non-fatal**: the sequencer keeps producing blocks even while
the factory isn't ready (otherwise the `initialize` tx above could never
confirm). The relayer retries setup every few seconds, logging the actionable
cause each time — e.g.:

```
WARN bridge not ready yet; will retry in 5s. ... BridgeFactory predeploy at
     0x4200…0010 is not initialized. Call initialize(relayer) once after chain start.
```

Once `initialize` lands, the next retry succeeds and relaying begins. No restart
needed. The same retry covers other misconfigurations (missing predeploy → run
`solidity/scripts/gen-genesis.sh` and restart Reth; wrong relayer →
`setRelayer`), all without stopping block production.

## Trust model

The sequencer is trusted for ordering and relaying. Both
directions are relayed by the operator, but they are deterministic and
replay-safe:

- Minting requires a strictly increasing Sui deposit nonce, so the sequencer
  cannot inflate supply without a real deposit.
- Releases require a real on-L2 burn, which the sequencer cannot forge for
  another user (burns come from the token holder).
- **Withdrawals are idempotent** on `(coin_type, l2_nonce)` in `bridge::withdraw`
  via a `processed_withdrawals` table, so a relayer crash between the Sui release
  and its cursor persist can never release the same burn twice.
- **Deposits cannot strand**: the vault accepts any coin type, and the factory
  guarantees every coin type can gain exactly one canonical L2 token
  (permissionless `createBridge`, duplicates revert), which the relayer mints
  against. Value can therefore always be represented and later burned for
  release.
- **Only factory-registered tokens can trigger releases**: the relayer's
  withdrawal log query is address-filtered to the factory's tokens, so a
  contract outside the factory emitting forged `WithdrawalInitiated` events can
  never drain the vault.
- **Mints are confirmed by receipt** before the relayer advances its deposit
  cursor, and the cursor is synced against on-chain `lastMintedDepositNonce` /
  `mintedAny` on startup, so an execution revert (e.g. a lost `relayer` role) is
  retried rather than silently skipping a deposit.

The accepted residual risk is the single-sequencer one: the operator, holding
both `BridgeCap` and the `relayer` key, _can_ mint unbacked L2 tokens or release
locked coins without a burn. That is the same trust assumption as the rest of
the chain (the sequencer can reorder/censor). Sequencer censorship or stall will
be tackled in the future.

One cosmetic caveat: the first caller of `createBridge` picks the token's name
and symbol, and `tokenFor` keys on the exact coin-type string. The relayer
always uses the canonical Move spelling (full 64-hex-digit address, no `0x`)
and ignores twins registered under variant spellings, so there is exactly one
token it will ever mint or honor burns for.
