//! Full-stack e2e: real `podseq` binary + Reth + public Sui/Walrus testnet.
//!
//! Verifies the production binary, end to end:
//!   - podseq produces blocks (Reth `eth_blockNumber` advances).
//!   - A user tx we send lands on Reth.
//!   - podseq settles on Sui: `latest_height` advances past 0.
//!   - The Walrus blob for a settled height decodes to a block at that height.
//!
//! Skips itself when no funded Sui key is available (CI secret `SUI_SIGNER_KEY`
//! or `docker/secrets/sui.key` locally). Slow: real Sui checkpoint latency +
//! Walrus publication, several minutes end-to-end.

use std::str::FromStr;
use std::time::{Duration, Instant};

use alloy_consensus::{TxEip1559, TypedTransaction};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TxSigner;
use alloy_primitives::{Address, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{anyhow, bail, Context, Result};
use podseq_core::{Block, DataAvailability as _};
use podseq_e2e::FullStack;
use podseq_sui::{
    settlement::{commitment_at, latest_height, table_uid},
    Client as SuiClient, Config as SuiConfig,
};
use serde::Deserialize;

const RPC_PORT: u16 = 18745;
const ENGINE_PORT: u16 = 18751;

/// Hardhat dev account funded by `examples/reth-genesis.json`.
const GENESIS_PKEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const GENESIS_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

const SUI_RPC: &str = "https://fullnode.testnet.sui.io:443";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_stack_produces_settles_and_serves_blobs() -> Result<()> {
    require_docker();
    let stack = FullStack::start(RPC_PORT, ENGINE_PORT)
        .await
        .context("starting full stack")?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let rpc_url = stack.ports().rpc_url();

    // podseq auto-deploys settlement on first start, which takes real Sui gas +
    // checkpoint latency. Wait for the deploy to finish by polling the config
    // file for the registry_id (podseq writes it back after auto-deploy).
    let registry_id = {
        let deadline = Instant::now() + Duration::from_secs(300);
        let id = loop {
            if stack.podseq_exited() {
                stack.dump_logs();
                bail!("podseq container exited before settling; see logs above");
            }
            if let Some(id) = read_registry_id(&stack.config_path())? {
                break id;
            }
            if Instant::now() >= deadline {
                stack.dump_logs();
                bail!("timeout waiting for settlement auto-deploy (no registry_id in config)");
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        println!("e2e: registry_id = {id}");
        id
    };

    // 1. podseq is producing: Reth height advances past 0.
    let produced_height = wait_for_height(&http, &rpc_url, 1, Duration::from_secs(60)).await?;
    println!("e2e: podseq produced up to height {produced_height}");

    // 2. A user tx lands on Reth. Send a 1-wei self-transfer from the funded
    //    genesis account; assert the receipt appears within a minute.
    let signer = PrivateKeySigner::from_str(GENESIS_PKEY)?;
    let from: Address = GENESIS_ADDRESS.parse()?;
    let chain_id = eth_chain_id(&http, &rpc_url).await?;
    let nonce = eth_get_transaction_count(&http, &rpc_url, from).await?;
    let tx_hash = send_self_transfer(&http, &rpc_url, &signer, from, chain_id, nonce).await?;
    println!("e2e: sent user tx {tx_hash}");

    let receipt_deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() >= receipt_deadline {
            bail!("user tx {tx_hash} was never included");
        }
        if let Some(receipt) = eth_get_transaction_receipt(&http, &rpc_url, &tx_hash).await? {
            if receipt.get("status").is_some() {
                println!("e2e: user tx included");
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 3. Settlement advances on Sui: latest_height moves past 0.
    let settled = wait_for_settlement(&registry_id, Duration::from_secs(300)).await?;
    println!("e2e: settlement at height {settled}");

    // 4. Walrus blob for a settled height is retrievable and decodes to a block
    //    at that height. Proves DA + settlement are coherent end to end.
    let table = table_uid(SUI_RPC, &registry_id).await?;
    let blob_id = commitment_at(SUI_RPC, &table, settled)
        .await?
        .with_context(|| format!("commitment_at returned None for settled height {settled}"))?;

    let sui_client = SuiClient::new(SuiConfig {
        aggregator_url: "https://aggregator.walrus-testnet.walrus.space".into(),
        ..SuiConfig::default()
    })?;
    let blocks: Vec<Block> = sui_client
        .fetch(&blob_id)
        .await
        .context("fetching blob from Walrus")?;
    let matching = blocks
        .iter()
        .find(|b| b.header.height == settled)
        .with_context(|| {
            format!(
                "settled height {settled} not in blob ({} blocks)",
                blocks.len()
            )
        })?;
    assert!(
        matching.signature.is_some(),
        "settled block must carry a sequencer signature"
    );
    println!("e2e: blob for height {settled} fetched and decoded");

    Ok(())
}

fn require_docker() {
    let available = std::process::Command::new("docker")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok();
    if !available {
        panic!("docker is not available; the full-stack e2e test requires it");
    }
}

/// Reads `sui.registry_id` from the podseq config file, returning it only once
/// it is set and looks like a Sui object id (`0x` + 64 hex). Returns `Ok(None)`
/// when the field is absent (auto-deploy not finished yet).
fn read_registry_id(path: &std::path::Path) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let value: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let id = match value
        .get("sui")
        .and_then(|s| s.get("registry_id"))
        .and_then(|v| v.as_str())
    {
        Some(id) => id,
        None => return Ok(None),
    };
    if id.starts_with("0x") && id.len() == 66 {
        Ok(Some(id.to_string()))
    } else {
        Ok(None)
    }
}

async fn wait_for_height(
    http: &reqwest::Client,
    rpc_url: &str,
    min: u64,
    timeout: Duration,
) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    loop {
        match eth_block_number(http, rpc_url).await {
            Ok(h) if h >= min => return Ok(h),
            Ok(_) => {}
            Err(e) => eprintln!("e2e: eth_blockNumber retrying: {e}"),
        }
        if Instant::now() >= deadline {
            bail!("wait_for_height({min}) timed out after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_settlement(registry_id: &str, timeout: Duration) -> Result<u64> {
    let deadline = Instant::now() + timeout;
    loop {
        match latest_height(SUI_RPC, registry_id).await {
            Ok(h) if h > 0 => return Ok(h),
            Ok(_) => {}
            Err(e) => eprintln!("e2e: latest_height RPC error (retrying): {e}"),
        }
        if Instant::now() >= deadline {
            bail!("settlement never advanced on Sui after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[derive(Deserialize)]
struct RpcEnvelope<R> {
    result: Option<R>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

async fn rpc_call<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    rpc_url: &str,
    method: &str,
    params: Vec<serde_json::Value>,
) -> Result<T> {
    let env: RpcEnvelope<T> = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = env.error {
        bail!("rpc error ({method}): {e}");
    }
    env.result
        .ok_or_else(|| anyhow!("rpc ({method}) returned no result"))
}

async fn eth_block_number(http: &reqwest::Client, rpc_url: &str) -> Result<u64> {
    let s: String = rpc_call(http, rpc_url, "eth_blockNumber", vec![]).await?;
    Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
}

async fn eth_chain_id(http: &reqwest::Client, rpc_url: &str) -> Result<u64> {
    let s: String = rpc_call(http, rpc_url, "eth_chainId", vec![]).await?;
    Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
}

async fn eth_get_transaction_count(
    http: &reqwest::Client,
    rpc_url: &str,
    address: Address,
) -> Result<u64> {
    let s: String = rpc_call(
        http,
        rpc_url,
        "eth_getTransactionCount",
        vec![format!("{address:?}").into(), "pending".into()],
    )
    .await?;
    Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
}

async fn eth_get_transaction_receipt(
    http: &reqwest::Client,
    rpc_url: &str,
    tx_hash: &str,
) -> Result<Option<serde_json::Value>> {
    // eth_getTransactionReceipt returns null when the tx hasn't been mined yet.
    // Don't use rpc_call (which errors on null result); handle it directly.
    let resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getTransactionReceipt",
            "params": [tx_hash],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = resp.get("error") {
        bail!("rpc error (eth_getTransactionReceipt): {error}");
    }
    match resp.get("result") {
        Some(serde_json::Value::Null) | None => Ok(None),
        Some(v) => Ok(Some(v.clone())),
    }
}

async fn send_self_transfer(
    http: &reqwest::Client,
    rpc_url: &str,
    signer: &PrivateKeySigner,
    from: Address,
    chain_id: u64,
    nonce: u64,
) -> Result<String> {
    let price_hex: String = rpc_call(http, rpc_url, "eth_gasPrice", vec![]).await?;
    let price = u128::from_str_radix(price_hex.trim_start_matches("0x"), 16)?;

    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: price,
        max_priority_fee_per_gas: price.min(2_000_000_000),
        to: TxKind::Call(from),
        value: U256::from(1),
        access_list: Default::default(),
        input: Default::default(),
    };
    let sig = signer.sign_transaction(&mut tx).await?;
    let typed: TypedTransaction = tx.into();
    let envelope = typed.into_envelope(sig);
    let raw = envelope.encoded_2718();

    let hash: String = rpc_call(
        http,
        rpc_url,
        "eth_sendRawTransaction",
        vec![format!("0x{}", hex::encode(&raw)).into()],
    )
    .await?;
    Ok(hash)
}
