//! Sequencer-side force-inclusion integration.
//!
//! Wires `move/sources/inbox.move` into the block-building path
//! ([`crate::runner`]): a poller reads new inbox nonces over a mocked-able
//! transport and submits the raw transactions to Reth's pool, block building
//! marks which built block contains each forced transaction (idempotent,
//! persisted marker, mirroring the pending-block store), and settlement
//! refuses to run while a nonce is past its block deadline.
//!
//! Deadline model: the on-chain deadline is anchored on Sui clock ms; the
//! sequencer-side gate approximates it in L2 blocks (`first_seen_height +
//! deadline_blocks`, with `deadline_blocks` derived from the block time and
//! the on-chain inclusion window) so refusal happens before the settle call
//! is even built. Contract-side enforcement is the settlement entrypoint's
//! deadline check (see `docs/src/force-inclusion.md`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{keccak256, Bytes, B256};
use anyhow::Context as _;
use podseq_core::Settlement as _;
use podseq_sui::deadline::{parse_clock, SUI_CLOCK_ID};
use podseq_sui::inbox::{
    parse_inbox_status, parse_pending_tx, InboxReader, InboxTransport, SuiInboxTransport,
    WindowView, DEFAULT_WINDOW_MS,
};
use sui_sdk_types::Address;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::bridge::eth_send_raw_transaction;
use crate::config::SuiConfig;
use crate::metrics::PodseqMetrics;
use crate::store::{InboxStore, NonceRecord, StoreError};

/// Default inclusion window on chain (ms), mirrored from `podseq_sui::inbox`
/// (`DEFAULT_WINDOW_MS`, i.e. `inbox.move`'s `INCLUSION_WINDOW_MS`): a forced
/// transaction must be included in a block whose timestamp is at most this
/// many ms after submission. This is only the seed: with a configured
/// settlement package id the tracker re-derives the budget from the chain's
/// window on every sync (it is operator-adjustable).
pub const INCLUSION_WINDOW_MS: u64 = DEFAULT_WINDOW_MS;

/// Converts an on-chain inclusion window into a block budget for the
/// sequencer-side gate: ceil(window / block_time).
pub fn deadline_blocks_for(window_ms: u64, block_time_ms: u64) -> u64 {
    window_ms.div_ceil(block_time_ms.max(1))
}

/// Errors from the sequencer-side inbox tracker.
#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("inbox transport: {0}")]
    Transport(#[from] podseq_sui::inbox::InboxError),
    #[error("inbox store: {0}")]
    Store(#[from] StoreError),
    #[error("corrupt inbox record: {0}")]
    Corrupt(String),
}

/// Verdict of the settlement gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltCheck {
    /// Every tracked nonce is included or within its deadline: settle.
    Clear,
    /// A nonce passed its deadline without inclusion: refuse to settle.
    Halt {
        nonce: u64,
        deadline_height: u64,
        next_height: u64,
    },
}

/// One newly discovered forced transaction, ready for pool submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedSubmission {
    pub nonce: u64,
    pub tx_bytes: Vec<u8>,
    pub tx_hash: B256,
    pub deadline_ms: u64,
    pub deadline_height: u64,
    pub first_seen_height: u64,
}

/// Tracks inbox nonces from chain to inclusion.
///
/// All state transitions go through [`InboxStore`] markers, so a crash at any
/// point re-derives the same state: submissions are persisted before the
/// transaction enters the pool (persist-before-settle), and inclusions are
/// persisted before the settle call that covers them.
///
/// The block budget is chain-derived: with a settlement package id
/// configured, every sync re-reads the inbox's inclusion window (operator
/// adjustable: narrowings apply immediately, widenings behind a timelock) and
/// re-derives `deadline_blocks`. Read errors keep the previous budget.
pub struct InboxTracker {
    transport: Arc<dyn InboxTransport>,
    inbox_id: Address,
    store: InboxStore,
    block_time_ms: u64,
    window_package: Option<Address>,
    deadline_blocks: AtomicU64,
}

