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

use podseq_sui::inbox::{InboxError, InboxFuture, InboxTransport};
use podseq_sui::rotation::{
    self, Action, AdoptedKey, AdoptionGuard, Plan, RotationExecutor, RotationTargets,
    RotationWatcher,
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

// ---------- entombment migration (pre-rotation deployments) ----------

/// Cap object ids the migration needs, resolved from config.
#[derive(Debug, Clone, Copy, Default)]
struct EntombCaps {
    settler: Option<Address>,
    bridge: Option<Address>,
}

/// What the migration did for one guarded object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntombStep {
    /// The rotation state exists; nothing to do.
    Present,
    /// The entombment PTB was submitted.
    Entombed,
    /// The state is missing but no cap id is configured: the manual ceremony
    /// stays mandatory (fail closed, loudly).
    Unconfigured,
}

/// What one migration poll did, per guarded object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MigrationReport {
    settler: EntombStep,
    bridge: EntombStep,
}

impl MigrationReport {
    /// At least one entombment PTB was submitted.
    fn healed(&self) -> bool {
        matches!(self.settler, EntombStep::Entombed) || matches!(self.bridge, EntombStep::Entombed)
    }

    /// A missing state has no configured cap id and cannot auto-heal.
    fn blocked(&self) -> bool {
        matches!(self.settler, EntombStep::Unconfigured)
            || matches!(self.bridge, EntombStep::Unconfigured)
    }
}

/// Submits the entombment ceremony PTBs. Mocked in tests; the production
/// executor wraps the shared settlement signer (the caps' owner).
trait EntombExecutor: Send + Sync + 'static {
    fn entomb_settler<'a>(
        &'a self,
        cap: Address,
    ) -> InboxFuture<'a, Result<(), podseq_sui::SettlementError>>;

    fn entomb_bridge<'a>(
        &'a self,
        cap: Address,
    ) -> InboxFuture<'a, Result<(), podseq_sui::SettlementError>>;
}

/// Entombs every guarded object whose rotation state is missing: the
/// migration for a deployment upgraded from a pre-rotation package (fresh
/// deploys entomb during `deploy`/`initialize_with_key`).
///
/// Per object, a missing state with a configured cap id is entombed now; one
/// without a cap id reports [`EntombStep::Unconfigured`]. Transport errors
/// propagate — an unreadable chain state never entombs, because the ceremony
/// moves an owned object.
async fn ensure_entombed<T: InboxTransport>(
    watcher: &RotationWatcher<T>,
    targets: &RotationTargets,
    caps: &EntombCaps,
    executor: &dyn EntombExecutor,
) -> Result<MigrationReport, InboxError> {
    let settler = if watcher.entombment_needed(targets.registry).await? {
        match caps.settler {
            Some(cap) => {
                executor
                    .entomb_settler(cap)
                    .await
                    .map_err(|e| InboxError::Rpc(format!("entomb settler cap ptb: {e}")))?;
                EntombStep::Entombed
            }
            None => EntombStep::Unconfigured,
        }
    } else {
        EntombStep::Present
    };
    let bridge = if watcher.entombment_needed(targets.vault).await? {
        match caps.bridge {
            Some(cap) => {
                executor
                    .entomb_bridge(cap)
                    .await
                    .map_err(|e| InboxError::Rpc(format!("entomb bridge cap ptb: {e}")))?;
                EntombStep::Entombed
            }
            None => EntombStep::Unconfigured,
        }
    } else {
        EntombStep::Present
    };
    Ok(MigrationReport { settler, bridge })
}

