//! Wire format for blocks stored on Walrus.
//!
//! Uses BCS (Binary Canonical Serialization) for compact, deterministic encoding.
//! A [`podseq_core::Block`] serializes as: header fields + data + optional signature.

use podseq_core::{Block, Header};
use serde::{Deserialize, Serialize};

use crate::Error;

/// On-blob serialization of a block.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireBlock {
    pub height: u64,
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub timestamp: u64,
    pub data: Vec<u8>,
    pub signature: Option<Vec<u8>>,
}

impl From<&Block> for WireBlock {
    fn from(block: &Block) -> Self {
        let h = &block.header;
        Self {
            height: h.height,
            parent_hash: h.parent_hash,
            state_root: h.state_root,
            timestamp: h.timestamp,
            data: block.data.clone(),
            signature: block.signature.map(|s| s.to_vec()),
        }
    }
}

impl From<WireBlock> for Block {
    fn from(w: WireBlock) -> Self {
        Self {
            header: Header {
                height: w.height,
                parent_hash: w.parent_hash,
                state_root: w.state_root,
                timestamp: w.timestamp,
            },
            data: w.data,
            signature: w.signature.and_then(|s| s.try_into().ok()),
        }
    }
}

/// Serializes a batch of blocks into BCS bytes to store on Walrus.
pub(crate) fn encode(blocks: &[Block]) -> Result<Vec<u8>, Error> {
    let wire: Vec<WireBlock> = blocks.iter().map(WireBlock::from).collect();
    bcs::to_bytes(&wire).map_err(|e| Error::Serde(serde::de::Error::custom(e.to_string())))
}

/// Deserializes a batch of blocks from BCS bytes retrieved from Walrus.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<Block>, Error> {
    let wire: Vec<WireBlock> = bcs::from_bytes(bytes)
        .map_err(|e| Error::Serde(serde::de::Error::custom(e.to_string())))?;
    Ok(wire.into_iter().map(Block::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_block() -> Block {
        Block {
            header: Header {
                height: 42,
                parent_hash: [1; 32],
                state_root: [2; 32],
                timestamp: 1_700_000_000,
            },
            data: vec![0xde, 0xad, 0xbe, 0xef],
            signature: None,
        }
    }

    fn sample_block_signed() -> Block {
        let mut b = sample_block();
        b.signature = Some([0xab; 64]);
        b
    }

    #[test]
    fn roundtrips_block() {
        let original = vec![sample_block()];
        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].header, original[0].header);
        assert_eq!(decoded[0].data, original[0].data);
        assert_eq!(decoded[0].signature, original[0].signature);
    }

    #[test]
    fn roundtrips_signed_block() {
        let original = vec![sample_block_signed()];
        let encoded = encode(&original).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded[0].signature, original[0].signature);
    }

    #[test]
    fn roundtrips_batch() {
        let batch = vec![sample_block(), sample_block_signed(), sample_block()];
        let encoded = encode(&batch).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.len(), batch.len());
        for (d, b) in decoded.iter().zip(batch.iter()) {
            assert_eq!(d.header, b.header);
            assert_eq!(d.data, b.data);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"not bcs").is_err());
    }

    #[test]
    fn bcs_is_smaller_than_json() {
        let block = vec![sample_block()];
        let bcs_size = encode(&block).unwrap().len();
        let json_size = serde_json::to_vec(&WireBlock::from(&sample_block()))
            .unwrap()
            .len();
        assert!(
            bcs_size < json_size,
            "BCS ({bcs_size}) should be < JSON ({json_size})"
        );
    }
}
