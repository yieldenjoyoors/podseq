//! Block signing for podseq.
//!
//! podseq does not order transactions itself. Block contents are decided by
//! Reth (the execution client fills each block from its own mempool, subject to
//! the chain's gas limit). podseq's job is to *produce* blocks on a timer,
//! sign their headers so full nodes can attribute them to the sequencer, and
//! anchor them on DA + settlement.
//!
//! See `docs/src/components/sequencer.md` for the rationale.

#![forbid(unsafe_code)]

use std::path::Path;

use podseq_core::{BlockSigner, Error, Header, Signature};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::Signer;
use sui_sdk_types::{Address, Ed25519PublicKey, SimpleSignature};

/// Signs block headers with an ed25519 key (suiprivkey format).
pub struct Ed25519BlockSigner {
    key: Ed25519PrivateKey,
}

impl Ed25519BlockSigner {
    /// Loads the signer key from a suiprivkey file.
    pub fn from_suiprivkey_file(path: &Path) -> Result<Self, Error> {
        let key_str = std::fs::read_to_string(path)
            .map_err(|e| Error::Execution(format!("reading block key: {e}")))?
            .trim()
            .to_string();
        let key = Ed25519PrivateKey::from_suiprivkey(&key_str)
            .map_err(|e| Error::Execution(format!("invalid block key: {e}")))?;
        Ok(Self { key })
    }

    /// Returns the sequencer's ed25519 public key.
    pub fn pub_key(&self) -> Ed25519PublicKey {
        self.key.public_key()
    }

    /// Returns the Sui address derived from the public key.
    pub fn address(&self) -> Address {
        self.key.public_key().derive_address()
    }
}

impl BlockSigner for Ed25519BlockSigner {
    fn sign_header(&self, header: &Header) -> Result<Signature, Error> {
        let msg = header.signing_message();
        let sig: SimpleSignature = self
            .key
            .try_sign(&msg)
            .map_err(|e| Error::Execution(format!("signing header: {e}")))?;
        let SimpleSignature::Ed25519 { signature, .. } = sig else {
            return Err(Error::Execution("unexpected signature scheme".into()));
        };
        Ok(signature.into())
    }
}
