//! Full node sync: reconstructs the chain from DA + settlement.
//!
//! Polls the Sui settlement Registry for finalized blobs, fetches each batch
//! from Walrus, verifies the sequencer signature, and feeds blocks to Reth.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
};
use anyhow::{bail, Context, Result};
use podseq_core::{DataAvailability, Header};
use podseq_engine::{Engine, EngineError, PARENT_BEACON_BLOCK_ROOT};
use podseq_p2p::{AdmissionHandle, BlockReceiver};
use podseq_sui::inbox::{InboxError, InboxTransport, SuiInboxTransport};
use podseq_sui::peers::PeerBookReader;
use podseq_sui::state_root::StateRootReader;
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

/// The engine calls the full node makes while applying and finalizing
/// blocks. Isolated as a trait so the applied-block integrity gate runs in
/// unit tests without a Reth process.
trait EngineOps: Send + Sync {
    fn current_head(&self) -> EngineFuture<'_, Result<B256, EngineError>>;
    fn block_number(&self) -> EngineFuture<'_, Result<u64, EngineError>>;
    fn new_payload(
        &self,
        payload: &ExecutionPayloadV3,
    ) -> EngineFuture<'_, Result<PayloadStatus, EngineError>>;
    fn fork_choice(
        &self,
        state: ForkchoiceState,
    ) -> EngineFuture<'_, Result<ForkchoiceUpdated, EngineError>>;
    fn block_by_number(&self, height: u64) -> EngineFuture<'_, Result<Option<B256>, EngineError>>;
    fn block_state_root(&self, number: u64) -> EngineFuture<'_, Result<Option<B256>, EngineError>>;
}

type EngineFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl EngineOps for Engine {
    fn current_head(&self) -> EngineFuture<'_, Result<B256, EngineError>> {
        Box::pin(Engine::current_head(self))
    }

    fn block_number(&self) -> EngineFuture<'_, Result<u64, EngineError>> {
        Box::pin(self.rpc().block_number())
    }

    fn new_payload(
        &self,
        payload: &ExecutionPayloadV3,
    ) -> EngineFuture<'_, Result<PayloadStatus, EngineError>> {
        let payload = payload.clone();
        Box::pin(async move {
            self.rpc()
                .new_payload_v4(payload, vec![], PARENT_BEACON_BLOCK_ROOT)
                .await
        })
    }

    fn fork_choice(
        &self,
        state: ForkchoiceState,
    ) -> EngineFuture<'_, Result<ForkchoiceUpdated, EngineError>> {
        Box::pin(async move { self.rpc().fork_choice_updated_v3(state, None).await })
    }

    fn block_by_number(&self, height: u64) -> EngineFuture<'_, Result<Option<B256>, EngineError>> {
        Box::pin(self.rpc().block_by_number(height))
    }

    fn block_state_root(&self, number: u64) -> EngineFuture<'_, Result<Option<B256>, EngineError>> {
        Box::pin(self.rpc().block_state_root(number))
    }
}

/// Verdict of the applied-block integrity gate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BlockRejection {
    /// Reth is still evaluating the payload's parent chain.
    Syncing,
    /// Reth judged the payload invalid against its own chain.
    Invalid { detail: String },
    /// The engine validated a different hash than the payload claims.
    ValidatedHashMismatch { validated: B256, claimed: B256 },
    /// The served payload disagrees with the header the sequencer signed.
    HeaderBinding { detail: String },
    /// The header does not chain onto this node's verified head.
    ParentMismatch { expected: B256, actual: B256 },
}

/// Whether a block was applied to the head or rejected by the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApplyOutcome {
    Applied,
    Rejected(BlockRejection),
}

/// The payload is what a source (peer, aggregator) served; the header is
/// what the sequencer signed. The two must describe the same block before
/// any engine call, or the served data is not the signed block's data.
fn bind_payload_to_header(header: &Header, payload: &ExecutionPayloadV3) -> Option<BlockRejection> {
    let inner = &payload.payload_inner.payload_inner;
    if inner.parent_hash != B256::from(header.parent_hash)
        || inner.block_number != header.height
        || inner.timestamp != header.timestamp
    {
        return Some(BlockRejection::HeaderBinding {
            detail: format!(
                "payload parent={} number={} timestamp={} vs header parent={} height={} timestamp={}",
                inner.parent_hash,
                inner.block_number,
                inner.timestamp,
                B256::from(header.parent_hash),
                header.height,
                header.timestamp
            ),
        });
    }
    None
}

/// Reth's verdict binds the payload to execution: VALID means the post-state
/// matched the payload's claimed state root against the claimed parent on
/// Reth's own chain, and the claimed root is the one the header signed.
fn execution_verdict(
    status: &PayloadStatus,
    payload: &ExecutionPayloadV3,
) -> Option<BlockRejection> {
    match &status.status {
        PayloadStatusEnum::Valid => {}
        PayloadStatusEnum::Syncing | PayloadStatusEnum::Accepted => {
            return Some(BlockRejection::Syncing)
        }
        PayloadStatusEnum::Invalid { validation_error } => {
            return Some(BlockRejection::Invalid {
                detail: validation_error.clone(),
            })
        }
    }
    let claimed = payload.payload_inner.payload_inner.block_hash;
    if status.latest_valid_hash != Some(claimed) {
        return Some(BlockRejection::ValidatedHashMismatch {
            validated: status.latest_valid_hash.unwrap_or_default(),
            claimed,
        });
    }
    None
}

