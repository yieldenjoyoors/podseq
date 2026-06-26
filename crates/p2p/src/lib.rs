//! Peer-to-peer networking backed by Commonware.
//!
//! Wraps a `discovery::Network` for peer discovery with a `buffered::Engine`
//! for block propagation, so the sequencer disseminates blocks and full nodes
//! receive them without polling DA.

#![forbid(unsafe_code)]

mod config;
mod key;
mod message;
mod node;

pub use config::P2pConfig;
pub use key::load_or_generate_key;
pub use message::BlockMessage;
pub use node::{BlockBroadcaster, BlockReceiver, P2pNode};

/// The p2p identity keypair type used by Commonware.
pub type IdentityKey = commonware_cryptography::ed25519::PrivateKey;
/// The corresponding public key.
pub type IdentityPubkey = commonware_cryptography::ed25519::PublicKey;
