//! Enshrined bridge relayer.
//!
//! Runs inside the sequencer process and relays both directions with no external
//! relayer:
//!
//! - Sui deposit → L2 mint: reads each deposit by nonce from the Sui `Vault` and
//!   submits a signed `mint` transaction to the L2 `Bridge`.
//! - L2 burn → Sui release: reads `WithdrawalInitiated` logs from the L2 `Bridge`
//!   and calls `bridge::withdraw` on Sui.
//!
//! Cursors are persisted to disk so the relayer resumes after a restart without
//! re-minting or re-releasing. Mint nonces on L2 and the Sui withdraw path are
//! both strictly increasing, so a duplicate after a crash is a safe no-op.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use alloy_consensus::{TxEip1559, TypedTransaction};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::TxSigner;
use alloy_primitives::{keccak256, Address, TxKind};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{anyhow, bail, Context, Result};
use podseq_sui::bridge::{deposit_at, deposit_nonce, deposits_table_uid};
use serde::{Deserialize, Serialize};
use sui_sdk_types::Address as SuiAddress;
use sui_sdk_types::TypeTag;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::config::{BridgeConfig, Config};

/// Fixed genesis predeploy address of the L2 `Bridge` contract.
pub const BRIDGE_PREDEPLOY_ADDRESS_HEX: &str = "0x4200000000000000000000000000000000000010";

/// Gas limit for a mint transaction.
const MINT_GAS_LIMIT: u64 = 120_000;

/// Relayer cursors, persisted next to the chain state.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Cursors {
    /// Next Sui deposit nonce to mint on L2.
    next_deposit_nonce: u64,
    /// Next L2 block (exclusive) up to which withdrawal logs have been read.
    next_l2_block: u64,
}

/// Bridges a single coin between Sui and the L2.
pub struct BridgeRelayer {
    sui_rpc: String,
    sui_bridge: Arc<Mutex<podseq_sui::bridge::BridgeClient>>,
    vault_id: String,
    coin_type: TypeTag,
    coin_type_bytes: Vec<u8>,
    l2_rpc: String,
    l2_token: Address,
    signer: PrivateKeySigner,
    chain_id: u64,
    gas_limit: u64,
    poll_interval: Duration,
    deposits_table_uid: SuiAddress,
    cursors: Arc<Mutex<Cursors>>,
    cursors_path: PathBuf,
    metrics: Arc<crate::metrics::PodseqMetrics>,
}

impl std::fmt::Debug for BridgeRelayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeRelayer")
            .field("l2_token", &self.l2_token)
            .field("vault_id", &self.vault_id)
            .finish_non_exhaustive()
    }
}

