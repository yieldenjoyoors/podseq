//! Enshrined bridge: Sui vault client.
//!
//! [`BridgeClient`] submits releases on Sui (`bridge::withdraw`) and reads
//! deposits by nonce. Deposits are stored on-chain as `Table<u64, DepositRecord>`
//! entries (see `move/sources/bridge.move`) because the Sui gRPC read API has no
//! event query; the relayer reads them the same way full nodes read settled
//! heights, one dynamic-field object per nonce.

use std::path::Path;

use podseq_core::Error as CoreError;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_rpc::field::FieldMask;
use sui_rpc::field::FieldMaskUtil;
use sui_rpc::proto::sui::rpc::v2::owner;
use sui_rpc::proto::sui::rpc::v2::GetObjectRequest;
use sui_sdk_types::Address;
use sui_sdk_types::Identifier;
use sui_sdk_types::TypeTag;
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};
use thiserror::Error;
use tracing::info;

use crate::settlement::{find_created_object, sign_and_execute};

/// Errors from Sui bridge read/release transactions.
#[derive(Debug, Error)]
pub enum BridgeError {
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

impl From<SettlementError> for BridgeError {
    fn from(e: crate::settlement::SettlementError) -> Self {
        match e {
            crate::settlement::SettlementError::Io(e) => BridgeError::Io(e),
            crate::settlement::SettlementError::Key(s) => BridgeError::Key(s),
            crate::settlement::SettlementError::Rpc(s) => BridgeError::Rpc(s),
            crate::settlement::SettlementError::Build(s) => BridgeError::Build(s),
            crate::settlement::SettlementError::Execution(s) => BridgeError::Execution(s),
            crate::settlement::SettlementError::Parse(s) => BridgeError::Parse(s),
        }
    }
}

use crate::settlement::SettlementError;

/// A single stored deposit, mirrored from `bridge::DepositRecord`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DepositRecord {
    pub amount: u64,
    /// 20-byte EVM recipient address.
    pub recipient_l2: Vec<u8>,
    /// Coin `TypeName` bytes (e.g. `0x2::sui::SUI`).
    pub coin_type: Vec<u8>,
}

/// Sui bridge client that releases locked coins and reads deposits.
pub struct BridgeClient {
    key: Ed25519PrivateKey,
    sender: Address,
    package: Address,
    cap: Address,
    vault: Address,
    rpc: sui_rpc::Client,
}

impl std::fmt::Debug for BridgeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeClient")
            .field("sender", &self.sender)
            .field("package", &self.package)
            .field("vault", &self.vault)
            .finish_non_exhaustive()
    }
}

impl BridgeClient {
    /// Creates a bridge client from a key file and the on-chain bridge object IDs.
    pub fn new(
        key_path: &Path,
        package_id: &str,
        cap_id: &str,
        vault_id: &str,
        rpc_url: &str,
    ) -> Result<Self, BridgeError> {
        let key_str = std::fs::read_to_string(key_path)
            .map_err(BridgeError::Io)?
            .trim()
            .to_string();
        let key = crate::parse_signer_key(&key_str).map_err(BridgeError::Key)?;
        let sender = key.public_key().derive_address();
        let rpc = sui_rpc::Client::new(rpc_url).map_err(|e| BridgeError::Rpc(e.to_string()))?;
        Ok(Self {
            key,
            sender,
            package: parse_address(package_id, "package id")?,
            cap: parse_address(cap_id, "cap id")?,
            vault: parse_address(vault_id, "vault id")?,
            rpc,
        })
    }

    /// Releases `amount` of `coin_type` to `recipient` on Sui via `bridge::withdraw`.
    ///
    /// `coin_type` is the Sui `TypeTag` (e.g. `0x2::sui::SUI`). `l2_nonce` is the
    /// L2 withdrawal nonce that triggered the release, recorded in the emitted
    /// `Withdrawal` event for indexer correlation.
    pub async fn withdraw(
        &mut self,
        coin_type: TypeTag,
        recipient: Address,
        amount: u64,
        l2_nonce: u64,
    ) -> Result<(), BridgeError> {
        let function = bridge_function(self.package, "withdraw").with_type_args(vec![coin_type]);

        let mut builder = TransactionBuilder::new();
        let cap = builder.object(ObjectInput::new(self.cap));
        let vault = builder.object(ObjectInput::new(self.vault));
        let recipient_arg = builder.pure(&recipient);
        let amount_arg = builder.pure(&amount);
        let l2_nonce_arg = builder.pure(&l2_nonce);
        builder.move_call(
            function,
            vec![cap, vault, recipient_arg, amount_arg, l2_nonce_arg],
        );
        builder.set_sender(self.sender);

        let tx = self.build_tx(builder).await?;
        self.submit(tx, "bridge withdraw").await?;
        info!(%recipient, amount, l2_nonce, "bridge withdrawal released on Sui");
        Ok(())
    }

