//! Reth Engine API client for building and finalizing blocks.

#![forbid(unsafe_code)]

pub mod auth;
pub mod client;
pub mod mempool;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadAttributes, PayloadId,
    PayloadStatusEnum,
};
use podseq_core::{Block, Error, Header};

pub use auth::Auth;
pub use client::{Client, EngineError};
pub use mempool::{MempoolClient, MempoolError};

/// Parent beacon block root applied to every block.
/// This consensus client and has no beacon chain, so there is no real
/// parent beacon block root.
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
    pub async fn current_head(&self) -> Result<B256, EngineError> {
        let height = self.rpc.block_number().await?;
        let hash = self
            .rpc
            .block_by_number(height)
            .await?
            .ok_or_else(|| EngineError::Rpc {
                code: -1,
                message: format!("block {height} not found; is the EL initialized?"),
            })?;
        Ok(hash)
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
}
