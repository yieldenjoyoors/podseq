//! Exit-queue client: the escape path's request and redemption sides.
//!
//! The unlock is the inbox-derived halt the Move module evaluates from chain
//! data alone (`exit_queue::halt_expired_deadline`: the oldest pending forced
//! transaction past its deadline plus the halt margin). Once that fires, a
//! user proves an L2 balance against a settled state root and records the
//! request in the shared exit queue. Redemption consumes the record: the
//! owner signs the payout with their L2 key (EIP-191 over
//! `podseq-exit:v1:` ‖ address ‖ amount ‖ recipient), and a two-command PTB
//! — `exit_queue::redeem` then `bridge::escape_release` — pays the remainder
//! to the signed Sui recipient without any sequencer capability.
//!
//! Docs: `docs/src/exit-queue.md`.

use alloy_primitives::keccak256;
use k256::ecdsa::SigningKey;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Address;
use sui_sdk_types::TypeTag;
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::deadline::SUI_CLOCK_ID;
use crate::inbox::{InboxError, InboxTransport};
use crate::rotation::{module_function, submit_rotation_tx};
use crate::settlement::SettlementError;

/// Domain separator the Move module requires at the start of the signed
/// payload (`exit_queue::PAYLOAD_DOMAIN`).
pub const EXIT_PAYLOAD_DOMAIN: &[u8] = b"podseq-exit:v1:";

/// Byte length of the signed redemption payload: 15-byte domain ‖ 20-byte
/// address ‖ 32-byte amount ‖ 32-byte recipient.
pub const EXIT_PAYLOAD_LEN: usize = 99;

/// Object ids the `request_exit` PTB touches. All three big objects are
/// shared; the Clock is the framework singleton at `0x6`.
#[derive(Debug, Clone)]
pub struct ExitTargets {
    /// Deployed settlement Move package id.
    pub package: Address,
    /// Shared exit queue (`exit_queue::initialize`).
    pub queue: Address,
    /// Shared settlement Registry (anchors the state roots).
    pub registry: Address,
    /// Shared force-inclusion Inbox (carries the halt verdict).
    pub inbox: Address,
}

/// Object ids the redemption PTB touches. `vault` is the shared bridge
/// `Vault` whose reserve pays out; the coin type picks which reserve.
#[derive(Debug, Clone)]
pub struct RedeemTargets {
    /// Deployed settlement Move package id.
    pub package: Address,
    /// Shared exit queue.
    pub queue: Address,
    /// Shared force-inclusion Inbox (carries the halt verdict).
    pub inbox: Address,
    /// Shared bridge Vault (the paying reserve).
    pub vault: Address,
}

/// Builds the canonical redemption payload: domain ‖ L2 address ‖ big-endian
/// `amount_be` ‖ Sui recipient. This is the exact byte string the owner signs
/// with their L2 key (EIP-191 personal signature); the Move module re-derives
/// the amount from the queue record and rejects a signature over anything
/// else. `amount_be` is the 32-byte big-endian encoding of the L2-side u256
/// amount (convert it at the call site, where the L2 numeric types live).
pub fn exit_payload(address: [u8; 20], amount_be: [u8; 32], recipient: Address) -> Vec<u8> {
    let mut payload = EXIT_PAYLOAD_DOMAIN.to_vec();
    payload.extend_from_slice(&address);
    payload.extend_from_slice(&amount_be);
    payload.extend_from_slice(recipient.as_ref());
    payload
}

/// The EIP-191 personal-sign preimage for `payload`: the header
/// (`\x19Ethereum Signed Message:\n` ‖ decimal ASCII byte length) followed
/// by the payload — the byte string a wallet hashes into `personal_sign`.
/// Public so tools and the fuzz oracle hash the exact bytes the signer
/// hashed.
pub fn eip191_preimage(payload: &[u8]) -> Vec<u8> {
    let mut out = b"\x19Ethereum Signed Message:\n".to_vec();
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.extend_from_slice(payload);
    out
}

