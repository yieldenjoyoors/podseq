# Sequencer (`podseq-sequencer`)

The sequencer crate orders pending transactions into batches and signs produced block
headers. It implements [`Sequencer`](./core.md#sequencer) and [`BlockSigner`](./core.md#blocksigner).

## `SingleSequencer`

The current implementation is a single-operator sequencer: one designated node that
orders all transactions.

```rust
pub struct SingleSequencer {
    pending: Vec<Vec<u8>>,
}
```

- `submit(tx)` adds a transaction to the pending pool.
- `next_batch()` drains the pending transactions into an ordered `Batch`.

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

Podseq starts with single sequencing. The trait abstraction supports based
sequencing or a consensus-backed sequencer as a future upgrade.

## Status

`SingleSequencer` holds transactions in memory as 32-byte tx hashes; the runner
feeds them from Reth's mempool before each production tick and drains the ordered
batch during block production. Real ordering policy and batch formation are pending.