impl std::fmt::Debug for InboxTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxTracker")
            .field("inbox_id", &self.inbox_id)
            .field(
                "deadline_blocks",
                &self.deadline_blocks.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl InboxTracker {
    /// Creates a tracker over `inbox_id` with the given persisted marker
    /// store, block time, and chain-package id for window reads. The seeded
    /// `deadline_blocks` applies until the first window read succeeds;
    /// without a package id the seed stands for the tracker's lifetime.
    pub fn new(
        transport: Arc<dyn InboxTransport>,
        inbox_id: Address,
        store: InboxStore,
        deadline_blocks: u64,
        block_time_ms: u64,
        window_package: Option<Address>,
    ) -> Self {
        Self {
            transport,
            inbox_id,
            store,
            block_time_ms,
            window_package,
            deadline_blocks: AtomicU64::new(deadline_blocks),
        }
    }

    /// Polls the inbox for nonces above the local cursor and records each new
    /// one as submitted, before the caller puts it into the L2 pool.
    ///
    /// Returns only freshly tracked submissions. Nonces below the on-chain
    /// settle cursor are skipped (already covered by settlement), and nonces
    /// with no pending entry (pruned between reads) are retried next pass.
    pub async fn sync(&self, at_height: u64) -> Result<Vec<TrackedSubmission>, TrackerError> {
        let contents = self.transport.object_contents(&self.inbox_id).await?;
        let status = parse_inbox_status(&contents)?;
        self.refresh_window().await;
        let tracked = self.store.snapshot()?;

        let deadline_height =
            at_height.saturating_add(self.deadline_blocks.load(Ordering::Relaxed));
        let mut fresh = Vec::new();
        for nonce in status.included_count..status.next_nonce {
            if tracked.contains_key(&nonce) {
                continue;
            }
            let Some(entry) = self
                .transport
                .field_contents(&status.pending_table_uid, nonce)
                .await?
            else {
                // Pruned (covered) between the two reads: nothing to do, and
                // the next pass re-derives the range.
                continue;
            };
            let pending = parse_pending_tx(&entry)?;
            let tx_hash: B256 = keccak256(&pending.tx_bytes);
            self.store.record_submitted(
                nonce,
                deadline_height,
                pending.deadline_ms,
                at_height,
                format!("{tx_hash:#x}"),
                hex::encode(&pending.tx_bytes),
            )?;
            fresh.push(TrackedSubmission {
                nonce,
                tx_hash,
                tx_bytes: pending.tx_bytes,
                deadline_ms: pending.deadline_ms,
                deadline_height,
                first_seen_height: at_height,
            });
        }
        Ok(fresh)
    }

    /// Re-derives the block budget from the chain's inclusion window.
    ///
    /// Best effort by design: any read error (transport, clock, decode) keeps
    /// the previous budget with a warning — the settlement gate must never
    /// pause because a window read degraded. A tracker without a package id
    /// never refreshes (the seed stands). A narrowed window shrinks the
    /// budget immediately; an elapsed staged widening grows it without any
    /// transaction, mirroring `inbox::effective_window_ms`.
    async fn refresh_window(&self) {
        let Some(package) = self.window_package else {
            return;
        };
        let now_ms = match self.transport.object_contents(&SUI_CLOCK_ID).await {
            Ok(bytes) => match parse_clock(&bytes) {
                Ok(ms) => ms,
                Err(e) => {
                    warn!(error = %e, "clock decode failed; keeping the previous window budget");
                    return;
                }
            },
            Err(e) => {
                warn!(error = %e, "clock read failed; keeping the previous window budget");
                return;
            }
        };
        let reader = InboxReader::new(Arc::clone(&self.transport), self.inbox_id);
        let view: WindowView = match reader.window(package, now_ms).await {
            Ok(view) => view,
            Err(e) => {
                warn!(error = %e, "window read failed; keeping the previous window budget");
                return;
            }
        };
        let blocks = deadline_blocks_for(view.effective_ms, self.block_time_ms);
        let previous = self.deadline_blocks.swap(blocks, Ordering::Relaxed);
        if previous != blocks {
            info!(
                window_ms = view.effective_ms,
                override_ms = ?view.override_ms,
                pending = ?view.pending.as_ref().map(|p| p.new_window_ms),
                deadline_blocks = blocks,
                "inclusion-window budget updated from chain"
            );
        }
    }

    /// Marks every tracked-but-unincluded nonce whose transaction bytes appear
    /// in the block at `height` as included there.
    ///
    /// Idempotent: replaying the same block re-marks the same height
    /// harmlessly. A matching transaction surfacing at a *different* height
    /// than an existing marker means the same forced transaction landed in two
    /// blocks (a double-include); that errors instead of being recorded.
    ///
    /// Returns the nonces newly marked included by this call.
    pub fn observe_transactions(
        &self,
        height: u64,
        raw_txs: &[Bytes],
    ) -> Result<Vec<u64>, TrackerError> {
        let block_hashes: std::collections::HashSet<B256> =
            raw_txs.iter().map(|tx| keccak256(tx.as_ref())).collect();
        let mut marked = Vec::new();
        for (nonce, record) in self.store.snapshot()? {
            let tx_hash = match &record {
                NonceRecord::Submitted { tx_hash, .. } | NonceRecord::Included { tx_hash, .. } => {
                    tx_hash
                }
            };
            if !block_hashes.contains(&parse_tx_hash(tx_hash)?) {
                continue;
            }
            let newly = matches!(record, NonceRecord::Submitted { .. });
            // Already-included nonces go through the store too: a second
            // height trips the double-include rejection there.
            self.store.mark_included(nonce, height)?;
            if newly {
                marked.push(nonce);
            }
        }
        Ok(marked)
    }

    /// Marks a single nonce included at `height` (crash-recovery path: a
    /// receipt guard found the transaction already mined). Re-marking an
    /// already included nonce is a no-op at the same height.
    pub fn observe_inclusion(&self, nonce: u64, height: u64) -> Result<(), TrackerError> {
        match self.store.get(nonce)? {
            Some(NonceRecord::Included {
                height: existing, ..
            }) if existing == height => Ok(()),
            Some(NonceRecord::Included {
                height: existing, ..
            }) => Err(TrackerError::Store(StoreError::Conflict(format!(
                "inbox nonce {nonce} already included at height {existing}, \
                     refusing second inclusion at height {height}"
            )))),
            Some(NonceRecord::Submitted { .. }) => Ok(self.store.mark_included(nonce, height)?),
            None => Err(TrackerError::Corrupt(format!(
                "cannot mark untracked inbox nonce {nonce} included"
            ))),
        }
    }

    /// Returns all submitted-but-unincluded nonces with their transaction
    /// bytes, for (re-)submission to the pool. Included nonces are excluded
    /// by construction, so a post-crash re-pass can never double-include.
    pub fn pending_submissions(&self) -> Result<Vec<TrackedSubmission>, TrackerError> {
        self.store
            .submitted()?
            .into_iter()
            .map(|(nonce, record)| match record {
                NonceRecord::Submitted {
                    deadline_height,
                    deadline_ms,
                    first_seen_height,
                    tx_hash,
                    tx_bytes,
                } => Ok(TrackedSubmission {
                    nonce,
                    tx_hash: parse_tx_hash(&tx_hash)?,
                    tx_bytes: decode_tx_bytes(&tx_bytes)?,
                    deadline_ms,
                    deadline_height,
                    first_seen_height,
                }),
                other => Err(TrackerError::Corrupt(format!(
                    "submitted() returned a {other:?} record for nonce {nonce}"
                ))),
            })
            .collect()
    }

    /// Returns the sequencer's inclusion cursor: the count of tracked nonces
    /// marked included. Passed to `settle` alongside the on-chain cursor
    /// (which never regresses) so covered entries are pruned.
    pub fn settle_cursor(&self) -> Result<u64, TrackerError> {
        Ok(self.store.included_count()?)
    }

    /// Settlement gate: `Halt` when a tracked nonce is past its block
    /// deadline without inclusion, `Clear` otherwise.
    pub fn halt_check(&self, next_height: u64) -> Result<HaltCheck, TrackerError> {
        for (nonce, record) in self.store.snapshot()? {
            let NonceRecord::Submitted {
                deadline_height, ..
            } = record
            else {
                continue;
            };
            if deadline_height < next_height {
                return Ok(HaltCheck::Halt {
                    nonce,
                    deadline_height,
                    next_height,
                });
            }
            // Deadlines are non-decreasing in nonce order (each sync anchors
            // at a later height), so the first submitted nonce decides.
            break;
        }
        Ok(HaltCheck::Clear)
    }
}

/// Parses a stored hex tx hash back into a digest.
fn parse_tx_hash(hex_hash: &str) -> Result<B256, TrackerError> {
    let bytes = decode_tx_bytes(hex_hash)?;
    if bytes.len() != 32 {
        return Err(TrackerError::Corrupt(format!(
            "tx hash must be 32 bytes, got {}: {hex_hash}",
            bytes.len()
        )));
    }
    Ok(B256::from_slice(&bytes))
}

/// Decodes stored hex transaction bytes (tolerates a `0x` prefix).
fn decode_tx_bytes(hex_bytes: &str) -> Result<Vec<u8>, TrackerError> {
    hex::decode(hex_bytes.trim_start_matches("0x"))
        .map_err(|e| TrackerError::Corrupt(format!("invalid hex tx payload: {e}")))
}

/// Everything the runner needs to drive the inbox integration.
#[derive(Clone)]
pub struct InboxIntegration {
    tracker: Arc<InboxTracker>,
    http: reqwest::Client,
    l2_rpc_url: String,
}

impl std::fmt::Debug for InboxIntegration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxIntegration")
            .field("l2_rpc_url", &self.l2_rpc_url)
            .finish_non_exhaustive()
    }
}

