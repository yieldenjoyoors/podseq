# MCP Server

`podseq mcp` serves a read-only MCP (Model Context Protocol) endpoint so LLM
clients (Claude, Cursor, or any MCP client) can inspect a podseq node. The
tools report podseq's own state: the local store, the Sui settlement
registry, Walrus blobs, and the bridge vault. They do not proxy the Reth
JSON-RPC; anything execution-level belongs to Reth's own endpoints.

The server loads no keys, signs nothing, and never touches the Engine API.

## Run

```sh
podseq mcp --config podseq.toml --listen 127.0.0.1:9101
```

The endpoint binds loopback by default. Change `--listen` only if you mean to
expose it, since anyone who can reach it can read chain state and the
configured endpoints.

The server reads the same data directory as the node (blocks, `state.json`,
`pending.json`). Those files are written atomically, so inspection is safe
while a sequencer or full node is running, and works on a stopped node too,
which is useful for crash recovery ("which heights never settled?").

## Transport

The server speaks the MCP streamable HTTP transport in stateless mode:

- Clients POST JSON-RPC 2.0 messages to `/mcp` and receive a single
  `application/json` response.
- Notifications (messages without an `id`) are answered with `202 Accepted`
  and no body.
- There is no session state and no SSE stream, so `GET /mcp` returns `405`.
- Protocol versions `2025-06-18` and `2025-03-26` are supported. Batches
  (arrays of messages, `2025-03-26` only) are answered with a matching array.

## Tools

| Tool         | Arguments             | Description                                                                                                                            |
| ------------ | --------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `status`     | none                  | Mode, chain head from `state.json`, stored block count, heights pending finalization, settled height from the registry, settlement lag |
| `get_block`  | `height`              | Stored podseq block: header hashes, timestamp, payload size, signature presence, pending flag, settled Walrus blob id                  |
| `get_blob`   | `blob_id` (base64url) | Blob contents from the Walrus aggregator, decoded into the block batch it carries                                                      |
| `settlement` | optional `height`     | Latest settled height from the Sui registry; with `height`, the blob commitment for that height                                        |
| `bridge`     | none                  | Bridge vault state: next deposit and withdraw nonce                                                                                    |

`settlement` and `get_block`'s settlement lookup require `sui.registry_id` in
the config; `bridge` requires `bridge.vault_id`. Unconfigured tools report an
error telling you which key is missing. Settlement reads are bounded by a 10 s
timeout so a stalled Sui RPC cannot hang the endpoint.

## Client configuration

Claude Code:

```sh
claude mcp add --transport http podseq http://127.0.0.1:9101/mcp
```

Clients that take a URL per server (Cursor and similar):

```json
{
  "mcpServers": {
    "podseq": { "url": "http://127.0.0.1:9101/mcp" }
  }
}
```

## Limits

- Requests are capped at 1 MiB (413 above that); chunked bodies are rejected.
- Blob fetches go through the Walrus aggregator with the node client's 60 s
  timeout; large batches can take a while.
- The endpoint holds no secrets: it reads endpoint URLs, object ids, and the
  data directory from the config, nothing else.
