//! Fuzzes the settlement deadline path: the halt classifier and the clock
//! parser under adversarial timestamps.
//!
//! Oracle: the documented boundary — a deadline is expired strictly after
//! `deadline + margin` (equality stays active) — and the Active/Halted/
//! Violated split over `last_settled_ts`. The oracle computes the boundary
//! independently, so an arithmetic regression (plain `+` overflow, `<`
//! instead of `<=`) surfaces as a failed assertion instead of a silent
//! halt-semantics change.

#![no_main]

use libfuzzer_sys::fuzz_target;
use podseq_sui::deadline::{classify, parse_clock, DeadlineStatus};
use podseq_sui::inbox::{InboxStatus, PendingTx};

fuzz_target!(|data: &[u8]| {
    // The clock parser must never panic on arbitrary bytes.
    let _ = parse_clock(data);

    if data.len() < 41 {
        return;
    }
    let u = |i: usize| u64::from_le_bytes(data[i * 8..i * 8 + 8].try_into().unwrap());
    let now = u(0);
    let margin = u(1);
    let deadline = u(2);
    let settled_ts = u(3);
    let included = u(4);

    let inbox = InboxStatus {
        next_nonce: included.wrapping_add(1),
        included_count: included,
        last_settled_ts: settled_ts,
        pending_table_uid: sui_sdk_types::Address::new([0u8; 32]),
    };
    let oldest = if data[40] & 1 == 0 {
        None
    } else {
        Some(PendingTx {
            deadline_ms: deadline,
            tx_bytes: Vec::new(),
        })
    };

    let status = classify(now, margin, &inbox, oldest.as_ref());

    match (&status, &oldest) {
        (DeadlineStatus::Active, None) => {}
        (DeadlineStatus::Active, Some(tx)) => {
            assert!(now <= tx.deadline_ms.saturating_add(margin))
        }
        (DeadlineStatus::Halted { .. }, Some(tx)) => {
            assert!(now > tx.deadline_ms.saturating_add(margin));
            assert!(inbox.last_settled_ts <= tx.deadline_ms);
        }
        (DeadlineStatus::Violated { .. }, Some(tx)) => {
            assert!(now > tx.deadline_ms.saturating_add(margin));
            assert!(inbox.last_settled_ts > tx.deadline_ms);
        }
        _ => panic!("classify returned a status inconsistent with its inputs"),
    }
    assert_eq!(
        status.is_halted(),
        !matches!(status, DeadlineStatus::Active)
    );
});