impl InboxIntegration {
    /// Assembles the integration from a tracker and the L2 RPC endpoint used
    /// for pool submission.
    pub fn from_parts(tracker: Arc<InboxTracker>, l2_rpc_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            tracker,
            http,
            l2_rpc_url,
        }
    }

    /// Builds the integration from node config. `None` when no inbox is
    /// configured (settlement without force inclusion), leaving the runner's
    /// behavior unchanged.
    pub fn from_config(
        sui: &SuiConfig,
        reth_rpc_url: &str,
        data_dir: &Path,
        block_time_ms: u64,
    ) -> Result<Option<Self>, TrackerError> {
        let Some(inbox_id) = sui.inbox_id.as_deref() else {
            return Ok(None);
        };
        let tracker = build_tracker(sui, inbox_id, data_dir, block_time_ms)?;
        Ok(Some(Self::from_parts(
            Arc::new(tracker),
            reth_rpc_url.to_string(),
        )))
    }

    pub fn tracker(&self) -> &Arc<InboxTracker> {
        &self.tracker
    }
}

/// Assembles the tracker from config: both object ids parse fail-closed, and
/// the block budget seeds from the default window until the first chain
/// window read lands.
fn build_tracker(
    sui: &SuiConfig,
    inbox_id: &str,
    data_dir: &Path,
    block_time_ms: u64,
) -> Result<InboxTracker, TrackerError> {
    let inbox_id: Address = inbox_id
        .parse()
        .map_err(|e| TrackerError::Corrupt(format!("parsing sui.inbox_id {inbox_id}: {e}")))?;
    let window_package = sui
        .settlement_package_id
        .as_deref()
        .map(|id| id.parse::<Address>())
        .transpose()
        .map_err(|e| TrackerError::Corrupt(format!("parsing sui.settlement_package_id: {e}")))?;
    let transport = Arc::new(SuiInboxTransport::new(&sui.rpc_url));
    Ok(InboxTracker::new(
        transport,
        inbox_id,
        InboxStore::new(data_dir),
        deadline_blocks_for(INCLUSION_WINDOW_MS, block_time_ms),
        block_time_ms,
        window_package,
    ))
}

/// Spawns the inbox poller when the integration is configured; no-op
/// otherwise. Exits promptly once `shutdown` is set (the interval bounds the
/// exit latency, so no handle needs joining).
pub fn spawn_poller(
    integration: Option<InboxIntegration>,
    metrics: Arc<PodseqMetrics>,
    shutdown: Arc<AtomicBool>,
    interval: Duration,
) {
    if let Some(integration) = integration {
        tokio::spawn(run_poller(integration, metrics, shutdown, interval));
    }
}

/// Poll loop: sync nonces from chain, persist markers, then submit pending
/// transactions to Reth's pool with a crash-replay receipt guard.
async fn run_poller(
    integration: InboxIntegration,
    metrics: Arc<PodseqMetrics>,
    shutdown: Arc<AtomicBool>,
    interval: Duration,
) {
    info!(interval = ?interval, "force-inclusion poller started");
    while !shutdown.load(Ordering::SeqCst) {
        if let Err(e) = poll_pass(&integration, &metrics).await {
            warn!(error = %e, "force-inclusion poll pass failed; retrying next interval");
        }
        tokio::time::sleep(interval).await;
    }
    info!("force-inclusion poller stopped");
}

