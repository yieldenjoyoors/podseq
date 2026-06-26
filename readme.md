# Podseq

Podseq is a Rust sequencer that posts EVM block data to [Walrus](https://docs.wal.app)
and settles on [Sui](https://sui.io). It drives [Reth](https://github.com/paradigmxyz/reth)
over the Engine API, forming a fully EVM-compatible L2 that uses Walrus as its data
availability layer and Sui for settlement and verification. Networking, storage, and
cryptography primitives are provided by [Commonware](https://commonware.xyz).

| Aspect       | Choice                                                                           |
| ------------ | -------------------------------------------------------------------------------- |
| Language     | Rust                                                                             |
| Execution    | Standalone Reth (Engine API)                                                     |
| DA           | Walrus                                                                           |
| Settlement   | Sui (Move contract)                                                              |
| P2P          | Commonware (p2p + broadcast)                                                     |
| Sui SDK      | [sui-rust-sdk](https://github.com/MystenLabs/sui-rust-sdk)                       |
| Walrus types | [walrus-core](https://github.com/MystenLabs/walrus/tree/main/crates/walrus-core) |

## Architecture

```text
┌─────────────────────────────────────────────────────────────────┐
│                          Client Apps                            │
│              (wallets, dapps, indexers, RPC)                    │
└─────────────────────────────┬───────────────────────────────────┘
                              │ JSON-RPC (Reth port 8545)
┌─────────────────────────────▼───────────────────────────────────┐
│                            Podseq                               │
│                                                                 │
│  Production loop (per tick):          Finalizer (background):   │
│    build → accept → sign                publish → Walrus         │
│         ↓                               commit → Sui             │
│    persist + broadcast → P2P            finalize → Reth          │
│                                                                 │
│  Engine API (port 8551, JWT)     DA (Walrus HTTP)  L1 (Sui gRPC)│
└─────────────────────────────────────────────────────────────────┘
```

Production and DA finalization run **concurrently**; block production never blocks on
DA latency. Blocks are persisted locally and broadcast via P2P before DA confirmation.

## Node modes

- `mode = "sequencer"` (default): produces blocks, broadcasts via P2P, posts to Walrus,
  settles on Sui
- `mode = "full"`: syncs from DA + settlement + P2P fast-sync, applies blocks to a
  local Reth

## CLI

```sh
podseq init config --out podseq.toml         # generate a config file
podseq keyring generate-settlement            # generate a settlement key (suiprivkey)
podseq keyring generate-block                 # generate a block signing key
podseq keyring generate-p2p                   # generate a p2p identity key
podseq keyring list                           # show configured keys
podseq status                                 # query Reth height + settlement config
podseq start                                  # start the node
```

## Crate layout

```text
crates/
├── core/        # Interfaces, types, and the Commonware runtime bridge
├── engine/      # Reth Engine API client (build, accept, finalize, JWT auth)
├── sequencer/   # SingleSequencer + Ed25519BlockSigner
├── store/       # Persistent storage (blocks, chain state, crash recovery)
├── sui/         # Walrus DA (HTTP) + Sui settlement (in-process sui-rust-sdk)
├── p2p/         # Commonware networking (discovery + broadcast + announce)
└── node/        # Binary: CLI, config, runner, full node sync
```

## Dependencies

| Library                                                             | Source                       | Role                              |
| ------------------------------------------------------------------- | ---------------------------- | --------------------------------- |
| `commonware-p2p`, `commonware-broadcast`, `commonware-runtime`      | crates.io (v2026.5)          | P2P networking and runtime bridge |
| `commonware-cryptography`, `commonware-codec`, `commonware-storage` | crates.io (v2026.5)          | Identity keys, wire encoding, I/O |
| `walrus-core`                                                       | git (no `sui-types` feature) | Blob types, encoding              |
| `sui-sdk-types`, `sui-transaction-builder`, `sui-crypto`, `sui-rpc` | git (sui-rust-sdk)           | In-process tx building + signing  |
| `alloy-rpc-types-engine`                                            | crates.io                    | Engine API types + JWT            |
| `reqwest`, `serde`, `tokio`, `clap`, `toml`                         | crates.io                    | HTTP, serialization, runtime, CLI |

## Build

```sh
cargo build --release
```

## Quick start

```sh
# 1. Generate a JWT secret (shared with Reth)
head -c 32 /dev/urandom | od -A n -t x1 | tr -d ' \n' > jwt.hex

# 2. Generate keys
podseq keyring generate-settlement --out sui.key
podseq keyring generate-block --out block.key
podseq keyring generate-p2p --out p2p.key

# 3. Generate and edit a config
podseq init config --out podseq.toml

# 4. Start the sequencer (deploys settlement contract on first start)
podseq start --config podseq.toml
```

## Settlement contract

The settlement Move contract (`move/sources/settlement.move`) is an append-only registry
on Sui mapping block height → Walrus blob ID. Only the sequencer (SettlerCap holder)
can write. Full nodes read it to discover which blocks have data available on Walrus.
On first start, the sequencer deploys the contract and writes the object IDs back to
the config.

## Test

```sh
cargo test --all
cargo clippy --all-targets
cargo fmt --all -- --check
```

## License

Licensed under the [Apache License, Version 2.0](./LICENSE).
