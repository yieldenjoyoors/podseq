//! Trustless deadline verification for full nodes.
//!
//! A full node decides — from Sui state alone, without trusting the
//! sequencer — whether a force-inclusion deadline has been exceeded and the
//! chain is correctly halted. It reads three objects over the same mocked
//! transport the sequencer's inbox reader uses:
//!
//! - the shared [`InboxStatus`] (cursor, settlement witness, pending table),
//! - the oldest pending entry at `included_count` (its deadline),
//! - Sui's shared `Clock`, the same consensus-time anchor `inbox::submit`
//!   deadlines are set against.
//!
//! The contract stores no halt flag: halt is the persistent shape of the
//! state (`enforce_settle` aborts on an expired oldest entry, so every
//! further settle fails until the transaction is included). A full node
//! therefore reproduces the decision:
//!
//! - [`DeadlineStatus::Active`] — no pending deadline is expired.
//! - [`DeadlineStatus::Halted`] — the oldest deadline is expired and the
//!   witness is consistent: `last_settled_ts` is at or before the deadline,
//!   exactly what the gate allows through. The chain is correctly halted.
//! - [`DeadlineStatus::Violated`] — the oldest deadline is expired but
//!   `last_settled_ts` is past it: a settle newer than the deadline went
//!   through while the entry was pending, which the gate must refuse.
//!   Unreachable under the audited contract; loud, locally halted alarm.
//!
//! Boundary: `enforce_settle` accepts a block whose timestamp equals the
//! deadline (`deadline >= block_ts`), so a full node only calls a deadline
//! expired strictly after `deadline + margin` — the margin absorbs the gap
//! between a full node's poll and the block timestamps that can still settle.
//!
//! ## Read protocol
//!
//! The three objects are read in separate round trips, so a settle can land
//! between them and leave the first read stale. The monitor therefore never
//! asserts a halt on a possibly stale snapshot: after a `Halted`/`Violated`
//! classification the inbox is re-read, and the verdict stands only if the
//! settle cursor (`included_count`) and witness (`last_settled_ts`) are
//! unchanged — the two fields a settle updates, which is why a racing
//! `submit` (which only moves `next_nonce`) cannot invalidate a verdict. An
//! `Active` verdict needs no confirmation: a removed pending entry proves
//! the include already happened, and a stale clock only ever delays a halt.
//! If a concurrent settle invalidates every read, the poll fails with
//! [`DeadlineError::InconsistentSnapshot`] — unconfirmed, never a halt.

use std::str::FromStr;

use sui_sdk_types::Address;
use thiserror::Error;

use crate::inbox::{
    parse_inbox_status, parse_pending_tx, InboxError, InboxStatus, InboxTransport, PendingTx,
};

/// Sui's shared `Clock` object (`sui::clock::Clock`), created at genesis with
/// the well-known object id `0x6`. Consensus time; the deadline anchor.
pub const SUI_CLOCK_ID: Address =
    Address::from_static("0x0000000000000000000000000000000000000000000000000000000000000006");

/// Grace period subtracted from a deadline check, in milliseconds.
///
/// Deadlines and the clock come from the same source, so this only absorbs
/// read-ordering effects: a block stamped exactly at the deadline is legal
/// and may still settle after the wall clock passed it.
pub const DEFAULT_MARGIN_MS: u64 = 1_000;

/// Deadline status of the chain, decided from Sui state alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadlineStatus {
    /// Every pending deadline is in the future (or the inbox is empty).
    Active,
    /// The oldest pending deadline is expired and the settlement witness is
    /// consistent: the chain is correctly halted.
    Halted {
        /// Nonce of the overdue transaction (`included_count`).
        nonce: u64,
        /// The expired deadline, in Sui clock milliseconds.
        deadline_ms: u64,
    },
    /// The oldest pending deadline is expired but settlement continued past
    /// it. The settle gate must be broken; full nodes halt locally.
    Violated {
        /// Nonce of the overdue transaction (`included_count`).
        nonce: u64,
        /// The expired deadline, in Sui clock milliseconds.
        deadline_ms: u64,
        /// Witness timestamp of the settle that ran past the deadline.
        settled_ts_ms: u64,
    },
}

