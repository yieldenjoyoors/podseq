//! Settled state roots: the on-chain proof anchor for balance proofs.
//!
//! `settlement::commit_state_root` anchors each settled block's state root in
//! a dynamic field of the Registry, composed into the settle PTB. This module
//! mirrors the Move side for readers (exit queue, light clients) and builds
//! the commit command for the sequencer's settle transaction.

use crate::inbox::{InboxError, InboxTransport};
use sui_sdk_types::Address;

/// Length of an L2 state root in bytes; mirrors `settlement::STATE_ROOT_LEN`.
pub const STATE_ROOT_LEN: usize = 32;

/// The `commit_state_root` command payload for the settle PTB: the settled
/// header's state root when the feature is enabled, `None` when the operator
/// pinned it off (settling against a package that predates the entrypoint —
/// appending the call would abort the whole PTB, halting settlement).
#[must_use]
pub fn state_root_command(enabled: bool, header: &podseq_core::Header) -> Option<Vec<u8>> {
    if enabled {
        Some(header.state_root.to_vec())
    } else {
        None
    }
}

/// Client-side mirror of `settlement::E_STATE_ROOT_MALFORMED`: the Move
/// entrypoint aborts on any other length, so building the PTB with a
/// different value would abort the composed settle transaction.
///
/// # Errors
///
/// Any length other than [`STATE_ROOT_LEN`].
pub fn validate_state_root(root: &[u8]) -> Result<(), InboxError> {
    if root.len() == STATE_ROOT_LEN {
        Ok(())
    } else {
        Err(InboxError::Parse(format!(
            "state root must be {STATE_ROOT_LEN} bytes, got {}",
            root.len()
        )))
    }
}

fn state_root_key_type_tag(package: Address) -> sui_sdk_types::TypeTag {
    // StructTag's fields are private; the string form parses into the same
    // canonical tag the Move module declares.
    let tag: sui_sdk_types::StructTag = format!("{package}::settlement::StateRootKey")
        .parse()
        .expect("static type tag");
    sui_sdk_types::TypeTag::Struct(Box::new(tag))
}

/// Returns the dynamic-field object id holding the state root of `height` on
/// the Registry; mirrors `settlement::state_root_key(height)`.
#[must_use]
pub fn state_root_field_id(registry: Address, package: Address, height: u64) -> Address {
    registry.derive_dynamic_child_id(&state_root_key_type_tag(package), &height.to_le_bytes())
}

/// Decodes the state root from raw `Field<StateRootKey, vector<u8>>` BCS.
///
/// # Errors
///
/// Truncated or malformed field BCS, or a value that is not
/// [`STATE_ROOT_LEN`] bytes.
pub fn parse_state_root_field(field_bcs: &[u8]) -> Result<[u8; STATE_ROOT_LEN], InboxError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FieldBcs {
        id: Address,
        name: u64,
        value: Vec<u8>,
    }
    let field: FieldBcs = bcs::from_bytes(field_bcs)
        .map_err(|e| InboxError::Parse(format!("state-root field BCS: {e}")))?;
    let value = field.value;
    if value.len() != STATE_ROOT_LEN {
        return Err(InboxError::Parse(format!(
            "state root must be {STATE_ROOT_LEN} bytes, got {}",
            value.len()
        )));
    }
    let mut root = [0u8; STATE_ROOT_LEN];
    root.copy_from_slice(&value);
    Ok(root)
}

/// Reads anchored state roots over the mockable transport (exit queue, light
/// clients, tests).
#[derive(Debug, Clone)]
pub struct StateRootReader<T> {
    transport: T,
    registry: Address,
    package: Address,
}

impl<T: InboxTransport> StateRootReader<T> {
    /// Creates a reader for the Registry at `registry` under `package`.
    pub fn new(transport: T, registry: Address, package: Address) -> Self {
        Self {
            transport,
            registry,
            package,
        }
    }

