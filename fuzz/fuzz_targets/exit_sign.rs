//! Fuzzes the exit-redemption signing seam: the 99-byte payload layout, the
//! EIP-191 personal-sign preimage, and the sign → recover round trip under
//! the Move module's recovery convention.
//!
//! Oracles, all computed independently of the implementation:
//! - Payload bytes are exactly `podseq-exit:v1:` ‖ L2 address ‖ amount ‖
//!   Sui recipient. The domain string is the constant the Move module's
//!   `redeem` rebuilds its preimage from — if the Rust side drifts from it,
//!   every wallet signature stops verifying and redemptions brick, so the
//!   harness pins it.
//! - The preimage is the EIP-191 header (`\x19Ethereum Signed Message:\n` ‖
//!   decimal ASCII length) ‖ payload, for ANY input length.
//! - `sign_exit_payload` returns a 65-byte signature whose v byte is the
//!   Ethereum encoding (27/28), and recovery (normalize v, recover from
//!   keccak256 over the preimage, take keccak256(X ‖ Y)[12..32]) reproduces
//!   the caller's address exactly.

#![no_main]

use alloy_primitives::keccak256;
use libfuzzer_sys::fuzz_target;
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use podseq_sui::exit_queue::{
    eip191_preimage, eth_address, exit_payload, sign_exit_payload, EXIT_PAYLOAD_LEN,
};
use sui_sdk_types::Address;

/// The domain constant the Move module's `redeem` expects. Keep in sync with
/// `move/sources/exit_queue.move`.
const MOVE_DOMAIN: &[u8] = b"podseq-exit:v1:";

/// Recovers the L2 address the way the Move module does: normalize the
/// Ethereum v byte (27/28 → recovery id 0/1), recover from keccak256 over
/// the EIP-191 preimage, and take the last 20 bytes of keccak256(X ‖ Y).
fn recovered_address(signature: &[u8; 65], payload: &[u8]) -> Option<[u8; 20]> {
    if !matches!(signature[64], 27 | 28) {
        return None;
    }
    let digest = keccak256(eip191_preimage(payload));
    let recid = RecoveryId::from_byte(signature[64] - 27)?;
    let mut rs = [0u8; 64];
    rs.copy_from_slice(&signature[..64]);
    let sig = Signature::from_slice(&rs).ok()?;
    let key = VerifyingKey::recover_from_prehash(digest.as_slice(), &sig, recid)
        .ok()?;
    let point = key.to_sec1_point(false);
    let addr_digest = keccak256(&point.as_bytes()[1..]);
    Some(addr_digest[12..32].try_into().expect("32-byte digest has a 20-byte tail"))
}

fuzz_target!(|data: &[u8]| {
    // The preimage must carry the exact EIP-191 header for any input size.
    let preimage = eip191_preimage(data);
    let mut header = b"\x19Ethereum Signed Message:\n".to_vec();
    header.extend_from_slice(data.len().to_string().as_bytes());
    assert_eq!(&preimage[..header.len()], header.as_slice(), "EIP-191 header drift");
    assert_eq!(preimage.len(), header.len() + data.len());

    if data.len() < 84 {
        return;
    }
    let mut address = [0u8; 20];
    address.copy_from_slice(&data[0..20]);
    let mut amount = [0u8; 32];
    amount.copy_from_slice(&data[20..52]);
    let recipient = Address::new(data[52..84].try_into().expect("84 bytes yields a 32-byte tail"));
    let recipient_bytes: &[u8] = recipient.as_ref();

    let payload = exit_payload(address, amount, recipient);
    assert_eq!(payload.len(), EXIT_PAYLOAD_LEN, "payload length drift");
    assert_eq!(&payload[..15], MOVE_DOMAIN, "domain drift from the Move module");
    assert_eq!(&payload[15..35], &address, "address field drift");
    assert_eq!(&payload[35..67], &amount, "amount field drift");
    assert_eq!(&payload[67..], recipient_bytes, "recipient field drift");

    // Signing key derived from the tail; from_slice rejects the zero scalar
    // and values ≥ the curve order, which is the no-panic exit.
    let mut seed = [0u8; 32];
    let tail = &data[84..];
    seed[..tail.len().min(32)].copy_from_slice(&tail[..tail.len().min(32)]);
    let Ok(key) = SigningKey::from_slice(&seed) else {
        return;
    };
    let owner = eth_address(&key);
    // Sign the payload for the CLAIMED input address, exactly like an owner
    // wallet signs its queue record; a mismatched key/claim is the case the
    // Move module rejects, and the seam must still produce a well-formed
    // signature for it.
    let (signed_payload, signature) = sign_exit_payload(&key, address, amount, recipient);
    assert_eq!(signed_payload, payload, "signer must sign the canonical payload");
    assert!(
        matches!(signature[64], 27 | 28),
        "v byte must be the Ethereum encoding, got {}",
        signature[64]
    );
    // Recovery yields the SIGNER's address, so an honest redemption (key
    // derived address == recorded address) closes the round trip — the
    // property the Move module's owner check relies on.
    assert_eq!(
        recovered_address(&signature, &payload),
        Some(owner),
        "the Move module's recovery must reproduce the signer address"
    );
});
