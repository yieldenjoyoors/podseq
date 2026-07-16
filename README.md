# Podseq

Podseq is a Rust sequencer that posts EVM block data to [Walrus](https://docs.wal.app)
for data availability and settles on [Sui](https://sui.io). It drives an out-of-process
[Reth](https://github.com/paradigmxyz/reth) node over the Engine API, forming a fully
EVM-compatible L2. Networking, storage, and cryptography primitives come from
[Commonware](https://commonware.xyz).

| Aspect          | Choice                       |
| --------------- | ---------------------------- |
| Execution       | Standalone Reth (Engine API) |
| DA / settlement | Walrus / Sui                 |
| P2P / consensus | Commonware                   |

## How it works

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

Block production and DA finalization run concurrently; producing blocks never blocks
on DA latency. Blocks are persisted locally and broadcast over P2P before DA
confirmation, so full nodes can sync fast.

A node runs as one of:

- `mode = "sequencer"` (default): produces blocks, broadcasts over P2P, posts to
  Walrus, and settles on Sui.
- `mode = "full"`: syncs from DA + settlement + P2P fast-sync and applies blocks to a
  local Reth.

For the full data flow and design context, see the
documentation at **[podseq.xyz/#/docs](https://podseq.xyz/#/docs)**.

## Repository layout

```text
crates/
├── core/        # Interfaces, types, Commonware runtime bridge
├── engine/      # Reth Engine API client (build, accept, finalize, JWT)
├── sequencer/   # Ed25519BlockSigner (block header signing)
├── store/       # Persistent storage (blocks, chain state, recovery)
├── sui/         # Walrus DA (HTTP) + Sui settlement + bridge vault client
├── p2p/         # Commonware networking (discovery + broadcast + announce)
└── node/        # Binary: CLI, config, runner, full node sync, bridge relayer
move/            # Settlement contract (Sui Move)
solidity/        # EVM-side contracts
e2e/             # Integration tests against a real Reth container
docs/            # Project documentation
```

Each crate has a dedicated page in the [components section](https://podseq.xyz/#/docs/components/core) of the docs.

## Build

Requirements: a recent Rust toolchain (see [`rust-toolchain.toml`](./rust-toolchain.toml)).

```sh
cargo build --release
```

The resulting binary is at `target/release/podseq`.

## Quick start

```sh
# 1. Generate a JWT secret (shared with Reth)
head -c 32 /dev/urandom | od -A n -t x1 | tr -d ' \n' > jwt.hex

# 2. Generate keys
podseq keyring generate-key --out sequencer.key    # Sui ed25519 (settlement + blocks)
podseq keyring generate-evm-key --out relayer.key  # bridge relayer (optional, secp256k1)

# 3. Generate and edit a config
podseq init config --out podseq.toml

# Do not forget to have Reth started prior to starting the sequencer.

# 4. Start the node (the sequencer deploys the settlement contract on first start)
podseq start --config podseq.toml
```

For a complete walkthrough: Reth setup, Walrus/Sui endpoints, key configuration, and
node modes; see the [Node Setup](https://podseq.xyz/#/docs~setup) guide.

### Block explorer and faucet

The Docker Compose stack includes a [Blockscout](https://github.com/blockscout/blockscout)
block explorer and an optional [eth-faucet](https://github.com/chainflag/eth-faucet),
both preconfigured for local development.

```sh
docker compose \
  -f docker-compose.yml -f docker-compose.testnet.yml up -d
```

- **Explorer UI**: http://localhost:4000
- **Faucet UI**: http://localhost:8080 (requires `FAUCET_PRIVATE_KEY` env var)

### Useful CLI commands

```sh
podseq init config --out podseq.toml   # generate a config file
podseq keyring generate-key            # sequencer key (Sui ed25519)
podseq keyring generate-evm-key        # bridge relayer EVM key (secp256k1)
podseq keyring list                    # show configured keys
podseq status                          # query Reth height + settlement config
podseq start                           # start the node
```

## Test

```sh
cargo test --all
cargo clippy --all-targets
cargo fmt --all -- --check
```

End-to-end integration tests run podseq against a real Reth container; see
[`e2e/README.md`](./e2e/README.md):

```sh
cargo test -p podseq-e2e -- --test-threads=1 --nocapture
```

## Documentation

Full documentation is at **[podseq.xyz/#/docs](https://podseq.xyz/#/docs)**:
architecture, block production, node setup, the settlement contract, the bridge,
the [roadmap](https://podseq.xyz/#/docs/roadmap), per-crate component notes,
and the development guide. Sources live in [`docs/src/`](./docs/src/).

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md).

## License

[Apache-2.0](./LICENSE).
