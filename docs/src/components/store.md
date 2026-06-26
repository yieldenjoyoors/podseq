# Store (`podseq-store`)

Persistent storage for block data, chain state, and crash recovery.

## Components

| Type           | Backend                      | Role                                                        |
| -------------- | ---------------------------- | ----------------------------------------------------------- |
| `BlockStore`   | File per height, BCS-encoded | Append and retrieve blocks by height.                       |
| `StateStore`   | Atomic JSON (`state.json`)   | Fork-choice state (head, safe, finalized) plus sync cursor. |
| `PendingStore` | Atomic JSON (`pending.json`) | Tracks produced-but-unsettled heights for crash recovery.   |

## Crash recovery

When the sequencer produces a block, it persists the block to `BlockStore`, marks the
height pending in `PendingStore`, then hands the block to the finalizer. The finalizer
clears the pending marker only after both DA publish and Sui settlement succeed.

On startup, the runner drains `PendingStore` and re-submits any orphaned blocks to the
finalizer. A missing block on disk triggers a warning and the marker is dropped so it
does not block recovery of later heights.

This gives the sequencer **at-least-once durability**: a crash between production and
settlement does not lose blocks.

## API

```rust
// Block store
block_store.put(&block)?;
let block = block_store.get(height)?;

// State store
state_store.save(&chain_state)?;
let state = state_store.load()?;

// Pending store (crash recovery)
pending_store.mark(height)?;
pending_store.clear(height)?;
let pending = pending_store.pending()?;
```

## Backend

The current backend is plain files: `blocks/{height}` (BCS) and `state.json` /
`pending.json` (atomic JSON via temp-rename). The public API can be migrated to
`commonware-storage::archive` and `metadata` later without changing callers. The
Commonware runtime bridge (`podseq_core::runtime`) is already in place.
