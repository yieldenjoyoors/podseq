# Settlement Contract

The settlement contract lives in `move/sources/settlement.move`. It anchors L2 block
commitments on Sui so any full node can verify that every block has data available on
Walrus.

## What it does

The contract is an append-only registry mapping block height → Walrus blob ID:

```move
public fun settle(
    _cap: &SettlerCap,
    registry: &mut Registry,
    blob_id: vector<u8>,
    height: u64,
)
```

- **Access control**: only the `SettlerCap` holder (the sequencer operator) can call
  `settle`. The cap is transferable for operator rotation.
- **Sequential enforcement**: a new `height` must be exactly `latest_height + 1`
  (no gaps). Heights below that are rejected.
- **Idempotent re-submit**: calling `settle` again at the _current_ height with
  the **same** blob id is a silent no-op success. This makes settlement safe to
  retry after a lost acknowledgment or a crash recovery; the sequencer never
  stalls on a block it already settled. Re-submitting a _different_ blob id at
  the current height aborts (`E_CONFLICTING_BLOB`), since that would be an
  equivocation.
- **Events**: emits `BlockSettled { height, blob_id }` only on a genuine new
  settlement (not on the idempotent re-submit path), so indexers/full nodes see
  each height exactly once.

## Objects

| Object       | Type   | Purpose                                                                    |
| ------------ | ------ | -------------------------------------------------------------------------- |
| `SettlerCap` | Owned  | Authorizes the sequencer to settle. Transferred to the operator at `init`. |
| `Registry`   | Shared | Append-only `Table<u64, vector<u8>>` mapping height → blob ID.             |

## Read API

```move
public fun commitment(registry: &Registry, height: u64): &vector<u8>
public fun latest_height(registry: &Registry): u64
```

Full nodes read the registry to find the blob ID for a given height, fetch the blob from
Walrus, decode the block, and re-execute it against Reth.

## Access control

Only the sequencer can call `settle`. This is enforced by the `SettlerCap`, an owned
Sui object required as a function argument. In Sui's ownership model, only the owner
of an object can include it in a transaction. The cap is sent to the operator at
`init`; it is transferable for operator rotation.

The node passes the `SettlerCap` and `Registry` objects in the `move_call`:

```rust
let cap = builder.object(ObjectInput::new(self.cap));       // access control
let registry = builder.object(ObjectInput::new(self.registry)); // shared state
let blob_id_arg = builder.pure(&blob.0.to_vec());             // block's blob
let height_arg = builder.pure(&header.height);                // block height
builder.move_call(function, vec![cap, registry, blob_id_arg, height_arg]);
```

The transaction is signed by the settlement key, which must correspond to the
`SettlerCap` owner's address. If the key does not match, the Sui runtime rejects
the transaction (the owned object is not accessible to the sender).

## Configuration

Settlement requires four values in configuration:

```toml
[sui]
rpc_url = "https://fullnode.testnet.sui.io:443"
settlement_package_id = "0x..."   # from `sui client publish`
settler_cap_id = "0x..."          # from `init` (owned by the sequencer)
registry_id = "0x..."             # from `init` (shared)

[signer]
settlement_key_path = "sui.key"   # suiprivkey matching the cap owner
```

The node refuses to enable settlement unless all four are present.

## Verification flow

```text
Full node reads Sui Registry
  → gets (height, blob_id) pairs
  → fetches each blob from Walrus aggregator
  → decodes the block
  → re-executes via Reth
  → verifies state root matches
```
