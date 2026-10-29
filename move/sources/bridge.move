/// Podseq enshrined bridge vault.
///
/// Holds Sui coins locked by users until they withdraw them back to Sui. The L2
/// side (`solidity/Bridge.sol`) mirrors each deposit as a mint and each burn as a
/// withdrawal request. The sequencer, holding `BridgeCap`, is the only party that
/// can release coins, so the bridge needs no external relayer.
///
/// Deposits are generic over the coin type: `deposit<SUI>` locks SUI, `deposit<USDS>`
/// locks USDSui, etc. Any coin type is accepted: on the L2 side a `BridgeFactory`
/// lets anyone create the canonical ERC20 for a coin type that lacks one, and the
/// relayer creates it on demand when a deposit arrives, so a deposit can always
/// be represented and later burned for release.
///
/// The Sui gRPC read API has no event query, so each deposit is also stored in a
/// `Table<u64, DepositRecord>` keyed by its nonce. The relayer reads deposits the
/// same way a full node reads settled heights: by fetching each nonce's dynamic
/// field object id. This mirrors `settlement::commitment_at`.
module podseq::bridge {
    use podseq::rotation;
    use sui::coin::{Self, Coin};
    use sui::bag::{Self, Bag};
    use sui::object::{Self, UID};
    use sui::transfer;
    use sui::tx_context::{Self, TxContext};
    use std::type_name::{Self, TypeName};
    use sui::table::{Self, Table};
    use sui::event;
    use std::ascii;
    use std::bcs;

    /// Deposit amount must be greater than zero.
    const E_ZERO_AMOUNT: u64 = 0;
    /// Vault holds less of this coin type than requested.
    const E_INSUFFICIENT_LIQUIDITY: u64 = 1;
    /// L2 recipient must be exactly 20 bytes.
    const E_INVALID_RECIPIENT: u64 = 2;
    /// The capability gate moved into rotation state; `withdraw` is retired.
    const E_ROTATED: u64 = 3;

    /// Authorizes the sequencer to release locked coins. Transferred to the
    /// operator at initialization; transferable for sequencer rotation.
    public struct BridgeCap has key, store {
        id: UID,
    }

    /// One locked deposit, readable by the relayer via its dynamic-field object.
    public struct DepositRecord has store {
        amount: u64,
        recipient_l2: vector<u8>,
        coin_type: vector<u8>,
    }

    /// Shared vault holding one locked `Coin<T>` per coin type and one
    /// `DepositRecord` per deposit nonce.
    public struct Vault has key {
        id: UID,
        /// Monotonic counter for deposits; mirrored on L2 as the mint nonce.
        deposit_nonce: u64,
        /// Monotonic counter for sequencer-initiated releases.
        withdraw_nonce: u64,
        /// `type_name<T>() -> Coin<T>`. Heterogeneous values require `Bag`.
        reserves: Bag,
        /// `nonce -> DepositRecord`.
        deposits: Table<u64, DepositRecord>,
        /// `withdraw_key<T>(l2_nonce) -> true`. Makes `withdraw` idempotent so a
        /// relayer crash between the Sui release and its cursor persist cannot
        /// release the same burn twice. Keyed per-coin-type because each L2
        /// `Bridge` has its own withdrawal-nonce sequence.
        processed_withdrawals: Table<vector<u8>, bool>,
    }

    /// Emitted on every deposit. For external indexers; the relayer reads state.
    public struct Deposit has copy, drop {
        nonce: u64,
        coin_type: TypeName,
        amount: u64,
        recipient_l2: vector<u8>,
    }

    /// Emitted on every release. Lets indexers track Sui outflows.
    public struct Withdrawal has copy, drop {
        nonce: u64,
        coin_type: TypeName,
        amount: u64,
        recipient_sui: address,
        l2_nonce: u64,
    }

    /// Creates and shares the vault. Transfers `BridgeCap` to the caller.
    ///
    /// Call once at package publish or via a dedicated setup transaction.
    #[allow(lint(self_transfer))]
    public fun initialize(ctx: &mut TxContext) {
        let vault = Vault {
            id: object::new(ctx),
            deposit_nonce: 0,
            withdraw_nonce: 0,
            reserves: bag::new(ctx),
            deposits: table::new(ctx),
            processed_withdrawals: table::new(ctx),
        };
        transfer::share_object(vault);
        transfer::transfer(
            BridgeCap { id: object::new(ctx) },
            tx_context::sender(ctx),
        );
    }