impl BridgeRelayer {
    /// Builds a relayer from config. Performs the one-time read of the deposits
    /// table UID so each [`deposit_at`] lookup is a single `get_object`.
    ///
    /// `sui_signer_key_path` is the Sui key owning `BridgeCap` (used for
    /// `bridge::withdraw`); the EVM relayer key comes from `cfg.l2_relayer_key_path`.
    /// `package_id` is the settlement package (the `bridge` module lives in it).
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        cfg: &BridgeConfig,
        sui_signer_key_path: &Path,
        package_id: &str,
        cap_id: &str,
        vault_id: &str,
        sui_rpc: &str,
        l2_rpc: &str,
        data_dir: &Path,
        metrics: Arc<crate::metrics::PodseqMetrics>,
    ) -> Result<Self> {
        let l2_token: Address = BRIDGE_PREDEPLOY_ADDRESS_HEX
            .parse()
            .context("internal: invalid BRIDGE_PREDEPLOY_ADDRESS_HEX")?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        // The L2 relayer EVM key is separate from the Sui wallet. It must match
        // the L2 `relayer` role and be funded with L2 gas. Loaded first because
        // the preflight needs its address.
        let relayer_key_path = cfg
            .l2_relayer_key_path
            .as_deref()
            .context("bridge.l2_relayer_key_path is required")?;
        let key_hex = std::fs::read_to_string(relayer_key_path)
            .with_context(|| format!("reading relayer key {}", relayer_key_path.display()))?
            .trim()
            .to_string();
        let key_bytes = hex::decode(key_hex.trim_start_matches("0x"))
            .context("relayer key is not valid hex")?;
        let signer = PrivateKeySigner::from_slice(&key_bytes)
            .context("invalid relayer private key (need 32-byte secp256k1 scalar)")?;
        info!(relayer = %signer.address(), %l2_token, "bridge relayer key loaded");

        // Preflight FIRST: fail fast with an actionable message if Reth's Bridge
        // predeploy is missing, uninitialized, or pointed at a different relayer.
        // Must run before reading coinType(), which is empty until initialized.
        verify_l2_bridge_predeploy(&http, l2_rpc, l2_token, signer.address()).await?;

        // Now safe: the contract is initialized, so coinType() is non-empty.
        let coin_type_str = eth_call_string(&http, l2_rpc, l2_token, &selector("coinType()"))
            .await
            .context("reading coinType() from L2 Bridge")?;
        let coin_type: TypeTag = coin_type_str
            .parse()
            .with_context(|| format!("parsing L2 coinType {coin_type_str} as a Sui TypeTag"))?;
        // Chain id is a property of the L2, not of the bridge: read it from Reth.
        let chain_id = eth_chain_id(&http, l2_rpc)
            .await
            .context("reading chain id from Reth via eth_chainId")?;

        let sui_bridge = podseq_sui::bridge::BridgeClient::new(
            sui_signer_key_path,
            package_id,
            cap_id,
            vault_id,
            sui_rpc,
        )
        .context("building Sui bridge client")?;

        let deposits_table_uid = deposits_table_uid(sui_rpc, vault_id)
            .await
            .context("reading deposits table UID from vault")?;

        let cursors_path = data_dir.join("bridge_cursors.json");
        let cursors = Arc::new(Mutex::new(load_cursors(&cursors_path).await));

        Ok(Self {
            sui_rpc: sui_rpc.to_string(),
            sui_bridge: Arc::new(Mutex::new(sui_bridge)),
            vault_id: vault_id.to_string(),
            coin_type,
            coin_type_bytes: move_type_name_bytes(&coin_type_str)
                .context("normalizing L2 coinType to Move type_name form")?,
            l2_rpc: l2_rpc.to_string(),
            l2_token,
            signer,
            chain_id,
            gas_limit: MINT_GAS_LIMIT,
            poll_interval: Duration::from_millis(cfg.poll_interval_ms.max(100)),
            deposits_table_uid,
            cursors,
            cursors_path,
            metrics,
        })
    }

    /// Runs both relay loops until `shutdown` is set.
    pub async fn run(self, shutdown: Arc<AtomicBool>) -> Result<()> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        // Seed the L2 block cursor on first run so we don't replay history.
        {
            let mut c = self.cursors.lock().await;
            if c.next_l2_block == 0 {
                c.next_l2_block = eth_block_number(&http, &self.l2_rpc).await?;
                persist_cursors(&self.cursors_path, &c).await;
            }
        }

        let next_deposit = self.cursors.lock().await.next_deposit_nonce;
        let next_l2_block = self.cursors.lock().await.next_l2_block;
        info!(
            next_deposit,
            next_l2_block,
            %self.l2_token,
            "bridge relayer started"
        );

        loop {
            if shutdown.load(Ordering::SeqCst) {
                info!("bridge relayer stopping");
                return Ok(());
            }

            if let Err(e) = self.relay_deposits(&http).await {
                warn!(error = %e, "bridge: deposit relay pass failed");
            }
            if let Err(e) = self.relay_withdrawals(&http).await {
                warn!(error = %e, "bridge: withdrawal relay pass failed");
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Direction 1: Sui deposit → L2 mint.
    ///
    /// The L2 mint cursor is advanced only against confirmed on-chain state:
    /// before sending, we check `mintedAny`/`lastMintedDepositNonce` so a
    /// crash-after-send (replayed nonce) is detected as already-done, and after
    /// sending we wait for a successful receipt so an execution revert never
    /// silently skips a deposit.
    async fn relay_deposits(&self, http: &reqwest::Client) -> Result<()> {
        let latest = match deposit_nonce(&self.sui_rpc, &self.vault_id).await {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "bridge: reading deposit nonce failed");
                return Ok(());
            }
        };

        loop {
            let nonce = self.cursors.lock().await.next_deposit_nonce;
            if nonce >= latest {
                return Ok(());
            }
            match deposit_at(&self.sui_rpc, &self.deposits_table_uid, nonce).await {
                Ok(Some(rec)) => {
                    if rec.coin_type == self.coin_type_bytes {
                        // Crash-replay guard: if the L2 already minted this nonce,
                        // sync the cursor forward instead of re-sending.
                        let (minted_any, last_minted) =
                            eth_mint_cursor(http, &self.l2_rpc, self.l2_token)
                                .await
                                .unwrap_or((false, 0));
                        if minted_any && last_minted >= nonce {
                            debug!(nonce, "bridge: deposit already minted; syncing cursor");
                        } else {
                            match self
                                .mint_on_l2(http, nonce, &rec.recipient_l2, rec.amount)
                                .await
                            {
                                Ok(()) => {}
                                Err(e) => {
                                    warn!(error = %e, nonce, "bridge: mint failed; will retry next pass");
                                    return Ok(());
                                }
                            }
                        }
                    } else {
                        debug!(nonce, "bridge: skipping deposit of other coin type");
                    }
                    let mut c = self.cursors.lock().await;
                    c.next_deposit_nonce = nonce + 1;
                    persist_cursors(&self.cursors_path, &c).await;
                }
                Ok(None) => return Ok(()),
                Err(e) => {
                    warn!(error = %e, nonce, "bridge: reading deposit failed");
                    return Ok(());
                }
            }
        }
    }

    /// Sends a mint tx and waits for its receipt, returning Ok only if the tx
    /// executed successfully. A network/RPC error or an on-chain revert is an
    /// `Err`, so [`relay_deposits`] does not advance the cursor past this nonce.
    async fn mint_on_l2(
        &self,
        http: &reqwest::Client,
        nonce: u64,
        recipient: &[u8],
        amount: u64,
    ) -> Result<()> {
        let recipient: Address = recipient
            .try_into()
            .map_err(|_| anyhow!("deposit recipient_l2 is not 20 bytes"))?;
        let calldata = mint_calldata(recipient, amount, nonce);

        let from = self.signer.address();
        let (nonce_tx, price) = tokio::try_join!(
            eth_get_transaction_count(http, &self.l2_rpc, from),
            eth_gas_price(http, &self.l2_rpc)
        )?;

        let mut tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce: nonce_tx,
            gas_limit: self.gas_limit,
            max_fee_per_gas: price,
            max_priority_fee_per_gas: price.min(2_000_000_000),
            to: TxKind::Call(self.l2_token),
            value: alloy_primitives::U256::ZERO,
            access_list: Default::default(),
            input: calldata.into(),
        };
        let sig = self
            .signer
            .sign_transaction(&mut tx)
            .await
            .context("signing mint tx")?;
        let typed: TypedTransaction = tx.into();
        let envelope = typed.into_envelope(sig);
        let raw = envelope.encoded_2718();

        let tx_hash = eth_send_raw_transaction(http, &self.l2_rpc, &raw).await?;
        // Confirm execution: acceptance into the mempool is not enough, since a
        // mint can revert at execution (e.g. lost relayer role) and we must not
        // advance the cursor past an un-minted deposit.
        wait_for_receipt_success(http, &self.l2_rpc, &tx_hash, self.poll_interval).await?;
        info!(nonce, %recipient, amount, "bridge: deposit minted on L2");
        self.metrics.bridge_deposits_total.inc();
        Ok(())
    }

    /// Direction 2: L2 burn → Sui release.
    async fn relay_withdrawals(&self, http: &reqwest::Client) -> Result<()> {
        let from_block = self.cursors.lock().await.next_l2_block;
        let latest = eth_block_number(http, &self.l2_rpc).await?;
        if latest < from_block {
            return Ok(());
        }

        let logs = eth_get_logs(
            http,
            &self.l2_rpc,
            self.l2_token,
            *WITHDRAWAL_TOPIC,
            from_block,
            latest,
        )
        .await?;

        for log in logs {
            let parsed = match parse_withdrawal_log(&log) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "bridge: skipping undecodable withdrawal log");
                    continue;
                }
            };
            // Advance the cursor block-wise regardless, but only after a
            // successful release; a failure leaves this withdrawal pending.
            let mut client = self.sui_bridge.lock().await;
            if let Err(e) = client
                .withdraw(
                    self.coin_type.clone(),
                    parsed.sui_recipient,
                    parsed.amount,
                    parsed.nonce,
                )
                .await
            {
                warn!(error = %e, nonce = parsed.nonce, "bridge: Sui withdraw failed; will retry next pass");
                return Ok(());
            }
            info!(
                nonce = parsed.nonce,
                amount = parsed.amount,
                "bridge: withdrawal released on Sui"
            );
            self.metrics.bridge_withdrawals_total.inc();
        }

        let mut c = self.cursors.lock().await;
        c.next_l2_block = latest + 1;
        persist_cursors(&self.cursors_path, &c).await;
        Ok(())
    }
}

