# Engine (`podseq-engine`)

The engine crate is the Reth integration layer. Podseq runs Reth as a
**separate process** and drives it over the authenticated Engine API
(port 8551) for block production.

## Modules

| Module   | Role                                                  |
| -------- | ----------------------------------------------------- |
| `auth`   | JWT token generation (HMAC-SHA256, `iat` claim).      |
| `client` | Authenticated JSON-RPC 2.0 client for the Engine API. |

## `Engine`

The high-level block production client:

```rust
let engine = Engine::new("http://localhost:8551", auth)?;
let built = engine.build(fc_state, attributes).await?;   // BuiltPayload
engine.accept(&built.payload, new_head, safe, finalized).await?;
engine.finalize(head, safe, finalized).await?;
```

Methods: `build` (forkchoiceUpdated + getPayload), `accept` (newPayload +
advance head), `finalize` (advance safe/finalized), `current_head` (discover
chain state from Reth), `block_number`.

The helper `payload_into_block` converts an `ExecutionPayloadV3` into a
`podseq_core::Block` (header + serialized payload) for DA persistence.

## Authentication

Every Engine API call carries a freshly signed JWT bearer token. The secret
is a 32-byte hex key shared with Reth:

```rust
let auth = Auth::from_file("jwt.hex")?;
let engine = Engine::new("http://localhost:8551", auth)?;
```
