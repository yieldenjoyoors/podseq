/// Cap-rotation state machine for the entombed SettlerCap and BridgeCap.
///
/// The caps live as dynamic fields of the shared objects they guard (see
/// `settlement::entomb_settler_cap` and `bridge::entomb_bridge_cap`), so
/// authority is no longer an object-ownership property: it is the
/// `RotationState` record stored next to them and checked against the
/// transaction sender. Rotation is a two-step, timelocked transition claimable
/// only by the pre-authorized target, and a claim starts a grace window during
/// which the previous authority's in-flight calls stay valid. Steering
/// (requesting a rotation) requires the object's `OpsCap`, minted at
/// entombment and held in operator custody — the hot sequencer key settles
/// but cannot steer. Design record: docs/src/cap-rotation.md.
module podseq::rotation {
    use sui::clock::{Self, Clock};
    use sui::dynamic_field as df;
    use sui::event;
    use sui::object::{Self, ID, UID};
    use sui::transfer;
    use std::option::{Self, Option};
    use sui::tx_context::{Self, TxContext};

    /// Sender is neither the current authority nor the previous one in grace.
    const E_NOT_AUTHORIZED: u64 = 0;
    /// No pending rotation.
    const E_NO_PENDING: u64 = 1;
    /// Only the pre-authorized target can claim.
    const E_NOT_CLAIM_TARGET: u64 = 2;
    /// The timelock has not elapsed.
    const E_CLAIM_TOO_EARLY: u64 = 3;
    /// The pre-authorization expired unclaimed.
    const E_CLAIM_WINDOW_EXPIRED: u64 = 4;
    /// Requested delay outside [`MIN_DELAY_MS`, `MAX_DELAY_MS`].
    const E_DELAY_OUT_OF_BOUNDS: u64 = 5;
    /// No previous authority to retire.
    const E_NO_PREVIOUS: u64 = 6;
    /// The parent already carries rotation state.
    const E_ALREADY_ENTOMBED: u64 = 7;
    /// The tombstoned `request_rotation` was called; steering requires the
    /// object's `OpsCap` via `request_rotation_ops`.
    const E_OPS_REQUIRED: u64 = 8;
    /// The `OpsCap` was minted for a different guarded object.
    const E_OPS_MISMATCH: u64 = 9;
    /// An `OpsCap` was already minted for this object; the mint is one-shot.
    const E_OPS_MINTED: u64 = 10;

    /// Lower bound for the requested timelock: still catches a mis-typed
    /// target before anything irreversible happens.
    const MIN_DELAY_MS: u64 = 600_000; // 10 minutes
    /// Upper bound for the requested timelock.
    const MAX_DELAY_MS: u64 = 604_800_000; // 7 days
    /// Conventional full observation window; callers pass their own delay.
    const DEFAULT_DELAY_MS: u64 = 86_400_000; // 24 hours
    /// A pre-authorization expires this long after it becomes claimable.
    const CLAIM_WINDOW_MS: u64 = 604_800_000; // 7 days
    /// How long the previous authority stays authorized after a claim. Must
    /// exceed the worst signing-to-finality lifetime of in-flight calls.
    const PREV_GRACE_MS: u64 = 600_000; // 10 minutes

    /// Returns [`MIN_DELAY_MS`], the smallest requestable timelock.
    public fun min_delay_ms(): u64 {
        MIN_DELAY_MS
    }

    /// Returns [`MAX_DELAY_MS`], the largest requestable timelock.
    public fun max_delay_ms(): u64 {
        MAX_DELAY_MS
    }

    /// Returns [`DEFAULT_DELAY_MS`], the conventional 24 h request delay.
    public fun default_delay_ms(): u64 {
        DEFAULT_DELAY_MS
    }

    /// Returns [`CLAIM_WINDOW_MS`], how long a pre-authorization stays
    /// claimable after it becomes eligible.
    public fun claim_window_ms(): u64 {
        CLAIM_WINDOW_MS
    }

    /// Returns [`PREV_GRACE_MS`], the previous owner's acceptance window.
    public fun prev_grace_ms(): u64 {
        PREV_GRACE_MS
    }

    /// Returns the `StateKey` singleton. Public so indexers and test teardown
    /// can name the dynamic-field entry; production calls go through the
    /// module entrypoints.
    public fun state_key(): StateKey {
        StateKey {}
    }