// ---------- ABI helpers ----------

/// ABI-encodes `mint(address,uint256,uint64)` calldata (selector + 3 fixed words).
fn mint_calldata(recipient: Address, amount: u64, nonce: u64) -> Vec<u8> {
    let mut out = keccak256(b"mint(address,uint256,uint64)")[..4].to_vec();
    // address is right-aligned in a 32-byte word
    let mut recipient_word = [0u8; 32];
    recipient_word[12..].copy_from_slice(recipient.as_ref());
    out.extend_from_slice(&recipient_word);
    let mut amount_word = [0u8; 32];
    amount_word[24..].copy_from_slice(&amount.to_be_bytes());
    out.extend_from_slice(&amount_word);
    let mut nonce_word = [0u8; 32];
    nonce_word[24..].copy_from_slice(&nonce.to_be_bytes());
    out.extend_from_slice(&nonce_word);
    out
}

/// keccak256("WithdrawalInitiated(uint64,address,bytes32,uint256)").
static WITHDRAWAL_TOPIC: LazyLock<[u8; 32]> =
    LazyLock::new(|| keccak256(b"WithdrawalInitiated(uint64,address,bytes32,uint256)").0);

struct ParsedWithdrawal {
    nonce: u64,
    amount: u64,
    sui_recipient: SuiAddress,
}

