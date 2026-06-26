# Design Decisions

Each section records what was chosen and why.

## Language and runtime

**Rust with Tokio.** The execution layer (Reth) is Rust. Sharing the language avoids
FFI or JSON boundaries for Engine API types, gives a single compile target, and lets
`cargo build` compile the whole stack. Tokio provides the async runtime; the
Commonware Tokio backend (`commonware-runtime`) coexists so Commonware primitives
(p2p, broadcast, storage) share the same executor.

## Execution: Reth via the Engine API

**Standalone Reth driven over the authenticated Engine API.** The Engine API is the
stable L2 integration point; Reth's internal crate APIs have no semver guarantees.
Running Reth as a separate process gives process isolation, fast compilation, and
independent upgrades. Podseq targets EVM L2s. A pluggable execution interface would
add abstraction cost for a single real target.

## Data availability: Walrus

Walrus provides Byzantine-fault-tolerant availability through erasure coding (~5x
overhead) and stays available with up to 2/3 of storage nodes down. Because Walrus
blob availability is mediated by Sui objects, the settlement chain can directly
verify that a blob is available and for how long. No external bridge.

## Settlement: Sui

Sui and Walrus are designed together. Sui verifies blob availability, records
sequencer commitments, and handles storage payments and epoch transitions on the
same chain as DA attestation.

## P2P: Commonware

`commonware-p2p` (authenticated, encrypted peer discovery) and
`commonware-broadcast` (buffered block propagation) provide the networking layer.
The sequencer broadcasts each signed block so full nodes can execute it before
DA confirmation. The protocol uses two channels: a buffered engine for block data,
and a raw channel for `(height, digest)` announcements so full nodes can poll for
new blocks and pull them from the broadcast cache.

## Signing model

Three keys, kept separate for operational and cryptographic hygiene:

| Key           | Config field                 | Format          | Purpose                                       |
| ------------- | ---------------------------- | --------------- | --------------------------------------------- |
| Settlement    | `signer.settlement_key_path` | suiprivkey      | Signs settlement Move transactions on Sui.    |
| Block signing | `signer.block_key_path`      | suiprivkey      | Signs produced block headers for attribution. |
| P2P identity  | `p2p.key_path`               | 32-byte ed25519 | Authenticates peer connections (Commonware).  |

Block signing and settlement both use `suiprivkey` for operational consistency
(one key format for the operator to manage). The p2p identity key uses
Commonware's native ed25519.

## Node-as-publisher

The podseq node publishes blobs to Walrus and signs the Sui transactions that
register them. The publishing path runs inside the node: latency, reliability, and
cost stay under the sequencer's control.

The node uses the lightweight [`sui-rust-sdk`](https://github.com/MystenLabs/sui-rust-sdk)
for all Sui interaction. Walrus publishing and settlement are both signed
in-process, without the `sui-sdk` monorepo.

## Core crate: `podseq-core`

`podseq-core` defines interfaces and types shared by every other crate.
It carries `commonware-runtime`. The runtime bridge (type aliases, trait
re-exports) is available to any consumer without a separate crate. The
`serde` dependency is gated behind a feature flag; downstream crates
opt in to serialization.

## Error handling

Library crates use `thiserror` for typed, matchable error enums so callers can
react to specific failures. The `node` binary uses `anyhow` for top-level
error handling. It only needs to log and exit.

## Configuration

All node settings live in a single TOML file (`--config`), not scattered CLI
flags. The node wires together Reth, Walrus, Sui, and p2p. Too many moving
parts for flags. A typed `Config` struct with serde defaults keeps the minimal
case (just the Reth JWT path) one line. Use `podseq init` to generate a starting
point.

## License: Apache 2.0

Apache 2.0 grants an explicit patent license from contributors to users. It is
compatible with Reth's, Walrus's, and the Sui SDK's licensing.