    /// Returns the `TombKey` singleton (see [`state_key`]).
    public fun tomb_key(): TombKey {
        TombKey {}
    }

    /// Returns the guarded object the cap steers.
    public fun ops_bound_object(ops: &OpsCap): ID {
        ops.for_id
    }

    /// Destroys the cap, one-way. Steering for its object is gone for good —
    /// no further rotation can be requested for it. Settlement is unaffected.
    public fun retire_ops_cap(ops: OpsCap) {
        let OpsCap { id, for_id } = ops;
        event::emit(OpsCapRetired { cap_id: for_id });
        id.delete();
    }

    /// Dynamic-field key under which the authority record is stored on the
    /// guarded object.
    public struct StateKey has copy, drop, store {}

    /// Dynamic-field key under which the entombed capability is stored.
    public struct TombKey has copy, drop, store {}

    /// Steering capability for one guarded object, minted by [`entomb`] and
    /// bound to that object forever. Holding it — not being the rotation
    /// authority — authorizes [`request_rotation_ops`], so the hot sequencer
    /// key can settle but cannot steer rotation. `store` lets the operator
    /// move it to cold custody with a plain transfer; [`retire_ops_cap`]
    /// destroys it one-way.
    public struct OpsCap has key, store {
        id: UID,
        /// The guarded object this cap steers (`E_OPS_MISMATCH` elsewhere).
        for_id: ID,
    }

    /// Authority record for one guarded object, stored under `StateKey`.
    public struct RotationState has copy, drop, store {
        /// The address whose settle/withdraw calls are authorized right now.
        authority: address,
        /// Previous authority, still authorized until its grace deadline.
        prev: Option<PreviousAuthority>,
        /// Pre-authorized next authority and the earliest claim time.
        pending: Option<PendingRotation>,
    }

    /// One pre-authorization.
    public struct PendingRotation has copy, drop, store {
        to: address,
        eligible_at_ms: u64,
    }

    /// Previous authority, authorized until `until_ms` (grace window).
    public struct PreviousAuthority has copy, drop, store {
        addr: address,
        until_ms: u64,
    }

    /// The capability is entombed and authority initialized to the sender.
    public struct CapEntombed has copy, drop {
        authority: address,
    }

    /// A rotation was pre-authorized (or re-authorized over a prior request).
    /// `authority` is the rotation authority the request applies to — the
    /// requester is the `OpsCap` holder, who need not be that authority.
    public struct RotationRequested has copy, drop {
        authority: address,
        to: address,
        eligible_at_ms: u64,
    }

    /// A pending request was cancelled by the authority.
    public struct RotationCancelled has copy, drop {
        authority: address,
    }

    /// The target claimed: authority flipped and the grace window started in
    /// the same transaction.
    public struct RotationClaimed has copy, drop {
        old_authority: address,
        new_authority: address,
        prev_until_ms: u64,
    }

    /// The previous authority was retired early (one-way).
    public struct PreviousRetired has copy, drop {
        authority: address,
    }

    /// An `OpsCap` was destroyed. Steering for its object is permanently
    /// gone: no further rotation can be requested for it.
    public struct OpsCapRetired has copy, drop {
        cap_id: ID,
    }

    /// An `OpsCap` was minted: by [`entomb_ops`] to the entombing sender, or
    /// by the one-shot [`mint_ops_cap`] custody ceremony to `recipient`.
    public struct OpsCapMinted has copy, drop {
        for_id: ID,
        recipient: address,
    }

    /// Entombs `cap` on `parent` and initializes authority to the sender.
    ///
    /// Signature frozen by the published package; superseded by
    /// [`entomb_ops`], which also mints the object's `OpsCap`. The body is a
    /// tombstone — entombment without an `OpsCap` would create a deployment
    /// nobody can steer, so every call aborts.
    public fun entomb<CAP: key + store>(
        _parent: &mut UID,
        _cap: CAP,
        _ctx: &TxContext,
    ) {
        abort E_OPS_REQUIRED
    }

