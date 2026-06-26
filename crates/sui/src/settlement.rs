//! In-process Sui settlement: deploy + commit.

use std::path::Path;

use podseq_core::{BlobId, Header};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_rpc::field::FieldMask;
use sui_rpc::field::FieldMaskUtil;
use sui_rpc::proto::sui::rpc::v2::owner;
use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionRequest;
use sui_rpc::proto::sui::rpc::v2::GetEpochRequest;
use sui_rpc::proto::sui::rpc::v2::GetObjectRequest;
use sui_sdk_types::Address;
use sui_sdk_types::Identifier;
use sui_sdk_types::TypeTag;
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};
use thiserror::Error;
use tracing::info;

/// Errors from Sui settlement deploy/commit transactions.
#[derive(Debug, Error)]
pub enum SettlementError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("key error: {0}")]
    Key(String),
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("transaction build error: {0}")]
    Build(String),
    #[error("transaction execution failed: {0}")]
    Execution(String),
    #[error("parse error: {0}")]
    Parse(String),
}

/// Object IDs produced by deploying the settlement package.
#[derive(Debug, Clone)]
pub struct DeployedContract {
    pub package_id: String,
    pub settler_cap_id: String,
    pub registry_id: String,
}

/// Sui settlement client that commits block anchors on-chain.
pub struct Settlement {
    key: Ed25519PrivateKey,
    sender: Address,
    package: Address,
    cap: Address,
    registry: Address,
    rpc: sui_rpc::Client,
}

impl std::fmt::Debug for Settlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settlement")
            .field("sender", &self.sender)
            .field("package", &self.package)
            .finish_non_exhaustive()
    }
}

impl Settlement {
    /// Creates a settlement client from a key file and on-chain object IDs.
    pub fn new(
        key_path: &Path,
        package_id: &str,
        cap_id: &str,
        registry_id: &str,
        rpc_url: &str,
    ) -> Result<Self, SettlementError> {
        let key_str = std::fs::read_to_string(key_path)
            .map_err(SettlementError::Io)?
            .trim()
            .to_string();
        let key = Ed25519PrivateKey::from_suiprivkey(&key_str)
            .map_err(|e| SettlementError::Key(e.to_string()))?;
        let sender = key.public_key().derive_address();
        let rpc = sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
        Ok(Self {
            key,
            sender,
            package: package_id
                .parse()
                .map_err(|e| SettlementError::Parse(format!("package id: {e}")))?,
            cap: cap_id
                .parse()
                .map_err(|e| SettlementError::Parse(format!("cap id: {e}")))?,
            registry: registry_id
                .parse()
                .map_err(|e| SettlementError::Parse(format!("registry id: {e}")))?,
            rpc,
        })
    }

    /// Commits a block header and blob id on Sui via the settle entrypoint.
    pub async fn commit(&mut self, header: &Header, blob: &BlobId) -> Result<(), SettlementError> {
        let function = Function::new(
            self.package,
            Identifier::new("settlement").map_err(|e| SettlementError::Parse(e.to_string()))?,
            Identifier::new("settle").map_err(|e| SettlementError::Parse(e.to_string()))?,
        );

        let mut builder = TransactionBuilder::new();
        let cap = builder.object(ObjectInput::new(self.cap));
        let registry = builder.object(ObjectInput::new(self.registry));
        let blob_id_arg = builder.pure(&blob.0.to_vec());
        let height_arg = builder.pure(&header.height);
        builder.move_call(function, vec![cap, registry, blob_id_arg, height_arg]);
        builder.set_sender(self.sender);

        let mut rpc = self.rpc.clone();
        let tx = builder
            .build(&mut rpc)
            .await
            .map_err(|e| SettlementError::Build(e.to_string()))?;

        let signature = self
            .key
            .sign_transaction(&tx)
            .map_err(|e| SettlementError::Key(e.to_string()))?;

        let response = self
            .rpc
            .execute_transaction_and_wait_for_checkpoint(
                ExecuteTransactionRequest::new(tx.into()).with_signatures(vec![signature.into()]),
                std::time::Duration::from_secs(30),
            )
            .await
            .map_err(|e| SettlementError::Execution(e.to_string()))?;

        let inner = response.into_inner();
        let status = inner.transaction().effects().status();
        if !status.success() {
            return Err(SettlementError::Execution(format!(
                "transaction failed: {}",
                status.error().description.clone().unwrap_or_default()
            )));
        }

        info!(
            height = header.height,
            digest = ?inner.transaction().transaction().digest(),
            "block committed on Sui"
        );

        Ok(())
    }

