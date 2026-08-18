//! Key management commands.

use std::path::{Path, PathBuf};

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

/// Derives the Sui address (0x + 64 hex) from the signer key file.
fn sui_address(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading signer key {}", path.display()))?;
    let key = podseq_core::parse_signer_key(raw.trim())
        .map_err(|e| anyhow::anyhow!("parsing signer key: {e}"))?;
    Ok(key.public_key().derive_address().to_string())
}

/// Derives the L2 EVM address (0x + 40 hex) from the relayer key file.
fn evm_address(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading EVM key {}", path.display()))?;
    let bytes = hex::decode(raw.trim()).context("decoding EVM key hex")?;
    let signer = PrivateKeySigner::from_slice(&bytes).context("invalid EVM key")?;
    Ok(signer.address().to_string())
}

/// Prints the keys configured in the config file with their derived addresses.
/// The p2p identity has no address; its ed25519 pubkey (peer id) is shown.
pub fn list(config: &crate::config::Config) {
    println!("Configured keys:");
    match &config.signer.key_path {
        Some(path) => {
            println!("  signer (Sui, ed25519): {}", path.display());
            print_derived("address", sui_address(path));
        }
        None => println!("  signer (Sui, ed25519): (not configured)"),
    }
    match &config.p2p.key_path {
        Some(path) => {
            println!("  p2p:                   {path}");
            print_derived("pubkey", podseq_p2p::read_pubkey_hex(Path::new(path)));
        }
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
            print_derived("address", evm_address(path));
        }
        None if config.bridge.enabled => {
            println!("  bridge relayer (EVM):  MISSING (bridge.enabled but no key)");
        }
        None => {}
    }
}

/// Prints a derived key attribute, or an error note when the key file cannot
/// be read or parsed.
fn print_derived(label: &str, derived: Result<String>) {
    match derived {
        Ok(value) => println!("    {label}: {value}"),
        Err(e) => println!("    {label}: unavailable ({e:#})"),
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

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "podseq-keyring-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn sui_address_derives_from_generated_key() {
        let dir = temp_dir("sui");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sequencer.key");
        generate_signer(&path).unwrap();

        let addr = sui_address(&path).unwrap();

        assert!(addr.starts_with("0x"));
        assert_eq!(addr.len(), 66, "Sui addresses are 0x + 64 hex chars");
        assert_eq!(
            addr,
            sui_address(&path).unwrap(),
            "derivation is deterministic"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn evm_address_derives_from_generated_key() {
        let dir = temp_dir("evm");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("relayer.key");
        generate_evm_key(&path).unwrap();

        let addr = evm_address(&path).unwrap();

        assert!(addr.starts_with("0x"));
        assert_eq!(addr.len(), 42, "EVM addresses are 0x + 40 hex chars");
        assert_eq!(
            addr,
            evm_address(&path).unwrap(),
            "derivation is deterministic"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn address_derivation_errors_on_missing_file() {
        let missing = std::env::temp_dir().join("podseq-keyring-does-not-exist.key");
        assert!(sui_address(&missing).is_err());
        assert!(evm_address(&missing).is_err());
    }

    #[test]
    fn list_handles_all_key_configurations() {
        let dir = temp_dir("list");
        std::fs::create_dir_all(&dir).unwrap();
        let signer = dir.join("sequencer.key");
        generate_signer(&signer).unwrap();
        let relayer = dir.join("relayer.key");
        generate_evm_key(&relayer).unwrap();
        let p2p = dir.join("p2p.key");
        std::fs::write(&p2p, "11".repeat(32)).unwrap();
        let garbage = dir.join("garbage.key");
        std::fs::write(&garbage, "not-a-key").unwrap();

        // All keys present and valid, bridge enabled.
        let mut config = crate::config::Config::testnet();
        config.signer.key_path = Some(signer);
        config.p2p.key_path = Some(p2p.to_string_lossy().into_owned());
        config.bridge.l2_relayer_key_path = Some(relayer);
        config.bridge.enabled = true;
        list(&config);

        // Nothing configured, bridge disabled.
        let mut config = crate::config::Config::testnet();
        config.signer.key_path = None;
        config.p2p.key_path = None;
        config.bridge.l2_relayer_key_path = None;
        config.bridge.enabled = false;
        list(&config);

        // Bridge enabled but relayer key missing.
        let mut config = crate::config::Config::testnet();
        config.signer.key_path = None;
        config.p2p.key_path = None;
        config.bridge.l2_relayer_key_path = None;
        config.bridge.enabled = true;
        list(&config);

        // Keys configured but unreadable.
        let mut config = crate::config::Config::testnet();
        config.signer.key_path = Some(garbage.clone());
        config.p2p.key_path = Some(garbage.to_string_lossy().into_owned());
        config.bridge.l2_relayer_key_path = Some(garbage);
        config.bridge.enabled = false;
        list(&config);

        std::fs::remove_dir_all(&dir).ok();
    }
}
