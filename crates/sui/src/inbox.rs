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
use std::sync::Arc;

use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::{Address, StructTag, TypeTag};
use sui_transaction_builder::ObjectInput;
use thiserror::Error;

use crate::deadline::SUI_CLOCK_ID;
use crate::settlement::{field_contents, object_contents, SettlementError};

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

/// Forwards the transport trait through `Arc<dyn InboxTransport>`, so
/// [`InboxReader`] can wrap the shared, object-safe transport the node and
/// monitor code already hold.
impl InboxTransport for Arc<dyn InboxTransport> {
    fn object_contents<'a>(
        &'a self,
        id: &'a Address,
    ) -> InboxFuture<'a, Result<Vec<u8>, InboxError>> {
        (**self).object_contents(id)
    }

    fn field_contents<'a>(
        &'a self,
        table: &'a Address,
        key: u64,
    ) -> InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
        (**self).field_contents(table, key)
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

    /// Reads the inclusion window at consensus time `now_ms`. Absent dynamic
    /// fields mean the default window — never an error — mirroring
    /// `inbox::effective_window_ms`.
    ///
    /// # Errors
    ///
    /// Rpc failures from the transport, or BCS decode errors on present
    /// fields.
    pub async fn window(&self, package: Address, now_ms: u64) -> Result<WindowView, InboxError> {
        let override_ms = match self
            .transport
            .object_contents(&window_field_id(self.inbox, package))
            .await
        {
            Ok(bytes) => Some(parse_window_override(&bytes)?),
            Err(InboxError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        let pending = match self
            .transport
            .object_contents(&pending_window_field_id(self.inbox, package))
            .await
        {
            Ok(bytes) => Some(parse_pending_window(&bytes)?),
            Err(InboxError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        let effective_ms = effective_window_ms(override_ms, pending.as_ref(), now_ms);
        Ok(WindowView {
            override_ms,
            pending,
            effective_ms,
        })
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

/// Default inclusion window (ms); mirrors `inbox::INCLUSION_WINDOW_MS` —
/// what an inbox with no stored override charges.
pub const DEFAULT_WINDOW_MS: u64 = 600_000;
/// Smallest adjustable window; mirrors `inbox::MIN_WINDOW_MS`.
pub const MIN_WINDOW_MS: u64 = 60_000;
/// Largest adjustable window; mirrors `inbox::MAX_WINDOW_MS`.
pub const MAX_WINDOW_MS: u64 = 3_600_000;
/// Smallest widening timelock; mirrors `inbox::MIN_WIDEN_DELAY_MS`.
pub const MIN_WIDEN_DELAY_MS: u64 = 600_000;
/// Largest widening timelock; mirrors `inbox::MAX_WIDEN_DELAY_MS`.
pub const MAX_WIDEN_DELAY_MS: u64 = 604_800_000;

/// BCS encoding of the `inbox::WindowKey` and `inbox::PendingWindowKey` unit
/// structs: no fields, no bytes.
pub const WINDOW_KEY_BCS: &[u8] = &[];

/// Mirrors `inbox::PendingWindow`: a staged widening, applied by `submit`
/// from `effective_at_ms` on without any further transaction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingWindow {
    /// The window that applies once the timelock has elapsed.
    pub new_window_ms: u64,
    /// Consensus time (Sui clock ms) from which `new_window_ms` applies.
    pub effective_at_ms: u64,
}

/// One inbox window read: the stored override, the staged widening, and the
/// window `submit` charges at the read's consensus time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    /// Stored override; `None` when the inbox runs on [`DEFAULT_WINDOW_MS`].
    pub override_ms: Option<u64>,
    /// Staged widening, if any.
    pub pending: Option<PendingWindow>,
    /// What `submit` charges right now: an elapsed staging beats the
    /// override, which beats the default.
    pub effective_ms: u64,
}

/// Returns the `inbox::WindowKey` type tag under `package`.
fn window_key_type_tag(package: Address) -> TypeTag {
    // StructTag's fields are private; the string form parses into the same
    // canonical tag the Move module declares.
    let tag: StructTag = format!("{package}::inbox::WindowKey")
        .parse()
        .expect("static type tag");
    TypeTag::Struct(Box::new(tag))
}

/// Returns the `inbox::PendingWindowKey` type tag under `package`.
fn pending_window_key_type_tag(package: Address) -> TypeTag {
    let tag: StructTag = format!("{package}::inbox::PendingWindowKey")
        .parse()
        .expect("static type tag");
    TypeTag::Struct(Box::new(tag))
}

/// Returns the dynamic-field object id holding the window override on
/// `inbox`. `package` is the deployed Move package id; mirrors
/// `inbox::window_key()`.
#[must_use]
pub fn window_field_id(inbox: Address, package: Address) -> Address {
    inbox.derive_dynamic_child_id(&window_key_type_tag(package), WINDOW_KEY_BCS)
}

/// Returns the dynamic-field object id holding a staged widening on `inbox`
/// (see [`window_field_id`]); mirrors `inbox::pending_window_key()`.
#[must_use]
pub fn pending_window_field_id(inbox: Address, package: Address) -> Address {
    inbox.derive_dynamic_child_id(&pending_window_key_type_tag(package), WINDOW_KEY_BCS)
}

/// Decodes the window override from raw `Field<WindowKey, u64>` BCS.
///
/// # Errors
///
/// Truncated or malformed field BCS.
pub fn parse_window_override(field_bcs: &[u8]) -> Result<u64, InboxError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FieldBcs {
        id: Address,
        value: u64,
    }
    let field: FieldBcs = bcs::from_bytes(field_bcs)
        .map_err(|e| InboxError::Parse(format!("window field BCS: {e}")))?;
    Ok(field.value)
}

/// Decodes a [`PendingWindow`] from raw `Field<PendingWindowKey,
/// PendingWindow>` BCS.
///
/// # Errors
///
/// Truncated or malformed field BCS.
pub fn parse_pending_window(field_bcs: &[u8]) -> Result<PendingWindow, InboxError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FieldBcs {
        id: Address,
        value: PendingWindow,
    }
    let field: FieldBcs = bcs::from_bytes(field_bcs)
        .map_err(|e| InboxError::Parse(format!("pending-window field BCS: {e}")))?;
    Ok(field.value)
}

/// The effective window at `now_ms`, mirroring `inbox::effective_window_ms`:
/// an elapsed staging wins, then the stored override, then the default.
#[must_use]
pub fn effective_window_ms(
    override_ms: Option<u64>,
    pending: Option<&PendingWindow>,
    now_ms: u64,
) -> u64 {
    if let Some(staged) = pending {
        if now_ms >= staged.effective_at_ms {
            return staged.new_window_ms;
        }
    }
    override_ms.unwrap_or(DEFAULT_WINDOW_MS)
}

/// Rejects a window change outside the on-chain bounds before a PTB is
/// built.
///
/// The delay check is deliberately partial: the chain enforces the widening
/// timelock only when the requested value widens (narrowings take `delay_ms`
/// unused), and the client cannot know which case it is without a read. A
/// `delay_ms` of 0 says "narrowing intended"; a widening sent with it aborts
/// on-chain with `E_DELAY_OUT_OF_BOUNDS` instead of silently skipping the
/// timelock.
///
/// # Errors
///
/// Out-of-bounds window or delay, as [`SettlementError::Parse`].
pub fn validate_window_change(new_window_ms: u64, delay_ms: u64) -> Result<(), SettlementError> {
    if !(MIN_WINDOW_MS..=MAX_WINDOW_MS).contains(&new_window_ms) {
        return Err(SettlementError::Parse(format!(
            "window {new_window_ms} ms outside [{MIN_WINDOW_MS}, {MAX_WINDOW_MS}]"
        )));
    }
    if delay_ms != 0 && !(MIN_WIDEN_DELAY_MS..=MAX_WIDEN_DELAY_MS).contains(&delay_ms) {
        return Err(SettlementError::Parse(format!(
            "widening delay {delay_ms} ms outside [{MIN_WIDEN_DELAY_MS}, {MAX_WIDEN_DELAY_MS}] \
             (0 = narrowing)"
        )));
    }
    Ok(())
}

/// The objects one window-change PTB touches: the deployed Move package, the
/// settlement Registry, the Inbox, and the Registry's `OpsCap`.
pub struct WindowTargets {
    /// Deployed settlement Move package id.
    pub package: Address,
    /// Shared settlement Registry object id.
    pub registry: Address,
    /// Shared force-inclusion Inbox object id.
    pub inbox: Address,
    /// The Registry's `OpsCap` (cold steering custody).
    pub ops_cap: Address,
}

/// Builds and executes `settlement::request_window_change`, signed by the
/// holder of the Registry's `OpsCap` — the steering authority in cold
/// custody. A narrowing applies immediately; a widening stages behind its
/// timelock and applies from its effective time on without any further
/// transaction.
///
/// # Errors
///
/// [`validate_window_change`] rejections, or Rpc/Build/Execution errors from
/// the submit path (an on-chain abort arrives as `Execution`).
///
/// # Panics
///
/// Only if the hard-coded module/function names stop being valid Sui
/// identifiers, which is a compile-time property.
pub async fn submit_window_change(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &WindowTargets,
    new_window_ms: u64,
    delay_ms: u64,
) -> Result<(), SettlementError> {
    validate_window_change(new_window_ms, delay_ms)?;
    crate::rotation::submit_rotation_tx(rpc_url, key, "window change", |builder| {
        let registry_arg = builder.object(ObjectInput::new(targets.registry));
        let inbox_arg = builder.object(ObjectInput::new(targets.inbox));
        let clock = builder.object(ObjectInput::new(SUI_CLOCK_ID));
        let window_arg = builder.pure(&new_window_ms);
        let delay_arg = builder.pure(&delay_ms);
        let ops_arg = builder.object(ObjectInput::new(targets.ops_cap));
        builder.move_call(
            crate::rotation::module_function(
                targets.package,
                "settlement",
                "request_window_change",
            )
            .expect("static name"),
            vec![
                registry_arg,
                inbox_arg,
                clock,
                window_arg,
                delay_arg,
                ops_arg,
            ],
        );
    })
    .await
}

/// Builds and executes `settlement::cancel_window_change`, dropping a staged
/// widening before its timelock elapses. Signed by the `OpsCap` holder.
///
/// # Errors
///
/// Rpc/Build/Execution errors from the submit path; cancelling with nothing
/// staged aborts on-chain (`E_NO_PENDING_WINDOW`).
///
/// # Panics
///
/// Only if the hard-coded module/function names stop being valid Sui
/// identifiers, which is a compile-time property.
pub async fn submit_cancel_window_change(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &WindowTargets,
) -> Result<(), SettlementError> {
    crate::rotation::submit_rotation_tx(rpc_url, key, "window cancel", |builder| {
        let registry_arg = builder.object(ObjectInput::new(targets.registry));
        let inbox_arg = builder.object(ObjectInput::new(targets.inbox));
        let ops_arg = builder.object(ObjectInput::new(targets.ops_cap));
        builder.move_call(
            crate::rotation::module_function(targets.package, "settlement", "cancel_window_change")
                .expect("static name"),
            vec![registry_arg, inbox_arg, ops_arg],
        );
    })
    .await
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

    // ---------- window state (adjustable inclusion window) ----------

    /// Mirrors `sui::dynamic_field::Field<inbox::WindowKey, u64>`: the unit
    /// key contributes no bytes, so the field is id + value.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct WindowFieldMirror {
        id: Address,
        value: u64,
    }

    /// Mirrors `sui::dynamic_field::Field<inbox::PendingWindowKey,
    /// inbox::PendingWindow>`.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct PendingWindowFieldMirror {
        id: Address,
        value: PendingWindowMirror,
    }

    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct PendingWindowMirror {
        new_window_ms: u64,
        effective_at_ms: u64,
    }

    fn window_field_bytes(value: u64) -> Vec<u8> {
        let field = WindowFieldMirror {
            id: Address::new([0xcc; 32]),
            value,
        };
        bcs::to_bytes(&field).unwrap()
    }

    fn pending_window_field_bytes(new_window_ms: u64, effective_at_ms: u64) -> Vec<u8> {
        let field = PendingWindowFieldMirror {
            id: Address::new([0xdd; 32]),
            value: PendingWindowMirror {
                new_window_ms,
                effective_at_ms,
            },
        };
        bcs::to_bytes(&field).unwrap()
    }

    #[test]
    fn window_field_ids_are_deterministic_and_distinct() {
        let inbox = Address::new([0x11; 32]);
        let package = Address::new([0x77; 32]);
        let window = window_field_id(inbox, package);
        let pending = pending_window_field_id(inbox, package);

        assert_eq!(window, window_field_id(inbox, package));
        assert_eq!(pending, pending_window_field_id(inbox, package));
        assert_ne!(window, pending);
        assert_ne!(window, window_field_id(Address::new([0x12; 32]), package));
        assert_ne!(window, window_field_id(inbox, Address::new([0x78; 32])));
    }

    #[test]
    fn parse_window_override_roundtrips() {
        assert_eq!(
            parse_window_override(&window_field_bytes(120_000)).unwrap(),
            120_000
        );
    }

    #[test]
    fn parse_window_override_rejects_truncated_bcs() {
        let full = window_field_bytes(1);
        assert!(parse_window_override(&full[..full.len() - 1]).is_err());
        assert!(parse_window_override(&[0u8; 8]).is_err());
    }

    #[test]
    fn parse_pending_window_roundtrips() {
        let pending =
            parse_pending_window(&pending_window_field_bytes(1_800_000, 1_005_000)).unwrap();
        assert_eq!(pending.new_window_ms, 1_800_000);
        assert_eq!(pending.effective_at_ms, 1_005_000);
    }

    #[test]
    fn parse_pending_window_rejects_truncated_bcs() {
        let full = pending_window_field_bytes(1, 2);
        assert!(parse_pending_window(&full[..full.len() - 1]).is_err());
    }

    #[test]
    fn effective_window_follows_override_then_elapsed_staging() {
        // No state at all: the default.
        assert_eq!(effective_window_ms(None, None, 0), DEFAULT_WINDOW_MS);
        // A narrowing override applies as soon as it exists.
        assert_eq!(effective_window_ms(Some(60_000), None, 1_000_000), 60_000);
        // A staged widening before its effective time is not applied.
        let pending = PendingWindow {
            new_window_ms: 1_800_000,
            effective_at_ms: 1_005_000,
        };
        assert_eq!(
            effective_window_ms(None, Some(&pending), 1_004_999),
            DEFAULT_WINDOW_MS
        );
        // At the boundary it applies (>=, matching the Move check).
        assert_eq!(
            effective_window_ms(None, Some(&pending), 1_005_000),
            1_800_000
        );
        // An elapsed staging beats a stored override.
        assert_eq!(
            effective_window_ms(Some(60_000), Some(&pending), 1_005_000),
            1_800_000
        );
        // An un-elapsed staging loses to the override.
        assert_eq!(
            effective_window_ms(Some(120_000), Some(&pending), 1_000),
            120_000
        );
    }

    #[tokio::test]
    async fn window_view_defaults_when_fields_are_absent() {
        let (transport, inbox_id) = mock_with_inbox(0, 0, &[]);
        let package = Address::new([0x77; 32]);
        let reader = InboxReader::new(transport, inbox_id);

        let view = reader.window(package, 1_000).await.unwrap();
        assert_eq!(view.override_ms, None);
        assert_eq!(view.pending, None);
        assert_eq!(view.effective_ms, DEFAULT_WINDOW_MS);
    }

    #[tokio::test]
    async fn window_view_reads_override_and_staged_widening() {
        let (mut transport, inbox_id) = mock_with_inbox(0, 0, &[]);
        let package = Address::new([0x77; 32]);
        transport.objects.insert(
            window_field_id(inbox_id, package),
            window_field_bytes(60_000),
        );
        transport.objects.insert(
            pending_window_field_id(inbox_id, package),
            pending_window_field_bytes(1_800_000, 1_005_000),
        );
        let reader = InboxReader::new(transport, inbox_id);

        let view = reader.window(package, 1_000).await.unwrap();
        assert_eq!(view.override_ms, Some(60_000));
        assert_eq!(
            view.pending.as_ref().map(|p| p.new_window_ms),
            Some(1_800_000)
        );
        assert_eq!(view.effective_ms, 60_000);

        // After the timelock the staging wins without any transaction.
        let view = reader.window(package, 1_005_000).await.unwrap();
        assert_eq!(view.effective_ms, 1_800_000);
    }

    #[tokio::test]
    async fn window_view_propagates_transport_errors() {
        let (mut transport, inbox_id) = mock_with_inbox(0, 0, &[]);
        transport.fail_objects = true;
        let reader = InboxReader::new(transport, inbox_id);

        assert!(matches!(
            reader.window(Address::new([0x77; 32]), 0).await,
            Err(InboxError::Rpc(msg)) if msg.contains("transport down")
        ));
    }

    #[test]
    fn validate_window_change_accepts_narrowings_and_bounded_widenings() {
        assert!(validate_window_change(MIN_WINDOW_MS, 0).is_ok());
        assert!(validate_window_change(MAX_WINDOW_MS, MIN_WIDEN_DELAY_MS).is_ok());
        assert!(validate_window_change(600_000, MAX_WIDEN_DELAY_MS).is_ok());
        // Delay 0 means "narrowing intended"; legality of the widening case
        // is decided on-chain against the live window.
        assert!(validate_window_change(600_000, 0).is_ok());
    }

    #[test]
    fn validate_window_change_rejects_out_of_bounds_windows() {
        assert!(validate_window_change(MIN_WINDOW_MS - 1, 0).is_err());
        assert!(validate_window_change(MAX_WINDOW_MS + 1, 0).is_err());
        assert!(validate_window_change(0, 0).is_err());
    }

    #[test]
    fn validate_window_change_rejects_out_of_bounds_widen_delays() {
        assert!(validate_window_change(600_000, MIN_WIDEN_DELAY_MS - 1).is_err());
        assert!(validate_window_change(600_000, MAX_WIDEN_DELAY_MS + 1).is_err());
        assert!(validate_window_change(600_000, 1).is_err());
    }
}