fn parse_withdrawal_log(log: &RpcLog) -> Result<ParsedWithdrawal> {
    // WithdrawalInitiated(uint64 indexed nonce, address indexed from,
    //                       bytes32 indexed suiRecipient, uint256 amount)
    let nonce_word = hex_to_fixed32(log.topics.get(1).context("missing nonce topic")?)?;
    let sui_word = hex_to_fixed32(log.topics.get(3).context("missing suiRecipient topic")?)?;
    let amount = log_data_u64(&log.data).context("missing/short amount")?;

    Ok(ParsedWithdrawal {
        nonce: word_u64(&nonce_word),
        amount,
        sui_recipient: SuiAddress::new(sui_word),
    })
}

/// Reads the low-order 8 bytes of a 32-byte big-endian word.
fn word_u64(word: &[u8; 32]) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&word[24..]);
    u64::from_be_bytes(arr)
}

fn log_data_u64(data: &str) -> Result<u64> {
    let bytes = hex::decode(data.trim_start_matches("0x")).context("amount not hex")?;
    anyhow::ensure!(bytes.len() >= 32, "log data shorter than 32 bytes");
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[24..32]);
    Ok(u64::from_be_bytes(arr))
}

fn hex_to_fixed32(s: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("topic not hex")?;
    let mut out = [0u8; 32];
    anyhow::ensure!(bytes.len() == 32, "topic not 32 bytes");
    out.copy_from_slice(&bytes);
    Ok(out)
}

// ---------- L2 JSON-RPC ----------

/// Loose decode of a log: topics/data are hex strings, block number is hex.
#[derive(Debug, Deserialize)]
struct RpcLog {
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    data: String,
}

async fn eth_get_logs(
    http: &reqwest::Client,
    l2_rpc: &str,
    address: Address,
    topic0: [u8; 32],
    from_block: u64,
    to_block: u64,
) -> Result<Vec<RpcLog>> {
    let req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_getLogs",
        "params": [{
            "address": address.to_checksum(None),
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{to_block:x}"),
            "topics": [format!("0x{}", hex::encode(topic0))]
        }]
    });
    rpc_call::<Vec<RpcLog>>(http, l2_rpc, req)
        .await
        .context("eth_getLogs")
}

async fn eth_block_number(http: &reqwest::Client, l2_rpc: &str) -> Result<u64> {
    let s = rpc_call::<String>(
        http,
        l2_rpc,
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_blockNumber", "params": [] }),
    )
    .await?;
    parse_hex_qty(&s)
}

/// Reads the L2 chain id, used to sign relay transactions. This is a property of
/// the L2 (Reth/genesis), not of the bridge, so it is read rather than configured.
async fn eth_chain_id(http: &reqwest::Client, l2_rpc: &str) -> Result<u64> {
    let s = rpc_call::<String>(
        http,
        l2_rpc,
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": [] }),
    )
    .await?;
    parse_hex_qty(&s)
}

async fn eth_get_transaction_count(
    http: &reqwest::Client,
    l2_rpc: &str,
    address: Address,
) -> Result<u64> {
    let s = rpc_call::<String>(
        http,
        l2_rpc,
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_getTransactionCount",
            "params": [address.to_checksum(None), "pending"] }),
    )
    .await?;
    parse_hex_qty(&s)
}

async fn eth_gas_price(http: &reqwest::Client, l2_rpc: &str) -> Result<u128> {
    let s = rpc_call::<String>(
        http,
        l2_rpc,
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_gasPrice", "params": [] }),
    )
    .await?;
    let v = parse_hex_qty(&s).unwrap_or(0);
    let gwei = u128::from(v);
    Ok(gwei.max(1_000_000_000))
}

async fn eth_send_raw_transaction(
    http: &reqwest::Client,
    l2_rpc: &str,
    raw: &[u8],
) -> Result<String> {
    let hash = rpc_call::<String>(
        http,
        l2_rpc,
        serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_sendRawTransaction",
            "params": [format!("0x{}", hex::encode(raw))] }),
    )
    .await?;
    debug!(%hash, "eth_sendRawTransaction accepted");
    Ok(hash)
}

