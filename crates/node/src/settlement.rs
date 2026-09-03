//! Settlement wiring: preflight checks and signer setup.
//!
//! The library (`podseq_sui::settlement`) owns deploy/commit/parse logic and
//! returns IDs; this module persists them to the config file and builds the
//! signer used by the sequencer. Symmetric with [`crate::bridge`] for the
//! bridge object IDs.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::Config;

/// Backoff between settlement deploy attempts.
const DEPLOY_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(15);

/// Startup preflight: fail fast with a clear message if settlement cannot be
/// configured, instead of a cryptic error deep in deploy/commit later.
///
/// - Always checks Sui RPC reachability.
/// - Existing IDs: verifies the Registry object is readable.
/// - First-start deploy: verifies the Move package is built (bytecode present).
pub async fn preflight(
    sui: &crate::config::SuiConfig,
    ids: (&Option<String>, &Option<String>, &Option<String>),
    signer_key_path: &Path,
) -> Result<()> {
    info!(rpc = %sui.rpc_url, "preflight: probing Sui RPC");
    if let Err(e) = podseq_sui::settlement::ping_rpc(&sui.rpc_url).await {
        anyhow::bail!(
            "Sui RPC unreachable at {}; is the node running and the URL correct? (error: {e})",
            sui.rpc_url
        );
    }

    match ids {
        (Some(_), Some(_), Some(registry_id)) => {
            // Validate the Registry object exists and is readable.
            if let Err(e) =
                podseq_sui::settlement::latest_height(&sui.rpc_url, registry_id).await
            {
                anyhow::bail!(
                    "cannot read settlement Registry {registry_id} on {}; verify sui.registry_id and that the contract is deployed (error: {e})",
                    sui.rpc_url
                );
            }
            Ok(())
        }
        (None, None, None) => {
            // First-start deploy: the Move package must be built first.
            let bytecode_dir = sui
                .move_dir
                .join("build/podseq_settlement/bytecode_modules");
            let built = bytecode_dir.is_dir()
                && std::fs::read_dir(&bytecode_dir)
                    .map(|mut it| {
                        it.any(|e| {
                            e.ok()
                                .is_some_and(|e| e.path().extension().is_some_and(|x| x == "mv"))
                        })
                    })
                    .unwrap_or(false);
            if !built {
                anyhow::bail!(
                    "settlement contract is not deployed and the Move package is not built: \
                     no .mv modules in {}. Run `sui move build` in {} first.",
                    bytecode_dir.display(),
                    sui.move_dir.display()
                );
            }
            if !signer_key_path.is_file() {
                anyhow::bail!(
                    "signer key not found at {}; settlement deploy needs a suiprivkey (fund its address with SUI for gas)",
                    signer_key_path.display()
                );
            }
            Ok(())
        }
        _ => anyhow::bail!(
            "settlement IDs are partially configured; either set all three (settlement_package_id, settler_cap_id, registry_id) or none"
        ),
    }
}

/// Resolves the settlement signer, either by attaching to existing object IDs
/// or by deploying the Move package on first start. Persists newly-deployed IDs
/// to the config file. Mutates `config` in place so the caller sees the new IDs.
pub async fn setup_signer(
    config: &mut Config,
    config_path: &Path,
    signer_key_path: &Path,
) -> Result<podseq_sui::SettlementSigner> {
    let ids = (
        &config.sui.settlement_package_id,
        &config.sui.settler_cap_id,
        &config.sui.registry_id,
    );

    match ids {
        (Some(pkg), Some(cap), Some(reg)) => {
            let settlement = podseq_sui::SettlementSigner::new(
                signer_key_path,
                pkg,
                cap,
                reg,
                &config.sui.rpc_url,
            )
            .context("building settlement signer")?;
            info!(key = %signer_key_path.display(), "settlement signer attached");
            Ok(settlement)
        }
        (None, None, None) => {
            info!(key = %signer_key_path.display(), "deploying settlement contract on first start");
            let bytecode_dir = config
                .sui
                .move_dir
                .join("build/podseq_settlement/bytecode_modules");
            let mut modules = Vec::new();
            for entry in std::fs::read_dir(&bytecode_dir).with_context(|| {
                format!(
                    "reading {} (run `sui move build` in {} first)",
                    bytecode_dir.display(),
                    config.sui.move_dir.display()
                )
            })? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "mv") {
                    modules.push(std::fs::read(&path)?);
                }
            }
            let deployed = deploy_with_retry(signer_key_path, &config.sui.rpc_url, modules).await?;

            config.sui.settlement_package_id = Some(deployed.package_id.clone());
            config.sui.settler_cap_id = Some(deployed.settler_cap_id.clone());
            config.sui.registry_id = Some(deployed.registry_id.clone());
            let updated = toml::to_string_pretty(&config).context("serializing updated config")?;
            std::fs::write(config_path, &updated)
                .with_context(|| format!("writing updated config to {}", config_path.display()))?;
            info!(config = %config_path.display(), "config updated with settlement IDs");

            podseq_sui::SettlementSigner::new(
                signer_key_path,
                &deployed.package_id,
                &deployed.settler_cap_id,
                &deployed.registry_id,
                &config.sui.rpc_url,
            )
            .context("building settlement signer after deploy")
        }
        _ => {
            anyhow::bail!("settlement IDs are partially configured; either set all three (settlement_package_id, settler_cap_id, registry_id) or none")
        }
    }
}

/// Deploys the settlement package, retrying until it succeeds.
///
/// A failed attempt may leave orphaned on-chain objects (a deadline expiry is
/// client-side and the tx may have executed), which is safe: each attempt
/// publishes its own package and the persisted IDs always come from the attempt
/// that completes fully. Without the retry, a transient Sui testnet spike kills
/// the node at first start.
async fn deploy_with_retry(
    signer_key_path: &Path,
    rpc_url: &str,
    modules: Vec<Vec<u8>>,
) -> Result<podseq_sui::DeployedContract> {
    loop {
        match podseq_sui::SettlementSigner::deploy(signer_key_path, rpc_url, modules.clone()).await
        {
            Ok(deployed) => {
                info!(registry_id = %deployed.registry_id, "settlement deploy complete");
                return Ok(deployed);
            }
            Err(e) => {
                warn!(
                    error = %format!("{e:#}"),
                    "settlement deploy failed; retrying in {DEPLOY_RETRY_BACKOFF:?}"
                );
                tokio::time::sleep(DEPLOY_RETRY_BACKOFF).await;
            }
        }
    }
}
