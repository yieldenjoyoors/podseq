//! Enshrined bridge relayer.
//!
//! Runs inside the sequencer process and relays both directions with no external
//! relayer:
//!
//! - Sui deposit → L2 mint: reads each deposit by nonce from the Sui `Vault`,
//!   finds (or creates, via the factory) the canonical L2 token for its coin
//!   type, and submits a signed `mint` transaction.
//! - L2 burn → Sui release: reads `WithdrawalInitiated` logs from every
//!   factory-registered token and calls `bridge::withdraw` on Sui.
//!
//! Cursors and the token registry are persisted together, so a restart resumes
//! without re-minting, re-releasing, or rescanning token history. Mint nonces on
//! L2 and the Sui withdraw path are both strictly increasing, so a duplicate
//! after a crash is a safe no-op.
//!
//! Security invariant: `bridge::withdraw` is only ever called for burns logged
//! by a factory-registered token, so the withdrawal log query is always
//! address-filtered to the known token set.

use std::collections::HashMap;
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
use podseq_sui::bridge::{deposit_at, vault_status};
use serde::{Deserialize, Serialize};
use sui_sdk_types::Address as SuiAddress;
use sui_sdk_types::TypeTag;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::config::{BridgeConfig, Config};

/// Fixed genesis predeploy address of the L2 `BridgeFactory` contract.
pub const BRIDGE_FACTORY_ADDRESS_HEX: &str = "0x4200000000000000000000000000000000000010";

/// Genesis predeploy of the canonical SUI `Bridge` token; configured and
/// adopted once. The relayer itself is registry-driven.
pub const BRIDGE_TOKEN_PREDEPLOY_ADDRESS_HEX: &str = "0x4200000000000000000000000000000000000011";

/// Gas limit for a mint transaction.
const MINT_GAS_LIMIT: u64 = 120_000;

/// Gas limit for a `createBridge` transaction.
const CREATE_BRIDGE_GAS_LIMIT: u64 = 3_000_000;

// Minimal L2 interfaces for the functions the relayer calls.
alloy_sol_types::sol! {
    #[derive(Debug)]
    interface IL2Token {
        function mint(address recipient, uint256 amount, uint64 nonce) external;
    }

    #[derive(Debug)]
    interface IBridgeFactory {
        function createBridge(string name, string symbol, string coinType) external returns (address);
    }
}

use alloy_sol_types::SolCall;

/// Relayer state persisted to disk. The registry is written in the same
/// operation as the cursors, so it always covers every `BridgeCreated` below
/// `next_l2_block`; anything above is absorbed by the relay loop.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Cursors {
    /// Next Sui deposit nonce to mint on L2.
    next_deposit_nonce: u64,
    /// Next L2 block (exclusive) up to which logs have been read.
    next_l2_block: u64,
    /// Coin-type bytes (hex) -> token address (hex).
    #[serde(default)]
    tokens: std::collections::BTreeMap<String, String>,
}

/// Canonical L2 tokens, keyed in both directions. Only ever populated from
/// factory `BridgeCreated` events, which is what makes the address set
/// trustworthy for withdrawal processing.
#[derive(Debug, Default)]
struct TokenRegistry {
    /// Move `type_name` bytes -> token address.
    by_coin_type: HashMap<Vec<u8>, Address>,
    /// Token address -> Move `type_name` bytes.
    by_address: HashMap<Address, Vec<u8>>,
}

impl TokenRegistry {
    /// Records a factory-created token. Returns `false` if another token already
    /// claims this coin type (a variant-spelling twin): one canonical token
    /// stays authoritative.
    fn insert(&mut self, coin_type: Vec<u8>, token: Address) -> bool {
        match self.by_coin_type.get(&coin_type) {
            Some(existing) if *existing != token => return false,
            _ => {}
        }
        self.by_address.insert(token, coin_type.clone());
        self.by_coin_type.insert(coin_type, token);
        true
    }

    fn token_for(&self, coin_type: &[u8]) -> Option<Address> {
        self.by_coin_type.get(coin_type).copied()
    }

    fn coin_for(&self, token: &Address) -> Option<Vec<u8>> {
        self.by_address.get(token).cloned()
    }

    fn addresses(&self) -> Vec<Address> {
        self.by_address.keys().copied().collect()
    }

    fn len(&self) -> usize {
        self.by_coin_type.len()
    }

    /// Persisted form for [`Cursors::tokens`].
    fn snapshot(&self) -> std::collections::BTreeMap<String, String> {
        self.by_coin_type
            .iter()
            .map(|(coin, token)| (hex::encode(coin), format!("{token:#x}")))
            .collect()
    }

    /// Rebuilds a registry from its persisted form. Malformed entries are a
    /// disk problem the operator must see, not something to silently drop.
    fn from_snapshot(snapshot: &std::collections::BTreeMap<String, String>) -> Result<Self> {
        let mut registry = Self::default();
        for (coin_hex, token_hex) in snapshot {
            let coin = hex::decode(coin_hex)
                .with_context(|| format!("corrupt token registry entry {coin_hex}"))?;
            let token: Address = token_hex
                .parse()
                .with_context(|| format!("corrupt token registry entry {token_hex}"))?;
            registry.insert(coin, token);
        }
        Ok(registry)
    }
}

