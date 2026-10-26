/// Podseq force-inclusion inbox.
///
/// Gives users an escape hatch from a censoring sequencer: anyone submits raw L2
/// transaction bytes here, and the sequencer must include them in an L2 block
/// before a deadline or the chain halts. Deadlines are anchored on Sui's shared
/// `Clock` at submit time and enforced against the timestamp of the block being
/// settled, so a transaction included on time is on time even if DA batching
/// delays its settlement (see `docs/src/force-inclusion.md`).
///
/// The module has no capability object: `submit` is permissionless, and the
/// inclusion cursor only moves inside `settlement::settle`, which is gated by
/// `SettlerCap`. The Sui gRPC read API has no event query, so the sequencer and
/// full nodes read pending transactions by nonce through each entry's
/// dynamic-field object, mirroring `bridge::deposit_record`.
module podseq::inbox {
    use sui::clock::{Self, Clock};
    use sui::event;
    use sui::object::{Self, UID};
    use std::option::{Self, Option};
    use sui::table::{Self, Table};
    use sui::transfer;
    use sui::tx_context::{Self, TxContext};

    /// Deadline window: a forced transaction must be included in a block whose
    /// timestamp is at most this many milliseconds after submission. 10 minutes
    /// absorbs the sequencer's poll interval, block build time, and DA batch lag
    /// while capping censorship at 10 minutes per transaction.
    const INCLUSION_WINDOW_MS: u64 = 600_000;

    /// Submitted transaction bytes are empty.
    const E_EMPTY_TX: u64 = 0;
    /// The inclusion cursor moved backwards.
    const E_CURSOR_REGRESSION: u64 = 1;
    /// The inclusion cursor covers nonces that were never submitted.
    const E_CURSOR_TOO_HIGH: u64 = 2;
    /// The settled block's timestamp is older than the previous settle's.
    const E_TIMESTAMP_REGRESSION: u64 = 3;
    /// The oldest pending transaction's deadline passed without inclusion.
    const E_DEADLINE_EXCEEDED: u64 = 4;

    /// One forced transaction awaiting inclusion.
    public struct PendingTx has store {
        /// Latest block timestamp by which this transaction must be included.
        deadline_ms: u64,
        /// Raw EVM transaction bytes, submitted to Reth's pool by the sequencer.
        tx_bytes: vector<u8>,
    }

    /// Shared inbox of forced L2 transactions.
    public struct Inbox has key {
        id: UID,
        /// Next nonce to assign to a submitted transaction. Nonces are dense:
        /// 0..next_nonce have all been assigned.
        next_nonce: u64,
        /// Count of nonces included in settled L2 blocks. Nonces below this
        /// count have been pruned from `pending`.
        included_count: u64,
        /// Timestamp of the most recently settled block. Monotone across
        /// settles; also serves as a settlement-progress witness for full nodes.
        last_settled_ts: u64,
        /// `nonce -> PendingTx` for every submitted-but-not-included
        /// transaction. Dense over `included_count..next_nonce`.
        pending: Table<u64, PendingTx>,
    }

    /// Emitted when a forced transaction is submitted.
    public struct ForcedTxSubmitted has copy, drop {
        nonce: u64,
        deadline_ms: u64,
    }

    /// Emitted when a settle call advances the inclusion cursor.
    public struct InclusionCursorAdvanced has copy, drop {
        from: u64,
        to: u64,
    }

    /// Creates and shares the inbox.
    ///
    /// Call once at package publish, alongside `settlement::initialize`.
    public fun initialize(ctx: &mut TxContext) {
        transfer::share_object(Inbox {
            id: object::new(ctx),
            next_nonce: 0,
            included_count: 0,
            last_settled_ts: 0,
            pending: table::new(ctx),
        });
    }

