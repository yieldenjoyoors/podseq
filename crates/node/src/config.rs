//! Node configuration.
//!
//! A single TOML file wires together every component:
//!
//! ```toml
//! [reth]
//! engine_url = "http://localhost:8551"
//! jwt_path   = "jwt.hex"
//!
//! [walrus]
//! epochs = 53               # 0/unset → max (53 ≈ 2 years)
//!
//! [signer]
//! key_path = "sequencer.key"      # sequencer mode
//! sequencer_pubkey = "0x..."      # full node mode (required)
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level node configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub reth: RethConfig,
    #[serde(default)]
    pub walrus: WalrusConfig,
    #[serde(default)]
    pub sui: SuiConfig,
    #[serde(default)]
    pub signer: SignerConfig,
    #[serde(default)]
    pub sequencer: SequencerConfig,
    #[serde(default)]
    pub p2p: P2pConfig,
    /// Directory for persistent chain state and blocks.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Node mode: "sequencer" or "full".
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("./data")
}

fn default_mode() -> String {
    "sequencer".into()
}

/// Reth execution-engine connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RethConfig {
    /// Engine API URL (auth via JWT).
    #[serde(default = "default_engine_url")]
    pub engine_url: String,
    /// Eth RPC URL for mempool access.
    #[serde(default = "default_rpc_url")]
    pub rpc_url: String,
    /// Path to the Engine API JWT secret.
    pub jwt_path: PathBuf,
}

fn default_rpc_url() -> String {
    "http://localhost:8545".into()
}

fn default_engine_url() -> String {
    "http://localhost:8551".into()
}

/// Walrus DA publisher/aggregator and blob settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalrusConfig {
    /// Walrus publisher endpoint.
    #[serde(default = "default_publisher")]
    pub publisher_url: String,
    /// Walrus aggregator endpoint.
    #[serde(default = "default_aggregator")]
    pub aggregator_url: String,
    /// Blob storage duration in epochs; 0/unset → max (53 ≈ 2 years).
    #[serde(default = "default_epochs")]
    pub epochs: u64,
    /// Flush a DA blob once buffered blocks reach this size.
    #[serde(default = "default_batch_size_bytes")]
    pub batch_size_bytes: usize,
    /// Bearer token for authenticated publishers.
    /// Leave unset when using an open  publisher.
    #[serde(default)]
    pub publisher_auth_token: Option<String>,
}

fn default_publisher() -> String {
    podseq_sui::TESTNET_PUBLISHER.into()
}

fn default_aggregator() -> String {
    podseq_sui::TESTNET_AGGREGATOR.into()
}

fn default_epochs() -> u64 {
    podseq_sui::MAX_EPOCHS
}

fn default_batch_size_bytes() -> usize {
    64 * 1024
}

impl Default for WalrusConfig {
    fn default() -> Self {
        Self {
            publisher_url: default_publisher(),
            aggregator_url: default_aggregator(),
            epochs: default_epochs(),
            batch_size_bytes: default_batch_size_bytes(),
            publisher_auth_token: None,
        }
    }
}

/// Sui settlement and RPC settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiConfig {
    /// Sui fullnode RPC URL.
    #[serde(default = "default_sui_rpc")]
    pub rpc_url: String,
    /// Path to the Move package directory (must contain a `build/` subdir).
    #[serde(default = "default_move_dir")]
    pub move_dir: PathBuf,
    /// Deployed settlement Move package ID.
    #[serde(default)]
    pub settlement_package_id: Option<String>,
    /// SettlerCap object ID (owned by the sequencer).
    #[serde(default)]
    pub settler_cap_id: Option<String>,
    /// Shared settlement Registry object ID.
    #[serde(default)]
    pub registry_id: Option<String>,
}

fn default_sui_rpc() -> String {
    "https://fullnode.testnet.sui.io:443".into()
}

fn default_move_dir() -> PathBuf {
    PathBuf::from("./move")
}

impl Default for SuiConfig {
    fn default() -> Self {
        Self {
            rpc_url: default_sui_rpc(),
            move_dir: default_move_dir(),
            settlement_package_id: None,
            settler_cap_id: None,
            registry_id: None,
        }
    }
}

/// Sequencer signing keys (mode-dependent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignerConfig {
    /// Sequencer signing key (suiprivkey); required in sequencer mode.
    #[serde(default = "default_signer_key_path")]
    pub key_path: Option<PathBuf>,
    /// Sequencer ed25519 pubkey (hex); required in full node mode.
    #[serde(default)]
    pub sequencer_pubkey: Option<String>,
}

