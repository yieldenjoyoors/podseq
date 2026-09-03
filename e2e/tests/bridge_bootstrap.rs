//! L2-side bridge e2e: validates the predeployed contracts end to end against
//! a real Reth, with no Sui dependency (and therefore no funded key).
//!
//! Covers the genesis predeploys, the one-time bootstrap
//! (`BridgeFactory.initialize` → `Bridge.initialize` → `adoptBridge`), the
//! factory's permissionless `createBridge` (and its duplicate rejection), the
//! adopted token's mint/withdrawal paths, and the `BridgeCreated` logs the
//! sequencer relayer rebuilds its token registry from.
//!
//! Blocks are produced by driving the Engine API directly, the same way the
//! sequencer does; sent transactions are picked up from the mempool.

use std::str::FromStr;
use std::time::{Duration, Instant};

use alloy_consensus::{TxEip1559, TypedTransaction};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TxSigner;
use alloy_primitives::{keccak256, Address, TxKind, B256, U256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{bail, Context, Result};
use podseq_e2e::Stack;
use podseq_engine::{Engine, PARENT_BEACON_BLOCK_ROOT};

const RPC_PORT: u16 = 19045;
const ENGINE_PORT: u16 = 19051;

/// Hardhat dev account funded by `examples/reth-genesis.json`.
const GENESIS_PKEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const GENESIS_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

/// BridgeFactory predeploy (genesis-planted bytecode, see `solidity/`).
const BRIDGE_FACTORY: &str = "0x4200000000000000000000000000000000000010";
/// Predeployed canonical SUI Bridge token (genesis-planted bytecode).
const BRIDGE_TOKEN_PREDEPLOY: &str = "0x4200000000000000000000000000000000000011";
const SUI_COIN_TYPE: &str = "0x2::sui::SUI";

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bridge_predeploys_bootstrap_and_relay_surfaces() -> Result<()> {
    require_docker();

    let stack = Stack::start(RPC_PORT, ENGINE_PORT)
        .await
        .context("starting e2e stack")?;
    let auth = podseq_engine::Auth::from_file(stack.jwt_path())?;
    let engine = Engine::new(&stack.ports().engine_url(), auth)?;
    let rpc_url = stack.ports().rpc_url();

    // Produce blocks in the background so transactions confirm.
    let driver = tokio::spawn(async move {
        drive_blocks(&engine).await.expect("block driver");
    });

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let signer = PrivateKeySigner::from_str(GENESIS_PKEY)?;
    let relayer: Address = GENESIS_ADDRESS.parse()?;
    let chain_id = eth_chain_id(&http, &rpc_url).await?;

    // ---- Predeploys exist and are unconfigured ----
    for (addr, label) in [
        (BRIDGE_FACTORY, "factory"),
        (BRIDGE_TOKEN_PREDEPLOY, "bridge"),
    ] {
        let code = eth_get_code(&http, &rpc_url, addr).await?;
        anyhow::ensure!(code.len() > 2, "{label} predeploy missing at {addr}");
    }
    let init = eth_call_bool(&http, &rpc_url, BRIDGE_FACTORY, &selector(b"initialized()")).await?;
    assert!(!init, "fresh factory must be uninitialized");

    // ---- Bootstrap: initialize x2 + adopt ----
    send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_FACTORY,
        encode_factory_initialize(relayer),
        100_000,
        "factory initialize",
    )
    .await?;
    send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_TOKEN_PREDEPLOY,
        encode_bridge_initialize("Bridged SUI", "SUI", SUI_COIN_TYPE, relayer),
        200_000,
        "bridge initialize",
    )
    .await?;
    send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_FACTORY,
        encode_adopt_bridge(SUI_COIN_TYPE, Address::from_str(BRIDGE_TOKEN_PREDEPLOY)?),
        200_000,
        "adoptBridge",
    )
    .await?;

    // The registry maps SUI to the predeployed token, and both contracts
    // report the relayer.
    assert_eq!(
        eth_call_address(
            &http,
            &rpc_url,
            BRIDGE_FACTORY,
            &token_for_selector(SUI_COIN_TYPE, b"tokenFor(string)")
        )
        .await?,
        Address::from_str(BRIDGE_TOKEN_PREDEPLOY)?
    );
    assert_eq!(
        eth_call_address(&http, &rpc_url, BRIDGE_FACTORY, &selector(b"relayer()")).await?,
        relayer
    );
    assert_eq!(
        eth_call_address(
            &http,
            &rpc_url,
            BRIDGE_TOKEN_PREDEPLOY,
            &selector(b"relayer()")
        )
        .await?,
        relayer
    );

    // ---- Bootstrap is one-time ----
    let dup = send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_FACTORY,
        encode_factory_initialize(relayer),
        100_000,
        "duplicate initialize",
    )
    .await;
    assert!(
        dup.is_err_and(|e| e.to_string().contains("already initialized")),
        "second factory initialize must revert"
    );

    // ---- Permissionless creation, and duplicate rejection ----
    send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_FACTORY,
        encode_create_bridge("Test Coin", "TST", "0x5::test::TST"),
        3_000_000,
        "createBridge",
    )
    .await?;
    let tst = eth_call_address(
        &http,
        &rpc_url,
        BRIDGE_FACTORY,
        &token_for_selector("0x5::test::TST", b"tokenFor(string)"),
    )
    .await?;
    anyhow::ensure!(
        tst != Address::ZERO,
        "created token address must be non-zero"
    );

    let twin = send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_FACTORY,
        encode_create_bridge("Twin", "TWN", "0x5::test::TST"),
        3_000_000,
        "twin createBridge",
    )
    .await;
    assert!(
        twin.is_err_and(|e| e.to_string().contains("already has a token")),
        "duplicate coin type must revert"
    );
    let readopt = send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_FACTORY,
        encode_adopt_bridge(SUI_COIN_TYPE, Address::from_str(BRIDGE_TOKEN_PREDEPLOY)?),
        200_000,
        "duplicate adopt",
    )
    .await;
    assert!(
        readopt.is_err_and(|e| e.to_string().contains("already has a token")),
        "adopting a claimed coin type must revert"
    );

    // ---- Mint against the adopted predeploy (the relayer's deposit path) ----
    let recipient = relayer;
    send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_TOKEN_PREDEPLOY,
        encode_mint(recipient, 1_000_000_000, 0),
        120_000,
        "mint nonce 0",
    )
    .await?;
    assert_eq!(
        eth_balance_of(&http, &rpc_url, BRIDGE_TOKEN_PREDEPLOY, GENESIS_ADDRESS).await?,
        1_000_000_000
    );

    let replay = send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_TOKEN_PREDEPLOY,
        encode_mint(recipient, 1, 0),
        120_000,
        "mint replay",
    )
    .await;
    assert!(
        replay.is_err_and(|e| e.to_string().contains("stale nonce")),
        "replayed mint nonce must revert"
    );

    // ---- Burn (the withdrawal path the relayer releases on Sui) ----
    send_tx(
        &http,
        &rpc_url,
        &signer,
        chain_id,
        BRIDGE_TOKEN_PREDEPLOY,
        encode_initiate_withdrawal([0x99; 32], 400_000_000),
        120_000,
        "initiateWithdrawal",
    )
    .await?;
    assert_eq!(
        eth_balance_of(&http, &rpc_url, BRIDGE_TOKEN_PREDEPLOY, GENESIS_ADDRESS).await?,
        600_000_000
    );

    // ---- The relayer's log sources exist: BridgeCreated on the factory,
    //       WithdrawalInitiated on the token ----
    let created_topic = format!(
        "0x{}",
        hex::encode(keccak256(b"BridgeCreated(address,string,string,string)"))
    );
    let created = eth_get_logs(&http, &rpc_url, BRIDGE_FACTORY, &created_topic).await?;
    assert_eq!(
        created, 2,
        "adopt + createBridge must each emit one BridgeCreated"
    );

    let withdrawal_topic = format!(
        "0x{}",
        hex::encode(keccak256(
            b"WithdrawalInitiated(uint64,address,bytes32,uint256)"
        ))
    );
    let burns = eth_get_logs(&http, &rpc_url, BRIDGE_TOKEN_PREDEPLOY, &withdrawal_topic).await?;
    assert_eq!(burns, 1, "the burn must emit one WithdrawalInitiated");

    driver.abort();
    Ok(())
}