/// Reads `(mintedAny, lastMintedDepositNonce)` from the L2 `Bridge` so the
/// relayer can detect an already-minted nonce after a crash.
async fn eth_mint_cursor(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
) -> Result<(bool, u64)> {
    // Pack two `eth_call`s: lastMintedDepositNonce() and mintedAny().
    let last = eth_call_u64(http, l2_rpc, token, &selector("lastMintedDepositNonce()"))
        .await
        .unwrap_or(0);
    let any = eth_call_bool(http, l2_rpc, token, &selector("mintedAny()")).await?;
    Ok((any, last))
}

/// Polls `eth_getTransactionReceipt` until the tx is mined, returning Ok only on
/// status 0x1. A revert is an `Err` so the caller does not advance its cursor.
async fn wait_for_receipt_success(
    http: &reqwest::Client,
    l2_rpc: &str,
    tx_hash: &str,
    poll: Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let receipt: Option<Receipt> = rpc_call(
            http,
            l2_rpc,
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "eth_getTransactionReceipt",
                "params": [tx_hash] }),
        )
        .await?;
        if let Some(r) = receipt {
            if r.status.trim_start_matches("0x").eq_ignore_ascii_case("1") {
                return Ok(());
            }
            bail!("mint tx {tx_hash} reverted on L2");
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for receipt of mint tx {tx_hash}");
        }
        tokio::time::sleep(poll.min(Duration::from_secs(2))).await;
    }
}

#[derive(Deserialize)]
struct Receipt {
    status: String,
}

fn selector(sig: &str) -> Vec<u8> {
    keccak256(sig.as_bytes())[..4].to_vec()
}

/// Encodes a coin type string as the bytes Move's `type_name::with_defining_ids`
/// emits on-chain: full 32-byte hex address, no `0x` prefix
/// (e.g. `0000...0002::sui::SUI`). Returns an error if `coin_type` does not parse
/// as a Sui `StructTag`: a malformed config would otherwise compare unequal to
/// every on-chain record and the relayer would silently never relay.
fn move_type_name_bytes(coin_type: &str) -> Result<Vec<u8>, anyhow::Error> {
    use std::str::FromStr;
    let tag = sui_sdk_types::StructTag::from_str(coin_type)
        .context("parsing coin type as a Sui StructTag")?;
    Ok(tag.to_string().trim_start_matches("0x").as_bytes().to_vec())
}

async fn eth_call_u64(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<u64> {
    let data = format!("0x{}", hex::encode(selector));
    let req = eth_call_request(token, data);
    let hex_str: String = rpc_call_raw(http, l2_rpc, req).await?;
    parse_hex_qty(&hex_str)
}

async fn eth_call_bool(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<bool> {
    let data = format!("0x{}", hex::encode(selector));
    let req = eth_call_request(token, data);
    let hex_str: String = rpc_call_raw(http, l2_rpc, req).await?;
    let bytes = hex::decode(hex_str.trim_start_matches("0x")).unwrap_or_default();
    Ok(bytes.last().is_some_and(|b| *b != 0))
}

/// Decodes an ABI-encoded `string` return (offset + length + utf-8 bytes).
async fn eth_call_string(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<String> {
    let data = format!("0x{}", hex::encode(selector));
    let req = eth_call_request(token, data);
    let hex_str: String = rpc_call_raw(http, l2_rpc, req).await?;
    let bytes = hex::decode(hex_str.trim_start_matches("0x")).context("eth_call not hex")?;
    decode_abi_string(&bytes)
}

/// Pure decode of an ABI `string` return value: a 32-byte offset (assumed 0x20),
/// a 32-byte length, then that many utf-8 bytes.
fn decode_abi_string(bytes: &[u8]) -> Result<String> {
    anyhow::ensure!(bytes.len() >= 64, "eth_call string return too short");
    let len_word: [u8; 32] = bytes[32..64].try_into().unwrap();
    let len = word_u64(&len_word) as usize;
    anyhow::ensure!(
        64 + len <= bytes.len(),
        "eth_call string length overruns buffer"
    );
    String::from_utf8(bytes[64..64 + len].to_vec()).context("coinType not valid utf-8")
}

fn eth_call_request(token: Address, data: String) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_call",
        "params": [{ "to": token.to_checksum(None), "data": data }, "latest"]
    })
}

/// Returns the runtime bytecode at `address` (empty if none). Used to confirm
/// the Bridge predeploy is actually present in Reth's genesis.
async fn eth_get_code(http: &reqwest::Client, l2_rpc: &str, address: Address) -> Result<Vec<u8>> {
    let req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_getCode",
        "params": [address.to_checksum(None), "latest"]
    });
    let hex_str: String = rpc_call_raw(http, l2_rpc, req).await?;
    Ok(hex::decode(hex_str.trim_start_matches("0x")).unwrap_or_default())
}

