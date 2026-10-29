/// Podseq settlement contract.
///
/// Anchors L2 block commitments on Sui so that any full node can verify every block
/// has data available on Walrus. The sequencer calls `settle` after posting each block
/// to Walrus; the contract records `(height, blob_id)` in an append-only registry.
module podseq::settlement {
    use podseq::inbox::{Self, Inbox};
    use podseq::rotation;
    use sui::object::{Self, UID};
    use sui::transfer;
    use sui::tx_context::{Self, TxContext};
    use sui::table::{Self, Table};
    use sui::event;

    /// Height must be exactly latest + 1 (no gaps, no duplicates).
    const E_INVALID_HEIGHT: u64 = 0;
    /// A re-submit at the current height used a conflicting blob id.
    const E_CONFLICTING_BLOB: u64 = 1;
    /// The capability gate moved into rotation state; `settle` is retired.
    const E_ROTATED: u64 = 2;

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

    /// Retired: the capability gate moved into rotation state on the Registry.
    ///
    /// The signature is frozen by Sui's upgrade-compatibility rules, so the
    /// entrypoint stays and aborts instead. The sequencer calls [`settle_as`];
    /// entombment (`entomb_settler_cap`) makes this input impossible to
    /// provide, and this abort is defense in depth against stale signers.
    public fun settle(
        _cap: &SettlerCap,
        _registry: &mut Registry,
        _inbox: &mut Inbox,
        _blob_id: vector<u8>,
        _height: u64,
        _block_ts: u64,
        _inbox_cursor: u64,
    ) {
        abort E_ROTATED
    }

    /// Records a settled block as the rotation-state authority (or the
    /// previous authority inside its grace window).
    ///
    /// Heights must be sequential; re-submitting the current height with the
    /// same blob id is a no-op success (crash-safe retry).
    ///
    /// Force inclusion: `block_ts` is the settled block's timestamp and
    /// `inbox_cursor` the count of inbox nonces included in settled blocks.
    /// Both are handed to [`inbox::enforce_settle`], which advances the inbox
    /// cursor and aborts (`E_DEADLINE_EXCEEDED`) once an included-by deadline
    /// has passed, halting the chain until the transaction is included.
    public fun settle_as(
        registry: &mut Registry,
        inbox: &mut Inbox,
        clock: &sui::clock::Clock,
        blob_id: vector<u8>,
        height: u64,
        block_ts: u64,
        inbox_cursor: u64,
        ctx: &TxContext,
    ) {
        rotation::assert_can_act(&registry.id, clock, ctx);
        do_settle(registry, inbox, blob_id, height, block_ts, inbox_cursor);
    }