/// Produces blocks forever (one per ~600ms); the caller aborts the task.
async fn drive_blocks(engine: &Engine) -> Result<()> {
    let mut parent = engine.current_head().await?;
    let mut timestamp = 1u64;
    loop {
        timestamp += 1;
        let attributes = PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
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
        engine
            .accept(
                &built.payload,
                built.block_hash,
                built.block_hash,
                built.block_hash,
            )
            .await?;
        engine
            .finalize(built.block_hash, built.block_hash, built.block_hash)
            .await?;
        parent = built.block_hash;
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
}

// ---------- tx submission ----------

/// Sends a signed EIP-1559 tx and waits for a successful receipt. A would-be
/// revert is detected via an `eth_call` dry-run first, surfacing the revert
/// string in the error.
#[allow(clippy::too_many_arguments)]
async fn send_tx(
    http: &reqwest::Client,
    rpc_url: &str,
    signer: &PrivateKeySigner,
    chain_id: u64,
    to: &str,
    calldata: Vec<u8>,
    gas_limit: u64,
    label: &str,
) -> Result<()> {
    let from = signer.address();
    let dry: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_call",
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
    // A reverting eth_call reports the failure in `error.message` (which embeds
    // the decoded revert string); surface it before spending gas.
    if let Some(err) = dry
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        bail!("{label} would revert: {err}");
    }
    if let Some(result) = dry.get("result").and_then(|v| v.as_str()) {
        if result.starts_with("0x08c379a0") {
            bail!("{label} would revert: {}", decode_revert_string(result));
        }
    }

    let nonce = eth_get_transaction_count(http, rpc_url, from).await?;
    let price_hex: String = rpc_call(http, rpc_url, "eth_gasPrice", vec![]).await?;
    let price = u128::from_str_radix(price_hex.trim_start_matches("0x"), 16)?;

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
            bail!("{label} tx {tx_hash} never confirmed");
        }
        let receipt: Option<serde_json::Value> = rpc_call(
            http,
            rpc_url,
            "eth_getTransactionReceipt",
            vec![tx_hash.clone().into()],
        )
        .await?;
        if let Some(receipt) = receipt {
            let status = receipt
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("0x0");
            if status.eq_ignore_ascii_case("0x1") {
                return Ok(());
            }
            bail!("{label} tx reverted; receipt: {receipt}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---------- ABI encoding ----------

fn selector(signature: &[u8]) -> Vec<u8> {
    keccak256(signature).0[..4].to_vec()
}

fn abi_word(v: U256) -> [u8; 32] {
    v.to_be_bytes()
}

fn push_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let padded_len = bytes.len().div_ceil(32) * 32;
    out.extend_from_slice(&abi_word(U256::from(bytes.len())));
    out.extend_from_slice(bytes);
    out.resize(out.len() + (padded_len - bytes.len()), 0);
}

fn encode_factory_initialize(relayer: Address) -> Vec<u8> {
    let mut out = selector(b"initialize(address)");
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(relayer.as_ref());
    out.extend_from_slice(&word);
    out
}

fn encode_bridge_initialize(
    name: &str,
    symbol: &str,
    coin_type: &str,
    relayer: Address,
) -> Vec<u8> {
    let mut out = selector(b"initialize(string,string,string,address)");
    let mut dynamic = Vec::new();
    let mut offset = 4 * 32;
    for s in [name, symbol, coin_type] {
        out.extend_from_slice(&abi_word(U256::from(offset)));
        let bytes = s.as_bytes();
        let padded_len = bytes.len().div_ceil(32) * 32;
        dynamic.extend_from_slice(&abi_word(U256::from(bytes.len())));
        dynamic.extend_from_slice(bytes);
        dynamic.resize(dynamic.len() + (padded_len - bytes.len()), 0);
        offset += 32 + padded_len;
    }
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(relayer.as_ref());
    out.extend_from_slice(&word);
    out.extend_from_slice(&dynamic);
    out
}

fn encode_adopt_bridge(coin_type: &str, token: Address) -> Vec<u8> {
    let mut out = selector(b"adoptBridge(string,address)");
    out.extend_from_slice(&abi_word(U256::from(64)));
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(token.as_ref());
    out.extend_from_slice(&word);
    push_string(&mut out, coin_type);
    out
}

fn encode_create_bridge(name: &str, symbol: &str, coin_type: &str) -> Vec<u8> {
    let mut out = selector(b"createBridge(string,string,string)");
    let mut dynamic = Vec::new();
    let mut offset = 3 * 32;
    for s in [name, symbol, coin_type] {
        out.extend_from_slice(&abi_word(U256::from(offset)));
        let bytes = s.as_bytes();
        let padded_len = bytes.len().div_ceil(32) * 32;
        dynamic.extend_from_slice(&abi_word(U256::from(bytes.len())));
        dynamic.extend_from_slice(bytes);
        dynamic.resize(dynamic.len() + (padded_len - bytes.len()), 0);
        offset += 32 + padded_len;
    }
    out.extend_from_slice(&dynamic);
    out
}

fn encode_mint(recipient: Address, amount: u64, nonce: u64) -> Vec<u8> {
    let mut out = selector(b"mint(address,uint256,uint64)");
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(recipient.as_ref());
    out.extend_from_slice(&word);
    out.extend_from_slice(&abi_word(U256::from(amount)));
    out.extend_from_slice(&abi_word(U256::from(nonce)));
    out
}

fn encode_initiate_withdrawal(sui_recipient: [u8; 32], amount: u64) -> Vec<u8> {
    let mut out = selector(b"initiateWithdrawal(bytes32,uint256)");
    out.extend_from_slice(&sui_recipient);
    out.extend_from_slice(&abi_word(U256::from(amount)));
    out
}

/// Calldata for `tokenFor(string)` with the coin type as the argument.
fn token_for_selector(coin_type: &str, sig: &[u8]) -> Vec<u8> {
    let mut out = selector(sig);
    out.extend_from_slice(&abi_word(U256::from(32)));
    push_string(&mut out, coin_type);
    out
}

// ---------- RPC helpers ----------

async fn rpc_call<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    rpc_url: &str,
    method: &str,
    params: Vec<serde_json::Value>,
) -> Result<T> {
    let resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = resp.get("error") {
        bail!("rpc {method} error: {error}");
    }
    Ok(serde_json::from_value(resp["result"].clone())?)
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
        vec![address.to_checksum(None).into(), "pending".into()],
    )
    .await?;
    Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
}

