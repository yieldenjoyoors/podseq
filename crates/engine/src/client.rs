//! JSON-RPC 2.0 client for the authenticated Engine API.

use std::sync::atomic::{AtomicU64, Ordering};

use alloy_primitives::B256;
use alloy_rpc_types_engine::{
    ExecutionPayloadEnvelopeV4, ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated,
    PayloadAttributes, PayloadId, PayloadStatus,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tracing::debug;

use crate::auth::Auth;

/// Errors returned by Engine API requests.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine api error ({code}): {message}")]
    Rpc { code: i64, message: String },
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("jwt error: {0}")]
    Jwt(#[from] jsonwebtoken::errors::Error),
    #[error("auth secret error: {0}")]
    AuthSecret(#[from] alloy_rpc_types_engine::JwtError),
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),
}

/// Issues authenticated Engine API JSON-RPC calls, tracking per-request ids.
#[derive(Debug)]
pub struct Client {
    http: reqwest::Client,
    endpoint: url::Url,
    auth: Auth,
    next_id: AtomicU64,
}

impl Client {
    /// Creates a client for the given Engine API URL and JWT secret.
    /// 30s timeout: long enough for block building, bounded to avoid hanging on a stalled node.
    pub fn new(engine_url: &str, auth: Auth) -> Result<Self, EngineError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            endpoint: engine_url.parse()?,
            auth,
            next_id: AtomicU64::new(1),
        })
    }

    /// Supplying payload attributes triggers block building.
    pub async fn fork_choice_updated_v3(
        &self,
        state: ForkchoiceState,
        attributes: Option<PayloadAttributes>,
    ) -> Result<ForkchoiceUpdated, EngineError> {
        self.call(
            "engine_forkchoiceUpdatedV3",
            vec![
                serde_json::to_value(state)?,
                serde_json::to_value(attributes)?,
            ],
        )
        .await
    }

    /// Returns the execution payload built for `payload_id` (`engine_getPayloadV4`).
    pub async fn get_payload_v4(
        &self,
        payload_id: PayloadId,
    ) -> Result<ExecutionPayloadEnvelopeV4, EngineError> {
        self.call(
            "engine_getPayloadV4",
            vec![serde_json::to_value(payload_id)?],
        )
        .await
    }

    /// Submits a built payload to the node for validation (`engine_newPayloadV4`).
    pub async fn new_payload_v4(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> Result<PayloadStatus, EngineError> {
        self.call(
            "engine_newPayloadV4",
            vec![
                serde_json::to_value(payload)?,
                serde_json::to_value(versioned_hashes)?,
                serde_json::to_value(parent_beacon_block_root)?,
                // EIP-7685 execution requests: this dev chain has no
                // deposit/withdrawal/consolidation requests.
                serde_json::Value::Array(vec![]),
            ],
        )
        .await
    }

    /// Returns the current chain head height (`eth_blockNumber`).
    pub async fn block_number(&self) -> Result<u64, EngineError> {
        let hex: String = self.call("eth_blockNumber", vec![]).await?;
        u64::from_str_radix(hex.trim_start_matches("0x"), 16).map_err(|e| EngineError::Rpc {
            code: -3,
            message: format!("invalid block number: {e}"),
        })
    }

    /// Returns the hash of the block at `number`, or `None` if it doesn't exist.
    pub async fn block_by_number(&self, number: u64) -> Result<Option<B256>, EngineError> {
        let hex_num = format!("0x{number:x}");
        let block: Option<Value> = self
            .call(
                "eth_getBlockByNumber",
                vec![
                    serde_json::to_value(&hex_num)?,
                    serde_json::to_value(false)?,
                ],
            )
            .await?;
        match block {
            Some(obj) => {
                let hash: String =
                    serde_json::from_value(obj.get("hash").cloned().unwrap_or(Value::Null))
                        .map_err(|e| EngineError::Rpc {
                            code: -3,
                            message: format!("invalid block hash: {e}"),
                        })?;
                Ok(Some(hash.parse().map_err(|e| EngineError::Rpc {
                    code: -3,
                    message: format!("invalid block hash format: {e}"),
                })?))
            }
            None => Ok(None),
        }
    }

    async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<R, EngineError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let token = self.auth.token()?;

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        debug!(%method, id, "engine api request");

        let response = self
            .http
            .post(self.endpoint.as_str())
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?
            .json::<Value>()
            .await?;

        if let Some(error) = response.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return Err(EngineError::Rpc { code, message });
        }

        let result = response.get("result").ok_or_else(|| EngineError::Rpc {
            code: -1,
            message: "response missing result field".into(),
        })?;

        Ok(serde_json::from_value(result.clone())?)
    }
}
