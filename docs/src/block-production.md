# Block Production

This page traces a transaction from the Reth mempool all the way to settlement on Sui.

## Flow

```text
User Tx → Reth mempool
              │
              ▼
      Podseq polls mempool (txpool_content)
              │
              ▼
      SingleSequencer orders pending txs
              │
              ▼
      engine_forkchoiceUpdated  ──► Reth builds payload
              │
              ▼
      engine_getPayload         ──► Reth returns block
              │
              ├──► P2P broadcast (block data on channel 0)
             │     + P2P announce (height + digest on channel 1)
             │     → full nodes pull block from broadcast cache
              │
              ▼
      Walrus batch publish ──► Walrus blob (hard confirmation)
              │
              ▼
      engine_newPayload + forkchoiceUpdated ──► Reth finalizes
              │
              ▼
      Sui settlement (per-block: blob availability attestation)
```

## Steps in detail

1. **Collect & order.** The sequencer pulls transactions from the Reth mempool and
   orders them into a [`Batch`](./components/sequencer.md).

2. **Build.** Podseq calls `engine_forkchoiceUpdatedV3` with payload attributes. Reth
   starts building a block from its pending transactions.

3. **Retrieve.** Podseq calls `engine_getPayloadV4(payloadId)` to get the assembled
   execution payload from Reth.

4. **Broadcast (soft confirmation).** The new block is gossiped over the P2P network
   (Commonware) so full nodes can execute it immediately, before it is posted to DA.

5. **Submit to DA (hard confirmation).** Blocks are buffered and posted to Walrus in
   **batches by size** (`walrus.batch_size_bytes`) under a single permanent blob. Once
   Walrus attests availability, the data is retrievable by anyone.

6. **Finalize.** Podseq calls `engine_newPayloadV4` and
   `engine_forkchoiceUpdatedV3` to tell Reth to accept and finalize the block.

7. **Settle.** The Walrus blob ID and the sequencer's commitment are posted to a Sui
   Move contract **per block** (each height in a batch references the batch's blob id).
   Full nodes use this to verify that every block has published data.

## Concurrent production and finalization

Production and DA finalization run in parallel:

- **Production task** (per tick): builds a block, accepts it (`newPayload` + advance
  head), sends it to a channel. Never waits for DA.
- **Finalizer task** (background): drains the channel, buffers blocks until they reach
  `walrus.batch_size_bytes`, posts one Walrus blob per batch, settles each block on Sui,
  and advances safe/finalized. Retries DA failures with exponential backoff.

If DA is slow, production continues at its own pace. The finalizer batches and catches
up when DA recovers. Finalization is sequential (no skipping gaps), but production is
never blocked by DA latency.

On shutdown, any buffered or in-flight block is finalized locally in Reth and left
pending on disk, so it is recovered and re-uploaded on the next startup; no data is
dropped.

See [Engine API](./engine-api.md) for the JSON-RPC method details and
[Walrus & Sui](./walrus-sui.md) for the DA/settlement integration.
