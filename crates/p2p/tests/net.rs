//! Integration: a block broadcast by one node reaches a peered node.
//!
//! Exercises the full dissemination path — authenticated discovery, SAKE/CUPS
//! handshakes, the buffered broadcast engine, and the announce side-channel —
//! on loopback. The receiver learns the digest from the announce channel and
//! pulls the block via a late subscribe, which also covers the cache-hit path
//! a full node hits when it subscribes after the sequencer already broadcast.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// `Digestible` supplies `BlockMessage::digest`, used to address the block in the engine cache.
use commonware_cryptography::Digestible as _;
use podseq_core::runtime::{Runner, RunnerTrait as _, Supervisor as _};
use podseq_core::{Block, Header};
use podseq_p2p::{load_or_generate_key, read_pubkey_hex, BlockMessage, P2pConfig, P2pNode};

/// A block with deterministic contents.
fn sample_block(height: u64, fill: u8) -> Block {
    Block {
        header: Header {
            height,
            parent_hash: [0; 32],
            state_root: [fill; 32],
            timestamp: height * 1000,
        },
        data: vec![fill; 128],
        signature: Some([0xcc; 64]),
    }
}

/// A free loopback port (bind-and-drop; small race, acceptable for tests).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind 127.0.0.1:0")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A fresh identity key file in a unique temp dir.
fn temp_key(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "podseq-p2p-net-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir.join("p2p.key")
}

fn config(
    key: &std::path::Path,
    listen: SocketAddr,
    bootstrappers: Vec<(String, SocketAddr)>,
) -> P2pConfig {
    P2pConfig {
        key_path: Some(key.display().to_string()),
        listen_addr: listen,
        dialable_addr: Some(listen),
        application_namespace: b"podseq-p2p-net-test".to_vec(),
        bootstrap_peers: bootstrappers,
        ..Default::default()
    }
}

#[test]
fn dynamically_admitted_peer_receives_broadcasts() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init()
        .ok();
    let outcome =
        Runner::default().start(|context| async move { run_dynamic_admission(context).await });
    outcome.expect("dynamic admission failed");
}

async fn run_dynamic_admission(context: podseq_core::runtime::Context) -> Result<(), String> {
    let key_a = temp_key("dyn-a");
    let key_b = temp_key("dyn-b");
    load_or_generate_key(&key_a).expect("key a");
    load_or_generate_key(&key_b).expect("key b");
    let pk_a_hex = read_pubkey_hex(&key_a).expect("pubkey a");
    let pk_b_hex = read_pubkey_hex(&key_b).expect("pubkey b");
    let sequencer_identity = decode_identity(&pk_a_hex)?;
    let fullnode_identity = decode_identity(&pk_b_hex)?;

    let addr_a: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("addr a");
    let addr_b: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("addr b");

    // The registry-committee topology: tracked sets come from the chain, so
    // neither node registers a startup set. The full node (B) keeps the
    // sequencer (A) as a bootstrapper for its dial address — tracked sets
    // carry identities only — while A needs no configuration at all.
    let mut cfg_a = config(&key_a, addr_a, vec![]);
    cfg_a.registry_admission = true;
    let mut cfg_b = config(&key_b, addr_b, vec![(pk_a_hex, addr_a)]);
    cfg_b.registry_admission = true;
    let node_a = P2pNode::new(&context.child("a"), &cfg_a).map_err(|e| format!("node a: {e}"))?;
    let node_b = P2pNode::new(&context.child("b"), &cfg_b).map_err(|e| format!("node b: {e}"))?;

    let broadcaster = node_a.broadcaster();
    let receiver = node_b.receiver();

    // Phase 1: book version 0 (empty, or unread) — nobody is admitted and
    // A bounces B before any handshake.
    let block = sample_block(7, 0x01);
    let digest = BlockMessage::from(block.clone()).digest();
    let unadmitted_until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < unadmitted_until {
        broadcaster.announce(7, &digest);
        if receiver.poll_next().await.is_some() {
            return Err("announce reached a peer that was never admitted".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Phase 2: the registry book lands at version 1 listing both committee
    // members; each node registers the identical chain-derived content
    // under the identical peer-set index.
    node_a
        .admission()
        .admit(1, [sequencer_identity, fullnode_identity]);
    node_b
        .admission()
        .admit(1, [sequencer_identity, fullnode_identity]);

    // Phase 3: with both sides tracked, the announce side-channel delivers.
    let deadline = Instant::now() + Duration::from_secs(75);
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for peering after admission".into());
        }
        broadcaster.announce(7, &digest);
        if receiver.poll_next().await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Phase 4: admission stability. Peer-set gossip compares tracked-set
    // content per index and KILLS peers on disagreement; a divergent
    // admission scheme shows up as a connect/kill loop here. Surviving one
    // full gossip cycle (5s in Config::local) and re-delivering proves the
    // sets agree.
    tokio::time::sleep(Duration::from_secs(6)).await;
    broadcaster.broadcast(block.clone());
    let delivered = tokio::time::timeout(Duration::from_secs(30), receiver.receive(&digest))
        .await
        .map_err(|_| "connection did not survive the gossip cycle".to_string())?
        .ok_or("subscribe canceled")?;
    assert_eq!(delivered.header.height, 7);
    Ok(())
}

/// Decodes a 64-char hex p2p identity into its 32 bytes.
fn decode_identity(hex: &str) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex, &mut out).map_err(|e| format!("identity hex: {e}"))?;
    Ok(out)
}