/// The Ethereum address behind an L2 key: the last 20 bytes of
/// `keccak256(X ‖ Y)` over the uncompressed public key — the same derivation
/// the Move module redoes when it verifies a redemption signature.
pub fn eth_address(key: &SigningKey) -> [u8; 20] {
    let point = key.verifying_key().to_sec1_point(false);
    let digest = keccak256(&point.as_bytes()[1..]);
    let mut out = [0u8; 20];
    out.copy_from_slice(&digest[12..]);
    out
}

/// Signs [`exit_payload`] the way wallets sign with `personal_sign`: EIP-191
/// over the 99-byte payload, hashed with keccak256, ECDSA secp256k1, and a
/// 65-byte `(r, s, v)` whose v byte is the Ethereum encoding (27 + recovery
/// id; the Move module normalizes both forms). Returns the payload that was
/// signed alongside the signature, so callers bind what they broadcast.
pub fn sign_exit_payload(
    key: &SigningKey,
    address: [u8; 20],
    amount_be: [u8; 32],
    recipient: Address,
) -> (Vec<u8>, [u8; 65]) {
    let payload = exit_payload(address, amount_be, recipient);
    let digest = keccak256(eip191_preimage(&payload));
    let (signature, recid) = key.sign_prehash_recoverable(digest.as_slice());
    let mut out = [0u8; 65];
    out[..64].copy_from_slice(&signature.to_bytes());
    out[64] = 27 + recid.to_byte();
    (payload, out)
}

/// Appends `exit_queue::request_exit` to `builder`.
///
/// `address` is the 20-byte L2 address the balance is proven for; `proof` is
/// the root-first RLP node list (`eth_getProof` shape) against the state
/// root anchored at `height`. Recording is infallible: only
/// `TransactionBuilder::build` talks to the network, and the Move module /
/// function names are compile-time constants.
pub fn append_request_exit(
    builder: &mut TransactionBuilder,
    targets: &ExitTargets,
    height: u64,
    address: [u8; 20],
    proof: Vec<Vec<u8>>,
) {
    let queue_arg = builder.object(ObjectInput::new(targets.queue));
    let registry_arg = builder.object(ObjectInput::new(targets.registry));
    let inbox_arg = builder.object(ObjectInput::new(targets.inbox));
    let clock_arg = builder.object(ObjectInput::new(SUI_CLOCK_ID));
    let height_arg = builder.pure(&height);
    let address_arg = builder.pure(&address.to_vec());
    let proof_arg = builder.pure(&proof);
    builder.move_call(
        module_function(targets.package, "exit_queue", "request_exit").expect("static name"),
        vec![
            queue_arg,
            registry_arg,
            inbox_arg,
            clock_arg,
            height_arg,
            address_arg,
            proof_arg,
        ],
    );
}

/// One redemption call's owner-side inputs — exactly what the Move module
/// verifies: the recorded L2 address, the signed recipient, the signed
/// payload ([`exit_payload`]), the 65-byte EIP-191 signature, and the coin
/// type whose vault reserve pays out.
#[derive(Debug, Clone)]
pub struct RedeemRequest {
    /// The recorded 20-byte L2 address.
    pub address: [u8; 20],
    /// The Sui address the vault pays (must be what the signature binds).
    pub recipient: Address,
    /// The signed payload bytes.
    pub payload: Vec<u8>,
    /// The 65-byte `(r, s, v)` signature over `payload`.
    pub signature: Vec<u8>,
    /// The coin type paying out.
    pub coin_type: TypeTag,
}

/// Appends the redemption pair to `builder`: `exit_queue::redeem` verifies
/// the halt verdict, the queue record, and the owner's signature, then mints
/// the claim; `bridge::escape_release` consumes it and pays the signed
/// recipient from the vault's reserve.
///
/// Building is infallible; the on-chain gates decide at execution time.
pub fn append_redeem(
    builder: &mut TransactionBuilder,
    targets: &RedeemTargets,
    request: &RedeemRequest,
) {
    let queue_arg = builder.object(ObjectInput::new(targets.queue));
    let inbox_arg = builder.object(ObjectInput::new(targets.inbox));
    let clock_arg = builder.object(ObjectInput::new(SUI_CLOCK_ID));
    let address_arg = builder.pure(&request.address.to_vec());
    let recipient_arg = builder.pure(&request.recipient);
    let payload_arg = builder.pure(&request.payload);
    let signature_arg = builder.pure(&request.signature);
    let claim = builder.move_call(
        module_function(targets.package, "exit_queue", "redeem").expect("static name"),
        vec![
            queue_arg,
            inbox_arg,
            clock_arg,
            address_arg,
            recipient_arg,
            payload_arg,
            signature_arg,
        ],
    );
    let vault_arg = builder.object(ObjectInput::new(targets.vault));
    builder.move_call(
        module_function(targets.package, "bridge", "escape_release")
            .expect("static name")
            .with_type_args(vec![request.coin_type.clone()]),
        vec![vault_arg, claim],
    );
}

