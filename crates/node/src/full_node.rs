//! Full node sync: reconstructs the chain from DA + settlement.
//!
//! Polls the Sui settlement Registry for finalized blobs, fetches each batch
//! from Walrus, verifies the sequencer signature, and feeds blocks to Reth.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use anyhow::{Context, Result};
use podseq_core::DataAvailability;
use podseq_engine::{Engine, PARENT_BEACON_BLOCK_ROOT};
use podseq_p2p::{AdmissionHandle, BlockReceiver};
use podseq_sui::inbox::{InboxError, InboxTransport, SuiInboxTransport};
use podseq_sui::peers::PeerBookReader;
use podseq_sui::{
    deadline::{DeadlineError, DeadlineStatus},
    Client as SuiClient,
};

use crate::metrics::PodseqMetrics;
use crate::store::StateStore;
use std::str::FromStr;
use tokio::signal;
use tracing::{debug, error, info, warn};

use crate::config::Config;

/// Reconstructs the chain from DA + settlement, feeding blocks to Reth.
pub struct FullNode {
    engine: Engine,
    sui: SuiClient,
    registry_id: String,
    rpc_url: String,
    poll_interval: Duration,
    synced_height: u64,
    head: B256,
    /// Height of `head`. Tracked so the DA path can tell whether a settled
    /// block was already applied (via p2p) and only needs finalizing.
    head_height: u64,
    safe: B256,
    finalized: B256,
    /// Restored fork-choice hashes from `state.json`; used to skip the Reth
    /// head-discovery round-trip. `None` on first run (no state yet).
    persisted_forkchoice: Option<(B256, B256, B256)>,
    state_store: StateStore,
    p2p_receiver: Option<BlockReceiver>,
    sequencer_pubkey: Option<sui_crypto::ed25519::Ed25519VerifyingKey>,
    /// Cached commitments-table UID (immutable).
    table_uid: Option<sui_sdk_types::Address>,
    /// Trustless deadline monitor over the shared inbox; `None` when no
    /// `sui.inbox_id` is configured (deadline verification disabled).
    deadline: Option<podseq_sui::deadline::DeadlineMonitor<podseq_sui::inbox::SuiInboxTransport>>,
    halt_gate: HaltGate,
    /// Registry-committee admission: the p2p handle plus the peer-book
    /// reader it is fed from. `None` when `p2p.registry_admission` is off
    /// or the package/registry ids are not configured.
    admission: Option<(AdmissionHandle, PeerBookReader<SuiInboxTransport>)>,
    /// Shared Prometheus instruments. Gauges are written even when the
    /// `[metrics]` endpoint is disabled, so a later scrape config change
    /// finds live values.
    metrics: Arc<PodseqMetrics>,
}

impl FullNode {
    /// Creates a full node from config, requiring the sequencer pubkey.
    pub fn new(
        engine: Engine,
        sui: SuiClient,
        config: &Config,
        p2p_receiver: Option<BlockReceiver>,
        admission_handle: Option<AdmissionHandle>,
        metrics: Arc<PodseqMetrics>,
    ) -> Result<Self> {
        crate::store::init(&config.data_dir)
            .with_context(|| format!("initializing storage under {}", config.data_dir.display()))?;
        let poll_interval = Duration::from_millis(config.sequencer.block_time_ms.max(100));

        // Full nodes verify; they never need the sequencer's private key.
        let pubkey_hex = config
            .signer
            .sequencer_pubkey
            .as_ref()
            .context("sequencer_pubkey is required in full node mode (set signer.sequencer_pubkey to the sequencer's ed25519 public key)")?;
        let public_key = sui_sdk_types::Ed25519PublicKey::from_str(pubkey_hex)
            .with_context(|| format!("invalid sequencer_pubkey: {pubkey_hex}"))?;
        let sequencer_pubkey = sui_crypto::ed25519::Ed25519VerifyingKey::new(&public_key)
            .context("building sequencer verifying key")?;
        let sequencer_address = public_key.derive_address();
        info!("Checking sequencer address in full-node mode: {sequencer_address}");

        // The registry ID is required to read settlements; fail fast with a
        // clear message instead of a cryptic RPC error mid-sync.
        let registry_id = config
            .sui
            .registry_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .context(
                "sui.registry_id is required in full node mode (the shared Registry object ID)",
            )?;

        // Resume from storage: the DA consumption cursor plus the fork-choice
        // hashes we last applied.
        let (synced_height, persisted_forkchoice) = match StateStore::new(&config.data_dir)
            .load()
            .ok()
            .flatten()
        {
            Some(state) => {
                let height = state.height;
                let fc = parse_forkchoice(&state);
                if height > 0 {
                    if let Some((h, _, _)) = fc {
                        info!(synced_height = height, head = ?h, "full node: resumed from storage");
                    } else {
                        info!(
                            synced_height = height,
                            "full node: resumed DA sync cursor from storage"
                        );
                    }
                }
                (height, fc)
            }
            None => (0, None),
        };

        // Deadline verification: read-only, from Sui state alone. Optional so
        // existing full-node configs keep working; without the inbox id the
        // node cannot detect a halt on its own.
        let deadline = build_deadline_monitor(config);

        // Committee admission: register the chain-derived peer book under
        // its on-chain version, so peering is a property of the chain, not
        // of operator configuration.
        let admission = build_admission(config, admission_handle);

        Ok(Self {
            engine,
            sui,
            registry_id,
            rpc_url: config.sui.rpc_url.clone(),
            poll_interval,
            synced_height,
            head: B256::ZERO,
            head_height: synced_height,
            safe: B256::ZERO,
            finalized: B256::ZERO,
            persisted_forkchoice,
            state_store: StateStore::new(&config.data_dir),
            p2p_receiver,
            sequencer_pubkey: Some(sequencer_pubkey),
            table_uid: None,
            deadline,
            halt_gate: HaltGate::default(),
            admission,
            metrics,
        })
    }

