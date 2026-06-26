# Walrus & Sui

Podseq pairs Walrus (data availability) with Sui (settlement). Walrus exposes blob
availability as Sui objects, so the settlement layer verifies DA on the same chain
without an external bridge.

## Walrus: data availability

Blocks are buffered and posted to a Walrus publisher **in batches by size**: one
blob holds many blocks, amortizing the per-blob certification cost. Blobs are
stored **permanent** (not deletable, even by the sequencer) for the configured
number of epochs (`walrus.epochs`, default = max, ≈ 2 years).

| Property          | Value                                       |
| ----------------- | ------------------------------------------- |
| Encoding          | Erasure-coded, sharded across storage nodes |
| Storage overhead  | ~5x the blob size                           |
| Read availability | Succeeds with ≥ 1/3 of nodes responsive     |
| Write tolerance   | Tolerates ≤ 1/3 unavailable nodes           |
| Blob addressing   | Content-addressed 256-bit `BlobId`          |
| Access            | CLI, SDK, HTTP (publisher/aggregator)       |

Podseq writes through a **publisher** and reads through an **aggregator**. Both are HTTP
endpoints operated by Walrus storage-node operators.

## Sui: settlement

For each block, podseq submits a Move transaction on Sui recording (settlement is
per-block even when DA is batched, so every height maps to its batch's blob id):

- the Walrus `BlobId` of the block data, and
- the sequencer's commitment (block hash and height).

Because Walrus blobs and storage space are Sui objects, the settlement contract can:

- check that a referenced blob is available and for how long,
- record and verify sequencer commitments,
- extend blob lifetimes,
- mediate storage payments and epoch transitions.

## End-to-end trust model

```text
Block produced
     │
     ▼
Walrus blob  ──► availability attested by the storage committee
     │
     ▼
Sui settlement ──► blob ID + sequencer commitment recorded on L1
     │
     ▼
Full nodes verify: every batch has an available, referenced blob on Sui
```

A full node reconstructing the chain reads the commitments from Sui, fetches each
blob (a batch) from Walrus, decodes all blocks in it, picks the block for the
current height, verifies the sequencer signature, and re-executes it against Reth.
If the blob is missing, the height isn't in the batch, or the signature does not
verify, the block is rejected.
