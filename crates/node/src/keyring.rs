//! Key management commands.

use std::path::PathBuf;

use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, Result};
use sui_crypto::ed25519::Ed25519PrivateKey;

fn generate_key(out: &PathBuf, label: &str) -> Result<()> {
    let key = Ed25519PrivateKey::generate(rand::rngs::OsRng);
    let suiprivkey = key
        .to_suiprivkey()
        .map_err(|e| anyhow::anyhow!("encoding key: {e}"))?;
    let address = key.public_key().derive_address();

    std::fs::write(out, &suiprivkey)
        .with_context(|| format!("writing {label} key to {}", out.display()))?;

    println!("{label} key written to: {}", out.display());
    println!("Address: {address}");
    Ok(())
}

/// Generates a new signer key and writes it to a file.
pub fn generate_signer(out: &PathBuf) -> Result<()> {
    generate_key(out, "Signer")?;
    println!("\nFund this address with SUI (for gas) on the target network.");
    Ok(())
}

/// Generates a secp256k1 EVM private key for the bridge relayer and writes it as
/// 32-byte lowercase hex (the format `bridge.l2_relayer_key_path` expects). Its
/// derived address must hold the L2 `Bridge` `relayer` role and be funded with
/// L2 gas.
pub fn generate_evm_key(out: &PathBuf) -> Result<()> {
    let signer = PrivateKeySigner::random();
    let bytes = signer.to_bytes();
    let address = signer.address();

    std::fs::write(out, hex::encode(bytes.0))
        .with_context(|| format!("writing EVM key to {}", out.display()))?;

    println!("EVM relayer key written to: {}", out.display());
    println!("L2 address: {address}");
    println!("\nSet this address as the `relayer` on Bridge.sol at predeploy,");
    println!("and fund it with L2 gas.");
    Ok(())
}

/// Prints the keys configured in the config file.
pub fn list(config: &crate::config::Config) {
    println!("Configured keys:");
    match &config.signer.key_path {
        Some(path) => println!("  signer (Sui, ed25519): {}", path.display()),
        None => println!("  signer (Sui, ed25519): (not configured)"),
    }
    match &config.p2p.key_path {
        Some(path) => println!("  p2p:                   {}", path),
        None => println!("  p2p:                   (not configured)"),
    }
    match &config.bridge.l2_relayer_key_path {
        Some(path) => {
            let note = if config.bridge.enabled {
                ""
            } else {
                " (bridge disabled)"
            };
            println!("  bridge relayer (EVM):  {}{}", path.display(), note);
        }
        None if config.bridge.enabled => {
            println!("  bridge relayer (EVM):  MISSING (bridge.enabled but no key)");
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_evm_key_writes_32_byte_hex() {
        let dir = std::env::temp_dir().join(format!(
            "podseq-keyring-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("relayer.key");

        generate_evm_key(&path).unwrap();

        let hex_str = std::fs::read_to_string(&path).unwrap();
        let bytes = hex::decode(hex_str.trim()).unwrap();
        assert_eq!(bytes.len(), 32, "secp256k1 scalar must be 32 bytes");
        // Round-trips through PrivateKeySigner (validates the scalar).
        PrivateKeySigner::from_slice(&bytes).unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }
}