    /// Runs the sync loop until SIGINT.
    pub async fn run(mut self) -> Result<()> {
        // Fast path: reuse the fork-choice hashes persisted at the last applied
        // block. Falls back to Reth on first run (or an unreadable state file).
        match self.persisted_forkchoice.take() {
            Some((head, safe, finalized)) => {
                self.head = head;
                self.head_height = self.synced_height;
                self.safe = safe;
                self.finalized = finalized;
                info!(head = ?self.head, "full node: starting sync from persisted head");
            }
            None => {
                info!("full node: discovering chain head from Reth");
                self.head = self
                    .engine
                    .current_head()
                    .await
                    .context("discovering chain head")?;
                self.head_height = self
                    .engine
                    .rpc()
                    .block_number()
                    .await
                    .context("querying head height from Reth")?;
                self.safe = self.head;
                self.finalized = self.head;
                self.synced_height = self.head_height;
                info!(head = ?self.head, "full node: starting sync");
            }
        }

        let mut ticker = tokio::time::interval(self.poll_interval);

        // Committee admission poll: runs beside the sync loop, registers
        // new book versions under their on-chain index. Aborted on drop,
        // i.e. on any run() exit path.
        let _admission_task = AbortOnDrop(spawn_admission_poll(self.admission.take()));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Some(monitor) = &self.deadline {
                        let outcome = monitor.status().await;
                        record_deadline(&self.metrics, &outcome, unix_now_ms());
                        if self.halt_gate.update(outcome) {
                            continue;
                        }
                    }
                    if let Err(e) = self.sync_from_settlement().await {
                        error!(error = ?e, "full node: DA sync failed");
                    }
                    if let Err(e) = self.poll_p2p().await {
                        error!(error = ?e, "full node: P2P poll failed");
                    }
                }
                _ = signal::ctrl_c() => {
                    info!("received SIGINT, shutting down full node");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn sync_from_settlement(&mut self) -> Result<()> {
        let latest =
            podseq_sui::settlement::latest_height(&self.rpc_url, &self.registry_id).await?;
        // Written before the up-to-date early return: the settled height is
        // the settled height whether or not new blocks were waiting, and an
        // alert wants to see it stop advancing.
        self.metrics.settlement_height.set(latest as i64);

        if latest <= self.synced_height {
            return Ok(());
        }

        debug!(
            synced = self.synced_height,
            latest, "full node: syncing finalized blocks from DA"
        );

        // The commitments-table UID is immutable; fetch it once and reuse it.
        let table_uid = match self.table_uid {
            Some(uid) => uid,
            None => {
                let uid = podseq_sui::settlement::table_uid(&self.rpc_url, &self.registry_id)
                    .await
                    .context("reading commitments table UID from registry")?;
                self.table_uid = Some(uid);
                uid
            }
        };

        // Read commitments one height at a time by their dynamic-field object id,
        // so each poll costs O(new heights).
        // Consecutive heights often share one blob id; cache to avoid re-fetching.
        let mut cached_blob: Option<(podseq_core::BlobId, Vec<podseq_core::Block>)> = None;

        for height in (self.synced_height + 1)..=latest {
            let blob_id = podseq_sui::settlement::commitment_at(&self.rpc_url, &table_uid, height)
                .await
                .with_context(|| format!("reading settlement for height {height}"))?
                .with_context(|| format!("height {height} not settled"))?;

            let blocks = if let Some((id, blocks)) = cached_blob.as_ref() {
                if id == &blob_id {
                    blocks.clone()
                } else {
                    let fetched = self.sui.fetch(&blob_id).await?;
                    cached_blob = Some((blob_id, fetched.clone()));
                    fetched
                }
            } else {
                let fetched = self.sui.fetch(&blob_id).await?;
                cached_blob = Some((blob_id, fetched.clone()));
                fetched
            };

            let block = blocks
                .into_iter()
                .find(|b| b.header.height == height)
                .with_context(|| format!("height {height} not found in DA batch"))?;

            self.verify_signature(&block)?;
            self.apply_and_finalize(&block).await?;
        }

        Ok(())
    }

    fn verify_signature(&self, block: &podseq_core::Block) -> Result<()> {
        let pubkey = self
            .sequencer_pubkey
            .as_ref()
            .context("cannot verify blocks: no sequencer public key configured")?;
        if let Err(e) = podseq_sui::verify_block_signature(block, pubkey) {
            error!(height = block.header.height, error = %e, "signature verification FAILED");
            anyhow::bail!(e);
        }
        Ok(())
    }

    async fn poll_p2p(&mut self) -> Result<()> {
        let receiver = match &self.p2p_receiver {
            Some(r) => r,
            None => return Ok(()),
        };
        let announce = match receiver.poll_next().await {
            Some(a) => a,
            None => return Ok(()),
        };

        // Settled heights are finalized by the DA path; ignore stale or
        // equivocating p2p announces at or below the DA frontier.
        if announce.height <= self.synced_height {
            debug!(
                height = announce.height,
                "full node: ignoring p2p announce at/below DA frontier"
            );
            return Ok(());
        }

        info!(height = announce.height, "full node: p2p announce received");
        let block = match receiver.receive(&announce.digest).await {
            Some(b) => b,
            None => {
                debug!(
                    height = announce.height,
                    "full node: p2p block not yet available in cache"
                );
                return Ok(());
            }
        };

        self.verify_signature(&block)?;
        self.apply_block(&block).await?;
        Ok(())
    }

    /// Submits a block to Reth and advances `head`.
    ///
    /// The execution payload is sent once via `engine_newPayloadV4`, and the
    /// fork-choice `head` is advanced. `safe`/`finalized` and the sync cursor
    /// are untouched; they only move in [`mark_finalized`], driven by the DA
    /// path. This is used for both p2p (optimistic head) and DA blocks that
    /// were not yet applied.
    async fn apply_block(&mut self, block: &podseq_core::Block) -> Result<()> {
        let payload: alloy_rpc_types_engine::ExecutionPayloadV3 =
            serde_json::from_slice(&block.data).context("decoding execution payload")?;

        let block_hash = payload.payload_inner.payload_inner.block_hash;

        self.engine
            .rpc()
            .new_payload_v4(payload, vec![], PARENT_BEACON_BLOCK_ROOT)
            .await
            .context("submitting block to Reth via engine_newPayloadV4")?;

        self.head = block_hash;
        self.head_height = block.header.height;
        self.metrics.block_height.set(block.header.height as i64);

        let fc_state = ForkchoiceState {
            head_block_hash: self.head,
            safe_block_hash: self.safe,
            finalized_block_hash: self.finalized,
        };
        self.engine
            .rpc()
            .fork_choice_updated_v3(fc_state, None)
            .await
            .context("advancing forkchoice via engine_forkchoiceUpdatedV3")?;

        info!(
            height = block.header.height,
            ?block_hash,
            "full node: block applied to head"
        );

        Ok(())
    }

    /// Marks an already-applied block as safe+finalized and advances the DA
    /// sync cursor. This is the finalization flow; it never re-submits the
    /// payload, it only moves the fork-choice `finalized` pointer via
    /// `engine_forkchoiceUpdatedV3` and persists the cursor.
    async fn mark_finalized(&mut self, block: &podseq_core::Block) -> Result<()> {
        let payload: alloy_rpc_types_engine::ExecutionPayloadV3 =
            serde_json::from_slice(&block.data).context("decoding execution payload")?;
        let block_hash = payload.payload_inner.payload_inner.block_hash;

        self.safe = block_hash;
        self.finalized = block_hash;
        self.synced_height = block.header.height;

        let fc_state = ForkchoiceState {
            head_block_hash: self.head,
            safe_block_hash: self.safe,
            finalized_block_hash: self.finalized,
        };
        self.engine
            .rpc()
            .fork_choice_updated_v3(fc_state, None)
            .await
            .context("advancing finalized via engine_forkchoiceUpdatedV3")?;

        if let Err(e) = self.state_store.save(&crate::store::ChainState {
            head: format!("{:#x}", self.head),
            safe: format!("{:#x}", self.safe),
            finalized: format!("{:#x}", self.finalized),
            height: self.synced_height,
            timestamp: block.header.timestamp,
        }) {
            warn!(height = self.synced_height, error = %e, "full node: failed to persist sync cursor");
        }

        info!(
            height = self.synced_height,
            ?block_hash,
            "full node: block finalized"
        );

        Ok(())
    }

    /// Applies a DA-settled block if it is not already Reth's block at that
    /// height, then marks it finalized. Re-applying is avoided in the common
    /// case (the block was already pulled in via p2p) so finalization never
    /// reorgs; a mismatch (sequencer equivocation) re-applies the canonical DA
    /// block, reorging onto it.
    async fn apply_and_finalize(&mut self, block: &podseq_core::Block) -> Result<()> {
        let payload: alloy_rpc_types_engine::ExecutionPayloadV3 =
            serde_json::from_slice(&block.data).context("decoding execution payload")?;
        let block_hash = payload.payload_inner.payload_inner.block_hash;
        let height = block.header.height;

        let already_applied = if self.head_height >= height {
            // A p2p block at/above this height was applied; check that Reth's
            // block here matches the canonical DA block.
            self.engine
                .rpc()
                .block_by_number(height)
                .await
                .context("querying Reth block for DA verification")?
                == Some(block_hash)
        } else {
            false
        };

        if !already_applied {
            self.apply_block(block).await?;
        }
        self.mark_finalized(block).await?;
        Ok(())
    }
}

/// Parses the persisted fork-choice hashes from chain state.
///
/// Returns `None` if any hash is missing or unparseable, so the caller falls
/// back to discovering the head from Reth. Empty strings (a fresh or partial
/// state file) are treated as absent rather than errors.
fn parse_forkchoice(state: &crate::store::ChainState) -> Option<(B256, B256, B256)> {
    let parse = |s: &str| {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        B256::from_str(s)
            .map_err(
                |e| warn!(hash = s, error = %e, "full node: ignoring unparseable persisted hash"),
            )
            .ok()
    };
    Some((
        parse(&state.head)?,
        parse(&state.safe)?,
        parse(&state.finalized)?,
    ))
}

/// Builds the trustless deadline monitor from node config.
///
/// `None` disables verification: no `sui.inbox_id` configured (warned — the
/// node cannot detect a halt on its own), or an id that does not parse.
fn build_deadline_monitor(
    config: &Config,
) -> Option<podseq_sui::deadline::DeadlineMonitor<podseq_sui::inbox::SuiInboxTransport>> {
    let id = config.sui.inbox_id.as_deref().unwrap_or("").trim();
    if id.is_empty() {
        warn!(
            "full node: sui.inbox_id unset — deadline verification disabled (the node will not detect a force-inclusion halt)"
        );
        return None;
    }
    match podseq_sui::deadline::monitor_from_config(
        &config.sui.rpc_url,
        id,
        podseq_sui::deadline::DEFAULT_MARGIN_MS,
    ) {
        Ok(monitor) => {
            info!(inbox = id, "full node: deadline verification enabled");
            Some(monitor)
        }
        Err(e) => {
            warn!(error = %e, "full node: deadline verification disabled");
            None
        }
    }
}

/// Cadence for registry-committee admission polls. The book changes at
/// human/operational speed; one read every 10s is cheap and fresh.
const ADMISSION_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Spawns the committee admission poll task, if configured.
fn spawn_admission_poll(
    admission: Option<(AdmissionHandle, PeerBookReader<SuiInboxTransport>)>,
) -> Option<tokio::task::JoinHandle<()>> {
    admission.map(|(handle, reader)| {
        tokio::spawn(async move {
            let mut applied: Option<u64> = None;
            let mut ticker = tokio::time::interval(ADMISSION_POLL_INTERVAL);
            loop {
                ticker.tick().await;
                match poll_admission(&reader, &handle, &mut applied).await {
                    Ok(version) => {
                        debug!(version, "full node: committee admission synced");
                    }
                    Err(e) => warn!(
                        error = %e,
                        "full node: committee admission poll failed; keeping previous set"
                    ),
                }
            }
        })
    })
}

/// Aborts the admission poll task when the sync loop exits, on any path.
struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// Registers a chain-derived peer set under its version — the p2p seam,
/// so the poll is unit-testable without a live discovery network.
trait CommitteeAdmission {
    /// Registers `peers` under peer-set index `epoch`.
    fn admit(&self, epoch: u64, peers: impl IntoIterator<Item = [u8; 32]>);
}

impl CommitteeAdmission for AdmissionHandle {
    fn admit(&self, epoch: u64, peers: impl IntoIterator<Item = [u8; 32]>) {
        AdmissionHandle::admit(self, epoch, peers);
    }
}

/// Reads the peer book once and registers new versions under their
/// on-chain index. Unchanged versions are a no-op; transport errors
/// propagate so the caller keeps the previously applied set.
async fn poll_admission<T: InboxTransport, A: CommitteeAdmission>(
    reader: &PeerBookReader<T>,
    admission: &A,
    applied: &mut Option<u64>,
) -> Result<u64, InboxError> {
    let snapshot = reader.snapshot().await?;
    if *applied != Some(snapshot.version) {
        admission.admit(snapshot.version, snapshot.peers.iter().map(|p| p.pubkey));
        *applied = Some(snapshot.version);
    }
    Ok(snapshot.version)
}

/// Builds the committee admission pair from node config.
///
/// `None` disables admission: no p2p handle (static `bootstrap_peers`
/// mode), no configured package id, or an id that does not parse.
fn build_admission(
    config: &Config,
    handle: Option<AdmissionHandle>,
) -> Option<(AdmissionHandle, PeerBookReader<SuiInboxTransport>)> {
    let handle = handle?;
    let registry = config
        .sui
        .registry_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| podseq_sui::settlement::parse_address(s, "sui.registry_id").ok());
    let package = config
        .sui
        .settlement_package_id
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| podseq_sui::settlement::parse_address(s, "sui.settlement_package_id").ok());
    let (Some(registry), Some(package)) = (registry, package) else {
        warn!(
            "full node: p2p.registry_admission requires sui.registry_id and sui.settlement_package_id — disabled"
        );
        return None;
    };
    let reader = PeerBookReader::new(
        SuiInboxTransport::new(config.sui.rpc_url.clone()),
        registry,
        package,
    );
    Some((handle, reader))
}