    /// Builds a transaction against this client's Sui connection.
    async fn build_tx(
        &self,
        builder: TransactionBuilder,
    ) -> Result<sui_sdk_types::Transaction, BridgeError> {
        let mut rpc = self.rpc.clone();
        builder
            .build(&mut rpc)
            .await
            .map_err(|e| BridgeError::Build(e.to_string()))
    }

    /// Signs, submits, and confirms a transaction, requiring on-chain success.
    ///
    /// Signs, submits, and confirms a transaction, requiring on-chain success:
    /// a tx can execute and still abort (e.g. a gas-object race).
    async fn submit(&self, tx: sui_sdk_types::Transaction, label: &str) -> Result<(), BridgeError> {
        let mut rpc = self.rpc.clone();
        let response = sign_and_execute(&mut rpc, &self.key, tx, label).await?;
        let status = response.transaction().effects().status();
        if !status.success() {
            return Err(BridgeError::Execution(format!(
                "{label} failed: {}",
                status.error().description.clone().unwrap_or_default()
            )));
        }
        Ok(())
    }

    /// Calls `bridge::initialize` on an already-published package and returns the
    /// created object IDs. Mirrors settlement auto-deploy: the `bridge` module is
    /// in the same package as `settlement`, so it is on-chain after settlement's
    /// publish — this just creates the shared `Vault` and transfers `BridgeCap` to
    /// the signer. Run once on first start; persist the IDs to config.
    pub async fn initialize(
        key_path: &Path,
        rpc_url: &str,
        package_id: &str,
    ) -> Result<DeployedBridge, BridgeError> {
        let key_str = std::fs::read_to_string(key_path)
            .map_err(BridgeError::Io)?
            .trim()
            .to_string();
        let key = crate::parse_signer_key(&key_str).map_err(BridgeError::Key)?;
        let sender = key.public_key().derive_address();
        let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| BridgeError::Rpc(e.to_string()))?;
        let package = parse_address(package_id, "package id")?;

        let mut builder = TransactionBuilder::new();
        builder.move_call(bridge_function(package, "initialize"), vec![]);
        builder.set_sender(sender);
        let tx = builder
            .build(&mut rpc)
            .await
            .map_err(|e| BridgeError::Build(e.to_string()))?;
        let changes = sign_and_execute(&mut rpc, &key, tx, "bridge initialize")
            .await?
            .transaction()
            .effects()
            .changed_objects()
            .to_vec();

        let vault_id =
            find_created_object(&changes, owner::OwnerKind::Shared).ok_or_else(|| {
                BridgeError::Execution("Vault not found in bridge initialize response".into())
            })?;
        let cap_id = find_created_object(&changes, owner::OwnerKind::Address).ok_or_else(|| {
            BridgeError::Execution("BridgeCap not found in bridge initialize response".into())
        })?;

        info!(%package_id, %vault_id, %cap_id, "bridge Vault initialized on Sui");
        Ok(DeployedBridge { vault_id, cap_id })
    }
}

/// Builds a `bridge::<name>` move-call target. `name` is always a hard-coded
/// literal, so identifier validity is a compile-time property.
fn bridge_function(package: Address, name: &str) -> Function {
    Function::new(
        package,
        Identifier::new("bridge").expect("static identifier"),
        Identifier::new(name).expect("hard-coded identifier"),
    )
}

/// Object IDs created by `bridge::initialize`.
#[derive(Debug, Clone)]
pub struct DeployedBridge {
    pub vault_id: String,
    pub cap_id: String,
}

