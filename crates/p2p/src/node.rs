//! P2p node wiring the Commonware discovery network and broadcast engine.
//!
//! Uses two p2p channels: block broadcast (ch. 0) and block announce (ch. 1).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use commonware_broadcast::Broadcaster as _;
use commonware_cryptography::{
    sha256::Digest, ChaCha20Poly1305, Digested, Digestible, Signer as _,
};
use commonware_p2p::{authenticated::discovery, Ingress, Manager, Recipients, Sender as RawSender};
use commonware_parallel::Sequential;
use commonware_runtime::{IoBuf, Quota, Supervisor as _};
use commonware_stream::{cups, sake, SakeCups};
use commonware_utils::{ordered::Set, NZUsize};
use tracing::{debug, info, warn};

use crate::config::P2pConfig;
use crate::message::BlockMessage;

const BLOCK_CHANNEL: u64 = 0;
const ANNOUNCE_CHANNEL: u64 = 1;

type Pk = commonware_cryptography::ed25519::PublicKey;
type AnnounceSender =
    commonware_p2p::authenticated::discovery::Sender<Pk, commonware_runtime::tokio::Context>;
type AnnounceReceiver = commonware_p2p::authenticated::discovery::Receiver<Pk>;

/// Handshake parameters shared by every podseq peer: the SAKE authenticated
/// key exchange over CUPS, encrypted with `ChaCha20Poly1305`.
fn handshake(signer: crate::IdentityKey) -> SakeCups<crate::IdentityKey, ChaCha20Poly1305> {
    cups::Cups::<_, ChaCha20Poly1305>::new(
        sake::Sake {
            signer,
            synchrony_bound: Duration::from_secs(5),
            max_handshake_age: Duration::from_secs(10),
            version: sake::Version::V1,
        },
        cups::Version::V1,
    )
}

/// Holds the background task handles for the network and broadcast engine.
pub struct P2pHandles {
    _network_handle: commonware_runtime::Handle<()>,
    _engine_handle: commonware_runtime::Handle<()>,
}

/// A runtime-updatable admission set: which peer identities the discovery
/// tracker admits (and dials).
///
/// With [`P2pConfig::registry_admission`], the full-node sync loop registers
/// the chain-derived peer book under its on-chain version on every poll, so
/// admission is derived from chain state instead of operator-side
/// configuration. Content for a given version must be identical on every
/// node — the discovery gossip kills connections whose peers disagree.
#[derive(Clone)]
pub struct AdmissionHandle {
    oracle: Arc<Mutex<discovery::Oracle<Pk>>>,
    max_peers: usize,
    epoch: Arc<AtomicU64>,
}

impl AdmissionHandle {
    /// Registers `peers` under peer-set index `epoch` (the on-chain book
    /// version), replacing older sets as the gossip source of truth.
    ///
    /// The set is exactly the chain-derived list, deduplicated and truncated
    /// to the tracker's `max_peers_per_set` bound (the tracker asserts and
    /// would panic above it). Truncation is a config-error guard: divergent
    /// truncation across nodes re-introduces the gossip disagreement the
    /// versioned content exists to prevent, so keep the bound above the
    /// on-chain committee cap.
    ///
    /// # Panics
    ///
    /// Panics if the discovery oracle's mailbox mutex is poisoned (a prior
    /// holder panicked mid-`track`).
    pub fn admit(&self, epoch: u64, peers: impl IntoIterator<Item = [u8; 32]>) {
        let set = admission_set(peers.into_iter().collect(), self.max_peers);
        let tracked: Result<Vec<Pk>, _> = set.iter().copied().map(load_pubkey).collect();
        let tracked = match tracked {
            Ok(tracked) => tracked,
            Err(e) => {
                warn!(error = %e, "p2p admission update skipped: undecodable identity");
                return;
            }
        };
        let count = tracked.len();
        self.oracle
            .lock()
            .expect("p2p admission oracle mutex")
            .track(epoch, Set::from_iter_dedup(tracked));
        self.epoch.store(epoch, Ordering::SeqCst);
        info!(epoch, peers = count, "p2p admission set updated");
    }