    /// Publishes the settlement package and returns the created object IDs.
    pub async fn deploy(
        key_path: &Path,
        rpc_url: &str,
        modules: Vec<Vec<u8>>,
    ) -> Result<DeployedContract, SettlementError> {
        let key_str = std::fs::read_to_string(key_path)
            .map_err(SettlementError::Io)?
            .trim()
            .to_string();
        let key = Ed25519PrivateKey::from_suiprivkey(&key_str)
            .map_err(|e| SettlementError::Key(e.to_string()))?;
        let sender = key.public_key().derive_address();
        let mut rpc =
            sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;

        // 1. Publish the package. Transfer the returned UpgradeCap to the sender.
        let mut builder = TransactionBuilder::new();
        builder.set_sender(sender);
        // Pass the transitive dependency addresses (MoveStdlib = 0x1, Sui = 0x2).
        let cap = builder.publish(
            modules,
            vec![
                "0x0000000000000000000000000000000000000000000000000000000000000001"
                    .parse()
                    .map_err(|e| {
                        SettlementError::Parse(format!("invalid MoveStdlib address: {e}"))
                    })?,
                "0x0000000000000000000000000000000000000000000000000000000000000002"
                    .parse()
                    .map_err(|e| SettlementError::Parse(format!("invalid Sui address: {e}")))?,
            ],
        );
        let sender_arg = builder.pure(&sender);
        builder.move_call(
            Function::new(
                "0x0000000000000000000000000000000000000000000000000000000000000002"
                    .parse()
                    .map_err(|e| SettlementError::Parse(format!("invalid Sui address: {e}")))?,
                Identifier::new("transfer").map_err(|e| SettlementError::Parse(e.to_string()))?,
                Identifier::new("public_transfer")
                    .map_err(|e| SettlementError::Parse(e.to_string()))?,
            )
            .with_type_args(vec!["0x2::package::UpgradeCap"
                .parse()
                .map_err(|e| SettlementError::Parse(format!("type arg: {e}")))?]),
            vec![cap, sender_arg],
        );
        let publish_tx = builder
            .build(&mut rpc)
            .await
            .map_err(|e| SettlementError::Build(format!("publish: {e}")))?;
        let effects = sign_and_execute(&mut rpc, &key, publish_tx, "publish")
            .await?
            .transaction()
            .effects()
            .clone();

        // The package is the ChangedObject with output_state == PackageWrite.
        use sui_rpc::proto::sui::rpc::v2::changed_object::OutputObjectState;
        let package_id = effects
            .changed_objects()
            .iter()
            .find(|c| c.output_state() == OutputObjectState::PackageWrite)
            .and_then(|c| c.object_id.clone())
            .ok_or_else(|| {
                SettlementError::Execution("package not found in publish response".into())
            })?;

        info!(package_id = %package_id, "settlement package published");

        // 2. Initialize the package (creates the shared Registry + SettlerCap).
        let package_addr: Address = package_id
            .parse()
            .map_err(|e| SettlementError::Parse(format!("package id: {e}")))?;
        let mut builder = TransactionBuilder::new();
        builder.move_call(
            Function::new(
                package_addr,
                Identifier::new("settlement").map_err(|e| SettlementError::Parse(e.to_string()))?,
                Identifier::new("initialize").map_err(|e| SettlementError::Parse(e.to_string()))?,
            ),
            vec![],
        );
        builder.set_sender(sender);
        let init_tx = builder
            .build(&mut rpc)
            .await
            .map_err(|e| SettlementError::Build(format!("initialize: {e}")))?;
        let changes = sign_and_execute(&mut rpc, &key, init_tx, "initialize")
            .await?
            .transaction()
            .effects()
            .changed_objects()
            .to_vec();

        // Registry: shared object created by `initialize`.
        let registry_id =
            find_created_object(&changes, owner::OwnerKind::Shared).ok_or_else(|| {
                SettlementError::Execution("Registry not found in init response".into())
            })?;
        // SettlerCap: created and address-owned by the sender.
        let settler_cap_id =
            find_created_object(&changes, owner::OwnerKind::Address).ok_or_else(|| {
                SettlementError::Execution("SettlerCap not found in init response".into())
            })?;

        info!(
            package_id = %package_id,
            registry_id = %registry_id,
            settler_cap_id = %settler_cap_id,
            "settlement contract initialized"
        );

        Ok(DeployedContract {
            package_id,
            settler_cap_id,
            registry_id,
        })
    }
}