impl Default for SignerConfig {
    fn default() -> Self {
        Self {
            key_path: default_signer_key_path(),
            sequencer_pubkey: None,
        }
    }
}

fn default_signer_key_path() -> Option<PathBuf> {
    Some(PathBuf::from("sequencer.key"))
}

/// Peer-to-peer networking settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2pConfig {
    /// Path to the p2p identity key file.
    #[serde(default = "default_p2p_key_path")]
    pub key_path: Option<String>,
    /// Socket address to listen on.
    #[serde(default = "default_p2p_listen")]
    pub listen_addr: String,
    /// Advertised dial-back address (may differ from `listen_addr` behind NAT).
    pub dialable_addr: Option<String>,
    /// Format: "pubkey_hex@addr:port".
    #[serde(default)]
    pub bootstrap_peers: Vec<String>,
    /// Disable p2p networking.
    #[serde(default)]
    pub no_p2p: bool,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            key_path: default_p2p_key_path(),
            listen_addr: default_p2p_listen(),
            dialable_addr: None,
            bootstrap_peers: Vec::new(),
            no_p2p: false,
        }
    }
}

fn default_p2p_key_path() -> Option<String> {
    Some("p2p.key".into())
}

fn default_p2p_listen() -> String {
    "0.0.0.0:9000".into()
}

/// Sequencer block-production settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequencerConfig {
    /// Block production interval, in milliseconds.
    #[serde(default = "default_block_time_ms")]
    pub block_time_ms: u64,
    /// Fee recipient (20-byte Ethereum address).
    #[serde(default = "default_fee_recipient")]
    pub fee_recipient: String,
    /// Initial head block hash; if unset, the sequencer queries Reth.
    #[serde(default)]
    pub genesis_hash: Option<String>,
}

fn default_block_time_ms() -> u64 {
    2000
}

fn default_fee_recipient() -> String {
    "0x0000000000000000000000000000000000000000".into()
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            block_time_ms: default_block_time_ms(),
            fee_recipient: default_fee_recipient(),
            genesis_hash: None,
        }
    }
}

impl Config {
    /// Loads config from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        Self::from_str(&text)
    }

    /// Parses config from a TOML string.
    pub fn from_str(text: &str) -> Result<Self> {
        toml::from_str(text).context("parsing config TOML")
    }

    /// Returns a default testnet config.
    pub fn testnet() -> Self {
        Self {
            reth: RethConfig {
                engine_url: default_engine_url(),
                rpc_url: default_rpc_url(),
                jwt_path: PathBuf::from("jwt.hex"),
            },
            walrus: WalrusConfig::default(),
            sui: SuiConfig::default(),
            signer: SignerConfig::default(),
            sequencer: SequencerConfig::default(),
            p2p: P2pConfig::default(),
            data_dir: default_data_dir(),
            mode: default_mode(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let toml = r#"
[reth]
engine_url = "http://localhost:8551"
jwt_path = "jwt.hex"

[walrus]
epochs = 5

[sui]
rpc_url = "https://fullnode.mainnet.sui.io:443"
settlement_package_id = "0xabc"

[signer]
key_path = "sequencer.key"
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.reth.engine_url, "http://localhost:8551");
        assert_eq!(config.walrus.epochs, 5);
        assert_eq!(config.sui.settlement_package_id.as_deref(), Some("0xabc"));
        assert_eq!(config.signer.key_path, Some(PathBuf::from("sequencer.key")));
    }

    #[test]
    fn applies_defaults_for_missing_sections() {
        let toml = r#"
[reth]
jwt_path = "jwt.hex"
"#;
        let config = Config::from_str(toml).unwrap();
        assert_eq!(config.reth.engine_url, "http://localhost:8551");
        assert!(config.walrus.publisher_url.contains("walrus-testnet"));
        assert_eq!(config.walrus.epochs, podseq_sui::MAX_EPOCHS);
        assert_eq!(config.signer.key_path, Some(PathBuf::from("sequencer.key")));
    }

    #[test]
    fn rejects_missing_jwt_path() {
        let toml = r#"
[reth]
engine_url = "http://localhost:8551"
"#;
        assert!(Config::from_str(toml).is_err());
    }

    #[test]
    fn roundtrips_through_serialize() {
        let config = Config::testnet();
        let serialized = toml::to_string(&config).unwrap();
        let parsed = Config::from_str(&serialized).unwrap();
        assert_eq!(parsed.reth.jwt_path, config.reth.jwt_path);
        assert_eq!(parsed.walrus.epochs, config.walrus.epochs);
    }
}
