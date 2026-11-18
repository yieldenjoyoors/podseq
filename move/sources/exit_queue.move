//! Exit queue: verify L2 balance proofs against settled state roots, and
//! redeem them without the sequencer.
//!
//! Force inclusion gets transactions in; the exit queue gets value out. A
//! censoring sequencer also refuses withdrawals, so the bridge needs the same
//! property the inbox has: a path that works without the sequencer's
//! cooperation. The escape path proves an L2 balance to this module with a
//! Merkle-Patricia-Trie account proof, and the proof is checked against the
//! state root [`settlement::commit_state_root`] anchored for a settled
//! height, so a light client can verify one balance without executing any L2
//! block and without trusting the sequencer.
//!
//! A proof is the root-first list of RLP-encoded trie nodes from the state
//! root to the account's leaf (the shape `eth_getProof` returns). This module
//! decodes RLP and hex-prefix paths, walks the key `keccak256(address)`
//! nibble-by-nibble, verifies every hashed child link against the anchored
//! root, and decodes the leaf's account RLP into `(nonce, balance)`.
//!
//! Redemption consumes a recorded request: the owner signs the payout with
//! their L2 key (EIP-191 over domain ‖ address ‖ amount ‖ recipient), the
//! recovered address must equal the recorded one, and [`redeem`] mints a
//! `bridge::ExitClaim` hot potato that releases the vault's coins. The
//! queue serves records in proof order, ascending `(height, seq)`, and a
//! head that never redeems is demoted after a ten-minute window, so
//! strict order cannot become a veto.
//!
//! See `docs/src/exit-queue.md` for the design and the rejected
//! alternatives (sequencer attestations, ZK transitions, challenge windows).
module podseq::exit_queue {
    use std::option::Self;
    use std::vector;
    use podseq::bridge::{Self, Vault};
    use sui::address;
    use sui::clock::{Self, Clock};
    use sui::coin;
    use sui::dynamic_field;
    use sui::ecdsa_k1;
    use sui::event;
    use sui::hash;
    use sui::object::{Self, UID};
    use sui::table::{Self, Table};
    use sui::transfer;
    use sui::tx_context::{Self, TxContext};
    use podseq::inbox::{Self, Inbox};
    use podseq::settlement::{Self, Registry};

    /// Address is not 20 bytes.
    const E_ADDRESS: u64 = 1;
    /// No state root is anchored for this height.
    const E_NO_ANCHOR: u64 = 2;
    /// The proof contains no nodes.
    const E_EMPTY_PROOF: u64 = 3;
    /// keccak(proof[0]) is not the anchored root.
    const E_ROOT_MISMATCH: u64 = 4;
    /// Truncated or malformed RLP.
    const E_RLP: u64 = 5;
    /// A trie node is not a branch, extension, or leaf, or a child reference
    /// is neither a 32-byte hash nor an inline node.
    const E_NODE_SHAPE: u64 = 6;
    /// keccak(node) does not match the hash the parent embedded.
    const E_LINK: u64 = 7;
    /// The proof ran out of nodes while a child referenced a hash.
    const E_PROOF_SHORT: u64 = 8;
    /// The proof carries nodes the walk never consumed.
    const E_PROOF_EXTRA: u64 = 9;
    /// The walk hit an empty branch slot, or a node's path does not match the
    /// key it claims to route.
    const E_PATH: u64 = 10;
    /// The leaf value is not a four-item account RLP.
    const E_ACCOUNT: u64 = 11;
    /// The chain is not in the inbox-derived halt state: no pending forced
    /// transaction is past its deadline, so the escape path stays closed.
    const E_NOT_HALTED: u64 = 12;
    /// No escape request is recorded for this address.
    const E_NO_REQUEST: u64 = 13;
    /// The signature is not a 65-byte `(r, s, v)` value, or the recovered
    /// address is not the recorded one.
    const E_SIGNATURE: u64 = 14;
    /// The signed payload does not bind what redemption pays: wrong length,
    /// domain, address, amount, or recipient.
    const E_PAYLOAD: u64 = 15;
    /// The remaining proven balance exceeds the u64 amounts the vault holds.
    const E_AMOUNT: u64 = 16;
    /// Nothing is left to redeem: the recorded balance is fully paid out.
    const E_NOTHING_TO_REDEEM: u64 = 17;
    /// The address is not the queue head: an indexed record redeems only at
    /// the head, and a record outside the index waits until it drains.
    const E_NOT_QUEUE_HEAD: u64 = 18;
    /// The ordered index is empty, so the skip valve has nothing to pass.
    const E_QUEUE_EMPTY: u64 = 19;
    /// The head has not sat in the queue past [`SKIP_AFTER_MS`], so the
    /// valve will not demote it yet.
    const E_SKIP_EARLY: u64 = 20;

    /// Longest RLP length-of-length prefix accepted, matching Ethereum's 8.
    const MAX_LEN_BYTES: u64 = 8;

    /// Byte length of a recoverable ECDSA signature: `r ‖ s ‖ v`.
    const SIGNATURE_LEN: u64 = 65;
    /// Byte length of the signed redemption payload: 15-byte domain ‖ 20-byte
    /// address ‖ 32-byte amount ‖ 32-byte recipient.
    const PAYLOAD_LEN: u64 = 99;
    /// Domain separator binding the signature to this redemption path.
    const PAYLOAD_DOMAIN: vector<u8> = b"podseq-exit:v1:";
    /// The framework's hash selector for keccak256 in the ecdsa natives (its
    /// `ecdsa_k1::KECCAK256` constant is private).
    const ECRECOVER_KECCAK256: u8 = 0;

    /// Grace period after the oldest pending deadline before the escape path
    /// treats the chain as halted. Generous on purpose: a transaction included
    /// on time stays pending on Sui until its block settles (the DA batch
    /// flush alone can hold it for two minutes), and opening exits while
    /// settlement merely lags is the dangerous direction — the margin must
    /// dominate any legitimate settle lag. A genuine halt has the opposite
    /// shape: every settle aborts at the inbox gate, `last_settled_ts`
    /// freezes, and the entry stays pending forever.
    const HALT_MARGIN_MS: u64 = 600_000;

    /// How long a head may hold the ordered payout queue before anyone can
    /// demote it with [`skip_head`]. Same margin class as
    /// [`HALT_MARGIN_MS`]: the exit path already pays ten minutes for
    /// settlement-lag safety, and the valve pays the same for strict
    /// order. A head its owner never signs blocks every payout behind it
    /// until the window elapses — the cost of serving exits in proof
    /// order.
    const SKIP_AFTER_MS: u64 = 600_000;

    /// Failure marker for positive tests (the assertion that failed is named
    /// by the test, not the code).
    #[test_only]
    const E_TEST: u64 = 99;

    /// Verifies an account proof against the state root anchored for
    /// `height`, and returns the account's `(nonce, balance)`.
    ///
    /// `address` is the 20-byte L2 address; `proof` is the root-first list of
    /// RLP-encoded trie nodes from that root to the account's leaf (the shape
    /// `eth_getProof` returns). Every hashed child link is checked, the walk
    /// must consume exactly the whole proof, and the anchored root must exist
    /// — a proof is only as trustworthy as the settled root it verifies
    /// against.
    public fun verify_account(
        registry: &Registry,
        height: u64,
        address: vector<u8>,
        proof: vector<vector<u8>>,
    ): (u64, u256) {
        assert!(vector::length(&address) == 20, E_ADDRESS);
        let anchored = settlement::state_root(registry, height);
        assert!(option::is_some(&anchored), E_NO_ANCHOR);
        let root = option::destroy_some(anchored);
        assert!(!proof.is_empty(), E_EMPTY_PROOF);
        let node = *vector::borrow(&proof, 0);
        assert!(hash::keccak256(&node) == root, E_ROOT_MISMATCH);
        let key = key_nibbles(&address);
        let mut consumed: u64 = 1;
        let value = walk(&proof, &key, &mut consumed, node);
        assert!(consumed == vector::length(&proof), E_PROOF_EXTRA);
        decode_account(&value)
    }

    /// Expands `keccak256(address)` into its 64 hex nibbles: the trie key.
    fun key_nibbles(address: &vector<u8>): vector<u8> {
        let digest = hash::keccak256(address);
        let mut nibbles: vector<u8> = vector[];
        let mut i: u64 = 0;
        while (i < vector::length(&digest)) {
            let b = *vector::borrow(&digest, i);
            vector::push_back(&mut nibbles, b >> 4);
            vector::push_back(&mut nibbles, b % 16);
            i = i + 1;
        };
        nibbles
    }

    /// Walks the proof along `key`'s nibbles from the root node and returns
    /// the leaf's value bytes. `consumed` tracks how many proof nodes the
    /// walk dereferenced; a leaf ends the walk, a branch routes by the next
    /// nibble, an extension routes by its decoded path.
    fun walk(
        proof: &vector<vector<u8>>,
        key: &vector<u8>,
        consumed: &mut u64,
        node: vector<u8>,
    ): vector<u8> {
        let mut node = node;
        let mut i: u64 = 0;
        loop {
            let (start, len, is_list) = header(&node, 0);
            assert!(is_list, E_NODE_SHAPE);
            let items = list_children(&node, start, len);
            let item_count = vector::length(&items);
            if (item_count == 2) {
                let path = string_content(vector::borrow(&items, 0));
                let (is_leaf, nibbles) = decode_hp(&path);
                let prefix_len = vector::length(&nibbles);
                assert!(i + prefix_len <= vector::length(key), E_PATH);
                assert!(nibbles_match(key, i, &nibbles), E_PATH);
                if (is_leaf) {
                    return string_content(vector::borrow(&items, 1))
                };
                i = i + prefix_len;
                node = resolve(vector::borrow(&items, 1), proof, consumed);
            } else if (item_count == 17) {
                if (i == vector::length(key)) {
                    let value = string_content(vector::borrow(&items, 16));
                    assert!(!vector::is_empty(&value), E_PATH);
                    return value
                };
                let idx = (*vector::borrow(key, i)) as u64;
                i = i + 1;
                let child = vector::borrow(&items, idx);
                let empty_slot = vector::length(child) == 1 && *vector::borrow(child, 0) == 0x80;
                assert!(!empty_slot, E_PATH);
                node = resolve(child, proof, consumed);
            } else {
                abort E_NODE_SHAPE
            }
        }
    }

    /// Resolves a branch/extension child reference: a 32-byte string is a
    /// hash into the next proof node (checked); a list item is an inline node
    /// (used as-is). Anything else is a malformed node.
    fun resolve(
        child_ref: &vector<u8>,
        proof: &vector<vector<u8>>,
        consumed: &mut u64,
    ): vector<u8> {
        let first = *vector::borrow(child_ref, 0);
        if (first >= 0xc0) {
            return *child_ref
        };
        let digest = string_content(child_ref);
        assert!(vector::length(&digest) == 32, E_NODE_SHAPE);
        let idx = *consumed;
        assert!(idx < vector::length(proof), E_PROOF_SHORT);
        let node = *vector::borrow(proof, idx);
        *consumed = idx + 1;
        assert!(hash::keccak256(&node) == digest, E_LINK);
        node
    }

    /// The byte content of an RLP string item given its full encoding.
    fun string_content(item: &vector<u8>): vector<u8> {
        let (start, len, is_list) = header(item, 0);
        assert!(!is_list, E_NODE_SHAPE);
        range(item, start, start + len)
    }

    /// Splits a list item's content span into its children's full encodings.
    fun list_children(data: &vector<u8>, start: u64, len: u64): vector<vector<u8>> {
        let mut items: vector<vector<u8>> = vector[];
        let mut off = start;
        let end = start + len;
        while (off < end) {
            let (content_start, content_len, _) = header(data, off);
            vector::push_back(&mut items, range(data, off, content_start + content_len));
            off = content_start + content_len;
        };
        items
    }

    /// Decodes the RLP item at `offset`: `(content_start, content_len,
    /// is_list)`, with the item's bounds checked against `data`.
    fun header(data: &vector<u8>, offset: u64): (u64, u64, bool) {
        let total = vector::length(data);
        assert!(offset < total, E_RLP);
        let b = *vector::borrow(data, offset);
        let (start, content_len, is_list) = if (b < 0x80) {
            // a single byte is its own content
            (offset, 1, false)
        } else if (b < 0xb8) {
            ((offset + 1), (b as u64) - 0x80, false)
        } else if (b < 0xc0) {
            let n = (b as u64) - 0xb7;
            ((offset + 1 + n), be_len(data, offset + 1, n), false)
        } else if (b < 0xf8) {
            ((offset + 1), (b as u64) - 0xc0, true)
        } else {
            let n = (b as u64) - 0xf7;
            ((offset + 1 + n), be_len(data, offset + 1, n), true)
        };
        assert!(start + content_len <= total, E_RLP);
        (start, content_len, is_list)
    }

