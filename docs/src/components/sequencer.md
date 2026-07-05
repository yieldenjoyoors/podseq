# Block signer (`podseq-sequencer`)

This crate signs block headers so full nodes can attribute each block to the
authorized sequencer. It does **not** order transactions: `PayloadAttributes`
has no `transactions` field, so Reth fills each block from its own mempool
(gas-price greedy, gas-limit capped). Podseq is a block producer — it owns
timing, signing, DA publication, settlement, and P2P broadcast.

## `Ed25519BlockSigner`

Signs block headers with an ed25519 key loaded from a `suiprivkey` file. Full
nodes verify signatures against the configured `signer.sequencer_pubkey`.

```rust
let signer = Ed25519BlockSigner::from_suiprivkey_file(path)?;
let signature = signer.sign_header(&header)?;
```

## Trust model

The single sequencer is trusted for liveness (block production halts if it
goes offline). It is **not** trusted for state validity: every block is
executed and verified by Reth, and every block's data is anchored on Walrus +
Sui so full nodes can independently reconstruct the chain. Forced inclusion
and exit queues ([Roadmap](../roadmap.md) Phase 2) are the planned mechanisms
for reducing the sequencer's censorship and liveness power.
