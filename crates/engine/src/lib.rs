//! Reth Engine API client for building and finalizing blocks.

#![forbid(unsafe_code)]

pub mod auth;
pub mod client;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId,
    PayloadStatusEnum,
};
use podseq_core::{Block, Error, Header};

pub use auth::Auth;
pub use client::{Client, EngineError};

/// Parent beacon block root applied to every block. This consensus client
/// has no beacon chain, so there is no real parent beacon block root.
pub const PARENT_BEACON_BLOCK_ROOT: B256 = B256::ZERO;

/// Engine API facade for building, accepting, and finalizing blocks.
#[derive(Debug)]
pub struct Engine {
    rpc: Client,
}

/// An executed payload produced by the Engine API build flow.
pub struct BuiltPayload {
    pub payload_id: PayloadId,
    pub payload: ExecutionPayloadV3,
    pub block_hash: B256,
    pub height: u64,
    pub timestamp: u64,
}

impl Engine {
    /// Creates an Engine facade for the given Engine API URL and JWT secret.
    pub fn new(engine_url: &str, auth: Auth) -> Result<Self, EngineError> {
        Ok(Self {
            rpc: Client::new(engine_url, auth)?,
        })
    }

    /// Returns the underlying Engine API client.
    pub fn rpc(&self) -> &Client {
        &self.rpc
    }

    /// Returns the current chain head block number.
    pub async fn block_number(&self) -> Result<u64, EngineError> {
        self.rpc.block_number().await
    }

    /// Returns the current head block hash via `eth_blockNumber` + `eth_getBlockByNumber`.
    ///
    /// Avoids `forkchoiceUpdatedV3` with an all-zero state: some execution clients
    /// (notably Reth) reject that with `-38002: Invalid forkchoice state` before
    /// the chain has a canonical head they can reference. Standard `eth_` calls
    /// work on any Engine API endpoint.
    ///
    /// Retries with backoff on transient errors for up to 60 seconds, so the
    /// sequencer can start alongside Reth without a race on RPC readiness.
    /// Transient here means transport failures and the "block not found" RPC
    /// path: a just-started Reth can answer `eth_blockNumber` before the block
    /// at that height is retrievable.
    pub async fn current_head(&self) -> Result<B256, EngineError> {
        Ok(self.current_head_with_height().await?.0)
    }

