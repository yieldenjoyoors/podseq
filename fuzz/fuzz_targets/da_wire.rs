//! Fuzzes the DA wire path: the BCS batch codec crossing the Walrus trust
//! boundary, plus the blob-id encoding around it.
//!
//! Oracles: (1) arbitrary bytes never panic the decoder, any successful decode
//! is non-empty, and decoded batches re-encode losslessly; (2)
//! `decode(encode(batch)) == batch`, including signature presence; (3) encoding
//! is deterministic — identical batches must produce identical bytes, so a
//! re-upload after a crash dedupes to the same Walrus blob id instead of
//! forking it; (4) blob ids roundtrip and malformed strings are rejected
//! without panicking.

#![no_main]

use libfuzzer_sys::fuzz_target;
use podseq_core::{BlobId, Block, Header};
use podseq_sui::{blob_id, wire};

fn fill<const N: usize>(seed: &[u8], offset: usize) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = seed[(offset + i) % seed.len()];
    }
    out
}

fn block_from(seed: &[u8], signed: bool) -> Block {
    Block {
        header: Header {
            height: u64::from_le_bytes(fill(seed, 0)),
            parent_hash: fill(seed, 8),
            state_root: fill(seed, 40),
            timestamp: u64::from_le_bytes(fill(seed, 72)),
        },
        data: fill::<24>(seed, 80).to_vec(),
        signature: if signed { Some(fill(seed, 104)) } else { None },
    }
}

fuzz_target!(|data: &[u8]| {
    // (1) Arbitrary bytes at the DA trust boundary: may error, never panic.
    if let Ok(blocks) = wire::decode(data) {
        assert!(!blocks.is_empty(), "decoded batches are non-empty");
        let bytes = wire::encode(&blocks).unwrap();
        assert_eq!(
            wire::decode(&bytes).unwrap(),
            blocks,
            "decoded batches re-encode losslessly"
        );
    }

    // (2)+(3) Roundtrip and determinism on structured batches.
    if data.len() >= 3 {
        let count = (data[0] % 5) as usize;
        let seed = &data[2..];
        let batch: Vec<Block> = (0..count)
            .map(|i| block_from(seed, data[1] >> i & 1 == 1))
            .collect();
        if !batch.is_empty() {
            let bytes = wire::encode(&batch).unwrap();
            assert_eq!(
                wire::decode(&bytes).unwrap(),
                batch,
                "roundtrip must preserve the batch, including signature presence"
            );
            assert_eq!(
                wire::encode(&batch).unwrap(),
                bytes,
                "encoding must be deterministic: same batch, same Walrus blob id"
            );
        }
    }

    // (4) Blob ids: roundtrip on valid inputs, no panic on arbitrary ones.
    if let Ok(id) = blob_id::decode(std::str::from_utf8(data).unwrap_or("")) {
        assert_eq!(blob_id::decode(&blob_id::encode(&id)).unwrap(), id);
    }
    if data.len() >= 32 {
        let id = BlobId(data[..32].try_into().unwrap());
        assert_eq!(blob_id::decode(&blob_id::encode(&id)).unwrap(), id);
    }
});
