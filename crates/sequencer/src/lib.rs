//! Block signing and forced inclusion for podseq.
//!
//! Block contents come from Reth: the execution client fills each block from
//! its own mempool (gas-price greedy, gas-limit capped). podseq's job is to
//! *produce* blocks on a timer, sign their headers so full nodes can attribute
//! them to the sequencer, and anchor them on DA + settlement.
//!
//! Forced inclusion removes the sequencer's censoring power. Users post a tx
//! to a Sui-side inbox; the sequencer must include it within N blocks or halt.
//! The sequencer pulls unread inbox entries, submits each to Reth's mempool
//! via `eth_sendRawTransaction`, and advances the inbox cursor once the tx is
//! mined. Forced txs enter the same mempool as user txs and ride the same
//! gas-limit cap, so this stays compatible with Reth-owned block contents.
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
    ///
    /// Accepts both formats the Sui CLI produces: Bech32 `suiprivkey1...` and
    /// raw base64 (33-byte flag + key payload).
    pub fn from_suiprivkey_file(path: &Path) -> Result<Self, Error> {
        let key_str = std::fs::read_to_string(path)
            .map_err(|e| Error::Execution(format!("reading block key: {e}")))?
            .trim()
            .to_string();
        let key = parse_ed25519_key(&key_str)
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

/// Parses an ed25519 private key from either format the Sui CLI produces:
/// Bech32 `suiprivkey1...` or raw base64 (33-byte flag + key payload).
fn parse_ed25519_key(s: &str) -> Result<sui_crypto::ed25519::Ed25519PrivateKey, String> {
    let trimmed = s.trim();
    if trimmed.starts_with("suiprivkey") {
        return sui_crypto::ed25519::Ed25519PrivateKey::from_suiprivkey(&trimmed.to_lowercase())
            .map_err(|e| e.to_string());
    }
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, trimmed)
        .map_err(|e| format!("not bech32 and not valid base64: {e}"))?;
    if raw.len() != 33 {
        return Err(format!(
            "base64 payload is {} bytes, expected 33",
            raw.len()
        ));
    }
    if raw[0] != 0x00 {
        return Err(format!("unsupported key scheme flag: {}", raw[0]));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw[1..]);
    Ok(sui_crypto::ed25519::Ed25519PrivateKey::new(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same key in both formats the Sui CLI emits: Bech32 (`suiprivkey1...`)
    // and raw base64 (33-byte flag + ed25519 scalar).
    const BECH32_KEY: &str =
        "suiprivkey1qquyqrneucq64ggzftlm4lsnkqd7jxjjf0wwzjn65jnue0c4n7kh6nj0zzk";
    const BASE64_KEY: &str = "ADhADnnmAaqhAkr/uv4TsBvpGlJL3OFKeqSnzL8Vn619";

    #[test]
    fn parse_ed25519_key_accepts_bech32_and_base64() {
        let from_bech32 = parse_ed25519_key(BECH32_KEY).expect("bech32 key should parse");
        let from_b64 = parse_ed25519_key(BASE64_KEY).expect("base64 key should parse");
        assert_eq!(
            from_bech32.public_key(),
            from_b64.public_key(),
            "both formats must derive the same public key",
        );
    }

    #[test]
    fn parse_ed25519_key_trims_whitespace() {
        let padded = format!("  {}  ", BASE64_KEY);
        let key = parse_ed25519_key(&padded).expect("whitespace should be trimmed");
        let baseline = parse_ed25519_key(BASE64_KEY).expect("baseline key should parse");
        assert_eq!(key.public_key(), baseline.public_key());
    }

    #[test]
    fn parse_ed25519_key_rejects_wrong_payload_length() {
        // 32 bytes instead of 33: valid base64 but missing the scheme flag byte.
        let too_short =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 32]);
        assert!(parse_ed25519_key(&too_short).is_err());
    }

    #[test]
    fn parse_ed25519_key_rejects_unsupported_scheme_flag() {
        // Flag byte 0x01 is secp256k1, not ed25519's 0x00.
        let mut raw = vec![0x01];
        raw.extend_from_slice(&[0u8; 32]);
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        assert!(parse_ed25519_key(&encoded).is_err());
    }

    #[test]
    fn parse_ed25519_key_rejects_garbage() {
        assert!(parse_ed25519_key("").is_err());
        assert!(parse_ed25519_key("not-a-key").is_err());
    }
}
