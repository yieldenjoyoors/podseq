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

- `P2pConfig`: identity key path, listen address, bootstrap peers.
- `P2pNode::new(context, config)`: creates the network, registers channels,
  starts the broadcast engine and background tasks. Returns broadcaster + receiver.
- `BlockBroadcaster`: `broadcast(block)` and `announce(height, digest)`.
- `BlockReceiver`: `poll_next()` (announce channel) and `receive(digest)` (pull
  from broadcast cache).

## Configuration

```toml
[p2p]
key_path = "p2p.key"             # 64-char hex ed25519 seed
listen_addr = "0.0.0.0:9000"     # socket to bind
dialable_addr = "1.2.3.4:9000"   # optional, for NAT
bootstrap_peers = [              # optional
  "abc123...@1.2.3.4:9000",
]
```

The identity key is generated on first start or via:

```sh
podseq keyring generate-p2p --out p2p.key
```