/// Builds, signs, and confirms a `request_exit` PTB.
///
/// The chain-side gate decides: a PTB against a healthy chain aborts with
/// `E_NOT_HALTED`, and a bad proof aborts in the verifier. Nothing here
/// second-guesses what chain data already settles.
///
/// # Errors
///
/// Rpc/Build/Execution errors from the submit path (an on-chain abort
/// arrives as `Execution`).
///
/// # Panics
///
/// Only if the hard-coded module/function names stop being valid Sui
/// identifiers, which is a compile-time property.
pub async fn submit_request_exit(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &ExitTargets,
    height: u64,
    address: [u8; 20],
    proof: Vec<Vec<u8>>,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, key, "exit request", |builder| {
        append_request_exit(builder, targets, height, address, proof);
    })
    .await
}

/// Builds, signs, and confirms a redemption PTB.
///
/// The chain-side gates decide everything: the halt verdict, the recorded
/// remainder, the signature bindings, and the vault's liquidity. A PTB
/// against a healthy chain aborts with `E_NOT_HALTED`; a replayed signature
/// aborts because the remainder it names no longer matches the record.
///
/// # Errors
///
/// Rpc/Build/Execution errors from the submit path (an on-chain abort
/// arrives as `Execution`).
///
/// # Panics
///
/// Only if the hard-coded module/function names stop being valid Sui
/// identifiers, which is a compile-time property.
pub async fn submit_redeem(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &RedeemTargets,
    request: &RedeemRequest,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, key, "exit redemption", |builder| {
        append_redeem(builder, targets, request);
    })
    .await
}

// ---------- ordered redemption ----------
//
// The queue serves exits in `(height, seq)` order. These pieces read the
// order (so a user can tell where a record stands before signing anything)
// and drive the valve that keeps strict order from becoming a veto.

/// Object ids the `skip_head` valve PTB touches.
#[derive(Debug, Clone)]
pub struct SkipTargets {
    /// Deployed settlement Move package id.
    pub package: Address,
    /// Shared exit queue.
    pub queue: Address,
}

/// Appends `exit_queue::skip_head` to `builder`: demotes a head that has
/// sat past `skip_after_ms` without its owner redeeming, sending it behind
/// every indexed record.
///
/// Building is infallible; the valve's clock gate decides on-chain.
pub fn append_skip_head(builder: &mut TransactionBuilder, targets: &SkipTargets) {
    let queue_arg = builder.object(ObjectInput::new(targets.queue));
    let clock_arg = builder.object(ObjectInput::new(SUI_CLOCK_ID));
    builder.move_call(
        module_function(targets.package, "exit_queue", "skip_head").expect("static name"),
        vec![queue_arg, clock_arg],
    );
}

/// Builds, signs, and confirms a `skip_head` PTB.
///
/// The chain-side gate decides: a head inside its window aborts with
/// `E_SKIP_EARLY`, an empty index with `E_QUEUE_EMPTY`.
///
/// # Errors
///
/// Rpc/Build/Execution errors from the submit path (an on-chain abort
/// arrives as `Execution`).
///
/// # Panics
///
/// Only if the hard-coded module/function names stop being valid Sui
/// identifiers, which is a compile-time property.
pub async fn submit_skip_head(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    targets: &SkipTargets,
) -> Result<(), SettlementError> {
    submit_rotation_tx(rpc_url, key, "exit queue skip", |builder| {
        append_skip_head(builder, targets);
    })
    .await
}

