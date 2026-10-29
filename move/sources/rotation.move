/// Cap-rotation state machine for the entombed SettlerCap and BridgeCap.
///
/// The caps live as dynamic fields of the shared objects they guard (see
/// `settlement::entomb_settler_cap` and `bridge::entomb_bridge_cap`), so
/// authority is no longer an object-ownership property: it is the
/// `RotationState` record stored next to them and checked against the
/// transaction sender. Rotation is a two-step, timelocked transition claimable
/// only by the pre-authorized target, and a claim starts a grace window during
/// which the previous authority's in-flight calls stay valid. Design record:
/// docs/src/cap-rotation.md.
module podseq::rotation {
    use sui::clock::{Self, Clock};
    use sui::dynamic_field as df;
    use sui::event;
    use sui::object::UID;
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

    /// Dynamic-field key under which the authority record is stored on the
    /// guarded object.
    public struct StateKey has copy, drop, store {}

    /// Dynamic-field key under which the entombed capability is stored.
    public struct TombKey has copy, drop, store {}

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

    /// Entombs `cap` on `parent` and initializes authority to the sender.
    ///
    /// The last transaction ever gated by object ownership of the cap: from
    /// here on, calls are checked against `RotationState`. `initialize` still
    /// transfers the caps to the operator; entombment is the migration step.
    public fun entomb<CAP: key + store>(parent: &mut UID, cap: CAP, ctx: &TxContext) {
        assert!(!df::exists(parent, StateKey {}), E_ALREADY_ENTOMBED);
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
        event::emit(CapEntombed { authority });
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

    /// Pre-authorizes `to` to claim after `delay_ms`, replacing any pending
    /// request. Authority only — the previous authority in its grace window
    /// can settle but cannot steer rotation.
    public fun request_rotation(
        parent: &mut UID,
        to: address,
        delay_ms: u64,
        clock: &Clock,
        ctx: &TxContext,
    ) {
        assert_is_authority(parent, ctx);
        assert!(MIN_DELAY_MS <= delay_ms && delay_ms <= MAX_DELAY_MS, E_DELAY_OUT_OF_BOUNDS);
        let eligible_at_ms = clock::timestamp_ms(clock) + delay_ms;
        df::borrow_mut<StateKey, RotationState>(parent, StateKey {}).pending =
            option::some(PendingRotation { to, eligible_at_ms });
        event::emit(RotationRequested {
            authority: tx_context::sender(ctx),
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
        entomb(&mut parent.id, TestCap { id: object::new(ctx) }, ctx);
        parent
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

        request_rotation(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ctx_a);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // Through the last grace millisecond both authorities can act.
        clock::set_for_testing(&mut clock, MIN_DELAY_MS + PREV_GRACE_MS);
        let ctx_b2 = ctx_for(BOB, 3);
        assert_can_act(&parent.id, &clock, &ctx_b2);
        let ctx_a2 = ctx_for(ALICE, 4);
        assert_can_act(&parent.id, &clock, &ctx_a2);

        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun prev_is_rejected_one_ms_past_grace() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ctx_a);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        clock::set_for_testing(&mut clock, MIN_DELAY_MS + PREV_GRACE_MS + 1);
        let ctx_a2 = ctx_for(ALICE, 3);
        assert_can_act(&parent.id, &clock, &ctx_a2);

        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test]
    fun re_request_overwrites_the_target_and_the_clock() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ctx_a);
        clock::set_for_testing(&mut clock, 1_000);
        request_rotation(&mut parent.id, CAROL, MIN_DELAY_MS, &clock, &ctx_a);
        let target = pending_target(state(&parent.id));
        assert!(option::is_some(&target) && *option::borrow(&target) == CAROL);

        // Only the latest target can claim, at its own eligibility.
        clock::set_for_testing(&mut clock, 1_000 + MIN_DELAY_MS);
        let ctx_c = ctx_for(CAROL, 2);
        claim_rotation(&mut parent.id, &clock, &ctx_c);
        assert!(authority(state(&parent.id)) == CAROL);

        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED)]
    fun an_older_previous_loses_access_when_a_new_claim_replaces_it() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ctx_a);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_b);

        // Bob (now authority) re-requests to Carol; the claim replaces the
        // grace record, so Alice loses access immediately.
        let ctx_b2 = ctx_for(BOB, 3);
        clock::set_for_testing(&mut clock, 2 * MIN_DELAY_MS);
        request_rotation(&mut parent.id, CAROL, MIN_DELAY_MS, &clock, &ctx_b2);
        let ctx_c = ctx_for(CAROL, 4);
        clock::set_for_testing(&mut clock, 3 * MIN_DELAY_MS);
        claim_rotation(&mut parent.id, &clock, &ctx_c);
        let ctx_a2 = ctx_for(ALICE, 5);
        assert_can_act(&parent.id, &clock, &ctx_a2);

        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }

    #[test, expected_failure(abort_code = E_NOT_CLAIM_TARGET)]
    fun the_authority_cannot_claim_its_own_request() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut parent = parent_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut parent.id, BOB, MIN_DELAY_MS, &clock, &ctx_a);
        clock::set_for_testing(&mut clock, MIN_DELAY_MS);
        let ctx_a2 = ctx_for(ALICE, 2);
        claim_rotation(&mut parent.id, &clock, &ctx_a2);

        clock::destroy_for_testing(clock);
        destroy_parent(&mut parent);
        destroy(parent);
    }
}