    /// Entombs `cap` on `parent`, initializes authority to the sender, and
    /// mints the object's `OpsCap` to the sender.
    ///
    /// The last transaction ever gated by object ownership of the cap: from
    /// here on, calls are checked against `RotationState` and steering
    /// against the freshly minted `OpsCap`. `initialize` still transfers the
    /// caps to the operator; entombment is the migration step, and the mint
    /// makes the operator the first steering authority. The sender should
    /// move the `OpsCap` to cold custody before going live.
    public fun entomb_ops<CAP: key + store>(parent: &mut UID, cap: CAP, ctx: &mut TxContext) {
        assert!(!df::exists(parent, StateKey {}), E_ALREADY_ENTOMBED);
        assert!(!df::exists(parent, OpsMintedKey {}), E_OPS_MINTED);
        let authority = tx_context::sender(ctx);
        df::add(parent, TombKey {}, cap);
        df::add(
            parent,
            StateKey {},
            RotationState {
                authority,
                prev: option::none(),
                pending: option::none(),
            },
        );
        df::add(parent, OpsMintedKey {}, true);
        mint_and_transfer(parent, authority, ctx);
        event::emit(CapEntombed { authority });
    }

    /// One-shot custody ceremony for deployments entombed before the
    /// `OpsCap` existed (their `RotationState` is live but no cap was ever
    /// minted): mints the object's `OpsCap` straight to `recipient` — the
    /// cold steering address — without the hot key ever holding it.
    ///
    /// Authority only (the same key that settles), and once per object: the
    /// mint marker dynamic field makes a later re-mint impossible, so a
    /// key compromised after the ceremony cannot produce itself a cap.
    /// Deployments whose caps live in a foreign wallet settle fail-closed
    /// until their operator runs this or the `entomb_ops` ceremony.
    public fun mint_ops_cap(parent: &mut UID, recipient: address, ctx: &mut TxContext) {
        assert_is_authority(parent, ctx);
        assert!(!df::exists(parent, OpsMintedKey {}), E_OPS_MINTED);
        df::add(parent, OpsMintedKey {}, true);
        mint_and_transfer(parent, recipient, ctx);
    }

    /// Returns the `OpsMintedKey` singleton for indexers and tests.
    public fun ops_minted_key(): OpsMintedKey {
        OpsMintedKey {}
    }

    /// Dynamic-field key marking that an `OpsCap` was already minted for the
    /// guarded object.
    public struct OpsMintedKey has copy, drop, store {}

    /// Mints the `OpsCap` for `parent` and transfers it to `recipient`.
    fun mint_and_transfer(parent: &UID, recipient: address, ctx: &mut TxContext) {
        transfer::public_transfer(
            OpsCap { id: object::new(ctx), for_id: object::uid_to_inner(parent) },
            recipient,
        );
        event::emit(OpsCapMinted {
            for_id: object::uid_to_inner(parent),
            recipient,
        });
    }

    /// Borrows the authority record. Aborts when the parent was never
    /// entombed — callers treat that as fail-closed.
    public fun state(parent: &UID): &RotationState {
        df::borrow(parent, StateKey {})
    }

    /// Returns the current authority.
    public fun authority(st: &RotationState): address {
        st.authority
    }

    /// Returns the pre-authorized target, if any.
    public fun pending_target(st: &RotationState): Option<address> {
        if (option::is_some(&st.pending)) {
            option::some(option::borrow(&st.pending).to)
        } else {
            option::none()
        }
    }

    /// Aborts unless `sender` may settle/withdraw now: the current authority,
    /// or the previous one inside its grace window.
    public fun assert_can_act(parent: &UID, clock: &Clock, ctx: &TxContext) {
        let st = state(parent);
        let sender = tx_context::sender(ctx);
        if (sender == st.authority) {
            return
        };
        if (option::is_some(&st.prev)) {
            let prev = *option::borrow(&st.prev);
            if (prev.addr == sender && clock::timestamp_ms(clock) <= prev.until_ms) {
                return
            };
        };
        abort E_NOT_AUTHORIZED
    }

    /// Pre-authorizes `to` to claim after `delay_ms`.
    ///
    /// Signature frozen by the published package; steering moved to
    /// [`request_rotation_ops`], which requires the object's `OpsCap`. The
    /// body is a tombstone: every call aborts, so no key can steer rotation
    /// through this entrypoint.
    public fun request_rotation(
        _parent: &mut UID,
        _to: address,
        _delay_ms: u64,
        _clock: &Clock,
        _ctx: &TxContext,
    ) {
        abort E_OPS_REQUIRED
    }