/// One pass: sync from chain, then (re-)submit every pending transaction.
async fn poll_pass(integration: &InboxIntegration, metrics: &PodseqMetrics) -> anyhow::Result<()> {
    let tracker = integration.tracker();
    let at_height = u64::try_from(metrics.block_height.get().max(0)).unwrap_or(0);

    for submission in tracker.sync(at_height).await? {
        info!(
            nonce = submission.nonce,
            deadline_height = submission.deadline_height,
            hash = %submission.tx_hash,
            "forced transaction tracked from inbox"
        );
    }

    for submission in tracker.pending_submissions()? {
        // Crash-replay guard (mirrors the bridge relayer): a mined
        // transaction must sync its marker, never be re-sent to the pool.
        if let Some(height) = eth_get_transaction_receipt_height(
            &integration.http,
            &integration.l2_rpc_url,
            submission.tx_hash,
        )
        .await?
        {
            tracker.observe_inclusion(submission.nonce, height)?;
            info!(
                nonce = submission.nonce,
                height, "forced transaction already mined; inclusion marker synced"
            );
            continue;
        }
        eth_send_raw_transaction(
            &integration.http,
            &integration.l2_rpc_url,
            &submission.tx_bytes,
        )
        .await?;
        debug!(
            nonce = submission.nonce,
            hash = %submission.tx_hash,
            "forced transaction submitted to the L2 pool"
        );
    }
    Ok(())
}

/// Queries Reth for a transaction receipt; `Ok(Some(height))` when mined.
async fn eth_get_transaction_receipt_height(
    http: &reqwest::Client,
    url: &str,
    hash: B256,
) -> anyhow::Result<Option<u64>> {
    let response = http
        .post(url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getTransactionReceipt",
            "params": [format!("{hash:#x}")],
        }))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    let result = response
        .get("result")
        .ok_or_else(|| anyhow::anyhow!("receipt response missing result: {response}"))?;
    if result.is_null() {
        return Ok(None);
    }
    let number = result
        .get("blockNumber")
        .and_then(|v| v.as_str())
        .context("receipt missing blockNumber")?;
    Ok(Some(
        u64::from_str_radix(number.trim_start_matches("0x"), 16)
            .context("receipt blockNumber is not hex")?,
    ))
}

/// Settlement-side gate wait: refuses (returns `false` on shutdown, spins
/// otherwise) while the tracker reports an expired nonce for `height`.
///
/// A gate read error fails open: the settlement contract re-checks the
/// deadline on chain inside `settle`, so the sequencer-side gate is an early
/// refusal, not the enforcement point.
pub async fn wait_inclusion_gate(
    inbox: Option<&InboxIntegration>,
    height: u64,
    shutting_down: &Arc<AtomicBool>,
    backoff: Duration,
) -> bool {
    let Some(inbox) = inbox else {
        return true;
    };
    loop {
        if shutting_down.load(Ordering::SeqCst) {
            return false;
        }
        match inbox.tracker().halt_check(height) {
            Ok(HaltCheck::Clear) => return true,
            Ok(HaltCheck::Halt {
                nonce,
                deadline_height,
                ..
            }) => {
                warn!(
                    height,
                    nonce,
                    deadline_height,
                    "settlement paused: force-inclusion deadline exceeded without inclusion"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "force-inclusion gate unreadable; settling (contract re-checks the deadline)"
                );
                return true;
            }
        }
    }
}

/// Commits a block, passing the sequencer's inclusion cursor when the inbox
/// integration is configured; falls back to the plain on-chain cursor read
/// otherwise (and when the local cursor is unreadable). Thin dispatcher so
/// the settle path adds no branching.
pub async fn commit_with_inbox(
    sui: &podseq_sui::Client,
    inbox: Option<&InboxIntegration>,
    block: &podseq_core::Block,
    blob_id: &podseq_core::BlobId,
) -> Result<(), podseq_core::Error> {
    let Some(inbox) = inbox else {
        return sui.commit(block, blob_id).await;
    };
    match inbox.tracker().settle_cursor() {
        Ok(cursor) => sui.commit_with_cursor(block, blob_id, cursor).await,
        Err(e) => {
            warn!(
                error = %e,
                "inclusion cursor unreadable; settling with the on-chain cursor"
            );
            sui.commit(block, blob_id).await
        }
    }
}