/// Reads an ABI `address` return (right-aligned in a 32-byte word).
async fn eth_call_address(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<Address> {
    let data = format!("0x{}", hex::encode(selector));
    let req = eth_call_request(token, data);
    let hex_str: String = rpc_call_raw(http, l2_rpc, req).await?;
    let bytes = hex::decode(hex_str.trim_start_matches("0x")).context("eth_call not hex")?;
    decode_abi_address(&bytes)
}

/// Pure decode of an ABI `address` return value: the low 20 bytes of a 32-byte
/// word (bytes 12..32).
fn decode_abi_address(bytes: &[u8]) -> Result<Address> {
    anyhow::ensure!(bytes.len() >= 32, "eth_call address return too short");
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&bytes[12..32]);
    Ok(Address::from(addr))
}

/// Preflight check that the L2 `Bridge` predeploy is present, initialized, and
/// configured for `expected_relayer`. Returns actionable errors so a
/// misconfigured Reth fails fast at startup instead of silently dropping
/// deposits mid-relay.
pub async fn verify_l2_bridge_predeploy(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    expected_relayer: Address,
) -> Result<()> {
    let code = eth_get_code(http, l2_rpc, token).await?;
    if code.len() <= 2 {
        bail!("Bridge predeploy not found at {token}: Reth has no contract there.");
    }

    let initialized = eth_call_bool(http, l2_rpc, token, &selector("initialized()"))
        .await
        .context("calling initialized() on L2 Bridge")?;
    if !initialized {
        bail!(
            "Bridge predeploy at {token} is not initialized. Call \
             initialize(name, symbol, coinType, relayer) once after chain start."
        );
    }

    let onchain_relayer = eth_call_address(http, l2_rpc, token, &selector("relayer()"))
        .await
        .context("calling relayer() on L2 Bridge")?;
    if onchain_relayer != expected_relayer {
        bail!(
            "Bridge relayer role is {onchain_relayer}, but the configured relayer \
             key is {expected_relayer}. Call setRelayer({expected_relayer}) from the \
             current relayer, or re-initialize with the right key."
        );
    }
    Ok(())
}

/// `rpc_call` for calls that return a bare hex string (not a JSON object),
/// i.e. `eth_call` whose result is itself a hex string inside the envelope.
async fn rpc_call_raw(
    http: &reqwest::Client,
    l2_rpc: &str,
    req: serde_json::Value,
) -> Result<String> {
    rpc_call::<String>(http, l2_rpc, req).await
}

#[derive(Deserialize)]
struct RpcEnvelope<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

async fn rpc_call<T: for<'de> Deserialize<'de>>(
    http: &reqwest::Client,
    l2_rpc: &str,
    req: serde_json::Value,
) -> Result<T> {
    let env: RpcEnvelope<T> = http
        .post(l2_rpc)
        .json(&req)
        .send()
        .await
        .context("L2 RPC send")?
        .json()
        .await
        .context("L2 RPC decode")?;
    if let Some(e) = env.error {
        bail!(
            "L2 rpc error {code}: {message}",
            code = e.code,
            message = e.message
        );
    }
    env.result
        .ok_or_else(|| anyhow!("L2 rpc returned no result"))
}

fn parse_hex_qty(s: &str) -> Result<u64> {
    Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0))
}

// ---------- process lifecycle ----------

/// Spawns the bridge relayer as a non-fatal background task, returning the task
/// handle and a shutdown flag for graceful drain. Returns `None` if the bridge
/// is disabled in config.
///
/// The relayer only starts once the L2 predeploy is initialized: setup retries
/// with backoff, and the spawned task owns its own copy of `config` so it can
/// persist Sui object IDs to `config_path` independently of the runner.
pub fn spawn(
    config: Config,
    config_path: PathBuf,
    signer_key_path: PathBuf,
    metrics: Arc<crate::metrics::PodseqMetrics>,
) -> Option<(tokio::task::JoinHandle<()>, Arc<AtomicBool>)> {
    if !config.bridge.enabled {
        return None;
    }
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        relay_until_ready(
            config,
            config_path,
            signer_key_path,
            shutdown_clone,
            metrics,
        )
        .await;
    });
    info!(predeploy = %BRIDGE_PREDEPLOY_ADDRESS_HEX, "bridge enabled (will relay once L2 contract is initialized)");
    Some((handle, shutdown))
}