/// Client-side mirror of `exit_queue::QueueNode` — one node of the ordered
/// index's linked list over payable records.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueueNode {
    /// The record served immediately before this one, `None` at the head.
    pub prev: Option<Vec<u8>>,
    /// The record served immediately after this one, `None` at the tail.
    pub next: Option<Vec<u8>>,
    /// Clock reading when the record entered the index; the skip valve's
    /// deadline is this plus `skip_after_ms`.
    pub recorded_at_ms: u64,
}

/// BCS mirror of the module's unit key `QueueHeadKey` (serializes to zero
/// bytes, like the Move struct).
#[derive(serde::Serialize, serde::Deserialize)]
struct QueueHeadKeyBcs;

/// BCS mirror of `exit_queue::QueueAddrKey` / `SkipMarkKey` — a key struct
/// wrapping the 20-byte L2 address.
#[derive(serde::Serialize, serde::Deserialize)]
struct QueueAddrKeyBcs {
    address: Vec<u8>,
}

/// The `Field<K, V>` shape Sui stores for a dynamic field: object id, the
/// key, and the value.
#[derive(serde::Deserialize)]
struct FieldBcs<K, V> {
    #[allow(dead_code)]
    id: Address,
    #[allow(dead_code)]
    name: K,
    value: V,
}

fn queue_key_type_tag(package: Address, name: &str) -> TypeTag {
    // StructTag's fields are private; the string form parses into the same
    // canonical tag the Move module declares.
    let tag: sui_sdk_types::StructTag = format!("{package}::exit_queue::{name}")
        .parse()
        .expect("static type tag");
    TypeTag::Struct(Box::new(tag))
}

/// Returns the dynamic-field object id holding the ordered index's head
/// pointer; mirrors `exit_queue::QueueHeadKey` under `queue`.
#[must_use]
pub fn queue_head_field_id(queue: Address, package: Address) -> Address {
    let key = bcs::to_bytes(&QueueHeadKeyBcs).expect("unit key serializes to zero bytes");
    queue.derive_dynamic_child_id(&queue_key_type_tag(package, "QueueHeadKey"), &key)
}

/// Returns the dynamic-field object id holding `address`'s index node;
/// mirrors `exit_queue::QueueAddrKey { address }` under `queue`.
#[must_use]
pub fn queue_node_field_id(queue: Address, package: Address, address: [u8; 20]) -> Address {
    let key = bcs::to_bytes(&QueueAddrKeyBcs {
        address: address.to_vec(),
    })
    .expect("a vector serializes");
    queue.derive_dynamic_child_id(&queue_key_type_tag(package, "QueueAddrKey"), &key)
}

/// Returns the dynamic-field object id marking `address`'s record skipped;
/// mirrors `exit_queue::SkipMarkKey { address }` under `queue`.
#[must_use]
pub fn skip_mark_field_id(queue: Address, package: Address, address: [u8; 20]) -> Address {
    let key = bcs::to_bytes(&QueueAddrKeyBcs {
        address: address.to_vec(),
    })
    .expect("a vector serializes");
    queue.derive_dynamic_child_id(&queue_key_type_tag(package, "SkipMarkKey"), &key)
}

/// Decodes the value of a dynamic-field `Field<K, V>` blob.
///
/// # Errors
///
/// Truncated or malformed field BCS.
fn parse_queue_field<K: serde::de::DeserializeOwned, V: serde::de::DeserializeOwned>(
    field_bcs: &[u8],
    what: &'static str,
) -> Result<V, InboxError> {
    let field: FieldBcs<K, V> = bcs::from_bytes(field_bcs)
        .map_err(|e| InboxError::Parse(format!("{what} field BCS: {e}")))?;
    Ok(field.value)
}

/// Reads the ordered redemption index over the mockable transport: the
/// head, a record's node (its neighbours and valve clock), and its skip
/// mark.
#[derive(Debug, Clone)]
pub struct QueueOrderReader<T> {
    transport: T,
    queue: Address,
    package: Address,
}

impl<T: InboxTransport> QueueOrderReader<T> {
    /// Creates a reader for the exit queue at `queue` under `package`.
    pub fn new(transport: T, queue: Address, package: Address) -> Self {
        Self {
            transport,
            queue,
            package,
        }
    }