    /// Reads a big-endian length of `n` bytes at `offset`.
    fun be_len(data: &vector<u8>, offset: u64, n: u64): u64 {
        assert!(n >= 1 && n <= MAX_LEN_BYTES, E_RLP);
        assert!(offset + n <= vector::length(data), E_RLP);
        let mut value: u64 = 0;
        let mut i: u64 = 0;
        while (i < n) {
            value = (value << 8) | (*vector::borrow(data, offset + i) as u64);
            i = i + 1;
        };
        value
    }

    /// Splits `data[from..to]` into a new vector.
    fun range(data: &vector<u8>, from: u64, to: u64): vector<u8> {
        let mut out: vector<u8> = vector[];
        let mut i = from;
        while (i < to) {
            vector::push_back(&mut out, *vector::borrow(data, i));
            i = i + 1;
        };
        out
    }

    /// Decodes a hex-prefix path: `(is_leaf, nibbles)`. The first byte's high
    /// nibble flags leaf vs extension and the path's parity; the low nibble
    /// carries the first path nibble when the count is odd.
    fun decode_hp(path: &vector<u8>): (bool, vector<u8>) {
        let len = vector::length(path);
        assert!(len > 0, E_NODE_SHAPE);
        let flag = *vector::borrow(path, 0) >> 4;
        assert!(flag < 4, E_NODE_SHAPE);
        let mut nibbles: vector<u8> = vector[];
        let mut i: u64 = 1;
        if (flag % 2 == 1) {
            vector::push_back(&mut nibbles, *vector::borrow(path, 0) % 16);
        };
        while (i < len) {
            let b = *vector::borrow(path, i);
            vector::push_back(&mut nibbles, b >> 4);
            vector::push_back(&mut nibbles, b % 16);
            i = i + 1;
        };
        (flag >= 2, nibbles)
    }

    /// True when `key[from..]` starts with `prefix` (nibbles). The caller
    /// bounds-checks `from` against the key length first.
    fun nibbles_match(key: &vector<u8>, from: u64, prefix: &vector<u8>): bool {
        let mut i: u64 = 0;
        while (i < vector::length(prefix)) {
            if (*vector::borrow(key, from + i) != *vector::borrow(prefix, i)) {
                return false
            };
            i = i + 1;
        };
        true
    }

    /// Decodes the leaf value (an account RLP list) into `(nonce, balance)`.
    /// The storage root and code hash must be 32-byte strings; their values
    /// are not needed for a balance proof and are validated for shape only.
    fun decode_account(value: &vector<u8>): (u64, u256) {
        let (start, len, is_list) = header(value, 0);
        assert!(is_list, E_ACCOUNT);
        let items = list_children(value, start, len);
        assert!(vector::length(&items) == 4, E_ACCOUNT);
        let nonce_bytes = string_content(vector::borrow(&items, 0));
        let nonce = be_u64(&nonce_bytes, 8);
        let balance_bytes = string_content(vector::borrow(&items, 1));
        let balance = be_u256(&balance_bytes);
        let storage_root = string_content(vector::borrow(&items, 2));
        let code_hash = string_content(vector::borrow(&items, 3));
        assert!(
            vector::length(&storage_root) == 32 && vector::length(&code_hash) == 32,
            E_ACCOUNT,
        );
        (nonce, balance)
    }

    /// A big-endian byte string as a u64, at most `max_len` bytes.
    fun be_u64(content: &vector<u8>, max_len: u64): u64 {
        assert!(vector::length(content) <= max_len, E_ACCOUNT);
        let mut value: u64 = 0;
        let mut i: u64 = 0;
        while (i < vector::length(content)) {
            value = (value << 8) | (*vector::borrow(content, i) as u64);
            i = i + 1;
        };
        value
    }

    /// A big-endian byte string (at most 32 bytes) as a u256.
    fun be_u256(content: &vector<u8>): u256 {
        assert!(vector::length(content) <= 32, E_ACCOUNT);
        let mut value: u256 = 0;
        let mut i: u64 = 0;
        while (i < vector::length(content)) {
            value = (value << 8) | (*vector::borrow(content, i) as u256);
            i = i + 1;
        };
        value
    }

    // ---------- the escape queue ----------
    //
    // Force inclusion gets transactions in; this gets value out. The unlock
    // is the inbox-derived halt the deadline machinery already implies: when
    // the oldest pending forced transaction has been past its deadline for
    // longer than the margin, every settle aborts and exits must work
    // without the sequencer.

    /// One proven escape request.
    public struct ExitRequest has copy, drop, store {
        /// Settled height whose anchored state root the balance was proven
        /// against. This is the request's queue position.
        height: u64,
        /// Account nonce at that height.
        nonce: u64,
        /// Account balance at that height.
        balance: u256,
        /// Total order among requests at the same height.
        seq: u64,
        /// What redemption has already paid out against this record. Carried
        /// through reproofs, so the cumulative payout never exceeds the
        /// balance proven at some settled height.
        paid: u256,
    }

    /// Shared record of escape requests. Singleton by convention, like the
    /// `Inbox`: created once at package publish by [`initialize`], and the
    /// canonical id is whatever exits are consumed against. Deployments that
    /// predate the module create it by calling [`initialize`] in a
    /// standalone PTB.
    public struct ExitQueue has key {
        id: UID,
        /// L2 address (20 bytes) -> the account's request.
        requests: Table<vector<u8>, ExitRequest>,
        /// Sequence for the next distinct request.
        next_seq: u64,
    }

    // ---------- ordered payout index ----------
    //
    // Redemption serves exits in proof order: ascending `(height, seq)`.
    // `requests` maps address -> record; the index below links the payable
    // records in serving order as dynamic fields of the `ExitQueue` object
    // itself. The struct layout froze at publish, and dynamic fields are
    // how this package grows without breaking deployed objects (the
    // adjustable inclusion window did the same to the `Inbox`).
    //
    // Redemption serves the head and nothing else; a paid head leaves the
    // index and its successor becomes the head. The skip valve keeps
    // strict order from becoming a veto: after [`SKIP_AFTER_MS`] anyone
    // can demote a head that never redeems, sending it behind the whole
    // queue.

    /// Dynamic-field key holding the index head's address. The field's
    /// presence is the index's "not empty" state.
    public struct QueueHeadKey has copy, drop, store {}

    /// Dynamic-field key for one index node, keyed by the L2 address.
    public struct QueueAddrKey has copy, drop, store {
        address: vector<u8>,
    }

    /// One node of the ordered index: the linked list over payable
    /// records, ascending by `(height, seq)`.
    public struct QueueNode has copy, drop, store {
        prev: Option<vector<u8>>,
        next: Option<vector<u8>>,
        /// Clock reading when the record entered the index — the skip
        /// valve's clock. A same-height reproof rewrites the record but
        /// leaves this untouched, so nobody can keep a head fresh forever
        /// by replaying public proofs; only a strictly-earlier proof
        /// re-enters a record, and heights run out.
        recorded_at_ms: u64,
    }

    /// Dynamic-field key holding the index size.
    public struct QueueCountKey has copy, drop, store {}

    /// Dynamic-field key marking a demoted record. Skipped records keep
    /// their request and their paid counter; they redeem once every
    /// indexed record ahead of them has been served.
    public struct SkipMarkKey has copy, drop, store {
        address: vector<u8>,
    }

    /// Emitted when an escape request is recorded or improved. A proof that
    /// would regress the request's height is ignored silently: the state
    /// does not change, so there is nothing to observe.
    public struct ExitRequested has copy, drop {
        /// The 20-byte L2 address the balance was proven for.
        address: vector<u8>,
        /// Settled height the balance was proven against.
        height: u64,
        /// Sequence the request holds.
        seq: u64,
        /// Proven account nonce.
        nonce: u64,
        /// Proven account balance.
        balance: u256,
        /// The expired oldest deadline that unlocked this request.
        expired_deadline_ms: u64,
    }

    /// Creates and shares the exit queue. Call once at package publish,
    /// alongside `inbox::initialize`.
    public fun initialize(ctx: &mut TxContext) {
        transfer::share_object(ExitQueue {
            id: object::new(ctx),
            requests: table::new(ctx),
            next_seq: 0,
        });
    }

    /// Returns [`HALT_MARGIN_MS`], the escape path's halt margin.
    public fun halt_margin_ms(): u64 {
        HALT_MARGIN_MS
    }

    /// The inbox-derived halt verdict, purely from chain data: the expired
    /// oldest deadline once the oldest pending forced transaction has been
    /// past its deadline for more than [`halt_margin_ms`], `none` otherwise
    /// (nothing pending, the deadline still holds, or the margin has not
    /// elapsed).
    ///
    /// The deadline comes from the inbox and the time from Sui's shared
    /// `Clock` — the same chain data the full-node deadline monitor reads,
    /// so anyone can evaluate the predicate in their own PTB without
    /// trusting a sequencer endpoint. Strict `now > deadline + margin`: the
    /// exact boundary is not a halt yet, mirroring the monitor's grace.
    ///
    /// The margin is 10 minutes where the monitor's is 1 second because the
    /// two close different gaps. The monitor re-reads the inbox to confirm
    /// a verdict against a settle that raced it; a PTB sees one snapshot
    /// and cannot re-read, so this gate pays in margin instead — long
    /// enough that an on-time inclusion has had its block settled and its
    /// entry pruned, which is the false positive the margin exists to
    /// prevent.
    public fun halt_expired_deadline(inbox: &Inbox, clock: &Clock): Option<u64> {
        let oldest = inbox::oldest_pending_deadline(inbox);
        if (!option::is_some(&oldest)) {
            return oldest
        };
        let deadline = option::destroy_some(oldest);
        if (clock.timestamp_ms() > deadline + HALT_MARGIN_MS) {
            option::some(deadline)
        } else {
            option::none()
        }
    }

    /// The recorded request for `address`, `none` when it never requested.
    public fun request_at(queue: &ExitQueue, address: vector<u8>): Option<ExitRequest> {
        if (table::contains(&queue.requests, address)) {
            option::some(*table::borrow(&queue.requests, address))
        } else {
            option::none()
        }
    }

    /// What redemption has already paid out against `address`'s request,
    /// `none` when it never requested. A user computing what to sign needs
    /// this: the signed amount must equal `balance - paid`.
    public fun paid_at(queue: &ExitQueue, address: vector<u8>): Option<u256> {
        if (table::contains(&queue.requests, address)) {
            option::some((*table::borrow(&queue.requests, address)).paid)
        } else {
            option::none()
        }
    }

    /// Emitted when a request is redeemed. The vault release itself emits
    /// `bridge::EscapeReleased` in the same PTB.
    public struct ExitRedeemed has copy, drop {
        /// The 20-byte L2 address whose balance was paid out.
        address: vector<u8>,
        /// Settled height the request was proven at.
        height: u64,
        /// Sequence the request holds.
        seq: u64,
        /// What the vault releases in this PTB.
        amount: u64,
        /// Where the released coin goes.
        recipient: address,
    }

    /// How many accounts hold a recorded request.
    public fun request_count(queue: &ExitQueue): u64 {
        table::length(&queue.requests)
    }

    /// Emitted when the valve demotes a head that never redeemed. The
    /// record stays: it can still redeem, behind everyone indexed.
    public struct ExitSkipped has copy, drop {
        /// The 20-byte L2 address that was head.
        address: vector<u8>,
        /// Settled height the request was proven at.
        height: u64,
        /// Sequence the request holds.
        seq: u64,
    }

    /// The ordered index's head — the payable record every redemption must
    /// serve first, `none` when no indexed record is pending. Records
    /// outside the index (zero-balance proofs, fully-paid records, and
    /// skipped ones) wait behind it.
    public fun queue_head(queue: &ExitQueue): Option<vector<u8>> {
        index_head_addr(queue)
    }

    /// How many records sit in the ordered index.
    public fun queue_size(queue: &ExitQueue): u64 {
        index_size(queue)
    }

    /// Whether the valve has demoted `address`'s record: skipped records
    /// redeem once the ordered index drains, not in their old position.
    public fun is_skipped(queue: &ExitQueue, address: vector<u8>): bool {
        dynamic_field::exists_with_type<SkipMarkKey, bool>(&queue.id, SkipMarkKey { address })
    }

    /// How long a head may hold the queue before [`skip_head`] may demote
    /// it.
    public fun skip_after_ms(): u64 {
        SKIP_AFTER_MS
    }

    /// The liveness valve. The head holds the queue only so long: once it
    /// has sat for [`SKIP_AFTER_MS`] without its owner redeeming, anyone
    /// can demote it — the head moves to its successor and the skipped
    /// record goes behind every indexed record. Strict order stays the
    /// rule for owners who show up; an absent one cannot hold everyone
    /// hostage past the window. The record keeps its request and its paid
    /// counter, and a skipped record never re-enters the index.
    public fun skip_head(queue: &mut ExitQueue, clock: &Clock) {
        let head = index_head_addr(queue);
        assert!(option::is_some(&head), E_QUEUE_EMPTY);
        let address = option::destroy_some(head);
        let node = *dynamic_field::borrow<QueueAddrKey, QueueNode>(&queue.id, QueueAddrKey { address });
        assert!(clock.timestamp_ms() > node.recorded_at_ms + SKIP_AFTER_MS, E_SKIP_EARLY);
        let record = *table::borrow(&queue.requests, address);
        index_unlink(queue, address);
        dynamic_field::add(&mut queue.id, SkipMarkKey { address }, true);
        event::emit(ExitSkipped {
            address,
            height: record.height,
            seq: record.seq,
        });
    }