    /// Locks `coin` into the vault, crediting `recipient_l2` on L2.
    ///
    /// `recipient_l2` is the 20-byte EVM address that will receive the minted
    /// bridged token. Anyone may call this with any coin type; the locked coins
    /// can only leave via [`withdraw`], which is gated by `BridgeCap`.
    public fun deposit<T>(
        vault: &mut Vault,
        coin: Coin<T>,
        recipient_l2: vector<u8>,
        _ctx: &mut TxContext,
    ) {
        let amount = coin::value(&coin);
        assert!(amount > 0, E_ZERO_AMOUNT);
        assert!(vector::length(&recipient_l2) == 20, E_INVALID_RECIPIENT);

        if (bag::contains(&vault.reserves, type_key<T>())) {
            let held: &mut Coin<T> = bag::borrow_mut(&mut vault.reserves, type_key<T>());
            coin::join(held, coin);
        } else {
            bag::add(&mut vault.reserves, type_key<T>(), coin);
        };

        let nonce = vault.deposit_nonce;
        table::add(&mut vault.deposits, nonce, DepositRecord {
            amount,
            recipient_l2: recipient_l2,
            coin_type: type_key<T>(),
        });
        vault.deposit_nonce = nonce + 1;
        event::emit(Deposit {
            nonce,
            coin_type: type_name::with_defining_ids<T>(),
            amount,
            recipient_l2,
        });
    }

    /// Retired: the capability gate moved into rotation state on the Vault.
    ///
    /// The signature is frozen by Sui's upgrade-compatibility rules, so the
    /// entrypoint stays and aborts instead. The relayer calls
    /// [`withdraw_as`]; entombment (`entomb_bridge_cap`) makes this input
    /// impossible to provide, and this abort is defense in depth.
    #[allow(unused_type_parameter)]
    public fun withdraw<T>(
        _cap: &BridgeCap,
        _vault: &mut Vault,
        _recipient: address,
        _amount: u64,
        _l2_nonce: u64,
        _ctx: &mut TxContext,
    ): bool {
        abort E_ROTATED
    }

    /// Releases `amount` of coin type `T` to `recipient` on Sui as the
    /// rotation-state authority (or the previous authority inside its grace
    /// window).
    ///
    /// `l2_nonce` is the L2 withdrawal nonce that triggered the release,
    /// replayed in the `Withdrawal` event for indexer correlation. Idempotent
    /// per `(T, l2_nonce)`: re-submitting the same burn is a no-op that
    /// returns `false`, so a relayer crash can never release coins twice.
    public fun withdraw_as<T>(
        vault: &mut Vault,
        clock: &sui::clock::Clock,
        recipient: address,
        amount: u64,
        l2_nonce: u64,
        ctx: &mut TxContext,
    ): bool {
        rotation::assert_can_act(&vault.id, clock, ctx);
        do_withdraw<T>(vault, recipient, amount, l2_nonce, ctx)
    }

    /// The withdraw body shared by the rotation-aware entrypoint.
    fun do_withdraw<T>(
        vault: &mut Vault,
        recipient: address,
        amount: u64,
        l2_nonce: u64,
        ctx: &mut TxContext,
    ): bool {
        let key = withdraw_key<T>(l2_nonce);
        if (table::contains(&vault.processed_withdrawals, key)) {
            return false
        };
        table::add(&mut vault.processed_withdrawals, key, true);

        assert!(amount > 0, E_ZERO_AMOUNT);
        assert!(bag::contains(&vault.reserves, type_key<T>()), E_INSUFFICIENT_LIQUIDITY);
        let held: &mut Coin<T> = bag::borrow_mut(&mut vault.reserves, type_key<T>());
        assert!(coin::value(held) >= amount, E_INSUFFICIENT_LIQUIDITY);

        let coin = coin::split(held, amount, ctx);
        let nonce = vault.withdraw_nonce;
        vault.withdraw_nonce = nonce + 1;
        event::emit(Withdrawal {
            nonce,
            coin_type: type_name::with_defining_ids<T>(),
            amount,
            recipient_sui: recipient,
            l2_nonce,
        });
        transfer::public_transfer(coin, recipient);
        true
    }