    /// Records a forced L2 transaction under the next nonce.
    ///
    /// Permissionless: anyone censored by the sequencer can call this. The
    /// deadline is consensus time plus [`INCLUSION_WINDOW_MS`].
    public fun submit(inbox: &mut Inbox, clock: &Clock, tx_bytes: vector<u8>) {
        assert!(tx_bytes.length() > 0, E_EMPTY_TX);
        let nonce = inbox.next_nonce;
        let deadline_ms = clock.timestamp_ms() + INCLUSION_WINDOW_MS;
        inbox.pending.add(nonce, PendingTx { deadline_ms, tx_bytes });
        inbox.next_nonce = nonce + 1;
        event::emit(ForcedTxSubmitted { nonce, deadline_ms });
    }

    /// Advances the inclusion cursor to `cursor` and enforces deadlines.
    ///
    /// Called only from `settlement::settle` with the timestamp of the block
    /// being settled. Covered entries are pruned first, so a transaction whose
    /// deadline passed during a DA outage still settles cleanly once its block
    /// lands. Then, if the oldest remaining entry's deadline is older than
    /// `block_ts`, the call aborts: every further settle fails until the
    /// transaction is included, which is the chain halt.
    ///
    /// Because an aborting transaction has no effects, halt is not a stored
    /// flag. It is the persistent shape of the state: the entry stays pending,
    /// its deadline stays expired, and each retry aborts again.
    public(package) fun enforce_settle(inbox: &mut Inbox, block_ts: u64, cursor: u64) {
        assert!(cursor >= inbox.included_count, E_CURSOR_REGRESSION);
        assert!(cursor <= inbox.next_nonce, E_CURSOR_TOO_HIGH);
        assert!(block_ts >= inbox.last_settled_ts, E_TIMESTAMP_REGRESSION);

        if (cursor > inbox.included_count) {
            let mut nonce = inbox.included_count;
            while (nonce < cursor) {
                let PendingTx { deadline_ms: _, tx_bytes: _ } = inbox.pending.remove(nonce);
                nonce = nonce + 1;
            };
            event::emit(InclusionCursorAdvanced { from: inbox.included_count, to: cursor });
            inbox.included_count = cursor;
        };
        inbox.last_settled_ts = block_ts;

        // Deadlines are non-decreasing in nonce order (the clock is monotone),
        // so only the oldest pending entry can be expired: one table read.
        if (inbox.pending.contains(inbox.included_count)) {
            let oldest = inbox.pending.borrow(inbox.included_count);
            assert!(oldest.deadline_ms >= block_ts, E_DEADLINE_EXCEEDED);
        };
    }

    /// Returns the next nonce that `submit` will assign.
    public fun next_nonce(inbox: &Inbox): u64 {
        inbox.next_nonce
    }

    /// Returns the count of nonces included in settled blocks.
    public fun included_count(inbox: &Inbox): u64 {
        inbox.included_count
    }

    /// Returns the timestamp of the most recently settled block.
    public fun last_settled_ts(inbox: &Inbox): u64 {
        inbox.last_settled_ts
    }

    /// Returns the pending transaction for `nonce`, if it is still awaiting
    /// inclusion. Aborts if `nonce` is out of range.
    public fun pending_at(inbox: &Inbox, nonce: u64): &PendingTx {
        inbox.pending.borrow(nonce)
    }

    /// Deadline of the oldest pending transaction, `none` when the inbox is
    /// empty. Full nodes detect a halt by comparing this against their own
    /// clock, with a margin, together with settlement liveness: an expired
    /// oldest deadline means every further settle aborts until the transaction
    /// is included.
    public fun oldest_pending_deadline(inbox: &Inbox): Option<u64> {
        if (inbox.pending.contains(inbox.included_count)) {
            option::some(inbox.pending.borrow(inbox.included_count).deadline_ms)
        } else {
            option::none()
        }
    }

    // ---------- tests ----------

    /// Builds an unshared inbox for tests.
    #[test_only]
    public fun inbox_for_testing(ctx: &mut TxContext): Inbox {
        Inbox {
            id: object::new(ctx),
            next_nonce: 0,
            included_count: 0,
            last_settled_ts: 0,
            pending: table::new(ctx),
        }
    }

    /// Destroys an inbox; every pending entry must be pruned first.
    #[test_only]
    public fun destroy_inbox_for_testing(inbox: Inbox) {
        let Inbox { id, next_nonce: _, included_count: _, last_settled_ts: _, pending } = inbox;
        table::destroy_empty(pending);
        id.delete();
    }

