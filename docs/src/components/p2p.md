# P2P (`podseq-p2p`)

The p2p crate handles block propagation between the sequencer and full nodes.
It is built on [Commonware](https://commonware.xyz) primitives.

## Stack

| Primitive                    | Role                                                                                                    |
| ---------------------------- | ------------------------------------------------------------------------------------------------------- |
| `commonware-p2p`             | Authenticated, encrypted peer connections and peer discovery via the `authenticated::discovery` module. |
| `commonware-broadcast`       | Buffered broadcast engine for block data (deduplication, caching, pull-by-digest).                      |
| `commonware-cryptography`    | ed25519 peer identity keys.                                                                             |
| `commonware-runtime` (tokio) | Commonware's tokio backend, shared with the node binary for coexistence.                                |

## Two-channel protocol

Two p2p channels are registered on the discovery network:

| Channel              | ID  | Purpose                                                                                                       |
| -------------------- | --- | ------------------------------------------------------------------------------------------------------------- |
| Broadcast (buffered) | 0   | Full blocks. The broadcast engine caches blocks per peer and serves them by digest.                           |
| Announce (raw)       | 1   | 40-byte `(height, digest)` notifications. The sequencer sends these so full nodes know which digests to pull. |

### Flow

1. **Sequencer** produces a block → signs it → persists it →  
   `broadcast(block)` on channel 0 + `announce(height, digest)` on channel 1.
2. **Full node** polls channel 1 (async, 100ms timeout) → on receiving an  
   announce, calls `receive(digest)` on channel 0 → pulls the block from the  
   broadcast cache → verifies signature → applies to Reth as an **optimistic
   head only**. p2p blocks are never marked safe or finalized.
3. **DA sync** is the verification path. Every height is fetched from the Sui
   Registry → Walrus. If the block was already pulled in via p2p (and matches
   canonical DA block), it is only **finalized**: the `safe`/`finalized`
   fork-choice pointers advance without re-submitting the payload. Only a
   mismatch (sequencer equivocation) re-applies the canonical DA block and
   reorgs onto it. p2p reduces latency to head; finality always comes from DA.
   If a p2p announce is dropped, the block still arrives through DA.

## Public API

- `P2pConfig`: identity key path, listen address, bootstrap peers, network
  mailbox size, peer-set bound.
- `P2pNode::new(&context, config)`: creates the network, registers channels,
  starts the broadcast engine and background tasks (synchronous; the commonware
  runtime supplies the executor). Returns broadcaster + receiver.
- `BlockBroadcaster`: `broadcast(block)` and `announce(height, digest)`.
- `BlockReceiver`: `poll_next()` (announce channel) and `receive(digest)` (pull
  from broadcast cache).

## Admission

A node admits exactly `bootstrap_peers ∪ {self}`: the discovery tracker bounces
every other connection before any handshake work. Both sides must therefore list
each other — a full node pointing `bootstrap_peers` at the sequencer works only
if the sequencer also lists that full node. Dynamic peer admission (full nodes
appearing without sequencer-side config) is future work; the natural source is
the committee published in the Sui registry.

A block broadcast before a peer connects is not re-pushed to that peer: the
broadcast engine pushes to currently-connected peers only, and its `get` is
local-cache. Historical recovery is Walrus DA's job; p2p blocks are an
optimistic head ahead of DA finality.

## Configuration

```toml
[p2p]
key_path = "p2p.key"             # 64-char hex ed25519 seed
listen_addr = "0.0.0.0:9000"     # socket to bind
dialable_addr = "1.2.3.4:9000"   # optional, for NAT
bootstrap_peers = [              # optional; also the admission set (see above)
  "abc123...@1.2.3.4:9000",
]
mailbox_size = 1024              # optional, network actor mailboxes
max_peers_per_set = 32           # optional, distinct identities per peer-set index
```

The identity key is **generated automatically on first start** and persisted to
`key_path`.