async fn eth_call_raw(
    http: &reqwest::Client,
    rpc_url: &str,
    to: &str,
    calldata: &[u8],
) -> Result<Vec<u8>> {
    let to_addr: Address = to.parse()?;
    let resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_call",
            "params": [{ "to": format!("{to_addr:?}"),
                         "data": format!("0x{}", hex::encode(calldata)) }, "latest"],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = resp.get("error") {
        bail!("eth_call error: {error}");
    }
    let hex = resp.get("result").and_then(|v| v.as_str()).unwrap_or("0x");
    Ok(hex::decode(hex.trim_start_matches("0x"))?)
}

async fn eth_call_bool(
    http: &reqwest::Client,
    rpc_url: &str,
    to: &str,
    calldata: &[u8],
) -> Result<bool> {
    let bytes = eth_call_raw(http, rpc_url, to, calldata).await?;
    Ok(bytes.last().is_some_and(|b| *b != 0))
}

async fn eth_call_address(
    http: &reqwest::Client,
    rpc_url: &str,
    to: &str,
    calldata: &[u8],
) -> Result<Address> {
    let bytes = eth_call_raw(http, rpc_url, to, calldata).await?;
    anyhow::ensure!(bytes.len() >= 32, "eth_call address return too short");
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes[12..32]);
    Ok(Address::from(addr))
}

