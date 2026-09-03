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
use podseq_core::Block;
use podseq_e2e::abi::{
    encode_adopt_bridge, encode_bridge_initialize, encode_create_bridge, encode_factory_initialize,
    selector,
};
use podseq_e2e::eth::{
    eth_balance_of, eth_call_address, eth_call_bool, eth_chain_id, eth_gas_price,
    eth_get_transaction_count, rpc_call,
};
use podseq_e2e::{
    require_docker, sui_signer_key as sui_signer_key_str, FullStack, BRIDGE_FACTORY,
    BRIDGE_TOKEN_PREDEPLOY, GENESIS_ADDRESS, GENESIS_PKEY,
};
use podseq_sui::{
    settlement::{commitment_at, latest_height, table_uid},
    Client as SuiClient, Config as SuiConfig,
};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_sdk_types::Identifier;
use sui_sdk_types::StructTag;
use sui_sdk_types::TypeTag;
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

const RPC_PORT: u16 = 18745;
const ENGINE_PORT: u16 = 18751;

const SUI_RPC: &str = "https://fullnode.testnet.sui.io:443";

/// SUI coin type bridged by the test.
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
    bootstrap_l2_bridge(&http, &rpc_url, &relayer_signer, chain_id).await?;
    println!("e2e: factory initialized, predeployed SUI token adopted");
    wait_for_relayer_ready(&http, &rpc_url, BRIDGE_FACTORY, relayer_addr).await?;
    println!("e2e: relayer is relaying");

    // Load the Sui signer to submit the deposit.
    let sui_key = sui_signer_key()?;
    let sender = sui_key.public_key().derive_address();
    let l2_recipient: [u8; 20] = GENESIS_ADDRESS.parse::<Address>()?.into();

    // Direction 1: Sui deposit → L2 mint.
    let balance_before =
        eth_balance_of(&http, &rpc_url, BRIDGE_TOKEN_PREDEPLOY, GENESIS_ADDRESS).await?;
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
        BRIDGE_TOKEN_PREDEPLOY,
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
        BRIDGE_TOKEN_PREDEPLOY,
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

/// Checks the funded Sui key has enough balance for settlement auto-deploy
/// (~0.03 SUI). Each test run deploys fresh because the workdir is ephemeral,
/// so the key drains over time and must be refilled from the testnet faucet.
async fn preflight_sui_balance() -> Result<()> {
    let key_str = sui_signer_key_str()?.context("no funded Sui key available")?;
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
        match podseq_e2e::eth::eth_block_number(http, rpc_url).await {
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
    let price = eth_gas_price(http, rpc_url).await?;

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
    let key_str = sui_signer_key_str()?.context("no funded Sui key available")?;
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
    let price = eth_gas_price(http, rpc_url).await?;

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
/// Sends a signed tx from the relayer account to an L2 contract and waits for
/// a successful receipt, returning it.
#[allow(clippy::too_many_arguments)]
async fn send_contract_tx(
    http: &reqwest::Client,
    rpc_url: &str,
    relayer: &PrivateKeySigner,
    chain_id: u64,
    to: &str,
    calldata: Vec<u8>,
    gas_limit: u64,
    label: &str,
) -> Result<serde_json::Value> {
    let from = relayer.address();
    let nonce = eth_get_transaction_count(http, rpc_url, from).await?;
    let price = eth_gas_price(http, rpc_url).await?;

    // Dry-run via eth_call to surface the revert reason before spending gas.
    let call_resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{
                "from": format!("{from:?}"),
                "to": to,
                "data": format!("0x{}", hex::encode(&calldata)),
            }, "latest"],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(result) = call_resp.get("result").and_then(|v| v.as_str()) {
        if result.starts_with("0x08c379a0") {
            bail!(
                "{label} would revert: {}",
                podseq_e2e::abi::decode_revert_string(result)
            );
        }
    }

    let mut tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit,
        max_fee_per_gas: price,
        max_priority_fee_per_gas: price.min(2_000_000_000),
        to: TxKind::Call(Address::from_str(to)?),
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
            bail!("{label} tx {tx_hash} never confirmed");
        }
        if let Some(receipt) = eth_get_transaction_receipt(http, rpc_url, &tx_hash).await? {
            let status = receipt
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("0x0");
            if status.eq_ignore_ascii_case("0x1") {
                return Ok(receipt);
            }
            if status.eq_ignore_ascii_case("0x0") {
                bail!("{label} tx reverted; receipt: {receipt}");
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Brings up the L2 side of the bridge: initializes the factory, configures the
/// predeployed canonical SUI token, adopts it into the factory registry, and
/// exercises the factory's permissionless creation path with an unrelated coin
/// type. All calls are one-time on a fresh chain.
async fn bootstrap_l2_bridge(
    http: &reqwest::Client,
    rpc_url: &str,
    relayer: &PrivateKeySigner,
    chain_id: u64,
) -> Result<()> {
    let from = relayer.address();
    send_contract_tx(
        http,
        rpc_url,
        relayer,
        chain_id,
        BRIDGE_FACTORY,
        encode_factory_initialize(from),
        100_000,
        "factory initialize",
    )
    .await?;

    send_contract_tx(
        http,
        rpc_url,
        relayer,
        chain_id,
        BRIDGE_TOKEN_PREDEPLOY,
        encode_bridge_initialize("Bridged SUI", "SUI", COIN_TYPE, from),
        200_000,
        "bridge initialize",
    )
    .await?;

    send_contract_tx(
        http,
        rpc_url,
        relayer,
        chain_id,
        BRIDGE_FACTORY,
        encode_adopt_bridge(COIN_TYPE, Address::from_str(BRIDGE_TOKEN_PREDEPLOY)?),
        200_000,
        "adoptBridge",
    )
    .await?;

    // The factory's permissionless creation path: anyone (here the same
    // account) can create the canonical token for a new coin type.
    send_contract_tx(
        http,
        rpc_url,
        relayer,
        chain_id,
        BRIDGE_FACTORY,
        encode_create_bridge("Test Coin", "TST", "0x5::test::TST"),
        3_000_000,
        "createBridge",
    )
    .await?;
    Ok(())
}

/// Polls until the factory is initialized and its relayer matches: that is the
/// relayer's own readiness condition. Tokens are created on demand.
async fn wait_for_relayer_ready(
    http: &reqwest::Client,
    rpc_url: &str,
    factory: &str,
    expected_relayer: Address,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let init_sel = selector(b"initialized()");
    let relayer_sel = selector(b"relayer()");
    loop {
        if eth_call_bool(http, rpc_url, factory, &init_sel)
            .await
            .unwrap_or(false)
            && eth_call_address(http, rpc_url, factory, &relayer_sel)
                .await
                .is_ok_and(|r| r == expected_relayer)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("bridge factory never became ready (initialized + relayer set)");
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
    token: &str,
    chain_id: u64,
    sui_recipient: [u8; 32],
    amount: u64,
) -> Result<()> {
    let from = signer.address();
    let nonce = eth_get_transaction_count(http, rpc_url, from).await?;
    let price = eth_gas_price(http, rpc_url).await?;

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
        to: TxKind::Call(Address::from_str(token)?),
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

/// The vault's `withdraw_nonce`, read via the podseq Sui client.
async fn sui_vault_withdraw_nonce(vault_id: &str) -> Result<u64> {
    podseq_sui::bridge::vault_status(SUI_RPC, vault_id)
        .await
        .map(|status| status.withdraw_nonce)
        .map_err(|e| anyhow!("vault status: {e}"))
}
