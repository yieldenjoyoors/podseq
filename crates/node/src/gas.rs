//! Gas top-up watcher: keeps the settlement signer funded from a reserve key.
//!
//! Runs on the sequencer in place of the old balance-gauge-only task: every
//! cycle reads both balances (the settlement side through the rotation-aware
//! client, so a swapped key is watched from its first poll), writes the
//! gauges, and applies [`podseq_sui::gas::evaluate`]. The transfer is
//! submitted by the reserve key; without `sui.reserve_key_path` the watcher
//! still runs and only reports the gauges.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use podseq_sui::gas::{self, Action};
use podseq_sui::settlement;
use sui_crypto::ed25519::Ed25519PrivateKey;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::metrics::PodseqMetrics;

/// How often the watcher polls balances (and considers a top-up).
const POLL_INTERVAL_SECS: u64 = 900;

/// Spawns the watcher. Always runs: it writes the balance gauges even when
/// no reserve key is configured (the pre-top-up gauge task's behavior).
pub(crate) fn spawn(
    config: &Config,
    client: podseq_sui::Client,
    metrics: Arc<PodseqMetrics>,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    let reserve_key = config.sui.reserve_key_path.as_deref().map(load_reserve_key);
    let policy = gas::TopUpPolicy {
        min_balance_mist: config.sui.gas_min_balance_mist,
        topup_amount_mist: config.sui.gas_topup_amount_mist,
        reserve_floor_mist: config.sui.gas_reserve_floor_mist,
    };
    let rpc_url = config.sui.rpc_url.clone();

    tokio::spawn(async move {
        let reserve_key = match reserve_key {
            Some(Ok(key)) => Some(key),
            Some(Err(e)) => {
                error!(error = %e, "gas top-up disabled: the reserve key cannot be loaded");
                None
            }
            None => {
                info!("gas top-up disabled: sui.reserve_key_path not set");
                None
            }
        };

        loop {
            poll_once(&client, &rpc_url, reserve_key.as_ref(), &policy, &metrics).await;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)) => {},
                _ = shutdown.notified() => break,
            }
        }
    })
}

/// One watcher cycle: read both balances, write the gauges, act on the
/// policy. Any read failure fails closed — gauges keep their last value and
/// no coins move.
async fn poll_once(
    client: &podseq_sui::Client,
    rpc_url: &str,
    reserve_key: Option<&Ed25519PrivateKey>,
    policy: &gas::TopUpPolicy,
    metrics: &PodseqMetrics,
) {
    let settler = match client.sui_balance().await {
        Ok(Some(mist)) => {
            metrics.sui_gas_balance.set(mist as i64);
            Some(mist)
        }
        Ok(None) => {
            warn!("no settlement signer attached; skipping gas top-up poll");
            None
        }
        Err(e) => {
            warn!(error = %e, "failed to read the settlement key's SUI balance");
            None
        }
    };

    let reserve = match reserve_key {
        Some(key) => {
            let address = key.public_key().derive_address();
            match settlement::sui_balance(rpc_url, &address.to_hex()).await {
                Ok(mist) => {
                    metrics.reserve_sui_balance.set(mist as i64);
                    Some(mist)
                }
                Err(e) => {
                    warn!(error = %e, "failed to read the reserve's SUI balance");
                    None
                }
            }
        }
        None => None,
    };

    match gas::evaluate(settler, reserve, policy) {
        Action::Skip => {}
        Action::ReserveTooLow {
            reserve_mist,
            needed_mist,
        } => warn!(
            reserve_mist,
            needed_mist, "settlement key below the gas floor but the reserve cannot fund a top-up"
        ),
        Action::TopUp { amount_mist } => {
            let Some(key) = reserve_key else {
                return;
            };
            let Some(recipient) = client.settlement_sender() else {
                return;
            };
            match gas::submit_top_up(rpc_url, key, recipient, amount_mist).await {
                Ok(()) => {
                    metrics.gas_topups_total.inc();
                    info!(amount_mist, recipient = %recipient, "topped up the settlement key's gas");
                }
                Err(e) => warn!(error = %e, "gas top-up transaction failed"),
            }
        }
    }
}

/// Loads the reserve key from a suiprivkey file.
fn load_reserve_key(path: &Path) -> Result<Ed25519PrivateKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading gas reserve key {}", path.display()))?;
    podseq_core::parse_signer_key(raw.trim())
        .map_err(|e| anyhow::anyhow!("parsing gas reserve key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a throwaway suiprivkey file with a fixed scalar.
    fn temp_key_file(tag: &str, scalar: [u8; 32]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-node-gas-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reserve.key");
        std::fs::write(
            &path,
            Ed25519PrivateKey::new(scalar)
                .to_suiprivkey()
                .expect("static key encoding"),
        )
        .unwrap();
        path
    }

    #[test]
    fn loads_a_suiprivkey_reserve_key() {
        let path = temp_key_file("load", [7u8; 32]);
        let key = load_reserve_key(&path).expect("load");
        // The loaded key derives the same address as the written scalar.
        let direct = Ed25519PrivateKey::new([7u8; 32]);
        assert_eq!(
            key.public_key().derive_address(),
            direct.public_key().derive_address()
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_reserve_key_file_is_a_load_error() {
        let path = std::env::temp_dir().join(format!(
            "podseq-node-gas-missing-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(load_reserve_key(&path).is_err());
    }
}