    /// The last peer-set index this handle registered.
    #[must_use]
    pub fn applied_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

/// Normalizes one tracked set: deduplicated, sorted, truncated to
/// `max_peers` entries so the tracker's size assert cannot fire.
fn admission_set(peers: Vec<[u8; 32]>, max_peers: usize) -> Vec<[u8; 32]> {
    let mut set: Vec<[u8; 32]> = peers;
    set.sort_unstable();
    set.dedup();
    set.truncate(max_peers);
    set
}

/// A lightweight block announcement: height + SHA-256 digest.
#[derive(Debug, Clone, Copy)]
pub struct BlockAnnounce {
    pub height: u64,
    pub digest: Digest,
}

impl BlockAnnounce {
    /// 40-byte wire format: 8 height LE + 32 digest.
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(40);
        buf.extend_from_slice(&self.height.to_le_bytes());
        buf.extend_from_slice(&self.digest);
        buf
    }

    fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() != 40 {
            return None;
        }
        let height = u64::from_le_bytes(buf[..8].try_into().ok()?);
        let digest: [u8; 32] = buf[8..].try_into().ok()?;
        Some(Self {
            height,
            digest: digest.into(),
        })
    }
}

/// Broadcasts blocks to all connected peers.
#[derive(Clone)]
pub struct BlockBroadcaster {
    mailbox: commonware_broadcast::buffered::Mailbox<
        commonware_cryptography::ed25519::PublicKey,
        BlockMessage,
    >,
    announce_tx: Arc<Mutex<AnnounceSender>>,
}

impl BlockBroadcaster {
    /// Sends a block to all peers via the broadcast engine.
    pub fn broadcast(&self, block: podseq_core::Block) -> bool {
        let msg = BlockMessage::from(block);
        let height = msg.0.header.height;
        let digest = msg.digest();
        let digested = Digested::precomputed(msg, digest);
        let accepted = self.mailbox.broadcast(Recipients::All, digested).accepted();
        if accepted {
            debug!(height, ?digest, "block broadcast accepted");
        } else {
            warn!(height, ?digest, "block broadcast rejected (backpressure)");
        }
        accepted
    }

    /// Lets full nodes pull a block from the broadcast cache by its digest.
    ///
    /// # Panics
    ///
    /// Panics if the announce sender's mutex is poisoned (a prior send panicked
    /// while holding the lock).
    pub fn announce(&self, height: u64, digest: &Digest) {
        let announce = BlockAnnounce {
            height,
            digest: *digest,
        };
        let data = IoBuf::from(announce.encode());
        let mut sender = self.announce_tx.lock().unwrap();
        let _ = sender.send(Recipients::All, data, false);
        debug!(height, ?digest, "block announce submitted");
    }
}

/// Receives blocks from the p2p broadcast engine.
pub struct BlockReceiver {
    mailbox: commonware_broadcast::buffered::Mailbox<
        commonware_cryptography::ed25519::PublicKey,
        BlockMessage,
    >,
    announce_rx: Arc<tokio::sync::Mutex<Option<AnnounceReceiver>>>,
}

impl BlockReceiver {
    /// Waits for the block with `digest` to arrive, then returns it.
    pub async fn receive(&self, digest: &Digest) -> Option<podseq_core::Block> {
        let receiver = self.mailbox.subscribe(*digest);
        receiver
            .await
            .ok()
            .map(|msg| podseq_core::Block::from(Arc::unwrap_or_clone(msg.inner_shared())))
    }

    /// Polls for the next block announce with a short timeout; returns None if none arrives.
    pub async fn poll_next(&self) -> Option<BlockAnnounce> {
        use commonware_p2p::Receiver;
        let mut rx_guard = self.announce_rx.lock().await;
        let rx = rx_guard.as_mut()?;
        // Short timeout so the caller's loop isn't blocked.
        let recv_fut = rx.recv();
        match tokio::time::timeout(std::time::Duration::from_millis(100), recv_fut).await {
            Ok(Ok((_peer, buf))) => {
                if buf.len() == 40 {
                    if let Some(a) = BlockAnnounce::decode(buf.as_ref()) {
                        return Some(a);
                    }
                }
                debug!(len = buf.len(), "malformed block announce; skipping");
                None
            }
            _ => None,
        }
    }
}