/// One-way latch for settled blocks that fail local validation. Encode
/// determinism pins a batch to its blob id, so re-fetching cannot change
/// the verdict — recovery is an operator event (a re-publish changes the
/// settlement commitment), like the state-root latch.
#[derive(Debug, Default)]
struct BlockGate {
    latched: bool,
}

impl BlockGate {
    fn latch(&mut self) {
        self.latched = true;
    }

    fn latched(&self) -> bool {
        self.latched
    }
}

/// Reconstructs the chain from DA + settlement, feeding blocks to Reth.
pub struct FullNode {
    ops: Box<dyn EngineOps>,
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
    /// Settled-state-root verification: the on-chain anchor reader per
    /// settled height. `None` when `sui.verify_state_roots` is off or the
    /// registry/package ids are missing or invalid.
    state_root_reader: Option<StateRootReader<SuiInboxTransport>>,
    /// Latched by a proven settled-root mismatch: finalization stops until
    /// an operator intervenes (a wrong anchor cannot self-heal).
    state_root_gate: StateRootGate,
    /// Latched by a settled block that fails the applied-block integrity
    /// gate: the sync stops until an operator intervenes (a rejected blob
    /// cannot self-heal by re-fetching).
    block_gate: BlockGate,
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
            ops: Box::new(engine),
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
            state_root_reader: build_state_root_reader(config),
            state_root_gate: StateRootGate::default(),
            block_gate: BlockGate::default(),
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
                    .ops
                    .current_head()
                    .await
                    .context("discovering chain head")?;
                self.head_height = self
                    .ops
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
                    if self.state_root_gate.latched() {
                        continue;
                    }
                    if self.block_gate.latched() {
                        continue;
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
            self.verify_and_record_state_root(height, &block.header.state_root)
                .await?;
        }

