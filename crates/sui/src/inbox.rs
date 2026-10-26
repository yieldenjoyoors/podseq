//! Force-inclusion inbox reads over the Sui gRPC ledger API.
//!
//! The sequencer-side counterpart to `move/sources/inbox.move`: it reads the
//! inbox's shared object and its `pending` nonce table the same way the
//! settlement and bridge modules read chain state — raw BCS via `get_object`,
//! one dynamic-field object per nonce (the gRPC read API has no event query).
//!
//! All transport goes through [`InboxTransport`], so callers above the read API
//! (the sequencer's inclusion tracker) can be tested against a mocked
//! transport without a Sui node.

use std::future::Future;
use std::pin::Pin;

use sui_sdk_types::Address;
use thiserror::Error;

use crate::settlement::{field_contents, object_contents};

/// A boxed, sendable future returned by the transport trait, so the trait is
/// object-safe (`Arc<dyn InboxTransport>`) without an async-trait dependency.
pub type InboxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors from reading force-inclusion inbox state over Sui.
#[derive(Debug, Error)]
pub enum InboxError {
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("parse error: {0}")]
    Parse(String),
}

/// Snapshot of the shared `Inbox` object, mirrored from
/// `move/sources/inbox.move`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxStatus {
    /// Next nonce that `inbox::submit` will assign.
    pub next_nonce: u64,
    /// Count of nonces included in settled L2 blocks (the settle cursor).
    pub included_count: u64,
    /// Timestamp of the most recently settled block.
    pub last_settled_ts: u64,
    /// UID of the `pending` table; fixed at `initialize`, reused for
    /// [`InboxReader::pending_at`] lookups.
    pub pending_table_uid: Address,
}

/// One submitted-but-not-included transaction, mirrored from `inbox::PendingTx`
/// (the value of a `Field<u64, PendingTx>` dynamic field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTx {
    /// Latest block timestamp (ms, Sui clock) by which the transaction must be
    /// included in an L2 block.
    pub deadline_ms: u64,
    /// Raw EVM transaction bytes as submitted by the user.
    pub tx_bytes: Vec<u8>,
}

/// Read boundary over the Sui gRPC ledger API, narrowed to exactly what the
/// inbox needs: two `get_object` shapes. Tests mock this trait; production
/// wires [`SuiInboxTransport`].
pub trait InboxTransport: Send + Sync + 'static {
    /// Fetches the raw BCS contents of an object. Errors when it does not
    /// exist.
    fn object_contents<'a>(
        &'a self,
        id: &'a Address,
    ) -> InboxFuture<'a, Result<Vec<u8>, InboxError>>;

    /// Fetches the raw BCS contents of a `Table<u64, V>` entry by key.
    /// `Ok(None)` when the key has no entry.
    fn field_contents<'a>(
        &'a self,
        table: &'a Address,
        key: u64,
    ) -> InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>>;
}

/// Production transport: reads through the same helpers the settlement and
/// bridge modules use (one `get_object` per call).
#[derive(Debug, Clone)]
pub struct SuiInboxTransport {
    rpc_url: String,
}

impl SuiInboxTransport {
    /// Creates a transport against a Sui fullnode gRPC endpoint.
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            rpc_url: rpc_url.into(),
        }
    }
}

impl InboxTransport for SuiInboxTransport {
    fn object_contents<'a>(
        &'a self,
        id: &'a Address,
    ) -> InboxFuture<'a, Result<Vec<u8>, InboxError>> {
        Box::pin(async move {
            object_contents(&self.rpc_url, id, "inbox")
                .await
                .map_err(|e| match e {
                    crate::settlement::SettlementError::Execution(msg)
                        if msg.contains("not found") || msg.contains("no contents") =>
                    {
                        InboxError::NotFound(msg)
                    }
                    other => InboxError::Rpc(other.to_string()),
                })
        })
    }

    fn field_contents<'a>(
        &'a self,
        table: &'a Address,
        key: u64,
    ) -> InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
        Box::pin(async move {
            field_contents(&self.rpc_url, table, key)
                .await
                .map_err(|e| InboxError::Rpc(e.to_string()))
        })
    }
}

/// Typed reader over the force-inclusion inbox shared object.
#[derive(Debug, Clone)]
pub struct InboxReader<T> {
    transport: T,
    inbox: Address,
}