#[test]
fn block_flows_from_broadcaster_to_peered_receiver() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init()
        .ok();
    let outcome = Runner::default().start(|context| async move { run_exchange(context).await });
    outcome.expect("p2p exchange failed");
}

async fn run_exchange(context: podseq_core::runtime::Context) -> Result<(), String> {
    let key_a = temp_key("a");
    let key_b = temp_key("b");
    load_or_generate_key(&key_a).expect("key a");
    load_or_generate_key(&key_b).expect("key b");
    let pk_a_hex = read_pubkey_hex(&key_a).expect("pubkey a");
    let pk_b_hex = read_pubkey_hex(&key_b).expect("pubkey b");

    let addr_a: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("addr a");
    let addr_b: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("addr b");

    // Mutual committee config, as in a real deployment: each node lists the
    // other, so both bouncers admit the peer.
    let node_a = P2pNode::new(
        &context.child("a"),
        &config(&key_a, addr_a, vec![(pk_b_hex, addr_b)]),
    )
    .map_err(|e| format!("node a: {e}"))?;
    let node_b = P2pNode::new(
        &context.child("b"),
        &config(&key_b, addr_b, vec![(pk_a_hex, addr_a)]),
    )
    .map_err(|e| format!("node b: {e}"))?;

    let broadcaster = node_a.broadcaster();
    let receiver = node_b.receiver();

    // Phase 1: peering. Announces retry until B's side channel delivers
    // one, which proves the authenticated connection is up in both
    // directions.
    let block = sample_block(42, 0xab);
    let msg = BlockMessage::from(block.clone());
    let digest = msg.digest();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for peering".into());
        }
        broadcaster.announce(42, &digest);
        if receiver.poll_next().await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Phase 2: steady-state delivery. With the peers connected, the
    // broadcast engine pushes the block to B directly — the path a full
    // node takes for every block produced after it attached. (A block
    // broadcast before a peer connects is not re-pushed; historical
    // recovery is Walrus DA's job, not the p2p layer's.)
    assert!(
        broadcaster.broadcast(block.clone()),
        "broadcast engine rejected the block"
    );
    let received = tokio::time::timeout(Duration::from_secs(30), receiver.receive(&digest))
        .await
        .map_err(|_| "timed out waiting for block delivery".to_string())?
        .ok_or("subscribe canceled")?;
    let got = received;
    assert_eq!(got.header.height, 42);
    assert_eq!(got.data, block.data);
    assert_eq!(got.signature, block.signature);

    // Digest agreement end to end: the receiver computes the same digest for
    // the delivered block, so announces remain addressable.
    assert_eq!(BlockMessage::from(got).digest(), digest);
    Ok(())
}