    /// Entombs the BridgeCap inside the Vault and initializes rotation state
    /// with the sender as authority. The migration step after a package
    /// upgrade; ideally in the same programmable transaction block as the
    /// upgrade, signed by the current cap owner.
    public fun entomb_bridge_cap(cap: BridgeCap, vault: &mut Vault, ctx: &TxContext) {
        rotation::entomb(&mut vault.id, cap, ctx)
    }

    /// Pre-authorizes `to` to claim the bridge authority after `delay_ms`.
    public fun request_rotation(
        vault: &mut Vault,
        clock: &sui::clock::Clock,
        to: address,
        delay_ms: u64,
        ctx: &TxContext,
    ) {
        rotation::request_rotation(&mut vault.id, to, delay_ms, clock, ctx)
    }

    /// Cancels the pending bridge rotation. Authority only.
    public fun cancel_rotation(vault: &mut Vault, ctx: &TxContext) {
        rotation::cancel_rotation(&mut vault.id, ctx)
    }

    /// Claims the bridge authority. Pending target only, inside the claim
    /// window; starts the previous owner's grace window.
    public fun claim_rotation(vault: &mut Vault, clock: &sui::clock::Clock, ctx: &TxContext) {
        rotation::claim_rotation(&mut vault.id, clock, ctx)
    }

    /// Retires the previous bridge authority early. Authority only.
    public fun retire_previous(vault: &mut Vault, ctx: &TxContext) {
        rotation::retire_previous(&mut vault.id, ctx)
    }

    /// Borrows the bridge rotation state (for indexers and tests; the node
    /// reads the dynamic field's BCS directly).
    public fun rotation_state(vault: &Vault): &rotation::RotationState {
        rotation::state(&vault.id)
    }

    /// Returns the next deposit nonce to be assigned.
    public fun deposit_nonce(vault: &Vault): u64 {
        vault.deposit_nonce
    }

    /// Returns the next withdraw nonce to be assigned.
    public fun withdraw_nonce(vault: &Vault): u64 {
        vault.withdraw_nonce
    }

    /// Returns the deposit record for `nonce`, if it exists.
    public fun deposit_record(vault: &Vault, nonce: u64): &DepositRecord {
        table::borrow(&vault.deposits, nonce)
    }

    /// Returns the currently locked balance for coin type `T`.
    public fun reserve<T>(vault: &Vault): u64 {
        if (bag::contains(&vault.reserves, type_key<T>())) {
            let held: &Coin<T> = bag::borrow(&vault.reserves, type_key<T>());
            coin::value(held)
        } else {
            0
        }
    }

    /// Stable byte key for a coin type. `type_name::get_raw` is unavailable on
    /// all toolchain versions, so round-trip through the string form.
    fun type_key<T>(): vector<u8> {
        let s = type_name::with_defining_ids<T>().into_string();
        *ascii::as_bytes(&s)
    }

    /// Dedup key for a withdrawal: `type_key<T>() || l2_nonce_le`. Per-coin-type
    /// because each L2 `Bridge` has its own withdrawal-nonce sequence.
    fun withdraw_key<T>(l2_nonce: u64): vector<u8> {
        let mut key = type_key<T>();
        // BCS of a u64 is exactly its 8 little-endian bytes.
        vector::append(&mut key, bcs::to_bytes(&l2_nonce));
        key
    }

    // ---------- tests ----------

    #[test_only]
    use sui::balance;
    #[test_only]
    use sui::clock;
    #[test_only]
    use sui::dynamic_field as df;
    #[test_only]
    use std::option;

    /// Phantom coin type standing in for a real bridged asset.
    #[test_only]
    public struct TestCoin has drop {}

