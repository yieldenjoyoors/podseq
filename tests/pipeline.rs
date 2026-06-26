//! Integration tests for the podseq pipeline.
//!
//! These tests exercise block signing and the BCS wire encoding without
//! external services. P2P broadcast tests live with the Commonware transport
//! once it lands.

use podseq_core::{Block, BlockSigner, Header};

fn sample_block(height: u64) -> Block {
    Block {
        header: Header {
            height,
            parent_hash: [0; 32],
            state_root: [height as u8; 32],
            timestamp: height * 1000,
        },
        data: vec![0xde, 0xad, 0xbe, 0xef],
        signature: None,
    }
}

/// A test block signer that uses a fixed key.
struct TestSigner {
    key: sui_crypto::ed25519::Ed25519PrivateKey,
}

impl TestSigner {
    fn new() -> Self {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        Self {
            key: sui_crypto::ed25519::Ed25519PrivateKey::generate(&mut rng),
        }
    }

    fn verifying_key(&self) -> sui_crypto::ed25519::Ed25519VerifyingKey {
        self.key.verifying_key()
    }
}

impl BlockSigner for TestSigner {
    fn sign_header(&self, header: &Header) -> Result<podseq_core::Signature, podseq_core::Error> {
        let msg = header.signing_message();
        use sui_crypto::Signer;
        let sig: sui_sdk_types::SimpleSignature = self
            .key
            .try_sign(&msg)
            .map_err(|e| podseq_core::Error::Execution(e.to_string()))?;
        let sui_sdk_types::SimpleSignature::Ed25519 { signature, .. } = sig else {
            return Err(podseq_core::Error::Execution("unexpected scheme".into()));
        };
        Ok(signature.into())
    }
}

/// Verify a block's signature against a known public key via the shared helper.
fn verify_block(
    block: &Block,
    pubkey: &sui_crypto::ed25519::Ed25519VerifyingKey,
) -> Result<(), String> {
    podseq_sui::verify_block_signature(block, pubkey).map_err(|e| e.to_string())
}

#[test]
fn sign_and_verify_block() {
    let signer = TestSigner::new();
    let pubkey = signer.verifying_key();

    let mut block = sample_block(1);
    block.signature = Some(signer.sign_header(&block.header).unwrap());

    verify_block(&block, &pubkey).unwrap();
}

#[test]
fn sign_and_verify_tampered_block_fails() {
    let signer = TestSigner::new();
    let pubkey = signer.verifying_key();

    let mut block = sample_block(1);
    block.signature = Some(signer.sign_header(&block.header).unwrap());

    // Tamper with the block height.
    block.header.height = 999;

    assert!(verify_block(&block, &pubkey).is_err());
}

#[test]
fn unsigned_block_is_rejected() {
    let signer = TestSigner::new();
    let pubkey = signer.verifying_key();

    // No signature set; the helper must reject it outright.
    let block = sample_block(1);
    assert!(block.signature.is_none());
    assert!(verify_block(&block, &pubkey).is_err());
}

#[test]
fn bcs_wire_roundtrip_preserves_signature() {
    let signer = TestSigner::new();
    let mut block = sample_block(7);
    block.signature = Some(signer.sign_header(&block.header).unwrap());

    #[derive(serde::Serialize, serde::Deserialize)]
    struct WireBlock {
        height: u64,
        parent_hash: [u8; 32],
        state_root: [u8; 32],
        timestamp: u64,
        data: Vec<u8>,
        signature: Option<Vec<u8>>,
    }

    let wire = WireBlock {
        height: block.header.height,
        parent_hash: block.header.parent_hash,
        state_root: block.header.state_root,
        timestamp: block.header.timestamp,
        data: block.data.clone(),
        signature: block.signature.map(|s| s.to_vec()),
    };

    let encoded = bcs::to_bytes(&wire).unwrap();
    assert!(encoded.len() < 200);

    let decoded: WireBlock = bcs::from_bytes(&encoded).unwrap();
    assert_eq!(decoded.height, block.header.height);
    assert_eq!(decoded.signature.unwrap().len(), 64);
}