/// Prometheus enum code for one deadline-status read, written to
/// `podseq_deadline_status`. 0 (the gauge's zero value) means deadline
/// verification is disabled (no `sui.inbox_id`).
fn deadline_status_code(read: &Result<DeadlineStatus, DeadlineError>) -> i64 {
    match read {
        Ok(DeadlineStatus::Active) => 1,
        Ok(DeadlineStatus::Halted { .. }) => 2,
        Ok(DeadlineStatus::Violated { .. }) => 3,
        Err(_) => 4,
    }
}

/// Seconds the oldest pending forced transaction is past its deadline at
/// `now_ms`, floored from milliseconds; 0 unless the read is a halt or
/// violation (an active chain has nothing overdue, and a failed read has no
/// depth to report).
fn deadline_overdue_seconds(read: &Result<DeadlineStatus, DeadlineError>, now_ms: u64) -> i64 {
    let deadline_ms = match read {
        Ok(DeadlineStatus::Halted { deadline_ms, .. })
        | Ok(DeadlineStatus::Violated { deadline_ms, .. }) => *deadline_ms,
        _ => return 0,
    };
    (now_ms.saturating_sub(deadline_ms) / 1_000) as i64
}

/// Writes the deadline gauges for one poll. Wall-clock time is fine here:
/// the gauge feeds alerts, not consensus — the Sui Clock remains the only
/// deadline authority.
fn record_deadline(
    metrics: &PodseqMetrics,
    read: &Result<DeadlineStatus, DeadlineError>,
    now_ms: u64,
) {
    metrics.deadline_status.set(deadline_status_code(read));
    metrics
        .deadline_overdue_seconds
        .set(deadline_overdue_seconds(read, now_ms));
}

