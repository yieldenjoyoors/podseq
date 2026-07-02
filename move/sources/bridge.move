/// Podseq enshrined bridge vault.
///
/// Holds Sui coins locked by users until they withdraw them back to Sui. The L2
/// side (`solidity/Bridge.sol`) mirrors each deposit as a mint and each burn as a
/// withdrawal request. The sequencer, holding `BridgeCap`, is the only party that
/// can release coins, so the bridge needs no external relayer.
///
/// Deposits are generic over the coin type: `deposit<SUI>` locks SUI, `deposit<USDS>`
/// locks USDSui, etc. Each coin type is tracked independently in a `Bag` keyed by
/// its `TypeName`, so a single vault serves every bridged asset.
///
/// The Sui gRPC read API has no event query, so each deposit is also stored in a
/// `Table<u64, DepositRecord>` keyed by its nonce. The relayer reads deposits the
/// same way a full node reads settled heights: by fetching each nonce's dynamic
/// field object id. This mirrors `settlement::commitment_at`.
module podseq::bridge {
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
    /// bridged token. Anyone may call this; the locked coins can only leave via
    /// [`withdraw`], which is gated by `BridgeCap`.
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

    /// Releases `amount` of coin type `T` to `recipient` on Sui.
    ///
    /// Only the `BridgeCap` holder (the sequencer) calls this, in response to a
    /// burn observed on L2. `l2_nonce` is the L2 withdrawal nonce that triggered
    /// the release, replayed in the event so indexers can correlate the two sides.
    ///
    /// Idempotent per `(T, l2_nonce)`: re-submitting the same burn is a no-op that
    /// returns `false` (transaction still succeeds). This is what makes a relayer
    /// crash safe — the relayer advances its cursor on tx success, and a replay
    /// can never release coins twice.
    public fun withdraw<T>(
        _cap: &BridgeCap,
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
}