impl<T: InboxTransport> InboxReader<T> {
    /// Creates a reader for the inbox object at `inbox`.
    pub fn new(transport: T, inbox: Address) -> Self {
        Self { transport, inbox }
    }

    /// Reads the full inbox snapshot in one `get_object`.
    pub async fn status(&self) -> Result<InboxStatus, InboxError> {
        let bytes = self.transport.object_contents(&self.inbox).await?;
        parse_inbox_status(&bytes)
    }

    /// Reads the next expected inbox nonce (what `submit` will assign next).
    pub async fn next_nonce(&self) -> Result<u64, InboxError> {
        Ok(self.status().await?.next_nonce)
    }

    /// Reads one pending transaction by nonce via its dynamic-field object.
    /// `Ok(None)` when the nonce is not pending (never submitted, or already
    /// pruned by the settle cursor).
    pub async fn pending_at(&self, nonce: u64) -> Result<Option<PendingTx>, InboxError> {
        let table = self.status().await?.pending_table_uid;
        match self.transport.field_contents(&table, nonce).await? {
            None => Ok(None),
            Some(bytes) => parse_pending_tx(&bytes).map(Some),
        }
    }
}

/// Decodes an [`InboxStatus`] from raw inbox BCS contents.
///
/// Mirrors the Move layout: `Inbox { id: UID, next_nonce: u64,
/// included_count: u64, last_settled_ts: u64, pending: Table<u64, PendingTx> }`
/// (`UID`/`ID` and `Table`'s `id`/`size` flatten to their fields).
pub fn parse_inbox_status(contents: &[u8]) -> Result<InboxStatus, InboxError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct InboxBcs {
        id: Address,
        next_nonce: u64,
        included_count: u64,
        last_settled_ts: u64,
        pending: TableBcs,
    }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct TableBcs {
        id: Address,
        size: u64,
    }
    let inbox: InboxBcs =
        bcs::from_bytes(contents).map_err(|e| InboxError::Parse(format!("inbox BCS: {e}")))?;
    Ok(InboxStatus {
        next_nonce: inbox.next_nonce,
        included_count: inbox.included_count,
        last_settled_ts: inbox.last_settled_ts,
        pending_table_uid: inbox.pending.id,
    })
}