/// Wall-clock milliseconds since the Unix epoch.
fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis() as u64
}

/// Turns deadline-status reads into a pause decision for the sync loop.
///
/// A confirmed halt or a violated gate pauses block application; a transport
/// error or an unconfirmed read
/// ([`DeadlineError::InconsistentSnapshot`]) never does (availability over
/// false positives). Status transitions are logged once, not every tick.
#[derive(Default)]
struct HaltGate {
    last: Option<DeadlineStatus>,
}

impl HaltGate {
    /// Consumes one read and returns whether the node must skip applying
    /// blocks this tick.
    fn update(&mut self, read: Result<DeadlineStatus, DeadlineError>) -> bool {
        match read {
            Ok(status) if status.is_halted() => {
                if self.last.as_ref() != Some(&status) {
                    error!(
                        status = ?status,
                        "full node: force-inclusion halt — not applying further blocks"
                    );
                }
                self.last = Some(status);
                true
            }
            Ok(_) => {
                if self.last.take().is_some() {
                    info!("full node: halt cleared — overdue transaction included");
                }
                false
            }
            Err(e) => {
                warn!(error = %e, "full node: deadline check unavailable; assuming active");
                false
            }
        }
    }
}

#[cfg(test)]
mod admission_poll_tests {
    use super::*;
    use podseq_sui::peers::{PeerBookSnapshot, PublishedPeer};
    use std::cell::RefCell;
    use sui_sdk_types::Address;