    /// Mirrors `rotation::E_*`; constants are module-private there.
    #[test_only]
    const E_NOT_AUTHORIZED_FOR_TEST: u64 = 0;
    #[test_only]
    const E_NOT_CLAIM_TARGET_FOR_TEST: u64 = 2;
    #[test_only]
    const E_CLAIM_TOO_EARLY_FOR_TEST: u64 = 3;

    #[test_only]
    const ALICE: address = @0xA;
    #[test_only]
    const BOB: address = @0xB;

    /// Sender-switching test context; `dummy` pins the sender to @0x0.
    ///
    /// `tx_context::sender` reads a runtime native: in unit tests the
    /// effective sender is the one from the most recently *constructed*
    /// context, so bind a fresh context before every sender-sensitive call.
    #[test_only]
    fun ctx_for(sender: address, hint: u64): TxContext {
        tx_context::new_from_hint(sender, hint, 0, 0, 0)
    }

    #[test_only]
    fun test_cap(ctx: &mut TxContext): BridgeCap {
        BridgeCap { id: object::new(ctx) }
    }

    /// A vault whose cap is entombed, so `withdraw_as` is callable.
    #[test_only]
    fun rotated_vault_for_testing(ctx: &mut TxContext): Vault {
        let mut vault = test_vault(ctx);
        entomb_bridge_cap(test_cap(ctx), &mut vault, ctx);
        vault
    }

    /// Removes the entombed cap and rotation state so tests can destroy the
    /// vault.
    #[test_only]
    fun destroy_rotation_for_testing(vault: &mut Vault) {
        let _ = df::remove<rotation::StateKey, rotation::RotationState>(
            &mut vault.id,
            rotation::state_key(),
        );
        let cap = df::remove<rotation::TombKey, BridgeCap>(&mut vault.id, rotation::tomb_key());
        let BridgeCap { id } = cap;
        id.delete();
    }

    /// Removes a processed-withdrawal marker so the table can be destroyed.
    #[test_only]
    fun clear_withdrawal_for_testing<T>(vault: &mut Vault, l2_nonce: u64) {
        table::remove<vector<u8>, bool>(&mut vault.processed_withdrawals, withdraw_key<T>(l2_nonce));
    }

    #[test_only]
    fun test_vault(ctx: &mut TxContext): Vault {
        Vault {
            id: object::new(ctx),
            deposit_nonce: 0,
            withdraw_nonce: 0,
            reserves: bag::new(ctx),
            deposits: table::new(ctx),
            processed_withdrawals: table::new(ctx),
        }
    }

    #[test_only]
    fun destroy_vault_empty(vault: Vault) {
        let Vault {
            id,
            deposit_nonce: _,
            withdraw_nonce: _,
            reserves,
            deposits,
            processed_withdrawals,
        } = vault;
        bag::destroy_empty(reserves);
        table::destroy_empty(deposits);
        table::destroy_empty(processed_withdrawals);
        object::delete(id);
    }

    #[test_only]
    const TEST_RECIPIENT: vector<u8> = x"0000000000000000000000000000000000000000";

    #[test]
    fun deposit_locks_coins_and_records_nonce() {
        let ctx = &mut tx_context::dummy();
        let mut vault = test_vault(ctx);

        let coin = coin::mint_for_testing<TestCoin>(250, ctx);
        deposit<TestCoin>(&mut vault, coin, TEST_RECIPIENT, ctx);
        assert!(reserve<TestCoin>(&vault) == 250, 0);
        assert!(deposit_nonce(&vault) == 1, 0);

        drain_test_reserve<TestCoin>(&mut vault);
        let rec = table::remove(&mut vault.deposits, 0);
        let DepositRecord { amount: _, recipient_l2: _, coin_type: _ } = rec;
        destroy_vault_empty(vault);
    }

    /// Phantom second coin type.
    #[test_only]
    public struct OtherCoin has drop {}