/// Logs one migration attempt.
///
/// A blocked report repeats every poll by design, until the operator
/// configures the cap id or runs the ceremony; a plain read failure (nothing
/// to migrate) keeps the old warn.
fn log_migration(outcome: Result<MigrationReport, InboxError>, read_error: Option<&InboxError>) {
    let report = match outcome {
        Ok(report) => report,
        Err(e) => {
            warn!(error = %format!("{e}"), "rotation: migration read failed; will retry");
            return;
        }
    };
    if report.blocked() {
        error!(
            ?report,
            "rotation: rotation state missing and the cap id is not configured; \
             settling fails closed — set sui.settler_cap_id / bridge.cap_id \
             (the signer key must own the caps) or run the entombment ceremony \
             (docs/src/cap-rotation.md)"
        );
    } else if report.healed() {
        info!(?report, "rotation: entombment migration applied");
    } else if let Some(e) = read_error {
        warn!(error = %format!("{e}"), "rotation: status read failed");
    }
}

/// Parses a configured object id, treating an unparseable value as unset
/// (with a loud note) instead of disabling the watcher.
fn parse_optional_id(raw: Option<&str>, field: &str) -> Option<Address> {
    let id = raw?;
    match podseq_sui::settlement::parse_address(id, field) {
        Ok(addr) => Some(addr),
        Err(e) => {
            warn!(
                error = %e,
                field,
                "rotation migration: unparseable cap id; treating as unset"
            );
            None
        }
    }
}

/// Where the watcher swaps keys: the shared settlement signer, the shared
/// address snapshot, and the relayer's Sui bridge client (populated by the
/// bridge task once its setup completes).
pub struct SwapSinks {
    settlement: Arc<Mutex<SettlementSigner>>,
    sender_snapshot: Arc<RwLock<Address>>,
    sui_bridge: Arc<OnceCell<Arc<Mutex<podseq_sui::bridge::BridgeClient>>>>,
    /// Records each adopt so a relayer that starts after the swap
    /// initializes on the rotated key instead of the stale key file.
    adopted_key: AdoptedKey,
    rpc_url: String,
    targets: RotationTargets,
}

impl SwapSinks {
    /// Swaps every Sui signer to `key` (settlement now, bridge as soon as it
    /// exists) and updates the address snapshot.
    ///
    /// The adopt is recorded in `adopted_key` unconditionally and first: the
    /// relayer may not exist yet (its setup retries while the L2 predeploy
    /// is missing), and a late `BridgeClient` built from the key file would
    /// resurrect the previous key after the swap.
    async fn swap_signers(&self, key: &Ed25519PrivateKey) {
        let address = key.public_key().derive_address();
        self.adopted_key.set(key.clone());
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

impl EntombExecutor for SwapSinks {
    fn entomb_settler<'a>(
        &'a self,
        cap: Address,
    ) -> InboxFuture<'a, Result<(), podseq_sui::SettlementError>> {
        Box::pin(async move {
            // The current settlement key signs: it owns the cap, and a
            // rotation adopt may have swapped it in — never cache a key here.
            let signer = self.settlement.lock().await;
            rotation::submit_entomb_settler(
                &self.rpc_url,
                signer.key(),
                self.targets.package,
                self.targets.registry,
                cap,
            )
            .await
        })
    }

    fn entomb_bridge<'a>(
        &'a self,
        cap: Address,
    ) -> InboxFuture<'a, Result<(), podseq_sui::SettlementError>> {
        Box::pin(async move {
            let signer = self.settlement.lock().await;
            rotation::submit_entomb_bridge(
                &self.rpc_url,
                signer.key(),
                self.targets.package,
                self.targets.vault,
                cap,
            )
            .await
        })
    }
}

/// The three deployed object ids the watcher needs.
fn resolve_contract_ids(config: &Config) -> Option<(String, String, String)> {
    match (
        &config.sui.settlement_package_id,
        &config.sui.registry_id,
        &config.bridge.vault_id,
    ) {
        (Some(p), Some(r), Some(v)) => Some((p.clone(), r.clone(), v.clone())),
        _ => {
            info!("rotation watcher disabled: settlement/registry/vault not fully configured");
            None
        }
    }
}

