//! Key management commands.

use std::path::PathBuf;

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

/// Prints the keys configured in the config file.
pub fn list(config: &crate::config::Config) {
    println!("Configured keys:");
    match &config.signer.key_path {
        Some(path) => println!("  signer: {}", path.display()),
        None => println!("  signer: (not configured)"),
    }
}