    #[test]
    fun deposit_accepts_any_coin_independently() {
        // Two unrelated coin types share one vault; reserves and records stay
        // independent, and both draw from the same global nonce stream.
        let ctx = &mut tx_context::dummy();
        let mut vault = test_vault(ctx);

        let coin = coin::mint_for_testing<TestCoin>(100, ctx);
        deposit<TestCoin>(&mut vault, coin, TEST_RECIPIENT, ctx);
        let other = coin::mint_for_testing<OtherCoin>(50, ctx);
        deposit<OtherCoin>(&mut vault, other, TEST_RECIPIENT, ctx);

        assert!(reserve<TestCoin>(&vault) == 100, 0);
        assert!(reserve<OtherCoin>(&vault) == 50, 0);
        assert!(deposit_nonce(&vault) == 2, 0);

        drain_test_reserve<TestCoin>(&mut vault);
        drain_test_reserve<OtherCoin>(&mut vault);
        let rec0 = table::remove(&mut vault.deposits, 0);
        let DepositRecord { amount: _, recipient_l2: _, coin_type: _ } = rec0;
        let rec1 = table::remove(&mut vault.deposits, 1);
        let DepositRecord { amount: _, recipient_l2: _, coin_type: _ } = rec1;
        destroy_vault_empty(vault);
    }

    /// Empties the vault's `T` reserve so tests can destroy the vault.
    #[test_only]
    fun drain_test_reserve<T: drop>(vault: &mut Vault) {
        let held = bag::remove<vector<u8>, Coin<T>>(&mut vault.reserves, type_key<T>());
        balance::destroy_for_testing(coin::into_balance(held));
    }

