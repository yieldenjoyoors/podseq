//! Fuzzes the cap-rotation read model: BCS parsing, role derivation, and
//! plan folding under adversarial timestamps and authority records.
//!
//! The state machine is total (no panics), and the folded plan must agree
//! with the per-object role — the table in docs/src/cap-rotation.md
//! restated as an oracle.

#![no_main]

use libfuzzer_sys::fuzz_target;
use podseq_sui::rotation::{
    derive_role, parse_rotation_state, plan, PreviousAuthority, PendingRotation, Role, RotationState,
};
use sui_sdk_types::Address;

/// Extracts a u64 biased toward the boundaries the state machine compares
/// against: zero, max, and values near the delay/grace/window constants.
fn pick(data: &[u8], i: usize) -> u64 {
    let raw = u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
    match raw % 6 {
        0 => 0,
        1 => u64::MAX,
        2 => raw.wrapping_add(600_000), // MIN_DELAY_MS / PREV_GRACE_MS
        3 => raw.wrapping_add(604_800_000), // CLAIM_WINDOW_MS
        4 => raw.wrapping_sub(604_800_000),
        _ => raw,
    }
}

fuzz_target!(|data: &[u8]| {
    // The wire parser must never panic on arbitrary bytes.
    let _ = parse_rotation_state(data);

    // pick() reads u64s at slots 0..=8 (bytes 0..72); direct indices reach
    // byte 24. Shorter inputs only exercise the parser above.
    if data.len() < 72 {
        return;
    }
    let state = RotationState {
        authority: Address::new([data[0]; 32]),
        prev: if data[1] & 1 == 0 {
            None
        } else {
            Some(PreviousAuthority {
                // The prev record can name any address; the role table must
                // only treat the *active* key's record as an adoption case.
                addr: Address::new([data[2]; 32]),
                until_ms: pick(data, 3),
            })
        },
        pending: if data[4] & 1 == 0 {
            None
        } else {
            Some(PendingRotation {
                to: Address::new([data[5]; 32]),
                eligible_at_ms: pick(data, 6),
            })
        },
    };
    let active = Address::new([data[24]; 32]);
    let staged = Address::new([data[7]; 32]);
    let now = pick(data, 8);

    let role = derive_role(&state, now, &active, &staged);
    let folded = plan(&state, &state, now, &active, &staged);

    // Oracle: with both objects in the same state, the plan is exactly the
    // fold of that one role.
    match (&role, &folded) {
        (Role::Misconfigured, _) => {
            assert_eq!(folded, podseq_sui::rotation::Plan::Misconfigured)
        }
        (Role::ClaimReady, _) => assert_eq!(folded, podseq_sui::rotation::Plan::Claim),
        (Role::Adopt { .. }, _) => assert_eq!(folded, podseq_sui::rotation::Plan::Adopt),
        (Role::Active, _) | (Role::WaitingClaim { .. }, _) => {
            assert_eq!(folded, podseq_sui::rotation::Plan::Idle)
        }
        (Role::StagedIsAuthority, _) => {
            let expected = if state.prev.is_some() {
                podseq_sui::rotation::Plan::Retire
            } else {
                podseq_sui::rotation::Plan::Idle
            };
            assert_eq!(folded, expected);
        }
    }
});