/// Relays bridged coins between Sui and the L2, one canonical token per type.
pub struct BridgeRelayer {
    http: reqwest::Client,
    sui_rpc: String,
    sui_bridge: Arc<Mutex<podseq_sui::bridge::BridgeClient>>,
    vault_id: String,
    l2_rpc: String,
    factory: Address,
    signer: PrivateKeySigner,
    chain_id: u64,
    mint_gas_limit: u64,
    create_gas_limit: u64,
    poll_interval: Duration,
    deposits_table_uid: SuiAddress,
    tokens: Mutex<TokenRegistry>,
    cursors: Arc<Mutex<Cursors>>,
    cursors_path: PathBuf,
    metrics: Arc<crate::metrics::PodseqMetrics>,
}

impl std::fmt::Debug for BridgeRelayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeRelayer")
            .field("factory", &self.factory)
            .field("vault_id", &self.vault_id)
            .finish_non_exhaustive()
    }
}

impl BridgeRelayer {
    /// Builds a relayer from config: reads the deposits-table UID once, and
    /// restores (or first indexes) the token registry.
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
        let factory: Address = BRIDGE_FACTORY_ADDRESS_HEX
            .parse()
            .context("internal: invalid BRIDGE_FACTORY_ADDRESS_HEX")?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        // The L2 relayer EVM key is separate from the Sui wallet. It must match
        // the factory's `relayer` (the mint authority on every token) and be
        // funded with L2 gas. Loaded first because the preflight needs its
        // address.
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
        info!(relayer = %signer.address(), %factory, "bridge relayer key loaded");

        // Preflight FIRST: fail fast with an actionable message if Reth's
        // factory predeploy is missing, uninitialized, or pointed at a different
        // relayer.
        verify_l2_bridge_factory(&http, l2_rpc, factory, signer.address()).await?;

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

        let status = vault_status(sui_rpc, vault_id)
            .await
            .context("reading vault status")?;

        let cursors_path = data_dir.join("bridge_cursors.json");
        let mut cursors = load_cursors(&cursors_path).await;
        let tokens =
            restore_token_registry(&http, l2_rpc, factory, &mut cursors, &cursors_path).await?;

