//! P2P configuration.

use std::net::SocketAddr;

/// Configuration for a podseq p2p node.
#[derive(Debug, Clone)]
pub struct P2pConfig {
    /// If absent, a key is generated and saved to this path.
    pub key_path: Option<String>,
    /// Socket address the node listens on.
    pub listen_addr: SocketAddr,
    /// Advertised to peers for dial-back; defaults to listen_addr when unset, may differ behind NAT.
    pub dialable_addr: Option<SocketAddr>,
    /// Prevents replay of authenticated traffic across different applications.
    pub application_namespace: Vec<u8>,
    /// Bootstrap peers as (64-char hex pubkey, socket addr).
    pub bootstrap_peers: Vec<(String, SocketAddr)>,
    /// Maximum message size in bytes.
    pub max_message_size: u32,
    /// Messages cached per peer by the broadcast engine.
    pub broadcast_cache_size: usize,
    /// Pending subscribe/get requests buffered by the broadcast engine.
    pub broadcast_mailbox_size: usize,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            key_path: Some("./p2p.key".into()),
            listen_addr: "0.0.0.0:9000".parse().unwrap(),
            dialable_addr: None,
            application_namespace: b"podseq-v1".to_vec(),
            bootstrap_peers: Vec::new(),
            max_message_size: 2 * 1024 * 1024,
            broadcast_cache_size: 128,
            broadcast_mailbox_size: 1024,
        }
    }
}