/// Signs `tx` with `key` and submits it, waiting for checkpoint inclusion.
/// `label` is prefixed to error messages and must match the failing phase.
async fn sign_and_execute(
    rpc: &mut sui_rpc::Client,
    key: &Ed25519PrivateKey,
    tx: sui_sdk_types::Transaction,
    label: &str,
) -> Result<sui_rpc::proto::sui::rpc::v2::ExecuteTransactionResponse, SettlementError> {
    let signature = key
        .sign_transaction(&tx)
        .map_err(|e| SettlementError::Key(format!("{label}: {e}")))?;
    let response = rpc
        .execute_transaction_and_wait_for_checkpoint(
            ExecuteTransactionRequest::new(tx.into()).with_signatures(vec![signature.into()]),
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| SettlementError::Execution(format!("{label}: {e}")))?;
    Ok(response.into_inner())
}

/// Returns the first object id created (`IdOperation::Created`) owned by `kind`.
fn find_created_object(
    changes: &[sui_rpc::proto::sui::rpc::v2::ChangedObject],
    kind: owner::OwnerKind,
) -> Option<String> {
    use sui_rpc::proto::sui::rpc::v2::changed_object::IdOperation;
    changes
        .iter()
        .find(|c| c.id_operation() == IdOperation::Created && c.output_owner().kind() == kind)
        .and_then(|c| c.object_id.clone())
}

/// Reads the latest committed height from the registry object.
pub async fn latest_height(rpc_url: &str, registry_id: &str) -> Result<u64, SettlementError> {
    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
    let registry: Address = parse_address(registry_id)?;

    let response = rpc
        .ledger_client()
        .get_object(
            GetObjectRequest::new(&registry).with_read_mask(FieldMask::from_str("contents")),
        )
        .await
        .map_err(|e| SettlementError::Rpc(format!("get_object: {e}")))?;

    let obj = response
        .into_inner()
        .object
        .ok_or_else(|| SettlementError::Execution("registry object not found".into()))?;

    let contents = obj
        .contents
        .and_then(|bcs| bcs.value)
        .ok_or_else(|| SettlementError::Execution("registry has no contents".into()))?;

    parse_latest_height(&contents)
}

/// Probes Sui RPC reachability with a cheap no-arg epoch query.
///
/// Used as a startup preflight so an unreachable or misconfigured RPC fails
/// fast with a clear message instead of a cryptic deploy/commit error later.
pub async fn ping_rpc(rpc_url: &str) -> Result<(), SettlementError> {
    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
    let request =
        GetEpochRequest::latest().with_read_mask(FieldMask::from_paths(["reference_gas_price"]));
    rpc.ledger_client()
        .get_epoch(request)
        .await
        .map_err(|e| SettlementError::Rpc(format!("get_epoch: {e}")))?;
    Ok(())
}

/// Decodes the `latest_height` field from raw registry BCS contents.
///
/// Layout: `[32 bytes: id][8 bytes: latest_height (le)][32 bytes: commitments table]`.
pub fn parse_latest_height(contents: &[u8]) -> Result<u64, SettlementError> {
    if contents.len() < 40 {
        return Err(SettlementError::Execution(format!(
            "registry BCS too short: {} bytes",
            contents.len()
        )));
    }

    let latest = u64::from_le_bytes(
        contents[32..40]
            .try_into()
            .map_err(|_| SettlementError::Execution("invalid latest_height".into()))?,
    );
    Ok(latest)
}