/// Runs the bridge relayer, retrying setup until the L2 contract is ready or the
/// sequencer shuts down. Non-fatal: the sequencer produces blocks throughout, so
/// the operator's `Bridge.initialize` tx can confirm and make the predeploy usable.
/// Each failed attempt logs the actionable cause (missing predeploy / not
/// initialized / relayer mismatch) via `{e:#}`.
async fn relay_until_ready(
    mut config: Config,
    config_path: PathBuf,
    signer_key_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    metrics: Arc<crate::metrics::PodseqMetrics>,
) {
    let backoff = Duration::from_secs(5);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match setup_relayer(&mut config, &config_path, &signer_key_path, &metrics).await {
            Ok(relayer) => {
                if let Err(e) = relayer.run(shutdown.clone()).await {
                    warn!(error = %e, "bridge relayer exited with error; will restart");
                }
                // `run` only returns on shutdown or error. On shutdown, stop;
                // on error, the loop restarts it after backoff.
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
            }
            Err(e) => {
                // Most common first-run cause: the predeploy is not yet
                // initialized. Surface the full chain so the operator knows
                // exactly what to do (e.g. call `initialize(...)` once).
                warn!(
                    error = %format!("{e:#}"),
                    "bridge not ready yet; will retry in {:?}. \
                     If you just started the chain, initialize the L2 Bridge once \
                     (see docs/src/bridge.md).",
                    backoff
                );
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// Resolves the bridge's Sui object IDs (reusing the settlement package, which
/// already contains the `bridge` module) and builds the relayer.
///
/// If `cap_id`/`vault_id` are unset, calls `bridge::initialize` on first start and
/// persists the created IDs to the config file.
async fn setup_relayer(
    config: &mut Config,
    config_path: &Path,
    signer_key_path: &Path,
    metrics: &Arc<crate::metrics::PodseqMetrics>,
) -> Result<BridgeRelayer> {
    let package_id = config
        .sui
        .settlement_package_id
        .as_ref()
        .context("settlement must be deployed before the bridge can start")?;

    let (cap_id, vault_id) = match (&config.bridge.cap_id, &config.bridge.vault_id) {
        (Some(cap), Some(vault)) => (cap.clone(), vault.clone()),
        (None, None) => {
            info!(key = %signer_key_path.display(), "initializing bridge Vault on first start");
            let deployed = podseq_sui::bridge::BridgeClient::initialize(
                signer_key_path,
                &config.sui.rpc_url,
                package_id,
            )
            .await
            .context("initializing bridge Vault on Sui")?;

            config.bridge.cap_id = Some(deployed.cap_id.clone());
            config.bridge.vault_id = Some(deployed.vault_id.clone());
            let updated = toml::to_string_pretty(&*config).context("serializing updated config")?;
            std::fs::write(config_path, &updated)
                .with_context(|| format!("writing updated config to {}", config_path.display()))?;
            info!(config = %config_path.display(), "config updated with bridge object IDs");
            (deployed.cap_id, deployed.vault_id)
        }
        _ => anyhow::bail!("bridge.cap_id and bridge.vault_id must both be set or both unset"),
    };

    BridgeRelayer::new(
        &config.bridge,
        signer_key_path,
        package_id,
        &cap_id,
        &vault_id,
        &config.sui.rpc_url,
        &config.reth.rpc_url,
        &config.data_dir,
        Arc::clone(metrics),
    )
    .await
}

// ---------- cursor persistence ----------

async fn load_cursors(path: &Path) -> Cursors {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(c) => {
                info!(path = %path.display(), "bridge cursors loaded");
                c
            }
            Err(_) => Cursors::default(),
        },
        Err(_) => Cursors::default(),
    }
}

async fn persist_cursors(path: &Path, cursors: &Cursors) {
    let Ok(json) = serde_json::to_string_pretty(cursors) else {
        return;
    };
    if let Err(e) = tokio::fs::write(path, json).await {
        warn!(error = %e, "bridge: failed to persist cursors");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    #[test]
    fn mint_selector_is_stable() {
        let selector = &mint_calldata(Address::ZERO, 0, 0)[..4];
        assert_eq!(selector, &keccak256(b"mint(address,uint256,uint64)")[..4]);
    }

    #[test]
    fn mint_calldata_right_aligns_address() {
        let recipient = Address::from([0xaa; 20]);
        let calldata = mint_calldata(recipient, 0, 0);
        // address word: 12 zero bytes then 20 bytes of 0xaa
        assert_eq!(&calldata[4..16], &[0u8; 12]);
        assert_eq!(&calldata[16..36], &[0xaa; 20]);
    }

    #[test]
    fn mint_calldata_encodes_fixed_words() {
        let recipient = Address::with_last_byte(0x42);
        let calldata = mint_calldata(recipient, 0xab, 0x05);
        assert_eq!(calldata.len(), 4 + 96); // selector + 3 words
                                            // recipient word left-padded (right-aligned): 12 zeros then address.
        assert_eq!(&calldata[4..16], &[0u8; 12]);
        assert_eq!(calldata[4 + 31], 0x42);
        // amount in the low byte of its word.
        assert_eq!(calldata[4 + 32 + 31], 0xab);
        // nonce in the low byte of its word.
        assert_eq!(calldata[4 + 64 + 31], 0x05);
    }

    #[test]
    fn withdrawal_topic_matches_signature() {
        assert_eq!(
            *WITHDRAWAL_TOPIC,
            keccak256(b"WithdrawalInitiated(uint64,address,bytes32,uint256)").0
        );
    }

    #[test]
    fn parse_withdrawal_log_decodes_topics_and_data() {
        let mut nonce_topic = [0u8; 32];
        nonce_topic[31] = 7;
        let mut sui_topic = [0u8; 32];
        sui_topic[0] = 0x99;
        let mut data = vec![0u8; 32];
        data[31] = 0x10;
        let log = RpcLog {
            topics: vec![
                hex::encode(*WITHDRAWAL_TOPIC),
                hex::encode(nonce_topic),
                hex::encode([0u8; 32]),
                hex::encode(sui_topic),
            ],
            data: hex::encode(&data),
        };
        let parsed = parse_withdrawal_log(&log).unwrap();
        assert_eq!(parsed.nonce, 7);
        assert_eq!(parsed.amount, 0x10);
        assert_eq!(parsed.sui_recipient, SuiAddress::new(sui_topic));
    }

    #[test]
    fn parse_withdrawal_log_rejects_short_data() {
        let log = RpcLog {
            topics: vec![
                hex::encode(*WITHDRAWAL_TOPIC),
                hex::encode([0u8; 32]),
                hex::encode([0u8; 32]),
                hex::encode([0u8; 32]),
            ],
            data: hex::encode([0u8; 10]),
        };
        assert!(parse_withdrawal_log(&log).is_err());
    }

    #[test]
    fn u256_compat_smoke() {
        // Guards that U256 is still reachable in this module's dep graph.
        let _ = U256::ZERO;
    }

    #[test]
    fn mint_cursor_selectors_match_contract() {
        // Guards the eth_call selectors used to sync the deposit cursor.
        assert_eq!(
            selector("lastMintedDepositNonce()"),
            keccak256(b"lastMintedDepositNonce()")[..4].to_vec()
        );
        assert_eq!(
            selector("mintedAny()"),
            keccak256(b"mintedAny()")[..4].to_vec()
        );
        assert_eq!(
            selector("coinType()"),
            keccak256(b"coinType()")[..4].to_vec()
        );
    }

    #[test]
    fn decode_abi_string_reads_coin_type() {
        // ABI encoding of string "0x2::sui::SUI".
        let payload = b"0x2::sui::SUI";
        let mut bytes = vec![0u8; 32]; // offset word = 0x20
        bytes[31] = 0x20;
        bytes.extend_from_slice(&[0u8; 24]); // length word high bytes
        bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(payload);
        let pad = (32 - (payload.len() % 32)) % 32;
        bytes.extend(std::iter::repeat_n(0u8, pad));
        assert_eq!(decode_abi_string(&bytes).unwrap(), "0x2::sui::SUI");
    }

    #[test]
    fn decode_abi_string_rejects_short_buffer() {
        assert!(decode_abi_string(&[0u8; 10]).is_err());
    }

    #[test]
    fn move_type_name_bytes_matches_move_form() {
        // Move's type_name::with_defining_ids emits full 32-byte hex addresses
        // without the 0x prefix.
        let short = move_type_name_bytes("0x2::sui::SUI").expect("short form should parse");
        let canonical = move_type_name_bytes(
            "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        )
        .expect("canonical form should parse");
        assert_eq!(short, canonical);
        assert_eq!(
            short,
            b"0000000000000000000000000000000000000000000000000000000000000002::sui::SUI".to_vec()
        );
    }

    #[test]
    fn move_type_name_bytes_rejects_malformed() {
        // A bad config should fail loudly at construction, not silently never
        // match any on-chain record.
        assert!(move_type_name_bytes("not-a-coin-type").is_err());
        assert!(move_type_name_bytes("").is_err());
    }

    #[test]
    fn decode_abi_address_reads_low_20_bytes() {
        // ABI address: 12 zero bytes + the 20-byte address.
        let mut word = vec![0u8; 12];
        word.extend_from_slice(&[
            0xde, 0xad, 0xbe, 0xef, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        ]);
        let addr = decode_abi_address(&word).unwrap();
        assert_eq!(addr.0 .0[0], 0xde);
        assert_eq!(addr.0 .0[19], 0x0f);
    }

    #[test]
    fn decode_abi_address_rejects_short_buffer() {
        assert!(decode_abi_address(&[0u8; 10]).is_err());
    }
}
