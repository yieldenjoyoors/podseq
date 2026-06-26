# Development Guide

## Prerequisites

- Rust stable (managed via `rust-toolchain.toml`)
- A Reth node, Walrus access, and a Sui RPC for integration

## CLI

```sh
podseq init --out podseq.toml               # generate a config file
podseq keyring generate-settlement          # generate a settlement key
podseq keyring generate-block               # generate a block signing key
podseq keyring generate-p2p                 # generate a p2p identity key
podseq keyring list                         # show configured keys
podseq status                               # query Reth height + settlement
podseq start                                # start the node
```

## Node modes

The node runs in two modes, selected by `mode` in the config:

- `mode = "sequencer"` (default): produces blocks, broadcasts them via P2P, posts to
  Walrus, and settles on Sui.
- `mode = "full"`: syncs from DA + settlement + P2P. Reads commitments from the Sui
  Registry, fetches blocks from Walrus, polled P2P fast-sync, and applies them to Reth.

## Build

```sh
cargo build --release
```

The binary is built at `target/release/podseq`.

## Run

Podseq is configured via a single TOML file. Generate a starting point:

```sh
podseq init --out podseq.toml
```

Then start the node:

```sh
podseq start --config podseq.toml
```

Or set the path via environment:

```sh
PODSEQ_CONFIG=podseq.toml podseq start
```

A minimal config only needs the Reth section (everything else defaults to testnet):

```toml
[reth]
engine_url = "http://localhost:8551"
# rpc_url = "http://localhost:8545"    # mempool queries
jwt_path = "jwt.hex"
```

To enable p2p networking, add `[p2p]`:

```toml
[p2p]
key_path = "p2p.key"
listen_addr = "0.0.0.0:9000"
```

The p2p identity key is generated automatically if not present, or via:

```sh
podseq keyring generate-p2p --out p2p.key
```

To sign settlement transactions in-process, add `suiprivkey` paths:

```toml
[signer]
settlement_key_path = "sui.key"
block_key_path = "block.key"
```

## Test

```sh
cargo test --all
```

Unit tests live in each crate under `#[cfg(test)] mod tests`. The node
binary uses the Commonware deterministic runner for test isolation.

## Lint and format

```sh
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

All code must pass both before a change is considered complete.

## Documentation

The documentation lives in `docs/` and is rendered by the site in `web/`. Edit any
file under `docs/src/` and the site picks it up on the next reload. No build step.

## Project layout

```text
podseq/
├── crates/
│   ├── core/        # Interfaces and Commonware runtime bridge
│   ├── engine/      # Reth Engine API client
│   ├── sequencer/   # Ordering + block signing
│   ├── store/       # Persistent storage (blocks, state, crash recovery)
│   ├── sui/         # Walrus DA publishing + Sui settlement
│   ├── p2p/         # Block propagation (Commonware discovery + broadcast)
│   └── node/        # Binary: CLI, config, runner, full node
├── docs/            # Markdown source for the docs
├── web/             # Site that renders the docs
├── Cargo.toml       # Workspace
└── LICENSE
```

## Contributing

Open an issue first for non-trivial changes so the approach can be discussed. Keep commits
focused and atomic. Every change should pass `cargo test`, `cargo clippy`, and
`cargo fmt --check`.
