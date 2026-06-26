# Engine API

Podseq acts as the consensus client driving Reth over the Ethereum Engine API: the same
JSON-RPC interface an Ethereum consensus client uses to drive an execution client. All
calls are JWT-authenticated on port `8551`.

## Authentication

Generate a shared 32-byte secret and give both sides the same file:

```sh
openssl rand -hex 32 > jwt.hex
```

- Reth: `--authrpc.jwtsecret jwt.hex`
- Podseq: `--reth.jwt jwt.hex`

## Ports

| Port | Purpose                 |
| ---- | ----------------------- |
| 8545 | JSON-RPC (public)       |
| 8551 | Engine API (authenticated) |

## Block production cycle

```text
Podseq                                     Reth
   │                                          │
   │  1. engine_forkchoiceUpdatedV3           │
   │     (headBlockHash, payloadAttributes)   │
   │─────────────────────────────────────────►│
   │                                          │
   │  2. {payloadId}                          │
   │◄─────────────────────────────────────────│
   │                                          │
   │  3. engine_getPayloadV3(payloadId)       │
   │─────────────────────────────────────────►│
   │                                          │
   │  4. {executionPayload, blockValue}       │
   │◄─────────────────────────────────────────│
   │                                          │
   │  [Podseq broadcasts to P2P, submits DA]  │
   │                                          │
   │  5. engine_newPayloadV3(executionPayload)│
   │─────────────────────────────────────────►│
   │                                          │
   │  6. {status: VALID}                      │
   │◄─────────────────────────────────────────│
   │                                          │
   │  7. engine_forkchoiceUpdatedV3           │
   │     (newHeadBlockHash)                   │
   │─────────────────────────────────────────►│
```

## Methods

### `engine_forkchoiceUpdatedV3`

Updates fork choice and optionally starts building a new block.

```json
{
  "method": "engine_forkchoiceUpdatedV3",
  "params": [
    {
      "headBlockHash": "0x...",
      "safeBlockHash": "0x...",
      "finalizedBlockHash": "0x..."
    },
    {
      "timestamp": "0x...",
      "prevRandao": "0x...",
      "suggestedFeeRecipient": "0x...",
      "withdrawals": [],
      "parentBeaconBlockRoot": "0x..."
    }
  ]
}
```

Response:

```json
{
  "payloadStatus": { "status": "VALID", "latestValidHash": "0x..." },
  "payloadId": "0x..."
}
```

### `engine_getPayloadV3`

Retrieves a built payload by its `payloadId`.

```json
{
  "method": "engine_getPayloadV3",
  "params": ["0x...payloadId"]
}
```

### `engine_newPayloadV3`

Validates and executes a payload.

```json
{
  "method": "engine_newPayloadV3",
  "params": [
    { "executionPayload": "..." },
    ["0x...versionedHashes"],
    "0x...parentBeaconBlockRoot"
  ]
}
```

## Status codes

| Status     | Meaning                              |
| ---------- | ------------------------------------ |
| `VALID`    | Payload is valid                     |
| `INVALID`  | Payload is invalid                   |
| `SYNCING`  | Node is syncing                      |
| `ACCEPTED` | Payload accepted, not yet validated  |