    // ---------- index internals ----------

    /// Lexicographic `(height, seq)` ordering: is `(h1, s1)` at or before
    /// `(h2, s2)`?
    fun key_at_or_before(h1: u64, s1: u64, h2: u64, s2: u64): bool {
        h1 < h2 || (h1 == h2 && s1 <= s2)
    }

    /// The index head's address, `none` when the index is empty. The
    /// `QueueHeadKey` field's presence is the "not empty" state.
    fun index_head_addr(queue: &ExitQueue): Option<vector<u8>> {
        if (dynamic_field::exists_with_type<QueueHeadKey, vector<u8>>(&queue.id, QueueHeadKey {})) {
            option::some(*dynamic_field::borrow<QueueHeadKey, vector<u8>>(&queue.id, QueueHeadKey {}))
        } else {
            option::none()
        }
    }

    fun index_size(queue: &ExitQueue): u64 {
        if (dynamic_field::exists_with_type<QueueCountKey, u64>(&queue.id, QueueCountKey {})) {
            *dynamic_field::borrow<QueueCountKey, u64>(&queue.id, QueueCountKey {})
        } else {
            0
        }
    }

    fun index_set_size(queue: &mut ExitQueue, size: u64) {
        if (dynamic_field::exists_with_type<QueueCountKey, u64>(&queue.id, QueueCountKey {})) {
            *dynamic_field::borrow_mut<QueueCountKey, u64>(&mut queue.id, QueueCountKey {}) = size;
        } else {
            dynamic_field::add(&mut queue.id, QueueCountKey {}, size);
        };
    }

    /// The `next` link of `address`'s node. The node must exist: callers
    /// only walk links they were handed by the index itself.
    fun index_node_next(queue: &ExitQueue, address: vector<u8>): Option<vector<u8>> {
        let node = *dynamic_field::borrow<QueueAddrKey, QueueNode>(&queue.id, QueueAddrKey { address });
        node.next
    }

    /// Inserts `address`'s payable record into the ordered index at its
    /// `(height, seq)` position — the walk starts at the head and stops at
    /// the first node ordered after it, so a record that belongs at the
    /// front costs one hop and a record that belongs at the back costs the
    /// whole walk. The record must exist in `requests` and hold no node.
    fun index_insert(queue: &mut ExitQueue, address: vector<u8>, now_ms: u64) {
        let record = *table::borrow(&queue.requests, address);
        let mut prev: Option<vector<u8>> = option::none();
        let mut next = index_head_addr(queue);
        let mut walking = option::is_some(&next);
        while (walking) {
            let cur = option::extract(&mut next);
            let cur_record = *table::borrow(&queue.requests, cur);
            if (key_at_or_before(cur_record.height, cur_record.seq, record.height, record.seq)) {
                prev = option::some(cur);
                next = index_node_next(queue, cur);
                walking = option::is_some(&next);
            } else {
                // The successor; stop with `prev` as the predecessor.
                next = option::some(cur);
                walking = false;
            };
        };
        dynamic_field::add(&mut queue.id, QueueAddrKey { address }, QueueNode {
            prev: prev,
            next: next,
            recorded_at_ms: now_ms,
        });
        if (option::is_some(&prev)) {
            let p = *option::borrow(&prev);
            dynamic_field::borrow_mut<QueueAddrKey, QueueNode>(
                &mut queue.id,
                QueueAddrKey { address: p },
            ).next = option::some(address);
        } else if (dynamic_field::exists_with_type<QueueHeadKey, vector<u8>>(
            &queue.id,
            QueueHeadKey {},
        )) {
            *dynamic_field::borrow_mut<QueueHeadKey, vector<u8>>(&mut queue.id, QueueHeadKey {}) =
                address;
        } else {
            dynamic_field::add(&mut queue.id, QueueHeadKey {}, address);
        };
        if (option::is_some(&next)) {
            let n = *option::borrow(&next);
            dynamic_field::borrow_mut<QueueAddrKey, QueueNode>(
                &mut queue.id,
                QueueAddrKey { address: n },
            ).prev = option::some(address);
        };
        let size = index_size(queue) + 1;
        index_set_size(queue, size);
    }

    /// Removes `address`'s node from the ordered index, relinking its
    /// neighbours; an unlinked head hands the head to its successor, and
    /// the last record empties the index.
    fun index_unlink(queue: &mut ExitQueue, address: vector<u8>) {
        let QueueNode { prev, next, recorded_at_ms: _ } =
            dynamic_field::remove<QueueAddrKey, QueueNode>(&mut queue.id, QueueAddrKey { address });
        if (option::is_some(&prev)) {
            let p = *option::borrow(&prev);
            dynamic_field::borrow_mut<QueueAddrKey, QueueNode>(
                &mut queue.id,
                QueueAddrKey { address: p },
            ).next = next;
        } else if (option::is_some(&next)) {
            *dynamic_field::borrow_mut<QueueHeadKey, vector<u8>>(&mut queue.id, QueueHeadKey {}) =
                *option::borrow(&next);
        } else {
            // The index drained.
            dynamic_field::remove<QueueHeadKey, vector<u8>>(&mut queue.id, QueueHeadKey {});
        };
        if (option::is_some(&next)) {
            let n = *option::borrow(&next);
            dynamic_field::borrow_mut<QueueAddrKey, QueueNode>(
                &mut queue.id,
                QueueAddrKey { address: n },
            ).prev = prev;
        };
        let size = index_size(queue);
        index_set_size(queue, size - 1);
    }

    /// Records (or improves) an escape request — the escape entrypoint.
    ///
    /// Fires only in the inbox-derived halt state ([`halt_expired_deadline`]):
    /// a sequencer censoring withdrawals drives the inbox past its deadline,
    /// every settle aborts, and value leaves through proofs instead. The
    /// balance is proven with [`verify_account`] against the state root
    /// anchored at `height`, so the request carries only what settlement
    /// committed.
    ///
    /// The request is keyed by the 20-byte L2 address. A first proof claims
    /// `(height, seq)`. A later proof at an earlier height replaces the
    /// record and keeps the sequence; at the same height it re-records the
    /// same account (a trie is deterministic, so the values cannot differ);
    /// at a later height it is ignored. Queue position never regresses, and
    /// nobody can force it backward: proofs are public chain data, so anyone
    /// can produce a valid proof at any later height, and the rule makes
    /// that useless. The redeemed total ([`paid_at`]) is carried through
    /// replacements, so the cumulative payout never exceeds the balance
    /// proven at some settled height. Redemption serves the queue in
    /// `(height, seq)` order: [`redeem`] pays only the head, and the
    /// [`skip_head`] valve demotes a head that sits past
    /// [`skip_after_ms`] without its owner redeeming, so an earlier proof
    /// cannot be overtaken by a later one.
    public fun request_exit(
        queue: &mut ExitQueue,
        registry: &Registry,
        inbox: &Inbox,
        clock: &Clock,
        height: u64,
        address: vector<u8>,
        proof: vector<vector<u8>>,
    ) {
        let expired = halt_expired_deadline(inbox, clock);
        assert!(option::is_some(&expired), E_NOT_HALTED);
        let expired_deadline_ms = option::destroy_some(expired);
        let (nonce, balance) = verify_account(registry, height, address, proof);
        let now_ms = clock.timestamp_ms();

        if (table::contains(&queue.requests, address)) {
            let current = *table::borrow(&queue.requests, address);
            if (height <= current.height) {
                let improved = height < current.height;
                *table::borrow_mut(&mut queue.requests, address) = ExitRequest {
                    height,
                    nonce,
                    balance,
                    seq: current.seq,
                    paid: current.paid,
                };
                event::emit(ExitRequested {
                    address,
                    height,
                    seq: current.seq,
                    nonce,
                    balance,
                    expired_deadline_ms,
                });
                // Index bookkeeping. A strictly-earlier proof can move the
                // record's position and change what remains payable, so its
                // node unlinks and re-enters at the new position — with the
                // valve clock restarted, the one honest way a record that
                // moves forward buys itself a fresh window. A same-height
                // replay changes nothing (a trie is deterministic, so the
                // balance cannot differ), and its node — valve clock
                // included — stays untouched: nobody keeps a head fresh by
                // replaying public proofs.
                let indexed = dynamic_field::exists_with_type<QueueAddrKey, QueueNode>(
                    &queue.id,
                    QueueAddrKey { address },
                );
                if (improved && indexed) {
                    index_unlink(queue, address);
                };
                let updated = *table::borrow(&queue.requests, address);
                if (improved
                    && !is_skipped(queue, address)
                    && updated.balance > updated.paid) {
                    index_insert(queue, address, now_ms);
                };
            };
        } else {
            let seq = queue.next_seq;
            queue.next_seq = seq + 1;
            table::add(&mut queue.requests, address, ExitRequest {
                height,
                nonce,
                balance,
                seq,
                paid: 0,
            });
            event::emit(ExitRequested {
                address,
                height,
                seq,
                nonce,
                balance,
                expired_deadline_ms,
            });
            // Payable records enter the index. A zero-balance proof has
            // nothing to serve and would only ever gate the queue.
            if (balance > 0) {
                index_insert(queue, address, now_ms);
            };
        };
    }

    /// Redeems a recorded request: the owner signs the payout with their L2
    /// key and the vault releases what remains — no sequencer capability
    /// involved.
    ///
    /// The queue serves exits in `(height, seq)` order: an indexed record
    /// redeems only at the head, and a paid record hands the position to
    /// its successor. Records outside the index — skipped by the valve, or
    /// recorded before the index existed — wait until it drains.
    ///
    /// Fires only in the inbox-derived halt state (the same gate as
    /// [`request_exit`]). The payout is `balance - paid` from the recorded
    /// request, bound three ways before any value moves: the recovered
    /// signer's Ethereum address must equal the recorded address; the
    /// payload must name (domain, address, amount, recipient) with the
    /// amount equal to the state-derived remainder, so a replayed signature
    /// covers a past amount and fails; and the claim is a
    /// `bridge::ExitClaim` hot potato that only `bridge::escape_release`
    /// accepts, so the PTB cannot drop it or spend it twice.
    ///
    /// `payload` is the exact bytes the owner signed: 15-byte domain
    /// [`PAYLOAD_DOMAIN`] ‖ 20-byte address ‖ 32-byte big-endian amount ‖
    /// 32-byte recipient. `signature` is the 65-byte `(r, s, v)` EIP-191
    /// personal-signature value (v 27/28, or the bare recovery id 0/1).
    public fun redeem(
        queue: &mut ExitQueue,
        inbox: &Inbox,
        clock: &Clock,
        address: vector<u8>,
        recipient: address,
        payload: vector<u8>,
        signature: vector<u8>,
    ): bridge::ExitClaim {
        let expired = halt_expired_deadline(inbox, clock);
        assert!(option::is_some(&expired), E_NOT_HALTED);
        assert!(table::contains(&queue.requests, address), E_NO_REQUEST);
        let request = *table::borrow(&queue.requests, address);
        // Saturating remainder: an earlier-height proof can replace the
        // record with a smaller balance after a full payout, and u256
        // subtraction would wrap on that shape instead of saying "nothing".
        let remaining = if (request.balance > request.paid) {
            request.balance - request.paid
        } else {
            0
        };
        assert!(remaining > 0, E_NOTHING_TO_REDEEM);
        // The vault's coin amounts are u64; a balance beyond that cannot be
        // backed by deposits made through this bridge in the first place.
        assert!(remaining <= 0xFFFFFFFFFFFFFFFFu256, E_AMOUNT);
        // Ordered consumption. An indexed record redeems only at the head;
        // a record outside the index — skipped by the valve, or recorded
        // before the index existed — waits until the indexed queue drains.
        let indexed = dynamic_field::exists_with_type<QueueAddrKey, QueueNode>(
            &queue.id,
            QueueAddrKey { address },
        );
        if (indexed) {
            let head = index_head_addr(queue);
            assert!(option::is_some(&head) && *option::borrow(&head) == address, E_NOT_QUEUE_HEAD);
        } else {
            assert!(index_size(queue) == 0, E_NOT_QUEUE_HEAD);
        };
        verify_redemption_signature(&address, recipient, remaining, &payload, &signature);

        *table::borrow_mut(&mut queue.requests, address) = ExitRequest {
            height: request.height,
            nonce: request.nonce,
            balance: request.balance,
            seq: request.seq,
            paid: request.balance,
        };
        event::emit(ExitRedeemed {
            address,
            height: request.height,
            seq: request.seq,
            amount: (remaining as u64),
            recipient,
        });
        if (indexed) {
            // The served record leaves the index; its successor becomes the
            // head.
            index_unlink(queue, address);
        };
        bridge::escape_claim(address, (remaining as u64), recipient)
    }