    /// Submits and prunes everything so the inbox can be destroyed.
    #[test_only]
    public fun drain_for_testing(inbox: &mut Inbox) {
        let mut nonce = inbox.included_count;
        while (nonce < inbox.next_nonce) {
            let PendingTx { deadline_ms: _, tx_bytes: _ } = inbox.pending.remove(nonce);
            nonce = nonce + 1;
        };
        inbox.included_count = inbox.next_nonce;
    }

    #[test]
    fun submit_assigns_dense_increasing_nonces() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        submit(&mut inbox, &clock, b"tx-a");
        submit(&mut inbox, &clock, b"tx-b");

        assert!(next_nonce(&inbox) == 2);
        assert!(pending_at(&inbox, 0).tx_bytes == b"tx-a");
        assert!(pending_at(&inbox, 1).tx_bytes == b"tx-b");

        drain_for_testing(&mut inbox);
        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }

    #[test]
    fun submit_anchors_deadline_on_clock() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_000_000);

        submit(&mut inbox, &clock, b"tx");

        assert!(pending_at(&inbox, 0).deadline_ms == 1_000_000 + 600_000);

        drain_for_testing(&mut inbox);
        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }

    #[test, expected_failure(abort_code = E_EMPTY_TX)]
    fun submit_rejects_empty_transaction_bytes() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        submit(&mut inbox, &clock, b"");

        // Unreachable after the abort; consumes the test objects so the
        // unused-value check passes.
        clock::destroy_for_testing(clock);
        drain_for_testing(&mut inbox);
        destroy_inbox_for_testing(inbox);
    }

    #[test]
    fun settle_gate_prunes_covered_entries_and_advances_cursor() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        submit(&mut inbox, &clock, b"tx-a");
        submit(&mut inbox, &clock, b"tx-b");
        submit(&mut inbox, &clock, b"tx-c");

        enforce_settle(&mut inbox, 5_000, 2);

        assert!(included_count(&inbox) == 2);
        assert!(pending_at(&inbox, 2).tx_bytes == b"tx-c");
        assert!(last_settled_ts(&inbox) == 5_000);

        drain_for_testing(&mut inbox);
        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }

    #[test]
    fun settle_gate_accepts_covering_all_submitted_nonces() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        submit(&mut inbox, &clock, b"tx-a");
        enforce_settle(&mut inbox, 1_000, 1);

        assert!(included_count(&inbox) == 1);
        assert!(next_nonce(&inbox) == 1);
        assert!(oldest_pending_deadline(&inbox) == option::none());

        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }

    #[test, expected_failure(abort_code = E_CURSOR_REGRESSION)]
    fun settle_gate_rejects_cursor_regression() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        submit(&mut inbox, &clock, b"tx-a");
        submit(&mut inbox, &clock, b"tx-b");
        enforce_settle(&mut inbox, 1_000, 2);
        enforce_settle(&mut inbox, 2_000, 1);

        clock::destroy_for_testing(clock);
        drain_for_testing(&mut inbox);
        destroy_inbox_for_testing(inbox);
    }

    #[test, expected_failure(abort_code = E_CURSOR_TOO_HIGH)]
    fun settle_gate_rejects_cursor_above_next_nonce() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        submit(&mut inbox, &clock, b"tx-a");
        enforce_settle(&mut inbox, 1_000, 2);

        clock::destroy_for_testing(clock);
        drain_for_testing(&mut inbox);
        destroy_inbox_for_testing(inbox);
    }

    #[test, expected_failure(abort_code = E_TIMESTAMP_REGRESSION)]
    fun settle_gate_rejects_timestamp_regression() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);

        enforce_settle(&mut inbox, 5_000, 0);
        enforce_settle(&mut inbox, 4_999, 0);

        drain_for_testing(&mut inbox);
        destroy_inbox_for_testing(inbox);
    }

    #[test]
    fun late_settlement_does_not_halt_included_transaction() {
        // The core lenient property: a transaction included before its deadline
        // settles cleanly even when DA batching delays settlement far past the
        // deadline, because covered entries are pruned before the check.
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        submit(&mut inbox, &clock, b"tx-a");
        // Block built long after the deadline (outage recovery), containing the
        // transaction: the cursor covers it at settle time.
        enforce_settle(&mut inbox, 10_000_000, 1);

        assert!(included_count(&inbox) == 1);
        assert!(oldest_pending_deadline(&inbox) == option::none());

        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }

    #[test, expected_failure(abort_code = E_DEADLINE_EXCEEDED)]
    fun settle_gate_halts_on_expired_oldest_pending() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_000_000);

        submit(&mut inbox, &clock, b"tx-a");
        // Deadline is 1_600_000; the block being settled is newer and does not
        // contain the transaction.
        enforce_settle(&mut inbox, 2_000_000, 0);

        clock::destroy_for_testing(clock);
        drain_for_testing(&mut inbox);
        destroy_inbox_for_testing(inbox);
    }

    #[test]
    fun settle_gate_accepts_block_at_exact_deadline_timestamp() {
        // Boundary: a transaction must be included in a block whose timestamp
        // is at most the deadline, so equality passes and only a strictly
        // later block aborts.
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_000_000);

        submit(&mut inbox, &clock, b"tx-a");
        enforce_settle(&mut inbox, 1_600_000, 0);

        assert!(included_count(&inbox) == 0);
        assert!(last_settled_ts(&inbox) == 1_600_000);

        drain_for_testing(&mut inbox);
        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }

    #[test, expected_failure(abort_code = E_DEADLINE_EXCEEDED)]
    fun settle_gate_halts_at_epoch_scale_timestamps() {
        // Realistic magnitudes: deadlines and settled timestamps are epoch
        // milliseconds (~1.79e12 in 2026). Callers that pass the L2 header's
        // unix-SECONDS timestamp (~1.79e9) would never trip this abort, so
        // pin the units the gate actually compares.
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_793_045_000_000);

        submit(&mut inbox, &clock, b"tx-a");
        // Deadline is 1_793_045_600_000; the block being settled is one
        // millisecond past it and does not contain the transaction.
        enforce_settle(&mut inbox, 1_793_045_600_001, 0);

        clock::destroy_for_testing(clock);
        drain_for_testing(&mut inbox);
        destroy_inbox_for_testing(inbox);
    }

    #[test]
    fun halt_clears_once_overdue_transaction_is_covered() {
        // Two transactions submitted at different times. The older one's
        // deadline expires, but once a block including it settles, only the
        // younger entry remains pending and its deadline still holds.
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_000_000);

        submit(&mut inbox, &clock, b"tx-a");
        clock::increment_for_testing(&mut clock, 700_000);
        submit(&mut inbox, &clock, b"tx-b");

        let (first_deadline, second_deadline) = (
            pending_at(&inbox, 0).deadline_ms,
            pending_at(&inbox, 1).deadline_ms,
        );
        assert!(first_deadline < second_deadline);

        // Settling a block that includes only tx-a, past tx-a's deadline but
        // before tx-b's: succeeds, and the halt condition is gone.
        enforce_settle(&mut inbox, 1_800_000, 1);

        assert!(included_count(&inbox) == 1);
        let remaining = oldest_pending_deadline(&inbox);
        assert!(remaining == option::some(1_700_000 + 600_000));

        drain_for_testing(&mut inbox);
        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }

    #[test]
    fun oldest_pending_deadline_tracks_the_cursor() {
        let ctx = &mut tx_context::dummy();
        let mut inbox = inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        clock::set_for_testing(&mut clock, 1_000_000);

        submit(&mut inbox, &clock, b"tx-a");
        clock::increment_for_testing(&mut clock, 5);
        submit(&mut inbox, &clock, b"tx-b");

        assert!(oldest_pending_deadline(&inbox) == option::some(1_600_000));

        enforce_settle(&mut inbox, 1_000, 1);

        assert!(oldest_pending_deadline(&inbox) == option::some(1_600_005));

        drain_for_testing(&mut inbox);
        clock::destroy_for_testing(clock);
        destroy_inbox_for_testing(inbox);
    }
}
