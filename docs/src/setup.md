# Node Setup

This guide sets up a podseq sequencer node: Reth, Walrus access, a signer key, and
p2p identity.

## Prerequisites

- [Rust](https://www.rust-lang.org) stable (`rust-toolchain.toml` pins it)
- [Reth](https://reth.rs) running in L2 mode (Engine API on `localhost:8551`)
- Access to a Walrus publisher and aggregator (Testnet endpoints are built in)
- A funded Sui address for settlement (SUI for gas)

## 1. Reth (execution layer)

Generate a shared JWT secret:

```sh
openssl rand -hex 32 > jwt.hex
```

Start Reth with `--authrpc.jwtsecret jwt.hex` on its Engine API port (`8551`). Both
Reth and podseq must use the same `jwt.hex`.

## 2. Keys

### Signer key

A single Ed25519 `suiprivkey` (Bech32) serves double duty: it signs settlement
transactions on Sui and block headers. Generate it with the Sui CLI:

```sh
sui keytool generate ed25519
```

Or use podseq:

```sh
podseq keyring generate-key --out sequencer.key
```

Save the `suiprivkey...` string to a file (e.g. `sequencer.key`). The corresponding
address needs SUI for gas.

### P2P identity key

Authenticated peer connections use a Commonware ed25519 key (32 hex bytes in a file):

```sh
podseq keyring generate-p2p --out p2p.key
```

The file contains a 64-char hex string.

## 3. podseq configuration

Generate a starting config:

```sh
podseq init config --out podseq.toml
```

Edit it:

```toml
[reth]
engine_url = "http://localhost:8551"
# rpc_url = "http://localhost:8545"    # mempool queries (defaults to 8545)
jwt_path = "jwt.hex"

[walrus]
publisher_url = "https://publisher.walrus-testnet.walrus.space"
aggregator_url = "https://aggregator.walrus-testnet.walrus.space"
# epochs = 53               # storage lifetime; default = max (53 ≈ 2 years)
# batch_size_bytes = 65536  # flush a DA blob once blocks reach this size

[sui]
rpc_url = "https://fullnode.testnet.sui.io:443"
# settlement_package_id is only needed by the sequencer (for committing blocks).

[p2p]
key_path = "p2p.key"
listen_addr = "0.0.0.0:9000"
# bootstrap_peers = ["abc123@1.2.3.4:9000"]   # optional

[signer]
# Sequencer mode: private key (suiprivkey) that signs settlement + block headers.
key_path = "sequencer.key"
# Full node mode: the sequencer's ed25519 public key (hex) to verify blocks.
# Required in full node mode; ignored in sequencer mode.
# sequencer_pubkey = "0x..."
```

A minimal sequencer config only needs `[reth] jwt_path` and `[signer] key_path`.
Everything else defaults to Testnet. **Full node** mode requires
`[signer] sequencer_pubkey` instead of `key_path` (see [Full node](#full-node)).

## 4. Run

```sh
podseq start --config podseq.toml
```

The node loads the JWT, connects to Reth, builds the Sui-layer client, starts the p2p
network, and begins the sequencer loop. On startup the sequencer prints its derived
address and public key in a banner:

```
╔════════════════════════════════════════════════════════════════╗
║ SEQUENCER ADDRESS: 0x...
║ sequencer pubkey:  0x...
╚════════════════════════════════════════════════════════════════╝
```

Copy the `sequencer pubkey` value into the `signer.sequencer_pubkey` field of any
full node that should verify this sequencer's blocks.

## Full node

A full node does **not** produce blocks. It reconstructs the chain from DA +
settlement, verifies every block signature against the sequencer's public key,
and re-executes blocks against a local Reth.

Set `mode = "full"` (or run `podseq start --mode full`).

### Minimal full node config

```toml
mode = "full"

[reth]
engine_url = "http://localhost:8551"
jwt_path   = "jwt.hex"

[signer]
# The sequencer's ed25519 public key (hex).
sequencer_pubkey = "0x..."

[sui]
rpc_url = "https://fullnode.testnet.sui.io:443"
registry_id           = "0x..."   # the shared Registry object
```

The node refuses to start if `sequencer_pubkey` or `registry_id` is missing. A full node does **not** use `signer.key_path`, `sui.settler_cap_id`, `sui.settlement_package_id` or
`walrus.publisher_url`.

### Fast-sync (optional)

Add a `[p2p]` section and point `bootstrap_peers` at the
sequencer to receive block announcements ahead of DA finality:

```toml
[p2p]
key_path = "p2p.key"
listen_addr = "0.0.0.0:9001"
bootstrap_peers = ["<sequencer-pubkey-hex>@<seq-host>:9000"]
```

p2p blocks are applied as an **optimistic head only**; they are never marked
safe or finalized. Applying and finalizing are separate flows: a block's
payload is submitted to Reth once (via p2p, or via DA if there was no p2p), and
finality only advances the `safe`/`finalized` fork-choice pointers. When DA
settles a height already applied via p2p, the block is just finalized (the DA
block hash is checked against Reth's block at that height; only a mismatch
re-applies and reorgs). Without p2p the node still syncs purely from DA +
settlement, at finality lag.

### Sync behavior

- The DA sync cursor (`synced_height`) and the last **DA-verified** fork-choice
  hashes (`head`/`safe`/`finalized`) are persisted to `data_dir` and resumed on
  restart, so the node skips re-reading the head from Reth. On first run (no
  state yet) it discovers the head from Reth instead. Only DA-settled blocks
  advance the cursor or the persisted state; optimistic p2p heads are not
  persisted.
- A block whose signature fails verification is rejected and sync halts with an
  error.

## Troubleshooting

| Symptom                               | Fix                                                                                                                                                                                                      |
| ------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `loading JWT secret ... failed`       | `jwt.hex` must be a 32-byte hex string shared with Reth.                                                                                                                                                 |
| `no signer key configured`            | Add `signer.key_path` (a `suiprivkey` file) in sequencer mode.                                                                                                                                           |
| `sequencer_pubkey is required ...`    | Full node mode needs `signer.sequencer_pubkey` (from the sequencer banner).                                                                                                                              |
| `sui.registry_id is required ...`     | Full node mode needs the shared Registry object ID (from the sequencer config).                                                                                                                          |
| `no settlement_package_id configured` | Sequencer mode **requires** settlement: deploy the Move package and set `sui.settlement_package_id`, `sui.settler_cap_id`, and `sui.registry_id` (or leave all three unset for first-start auto-deploy). |
| `p2p is not configured`               | Add `[p2p] key_path` to config, or run `podseq keyring generate-p2p`.                                                                                                                                    |
