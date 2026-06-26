/// Podseq settlement contract.
///
/// Anchors L2 block commitments on Sui so that any full node can verify every block
/// has data available on Walrus. The sequencer calls `settle` after posting each block
/// to Walrus; the contract records `(height, blob_id)` in an append-only registry.
module podseq::settlement {
    use sui::object::{Self, UID};
    use sui::transfer;
    use sui::tx_context::{Self, TxContext};
    use sui::table::{Self, Table};
    use sui::event;

    /// Height must be exactly latest + 1 (no gaps, no duplicates).
    const E_INVALID_HEIGHT: u64 = 0;
    /// A re-submit at the current height used a conflicting blob id.
    const E_CONFLICTING_BLOB: u64 = 1;

    /// Authorizes the sequencer to settle blocks. Transferred to the operator at
    /// initialization; transferable for sequencer rotation.
    public struct SettlerCap has key, store {
        id: UID,
    }

    /// Shared, append-only registry mapping block height → Walrus blob ID.
    public struct Registry has key {
        id: UID,
        latest_height: u64,
        commitments: Table<u64, vector<u8>>,
    }

    /// Emitted when a block is settled. Full nodes and indexers listen for this.
    public struct BlockSettled has copy, drop {
        height: u64,
        blob_id: vector<u8>,
    }

    /// Creates and shares the registry. Transfers SettlerCap to the caller.
    ///
    /// Call once at package publish or via a dedicated setup transaction.
    #[allow(lint(self_transfer))]
    public fun initialize(ctx: &mut TxContext) {
        let registry = Registry {
            id: object::new(ctx),
            latest_height: 0,
            commitments: table::new(ctx),
        };
        transfer::share_object(registry);
        transfer::transfer(
            SettlerCap { id: object::new(ctx) },
            tx_context::sender(ctx),
        );
    }

    /// Records that block `height` has its data available at `blob_id` on Walrus.
    ///
    /// Only the SettlerCap holder can call this. Heights must be sequential.
    ///
    /// Idempotent: re-submitting the *current* height with the same blob id is a
    /// no-op success.
    public fun settle(
        _cap: &SettlerCap,
        registry: &mut Registry,
        blob_id: vector<u8>,
        height: u64,
    ) {
        if (height == registry.latest_height) {
            // Idempotent re-submit: the block already settled. Require the blob
            // to match; a conflicting blob is an equivocation.
            assert!(*table::borrow(&registry.commitments, height) == blob_id, E_CONFLICTING_BLOB);
        } else {
            assert!(height == registry.latest_height + 1, E_INVALID_HEIGHT);
            table::add(&mut registry.commitments, height, blob_id);
            registry.latest_height = height;
            event::emit(BlockSettled { height, blob_id });
        };
    }

    /// Returns the blob ID for a given height, if settled.
    public fun commitment(registry: &Registry, height: u64): &vector<u8> {
        table::borrow(&registry.commitments, height)
    }

    /// Returns the latest settled height.
    public fun latest_height(registry: &Registry): u64 {
        registry.latest_height
    }
}