    /// Recovers the signer from `signature` over the EIP-191 encoding of
    /// `payload` and asserts the bindings: the payload names the recorded
    /// address, `amount`, and `recipient`, and the recovered Ethereum
    /// address equals the recorded address.
    fun verify_redemption_signature(
        address: &vector<u8>,
        recipient: address,
        amount: u256,
        payload: &vector<u8>,
        signature: &vector<u8>,
    ) {
        assert!(vector::length(signature) == SIGNATURE_LEN, E_SIGNATURE);
        assert!(vector::length(payload) == PAYLOAD_LEN, E_PAYLOAD);
        assert!(range(payload, 0, 15) == PAYLOAD_DOMAIN, E_PAYLOAD);
        assert!(range(payload, 15, 35) == *address, E_PAYLOAD);
        assert!(be_u256(&range(payload, 35, 67)) == amount, E_PAYLOAD);
        assert!(range(payload, 67, 99) == address::to_bytes(recipient), E_PAYLOAD);
        let recovered = recover_eth_address(signature, &eip191_preimage(payload));
        assert!(recovered == *address, E_SIGNATURE);
    }

    /// The Ethereum address behind `signature`: normalize the v byte
    /// (Ethereum 27/28 → recovery id 0/1, both accepted by the framework's
    /// recoverable-signature native), recover the compressed public key from
    /// the keccak256 preimage, decompress, and take the last 20 bytes of
    /// `keccak256(X ‖ Y)`.
    fun recover_eth_address(signature: &vector<u8>, preimage: &vector<u8>): vector<u8> {
        let v = *vector::borrow(signature, 64);
        let normalized = if (v >= 27) {
            v - 27
        } else {
            v
        };
        assert!(normalized <= 3, E_SIGNATURE);
        let mut sig = *signature;
        *vector::borrow_mut(&mut sig, 64) = normalized;
        let compressed = ecdsa_k1::secp256k1_ecrecover(&sig, preimage, ECRECOVER_KECCAK256);
        let uncompressed = ecdsa_k1::decompress_pubkey(&compressed);
        // uncompressed is 0x04 ‖ X ‖ Y; the account digest covers X ‖ Y and
        // the address is its last 20 bytes.
        let digest = hash::keccak256(&range(&uncompressed, 1, 65));
        range(&digest, 12, 32)
    }

    /// The EIP-191 personal-sign preimage: `b"\x19Ethereum Signed Message:\n"`
    /// ‖ decimal ASCII byte length ‖ payload. Signing this with the L2 key
    /// is what wallets produce with `personal_sign`.
    fun eip191_preimage(payload: &vector<u8>): vector<u8> {
        let mut out: vector<u8> = b"\x19Ethereum Signed Message:\n";
        let mut digits: vector<u8> = vector[];
        let mut n = vector::length(payload);
        if (n == 0) {
            vector::push_back(&mut digits, 0x30);
        };
        while (n > 0) {
            vector::push_back(&mut digits, (0x30 + (n % 10) as u8));
            n = n / 10;
        };
        // The loop collects least-significant first; append reversed.
        let mut i = vector::length(&digits);
        while (i > 0) {
            i = i - 1;
            vector::push_back(&mut out, *vector::borrow(&digits, i));
        };
        vector::append(&mut out, *payload);
        out
    }

    // ---------- fixtures (generated by crates/sui/examples/mpt_fixture.rs) ----------
    //
    // Account proofs built with alloy-trie (reth's node encoder) over small
    // tries and validated against their own roots before printing. The tries:
    // SINGLE — one account (the proof is the root leaf itself); SHARED — two
    // accounts whose keccak keys share the first 10 nibbles (root extension
    // node); MULTI — nine accounts, the target's proof walking a root branch,
    // a sub-trie extension, a second branch, and the leaf; MULTI_ZERO — the
    // same trie, an account with zero balance (an empty RLP integer).

    #[test_only]
    const SINGLE_ROOT: vector<u8> = x"b53e33bb803cd92fc3e97978acfcce1f97ddbd35db6b7df13b6c24507b20a97b";
    #[test_only]
    const SINGLE_NODE_0: vector<u8> = x"f86ca12053e6633aae56ed85af8af8ecbe4d12c899024ad68952400c0c68f52503333f78b848f846018203e7a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470";
    #[test_only]
    const SINGLE_ADDRESS: vector<u8> = x"6f605da6fd75acb56dbc2461367fdc705972750b";
    // keccak(address): 53e6633aae56ed85af8af8ecbe4d12c899024ad68952400c0c68f52503333f78
    #[test_only]
    const SINGLE_NONCE: u64 = 1;
    #[test_only]
    const SINGLE_BALANCE: u256 = 999;

    #[test_only]
    const SHARED_ROOT: vector<u8> = x"3302d6f7f4b7f31f0baf25e35221f6ab074061dc4150bdbdd92ae8cf03546435";
    #[test_only]
    const SHARED_NODE_0: vector<u8> = x"e886002a0d25b2a2a06ce3dd313d9c4d603e13c28f606cf4ca983e0ee890a2d6cbb5a806d8b0b3c677";
    #[test_only]
    const SHARED_NODE_1: vector<u8> = x"f851808080808080a0fb4f34e96046cf2062d17933b171cb5031171c8646b2076f15c7150a459edba4a00dce7e864419d585d00ab11bf687bd91166b7dbefea440ddf9e4106db8f8cf97808080808080808080";
    #[test_only]
    const SHARED_NODE_2: vector<u8> = x"f86f9b3f42ed32df167f194ed701bd668bcaf825875a7ea999150ac27322b851f84f078b02b036da601a044b400000a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470";
    #[test_only]
    const SHARED_ADDRESS: vector<u8> = x"69b0f713f19e258f2640c0ae5a27dcbd3f5ad62b";
    // keccak(address): 2a0d25b2a26f42ed32df167f194ed701bd668bcaf825875a7ea999150ac27322
    #[test_only]
    const SHARED_NONCE: u64 = 7;
    #[test_only]
    const SHARED_BALANCE: u256 = 3250000000000000000000000;

    #[test_only]
    const MULTI_ROOT: vector<u8> = x"7a1223506c5f240fc25db791ef5c8c931be47cd36ba921e31874b6857bc91479";
    #[test_only]
    const MULTI_NODE_0: vector<u8> = x"f8d180a035e456994d4a80ed86a4021464b92ba6eaa85664b2637aede1a8e91f59ba103f80808080a0126a748c7b2e847c4fcef2f179b8c8afb6b3910dbf05b41fb5d1ed3a37168706a0424784c69941ea502d95261132b233688c037f409b1db06b4ccdb5a6115eddbc80a0c8f9ad752ed6fd9ca789902252f0796104a8d4c888afb9fb6bff9105b37e8830a074bc724d31614b1195c9b1096b0fa0e2f019479cd86727c03dff3559158d1c28a0728ada02556592164c72d71fc24ff11e52613810324e1ee70f3cb34254f965108080808080";
    #[test_only]
    const MULTI_NODE_1: vector<u8> = x"e58311d4fea0dd6c1098c60f0e8e6aaf2d48d3dc5eede31fa1d13ba0e249c275e4d7b0dd9ae0";
    #[test_only]
    const MULTI_NODE_2: vector<u8> = x"f851808080a0e36b0a64ade76f61094a25b70682d743b85e27d83aaa1fb7a97b6021b8d79f648080a03cb21269c1633ca9317d05236611bd44ec7bd2289cd8e97daa81110bdbff7a6c80808080808080808080";
    #[test_only]
    const MULTI_NODE_3: vector<u8> = x"f86e9d3db8e332c28ae2e79ea922524f2783d482b59742ea342d984d66a532e3b84ef84c0f8853444835ec580000a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470";
    #[test_only]
    const MULTI_ADDRESS: vector<u8> = x"ed6a1790227dfcf3ff9bbd2c4d3d1c2db5ba89b6";
    // keccak(address): a1d4fe6db8e332c28ae2e79ea922524f2783d482b59742ea342d984d66a532e3
    #[test_only]
    const MULTI_NONCE: u64 = 15;
    #[test_only]
    const MULTI_BALANCE: u256 = 6000000000000000000;

    #[test_only]
    const MULTI_ZERO_ROOT: vector<u8> = x"7a1223506c5f240fc25db791ef5c8c931be47cd36ba921e31874b6857bc91479";
    #[test_only]
    const MULTI_ZERO_NODE_0: vector<u8> = x"f8d180a035e456994d4a80ed86a4021464b92ba6eaa85664b2637aede1a8e91f59ba103f80808080a0126a748c7b2e847c4fcef2f179b8c8afb6b3910dbf05b41fb5d1ed3a37168706a0424784c69941ea502d95261132b233688c037f409b1db06b4ccdb5a6115eddbc80a0c8f9ad752ed6fd9ca789902252f0796104a8d4c888afb9fb6bff9105b37e8830a074bc724d31614b1195c9b1096b0fa0e2f019479cd86727c03dff3559158d1c28a0728ada02556592164c72d71fc24ff11e52613810324e1ee70f3cb34254f965108080808080";
    #[test_only]
    const MULTI_ZERO_NODE_1: vector<u8> = x"f869a0324bb16e7f105ba99ff6723cc45f3c6105c292c49d923d8e6c82526a088886e3b846f8440980a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470";
    #[test_only]
    const MULTI_ZERO_ADDRESS: vector<u8> = x"d6c7e785c2104a6dfa3740a352de63bc369880fe";
    // keccak(address): 724bb16e7f105ba99ff6723cc45f3c6105c292c49d923d8e6c82526a088886e3
    #[test_only]
    const MULTI_ZERO_NONCE: u64 = 9;
    #[test_only]
    const MULTI_ZERO_BALANCE: u256 = 0;

    #[test_only]
    const REPLACE_ROOT: vector<u8> = x"2bc7d9af39a6374cb0b2a78b66985fc87beff1a489a1fe6003019ec7c1a48a7f";
    #[test_only]
    const REPLACE_NODE_0: vector<u8> = x"f8518080808080a0ccaf1e2e06721a37f67ca9af139ae6d0c0fa17ec4e9f9dc9f27a52645c36e64ea05b1394afef92834f645432f2269a921f82380f42ce6d7badb73a8db60778f1da80808080808080808080";
    #[test_only]
    const REPLACE_NODE_1: vector<u8> = x"f86ba033e6633aae56ed85af8af8ecbe4d12c899024ad68952400c0c68f52503333f78b848f846028201f4a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470";
    // Same address as SINGLE: the REPLACE trie carries that account at a
    // later state, so one address has valid proofs at two anchored heights.
    #[test_only]
    const REPLACE_ADDRESS: vector<u8> = x"6f605da6fd75acb56dbc2461367fdc705972750b";
    #[test_only]
    const REPLACE_NONCE: u64 = 2;
    #[test_only]
    const REPLACE_BALANCE: u256 = 500;

    // ---------- tests ----------

    /// Anchors `root` at height 1 of a fresh single-height registry.
    #[test_only]
    public fun anchored_registry_for_testing(
        ctx: &mut TxContext,
        root: vector<u8>,
    ): (Registry, Inbox, Clock) {
        let (mut registry, inbox, clock) = settlement::settled_registry_for_testing(ctx, 1);
        settlement::commit_state_root(&mut registry, &clock, 1, root, ctx);
        (registry, inbox, clock)
    }

    /// Tears the anchored-registry helper down.
    #[test_only]
    public fun teardown(mut registry: Registry, inbox: Inbox, clock: Clock): u64 {
        settlement::destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        settlement::destroy_rotation_for_testing(&mut registry);
        settlement::destroy_registry(registry);
        0
    }

    /// Builds an unshared exit queue for tests.
    #[test_only]
    public fun queue_for_testing(ctx: &mut TxContext): ExitQueue {
        ExitQueue {
            id: object::new(ctx),
            requests: table::new(ctx),
            next_seq: 0,
        }
    }

