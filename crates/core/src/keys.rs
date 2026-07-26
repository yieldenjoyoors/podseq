//! Sui signer key parsing, shared across crates.

use sui_crypto::ed25519::Ed25519PrivateKey;

/// Parses a Sui ed25519 signer key from a string, accepting both formats the
/// Sui CLI produces:
/// - Bech32 `suiprivkey1...` (the canonical encoding `from_suiprivkey` expects).
/// - Raw base64: the contents of a `.key` file written by `sui keytool`, a
///   33-byte payload = 1-byte scheme flag (`0x00` for ed25519) + 32-byte scalar.
///
/// The string is lowercased before bech32 decoding so mixed-case inputs don't
/// fail.
pub fn parse_signer_key(s: &str) -> Result<Ed25519PrivateKey, String> {
    let trimmed = s.trim();
    if trimmed.starts_with("suiprivkey") {
        return Ed25519PrivateKey::from_suiprivkey(&trimmed.to_lowercase())
            .map_err(|e| e.to_string());
    }
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, trimmed)
        .map_err(|e| format!("not bech32 and not valid base64: {e}"))?;
    if raw.len() != 33 {
        return Err(format!(
            "base64 payload is {} bytes, expected 33 (flag + ed25519 key)",
            raw.len()
        ));
    }
    if raw[0] != 0x00 {
        return Err(format!(
            "unsupported key scheme flag: {} (expected 0 for ed25519)",
            raw[0]
        ));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw[1..]);
    Ok(Ed25519PrivateKey::new(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BECH32_KEY: &str =
        "suiprivkey1qquyqrneucq64ggzftlm4lsnkqd7jxjjf0wwzjn65jnue0c4n7kh6nj0zzk";
    const BASE64_KEY: &str = "ADhADnnmAaqhAkr/uv4TsBvpGlJL3OFKeqSnzL8Vn619";

    #[test]
    fn accepts_bech32_and_base64() {
        let from_bech32 = parse_signer_key(BECH32_KEY).expect("bech32 key should parse");
        let from_b64 = parse_signer_key(BASE64_KEY).expect("base64 key should parse");
        assert_eq!(
            from_bech32.public_key(),
            from_b64.public_key(),
            "both formats must derive the same public key",
        );
    }

    #[test]
    fn trims_whitespace() {
        let padded = format!("  {}  ", BASE64_KEY);
        let key = parse_signer_key(&padded).expect("whitespace should be trimmed");
        let baseline = parse_signer_key(BASE64_KEY).expect("baseline key should parse");
        assert_eq!(key.public_key(), baseline.public_key());
    }

    #[test]
    fn rejects_wrong_payload_length() {
        let too_short =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0u8; 32]);
        assert!(parse_signer_key(&too_short).is_err());
    }

    #[test]
    fn rejects_unsupported_scheme_flag() {
        let mut raw = vec![0x01];
        raw.extend_from_slice(&[0u8; 32]);
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw);
        assert!(parse_signer_key(&encoded).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_signer_key("not-a-key").is_err());
        assert!(parse_signer_key("").is_err());
    }
}
