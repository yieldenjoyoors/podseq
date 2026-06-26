# Core (`podseq-core`)

`podseq-core` defines the shared types and the four traits that every implementation
crate builds against. It has **no external dependencies** beyond `std`. Interfaces
are stable; any implementation can be swapped without touching consumers.

## Types

| Type     | Meaning                                            |
| -------- | -------------------------------------------------- |
| `Hash`   | 32-byte hash identifying a block or header         |
| `BlobId` | 256-bit Walrus blob identifier (content-addressed) |
| `Header` | Sequencer-produced block header                    |
| `Block`  | A header plus its committed transaction data       |
| `Batch`  | An ordered list of transactions awaiting execution |
| `Error`  | Typed error enum shared across all crates          |

## Traits

### `Sequencer`

Orders transactions into a batch.

```rust
pub trait Sequencer: Send + Sync {
    fn next_batch(&self) -> impl Future<Output = Result<Batch, Error>> + Send;
}
```

### `Executor`

Drives the execution layer (Reth) to turn a batch into a block.

```rust
pub trait Executor: Send + Sync {
    fn execute(&self, batch: &Batch) -> impl Future<Output = Result<Block, Error>> + Send;
}
```

### `DataAvailability`

Posts and retrieves block data from the DA layer (Walrus). A single blob holds a
**batch** of blocks: one `publish` stores many blocks under one blob id, and a
`fetch` returns the whole batch. Callers locate a specific block by height.

```rust
pub trait DataAvailability: Send + Sync {
    fn publish(&self, blocks: &[Block]) -> impl Future<Output = Result<BlobId, Error>> + Send;
    fn fetch(&self, id: &BlobId) -> impl Future<Output = Result<Vec<Block>, Error>> + Send;
}
```

### `Settlement`

Anchors a block and its blob commitment on the settlement layer (Sui). Called
once per block (even when DA is batched) so each height maps to a blob id.

```rust
pub trait Settlement: Send + Sync {
    fn commit(
        &self,
        block: &Block,
        blob: &BlobId,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}
```

## Async traits return `impl Future + Send`

The sequencer runs methods on Tokio tasks that must be `Send`. Trait `async fn` does not
impose `Send` on the returned future, so the core desugars each method to
`fn ... -> impl Future<...> + Send`. Implementations may still use plain `async fn`.