    /// Destroys an exit queue; every recorded request's address must be
    /// named and is removed first (the table has no iteration), along with
    /// its ordered-index node and skip mark if it holds one.
    #[test_only]
    public fun teardown_queue(queue: ExitQueue, addresses: vector<vector<u8>>) {
        let ExitQueue { id: mut id, mut requests, next_seq: _ } = queue;
        let mut i: u64 = 0;
        let n = vector::length(&addresses);
        while (i < n) {
            let address = *vector::borrow(&addresses, i);
            if (table::contains(&requests, address)) {
                let ExitRequest { height: _, nonce: _, balance: _, seq: _, paid: _ } =
                    table::remove(&mut requests, address);
            };
            if (dynamic_field::exists_with_type<QueueAddrKey, QueueNode>(
                &id,
                QueueAddrKey { address },
            )) {
                let QueueNode { prev: _, next: _, recorded_at_ms: _ } =
                    dynamic_field::remove<QueueAddrKey, QueueNode>(&mut id, QueueAddrKey { address });
            };
            if (dynamic_field::exists_with_type<SkipMarkKey, bool>(&id, SkipMarkKey { address })) {
                dynamic_field::remove<SkipMarkKey, bool>(&mut id, SkipMarkKey { address });
            };
            i = i + 1;
        };
        if (dynamic_field::exists_with_type<QueueHeadKey, vector<u8>>(&id, QueueHeadKey {})) {
            dynamic_field::remove<QueueHeadKey, vector<u8>>(&mut id, QueueHeadKey {});
        };
        if (dynamic_field::exists_with_type<QueueCountKey, u64>(&id, QueueCountKey {})) {
            dynamic_field::remove<QueueCountKey, u64>(&mut id, QueueCountKey {});
        };
        table::destroy_empty(requests);
        id.delete();
    }

    /// Anchors `root_a` at height 1 and `root_b` at height 2 of a fresh
    /// two-height registry.
    #[test_only]
    public fun two_height_registry_for_testing(
        ctx: &mut TxContext,
        root_a: vector<u8>,
        root_b: vector<u8>,
    ): (Registry, Inbox, Clock) {
        let (mut registry, mut inbox, clock) = settlement::settled_registry_for_testing(ctx, 1);
        settlement::commit_state_root(&mut registry, &clock, 1, root_a, ctx);
        settlement::settle_as(&mut registry, &mut inbox, &clock, b"blob", 2, 2_000, 0, ctx);
        settlement::commit_state_root(&mut registry, &clock, 2, root_b, ctx);
        (registry, inbox, clock)
    }

