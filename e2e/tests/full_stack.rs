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
use alloy_primitives::{keccak256, Address, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{anyhow, bail, Context, Result};
use podseq_core::Block;
use podseq_e2e::FullStack;
use podseq_sui::{
    settlement::{commitment_at, latest_height, table_uid},
    Client as SuiClient, Config as SuiConfig,
};
use serde::Deserialize;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_rpc::field::FieldMask;
use sui_rpc::field::FieldMaskUtil;
use sui_sdk_types::Identifier;
use sui_sdk_types::StructTag;
use sui_sdk_types::TypeTag;
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

const RPC_PORT: u16 = 18745;
const ENGINE_PORT: u16 = 18751;

/// Hardhat dev account funded by `examples/reth-genesis.json`.
const GENESIS_PKEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const GENESIS_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

const SUI_RPC: &str = "https://fullnode.testnet.sui.io:443";

/// Bridge predeploy address (genesis-planted bytecode, see `solidity/`).
const BRIDGE_PREDEPLOY: &str = "0x4200000000000000000000000000000000000010";
/// SUI coin type bridged by the test's `Bridge` instance.
const COIN_TYPE: &str = "0x2::sui::SUI";
/// Amount deposited and then withdrawn back, in MIST (0.01 SUI).
const BRIDGE_AMOUNT: u64 = 10_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_stack_produces_settles_and_serves_blobs() -> Result<()> {
    require_docker();

    // Preflight: settlement auto-deploy costs ~0.03 SUI per run. The funded key
    // drains over time; bail early with an actionable message instead of waiting
    // 10 minutes for the deploy to time out.
    preflight_sui_balance().await?;

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
            if let Some(id) = stack.read_config_string("sui", "registry_id")? {
                if id.starts_with("0x") && id.len() == 66 {
                    break id;
                }
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
    let table = sui_query_retrying(|| async {
        table_uid(SUI_RPC, &registry_id)
            .await
            .map_err(|e| anyhow!("table_uid: {e}"))
    })
    .await?;
    let blob_id = sui_query_retrying(|| async {
        commitment_at(SUI_RPC, &table, settled)
            .await
            .map_err(|e| anyhow!("commitment_at: {e}"))
    })
    .await?
    .with_context(|| format!("commitment_at returned None for settled height {settled}"))?;

    let sui_client = SuiClient::new(SuiConfig {
        aggregator_url: "https://aggregator.walrus-testnet.walrus.space".into(),
        ..SuiConfig::default()
    })?;
    // Walrus aggregators are eventually consistent and intermittently flaky;
    // the blob was published seconds ago. Retry transport errors and
    // transient read failures instead of failing a 10-minute run on a single
    // hiccup.
    let blocks: Vec<Block> = fetch_blob_retrying(&sui_client, &blob_id, Duration::from_secs(120))
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

    // ---- Bridge (both directions) ----
    // Settlement is already deployed; the bridge vault auto-inits from the same
    // package. Reusing the running stack avoids a second settlement deploy
    // (which would double the SUI gas cost).
    let package_id = wait_for_config_id(&stack, "sui", "settlement_package_id").await?;
    let vault_id = wait_for_config_id(&stack, "bridge", "vault_id").await?;
    println!("e2e: bridge vault = {vault_id}");

    let relayer_signer = load_relayer_signer(stack.relayer_key_path())?;
    let relayer_addr = relayer_signer.address();
    println!("e2e: relayer evm address = {relayer_addr}");

    // Fund the relayer from the genesis account, then initialize the L2 Bridge.
    fund_relayer(&http, &rpc_url, relayer_addr, chain_id).await?;
    println!("e2e: relayer funded");
    initialize_l2_bridge(&http, &rpc_url, &relayer_signer, chain_id).await?;
    println!("e2e: L2 bridge initialized");
    wait_for_relayer_ready(&http, &rpc_url, BRIDGE_PREDEPLOY).await?;
    println!("e2e: relayer is relaying");

    // Load the Sui signer to submit the deposit.
    let sui_key = sui_signer_key()?;
    let sender = sui_key.public_key().derive_address();
    let l2_recipient: [u8; 20] = GENESIS_ADDRESS.parse::<Address>()?.into();

    // Direction 1: Sui deposit → L2 mint.
    let balance_before = eth_balance_of(&http, &rpc_url, BRIDGE_PREDEPLOY, GENESIS_ADDRESS).await?;
    sui_deposit(
        &sui_key,
        sender,
        &package_id,
        &vault_id,
        l2_recipient,
        BRIDGE_AMOUNT,
    )
    .await?;
    println!("e2e: deposited {BRIDGE_AMOUNT} MIST on Sui");
    if wait_for_l2_balance(
        &http,
        &rpc_url,
        BRIDGE_PREDEPLOY,
        GENESIS_ADDRESS,
        balance_before + BRIDGE_AMOUNT,
        Duration::from_secs(180),
    )
    .await
    .is_err()
    {
        stack.dump_logs();
        bail!("mint not observed on L2 after deposit; see podseq logs above");
    }
    println!("e2e: mint observed on L2");

    // Direction 2: L2 burn → Sui release.
    let sui_recipient_bytes = sender.into_inner();
    let reserve_before =
        sui_query_retrying(|| async { sui_vault_withdraw_nonce(&vault_id).await }).await?;
    initiate_withdrawal(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        sui_recipient_bytes,
        BRIDGE_AMOUNT,
    )
    .await?;
    println!("e2e: burn initiated on L2");
    if wait_for_sui_release(&vault_id, reserve_before, Duration::from_secs(180))
        .await
        .is_err()
    {
        stack.dump_logs();
        bail!("Sui release not observed after burn; see podseq logs above");
    }
    println!("e2e: withdrawal released on Sui");

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
        eprintln!("skipping: docker is not available");
        std::process::exit(0);
    }
}

/// Checks the funded Sui key has enough balance for settlement auto-deploy
/// (~0.03 SUI). Each test run deploys fresh because the workdir is ephemeral,
/// so the key drains over time and must be refilled from the testnet faucet.
async fn preflight_sui_balance() -> Result<()> {
    let key_str = if let Ok(k) = std::env::var("SUI_SIGNER_KEY") {
        k.trim().to_string()
    } else {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("docker/secrets/sui.key");
        std::fs::read_to_string(path)?.trim().to_string()
    };
    let key =
        podseq_sui::parse_signer_key(&key_str).map_err(|e| anyhow!("invalid sui key: {e}"))?;
    let sender = key.public_key().derive_address();
    let balance = sui_query_retrying(|| async {
        podseq_sui::settlement::sui_balance(SUI_RPC, &sender.to_string())
            .await
            .context("checking SUI balance")
    })
    .await?;
    // Settlement deploy + bridge vault init + several commits need ~0.05 SUI.
    const MIN_BALANCE_MIST: u64 = 100_000_000; // 0.1 SUI
    if balance < MIN_BALANCE_MIST {
        let sui = balance as f64 / 1_000_000_000.0;
        bail!(
            "funded Sui key has {balance} MIST ({sui:.4} SUI); need at least \
             {MIN_BALANCE_MIST} (0.1 SUI) for settlement + bridge gas. \
             Refill from the testnet faucet."
        );
    }
    println!("e2e: SUI balance = {balance} MIST");
    Ok(())
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

/// Runs a read-only Sui query with retry. These calls race no transaction,
/// but the gRPC endpoint hiccups intermittently; a single failure would waste
/// the whole run.
async fn sui_query_retrying<F, Fut, T>(op: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, anyhow::Error>>,
{
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut backoff = Duration::from_secs(1);
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if Instant::now() < deadline => {
                eprintln!("e2e: sui query retrying: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Fetches a Walrus blob, retrying transient errors. The blob is published
/// seconds before this runs; the aggregator may 404 or reset until it propagates.
async fn fetch_blob_retrying(
    client: &SuiClient,
    blob_id: &podseq_core::BlobId,
    timeout: Duration,
) -> Result<Vec<Block>> {
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_secs(2);
    let max_backoff = Duration::from_secs(15);
    loop {
        match client.fetch_blob(blob_id).await {
            Ok(bytes) => match podseq_sui::wire::decode(&bytes) {
                Ok(blocks) => return Ok(blocks),
                Err(e) => return Err(e).context("decoding walrus blob"),
            },
            Err(e) if e.is_transient() => {
                if Instant::now() >= deadline {
                    bail!("walrus fetch of {blob_id:?} never succeeded after {timeout:?}: {e}");
                }
                eprintln!("e2e: walrus fetch retrying: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
            Err(e) => return Err(e).context("walrus fetch failed"),
        }
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

// ---------- Bridge helpers ----------

/// Polls the config for an object id podseq writes back.
async fn wait_for_config_id(stack: &FullStack, section: &str, key: &str) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if stack.podseq_exited() {
            stack.dump_logs();
            bail!("podseq exited before writing {section}.{key}");
        }
        if let Some(id) = stack.read_config_string(section, key)? {
            if id.starts_with("0x") && id.len() == 66 {
                return Ok(id);
            }
        }
        if Instant::now() >= deadline {
            stack.dump_logs();
            bail!("timeout waiting for {section}.{key} in config");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Loads the relayer EVM key (32-byte secp256k1 hex) written by the harness.
fn load_relayer_signer(path: &std::path::Path) -> Result<PrivateKeySigner> {
    let hex_str = std::fs::read_to_string(path)
        .with_context(|| format!("reading relayer key {}", path.display()))?;
    let bytes = hex::decode(hex_str.trim().trim_start_matches("0x"))?;
    Ok(PrivateKeySigner::from_slice(&bytes)?)
}

/// Loads the funded Sui signer key from env or file.
fn sui_signer_key() -> Result<Ed25519PrivateKey> {
    let key_str = if let Ok(k) = std::env::var("SUI_SIGNER_KEY") {
        k.trim().to_string()
    } else {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("docker/secrets/sui.key");
        std::fs::read_to_string(path)?.trim().to_string()
    };
    podseq_sui::parse_signer_key(&key_str).map_err(|e| anyhow!("invalid sui key: {e}"))
}

/// Sends 0.5 native token from the genesis account to the relayer.
async fn fund_relayer(
    http: &reqwest::Client,
    rpc_url: &str,
    relayer: Address,
    chain_id: u64,
) -> Result<()> {
    let genesis = PrivateKeySigner::from_str(GENESIS_PKEY)?;
    let from: Address = GENESIS_ADDRESS.parse()?;
    let nonce = eth_get_transaction_count(http, rpc_url, from).await?;
    let price_hex: String = rpc_call(http, rpc_url, "eth_gasPrice", vec![]).await?;
    let price = u128::from_str_radix(price_hex.trim_start_matches("0x"), 16)?;

    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: price,
        max_priority_fee_per_gas: price.min(2_000_000_000),
        to: TxKind::Call(relayer),
        // Genesis holds ~0.9996 ETH; send half so the transfer + gas fits.
        value: U256::from(500_000_000_000_000_000u64),
        access_list: Default::default(),
        input: Default::default(),
    };
    let sig = genesis.sign_transaction(&mut tx).await?;
    let typed: TypedTransaction = tx.into();
    let raw = typed.into_envelope(sig).encoded_2718();
    let _: String = rpc_call(
        http,
        rpc_url,
        "eth_sendRawTransaction",
        vec![format!("0x{}", hex::encode(&raw)).into()],
    )
    .await?;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() >= deadline {
            bail!("relayer funding tx never confirmed");
        }
        let bal_hex: String = rpc_call(
            http,
            rpc_url,
            "eth_getBalance",
            vec![format!("{relayer:?}").into(), "latest".into()],
        )
        .await?;
        if u128::from_str_radix(bal_hex.trim_start_matches("0x"), 16)? > 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Returns the first 4 bytes of `keccak256(signature)` — the Solidity selector.
fn selector(signature: &[u8]) -> Vec<u8> {
    keccak256(signature).0[..4].to_vec()
}

/// Reads a `uint256` length word (right-aligned in 32 bytes) as `usize`.
fn abi_word_len(word: &[u8]) -> usize {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&word[24..]);
    u64::from_be_bytes(arr) as usize
}

/// Decodes an ABI `string` whose 32-byte length word starts at `len_offset`.
/// `len_offset` is 32 for a direct `string` return and 36 for a revert payload.
fn decode_abi_string_at(bytes: &[u8], len_offset: usize) -> Result<String> {
    anyhow::ensure!(
        bytes.len() >= len_offset + 32,
        "abi string return too short"
    );
    let len = abi_word_len(&bytes[len_offset..len_offset + 32]);
    let data_pos = len_offset + 32;
    let end = data_pos
        .checked_add(len)
        .context("abi string length overruns usize")?;
    anyhow::ensure!(end <= bytes.len(), "abi string length overruns buffer");
    Ok(String::from_utf8_lossy(&bytes[data_pos..end]).to_string())
}

/// ABI-encodes `initialize(string,string,string,address)`.
fn encode_initialize(name: &str, symbol: &str, coin_type: &str, relayer: Address) -> Vec<u8> {
    let selector = selector(b"initialize(string,string,string,address)");
    let mut out = selector.to_vec();
    // Header: 3 string offsets + 1 static address = 4 words.
    let mut dynamic = Vec::new();
    let mut offset = 4 * 32;
    for s in [name, symbol, coin_type] {
        out.extend_from_slice(&U256::from(offset).to_be_bytes::<32>());
        let bytes = s.as_bytes();
        let padded_len = bytes.len().div_ceil(32) * 32;
        dynamic.extend_from_slice(&U256::from(bytes.len()).to_be_bytes::<32>());
        dynamic.extend_from_slice(bytes);
        dynamic.resize(dynamic.len() + (padded_len - bytes.len()), 0);
        offset += 32 + padded_len;
    }
    let mut addr_word = [0u8; 32];
    addr_word[12..].copy_from_slice(relayer.as_ref());
    out.extend_from_slice(&addr_word);
    out.extend_from_slice(&dynamic);
    out
}

/// Calls `Bridge.initialize(string,string,string,address)` on the L2 predeploy.
async fn initialize_l2_bridge(
    http: &reqwest::Client,
    rpc_url: &str,
    relayer: &PrivateKeySigner,
    chain_id: u64,
) -> Result<()> {
    let from = relayer.address();
    let nonce = eth_get_transaction_count(http, rpc_url, from).await?;
    let price_hex: String = rpc_call(http, rpc_url, "eth_gasPrice", vec![]).await?;
    let price = u128::from_str_radix(price_hex.trim_start_matches("0x"), 16)?;
    let calldata = encode_initialize("Bridged SUI", "SUI", COIN_TYPE, from);

    // Dry-run via eth_call to surface the revert reason before spending gas.
    let call_resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{
                "from": format!("{from:?}"),
                "to": BRIDGE_PREDEPLOY,
                "data": format!("0x{}", hex::encode(&calldata)),
            }, "latest"],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(result) = call_resp.get("result").and_then(|v| v.as_str()) {
        if result.starts_with("0x08c379a0") {
            bail!("initialize would revert: {}", decode_revert_string(result));
        }
    }

    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 500_000,
        max_fee_per_gas: price,
        max_priority_fee_per_gas: price.min(2_000_000_000),
        to: TxKind::Call(Address::from_str(BRIDGE_PREDEPLOY)?),
        value: U256::ZERO,
        access_list: Default::default(),
        input: calldata.into(),
    };
    let sig = relayer.sign_transaction(&mut tx).await?;
    let typed: TypedTransaction = tx.into();
    let raw = typed.into_envelope(sig).encoded_2718();
    let tx_hash: String = rpc_call(
        http,
        rpc_url,
        "eth_sendRawTransaction",
        vec![format!("0x{}", hex::encode(&raw)).into()],
    )
    .await?;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() >= deadline {
            bail!("initialize tx {tx_hash} never confirmed");
        }
        if let Some(receipt) = eth_get_transaction_receipt(http, rpc_url, &tx_hash).await? {
            let status = receipt
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("0x0");
            if status.eq_ignore_ascii_case("0x1") {
                return Ok(());
            }
            if status.eq_ignore_ascii_case("0x0") {
                bail!("initialize tx reverted; receipt: {receipt}");
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Decodes a Solidity revert string from an `eth_call` result.
fn decode_revert_string(hex_result: &str) -> String {
    let bytes = match hex::decode(hex_result.trim_start_matches("0x")) {
        Ok(b) => b,
        Err(_) => return hex_result.to_string(),
    };
    decode_abi_string_at(&bytes, 36).unwrap_or_else(|_| hex_result.to_string())
}

/// Polls `coinType()` on the predeploy; non-empty means the relayer accepts it.
async fn wait_for_relayer_ready(http: &reqwest::Client, rpc_url: &str, bridge: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let selector = selector(b"coinType()");
    loop {
        if let Ok(s) = eth_call_string(http, rpc_url, bridge, &selector).await {
            if !s.is_empty() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!("relayer never marked the L2 bridge ready");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Submits `bridge::deposit<SUI>(vault, coin, recipient_l2)` on Sui.
async fn sui_deposit(
    key: &Ed25519PrivateKey,
    sender: sui_sdk_types::Address,
    package_id: &str,
    vault_id: &str,
    recipient_l2: [u8; 20],
    amount: u64,
) -> Result<()> {
    use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionRequest;

    let mut rpc = sui_rpc::Client::new(SUI_RPC).map_err(|e| anyhow!("sui rpc: {e}"))?;
    let sui_type = TypeTag::from(StructTag::sui());
    let vault_id_addr = vault_id.parse().unwrap();

    // The gas coin races settlement's ongoing commits (~every 2s): Sui rejects
    // the tx when the object is locked or its version has moved. Rebuild and
    // retry fast enough to slip between commits.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut backoff = Duration::from_millis(500);
    loop {
        let vault_input = ObjectInput::new(vault_id_addr);
        let function = Function::new(
            package_id.parse().unwrap(),
            Identifier::new("bridge").unwrap(),
            Identifier::new("deposit").unwrap(),
        )
        .with_type_args(vec![sui_type.clone()]);

        let mut builder = TransactionBuilder::new();
        let coin_arg = builder.coin(StructTag::sui(), amount);
        let vault_arg = builder.object(vault_input);
        let recipient_arg = builder.pure(&recipient_l2.to_vec());
        builder.move_call(function, vec![vault_arg, coin_arg, recipient_arg]);
        builder.set_sender(sender);

        let attempt = async {
            let tx = builder
                .build(&mut rpc)
                .await
                .map_err(|e| anyhow!("build: {e}"))?;
            let signature = key
                .sign_transaction(&tx)
                .map_err(|e| anyhow!("sign: {e}"))?;
            let response = rpc
                .execute_transaction_and_wait_for_checkpoint(
                    ExecuteTransactionRequest::new(tx.into())
                        .with_signatures(vec![signature.into()]),
                    Duration::from_secs(30),
                )
                .await
                .map_err(|e| anyhow!("execute: {e}"))?;
            Ok::<_, anyhow::Error>(response)
        };

        match attempt.await {
            Ok(response) => {
                let response = response.into_inner();
                let status = response.transaction().effects().status().clone();
                if !status.success() {
                    bail!(
                        "deposit failed: {}",
                        status.error().description.clone().unwrap_or_default()
                    );
                }
                return Ok(());
            }
            Err(e) => {
                let msg = format!("{e}");
                let object_conflict = msg.contains("unavailable for consumption")
                    || msg.contains("already locked by a different transaction")
                    || msg.contains("ObjectDeleted")
                    || msg.contains("could not find the referenced object")
                    || msg.contains("provided version doesn't match");
                if !object_conflict || Instant::now() >= deadline {
                    return Err(e).context("execute deposit");
                }
                eprintln!("e2e: deposit retrying (object conflict): {msg}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(1));
            }
        }
    }
}

/// Calls `initiateWithdrawal(bytes32,uint256)` on the L2 Bridge (burns tokens).
async fn initiate_withdrawal(
    http: &reqwest::Client,
    rpc_url: &str,
    signer: &PrivateKeySigner,
    chain_id: u64,
    sui_recipient: [u8; 32],
    amount: u64,
) -> Result<()> {
    let from = signer.address();
    let nonce = eth_get_transaction_count(http, rpc_url, from).await?;
    let price_hex: String = rpc_call(http, rpc_url, "eth_gasPrice", vec![]).await?;
    let price = u128::from_str_radix(price_hex.trim_start_matches("0x"), 16)?;

    let selector = selector(b"initiateWithdrawal(bytes32,uint256)");
    let mut calldata = selector;
    calldata.extend_from_slice(&sui_recipient);
    let mut amount_word = [0u8; 32];
    amount_word[24..].copy_from_slice(&amount.to_be_bytes());
    calldata.extend_from_slice(&amount_word);

    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 100_000,
        max_fee_per_gas: price,
        max_priority_fee_per_gas: price.min(2_000_000_000),
        to: TxKind::Call(Address::from_str(BRIDGE_PREDEPLOY)?),
        value: U256::ZERO,
        access_list: Default::default(),
        input: calldata.into(),
    };
    let sig = signer.sign_transaction(&mut tx).await?;
    let typed: TypedTransaction = tx.into();
    let raw = typed.into_envelope(sig).encoded_2718();
    let tx_hash: String = rpc_call(
        http,
        rpc_url,
        "eth_sendRawTransaction",
        vec![format!("0x{}", hex::encode(&raw)).into()],
    )
    .await?;

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() >= deadline {
            bail!("withdrawal tx {tx_hash} never confirmed");
        }
        if let Some(receipt) = eth_get_transaction_receipt(http, rpc_url, &tx_hash).await? {
            if let Some(status) = receipt.get("status").and_then(|s| s.as_str()) {
                if status.eq_ignore_ascii_case("0x1") {
                    return Ok(());
                }
                if status.eq_ignore_ascii_case("0x0") {
                    bail!("withdrawal tx reverted");
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Polls the L2 balance until it reaches `target`.
async fn wait_for_l2_balance(
    http: &reqwest::Client,
    rpc_url: &str,
    token: &str,
    holder: &str,
    target: u64,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match eth_balance_of(http, rpc_url, token, holder).await {
            Ok(bal) if bal >= target => return Ok(()),
            Ok(_) => {}
            Err(e) if Instant::now() < deadline => eprintln!("e2e: eth_balanceOf retrying: {e}"),
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            bail!("L2 balance for {holder} never reached {target}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Polls the vault's withdraw_nonce until it advances (release observed).
async fn wait_for_sui_release(vault_id: &str, before: u64, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match sui_vault_withdraw_nonce(vault_id).await {
            Ok(now) if now != before => return Ok(()),
            Ok(_) => {}
            Err(e) if Instant::now() < deadline => eprintln!("e2e: sui vault nonce retrying: {e}"),
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            bail!("Sui vault withdraw_nonce never changed from {before}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Reads the vault's deposit_nonce from raw BCS contents.
/// Reads a little-endian u64 at `offset` from the vault's raw BCS contents.
async fn sui_vault_u64(vault_id: &str, offset: usize, label: &str) -> Result<u64> {
    use sui_rpc::proto::sui::rpc::v2::GetObjectRequest;

    let mut rpc = sui_rpc::Client::new(SUI_RPC).map_err(|e| anyhow!("sui rpc: {e}"))?;
    let response = rpc
        .ledger_client()
        .get_object(
            GetObjectRequest::new(
                &vault_id
                    .parse()
                    .map_err(|e: sui_sdk_types::AddressParseError| anyhow!("{e}"))?,
            )
            .with_read_mask(FieldMask::from_str("contents")),
        )
        .await
        .map_err(|e| anyhow!("get vault: {e}"))?;
    let bytes = response
        .into_inner()
        .object
        .and_then(|o| o.contents)
        .and_then(|c| c.value)
        .context("vault has no contents")?;
    let end = offset.checked_add(8).context("offset overflow")?;
    anyhow::ensure!(bytes.len() >= end, "vault BCS too short for {label}");
    Ok(u64::from_le_bytes(bytes[offset..end].try_into().unwrap()))
}

/// The vault's `withdraw_nonce` (BCS offset 40: after UID + deposit_nonce).
async fn sui_vault_withdraw_nonce(vault_id: &str) -> Result<u64> {
    sui_vault_u64(vault_id, 40, "withdraw_nonce").await
}

/// `balanceOf(address)` via eth_call.
async fn eth_balance_of(
    http: &reqwest::Client,
    rpc_url: &str,
    token: &str,
    holder: &str,
) -> Result<u64> {
    let selector = selector(b"balanceOf(address)");
    let mut addr_word = [0u8; 32];
    let holder_addr: Address = holder.parse()?;
    addr_word[12..].copy_from_slice(holder_addr.as_ref());
    let mut calldata = selector;
    calldata.extend_from_slice(&addr_word);

    let to: Address = token.parse()?;
    let resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{
                "to": format!("{to:?}"),
                "data": format!("0x{}", hex::encode(&calldata)),
            }, "latest"],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = resp.get("error") {
        bail!("eth_call balanceOf error: {error}");
    }
    let result = resp
        .get("result")
        .and_then(|v| v.as_str())
        .context("eth_call balanceOf: no result")?;
    Ok(u64::from_str_radix(result.trim_start_matches("0x"), 16)?)
}

/// eth_call returning a Solidity string (for coinType()).
async fn eth_call_string(
    http: &reqwest::Client,
    rpc_url: &str,
    to: &str,
    selector: &[u8],
) -> Result<String> {
    let to_addr: Address = to.parse()?;
    let resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{
                "to": format!("{to_addr:?}"),
                "data": format!("0x{}", hex::encode(selector)),
            }, "latest"],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = resp.get("error") {
        bail!("eth_call error: {error}");
    }
    let hex = resp.get("result").and_then(|v| v.as_str()).unwrap_or("0x");
    let bytes = hex::decode(hex.trim_start_matches("0x"))?;
    Ok(decode_abi_string_at(&bytes, 32).unwrap_or_default())
}

#[cfg(test)]
mod abi_tests {
    use super::*;

    /// Builds the ABI encoding of a Solidity `string` with optional 4-byte
    /// selector prefix (for revert payloads) and a 32-byte offset word (for
    /// direct `string` returns).
    fn encode_string(payload: &str, with_selector: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if with_selector {
            out.extend_from_slice(&[0x08, 0xc3, 0x79, 0xa0]); // Error(string) selector
        }
        // offset word = 0x20 (32)
        out.extend_from_slice(&[0u8; 31]);
        out.push(0x20);
        // length word (32 bytes, big-endian, value in low bytes)
        out.extend_from_slice(&[0u8; 24]);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        out.extend_from_slice(payload.as_bytes());
        let pad = (32 - (payload.len() % 32)) % 32;
        out.extend(std::iter::repeat_n(0u8, pad));
        out
    }

    #[test]
    fn abi_word_len_reads_right_aligned_value() {
        let mut word = vec![0u8; 32];
        word[24..].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(abi_word_len(&word), 0x0102030405060708);
    }

    #[test]
    fn decode_abi_string_reads_coin_type_return() {
        let bytes = encode_string("0x2::sui::SUI", false);
        assert_eq!(decode_abi_string_at(&bytes, 32).unwrap(), "0x2::sui::SUI");
    }

    #[test]
    fn decode_abi_string_reads_revert_payload() {
        let bytes = encode_string("bad coin type", true);
        assert_eq!(decode_abi_string_at(&bytes, 36).unwrap(), "bad coin type");
    }

    #[test]
    fn decode_abi_string_does_not_panic_on_full_length_word() {
        // Regression: a fully-populated 32-byte length word must error, not panic.
        let mut bytes = vec![0u8; 32];
        bytes.extend_from_slice(&[0xff; 32]);
        bytes.extend_from_slice(&[0u8; 32]);
        assert!(decode_abi_string_at(&bytes, 32).is_err());
    }

    #[test]
    fn decode_abi_string_rejects_short_buffer() {
        assert!(decode_abi_string_at(&[0u8; 10], 32).is_err());
        assert!(decode_abi_string_at(&[0u8; 10], 36).is_err());
    }

    #[test]
    fn decode_revert_string_roundtrips_known_payload() {
        let bytes = encode_string("invalid opcode", true);
        let hex = format!("0x{}", hex::encode(&bytes));
        assert_eq!(decode_revert_string(&hex), "invalid opcode");
    }
}