impl DeadlineStatus {
    /// Whether the node must stop applying blocks: a confirmed halt, or a
    /// violated gate (data no longer trustworthy).
    pub fn is_halted(&self) -> bool {
        matches!(
            self,
            DeadlineStatus::Halted { .. } | DeadlineStatus::Violated { .. }
        )
    }
}

/// Errors from deadline verification.
#[derive(Debug, Error)]
pub enum DeadlineError {
    #[error("inbox error: {0}")]
    Inbox(#[from] InboxError),
    #[error("invalid inbox id: {0}")]
    InboxId(String),
    #[error("inbox changed under all {attempts} reads; deadline status unconfirmed")]
    InconsistentSnapshot {
        /// Read attempts made before giving up.
        attempts: usize,
    },
}

/// Classifies the deadline status from a single consistent read.
///
/// `oldest` is the pending entry at `inbox.included_count`, `None` when the
/// inbox is drained. A deadline is expired when
/// `now > deadline + margin` (a settle stamped at exactly the deadline is
/// legal, so equality stays active).
pub fn classify(
    now_ms: u64,
    margin_ms: u64,
    inbox: &InboxStatus,
    oldest: Option<&PendingTx>,
) -> DeadlineStatus {
    let Some(tx) = oldest else {
        return DeadlineStatus::Active;
    };
    let nonce = inbox.included_count;
    let grace = tx.deadline_ms.saturating_add(margin_ms);
    if now_ms <= grace {
        return DeadlineStatus::Active;
    }
    if inbox.last_settled_ts <= tx.deadline_ms {
        DeadlineStatus::Halted {
            nonce,
            deadline_ms: tx.deadline_ms,
        }
    } else {
        DeadlineStatus::Violated {
            nonce,
            deadline_ms: tx.deadline_ms,
            settled_ts_ms: inbox.last_settled_ts,
        }
    }
}

/// Decodes the shared Clock's `timestamp_ms` from raw BCS contents.
///
/// Mirrors `sui::clock::Clock { id: UID, timestamp_ms: u64 }` (`UID`/`ID`
/// flatten to their fields).
pub fn parse_clock(contents: &[u8]) -> Result<u64, InboxError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct ClockBcs {
        id: Address,
        timestamp_ms: u64,
    }
    let clock: ClockBcs =
        bcs::from_bytes(contents).map_err(|e| InboxError::Parse(format!("clock BCS: {e}")))?;
    Ok(clock.timestamp_ms)
}

/// Polls the inbox and clock and classifies the chain's deadline status.
///
/// Read-only, sequencer-independent: everything it needs is Sui state any
/// full node can read.
#[derive(Debug, Clone)]
pub struct DeadlineMonitor<T> {
    transport: T,
    inbox: Address,
    margin_ms: u64,
}

impl<T: InboxTransport> DeadlineMonitor<T> {
    /// Read attempts before a halt verdict is reported as unconfirmed:
    /// every `Halted`/`Violated` classification is re-confirmed against a
    /// fresh inbox read, so a concurrent settle may invalidate the snapshot
    /// this many times.
    const CONFIRM_LIMIT: usize = 3;

    /// Creates a monitor for the inbox at `inbox`, expiring deadlines after
    /// `margin_ms` of grace.
    pub fn new(transport: T, inbox: Address, margin_ms: u64) -> Self {
        Self {
            transport,
            inbox,
            margin_ms,
        }
    }