    /// Pre-authorizes `to` to claim after `delay_ms`, replacing any pending
    /// request. Authorized by holding the object's `OpsCap` — the steering
    /// authority — not by being the rotation authority, so the hot sequencer
    /// key can settle but cannot steer. The previous authority in its grace
    /// window can settle but can steer only if it also holds the cap. The
    /// sender is never read: the capability is the whole check.
    ///
    /// Aborts with `E_OPS_MISMATCH` when the cap was minted for another
    /// guarded object.
    public fun request_rotation_ops(
        parent: &mut UID,
        to: address,
        delay_ms: u64,
        clock: &Clock,
        ops: &OpsCap,
    ) {
        assert!(ops.for_id == object::uid_to_inner(parent), E_OPS_MISMATCH);
        assert!(MIN_DELAY_MS <= delay_ms && delay_ms <= MAX_DELAY_MS, E_DELAY_OUT_OF_BOUNDS);
        let eligible_at_ms = clock::timestamp_ms(clock) + delay_ms;
        let authority = state(parent).authority;
        df::borrow_mut<StateKey, RotationState>(parent, StateKey {}).pending =
            option::some(PendingRotation { to, eligible_at_ms });
        event::emit(RotationRequested {
            authority,
            to,
            eligible_at_ms,
        });
    }

    /// Cancels the pending request. Authority only.
    public fun cancel_rotation(parent: &mut UID, ctx: &TxContext) {
        assert_is_authority(parent, ctx);
        let st = df::borrow_mut<StateKey, RotationState>(parent, StateKey {});
        assert!(option::is_some(&st.pending), E_NO_PENDING);
        st.pending = option::none();
        event::emit(RotationCancelled {
            authority: tx_context::sender(ctx),
        });
    }

    /// Completes the rotation: only the pre-authorized target, only inside
    /// the claim window. Flips authority and starts the previous owner's grace
    /// window in the same transaction — the atomic cut-over.
    public fun claim_rotation(parent: &mut UID, clock: &Clock, ctx: &TxContext) {
        let st = df::borrow_mut<StateKey, RotationState>(parent, StateKey {});
        assert!(option::is_some(&st.pending), E_NO_PENDING);
        let pending = *option::borrow(&st.pending);
        let sender = tx_context::sender(ctx);
        assert!(sender == pending.to, E_NOT_CLAIM_TARGET);
        let now = clock::timestamp_ms(clock);
        assert!(now >= pending.eligible_at_ms, E_CLAIM_TOO_EARLY);
        assert!(now <= pending.eligible_at_ms + CLAIM_WINDOW_MS, E_CLAIM_WINDOW_EXPIRED);

        let old = st.authority;
        let prev_until_ms = now + PREV_GRACE_MS;
        st.authority = sender;
        st.prev = option::some(PreviousAuthority {
            addr: old,
            until_ms: prev_until_ms,
        });
        st.pending = option::none();
        event::emit(RotationClaimed {
            old_authority: old,
            new_authority: sender,
            prev_until_ms,
        });
    }

    /// Retires the previous authority early, after the swap is verified.
    /// Authority only, one-way.
    public fun retire_previous(parent: &mut UID, ctx: &TxContext) {
        assert_is_authority(parent, ctx);
        let st = df::borrow_mut<StateKey, RotationState>(parent, StateKey {});
        assert!(option::is_some(&st.prev), E_NO_PREVIOUS);
        st.prev = option::none();
        event::emit(PreviousRetired {
            authority: tx_context::sender(ctx),
        });
    }

    fun assert_is_authority(parent: &UID, ctx: &TxContext) {
        assert!(tx_context::sender(ctx) == state(parent).authority, E_NOT_AUTHORIZED);
    }

    // ---------- tests ----------

    #[test_only]
    public struct TestParent has key {
        id: UID,
    }

    #[test_only]
    public struct TestCap has key, store {
        id: UID,
    }

    #[test_only]
    const ALICE: address = @0xA;
    #[test_only]
    const BOB: address = @0xB;
    #[test_only]
    const CAROL: address = @0xC;
    /// The cold steering holder; deliberately never the rotation authority.
    #[test_only]
    const OPS: address = @0xD;

