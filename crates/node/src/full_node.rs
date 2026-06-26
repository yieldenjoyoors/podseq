//! Full node sync: reconstructs the chain from DA + settlement.
//!
//! Polls the Sui settlement Registry for finalized blobs, fetches each batch
//! from Walrus, verifies the sequencer signature, and feeds blocks to Reth.

use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_engine::ForkchoiceState;
use anyhow::{Context, Result};
use podseq_core::DataAvailability;
use podseq_engine::{Engine, PARENT_BEACON_BLOCK_ROOT};
use podseq_p2p::BlockReceiver;
use podseq_store::StateStore;
use podseq_sui::Client as SuiClient;
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
}

impl FullNode {
    /// Creates a full node from config, requiring the sequencer pubkey.
    pub fn new(
        engine: Engine,
        sui: SuiClient,
        config: &Config,
        p2p_receiver: Option<BlockReceiver>,
    ) -> Result<Self> {
        podseq_store::init(&config.data_dir).expect("failed to init storage");
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
        info!("╔{}╗", "═".repeat(80));
        info!("║ FULL NODE: verifying sequencer: {sequencer_address}");
        info!("╚{}╝", "═".repeat(80));

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

        loop {
            tokio::select! {
                _ = ticker.tick() => {
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

        if latest <= self.synced_height {
            return Ok(());
        }

        debug!(
            synced = self.synced_height,
            latest, "full node: syncing finalized blocks from DA"
        );

        // The commitments-table UID is immutable; fetch it once and reuse it.
        if self.table_uid.is_none() {
            self.table_uid = Some(
                podseq_sui::settlement::table_uid(&self.rpc_url, &self.registry_id)
                    .await
                    .context("reading commitments table UID from registry")?,
            );
        }
        let table_uid = self.table_uid.expect("cached");

        // Read commitments one height at a time by their dynamic-field object id,
        // so each poll costs O(new heights).
        // Consecutive heights often share one blob id; cache to avoid re-fetching.
        let mut cached_blob: Option<(podseq_core::BlobId, Vec<podseq_core::Block>)> = None;

        for height in (self.synced_height + 1)..=latest {
            let blob_id = podseq_sui::settlement::commitment_at(&self.rpc_url, &table_uid, height)
                .await
                .with_context(|| format!("reading settlement for height {height}"))?
                .with_context(|| format!("height {height} not settled"))?;

            let blocks = if cached_blob.as_ref().is_some_and(|(id, _)| id == &blob_id) {
                cached_blob.as_ref().unwrap().1.clone()
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

        if let Err(e) = self.state_store.save(&podseq_store::ChainState {
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
fn parse_forkchoice(state: &podseq_store::ChainState) -> Option<(B256, B256, B256)> {
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