/// Settles one block under the force-inclusion gate: refuses while a tracked
/// nonce is past its block deadline (stalling with backoff — the pause ends
/// the moment the overdue transaction is included), then commits with the
/// sequencer's inclusion cursor. Returns `false` only when shutdown
/// interrupted the wait or the commit.
pub async fn settle_with_gate(
    sui: &podseq_sui::Client,
    inbox: Option<&InboxIntegration>,
    block: &podseq_core::Block,
    blob_id: &podseq_core::BlobId,
    shutting_down: &Arc<AtomicBool>,
    metrics: &PodseqMetrics,
) -> bool {
    let height = block.header.height;
    if !wait_inclusion_gate(inbox, height, shutting_down, Duration::from_millis(500)).await {
        return false;
    }
    let committed = crate::runner::retry_with_backoff(
        shutting_down,
        Duration::from_millis(500),
        Duration::from_secs(10),
        || async {
            let started = std::time::Instant::now();
            match commit_with_inbox(sui, inbox, block, blob_id).await {
                Ok(()) => {
                    metrics
                        .settlement_duration
                        .observe(started.elapsed().as_secs_f64());
                    metrics.settlement_height.set(height as i64);
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            }
        },
    )
    .await;
    match committed {
        Some(()) => {
            info!(height, "block committed on Sui");
            true
        }
        None => false,
    }
}

/// Marks inclusions for a freshly built block. Standalone helper so
/// `produce_block` gains no branches.
pub fn observe_block_inclusions(inbox: Option<&InboxIntegration>, raw_txs: &[Bytes], height: u64) {
    let Some(inbox) = inbox else {
        return;
    };
    match inbox.tracker().observe_transactions(height, raw_txs) {
        Ok(marked) if !marked.is_empty() => {
            info!(height, nonces = ?marked, "forced transactions included in block");
        }
        Ok(_) => {}
        Err(e) => {
            warn!(height, error = %e, "failed to record forced-tx inclusions");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InboxStore;
    use std::collections::{BTreeMap, BTreeSet};

    const INBOX_ID: [u8; 32] = [0x11; 32];
    const TABLE_UID: [u8; 32] = [0x22; 32];

    /// BCS mirror of `Inbox` (see `move/sources/inbox.move`).
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct InboxMirror {
        id: Address,
        next_nonce: u64,
        included_count: u64,
        last_settled_ts: u64,
        pending: TableMirror,
    }
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct TableMirror {
        id: Address,
        size: u64,
    }
    /// BCS mirror of `sui::dynamic_field::Field<u64, inbox::PendingTx>`.
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct FieldMirror {
        id: Address,
        name: u64,
        value: PendingTxMirror,
    }
    #[derive(serde::Serialize)]
    #[allow(dead_code)]
    struct PendingTxMirror {
        deadline_ms: u64,
        tx_bytes: Vec<u8>,
    }

    fn inbox_bcs(next_nonce: u64, included_count: u64) -> Vec<u8> {
        let inbox = InboxMirror {
            id: Address::new([0; 32]),
            next_nonce,
            included_count,
            last_settled_ts: 1_000,
            pending: TableMirror {
                id: Address::new(TABLE_UID),
                size: next_nonce - included_count,
            },
        };
        bcs::to_bytes(&inbox).unwrap()
    }

    fn field_bcs(nonce: u64, deadline_ms: u64, tx_bytes: &[u8]) -> Vec<u8> {
        let field = FieldMirror {
            id: Address::new([0xbb; 32]),
            name: nonce,
            value: PendingTxMirror {
                deadline_ms,
                tx_bytes: tx_bytes.to_vec(),
            },
        };
        bcs::to_bytes(&field).unwrap()
    }

    /// Mock at the `get_object` transport boundary, same shape as the sui-crate
    /// mock: two maps, one per read shape. Ids in `fail_ids` error instead of
    /// serving contents, to exercise degraded window reads.
    #[derive(Default)]
    struct MockTransport {
        objects: BTreeMap<Address, Vec<u8>>,
        fields: BTreeMap<(Address, u64), Vec<u8>>,
        fail_ids: BTreeSet<Address>,
    }

    impl MockTransport {
        fn with_inbox(next_nonce: u64, included_count: u64, pending: &[(u64, &[u8])]) -> Self {
            let mut t = Self::default();
            t.objects.insert(
                Address::new(INBOX_ID),
                inbox_bcs(next_nonce, included_count),
            );
            let table = Address::new(TABLE_UID);
            for (nonce, tx) in pending {
                t.fields
                    .insert((table, *nonce), field_bcs(*nonce, 1_600_000 + nonce, tx));
            }
            t
        }
    }

    impl InboxTransport for MockTransport {
        fn object_contents<'a>(
            &'a self,
            id: &'a Address,
        ) -> podseq_sui::inbox::InboxFuture<'a, Result<Vec<u8>, podseq_sui::inbox::InboxError>>
        {
            Box::pin(async move {
                if self.fail_ids.contains(id) {
                    return Err(podseq_sui::inbox::InboxError::Rpc(format!(
                        "read down: {id}"
                    )));
                }
                self.objects.get(id).cloned().ok_or_else(|| {
                    podseq_sui::inbox::InboxError::NotFound(format!("no object {id}"))
                })
            })
        }

        fn field_contents<'a>(
            &'a self,
            table: &'a Address,
            key: u64,
        ) -> podseq_sui::inbox::InboxFuture<
            'a,
            Result<Option<Vec<u8>>, podseq_sui::inbox::InboxError>,
        > {
            Box::pin(async move { Ok(self.fields.get(&(*table, key)).cloned()) })
        }
    }

    fn tmp_dir() -> std::path::PathBuf {
        // Process id + counter instead of a clock: tests run in parallel in
        // one process, and same-nanosecond collisions would share a data dir.
        use std::sync::atomic::{AtomicU64, Ordering};
        static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "podseq-inbox-tracker-test-{}-{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct TestRig {
        tracker: InboxTracker,
        dir: std::path::PathBuf,
    }

    fn rig(
        next_nonce: u64,
        included_count: u64,
        pending: &[(u64, &[u8])],
        deadline_blocks: u64,
    ) -> TestRig {
        let dir = tmp_dir();
        let transport = Arc::new(MockTransport::with_inbox(
            next_nonce,
            included_count,
            pending,
        ));
        let tracker = InboxTracker::new(
            Arc::clone(&transport) as Arc<dyn InboxTransport>,
            Address::new(INBOX_ID),
            InboxStore::new(&dir),
            deadline_blocks,
            1_000,
            None,
        );
        TestRig { tracker, dir }
    }

    impl Drop for TestRig {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn bytes_of(b: &[u8]) -> Bytes {
        Bytes::from(b.to_vec())
    }

    #[tokio::test]
    async fn sync_reads_next_nonce_and_records_submissions_before_pool() {
        let r = rig(2, 0, &[(0, b"tx-a"), (1, b"tx-b")], 3);

        let fresh = r.tracker.sync(10).await.unwrap();
        assert_eq!(fresh.len(), 2);
        assert_eq!(fresh[0].nonce, 0);
        assert_eq!(fresh[0].tx_bytes, b"tx-a".to_vec());
        assert_eq!(fresh[0].tx_hash, keccak256(b"tx-a"));
        // Deadline anchored at the observation height plus the block budget.
        assert_eq!(fresh[0].deadline_height, 13);
        assert_eq!(fresh[0].deadline_ms, 1_600_000);
        assert_eq!(fresh[1].nonce, 1);
        assert_eq!(fresh[1].deadline_height, 13);

        // Markers persisted before the caller submits to the pool.
        assert!(matches!(r.tracker.pending_submissions().unwrap().len(), 2));
    }

    #[tokio::test]
    async fn sync_skips_nonces_below_the_onchain_cursor() {
        let r = rig(3, 2, &[(0, b"tx-a"), (1, b"tx-b"), (2, b"tx-c")], 3);

        let fresh = r.tracker.sync(5).await.unwrap();
        // Nonces 0 and 1 are already covered by the on-chain settle cursor:
        // only nonce 2 is sequencer business.
        assert_eq!(fresh.iter().map(|s| s.nonce).collect::<Vec<_>>(), vec![2]);
    }

    #[tokio::test]
    async fn sync_tolerates_entries_missing_between_reads() {
        // Nonce 1 was pruned (covered) between the inbox read and the field
        // read: skip it, track the rest, never error.
        let r = rig(2, 0, &[(0, b"tx-a")], 3);
        // (nonce 1 has no entry in the mock's field map)

        let fresh = r.tracker.sync(4).await.unwrap();
        assert_eq!(fresh.iter().map(|s| s.nonce).collect::<Vec<_>>(), vec![0]);
        assert!(!r.tracker.pending_submissions().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sync_is_idempotent_across_passes() {
        let r = rig(1, 0, &[(0, b"tx-a")], 3);

        let first = r.tracker.sync(1).await.unwrap();
        assert_eq!(first.len(), 1);
        let second = r.tracker.sync(2).await.unwrap();
        assert!(second.is_empty());
        assert_eq!(r.tracker.pending_submissions().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn observe_transactions_marks_the_block_that_contains_the_tx() {
        let r = rig(2, 0, &[(0, b"tx-a"), (1, b"tx-b")], 3);
        r.tracker.sync(10).await.unwrap();

        let block_txs = vec![bytes_of(b"unrelated"), bytes_of(b"tx-a")];
        let marked = r.tracker.observe_transactions(11, &block_txs).unwrap();
        assert_eq!(marked, vec![0]);
        assert_eq!(
            r.tracker.observe_transactions(11, &block_txs).unwrap(),
            Vec::<u64>::new(),
            "replaying the same block must not re-mark"
        );
    }

    #[tokio::test]
    async fn observe_transactions_rejects_double_inclusion_at_different_height() {
        let r = rig(1, 0, &[(0, b"tx-a")], 3);
        r.tracker.sync(10).await.unwrap();

        let block_txs = vec![bytes_of(b"tx-a")];
        r.tracker.observe_transactions(11, &block_txs).unwrap();
        // The same forced transaction appearing again at height 12 means it
        // landed in two blocks: refuse to record it.
        assert!(r.tracker.observe_transactions(12, &block_txs).is_err());
    }

    #[tokio::test]
    async fn pending_submissions_excludes_included_after_reload() {
        // Crash simulation: sync, include nonce 0, then rebuild the tracker
        // from the same data dir. The re-submission set must not contain the
        // included nonce, so a crash-recovery re-pass cannot double-include.
        let dir = tmp_dir();
        let transport = Arc::new(MockTransport::with_inbox(
            2,
            0,
            &[(0, b"tx-a"), (1, b"tx-b")],
        ));
        let store = InboxStore::new(&dir);
        {
            let tracker = InboxTracker::new(
                Arc::clone(&transport) as Arc<dyn InboxTransport>,
                Address::new(INBOX_ID),
                store,
                3,
                1_000,
                None,
            );
            tracker.sync(10).await.unwrap();
            tracker
                .observe_transactions(11, &[bytes_of(b"tx-a")])
                .unwrap();
        }
        let recovered = InboxTracker::new(
            Arc::clone(&transport) as Arc<dyn InboxTransport>,
            Address::new(INBOX_ID),
            InboxStore::new(&dir),
            3,
            1_000,
            None,
        );
        let pending: Vec<u64> = recovered
            .pending_submissions()
            .unwrap()
            .into_iter()
            .map(|s| s.nonce)
            .collect();
        assert_eq!(pending, vec![1]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn crash_between_mark_included_and_settle_preserves_cursor_and_gate() {
        // The K2 crash window: produce_block marked nonce 0 included at
        // height 11 (persist-before-settle), then the process died before the
        // settle call landed. Recovery rebuilds the tracker from the same
        // data dir and must (a) not lose the inclusion — the settle cursor
        // still counts it, so the re-settled block covers the nonce on chain,
        // (b) not double-include — the marked nonce never re-enters the pool
        // submission set, and (c) leave the settlement gate clear, so the
        // recovery path proceeds instead of halting on its own marker.
        let dir = tmp_dir();
        let transport = Arc::new(MockTransport::with_inbox(
            2,
            0,
            &[(0, b"tx-a"), (1, b"tx-b")],
        ));
        {
            let tracker = InboxTracker::new(
                Arc::clone(&transport) as Arc<dyn InboxTransport>,
                Address::new(INBOX_ID),
                InboxStore::new(&dir),
                3,
                1_000,
                None,
            );
            tracker.sync(10).await.unwrap();
            tracker
                .observe_transactions(11, &[bytes_of(b"tx-a")])
                .unwrap();
            // (crash: the settle call for height 11 never ran)
        }
        let recovered = InboxTracker::new(
            Arc::clone(&transport) as Arc<dyn InboxTransport>,
            Address::new(INBOX_ID),
            InboxStore::new(&dir),
            3,
            1_000,
            None,
        );
        assert_eq!(recovered.settle_cursor().unwrap(), 1);
        assert_eq!(
            recovered.halt_check(11).unwrap(),
            HaltCheck::Clear,
            "the recovered inclusion must not stall re-settlement"
        );
        let resubmit: Vec<u64> = recovered
            .pending_submissions()
            .unwrap()
            .into_iter()
            .map(|s| s.nonce)
            .collect();
        assert_eq!(resubmit, vec![1]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn settle_cursor_counts_included_nonces() {
        let r = rig(3, 0, &[(0, b"a"), (1, b"b"), (2, b"c")], 3);
        r.tracker.sync(1).await.unwrap();
        assert_eq!(r.tracker.settle_cursor().unwrap(), 0);

        r.tracker
            .observe_transactions(2, &[bytes_of(b"a"), bytes_of(b"b")])
            .unwrap();
        assert_eq!(r.tracker.settle_cursor().unwrap(), 2);
    }

    #[tokio::test]
    async fn halt_gate_refuses_only_past_the_deadline() {
        let r = rig(1, 0, &[(0, b"tx-a")], 3);
        r.tracker.sync(10).await.unwrap(); // deadline_height = 13

        assert_eq!(r.tracker.halt_check(11).unwrap(), HaltCheck::Clear);
        assert_eq!(r.tracker.halt_check(13).unwrap(), HaltCheck::Clear);
        assert_eq!(
            r.tracker.halt_check(14).unwrap(),
            HaltCheck::Halt {
                nonce: 0,
                deadline_height: 13,
                next_height: 14,
            }
        );
    }

    #[tokio::test]
    async fn halt_gate_clears_once_the_overdue_transaction_is_included() {
        // Late inclusion self-heals, mirroring the Move-level property: the
        // sequencer can always end the settlement pause by including the tx.
        let r = rig(1, 0, &[(0, b"tx-a")], 3);
        r.tracker.sync(10).await.unwrap();

        assert!(matches!(
            r.tracker.halt_check(20).unwrap(),
            HaltCheck::Halt { .. }
        ));
        r.tracker
            .observe_transactions(20, &[bytes_of(b"tx-a")])
            .unwrap();
        assert_eq!(r.tracker.halt_check(21).unwrap(), HaltCheck::Clear);
    }

    #[tokio::test]
    async fn halt_gate_ignores_included_transactions() {
        let r = rig(1, 0, &[(0, b"tx-a")], 3);
        r.tracker.sync(10).await.unwrap();
        r.tracker
            .observe_transactions(11, &[bytes_of(b"tx-a")])
            .unwrap();
        assert_eq!(r.tracker.halt_check(500).unwrap(), HaltCheck::Clear);
    }

    #[tokio::test]
    async fn observe_inclusion_marks_from_a_receipt() {
        // Crash-recovery path: a receipt guard finds the tx mined, the marker
        // lands without a pool re-send.
        let r = rig(1, 0, &[(0, b"tx-a")], 3);
        r.tracker.sync(10).await.unwrap();

        r.tracker.observe_inclusion(0, 12).unwrap();
        assert_eq!(r.tracker.settle_cursor().unwrap(), 1);
        r.tracker.observe_inclusion(0, 12).unwrap(); // idempotent replay
        assert_eq!(r.tracker.settle_cursor().unwrap(), 1);
    }

    #[test]
    fn deadline_blocks_derived_from_window_and_block_time() {
        // 10-minute window at the default 2 s block time, per the Move docs.
        assert_eq!(deadline_blocks_for(600_000, 2_000), 300);
        assert_eq!(deadline_blocks_for(600_000, 1_000), 600);
        // ceil, not floor: a 3 s block time must not under-budget the window.
        assert_eq!(deadline_blocks_for(600_000, 3_000), 200);
        // A zero block time degenerates to one-block budgeting, not a panic.
        assert_eq!(deadline_blocks_for(600_000, 0), 600_000);
    }

    // ---------- chain-derived window budget ----------

    /// Stand-in deployed package id for window dynamic-field derivation.
    const PACKAGE_ID: [u8; 32] = [0x77; 32];

    /// Mirror of the shared `Clock` object BCS: id + timestamp_ms.
    fn clock_bytes(now_ms: u64) -> Vec<u8> {
        let mut bytes = vec![0u8; 32];
        bytes.extend_from_slice(&now_ms.to_le_bytes());
        bytes
    }

    /// Mirror of `Field<inbox::WindowKey, u64>`: id + value.
    fn window_field_bytes(value: u64) -> Vec<u8> {
        let mut bytes = vec![0xee; 32];
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes
    }

    /// Mirror of `Field<inbox::PendingWindowKey, inbox::PendingWindow>`:
    /// id + new_window_ms + effective_at_ms.
    fn pending_window_field_bytes(new_window_ms: u64, effective_at_ms: u64) -> Vec<u8> {
        let mut bytes = vec![0xef; 32];
        bytes.extend_from_slice(&new_window_ms.to_le_bytes());
        bytes.extend_from_slice(&effective_at_ms.to_le_bytes());
        bytes
    }

    /// A rig whose tracker derives its block budget from the chain: the
    /// default window (600_000 ms) is the seed at a 1_000 ms block time, so
    /// the un-refreshed budget is 600 blocks.
    fn rig_window(mut mock: MockTransport) -> TestRig {
        let dir = tmp_dir();
        mock.objects
            .insert(podseq_sui::deadline::SUI_CLOCK_ID, clock_bytes(1_000_000));
        let transport = Arc::new(mock);
        let tracker = InboxTracker::new(
            Arc::clone(&transport) as Arc<dyn InboxTransport>,
            Address::new(INBOX_ID),
            InboxStore::new(&dir),
            deadline_blocks_for(600_000, 1_000),
            1_000,
            Some(Address::new(PACKAGE_ID)),
        );
        TestRig { tracker, dir }
    }

    #[tokio::test]
    async fn sync_derives_the_block_budget_from_the_chain_window() {
        let inbox_id = Address::new(INBOX_ID);
        let package = Address::new(PACKAGE_ID);
        let mut mock = MockTransport::with_inbox(1, 0, &[(0, b"tx-a")]);
        mock.objects.insert(
            podseq_sui::inbox::window_field_id(inbox_id, package),
            window_field_bytes(120_000),
        );
        let r = rig_window(mock);

        // Seeded at 600 blocks; the narrowed on-chain window applies from the
        // first sync: fresh submissions anchor at height + 120.
        let fresh = r.tracker.sync(10).await.unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].deadline_height, 130);
    }

    #[tokio::test]
    async fn sync_keeps_the_seeded_budget_without_window_state() {
        let r = rig_window(MockTransport::with_inbox(1, 0, &[(0, b"tx-a")]));

        // No override, no staging: the default window applies (600 blocks at
        // a 1_000 ms block time).
        let fresh = r.tracker.sync(10).await.unwrap();
        assert_eq!(fresh[0].deadline_height, 610);
    }

    #[tokio::test]
    async fn sync_applies_an_elapsed_staged_widening() {
        let inbox_id = Address::new(INBOX_ID);
        let package = Address::new(PACKAGE_ID);
        let mut mock = MockTransport::with_inbox(1, 0, &[(0, b"tx-a")]);
        mock.objects.insert(
            podseq_sui::inbox::pending_window_field_id(inbox_id, package),
            pending_window_field_bytes(1_800_000, 900_000),
        );
        let r = rig_window(mock);

        // The staging elapsed (clock 1_000_000 >= effective 900_000): the
        // widened window applies without any transaction.
        let fresh = r.tracker.sync(10).await.unwrap();
        assert_eq!(fresh[0].deadline_height, 1_810);
    }

    #[tokio::test]
    async fn sync_ignores_an_unelapsed_staged_widening() {
        let inbox_id = Address::new(INBOX_ID);
        let package = Address::new(PACKAGE_ID);
        let mut mock = MockTransport::with_inbox(1, 0, &[(0, b"tx-a")]);
        mock.objects.insert(
            podseq_sui::inbox::pending_window_field_id(inbox_id, package),
            pending_window_field_bytes(1_800_000, 2_000_000),
        );
        let r = rig_window(mock);

        // Staged but not yet effective: the default still applies.
        let fresh = r.tracker.sync(10).await.unwrap();
        assert_eq!(fresh[0].deadline_height, 610);
    }

    #[tokio::test]
    async fn sync_keeps_the_previous_budget_when_the_window_read_fails() {
        let inbox_id = Address::new(INBOX_ID);
        let package = Address::new(PACKAGE_ID);
        let mut mock = MockTransport::with_inbox(1, 0, &[(0, b"tx-a")]);
        mock.objects.insert(
            podseq_sui::inbox::window_field_id(inbox_id, package),
            window_field_bytes(120_000),
        );
        mock.fail_ids
            .insert(podseq_sui::inbox::window_field_id(inbox_id, package));
        let r = rig_window(mock);

        // The window read is degraded, the inbox read is not: sync proceeds,
        // submissions keep the seeded budget, no pause.
        let fresh = r.tracker.sync(10).await.unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].deadline_height, 610);
    }

    #[test]
    fn from_config_is_none_without_an_inbox() {
        let dir = tmp_dir();
        let sui = SuiConfig::default();
        let integration =
            InboxIntegration::from_config(&sui, "http://localhost:8545", &dir, 2_000).unwrap();
        assert!(integration.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_config_wires_the_tracker_with_the_window_package() {
        let dir = tmp_dir();
        let sui = SuiConfig {
            inbox_id: Some(format!("0x{}", hex_str(&[0x11; 32]))),
            settlement_package_id: Some(format!("0x{}", hex_str(&[0x77; 32]))),
            ..SuiConfig::default()
        };
        let integration =
            InboxIntegration::from_config(&sui, "http://localhost:8545", &dir, 1_000).unwrap();
        assert!(
            integration.is_some(),
            "an inbox id assembles the integration"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_config_tolerates_a_missing_package_id() {
        // Without a package id the window stays at the seeded default; the
        // integration still assembles.
        let dir = tmp_dir();
        let sui = SuiConfig {
            inbox_id: Some(format!("0x{}", hex_str(&[0x11; 32]))),
            settlement_package_id: None,
            ..SuiConfig::default()
        };
        let integration =
            InboxIntegration::from_config(&sui, "http://localhost:8545", &dir, 1_000).unwrap();
        assert!(integration.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_config_rejects_a_malformed_package_id() {
        let dir = tmp_dir();
        let sui = SuiConfig {
            inbox_id: Some(format!("0x{}", hex_str(&[0x11; 32]))),
            settlement_package_id: Some("not-an-address".into()),
            ..SuiConfig::default()
        };
        assert!(matches!(
            InboxIntegration::from_config(&sui, "http://localhost:8545", &dir, 1_000),
            Err(TrackerError::Corrupt(msg)) if msg.contains("settlement_package_id")
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_config_rejects_a_malformed_inbox_id() {
        let dir = tmp_dir();
        let sui = SuiConfig {
            inbox_id: Some("also-not-an-address".into()),
            settlement_package_id: Some(format!("0x{}", hex_str(&[0x77; 32]))),
            ..SuiConfig::default()
        };
        assert!(matches!(
            InboxIntegration::from_config(&sui, "http://localhost:8545", &dir, 1_000),
            Err(TrackerError::Corrupt(msg)) if msg.contains("inbox_id")
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Lowercase hex without a prefix (Address's Display form).
    fn hex_str(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
