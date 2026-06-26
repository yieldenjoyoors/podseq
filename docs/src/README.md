# Introduction

Podseq is a sequencer written in Rust. It writes EVM block data to
[Walrus](https://docs.wal.app) and settles on [Sui](https://sui.io). It drives an
out-of-process [Reth](https://github.com/paradigmxyz/reth) node over the Engine API,
forming a fully EVM-compatible L2. Walrus provides the data availability layer;
Sui handles settlement and verification. Networking, consensus, and storage
primitives come from [Commonware](https://commonware.xyz).

## Status

Under active development. The sequencer, Engine API client, Walrus DA publishing,
Sui settlement, persistent storage, crash recovery, and p2p block propagation
(via Commonware) are all implemented. Multi-sequencer consensus is pending.

## At a glance

| Aspect          | Choice                       |
| --------------- | ---------------------------- |
| Language        | Rust                         |
| Execution       | Standalone Reth (Engine API) |
| DA / settlement | Walrus / Sui                 |
| P2P / consensus | Commonware                   |

## Where to start

- New to the project? Read [Architecture](./architecture.md).
- Want to follow the data flow? See [Block Production](./block-production.md).
- Building or contributing? See the [Development Guide](./development.md).
