//! P2P configuration.

use std::net::SocketAddr;

/// Configuration for a podseq p2p node.
#[derive(Debug, Clone)]
pub struct P2pConfig {
    /// If absent, a key is generated and saved to this path.
    pub key_path: Option<String>,
    /// Socket address the node listens on.
    pub listen_addr: SocketAddr,
    /// Advertised to peers for dial-back; defaults to `listen_addr` when unset, may differ behind NAT.
    pub dialable_addr: Option<SocketAddr>,
    /// Prevents replay of authenticated traffic across different applications.
    pub application_namespace: Vec<u8>,
    /// Bootstrap peers as (64-char hex pubkey, socket addr).
    ///
    /// These are dialed on startup and persistently admitted: the tracker
    /// always accepts connections from a bootstrapper, and holds its
    /// dialable address. With [`P2pConfig::registry_admission`] disabled
    /// they also define the tracked peer set, so every node in a static
    /// deployment must list all of its peers here.
    pub bootstrap_peers: Vec<(String, SocketAddr)>,
    /// Admit peers from registry-published peer sets
    /// ([`crate::AdmissionHandle`]) instead of the static
    /// `bootstrap_peers ∪ {self}` set.
    ///
    /// Chain-derived sets are registered under the on-chain book version,
    /// so every node deriving its set from the same chain state produces
    /// identical content per index — the invariant the discovery gossip
    /// enforces by killing peers whose set content disagrees. When true,
    /// no initial peer set is registered at startup; admission begins
    /// with the first registry read. `bootstrap_peers` still provides
    /// dial addresses (tracked peer sets carry identities only).
    pub registry_admission: bool,
    /// Maximum message size in bytes.
    pub max_message_size: u32,
    /// Messages cached per peer by the broadcast engine.
    pub broadcast_cache_size: usize,
    /// Pending subscribe/get requests buffered by the broadcast engine.
    pub broadcast_mailbox_size: usize,
    /// Mailbox capacity for the discovery network's internal actors.
    pub mailbox_size: usize,
    /// Maximum distinct identities per peer-set index.
    ///
    /// Must exceed the on-chain committee cap (100), or registry admission
    /// truncates sets differently per node and the gossip kills the
    /// connections it was meant to enable.
    pub max_peers_per_set: usize,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            key_path: Some("./p2p.key".into()),
            listen_addr: "0.0.0.0:9000".parse().unwrap(),
            dialable_addr: None,
            application_namespace: b"podseq-v1".to_vec(),
            bootstrap_peers: Vec::new(),
            registry_admission: false,
            max_message_size: 2 * 1024 * 1024,
            broadcast_cache_size: 128,
            broadcast_mailbox_size: 1024,
            mailbox_size: 1024,
            max_peers_per_set: 128,
        }
    }
}