        Ok(())
    }

    /// Verifies the settled height's state root against the on-chain anchor
    /// and this node's own execution. A mismatch latches the gate and stops
    /// the sync; read errors are logged and never pause (same posture as the
    /// deadline gate).
    async fn verify_and_record_state_root(
        &mut self,
        height: u64,
        da_root: &[u8; 32],
    ) -> Result<()> {
        let outcome = match self.state_root_reader.as_ref() {
            Some(reader) => verify_settled_root(&self.ops, reader, height, da_root).await,
            None => return Ok(()),
        };
        record_state_root(&self.metrics, &outcome);
        match outcome {
            Ok(StateRootVerdict::Verified) => {
                debug!(height, "full node: settled state root verified");
            }
            Ok(StateRootVerdict::NoAnchor) => {
                debug!(height, "full node: no state root anchored for height");
            }
            Ok(StateRootVerdict::Mismatch {
                anchored,
                canonical,
            }) => {
                error!(
                    height,
                    da = %B256::from(*da_root),
                    canonical = %canonical,
                    anchored = ?anchored.map(B256::from),
                    "full node: STATE ROOT MISMATCH — the settled anchor or the signed \
                     block disagrees with this node's execution; halting finalization"
                );
                self.state_root_gate.latch();
                anyhow::bail!("state root mismatch at settled height {height}");
            }
            Err(e) => {
                warn!(
                    height,
                    error = %e,
                    "full node: state-root verification read failed; continuing"
                );
            }
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
        self.handle_announced_block(&block).await
    }

    /// Verifies and applies one announced block. A gate rejection is
    /// advisory here: p2p is not the finalization authority, so the block is
    /// skipped and the DA path re-adjudicates the same height from the
    /// settled blob.
    async fn handle_announced_block(&mut self, block: &podseq_core::Block) -> Result<()> {
        self.verify_signature(block)?;
        match self.apply_block(block).await {
            Ok(ApplyOutcome::Applied) => Ok(()),
            Ok(ApplyOutcome::Rejected(rejection)) => {
                info!(
                    height = block.header.height,
                    rejection = ?rejection,
                    "full node: skipping p2p block rejected by the integrity gate"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Applies a block to the head through the integrity gate: the payload
    /// must agree with the signed header, chain onto this node's verified
    /// head, and pass Reth's execution with the validated hash matching the
    /// payload. The head never advances on a rejection.
    async fn apply_block(&mut self, block: &podseq_core::Block) -> Result<ApplyOutcome> {
        let payload: ExecutionPayloadV3 =
            serde_json::from_slice(&block.data).context("decoding execution payload")?;

        if let Some(rejection) = bind_payload_to_header(&block.header, &payload) {
            return Ok(self.reject(rejection, block.header.height));
        }
        if block.header.height > self.head_height
            && (block.header.height != self.head_height + 1
                || block.header.parent_hash != self.head.0)
        {
            // Append path: the header must chain onto this node's verified
            // head, so a wrong parent or a skipped height (a gap between the
            // head and the announced block) rejects instead of desyncing the
            // optimistic chain. Heights at or below the head are the
            // equivocation re-apply path, adjudicated by the execution
            // verdict and apply_and_finalize's canonical check.
            return Ok(self.reject(
                BlockRejection::ParentMismatch {
                    expected: self.head,
                    actual: B256::from(block.header.parent_hash),
                },
                block.header.height,
            ));
        }

        let status = self
            .ops
            .new_payload(&payload)
            .await
            .context("submitting block to Reth via engine_newPayloadV4")?;
        if let Some(rejection) = execution_verdict(&status, &payload) {
            return Ok(self.reject(rejection, block.header.height));
        }

        let block_hash = payload.payload_inner.payload_inner.block_hash;
        self.head = block_hash;
        self.head_height = block.header.height;
        self.metrics.block_height.set(block.header.height as i64);

        let fc_state = ForkchoiceState {
            head_block_hash: self.head,
            safe_block_hash: self.safe,
            finalized_block_hash: self.finalized,
        };
        self.ops
            .fork_choice(fc_state)
            .await
            .context("advancing forkchoice via engine_forkchoiceUpdatedV3")?;

        info!(
            height = block.header.height,
            ?block_hash,
            "full node: block applied to head"
        );

        Ok(ApplyOutcome::Applied)
    }

    /// Records one gate rejection: evidence in the log plus the rejection
    /// counter. Shared by both apply paths.
    fn reject(&mut self, rejection: BlockRejection, height: u64) -> ApplyOutcome {
        warn!(
            height,
            rejection = ?rejection,
            "full node: block rejected by the integrity gate"
        );
        self.metrics.block_gate_rejections_total.inc();
        ApplyOutcome::Rejected(rejection)
    }

    /// Marks an already-applied block as safe+finalized and advances the DA
    /// sync cursor. This is the finalization flow; it never re-submits the
    /// payload, it only moves the fork-choice `finalized` pointer via
    /// `engine_forkchoiceUpdatedV3` and persists the cursor.
    async fn mark_finalized(&mut self, block: &podseq_core::Block) -> Result<()> {
        let payload: ExecutionPayloadV3 =
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
        self.ops
            .fork_choice(fc_state)
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
        let payload: ExecutionPayloadV3 =
            serde_json::from_slice(&block.data).context("decoding execution payload")?;
        let block_hash = payload.payload_inner.payload_inner.block_hash;
        let height = block.header.height;

        let already_applied = if self.head_height >= height {
            // A p2p block at/above this height was applied; check that Reth's
            // block here matches the canonical DA block.
            self.ops
                .block_by_number(height)
                .await
                .context("querying Reth block for DA verification")?
                == Some(block_hash)
        } else {
            false
        };

        if !already_applied {
            match self.apply_block(block).await? {
                ApplyOutcome::Applied => {}
                ApplyOutcome::Rejected(rejection) => {
                    // A settled block that fails local validation cannot
                    // self-heal: encode determinism pins the batch to its
                    // blob id, so re-fetch returns the same bytes. Latch and
                    // stop the sync; recovery is an operator event.
                    self.block_gate.latch();
                    self.metrics.block_gate_status.set(2);
                    error!(
                        height,
                        rejection = ?rejection,
                        "full node: latching the block gate on a rejected settled block"
                    );
                    bail!("DA block {height} rejected by the integrity gate: {rejection:?}");
                }
            }
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

/// Reads the canonical state root of a height from the local execution
/// layer. The seam keeps settled-root verification unit-testable without a
/// live Reth.
trait CanonicalRoot: Send + Sync + 'static {
    fn state_root_at<'a>(
        &'a self,
        height: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<B256>, EngineError>> + Send + 'a>>;
}

impl CanonicalRoot for Engine {
    fn state_root_at<'a>(
        &'a self,
        height: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<B256>, EngineError>> + Send + 'a>> {
        Box::pin(async move { self.rpc().block_state_root(height).await })
    }
}

impl CanonicalRoot for Box<dyn EngineOps> {
    fn state_root_at<'a>(
        &'a self,
        height: u64,
    ) -> Pin<Box<dyn Future<Output = Result<Option<B256>, EngineError>> + Send + 'a>> {
        self.block_state_root(height)
    }
}

/// What a settled height's three-way root comparison found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StateRootVerdict {
    /// Sui anchor, DA header, and local execution all agree.
    Verified,
    /// Nothing anchored for this height (a height settled before anchoring
    /// existed, or the sequencer pinned `commit_state_roots` off); DA and
    /// execution agree.
    NoAnchor,
    /// A provable disagreement between the on-chain anchor, the signed DA
    /// header, and this node's own execution.
    Mismatch {
        anchored: Option<[u8; 32]>,
        canonical: B256,
    },
}

/// Errors from settled-root verification. Both sources must answer for a
/// verdict; a settled height with no canonical block is an error too, not a
/// pass.
#[derive(Debug, thiserror::Error)]
enum StateRootVerifyError {
    #[error(transparent)]
    Inbox(#[from] InboxError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("no canonical block at settled height {0}")]
    MissingCanonical(u64),
}

/// Three-way comparison at a settled height: the DA header's state root
/// against the node's own execution, and against the Sui anchor the
/// sequencer committed at settle. Both sources are always read so a
/// mismatch carries the full evidence — the comparison halts the node
/// anyway, so attribution is worth more than the saved read.
async fn verify_settled_root<R: CanonicalRoot, T: InboxTransport>(
    canonical: &R,
    anchored: &StateRootReader<T>,
    height: u64,
    da_root: &[u8; 32],
) -> Result<StateRootVerdict, StateRootVerifyError> {
    let anchored_root = anchored.at(height).await?;
    let canonical_root = canonical
        .state_root_at(height)
        .await?
        .ok_or(StateRootVerifyError::MissingCanonical(height))?;
    let canonical_bytes: [u8; 32] = canonical_root.into();
    if *da_root != canonical_bytes {
        return Ok(StateRootVerdict::Mismatch {
            anchored: anchored_root,
            canonical: canonical_root,
        });
    }
    match anchored_root {
        Some(anchor) if anchor == *da_root => Ok(StateRootVerdict::Verified),
        Some(anchor) => Ok(StateRootVerdict::Mismatch {
            anchored: Some(anchor),
            canonical: canonical_root,
        }),
        None => Ok(StateRootVerdict::NoAnchor),
    }
}

/// Latched by a proven settled-root mismatch. Unlike a missed deadline, a
/// wrong anchor cannot self-heal — the node stops finalizing further
/// heights until an operator intervenes.
#[derive(Debug, Default)]
struct StateRootGate {
    mismatched: bool,
}

impl StateRootGate {
    fn latch(&mut self) {
        self.mismatched = true;
    }

    fn latched(&self) -> bool {
        self.mismatched
    }
}

/// Scraped status code for a settled-root verification outcome.
fn state_root_status_code(outcome: &Result<StateRootVerdict, StateRootVerifyError>) -> i64 {
    match outcome {
        Ok(StateRootVerdict::Verified) => 1,
        Ok(StateRootVerdict::NoAnchor) => 2,
        Ok(StateRootVerdict::Mismatch { .. }) => 3,
        Err(_) => 4,
    }
}

/// Writes the verification gauge and the mismatch counter.
fn record_state_root(
    metrics: &PodseqMetrics,
    outcome: &Result<StateRootVerdict, StateRootVerifyError>,
) {
    metrics
        .state_root_status
        .set(state_root_status_code(outcome));
    if matches!(outcome, Ok(StateRootVerdict::Mismatch { .. })) {
        metrics.state_root_mismatches_total.inc();
    }
}

/// Builds the anchor reader for settled-root verification. `None` disables
/// it: the flag pinned off, or a registry/package id that is missing, blank,
/// or unparseable (same posture as the other optional Sui readers).
fn build_state_root_reader(config: &Config) -> Option<StateRootReader<SuiInboxTransport>> {
    if !config.sui.verify_state_roots {
        info!("full node: sui.verify_state_roots off — settled-root verification disabled");
        return None;
    }
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
    match (registry, package) {
        (Some(registry), Some(package)) => {
            info!("full node: settled-root verification enabled");
            Some(StateRootReader::new(
                SuiInboxTransport::new(&config.sui.rpc_url),
                registry,
                package,
            ))
        }
        _ => {
            warn!(
                "full node: sui.registry_id/sui.settlement_package_id unset or invalid — settled-root verification disabled (the node will not detect a dishonest settled root)"
            );
            None
        }
    }
}

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

#[cfg(test)]
mod state_root_verification_tests {
    use super::*;
    use podseq_sui::state_root::{StateRootReader, STATE_ROOT_LEN};
    use prometheus_client::encoding::text::encode;
    use std::future::Future;
    use std::pin::Pin;
    use sui_sdk_types::Address;

    const REGISTRY: Address = Address::new([0x33; 32]);
    const PACKAGE: Address = Address::new([0x44; 32]);
    const HEIGHT: u64 = 7;

    /// Canonical-root source answering every height with a fixed value.
    enum FixedCanonical {
        Root(Option<B256>),
        Error(&'static str),
    }

    impl CanonicalRoot for FixedCanonical {
        fn state_root_at<'a>(
            &'a self,
            _height: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Option<B256>, EngineError>> + Send + 'a>> {
            match self {
                FixedCanonical::Root(state_root) => Box::pin(async move { Ok(*state_root) }),
                FixedCanonical::Error(message) => Box::pin(async move {
                    Err(EngineError::Rpc {
                        code: -3,
                        message: (*message).to_string(),
                    })
                }),
            }
        }
    }

    /// Transport answering every object read with a fixed outcome.
    enum FixedObject {
        Bytes(Vec<u8>),
        Missing,
        Broken,
    }

    impl InboxTransport for FixedObject {
        fn object_contents<'a>(
            &'a self,
            _id: &'a Address,
        ) -> podseq_sui::inbox::InboxFuture<'a, Result<Vec<u8>, InboxError>> {
            match self {
                FixedObject::Bytes(bytes) => {
                    let bytes = bytes.clone();
                    Box::pin(async move { Ok(bytes) })
                }
                FixedObject::Missing => {
                    Box::pin(async move { Err(InboxError::NotFound("missing".into())) })
                }
                FixedObject::Broken => {
                    Box::pin(async move { Err(InboxError::Parse("bad".into())) })
                }
            }
        }

        fn field_contents<'a>(
            &'a self,
            _table: &'a Address,
            _key: u64,
        ) -> podseq_sui::inbox::InboxFuture<'a, Result<Option<Vec<u8>>, InboxError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    fn root(n: u8) -> [u8; STATE_ROOT_LEN] {
        let mut r = [0u8; STATE_ROOT_LEN];
        r[31] = n;
        r
    }

    /// BCS for `Field<StateRootKey, vector<u8>>`, the shape
    /// `parse_state_root_field` decodes.
    fn anchored_field_bcs(value: &[u8; STATE_ROOT_LEN]) -> Vec<u8> {
        #[derive(serde::Serialize)]
        struct FieldBcs {
            id: Address,
            name: u64,
            value: Vec<u8>,
        }
        bcs::to_bytes(&FieldBcs {
            id: Address::new([0; 32]),
            name: HEIGHT,
            value: value.to_vec(),
        })
        .unwrap()
    }

    fn anchored_reader(transport: FixedObject) -> StateRootReader<FixedObject> {
        StateRootReader::new(transport, REGISTRY, PACKAGE)
    }

    #[tokio::test]
    async fn agreeing_anchor_da_and_execution_verify() {
        let canonical = FixedCanonical::Root(Some(B256::from(root(9))));
        let anchored = anchored_reader(FixedObject::Bytes(anchored_field_bcs(&root(9))));

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9))
            .await
            .unwrap();

        assert_eq!(outcome, StateRootVerdict::Verified);
    }

    #[tokio::test]
    async fn missing_anchor_is_not_a_mismatch() {
        let canonical = FixedCanonical::Root(Some(B256::from(root(9))));
        let anchored = anchored_reader(FixedObject::Missing);

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9))
            .await
            .unwrap();

        assert_eq!(outcome, StateRootVerdict::NoAnchor);
    }

    #[tokio::test]
    async fn da_root_disagreeing_with_execution_is_a_mismatch() {
        let canonical = FixedCanonical::Root(Some(B256::from(root(5))));
        let anchored = anchored_reader(FixedObject::Bytes(anchored_field_bcs(&root(9))));

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9))
            .await
            .unwrap();

        assert_eq!(
            outcome,
            StateRootVerdict::Mismatch {
                anchored: Some(root(9)),
                canonical: B256::from(root(5)),
            }
        );
    }

    #[tokio::test]
    async fn anchor_disagreeing_with_the_da_header_is_a_mismatch() {
        let canonical = FixedCanonical::Root(Some(B256::from(root(9))));
        let anchored = anchored_reader(FixedObject::Bytes(anchored_field_bcs(&root(4))));

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9))
            .await
            .unwrap();

        assert_eq!(
            outcome,
            StateRootVerdict::Mismatch {
                anchored: Some(root(4)),
                canonical: B256::from(root(9)),
            }
        );
    }

    #[tokio::test]
    async fn missing_anchor_with_diverging_execution_still_mismatches() {
        let canonical = FixedCanonical::Root(Some(B256::from(root(5))));
        let anchored = anchored_reader(FixedObject::Missing);

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9))
            .await
            .unwrap();

        assert_eq!(
            outcome,
            StateRootVerdict::Mismatch {
                anchored: None,
                canonical: B256::from(root(5)),
            }
        );
    }

    #[tokio::test]
    async fn missing_canonical_block_is_an_error() {
        let canonical = FixedCanonical::Root(None);
        let anchored = anchored_reader(FixedObject::Missing);

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9)).await;

        assert!(matches!(
            outcome,
            Err(StateRootVerifyError::MissingCanonical(HEIGHT))
        ));
    }

    #[tokio::test]
    async fn canonical_read_errors_propagate() {
        let canonical = FixedCanonical::Error("reth unreachable");
        let anchored = anchored_reader(FixedObject::Missing);

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9)).await;

        assert!(matches!(outcome, Err(StateRootVerifyError::Engine(_))));
    }

    #[tokio::test]
    async fn anchor_read_errors_propagate() {
        let canonical = FixedCanonical::Root(Some(B256::from(root(9))));
        let anchored = anchored_reader(FixedObject::Broken);

        let outcome = verify_settled_root(&canonical, &anchored, HEIGHT, &root(9)).await;

        assert!(matches!(outcome, Err(StateRootVerifyError::Inbox(_))));
    }

    #[test]
    fn gate_starts_unlatched_and_latches_one_way() {
        let mut gate = StateRootGate::default();
        assert!(!gate.latched());

        gate.latch();

        assert!(gate.latched());
    }

    fn verified() -> Result<StateRootVerdict, StateRootVerifyError> {
        Ok(StateRootVerdict::Verified)
    }

    fn no_anchor() -> Result<StateRootVerdict, StateRootVerifyError> {
        Ok(StateRootVerdict::NoAnchor)
    }

    fn mismatch() -> Result<StateRootVerdict, StateRootVerifyError> {
        Ok(StateRootVerdict::Mismatch {
            anchored: Some(root(1)),
            canonical: B256::from(root(2)),
        })
    }

    fn broken() -> Result<StateRootVerdict, StateRootVerifyError> {
        Err(StateRootVerifyError::Inbox(InboxError::Parse("bad".into())))
    }

    #[test]
    fn verified_reads_code_one() {
        assert_eq!(state_root_status_code(&verified()), 1);
    }

    #[test]
    fn no_anchor_reads_code_two() {
        assert_eq!(state_root_status_code(&no_anchor()), 2);
    }

    #[test]
    fn mismatch_reads_code_three() {
        assert_eq!(state_root_status_code(&mismatch()), 3);
    }

    #[test]
    fn read_errors_code_four() {
        assert_eq!(state_root_status_code(&broken()), 4);
    }

    #[test]
    fn record_writes_the_gauge_and_counts_mismatches() {
        let m = PodseqMetrics::new();
        record_state_root(&m, &mismatch());
        record_state_root(&m, &mismatch());

        let mut buffer = String::new();
        encode(&mut buffer, &m.registry).unwrap();

        assert!(buffer.contains("podseq_state_root_status 3"));
        assert!(buffer.contains("podseq_state_root_mismatches_total 2"));
    }

    #[test]
    fn non_mismatch_outcomes_do_not_count() {
        let m = PodseqMetrics::new();
        record_state_root(&m, &verified());
        record_state_root(&m, &broken());

        let mut buffer = String::new();
        encode(&mut buffer, &m.registry).unwrap();

        assert!(buffer.contains("podseq_state_root_status 4"));
        assert!(buffer.contains("podseq_state_root_mismatches_total 0"));
    }

    fn config_with_ids(flag: bool, registry: &str, package: &str) -> Config {
        let mut config = Config::from_str("[reth]\njwt_path = \"jwt.hex\"\n").unwrap();
        config.sui.verify_state_roots = flag;
        config.sui.registry_id = Some(registry.into());
        config.sui.settlement_package_id = Some(package.into());
        config
    }

    #[test]
    fn builds_a_reader_from_valid_ids() {
        let config = config_with_ids(
            true,
            "0x3333333333333333333333333333333333333333333333333333333333333333",
            "0x4444444444444444444444444444444444444444444444444444444444444444",
        );
        assert!(build_state_root_reader(&config).is_some());
    }

    #[test]
    fn pinned_off_disables_verification() {
        let config = config_with_ids(
            false,
            "0x3333333333333333333333333333333333333333333333333333333333333333",
            "0x4444444444444444444444444444444444444444444444444444444444444444",
        );
        assert!(build_state_root_reader(&config).is_none());
    }

    #[test]
    fn blank_package_id_disables_verification() {
        let config = config_with_ids(
            true,
            "0x3333333333333333333333333333333333333333333333333333333333333333",
            "  ",
        );
        assert!(build_state_root_reader(&config).is_none());
    }

    #[test]
    fn unparseable_package_id_disables_verification() {
        let config = config_with_ids(
            true,
            "0x3333333333333333333333333333333333333333333333333333333333333333",
            "not-an-address",
        );
        assert!(build_state_root_reader(&config).is_none());
    }

    #[test]
    fn blank_registry_id_disables_verification() {
        let config = config_with_ids(
            true,
            "  ",
            "0x4444444444444444444444444444444444444444444444444444444444444444",
        );
        assert!(build_state_root_reader(&config).is_none());
    }
}