async fn eth_get_code(http: &reqwest::Client, rpc_url: &str, to: &str) -> Result<Vec<u8>> {
    eth_call_code(http, rpc_url, to).await
}

async fn eth_call_code(http: &reqwest::Client, rpc_url: &str, to: &str) -> Result<Vec<u8>> {
    let to_addr: Address = to.parse()?;
    let resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_getCode",
            "params": [format!("{to_addr:?}"), "latest"],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = resp.get("error") {
        bail!("eth_getCode error: {error}");
    }
    let hex = resp.get("result").and_then(|v| v.as_str()).unwrap_or("0x");
    Ok(hex::decode(hex.trim_start_matches("0x"))?)
}

async fn eth_balance_of(
    http: &reqwest::Client,
    rpc_url: &str,
    token: &str,
    holder: &str,
) -> Result<u64> {
    let mut calldata = selector(b"balanceOf(address)");
    let holder_addr: Address = holder.parse()?;
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(holder_addr.as_ref());
    calldata.extend_from_slice(&word);
    let bytes = eth_call_raw(http, rpc_url, token, &calldata).await?;
    anyhow::ensure!(bytes.len() >= 32, "balanceOf return too short");
    let amount = U256::from_be_slice(&bytes[..32]);
    anyhow::ensure!(amount <= U256::from(u64::MAX), "balance overflows u64");
    Ok(amount.to::<u64>())
}

/// Number of logs with `topic0` emitted by `address` chain-wide.
async fn eth_get_logs(
    http: &reqwest::Client,
    rpc_url: &str,
    address: &str,
    topic0: &str,
) -> Result<usize> {
    let to: Address = address.parse()?;
    let resp: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "eth_getLogs",
            "params": [{
                "address": format!("{to:?}"),
                "fromBlock": "0x0",
                "toBlock": "latest",
                "topics": [topic0],
            }],
        }))
        .send()
        .await?
        .json()
        .await?;
    if let Some(error) = resp.get("error") {
        bail!("eth_getLogs error: {error}");
    }
    Ok(resp["result"].as_array().map(|a| a.len()).unwrap_or(0))
}

/// Decodes a Solidity revert string from an `eth_call` result.
fn decode_revert_string(hex_result: &str) -> String {
    let bytes = match hex::decode(hex_result.trim_start_matches("0x")) {
        Ok(b) => b,
        Err(_) => return hex_result.to_string(),
    };
    // Error(string): selector(4) + offset(32) + length(32) + data.
    if bytes.len() < 68 {
        return hex_result.to_string();
    }
    let len = U256::from_be_slice(&bytes[36..68]).to::<usize>();
    if 68 + len > bytes.len() {
        return hex_result.to_string();
    }
    String::from_utf8_lossy(&bytes[68..68 + len]).to_string()
}
