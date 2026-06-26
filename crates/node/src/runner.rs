//! Sequencer block production loop.
//!
//! Production and finalization run concurrently: a per-tick producer never
//! blocks on DA, while a background finalizer publishes to Walrus + settles on Sui.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use anyhow::{Context, Result};
use commonware_cryptography::Digestible as _;
use podseq_core::{Block, BlockSigner, DataAvailability, Settlement};
use podseq_engine::{payload_into_block, Engine, MempoolClient, PARENT_BEACON_BLOCK_ROOT};
use podseq_p2p::BlockBroadcaster;
use podseq_sequencer::SingleSequencer;
use podseq_store::{BlockStore, ChainState as StoredState, PendingStore, StateStore};
use podseq_sui::Client as SuiClient;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::config::SequencerConfig;

struct PendingBlock {
    block: Block,
    block_hash: B256,
    height: u64,
}

struct ChainState {
    head: B256,
    safe: B256,
    finalized: B256,
    timestamp: u64,
}

/// Runs the sequencer block production loop until shutdown.
pub struct Runner {
    engine: Engine,
    mempool: MempoolClient,
    sequencer: Arc<Mutex<SingleSequencer>>,
    sui: SuiClient,
    block_signer: Arc<dyn BlockSigner>,
    block_store: Arc<BlockStore>,
    state_store: Arc<StateStore>,
    pending_store: Arc<PendingStore>,
    broadcaster: Option<BlockBroadcaster>,
    block_time: Duration,
    fee_recipient: Address,
    genesis_hash: Option<B256>,
    da_batch_size: usize,
}