/// Decodes a [`PendingTx`] from a raw `Field<u64, PendingTx>` BCS buffer.
pub fn parse_pending_tx(field_bcs: &[u8]) -> Result<PendingTx, InboxError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FieldBcs {
        id: Address,
        name: u64,
        value: PendingTxBcs,
    }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct PendingTxBcs {
        deadline_ms: u64,
        tx_bytes: Vec<u8>,
    }
    let field: FieldBcs = bcs::from_bytes(field_bcs)
        .map_err(|e| InboxError::Parse(format!("pending-tx field BCS: {e}")))?;
    if field.value.tx_bytes.is_empty() {
        // inbox::submit aborts empty submissions (E_EMPTY_TX), so an empty
        // value on an existing entry cannot come from the contract.
        return Err(InboxError::Parse(
            "pending transaction has empty tx_bytes".into(),
        ));
    }
    Ok(PendingTx {
        deadline_ms: field.value.deadline_ms,
        tx_bytes: field.value.tx_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `Inbox { id: UID, next_nonce: u64, included_count: u64,
    /// last_settled_ts: u64, pending: Table }` under BCS. `UID`/`ID` and
    /// `Table`'s `id`/`size` flatten to their fields.
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

    fn inbox_contents(
        next_nonce: u64,
        included_count: u64,
        last_settled_ts: u64,
        table_uid: [u8; 32],
    ) -> Vec<u8> {
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
        let field = FieldMirror {
            id: Address::new([0xbb; 32]),
            name: nonce,
            value: PendingTxMirror {
                deadline_ms,
                tx_bytes: tx_bytes.to_vec(),
            },
        };
        bcs::to_bytes(&field).unwrap()
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
        ) -> InboxFuture<'a, Result<Vec<u8>, InboxError>> {
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
        ) -> InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(self.fields.get(&(*table, key)).cloned()) })
        }
    }

    fn mock_with_inbox(
        next_nonce: u64,
        included_count: u64,
        pending: &[(u64, u64, &[u8])],
    ) -> (MockTransport, Address) {
        let inbox_id = Address::new([0x11; 32]);
        let table_uid = [0x22u8; 32];
        let mut transport = MockTransport::default();
        transport.objects.insert(
            inbox_id,
            inbox_contents(next_nonce, included_count, 1_000, table_uid),
        );
        let table = Address::new(table_uid);
        for (nonce, deadline, tx) in pending {
            transport
                .fields
                .insert((table, *nonce), field_contents_bytes(*nonce, *deadline, tx));
        }
        (transport, inbox_id)
    }

    #[tokio::test]
    async fn status_reads_all_fields_from_one_get_object() {
        let (transport, inbox_id) = mock_with_inbox(7, 5, &[]);
        let reader = InboxReader::new(transport, inbox_id);

        let status = reader.status().await.unwrap();
        assert_eq!(status.next_nonce, 7);
        assert_eq!(status.included_count, 5);
        assert_eq!(status.last_settled_ts, 1_000);
        assert_eq!(status.pending_table_uid, Address::new([0x22; 32]));
    }

    #[tokio::test]
    async fn next_nonce_is_the_read_of_the_inbox_object() {
        let (transport, inbox_id) = mock_with_inbox(42, 40, &[]);
        let reader = InboxReader::new(transport, inbox_id);

        assert_eq!(reader.next_nonce().await.unwrap(), 42);
    }

    #[tokio::test]
    async fn pending_at_decodes_nonce_entries() {
        let (transport, inbox_id) =
            mock_with_inbox(2, 0, &[(0, 1_600_000, b"tx-a"), (1, 1_600_100, b"tx-b")]);
        let reader = InboxReader::new(transport, inbox_id);

        let tx = reader
            .pending_at(1)
            .await
            .unwrap()
            .expect("nonce 1 pending");
        assert_eq!(tx.deadline_ms, 1_600_100);
        assert_eq!(tx.tx_bytes, b"tx-b".to_vec());

        let first = reader
            .pending_at(0)
            .await
            .unwrap()
            .expect("nonce 0 pending");
        assert_eq!(first.deadline_ms, 1_600_000);
        assert_eq!(first.tx_bytes, b"tx-a".to_vec());
    }

    #[tokio::test]
    async fn pending_at_returns_none_for_unsubmitted_nonce() {
        let (transport, inbox_id) = mock_with_inbox(1, 0, &[(0, 1_600_000, b"tx-a")]);
        let reader = InboxReader::new(transport, inbox_id);

        assert!(reader.pending_at(5).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn transport_errors_propagate() {
        let (mut transport, inbox_id) = mock_with_inbox(0, 0, &[]);
        transport.fail_objects = true;
        let reader = InboxReader::new(transport, inbox_id);

        assert!(matches!(
            reader.next_nonce().await,
            Err(InboxError::Rpc(msg)) if msg.contains("transport down")
        ));
    }

    #[test]
    fn parse_inbox_status_reads_little_endian_fields() {
        let contents = inbox_contents(9, 3, 500, [0x44; 32]);
        let status = parse_inbox_status(&contents).unwrap();
        assert_eq!(status.next_nonce, 9);
        assert_eq!(status.included_count, 3);
        assert_eq!(status.last_settled_ts, 500);
        assert_eq!(status.pending_table_uid, Address::new([0x44; 32]));
    }

    #[test]
    fn parse_inbox_status_rejects_truncated_bcs() {
        let full = inbox_contents(0, 0, 0, [0; 32]);
        assert!(parse_inbox_status(&full[..full.len() - 1]).is_err());
        assert!(parse_inbox_status(&[0u8; 40]).is_err());
    }

    #[test]
    fn parse_pending_tx_roundtrips_against_bcs_layout() {
        let encoded = field_contents_bytes(4, 1_700_000, &[0xde, 0xad, 0xbe, 0xef]);
        let tx = parse_pending_tx(&encoded).unwrap();
        assert_eq!(tx.deadline_ms, 1_700_000);
        assert_eq!(tx.tx_bytes, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn parse_pending_tx_rejects_truncated_bcs() {
        assert!(parse_pending_tx(&[0u8; 42]).is_err());
    }

    #[test]
    fn parse_pending_tx_rejects_empty_tx_bytes() {
        // The Move module aborts empty submissions (E_EMPTY_TX), so an empty
        // value on an existing entry is a decoding/encoding bug, not a state
        // the sequencer should forward to the pool.
        let encoded = field_contents_bytes(0, 1, &[]);
        assert!(parse_pending_tx(&encoded).is_err());
    }
}