/// Extracts the commitments Table UID from raw registry BCS contents.
fn parse_table_uid(bytes: &[u8]) -> Result<Address, SettlementError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct RegistryBcs {
        id: sui_sdk_types::Address,
        latest_height: u64,
        commitments: TableBcs,
    }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct TableBcs {
        id: sui_sdk_types::Address,
        size: u64,
    }
    let registry: RegistryBcs =
        bcs::from_bytes(bytes).map_err(|e| SettlementError::Parse(format!("registry BCS: {e}")))?;
    Ok(registry.commitments.id)
}

/// Extracts the blob id value from a raw `Field<u64, vector<u8>>` BCS buffer.
fn parse_field_blob_id(bytes: &[u8]) -> Result<BlobId, SettlementError> {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct FieldBcs {
        id: sui_sdk_types::Address,
        name: u64,
        value: Vec<u8>,
    }
    let field: FieldBcs =
        bcs::from_bytes(bytes).map_err(|e| SettlementError::Parse(format!("field BCS: {e}")))?;
    parse_blob_id(&field.value)
}

/// Reads the immutable commitments-table UID from the registry object.
///
/// The table UID is fixed at `initialize` and never changes, so callers should
/// fetch it once and reuse it for every [`commitment_at`] lookup instead of
/// re-reading the registry per height.
pub async fn table_uid(rpc_url: &str, registry_id: &str) -> Result<Address, SettlementError> {
    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;
    let registry = parse_address(registry_id)?;
    let response = rpc
        .ledger_client()
        .get_object(
            GetObjectRequest::new(&registry).with_read_mask(FieldMask::from_str("contents")),
        )
        .await
        .map_err(|e| SettlementError::Rpc(format!("get_object: {e}")))?;
    let bytes = response
        .into_inner()
        .object
        .and_then(|o| o.contents)
        .and_then(|c| c.value)
        .ok_or_else(|| SettlementError::Execution("registry has no contents".into()))?;
    parse_table_uid(&bytes)
}

/// Reads a single settled `(height, blob_id)` entry by its dynamic-field object
/// id, O(1) in the number of settled heights.
///
/// `table_uid` is the commitments-table UID from [`table_uid`] (fetch once,
/// reuse). This issues exactly one `get_object` per call, so full-node sync is
/// O(new heights) RPC calls per poll regardless of table size. Returns `None`
/// if the height is not settled.
pub async fn commitment_at(
    rpc_url: &str,
    table_uid: &Address,
    height: u64,
) -> Result<Option<BlobId>, SettlementError> {
    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| SettlementError::Rpc(e.to_string()))?;

    // Dynamic field object id for Table<u64, vector<u8>>[height].
    let field_id = table_uid.derive_dynamic_child_id(&TypeTag::U64, &height.to_le_bytes());

    let response = rpc
        .ledger_client()
        .get_object(
            GetObjectRequest::new(&field_id).with_read_mask(FieldMask::from_str("contents")),
        )
        .await
        .map_err(|e| SettlementError::Rpc(format!("get_object (field): {e}")))?;

    let Some(obj) = response.into_inner().object else {
        return Ok(None);
    };
    let bytes = obj
        .contents
        .and_then(|c| c.value)
        .ok_or_else(|| SettlementError::Execution("field contents missing raw BCS".into()))?;

    Ok(Some(parse_field_blob_id(&bytes)?))
}

