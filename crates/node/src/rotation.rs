//! Cap-rotation watcher: chain-derived roles and the wallet hot-swap.
//!
//! Spawns one task that polls both `RotationState` records and the Sui
//! `Clock` over the mockable inbox transport, folds them into a plan
//! ([`podseq_sui::rotation::plan`]), and executes at most one step per poll
//! via [`SwapSinks`]:
//!
//! - claim: one PTB for both objects, signed by the staged key;
//! - adopt: in-place swap of the settlement and bridge signers to the staged
//!   key plus the shared address snapshot — block production and the relayer
//!   take the same locks on their next call and continue on the new key;
//! - retire: one PTB for both objects, one quiet interval after the adopt.
//!
//! There is no local rotation phase: the plan is derived from chain state on
//! every poll, so a restart mid-window resumes in the correct role (a claim
//! that landed before the swap replays as an adopt; a stale key in charge is
//! a loud misconfiguration and the on-chain gate keeps it from settling).
//! The relayer's EVM key is not a Sui capability holder and is out of scope.

use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, RwLock,
};
use std::time::Duration;

use anyhow::{Context, Result};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_sdk_types::Address;
use tokio::sync::{Mutex, OnceCell};
use tracing::{error, info, warn};

use podseq_sui::rotation::{
    self, Action, AdoptionGuard, Plan, RotationExecutor, RotationTargets, RotationWatcher,
};
use podseq_sui::SettlementSigner;

use crate::config::Config;

/// Poll cadence. One commit wait, so the adopt step lands well inside the
/// on-chain `PREV_GRACE_MS` backstop.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Quiet interval between the signer swap and the retire step: one poll.
/// Approximates the in-flight drain check; the on-chain grace window is the
/// real backstop.
const RETIRE_QUIET: Duration = POLL_INTERVAL;

/// Where the watcher swaps keys: the shared settlement signer, the shared
/// address snapshot, and the relayer's Sui bridge client (populated by the
/// bridge task once its setup completes).
pub struct SwapSinks {
    settlement: Arc<Mutex<SettlementSigner>>,
    sender_snapshot: Arc<RwLock<Address>>,
    sui_bridge: Arc<OnceCell<Arc<Mutex<podseq_sui::bridge::BridgeClient>>>>,
    rpc_url: String,
    targets: RotationTargets,
}

impl SwapSinks {
    /// Swaps every Sui signer to `key` (settlement now, bridge as soon as it
    /// exists) and updates the address snapshot.
    async fn swap_signers(&self, key: &Ed25519PrivateKey) {
        let address = key.public_key().derive_address();
        self.settlement.lock().await.swap_key(key.clone());
        *self.sender_snapshot.write().expect("address snapshot") = address;
        if let Some(bridge) = self.sui_bridge.get() {
            bridge.lock().await.swap_key(key.clone());
        }
        info!(%address, "rotation: signers adopted the staged key");
    }
}

impl RotationExecutor for SwapSinks {
    fn claim<'a>(
        &'a self,
        staged_key: &'a Ed25519PrivateKey,
    ) -> podseq_sui::inbox::InboxFuture<'a, Result<(), podseq_sui::SettlementError>> {
        Box::pin(
            async move { rotation::submit_claim(&self.rpc_url, staged_key, &self.targets).await },
        )
    }

    fn adopt<'a>(
        &'a self,
        staged_key: &'a Ed25519PrivateKey,
    ) -> podseq_sui::inbox::InboxFuture<'a, Result<(), podseq_sui::SettlementError>> {
        Box::pin(async move {
            self.swap_signers(staged_key).await;
            Ok(())
        })
    }

    fn retire_previous<'a>(
        &'a self,
        staged_key: &'a Ed25519PrivateKey,
    ) -> podseq_sui::inbox::InboxFuture<'a, Result<(), podseq_sui::SettlementError>> {
        Box::pin(async move {
            rotation::submit_retire_previous(&self.rpc_url, staged_key, &self.targets).await
        })
    }
}

