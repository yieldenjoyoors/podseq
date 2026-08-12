# Architecture

Podseq separates four concerns: producing blocks on a timer, executing them against an EVM,
publishing the resulting blocks to a DA layer, and settling commitments on L1.  
Note: Transaction selection and ordering within a block are
handled by Reth, not podseq: see [Sequencer](./components/sequencer.md).)

## System overview

```text
┌─────────────────────────────────────────────────────────────────┐
│                          Client Apps                            │
│              (wallets, dapps, indexers, RPC)                    │
└─────────────────────────────┬───────────────────────────────────┘
                              │ JSON-RPC / Engine API
┌─────────────────────────────▼───────────────────────────────────┐
│                            Podseq                               │
│  ┌───────────┐  ┌───────────┐  ┌───────────┐  ┌──────────────┐  │
│  │ Sequencer │  │  Reth     │  │    P2P    │  │   Walrus     │  │
│  │ (signing) │  │ (Engine)  │  │(Commonware)│  │  Submitter   │  │
│  └─────┬─────┘  └─────┬─────┘  └─────┬─────┘  └───────┬──────┘  │
└────────┼──────────────┼──────────────┼────────────────┼─────────┘
         │              │              │                │
         ▼              ▼              ▼                ▼
   In-process        Reth node     Block gossip     Walrus DA
   sequencer        (EVM state)    (soft           (erasure-coded)
   loop                            confirmations)
                                                       │
                                                       ▼
                                                    Sui L1
                                              (attestations,
                                               settlement)
```

## Crate layout

```text
crates/
├── core/        # Interfaces, types, Ed25519BlockSigner, Commonware runtime bridge
├── engine/      # Reth Engine API client
├── sui/         # Sui layer: Walrus DA + settlement + bridge vault (one wallet)
├── p2p/         # Block propagation (Commonware discovery + broadcast)
└── node/        # Binary: CLI, config, runner, full node, bridge relayer,
                 #        and the `store` module (blocks, state, crash recovery)
move/            # Settlement contract (Sui Move)
solidity/        # EVM-side contracts
e2e/             # Integration tests against a real Reth container
docs/            # Project documentation
```

`podseq-sui` owns Walrus data availability, Sui settlement, and the Sui side of
the enshrined bridge. All go through a single Sui wallet, so they live in the
same crate. See [Sui Settlement](./components/sui.md) and
[Bridge](./bridge.md).

## Design principles

1. **Zero-dependency core.** `podseq-core` contains only traits, types, and the
   canonical `BlockSigner` implementation, with no external dependencies beyond
   the Sui signer primitives. Interfaces are stable; any implementation can be
   swapped without touching consumers.

2. **One responsibility per crate.** Execution, DA/settlement, and networking
   each live in their own crate and communicate only through core traits. Block
   signing and persistent storage are single-consumer concerns and live next to
   their consumers (signing in `core`, storage in the `node` binary).

3. **Rust-native stack.** Async with Tokio, errors via `thiserror` in libraries and
   `anyhow` in the binary, serialization with Serde.

4. **Settlement-anchored DA.** Block data lives on Walrus. The Walrus blob ID and the
   sequencer commitment are anchored on Sui. A full node can independently verify
   availability from those two sources.

## Data flow

The end-to-end path of a transaction, from mempool to settlement, is described in
[Block Production](./block-production.md).
