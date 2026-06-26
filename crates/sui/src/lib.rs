//! Sui-layer integration for podseq: Walrus data availability + settlement.

#![forbid(unsafe_code)]

pub mod blob_id;
pub mod settlement;
pub mod wire;

use podseq_core::{BlobId, Block, DataAvailability, Settlement};
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, info};
use wire::{decode as decode_block, encode as encode_block};

/// Verifies a block's ed25519 signature against the sequencer's public key.
///
/// Returns `Ok(())` if the signature is valid, or an `InvalidBlock` error if the
/// block is unsigned or the signature does not verify. Full nodes and tests share
/// this so verification stays consistent across call sites.
pub fn verify_block_signature(
    block: &Block,
    pubkey: &sui_crypto::ed25519::Ed25519VerifyingKey,
) -> Result<(), podseq_core::Error> {
    use sui_crypto::Verifier;

    let sig = block.signature.ok_or_else(|| {
        podseq_core::Error::InvalidBlock(format!("block {} is unsigned", block.header.height))
    })?;
    let msg = block.header.signing_message();
    let sui_sig = sui_sdk_types::Ed25519Signature::new(sig);
    pubkey.verify(&msg, &sui_sig).map_err(|e| {
        podseq_core::Error::InvalidBlock(format!(
            "invalid signature on block {}: {e}",
            block.header.height
        ))
    })
}

pub use settlement::{DeployedContract, Settlement as SettlementSigner, SettlementError};

/// Default Walrus testnet aggregator endpoint.
pub const TESTNET_AGGREGATOR: &str = "https://aggregator.walrus-testnet.walrus.space";
/// Default Walrus testnet publisher endpoint.
pub const TESTNET_PUBLISHER: &str = "https://publisher.walrus-testnet.walrus.space";

/// Errors from Walrus store/read and Sui RPC interactions.
#[derive(Debug, Error)]
pub enum Error {
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("store failed ({status}): {body}")]
    Store { status: u16, body: String },
    #[error("read failed ({status}): {body}")]
    Read { status: u16, body: String },
    #[error("invalid blob id: {0}")]
    InvalidBlobId(String),
    #[error("store response missing blob id")]
    MissingBlobId,
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),
}

/// Connection configuration for Walrus and the Sui RPC.
#[derive(Debug, Clone)]
pub struct Config {
    pub publisher_url: String,
    pub aggregator_url: String,
    pub epochs: u64,
    pub sui_rpc_url: String,
    pub publisher_auth_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            publisher_url: TESTNET_PUBLISHER.into(),
            aggregator_url: TESTNET_AGGREGATOR.into(),
            epochs: MAX_EPOCHS,
            sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
            publisher_auth_token: None,
        }
    }
}

/// Maximum number of Walrus storage epochs a blob can be stored for (~2 years).
pub const MAX_EPOCHS: u64 = 53;

/// A Sui RPC client for Walrus DA and settlement.
#[derive(Debug)]
pub struct Client {
    http: reqwest::Client,
    config: Config,
    settlement: Option<Arc<Mutex<SettlementSigner>>>,
}

impl Client {
    /// Creates a client from the given [`Config`].
    pub fn new(config: Config) -> Result<Self, Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()?,
            config,
            settlement: None,
        })
    }

    /// Attaches a settlement signer, enabling on-chain commitments.
    pub fn with_settlement(mut self, settlement: SettlementSigner) -> Self {
        self.settlement = Some(Arc::new(Mutex::new(settlement)));
        self
    }

    pub fn has_settlement(&self) -> bool {
        self.settlement.is_some()
    }

    pub fn rpc_url(&self) -> &str {
        &self.config.sui_rpc_url
    }

    pub fn publisher_url(&self) -> &str {
        &self.config.publisher_url
    }

    pub fn aggregator_url(&self) -> &str {
        &self.config.aggregator_url
    }

    async fn store_blob(&self, bytes: Vec<u8>) -> Result<BlobId, Error> {
        // Blobs are always permanent (not deletable, even by the uploader) and
        // stored for the configured number of epochs. See
        // https://docs.wal.app/docs/http-api/storing-blobs
        let url = format!(
            "{}/v1/blobs?epochs={}&permanent=true",
            self.config.publisher_url, self.config.epochs
        );
        let len = bytes.len();
        debug!(%url, bytes = len, "uploading blob to Walrus");
        let started = std::time::Instant::now();

        let response = if let Some(token) = &self.config.publisher_auth_token {
            self.http
                .put(&url)
                .header("Authorization", format!("Bearer {token}"))
                .body(bytes)
                .send()
                .await?
        } else {
            self.http.put(&url).body(bytes).send().await?
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Store {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: StoreResponse = response.json().await?;
        let blob_id_str = parsed.blob_id().ok_or(Error::MissingBlobId)?;
        // newlyCreated = Walrus did full erasure-coding/certification (slow);
        // alreadyCertified = the blob was already stored (fast, e.g. re-upload).
        let kind = if parsed.newly_created.is_some() {
            "newlyCreated"
        } else if parsed.already_certified.is_some() {
            "alreadyCertified"
        } else {
            "unknown"
        };
        let elapsed = started.elapsed();
        info!(
            blob_id = %blob_id_str,
            kind,
            bytes = len,
            elapsed_ms = elapsed.as_millis() as u64,
            "walrus upload complete"
        );
        blob_id::decode(blob_id_str)
    }

    async fn fetch_blob(&self, id: &BlobId) -> Result<Vec<u8>, Error> {
        let encoded = blob_id::encode(id);
        let url = format!("{}/v1/blobs/{encoded}", self.config.aggregator_url);
        debug!(%url, "reading blob from Walrus");

        let response = self.http.get(&url).send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Read {
                status: status.as_u16(),
                body,
            });
        }

        Ok(response.bytes().await?.to_vec())
    }
}