    #[test]
    fun withdraw_as_releases_idempotently_per_l2_nonce() {
        let ctx = &mut ctx_for(ALICE, 1);
        let mut vault = rotated_vault_for_testing(ctx);
        let clock = clock::create_for_testing(ctx);

        let coin = coin::mint_for_testing<TestCoin>(250, ctx);
        deposit<TestCoin>(&mut vault, coin, TEST_RECIPIENT, ctx);

        assert!(withdraw_as<TestCoin>(&mut vault, &clock, @0x1, 100, 7, ctx), 0);
        assert!(reserve<TestCoin>(&vault) == 150, 0);
        assert!(withdraw_nonce(&vault) == 1, 0);

        // Crash-retry of the same L2 burn: no-op success.
        assert!(!withdraw_as<TestCoin>(&mut vault, &clock, @0x1, 100, 7, ctx), 0);
        assert!(reserve<TestCoin>(&vault) == 150, 0);
        assert!(withdraw_nonce(&vault) == 1, 0);

        // A distinct L2 nonce releases again.
        assert!(withdraw_as<TestCoin>(&mut vault, &clock, @0x2, 50, 8, ctx), 0);
        assert!(reserve<TestCoin>(&vault) == 100, 0);

        clock::destroy_for_testing(clock);
        drain_test_reserve<TestCoin>(&mut vault);
        let rec = table::remove(&mut vault.deposits, 0);
        let DepositRecord { amount: _, recipient_l2: _, coin_type: _ } = rec;
        clear_withdrawal_for_testing<TestCoin>(&mut vault, 7);
        clear_withdrawal_for_testing<TestCoin>(&mut vault, 8);
        destroy_rotation_for_testing(&mut vault);
        destroy_vault_empty(vault);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun withdraw_as_requires_the_authority() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut vault = rotated_vault_for_testing(&mut ctx_a);
        let clock = clock::create_for_testing(&mut ctx_a);

        let mut ctx_b = ctx_for(BOB, 2);
        withdraw_as<TestCoin>(&mut vault, &clock, @0x1, 1, 7, &mut ctx_b);
        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut vault);
        destroy_vault_empty(vault);
    }

    #[test]
    fun bridge_rotation_swaps_release_authority() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut vault = rotated_vault_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let coin = coin::mint_for_testing<TestCoin>(100, &mut ctx_a);
        deposit<TestCoin>(&mut vault, coin, TEST_RECIPIENT, &mut ctx_a);
        assert!(withdraw_as<TestCoin>(&mut vault, &clock, @0x1, 10, 7, &mut ctx_a), 0);

        request_rotation(&mut vault, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut vault, &clock, &ctx_b);
        assert!(rotation::authority(rotation_state(&vault)) == BOB);

        // The new authority releases; the old one still releases in grace.
        let mut ctx_b_mut = ctx_for(BOB, 5);
        assert!(withdraw_as<TestCoin>(&mut vault, &clock, @0x2, 10, 8, &mut ctx_b_mut), 0);
        let mut ctx_a2 = ctx_for(ALICE, 3);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms() + rotation::prev_grace_ms());
        assert!(withdraw_as<TestCoin>(&mut vault, &clock, @0x3, 10, 9, &mut ctx_a2), 0);

        let ctx_b2 = ctx_for(BOB, 4);
        retire_previous(&mut vault, &ctx_b2);

        clock::destroy_for_testing(clock);
        drain_test_reserve<TestCoin>(&mut vault);
        let rec = table::remove(&mut vault.deposits, 0);
        let DepositRecord { amount: _, recipient_l2: _, coin_type: _ } = rec;
        clear_withdrawal_for_testing<TestCoin>(&mut vault, 7);
        clear_withdrawal_for_testing<TestCoin>(&mut vault, 8);
        clear_withdrawal_for_testing<TestCoin>(&mut vault, 9);
        destroy_rotation_for_testing(&mut vault);
        destroy_vault_empty(vault);
    }

    #[test, expected_failure(abort_code = E_NOT_AUTHORIZED_FOR_TEST, location = rotation)]
    fun old_bridge_authority_is_rejected_after_retire_and_grace() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut vault = rotated_vault_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        let coin = coin::mint_for_testing<TestCoin>(100, &mut ctx_a);
        deposit<TestCoin>(&mut vault, coin, TEST_RECIPIENT, &mut ctx_a);

        request_rotation(&mut vault, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        let ctx_b = ctx_for(BOB, 2);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        claim_rotation(&mut vault, &clock, &ctx_b);
        retire_previous(&mut vault, &ctx_b);

        let mut ctx_a2 = ctx_for(ALICE, 3);
        clock::set_for_testing(
            &mut clock,
            rotation::min_delay_ms() + rotation::prev_grace_ms() + 1,
        );
        withdraw_as<TestCoin>(&mut vault, &clock, @0x1, 10, 7, &mut ctx_a2);

        clock::destroy_for_testing(clock);
        drain_test_reserve<TestCoin>(&mut vault);
        let rec = table::remove(&mut vault.deposits, 0);
        let DepositRecord { amount: _, recipient_l2: _, coin_type: _ } = rec;
        destroy_rotation_for_testing(&mut vault);
        destroy_vault_empty(vault);
    }

    #[test, expected_failure(abort_code = E_NOT_CLAIM_TARGET_FOR_TEST, location = rotation)]
    fun bridge_claim_by_the_wrong_owner_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut vault = rotated_vault_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut vault, &clock, ALICE, rotation::min_delay_ms(), &ctx_a);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms());
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut vault, &clock, &ctx_b);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut vault);
        destroy_vault_empty(vault);
    }

    #[test, expected_failure(abort_code = E_CLAIM_TOO_EARLY_FOR_TEST, location = rotation)]
    fun bridge_claim_before_the_timelock_elapses_aborts() {
        let mut ctx_a = ctx_for(ALICE, 1);
        let mut vault = rotated_vault_for_testing(&mut ctx_a);
        let mut clock = clock::create_for_testing(&mut ctx_a);

        request_rotation(&mut vault, &clock, BOB, rotation::min_delay_ms(), &ctx_a);
        clock::set_for_testing(&mut clock, rotation::min_delay_ms() - 1);
        let ctx_b = ctx_for(BOB, 2);
        claim_rotation(&mut vault, &clock, &ctx_b);

        clock::destroy_for_testing(clock);
        destroy_rotation_for_testing(&mut vault);
        destroy_vault_empty(vault);
    }

    #[test, expected_failure(abort_code = E_ROTATED)]
    fun withdraw_aborts_once_the_package_rotates() {
        let ctx = &mut tx_context::dummy();
        let mut vault = test_vault(ctx);
        let cap = test_cap(ctx);

        withdraw<TestCoin>(&cap, &mut vault, @0x1, 1, 7, ctx);

        destroy_vault_empty(vault);
        let BridgeCap { id } = cap;
        id.delete();
    }
}