/// A fully initialized p2p node ready to broadcast and receive blocks.
pub struct P2pNode {
    broadcaster: BlockBroadcaster,
    receiver: BlockReceiver,
    admission: AdmissionHandle,
    _handles: P2pHandles,
}

impl P2pNode {
    /// Builds the network, broadcast engine, and announce channel, then starts background tasks.
    /// `context` is consumed via `context.child()`; obtain it inside `Runner::start()`.
    ///
    /// # Errors
    ///
    /// Returns an error if the p2p key is missing from the config, cannot be
    /// read or written, or a bootstrap peer pubkey is malformed.
    ///
    /// # Panics
    ///
    /// Panics if a channel rate limit or the derived mailbox bound overflows.
    pub fn new(context: &commonware_runtime::tokio::Context, config: &P2pConfig) -> Result<Self> {
        let key_path = config.key_path.as_ref().map(std::path::PathBuf::from);
        let key_path = key_path.as_deref();
        let signer = if let Some(path) = key_path {
            crate::load_or_generate_key(path)?
        } else {
            anyhow::bail!("p2p key path is required; set key_path in config");
        };

        let our_pk = signer.public_key();
        let pk_hex = {
            use commonware_codec::Encode;
            hex::encode(our_pk.encode())
        };
        info!(pubkey = %pk_hex, "p2p identity");
        let own: [u8; 32] = {
            use commonware_codec::Encode;
            let encoded = our_pk.encode();
            encoded
                .as_ref()
                .try_into()
                .expect("ed25519 public key is 32 bytes")
        };

        let bootstrapper_keys: Vec<[u8; 32]> = config
            .bootstrap_peers
            .iter()
            .map(|(hex_key, _)| {
                let mut bytes = [0u8; 32];
                hex::decode_to_slice(hex_key, &mut bytes).with_context(|| {
                    format!("bootstrap peer pubkey must be valid 64-char hex, got {hex_key}")
                })?;
                Ok::<_, anyhow::Error>(bytes)
            })
            .collect::<Result<_>>()?;
        let bootstrappers: Vec<_> = bootstrapper_keys
            .iter()
            .copied()
            .zip(config.bootstrap_peers.iter().map(|(_, addr)| *addr))
            .map(|(bytes, addr)| {
                let pk = load_pubkey(bytes)?;
                Ok::<_, anyhow::Error>((pk, Ingress::from(addr)))
            })
            .collect::<Result<_>>()?;

        let dialable_addr = config.dialable_addr.unwrap_or(config.listen_addr);

        // Static mode registers `self ∪ bootstrap_peers` at startup: the
        // discovery tracker refuses connections from peers outside the
        // tracked set (`directory.eligible`), so tracking only self — the
        // original wiring — made every remote peer ineligible and block
        // dissemination unreachable. Registry mode instead registers no
        // startup set: tracked-set content must be identical per index on
        // every node (the gossip kills peers whose content disagrees), so
        // sets are only ever registered from the chain-derived book under
        // its on-chain version.
        let initial_set = initial_tracked_set(config, own, &bootstrapper_keys)?;
        let p2p_cfg = discovery::Config::local(
            handshake(signer.clone()),
            &config.application_namespace,
            config.listen_addr,
            dialable_addr,
            bootstrappers,
            NZUsize!(config.max_peers_per_set),
            config.max_message_size,
            NZUsize!(config.mailbox_size),
        );

        let (mut network, mut oracle) = discovery::Network::new(context.child("network"), p2p_cfg);

        if let Some(set) = initial_set {
            oracle.track(0, set);
        }
        info!("p2p network initialized");

        let (bc_sender, bc_receiver) = network.register(
            BLOCK_CHANNEL,
            Quota::per_second(std::num::NonZeroU32::new(256).unwrap()),
        );
        let (announce_sender, announce_receiver) = network.register(
            ANNOUNCE_CHANNEL,
            Quota::per_second(std::num::NonZeroU32::new(512).unwrap()),
        );

        let bc_cfg = commonware_broadcast::buffered::Config {
            public_key: our_pk.clone(),
            mailbox_size: NZUsize!(config.broadcast_mailbox_size),
            deque_size: config.broadcast_cache_size,
            priority: false,
            codec_config: commonware_codec::RangeCfg::from(..),
            peer_provider: oracle.clone(),
            strategy: Sequential,
        };
        let (engine, mailbox) = commonware_broadcast::buffered::Engine::<
            commonware_runtime::tokio::Context,
            commonware_cryptography::ed25519::PublicKey,
            BlockMessage,
            _,
            _,
        >::new(context.child("broadcast"), bc_cfg);

        let engine_handle = engine.start((bc_sender, bc_receiver));
        let network_handle = network.start();

        let handles = P2pHandles {
            _network_handle: network_handle,
            _engine_handle: engine_handle,
        };

        let mailbox = Arc::new(mailbox);
        let announce_tx = Arc::new(Mutex::new(announce_sender));
        let announce_rx = Arc::new(tokio::sync::Mutex::new(Some(announce_receiver)));

        let broadcaster = BlockBroadcaster {
            mailbox: (*mailbox).clone(),
            announce_tx,
        };
        let receiver = BlockReceiver {
            mailbox: (*mailbox).clone(),
            announce_rx,
        };
        let admission = AdmissionHandle {
            oracle: Arc::new(Mutex::new(oracle.clone())),
            max_peers: config.max_peers_per_set,
            epoch: Arc::new(AtomicU64::new(0)),
        };

        info!("p2p node started");
        Ok(Self {
            broadcaster,
            receiver,
            admission,
            _handles: handles,
        })
    }

