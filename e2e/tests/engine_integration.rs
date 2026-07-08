//! Engine API integration: build, accept, and finalize blocks against a real Reth.

use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use anyhow::{bail, Context, Result};
use podseq_e2e::Stack;
use podseq_engine::{Engine, PARENT_BEACON_BLOCK_ROOT};

const RPC_PORT: u16 = 18545;
const ENGINE_PORT: u16 = 18551;

fn require_docker() {
    let available = std::process::Command::new("docker")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !available {
        eprintln!("skipping: docker is not available");
        std::process::exit(0);
    }
}

async fn genesis_head(engine: &Engine) -> Result<B256> {
    let hash = engine.current_head().await?;
    Ok(hash)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn build_accept_finalize_advances_head() -> Result<()> {
    require_docker();

    let stack = Stack::start(RPC_PORT, ENGINE_PORT)
        .await
        .context("starting e2e stack")?;
    let auth = podseq_engine::Auth::from_file(stack.jwt_path())?;
    let engine = Engine::new(&stack.ports().engine_url(), auth)?;

    let genesis = genesis_head(&engine).await?;
    let genesis_height = engine.block_number().await?;
    assert_eq!(genesis_height, 0, "fresh dev chain must start at height 0");

    // Drive one full produce cycle: forkchoiceUpdated with payload attributes
    // (build), newPayload + forkchoiceUpdated (accept), then finalize.
    let timestamp = 1;
    let attributes = PayloadAttributes {
        timestamp,
        prev_randao: B256::ZERO,
        suggested_fee_recipient: alloy_primitives::Address::ZERO,
        withdrawals: Some(vec![]),
        parent_beacon_block_root: Some(PARENT_BEACON_BLOCK_ROOT),
        ..Default::default()
    };
    let genesis_state = ForkchoiceState {
        head_block_hash: genesis,
        safe_block_hash: genesis,
        finalized_block_hash: genesis,
    };

    let built = engine.build(genesis_state, attributes).await?;
    assert_eq!(built.height, 1, "first produced block must be height 1");

    engine
        .accept(
            &built.payload,
            built.block_hash,
            built.block_hash,
            built.block_hash,
        )
        .await
        .context("accepting built payload")?;

    // Head must now reflect the new block on the real Reth node.
    let new_head = wait_for_height(&engine, 1).await?;
    assert_eq!(
        new_head, built.block_hash,
        "head hash must match the built payload"
    );

    // Finalize: safe and finalized must advance to the produced block.
    let head = engine.current_head().await?;
    engine
        .finalize(head, built.block_hash, built.block_hash)
        .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn produces_consecutive_blocks() -> Result<()> {
    require_docker();

    let stack = Stack::start(RPC_PORT + 10, ENGINE_PORT + 10)
        .await
        .context("starting e2e stack")?;
    let auth = podseq_engine::Auth::from_file(stack.jwt_path())?;
    let engine = Engine::new(&stack.ports().engine_url(), auth)?;

    let mut parent = genesis_head(&engine).await?;
    let mut timestamp = 1u64;

    for (timestamp_offset, target) in (1u64..).zip(1..=3) {
        timestamp += timestamp_offset;
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: alloy_primitives::Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(PARENT_BEACON_BLOCK_ROOT),
            ..Default::default()
        };
        let state = ForkchoiceState {
            head_block_hash: parent,
            safe_block_hash: parent,
            finalized_block_hash: parent,
        };
        let built = engine.build(state, attributes).await?;
        assert_eq!(built.height, target as u64);
        engine
            .accept(
                &built.payload,
                built.block_hash,
                built.block_hash,
                built.block_hash,
            )
            .await?;
        parent = built.block_hash;
    }

    let final_height = engine.block_number().await?;
    assert_eq!(final_height, 3, "chain must advance to height 3");
    Ok(())
}

/// Polls `eth_blockNumber` until it reaches `target` (or times out).
async fn wait_for_height(engine: &Engine, target: u64) -> Result<B256> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Some(hash)) = engine.block_by_number(target).await {
            return Ok(hash);
        }
        if std::time::Instant::now() >= deadline {
            bail!("timeout waiting for height {target}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