/// Converts a raw byte vector into a fixed-size `BlobId`.
fn parse_blob_id(bytes: &[u8]) -> Result<BlobId, SettlementError> {
    if bytes.len() != 32 {
        return Err(SettlementError::Parse(format!(
            "expected 32-byte blob id, got {}",
            bytes.len()
        )));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(bytes);
    Ok(BlobId(arr))
}

fn parse_address(s: &str) -> Result<Address, SettlementError> {
    s.parse()
        .map_err(|e| SettlementError::Parse(format!("address {s}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_key() {
        let result = Settlement::new(
            Path::new("/nonexistent"),
            "0x2",
            "0x3",
            "0x4",
            "https://fullnode.testnet.sui.io:443",
        );
        assert!(result.is_err());
    }

    /// Builds a minimal registry BCS buffer with the given height.
    fn registry_contents(height: u64) -> Vec<u8> {
        let mut buf = vec![0u8; 40];
        buf[32..40].copy_from_slice(&height.to_le_bytes());
        buf
    }

    #[test]
    fn parse_latest_height_reads_little_endian_field() {
        // height = 123 → 0x7b little-endian at bytes 32..40; the leading 32-byte
        // id and trailing table bytes are ignored.
        let contents = registry_contents(123);
        assert_eq!(parse_latest_height(&contents).unwrap(), 123);
    }

    #[test]
    fn parse_latest_height_rejects_short_buffer() {
        // 39 bytes: one short of the 40-byte minimum.
        assert!(parse_latest_height(&[0u8; 39]).is_err());
    }

    /// Mirrors `Registry { id: UID, latest_height: u64, commitments: Table }`.
    /// `UID`/`ID` and `Table`'s `id`/`size` flatten under BCS to their fields.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct RegistryMirror {
        id: sui_sdk_types::Address,
        latest_height: u64,
        commitments: TableMirror,
    }
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct TableMirror {
        id: sui_sdk_types::Address,
        size: u64,
    }

    fn registry_contents_with_table(height: u64, table_uid: [u8; 32]) -> Vec<u8> {
        let registry = RegistryMirror {
            id: sui_sdk_types::Address::new([0; 32]),
            latest_height: height,
            commitments: TableMirror {
                id: sui_sdk_types::Address::new(table_uid),
                size: 0,
            },
        };
        bcs::to_bytes(&registry).unwrap()
    }

    #[test]
    fn parse_table_uid_reads_commitments_id() {
        let mut uid = [0u8; 32];
        uid[0] = 0xab;
        uid[31] = 0xcd;
        let contents = registry_contents_with_table(7, uid);
        assert_eq!(parse_table_uid(&contents).unwrap(), Address::new(uid));
    }

    #[test]
    fn parse_table_uid_rejects_truncated_bcs() {
        // Incomplete buffer: BCS deserialization fails.
        assert!(parse_table_uid(&[0u8; 71]).is_err());
    }

    /// Mirrors `sui::dynamic_field::Field<u64, vector<u8>>`. `UID`/`ID` flatten
    /// to a bare `Address` under BCS. Building the bytes this way (instead of
    /// manual offsets) guards against layout drift and varint mistakes.
    #[derive(serde::Serialize)]
    struct FieldMirror {
        #[allow(dead_code)]
        id: sui_sdk_types::Address,
        #[allow(dead_code)]
        name: u64,
        value: Vec<u8>,
    }

    #[test]
    fn parse_field_blob_id_roundtrips_against_bcs_layout() {
        let blob = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xab, 0xcd, 0xef,
        ];
        let field = FieldMirror {
            id: sui_sdk_types::Address::new([0xaa; 32]),
            name: 0x0706,
            value: blob.to_vec(),
        };
        let encoded = bcs::to_bytes(&field).unwrap();
        let parsed = parse_field_blob_id(&encoded).unwrap();
        assert_eq!(parsed.0, blob);
    }

    #[test]
    fn parse_field_blob_id_rejects_wrong_value_length() {
        // A valid Field whose value is not a 32-byte blob id.
        let field = FieldMirror {
            id: sui_sdk_types::Address::new([0; 32]),
            name: 9,
            value: vec![0x42; 200],
        };
        let encoded = bcs::to_bytes(&field).unwrap();
        assert!(matches!(
            parse_field_blob_id(&encoded),
            Err(SettlementError::Parse(_))
        ));
    }

    #[test]
    fn parse_field_blob_id_rejects_truncated_bcs() {
        assert!(parse_field_blob_id(&[0u8; 40]).is_err());
    }
}
