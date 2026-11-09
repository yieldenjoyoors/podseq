//! Exit-queue client: the escape path's request side.
//!
//! The unlock is the inbox-derived halt the Move module evaluates from chain
//! data alone (`exit_queue::halt_expired_deadline`: the oldest pending forced
//! transaction past its deadline plus the halt margin). Once that fires, a
//! user proves an L2 balance against a settled state root and records the
//! request in the shared exit queue with this module's PTB builder. The
//! queue record and its `(height, seq)` ordering are the contract the
//! redemption slice consumes; redemption itself is not wired yet.
//!
//! Docs: `docs/src/exit-queue.md`.

use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Address;
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::deadline::SUI_CLOCK_ID;
use crate::rotation::{module_function, submit_rotation_tx};
use crate::settlement::SettlementError;

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
}