    /// Sender-switching test context; `dummy` pins the sender to @0x0.
    ///
    /// `tx_context::sender` reads a runtime native, not the context struct:
    /// in unit tests the effective sender is the one from the most recently
    /// *constructed* context. Bind a fresh context right before every call
    /// whose sender matters, each with its own hint (ids derive from the tx
    /// hash, so repeated hints risk id collisions).
    #[test_only]
    fun ctx_for(sender: address, hint: u64): TxContext {
        tx_context::new_from_hint(sender, hint, 0, 0, 0)
    }

    #[test_only]
    fun parent_for_testing(ctx: &mut TxContext): TestParent {
        let mut parent = TestParent { id: object::new(ctx) };
        entomb_ops(&mut parent.id, TestCap { id: object::new(ctx) }, ctx);
        parent
    }

    /// A parent in the pre-`OpsCap` shape: rotation state live, no mint
    /// marker — what `mint_ops_cap` exists to migrate.
    #[test_only]
    fun legacy_parent_for_testing(ctx: &mut TxContext): TestParent {
        let mut parent = TestParent { id: object::new(ctx) };
        let authority = tx_context::sender(ctx);
        df::add(&mut parent.id, TombKey {}, TestCap { id: object::new(ctx) });
        df::add(
            &mut parent.id,
            StateKey {},
            RotationState {
                authority,
                prev: option::none(),
                pending: option::none(),
            },
        );
        parent
    }

    #[test_only]
    fun destroy_legacy_parent(parent: &mut TestParent) {
        let _ = df::remove<StateKey, RotationState>(&mut parent.id, StateKey {});
        let _ = df::remove<OpsMintedKey, bool>(&mut parent.id, OpsMintedKey {});
        let TestCap { id } = df::remove<TombKey, TestCap>(&mut parent.id, TombKey {});
        id.delete();
    }

    /// Test-only mint. Production caps come only from [`entomb_ops`] and
    /// [`mint_ops_cap`], which transfer them to a wallet — an owned object
    /// no unit test can reach — so steering tests mint an equivalent cap
    /// bound to the same parent id. Public test-only: the settlement and
    /// bridge modules' tests bind caps to their shared objects through it.
    #[test_only]
    public fun mint_ops_for_testing(parent: &UID, ctx: &mut TxContext): OpsCap {
        OpsCap { id: object::new(ctx), for_id: object::uid_to_inner(parent) }
    }

    #[test_only]
    fun destroy_parent(parent: &mut TestParent) {
        let _ = df::remove<StateKey, RotationState>(&mut parent.id, StateKey {});
        let TestCap { id } = df::remove<TombKey, TestCap>(&mut parent.id, TombKey {});
        id.delete();
    }

    #[test_only]
    fun destroy(parent: TestParent) {
        let TestParent { id } = parent;
        id.delete();
    }

