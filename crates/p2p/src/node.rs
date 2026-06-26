//! P2p node wiring the Commonware discovery network and broadcast engine.
//!
//! Uses two p2p channels: block broadcast (ch. 0) and block announce (ch. 1).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use commonware_broadcast::Broadcaster as _;
use commonware_cryptography::{sha256::Digest, Digestible, Signer as _};
use commonware_p2p::{authenticated::discovery, Ingress, Manager, Recipients, Sender as RawSender};
use commonware_runtime::{IoBuf, Quota, Supervisor as _};
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

/// Holds the background task handles for the network and broadcast engine.
pub struct P2pHandles {
    _network_handle: commonware_runtime::Handle<()>,
    _engine_handle: commonware_runtime::Handle<()>,
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
        let accepted = self.mailbox.broadcast(Recipients::All, msg).accepted();
        if accepted {
            debug!(height, ?digest, "block broadcast accepted");
        } else {
            warn!(height, ?digest, "block broadcast rejected (backpressure)");
        }
        accepted
    }

    /// Lets full nodes pull a block from the broadcast cache by its digest.
    pub fn announce(&self, height: u64, digest: &Digest) {
        let announce = BlockAnnounce {
            height,
            digest: *digest,
        };
        let data = IoBuf::from(announce.encode());
        let mut sender = self.announce_tx.lock().unwrap();
        let _ = sender.send(Recipients::All, data, false);
        debug!(height, ?digest, "block announce sent");
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
        receiver.await.ok().map(|BlockMessage(block)| block)
    }

    /// Polls for the next block announce with a short timeout; returns None if none arrives.
    pub async fn poll_next(&self) -> Option<BlockAnnounce> {
        use commonware_p2p::Receiver;
        let mut rx_guard = self.announce_rx.lock().await;
        let rx = match rx_guard.as_mut() {
            Some(r) => r,
            None => return None,
        };
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
    _handles: P2pHandles,
}

impl P2pNode {
    /// Builds the network, broadcast engine, and announce channel, then starts background tasks.
    /// `context` is consumed via `context.child()`; obtain it inside `Runner::start()`.
    pub async fn new(
        context: commonware_runtime::tokio::Context,
        config: &P2pConfig,
    ) -> Result<Self> {
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

        let bootstrappers: Vec<_> = config
            .bootstrap_peers
            .iter()
            .map(|(hex_key, addr)| {
                let mut bytes = [0u8; 32];
                hex::decode_to_slice(hex_key, &mut bytes)
                    .expect("bootstrap peer pubkey must be valid 64-char hex");
                let pk = load_pubkey(bytes);
                (pk, Ingress::from(*addr))
            })
            .collect();

        let dialable_addr = config.dialable_addr.unwrap_or(config.listen_addr);

        let p2p_cfg = discovery::Config::local(
            signer.clone(),
            &config.application_namespace,
            config.listen_addr,
            dialable_addr,
            bootstrappers,
            config.max_message_size,
        );

        let (mut network, mut oracle) = discovery::Network::new(context.child("network"), p2p_cfg);

        oracle.track(0, Set::try_from([our_pk.clone()]).unwrap_or_default());
        info!("p2p network initialized");

        let (bc_sender, bc_receiver) = network.register(
            BLOCK_CHANNEL,
            Quota::per_second(std::num::NonZeroU32::new(256).unwrap()),
            config.broadcast_mailbox_size,
        );
        let (announce_sender, announce_receiver) = network.register(
            ANNOUNCE_CHANNEL,
            Quota::per_second(std::num::NonZeroU32::new(512).unwrap()),
            256,
        );

        let bc_cfg = commonware_broadcast::buffered::Config {
            public_key: our_pk.clone(),
            mailbox_size: NZUsize!(config.broadcast_mailbox_size),
            deque_size: config.broadcast_cache_size,
            priority: false,
            codec_config: commonware_codec::RangeCfg::from(..),
            peer_provider: oracle.clone(),
        };
        let (engine, mailbox) = commonware_broadcast::buffered::Engine::<
            commonware_runtime::tokio::Context,
            commonware_cryptography::ed25519::PublicKey,
            BlockMessage,
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

        info!("p2p node started");
        Ok(Self {
            broadcaster,
            receiver,
            _handles: handles,
        })
    }

    /// Returns a cloneable handle for broadcasting blocks and announces.
    pub fn broadcaster(&self) -> BlockBroadcaster {
        self.broadcaster.clone()
    }

    /// Returns the block receiver.
    pub fn receiver(&self) -> BlockReceiver {
        BlockReceiver {
            mailbox: self.receiver.mailbox.clone(),
            announce_rx: Arc::clone(&self.receiver.announce_rx),
        }
    }
}

fn load_pubkey(bytes: [u8; 32]) -> commonware_cryptography::ed25519::PublicKey {
    use commonware_codec::Read;
    let mut buf: &[u8] = &bytes;
    commonware_cryptography::ed25519::PublicKey::read_cfg(&mut buf, &())
        .expect("32 bytes must deserialize to a valid ed25519 public key")
}
