//! Exit queue: verify L2 balance proofs against settled state roots.
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
//! See `docs/src/exit-queue.md` for the design and the rejected
//! alternatives (sequencer attestations, ZK transitions, challenge windows).
module podseq::exit_queue {
    use std::option::Self;
    use std::vector;
    use sui::hash;
    use podseq::settlement::{Self, Registry};
    #[test_only]
    use podseq::inbox::Inbox;
    #[test_only]
    use sui::clock::{Self, Clock};

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

    /// Longest RLP length-of-length prefix accepted, matching Ethereum's 8.
    const MAX_LEN_BYTES: u64 = 8;

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
}