impl Runner {
    /// Builds a runner from injected clients and sequencer config.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Engine,
        mempool: MempoolClient,
        sui: SuiClient,
        block_signer: Arc<dyn BlockSigner>,
        config: &SequencerConfig,
        genesis_hash: Option<B256>,
        data_dir: &std::path::Path,
        broadcaster: Option<BlockBroadcaster>,
        da_batch_size: usize,
    ) -> Self {
        podseq_store::init(data_dir).expect("failed to init storage");
        Self {
            engine,
            mempool,
            sequencer: Arc::new(Mutex::new(SingleSequencer::new())),
            sui,
            block_signer,
            block_store: Arc::new(BlockStore::new(data_dir)),
            state_store: Arc::new(StateStore::new(data_dir)),
            pending_store: Arc::new(PendingStore::new(data_dir)),
            broadcaster,
            block_time: Duration::from_millis(config.block_time_ms.max(100)),
            fee_recipient: config.fee_recipient.parse().unwrap_or_default(),
            genesis_hash,
            da_batch_size,
        }
    }

    /// Runs the production loop and a background finalizer until shutdown.
    pub async fn run(self) -> Result<()> {
        let engine = Arc::new(self.engine);

        // Try to load persisted state first (crash recovery).
        let (head, safe, finalized, timestamp) =
            if let Some(stored) = self.state_store.load().context("loading stored state")? {
                info!(height = stored.height, "recovered chain state from storage");
                let h: B256 = stored.head.parse().unwrap_or(B256::ZERO);
                let s: B256 = stored.safe.parse().unwrap_or(h);
                let f: B256 = stored.finalized.parse().unwrap_or(h);
                (h, s, f, stored.timestamp)
            } else {
                match self.genesis_hash {
                    Some(hash) => {
                        info!(?hash, "using configured genesis hash");
                        (hash, hash, hash, now_unix())
                    }
                    None => {
                        info!("discovering chain head from Reth");
                        let height = engine
                            .block_number()
                            .await
                            .context("querying block number from Reth")?;
                        let hash = engine
                            .block_by_number(height)
                            .await
                            .context("querying chain head from Reth")?
                            .with_context(|| {
                                format!("block {height} not found; is Reth initialized?")
                            })?;
                        info!(height, ?hash, "discovered chain head");
                        (hash, hash, hash, now_unix())
                    }
                }
            };

        let state = Arc::new(Mutex::new(ChainState {
            head,
            safe,
            finalized,
            timestamp,
        }));

        // Unbounded so production never stalls on the finalizer; blocks are
        // already persisted, so a backlog only delays DA.
        let (tx, rx) = mpsc::unbounded_channel::<PendingBlock>();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let mut finalizer_handle = tokio::spawn(finalize_blocks(
            engine.clone(),
            self.sui,
            state.clone(),
            rx,
            self.block_store.clone(),
            self.state_store.clone(),
            self.pending_store.clone(),
            shutting_down.clone(),
            self.da_batch_size,
        ));

        // Crash recovery: re-submit persisted blocks that never reached DA.
        recover_pending(&self.pending_store, &self.block_store, &tx);

        info!(
            block_time_ms = self.block_time.as_millis(),
            ?head,
            "sequencer loop started"
        );

        let fee_recipient = self.fee_recipient;
        let block_signer = self.block_signer;
        let block_store = self.block_store;
        let state_store = self.state_store;
        let pending_store = self.pending_store;
        let broadcaster = self.broadcaster;
        let mempool = self.mempool;
        let sequencer = self.sequencer;
        let mut ticker = tokio::time::interval(self.block_time);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if let Err(e) = refresh_mempool(&mempool, &sequencer).await {
                        warn!(error = %e, "failed to refresh mempool (block building continues)");
                    }
                    if let Err(e) = produce_block(
                        &engine, &state, fee_recipient, &block_signer, &tx,
                        &block_store, &state_store, &pending_store,
                        &broadcaster, &sequencer,
                    ).await {
                        error!(error = ?e, "block production failed");
                    }
                }
                _ = shutdown_signal() => {
                    info!("received shutdown signal, stopping sequencer");
                    shutting_down.store(true, Ordering::SeqCst);
                    break;
                }
            }
        }

        // Bound the drain so a DA/Sui outage (retrying with backoff) can't
        // stall exit; leftover pending blocks are recovered on restart.
        drop(tx);
        const SHUTDOWN_DRAIN: Duration = Duration::from_secs(30);
        match tokio::time::timeout(SHUTDOWN_DRAIN, &mut finalizer_handle).await {
            Ok(Ok(())) => info!("finalizer stopped"),
            Ok(Err(e)) => error!(error = %e, "finalizer task panicked"),
            Err(_) => {
                warn!(
                    drain = ?SHUTDOWN_DRAIN,
                    "finalizer did not finish in time; aborting (pending blocks recovered on restart)"
                );
                finalizer_handle.abort();
            }
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn produce_block(
    engine: &Engine,
    state: &Mutex<ChainState>,
    fee_recipient: Address,
    block_signer: &Arc<dyn BlockSigner>,
    tx: &mpsc::UnboundedSender<PendingBlock>,
    block_store: &BlockStore,
    state_store: &StateStore,
    pending_store: &PendingStore,
    broadcaster: &Option<BlockBroadcaster>,
    sequencer: &Mutex<SingleSequencer>,
) -> Result<()> {
    let s = state.lock().await;
    let head = s.head;
    let safe = s.safe;
    let finalized = s.finalized;
    let timestamp = s.timestamp.max(now_unix()) + 1;
    drop(s);

    // podseq is the consensus client, so there is no beacon chain to supply a
    // real RANDAO. Derive a deterministic, per-block value from the next
    // height (the convention for single-sequencer chains).
    let next_height = block_store.latest_height().unwrap_or(0) + 1;
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: derive_prev_randao(next_height),
        suggested_fee_recipient: fee_recipient,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(PARENT_BEACON_BLOCK_ROOT),
        ..Default::default()
    };
    let fc_state = ForkchoiceState {
        head_block_hash: head,
        safe_block_hash: safe,
        finalized_block_hash: finalized,
    };

    let built = engine.build(fc_state, attributes).await?;

    let block_hash = built.block_hash;
    let height = built.height;
    info!(payload_id = ?built.payload_id, height, ?block_hash, "block built");

    engine
        .accept(&built.payload, block_hash, safe, finalized)
        .await
        .context("accepting payload")?;

    {
        let mut s = state.lock().await;
        s.head = block_hash;
        s.timestamp = timestamp;
    }
    info!(height, ?block_hash, "head advanced");

    // Reth executes via engine.build(); the drained batch is recorded for
    // auditability and future ordering control.
    {
        let mut seq = sequencer.lock().await;
        let batch = seq.drain();
        if !batch.transactions.is_empty() {
            info!(
                height,
                txs = batch.transactions.len(),
                "batch drained from mempool"
            );
        }
    }

    let mut block = payload_into_block(&built).context("converting payload to block")?;
    block.signature = Some(block_signer.sign_header(&block.header)?);

    // Persist + mark pending so a crash before settlement triggers re-submission.
    block_store.put(&block).context("persisting block")?;
    pending_store
        .mark(height)
        .with_context(|| format!("marking height {height} pending"))?;
    persist_state(
        state_store,
        state,
        block_store.latest_height().unwrap_or(height),
    )
    .await?;

    // Broadcast so full nodes receive it without waiting for DA settlement.
    if let Some(bc) = broadcaster {
        let msg = podseq_p2p::BlockMessage::from(block.clone());
        let digest = msg.digest();
        bc.broadcast(block.clone());
        bc.announce(height, &digest);
    }

    // Never blocks; a closed finalizer channel drops the block (recovered on restart).
    if let Err(e) = tx.send(PendingBlock {
        block,
        block_hash,
        height,
    }) {
        error!(error = %e, "finalizer channel closed; dropping block (recovered on restart)");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn finalize_blocks(
    engine: Arc<Engine>,
    sui: SuiClient,
    state: Arc<Mutex<ChainState>>,
    mut rx: mpsc::UnboundedReceiver<PendingBlock>,
    block_store: Arc<BlockStore>,
    state_store: Arc<StateStore>,
    pending_store: Arc<PendingStore>,
    shutting_down: Arc<AtomicBool>,
    batch_size_bytes: usize,
) {
    let mut batch: Vec<PendingBlock> = Vec::new();
    let mut batch_bytes: usize = 0;

    loop {
        tokio::select! {
            biased;
            _ = shutdown_guard(&shutting_down), if !batch.is_empty() => {
                finalize_buffered_locally(&engine, &state, std::mem::take(&mut batch)).await;
                break;
            }
            msg = rx.recv() => match msg {
                Some(p) => {
                    batch_bytes = batch_bytes.saturating_add(p.block.data.len());
                    batch.push(p);
                    if batch_bytes >= batch_size_bytes {
                        flush_batch(
                            &engine, &sui, &state, &mut batch, &mut batch_bytes,
                            &block_store, &state_store, &pending_store, &shutting_down,
                        ).await;
                    }
                }
                None => {
                    if !batch.is_empty() {
                        flush_batch(
                            &engine, &sui, &state, &mut batch, &mut batch_bytes,
                            &block_store, &state_store, &pending_store, &shutting_down,
                        ).await;
                    }
                    if !batch.is_empty() {
                        finalize_buffered_locally(&engine, &state, std::mem::take(&mut batch)).await;
                    }
                    break;
                }
            }
        }
    }

    info!("finalizer stopped");
}

/// Polled only via `tokio::select!`.
async fn shutdown_guard(shutting_down: &Arc<AtomicBool>) {
    while !shutting_down.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// On shutdown mid-publish, the batch is returned unflushed (left in `batch`)
/// for the caller to finalize locally.
#[allow(clippy::too_many_arguments)]
async fn flush_batch(
    engine: &Arc<Engine>,
    sui: &SuiClient,
    state: &Mutex<ChainState>,
    batch: &mut Vec<PendingBlock>,
    batch_bytes: &mut usize,
    block_store: &BlockStore,
    state_store: &StateStore,
    pending_store: &Arc<PendingStore>,
    shutting_down: &Arc<AtomicBool>,
) {
    if batch.is_empty() {
        return;
    }
    let blocks: Vec<Block> = batch.iter().map(|p| p.block.clone()).collect();
    let mut backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(10);

    let blob_id = loop {
        if shutting_down.load(Ordering::SeqCst) {
            return;
        }
        match sui.publish(&blocks).await {
            Ok(id) => break id,
            Err(e) => {
                error!(
                    blocks = batch.len(),
                    error = %e,
                    "DA batch publish failed; retrying in {:?}",
                    backoff
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    };
    info!(
        blocks = batch.len(),
        blob_id = %podseq_sui::blob_id::encode(&blob_id),
        "batch published to Walrus"
    );

    // Settle each block. Settlement is sequential (the contract enforces
    // latest_height + 1), so a block must commit before the next. A Sui outage
    // stalls here; production keeps running and blocks buffer in the channel
    // + on disk. Retry with backoff until success or shutdown.
    while !batch.is_empty() {
        let front = &batch[0];
        let settled = settle_block(sui, &front.block, &blob_id, shutting_down).await;
        if !settled {
            // Shutdown interrupted settlement: finalize the rest locally and
            // leave pending markers so they are re-settled on restart.
            finalize_buffered_locally(engine, state, std::mem::take(batch)).await;
            *batch_bytes = 0;
            return;
        }
        let p = batch.remove(0);
        let height = p.height;

        advance_finalized(engine, state, height, p.block_hash).await;

        if let Err(e) = pending_store.clear(height) {
            warn!(height, error = %e, "failed to clear pending marker");
        }

        persist_state(
            state_store,
            state,
            block_store.latest_height().unwrap_or(height),
        )
        .await
        .ok();
        info!(height, ?p.block_hash, "block finalized");
    }
    *batch_bytes = 0;
}

/// Retries settlement for a single block until it succeeds or shutdown.
/// Returns `true` if committed on Sui, `false` if shutdown interrupted it
/// (the pending marker is left intact so the block is re-settled on restart).
async fn settle_block(
    sui: &SuiClient,
    block: &Block,
    blob_id: &podseq_core::BlobId,
    shutting_down: &Arc<AtomicBool>,
) -> bool {
    let height = block.header.height;
    let committed = retry_with_backoff(
        shutting_down,
        Duration::from_millis(500),
        Duration::from_secs(10),
        || async { sui.commit(block, blob_id).await.map_err(|e| e.to_string()) },
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

/// Retries an operation with exponential backoff until it succeeds or shutdown
/// is requested. Returns `Some(())` on success, `None` if shutdown interrupted.
///
/// On each failure the backoff doubles, capped at `max_backoff`. Shutdown is
/// checked before each attempt, so a shutdown during a long backoff still exits
/// promptly on the next iteration.
async fn retry_with_backoff<F, Fut>(
    shutting_down: &Arc<AtomicBool>,
    initial: Duration,
    max_backoff: Duration,
    mut op: F,
) -> Option<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut backoff = initial;
    loop {
        if shutting_down.load(Ordering::SeqCst) {
            return None;
        }
        match op().await {
            Ok(()) => return Some(()),
            Err(e) => {
                warn!(error = %e, "operation failed; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

/// Pending markers are left intact so blocks are re-uploaded on the next startup.
async fn finalize_buffered_locally(
    engine: &Arc<Engine>,
    state: &Mutex<ChainState>,
    batch: Vec<PendingBlock>,
) {
    for p in batch {
        advance_finalized(engine, state, p.height, p.block_hash).await;
        info!(
            height = p.height,
            ?p.block_hash,
            "finalized locally at shutdown (re-uploaded on restart)"
        );
    }
}

/// A missing block on disk is unrecoverable: its pending marker is dropped so
/// it doesn't block recovery of later heights.
fn recover_pending(
    pending_store: &PendingStore,
    block_store: &BlockStore,
    tx: &mpsc::UnboundedSender<PendingBlock>,
) {
    let pending = match pending_store.pending() {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "failed to load pending heights; skipping recovery");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }
    info!(
        count = pending.len(),
        "recovering unsettled blocks from previous run"
    );

    for height in pending {
        match block_store.get(height) {
            Ok(block) => {
                // The execution block hash lives inside the persisted payload
                // (block.data), not on Header. Decode it the same way the full
                // node does so engine.finalize gets the correct hash.
                let block_hash = match decode_block_hash(&block.data) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!(height, error = %e, "recovery: block payload undecodable; dropping pending marker");
                        if let Err(clear_err) = pending_store.clear(height) {
                            warn!(height, error = %clear_err, "recovery: failed to drop pending marker");
                        }
                        continue;
                    }
                };
                if let Err(e) = tx.send(PendingBlock {
                    block,
                    block_hash,
                    height,
                }) {
                    error!(height, error = %e, "recovery: failed to re-queue block");
                } else {
                    info!(height, ?block_hash, "recovery: re-queued unsettled block");
                }
            }
            Err(e) => {
                warn!(height, error = %e, "recovery: block missing on disk; dropping pending marker");
                if let Err(clear_err) = pending_store.clear(height) {
                    warn!(height, error = %clear_err, "recovery: failed to drop pending marker");
                }
            }
        }
    }
}

async fn refresh_mempool(
    mempool: &MempoolClient,
    sequencer: &Mutex<SingleSequencer>,
) -> Result<()> {
    let hashes = mempool.pending_transactions().await?;
    if !hashes.is_empty() {
        debug!(count = hashes.len(), "mempool has pending transactions");
        let mut seq = sequencer.lock().await;
        for hash in &hashes {
            seq.submit(hash.to_vec());
        }
    }
    Ok(())
}

fn decode_block_hash(data: &[u8]) -> Result<B256> {
    let payload: alloy_rpc_types_engine::ExecutionPayloadV3 =
        serde_json::from_slice(data).context("decoding execution payload for block hash")?;
    Ok(payload.payload_inner.payload_inner.block_hash)
}

/// `height` is the latest produced height, recording how far the sequencer got.
async fn persist_state(store: &StateStore, state: &Mutex<ChainState>, height: u64) -> Result<()> {
    let s = state.lock().await;
    let stored = StoredState {
        head: format!("{:#x}", s.head),
        safe: format!("{:#x}", s.safe),
        finalized: format!("{:#x}", s.finalized),
        height,
        timestamp: s.timestamp,
    };
    drop(s);
    store.save(&stored).context("saving chain state")?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn derive_prev_randao(height: u64) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[24..].copy_from_slice(&height.to_be_bytes());
    B256::from(bytes)
}

/// Shared by the normal finalize path and shutdown.
async fn advance_finalized(
    engine: &Engine,
    state: &Mutex<ChainState>,
    height: u64,
    block_hash: B256,
) {
    let head = state.lock().await.head;
    if let Err(e) = engine.finalize(head, block_hash, block_hash).await {
        error!(height, error = %e, "failed to advance finalized");
    }
    let mut s = state.lock().await;
    s.safe = block_hash;
    s.finalized = block_hash;
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
    use podseq_core::Header;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-runner-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn block_with_hash(height: u64, hash: B256) -> podseq_core::Block {
        let payload = ExecutionPayloadV3 {
            payload_inner: ExecutionPayloadV2 {
                payload_inner: ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Default::default(),
                    prev_randao: B256::ZERO,
                    block_number: height,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: height,
                    extra_data: Default::default(),
                    base_fee_per_gas: alloy_primitives::U256::from(7),
                    block_hash: hash,
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        };
        let data = serde_json::to_vec(&payload).unwrap();
        podseq_core::Block {
            header: Header {
                height,
                parent_hash: [0; 32],
                state_root: [0; 32],
                timestamp: height,
            },
            data,
            signature: None,
        }
    }

    #[test]
    fn decode_block_hash_reads_payload_hash() {
        let hash = B256::from_slice(&[0xab; 32]);
        let block = block_with_hash(5, hash);
        assert_eq!(decode_block_hash(&block.data).unwrap(), hash);
    }

    #[test]
    fn recover_pending_requeues_settled_missing_blocks() {
        let dir = tmp_dir();
        let block_store = BlockStore::new(&dir);
        let pending_store = PendingStore::new(&dir);
        let (tx, mut rx) = mpsc::unbounded_channel::<PendingBlock>();

        let hash_a = B256::from_slice(&[0xa1; 32]);
        block_store.put(&block_with_hash(10, hash_a)).unwrap();
        pending_store.mark(10).unwrap();
        pending_store.mark(11).unwrap();

        recover_pending(&pending_store, &block_store, &tx);

        let recovered = rx.try_recv().unwrap();
        assert_eq!(recovered.height, 10);
        assert_eq!(recovered.block_hash, hash_a);
        assert!(rx.try_recv().is_err());

        assert_eq!(pending_store.pending().unwrap(), vec![10]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recover_pending_noop_when_empty() {
        let dir = tmp_dir();
        let block_store = BlockStore::new(&dir);
        let pending_store = PendingStore::new(&dir);
        let (tx, mut rx) = mpsc::unbounded_channel::<PendingBlock>();

        recover_pending(&pending_store, &block_store, &tx);
        assert!(rx.try_recv().is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn retry_with_backoff_succeeds_after_failures() {
        let shutting_down = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = Arc::clone(&attempts);

        // Fail the first 3 attempts, succeed on the 4th.
        let result = retry_with_backoff(
            &shutting_down,
            Duration::from_millis(1),
            Duration::from_millis(8),
            || {
                let n = attempts_clone.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 3 {
                        Err("transient".into())
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await;

        assert_eq!(result, Some(()));
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn retry_with_backoff_succeeds_immediately() {
        let shutting_down = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);

        let result = retry_with_backoff(
            &shutting_down,
            Duration::from_secs(60),
            Duration::from_secs(60),
            || {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                async move { Ok(()) }
            },
        )
        .await;

        assert_eq!(result, Some(()));
        // Never slept: a single successful call.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_with_backoff_aborts_on_shutdown() {
        let shutting_down = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = Arc::clone(&attempts);
        let shutdown_clone = Arc::clone(&shutting_down);

        // Always fail, but flip shutdown after the first attempt so the loop
        // exits on the next iteration instead of sleeping forever.
        let result = retry_with_backoff(
            &shutting_down,
            Duration::from_millis(1),
            Duration::from_millis(1),
            || {
                let n = attempts_clone.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    shutdown_clone.store(true, Ordering::SeqCst);
                }
                async move { Err("down".into()) }
            },
        )
        .await;

        assert_eq!(result, None);
        // Attempted at least once, but did not spin forever after shutdown.
        assert!(attempts.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn derive_prev_randao_is_deterministic_and_unique() {
        assert_eq!(
            derive_prev_randao(1),
            B256::from_slice(&[
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 1
            ])
        );
        let h = derive_prev_randao(0x100);
        assert_eq!(h[30], 1);
        assert_eq!(h[31], 0);
        assert_ne!(derive_prev_randao(1), derive_prev_randao(2));
    }
}