    /// Always-failing transport: admission polls must never fake a version.
    struct DownTransport;

    impl InboxTransport for DownTransport {
        fn object_contents<'a>(
            &'a self,
            _id: &'a Address,
        ) -> podseq_sui::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            Box::pin(async move { Err(InboxError::Rpc("transport down".into())) })
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> podseq_sui::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    /// Transport serving one fixed snapshot (id ignored).
    struct FixedTransport {
        snapshot: PeerBookSnapshot,
    }

    impl InboxTransport for FixedTransport {
        fn object_contents<'a>(
            &'a self,
            _id: &'a Address,
        ) -> podseq_sui::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            let snapshot = self.snapshot.clone();
            Box::pin(async move {
                let peers = snapshot
                    .peers
                    .iter()
                    .map(|p| Entry {
                        key: p.pubkey.to_vec(),
                        value: p.addr.clone(),
                    })
                    .collect();
                let field = Field {
                    id: Address::new([0; 32]),
                    value: Book {
                        version: snapshot.version,
                        peers,
                    },
                };
                Ok(bcs::to_bytes(&field).unwrap())
            })
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> podseq_sui::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    #[derive(serde::Serialize)]
    struct Entry {
        key: Vec<u8>,
        value: String,
    }

    #[derive(serde::Serialize)]
    struct Book {
        version: u64,
        peers: Vec<Entry>,
    }