/// Vault state the relayer and tests read, decoded from the shared object's BCS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultStatus {
    /// Next deposit nonce to be assigned on Sui.
    pub deposit_nonce: u64,
    /// Next withdraw nonce to be assigned on Sui.
    pub withdraw_nonce: u64,
    /// UID of the `deposits` table; fixed at `initialize`, reused for
    /// [`deposit_at`] lookups.
    pub deposits_table_uid: Address,
}

/// Reads the relayer-relevant vault state in one `get_object`.
pub async fn vault_status(rpc_url: &str, vault_id: &str) -> Result<VaultStatus, BridgeError> {
    let bytes = vault_contents(rpc_url, vault_id).await?;
    parse_vault_status(&bytes)
}

/// Fetches the raw BCS contents of the vault object.
async fn vault_contents(rpc_url: &str, vault_id: &str) -> Result<Vec<u8>, BridgeError> {
    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| BridgeError::Rpc(e.to_string()))?;
    let vault = parse_address(vault_id, "vault id")?;
    let response = rpc
        .ledger_client()
        .get_object(GetObjectRequest::new(&vault).with_read_mask(FieldMask::from_str("contents")))
        .await
        .map_err(|e| BridgeError::Rpc(format!("get_object: {e}")))?;
    let bytes = response
        .into_inner()
        .object
        .and_then(|o| o.contents)
        .and_then(|c| c.value)
        .ok_or_else(|| BridgeError::Execution("vault has no contents".into()))?;
    Ok(bytes.to_vec())
}