    /// Same as [`current_head`](Self::current_head) but also returns the height,
    /// so callers that need both avoid a second `eth_blockNumber` round trip.
    pub async fn current_head_with_height(&self) -> Result<(B256, u64), EngineError> {
        let mut backoff = std::time::Duration::from_millis(500);
        let max_backoff = std::time::Duration::from_secs(5);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            match self.current_head_inner().await {
                Ok((hash, height)) => return Ok((hash, height)),
                Err(e) if is_transient(&e) && std::time::Instant::now() < deadline => {
                    tracing::warn!(error = %e, ?backoff, "Reth RPC not ready; retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn current_head_inner(&self) -> Result<(B256, u64), EngineError> {
        let height = self.rpc.block_number().await?;
        let hash = self
            .rpc
            .block_by_number(height)
            .await?
            .ok_or_else(|| EngineError::Rpc {
                code: -1,
                message: format!("block {height} not found; is the EL initialized?"),
            })?;
        Ok((hash, height))
    }

    /// Returns the block hash at the given height, if present.
    pub async fn block_by_number(&self, number: u64) -> Result<Option<B256>, EngineError> {
        self.rpc.block_by_number(number).await
    }

    /// Builds a block for the given state and attributes and returns the payload.
    pub async fn build(
        &self,
        state: ForkchoiceState,
        attributes: PayloadAttributes,
    ) -> Result<BuiltPayload, EngineError> {
        let updated = self.wait_for_forkchoice(state, Some(attributes)).await?;

        let payload_id = updated.payload_id.ok_or(EngineError::Rpc {
            code: -32000,
            message: "forkchoiceUpdated returned no payload id".into(),
        })?;

        let envelope = self.rpc.get_payload_v4(payload_id).await?;
        let payload = envelope.envelope_inner.execution_payload;
        let block_hash = payload.payload_inner.payload_inner.block_hash;
        let height = payload.payload_inner.payload_inner.block_number;
        let timestamp = payload.timestamp();
        Ok(BuiltPayload {
            payload_id,
            payload,
            block_hash,
            height,
            timestamp,
        })
    }

    /// Submits a payload via newPayload and advances the forkchoice head to it.
    pub async fn accept(
        &self,
        payload: &ExecutionPayloadV3,
        new_head: B256,
        safe: B256,
        finalized: B256,
    ) -> Result<(), EngineError> {
        let status = self
            .rpc
            .new_payload_v4(payload.clone(), vec![], PARENT_BEACON_BLOCK_ROOT)
            .await?;

        if status.status != PayloadStatusEnum::Valid {
            return Err(EngineError::Rpc {
                code: -32002,
                message: format!("newPayload rejected payload: {:?}", status.status),
            });
        }

        let fc_state = ForkchoiceState {
            head_block_hash: new_head,
            safe_block_hash: safe,
            finalized_block_hash: finalized,
        };
        self.wait_for_forkchoice(fc_state, None).await?;
        Ok(())
    }

    /// Updates the forkchoice head, safe, and finalized hashes.
    pub async fn finalize(
        &self,
        head: B256,
        safe: B256,
        finalized: B256,
    ) -> Result<(), EngineError> {
        let fc_state = ForkchoiceState {
            head_block_hash: head,
            safe_block_hash: safe,
            finalized_block_hash: finalized,
        };
        self.wait_for_forkchoice(fc_state, None).await?;
        Ok(())
    }

    /// Calls `forkchoiceUpdatedV3` and retries with backoff if the status is `SYNCING`.
    /// Caps at 10 retries (~30 s total) so Ctrl+C can interrupt.
    async fn wait_for_forkchoice(
        &self,
        state: ForkchoiceState,
        attributes: Option<PayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineError> {
        let mut backoff = std::time::Duration::from_millis(500);
        let max_backoff = std::time::Duration::from_secs(5);
        const MAX_RETRIES: u32 = 10;

        for _ in 0..MAX_RETRIES {
            let result = self
                .rpc
                .fork_choice_updated_v3(state, attributes.clone())
                .await?;
            tracing::debug!(
                status = ?result.payload_status.status,
                payload_id = ?result.payload_id,
                latest_valid_hash = ?result.payload_status.latest_valid_hash,
                "forkchoiceUpdated V3 response"
            );
            match result.payload_status.status {
                PayloadStatusEnum::Syncing => {
                    tracing::warn!(?backoff, "Reth is syncing; retrying forkchoiceUpdated");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
                PayloadStatusEnum::Valid => return Ok(result),
                ref other => {
                    return Err(EngineError::Rpc {
                        code: -32001,
                        message: format!("forkchoiceUpdated returned {other:?}"),
                    });
                }
            }
        }

        Err(EngineError::Rpc {
            code: -32001,
            message: "Reth still syncing after max retries".into(),
        })
    }
}

/// Classifies errors that can resolve themselves as Reth finishes starting.
/// Transport failures and the synthesized "block not found" RPC error both
/// qualify; anything else is a real failure the caller should see immediately.
fn is_transient(e: &EngineError) -> bool {
    match e {
        EngineError::Transport(_) => true,
        EngineError::Rpc { code: -1, message } => message.starts_with("block "),
        _ => false,
    }
}

/// Converts a built Engine API payload into a core `Block`.
pub fn payload_into_block(built: &BuiltPayload) -> Result<Block, Error> {
    let inner = &built.payload.payload_inner.payload_inner;
    let header = Header {
        height: built.height,
        parent_hash: inner.parent_hash.into(),
        state_root: inner.state_root.into(),
        timestamp: built.timestamp,
    };
    let data = serde_json::to_vec(&built.payload)
        .map_err(|e| Error::Execution(format!("encode payload: {e}")))?;
    Ok(Block {
        header,
        data,
        signature: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn sample_payload() -> BuiltPayload {
        let payload = ExecutionPayloadV3 {
            payload_inner: alloy_rpc_types_engine::ExecutionPayloadV2 {
                payload_inner: alloy_rpc_types_engine::ExecutionPayloadV1 {
                    parent_hash: B256::ZERO,
                    fee_recipient: Address::ZERO,
                    state_root: B256::ZERO,
                    receipts_root: B256::ZERO,
                    logs_bloom: Default::default(),
                    prev_randao: B256::ZERO,
                    block_number: 42,
                    gas_limit: 30_000_000,
                    gas_used: 0,
                    timestamp: 1_700_000_000,
                    extra_data: Default::default(),
                    base_fee_per_gas: alloy_primitives::U256::from(7),
                    block_hash: B256::ZERO,
                    transactions: vec![],
                },
                withdrawals: vec![],
            },
            blob_gas_used: 0,
            excess_blob_gas: 0,
        };
        BuiltPayload {
            payload_id: PayloadId::new([0u8; 8]),
            payload,
            block_hash: B256::ZERO,
            height: 42,
            timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn payload_into_block_maps_header_fields() {
        let block = payload_into_block(&sample_payload()).unwrap();
        assert_eq!(block.header.height, 42);
        assert_eq!(block.header.timestamp, 1_700_000_000);
        assert!(!block.data.is_empty());
    }

    #[test]
    fn is_transient_classifies_startup_errors() {
        // The synthesized "block not found" error from current_head_inner is retried.
        let block_missing = EngineError::Rpc {
            code: -1,
            message: "block 0 not found; is the EL initialized?".into(),
        };
        assert!(is_transient(&block_missing));

        // A real RPC error from the node is not retried.
        let rpc_real = EngineError::Rpc {
            code: -38002,
            message: "Invalid forkchoice state".into(),
        };
        assert!(!is_transient(&rpc_real));

        // A synthesized error about something else is not retried.
        let other = EngineError::Rpc {
            code: -1,
            message: "invalid block number: parse error".into(),
        };
        assert!(!is_transient(&other));
    }
}
