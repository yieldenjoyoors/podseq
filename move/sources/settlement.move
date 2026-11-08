/// Podseq settlement contract.
///
/// Anchors L2 block commitments on Sui so that any full node can verify every block
/// has data available on Walrus. The sequencer calls `settle` after posting each block
/// to Walrus; the contract records `(height, blob_id)` in an append-only registry.
module podseq::settlement {
    use std::string::{Self, String};
    use std::option;
    use podseq::inbox::{Self, Inbox};
    use podseq::rotation;
    use sui::dynamic_field as df;
    use sui::object::{Self, UID};
    use sui::package;
    use sui::package::{UpgradeCap, UpgradeReceipt, UpgradeTicket};
    use sui::transfer;
    use sui::tx_context::{Self, TxContext};
    use sui::table::{Self, Table};
    use sui::vec_map::{Self, VecMap};
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

    /// Emitted when a settled block's state root is anchored on chain. This is
    /// the proof anchor for balance proofs: light clients verify an L2 balance
    /// against the committed root without executing any block.
    public struct StateRootCommitted has copy, drop {
        height: u64,
        state_root: vector<u8>,
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
    /// state with the sender as authority.
    ///
    /// Signature frozen by the published package; superseded by
    /// [`entomb_settler_cap_ops`], which also mints the Registry's `OpsCap`.
    /// The body delegates to the tombstoned `rotation::entomb` and aborts —
    /// entombment without an `OpsCap` would create a deployment nobody can
    /// steer.
    public fun entomb_settler_cap(cap: SettlerCap, registry: &mut Registry, ctx: &TxContext) {
        rotation::entomb(&mut registry.id, cap, ctx)
    }

    /// Entombs the SettlerCap inside the Registry, initializes rotation
    /// state with the sender as authority, and mints the Registry's `OpsCap`
    /// to the sender. The migration step after a package upgrade; ideally in
    /// the same programmable transaction block as the upgrade, signed by the
    /// current cap owner. Move the minted `OpsCap` to cold custody before
    /// going live.
    public fun entomb_settler_cap_ops(
        cap: SettlerCap,
        registry: &mut Registry,
        ctx: &mut TxContext,
    ) {
        rotation::entomb_ops(&mut registry.id, cap, ctx)
    }

    /// Pre-authorizes `to` to claim the settler authority after `delay_ms`.
    ///
    /// Signature frozen by the published package; the body delegates to the
    /// tombstoned `rotation::request_rotation` and aborts — steering
    /// requires the Registry's `OpsCap` via [`request_rotation_ops`].
    public fun request_rotation(
        registry: &mut Registry,
        clock: &sui::clock::Clock,
        to: address,
        delay_ms: u64,
        ctx: &TxContext,
    ) {
        rotation::request_rotation(&mut registry.id, to, delay_ms, clock, ctx)
    }

    /// Pre-authorizes `to` to claim the settler authority after `delay_ms`,
    /// authorized by holding the Registry's `OpsCap` — the steering
    /// authority — rather than by being the rotation authority. Aborts with
    /// `rotation::E_OPS_MISMATCH` for a cap minted for another object.
    public fun request_rotation_ops(
        registry: &mut Registry,
        clock: &sui::clock::Clock,
        to: address,
        delay_ms: u64,
        ops: &rotation::OpsCap,
    ) {
        rotation::request_rotation_ops(&mut registry.id, to, delay_ms, clock, ops)
    }

    /// Cancels the pending settler rotation. Authority only.
    public fun cancel_rotation(registry: &mut Registry, ctx: &TxContext) {
        rotation::cancel_rotation(&mut registry.id, ctx)
    }

    /// Sets the force-inclusion inclusion window: a narrowing applies
    /// immediately, a widening stages behind a timelock (see
    /// `inbox::stage_window_change`).
    ///
    /// Authorized by holding the Registry's `OpsCap` — the steering
    /// authority, held in cold custody — so the hot sequencer key can settle
    /// but cannot move the window. Aborts with `rotation::E_OPS_MISMATCH`
    /// for a cap minted for another object.
    public fun request_window_change(
        registry: &mut Registry,
        inbox: &mut Inbox,
        clock: &sui::clock::Clock,
        new_window_ms: u64,
        delay_ms: u64,
        ops: &rotation::OpsCap,
    ) {
        rotation::assert_ops_cap(&registry.id, ops);
        inbox::stage_window_change(inbox, new_window_ms, delay_ms, clock)
    }

    /// Cancels a staged window widening (see [`request_window_change`]).
    public fun cancel_window_change(
        registry: &mut Registry,
        inbox: &mut Inbox,
        ops: &rotation::OpsCap,
    ) {
        rotation::assert_ops_cap(&registry.id, ops);
        inbox::cancel_window_change(inbox)
    }

    /// Anchors the state root of the settled block at `height`.
    ///
    /// Called in the same programmable transaction block as [`settle_as`],
    /// immediately after it: the root names the settled tip, so the pair is
    /// atomic (both land or neither does). The same sender check as
    /// [`settle_as`] applies — the rotation authority, or the previous
    /// authority inside its grace window.
    ///
    /// The anchored root is the proof anchor for balance proofs (exit queue):
    /// a light client verifies an L2 balance against the root committed at the
    /// height whose state included it, without executing blocks or trusting
    /// the sequencer.
    ///
    /// Re-committing the identical root is a no-op success (crash-safe retry
    /// of the composed PTB); a different value aborts — the header carrying
    /// the root is signed and committed in the blob, so a mismatch is an
    /// attributable equivocation, mirroring [`E_CONFLICTING_BLOB`].
    public fun commit_state_root(
        registry: &mut Registry,
        clock: &sui::clock::Clock,
        height: u64,
        state_root: vector<u8>,
        ctx: &TxContext,
    ) {
        rotation::assert_can_act(&registry.id, clock, ctx);
        assert!(state_root.length() == STATE_ROOT_LEN, E_STATE_ROOT_MALFORMED);
        assert!(height == registry.latest_height, E_STATE_ROOT_HEIGHT);
        if (df::exists<StateRootKey>(&registry.id, StateRootKey { height })) {
            assert!(
                *df::borrow<StateRootKey, vector<u8>>(&registry.id, StateRootKey { height })
                    == state_root,
                E_STATE_ROOT_CONFLICT,
            );
            return
        };
        df::add(&mut registry.id, StateRootKey { height }, state_root);
        event::emit(StateRootCommitted { height, state_root })
    }