/// Mirrors the Move `Vault` layout (`move/sources/bridge.move`).
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct VaultBcs {
    id: Address,
    deposit_nonce: u64,
    withdraw_nonce: u64,
    reserves: BagBcs,
    deposits: TableBcs,
    processed_withdrawals: TableBcs,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct BagBcs {
    id: Address,
    size: u64,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct TableBcs {
    id: Address,
    size: u64,
}

/// Decodes [`VaultStatus`] from raw vault BCS.
pub fn parse_vault_status(contents: &[u8]) -> Result<VaultStatus, BridgeError> {
    let vault: VaultBcs =
        bcs::from_bytes(contents).map_err(|e| BridgeError::Parse(format!("vault BCS: {e}")))?;
    Ok(VaultStatus {
        deposit_nonce: vault.deposit_nonce,
        withdraw_nonce: vault.withdraw_nonce,
        deposits_table_uid: vault.deposits.id,
    })
}

/// Reads a single deposit by nonce via its dynamic-field object id.
///
/// `table_uid` is [`VaultStatus::deposits_table_uid`]. Returns `None` if the
/// nonce has not been deposited yet.
pub async fn deposit_at(
    rpc_url: &str,
    table_uid: &Address,
    nonce: u64,
) -> Result<Option<DepositRecord>, BridgeError> {
    let mut rpc = sui_rpc::Client::new(rpc_url).map_err(|e| BridgeError::Rpc(e.to_string()))?;
    let field_id = table_uid.derive_dynamic_child_id(&TypeTag::U64, &nonce.to_le_bytes());
    let response = rpc
        .ledger_client()
        .get_object(
            GetObjectRequest::new(&field_id).with_read_mask(FieldMask::from_str("contents")),
        )
        .await
        .map_err(|e| BridgeError::Rpc(format!("get_object (field): {e}")))?;
    let Some(obj) = response.into_inner().object else {
        return Ok(None);
    };
    let bytes = obj
        .contents
        .and_then(|c| c.value)
        .ok_or_else(|| BridgeError::Execution("field contents missing raw BCS".into()))?;
    Ok(Some(parse_field_deposit(&bytes)?))
}

/// Extracts the deposit record from a raw `Field<u64, DepositRecord>` BCS buffer.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct FieldBcs {
    id: Address,
    name: u64,
    value: DepositRecord,
}

fn parse_field_deposit(bytes: &[u8]) -> Result<DepositRecord, BridgeError> {
    let field: FieldBcs =
        bcs::from_bytes(bytes).map_err(|e| BridgeError::Parse(format!("field BCS: {e}")))?;
    if field.value.recipient_l2.len() != 20 {
        return Err(BridgeError::Parse(format!(
            "recipient_l2 must be 20 bytes, got {}",
            field.value.recipient_l2.len()
        )));
    }
    Ok(field.value)
}

fn parse_address(s: &str, label: &str) -> Result<Address, BridgeError> {
    s.parse()
        .map_err(|e| BridgeError::Parse(format!("{label} {s}: {e}")))
}

impl From<BridgeError> for CoreError {
    fn from(e: BridgeError) -> Self {
        CoreError::Network(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_key() {
        let result = BridgeClient::new(
            Path::new("/nonexistent"),
            "0x2",
            "0x3",
            "0x4",
            "https://fullnode.testnet.sui.io:443",
        );
        assert!(result.is_err());
    }

    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct VaultMirror {
        id: Address,
        deposit_nonce: u64,
        withdraw_nonce: u64,
        reserves: ReservesMirror,
        deposits: TableMirror,
        processed_withdrawals: TableMirror,
    }
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct ReservesMirror {
        id: Address,
        size: u64,
    }
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct TableMirror {
        id: Address,
        size: u64,
    }

    fn vault_contents(deposit_nonce: u64, withdraw_nonce: u64, table_uid: [u8; 32]) -> Vec<u8> {
        let vault = VaultMirror {
            id: Address::new([0; 32]),
            deposit_nonce,
            withdraw_nonce,
            reserves: ReservesMirror {
                id: Address::new([0xff; 32]),
                size: 0,
            },
            deposits: TableMirror {
                id: Address::new(table_uid),
                size: 0,
            },
            processed_withdrawals: TableMirror {
                id: Address::new([0xee; 32]),
                size: 0,
            },
        };
        bcs::to_bytes(&vault).unwrap()
    }

    #[test]
    fn parse_vault_status_reads_all_fields() {
        let mut uid = [0u8; 32];
        uid[0] = 0xab;
        uid[31] = 0xcd;
        let bytes = vault_contents(7, 3, uid);

        let status = parse_vault_status(&bytes).unwrap();
        assert_eq!(status.deposit_nonce, 7);
        assert_eq!(status.withdraw_nonce, 3);
        assert_eq!(status.deposits_table_uid, Address::new(uid));
    }

    #[test]
    fn parse_vault_status_rejects_truncated_bcs() {
        let full = vault_contents(0, 0, [0; 32]);
        // Drop trailing bytes so the table mirror cannot decode.
        assert!(parse_vault_status(&full[..full.len() - 1]).is_err());
        assert!(parse_vault_status(&[0u8; 16]).is_err());
    }

    #[derive(serde::Serialize)]
    struct FieldMirror {
        #[allow(dead_code)]
        id: Address,
        #[allow(dead_code)]
        name: u64,
        value: DepositMirror,
    }
    #[derive(serde::Serialize)]
    struct DepositMirror {
        amount: u64,
        recipient_l2: Vec<u8>,
        coin_type: Vec<u8>,
    }

    #[test]
    fn parse_field_deposit_roundtrips_against_bcs_layout() {
        let recipient = vec![0x11u8; 20];
        let coin = b"0x2::sui::SUI".to_vec();
        let field = FieldMirror {
            id: Address::new([0xaa; 32]),
            name: 5,
            value: DepositMirror {
                amount: 1_000_000_000,
                recipient_l2: recipient.clone(),
                coin_type: coin.clone(),
            },
        };
        let encoded = bcs::to_bytes(&field).unwrap();
        let parsed = parse_field_deposit(&encoded).unwrap();
        assert_eq!(parsed.amount, 1_000_000_000);
        assert_eq!(parsed.recipient_l2, recipient);
        assert_eq!(parsed.coin_type, coin);
    }

    #[test]
    fn parse_field_deposit_rejects_wrong_recipient_length() {
        let field = FieldMirror {
            id: Address::new([0; 32]),
            name: 0,
            value: DepositMirror {
                amount: 1,
                recipient_l2: vec![0; 19],
                coin_type: vec![],
            },
        };
        let encoded = bcs::to_bytes(&field).unwrap();
        assert!(parse_field_deposit(&encoded).is_err());
    }

    #[test]
    fn parse_field_deposit_rejects_truncated_bcs() {
        assert!(parse_field_deposit(&[0u8; 40]).is_err());
    }
}