/// Loads the staged rotation key, if configured.
///
/// `Ok(None)` is migration-only mode (no key configured — an explicit
/// operator choice); an unreadable key file is an error, because the path
/// was configured and must work.
fn resolve_staged_key(config: &Config) -> Result<Option<Ed25519PrivateKey>> {
    match config.sui.rotation_key_path.as_deref().map(load_staged_key) {
        None => {
            info!(
                "rotation watcher in migration-only mode: sui.rotation_key_path not set \
                 (no staged key to claim)"
            );
            Ok(None)
        }
        Some(Ok(key)) => Ok(Some(key)),
        Some(Err(e)) => Err(e),
    }
}

/// Spawns the rotation watcher. `None` when the prerequisites are absent:
/// no deployed contract IDs (fresh node, nothing to watch yet), or an
/// unreadable staged key file.
///
/// Without a staged key (`sui.rotation_key_path` unset) the watcher runs in
/// migration-only mode: it keeps the rotation state seeded for deployments
/// upgraded from a pre-rotation package, but cannot claim or adopt.
///
/// Never fatal: a rotation outage must not take block production down.
pub async fn spawn(
    config: &Config,
    settlement: Arc<Mutex<SettlementSigner>>,
    sender_snapshot: Arc<RwLock<Address>>,
    sui_bridge: Arc<OnceCell<Arc<Mutex<podseq_sui::bridge::BridgeClient>>>>,
    adopted_key: AdoptedKey,
    shutdown: Arc<AtomicBool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let (package_id, registry_id, vault_id) = resolve_contract_ids(config)?;
    let staged_key = match resolve_staged_key(config) {
        Ok(key) => key,
        Err(e) => {
            warn!(
                error = %format!("{e:#}"),
                "rotation watcher disabled: staged key unreadable"
            );
            return None;
        }
    };

    let (package, registry, vault, active) =
        match parse_targets(&package_id, &registry_id, &vault_id, &settlement).await {
            Ok(parsed) => parsed,
            Err(e) => {
                warn!(
                    error = %format!("{e:#}"),
                    "rotation watcher disabled: invalid object ids or keys"
                );
                return None;
            }
        };
    let staged = staged_key
        .as_ref()
        .map(|key| key.public_key().derive_address());

    let caps = EntombCaps {
        settler: parse_optional_id(config.sui.settler_cap_id.as_deref(), "sui.settler_cap_id"),
        bridge: parse_optional_id(config.bridge.cap_id.as_deref(), "bridge.cap_id"),
    };

    let transport = podseq_sui::inbox::SuiInboxTransport::new(config.sui.rpc_url.clone());
    let watcher = RotationWatcher::new(transport, package, registry, vault, active, staged);
    let targets = RotationTargets {
        package,
        registry,
        vault,
    };
    let executor = Arc::new(SwapSinks {
        settlement,
        sender_snapshot,
        sui_bridge,
        adopted_key,
        rpc_url: config.sui.rpc_url.clone(),
        targets: targets.clone(),
    });

    info!(
        active = %active,
        staged = ?staged,
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
            // Migration-only mode: no staged key to claim or adopt, so the
            // watcher only keeps the rotation state seeded.
            let Some(staged_key) = staged_key.as_ref() else {
                let outcome = ensure_entombed(&watcher, &targets, &caps, executor.as_ref()).await;
                log_migration(outcome, None);
                continue;
            };
            match watcher.plan().await {
                Ok(plan) => {
                    if let Plan::Misconfigured = plan {
                        error!(
                            active = %active,
                            staged = ?staged,
                            "rotation: chain authority matches neither key; \
                             refusing to settle — check sui.rotation_key_path and the \
                             entombment ceremony (fail closed until resolved)"
                        );
                    }
                    let action =
                        rotation::apply(&plan, Some(staged_key), executor.as_ref(), &guard).await;
                    match action {
                        Ok(Action::Idle) | Ok(Action::Refused) => {}
                        Ok(other) => info!(?plan, ?other, "rotation: step executed"),
                        Err(e) => warn!(error = %e, "rotation: step failed; will retry"),
                    }
                }
                Err(e) => {
                    // The usual cause is a missing rotation state (a pre-
                    // rotation deployment): try the migration. Any other read
                    // failure keeps the plain warn.
                    let outcome =
                        ensure_entombed(&watcher, &targets, &caps, executor.as_ref()).await;
                    log_migration(outcome, Some(&e));
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
) -> Result<(Address, Address, Address, Address)> {
    let package = podseq_sui::settlement::parse_address(package_id, "package id")?;
    let registry = podseq_sui::settlement::parse_address(registry_id, "registry id")?;
    let vault = podseq_sui::settlement::parse_address(vault_id, "vault id")?;
    let active = settlement.lock().await.sender();
    Ok((package, registry, vault, active))
}

fn load_staged_key(path: &Path) -> Result<Ed25519PrivateKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading staged rotation key {}", path.display()))?;
    podseq_core::parse_signer_key(raw.trim())
        .map_err(|e| anyhow::anyhow!("parsing staged rotation key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a throwaway suiprivkey file with a fixed scalar.
    fn temp_key_file(tag: &str, scalar: [u8; 32]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-node-rotation-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sequencer.key");
        std::fs::write(
            &path,
            Ed25519PrivateKey::new(scalar)
                .to_suiprivkey()
                .expect("static key encoding"),
        )
        .unwrap();
        path
    }

    fn swap_sinks(tag: &str, adopted_key: AdoptedKey) -> SwapSinks {
        let settlement = SettlementSigner::new(
            &temp_key_file(tag, [0x5E; 32]),
            "0x2",
            "0x3",
            "0x4",
            "0x5",
            "http://localhost:9000",
        )
        .unwrap();
        SwapSinks {
            settlement: Arc::new(Mutex::new(settlement)),
            sender_snapshot: Arc::new(RwLock::new(Address::new([0xAA; 32]))),
            sui_bridge: Arc::new(OnceCell::new()),
            adopted_key,
            rpc_url: "http://localhost:9000".to_string(),
            targets: RotationTargets {
                package: Address::new([0x01; 32]),
                registry: Address::new([0x02; 32]),
                vault: Address::new([0x03; 32]),
            },
        }
    }

    #[tokio::test]
    async fn an_adopt_before_the_relayer_is_up_records_the_key_for_late_setup() {
        // Regression pin: the relayer's setup can complete AFTER the
        // rotation adopt (it retries while the L2 predeploy is missing). The
        // swap must record the adopted key so the late BridgeClient
        // constructor prefers it over the stale key file.
        let holder = AdoptedKey::new();
        let sinks = swap_sinks("late-relay", holder.clone());
        let staged = Ed25519PrivateKey::new([0x77; 32]);
        let staged_addr = staged.public_key().derive_address();

        assert!(sinks.sui_bridge.get().is_none());
        sinks.swap_signers(&staged).await;

        // The late relayer constructor resolves the adopted key.
        assert_eq!(
            holder.get().map(|k| k.public_key().derive_address()),
            Some(staged_addr)
        );
        // The running settlement signer and the shared snapshot swapped too.
        assert_eq!(sinks.settlement.lock().await.sender(), staged_addr);
        assert_eq!(*sinks.sender_snapshot.read().unwrap(), staged_addr);
    }

    #[tokio::test]
    async fn a_second_adopt_updates_the_holder_and_every_sink() {
        let holder = AdoptedKey::new();
        let sinks = swap_sinks("re-adopt", holder.clone());

        let first = Ed25519PrivateKey::new([0x66; 32]);
        sinks.swap_signers(&first).await;
        let second = Ed25519PrivateKey::new([0x77; 32]);
        let second_addr = second.public_key().derive_address();
        sinks.swap_signers(&second).await;

        assert_eq!(
            holder.get().map(|k| k.public_key().derive_address()),
            Some(second_addr)
        );
        assert_eq!(*sinks.sender_snapshot.read().unwrap(), second_addr);
        assert_eq!(sinks.settlement.lock().await.sender(), second_addr);
    }

    // ---------- entombment migration (pre-rotation deployments) ----------

    use podseq_sui::SettlementError;

    const AUTHORITY: Address = Address::new([0xAA; 32]);
    const PACKAGE: Address = Address::new([0x21; 32]);
    const REGISTRY: Address = Address::new([0x22; 32]);
    const VAULT: Address = Address::new([0x23; 32]);
    const SETTLER_CAP: Address = Address::new([0x31; 32]);
    const BRIDGE_CAP: Address = Address::new([0x32; 32]);

    /// In-memory transport with optional injected read failures.
    #[derive(Default)]
    struct MockChain {
        objects: std::collections::BTreeMap<Address, Vec<u8>>,
        fail_reads: Vec<Address>,
    }

    impl InboxTransport for MockChain {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            Box::pin(async move {
                if self.fail_reads.contains(id) {
                    return Err(InboxError::Rpc("injected transport failure".into()));
                }
                self.objects
                    .get(id)
                    .cloned()
                    .ok_or_else(|| InboxError::NotFound(format!("no object {id}")))
            })
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    /// BCS of `Field<rotation::StateKey, rotation::RotationState>` with the
    /// given authority — the wire shape `object_contents` returns for an
    /// entombed parent. `None` options encode as one zero byte regardless of
    /// the inner type.
    fn entombed_state_bcs(authority: Address) -> Vec<u8> {
        #[derive(serde::Serialize)]
        struct FieldMirror {
            #[allow(dead_code)]
            id: Address,
            value: StateMirror,
        }
        #[derive(serde::Serialize)]
        struct StateMirror {
            #[allow(dead_code)]
            authority: Address,
            #[allow(dead_code)]
            prev: Option<()>,
            #[allow(dead_code)]
            pending: Option<()>,
        }
        bcs::to_bytes(&FieldMirror {
            id: Address::new([0x11; 32]),
            value: StateMirror {
                authority,
                prev: None,
                pending: None,
            },
        })
        .unwrap()
    }

    fn entombed_chain() -> MockChain {
        let mut chain = MockChain::default();
        for parent in [REGISTRY, VAULT] {
            chain.objects.insert(
                podseq_sui::rotation::rotation_state_field_id(parent, PACKAGE),
                entombed_state_bcs(AUTHORITY),
            );
        }
        chain
    }

    fn migration_watcher(chain: MockChain) -> RotationWatcher<MockChain> {
        RotationWatcher::new(chain, PACKAGE, REGISTRY, VAULT, AUTHORITY, None)
    }

    fn migration_targets() -> RotationTargets {
        RotationTargets {
            package: PACKAGE,
            registry: REGISTRY,
            vault: VAULT,
        }
    }

    fn caps(settler: Option<Address>, bridge: Option<Address>) -> EntombCaps {
        EntombCaps { settler, bridge }
    }

    /// Records submitted PTBs; `fail_settler` simulates the on-chain abort
    /// when a concurrent ceremony won the race.
    #[derive(Default)]
    struct RecordingExecutor {
        log: std::sync::Mutex<Vec<&'static str>>,
        fail_settler: bool,
    }

    impl RecordingExecutor {
        fn steps(&self) -> Vec<&'static str> {
            self.log.lock().unwrap().clone()
        }
    }

    impl EntombExecutor for RecordingExecutor {
        fn entomb_settler<'a>(
            &'a self,
            _cap: Address,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                self.log.lock().unwrap().push("settler");
                if self.fail_settler {
                    return Err(SettlementError::Execution(
                        "entomb raced a concurrent ceremony".into(),
                    ));
                }
                Ok(())
            })
        }

        fn entomb_bridge<'a>(
            &'a self,
            _cap: Address,
        ) -> InboxFuture<'a, Result<(), SettlementError>> {
            Box::pin(async move {
                self.log.lock().unwrap().push("bridge");
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn the_migration_entombs_every_object_missing_its_state() {
        let executor = RecordingExecutor::default();

        let report = ensure_entombed(
            &migration_watcher(MockChain::default()),
            &migration_targets(),
            &caps(Some(SETTLER_CAP), Some(BRIDGE_CAP)),
            &executor,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            MigrationReport {
                settler: EntombStep::Entombed,
                bridge: EntombStep::Entombed,
            }
        );
        assert_eq!(executor.steps(), vec!["settler", "bridge"]);
    }

    #[tokio::test]
    async fn the_migration_leaves_present_states_alone() {
        let executor = RecordingExecutor::default();

        let report = ensure_entombed(
            &migration_watcher(entombed_chain()),
            &migration_targets(),
            &caps(Some(SETTLER_CAP), Some(BRIDGE_CAP)),
            &executor,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            MigrationReport {
                settler: EntombStep::Present,
                bridge: EntombStep::Present,
            }
        );
        assert!(executor.steps().is_empty());
    }

    #[tokio::test]
    async fn the_migration_heals_only_the_missing_side() {
        // Half-entombed chain (a crash between the two PTBs): the next poll
        // retries exactly the missing half.
        let mut chain = entombed_chain();
        chain
            .objects
            .remove(&podseq_sui::rotation::rotation_state_field_id(
                VAULT, PACKAGE,
            ));
        let executor = RecordingExecutor::default();

        let report = ensure_entombed(
            &migration_watcher(chain),
            &migration_targets(),
            &caps(Some(SETTLER_CAP), Some(BRIDGE_CAP)),
            &executor,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            MigrationReport {
                settler: EntombStep::Present,
                bridge: EntombStep::Entombed,
            }
        );
        assert_eq!(executor.steps(), vec!["bridge"]);
    }

    #[tokio::test]
    async fn the_migration_refuses_missing_states_without_a_cap_id() {
        let executor = RecordingExecutor::default();

        let report = ensure_entombed(
            &migration_watcher(MockChain::default()),
            &migration_targets(),
            &caps(None, None),
            &executor,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            MigrationReport {
                settler: EntombStep::Unconfigured,
                bridge: EntombStep::Unconfigured,
            }
        );
        assert!(executor.steps().is_empty());
    }

    #[tokio::test]
    async fn the_migration_mixes_a_heal_with_an_unconfigured_refusal() {
        let executor = RecordingExecutor::default();

        let report = ensure_entombed(
            &migration_watcher(MockChain::default()),
            &migration_targets(),
            &caps(Some(SETTLER_CAP), None),
            &executor,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            MigrationReport {
                settler: EntombStep::Entombed,
                bridge: EntombStep::Unconfigured,
            }
        );
        assert_eq!(executor.steps(), vec!["settler"]);
        assert!(report.blocked());
    }

    #[tokio::test]
    async fn the_migration_propagates_transport_errors_without_submitting() {
        // An unreadable chain state must never be read as "needs entombment":
        // entombing on a stale read would move a cap the node may not own.
        let mut chain = MockChain::default();
        chain
            .fail_reads
            .push(podseq_sui::rotation::rotation_state_field_id(
                REGISTRY, PACKAGE,
            ));
        let executor = RecordingExecutor::default();

        let err = ensure_entombed(
            &migration_watcher(chain),
            &migration_targets(),
            &caps(Some(SETTLER_CAP), Some(BRIDGE_CAP)),
            &executor,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, InboxError::Rpc(_)));
        assert!(executor.steps().is_empty());
    }

    #[tokio::test]
    async fn an_aborted_entomb_ptb_stops_the_poll_and_retries_next_time() {
        // The settler PTB raced a concurrent ceremony and aborted: the poll
        // fails, the bridge side is not attempted, and the next poll re-reads
        // (the entombed side then reports Present).
        let executor = RecordingExecutor {
            fail_settler: true,
            ..Default::default()
        };

        let err = ensure_entombed(
            &migration_watcher(MockChain::default()),
            &migration_targets(),
            &caps(Some(SETTLER_CAP), Some(BRIDGE_CAP)),
            &executor,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, InboxError::Rpc(msg) if msg.contains("raced a concurrent ceremony")));
        assert_eq!(executor.steps(), vec!["settler"]);
    }

    #[test]
    fn migration_logging_handles_every_report_shape() {
        // Coverage of the four log paths; tracing output is discarded here.
        log_migration(
            Ok(MigrationReport {
                settler: EntombStep::Entombed,
                bridge: EntombStep::Present,
            }),
            None,
        );
        log_migration(
            Ok(MigrationReport {
                settler: EntombStep::Unconfigured,
                bridge: EntombStep::Present,
            }),
            None,
        );
        let read_err = InboxError::Rpc("injected".into());
        log_migration(
            Ok(MigrationReport {
                settler: EntombStep::Present,
                bridge: EntombStep::Present,
            }),
            Some(&read_err),
        );
        log_migration(Err(InboxError::NotFound("injected".into())), None);
    }

    #[test]
    fn contract_id_resolution_requires_the_full_triple() {
        let mut config = Config::testnet();
        assert!(resolve_contract_ids(&config).is_none());

        config.sui.settlement_package_id = Some(format!("{PACKAGE}"));
        config.sui.registry_id = Some(format!("{REGISTRY}"));
        config.bridge.vault_id = Some(format!("{VAULT}"));
        assert_eq!(
            resolve_contract_ids(&config),
            Some((
                format!("{PACKAGE}"),
                format!("{REGISTRY}"),
                format!("{VAULT}")
            ))
        );

        // A partial triple stays disabled.
        config.bridge.vault_id = None;
        assert!(resolve_contract_ids(&config).is_none());
    }

    #[test]
    fn staged_key_resolution_distinguishes_unset_from_unreadable() {
        let mut config = Config::testnet();
        config.sui.rotation_key_path = None;
        assert!(resolve_staged_key(&config).unwrap().is_none());

        let dir = std::env::temp_dir().join(format!(
            "podseq-rotation-resolve-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("staged.key");
        std::fs::write(
            &key_path,
            Ed25519PrivateKey::new([0x5E; 32])
                .to_suiprivkey()
                .expect("static key encoding"),
        )
        .unwrap();

        config.sui.rotation_key_path = Some(key_path);
        let key = resolve_staged_key(&config).unwrap().expect("staged key");
        assert_eq!(
            key.public_key().derive_address(),
            Ed25519PrivateKey::new([0x5E; 32])
                .public_key()
                .derive_address()
        );

        config.sui.rotation_key_path = Some(dir.join("missing.key"));
        assert!(resolve_staged_key(&config).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migration_reports_classify_for_logging() {
        let healed = MigrationReport {
            settler: EntombStep::Entombed,
            bridge: EntombStep::Present,
        };
        let blocked = MigrationReport {
            settler: EntombStep::Unconfigured,
            bridge: EntombStep::Present,
        };
        let quiet = MigrationReport {
            settler: EntombStep::Present,
            bridge: EntombStep::Present,
        };

        assert!(healed.healed());
        assert!(!healed.blocked());
        assert!(blocked.blocked());
        assert!(!blocked.healed());
        assert!(!quiet.healed() && !quiet.blocked());
    }

    #[test]
    fn cap_id_parsing_treats_unparseable_ids_as_unset() {
        assert_eq!(parse_optional_id(None, "f"), None);
        assert_eq!(
            parse_optional_id(Some(&format!("{SETTLER_CAP:?}")), "f"),
            None // Debug format is not a valid 0x id
        );
        assert_eq!(
            parse_optional_id(Some(&format!("{SETTLER_CAP}")), "f"),
            Some(SETTLER_CAP)
        );
    }
}
