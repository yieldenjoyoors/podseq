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

## State roots

`settle` records that a block's data is available; it does not say what the block
*did*. For that, the sequencer anchors the settled block's state root in the same
transaction:

```move
public fun commit_state_root(
    registry: &mut Registry,
    clock: &sui::clock::Clock,
    height: u64,
    state_root: vector<u8>,
    ctx: &TxContext,
)
public fun state_root(registry: &Registry, height: u64): Option<vector<u8>>
```

The sequencer composes the call immediately after `settle_as` in one PTB, so a
root and its settle are atomic, and `commit_state_root` accepts only the settled
tip (`height == latest_height`). Authorization is the same sender check settle
uses, so only the settlement authority can anchor a root. Re-committing the
identical root is a no-op success; a different value aborts
(`E_STATE_ROOT_CONFLICT`), because the header carrying the root is signed and
committed in the blob, which makes the mismatch an attributable equivocation.

The anchored root is the proof anchor for balance proofs (see
[exit queue](./exit-queue.md)): `exit_queue::verify_account` verifies an L2
balance against the root committed at the height whose state included it,
without executing blocks. The escape path itself unlocks on the inbox-derived
halt (`exit_queue::halt_expired_deadline`) and records proven balances in the
shared exit queue (`exit_queue::request_exit`, ordered by `(height, seq)`);
the deploy ceremony creates the queue alongside the inbox.

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

Settlement needs three object IDs and the sequencer's Sui key. On first start,
if all three IDs are unset, the sequencer **publishes the package and writes the
IDs back to the config** automatically — so you usually leave them unset. To reuse
an existing deployment, set them:

```toml
[sui]
rpc_url = "https://fullnode.testnet.sui.io:443"
settlement_package_id = "0x..."   # from publish (auto-written on first start)
settler_cap_id = "0x..."          # from `init` (owned by the sequencer)
registry_id = "0x..."             # from `init` (shared)

[signer]
key_path = "sequencer.key"        # suiprivkey matching the cap owner
```

The IDs must be all set or all unset; partial configuration is rejected.

> The enshrined [Bridge](./bridge.md) reuses this same package (the `bridge`
> module ships alongside `settlement`) and the same `[signer] key_path`.

## Verification flow

```text
Full node reads Sui Registry
  → gets (height, blob_id) pairs
  → fetches each blob from Walrus aggregator
  → decodes the block
  → re-executes via Reth
  → verifies state root matches
```
