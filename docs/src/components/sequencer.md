# Block signing (`podseq-core`)

Block headers are signed so full nodes can attribute each block to the
authorized sequencer. Reth owns transaction selection and ordering
(`PayloadAttributes` has no `transactions` field, so Reth fills each block
from its own mempool, gas-price greedy and gas-limit capped). Podseq's role is
block production: timing, signing, DA publication, settlement, and P2P
broadcast.

## `Ed25519BlockSigner`

The canonical `BlockSigner` implementation lives in `podseq-core`, next to the
trait. It signs block headers with an ed25519 key loaded from a `suiprivkey`
file. Full nodes verify signatures against the configured
`signer.sequencer_pubkey`.

```rust
let signer = Ed25519BlockSigner::from_suiprivkey_file(path)?;
let signature = signer.sign_header(&header)?;
```

## Forced inclusion

The single sequencer can censor any ordinary tx by refusing to include it.
Forced inclusion removes that power: users post a tx to a Sui-side inbox, and
the sequencer must include it within N blocks or halt. After each
`engine.build()`, the sequencer pulls unread inbox entries, submits each to
Reth's mempool via `eth_sendRawTransaction`, and advances the inbox cursor
once the tx is mined. Full nodes verify the liveness invariant on settlement.

Forced txs enter the same mempool as user txs and ride the same gas-limit
cap, so block validity is unchanged. The sequencer's only choice is to
include the forced tx or stop producing blocks.

## Trust model

The single sequencer is trusted for liveness (block production halts if it
goes offline). State validity is verifiable by anyone: every block is
executed by Reth, and every block's data is anchored on Walrus + Sui so full
nodes independently reconstruct the chain. Forced inclusion bounds the
sequencer's censorship power; exit queues bound its liveness power.