    /// The ordered index's head — the record every redemption must serve
    /// next. `Ok(None)` when the index is empty (no payable record is
    /// ordered ahead of anyone).
    ///
    /// # Errors
    ///
    /// Rpc failures from the transport, or BCS decode errors on a present
    /// field.
    pub async fn head(&self) -> Result<Option<Vec<u8>>, InboxError> {
        match self
            .transport
            .object_contents(&queue_head_field_id(self.queue, self.package))
            .await
        {
            Ok(bytes) => Ok(Some(parse_queue_field::<QueueHeadKeyBcs, Vec<u8>>(
                &bytes,
                "queue head",
            )?)),
            Err(InboxError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `address`'s node — its neighbours and valve clock. `Ok(None)` when
    /// the record holds no node: it is fully paid, zero-balance, or skipped,
    /// and it redeems behind the whole index.
    ///
    /// # Errors
    ///
    /// Rpc failures from the transport, or BCS decode errors on a present
    /// field.
    pub async fn node(&self, address: [u8; 20]) -> Result<Option<QueueNode>, InboxError> {
        match self
            .transport
            .object_contents(&queue_node_field_id(self.queue, self.package, address))
            .await
        {
            Ok(bytes) => Ok(Some(parse_queue_field::<QueueAddrKeyBcs, QueueNode>(
                &bytes,
                "queue node",
            )?)),
            Err(InboxError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether the valve has demoted `address`'s record.
    ///
    /// # Errors
    ///
    /// Rpc failures from the transport.
    pub async fn is_skipped(&self, address: [u8; 20]) -> Result<bool, InboxError> {
        match self
            .transport
            .object_contents(&skip_mark_field_id(self.queue, self.package, address))
            .await
        {
            Ok(_) => Ok(true),
            Err(InboxError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::keccak256;
    use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};

    /// Mirrors the Move module's `recover_eth_address`: normalize the v byte,
    /// recover from keccak256 over the EIP-191 preimage, and take the last 20
    /// bytes of keccak256(X ‖ Y). The owner-side signer must produce exactly
    /// what this recovery accepts — the live bench redeem depends on it.
    fn recovered_address(signature: &[u8; 65], payload: &[u8]) -> [u8; 20] {
        let preimage = eip191_preimage(payload);
        let digest = keccak256(&preimage);
        let v = signature[64];
        let normalized = if v >= 27 { v - 27 } else { v };
        let recid = RecoveryId::from_byte(normalized).expect("normalized v is 0..4");
        let mut rs = [0u8; 64];
        rs.copy_from_slice(&signature[..64]);
        let sig = Signature::from_slice(&rs).expect("r ‖ s parses");
        let recovered = VerifyingKey::recover_from_prehash(digest.as_slice(), &sig, recid)
            .expect("recovers under the module's conventions");
        let point = recovered.to_sec1_point(false);
        let addr_digest = keccak256(&point.as_bytes()[1..]);
        let mut out = [0u8; 20];
        out.copy_from_slice(&addr_digest[12..]);
        out
    }

    #[test]
    fn sign_exit_payload_recovers_the_owner_like_the_module() {
        let key = SigningKey::from_slice(&[0x42; 32]).expect("valid scalar");
        let owner = eth_address(&key);
        let mut amount_be = [0u8; 32];
        amount_be[31] = 200;
        let recipient = Address::new([0xEE; 32]);

        let (payload, signature) = sign_exit_payload(&key, owner, amount_be, recipient);
        // The signature is 65 bytes with the Ethereum v encoding the module
        // normalizes (27/28 → recovery id 0/1).
        assert_eq!(signature.len(), 65);
        assert!(
            (27..=28).contains(&signature[64]),
            "v byte {} is not the Ethereum encoding",
            signature[64]
        );

        // The signed payload is the canonical one, and the module's recovery
        // lands on the owner address the payload names.
        let mut amount_be_again = [0u8; 32];
        amount_be_again[31] = 200;
        assert_eq!(payload, exit_payload(owner, amount_be_again, recipient));
        assert_eq!(recovered_address(&signature, &payload), owner);
    }

    #[test]
    fn request_exit_append_records_without_a_network_round_trip() {
        // The builder records the commands offline; only `.build(rpc)`
        // touches the network, so the PTB shape is exercisable in a unit
        // test. The on-chain gate (halt verdict, proof verifier) is the
        // Move module's job, not this builder's.
        let mut builder = TransactionBuilder::new();
        let targets = ExitTargets {
            package: Address::new([0x11; 32]),
            queue: Address::new([0x22; 32]),
            registry: Address::new([0x33; 32]),
            inbox: Address::new([0x44; 32]),
        };
        append_request_exit(
            &mut builder,
            &targets,
            7,
            [0xAB; 20],
            vec![vec![0xd1; 8], vec![0x82; 4]],
        );
    }

    #[test]
    fn empty_proof_still_records_offline() {
        // An empty proof records fine here and aborts on-chain with
        // E_EMPTY_PROOF: the builder does not pre-validate what the
        // verifier rejects.
        let mut builder = TransactionBuilder::new();
        let targets = ExitTargets {
            package: Address::new([0x11; 32]),
            queue: Address::new([0x22; 32]),
            registry: Address::new([0x33; 32]),
            inbox: Address::new([0x44; 32]),
        };
        append_request_exit(&mut builder, &targets, 1, [0xCD; 20], vec![]);
    }

    #[test]
    fn exit_payload_matches_the_documented_layout() {
        // domain ‖ address ‖ amount ‖ recipient, exactly 99 bytes, amount
        // big-endian in 32 bytes.
        let mut amount_be = [0u8; 32];
        amount_be[30] = 0x04;
        amount_be[31] = 0xd2;
        let payload = exit_payload([0xAB; 20], amount_be, Address::new([0xC0; 32]));
        assert_eq!(payload.len(), EXIT_PAYLOAD_LEN);
        assert_eq!(&payload[..15], EXIT_PAYLOAD_DOMAIN);
        assert_eq!(&payload[15..35], &[0xAB; 20]);
        assert_eq!(&payload[35..67], &amount_be);
        assert_eq!(&payload[67..99], &[0xC0; 32]);
    }

    #[test]
    fn redeem_append_chains_both_commands_offline() {
        // The claim argument must flow from redeem into escape_release; the
        // builder records the pair offline, and the Move module's halt
        // verdict, signature bindings, and liquidity checks decide on-chain.
        let mut builder = TransactionBuilder::new();
        let targets = RedeemTargets {
            package: Address::new([0x11; 32]),
            queue: Address::new([0x22; 32]),
            inbox: Address::new([0x44; 32]),
            vault: Address::new([0x55; 32]),
        };
        let mut amount_be = [0u8; 32];
        amount_be[31] = 200;
        let payload = exit_payload([0xAB; 20], amount_be, Address::new([0xEE; 32]));
        let request = RedeemRequest {
            address: [0xAB; 20],
            recipient: Address::new([0xEE; 32]),
            payload,
            signature: vec![0x5a; 65],
            coin_type: "0x2::sui::SUI".parse().expect("valid type tag"),
        };
        append_redeem(&mut builder, &targets, &request);
    }

    #[test]
    fn skip_append_records_without_a_network_round_trip() {
        // The valve PTB records offline: queue first, then the Clock
        // singleton, matching the Move signature `skip_head(queue, clock)`.
        let mut builder = TransactionBuilder::new();
        let targets = SkipTargets {
            package: Address::new([0x11; 32]),
            queue: Address::new([0x22; 32]),
        };
        append_skip_head(&mut builder, &targets);
    }

    // ---------- ordered-index read model ----------

    const QUEUE: Address = Address::new([0x66; 32]);
    const PACKAGE: Address = Address::new([0x11; 32]);

    /// The `Field<K, V>` shape for fixtures: BCS is id ‖ name ‖ value,
    /// matching the on-chain dynamic-field layout.
    #[derive(serde::Serialize)]
    struct FieldFix<K, V> {
        id: Address,
        name: K,
        value: V,
    }

    #[test]
    fn queue_field_ids_are_deterministic_and_distinct() {
        let head = queue_head_field_id(QUEUE, PACKAGE);
        assert_eq!(head, queue_head_field_id(QUEUE, PACKAGE));
        let node_a = queue_node_field_id(QUEUE, PACKAGE, [0xAB; 20]);
        let node_b = queue_node_field_id(QUEUE, PACKAGE, [0xCD; 20]);
        // Same queue, different addresses -> different nodes; the head,
        // node, and skip-mark ids all differ from one another.
        assert_ne!(node_a, node_b);
        assert_ne!(head, node_a);
        let skip = skip_mark_field_id(QUEUE, PACKAGE, [0xAB; 20]);
        assert_ne!(skip, node_a);
        assert_ne!(skip, head);
        // A different queue repoints every id.
        assert_ne!(queue_head_field_id(Address::new([0x77; 32]), PACKAGE), head);
    }

    #[tokio::test]
    async fn reader_decodes_a_present_head() {
        let head_bytes = vec![0xAB; 20];
        let field = bcs::to_bytes(&FieldFix {
            id: Address::new([0x01; 32]),
            name: QueueHeadKeyBcs,
            value: head_bytes.clone(),
        })
        .expect("serializes");
        let transport =
            MockTransport::default().with_object(queue_head_field_id(QUEUE, PACKAGE), field);
        let head = QueueOrderReader::new(transport, QUEUE, PACKAGE)
            .head()
            .await
            .expect("reads");
        assert_eq!(head, Some(head_bytes));
    }

    #[tokio::test]
    async fn reader_maps_an_empty_index_to_none() {
        let transport = MockTransport::default();
        let head = QueueOrderReader::new(transport, QUEUE, PACKAGE)
            .head()
            .await
            .expect("missing is not an error");
        assert_eq!(head, None);
    }

    #[tokio::test]
    async fn reader_decodes_a_node_with_neighbours_and_clock() {
        let node = QueueNode {
            prev: Some(vec![0xAB; 20]),
            next: None,
            recorded_at_ms: 1_234_567,
        };
        let field = bcs::to_bytes(&FieldFix {
            id: Address::new([0x02; 32]),
            name: QueueAddrKeyBcs {
                address: vec![0xCD; 20],
            },
            value: node.clone(),
        })
        .expect("serializes");
        let transport = MockTransport::default()
            .with_object(queue_node_field_id(QUEUE, PACKAGE, [0xCD; 20]), field);
        let read = QueueOrderReader::new(transport, QUEUE, PACKAGE)
            .node([0xCD; 20])
            .await
            .expect("reads");
        assert_eq!(read, Some(node));
    }

    #[tokio::test]
    async fn reader_maps_a_nodeless_record_to_none() {
        let transport = MockTransport::default();
        let read = QueueOrderReader::new(transport, QUEUE, PACKAGE)
            .node([0xAB; 20])
            .await
            .expect("missing is not an error");
        assert_eq!(read, None);
    }

    #[tokio::test]
    async fn reader_reads_the_skip_mark_by_field_presence() {
        let transport = MockTransport::default().with_object(
            skip_mark_field_id(QUEUE, PACKAGE, [0xAB; 20]),
            // The mark's value is a bool; presence is what the reader
            // checks, and the BCS `true` byte rides along.
            bcs::to_bytes(&FieldFix {
                id: Address::new([0x03; 32]),
                name: QueueAddrKeyBcs {
                    address: vec![0xAB; 20],
                },
                value: true,
            })
            .expect("serializes"),
        );
        let reader = QueueOrderReader::new(transport, QUEUE, PACKAGE);
        assert!(reader.is_skipped([0xAB; 20]).await.expect("reads"));
        assert!(!reader.is_skipped([0xCD; 20]).await.expect("reads"));
    }

    #[tokio::test]
    async fn reader_propagates_transport_errors() {
        let err = QueueOrderReader::new(FailingTransport, QUEUE, PACKAGE)
            .head()
            .await
            .expect_err("transport errors propagate");
        assert!(matches!(err, InboxError::Rpc(_)));
    }

    /// Mock at the `get_object` boundary: present ids serve contents, absent
    /// ids map to `NotFound`.
    #[derive(Default)]
    struct MockTransport {
        objects: std::collections::BTreeMap<Address, Vec<u8>>,
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
