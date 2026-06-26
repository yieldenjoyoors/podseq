//! Wire-format block message for the Commonware broadcast engine.
//!
//! Wraps a [`podseq_core::Block`] with `Codec` and `Digestible` for broadcast.

#![forbid(unsafe_code)]

use bytes::{Buf, BufMut};
use commonware_codec::{EncodeSize, Error as CodecError, RangeCfg, Read, ReadRangeExt as _, Write};
use commonware_cryptography::{sha256::Digest, Digestible, Hasher, Sha256};
use podseq_core::Block;

/// A block ready for broadcast over the p2p network.
/// Digest = SHA-256(header signing message ‖ data hash).
#[derive(Debug, Clone)]
pub struct BlockMessage(pub Block);

impl BlockMessage {
    fn bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.0).expect("Block must serialize to JSON")
    }
}

impl Digestible for BlockMessage {
    type Digest = Digest;
    fn digest(&self) -> Digest {
        let header_bytes = self.0.header.signing_message();
        let mut payload = Sha256::hash(&self.0.data).to_vec();
        payload.extend_from_slice(&header_bytes);
        Sha256::hash(&payload)
    }
}

impl From<Block> for BlockMessage {
    fn from(block: Block) -> Self {
        Self(block)
    }
}
impl From<BlockMessage> for Block {
    fn from(msg: BlockMessage) -> Self {
        msg.0
    }
}

impl Write for BlockMessage {
    fn write(&self, buf: &mut impl BufMut) {
        self.bytes().write(buf);
    }
}

impl EncodeSize for BlockMessage {
    fn encode_size(&self) -> usize {
        self.bytes().encode_size()
    }
}

impl Read for BlockMessage {
    type Cfg = RangeCfg<usize>;
    fn read_cfg(buf: &mut impl Buf, range: &Self::Cfg) -> Result<Self, CodecError> {
        let data = Vec::<u8>::read_range(buf, *range)?;
        let block = serde_json::from_slice(&data)
            .map_err(|e| CodecError::Wrapped("block json", Box::new(e)))?;
        Ok(Self(block))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use podseq_core::Header;

    fn sample_block(height: u64) -> Block {
        Block {
            header: Header {
                height,
                parent_hash: [0; 32],
                state_root: [height as u8; 32],
                timestamp: height * 1000,
            },
            data: vec![0xaa; 100],
            signature: Some([0xbb; 64]),
        }
    }

    #[test]
    fn digest_is_deterministic() {
        let m1 = BlockMessage(sample_block(1));
        let m2 = BlockMessage(sample_block(1));
        assert_eq!(m1.digest(), m2.digest());
    }

    #[test]
    fn different_blocks_have_different_digests() {
        let m1 = BlockMessage(sample_block(1));
        let m2 = BlockMessage(sample_block(2));
        assert_ne!(m1.digest(), m2.digest());
    }

    #[test]
    fn codec_roundtrip() {
        let msg = BlockMessage(sample_block(7));
        let size = msg.encode_size();
        let mut buf = bytes::BytesMut::with_capacity(size);
        msg.write(&mut buf);

        let cfg = RangeCfg::from(..);
        let mut reader: &[u8] = &buf;
        let decoded = BlockMessage::read_cfg(&mut reader, &cfg).unwrap();
        assert_eq!(decoded.0.header.height, 7);
        assert_eq!(decoded.0.data, msg.0.data);
        assert_eq!(decoded.0.signature, msg.0.signature);
        assert_eq!(decoded.digest(), msg.digest());
    }
}
