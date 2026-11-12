//! Sui gas top-up: keeps the settlement signer funded from a reserve key.
//!
//! An empty settlement key stalls settlement, and a stalled settlement is
//! indistinguishable from censorship to deadline verification — a funded key
//! is what keeps the force-inclusion machinery honest. The watcher (node
//! side) polls both balances every cycle and applies the [`plan`] policy;
//! the transfer itself is a native PTB (SplitCoins from the reserve's gas
//! coin, TransferObjects to the settlement signer), signed by the reserve
//! key.

use sui_sdk_types::Address;
use sui_transaction_builder::TransactionBuilder;

use crate::rotation::submit_rotation_tx;
use crate::settlement::SettlementError;

/// Thresholds for the top-up policy, in mist.
///
/// Defaults are sized against the measured per-settle cost (see the exit
/// queue gas table): at a 2 s settle cadence the settlement key burns on the
/// order of 200 SUI/day, so the floor buys days of headroom and the dose
/// refills for over a week.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopUpPolicy {
    /// Top up once the settlement signer's balance falls strictly below
    /// this. Zero disables top-ups entirely.
    pub min_balance_mist: u64,
    /// Amount moved per top-up. Zero disables top-ups entirely.
    pub topup_amount_mist: u64,
    /// The reserve never dips below this: a dose that would undercut the
    /// floor is refused.
    pub reserve_floor_mist: u64,
}

impl Default for TopUpPolicy {
    fn default() -> Self {
        Self {
            min_balance_mist: 500_000_000_000,
            topup_amount_mist: 2_000_000_000_000,
            reserve_floor_mist: 500_000_000_000,
        }
    }
}

/// One balance-poll verdict for the settlement signer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopUpDecision {
    /// At or above the floor, or a degenerate configuration.
    Noop,
    /// Move this much from the reserve to the settlement signer.
    TopUp { amount_mist: u64 },
    /// The settlement signer is below the floor, but the reserve cannot pay
    /// without violating its own floor (or cannot pay at all).
    ReserveTooLow,
}

/// What the watcher should do after one poll.
///
/// [`Action::Skip`] covers healthy chains, degenerate configuration, and
/// failed reads alike: with a missing balance the watcher must not move
/// coins it cannot size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Skip,
    TopUp { amount_mist: u64 },
    ReserveTooLow { reserve_mist: u64, needed_mist: u64 },
}

/// Applies the top-up policy to one pair of balances.
pub fn plan(settler_mist: u64, reserve_mist: u64, policy: &TopUpPolicy) -> TopUpDecision {
    if settler_mist >= policy.min_balance_mist || policy.topup_amount_mist == 0 {
        return TopUpDecision::Noop;
    }
    if reserve_mist < policy.topup_amount_mist
        || reserve_mist - policy.topup_amount_mist < policy.reserve_floor_mist
    {
        return TopUpDecision::ReserveTooLow;
    }
    TopUpDecision::TopUp {
        amount_mist: policy.topup_amount_mist,
    }
}

/// Wraps [`plan`] with the read-failure rule: any missing balance skips the
/// cycle (fail closed — never move coins on stale data).
pub fn evaluate(
    settler_mist: Option<u64>,
    reserve_mist: Option<u64>,
    policy: &TopUpPolicy,
) -> Action {
    let (Some(settler), Some(reserve)) = (settler_mist, reserve_mist) else {
        return Action::Skip;
    };
    match plan(settler, reserve, policy) {
        TopUpDecision::Noop => Action::Skip,
        TopUpDecision::TopUp { amount_mist } => Action::TopUp { amount_mist },
        TopUpDecision::ReserveTooLow => Action::ReserveTooLow {
            reserve_mist: reserve,
            needed_mist: policy.topup_amount_mist,
        },
    }
}

/// Appends the reserve-funded top-up to `builder` as a native PTB:
/// `SplitCoins(gas, [amount])` followed by
/// `TransferObjects([coin], recipient)`.
///
/// The reserve key is the transaction sender (set by the submit path); its
/// gas coin is both the fee payer and the source of the split.
pub fn append_transfer(builder: &mut TransactionBuilder, recipient: Address, amount_mist: u64) {
    let gas = builder.gas();
    let amount = builder.pure(&amount_mist);
    let coins = builder.split_coins(gas, vec![amount]);
    let recipient_arg = builder.pure(&recipient);
    builder.transfer_objects(coins, recipient_arg);
}