impl DataAvailability for Client {
    async fn publish(&self, blocks: &[Block]) -> Result<BlobId, podseq_core::Error> {
        let bytes = encode_block(blocks).map_err(to_core)?;
        self.store_blob(bytes).await.map_err(to_core)
    }

    async fn fetch(&self, id: &BlobId) -> Result<Vec<Block>, podseq_core::Error> {
        let bytes = self.fetch_blob(id).await.map_err(to_core)?;
        decode_block(&bytes).map_err(to_core)
    }
}

impl Settlement for Client {
    async fn commit(&self, block: &Block, blob: &BlobId) -> Result<(), podseq_core::Error> {
        let settlement = self
            .settlement
            .as_ref()
            .ok_or_else(|| podseq_core::Error::Settlement("settlement not configured".into()))?;
        let mut guard = settlement.lock().await;
        guard
            .commit(&block.header, blob)
            .await
            .map_err(|e| podseq_core::Error::Settlement(e.to_string()))
    }
}

// All callers are the `DataAvailability` impl (publish/fetch), so every Walrus
// transport/parse error is a DA failure.
fn to_core(e: Error) -> podseq_core::Error {
    podseq_core::Error::DataAvailability(e.to_string())
}

#[derive(Debug, Deserialize)]
struct StoreResponse {
    #[serde(rename = "newlyCreated")]
    newly_created: Option<NewlyCreated>,
    #[serde(rename = "alreadyCertified")]
    already_certified: Option<AlreadyCertified>,
}

impl StoreResponse {
    fn blob_id(&self) -> Option<&str> {
        self.newly_created
            .as_ref()
            .map(|n| n.blob_object.blob_id.as_str())
            .or_else(|| self.already_certified.as_ref().map(|a| a.blob_id.as_str()))
    }
}

#[derive(Debug, Deserialize)]
struct NewlyCreated {
    #[serde(rename = "blobObject")]
    blob_object: BlobObject,
}

#[derive(Debug, Deserialize)]
struct BlobObject {
    #[serde(rename = "blobId")]
    blob_id: String,
}

#[derive(Debug, Deserialize)]
struct AlreadyCertified {
    #[serde(rename = "blobId")]
    blob_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use podseq_core::Header;

    #[test]
    fn store_response_parses_newly_created() {
        let json = r#"{
            "newlyCreated": {
                "blobObject": {
                    "id": "0xe91eee8c5b6f35b9a250cfc29e30f0d9e5463a21fd8d1ddb0fc22d44db4eac50",
                    "registeredEpoch": 34,
                    "blobId": "M4hsZGQ1oCktdzegB6HnI6Mi28S2nqOPHxK-W7_4BUk",
                    "size": 17,
                    "encodingType": "RS2",
                    "certifiedEpoch": 34,
                    "deletable": false
                },
                "cost": 132300
            }
        }"#;
        let resp: StoreResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.blob_id().unwrap(),
            "M4hsZGQ1oCktdzegB6HnI6Mi28S2nqOPHxK-W7_4BUk"
        );
    }

    #[test]
    fn store_response_parses_already_certified() {
        let json = r#"{
            "alreadyCertified": {
                "blobId": "M4hsZGQ1oCktdzegB6HnI6Mi28S2nqOPHxK-W7_4BUk",
                "endEpoch": 35
            }
        }"#;
        let resp: StoreResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.blob_id().unwrap(),
            "M4hsZGQ1oCktdzegB6HnI6Mi28S2nqOPHxK-W7_4BUk"
        );
    }

    #[test]
    fn client_builds_with_defaults() {
        let client = Client::new(Config::default()).unwrap();
        assert!(client.publisher_url().contains("walrus-testnet"));
        assert!(client.aggregator_url().contains("aggregator"));
        assert_eq!(client.rpc_url(), "https://fullnode.testnet.sui.io:443");
    }

    #[tokio::test]
    async fn roundtrip_encode_decode_block() {
        let block = Block {
            header: Header {
                height: 1,
                parent_hash: [0; 32],
                state_root: [1; 32],
                timestamp: 1,
            },
            data: vec![1, 2, 3],
            signature: None,
        };
        let batch = vec![block.clone()];
        let bytes = wire::encode(&batch).unwrap();
        let back = wire::decode(&bytes).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].header, block.header);
        assert_eq!(back[0].data, block.data);
    }
}