    #[test]
    fun verifies_the_single_leaf_proof() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let proof = vector[SINGLE_NODE_0];
        let (nonce, balance) = verify_account(&registry, 1, SINGLE_ADDRESS, proof);
        assert!(nonce == SINGLE_NONCE, E_TEST);
        assert!(balance == SINGLE_BALANCE, E_TEST);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun verifies_the_proof_with_the_root_extension() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SHARED_ROOT);
        let proof = vector[SHARED_NODE_0, SHARED_NODE_1, SHARED_NODE_2];
        let (nonce, balance) = verify_account(&registry, 1, SHARED_ADDRESS, proof);
        assert!(nonce == SHARED_NONCE, E_TEST);
        assert!(balance == SHARED_BALANCE, E_TEST);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun verifies_the_deep_proof() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, MULTI_ROOT);
        let proof = vector[MULTI_NODE_0, MULTI_NODE_1, MULTI_NODE_2, MULTI_NODE_3];
        let (nonce, balance) = verify_account(&registry, 1, MULTI_ADDRESS, proof);
        assert!(nonce == MULTI_NONCE, E_TEST);
        assert!(balance == MULTI_BALANCE, E_TEST);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun verifies_a_zero_balance_account() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, MULTI_ZERO_ROOT);
        let proof = vector[MULTI_ZERO_NODE_0, MULTI_ZERO_NODE_1];
        let (nonce, balance) = verify_account(&registry, 1, MULTI_ZERO_ADDRESS, proof);
        assert!(nonce == MULTI_ZERO_NONCE, E_TEST);
        assert!(balance == MULTI_ZERO_BALANCE, E_TEST);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_NO_ANCHOR, location = Self)]
    fun rejects_a_height_with_no_anchored_root() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let proof = vector[SINGLE_NODE_0];
        verify_account(&registry, 2, SINGLE_ADDRESS, proof);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_EMPTY_PROOF, location = Self)]
    fun rejects_an_empty_proof() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        verify_account(&registry, 1, SINGLE_ADDRESS, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_ROOT_MISMATCH, location = Self)]
    fun rejects_a_proof_against_a_foreign_root() {
        let ctx = &mut tx_context::dummy();
        let other = hash::keccak256(&x"deadbeefcafe0123456789abcdef0123456789abcdef0123456789abcdef0123");
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, other);
        let proof = vector[SINGLE_NODE_0];
        verify_account(&registry, 1, SINGLE_ADDRESS, proof);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_ROOT_MISMATCH, location = Self)]
    fun rejects_a_tampered_root_node() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut tampered = SINGLE_NODE_0;
        let len = vector::length(&tampered);
        let b = *vector::borrow(&tampered, len - 1);
        *vector::borrow_mut(&mut tampered, len - 1) = b ^ 1;
        verify_account(&registry, 1, SINGLE_ADDRESS, vector[tampered]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_LINK, location = Self)]
    fun rejects_a_tampered_child_node() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, MULTI_ROOT);
        let mut tampered = MULTI_NODE_1;
        let len = vector::length(&tampered);
        let b = *vector::borrow(&tampered, len - 1);
        *vector::borrow_mut(&mut tampered, len - 1) = b ^ 1;
        let proof = vector[MULTI_NODE_0, tampered, MULTI_NODE_2, MULTI_NODE_3];
        verify_account(&registry, 1, MULTI_ADDRESS, proof);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_PATH, location = Self)]
    fun rejects_a_foreign_address() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let proof = vector[SINGLE_NODE_0];
        verify_account(&registry, 1, SHARED_ADDRESS, proof);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_PROOF_SHORT, location = Self)]
    fun rejects_a_truncated_proof() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, MULTI_ROOT);
        let proof = vector[MULTI_NODE_0, MULTI_NODE_1, MULTI_NODE_2];
        verify_account(&registry, 1, MULTI_ADDRESS, proof);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_PROOF_EXTRA, location = Self)]
    fun rejects_unconsumed_proof_nodes() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let proof = vector[SINGLE_NODE_0, MULTI_NODE_3];
        verify_account(&registry, 1, SINGLE_ADDRESS, proof);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_ADDRESS, location = Self)]
    fun rejects_a_wrong_length_address() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let short = vector[0x6f, 0x60, 0x5d, 0xa6, 0xfd, 0x75, 0xac, 0xb5, 0x6d, 0xbc,
            0x24, 0x61, 0x36, 0x7f, 0xdc, 0x70, 0x59, 0x72, 0x75];
        verify_account(&registry, 1, short, vector[SINGLE_NODE_0]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_PATH, location = Self)]
    fun rejects_a_dead_branch_slot() {
        let ctx = &mut tx_context::dummy();
        // A branch with every slot empty (d1 + 17 empty-string items):
        // whatever nibble the key starts with, the walk hits an empty slot.
        let dead = x"d18080808080808080808080808080808080808080";
        let root = hash::keccak256(&dead);
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, root);
        verify_account(&registry, 1, SINGLE_ADDRESS, vector[dead]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_NODE_SHAPE, location = Self)]
    fun rejects_a_root_that_is_not_a_trie_node() {
        let ctx = &mut tx_context::dummy();
        // A three-item RLP list is neither a branch, extension, nor leaf.
        let node = x"c3010203";
        let root = hash::keccak256(&node);
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, root);
        verify_account(&registry, 1, SINGLE_ADDRESS, vector[node]);
        teardown(registry, inbox, clock);
    }

    // ---------- RLP and hex-prefix unit tests ----------

    #[test]
    fun rlp_header_and_children_decode_known_vectors() {
        // "dog": short string
        let dog = x"83646f67";
        let (start, len, is_list) = header(&dog, 0);
        assert!(start == 1 && len == 3 && !is_list, E_TEST);
        assert!(string_content(&dog) == x"646f67", E_TEST);
        // a single byte below 0x80 is its own content
        let (s2, l2, is_list2) = header(&x"01", 0);
        assert!(s2 == 0 && l2 == 1 && !is_list2, E_TEST);
        assert!(string_content(&x"01") == x"01", E_TEST);
        // the empty string
        let (s3, l3, is_list3) = header(&x"80", 0);
        assert!(s3 == 1 && l3 == 0 && !is_list3, E_TEST);
        assert!(vector::is_empty(&string_content(&x"80")), E_TEST);
        // [cat, dog]: a list of two short strings
        let cats_dogs = x"c88363617483646f67";
        let (s4, l4, is_list4) = header(&cats_dogs, 0);
        assert!(s4 == 1 && l4 == 8 && is_list4, E_TEST);
        let items = list_children(&cats_dogs, s4, l4);
        assert!(vector::length(&items) == 2, E_TEST);
        assert!(*vector::borrow(&items, 0) == x"83636174", E_TEST);
        assert!(string_content(vector::borrow(&items, 1)) == x"646f67", E_TEST);
        // a long string uses a two-byte length prefix
        let mut long = vector[];
        vector::push_back(&mut long, 0xb9);
        vector::push_back(&mut long, 0x01);
        vector::push_back(&mut long, 0x00);
        let mut j: u64 = 0;
        while (j < 256) {
            vector::push_back(&mut long, 0xaa);
            j = j + 1;
        };
        let (s5, l5, is_list5) = header(&long, 0);
        assert!(s5 == 3 && l5 == 256 && !is_list5, E_TEST);
        assert!(vector::length(&string_content(&long)) == 256, E_TEST);
    }

    #[test, expected_failure(abort_code = E_RLP, location = Self)]
    fun rlp_rejects_a_truncated_length_prefix() {
        // 0xb8 promises one length byte; the input ends there.
        header(&x"b8", 0);
    }

    #[test, expected_failure(abort_code = E_RLP, location = Self)]
    fun rlp_rejects_content_shorter_than_its_declared_length() {
        // 0xb9 with two length bytes declaring 256 content bytes, none present.
        header(&x"b90100", 0);
    }

    #[test]
    fun decode_hp_known_vectors() {
        // even extension path from the SHARED fixture: 00 2a0d25b2a2
        let (is_leaf, nibbles) = decode_hp(&x"002a0d25b2a2");
        assert!(!is_leaf, E_TEST);
        let expected = vector[0x2, 0xa, 0x0, 0xd, 0x2, 0x5, 0xb, 0x2, 0xa, 0x2];
        assert!(nibbles == expected, E_TEST);
        // odd leaf path: 3d flags leaf+odd with first nibble d, then nibbles of 0x3d
        let (is_leaf2, nibbles2) = decode_hp(&x"3d3d");
        assert!(is_leaf2, E_TEST);
        let expected2 = vector[0xd, 0x3, 0xd];
        assert!(nibbles2 == expected2, E_TEST);
    }

    #[test, expected_failure(abort_code = E_NODE_SHAPE, location = Self)]
    fun decode_hp_rejects_an_empty_path() {
        decode_hp(&x"");
    }

    #[test]
    fun big_endian_integers_decode() {
        assert!(be_u64(&x"", 8) == 0, E_TEST);
        assert!(be_u64(&x"03e7", 8) == 999, E_TEST);
        assert!(be_u64(&x"0102030405060708", 8) == 72623859790382856, E_TEST);
        assert!(be_u64(&x"ffffffffffffffff", 8) == 18446744073709551615, E_TEST);
        assert!(be_u256(&x"ff") == 255, E_TEST);
        assert!(
            be_u256(&x"ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff")
                == 0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffu256,
            E_TEST,
        );
    }

    #[test, expected_failure(abort_code = E_ACCOUNT, location = Self)]
    fun be_u64_rejects_more_than_eight_bytes() {
        be_u64(&x"010203040506070809", 8);
    }

    #[test, expected_failure(abort_code = E_ACCOUNT, location = Self)]
    fun decode_account_rejects_a_non_list_value() {
        decode_account(&x"80");
    }

    #[test, expected_failure(abort_code = E_ACCOUNT, location = Self)]
    fun decode_account_rejects_a_wrong_item_count() {
        decode_account(&x"c20102");
    }

    // ---------- the unlock and the escape request ----------
    //
    // The escape path opens only in the inbox-derived halt state: the oldest
    // pending forced transaction past its deadline plus the halt margin. The
    // fixtures above carry the balances; the inbox carries the halt.

    #[test]
    fun halt_verdict_is_none_for_an_empty_inbox() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let queue = queue_for_testing(ctx);

        assert!(option::is_none(&halt_expired_deadline(&inbox, &clock)), E_TEST);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun halt_verdict_is_none_while_the_deadline_holds() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let queue = queue_for_testing(ctx);

        // Submitted at clock 0: the deadline is 600_000 and the margin ends
        // at 1_200_000. One millisecond before that, the chain is not halted.
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_199_999);

        assert!(option::is_none(&halt_expired_deadline(&inbox, &clock)), E_TEST);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun halt_verdict_stays_none_at_the_exact_margin_boundary() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_000);

        assert!(option::is_none(&halt_expired_deadline(&inbox, &clock)), E_TEST);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun halt_verdict_fires_strictly_after_the_margin() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        let verdict = halt_expired_deadline(&inbox, &clock);
        assert!(option::is_some(&verdict), E_TEST);
        // The verdict carries the expired deadline, not the clock reading.
        assert!(*option::borrow(&verdict) == 600_000, E_TEST);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun inclusion_clears_the_halt_verdict() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);
        assert!(option::is_some(&halt_expired_deadline(&inbox, &clock)), E_TEST);

        // A block stamped exactly at the deadline includes the transaction;
        // settling it prunes the entry, and the verdict clears even though
        // the wall clock is far past the margin.
        inbox::enforce_settle(&mut inbox, 600_000, 1);
        assert!(option::is_none(&halt_expired_deadline(&inbox, &clock)), E_TEST);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun halt_margin_matches_the_documented_value() {
        assert!(halt_margin_ms() == 600_000, E_TEST);
    }

    #[test]
    fun request_exit_records_the_proven_balance() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        let request = request_at(&queue, SINGLE_ADDRESS);
        assert!(option::is_some(&request), E_TEST);
        let recorded = *option::borrow(&request);
        assert!(recorded.height == 1, E_TEST);
        assert!(recorded.nonce == SINGLE_NONCE, E_TEST);
        assert!(recorded.balance == SINGLE_BALANCE, E_TEST);
        assert!(recorded.seq == 0, E_TEST);
        assert!(request_count(&queue) == 1, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_NOT_HALTED, location = Self)]
    fun request_exit_refuses_a_healthy_chain() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_NOT_HALTED, location = Self)]
    fun request_exit_refuses_before_the_margin_elapses() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_000);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_EMPTY_PROOF, location = Self)]
    fun request_exit_requires_a_provable_proof() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(&mut queue, &registry, &inbox, &clock, 1, SINGLE_ADDRESS, vector[]);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun reproof_at_the_same_height_is_idempotent() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        let request = request_at(&queue, SINGLE_ADDRESS);
        assert!(option::is_some(&request), E_TEST);
        let recorded = *option::borrow(&request);
        assert!(recorded.seq == 0, E_TEST);
        assert!(request_count(&queue) == 1, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun later_height_never_regresses_the_claim() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, REPLACE_ROOT);
        let mut queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        // A valid proof at a later height — the shape an attacker with public
        // chain data can always produce — must not push the request back.
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            REPLACE_ADDRESS,
            vector[REPLACE_NODE_0, REPLACE_NODE_1],
        );

        let request = request_at(&queue, REPLACE_ADDRESS);
        assert!(option::is_some(&request), E_TEST);
        let recorded = *option::borrow(&request);
        assert!(recorded.height == 1, E_TEST);
        assert!(recorded.nonce == SINGLE_NONCE, E_TEST);
        assert!(recorded.balance == SINGLE_BALANCE, E_TEST);
        assert!(recorded.seq == 0, E_TEST);
        assert!(request_count(&queue) == 1, E_TEST);

        teardown_queue(queue, vector[REPLACE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun earlier_height_replaces_the_request_keeping_the_sequence() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, REPLACE_ROOT);
        let mut queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            REPLACE_ADDRESS,
            vector[REPLACE_NODE_0, REPLACE_NODE_1],
        );
        let first = request_at(&queue, REPLACE_ADDRESS);
        assert!(option::is_some(&first), E_TEST);
        let claimed = *option::borrow(&first);
        assert!(claimed.height == 2, E_TEST);
        assert!(claimed.nonce == REPLACE_NONCE, E_TEST);
        assert!(claimed.balance == REPLACE_BALANCE, E_TEST);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        let request = request_at(&queue, REPLACE_ADDRESS);
        assert!(option::is_some(&request), E_TEST);
        let recorded = *option::borrow(&request);
        assert!(recorded.height == 1, E_TEST);
        assert!(recorded.nonce == SINGLE_NONCE, E_TEST);
        assert!(recorded.balance == SINGLE_BALANCE, E_TEST);
        // The earlier claim keeps the sequence it already held.
        assert!(recorded.seq == 0, E_TEST);
        assert!(request_count(&queue) == 1, E_TEST);

        teardown_queue(queue, vector[REPLACE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun second_address_claims_the_next_sequence() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, MULTI_ROOT);
        let mut queue = queue_for_testing(ctx);

        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            MULTI_ADDRESS,
            vector[MULTI_NODE_0, MULTI_NODE_1, MULTI_NODE_2, MULTI_NODE_3],
        );

        let second = request_at(&queue, MULTI_ADDRESS);
        assert!(option::is_some(&second), E_TEST);
        assert!((*option::borrow(&second)).seq == 1, E_TEST);
        assert!(request_count(&queue) == 2, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS, MULTI_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun request_at_is_none_for_an_unknown_address() {
        let ctx = &mut tx_context::dummy();
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let queue = queue_for_testing(ctx);

        assert!(option::is_none(&request_at(&queue, SINGLE_ADDRESS)), E_TEST);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    // ---------- redemption ----------
    //
    // Redeeming consumes a recorded request: the owner signs the payout
    // (EIP-191 over domain ‖ address ‖ amount ‖ recipient) with their L2
    // key, the recovered address must equal the recorded one, and the
    // signed amount must equal the state-derived remainder. The returned
    // claim releases the vault's coins without any sequencer capability.
    // The signer fixtures use the framework's test-only Secp256k1 natives,
    // so the addresses derive from the seeds at test time and no external
    // fixture generation is involved.

    #[test_only]
    const SEED_A: vector<u8> = x"416c696365206578697420736565642030303030303030303030303030303030";
    #[test_only]
    const SEED_B: vector<u8> = x"426f622065786974207365656420303030303030303030303030303030303030";
    #[test_only]
    const SEED_C: vector<u8> = x"4361726f6c206578697420736565642030303030303030303030303030303030";
    #[test_only]
    const VAULT_RECIPIENT: vector<u8> = x"1111111111111111111111111111111111111111";
    #[test_only]
    const SUI_RECIPIENT: address = @0xC0FFEE;
    #[test_only]
    const REDEEM_AMOUNT: u256 = 200;

    #[test_only]
    const SECP256K1_N: u256 = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;

    /// Funds a fresh vault with `amount` of `T`.
    #[test_only]
    public fun funded_vault_for_testing<T>(ctx: &mut TxContext, amount: u64): Vault {
        let mut vault = bridge::vault_for_testing(ctx);
        let coin = coin::mint_for_testing<T>(amount, ctx);
        bridge::deposit<T>(&mut vault, coin, VAULT_RECIPIENT, ctx);
        vault
    }

    /// Tears a funded vault down: drops the `T` reserve (a test coin has
    /// `drop`) and destroys the now-empty structures.
    #[test_only]
    public fun teardown_vault_for_testing<T>(vault: Vault) {
        let mut v = vault;
        bridge::drain_for_testing<T>(&mut v);
        bridge::destroy_vault_for_testing(v);
    }

    /// Records a synthetic request (the redeem path consumes records; the
    /// proof machinery that fills them is covered by the request tests).
    #[test_only]
    fun record_for_testing(queue: &mut ExitQueue, address: vector<u8>, balance: u256) {
        let seq = queue.next_seq;
        queue.next_seq = seq + 1;
        table::add(&mut queue.requests, address, ExitRequest {
            height: 1,
            nonce: 0,
            balance,
            seq,
            paid: 0,
        });
    }

    /// Marks part of a request as already redeemed, standing in for a prior
    /// redeem whose full flow needs a vault.
    #[test_only]
    fun mark_paid_for_testing(queue: &mut ExitQueue, address: vector<u8>, paid: u256) {
        let request = *table::borrow(&queue.requests, address);
        *table::borrow_mut(&mut queue.requests, address) = ExitRequest {
            height: request.height,
            nonce: request.nonce,
            balance: request.balance,
            seq: request.seq,
            paid,
        };
    }

    /// The Ethereum address of the key generated from `seed`.
    #[test_only]
    fun eth_address_for_testing(seed: vector<u8>): vector<u8> {
        let kp = ecdsa_k1::secp256k1_keypair_from_seed(&seed);
        let uncompressed = ecdsa_k1::decompress_pubkey(ecdsa_k1::public_key(&kp));
        let digest = hash::keccak256(&range(&uncompressed, 1, 65));
        range(&digest, 12, 32)
    }

    /// Signs the EIP-191 encoding of `payload` with the key from `seed` and
    /// returns the 65-byte `(r, s, v)` signature.
    #[test_only]
    fun eth_sign_for_testing(seed: vector<u8>, payload: &vector<u8>): vector<u8> {
        let kp = ecdsa_k1::secp256k1_keypair_from_seed(&seed);
        ecdsa_k1::secp256k1_sign(
            ecdsa_k1::private_key(&kp),
            &eip191_preimage(payload),
            ECRECOVER_KECCAK256,
            true,
        )
    }

    /// The canonical redemption payload: domain ‖ address ‖ amount ‖
    /// recipient.
    #[test_only]
    fun payload_for_testing(addr: &vector<u8>, amount: u256, recipient: address): vector<u8> {
        let mut p = PAYLOAD_DOMAIN;
        vector::append(&mut p, *addr);
        vector::append(&mut p, u256_to_32be(amount));
        vector::append(&mut p, address::to_bytes(recipient));
        p
    }

    /// `amount` as 32 big-endian bytes.
    #[test_only]
    fun u256_to_32be(amount: u256): vector<u8> {
        let mut out: vector<u8> = vector[];
        let mut rest = amount;
        let mut j: u64 = 0;
        while (j < 32) {
            vector::push_back(&mut out, (rest % 256) as u8);
            rest = rest / 256;
            j = j + 1;
        };
        vector::reverse(&mut out);
        out
    }

    /// The sibling signature: `s ← n − s` with the recovery id flipped,
    /// which recovers the same key (the standard recid identity). Pins that
    /// both recovery paths bind to the same owner.
    #[test_only]
    fun flipped_recovery_for_testing(sig: &vector<u8>): vector<u8> {
        let s = be_u256(&range(sig, 32, 64));
        let v = *vector::borrow(sig, 64);
        let mut out = range(sig, 0, 32);
        vector::append(&mut out, u256_to_32be(SECP256K1_N - s));
        vector::push_back(&mut out, if (v == 0) 1 else 0);
        out
    }

    /// A halted chain with a synthetic request for `address` holding
    /// `balance` — the redeem path consumes records, and the request path's
    /// proof machinery is already covered above.
    #[test_only]
    fun halted_queue_with_record_for_testing(
        ctx: &mut TxContext,
        address: vector<u8>,
        balance: u256,
    ): (Registry, Inbox, Clock, ExitQueue) {
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);
        let mut queue = queue_for_testing(ctx);
        let seq = queue.next_seq;
        queue.next_seq = seq + 1;
        table::add(&mut queue.requests, address, ExitRequest {
            height: 1,
            nonce: 0,
            balance,
            seq,
            paid: 0,
        });
        (registry, inbox, clock, queue)
    }

    #[test_only]
    fun teardown_with_queue(
        registry: Registry,
        inbox: Inbox,
        clock: Clock,
        queue: ExitQueue,
        addresses: vector<vector<u8>>,
    ) {
        teardown_queue(queue, addresses);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun redeem_pays_the_remaining_balance_to_the_signed_recipient() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        let claim = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);

        assert!(bridge::reserve<bridge::TestCoin>(&vault) == 300, E_TEST);
        assert!(request_count(&queue) == 1, E_TEST);
        let paid = paid_at(&queue, owner);
        assert!(option::is_some(&paid), E_TEST);
        assert!(option::destroy_some(paid) == REDEEM_AMOUNT, E_TEST);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test]
    fun redeem_accepts_the_sibling_recovery_id() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = flipped_recovery_for_testing(&eth_sign_for_testing(SEED_A, &payload));
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        let claim = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);

        assert!(bridge::reserve<bridge::TestCoin>(&vault) == 300, E_TEST);
        let paid = paid_at(&queue, owner);
        assert!(option::destroy_some(paid) == REDEEM_AMOUNT, E_TEST);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test]
    fun redeem_accepts_ethereum_encoded_v() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let mut sig = eth_sign_for_testing(SEED_A, &payload);
        let v = *vector::borrow(&sig, 64);
        *vector::borrow_mut(&mut sig, 64) = v + 27;
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        let claim = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);

        assert!(bridge::reserve<bridge::TestCoin>(&vault) == 300, E_TEST);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_NOT_HALTED, location = Self)]
    fun redeem_refuses_a_healthy_chain() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        record_for_testing(&mut queue, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_NO_REQUEST, location = Self)]
    fun redeem_refuses_an_unrequested_address() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[]);
    }

    #[test, expected_failure(abort_code = E_NOTHING_TO_REDEEM, location = Self)]
    fun second_redeem_aborts_after_the_full_balance_is_paid() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        let claim = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);
        // The same signature replayed: the remainder is now zero, so the
        // replay aborts before the signature is even checked.
        let claim2 = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim2, ctx);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_SIGNATURE, location = Self)]
    fun redeem_refuses_a_foreign_signer() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let attacker = eth_address_for_testing(SEED_B);
        // The payload binds the recorded address; the attacker's key signs
        // it, so the recovered address is the attacker's, not the owner's.
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_B, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        assert!(attacker != owner, E_TEST);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_PAYLOAD, location = Self)]
    fun redeem_refuses_a_recipient_not_bound_by_the_signature() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let other_recipient = @0xDEAD;
        // Signed for SUI_RECIPIENT, redeemed to someone else: the payload
        // no longer matches the call argument.
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, other_recipient, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_PAYLOAD, location = Self)]
    fun redeem_refuses_an_amount_not_matching_the_record() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        // Signed for 999, recorded balance 200: the state-derived remainder
        // is what redemption pays, and the signature must cover exactly it.
        let payload = payload_for_testing(&owner, 999, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure]
    fun redeem_refuses_a_tampered_signature() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let mut sig = eth_sign_for_testing(SEED_A, &payload);
        *vector::borrow_mut(&mut sig, 5) = *vector::borrow(&sig, 5) ^ 1;
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_SIGNATURE, location = Self)]
    fun redeem_refuses_a_short_signature() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let mut short = range(&sig, 0, 64);
        vector::push_back(&mut short, 0);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, short);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_SIGNATURE, location = Self)]
    fun redeem_refuses_an_unrecoverable_v() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let mut sig = eth_sign_for_testing(SEED_A, &payload);
        // EIP-155-style v (chain-id encoded) is not an EIP-191 personal
        // signature: after the 27-shift it is not a recovery id.
        *vector::borrow_mut(&mut sig, 64) = 35;
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_PAYLOAD, location = Self)]
    fun redeem_refuses_a_wrong_length_payload() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let mut payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        vector::pop_back(&mut payload);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = E_PAYLOAD, location = Self)]
    fun redeem_refuses_a_foreign_domain() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        // The payload is well-formed otherwise, so this isolates the
        // domain binding.
        let mut payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        *vector::borrow_mut(&mut payload, 0) = 0x78;
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test, expected_failure(abort_code = 1, location = podseq::bridge)]
    fun escape_release_refuses_when_the_vault_lacks_liquidity() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        // An empty vault: the claim mints, the release aborts, and the
        // whole PTB (including the paid advance) rolls back.
        let mut vault = bridge::vault_for_testing(ctx);

        let claim = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test]
    fun escape_release_leaves_the_withdraw_nonce_untouched() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        let claim = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);

        // Escape releases have no L2 nonce: they must not consume the
        // sequencer's withdrawal sequence or its idempotency markers.
        assert!(bridge::withdraw_nonce(&vault) == 0, E_TEST);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner]);
    }

    #[test]
    fun payload_and_preimage_have_the_documented_shapes() {
        let owner = x"0102030405060708090a0b0c0d0e0f1011121314";
        let payload = payload_for_testing(&owner, 1234, SUI_RECIPIENT);
        assert!(vector::length(&payload) == 99, E_TEST);
        assert!(range(&payload, 0, 15) == PAYLOAD_DOMAIN, E_TEST);
        assert!(range(&payload, 15, 35) == owner, E_TEST);
        let mut expected_amount: vector<u8> = vector[];
        let mut i = 30;
        while (i > 0) {
            i = i - 1;
            vector::push_back(&mut expected_amount, 0);
        };
        vector::push_back(&mut expected_amount, 0x04);
        vector::push_back(&mut expected_amount, 0xd2);
        assert!(range(&payload, 35, 67) == expected_amount, E_TEST);
        assert!(range(&payload, 67, 99) == address::to_bytes(SUI_RECIPIENT), E_TEST);

        // EIP-191: prefix ‖ decimal ASCII length ‖ payload.
        let preimage = eip191_preimage(&payload);
        assert!(range(&preimage, 0, 26) == b"\x19Ethereum Signed Message:\n", E_TEST);
        assert!(range(&preimage, 26, 28) == b"99", E_TEST);
        assert!(range(&preimage, 28, 127) == payload, E_TEST);

        // A 65-byte payload encodes its length as "65".
        let mut short: vector<u8> = vector[];
        let mut j: u64 = 0;
        while (j < 65) {
            vector::push_back(&mut short, 0xab);
            j = j + 1;
        };
        let preimage65 = eip191_preimage(&short);
        assert!(range(&preimage65, 26, 28) == b"65", E_TEST);
        assert!(vector::length(&preimage65) == 26 + 2 + 65, E_TEST);
    }

    #[test]
    fun reproof_preserves_the_redeemed_total() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        // Redeem 400 of the 999 proven balance, then re-prove at the same
        // height: the idempotent re-record must carry the redeemed total.
        mark_paid_for_testing(&mut queue, SINGLE_ADDRESS, 400);
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        let paid = paid_at(&queue, SINGLE_ADDRESS);
        assert!(option::destroy_some(paid) == 400, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun earlier_height_replace_keeps_the_redeemed_total() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, REPLACE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            REPLACE_ADDRESS,
            vector[REPLACE_NODE_0, REPLACE_NODE_1],
        );
        mark_paid_for_testing(&mut queue, REPLACE_ADDRESS, 400);
        // An earlier-height proof replaces the record: balance 999 at
        // height 1 now, redeemed total still 400, so 599 remains claimable.
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        let request = request_at(&queue, REPLACE_ADDRESS);
        assert!(option::is_some(&request), E_TEST);
        let recorded = *option::borrow(&request);
        assert!(recorded.balance == SINGLE_BALANCE, E_TEST);
        let paid = paid_at(&queue, REPLACE_ADDRESS);
        assert!(option::destroy_some(paid) == 400, E_TEST);

        teardown_queue(queue, vector[REPLACE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    // ---------- ordered redemption ----------
    //
    // The queue serves exits in `(height, seq)` order: the head first, the
    // valve demoting a head that never redeems. Index-state tests drive
    // `request_exit` with real proofs; ordered-redeem tests record
    // synthetic requests for the signer seeds (the redeem fixtures'
    // existing pattern) and index them directly, because the recorded
    // address must equal the recovered signer and the trie fixtures pin
    // fixed addresses.

    /// Anchors `root_a` at height 1, `root_b` at height 2, and `root_c` at
    /// height 3 of a fresh three-height registry.
    #[test_only]
    public fun three_height_registry_for_testing(
        ctx: &mut TxContext,
        root_a: vector<u8>,
        root_b: vector<u8>,
        root_c: vector<u8>,
    ): (Registry, Inbox, Clock) {
        let (mut registry, mut inbox, clock) = settlement::settled_registry_for_testing(ctx, 1);
        settlement::commit_state_root(&mut registry, &clock, 1, root_a, ctx);
        settlement::settle_as(&mut registry, &mut inbox, &clock, b"blob", 2, 2_000, 0, ctx);
        settlement::commit_state_root(&mut registry, &clock, 2, root_b, ctx);
        settlement::settle_as(&mut registry, &mut inbox, &clock, b"blob", 3, 3_000, 0, ctx);
        settlement::commit_state_root(&mut registry, &clock, 3, root_c, ctx);
        (registry, inbox, clock)
    }

    /// Records a synthetic request at `height` (the redeem fixtures'
    /// `record_for_testing` pins height 1).
    #[test_only]
    fun recorded_request_for_testing(
        queue: &mut ExitQueue,
        address: vector<u8>,
        height: u64,
        balance: u256,
    ) {
        let seq = queue.next_seq;
        queue.next_seq = seq + 1;
        table::add(&mut queue.requests, address, ExitRequest {
            height,
            nonce: 0,
            balance,
            seq,
            paid: 0,
        });
    }

    /// Inserts an index node for a synthetic record — standing in for the
    /// proof path that normally feeds the index.
    #[test_only]
    fun index_for_testing(queue: &mut ExitQueue, address: vector<u8>, now_ms: u64) {
        index_insert(queue, address, now_ms)
    }

    #[test]
    fun skip_after_ms_matches_the_documented_value() {
        assert!(skip_after_ms() == 600_000, E_TEST);
    }

    #[test]
    fun first_ordered_claim_becomes_the_queue_head() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        let head = queue_head(&queue);
        assert!(option::is_some(&head), E_TEST);
        assert!(*option::borrow(&head) == SINGLE_ADDRESS, E_TEST);
        assert!(queue_size(&queue) == 1, E_TEST);
        assert!(!is_skipped(&queue, SINGLE_ADDRESS), E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun later_height_claim_queues_behind_the_head() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, MULTI_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            MULTI_ADDRESS,
            vector[MULTI_NODE_0, MULTI_NODE_1, MULTI_NODE_2, MULTI_NODE_3],
        );

        let head = queue_head(&queue);
        assert!(option::is_some(&head), E_TEST);
        assert!(*option::borrow(&head) == SINGLE_ADDRESS, E_TEST);
        assert!(queue_size(&queue) == 2, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS, MULTI_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun earlier_proof_takes_the_head_regardless_of_arrival() {
        // MULTI claims at height 2 first; SINGLE's height-1 proof arrives
        // second with a later sequence and must still pass it: the order
        // is the proof's (height, seq), not the arrival order.
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, MULTI_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            MULTI_ADDRESS,
            vector[MULTI_NODE_0, MULTI_NODE_1, MULTI_NODE_2, MULTI_NODE_3],
        );
        let head = queue_head(&queue);
        assert!(*option::borrow(&head) == MULTI_ADDRESS, E_TEST);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        let head = queue_head(&queue);
        assert!(option::is_some(&head), E_TEST);
        assert!(*option::borrow(&head) == SINGLE_ADDRESS, E_TEST);
        assert!(queue_size(&queue) == 2, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS, MULTI_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun zero_balance_claim_stays_out_of_the_index() {
        // MULTI_ZERO shares MULTI's trie (same anchored root) with balance
        // zero: a real proof, nothing payable, and no place in the payout
        // order — a zero-balance head could never be served, so it must
        // never gate the queue.
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, MULTI_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            MULTI_ADDRESS,
            vector[MULTI_NODE_0, MULTI_NODE_1, MULTI_NODE_2, MULTI_NODE_3],
        );
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            MULTI_ZERO_ADDRESS,
            vector[MULTI_ZERO_NODE_0, MULTI_ZERO_NODE_1],
        );

        let head = queue_head(&queue);
        assert!(option::is_some(&head), E_TEST);
        assert!(*option::borrow(&head) == MULTI_ADDRESS, E_TEST);
        assert!(queue_size(&queue) == 1, E_TEST);
        assert!(request_count(&queue) == 2, E_TEST);

        teardown_queue(queue, vector[MULTI_ADDRESS, MULTI_ZERO_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun regressing_proof_never_touches_the_index() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, REPLACE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        // The later REPLACE proof is the attacker shape: a valid proof at a
        // later height for the same address. The record does not move and
        // neither does the index.
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            REPLACE_ADDRESS,
            vector[REPLACE_NODE_0, REPLACE_NODE_1],
        );

        assert!(queue_size(&queue) == 1, E_TEST);
        let head = queue_head(&queue);
        assert!(*option::borrow(&head) == SINGLE_ADDRESS, E_TEST);
        let recorded = *option::borrow(&request_at(&queue, SINGLE_ADDRESS));
        assert!(recorded.height == 1, E_TEST);
        assert!(recorded.balance == SINGLE_BALANCE, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun same_height_claims_order_by_sequence() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        let owner_a = eth_address_for_testing(SEED_A);
        let owner_b = eth_address_for_testing(SEED_B);
        recorded_request_for_testing(&mut queue, owner_a, 1, 100);
        recorded_request_for_testing(&mut queue, owner_b, 1, 100);
        index_for_testing(&mut queue, owner_a, 1_000_000);
        index_for_testing(&mut queue, owner_b, 1_000_000);

        let head = queue_head(&queue);
        assert!(*option::borrow(&head) == owner_a, E_TEST);

        // Insertion order must not matter within a height: the sequence
        // decides, so indexing B first leaves A at the head.
        let mut queue_b = queue_for_testing(ctx);
        recorded_request_for_testing(&mut queue_b, owner_a, 1, 100);
        recorded_request_for_testing(&mut queue_b, owner_b, 1, 100);
        index_for_testing(&mut queue_b, owner_b, 1_000_000);
        index_for_testing(&mut queue_b, owner_a, 1_000_000);
        let head_b = queue_head(&queue_b);
        assert!(*option::borrow(&head_b) == owner_a, E_TEST);

        teardown_queue(queue_b, vector[owner_a, owner_b]);
        teardown_queue(queue, vector[owner_a, owner_b]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun skip_walks_the_whole_order_middle_to_last() {
        // A(h1), C(h3), B(h2) — arrival builds a middle insert, and the
        // skip sequence must walk A, B, C: proof height order, not
        // insertion order.
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        let owner_a = eth_address_for_testing(SEED_A);
        let owner_b = eth_address_for_testing(SEED_B);
        let owner_c = eth_address_for_testing(SEED_C);
        recorded_request_for_testing(&mut queue, owner_a, 1, 100);
        recorded_request_for_testing(&mut queue, owner_c, 3, 100);
        recorded_request_for_testing(&mut queue, owner_b, 2, 100);
        index_for_testing(&mut queue, owner_a, 1_000_000);
        index_for_testing(&mut queue, owner_c, 1_000_000);
        index_for_testing(&mut queue, owner_b, 1_000_000);

        clock::set_for_testing(&mut clock, 1_600_001);
        skip_head(&mut queue, &clock);
        let head = queue_head(&queue);
        assert!(*option::borrow(&head) == owner_b, E_TEST);
        skip_head(&mut queue, &clock);
        let head = queue_head(&queue);
        assert!(*option::borrow(&head) == owner_c, E_TEST);
        assert!(queue_size(&queue) == 1, E_TEST);
        skip_head(&mut queue, &clock);
        assert!(option::is_none(&queue_head(&queue)), E_TEST);
        assert!(queue_size(&queue) == 0, E_TEST);
        assert!(is_skipped(&queue, owner_a), E_TEST);
        assert!(is_skipped(&queue, owner_b), E_TEST);
        assert!(is_skipped(&queue, owner_c), E_TEST);

        teardown_queue(queue, vector[owner_a, owner_b, owner_c]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_QUEUE_EMPTY, location = Self)]
    fun skip_refuses_an_empty_queue() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_600_001);

        skip_head(&mut queue, &clock);

        teardown_queue(queue, vector[]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_SKIP_EARLY, location = Self)]
    fun skip_refuses_at_the_exact_valve_boundary() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        // The record entered at 1_200_001; the strict boundary is one
        // millisecond later, mirroring the halt gates' shape.
        clock::set_for_testing(&mut clock, 1_800_001);
        skip_head(&mut queue, &clock);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun skip_demotes_the_head_after_the_valve_window() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        clock::set_for_testing(&mut clock, 1_800_002);
        skip_head(&mut queue, &clock);

        assert!(option::is_none(&queue_head(&queue)), E_TEST);
        assert!(queue_size(&queue) == 0, E_TEST);
        assert!(is_skipped(&queue, SINGLE_ADDRESS), E_TEST);
        // The record survives the demotion with its request intact.
        let recorded = *option::borrow(&request_at(&queue, SINGLE_ADDRESS));
        assert!(recorded.balance == SINGLE_BALANCE, E_TEST);
        assert!(request_count(&queue) == 1, E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test]
    fun same_height_reproof_does_not_reset_the_valve_clock() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) = anchored_registry_for_testing(ctx, SINGLE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        // A same-height replay at 1_300_000 must not restart the head's
        // clock: at 1_800_002 the valve still fires because the record
        // entered at 1_200_001.
        clock::set_for_testing(&mut clock, 1_300_000);
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        clock::set_for_testing(&mut clock, 1_800_002);
        skip_head(&mut queue, &clock);
        assert!(option::is_none(&queue_head(&queue)), E_TEST);

        teardown_queue(queue, vector[SINGLE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_SKIP_EARLY, location = Self)]
    fun improve_resets_the_valve_clock() {
        // REPLACE claims at height 2, then improves to height 1: the
        // record re-enters the index and its valve clock restarts. The
        // skip below runs past the ORIGINAL entry's window (1_200_001 +
        // 600_000) but only 550_000 ms after the improvement — it must
        // refuse, which pins the reset.
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, REPLACE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            REPLACE_ADDRESS,
            vector[REPLACE_NODE_0, REPLACE_NODE_1],
        );
        clock::set_for_testing(&mut clock, 1_300_000);
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        // Past the ORIGINAL entry's window (1_200_001 + 600_000), but only
        // 550_000 ms after the improvement. The valve must refuse: the
        // improvement restarted the clock.
        clock::set_for_testing(&mut clock, 1_850_000);
        skip_head(&mut queue, &clock);

        teardown_queue(queue, vector[REPLACE_ADDRESS]);
        teardown(registry, inbox, clock);
    }

    #[test, expected_failure(abort_code = E_NOT_QUEUE_HEAD, location = Self)]
    fun ordered_redeem_refuses_a_later_position() {
        let ctx = &mut tx_context::dummy();
        let owner_a = eth_address_for_testing(SEED_A);
        let owner_b = eth_address_for_testing(SEED_B);
        let payload_b = payload_for_testing(&owner_b, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig_b = eth_sign_for_testing(SEED_B, &payload_b);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner_a, REDEEM_AMOUNT);
        recorded_request_for_testing(&mut queue, owner_b, 2, REDEEM_AMOUNT);
        index_for_testing(&mut queue, owner_a, 1_000_000);
        index_for_testing(&mut queue, owner_b, 1_000_000);

        // B holds the later position: its signature is valid, but its turn
        // is not — A's record stands ahead of it in proof order.
        let claim_sink = redeem(&mut queue, &inbox, &clock, owner_b, SUI_RECIPIENT, payload_b, sig_b);
        bridge::drop_claim_for_testing(claim_sink);

        teardown_with_queue(registry, inbox, clock, queue, vector[owner_a, owner_b]);
    }

    #[test]
    fun redeem_advances_the_head_to_the_next_record() {
        let ctx = &mut tx_context::dummy();
        let owner_a = eth_address_for_testing(SEED_A);
        let owner_b = eth_address_for_testing(SEED_B);
        let payload_a = payload_for_testing(&owner_a, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig_a = eth_sign_for_testing(SEED_A, &payload_a);
        let payload_b = payload_for_testing(&owner_b, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig_b = eth_sign_for_testing(SEED_B, &payload_b);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner_a, REDEEM_AMOUNT);
        recorded_request_for_testing(&mut queue, owner_b, 2, REDEEM_AMOUNT);
        index_for_testing(&mut queue, owner_a, 1_000_000);
        index_for_testing(&mut queue, owner_b, 1_000_000);
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        let claim = redeem(&mut queue, &inbox, &clock, owner_a, SUI_RECIPIENT, payload_a, sig_a);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);
        let head = queue_head(&queue);
        assert!(option::is_some(&head), E_TEST);
        assert!(*option::borrow(&head) == owner_b, E_TEST);
        assert!(queue_size(&queue) == 1, E_TEST);

        let claim = redeem(&mut queue, &inbox, &clock, owner_b, SUI_RECIPIENT, payload_b, sig_b);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);
        assert!(option::is_none(&queue_head(&queue)), E_TEST);
        assert!(queue_size(&queue) == 0, E_TEST);
        // Both payouts landed, in order.
        assert!(bridge::reserve<bridge::TestCoin>(&vault) == 100, E_TEST);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner_a, owner_b]);
    }

    #[test, expected_failure(abort_code = E_NOT_QUEUE_HEAD, location = Self)]
    fun skipped_record_cannot_jump_the_drained_queue() {
        let ctx = &mut tx_context::dummy();
        let owner_a = eth_address_for_testing(SEED_A);
        let owner_b = eth_address_for_testing(SEED_B);
        let payload_a = payload_for_testing(&owner_a, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig_a = eth_sign_for_testing(SEED_A, &payload_a);
        let (registry, inbox, mut clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner_a, REDEEM_AMOUNT);
        recorded_request_for_testing(&mut queue, owner_b, 2, REDEEM_AMOUNT);
        index_for_testing(&mut queue, owner_a, 1_000_000);
        index_for_testing(&mut queue, owner_b, 1_000_000);

        // A never redeems, so the valve demotes it — and a demoted record
        // still cannot jump the queue: B is indexed ahead of it, and the
        // skip-marked record waits for the whole index to drain (the
        // sibling test walks it through to the payout).
        clock::set_for_testing(&mut clock, 1_600_001);
        skip_head(&mut queue, &clock);
        let head = queue_head(&queue);
        assert!(*option::borrow(&head) == owner_b, E_TEST);
        let claim_sink = redeem(&mut queue, &inbox, &clock, owner_a, SUI_RECIPIENT, payload_a, sig_a);
        bridge::drop_claim_for_testing(claim_sink);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner_a, owner_b]);
    }

    #[test]
    fun skipped_record_redeems_once_the_queue_drains() {
        let ctx = &mut tx_context::dummy();
        let owner_a = eth_address_for_testing(SEED_A);
        let owner_b = eth_address_for_testing(SEED_B);
        let payload_a = payload_for_testing(&owner_a, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig_a = eth_sign_for_testing(SEED_A, &payload_a);
        let payload_b = payload_for_testing(&owner_b, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig_b = eth_sign_for_testing(SEED_B, &payload_b);
        let (registry, inbox, mut clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner_a, REDEEM_AMOUNT);
        recorded_request_for_testing(&mut queue, owner_b, 2, REDEEM_AMOUNT);
        index_for_testing(&mut queue, owner_a, 1_000_000);
        index_for_testing(&mut queue, owner_b, 1_000_000);
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        // The valve demotes A; B redeems in its turn; the drained queue
        // then lets the skipped record redeem behind everyone.
        clock::set_for_testing(&mut clock, 1_600_001);
        skip_head(&mut queue, &clock);
        let claim = redeem(&mut queue, &inbox, &clock, owner_b, SUI_RECIPIENT, payload_b, sig_b);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);
        assert!(option::is_none(&queue_head(&queue)), E_TEST);

        let claim = redeem(&mut queue, &inbox, &clock, owner_a, SUI_RECIPIENT, payload_a, sig_a);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);
        assert!(bridge::reserve<bridge::TestCoin>(&vault) == 100, E_TEST);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner_a, owner_b]);
    }

    #[test, expected_failure(abort_code = E_NOT_QUEUE_HEAD, location = Self)]
    fun legacy_record_waits_for_the_ordered_queue() {
        // A queue holding pre-index records (the deployment shape before
        // this index existed) plus one ordered claim: the legacy record
        // redeems after the ordered queue drains, never before it.
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        let claim_sink = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::drop_claim_for_testing(claim_sink);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner, SINGLE_ADDRESS]);
    }

    #[test]
    fun legacy_record_redeems_once_the_ordered_queue_drains() {
        let ctx = &mut tx_context::dummy();
        let owner = eth_address_for_testing(SEED_A);
        let payload = payload_for_testing(&owner, REDEEM_AMOUNT, SUI_RECIPIENT);
        let sig = eth_sign_for_testing(SEED_A, &payload);
        let (registry, inbox, mut clock, mut queue) =
            halted_queue_with_record_for_testing(ctx, owner, REDEEM_AMOUNT);
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );
        let mut vault = funded_vault_for_testing<bridge::TestCoin>(ctx, 500);

        // The valve empties the ordered queue; the legacy record then
        // redeems first-come among the leftovers.
        clock::set_for_testing(&mut clock, 1_800_002);
        skip_head(&mut queue, &clock);
        assert!(option::is_none(&queue_head(&queue)), E_TEST);
        assert!(queue_size(&queue) == 0, E_TEST);

        let claim = redeem(&mut queue, &inbox, &clock, owner, SUI_RECIPIENT, payload, sig);
        bridge::escape_release<bridge::TestCoin>(&mut vault, claim, ctx);
        assert!(bridge::reserve<bridge::TestCoin>(&vault) == 300, E_TEST);

        teardown_vault_for_testing<bridge::TestCoin>(vault);
        teardown_with_queue(registry, inbox, clock, queue, vector[owner, SINGLE_ADDRESS]);
    }

    #[test]
    fun skipped_claim_never_reenters_on_an_improvement() {
        let ctx = &mut tx_context::dummy();
        let (registry, mut inbox, mut clock) =
            two_height_registry_for_testing(ctx, SINGLE_ROOT, REPLACE_ROOT);
        let mut queue = queue_for_testing(ctx);
        inbox::submit(&mut inbox, &clock, b"forced");
        clock::set_for_testing(&mut clock, 1_200_001);

        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            2,
            REPLACE_ADDRESS,
            vector[REPLACE_NODE_0, REPLACE_NODE_1],
        );
        clock::set_for_testing(&mut clock, 1_800_002);
        skip_head(&mut queue, &clock);
        assert!(is_skipped(&queue, REPLACE_ADDRESS), E_TEST);

        // The improvement makes the record payable again (999 proven over
        // the height-1 state), but a demoted record stays behind the
        // whole queue: the index must stay empty.
        clock::set_for_testing(&mut clock, 1_900_000);
        request_exit(
            &mut queue,
            &registry,
            &inbox,
            &clock,
            1,
            SINGLE_ADDRESS,
            vector[SINGLE_NODE_0],
        );

        assert!(option::is_none(&queue_head(&queue)), E_TEST);
        assert!(queue_size(&queue) == 0, E_TEST);
        assert!(is_skipped(&queue, REPLACE_ADDRESS), E_TEST);

        teardown_queue(queue, vector[REPLACE_ADDRESS]);
        teardown(registry, inbox, clock);
    }
}
