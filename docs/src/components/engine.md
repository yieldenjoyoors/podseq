# Engine (`podseq-engine`)

The engine crate is the Reth integration layer. Podseq runs Reth as a
**separate process** and drives it over two HTTP JSON-RPC interfaces:
the authenticated Engine API (port 8551) for block production, and the
public RPC (port 8545) for mempool queries.

## Modules

| Module    | Role                                                                  |
| --------- | --------------------------------------------------------------------- |
| `auth`    | JWT token generation (HMAC-SHA256, `iat` claim).                      |
| `client`  | Authenticated JSON-RPC 2.0 client for the Engine API.                 |
| `mempool` | Lightweight JSON-RPC client for `txpool_content` and `txpool_status`. |

## `Engine`

The high-level block production client:

```rust
let engine = Engine::new("http://localhost:8551", auth)?;
let (payload_id, payload) = engine.build(fc_state, attributes).await?;
engine.accept(payload, hashes, parent_beacon, new_head, safe, finalized).await?;
engine.finalize(head, safe, finalized).await?;
```

Methods: `build` (forkchoiceUpdated + getPayload), `accept` (newPayload +
advance head), `finalize` (advance safe/finalized), `current_head` (discover
chain state from Reth), `block_number`.

The helper `payload_into_block` converts an `ExecutionPayloadV3` into a
`podseq_core::Block` (header + serialized payload) for DA persistence.

## `MempoolClient`

Queries Reth's public JSON-RPC (no JWT) for pending transaction data:

```rust
let mempool = MempoolClient::new("http://localhost:8545")?;
let hashes: Vec<[u8; 32]> = mempool.pending_transactions().await?;
let count: usize = mempool.pending_count().await?;
```

The runner polls the mempool before each block production tick, feeds pending
tx hashes to the sequencer, and drains the ordered batch for logging. If the
mempool RPC is unreachable, block production continues (Reth's internal
mempool still drives execution).

## Authentication

Every Engine API call carries a freshly signed JWT bearer token. The secret
is a 32-byte hex key shared with Reth:

```rust
let auth = Auth::from_file("jwt.hex")?;
let engine = Engine::new("http://localhost:8551", auth)?;
```
