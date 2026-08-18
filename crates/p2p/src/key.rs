//! P2P identity key loading and generation.
//!
//! Keys are stored as 64-char hex strings (32 raw bytes) in a text file.

use std::path::Path;

use anyhow::{Context, Result};
use commonware_codec::Encode;
use commonware_cryptography::ed25519::{self};
use commonware_cryptography::Signer as _;
use commonware_math::algebra::Random;
use tracing::info;

use crate::IdentityKey;

/// Loads the identity key from `path`, or generates a new one and saves it.
/// The file must contain exactly 64 hex characters (the 32-byte private key seed).
///
/// # Errors
///
/// Returns an error if an existing key file cannot be read or parsed, or if a
/// newly generated key cannot be written to `path`.
pub fn load_or_generate_key(path: &Path) -> Result<IdentityKey> {
    if path.exists() {
        let key = read_key(path)?;
        info!(key = %path.display(), "p2p identity key loaded");
        Ok(key)
    } else {
        let key = IdentityKey::random(rand::rng());
        let hex_str = hex::encode(key_seed(&key));
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent dir for p2p key at {}", path.display())
                })?;
            }
        }
        std::fs::write(path, &hex_str)
            .with_context(|| format!("writing p2p key to {}", path.display()))?;
        info!(key = %path.display(), "p2p identity key generated and saved");
        Ok(key)
    }
}

/// Reads the identity key at `path` and returns its public key as 64-char hex
/// (the peer identity used in `bootstrap_peers`). Unlike
/// [`load_or_generate_key`], this never creates a key file.
///
/// # Errors
///
/// Returns an error if the file cannot be read, is not 64 hex chars, or the
/// seed is not a valid ed25519 private key.
pub fn read_pubkey_hex(path: &Path) -> Result<String> {
    let key = read_key(path)?;
    Ok(hex::encode(key.public_key().encode()))
}

fn read_key(path: &Path) -> Result<IdentityKey> {
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
    load_from_seed(&seed)
}

fn load_from_seed(seed: &[u8]) -> Result<IdentityKey> {
    use commonware_codec::Read;
    let mut buf: &[u8] = seed;
    let key = ed25519::PrivateKey::read_cfg(&mut buf, &())
        .context("decoding p2p key seed (must be 32 raw bytes)")?;
    Ok(key)
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

    #[test]
    fn read_pubkey_hex_matches_generated_key() {
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

        let pk_hex = read_pubkey_hex(&key_path).unwrap();

        assert_eq!(pk_hex.len(), 64);
        assert_eq!(pk_hex, hex::encode(key.public_key().encode()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_pubkey_hex_does_not_generate_missing_key() {
        let dir = std::env::temp_dir().join(format!(
            "podseq-p2p-key-absent-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let key_path = dir.join("absent.key");

        assert!(read_pubkey_hex(&key_path).is_err());
        assert!(!key_path.exists());
    }
}
