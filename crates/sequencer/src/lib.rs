//! Transaction sequencing and block signing.

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

/// Single-operator sequencer that orders pending transactions.
#[derive(Debug, Default)]
pub struct SingleSequencer {
    pending: Vec<Vec<u8>>,
}

impl SingleSequencer {
    /// Creates an empty sequencer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a transaction for sequencing.
    pub fn submit(&mut self, tx: Vec<u8>) {
        self.pending.push(tx);
    }

    /// Removes and returns all pending transactions in FIFO order.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_returns_pending_in_fifo_order() {
        let mut seq = SingleSequencer::new();
        seq.submit(vec![1]);
        seq.submit(vec![2]);
        seq.submit(vec![3]);
        let batch = seq.drain();
        assert_eq!(batch, vec![vec![1], vec![2], vec![3]]);
        assert!(seq.drain().is_empty());
    }

    #[test]
    fn drain_on_empty_returns_empty() {
        let mut seq = SingleSequencer::new();
        assert!(seq.drain().is_empty());
    }
}
