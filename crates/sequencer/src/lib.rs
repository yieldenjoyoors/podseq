//! Transaction sequencing and block signing.

#![forbid(unsafe_code)]

use std::path::Path;

use podseq_core::{Batch, BlockSigner, Error, Header, Sequencer, Signature};
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

    /// Removes and returns all pending transactions as a batch.
    pub fn drain(&mut self) -> Batch {
        let txs = std::mem::take(&mut self.pending);
        Batch { transactions: txs }
    }

    /// Returns the number of queued transactions.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

impl Sequencer for SingleSequencer {
    async fn next_batch(&self) -> Result<Batch, Error> {
        Ok(Batch {
            transactions: self.pending.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn next_batch_returns_pending() {
        let mut seq = SingleSequencer::new();
        seq.submit(vec![1, 2, 3]);
        let batch = seq.next_batch().await.unwrap();
        assert_eq!(batch.transactions, vec![vec![1, 2, 3]]);
    }

    #[tokio::test]
    async fn empty_batch_has_no_transactions() {
        let seq = SingleSequencer::new();
        let batch = seq.next_batch().await.unwrap();
        assert!(batch.transactions.is_empty());
    }

    #[test]
    fn drain_clears_pending() {
        let mut seq = SingleSequencer::new();
        seq.submit(vec![1]);
        seq.submit(vec![2]);
        let batch = seq.drain();
        assert_eq!(batch.transactions.len(), 2);
        assert_eq!(seq.pending_count(), 0);
    }
}
