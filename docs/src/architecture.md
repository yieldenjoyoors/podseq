# Architecture

Podseq separates four concerns: ordering transactions, executing them against an EVM,
publishing the resulting blocks to a DA layer, and settling commitments on L1.
Each concern maps to a crate.

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
│  │ (ordering)│  │ (Engine)  │  │(Commonware)│  │  Submitter   │  │
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
├── core/        # Interfaces only
├── engine/      # Reth Engine API client
├── sequencer/   # Ordering + block signing
├── store/       # Persistent storage (blocks, state, pending crash-recovery)
├── sui/         # Sui layer: Walrus DA + settlement (one shared wallet)
├── p2p/         # Block propagation (Commonware discovery + broadcast)
└── node/        # Binary wiring everything together
```

`podseq-sui` owns both Walrus data availability and Sui settlement. Both go through
a single Sui wallet, so they live in the same crate. See
[Sui Settlement](./components/sui.md).

## Design principles

1. **Zero-dependency core.** `podseq-core` contains only traits and types with no
   external dependencies. Interfaces are stable; any implementation can be swapped
   without touching consumers.

2. **One responsibility per crate.** Sequencing, execution, DA, settlement, and
   networking each live in their own crate and communicate only through core traits.

3. **Rust-native stack.** Async with Tokio, errors via `thiserror` in libraries and
   `anyhow` in the binary, serialization with Serde. See
   [Design Decisions](./design-decisions.md) for rationale.

4. **Settlement-anchored DA.** Block data lives on Walrus. The Walrus blob ID and the
   sequencer commitment are anchored on Sui. A full node can independently verify
   availability from those two sources.

## Data flow

The end-to-end path of a transaction, from mempool to settlement, is described in
[Block Production](./block-production.md).
