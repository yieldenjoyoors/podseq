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

use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Address;
use sui_sdk_types::TypeTag;
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::deadline::SUI_CLOCK_ID;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
