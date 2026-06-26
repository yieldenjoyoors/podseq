# Sui layer (`podseq-sui`)

The sui crate owns Walrus data availability and Sui settlement. It implements both
[`DataAvailability`](./core.md#dataavailability) and [`Settlement`](./core.md#settlement).

## Dependencies

Two lightweight libraries, no monorepo:

- [`walrus-core`](https://github.com/MystenLabs/walrus/tree/main/crates/walrus-core): blob
  types and Red Stuff erasure encoding (without the `sui-types` feature).
- [`sui-rust-sdk`](https://github.com/MystenLabs/sui-rust-sdk): `sui-transaction-builder`
  for constructing transactions, `sui-crypto` for ed25519 signing, `sui-rpc` for submission.

## DA and settlement share a wallet

Both DA publishing and settlement signing go through the same Sui wallet. Sui's
object-version model requires that a single key be used by one client at a time, so
the two concerns live in the same crate.

## `Client`

```rust
pub struct Client {
    http: reqwest::Client,
    config: Config,
}

pub struct Config {
    pub publisher_url: String,
    pub aggregator_url: String,
    pub epochs: u64,      // storage lifetime; default MAX_EPOCHS (53 ≈ 2 years)
    pub sui_rpc_url: String,
}
```

## DA (Walrus)

- `publish(blocks)`: encodes the batch (BCS `Vec<WireBlock>`), stores it via the
  Walrus publisher in a single `PUT /v1/blobs`, returns the `BlobId`. The
  sequencer batches blocks by accumulated size (`walrus.batch_size_bytes`) so
  one certification covers many blocks.
- `fetch(blob_id)`: reads the blob from the Walrus aggregator and decodes the
  whole batch (`Vec<Block>`); callers pick the block by height.

Blobs are stored **permanent** (`permanent=true`) for the configured number of
epochs; not deletable, even by the uploader. Blob IDs are 256-bit values,
base64url-encoded. The wire format is BCS (`wire` module) so full nodes
reconstruct both header and payload for every block in the batch.

## Settlement

Anchors `(blob_id, block_height)` in a Move contract on Sui so full nodes can
verify every block has available data. Called once per block; even when DA is
batched, each height in the batch is committed individually (the batch's blob id
is shared across its heights). Implemented in-process via:

- `sui-transaction-builder`: constructs the `move_call(package::settlement::settle)`
  programmable transaction.
- `sui-crypto`: signs with an ed25519 key loaded from a `suiprivkey` (Bech32) file.
- `sui-rpc`: submits via gRPC and waits for checkpoint confirmation.

Mandatory in sequencer mode: every produced block is committed, so full nodes
can verify data availability for every height. Full nodes never sign; they
verify against `signer.sequencer_pubkey`.