    /// Reads the inbox, the clock, and the oldest pending entry, and returns
    /// the current deadline status.
    ///
    /// A `Halted`/`Violated` verdict is only asserted against a snapshot no
    /// concurrent settle invalidates: the inbox is re-read and its settle
    /// cursor and witness must be unchanged. If a concurrent settle keeps
    /// invalidating every read, the status is reported as
    /// [`DeadlineError::InconsistentSnapshot`] rather than asserted.
    pub async fn status(&self) -> Result<DeadlineStatus, DeadlineError> {
        for _ in 0..Self::CONFIRM_LIMIT {
            let (inbox, now_ms, oldest) = self.read_snapshot().await?;
            let status = classify(now_ms, self.margin_ms, &inbox, oldest.as_ref());
            if !status.is_halted() || self.confirm_unchanged(&inbox).await? {
                return Ok(status);
            }
        }
        Err(DeadlineError::InconsistentSnapshot {
            attempts: Self::CONFIRM_LIMIT,
        })
    }

    /// One read of the inbox object, the clock, and the oldest pending
    /// entry at the settle cursor.
    async fn read_snapshot(&self) -> Result<(InboxStatus, u64, Option<PendingTx>), DeadlineError> {
        let inbox_bytes = self.transport.object_contents(&self.inbox).await?;
        let inbox = parse_inbox_status(&inbox_bytes)?;

        let clock_bytes = self.transport.object_contents(&SUI_CLOCK_ID).await?;
        let now_ms = parse_clock(&clock_bytes)?;

        let oldest = match self
            .transport
            .field_contents(&inbox.pending_table_uid, inbox.included_count)
            .await?
        {
            None => None,
            Some(bytes) => Some(parse_pending_tx(&bytes)?),
        };
        Ok((inbox, now_ms, oldest))
    }

    /// Whether a fresh inbox read still agrees with `read` on the settle
    /// cursor and witness — the two fields a settle updates.
    async fn confirm_unchanged(&self, read: &InboxStatus) -> Result<bool, DeadlineError> {
        let confirm_bytes = self.transport.object_contents(&self.inbox).await?;
        let confirm = parse_inbox_status(&confirm_bytes)?;
        Ok(confirm.included_count == read.included_count
            && confirm.last_settled_ts == read.last_settled_ts)
    }
}

