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
pub fn encode(blocks: &[Block]) -> Result<Vec<u8>, Error> {
    let wire: Vec<WireBlock> = blocks.iter().map(WireBlock::from).collect();
    bcs::to_bytes(&wire).map_err(|e| Error::Serde(serde::de::Error::custom(e.to_string())))
}

/// Deserializes a batch of blocks from BCS bytes retrieved from Walrus.
///
/// These bytes cross the DA trust boundary, so the batch must be non-empty and
/// every signature must be absent or exactly 64 bytes: a corrupted signature
/// fails the decode instead of degrading into an unsigned block downstream.
///
/// # Errors
///
/// Returns [`Error::Serde`] when the bytes are not valid BCS, and
/// [`Error::InvalidBatch`] when the batch is empty or a signature is present
/// with a length other than 64.
pub fn decode(bytes: &[u8]) -> Result<Vec<Block>, Error> {
    let wire: Vec<WireBlock> = bcs::from_bytes(bytes)
        .map_err(|e| Error::Serde(serde::de::Error::custom(e.to_string())))?;
    if wire.is_empty() {
        return Err(Error::InvalidBatch("batch is empty".into()));
    }
    wire.into_iter()
        .map(|w| match w.signature {
            Some(ref sig) if sig.len() != 64 => Err(Error::InvalidBatch(format!(
                "block {} signature length {}, expected 64",
                w.height,
                sig.len()
            ))),
            _ => Ok(Block::from(w)),
        })
        .collect()
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

    fn wire_batch_bytes(signature: Option<Vec<u8>>) -> Vec<u8> {
        let wire = WireBlock {
            height: 1,
            parent_hash: [7u8; 32],
            state_root: [9u8; 32],
            timestamp: 1_700_000_000,
            data: vec![1, 2, 3],
            signature,
        };
        bcs::to_bytes(&vec![wire]).unwrap()
    }

    #[test]
    fn rejects_wrong_length_signature() {
        for len in [0usize, 1, 63, 65, 128] {
            let bytes = wire_batch_bytes(Some(vec![0u8; len]));
            let err = decode(&bytes)
                .expect_err("a corrupted signature must fail the decode, not drop the field");
            assert!(matches!(err, Error::InvalidBatch(_)), "len {len}: {err:?}");
        }
    }

    #[test]
    fn accepts_exact_signature_length() {
        let bytes = wire_batch_bytes(Some(vec![3u8; 64]));
        let blocks = decode(&bytes).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].signature, Some([3u8; 64]));
    }

    #[test]
    fn rejects_empty_batch() {
        let err = decode(&[0x00]).expect_err("an empty batch must fail the decode");
        assert!(matches!(err, Error::InvalidBatch(_)));
    }
}
