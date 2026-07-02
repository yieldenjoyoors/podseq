# Enshrined Bridge

Podseq ships a trust-minimized bridge between Sui and the L2. Users lock Sui
coins (USDSui, SUI, etc.) in a vault and receive a matching ERC20 on L2; burning
on L2 releases the original coins on Sui. There is **no external relayer**: the
sequencer process itself relays both directions, holding the Sui `BridgeCap` and
the L2 `relayer` role.

```text
            Sui                                   L2 (EVM)
┌──────────────────────────┐         ┌──────────────────────────┐
│ bridge::Vault (shared)   │         │ Bridge.sol (predeploy)   │
│  + BridgeCap (sequencer) │         │  + relayer (sequencer)   │
└────────────┬─────────────┘         └────────────┬─────────────┘
             │                                    │
   deposit<T>(coin, l2_recipient)        initiateWithdrawal(sui_recipient, amt)
             │ deposit_nonce (Table)              │ WithdrawalInitiated (logs)
             ▼                                    ▼
        ┌─────────────  sequencer relayer (in-process)  ─────────────┐
        │  reads deposits by nonce ──► mints on L2 (mint)            │
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

Deposits are generic over `T`, so a single vault serves every bridged asset. Each
coin type is tracked independently, so multiple `Bridge.sol` instances (one per
coin) can share the same vault — each relayer only mints deposits whose
`coin_type` matches its own token.

The Sui gRPC read API exposes no event query, so deposits are also written to the
`deposits` table and read by nonce (one `get_object` per nonce), exactly like the
settlement registry. See [Settlement Contract](./contract.md).

## L2 side: `Bridge.sol`

One predeploy per bridged coin, behaving as an ERC20 (9 decimals, matching Sui).
The `relayer` (sequencer EVM key) is the only minter; mint nonces must strictly
increase, so a retried relay is a safe no-op. Users call
`initiateWithdrawal(bytes32 suiRecipient, uint256 amount)` to burn; the relayer
watches `WithdrawalInitiated` logs and calls `bridge::withdraw` on Sui.

See `solidity/README.md` for compile and predeploy instructions.

## Rust: `podseq_sui::bridge` and the relayer

- `crates/sui/src/bridge.rs` — Sui vault client: reads deposits by nonce
  (`deposit_at`), reads the next nonce (`deposit_nonce`), and submits releases
  (`BridgeClient::withdraw`). Mirrors the settlement reader/writer.
- `crates/node/src/bridge.rs` — the in-process relayer (`BridgeRelayer`): polls
  Sui deposits → signs and sends `mint` to the L2, polls L2 `WithdrawalInitiated`
  logs → calls `bridge::withdraw` on Sui. Cursors are persisted to
  `bridge_cursors.json` so a restart resumes without re-minting or re-releasing.

## Configuration

Generate the relayer's EVM key with the keyring (it writes a 32-byte
secp256k1 scalar; the printed address must hold the L2 `relayer` role):

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
  - the `Bridge` is a genesis predeploy at the fixed address
    `0x4200000000000000000000000000000000000010` (see `solidity/`),
  - its `coinType()` is read at startup (which coin this instance bridges),
  - the chain id is read from Reth via `eth_chainId`,
  - mint gas is a fixed constant (`MINT_GAS_LIMIT`).

Two keys, two roles (both required when `enabled`):

- `signer.key_path` (the Sui suiprivkey, `sequencer.key`) — owns `BridgeCap`,
  signs `bridge::initialize` and `bridge::withdraw`.
- `bridge.l2_relayer_key_path` (a secp256k1 EVM key, `relayer.key`) — holds the
  L2 `relayer` role (set once via `Bridge.initialize` after chain start) and
  signs `mint`. Fund it with L2 gas.

The bridge is disabled by default; it only runs in sequencer mode.

## Bootstrap: initialize the L2 contract once

The `Bridge` is a genesis predeploy: its bytecode is planted at
`0x4200…0010`, but **genesis runs no constructor**, so it boots unconfigured
(`initialized == false`, empty `coinType`, `relayer == address(0)`). One manual
step after the chain is producing blocks.

First, **fund the relayer's EVM address with L2 gas** — it pays for `initialize`
and every later `mint`. The dev genesis only funds the hardhat account
(`0xf39F…`), so send it some native token:

```sh
# RELAYER is the address printed by `podseq keyring generate-evm-key`
# (or the `bridge relayer key loaded` log line). Fund it from the genesis account.
cast send <RELAYER_EVM_ADDRESS> --value 1ether \
  --private-key <GENESIS_ACCOUNT_PRIVATE_KEY>
```

Then initialize the contract:

```sh
cast send 0x4200000000000000000000000000000000000010 \
  "initialize(string,string,string,address)" \
  "Bridged USDSui" "USDS" "0x2::sui::SUI" <RELAYER_EVM_ADDRESS> \
  --private-key $(cat relayer.key)
```

(If `cast send` errors `gas required exceeds allowance (0)`, the relayer address
isn't funded — run the funding step above.)

`initialize` is callable by anyone but exactly once — the first caller sets the
initial `relayer`.

### The sequencer does NOT halt while waiting

The relayer is **non-fatal**: the sequencer keeps producing blocks even while
the L2 contract isn't ready (otherwise the `initialize` tx above could never
confirm). The relayer retries setup every few seconds, logging the actionable
cause each time — e.g.:

```
WARN bridge not ready yet; will retry in 5s. ... Bridge predeploy at
     0x4200…0010 is not initialized. Call initialize(...) once after chain start.
```

Once `initialize` lands, the next retry succeeds and relaying begins. No restart
needed. The same retry covers other misconfigurations (missing predeploy → run
`solidity/scripts/gen-genesis.sh`; wrong relayer → `setRelayer`), all without
stopping block production.

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
- **Mints are confirmed by receipt** before the relayer advances its deposit
  cursor, and the cursor is synced against on-chain `lastMintedDepositNonce` /
  `mintedAny` on startup, so an execution revert (e.g. a lost `relayer` role) is
  retried rather than silently skipping a deposit.

The accepted residual risk is the single-sequencer one: the operator, holding
both `BridgeCap` and the `relayer` key, _can_ mint unbacked L2 tokens or release
locked coins without a burn. That is the same trust assumption as the rest of
the chain (the sequencer can reorder/censor). Sequencer censorship or stall will
be tackled in the future.