    #[derive(serde::Serialize)]
    struct Field {
        id: Address,
        value: Book,
    }

    use podseq_sui::peers::parse_peer_book;

    /// Records admitted (version, peers) pairs.
    #[derive(Default)]
    struct MockAdmission {
        calls: RefCell<Vec<(u64, Vec<[u8; 32]>)>>,
    }

    impl CommitteeAdmission for MockAdmission {
        fn admit(&self, epoch: u64, peers: impl IntoIterator<Item = [u8; 32]>) {
            self.calls
                .borrow_mut()
                .push((epoch, peers.into_iter().collect()));
        }
    }

    fn reader_for(snapshot: PeerBookSnapshot) -> PeerBookReader<FixedTransport> {
        PeerBookReader::new(
            FixedTransport { snapshot },
            Address::new([1; 32]),
            Address::new([2; 32]),
        )
    }

    fn snapshot(version: u64, pubkeys: &[[u8; 32]]) -> PeerBookSnapshot {
        PeerBookSnapshot {
            version,
            peers: pubkeys
                .iter()
                .map(|k| PublishedPeer {
                    pubkey: *k,
                    addr: "seq:7000".into(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn a_new_version_is_admitted_under_its_index() {
        let reader = reader_for(snapshot(3, &[[0xaa; 32]]));
        let admission = MockAdmission::default();
        let mut applied = None;

        let version = poll_admission(&reader, &admission, &mut applied)
            .await
            .unwrap();

        assert_eq!(version, 3);
        assert_eq!(applied, Some(3));
        assert_eq!(*admission.calls.borrow(), vec![(3, vec![[0xaa; 32]])]);
    }

    #[tokio::test]
    async fn an_unchanged_version_is_a_no_op() {
        let reader = reader_for(snapshot(3, &[[0xaa; 32]]));
        let admission = MockAdmission::default();
        let mut applied = Some(3);

        poll_admission(&reader, &admission, &mut applied)
            .await
            .unwrap();

        assert!(admission.calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_version_change_re_admits() {
        let reader = reader_for(snapshot(4, &[[0xaa; 32], [0xbb; 32]]));
        let admission = MockAdmission::default();
        let mut applied = Some(3);

        poll_admission(&reader, &admission, &mut applied)
            .await
            .unwrap();

        assert_eq!(
            *admission.calls.borrow(),
            vec![(4, vec![[0xaa; 32], [0xbb; 32]])]
        );
        assert_eq!(applied, Some(4));
    }

    #[tokio::test]
    async fn a_transport_error_propagates_and_keeps_the_applied_version() {
        let reader =
            PeerBookReader::new(DownTransport, Address::new([1; 32]), Address::new([2; 32]));
        let admission = MockAdmission::default();
        let mut applied = Some(3);

        let result = poll_admission(&reader, &admission, &mut applied).await;

        assert!(matches!(result, Err(InboxError::Rpc(_))));
        assert_eq!(applied, Some(3));
        assert!(admission.calls.borrow().is_empty());
    }

    #[test]
    fn parse_peer_book_still_roundtrips_against_the_mirror() {
        let bytes = bcs::to_bytes(&Field {
            id: Address::new([0; 32]),
            value: Book {
                version: 9,
                peers: vec![Entry {
                    key: vec![0xcc; 32],
                    value: "a:1".into(),
                }],
            },
        })
        .unwrap();

        let snapshot = parse_peer_book(&bytes).unwrap();

        assert_eq!(snapshot.version, 9);
        assert_eq!(snapshot.peers[0].pubkey, [0xcc; 32]);
    }
}

#[cfg(test)]
mod halt_gate_tests {
    use super::*;

    fn halted() -> DeadlineStatus {
        DeadlineStatus::Halted {
            nonce: 7,
            deadline_ms: 1_600_000,
        }
    }

    fn violated() -> DeadlineStatus {
        DeadlineStatus::Violated {
            nonce: 7,
            deadline_ms: 1_600_000,
            settled_ts_ms: 1_900_000,
        }
    }

    #[test]
    fn active_reads_never_pause() {
        let mut gate = HaltGate::default();

        assert!(!gate.update(Ok(DeadlineStatus::Active)));
    }

    #[test]
    fn transport_errors_never_pause() {
        let mut gate = HaltGate::default();

        assert!(!gate.update(Err(DeadlineError::InboxId("down".into()))));
    }

    #[test]
    fn inconsistent_snapshot_errors_never_pause() {
        // An unconfirmed read is not a halt; the node keeps applying blocks.
        let mut gate = HaltGate::default();

        assert!(!gate.update(Err(DeadlineError::InconsistentSnapshot { attempts: 3 })));
    }

    #[test]
    fn halted_reads_pause_until_cleared() {
        let mut gate = HaltGate::default();

        assert!(gate.update(Ok(halted())));
        assert!(gate.update(Ok(halted())));

        assert!(!gate.update(Ok(DeadlineStatus::Active)));
    }

    #[test]
    fn violated_reads_pause_too() {
        let mut gate = HaltGate::default();

        assert!(gate.update(Ok(violated())));
    }
}

#[cfg(test)]
mod deadline_monitor_tests {
    use super::*;

    fn config_with(inbox_id: Option<&str>) -> Config {
        let mut config = Config::from_str("[reth]\njwt_path = \"jwt.hex\"\n").unwrap();
        config.sui.inbox_id = inbox_id.map(str::to_string);
        config
    }

    #[test]
    fn builds_a_monitor_from_a_valid_inbox_id() {
        let config = config_with(Some(
            "0x0000000000000000000000000000000000000000000000000000000000000001",
        ));

        assert!(build_deadline_monitor(&config).is_some());
    }

    #[test]
    fn no_inbox_id_disables_verification() {
        let config = config_with(None);

        assert!(build_deadline_monitor(&config).is_none());
    }

    #[test]
    fn blank_inbox_id_disables_verification() {
        let config = config_with(Some("  "));

        assert!(build_deadline_monitor(&config).is_none());
    }

    #[test]
    fn unparseable_inbox_id_disables_verification() {
        let config = config_with(Some("not-an-address"));

        assert!(build_deadline_monitor(&config).is_none());
    }
}

#[cfg(test)]
mod halt_metrics_tests {
    use super::*;

    use podseq_sui::deadline::DeadlineError;
    use podseq_sui::inbox::InboxError;
    use prometheus_client::encoding::text::encode;

    fn active() -> Result<DeadlineStatus, DeadlineError> {
        Ok(DeadlineStatus::Active)
    }

    fn halted(deadline_ms: u64) -> Result<DeadlineStatus, DeadlineError> {
        Ok(DeadlineStatus::Halted {
            nonce: 3,
            deadline_ms,
        })
    }

    fn violated(deadline_ms: u64, settled_ts_ms: u64) -> Result<DeadlineStatus, DeadlineError> {
        Ok(DeadlineStatus::Violated {
            nonce: 3,
            deadline_ms,
            settled_ts_ms,
        })
    }

    fn broken() -> Result<DeadlineStatus, DeadlineError> {
        Err(DeadlineError::Inbox(InboxError::Parse("bad".into())))
    }

    #[test]
    fn active_reads_code_one() {
        assert_eq!(deadline_status_code(&active()), 1);
    }

    #[test]
    fn halted_reads_code_two() {
        assert_eq!(deadline_status_code(&halted(1_000)), 2);
    }

    #[test]
    fn violated_reads_code_three() {
        assert_eq!(deadline_status_code(&violated(1_000, 2_000)), 3);
    }

    #[test]
    fn read_errors_code_four() {
        assert_eq!(deadline_status_code(&broken()), 4);
    }

    #[test]
    fn disabled_verification_leaves_the_gauge_at_zero() {
        // The gauge's zero value is the documented "disabled" state.
        let m = PodseqMetrics::new();
        let mut buffer = String::new();
        encode(&mut buffer, &m.registry).unwrap();
        assert!(buffer.contains("podseq_deadline_status 0"));
    }

    #[test]
    fn overdue_seconds_floor_from_milliseconds() {
        // 2.5 s past the deadline reads as 2 overdue seconds.
        assert_eq!(deadline_overdue_seconds(&halted(8_000), 10_500), 2);
    }

    #[test]
    fn overdue_is_zero_at_the_deadline() {
        assert_eq!(deadline_overdue_seconds(&halted(10_000), 10_000), 0);
    }

    #[test]
    fn overdue_is_zero_when_the_deadline_is_in_the_future() {
        assert_eq!(deadline_overdue_seconds(&violated(10_000, 1_000), 9_999), 0);
    }

    #[test]
    fn active_and_error_reads_have_no_overdue_depth() {
        assert_eq!(deadline_overdue_seconds(&active(), 99_000), 0);
        assert_eq!(deadline_overdue_seconds(&broken(), 99_000), 0);
    }

    #[test]
    fn recording_writes_status_and_overdue_gauges() {
        let m = PodseqMetrics::new();

        record_deadline(&m, &halted(2_500), 5_000);
        let mut buffer = String::new();
        encode(&mut buffer, &m.registry).unwrap();
        assert!(buffer.contains("podseq_deadline_status 2"));
        assert!(buffer.contains("podseq_deadline_overdue_seconds 2"));

        // A later active read rewrites both: the halt is over and the
        // overdue depth is back to zero.
        record_deadline(&m, &active(), 5_000);
        let mut buffer = String::new();
        encode(&mut buffer, &m.registry).unwrap();
        assert!(buffer.contains("podseq_deadline_status 1"));
        assert!(buffer.contains("podseq_deadline_overdue_seconds 0"));
    }

    #[test]
    fn recording_an_error_keeps_the_status_visible() {
        let m = PodseqMetrics::new();

        record_deadline(&m, &broken(), 5_000);
        let mut buffer = String::new();
        encode(&mut buffer, &m.registry).unwrap();
        assert!(buffer.contains("podseq_deadline_status 4"));
        assert!(buffer.contains("podseq_deadline_overdue_seconds 0"));
    }
}
