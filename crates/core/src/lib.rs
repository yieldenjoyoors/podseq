//! Core types and interfaces for the podseq sequencer.

#![forbid(unsafe_code)]

pub mod runtime;

use std::fmt;
use std::future::Future;

use serde::{Deserialize, Serialize};

/// A 32-byte hash identifying a block or header.
pub type Hash = [u8; 32];

/// A 64-byte signature produced by a block signer.
pub type Signature = [u8; 64];

/// Identifier for a blob published to the data availability layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobId(pub Hash);

/// Sequencer-produced block header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub height: u64,
    pub parent_hash: Hash,
    pub state_root: Hash,
    pub timestamp: u64,
}

/// A sequencer block: header, opaque payload data, and optional signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub header: Header,
    pub data: Vec<u8>,
    #[serde(with = "serde_signature")]
    pub signature: Option<Signature>,
}

/// An ordered batch of transactions to be executed into a block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Batch {
    pub transactions: Vec<Vec<u8>>,
}

/// Produces the next batch of transactions for execution.
pub trait Sequencer: Send + Sync {
    fn next_batch(&self) -> impl Future<Output = Result<Batch, Error>> + Send;
}

/// Executes a batch of transactions and produces a block.
pub trait Executor: Send + Sync {
    fn execute(&self, batch: &Batch) -> impl Future<Output = Result<Block, Error>> + Send;
}

/// Treats a blob as a batch: one publish stores one or more blocks under a single blob id, and a fetch returns the whole batch.
pub trait DataAvailability: Send + Sync {
    fn publish(&self, blocks: &[Block]) -> impl Future<Output = Result<BlobId, Error>> + Send;
    fn fetch(&self, id: &BlobId) -> impl Future<Output = Result<Vec<Block>, Error>> + Send;
}

/// Commits a block and its DA blob id to the settlement layer.
pub trait Settlement: Send + Sync {
    fn commit(
        &self,
        block: &Block,
        blob: &BlobId,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Signs a header to produce a block signature.
pub trait BlockSigner: Send + Sync {
    fn sign_header(&self, header: &Header) -> Result<Signature, Error>;
}

impl Header {
    /// BCS-like encoding of the header fields for signing.
    pub fn signing_message(&self) -> Vec<u8> {
        let mut msg = Vec::with_capacity(8 + 32 + 32 + 8);
        msg.extend_from_slice(&self.height.to_le_bytes());
        msg.extend_from_slice(&self.parent_hash);
        msg.extend_from_slice(&self.state_root);
        msg.extend_from_slice(&self.timestamp.to_le_bytes());
        msg
    }
}

/// Errors returned by core sequencer operations.
#[derive(Debug)]
pub enum Error {
    InvalidBlock(String),
    Execution(String),
    DataAvailability(String),
    Settlement(String),
    Network(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidBlock(msg) => write!(f, "invalid block: {msg}"),
            Error::Execution(msg) => write!(f, "execution failure: {msg}"),
            Error::DataAvailability(msg) => write!(f, "data availability failure: {msg}"),
            Error::Settlement(msg) => write!(f, "settlement failure: {msg}"),
            Error::Network(msg) => write!(f, "network failure: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// Serde helpers for the `Signature` type (arrays > 32 don't auto-derive serde).
mod serde_signature {
    use super::Signature;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(sig: &Option<Signature>, s: S) -> Result<S::Ok, S::Error> {
        sig.map(|arr| arr.to_vec()).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Signature>, D::Error> {
        let opt: Option<Vec<u8>> = Option::deserialize(d)?;
        opt.map(|v| {
            v.try_into()
                .map_err(|_| serde::de::Error::custom("expected 64-byte signature"))
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_id_roundtrips() {
        let id = BlobId([1u8; 32]);
        assert_eq!(id.0, [1u8; 32]);
    }

    #[test]
    fn batch_defaults_empty() {
        assert!(Batch::default().transactions.is_empty());
    }

    #[test]
    fn error_displays() {
        let err = Error::DataAvailability("timeout".into());
        assert_eq!(err.to_string(), "data availability failure: timeout");
    }

    #[test]
    fn header_signing_message_is_deterministic() {
        let h = Header {
            height: 42,
            parent_hash: [1; 32],
            state_root: [2; 32],
            timestamp: 100,
        };
        let msg1 = h.signing_message();
        let msg2 = h.signing_message();
        assert_eq!(msg1, msg2);
        assert_eq!(msg1.len(), 80);
    }
}
