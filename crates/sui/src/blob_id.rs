//! Walrus blob ID encoding.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use podseq_core::BlobId;

use crate::Error;

pub(crate) fn decode(s: &str) -> Result<BlobId, Error> {
    let bytes = URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .map_err(|e| Error::InvalidBlobId(format!("base64url decode: {e}")))?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidBlobId(format!("expected 32 bytes, got {}", bytes.len())))?;
    Ok(BlobId(arr))
}

/// Encode a blob ID to its base64url string representation (no padding).
pub fn encode(id: &BlobId) -> String {
    URL_SAFE_NO_PAD.encode(id.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "M4hsZGQ1oCktdzegB6HnI6Mi28S2nqOPHxK-W7_4BUk";

    #[test]
    fn roundtrips_blob_id() {
        let id = decode(SAMPLE).unwrap();
        assert_eq!(encode(&id), SAMPLE);
    }

    #[test]
    fn rejects_short_input() {
        assert!(decode("AAAA").is_err());
    }

    #[test]
    fn rejects_non_base64() {
        assert!(decode("!!!not-base64!!!").is_err());
    }
}