    /// Returns [`STATE_ROOT_LEN`], the required root length in bytes.
    public fun state_root_len(): u64 {
        STATE_ROOT_LEN
    }

    /// Returns the [`StateRootKey`] for `height`, for indexers and test
    /// teardown to name the dynamic-field entry; production reads go through
    /// [`state_root`].
    public fun state_root_key(height: u64): StateRootKey {
        StateRootKey { height }
    }

    /// The state root anchored for `height`, `none` when the block settled
    /// before state-root anchoring was adopted (or the height never settled).
    public fun state_root(registry: &Registry, height: u64): Option<vector<u8>> {
        if (df::exists<StateRootKey>(&registry.id, StateRootKey { height })) {
            option::some(*df::borrow<StateRootKey, vector<u8>>(&registry.id, StateRootKey { height }))
        } else {
            option::none()
        }
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

    /// A peer identity must be exactly `PEER_KEY_BYTES` long.
    const E_INVALID_PEER_KEY: u64 = 3;
    /// An advertised dial address must be non-empty and bounded.
    const E_INVALID_PEER_ADDR: u64 = 4;
    /// The peer book does not contain the identity.
    const E_PEER_MISSING: u64 = 5;
    /// The committee peer book is full.
    const E_PEER_FULL: u64 = 6;

    /// Length of an ed25519 p2p identity in bytes.
    const PEER_KEY_BYTES: u64 = 32;
    /// Upper bound for an advertised dial address in bytes.
    const MAX_PEER_ADDR_BYTES: u64 = 256;
    /// Committee peer cap; publish is authority-gated, the bound keeps the
    /// book and its BCS snapshot small.
    const MAX_PEERS: u64 = 100;

    /// Dynamic-field key under which the [`PeerBook`] lives on a Registry.
    public struct PeerKey has copy, drop, store {}

    /// Published p2p endpoints of the settlement committee: ed25519 p2p
    /// identity → dialable address, plus a mutation counter.
    ///
    /// Held as a dynamic field because the `Registry` struct layout is
    /// frozen at package publish; existing deployments adopt the book
    /// lazily on the first publish. Full nodes read its BCS over the Sui
    /// ledger API and admit exactly these peers — discovery from chain
    /// data, no operator-side bootstrap configuration.
    ///
    /// `version` increments on every mutation and is the peer-set index
    /// nodes register under, so every node derives identical tracked-set
    /// content for a given index from chain state alone (the discovery
    /// gossip kills peers whose set content disagrees per index).
    public struct PeerBook has store {
        version: u64,
        peers: VecMap<vector<u8>, String>,
    }

    /// Emitted when a committee peer is published or its address updated.
    public struct PeerPublished has copy, drop {
        pubkey: vector<u8>,
        addr: String,
    }

    /// Emitted when a committee peer is removed.
    public struct PeerRemoved has copy, drop {
        pubkey: vector<u8>,
    }

    /// Publishes (or updates) the committee peer for `pubkey`.
    ///
    /// Authorized like [`settle_as`]: the rotation authority, or the
    /// previous authority inside its grace window, so a cap rotation can
    /// repoint the committee address atomically with the key swap.
    public fun publish_peer(
        registry: &mut Registry,
        clock: &sui::clock::Clock,
        pubkey: vector<u8>,
        addr: String,
        ctx: &TxContext,
    ) {
        rotation::assert_can_act(&registry.id, clock, ctx);
        assert!(vector::length(&pubkey) == PEER_KEY_BYTES, E_INVALID_PEER_KEY);
        let addr_len = string::length(&addr);
        assert!(addr_len > 0 && addr_len <= MAX_PEER_ADDR_BYTES, E_INVALID_PEER_ADDR);

        let key_copy = pubkey;
        let addr_copy = addr;
        let book = peer_book_mut(registry);
        book.version = book.version + 1;
        if (vec_map::contains(&book.peers, &pubkey)) {
            *vec_map::get_mut(&mut book.peers, &pubkey) = addr;
        } else {
            assert!(vec_map::length(&book.peers) < MAX_PEERS, E_PEER_FULL);
            vec_map::insert(&mut book.peers, pubkey, addr);
        };
        event::emit(PeerPublished { pubkey: key_copy, addr: addr_copy });
    }

    /// Removes a committee peer. Aborts when the identity is absent.
    public fun remove_peer(
        registry: &mut Registry,
        clock: &sui::clock::Clock,
        pubkey: vector<u8>,
        ctx: &TxContext,
    ) {
        rotation::assert_can_act(&registry.id, clock, ctx);
        assert!(vector::length(&pubkey) == PEER_KEY_BYTES, E_INVALID_PEER_KEY);
        assert!(
            df::exists_with_type<PeerKey, PeerBook>(&registry.id, PeerKey {}),
            E_PEER_MISSING,
        );
        let book = df::borrow_mut<PeerKey, PeerBook>(&mut registry.id, PeerKey {});
        assert!(vec_map::contains(&book.peers, &pubkey), E_PEER_MISSING);
        book.version = book.version + 1;
        let (key, _addr) = vec_map::remove(&mut book.peers, &pubkey);
        event::emit(PeerRemoved { pubkey: key });
    }

    /// Number of published committee peers (0 before the first publish).
    public fun peer_count(registry: &Registry): u64 {
        if (!df::exists_with_type<PeerKey, PeerBook>(&registry.id, PeerKey {})) return 0;
        let book = df::borrow<PeerKey, PeerBook>(&registry.id, PeerKey {});
        vec_map::length(&book.peers)
    }

    /// Whether `pubkey` is in the peer book.
    public fun has_peer(registry: &Registry, pubkey: &vector<u8>): bool {
        if (!df::exists_with_type<PeerKey, PeerBook>(&registry.id, PeerKey {})) return false;
        let book = df::borrow<PeerKey, PeerBook>(&registry.id, PeerKey {});
        vec_map::contains(&book.peers, pubkey)
    }

    /// The advertised dial address for `pubkey`.
    public fun peer_addr(registry: &Registry, pubkey: &vector<u8>): String {
        assert!(vector::length(pubkey) == PEER_KEY_BYTES, E_INVALID_PEER_KEY);
        assert!(
            df::exists_with_type<PeerKey, PeerBook>(&registry.id, PeerKey {}),
            E_PEER_MISSING,
        );
        let book = df::borrow<PeerKey, PeerBook>(&registry.id, PeerKey {});
        *vec_map::get(&book.peers, pubkey)
    }

    /// The book's mutation counter (the peer-set index nodes register
    /// under); 0 before the first publish.
    public fun peer_book_version(registry: &Registry): u64 {
        if (!df::exists_with_type<PeerKey, PeerBook>(&registry.id, PeerKey {})) return 0;
        let book = df::borrow<PeerKey, PeerBook>(&registry.id, PeerKey {});
        book.version
    }

    /// Borrow the peer book, creating it on first use so pre-existing
    /// `Registry` objects adopt the book lazily (frozen struct layout).
    fun peer_book_mut(registry: &mut Registry): &mut PeerBook {
        if (!df::exists_with_type<PeerKey, PeerBook>(&registry.id, PeerKey {})) {
            df::add(&mut registry.id, PeerKey {}, PeerBook { version: 0, peers: vec_map::empty() });
        };
        df::borrow_mut<PeerKey, PeerBook>(&mut registry.id, PeerKey {})
    }

    // ---------- package upgrade custody ----------

    /// No `UpgradeCap` is entombed — never entombed, or already frozen.
    const E_NO_UPGRADE_CAP: u64 = 7;
    /// An `UpgradeCap` is already entombed.
    const E_UPGRADE_CAP_PRESENT: u64 = 8;
    /// A requested upgrade policy is weaker than the cap's current one.
    const E_TOO_PERMISSIVE: u64 = 9;
    /// State root is not [`STATE_ROOT_LEN`] bytes.
    const E_STATE_ROOT_MALFORMED: u64 = 10;
    /// Height is not the settled tip (commit_state_root anchors the tip only).
    const E_STATE_ROOT_HEIGHT: u64 = 11;
    /// A different root was already anchored for this height: equivocation.
    const E_STATE_ROOT_CONFLICT: u64 = 12;

    /// Length of an L2 state root in bytes (a 32-byte trie root).
    const STATE_ROOT_LEN: u64 = 32;

    /// Dynamic-field key: the state root anchored for `height`.
    public struct StateRootKey has copy, drop, store {
        height: u64,
    }

    /// Dynamic-field key under which the package's `UpgradeCap` is entombed.
    public struct UpgradeKey has copy, drop, store {}

    /// Returns the `UpgradeKey` singleton for indexers and tests.
    public fun upgrade_key(): UpgradeKey {
        UpgradeKey {}
    }

    /// Emitted when the package's `UpgradeCap` is entombed.
    public struct UpgradeCapEntombed has copy, drop {
        package: object::ID,
    }

    /// Emitted when the upgrade policy is tightened.
    public struct UpgradePolicyRestricted has copy, drop {
        package: object::ID,
        policy: u8,
    }

    /// Emitted when the package is made permanently immutable.
    public struct PackageFrozen has copy, drop {
        package: object::ID,
    }

    /// Entombs the package's `UpgradeCap` inside the `Registry`.
    ///
    /// The `UpgradeCap` is the true backstop — an upgrade can repoint every
    /// gate in this package — so it lives under the steering custody class:
    /// gated by the Registry's `OpsCap`, the same capability that steers
    /// rotation. From this transaction on, raw-cap upgrade ceremonies are
    /// impossible: the only authorization path is
    /// [`authorize_package_upgrade`]. Entombment is one way (no un-entomb).
    ///
    /// The Registry's rotation state must already exist (the settlement
    /// entombment seeds it), and the cap must match this package — verify
    /// `package::upgrade_package(&cap)` against the deployed package id
    /// before calling. A foreign cap only locks its own package out of
    /// upgrades, but the slot never opens again.
    public fun entomb_upgrade_cap(cap: UpgradeCap, registry: &mut Registry, ops: &rotation::OpsCap) {
        rotation::assert_ops_cap(&registry.id, ops);
        // Fail closed unless the steering infra (rotation state) is in place.
        let _ = rotation::state(&registry.id);
        assert!(
            !df::exists_with_type<UpgradeKey, UpgradeCap>(&registry.id, UpgradeKey {}),
            E_UPGRADE_CAP_PRESENT,
        );
        let pkg = package::upgrade_package(&cap);
        df::add(&mut registry.id, UpgradeKey {}, cap);
        event::emit(UpgradeCapEntombed { package: pkg });
    }

    /// Issues an upgrade ticket from the entombed `UpgradeCap`.
    ///
    /// `OpsCap`-gated. The ticket is a hot potato: it must be consumed by
    /// the `Upgrade` transaction command in the same programmable transaction
    /// block and finalized through [`commit_package_upgrade`], or the whole
    /// block reverts — an upgrade is atomic by construction. The requested
    /// `policy` must be at least as restrictive as the cap's; the framework
    /// re-checks both policy and bytecode digest at execution.
    public fun authorize_package_upgrade(
        registry: &mut Registry,
        ops: &rotation::OpsCap,
        policy: u8,
        digest: vector<u8>,
    ): UpgradeTicket {
        rotation::assert_ops_cap(&registry.id, ops);
        assert!(
            df::exists_with_type<UpgradeKey, UpgradeCap>(&registry.id, UpgradeKey {}),
            E_NO_UPGRADE_CAP,
        );
        package::authorize_upgrade(
            df::borrow_mut<UpgradeKey, UpgradeCap>(&mut registry.id, UpgradeKey {}),
            policy,
            digest,
        )
    }

    /// Consumes an upgrade receipt, finalizing the upgrade on the entombed
    /// cap. `OpsCap`-gated; only reachable in the same transaction block as
    /// [`authorize_package_upgrade`] (the receipt is a hot potato).
    public fun commit_package_upgrade(
        registry: &mut Registry,
        ops: &rotation::OpsCap,
        receipt: UpgradeReceipt,
    ) {
        rotation::assert_ops_cap(&registry.id, ops);
        assert!(
            df::exists_with_type<UpgradeKey, UpgradeCap>(&registry.id, UpgradeKey {}),
            E_NO_UPGRADE_CAP,
        );
        package::commit_upgrade(
            df::borrow_mut<UpgradeKey, UpgradeCap>(&mut registry.id, UpgradeKey {}),
            receipt,
        );
    }

    /// Tightens the entombed cap's upgrade policy — the middle rung of the
    /// aging ladder between the deploy-time `COMPATIBLE` default and
    /// [`freeze_package`]. Only the two framework tiers are accepted, and
    /// the framework rejects any loosening.
    public fun restrict_package_upgrades(registry: &mut Registry, ops: &rotation::OpsCap, policy: u8) {
        rotation::assert_ops_cap(&registry.id, ops);
        assert!(
            df::exists_with_type<UpgradeKey, UpgradeCap>(&registry.id, UpgradeKey {}),
            E_NO_UPGRADE_CAP,
        );
        let cap = df::borrow_mut<UpgradeKey, UpgradeCap>(&mut registry.id, UpgradeKey {});
        let pkg = package::upgrade_package(cap);
        if (policy == package::additive_policy()) {
            package::only_additive_upgrades(cap);
        } else if (policy == package::dep_only_policy()) {
            package::only_dep_upgrades(cap);
        } else {
            abort E_TOO_PERMISSIVE
        };
        event::emit(UpgradePolicyRestricted { package: pkg, policy });
    }

    /// Destroys the entombed `UpgradeCap`, making the package permanently
    /// immutable. One way and irreversible: no upgrade of any kind is
    /// possible afterwards. The terminal rung of the aging ladder.
    public fun freeze_package(registry: &mut Registry, ops: &rotation::OpsCap) {
        rotation::assert_ops_cap(&registry.id, ops);
        assert!(
            df::exists_with_type<UpgradeKey, UpgradeCap>(&registry.id, UpgradeKey {}),
            E_NO_UPGRADE_CAP,
        );
        let cap = df::remove<UpgradeKey, UpgradeCap>(&mut registry.id, UpgradeKey {});
        let pkg = package::upgrade_package(&cap);
        package::make_immutable(cap);
        event::emit(PackageFrozen { package: pkg });
    }

    /// Returns the entombed cap's package id, if custody was taken.
    public fun upgrade_package_id(registry: &Registry): option::Option<object::ID> {
        if (df::exists_with_type<UpgradeKey, UpgradeCap>(&registry.id, UpgradeKey {})) {
            option::some(package::upgrade_package(df::borrow<UpgradeKey, UpgradeCap>(
                &registry.id,
                UpgradeKey {},
            )))
        } else {
            option::none()
        }
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
        let Registry { mut id, latest_height, mut commitments } = registry;
        let mut height = latest_height;
        while (height > 0) {
            table::remove(&mut commitments, height);
            if (df::exists<StateRootKey>(&id, StateRootKey { height })) {
                let _ = df::remove<StateRootKey, vector<u8>>(&mut id, StateRootKey { height });
            };
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
    const E_OPS_REQUIRED_FOR_TEST: u64 = 8;

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
        entomb_settler_cap_ops(test_cap(ctx), &mut registry, ctx);
        registry
    }

    /// An `OpsCap` bound to the registry, mirroring what the entombment
    /// mint transferred to the sender. Public test-only: see
    /// [`rotated_registry_for_testing`].
    #[test_only]
    public fun ops_cap_for_testing(registry: &Registry, ctx: &mut TxContext): rotation::OpsCap {
        rotation::mint_ops_for_testing(&registry.id, ctx)
    }

    /// Removes the peer book so tests can destroy the registry. Public
    /// test-only (see [`rotated_registry_for_testing`]).
    #[test_only]
    public fun destroy_peer_book_for_testing(registry: &mut Registry) {
        if (df::exists_with_type<PeerKey, PeerBook>(&registry.id, PeerKey {})) {
            let PeerBook { version: _, mut peers } =
                df::remove<PeerKey, PeerBook>(&mut registry.id, PeerKey {});
            while (!peers.is_empty()) {
                peers.pop();
            };
            peers.destroy_empty();
        };
    }

    /// Removes the entombed cap, the mint marker, and the rotation state so
    /// tests can destroy the registry. Public test-only (see
    /// [`rotated_registry_for_testing`]).
    #[test_only]
    public fun destroy_rotation_for_testing(registry: &mut Registry) {
        let _ = df::remove<rotation::StateKey, rotation::RotationState>(
            &mut registry.id,
            rotation::state_key(),
        );
        let _ = df::remove<rotation::OpsMintedKey, bool>(
            &mut registry.id,
            rotation::ops_minted_key(),
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

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
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

        rotation::retire_ops_cap(ops);
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

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
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

        rotation::retire_ops_cap(ops);
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

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms() - 1);
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_CLAIM_TARGET_FOR_TEST, location = rotation)]
    fun claim_by_the_wrong_owner_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        let ctx_c = ctx_for(CAROL, 2);
        claim_rotation(&mut registry, &clock, &ctx_c);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_PENDING_FOR_TEST, location = rotation)]
    fun double_claim_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);
        let ctx_b2 = ctx_for(BOB, 3);
        claim_rotation(&mut registry, &clock, &ctx_b2);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_CLAIM_WINDOW_EXPIRED_FOR_TEST, location = rotation)]
    fun claim_after_the_window_expires_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        clock::set_for_testing(
            &mut clock,
            rotation::min_delay_ms() + rotation::claim_window_ms() + 1,
        );
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_DELAY_OUT_OF_BOUNDS_FOR_TEST, location = rotation)]
    fun request_below_the_minimum_delay_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        let ops = ops_cap_for_testing(&registry, ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms() - 1, &ops);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_DELAY_OUT_OF_BOUNDS_FOR_TEST, location = rotation)]
    fun request_above_the_maximum_delay_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        let ops = ops_cap_for_testing(&registry, ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::max_delay_ms() + 1, &ops);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_OPS_REQUIRED_FOR_TEST, location = rotation)]
    fun the_tombstoned_request_wrapper_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        // Steering is never sender-checked, so there is no "non-authority
        // request" anymore: the old wrapper is a tombstone that rejects the
        // authority herself — only the `OpsCap` opens the ops entrypoint.
        request_rotation(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun cancel_clears_the_pending_request() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        let ops = ops_cap_for_testing(&registry, ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        cancel_rotation(&mut registry, ctx_a);
        assert!(option::is_none(&rotation::pending_target(rotation_state(&registry))));

        // A re-request overwrites cleanly after a cancel.
        request_rotation_ops(&mut registry, &clock, CAROL, rotation::min_delay_ms(), &ops);
        let target = rotation::pending_target(rotation_state(&registry));
        assert!(option::is_some(&target) && *option::borrow(&target) == CAROL);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    // ---------- window steering (force inclusion) ----------
    //
    // The inclusion window is adjustable: the Registry's `OpsCap` steers it,
    // narrowings apply immediately, widenings stage behind a timelock.

    /// Mirrors `inbox::E_WINDOW_OUT_OF_BOUNDS`; own constants are private.
    #[test_only]
    const E_WINDOW_OUT_OF_BOUNDS_FOR_TEST: u64 = 5;

    /// Mirrors `inbox::E_NO_PENDING_WINDOW`; own constants are private.
    #[test_only]
    const E_NO_PENDING_WINDOW_FOR_TEST: u64 = 7;

    #[test]
    fun ops_gated_narrowing_applies_immediately() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        clock::set_for_testing(&mut clock, 1_000_000);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_window_change(&mut registry, &mut inbox, &clock, 60_000, 0, &ops);

        assert!(inbox::window_override_ms(&inbox) == option::some(60_000));
        assert!(option::is_none(&inbox::pending_window(&inbox)));
        assert!(inbox::effective_window_ms(&inbox, &clock) == 60_000);

        // Fresh submissions charge the narrowed window.
        inbox::submit(&mut inbox, &clock, b"tx");
        assert!(inbox::pending_deadline_ms_for_testing(&inbox, 0) == 1_000_000 + 60_000);

        inbox::drain_for_testing(&mut inbox);
        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun ops_gated_widening_stages_behind_the_timelock() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        clock::set_for_testing(&mut clock, 1_000_000);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_window_change(
            &mut registry,
            &mut inbox,
            &clock,
            1_800_000,
            inbox::min_widen_delay_ms(),
            &ops,
        );

        // Staged, not applied: submissions still charge the default window
        // until the timelock elapses, then flip without any transaction.
        let staged = inbox::pending_window(&inbox);
        assert!(option::is_some(&staged));
        assert!(inbox::effective_window_ms(&inbox, &clock) == 600_000);
        inbox::submit(&mut inbox, &clock, b"tx");
        assert!(inbox::pending_deadline_ms_for_testing(&inbox, 0) == 1_000_000 + 600_000);
        inbox::drain_for_testing(&mut inbox);

        clock::set_for_testing(&mut clock, 1_000_000 + inbox::min_widen_delay_ms());
        assert!(inbox::effective_window_ms(&inbox, &clock) == 1_800_000);
        inbox::submit(&mut inbox, &clock, b"tx-2");
        assert!(inbox::pending_deadline_ms_for_testing(&inbox, 1)
            == 1_000_000 + inbox::min_widen_delay_ms() + 1_800_000);

        inbox::drain_for_testing(&mut inbox);
        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun ops_gated_cancel_removes_the_staged_widening() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);
        clock::set_for_testing(&mut clock, 1_000_000);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_window_change(
            &mut registry,
            &mut inbox,
            &clock,
            1_800_000,
            inbox::min_widen_delay_ms(),
            &ops,
        );
        cancel_window_change(&mut registry, &mut inbox, &ops);

        assert!(option::is_none(&inbox::pending_window(&inbox)));
        assert!(inbox::effective_window_ms(&inbox, &clock) == 600_000);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_OPS_MISMATCH_FOR_TEST, location = rotation)]
    fun window_change_rejects_a_foreign_ops_cap() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut ctx_b = ctx_for(BOB, 2);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let other = registry_for_testing(&mut ctx_b);
        let foreign = ops_cap_for_testing(&other, &mut ctx_b);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        request_window_change(&mut registry, &mut inbox, &clock, 60_000, 0, &foreign);

        rotation::retire_ops_cap(foreign);
        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_registry(other);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_WINDOW_OUT_OF_BOUNDS_FOR_TEST, location = inbox)]
    fun window_change_rejects_windows_above_the_ceiling() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_window_change(
            &mut registry,
            &mut inbox,
            &clock,
            inbox::max_window_ms() + 1,
            0,
            &ops,
        );

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_PENDING_WINDOW_FOR_TEST, location = inbox)]
    fun window_cancel_without_a_staged_change_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut inbox = inbox::inbox_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        cancel_window_change(&mut registry, &mut inbox, &ops);

        rotation::retire_ops_cap(ops);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_PENDING_FOR_TEST, location = rotation)]
    fun claim_after_cancel_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        cancel_rotation(&mut registry, &ctx_a);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut registry, &clock, &ctx_b);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun cancel_by_a_non_authority_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        let ctx_b = ctx_for(BOB, 2);
        cancel_rotation(&mut registry, &ctx_b);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_ALREADY_ENTOMBED_FOR_TEST, location = rotation)]
    fun entomb_twice_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 1);
        let mut registry = rotated_registry_for_testing(ctx_a);

        entomb_settler_cap_ops(test_cap(ctx_a), &mut registry, ctx_a);

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

    // ---------- settled state roots (exit-queue proof anchor) ----------

    /// Mirrors `settlement::E_STATE_ROOT_MALFORMED`; own constants are private.
    #[test_only]
    const E_STATE_ROOT_MALFORMED_FOR_TEST: u64 = 10;

    /// Mirrors `settlement::E_STATE_ROOT_HEIGHT`; own constants are private.
    #[test_only]
    const E_STATE_ROOT_HEIGHT_FOR_TEST: u64 = 11;

    /// Mirrors `settlement::E_STATE_ROOT_CONFLICT`; own constants are private.
    #[test_only]
    const E_STATE_ROOT_CONFLICT_FOR_TEST: u64 = 12;

    /// Settles heights 1..=`to` with placeholder blobs and no inbox traffic.
    #[test_only]
    public fun settled_registry_for_testing(
        ctx: &mut TxContext,
        to: u64,
    ): (Registry, inbox::Inbox, sui::clock::Clock) {
        let mut registry = rotated_registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);
        let mut height = 1;
        while (height <= to) {
            settle_as(&mut registry, &mut inbox, &clock, b"blob", height, height * 1_000, 0, ctx);
            height = height + 1;
        };
        (registry, inbox, clock)
    }

    #[test]
    fun state_root_is_none_before_any_commit() {
        let ctx = &mut tx_context::dummy();
        let (mut registry, inbox, clock) = settled_registry_for_testing(ctx, 1);

        assert!(option::is_none(&state_root(&registry, 1)));

        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun commit_state_root_stores_the_settled_root() {
        let ctx = &mut tx_context::dummy();
        let (mut registry, inbox, clock) = settled_registry_for_testing(ctx, 1);
        let root = digest_for_testing(7);

        commit_state_root(&mut registry, &clock, 1, root, ctx);

        assert!(option::is_some(&state_root(&registry, 1)));
        assert!(*option::borrow(&state_root(&registry, 1)) == root);

        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun commit_state_root_keeps_heights_independent() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let mut inbox = inbox::inbox_for_testing(ctx);
        let mut clock = clock::create_for_testing(ctx);
        let root_1 = digest_for_testing(1);
        let root_2 = digest_for_testing(2);

        // Each settle composes its own root commit at the tip.
        settle_as(&mut registry, &mut inbox, &clock, b"blob", 1, 1_000, 0, ctx);
        commit_state_root(&mut registry, &clock, 1, root_1, ctx);
        settle_as(&mut registry, &mut inbox, &clock, b"blob", 2, 2_000, 0, ctx);
        commit_state_root(&mut registry, &clock, 2, root_2, ctx);

        assert!(*option::borrow(&state_root(&registry, 1)) == root_1);
        assert!(*option::borrow(&state_root(&registry, 2)) == root_2);

        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_STATE_ROOT_MALFORMED_FOR_TEST, location = Self)]
    fun commit_state_root_rejects_a_root_of_the_wrong_length() {
        let ctx = &mut tx_context::dummy();
        let (mut registry, inbox, clock) = settled_registry_for_testing(ctx, 1);

        commit_state_root(&mut registry, &clock, 1, x"0001020304", ctx);

        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_STATE_ROOT_HEIGHT_FOR_TEST, location = Self)]
    fun commit_state_root_rejects_a_height_above_the_settled_tip() {
        let ctx = &mut tx_context::dummy();
        let (mut registry, inbox, clock) = settled_registry_for_testing(ctx, 1);

        commit_state_root(&mut registry, &clock, 2, digest_for_testing(2), ctx);

        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_STATE_ROOT_HEIGHT_FOR_TEST, location = Self)]
    fun commit_state_root_rejects_a_height_below_the_settled_tip() {
        let ctx = &mut tx_context::dummy();
        let (mut registry, inbox, clock) = settled_registry_for_testing(ctx, 2);

        commit_state_root(&mut registry, &clock, 1, digest_for_testing(1), ctx);

        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun recommitting_the_same_root_is_a_no_op() {
        let ctx = &mut tx_context::dummy();
        let (mut registry, inbox, clock) = settled_registry_for_testing(ctx, 1);
        let root = digest_for_testing(7);

        commit_state_root(&mut registry, &clock, 1, root, ctx);
        // Crash-retry of the composed settle PTB re-runs both commands.
        commit_state_root(&mut registry, &clock, 1, root, ctx);

        assert!(*option::borrow(&state_root(&registry, 1)) == root);

        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_STATE_ROOT_CONFLICT_FOR_TEST, location = Self)]
    fun recommitting_a_different_root_aborts() {
        let ctx = &mut tx_context::dummy();
        let (mut registry, inbox, clock) = settled_registry_for_testing(ctx, 1);

        commit_state_root(&mut registry, &clock, 1, digest_for_testing(7), ctx);
        commit_state_root(&mut registry, &clock, 1, digest_for_testing(8), ctx);

        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun commit_state_root_requires_the_rotation_authority() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let (mut registry, inbox, clock) = settled_registry_for_testing(&mut ctx_a, 1);
        // A different sender cannot anchor a state root, even at the tip.
        let ctx_b = ctx_for(BOB, 2);
        commit_state_root(&mut registry, &clock, 1, digest_for_testing(7), &ctx_b);

        rotation::retire_ops_cap(ops_cap_for_testing(&registry, &mut ctx_a));
        destroy_inbox_helper(inbox);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    /// Placeholder 32-byte root; the commit path only checks the length.
    #[test_only]
    fun digest_for_testing(seed: u8): vector<u8> {
        let mut root = vector::empty<u8>();
        let mut i = 0;
        while (i < 32) {
            vector::push_back(&mut root, seed);
            i = i + 1;
        };
        root
    }

    /// Drains pending entries so the inbox can be destroyed in these tests.
    /// Public test-only (see [`rotated_registry_for_testing`]).
    #[test_only]
    public fun destroy_inbox_helper(mut inbox: inbox::Inbox) {
        inbox::drain_for_testing(&mut inbox);
        inbox::destroy_inbox_for_testing(inbox);
    }

    // ---------- peer book tests (registry-published committee) ----------

    /// Mirrors `settlement::E_INVALID_PEER_KEY`; own constants are private.
    #[test_only]
    const E_INVALID_PEER_KEY_SELF: u64 = 3;
    #[test_only]
    const E_INVALID_PEER_ADDR_SELF: u64 = 4;
    #[test_only]
    const E_PEER_MISSING_SELF: u64 = 5;
    #[test_only]
    const E_PEER_FULL_SELF: u64 = 6;

    #[test_only]
    const PEER_KEY_LEN: u64 = 32;

    /// A `len`-byte all-zero vector (this stdlib has no `vector::repeat`).
    #[test_only]
    fun zeros(len: u64): vector<u8> {
        let mut v = vector[];
        let mut i = 0;
        while (i < len) {
            vector::push_back(&mut v, 0);
            i = i + 1;
        };
        v
    }

    /// A deterministic 32-byte test peer identity.
    #[test_only]
    fun peer_key_for(n: u64): vector<u8> {
        let mut key = zeros(PEER_KEY_LEN);
        let mut i = 0;
        let mut v = n;
        while (i < 8) {
            *vector::borrow_mut(&mut key, PEER_KEY_LEN - 8 + i) = (v & 0xff) as u8;
            v = v / 256;
            i = i + 1;
        };
        key
    }

    #[test]
    fun peer_book_is_empty_before_the_first_publish() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);

        assert!(peer_count(&registry) == 0);
        assert!(!has_peer(&registry, &peer_key_for(1)));

        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun publish_peer_upserts_and_remove_deletes() {
        let ctx_a = &mut ctx_for(ALICE, 41);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        let key_a = peer_key_for(1);
        let key_b = peer_key_for(2);
        publish_peer(&mut registry, &clock, key_a, string::utf8(b"seq.example:7000"), ctx_a);
        publish_peer(&mut registry, &clock, key_b, string::utf8(b"fn.example:7000"), ctx_a);
        assert!(peer_count(&registry) == 2);
        assert!(has_peer(&registry, &key_a));
        assert!(peer_addr(&registry, &key_a) == string::utf8(b"seq.example:7000"));

        // Upsert: same identity, new dial address, count unchanged.
        publish_peer(&mut registry, &clock, key_a, string::utf8(b"seq2.example:7000"), ctx_a);
        assert!(peer_count(&registry) == 2);
        assert!(peer_addr(&registry, &key_a) == string::utf8(b"seq2.example:7000"));

        remove_peer(&mut registry, &clock, key_b, ctx_a);
        assert!(peer_count(&registry) == 1);
        assert!(!has_peer(&registry, &key_b));

        clock::destroy_for_testing(clock);
        destroy_peer_book_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun the_book_version_advances_on_every_mutation() {
        let ctx_a = &mut ctx_for(ALICE, 60);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        assert!(peer_book_version(&registry) == 0);
        publish_peer(&mut registry, &clock, peer_key_for(1), string::utf8(b"a:1"), ctx_a);
        assert!(peer_book_version(&registry) == 1);
        // An upsert is also a mutation: the version is the gossip epoch.
        publish_peer(&mut registry, &clock, peer_key_for(1), string::utf8(b"a:2"), ctx_a);
        assert!(peer_book_version(&registry) == 2);
        remove_peer(&mut registry, &clock, peer_key_for(1), ctx_a);
        assert!(peer_book_version(&registry) == 3);

        clock::destroy_for_testing(clock);
        destroy_peer_book_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_PEER_MISSING_SELF)]
    fun remove_peer_with_an_unknown_key_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 42);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        remove_peer(&mut registry, &clock, peer_key_for(9), ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_INVALID_PEER_KEY_SELF)]
    fun publish_peer_rejects_a_malformed_identity() {
        let ctx_a = &mut ctx_for(ALICE, 43);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        publish_peer(&mut registry, &clock, zeros(31), string::utf8(b"a:1"), ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_INVALID_PEER_ADDR_SELF)]
    fun publish_peer_rejects_an_empty_address() {
        let ctx_a = &mut ctx_for(ALICE, 44);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        publish_peer(&mut registry, &clock, peer_key_for(1), string::utf8(b""), ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_INVALID_PEER_ADDR_SELF)]
    fun publish_peer_rejects_an_oversized_address() {
        let ctx_a = &mut ctx_for(ALICE, 45);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        publish_peer(&mut registry, &clock, peer_key_for(1), string::utf8(zeros(257)), ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_PEER_FULL_SELF)]
    fun publish_peer_caps_the_committee() {
        let ctx_a = &mut ctx_for(ALICE, 46);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        let mut i = 0;
        while (i < MAX_PEERS) {
            publish_peer(&mut registry, &clock, peer_key_for(i), string::utf8(b"p:1"), ctx_a);
            i = i + 1;
        };
        publish_peer(&mut registry, &clock, peer_key_for(MAX_PEERS), string::utf8(b"p:2"), ctx_a);

        clock::destroy_for_testing(clock);
        destroy_peer_book_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun publish_peer_by_a_stranger_aborts() {
        let ctx_a = &mut ctx_for(ALICE, 47);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        let ctx_c = ctx_for(CAROL, 48);
        publish_peer(&mut registry, &clock, peer_key_for(1), string::utf8(b"c:1"), &ctx_c);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun the_previous_authority_publishes_in_grace() {
        let mut ctx_a = ctx_for(ALICE, 49);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        let ctx_b = ctx_for(BOB, 50);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);

        // Inside grace the old key still steers the committee book.
        let ctx_a2 = ctx_for(ALICE, 51);
        publish_peer(&mut registry, &clock, peer_key_for(1), string::utf8(b"old:1"), &ctx_a2);
        assert!(peer_count(&registry) == 1);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_peer_book_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun the_previous_authority_cannot_publish_after_retire() {
        let mut ctx_a = ctx_for(ALICE, 56);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        let ctx_b = ctx_for(BOB, 57);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);
        let ctx_b2 = ctx_for(BOB, 58);
        retire_previous(&mut registry, &ctx_b2);

        // Retire kills the grace window one-way; the old key cannot write.
        let ctx_a2 = ctx_for(ALICE, 59);
        publish_peer(&mut registry, &clock, peer_key_for(2), string::utf8(b"old:2"), &ctx_a2);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun the_new_authority_publishes_right_after_the_claim() {
        let mut ctx_a = ctx_for(ALICE, 53);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        let ctx_b = ctx_for(BOB, 54);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);
        publish_peer(&mut registry, &clock, peer_key_for(1), string::utf8(b"new:1"), &ctx_b);
        assert!(peer_addr(&registry, &peer_key_for(1)) == string::utf8(b"new:1"));

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_peer_book_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_INVALID_PEER_KEY_SELF)]
    fun remove_peer_rejects_a_malformed_identity() {
        let ctx_a = &mut ctx_for(ALICE, 55);
        let mut registry = rotated_registry_for_testing(ctx_a);
        let clock = clock::create_for_testing(ctx_a);

        remove_peer(&mut registry, &clock, zeros(33), ctx_a);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
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
        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);

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

        rotation::retire_ops_cap(ops);
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
        let ops = ops_cap_for_testing(&registry, &mut ctx_a);
        request_rotation_ops(&mut registry, &clock, BOB, rotation::min_delay_ms(), &ops);
        let ctx_b = ctx_for(BOB, 37);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut registry, &clock, &ctx_b);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-2", 2, 2_000, 0, &ctx_b);

        // Inside grace the old key still settles new heights, but re-signing
        // an already-settled height with a different blob is equivocation,
        // grace window or not.
        let ctx_a2 = ctx_for(ALICE, 38);
        settle_as(&mut registry, &mut inbox, &clock, b"blob-2-fork", 2, 2_000, 0, &ctx_a2);

        rotation::retire_ops_cap(ops);
        clock::destroy_for_testing(clock);
        destroy_inbox_helper(inbox);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    // ---------- package upgrade custody tests ----------

    /// Mirrors this module's `E_NO_UPGRADE_CAP`.
    #[test_only]
    const E_NO_UPGRADE_CAP_FOR_TEST: u64 = 7;
    /// Mirrors this module's `E_UPGRADE_CAP_PRESENT`.
    #[test_only]
    const E_UPGRADE_CAP_PRESENT_FOR_TEST: u64 = 8;
    /// Mirrors this module's `E_TOO_PERMISSIVE`.
    #[test_only]
    const E_TOO_PERMISSIVE_FOR_TEST: u64 = 9;
    /// Mirrors `rotation::E_OPS_MISMATCH`.
    #[test_only]
    const E_OPS_MISMATCH_FOR_TEST: u64 = 9;

    /// A package `UpgradeCap` for a stand-in package id, mirroring what the
    /// publish transaction returns to the deployer.
    #[test_only]
    fun upgrade_cap_for_testing(ctx: &mut TxContext): package::UpgradeCap {
        package::test_publish(object::id_from_address(@0x42), ctx)
    }

    /// Frees the entombed `UpgradeCap`, if any, so tests can destroy the
    /// registry. Consumes the cap through `make_immutable`.
    #[test_only]
    public fun destroy_upgrade_cap_for_testing(registry: &mut Registry) {
        if (df::exists_with_type<UpgradeKey, package::UpgradeCap>(&registry.id, UpgradeKey {})) {
            let cap = df::remove<UpgradeKey, package::UpgradeCap>(&mut registry.id, UpgradeKey {});
            package::make_immutable(cap);
        };
    }

    #[test]
    fun upgrade_ceremony_entombs_authorizes_and_commits() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);
        let pkg_id = option::borrow(&upgrade_package_id(&registry));
        assert!(*pkg_id == object::id_from_address(@0x42));

        let ticket = authorize_package_upgrade(
            &mut registry,
            &ops,
            package::compatible_policy(),
            b"fake-digest",
        );
        // Stands in for the `Upgrade` transaction command: the framework's
        // test helper converts the ticket into the receipt the validators
        // would produce for a matching upgrade.
        let receipt = package::test_upgrade(ticket);
        commit_package_upgrade(&mut registry, &ops, receipt);

        let cap = df::borrow<UpgradeKey, UpgradeCap>(&registry.id, upgrade_key());
        assert!(package::version(cap) == 2);

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_OPS_MISMATCH_FOR_TEST, location = rotation)]
    fun upgrade_entombment_rejects_a_foreign_ops_cap() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut ctx_b = ctx_for(BOB, 2);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let other = registry_for_testing(&mut ctx_b);
        let foreign = ops_cap_for_testing(&other, &mut ctx_b);

        entomb_upgrade_cap(upgrade_cap_for_testing(&mut ctx_a), &mut registry, &foreign);

        rotation::retire_ops_cap(foreign);
        destroy_registry(other);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_UPGRADE_CAP_FOR_TEST)]
    fun authorize_without_custody_aborts() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        // The runtime abort fires at the missing-slot check; the binding
        // keeps the hot-potato ticket well-formed for compilation.
        let ticket = authorize_package_upgrade(
            &mut registry,
            &ops,
            package::compatible_policy(),
            b"fake-digest",
        );
        let receipt = package::test_upgrade(ticket);
        commit_package_upgrade(&mut registry, &ops, receipt);

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_UPGRADE_CAP_PRESENT_FOR_TEST)]
    fun double_upgrade_entombment_aborts() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);
        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_UPGRADE_CAP_FOR_TEST)]
    fun authorize_after_freeze_aborts() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);
        freeze_package(&mut registry, &ops);
        assert!(option::is_none(&upgrade_package_id(&registry)));

        // The runtime abort fires at the missing-slot check; the binding
        // keeps the hot-potato ticket well-formed for compilation.
        let ticket = authorize_package_upgrade(
            &mut registry,
            &ops,
            package::compatible_policy(),
            b"fake-digest",
        );
        let receipt = package::test_upgrade(ticket);
        commit_package_upgrade(&mut registry, &ops, receipt);

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_NO_UPGRADE_CAP_FOR_TEST)]
    fun double_freeze_aborts() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);
        freeze_package(&mut registry, &ops);
        freeze_package(&mut registry, &ops);

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_OPS_MISMATCH_FOR_TEST, location = rotation)]
    fun freeze_rejects_a_foreign_ops_cap() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut ctx_b = ctx_for(BOB, 2);
        let mut registry = rotated_registry_for_testing(&mut ctx_a);
        let other = registry_for_testing(&mut ctx_b);
        let foreign = ops_cap_for_testing(&other, &mut ctx_b);

        entomb_upgrade_cap(upgrade_cap_for_testing(&mut ctx_a), &mut registry, &foreign);

        rotation::retire_ops_cap(foreign);
        destroy_registry(other);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test]
    fun restrict_ladder_tightens_monotonically() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);
        restrict_package_upgrades(&mut registry, &ops, package::additive_policy());
        let cap = df::borrow<UpgradeKey, package::UpgradeCap>(&registry.id, upgrade_key());
        assert!(package::upgrade_policy(cap) == package::additive_policy());

        restrict_package_upgrades(&mut registry, &ops, package::dep_only_policy());
        let cap = df::borrow<UpgradeKey, package::UpgradeCap>(&registry.id, upgrade_key());
        assert!(package::upgrade_policy(cap) == package::dep_only_policy());

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_TOO_PERMISSIVE_FOR_TEST)]
    fun restrict_cannot_loosen_the_policy() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);
        restrict_package_upgrades(&mut registry, &ops, package::additive_policy());
        restrict_package_upgrades(&mut registry, &ops, package::compatible_policy());

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }

    #[test, expected_failure(abort_code = E_TOO_PERMISSIVE_FOR_TEST)]
    fun restrict_rejects_unknown_policy_values() {
        let ctx = &mut tx_context::dummy();
        let mut registry = rotated_registry_for_testing(ctx);
        let ops = ops_cap_for_testing(&registry, ctx);

        entomb_upgrade_cap(upgrade_cap_for_testing(ctx), &mut registry, &ops);
        restrict_package_upgrades(&mut registry, &ops, 63);

        rotation::retire_ops_cap(ops);
        destroy_upgrade_cap_for_testing(&mut registry);
        destroy_rotation_for_testing(&mut registry);
        destroy_registry(registry);
    }
}