    /// The settle body shared by the rotation-aware entrypoint.
    fun do_settle(
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
        }
    }

    /// Entombs the SettlerCap inside the Registry and initializes rotation
    /// state with the sender as authority. The migration step after a package
    /// upgrade; ideally in the same programmable transaction block as the
    /// upgrade, signed by the current cap owner.
    public fun entomb_settler_cap(cap: SettlerCap, registry: &mut Registry, ctx: &TxContext) {
        rotation::entomb(&mut registry.id, cap, ctx)
    }

    /// Pre-authorizes `to` to claim the settler authority after `delay_ms`.
    public fun request_rotation(
        registry: &mut Registry,
        clock: &sui::clock::Clock,
        to: address,
        delay_ms: u64,
        ctx: &TxContext,
    ) {
        rotation::request_rotation(&mut registry.id, to, delay_ms, clock, ctx)
    }

    /// Cancels the pending settler rotation. Authority only.
    public fun cancel_rotation(registry: &mut Registry, ctx: &TxContext) {
        rotation::cancel_rotation(&mut registry.id, ctx)
    }

    /// Claims the settler authority. Pending target only, inside the claim
    /// window; starts the previous owner's grace window.
    public fun claim_rotation(
        registry: &mut Registry,
        clock: &sui::clock::Clock,
        ctx: &TxContext,
    ) {
        rotation::claim_rotation(&mut registry.id, clock, ctx)
    }

    /// Retires the previous settler authority early. Authority only.
    public fun retire_previous(registry: &mut Registry, ctx: &TxContext) {
        rotation::retire_previous(&mut registry.id, ctx)
    }

    /// Borrows the settler rotation state (for indexers and tests; the node
    /// reads the dynamic field's BCS directly).
    public fun rotation_state(registry: &Registry): &rotation::RotationState {
        rotation::state(&registry.id)
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
    #[test_only]
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
    /// Public test-only: the bridge module's cross-object gate tests build
    /// and tear down registries too.
    #[test_only]
    public fun destroy_registry(registry: Registry) {
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
        let mut registry = rotated_registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"tx-a");
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 1_000, 1, ctx);

        assert!(latest_height(&registry) == 1);
        assert!(inbox::included_count(&inbox) == 1);
        assert!(inbox::last_settled_ts(&inbox) == 1_000);

        settle_as(&mut registry, &mut inbox, &clock, b"blob-2", 2, 2_000, 1, ctx);
        assert!(latest_height(&registry) == 2);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun settle_idempotent_retry_skips_the_inbox_gate() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"tx-a");
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 1_000, 1, ctx);

        // Crash-retry of the same height with a stale cursor and timestamp:
        // succeeds and leaves the inbox untouched.
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 0, 0, ctx);

        assert!(latest_height(&registry) == 1);
        assert!(inbox::included_count(&inbox) == 1);
        assert!(inbox::last_settled_ts(&inbox) == 1_000);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_DEADLINE_EXCEEDED_FOR_TEST, location = inbox)]
    fun settle_halts_after_deadline_miss() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_000_000);

        inbox::submit(&mut inbox, &clock, b"tx-a");
        // Deadline is 1_600_000; the block being settled is newer and does not
        // contain the transaction.
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 2_000_000, 0, ctx);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    // ---------- rotation tests ----------

    /// Mirrors `rotation::E_*`; constants are module-private there.
    #[test_only]
    const E_NOT_AUTHORIZED_FOR_TEST: u64 = 0;
    #[test_only]
    const E_NO_PENDING_FOR_TEST: u64 = 1;
    #[test_only]
    const E_NOT_CLAIM_TARGET_FOR_TEST: u64 = 2;
    #[test_only]
    const E_CLAIM_TOO_EARLY_FOR_TEST: u64 = 3;
    #[test_only]
    const E_CLAIM_WINDOW_EXPIRED_FOR_TEST: u64 = 4;
    #[test_only]
    const E_DELAY_OUT_OF_BOUNDS_FOR_TEST: u64 = 5;
    #[test_only]
    const E_NO_PREVIOUS_FOR_TEST: u64 = 6;
    #[test_only]
    const E_ALREADY_ENTOMBED_FOR_TEST: u64 = 7;

    #[test_only]
    use sui::dynamic_field as df;
    #[test_only]
    use std::option;

    #[test_only]
    const ALICE: address = @0xA;
    #[test_only]
    const BOB: address = @0xB;
    #[test_only]
    const CAROL: address = @0xC;

    /// Sender-switching test context; `dummy` pins the sender to @0x0.
    ///
    /// `tx_context::sender` reads a runtime native: in unit tests the
    /// effective sender is the one from the most recently *constructed*
    /// context, so bind a fresh context before every sender-sensitive call.
    #[test_only]
    fun ctx_for(sender: address, hint: u64): TxContext {
        tx_context::new_from_hint(sender, hint, 0, 0, 0)
    }

    /// A registry whose cap is entombed, so `settle_as` is callable.
    /// Public test-only: used by the bridge module's cross-object tests.
    #[test_only]
    public fun rotated_registry_for_testing(ctx: &mut TxContext): Registry {
        let mut registry = registry_for_testing(ctx);
        entomb_settler_cap(test_cap(ctx), &mut registry, ctx);
        registry
    }

    /// Removes the entombed cap and the rotation state so tests can destroy
    /// the registry. Public test-only (see [`rotated_registry_for_testing`]).
    #[test_only]
    public fun destroy_rotation_for_testing(registry: &mut Registry) {
        let _ = df::remove<rotation::StateKey, rotation::RotationState>(
            &mut registry.id,
            rotation::state_key(),
        );
        let cap = df::remove<rotation::TombKey, SettlerCap>(&mut registry.id, rotation::tomb_key());
        destroy_cap(cap);
    }

    #[test]
    fun rotation_lifecycle_preauthorize_wait_claim_and_retire() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        // Pre-rotation: the current authority settles.
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 1_000, 0, &ctx_a);
        assert!(latest_height(&registry) == 1);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        let target = rotation::pending_target(rotation_state(&registry));
        assert!(option::is_some(&target) && *option::borrow(&target) == BOB);

        // The claim is the atomic cut-over: authority flips and the grace
        // window for the old key starts in the same transaction.
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);
        assert!(rotation::authority(rotation_state(&registry)) == BOB);

        settle_as(&mut registry, &mut inbox, &clock, b"blob-2", 2, 2_000, 0, &ctx_b);

        // The previous authority settles up to and including the grace end.
        let ctx_a2 = ctx_for(ALICE, 3);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms() + rotation::prev_grace_ms());
        settle_as(&mut registry, &mut inbox, &clock, b"blob-3", 3, 3_000, 0, &ctx_a2);

        // Retire is the one-way "done with the old key" signal.
        let ctx_b2 = ctx_for(BOB, 4);
        retire_previous(&mut registry, &ctx_b2);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun old_owner_is_rejected_after_retire_and_grace() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);
        retire_previous(&mut registry, &ctx_b);

        let ctx_a2 = ctx_for(ALICE, 3);
        clock::set_for_testing(
            &mut clock,
            rotation::min_delay_ms() + rotation::prev_grace_ms() + 1,
        );
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 1_000, 0, &ctx_a2);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun settle_as_rejects_a_sender_that_never_held_the_cap() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        let ctx_c = ctx_for(CAROL, 2);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 1_000, 0, &ctx_c);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_CLAIM_TOO_EARLY_FOR_TEST, location = rotation)]
    fun claim_before_the_timelock_elapses_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms() - 1);
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_CLAIM_TARGET_FOR_TEST, location = rotation)]
    fun claim_by_the_wrong_owner_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        let ctx_c = ctx_for(CAROL, 2);
        claim_rotation(&mut registry, &clock, &ctx_c);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_PENDING_FOR_TEST, location = rotation)]
    fun double_claim_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);
        let ctx_b2 = ctx_for(BOB, 3);
        claim_rotation(&mut registry, &clock, &ctx_b2);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_CLAIM_WINDOW_EXPIRED_FOR_TEST, location = rotation)]
    fun claim_after_the_window_expires_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        clock::set_for_testing(
            &mut clock,
            rotation::min_delay_ms() + rotation::claim_window_ms() + 1,
        );
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_DELAY_OUT_OF_BOUNDS_FOR_TEST, location = rotation)]
    fun request_below_the_minimum_delay_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms() - 1, ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_DELAY_OUT_OF_BOUNDS_FOR_TEST, location = rotation)]
    fun request_above_the_maximum_delay_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::max_delay_ms() + 1, ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun request_by_a_non_authority_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        let ctx_b = ctx_for(BOB, 2);
        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_b);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun cancel_clears_the_pending_request() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), ctx_a);
        cancel_rotation(&mut registry, ctx_a);
        assert!(option::is_none(&rotation::pending_target(rotation_state(&registry))));

        // A re-request overwrites cleanly after a cancel.
        request_rotation(&mut registry, &clock, CAROL, rotation::min_delay_ms(), ctx_a);
        let target = rotation::pending_target(rotation_state(&registry));
        assert!(option::is_some(&target) && *option::borrow(&target) == CAROL);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_PENDING_FOR_TEST, location = rotation)]
    fun claim_after_cancel_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        cancel_rotation(&mut registry, &ctx_a);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun cancel_by_a_non_authority_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        let ctx_b = ctx_for(BOB, 2);
        cancel_rotation(&mut registry, &ctx_b);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_ALREADY_ENTOMBED_FOR_TEST, location = rotation)]
    fun entomb_twice_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);

        entomb_settler_cap(test_cap(ctx_a), &mut registry, ctx_a);

        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_PREVIOUS_FOR_TEST, location = rotation)]
    fun retire_without_a_previous_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);

        retire_previous(&mut registry, ctx_a);

        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_ROTATED)]
    fun settle_aborts_once_the_package_rotates() {
        let ctx = &mut tx_context::dummy();
        let mut registry = registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let cap = test_cap(ctx);

        settle(&cap, &mut registry, &mut inbox, b"blob", 1, 0, 0);

        destroy_registry(registry);
        destroy_inbox_helper(inbox);
        destroy_cap(cap);
    }

    /// Drains pending entries so the inbox can be destroyed in these tests.
    /// Public test-only (see [`rotated_registry_for_testing`]).
    #[test_only]
    public fun destroy_inbox_helper(mut inbox: inbox::Inbox) {
        inbox::drain_for_testing(&mut inbox);
        inbox::destroy_inbox_for_testing(inbox);
    }

    // ---------- adversarial: settlement across the rotation window ----------

    #[test]
    fun settles_interleave_with_the_claim_and_retry_across_the_swap() {
        let mut ctx_a = ctx_for(ALICE, 31);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        // Pre-authorization phase: the sitting authority settles.
        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 1_000, 0, &ctx_a);
        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);

        // Timelock wait: the old key keeps settling; heights stay sequential.
        clock::set_for_testing(&mut clock, rotation::min_delay_ms() / 2);
        let ctx_a2 = ctx_for(ALICE, 32);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-2", 2, 2_000, 0, &ctx_a2);

        // Claim at the eligibility point, then both keys settle inside the
        // grace window: either interleaving is a valid sequential history.
        let ctx_b = ctx_for(BOB, 33);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-3", 3, 3_000, 0, &ctx_b);

        let ctx_a3 = ctx_for(ALICE, 34);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms() + rotation::prev_grace_ms());
        settle_as(&mut registry, &mut inbox, &clock, b"blob-4", 4, 4_000, 0, &ctx_a3);

        // A crash-retry of the old key's block under the new key: same
        // height, same blob — the idempotent branch, not a second apply.
        let ctx_b2 = ctx_for(BOB, 35);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-4", 4, 0, 0, &ctx_b2);

        assert!(latest_height(&registry) == 4);
        assert!(commitment(&registry, 4) == b"blob-4");
        assert!(inbox::included_count(&inbox) == 0);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_CONFLICTING_BLOB)]
    fun the_previous_owner_cannot_rewrite_a_height_after_the_swap() {
        let mut ctx_a = ctx_for(ALICE, 36);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        settle_as(&mut registry, &mut inbox, &clock, b"blob-1", 1, 1_000, 0, &ctx_a);
        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        let ctx_b = ctx_for(BOB, 37);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-2", 2, 2_000, 0, &ctx_b);

        // Inside grace the old key still settles new heights, but re-signing
        // an already-settled height with a different blob is equivocation,
        // grace window or not.
        let ctx_a2 = ctx_for(ALICE, 38);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-2-fork", 2, 2_000, 0, &ctx_a2);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }
}
