# Sequencer (`podseq-sequencer`)

The sequencer crate holds pending transactions in a FIFO queue and signs produced
block headers. It exposes [`BlockSigner`](./core.md#blocksigner) for signature
verification by full nodes.

## `SingleSequencer`

A single-operator sequencer: one designated node that holds the queue.

```rust
pub struct SingleSequencer {
    pending: Vec<Vec<u8>>,
}
```

- `submit(tx)` appends a transaction to the queue.
- `drain()` takes all pending transactions in FIFO order, leaving the queue empty.

## `Ed25519BlockSigner`

Signs block headers with an ed25519 key loaded from a `suiprivkey` file. Full
nodes verify that a signed block was produced by the authorized sequencer.

```rust
let signer = Ed25519BlockSigner::from_suiprivkey_file(path)?;
let signature = signer.sign_header(&header)?;
```

## Trade-offs of single sequencing

| Property              | Single sequencer                 |
| --------------------- | -------------------------------- |
| Block time            | Sub-second possible              |
| Censorship resistance | Requires a forced-inclusion path |
| Liveness              | Sequencer must be online         |
| MEV control           | Sequencer controlled             |

Podseq starts with single sequencing. Trust-minimization (forced inclusion,
exit queue, stall detection) is handled outside this crate — see
[Roadmap](../roadmap.md).

## Status

Reth owns block contents: `produce_block` calls `engine.build()`, and Reth
pulls from its own mempool and orders by its own policy (gas price). The
`SingleSequencer` queue does not currently feed into the block — drained hashes
are logged only. It exists so podseq can later take control of block contents
(explicit transaction list via `PayloadAttributes` or `newPayload`) and apply an
opinionated ordering policy. Until then, ordering is delegated to Reth.