/// Builds, signs, and confirms a top-up PTB signed by the reserve key.
///
/// # Errors
///
/// Rpc/Build/Execution errors from the submit path (an on-chain abort
/// arrives as `Execution`).
pub async fn submit_top_up(
    rpc_url: &str,
    reserve_key: &sui_crypto::ed25519::Ed25519PrivateKey,
    recipient: Address,
    amount_mist: u64,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, reserve_key, "gas top-up", |builder| {
        append_transfer(builder, recipient, amount_mist);
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    use sui_sdk_types::{Argument, Command, Digest};
    use sui_transaction_builder::ObjectInput;

    fn policy() -> TopUpPolicy {
        TopUpPolicy {
            min_balance_mist: 10_000,
            topup_amount_mist: 50_000,
            reserve_floor_mist: 10_000,
        }
    }

    #[test]
    fn at_the_floor_is_a_noop() {
        assert_eq!(plan(10_000, 1_000_000, &policy()), TopUpDecision::Noop);
    }

    #[test]
    fn strictly_below_the_floor_tops_up_a_fixed_dose() {
        assert_eq!(
            plan(9_999, 1_000_000, &policy()),
            TopUpDecision::TopUp {
                amount_mist: 50_000
            }
        );
    }

    #[test]
    fn reserve_below_the_dose_refuses() {
        assert_eq!(plan(0, 49_999, &policy()), TopUpDecision::ReserveTooLow);
    }

    #[test]
    fn dose_that_would_undercut_the_reserve_floor_refuses() {
        // 60_000 - 50_000 = 10_000 is exactly the floor: allowed.
        assert_eq!(
            plan(0, 60_000, &policy()),
            TopUpDecision::TopUp {
                amount_mist: 50_000
            }
        );
        // 59_999 would leave the reserve one mist under its floor: refused.
        assert_eq!(plan(0, 59_999, &policy()), TopUpDecision::ReserveTooLow);
    }

    #[test]
    fn zero_dose_never_moves_coins() {
        let p = TopUpPolicy {
            min_balance_mist: 10_000,
            topup_amount_mist: 0,
            reserve_floor_mist: 0,
        };
        assert_eq!(plan(0, 0, &p), TopUpDecision::Noop);
    }

    #[test]
    fn zero_floor_disables_topups_entirely() {
        let p = TopUpPolicy {
            min_balance_mist: 0,
            topup_amount_mist: 50_000,
            reserve_floor_mist: 0,
        };
        assert_eq!(plan(0, 1_000_000, &p), TopUpDecision::Noop);
    }

    #[test]
    fn failed_reads_fail_closed() {
        assert_eq!(evaluate(None, Some(1_000_000), &policy()), Action::Skip);
        assert_eq!(evaluate(Some(0), None, &policy()), Action::Skip);
        assert_eq!(evaluate(None, None, &policy()), Action::Skip);
    }

    #[test]
    fn evaluate_maps_the_plan_verdicts() {
        assert_eq!(evaluate(Some(20_000), Some(1_000), &policy()), Action::Skip);
        assert_eq!(
            evaluate(Some(0), Some(1_000_000), &policy()),
            Action::TopUp {
                amount_mist: 50_000
            }
        );
        assert_eq!(
            evaluate(Some(0), Some(5_000), &policy()),
            Action::ReserveTooLow {
                reserve_mist: 5_000,
                needed_mist: 50_000
            }
        );
    }

    #[test]
    fn transfer_ptb_splits_gas_and_pays_the_recipient() {
        let reserve = Address::new([0x01; 32]);
        let recipient = Address::new([0x02; 32]);

        let mut builder = TransactionBuilder::new();
        builder.set_sender(reserve);
        builder.add_gas_objects(vec![ObjectInput::owned(
            Address::new([0x03; 32]),
            1,
            Digest::new([0x04; 32]),
        )]);
        builder.set_gas_budget(50_000_000);
        builder.set_gas_price(1_000);
        append_transfer(&mut builder, recipient, 123_456);

        let tx = builder.try_build().expect("offline build");
        assert_eq!(tx.sender, reserve);

        let sui_sdk_types::ProgrammableTransaction { commands, .. } = match &tx.kind {
            sui_sdk_types::TransactionKind::ProgrammableTransaction(p) => p,
            other => panic!("expected a programmable transaction, got {other:?}"),
        };
        assert_eq!(commands.len(), 2, "SplitCoins + TransferObjects");

        let sui_sdk_types::SplitCoins { coin, amounts } = match &commands[0] {
            Command::SplitCoins(c) => c,
            other => panic!("expected SplitCoins first, got {other:?}"),
        };
        assert_eq!(*coin, Argument::Gas, "the reserve's gas coin is the source");
        assert_eq!(amounts, &[Argument::Input(0)], "the dose is the only input");

        let sui_sdk_types::TransferObjects { objects, address } = match &commands[1] {
            Command::TransferObjects(t) => t,
            other => panic!("expected TransferObjects second, got {other:?}"),
        };
        assert_eq!(
            objects,
            &[Argument::NestedResult(0, 0)],
            "the split coin moves"
        );
        assert_eq!(
            *address,
            Argument::Input(1),
            "recipient is the second input"
        );
    }
}
