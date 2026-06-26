//! P2P identity key loading and generation.
//!
//! Keys are stored as 64-char hex strings (32 raw bytes) in a text file.

use std::path::Path;

use anyhow::{Context, Result};
use commonware_cryptography::ed25519::{self};
use commonware_math::algebra::Random;
use rand::rngs::OsRng;
use tracing::info;

use crate::IdentityKey;

/// Loads the identity key from `path`, or generates a new one and saves it.
/// The file must contain exactly 64 hex characters (the 32-byte private key seed).
pub fn load_or_generate_key(path: &Path) -> Result<IdentityKey> {
    if path.exists() {
        let hex = std::fs::read_to_string(path)
            .with_context(|| format!("reading p2p key from {}", path.display()))?
            .trim()
            .to_string();
        if hex.len() != 64 {
            anyhow::bail!(
                "p2p key file must contain exactly 64 hex chars, got {}",
                hex.len()
            );
        }
        let seed = hex::decode(&hex).with_context(|| "decoding p2p key hex")?;
        let key = load_from_seed(&seed);
        info!(key = %path.display(), "p2p identity key loaded");
        Ok(key)
    } else {
        let key = IdentityKey::random(&mut OsRng);
        let hex_str = hex::encode(key_seed(&key));
        std::fs::write(path, &hex_str)
            .with_context(|| format!("writing p2p key to {}", path.display()))?;
        info!(key = %path.display(), "p2p identity key generated and saved");
        Ok(key)
    }
}

fn load_from_seed(seed: &[u8]) -> IdentityKey {
    use commonware_codec::Read;
    let mut buf: &[u8] = seed;
    ed25519::PrivateKey::read_cfg(&mut buf, &()).expect("invalid seed")
}

fn key_seed(key: &IdentityKey) -> Vec<u8> {
    use commonware_codec::Encode;
    key.encode().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_load() {
        let dir = std::env::temp_dir().join(format!(
            "podseq-p2p-key-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("p2p.key");

        let key = load_or_generate_key(&key_path).unwrap();
        let hex_str = std::fs::read_to_string(&key_path).unwrap();
        assert_eq!(hex_str.len(), 64);

        let key2 = load_or_generate_key(&key_path).unwrap();
        assert_eq!(key_seed(&key), key_seed(&key2));

        std::fs::remove_dir_all(&dir).ok();
    }
}