    #[test]
    fun prev_is_authorized_through_the_last_grace_millisecond() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // Through the last grace millisecond both authorities can act.
        clock::set_for_testing(&mut clock, MIN_DELAY_MS + PREV_GRACE_MS);
        let ctx_b2 = ctx_for(BOB, 3);
        assert_can_act(&parent.id, &clock, &ctx_b2);
        let ctx_a2 = ctx_for(ALICE, 4);
        assert_can_act(&parent.id, &clock, &ctx_a2);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun prev_is_rejected_one_ms_past_grace() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        clock::set_for_testing(&mut clock, MIN_DELAY_MS + PREV_GRACE_MS + 1);
        let ctx_a2 = ctx_for(ALICE, 3);
        assert_can_act(&parent.id, &clock, &ctx_a2);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test]
    fun re_request_overwrites_the_target_and_the_clock() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        clock::set_for_testing(&mut clock, 1_000);
        request_rotation_ops(&mut parent.id, CAROL, MIN_DELAY_MS, &clock, &ops);
        let target = pending_target(state(&parent.id));
        assert!(option::is_some(&target) && *option::borrow(&target) == CAROL);

        // Only the latest target can claim, at its own eligibility.
        clock::set_for_testing(&mut clock, 1_000 + MIN_DELAY_MS);
        let ctx_c = ctx_for(CAROL, 2);
        claim_rotation(&mut parent.id, &clock, &ctx_c);
        assert!(authority(state(&parent.id)) == CAROL);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun an_older_previous_loses_access_when_a_new_claim_replaces_it() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // Bob (now authority) re-requests to Carol through the ops holder;
        // the claim replaces the grace record, so Alice loses access
        // immediately.
        clock::set_for_testing(&mut clock, 2 * MIN_DELAY_MS);
        request_rotation_ops(&mut parent.id, CAROL, MIN_DELAY_MS, &clock, &ops);
        let ctx_c = ctx_for(CAROL, 4);
        clock::set_for_testing(&mut clock, 3 * MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_c);
        let ctx_a2 = ctx_for(ALICE, 5);
        assert_can_act(&parent.id, &clock, &ctx_a2);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_CLAIM_TARGET)]
    fun the_authority_cannot_claim_its_own_request() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        // The ops holder steers to a new key; the current authority claiming
        // a request staged for someone else is still rejected.
        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        let ctx_a2 = ctx_for(ALICE, 2);
        claim_rotation(&mut parent.id, &clock, &ctx_a2);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    // ---------- adversarial: acting around the rotation window ----------

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun the_preauthorized_target_cannot_act_without_claiming() {
        let mut ctx_a = ctx_for(ALICE, 11);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        // Even past the eligibility point, the staged key is nobody until the
        // claim lands: a half-rotated sequencer signing with the new key
        // fails closed instead of acting with one foot in each role.
        clock::set_for_testing(&mut clock, MIN_DELAY_MS + 1);
        let ctx_b = ctx_for(BOB, 12);
        assert_can_act(&parent.id, &clock, &ctx_b);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_OPS_REQUIRED)]
    fun the_previous_owner_cannot_steer_during_grace() {
        let mut ctx_a = ctx_for(ALICE, 13);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let ctx_b = ctx_for(BOB, 14);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // Inside the grace window the previous authority may settle and
        // withdraw, but steering goes through `request_rotation_ops` and the
        // `OpsCap` she does not hold — the tombstoned entrypoint rejects her
        // (and everyone else) outright.
        let ctx_a2 = ctx_for(ALICE, 15);
        request_rotation(&mut parent.id, ALICE, MIN_DELAY_MS, &clock, &ctx_a2);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_OPS_REQUIRED)]
    fun the_tombstoned_request_aborts_even_for_the_authority() {
        let mut ctx_a = ctx_for(ALICE, 29);
        let mut parent = parent_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        // The hot sequencer key IS the authority here; steering still goes
        // through the cap, which it does not hold.
        request_rotation(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ctx_a);

        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test]
    fun the_ops_holder_can_request_without_being_the_authority() {
        let mut ctx_a = ctx_for(ALICE, 23);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        // Steering never reads the sender — the bound cap is the whole
        // check — so a cold holder Alice never authorized as authority can
        // stage the rotation, and only the target can complete it.
        let ctx_ops = ctx_for(OPS, 24);
        clock::set_for_testing(&mut clock, 1_000);
        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let target = pending_target(state(&parent.id));
        assert!(option::is_some(&target) && *option::borrow(&target) == BOB);

        clock::set_for_testing(&mut clock, 1_000 + MIN_DELAY_MS);
        let ctx_b = ctx_for(BOB, 25);
        claim_rotation(&mut parent.id, &clock, &ctx_b);
        assert!(authority(state(&parent.id)) == BOB);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
        let _ = ctx_ops;
    }

    #[test, expected_failure(abort_code = E_OPS_MISMATCH)]
    fun an_ops_cap_bound_to_another_object_cannot_steer() {
        let mut ctx_a = ctx_for(ALICE, 26);
        let mut parent_a = parent_for_testing(&mut ctx_a);
        let mut ctx_b = ctx_for(BOB, 27);
        let mut parent_b = parent_for_testing(&mut ctx_b);
        let clock = clock::create_for_testing(&mut ctx_a);
        let ops_a = mint_ops_for_testing(&parent_a.id, &mut ctx_a);

        // Caps are bound at mint; parent B rejects A's cap even though the
        // holder is a legitimate steering authority elsewhere.
        request_rotation_ops(&mut parent_b.id, CAROL, MIN_DELAY_MS, &clock, &ops_a);

        retire_ops_cap(ops_a);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent_a);
        destroy(parent_a);
        destroy_parent(&mut parent_b);
        destroy(parent_b);
    }

    #[test]
    fun the_ops_holder_can_steer_during_grace() {
        let mut ctx_a = ctx_for(ALICE, 30);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let ctx_b = ctx_for(BOB, 31);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // Steering is the ops holder's power at all times, grace included:
        // the grace window only protects the previous authority's in-flight
        // settle/withdraw calls, and a claim staged now still waits out its
        // own timelock.
        clock::set_for_testing(&mut clock, MIN_DELAY_MS + 1);
        request_rotation_ops(&mut parent.id, CAROL, MIN_DELAY_MS, &clock, &ops);
        let target = pending_target(state(&parent.id));
        assert!(option::is_some(&target) && *option::borrow(&target) == CAROL);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test]
    fun retiring_the_ops_cap_is_one_way() {
        let mut ctx_a = ctx_for(ALICE, 32);
        let mut parent = parent_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        assert!(ops_bound_object(&ops) == object::uid_to_inner(&parent.id));
        // Provable retirement: after this the cap cannot even be named by a
        // transaction, so steering is structurally unreachable.
        retire_ops_cap(ops);

        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_OPS_REQUIRED)]
    fun the_tombstoned_entomb_aborts() {
        let mut ctx_a = ctx_for(ALICE, 33);
        let mut parent = TestParent { id: object::new(&mut ctx_a) };

        entomb(&mut parent.id, TestCap { id: object::new(&mut ctx_a) }, &ctx_a);

        destroy(parent);
    }

    #[test]
    fun the_legacy_mint_marks_the_object_and_reaches_the_recipient() {
        let mut ctx_a = ctx_for(ALICE, 34);
        let mut parent = legacy_parent_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        // The authority runs the one-shot ceremony straight to the cold
        // steering address; the marker blocks any later re-mint.
        assert!(!df::exists(&parent.id, OpsMintedKey {}));
        mint_ops_cap(&mut parent.id, OPS, &mut ctx_a);
        assert!(df::exists(&parent.id, OpsMintedKey {}));

        // Steering works from the recipient side through the same bound-cap
        // check the entombment mint produces.
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);
        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let target = pending_target(state(&parent.id));
        assert!(option::is_some(&target) && *option::borrow(&target) == BOB);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_legacy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun the_legacy_mint_is_authority_only() {
        let mut ctx_a = ctx_for(ALICE, 35);
        let mut parent = parent_for_testing(&mut ctx_a);

        let mut ctx_b = ctx_for(BOB, 36);
        mint_ops_cap(&mut parent.id, BOB, &mut ctx_b);

        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_OPS_MINTED)]
    fun the_legacy_mint_is_one_shot() {
        let mut ctx_a = ctx_for(ALICE, 37);
        let mut parent = legacy_parent_for_testing(&mut ctx_a);

        mint_ops_cap(&mut parent.id, OPS, &mut ctx_a);
        mint_ops_cap(&mut parent.id, OPS, &mut ctx_a);

        destroy_legacy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun the_previous_owner_cannot_retire_itself_during_grace() {
        let mut ctx_a = ctx_for(ALICE, 16);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let ctx_b = ctx_for(BOB, 17);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // Retiring is the authority's one-way kill switch; the previous
        // authority cannot end its own window early to then blame the swap.
        let ctx_a2 = ctx_for(ALICE, 18);
        retire_previous(&mut parent.id, &ctx_a2);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun retiring_the_previous_kills_its_window_immediately() {
        let mut ctx_a = ctx_for(ALICE, 19);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        let ops = mint_ops_for_testing(&parent.id, &mut ctx_a);

        request_rotation_ops(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ops);
        let ctx_b = ctx_for(BOB, 20);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // The watcher retires as soon as it observes the adopt; the one-way
        // retire — not the grace clock — is what stops the old key.
        let ctx_b2 = ctx_for(BOB, 21);
        retire_previous(&mut parent.id, &ctx_b2);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS + 1);
        let ctx_a2 = ctx_for(ALICE, 22);
        assert_can_act(&parent.id, &clock, &ctx_a2);

        retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }
}