    /// Returns the runtime admission handle for updating the tracked set.
    #[must_use]
    pub fn admission(&self) -> AdmissionHandle {
        self.admission.clone()
    }

    /// Returns a cloneable handle for broadcasting blocks and announces.
    #[must_use]
    pub fn broadcaster(&self) -> BlockBroadcaster {
        self.broadcaster.clone()
    }

    /// Returns the block receiver.
    #[must_use]
    pub fn receiver(&self) -> BlockReceiver {
        BlockReceiver {
            mailbox: self.receiver.mailbox.clone(),
            announce_rx: Arc::clone(&self.receiver.announce_rx),
        }
    }
}

fn load_pubkey(bytes: [u8; 32]) -> Result<commonware_cryptography::ed25519::PublicKey> {
    use commonware_codec::Read;
    let mut buf = bytes::Bytes::copy_from_slice(&bytes);
    commonware_cryptography::ed25519::PublicKey::read_cfg(&mut buf, &())
        .context("decoding 32-byte ed25519 public key")
}

/// The static-mode startup set: `self ∪ bootstrap_peers` under index 0.
///
/// `None` in registry mode — see [`P2pConfig::registry_admission`] for why
/// the startup set must not exist there.
fn initial_tracked_set(
    config: &P2pConfig,
    own: [u8; 32],
    bootstrapper_keys: &[[u8; 32]],
) -> Result<Option<Set<Pk>>> {
    if config.registry_admission {
        return Ok(None);
    }
    let tracked = admission_set(
        std::iter::once(own)
            .chain(bootstrapper_keys.iter().copied())
            .collect(),
        config.max_peers_per_set,
    );
    let tracked: Vec<Pk> = tracked
        .iter()
        .copied()
        .map(load_pubkey)
        .collect::<Result<_>>()?;
    Ok(Some(Set::from_iter_dedup(tracked)))
}

#[cfg(test)]
mod admission_tests {
    use super::admission_set;

    #[test]
    fn dedupes_and_sorts_identities() {
        let a = [1u8; 32];
        let b = [2u8; 32];

        assert_eq!(admission_set(vec![b, a, b], 8), vec![a, b]);
    }

    #[test]
    fn truncates_to_the_tracker_bound() {
        let set: Vec<[u8; 32]> = (0u8..10).map(|i| [i; 32]).collect();

        let truncated = admission_set(set, 4);

        assert_eq!(truncated.len(), 4);
        // Lowest-sorted identities survive truncation, deterministically.
        assert_eq!(truncated[0], [0u8; 32]);
        assert_eq!(truncated[3], [3u8; 32]);
    }

    #[test]
    fn an_empty_book_admits_nobody() {
        assert_eq!(admission_set(Vec::new(), 8), Vec::<[u8; 32]>::new());
    }

    #[test]
    fn a_zero_bound_admits_nobody() {
        let a = [1u8; 32];

        assert_eq!(admission_set(vec![a], 0), Vec::<[u8; 32]>::new());
    }
}