    /// The root anchored for `height`.
    ///
    /// `Ok(None)` when nothing is anchored yet (heights settled before
    /// state-root anchoring was adopted). Transport and parse errors
    /// propagate: an unreadable root must never be read as "no anchor", or a
    /// Sui partition would let a balance proof verify against stale state.
    ///
    /// # Errors
    ///
    /// Rpc failures from the transport, or BCS decode errors on a present
    /// field.
    pub async fn at(&self, height: u64) -> Result<Option<[u8; STATE_ROOT_LEN]>, InboxError> {
        match self
            .transport
            .object_contents(&state_root_field_id(self.registry, self.package, height))
            .await
        {
            Ok(bytes) => Ok(Some(parse_state_root_field(&bytes)?)),
            Err(InboxError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const REGISTRY: Address = Address::new([0x33; 32]);
    const PACKAGE: Address = Address::new([0x44; 32]);

    #[test]
    fn state_root_command_carries_the_header_root_when_enabled() {
        let mut root = [0u8; 32];
        root[31] = 9;
        let header = podseq_core::Header {
            height: 5,
            parent_hash: [0; 32],
            state_root: root,
            timestamp: 1_000,
        };
        let command = state_root_command(true, &header);
        assert_eq!(command.as_deref(), Some(root.as_slice()));
    }

    #[test]
    fn state_root_command_is_none_when_the_feature_is_disabled() {
        let header = podseq_core::Header {
            height: 5,
            parent_hash: [0; 32],
            state_root: [7; 32],
            timestamp: 1_000,
        };
        assert_eq!(state_root_command(false, &header), None);
    }

    #[test]
    fn validate_state_root_accepts_exactly_32_bytes() {
        assert!(validate_state_root(&[0u8; 32]).is_ok());
    }

    #[test]
    fn validate_state_root_rejects_other_lengths() {
        assert!(validate_state_root(&[0u8; 31]).is_err());
        assert!(validate_state_root(&[0u8; 33]).is_err());
        assert!(validate_state_root(&[]).is_err());
    }

    #[test]
    fn state_root_field_ids_are_deterministic_and_distinct() {
        let a = state_root_field_id(REGISTRY, PACKAGE, 1);
        assert_eq!(a, state_root_field_id(REGISTRY, PACKAGE, 1));
        assert_ne!(a, state_root_field_id(REGISTRY, PACKAGE, 2));
        assert_ne!(a, state_root_field_id(Address::new([0x34; 32]), PACKAGE, 1));
        assert_ne!(
            a,
            state_root_field_id(REGISTRY, Address::new([0x45; 32]), 1)
        );
    }

    #[test]
    fn parse_state_root_field_roundtrips() {
        // BCS of `Field<StateRootKey, vector<u8>>`: id (32 bytes), name (the
        // u64 height), value (uleb128 length prefix + bytes).
        let mut bytes = vec![0u8; 32];
        bytes[31] = 0x55;
        bytes.extend_from_slice(&7u64.to_le_bytes());
        bytes.extend_from_slice(&bcs::to_bytes(&vec![0xABu8; 32]).expect("serializes"));
        let root = parse_state_root_field(&bytes).expect("decodes");
        assert_eq!(root, [0xAB; 32]);
    }

    #[test]
    fn parse_state_root_field_rejects_short_and_oversized_values() {
        let mut short = vec![0u8; 32];
        short.extend_from_slice(&1u64.to_le_bytes());
        short.extend_from_slice(&bcs::to_bytes(&vec![0u8; 31]).expect("serializes"));
        assert!(parse_state_root_field(&short).is_err());

        let mut oversized = vec![0u8; 32];
        oversized.extend_from_slice(&1u64.to_le_bytes());
        oversized.extend_from_slice(&bcs::to_bytes(&vec![0u8; 33]).expect("serializes"));
        assert!(parse_state_root_field(&oversized).is_err());
    }

    #[test]
    fn parse_state_root_field_rejects_truncated_bcs() {
        let mut full = vec![0u8; 32];
        full.extend_from_slice(&1u64.to_le_bytes());
        full.extend_from_slice(&bcs::to_bytes(&[0u8; 32]).expect("serializes"));
        assert!(parse_state_root_field(&full[..full.len() - 1]).is_err());
    }

    /// Mock at the `get_object` boundary: present ids serve contents, absent
    /// ids map to `NotFound`.
    #[derive(Default)]
    struct MockTransport {
        objects: BTreeMap<Address, Vec<u8>>,
    }

    impl MockTransport {
        fn with_object(mut self, id: Address, bytes: Vec<u8>) -> Self {
            self.objects.insert(id, bytes);
            self
        }
    }

    impl InboxTransport for MockTransport {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> crate::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            Box::pin(async move {
                self.objects
                    .get(id)
                    .cloned()
                    .ok_or_else(|| InboxError::NotFound(format!("mock: {id}")))
            })
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> crate::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    fn field_bcs(root: [u8; 32]) -> Vec<u8> {
        let mut bytes = vec![0u8; 32];
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&bcs::to_bytes(&root.to_vec()).expect("serializes"));
        bytes
    }

    #[tokio::test]
    async fn reader_decodes_a_present_root() {
        let transport = MockTransport::default().with_object(
            state_root_field_id(REGISTRY, PACKAGE, 3),
            field_bcs([0xCD; 32]),
        );
        let root = StateRootReader::new(transport, REGISTRY, PACKAGE)
            .at(3)
            .await
            .expect("reads");
        assert_eq!(root, Some([0xCD; 32]));
    }

    #[tokio::test]
    async fn reader_maps_a_missing_root_to_none() {
        let transport = MockTransport::default();
        let root = StateRootReader::new(transport, REGISTRY, PACKAGE)
            .at(3)
            .await
            .expect("missing is not an error");
        assert_eq!(root, None);
    }

    #[tokio::test]
    async fn reader_propagates_transport_errors() {
        let transport = FailingTransport;
        let err = StateRootReader::new(transport, REGISTRY, PACKAGE)
            .at(3)
            .await
            .expect_err("transport errors propagate");
        assert!(matches!(err, InboxError::Rpc(_)));
    }

    /// Transport that always fails, to exercise error propagation.
    struct FailingTransport;

    impl InboxTransport for FailingTransport {
        fn object_contents<'a>(
            &'a self,
            _id: &'a Address,
        ) -> crate::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            Box::pin(async move { Err(InboxError::Rpc("read down".into())) })
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> crate::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Err(InboxError::Rpc("read down".into())) })
        }
    }
}