        Ok(Self {
            http,
            sui_rpc: sui_rpc.to_string(),
            sui_bridge: Arc::new(Mutex::new(sui_bridge)),
            vault_id: vault_id.to_string(),
            l2_rpc: l2_rpc.to_string(),
            factory,
            signer,
            chain_id,
            mint_gas_limit: MINT_GAS_LIMIT,
            create_gas_limit: CREATE_BRIDGE_GAS_LIMIT,
            poll_interval: Duration::from_millis(cfg.poll_interval_ms.max(100)),
            deposits_table_uid: status.deposits_table_uid,
            tokens: Mutex::new(tokens),
            cursors: Arc::new(Mutex::new(cursors)),
            cursors_path,
            metrics,
        })
    }

    /// Runs both relay loops until `shutdown` is set.
    pub async fn run(self, shutdown: Arc<AtomicBool>) -> Result<()> {
        let next_deposit = self.cursors.lock().await.next_deposit_nonce;
        let next_l2_block = self.cursors.lock().await.next_l2_block;
        let tokens = self.tokens.lock().await.len();
        info!(
            next_deposit,
            next_l2_block,
            tokens,
            %self.factory,
            "bridge relayer started"
        );

        loop {
            if shutdown.load(Ordering::SeqCst) {
                info!("bridge relayer stopping");
                return Ok(());
            }

            if let Err(e) = self.relay_deposits(&self.http).await {
                warn!(error = %e, "bridge: deposit relay pass failed");
            }
            if let Err(e) = self.relay_withdrawals(&self.http).await {
                warn!(error = %e, "bridge: withdrawal relay pass failed");
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// Direction 1: Sui deposit → L2 mint. The cursor advances only on
    /// confirmed on-chain state: already-minted nonces are detected before
    /// sending, and a receipt is required after, so a revert never skips a
    /// deposit.
    async fn relay_deposits(&self, http: &reqwest::Client) -> Result<()> {
        let latest = match vault_status(&self.sui_rpc, &self.vault_id).await {
            Ok(status) => status.deposit_nonce,
            Err(e) => {
                warn!(error = %e, "bridge: reading vault status failed");
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
                    let token = match self.ensure_token(http, &rec.coin_type).await {
                        Ok(t) => t,
                        Err(e) => {
                            warn!(error = %e, nonce, "bridge: resolving L2 token failed; will retry next pass");
                            return Ok(());
                        }
                    };
                    // Crash-replay guard: if this token already minted the nonce,
                    // sync the cursor forward instead of re-sending.
                    let (minted_any, last_minted) = eth_mint_cursor(http, &self.l2_rpc, token)
                        .await
                        .unwrap_or((false, 0));
                    if minted_any && last_minted >= nonce {
                        debug!(nonce, "bridge: deposit already minted; syncing cursor");
                    } else if let Err(e) = self
                        .mint_on_l2(http, token, nonce, &rec.recipient_l2, rec.amount)
                        .await
                    {
                        warn!(error = %e, nonce, "bridge: mint failed; will retry next pass");
                        return Ok(());
                    }
                    let mut c = self.cursors.lock().await;
                    c.next_deposit_nonce = nonce + 1;
                    // Same write as the cursor, keeping registry and cursor in
                    // step (see `Cursors`).
                    c.tokens = self.tokens.lock().await.snapshot();
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

    /// Returns the canonical L2 token for `coin_type`, creating it through the
    /// factory when none exists yet. The address comes from the `BridgeCreated`
    /// log in the confirmed receipt.
    async fn ensure_token(&self, http: &reqwest::Client, coin_type: &[u8]) -> Result<Address> {
        if let Some(token) = self.tokens.lock().await.token_for(coin_type) {
            return Ok(token);
        }
        let coin_str = String::from_utf8(coin_type.to_vec())
            .context("deposit coin_type bytes are not utf-8")?;
        let (name, symbol) = derive_token_metadata(&coin_str);
        let calldata = create_bridge_calldata(name.clone(), symbol.clone(), coin_str.clone());

        let tx_hash = self
            .send_l2_tx(http, self.factory, calldata, self.create_gas_limit)
            .await
            .context("sending createBridge tx")?;
        let receipt = wait_for_receipt(http, &self.l2_rpc, &tx_hash, self.poll_interval).await?;
        ensure_receipt_success(&receipt)?;

        let token = receipt
            .logs
            .iter()
            .find_map(|log| parse_bridge_created_log(log).ok())
            .map(|(_, token)| token)
            .context("createBridge receipt has no BridgeCreated log")?;
        let inserted = self.tokens.lock().await.insert(coin_type.to_vec(), token);
        if !inserted {
            // Another token already claims this coin type; mint into that one.
            warn!(%token, "bridge: twin token ignored for coin type");
            return self
                .tokens
                .lock()
                .await
                .token_for(coin_type)
                .context("canonical token missing after twin rejection");
        }
        info!(token = %token, coin_type = %coin_str, %name, %symbol, "bridge: canonical L2 token created");
        self.metrics.bridge_tokens_created_total.inc();
        Ok(token)
    }

    /// Sends a mint tx and waits for its receipt. Only a successful execution
    /// returns `Ok`, so [`relay_deposits`] never advances past an un-minted
    /// deposit.
    async fn mint_on_l2(
        &self,
        http: &reqwest::Client,
        token: Address,
        nonce: u64,
        recipient: &[u8],
        amount: u64,
    ) -> Result<()> {
        let recipient: Address = recipient
            .try_into()
            .map_err(|_| anyhow!("deposit recipient_l2 is not 20 bytes"))?;
        let calldata = mint_calldata(recipient, amount, nonce);

        let tx_hash = self
            .send_l2_tx(http, token, calldata, self.mint_gas_limit)
            .await
            .context("sending mint tx")?;
        wait_for_receipt_success(http, &self.l2_rpc, &tx_hash, self.poll_interval).await?;
        info!(nonce, %recipient, amount, %token, "bridge: deposit minted on L2");
        self.metrics.bridge_deposits_total.inc();
        Ok(())
    }

    /// Signs and submits an EIP-1559 tx. Confirmation is the caller's concern.
    async fn send_l2_tx(
        &self,
        http: &reqwest::Client,
        to: Address,
        calldata: Vec<u8>,
        gas_limit: u64,
    ) -> Result<String> {
        let from = self.signer.address();
        let (nonce_tx, price) = tokio::try_join!(
            eth_get_transaction_count(http, &self.l2_rpc, from),
            eth_gas_price(http, &self.l2_rpc)
        )?;

        let mut tx = TxEip1559 {
            chain_id: self.chain_id,
            nonce: nonce_tx,
            gas_limit,
            max_fee_per_gas: price,
            max_priority_fee_per_gas: price.min(2_000_000_000),
            to: TxKind::Call(to),
            value: alloy_primitives::U256::ZERO,
            access_list: Default::default(),
            input: calldata.into(),
        };
        let sig = self
            .signer
            .sign_transaction(&mut tx)
            .await
            .context("signing bridge tx")?;
        let typed: TypedTransaction = tx.into();
        let envelope = typed.into_envelope(sig);
        let raw = envelope.encoded_2718();
        eth_send_raw_transaction(http, &self.l2_rpc, &raw).await
    }

    /// Direction 2: L2 burn → Sui release. Tokens created in the block range
    /// are absorbed first, so a burn in the same range as its token's creation
    /// is still processed.
    async fn relay_withdrawals(&self, http: &reqwest::Client) -> Result<()> {
        let from_block = self.cursors.lock().await.next_l2_block;
        let latest = eth_block_number(http, &self.l2_rpc).await?;
        if latest < from_block {
            return Ok(());
        }

        {
            let mut tokens = self.tokens.lock().await;
            absorb_creations(
                http,
                &self.l2_rpc,
                self.factory,
                &mut tokens,
                from_block,
                latest,
            )
            .await?;
        }

        let addresses = self.tokens.lock().await.addresses();
        if !addresses.is_empty() {
            let logs = eth_get_logs(
                http,
                &self.l2_rpc,
                &addresses,
                *WITHDRAWAL_TOPIC,
                from_block,
                latest,
            )
            .await?;
            for log in logs {
                self.process_withdrawal_log(&log).await?;
            }
        }

        let mut c = self.cursors.lock().await;
        c.next_l2_block = latest + 1;
        // Same write as the cursor, keeping registry and cursor in step (see
        // `Cursors`).
        c.tokens = self.tokens.lock().await.snapshot();
        persist_cursors(&self.cursors_path, &c).await;
        Ok(())
    }

    /// Releases one `WithdrawalInitiated` log on Sui. The emitter must be a
    /// registered token; on failure the pass aborts so the burn is retried.
    async fn process_withdrawal_log(&self, log: &RpcLog) -> Result<()> {
        let emitter = parse_log_address(&log.address).context("log address not valid hex")?;
        let coin_bytes = self
            .tokens
            .lock()
            .await
            .coin_for(&emitter)
            .with_context(|| format!("withdrawal log from unknown token {emitter}"))?;
        let coin_str = String::from_utf8(coin_bytes).context("token coin_type not utf-8")?;
        let coin_tag = coin_type_tag(&coin_str)?;

        let parsed = parse_withdrawal_log(log)?;
        let mut client = self.sui_bridge.lock().await;
        if let Err(e) = client
            .withdraw(coin_tag, parsed.sui_recipient, parsed.amount, parsed.nonce)
            .await
        {
            // Return Err so the pass stops and the cursor stays put; the burn
            // is retried next pass. Sui-side idempotency makes the retry safe.
            return Err(anyhow!("Sui withdraw failed (nonce {}): {e}", parsed.nonce));
        }
        info!(
            nonce = parsed.nonce,
            amount = parsed.amount,
            "bridge: withdrawal released on Sui"
        );
        self.metrics.bridge_withdrawals_total.inc();
        Ok(())
    }
}

// ---------- ABI helpers ----------

/// ABI-encodes `mint(address,uint256,uint64)` calldata via the typed
/// `IL2Token::mintCall` wrapper, so the selector and word layout are derived
/// from the signature at compile time.
fn mint_calldata(recipient: Address, amount: u64, nonce: u64) -> Vec<u8> {
    IL2Token::mintCall {
        recipient,
        amount: alloy_primitives::U256::from(amount),
        nonce,
    }
    .abi_encode()
}

/// ABI-encodes `createBridge(string,string,string)` calldata via the typed
/// `IBridgeFactory::createBridgeCall` wrapper, so the selector and word layout
/// are derived from the signature at compile time.
fn create_bridge_calldata(name: String, symbol: String, coin_type: String) -> Vec<u8> {
    IBridgeFactory::createBridgeCall {
        name,
        symbol,
        coinType: coin_type,
    }
    .abi_encode()
}

/// keccak256("WithdrawalInitiated(uint64,address,bytes32,uint256)").
static WITHDRAWAL_TOPIC: LazyLock<[u8; 32]> =
    LazyLock::new(|| keccak256(b"WithdrawalInitiated(uint64,address,bytes32,uint256)").0);

/// keccak256("BridgeCreated(address,string,string,string)").
static BRIDGE_CREATED_TOPIC: LazyLock<[u8; 32]> =
    LazyLock::new(|| keccak256(b"BridgeCreated(address,string,string,string)").0);

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

/// Parses a `BridgeCreated(address indexed, string coinType, ...)` log into
/// `(coin_type string, token address)`. The name/symbol tail is ignored.
fn parse_bridge_created_log(log: &RpcLog) -> Result<(String, Address)> {
    let topic = hex_to_fixed32(log.topics.first().context("missing topics")?)?;
    anyhow::ensure!(topic == *BRIDGE_CREATED_TOPIC, "not a BridgeCreated log");
    let token_word = hex_to_fixed32(log.topics.get(1).context("missing token topic")?)?;
    let token = decode_word_address(&token_word)?;
    let coin = decode_first_abi_string(&log.data).context("decoding coinType")?;
    Ok((coin, token))
}

/// Pure decode of the first ABI-encoded `string` in a tuple tail: a 32-byte
/// offset, a 32-byte length, then that many utf-8 bytes.
fn decode_first_abi_string(data: &str) -> Result<String> {
    let bytes = hex::decode(data.trim_start_matches("0x")).context("log data not hex")?;
    anyhow::ensure!(bytes.len() >= 32, "log data shorter than one word");
    let off = word_u64(bytes[0..32].try_into().unwrap()) as usize;
    anyhow::ensure!(
        off.checked_add(32).is_some_and(|end| end <= bytes.len()),
        "string offset overruns"
    );
    let len = word_u64(bytes[off..off + 32].try_into().unwrap()) as usize;
    anyhow::ensure!(
        off.checked_add(32)
            .and_then(|s| s.checked_add(len))
            .is_some_and(|end| end <= bytes.len()),
        "string length overruns"
    );
    String::from_utf8(bytes[off + 32..off + 32 + len].to_vec()).context("string not utf-8")
}

/// Low 20 bytes of a 32-byte big-endian word, as an EVM address.
fn decode_word_address(word: &[u8; 32]) -> Result<Address> {
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&word[12..]);
    Ok(Address::from(addr))
}

/// Parses a coin type string into a `StructTag`, accepting the short form
/// (`0x2::sui::SUI`) and Move's stored form (full hex, no `0x`).
fn parse_struct_tag_flexible(coin_type: &str) -> Result<sui_sdk_types::StructTag> {
    use std::str::FromStr;
    let repaired = if coin_type.starts_with("0x") {
        coin_type.to_string()
    } else {
        format!("0x{coin_type}")
    };
    sui_sdk_types::StructTag::from_str(&repaired)
        .with_context(|| format!("parsing coin type {coin_type} as a Sui StructTag"))
}

/// Canonical coin-type key: Move `type_name` bytes, as Sui deposit records
/// carry them.
fn normalize_coin_type(coin_type: &str) -> Result<Vec<u8>> {
    Ok(parse_struct_tag_flexible(coin_type)?
        .to_string()
        .trim_start_matches("0x")
        .as_bytes()
        .to_vec())
}

/// Parses a coin type string into a `TypeTag` for `bridge::withdraw` calls.
fn coin_type_tag(coin_type: &str) -> Result<TypeTag> {
    Ok(TypeTag::from(parse_struct_tag_flexible(coin_type)?))
}

/// Parses a log's `address` field (20-byte hex, with or without 0x).
fn parse_log_address(s: &str) -> Result<Address> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("log address not hex")?;
    anyhow::ensure!(bytes.len() == 20, "log address not 20 bytes");
    Ok(Address::from_slice(&bytes))
}

/// Name and symbol for a relayer-created token: the fully-qualified coin type
/// and its struct name. Deterministic, needs no external metadata.
fn derive_token_metadata(coin_type: &str) -> (String, String) {
    let symbol = coin_type
        .rsplit("::")
        .next()
        .unwrap_or(coin_type)
        .to_string();
    (coin_type.to_string(), symbol)
}

/// Folds the factory's `BridgeCreated` logs over a block range into `tokens`,
/// ignoring twins (a second token for a claimed coin type). Returns the log
/// count.
async fn absorb_creations(
    http: &reqwest::Client,
    l2_rpc: &str,
    factory: Address,
    tokens: &mut TokenRegistry,
    from_block: u64,
    to_block: u64,
) -> Result<usize> {
    let logs = eth_get_logs(
        http,
        l2_rpc,
        std::slice::from_ref(&factory),
        *BRIDGE_CREATED_TOPIC,
        from_block,
        to_block,
    )
    .await?;
    let count = logs.len();
    for log in logs {
        match parse_bridge_created_log(&log) {
            Ok((coin_str, token)) => match normalize_coin_type(&coin_str) {
                Ok(coin) => {
                    if !tokens.insert(coin, token) {
                        warn!(%token, "bridge: twin token ignored for claimed coin type");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "bridge: skipping token with unparseable coin type")
                }
            },
            Err(e) => warn!(error = %e, "bridge: skipping undecodable BridgeCreated log"),
        }
    }
    Ok(count)
}

/// History indexer chunk size, in blocks.
const CREATION_SCAN_CHUNK: u64 = 2_500;
const MIN_CREATION_SCAN_CHUNK: u64 = 64;
/// Halve the chunk at this many logs per response: creation is permissionless,
/// so the count per range is unbounded.
const CREATION_SCAN_HALVE_AFTER: usize = 500;

/// Restores the token registry from the persisted snapshot, or indexes the
/// factory's full history in bounded chunks (first start, or a file from
/// before the registry was persisted). The result is stored with the block
/// cursor, so later restarts never rescan.
async fn restore_token_registry(
    http: &reqwest::Client,
    l2_rpc: &str,
    factory: Address,
    cursors: &mut Cursors,
    cursors_path: &Path,
) -> Result<TokenRegistry> {
    if !cursors.tokens.is_empty() && cursors.next_l2_block > 0 {
        let registry = TokenRegistry::from_snapshot(&cursors.tokens)?;
        info!(
            tokens = registry.len(),
            next_l2_block = cursors.next_l2_block,
            "bridge token registry restored"
        );
        return Ok(registry);
    }

    let head = eth_block_number(http, l2_rpc).await?;
    let mut registry = TokenRegistry::default();
    let mut chunk = CREATION_SCAN_CHUNK;
    let mut from = 0u64;
    while from <= head {
        let to = from.saturating_add(chunk - 1).min(head);
        match absorb_creations(http, l2_rpc, factory, &mut registry, from, to).await {
            Ok(found) => {
                if found >= CREATION_SCAN_HALVE_AFTER && chunk > MIN_CREATION_SCAN_CHUNK {
                    chunk /= 2;
                }
                from = to + 1;
            }
            Err(e) if chunk > MIN_CREATION_SCAN_CHUNK => {
                chunk /= 2;
                warn!(error = %e, chunk, "bridge: creation scan narrowed; retrying chunk");
            }
            Err(e) => return Err(e).context("indexing BridgeCreated history"),
        }
    }

    cursors.next_l2_block = head + 1;
    cursors.tokens = registry.snapshot();
    persist_cursors(cursors_path, cursors).await;
    info!(
        tokens = registry.len(),
        next_l2_block = cursors.next_l2_block,
        "bridge token registry indexed from factory history"
    );
    Ok(registry)
}

// ---------- L2 JSON-RPC ----------

/// Loose decode of a log: topics/data/address are hex strings.
#[derive(Debug, Deserialize)]
struct RpcLog {
    #[serde(default)]
    address: String,
    #[serde(default)]
    topics: Vec<String>,
    #[serde(default)]
    data: String,
}

/// Fetches logs by topic-0, restricted to `addresses`. For withdrawals the
/// filter is what keeps forged events from unregistered contracts out.
async fn eth_get_logs(
    http: &reqwest::Client,
    l2_rpc: &str,
    addresses: &[Address],
    topic0: [u8; 32],
    from_block: u64,
    to_block: u64,
) -> Result<Vec<RpcLog>> {
    anyhow::ensure!(
        !addresses.is_empty(),
        "eth_getLogs needs at least one address"
    );
    let address_filter: Vec<String> = addresses.iter().map(|a| a.to_checksum(None)).collect();
    let req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_getLogs",
        "params": [{
            "address": address_filter,
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

/// Polls `eth_getTransactionReceipt` until the tx is mined.
async fn wait_for_receipt(
    http: &reqwest::Client,
    l2_rpc: &str,
    tx_hash: &str,
    poll: Duration,
) -> Result<Receipt> {
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
            return Ok(r);
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for receipt of tx {tx_hash}");
        }
        tokio::time::sleep(poll.min(Duration::from_secs(2))).await;
    }
}

/// Polls for the receipt and requires status 0x1. A revert is an `Err` so the
/// caller does not advance its cursor.
async fn wait_for_receipt_success(
    http: &reqwest::Client,
    l2_rpc: &str,
    tx_hash: &str,
    poll: Duration,
) -> Result<()> {
    let receipt = wait_for_receipt(http, l2_rpc, tx_hash, poll).await?;
    ensure_receipt_success(&receipt)
}

fn ensure_receipt_success(receipt: &Receipt) -> Result<()> {
    if !receipt
        .status
        .trim_start_matches("0x")
        .eq_ignore_ascii_case("1")
    {
        bail!("tx reverted on L2");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Receipt {
    status: String,
    #[serde(default)]
    logs: Vec<RpcLog>,
}

fn selector(sig: &str) -> Vec<u8> {
    keccak256(sig.as_bytes())[..4].to_vec()
}

/// eth_call returning the raw decoded return bytes.
async fn eth_call(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<Vec<u8>> {
    let data = format!("0x{}", hex::encode(selector));
    let req = eth_call_request(token, data);
    let hex_str: String = rpc_call::<String>(http, l2_rpc, req).await?;
    hex::decode(hex_str.trim_start_matches("0x")).context("eth_call not hex")
}

async fn eth_call_u64(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<u64> {
    let bytes = eth_call(http, l2_rpc, token, selector).await?;
    anyhow::ensure!(bytes.len() >= 32, "eth_call u64 return too short");
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[24..32]);
    Ok(u64::from_be_bytes(arr))
}

async fn eth_call_bool(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<bool> {
    let bytes = eth_call(http, l2_rpc, token, selector).await?;
    Ok(bytes.last().is_some_and(|b| *b != 0))
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
    let hex_str: String = rpc_call::<String>(http, l2_rpc, req).await?;
    Ok(hex::decode(hex_str.trim_start_matches("0x")).unwrap_or_default())
}

/// Reads an ABI `address` return (right-aligned in a 32-byte word).
async fn eth_call_address(
    http: &reqwest::Client,
    l2_rpc: &str,
    token: Address,
    selector: &[u8],
) -> Result<Address> {
    let bytes = eth_call(http, l2_rpc, token, selector).await?;
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

/// Preflight check that the L2 `BridgeFactory` predeploy is present,
/// initialized, and configured for `expected_relayer`. Returns actionable errors
/// so a misconfigured Reth fails fast at startup instead of silently dropping
/// deposits mid-relay.
pub async fn verify_l2_bridge_factory(
    http: &reqwest::Client,
    l2_rpc: &str,
    factory: Address,
    expected_relayer: Address,
) -> Result<()> {
    let code = eth_get_code(http, l2_rpc, factory).await?;
    if code.len() <= 2 {
        bail!("BridgeFactory predeploy not found at {factory}: Reth has no contract there. Run solidity/scripts/gen-genesis.sh and restart Reth.");
    }

    // The canonical SUI token is also predeployed; a missing contract means a
    // stale genesis (the factory bytecode changed since it was generated).
    let token_predeploy: Address = BRIDGE_TOKEN_PREDEPLOY_ADDRESS_HEX
        .parse()
        .context("internal: invalid BRIDGE_TOKEN_PREDEPLOY_ADDRESS_HEX")?;
    let token_code = eth_get_code(http, l2_rpc, token_predeploy).await?;
    if token_code.len() <= 2 {
        bail!(
            "Bridge predeploy not found at {token_predeploy}: Reth has no contract \
             there. Run solidity/scripts/gen-genesis.sh and restart Reth."
        );
    }

    let initialized = eth_call_bool(http, l2_rpc, factory, &selector("initialized()"))
        .await
        .context("calling initialized() on L2 BridgeFactory")?;
    if !initialized {
        bail!(
            "BridgeFactory predeploy at {factory} is not initialized. Call \
             initialize(relayer) once after chain start."
        );
    }

    let onchain_relayer = eth_call_address(http, l2_rpc, factory, &selector("relayer()"))
        .await
        .context("calling relayer() on L2 BridgeFactory")?;
    if onchain_relayer != expected_relayer {
        bail!(
            "BridgeFactory relayer is {onchain_relayer}, but the configured relayer \
             key is {expected_relayer}. Call setRelayer({expected_relayer}) from the \
             current relayer, or re-initialize with the right key."
        );
    }
    Ok(())
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
    info!(predeploy = %BRIDGE_FACTORY_ADDRESS_HEX, "bridge enabled (will relay once the L2 factory is initialized)");
    Some((handle, shutdown))
}

/// Runs the bridge relayer, retrying setup until the L2 factory is ready or the
/// sequencer shuts down. Non-fatal: the sequencer produces blocks throughout, so
/// the operator's `BridgeFactory.initialize` tx can confirm and make the
/// predeploy usable. Each failed attempt logs the actionable cause (missing
/// predeploy / not initialized / relayer mismatch) via `{e:#}`.
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
            config.save(config_path)?;
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
        // Selector is derived from the signature by the `sol!` macro at compile
        // time; pin the known-good value so a rename is caught at test time.
        assert_eq!(
            IL2Token::mintCall::SELECTOR,
            keccak256(b"mint(address,uint256,uint64)")[..4]
        );
    }

    #[test]
    fn mint_calldata_roundtrips() {
        let recipient = Address::with_last_byte(0x42);
        let amount = 0xab_u64;
        let nonce = 0x05_u64;

        let calldata = mint_calldata(recipient, amount, nonce);

        // Selector + 3 head words, no tail (all args fit in static slots).
        assert_eq!(calldata.len(), 4 + 96);

        let decoded = IL2Token::mintCall::abi_decode(&calldata).expect("decode");
        assert_eq!(decoded.recipient, recipient);
        assert_eq!(decoded.amount, U256::from(amount));
        assert_eq!(decoded.nonce, nonce);
    }

    #[test]
    fn withdrawal_topic_matches_signature() {
        assert_eq!(
            *WITHDRAWAL_TOPIC,
            keccak256(b"WithdrawalInitiated(uint64,address,bytes32,uint256)").0
        );
        assert_eq!(
            *BRIDGE_CREATED_TOPIC,
            keccak256(b"BridgeCreated(address,string,string,string)").0
        );
    }

    fn log(address: &str, topics: Vec<String>, data: Vec<u8>) -> RpcLog {
        RpcLog {
            address: address.to_string(),
            topics,
            data: hex::encode(&data),
        }
    }

    fn word(v: u64) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&v.to_be_bytes());
        w
    }

    #[test]
    fn parse_withdrawal_log_decodes_topics_and_data() {
        let mut sui_topic = [0u8; 32];
        sui_topic[0] = 0x99;
        let mut data = vec![0u8; 32];
        data[31] = 0x10;
        let l = log(
            "0x4200000000000000000000000000000000000011",
            vec![
                hex::encode(*WITHDRAWAL_TOPIC),
                hex::encode(word(7)),
                hex::encode([0u8; 32]),
                hex::encode(sui_topic),
            ],
            data,
        );
        let parsed = parse_withdrawal_log(&l).unwrap();
        assert_eq!(parsed.nonce, 7);
        assert_eq!(parsed.amount, 0x10);
        assert_eq!(parsed.sui_recipient, SuiAddress::new(sui_topic));
    }

    #[test]
    fn parse_withdrawal_log_rejects_short_data() {
        let l = log(
            "0x42",
            vec![
                hex::encode(*WITHDRAWAL_TOPIC),
                hex::encode([0u8; 32]),
                hex::encode([0u8; 32]),
                hex::encode([0u8; 32]),
            ],
            vec![0u8; 10],
        );
        assert!(parse_withdrawal_log(&l).is_err());
    }

    #[test]
    fn mint_cursor_selectors_match_contract() {
        assert_eq!(
            selector("lastMintedDepositNonce()"),
            keccak256(b"lastMintedDepositNonce()")[..4].to_vec()
        );
        assert_eq!(
            selector("mintedAny()"),
            keccak256(b"mintedAny()")[..4].to_vec()
        );
    }

    #[test]
    fn decode_first_abi_string_reads_coin_type_from_tuple_tail() {
        // Tuple (string coinType, string name, string symbol): head is three
        // offset words; the first string's tail is [len][bytes].
        let payload = b"0x2::sui::SUI";
        let mut data = vec![0u8; 32];
        data[31] = 0x60; // offset of first string = 96
        data.extend_from_slice(&[0u8; 32]); // offset of second
        data.extend_from_slice(&[0u8; 32]); // offset of third
        data.extend_from_slice(&word(payload.len() as u64));
        data.extend_from_slice(payload);
        data.extend(std::iter::repeat_n(0u8, 32 - (payload.len() % 32)));
        assert_eq!(
            decode_first_abi_string(&format!("0x{}", hex::encode(&data))).unwrap(),
            "0x2::sui::SUI"
        );
    }

    #[test]
    fn decode_first_abi_string_rejects_overruns() {
        // Offset beyond the buffer.
        let mut data = vec![0u8; 32];
        data[31] = 0xff;
        assert!(decode_first_abi_string(&hex::encode(&data)).is_err());
        // Length beyond the buffer.
        let mut data = vec![0u8; 64];
        data[31] = 0x20;
        data[63] = 0x40;
        assert!(decode_first_abi_string(&hex::encode(&data)).is_err());
        assert!(decode_first_abi_string("0x").is_err());
    }

    #[test]
    fn parse_bridge_created_log_decodes_token_and_coin_type() {
        let payload = b"0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";
        let mut data = vec![0u8; 32];
        data[31] = 0x60;
        data.extend_from_slice(&[0u8; 32]);
        data.extend_from_slice(&[0u8; 32]);
        data.extend_from_slice(&word(payload.len() as u64));
        data.extend_from_slice(payload);

        let mut token_word = [0u8; 32];
        token_word[12..].copy_from_slice(&[0x11u8; 20]);
        let l = log(
            "0x4200000000000000000000000000000000000010",
            vec![hex::encode(*BRIDGE_CREATED_TOPIC), hex::encode(token_word)],
            data,
        );

        let (coin, token) = parse_bridge_created_log(&l).unwrap();
        assert_eq!(coin, String::from_utf8(payload.to_vec()).unwrap());
        assert_eq!(token, Address::from([0x11u8; 20]));
    }

    #[test]
    fn parse_bridge_created_log_rejects_wrong_topic() {
        let l = log(
            "0x42",
            vec![hex::encode(*WITHDRAWAL_TOPIC), hex::encode([0u8; 32])],
            vec![],
        );
        assert!(parse_bridge_created_log(&l).is_err());
    }

    #[test]
    fn parse_log_address_accepts_hex_with_and_without_prefix() {
        let addr: Address = "0x4200000000000000000000000000000000000010"
            .parse()
            .unwrap();
        assert_eq!(
            parse_log_address("0x4200000000000000000000000000000000000010").unwrap(),
            addr
        );
        assert_eq!(
            parse_log_address("4200000000000000000000000000000000000010").unwrap(),
            addr
        );
        assert!(parse_log_address("0xzz").is_err());
        assert!(parse_log_address("0x1234").is_err());
    }

    #[test]
    fn derive_token_metadata_uses_type_and_struct_name() {
        let (name, symbol) = derive_token_metadata("0x2::sui::SUI");
        assert_eq!(name, "0x2::sui::SUI");
        assert_eq!(symbol, "SUI");

        // Generic types keep the full tail as the symbol (cosmetic only).
        let (_, symbol) = derive_token_metadata("0x5::complex::Coin<0x2::sui::SUI>");
        assert_eq!(symbol, "SUI>");
    }

    #[test]
    fn normalize_coin_type_canonicalizes_all_spellings() {
        let canonical =
            b"0000000000000000000000000000000000000000000000000000000000000002::sui::SUI".to_vec();
        assert_eq!(normalize_coin_type("0x2::sui::SUI").unwrap(), canonical);
        assert_eq!(
            normalize_coin_type(
                "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"
            )
            .unwrap(),
            canonical
        );
        // Move's stored form: no `0x` prefix.
        assert_eq!(
            normalize_coin_type(
                "0000000000000000000000000000000000000000000000000000000000000002::sui::SUI"
            )
            .unwrap(),
            canonical
        );
        assert!(normalize_coin_type("not-a-type").is_err());
    }

    #[test]
    fn coin_type_tag_accepts_all_move_spellings() {
        let short = coin_type_tag("0x2::sui::SUI").unwrap();
        let full = coin_type_tag(
            "0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        )
        .unwrap();
        assert_eq!(short, full);
        assert!(coin_type_tag("not-a-type").is_err());
    }

    #[test]
    fn token_registry_maps_both_directions() {
        let mut reg = TokenRegistry::default();
        let coin =
            b"0000000000000000000000000000000000000000000000000000000000000002::sui::SUI".to_vec();
        let token: Address = Address::from([0x11u8; 20]);
        assert!(reg.insert(coin.clone(), token));
        assert_eq!(reg.token_for(&coin), Some(token));
        assert_eq!(reg.coin_for(&token), Some(coin.clone()));
        assert_eq!(reg.addresses(), vec![token]);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn token_registry_rejects_twins_and_allows_reinsert() {
        let mut reg = TokenRegistry::default();
        let coin = b"0x2::sui::SUI".to_vec();
        let a: Address = Address::from([0x11u8; 20]);
        let b: Address = Address::from([0x22u8; 20]);
        assert!(reg.insert(coin.clone(), a));
        // A second token for the same coin type is a twin: rejected.
        assert!(!reg.insert(coin.clone(), b));
        assert_eq!(reg.token_for(&coin), Some(a));
        // Re-recording the same pair is idempotent.
        assert!(reg.insert(coin.clone(), a));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn token_registry_snapshot_roundtrips() {
        let mut reg = TokenRegistry::default();
        let sui =
            b"0000000000000000000000000000000000000000000000000000000000000002::sui::SUI".to_vec();
        let tst =
            b"0000000000000000000000000000000000000000000000000000000000000005::test::TST".to_vec();
        let sui_token: Address = Address::from([0x11u8; 20]);
        let tst_token: Address = Address::from([0x22u8; 20]);
        reg.insert(sui.clone(), sui_token);
        reg.insert(tst.clone(), tst_token);

        let snapshot = reg.snapshot();
        let restored = TokenRegistry::from_snapshot(&snapshot).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.token_for(&sui), Some(sui_token));
        assert_eq!(restored.token_for(&tst), Some(tst_token));
        assert_eq!(restored.coin_for(&sui_token), Some(sui.clone()));
        assert_eq!(restored.coin_for(&tst_token), Some(tst.clone()));
    }

    #[test]
    fn token_registry_snapshot_is_deterministic() {
        let mut reg = TokenRegistry::default();
        reg.insert(b"0x5::b::B".to_vec(), Address::from([0x11u8; 20]));
        reg.insert(b"0x5::a::A".to_vec(), Address::from([0x22u8; 20]));
        // BTreeMap keeps entries sorted by coin type, so file diffs stay small.
        let keys: Vec<_> = reg.snapshot().keys().cloned().collect();
        assert_eq!(
            keys,
            vec![hex::encode(b"0x5::a::A"), hex::encode(b"0x5::b::B")]
        );
    }

    #[test]
    fn token_registry_from_snapshot_rejects_corruption() {
        let mut snapshot = std::collections::BTreeMap::new();
        snapshot.insert(hex::encode(b"0x2::sui::SUI"), "0x4200".to_string()); // bad address
        assert!(TokenRegistry::from_snapshot(&snapshot).is_err());

        let mut snapshot = std::collections::BTreeMap::new();
        snapshot.insert(
            "zz-not-hex".to_string(),
            Address::from([0x11u8; 20]).to_checksum(None).to_string(),
        );
        assert!(TokenRegistry::from_snapshot(&snapshot).is_err());

        assert!(TokenRegistry::from_snapshot(&std::collections::BTreeMap::new()).is_ok());
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
        assert!(decode_abi_address(&[0u8; 19]).is_err());
    }
}
