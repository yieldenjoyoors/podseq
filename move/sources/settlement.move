/// Podseq settlement contract.
///
/// Anchors L2 block commitments on Sui so that any full node can verify every block
/// has data available on Walrus. The sequencer calls `settle` after posting each block
/// to Walrus; the contract records `(height, blob_id)` in an append-only registry.
module podseq::settlement {
    use podseq::inbox::{Self, Inbox};
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
    ///
    /// Force inclusion: `block_ts` is the settled block's timestamp and
    /// `inbox_cursor` the count of inbox nonces included in settled blocks.
    /// Both are handed to [`inbox::enforce_settle`], which advances the inbox
    /// cursor and aborts (`E_DEADLINE_EXCEEDED`) once an included-by deadline
    /// has passed, halting the chain until the transaction is included.
    public fun settle(
        _cap: &SettlerCap,
        registry: &mut Registry,
        inbox: &mut Inbox,
        blob_id: vector<u8>,
        height: u64,
        block_ts: u64,
        inbox_cursor: u64,
    ) {
        if (height == registry.latest_height) {
            // Idempotent re-submit: the block already settled. Require the blob
            // to match; a conflicting blob is an equivocation. The inbox is not
            // touched, so crash retries can never trip the deadline gate.
            assert!(*table::borrow(&registry.commitments, height) == blob_id, E_CONFLICTING_BLOB);
        } else {
            assert!(height == registry.latest_height + 1, E_INVALID_HEIGHT);
            inbox::enforce_settle(inbox, block_ts, inbox_cursor);
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

    // ---------- tests ----------

    /// Mirrors `inbox::E_DEADLINE_EXCEEDED`; constants are module-private.
    const E_DEADLINE_EXCEEDED_FOR_TEST: u64 = 4;

    #[test_only]
    use sui::clock;

    /// Builds an unshared registry for tests.
    #[test_only]
    fun registry_for_testing(ctx: &mut TxContext): Registry {
        Registry {
            id: object::new(ctx),
            latest_height: 0,
            commitments: table::new(ctx),
        }
    }

    /// Destroys a registry after removing every commitment it recorded.
    #[test_only]
    fun destroy_registry(registry: Registry) {
        let Registry { id, latest_height, mut commitments } = registry;
        let mut height = latest_height;
        while (height > 0) {
            table::remove(&mut commitments, height);
            height = height - 1;
        };
        table::destroy_empty(commitments);
        id.delete();
    }

    /// SettlerCap stub; `settle` only reads the capability.
    #[test_only]
    fun test_cap(ctx: &mut TxContext): SettlerCap {
        SettlerCap { id: object::new(ctx) }
    }

    #[test_only]
    fun destroy_cap(cap: SettlerCap) {
        let SettlerCap { id } = cap;
        id.delete();
    }

    #[test]
    fun settle_passes_through_the_inbox_gate() {
        let ctx = &mut tx_context::dummy();
        let mut registry = registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);
        let cap = test_cap(ctx);

        inbox::submit(&mut inbox, &clock, b"tx-a");
        settle(&cap, &mut registry, &mut inbox, b"blob-1", 1, 1_000, 1);

        assert!(latest_height(&registry) == 1);
        assert!(inbox::included_count(&inbox) == 1);
        assert!(inbox::last_settled_ts(&inbox) == 1_000);

        settle(&cap, &mut registry, &mut inbox, b"blob-2", 2, 2_000, 1);
        assert!(latest_height(&registry) == 2);

        clock::destroy_for_testing(clock);
        destroy_registry(registry);
        destroy_inbox_helper(inbox);
        destroy_cap(cap);
    }

    #[test]
    fun settle_idempotent_retry_skips_the_inbox_gate() {
        let ctx = &mut tx_context::dummy();
        let mut registry = registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);
        let cap = test_cap(ctx);

        inbox::submit(&mut inbox, &clock, b"tx-a");
        settle(&cap, &mut registry, &mut inbox, b"blob-1", 1, 1_000, 1);

        // Crash-retry of the same height with a stale cursor and timestamp:
        // succeeds and leaves the inbox untouched.
        settle(&cap, &mut registry, &mut inbox, b"blob-1", 1, 0, 0);

        assert!(latest_height(&registry) == 1);
        assert!(inbox::included_count(&inbox) == 1);
        assert!(inbox::last_settled_ts(&inbox) == 1_000);

        clock::destroy_for_testing(clock);
        destroy_registry(registry);
        destroy_inbox_helper(inbox);
        destroy_cap(cap);
    }

    #[test, expected_failure(abort_code = E_DEADLINE_EXCEEDED_FOR_TEST, location = inbox)]
    fun settle_halts_after_deadline_miss() {
        let ctx = &mut tx_context::dummy();
        let mut registry = registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_000_000);
        let cap = test_cap(ctx);

        inbox::submit(&mut inbox, &clock, b"tx-a");
        // Deadline is 1_600_000; the block being settled is newer and does not
        // contain the transaction.
        settle(&cap, &mut registry, &mut inbox, b"blob-1", 1, 2_000_000, 0);

        clock::destroy_for_testing(clock);
        destroy_registry(registry);
        destroy_inbox_helper(inbox);
        destroy_cap(cap);
    }

    /// Drains pending entries so the inbox can be destroyed in these tests.
    #[test_only]
    fun destroy_inbox_helper(mut inbox: inbox::Inbox) {
        inbox::drain_for_testing(&mut inbox);
        inbox::destroy_inbox_for_testing(inbox);
    }
}