/// Builds a [`DeadlineMonitor`] from node config values.
///
/// `inbox_id` is the shared Inbox object ID from config; `rpc_url` is the Sui
/// fullnode endpoint.
pub fn monitor_from_config(
    rpc_url: &str,
    inbox_id: &str,
    margin_ms: u64,
) -> Result<DeadlineMonitor<crate::inbox::SuiInboxTransport>, DeadlineError> {
    let inbox = Address::from_str(inbox_id)
        .map_err(|_| DeadlineError::InboxId(format!("invalid sui.inbox_id: {inbox_id}")))?;
    Ok(DeadlineMonitor::new(
        crate::inbox::SuiInboxTransport::new(rpc_url),
        inbox,
        margin_ms,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `sui::clock::Clock { id: UID, timestamp_ms: u64 }`.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct ClockMirror {
        id: Address,
        timestamp_ms: u64,
    }

    fn clock_contents(timestamp_ms: u64) -> Vec<u8> {
        bcs::to_bytes(&ClockMirror {
            id: Address::new([0; 32]),
            timestamp_ms,
        })
        .unwrap()
    }

    /// Mirrors `Inbox { id: UID, next_nonce: u64, included_count: u64,
    /// last_settled_ts: u64, pending: Table }` under BCS.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct InboxMirror {
        id: Address,
        next_nonce: u64,
        included_count: u64,
        last_settled_ts: u64,
        pending: TableMirror,
    }

    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct TableMirror {
        id: Address,
        size: u64,
    }

    /// Mirrors `sui::dynamic_field::Field<u64, inbox::PendingTx>`.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct FieldMirror {
        id: Address,
        name: u64,
        value: PendingTxMirror,
    }

    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct PendingTxMirror {
        deadline_ms: u64,
        tx_bytes: Vec<u8>,
    }

    fn field_contents_bytes(nonce: u64, deadline_ms: u64, tx_bytes: &[u8]) -> Vec<u8> {
        bcs::to_bytes(&FieldMirror {
            id: Address::new([0xbb; 32]),
            name: nonce,
            value: PendingTxMirror {
                deadline_ms,
                tx_bytes: tx_bytes.to_vec(),
            },
        })
        .unwrap()
    }

    /// Builds an inbox snapshot with one oldest pending entry carrying
    /// `oldest_deadline` (`None` drains the inbox).
    fn inbox_snapshot(
        included_count: u64,
        next_nonce: u64,
        last_settled_ts: u64,
        oldest_deadline: Option<u64>,
    ) -> (InboxStatus, Option<PendingTx>) {
        let table_uid = Address::new([0x22; 32]);
        let status = InboxStatus {
            next_nonce,
            included_count,
            last_settled_ts,
            pending_table_uid: table_uid,
        };
        let oldest = oldest_deadline.map(|deadline_ms| PendingTx {
            deadline_ms,
            tx_bytes: b"tx".to_vec(),
        });
        (status, oldest)
    }

    #[test]
    fn classify_is_active_without_pending_entries() {
        let (inbox, oldest) = inbox_snapshot(3, 3, 5_000, None);

        assert_eq!(
            classify(9_000, 0, &inbox, oldest.as_ref()),
            DeadlineStatus::Active
        );
    }

    #[test]
    fn classify_is_active_before_the_deadline() {
        let (inbox, oldest) = inbox_snapshot(0, 1, 0, Some(1_600_000));

        assert_eq!(
            classify(1_599_999, 0, &inbox, oldest.as_ref()),
            DeadlineStatus::Active
        );
    }

    #[test]
    fn classify_keeps_a_deadline_stamped_now_active() {
        // enforce_settle accepts block_ts == deadline, so a full node must
        // not call the deadline expired at equality.
        let (inbox, oldest) = inbox_snapshot(0, 1, 0, Some(1_600_000));

        assert_eq!(
            classify(1_600_000, 0, &inbox, oldest.as_ref()),
            DeadlineStatus::Active
        );
    }

    #[test]
    fn classify_expires_only_after_deadline_plus_margin() {
        let (inbox, oldest) = inbox_snapshot(0, 1, 0, Some(1_600_000));

        let at_grace = classify(1_600_500, 500, &inbox, oldest.as_ref());
        assert_eq!(at_grace, DeadlineStatus::Active);

        let past_grace = classify(1_600_501, 500, &inbox, oldest.as_ref());
        assert_eq!(
            past_grace,
            DeadlineStatus::Halted {
                nonce: 0,
                deadline_ms: 1_600_000
            }
        );
    }

    #[test]
    fn classify_reports_halted_when_the_witness_is_consistent() {
        let (inbox, oldest) = inbox_snapshot(0, 1, 1_599_000, Some(1_600_000));

        assert_eq!(
            classify(1_700_000, 0, &inbox, oldest.as_ref()),
            DeadlineStatus::Halted {
                nonce: 0,
                deadline_ms: 1_600_000
            }
        );
    }

    #[test]
    fn classify_accepts_a_witness_stamped_at_the_deadline() {
        // A settle at block_ts == deadline is legal while the entry is
        // pending; the witness may equal the expired deadline.
        let (inbox, oldest) = inbox_snapshot(0, 1, 1_600_000, Some(1_600_000));

        assert_eq!(
            classify(1_700_000, 0, &inbox, oldest.as_ref()),
            DeadlineStatus::Halted {
                nonce: 0,
                deadline_ms: 1_600_000
            }
        );
    }

    #[test]
    fn classify_reports_violation_when_settlement_continued_past_the_deadline() {
        let (inbox, oldest) = inbox_snapshot(0, 1, 1_900_000, Some(1_600_000));

        assert_eq!(
            classify(2_000_000, 0, &inbox, oldest.as_ref()),
            DeadlineStatus::Violated {
                nonce: 0,
                deadline_ms: 1_600_000,
                settled_ts_ms: 1_900_000
            }
        );
    }

    #[test]
    fn classify_reports_the_oldest_pending_nonce() {
        let (inbox, oldest) = inbox_snapshot(4, 6, 0, Some(1_000));

        assert_eq!(
            classify(5_000, 0, &inbox, oldest.as_ref()),
            DeadlineStatus::Halted {
                nonce: 4,
                deadline_ms: 1_000
            }
        );
    }

    #[test]
    fn is_halted_covers_halt_and_violation_only() {
        assert!(!DeadlineStatus::Active.is_halted());
        assert!(DeadlineStatus::Halted {
            nonce: 0,
            deadline_ms: 1
        }
        .is_halted());
        assert!(DeadlineStatus::Violated {
            nonce: 0,
            deadline_ms: 1,
            settled_ts_ms: 2
        }
        .is_halted());
    }

    #[test]
    fn parse_clock_reads_timestamp_ms() {
        assert_eq!(parse_clock(&clock_contents(1_234_567)).unwrap(), 1_234_567);
    }

    #[test]
    fn parse_clock_rejects_truncated_bcs() {
        assert!(parse_clock(&[0u8; 39]).is_err());
        let full = clock_contents(1);
        assert!(parse_clock(&full[..full.len() - 1]).is_err());
    }

    /// In-memory transport: objects and dynamic-field entries served from
    /// maps, mirroring the two `get_object` shapes the real transport issues.
    #[derive(Default)]
    struct MockTransport {
        objects: std::collections::BTreeMap<Address, Vec<u8>>,
        fields: std::collections::BTreeMap<(Address, u64), Vec<u8>>,
        fail_objects: bool,
    }

    impl InboxTransport for MockTransport {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> crate::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            Box::pin(async move {
                if self.fail_objects {
                    return Err(InboxError::Rpc("transport down".into()));
                }
                self.objects
                    .get(id)
                    .cloned()
                    .ok_or_else(|| InboxError::NotFound(format!("no object {id}")))
            })
        }

        fn field_contents<'a>(
            &'a self,
            table: &'a Address,
            key: u64,
        ) -> crate::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(self.fields.get(&(*table, key)).cloned()) })
        }
    }

    /// Wires a mock with an inbox object, a clock, and one pending entry.
    fn mock_chain(
        included_count: u64,
        next_nonce: u64,
        last_settled_ts: u64,
        oldest: Option<(u64, u64)>,
        now_ms: u64,
    ) -> (MockTransport, Address) {
        let inbox_id = Address::new([0x11; 32]);
        let table_uid = [0x22u8; 32];
        let mut transport = MockTransport::default();
        transport.objects.insert(inbox_id, {
            let inbox = InboxMirror {
                id: Address::new([0; 32]),
                next_nonce,
                included_count,
                last_settled_ts,
                pending: TableMirror {
                    id: Address::new(table_uid),
                    size: next_nonce - included_count,
                },
            };
            bcs::to_bytes(&inbox).unwrap()
        });
        transport
            .objects
            .insert(SUI_CLOCK_ID, clock_contents(now_ms));
        if let Some((nonce, deadline)) = oldest {
            transport.fields.insert(
                (Address::new(table_uid), nonce),
                field_contents_bytes(nonce, deadline, b"tx"),
            );
        }
        (transport, inbox_id)
    }

    #[tokio::test]
    async fn monitor_reports_active_end_to_end() {
        let (transport, inbox_id) = mock_chain(2, 3, 1_000, Some((2, 900_000)), 500_000);
        let monitor = DeadlineMonitor::new(transport, inbox_id, 1_000);

        assert_eq!(monitor.status().await.unwrap(), DeadlineStatus::Active);
    }

    #[tokio::test]
    async fn monitor_reports_halted_end_to_end() {
        let (transport, inbox_id) = mock_chain(2, 3, 800_000, Some((2, 900_000)), 1_000_000);
        let monitor = DeadlineMonitor::new(transport, inbox_id, 1_000);

        assert_eq!(
            monitor.status().await.unwrap(),
            DeadlineStatus::Halted {
                nonce: 2,
                deadline_ms: 900_000
            }
        );
    }

    #[tokio::test]
    async fn monitor_reports_violated_end_to_end() {
        let (transport, inbox_id) = mock_chain(2, 3, 950_000, Some((2, 900_000)), 1_000_000);
        let monitor = DeadlineMonitor::new(transport, inbox_id, 1_000);

        assert_eq!(
            monitor.status().await.unwrap(),
            DeadlineStatus::Violated {
                nonce: 2,
                deadline_ms: 900_000,
                settled_ts_ms: 950_000
            }
        );
    }

    #[tokio::test]
    async fn monitor_stays_active_when_the_inbox_is_drained() {
        let (transport, inbox_id) = mock_chain(3, 3, 1_000, None, 5_000_000);
        let monitor = DeadlineMonitor::new(transport, inbox_id, 1_000);

        assert_eq!(monitor.status().await.unwrap(), DeadlineStatus::Active);
    }

    #[tokio::test]
    async fn monitor_propagates_transport_errors() {
        let (mut transport, inbox_id) = mock_chain(0, 0, 0, None, 0);
        transport.fail_objects = true;
        let monitor = DeadlineMonitor::new(transport, inbox_id, 1_000);

        assert!(matches!(
            monitor.status().await,
            Err(DeadlineError::Inbox(InboxError::Rpc(msg))) if msg.contains("transport down")
        ));
    }

    #[test]
    fn monitor_from_config_parses_the_inbox_id() {
        let monitor = monitor_from_config(
            "http://localhost:9000",
            "0x0000000000000000000000000000000000000000000000000000000000000006",
            DEFAULT_MARGIN_MS,
        )
        .unwrap();
        // Round-trips the id: the built monitor reads that object.
        assert_eq!(monitor.inbox, SUI_CLOCK_ID);
    }

    #[test]
    fn monitor_from_config_rejects_a_garbage_inbox_id() {
        assert!(matches!(
            monitor_from_config("http://localhost:9000", "not-an-address", 0),
            Err(DeadlineError::InboxId(_))
        ));
    }

    #[test]
    fn sui_clock_id_is_the_genesis_clock_object() {
        assert_eq!(
            SUI_CLOCK_ID,
            Address::from_str("0x6").unwrap(),
            "Sui's shared Clock is the well-known object 0x6"
        );
    }

    /// Transport with a scripted sequence of inbox versions: each inbox read
    /// serves the next version (the last one repeats), so tests can model Sui
    /// state changing between the monitor's reads. Clock and pending-table
    /// reads are static maps, and inbox reads are counted. The script is
    /// mutex-guarded because the transport trait reads through `&self`.
    struct ScriptedTransport {
        inbox_id: Address,
        script: std::sync::Mutex<Script>,
        clock: Vec<u8>,
        fields: std::collections::BTreeMap<(Address, u64), Vec<u8>>,
    }

    struct Script {
        inbox_versions: std::collections::VecDeque<Vec<u8>>,
        /// Re-queue every popped version so reads alternate forever instead
        /// of settling on the last one.
        cycle: bool,
        inbox_reads: usize,
    }

    impl ScriptedTransport {
        fn inbox_reads(&self) -> usize {
            self.script.lock().unwrap().inbox_reads
        }
    }

    /// BCS bytes of an `InboxMirror` with the given cursor state.
    fn inbox_bytes(
        table_uid: Address,
        next_nonce: u64,
        included_count: u64,
        last_settled_ts: u64,
    ) -> Vec<u8> {
        let inbox = InboxMirror {
            id: Address::new([0; 32]),
            next_nonce,
            included_count,
            last_settled_ts,
            pending: TableMirror {
                id: table_uid,
                size: next_nonce - included_count,
            },
        };
        bcs::to_bytes(&inbox).unwrap()
    }

    impl InboxTransport for ScriptedTransport {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> crate::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            Box::pin(async move {
                if *id == SUI_CLOCK_ID {
                    return Ok(self.clock.clone());
                }
                if *id == self.inbox_id {
                    let mut script = self.script.lock().unwrap();
                    script.inbox_reads += 1;
                    let last = script.inbox_versions.len() == 1 && !script.cycle;
                    let bytes = if last {
                        script
                            .inbox_versions
                            .front()
                            .cloned()
                            .expect("scripted inbox version")
                    } else {
                        let popped = script
                            .inbox_versions
                            .pop_front()
                            .expect("scripted inbox version");
                        if script.cycle {
                            script.inbox_versions.push_back(popped.clone());
                        }
                        popped
                    };
                    return Ok(bytes);
                }
                Err(InboxError::NotFound(format!("no object {id}")))
            })
        }

        fn field_contents<'a>(
            &'a self,
            table: &'a Address,
            key: u64,
        ) -> crate::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(self.fields.get(&(*table, key)).cloned()) })
        }
    }

    impl InboxTransport for std::sync::Arc<ScriptedTransport> {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> crate::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            (**self).object_contents(id)
        }

        fn field_contents<'a>(
            &'a self,
            table: &'a Address,
            key: u64,
        ) -> crate::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            (**self).field_contents(table, key)
        }
    }

    /// Builds a scripted transport whose first inbox read serves the
    /// pre-settle state (cursor 2, witness `pre_witness`, oldest entry
    /// pending at deadline `900_000`) and whose next read serves `post`:
    /// `(next_nonce, included_count, witness)`. The clock reads `1_000_000`
    /// ms, past the deadline's grace window.
    fn raced_transport(
        pre_witness: u64,
        post: (u64, u64, u64),
    ) -> std::sync::Arc<ScriptedTransport> {
        let inbox_id = Address::new([0x11; 32]);
        let table_uid = Address::new([0x22; 32]);
        std::sync::Arc::new(ScriptedTransport {
            inbox_id,
            script: std::sync::Mutex::new(Script {
                inbox_versions: std::collections::VecDeque::from(vec![
                    inbox_bytes(table_uid, 3, 2, pre_witness),
                    inbox_bytes(table_uid, post.0, post.1, post.2),
                ]),
                cycle: false,
                inbox_reads: 0,
            }),
            clock: clock_contents(1_000_000),
            fields: std::collections::BTreeMap::from([(
                (table_uid, 2),
                field_contents_bytes(2, 900_000, b"tx"),
            )]),
        })
    }

    /// Builds a scripted transport whose reads alternate forever between two
    /// halted-looking inbox states that differ in their witness, so no read
    /// sequence ever stabilizes.
    fn churning_transport() -> std::sync::Arc<ScriptedTransport> {
        let inbox_id = Address::new([0x11; 32]);
        let table_uid = Address::new([0x22; 32]);
        std::sync::Arc::new(ScriptedTransport {
            inbox_id,
            script: std::sync::Mutex::new(Script {
                inbox_versions: std::collections::VecDeque::from(vec![
                    inbox_bytes(table_uid, 3, 2, 800_000),
                    inbox_bytes(table_uid, 3, 2, 950_000),
                ]),
                cycle: true,
                inbox_reads: 0,
            }),
            clock: clock_contents(1_000_000),
            fields: std::collections::BTreeMap::from([(
                (table_uid, 2),
                field_contents_bytes(2, 900_000, b"tx"),
            )]),
        })
    }

    #[tokio::test]
    async fn a_settle_between_reads_cannot_flap_a_healthy_chain_to_halted() {
        // Read 1 serves the pre-settle inbox: cursor 2, witness before the
        // deadline, oldest entry still present. The next inbox read serves
        // the post-settle inbox: the entry was included and the witness
        // moved past the deadline. The stale first read classifies as
        // Halted; the monitor must not assert that halt on a snapshot that
        // a concurrent settle invalidates.
        let transport = raced_transport(800_000, (3, 3, 950_000));
        let monitor = DeadlineMonitor::new(transport.clone(), transport.inbox_id, 1_000);

        assert_eq!(monitor.status().await.unwrap(), DeadlineStatus::Active);
    }

    #[tokio::test]
    async fn a_witness_only_change_between_reads_reclassifies_before_halting() {
        // Same race, but the concurrent settle does not include the pending
        // entry — it only moves the witness past the deadline. After the
        // retry the truthful classification is Violated, not Halted.
        let transport = raced_transport(800_000, (3, 2, 950_000));
        let monitor = DeadlineMonitor::new(transport.clone(), transport.inbox_id, 1_000);

        assert_eq!(
            monitor.status().await.unwrap(),
            DeadlineStatus::Violated {
                nonce: 2,
                deadline_ms: 900_000,
                settled_ts_ms: 950_000,
            }
        );
    }

    #[tokio::test]
    async fn persistent_inbox_churn_never_yields_an_unconfirmed_halt() {
        // Every read is invalidated by a concurrent settle alternating the
        // witness, so no snapshot ever stabilizes. The monitor must report
        // the reads as unconfirmed instead of asserting a halt verdict it
        // could not confirm.
        let transport = churning_transport();
        let monitor = DeadlineMonitor::new(transport.clone(), transport.inbox_id, 1_000);

        assert!(matches!(
            monitor.status().await,
            Err(DeadlineError::InconsistentSnapshot { .. })
        ));
    }

    /// A stable transport serving one inbox version forever, with read
    /// counting.
    fn stable_transport(
        next_nonce: u64,
        included_count: u64,
        last_settled_ts: u64,
        oldest: Option<(u64, u64)>,
    ) -> std::sync::Arc<ScriptedTransport> {
        let inbox_id = Address::new([0x11; 32]);
        let table_uid = Address::new([0x22; 32]);
        let mut fields = std::collections::BTreeMap::new();
        if let Some((nonce, deadline)) = oldest {
            fields.insert(
                (table_uid, nonce),
                field_contents_bytes(nonce, deadline, b"tx"),
            );
        }
        std::sync::Arc::new(ScriptedTransport {
            inbox_id,
            script: std::sync::Mutex::new(Script {
                inbox_versions: std::collections::VecDeque::from(vec![inbox_bytes(
                    table_uid,
                    next_nonce,
                    included_count,
                    last_settled_ts,
                )]),
                cycle: false,
                inbox_reads: 0,
            }),
            clock: clock_contents(1_000_000),
            fields,
        })
    }

    #[tokio::test]
    async fn an_active_classification_skips_the_confirmation_read() {
        // Active verdicts need no confirmation: a removed pending entry
        // proves the include already happened, and a stale clock only ever
        // delays a halt. The common poll must stay at one inbox read.
        let transport = stable_transport(3, 3, 1_000, None);
        let monitor = DeadlineMonitor::new(transport.clone(), transport.inbox_id, 1_000);

        assert_eq!(monitor.status().await.unwrap(), DeadlineStatus::Active);
        assert_eq!(transport.inbox_reads(), 1);
    }

    #[tokio::test]
    async fn a_confirmed_halt_costs_one_extra_read() {
        // The halt path pays exactly one confirmation read when the state is
        // stable.
        let transport = stable_transport(3, 2, 800_000, Some((2, 900_000)));
        let monitor = DeadlineMonitor::new(transport.clone(), transport.inbox_id, 1_000);

        assert_eq!(
            monitor.status().await.unwrap(),
            DeadlineStatus::Halted {
                nonce: 2,
                deadline_ms: 900_000,
            }
        );
        assert_eq!(transport.inbox_reads(), 2);
    }
}