#[cfg(test)]
mod block_gate_tests {
    use super::*;
    use alloy_primitives::Address;
    use alloy_rpc_types_engine::{
        ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3, ForkchoiceUpdated,
        PayloadStatus, PayloadStatusEnum,
    };
    use podseq_core::keys::Ed25519BlockSigner;
    use podseq_core::BlockSigner;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use sui_crypto::ed25519::Ed25519PrivateKey;

    const HEAD_HASH: B256 = B256::new([0x11; 32]);
    const BLOCK_HASH: B256 = B256::new([0x22; 32]);
    const NEXT_HASH: B256 = B256::new([0x33; 32]);

    fn payload_fixture(
        height: u64,
        parent: B256,
        state_root: B256,
        block_hash: B256,
        timestamp: u64,
    ) -> ExecutionPayloadV3 {
        ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: parent,
                    fee_recipient: Address::ZERO,
                    state_root,
                    receipts_root: B256::ZERO,
                    logs_bloom: Default::default(),
                    prev_randao: B256::ZERO,
                    block_number: height,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp,
                    extra_data: Default::default(),
                    base_fee_per_gas: alloy_primitives::U256::from(7),
                    block_hash,
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        }
    }

    /// A block whose header and payload agree, chaining onto `parent`.
    fn chained_block(height: u64, parent: B256, block_hash: B256) -> podseq_core::Block {
        let payload = payload_fixture(
            height,
            parent,
            B256::new([0x44; 32]),
            block_hash,
            height * 2,
        );
        let header = Header {
            height,
            parent_hash: parent.into(),
            state_root: [0x44; 32],
            timestamp: height * 2,
        };
        podseq_core::Block {
            header,
            data: serde_json::to_vec(&payload).unwrap(),
            signature: None,
        }
    }

    /// Stub engine: records every call, serves programmed verdicts, and
    /// defaults to VALID with the payload's own hash as the validated hash.
    struct StubOps {
        calls: Arc<Mutex<Vec<&'static str>>>,
        payload_verdicts: Mutex<VecDeque<PayloadStatus>>,
        blocks_by_number: Mutex<VecDeque<Option<B256>>>,
    }

    impl StubOps {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                payload_verdicts: Mutex::new(VecDeque::new()),
                blocks_by_number: Mutex::new(VecDeque::new()),
            }
        }

        fn with_verdict(status: PayloadStatusEnum, validated: Option<B256>) -> Self {
            let ops = Self::new();
            ops.payload_verdicts
                .lock()
                .unwrap()
                .push_back(PayloadStatus {
                    status,
                    latest_valid_hash: validated,
                });
            ops
        }

        fn serving_block_at(block_hash: Option<B256>) -> Self {
            let ops = Self::new();
            ops.blocks_by_number.lock().unwrap().push_back(block_hash);
            ops
        }

        fn calls_handle(&self) -> Arc<Mutex<Vec<&'static str>>> {
            self.calls.clone()
        }
    }

    impl EngineOps for StubOps {
        fn current_head(&self) -> EngineFuture<'_, Result<B256, EngineError>> {
            Box::pin(async move { Ok(B256::ZERO) })
        }

        fn block_number(&self) -> EngineFuture<'_, Result<u64, EngineError>> {
            Box::pin(async move { Ok(0) })
        }

        fn new_payload(
            &self,
            payload: &ExecutionPayloadV3,
        ) -> EngineFuture<'_, Result<PayloadStatus, EngineError>> {
            let claimed = payload.payload_inner.payload_inner.block_hash;
            Box::pin(async move {
                self.calls.lock().unwrap().push("new_payload");
                Ok(self
                    .payload_verdicts
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(PayloadStatus {
                        status: PayloadStatusEnum::Valid,
                        latest_valid_hash: Some(claimed),
                    }))
            })
        }

        fn fork_choice(
            &self,
            _state: ForkchoiceState,
        ) -> EngineFuture<'_, Result<ForkchoiceUpdated, EngineError>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push("fork_choice");
                Ok(ForkchoiceUpdated {
                    payload_status: PayloadStatus {
                        status: PayloadStatusEnum::Valid,
                        latest_valid_hash: None,
                    },
                    payload_id: None,
                })
            })
        }

        fn block_by_number(
            &self,
            _height: u64,
        ) -> EngineFuture<'_, Result<Option<B256>, EngineError>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push("block_by_number");
                Ok(self
                    .blocks_by_number
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(None))
            })
        }

        fn block_state_root(
            &self,
            _number: u64,
        ) -> EngineFuture<'_, Result<Option<B256>, EngineError>> {
            Box::pin(async move { Ok(None) })
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-block-gate-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_node(
        ops: StubOps,
        head: B256,
        head_height: u64,
        metrics: Arc<PodseqMetrics>,
    ) -> FullNode {
        let dir = temp_dir("node");
        FullNode {
            ops: Box::new(ops),
            sui: podseq_sui::Client::new(podseq_sui::Config::default()).unwrap(),
            registry_id: String::new(),
            rpc_url: String::new(),
            poll_interval: Duration::from_millis(100),
            synced_height: head_height,
            head,
            head_height,
            safe: head,
            finalized: head,
            persisted_forkchoice: None,
            state_store: StateStore::new(&dir),
            p2p_receiver: None,
            sequencer_pubkey: None,
            table_uid: None,
            deadline: None,
            halt_gate: HaltGate::default(),
            admission: None,
            state_root_reader: None,
            state_root_gate: StateRootGate::default(),
            block_gate: BlockGate::default(),
            metrics,
        }
    }

    fn gate_metrics() -> Arc<PodseqMetrics> {
        Arc::new(PodseqMetrics::new())
    }

    #[tokio::test]
    async fn a_contiguous_valid_block_advances_the_head() {
        let metrics = gate_metrics();
        let ops = StubOps::new();
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());

        let block = chained_block(5, HEAD_HASH, NEXT_HASH);
        node.apply_block(&block).await.expect("apply");

        assert_eq!(node.head, NEXT_HASH);
        assert_eq!(node.head_height, 5);
        assert_eq!(metrics.block_height.get(), 5);
    }

    #[tokio::test]
    async fn an_invalid_execution_verdict_is_rejected() {
        let metrics = gate_metrics();
        let ops = StubOps::with_verdict(
            PayloadStatusEnum::Invalid {
                validation_error: "programmed".to_string(),
            },
            None,
        );
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());

        let block = chained_block(5, HEAD_HASH, NEXT_HASH);
        let outcome = node.apply_block(&block).await.expect("rpc ok");

        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(BlockRejection::Invalid { .. })
        ));
        assert_eq!(node.head, HEAD_HASH);
        assert_eq!(node.head_height, 4);
        assert_eq!(metrics.block_gate_rejections_total.get(), 1);
    }

    #[tokio::test]
    async fn a_syncing_execution_verdict_is_rejected() {
        let metrics = gate_metrics();
        let ops = StubOps::with_verdict(PayloadStatusEnum::Syncing, None);
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());

        let block = chained_block(5, HEAD_HASH, NEXT_HASH);
        let outcome = node.apply_block(&block).await.expect("rpc ok");

        assert_eq!(outcome, ApplyOutcome::Rejected(BlockRejection::Syncing));
        assert_eq!(node.head, HEAD_HASH);
    }

    #[tokio::test]
    async fn a_validated_hash_mismatch_is_rejected() {
        let metrics = gate_metrics();
        let stranger = B256::new([0x55; 32]);
        let ops = StubOps::with_verdict(PayloadStatusEnum::Valid, Some(stranger));
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());

        let block = chained_block(5, HEAD_HASH, NEXT_HASH);
        let outcome = node.apply_block(&block).await.expect("rpc ok");

        assert_eq!(
            outcome,
            ApplyOutcome::Rejected(BlockRejection::ValidatedHashMismatch {
                validated: stranger,
                claimed: NEXT_HASH,
            })
        );
    }

    #[tokio::test]
    async fn a_gap_announce_is_rejected_without_engine_calls() {
        let metrics = gate_metrics();
        let ops = StubOps::new();
        let calls = ops.calls_handle();
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());

        // Height 7 skips 5 and 6: the header cannot chain onto the head.
        let block = chained_block(7, B256::new([0x66; 32]), NEXT_HASH);
        let outcome = node.apply_block(&block).await.expect("rpc ok");

        assert!(matches!(
            outcome,
            ApplyOutcome::Rejected(BlockRejection::ParentMismatch { .. })
        ));
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(metrics.block_gate_rejections_total.get(), 1);
    }

    #[tokio::test]
    async fn a_wrong_parent_hash_is_rejected() {
        let metrics = gate_metrics();
        let ops = StubOps::new();
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());

        let other = B256::new([0x77; 32]);
        let block = chained_block(5, other, NEXT_HASH);
        let outcome = node.apply_block(&block).await.expect("rpc ok");

        assert_eq!(
            outcome,
            ApplyOutcome::Rejected(BlockRejection::ParentMismatch {
                expected: HEAD_HASH,
                actual: other,
            })
        );
        assert_eq!(metrics.block_gate_rejections_total.get(), 1);
    }

    #[tokio::test]
    async fn a_payload_disagreeing_with_its_header_is_rejected() {
        let metrics = gate_metrics();
        // Three independent tamper shapes, all rejected before any engine
        // call: the header the sequencer signed cannot be reconciled with
        // the served data.
        let mutations: Vec<(B256, u64, u64)> = vec![
            (B256::new([0x88; 32]), 5, 10), // parentHash tampered
            (HEAD_HASH, 6, 10),             // blockNumber tampered
            (HEAD_HASH, 5, 999),            // timestamp tampered
        ];
        for (parent, height, timestamp) in mutations {
            let ops = StubOps::new();
            let calls = ops.calls_handle();
            let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());

            let payload =
                payload_fixture(height, parent, B256::new([0x44; 32]), NEXT_HASH, timestamp);
            let header = Header {
                height: 5,
                parent_hash: HEAD_HASH.into(),
                state_root: [0x44; 32],
                timestamp: 10,
            };
            let block = podseq_core::Block {
                header,
                data: serde_json::to_vec(&payload).unwrap(),
                signature: None,
            };
            let outcome = node.apply_block(&block).await.expect("rpc ok");

            assert!(
                matches!(
                    outcome,
                    ApplyOutcome::Rejected(BlockRejection::HeaderBinding { .. })
                ),
                "expected HeaderBinding rejection for parent={parent:?} height={height} ts={timestamp}"
            );
            assert!(calls.lock().unwrap().is_empty());
            assert_eq!(node.head, HEAD_HASH);
        }
        assert_eq!(metrics.block_gate_rejections_total.get(), 3);
    }

    #[tokio::test]
    async fn an_already_applied_height_finalizes_without_resubmission() {
        let metrics = gate_metrics();
        // The p2p path already applied height 5 (head == its hash); the DA
        // batch arrives with the same block. Finalize only.
        let ops = StubOps::serving_block_at(Some(BLOCK_HASH));
        let mut node = test_node(ops, BLOCK_HASH, 5, metrics.clone());

        let block = chained_block(5, HEAD_HASH, BLOCK_HASH);
        node.apply_and_finalize(&block).await.expect("finalize");

        assert_eq!(node.finalized, BLOCK_HASH);
        assert_eq!(node.synced_height, 5);
    }

    #[tokio::test]
    async fn an_equivocating_da_block_reapplies_and_reorgs() {
        let metrics = gate_metrics();
        // Reth's block at this height differs from the canonical DA block:
        // re-apply (fork-level reorg onto the canonical chain).
        let ops = StubOps::serving_block_at(Some(BLOCK_HASH));
        let mut node = test_node(ops, BLOCK_HASH, 5, metrics.clone());

        let block = chained_block(5, HEAD_HASH, NEXT_HASH);
        node.apply_and_finalize(&block).await.expect("re-apply");

        assert_eq!(node.head, NEXT_HASH);
        assert_eq!(node.finalized, NEXT_HASH);
        assert_eq!(node.synced_height, 5);
    }

    #[tokio::test]
    async fn a_rejected_p2p_announcement_is_skipped_without_failing_the_poll() {
        let metrics = gate_metrics();
        // Signed header, tampered payload: signature passes, the gate
        // rejects, and the poll carries on (the DA path re-adjudicates).
        let key_file = temp_dir("signer").join("block.key");
        std::fs::write(
            &key_file,
            Ed25519PrivateKey::new([9u8; 32])
                .to_suiprivkey()
                .expect("static key encoding"),
        )
        .unwrap();
        let signer = Ed25519BlockSigner::from_suiprivkey_file(&key_file).unwrap();
        let public_key = signer.pub_key();
        let verifying_key = sui_crypto::ed25519::Ed25519VerifyingKey::new(&public_key);

        let ops = StubOps::new();
        ops.payload_verdicts
            .lock()
            .unwrap()
            .push_back(PayloadStatus {
                status: PayloadStatusEnum::Invalid {
                    validation_error: "programmed".to_string(),
                },
                latest_valid_hash: None,
            });
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());
        node.sequencer_pubkey = Some(verifying_key.expect("static key"));

        let mut block = chained_block(5, HEAD_HASH, NEXT_HASH);
        block.signature = Some(signer.sign_header(&block.header).unwrap());
        // Tamper the payload AFTER signing: signature stays valid, data lies
        // (gas_used is not a header field, so the binding still passes and
        // the execution verdict is what rejects).
        let mut payload: ExecutionPayloadV3 = serde_json::from_slice(&block.data).unwrap();
        payload.payload_inner.payload_inner.gas_used = 123_456;
        block.data = serde_json::to_vec(&payload).unwrap();

        node.handle_announced_block(&block)
            .await
            .expect("poll carries on");

        assert_eq!(node.head, HEAD_HASH);
        assert_eq!(metrics.block_gate_rejections_total.get(), 1);
    }

    #[tokio::test]
    async fn the_da_path_latches_after_a_rejection() {
        let metrics = gate_metrics();
        let ops = StubOps::with_verdict(
            PayloadStatusEnum::Invalid {
                validation_error: "programmed".to_string(),
            },
            None,
        );
        let mut node = test_node(ops, HEAD_HASH, 4, metrics.clone());
        assert!(!node.block_gate.latched());

        let block = chained_block(5, HEAD_HASH, NEXT_HASH);
        node.apply_and_finalize(&block)
            .await
            .expect_err("rejected blocks stop the DA sync");

        assert!(node.block_gate.latched());
        assert_eq!(metrics.block_gate_status.get(), 2);
    }
}
