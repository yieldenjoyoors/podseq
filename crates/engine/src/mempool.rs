//! JSON-RPC client for Reth's public mempool API (port 8545), distinct from the Engine API.

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

/// Errors returned by mempool JSON-RPC requests.
#[derive(Debug, Error)]
pub enum MempoolError {
    #[error("rpc error ({code}): {message}")]
    Rpc { code: i64, message: String },
    #[error("decode error: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),
}

/// Reads the node transaction pool over the public JSON-RPC API.
#[derive(Debug)]
pub struct MempoolClient {
    http: reqwest::Client,
    endpoint: url::Url,
}

impl MempoolClient {
    /// Creates a mempool client for the given public RPC URL.
    pub fn new(rpc_url: &str) -> Result<Self, MempoolError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            endpoint: rpc_url.parse()?,
        })
    }

    /// Returns the 32-byte hashes of all pending transactions in the pool.
    pub async fn pending_transactions(&self) -> Result<Vec<[u8; 32]>, MempoolError> {
        #[derive(Deserialize)]
        struct TxPoolContent {
            pending: std::collections::HashMap<String, std::collections::HashMap<String, TxInfo>>,
        }
        #[derive(Deserialize)]
        struct TxInfo {
            hash: String,
        }

        let content: TxPoolContent = self.call("txpool_content", vec![]).await?;

        let mut hashes = Vec::new();
        for txs in content.pending.values() {
            for info in txs.values() {
                let hash = hex_hash_to_bytes(&info.hash)?;
                hashes.push(hash);
            }
        }
        Ok(hashes)
    }

    /// Returns the number of pending transactions in the pool.
    pub async fn pending_count(&self) -> Result<usize, MempoolError> {
        #[derive(Deserialize)]
        struct TxPoolStatus {
            pending: String,
        }
        let status: TxPoolStatus = self.call("txpool_status", vec![]).await?;
        let count: usize = u64::from_str_radix(status.pending.trim_start_matches("0x"), 16)
            .map_err(|e| MempoolError::Rpc {
                code: -1,
                message: format!("invalid pending count: {e}"),
            })?
            .try_into()
            .unwrap_or(0);
        Ok(count)
    }

    async fn call<R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<R, MempoolError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        let response = self
            .http
            .post(self.endpoint.as_str())
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
            return Err(MempoolError::Rpc { code, message });
        }

        let result = response.get("result").ok_or(MempoolError::Rpc {
            code: -1,
            message: "response missing result field".into(),
        })?;

        Ok(serde_json::from_value(result.clone())?)
    }
}

fn hex_hash_to_bytes(hex: &str) -> Result<[u8; 32], MempoolError> {
    let hex = hex.trim_start_matches("0x");
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(hex, &mut bytes).map_err(|e| MempoolError::Rpc {
        code: -1,
        message: format!("invalid hex hash: {e}"),
    })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_valid_url() {
        assert!(MempoolClient::new("http://localhost:8545").is_ok());
    }

    #[test]
    fn rejects_invalid_url() {
        assert!(MempoolClient::new("not a url").is_err());
    }

    #[test]
    fn hex_hash_converts_0x_prefix() {
        let bytes =
            hex_hash_to_bytes("0xabcd123400000000000000000000000000000000000000000000000000000000")
                .unwrap();
        assert_eq!(bytes[0], 0xab);
        assert_eq!(bytes[1], 0xcd);
    }
}