/// Spawns the rotation watcher. `None` when the prerequisites are absent:
/// no deployed contract IDs (fresh node, nothing entombed yet), no staged
/// key (observe-only mode is the operator's explicit choice), or no staged
/// key file.
///
/// Never fatal: a rotation outage must not take block production down.
pub async fn spawn(
    config: &Config,
    settlement: Arc<Mutex<SettlementSigner>>,
    sender_snapshot: Arc<RwLock<Address>>,
    sui_bridge: Arc<OnceCell<Arc<Mutex<podseq_sui::bridge::BridgeClient>>>>,
    shutdown: Arc<AtomicBool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let (package_id, registry_id, vault_id) = match (
        &config.sui.settlement_package_id,
        &config.sui.registry_id,
        &config.bridge.vault_id,
    ) {
        (Some(p), Some(r), Some(v)) => (p.clone(), r.clone(), v.clone()),
        _ => {
            info!("rotation watcher disabled: settlement/registry/vault not fully configured");
            return None;
        }
    };
    let Some(staged_path) = config.sui.rotation_key_path.as_ref() else {
        info!("rotation watcher disabled: sui.rotation_key_path not set (no staged key to claim)");
        return None;
    };
    let staged_key = match load_staged_key(staged_path) {
        Ok(key) => key,
        Err(e) => {
            warn!(
                error = %format!("{e:#}"),
                "rotation watcher disabled: staged key unreadable"
            );
            return None;
        }
    };

    let (package, registry, vault, active, staged) = match parse_targets(
        &package_id,
        &registry_id,
        &vault_id,
        &settlement,
        &staged_key,
    )
    .await
    {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(
                error = %format!("{e:#}"),
                "rotation watcher disabled: invalid object ids or keys"
            );
            return None;
        }
    };

    let transport = podseq_sui::inbox::SuiInboxTransport::new(config.sui.rpc_url.clone());
    let watcher = RotationWatcher::new(transport, package, registry, vault, active, Some(staged));
    let executor = Arc::new(SwapSinks {
        settlement,
        sender_snapshot,
        sui_bridge,
        rpc_url: config.sui.rpc_url.clone(),
        targets: RotationTargets {
            package,
            registry,
            vault,
        },
    });

    info!(
        active = %active,
        staged = %staged,
        registry = %registry,
        vault = %vault,
        "rotation watcher started (chain-derived roles)"
    );
    Some(tokio::spawn(async move {
        let guard = AdoptionGuard::new(RETIRE_QUIET);
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        loop {
            if shutdown.load(Ordering::SeqCst) {
                info!("rotation watcher stopping");
                return;
            }
            ticker.tick().await;
            match watcher.plan().await {
                Ok(plan) => {
                    if let Plan::Misconfigured = plan {
                        error!(
                            active = %active,
                            staged = %staged,
                            "rotation: chain authority matches neither key; \
                             refusing to settle — check sui.rotation_key_path and the \
                             entombment ceremony (fail closed until resolved)"
                        );
                    }
                    let action =
                        rotation::apply(&plan, Some(&staged_key), executor.as_ref(), &guard).await;
                    match action {
                        Ok(Action::Idle) | Ok(Action::Refused) => {}
                        Ok(other) => info!(?plan, ?other, "rotation: step executed"),
                        Err(e) => warn!(error = %e, "rotation: step failed; will retry"),
                    }
                }
                Err(e) => {
                    // Not entombed yet, or a read error: retry next poll.
                    // After the migration PTB (upgrade + entombment) this
                    // clears on its own.
                    warn!(error = %format!("{e}"), "rotation: status read failed")
                }
            }
        }
    }))
}

async fn parse_targets(
    package_id: &str,
    registry_id: &str,
    vault_id: &str,
    settlement: &Mutex<SettlementSigner>,
    staged_key: &Ed25519PrivateKey,
) -> Result<(Address, Address, Address, Address, Address)> {
    let package = podseq_sui::settlement::parse_address(package_id, "package id")?;
    let registry = podseq_sui::settlement::parse_address(registry_id, "registry id")?;
    let vault = podseq_sui::settlement::parse_address(vault_id, "vault id")?;
    let active = settlement.lock().await.sender();
    let staged = staged_key.public_key().derive_address();
    Ok((package, registry, vault, active, staged))
}

fn load_staged_key(path: &Path) -> Result<Ed25519PrivateKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading staged rotation key {}", path.display()))?;
    podseq_core::parse_signer_key(raw.trim())
        .map_err(|e| anyhow::anyhow!("parsing staged rotation key: {e}"))
}
