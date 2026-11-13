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

Authenticated peer connections use a Commonware ed25519 key.

### Bridge relayer key (optional)

The enshrined bridge ([bridge](./bridge.md)) mints on L2 for Sui deposits and
releases on Sui for L2 burns. It needs a separate **secp256k1** EVM key (the L2
uses EVM signatures; Sui uses Ed25519 so they can't share a key). Generate it
with podseq:

```sh
podseq keyring generate-evm-key --out relayer.key
```

The printed address must be set as the L2 `Bridge` `relayer` (via the contract's
one-time `initialize`) and funded with L2 gas.

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
# batch_flush_ms = 120000   # flush once the oldest block waited this long (0 = off)

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

# Bridge (optional, sequencer mode only). On first start, if cap_id/vault_id are
# unset, the sequencer calls bridge::initialize and writes them back here.
# [bridge]
# enabled = true
# l2_relayer_key_path = "relayer.key"
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

## Rate limiting the public RPC

Reth serves the public JSON-RPC on port 8545 and the JWT-authenticated Engine
API on 8551. The node ships a rate-limiting proxy that sits in front of the
public port: it forwards POST requests to Reth on loopback and caps each client
IP with a token bucket (default 10 requests/s sustained, burst of 25). Over
budget requests get `429` with a `Retry-After` header. Bodies over
`max_body_bytes` get `413` without touching Reth, and once `max_in_flight`
upstream calls are in progress further requests get `503` until Reth drains.

Enable it in the config:

```toml
[rpc_proxy]
enabled = true
listen_addr = "0.0.0.0:18545"
upstream = "http://127.0.0.1:8545"
# requests_per_second = 10.0
# burst = 25
# max_body_bytes = 1048576
# max_in_flight = 256
# trusted_proxies = []   # LB IPs whose X-Forwarded-For is believed
```

Point users at port 18545 and keep Reth's 8545 bound to loopback. Behind a
load balancer, add its IP to `trusted_proxies`: the proxy only reads
`X-Forwarded-For` from peers on that list, so a direct client cannot choose its
own bucket by forging the header. The counters are on the metrics endpoint
(`podseq_rpc_proxy_requests_total`, `podseq_rpc_proxy_limited_total`); a
limited counter that climbs under normal traffic means the defaults are too
tight.

## Full node

A full node reconstructs the chain from DA + settlement, verifies every block
signature against the sequencer's public key, and re-executes blocks against a
local Reth. Only the sequencer produces blocks.

Set `mode = "full"` in the config.

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

The node refuses to start if `sequencer_pubkey` or `registry_id` is missing.
Full-node mode only needs `[reth]`, `[signer] sequencer_pubkey`, and
`[sui] registry_id` — `signer.key_path`, `sui.settler_cap_id`,
`sui.settlement_package_id`, and `walrus.publisher_url` are sequencer-only.
The sequencer can also carry a funded reserve key (`sui.reserve_key_path`,
plus `sui.gas_min_balance_mist`, `sui.gas_topup_amount_mist`,
`sui.gas_reserve_floor_mist`) that tops up the settlement key's gas
automatically; see [metrics](./metrics.md#keeping-the-settlement-key-funded).

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

## Docker Compose

The recommended way to run a local dev node is with Docker Compose:

```sh
docker compose \
  -f docker-compose.yml -f docker-compose.testnet.yml up -d
```

This starts Reth, podseq, a **Blockscout block explorer** (http://localhost:4000),
and an optional **development faucet** (http://localhost:8080, requires `FAUCET_PRIVATE_KEY`).
Both are preconfigured for local development. To disable either, comment out the
relevant services in `docker-compose.yml` (explorer) or `docker-compose.testnet.yml`
(faucet).

## Troubleshooting

| Symptom                               | Fix                                                                                                                                                                                                      |
| ------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `loading JWT secret ... failed`       | `jwt.hex` must be a 32-byte hex string shared with Reth.                                                                                                                                                 |
| `no signer key configured`            | Add `signer.key_path` (a `suiprivkey` file) in sequencer mode.                                                                                                                                           |
| `sequencer_pubkey is required ...`    | Full node mode needs `signer.sequencer_pubkey` (from the sequencer banner).                                                                                                                              |
| `sui.registry_id is required ...`     | Full node mode needs the shared Registry object ID (from the sequencer config).                                                                                                                          |
| `no settlement_package_id configured` | Sequencer mode **requires** settlement: deploy the Move package and set `sui.settlement_package_id`, `sui.settler_cap_id`, and `sui.registry_id` (or leave all three unset for first-start auto-deploy). |
| `p2p is not configured`               | Add a `[p2p]` section to config. The identity key is auto-generated on first start.                                                                                                                      |
