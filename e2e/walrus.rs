//! Self-hosted Walrus publisher for the full-stack e2e.
//!
//! The DA leg posts block batches to a Walrus publisher and reads them back
//! through an aggregator. The public testnet daemons are run by Mysten Labs
//! and their WAL wallets drain between operator refills, which fails every
//! POST with `500 could not find WAL coins` — not something a retry can ride
//! out. This module brings our own `walrus daemon` (combined
//! aggregator + publisher) up inside the compose stack, funded from the
//! faucet-PoW-refillable SUI: faucet SUI covers gas, and our own
//! exchange-swap PTB exchanges SUI 1:1 for testnet WAL at the official
//! exchange.
//!
//! Why not `walrus get-wal`: the testnet faucet pays into the address
//! BALANCE (the accumulator gas model — `sui client gas` lists a single
//! "address balance" entry, no coin objects), and the walrus client's gas
//! selection enumerates coin objects only. Live-probed 2026-11-26 with a
//! 20-SUI balance wallet: `get-wal` fails with `could not find SUI coins
//! with sufficient balance`. The swap is therefore our own PTB:
//! `SplitCoins(Gas)` materializes a SUI coin from the balance, and
//! `wal_exchange::exchange_all_for_wal` consumes it — the same exchange
//! contract get-wal drives.
//!
//! Cost shape that sizes the funding: Walrus bills the encoded size of a blob
//! (about 4.5x the original plus a fixed 64 MB of metadata per blob). The e2e
//! stores with `epochs = 1` — blobs only need to outlive the run — instead of
//! the production default of 53 epochs. Current testnet pricing (live probed
//! 2026-11-26 via `walrus info`) puts a store's metadata at 0.0003 WAL, so
//! the funding targets are sized to the daemon's own refill amount (2 WAL),
//! not to a per-blob estimate.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionRequest;
use sui_rpc::Client as RpcClient;
use sui_sdk_types::Address;
use sui_sdk_types::Identifier;
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

/// Walrus client image carrying the full `walrus` binary (daemon, get-wal).
/// The floating `testnet` tag tracks the release Walrus validates against the
/// current testnet protocol, the same posture as the pinned-then-pulled Reth
/// image: an old client build fails against a bumped protocol.
pub const WALRUS_IMAGE: &str = "mysten/walrus-service:testnet";

/// Port the daemon binds inside the container.
const DAEMON_PORT: u16 = 9000;

/// One faucet PoW payout is ~1 SUI; requests are budgeted in whole payouts.
const ONE_SUI_MIST: u64 = 1_000_000_000;

/// The testnet SUI→WAL exchange: the deployed `wal_exchange` package and the
/// shared `Exchange` object it serves. Pinned because the swap is our own
/// PTB (`append_exchange_swap`); the committed client-config fixture lists
/// the same exchange objects the `get-wal` path rotates through.
const WAL_EXCHANGE_PACKAGE: Address = Address::new([
    0x82, 0x59, 0x38, 0x28, 0xed, 0x3f, 0xcb, 0x8c, 0x6a, 0x23, 0x5e, 0xac, 0x9a, 0xbd, 0x0a, 0xdb,
    0xe9, 0xc5, 0xf9, 0xbb, 0xff, 0xa9, 0xb1, 0xe7, 0xa4, 0x5c, 0xdd, 0x88, 0x44, 0x81, 0xef, 0x9f,
]);
const WAL_EXCHANGE_OBJECT: Address = Address::new([
    0x19, 0x82, 0x51, 0x21, 0xc5, 0x20, 0x80, 0xbb, 0x10, 0x73, 0x66, 0x22, 0x31, 0xcf, 0xea, 0x5c,
    0x0e, 0x4d, 0x90, 0x5f, 0xd1, 0x3e, 0x95, 0xf2, 0x1e, 0x9a, 0x01, 0x8f, 0x2e, 0xf4, 0x18, 0x62,
]);

/// Per-transaction wait for checkpoint inclusion (the swap must land before
/// the daemon starts serving stores).
const TX_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Canonical testnet client config published by Walrus. Fetched at bring-up so
/// system-object moves are picked up; the committed fixture is the fallback.
const CLIENT_CONFIG_URL: &str = "https://docs.wal.app/setup/client_config_testnet.yaml";

/// The committed fallback copy of the testnet client config.
const CLIENT_CONFIG_FIXTURE: &str = include_str!("fixtures/walrus_client_config_testnet.yaml");

/// Funding targets for the daemon wallet, in the wallet's own smallest units
/// (MIST for SUI, FROST for WAL — 1 token = 1e9 units).
pub struct FundingTargets {
    /// SUI kept on the main wallet for gas and daemon refill transfers.
    pub sui_floor_mist: u64,
    /// WAL converted for blob storage spends.
    pub wal_target_frost: u64,
}

impl FundingTargets {
    /// Covers the run's blob stores plus the daemon's own sub-wallet refill
    /// amount (2 WAL, the `--wal-refill-amount` in the compose service).
    /// Re-sized 2026-11-26 from live testnet pricing: `walrus info` prices a
    /// store's metadata at 0.0003 WAL (the 64 MB metadata floor dominates),
    /// so a run's ~30 stores cost well under 0.1 WAL — the old 16 WAL target
    /// (sized when a store priced ~2.1 WAL) drove 20 faucet PoW rounds, ~18
    /// minutes of every CI startup.
    pub fn defaults() -> Self {
        Self {
            sui_floor_mist: 4_000_000_000,
            wal_target_frost: 2_000_000_000,
        }
    }
}

/// Faucet requests needed to fund a wallet holding `sui_balance_mist` up to
/// the targets: the WAL conversion spends SUI, so one faucet budget covers
/// floor plus WAL together. Each payout is ~1 SUI.
pub fn faucet_requests_needed(sui_balance_mist: u64, targets: &FundingTargets) -> u32 {
    let deficit = targets
        .sui_floor_mist
        .saturating_add(targets.wal_target_frost)
        .saturating_sub(sui_balance_mist);
    deficit.div_ceil(ONE_SUI_MIST) as u32
}

/// The `sui.keystore` JSON a fresh wallet key serializes to: a one-entry
/// array of base64(flag || key), flag 0x00 for ed25519. This is the
/// `FileBasedKeystore` format the walrus binary reads from `SUI_CONFIG_DIR`.
/// Takes the raw 32-byte scalar so the caller holds both representations;
/// `Ed25519PrivateKey::new(scalar)` is exactly what `generate` constructs.
pub fn keystore_json(key_bytes: &[u8; 32]) -> String {
    let mut bytes = vec![0u8];
    bytes.extend_from_slice(key_bytes);
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("[\"{encoded}\"]")
}

/// Generates the daemon's main wallet into `workdir/walrus/sui_config/` and
/// returns the keystore path with the wallet's Sui address.
pub fn prepare_wallet(workdir: &Path) -> Result<(PathBuf, String)> {
    let mut scalar = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut scalar);
    let key = Ed25519PrivateKey::new(scalar);
    let address = key.public_key().derive_address().to_string();

    let sui_config_dir = walrus_dir(workdir).join("sui_config");
    std::fs::create_dir_all(&sui_config_dir)?;
    let keystore = sui_config_dir.join("sui.keystore");
    std::fs::write(&keystore, keystore_json(&scalar))?;

    // The daemon creates sub-wallets under this directory at startup.
    std::fs::create_dir_all(walrus_dir(workdir).join("sub-wallets"))?;
    Ok((keystore, address))
}

/// The workdir directory holding the daemon wallet, client config, and
/// sub-wallets; bind-mounted into the compose services.
pub fn walrus_dir(workdir: &Path) -> PathBuf {
    workdir.join("walrus")
}

/// Writes the daemon's client config into the workdir. Split from the fetch
/// so the write path is testable without the network.
fn write_client_config(workdir: &Path, content: &str) -> Result<PathBuf> {
    let path = walrus_dir(workdir).join("client.yaml");
    std::fs::create_dir_all(walrus_dir(workdir))?;
    std::fs::write(&path, content)?;
    Ok(path)
}

/// Writes the daemon's client config into the workdir, preferring a fresh
/// fetch of the canonical testnet config and falling back to the committed
/// fixture when the docs site is unreachable.
pub async fn ensure_client_config(workdir: &Path, http: &reqwest::Client) -> Result<PathBuf> {
    let fetched = async {
        let resp = http
            .get(CLIENT_CONFIG_URL)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .ok()?;
        let text = resp.error_for_status().ok()?.text().await.ok()?;
        Some(text)
    };
    let content = match fetched.await {
        Some(text) if text.contains("system_object") => text,
        // Network hiccup or a docs-side format change: the committed copy is
        // the stable fallback.
        _ => CLIENT_CONFIG_FIXTURE.to_string(),
    };
    write_client_config(workdir, &content)
}

/// The compose `walrus` service: one daemon serving both the publisher write
/// path and the aggregator read path on the same port. Sub-wallet knobs are
/// tuned down from the defaults so refills move small amounts and a
/// faucet-sized main wallet lasts the run.
pub fn daemon_service(walrus_port: u16) -> String {
    format!(
        r#"  walrus:
    image: {WALRUS_IMAGE}
    pull_policy: always
    entrypoint: /opt/walrus/bin/walrus
    command:
      - --config
      - /walrus/client.yaml
      - daemon
      - --bind-address
      - 0.0.0.0:{DAEMON_PORT}
      - --sub-wallets-dir
      - /walrus/sub-wallets
      - --n-clients
      - "1"
      - --sub-wallets-min-balance
      - "2000000000"
      - --wal-refill-amount
      - "2000000000"
    environment:
      SUI_CONFIG_DIR: /walrus/sui_config
      RUST_LOG: info
    ports:
      - "{walrus_port}:{DAEMON_PORT}"
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:{DAEMON_PORT}/v1/api"]
      interval: 10s
      timeout: 5s
      retries: 30
      start_period: 60s
    volumes:
      - ./walrus/sui_config:/walrus/sui_config:z
      - ./walrus/client.yaml:/walrus/client.yaml:ro,z
      - ./walrus/sub-wallets:/walrus/sub-wallets:z
    restart: "no"
"#
    )
}

/// Appends the SUI→WAL swap commands to `builder`: `SplitCoins` from the Gas
/// input (the balance-model wallet holds no coin objects, so the coin must be
/// materialized from the balance) followed by
/// `wal_exchange::exchange_all_for_wal`. The exchange transfers the WAL to
/// the sender; the builder injects the `TxContext` argument from the function
/// signature.
pub fn append_exchange_swap(builder: &mut TransactionBuilder, sender: Address, amount_mist: u64) {
    let exchange_arg = builder.object(ObjectInput::shared(WAL_EXCHANGE_OBJECT, 1, true));
    let amount_arg = builder.pure(&amount_mist);
    // One amount → one split coin; `split_coins` returns one argument per
    // amount.
    let gas = builder.gas();
    let coin = builder.split_coins(gas, vec![amount_arg]);
    let coin = coin.into_iter().next().expect("one amount, one coin");
    // The exchange hands the WAL back as the call's return value; it must be
    // consumed (transferred to the sender) or simulation rejects the PTB with
    // unused-result-without-drop.
    let wal = builder.move_call(
        Function::new(
            WAL_EXCHANGE_PACKAGE,
            Identifier::new("wal_exchange").expect("static name"),
            Identifier::new("exchange_all_for_wal").expect("static name"),
        ),
        vec![exchange_arg, coin],
    );
    let recipient = builder.pure(&sender);
    builder.transfer_objects(vec![wal], recipient);
}

/// The keystore entry whose key derives `address` — a keystore may hold
/// several entries and the funding flow must swap from the FUNDED wallet,
/// not whatever entry happens to sort first.
pub fn wallet_key_for(keystore_path: &Path, address: &Address) -> Result<Ed25519PrivateKey> {
    let raw = std::fs::read_to_string(keystore_path)
        .with_context(|| format!("reading {}", keystore_path.display()))?;
    let entries: Vec<String> =
        serde_json::from_str(&raw).context("parsing the keystore JSON array")?;
    entries
        .iter()
        .filter_map(|entry| podseq_sui::parse_signer_key(entry).ok())
        .find(|key| &key.public_key().derive_address() == address)
        .context("no keystore entry derives the wallet address")
}

/// Builds, signs, and executes the SUI→WAL swap for the wallet behind `key`,
/// waiting for checkpoint inclusion.
///
/// # Errors
///
/// Rpc/build/signature errors; an on-chain abort arrives as an execution
/// error.
pub async fn swap_sui_for_wal(
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    amount_mist: u64,
) -> Result<()> {
    let sender = key.public_key().derive_address();
    let mut rpc = RpcClient::new(rpc_url)
        .map_err(|e| anyhow::anyhow!("rpc client: {e}"))
        .context("building the rpc client")?;
    let mut builder = TransactionBuilder::new();
    builder.set_sender(sender);
    append_exchange_swap(&mut builder, sender, amount_mist);
    let tx = builder
        .build(&mut rpc)
        .await
        .context("building the swap PTB")?;
    let digest = tx.digest().to_string();
    let signature = key.sign_transaction(&tx).context("signing the swap PTB")?;
    rpc.execute_transaction_and_wait_for_checkpoint(
        ExecuteTransactionRequest::new(tx.into()).with_signatures(vec![signature.into()]),
        TX_WAIT,
    )
    .await
    .with_context(|| format!("executing the swap PTB {digest}"))?;
    Ok(())
}

/// The `[walrus]` section of the e2e podseq.toml: both URLs point at the
/// in-stack daemon, and blobs store for one epoch.
pub fn e2e_walrus_config() -> &'static str {
    r#"[walrus]
# The in-stack daemon pays storage from its own faucet-funded wallet, so the
# e2e stores for 1 epoch — blobs only need to outlive the run — instead of the
# production default of 53.
publisher_url = "http://walrus:9000"
aggregator_url = "http://walrus:9000"
epochs = 1
"#
}

/// The SUI amount to swap for WAL: everything above the gas floor, capped at
/// the WAL target. `None` when the balance leaves nothing above the floor to
/// swap.
fn swap_amount(balance_mist: u64, targets: &FundingTargets) -> Option<u64> {
    let available = balance_mist.saturating_sub(targets.sui_floor_mist);
    let swap = available.min(targets.wal_target_frost);
    (swap > 0).then_some(swap)
}

/// Funds the daemon wallet for a run: tops SUI up to the targets through the
/// release-profile faucet helper (the PoW grind must outrun the checkpoint
/// freshness window), then exchanges WAL through our own swap PTB — see the
/// module docs for why `walrus get-wal` cannot spend a faucet-funded balance.
pub async fn fund_wallet(
    wallet_address: &str,
    keystore_path: &Path,
    rpc_url: &str,
    faucet_url: &str,
) -> Result<()> {
    let targets = FundingTargets::defaults();
    let balance = podseq_sui::settlement::sui_balance(rpc_url, wallet_address)
        .await
        .context("checking the walrus wallet SUI balance")?;
    let rounds = faucet_requests_needed(balance, &targets);
    if rounds > 0 {
        refill_wallet(
            rpc_url,
            faucet_url,
            wallet_address,
            targets.sui_floor_mist + targets.wal_target_frost,
            rounds as usize,
        )
        .await?;
    }
    let key = wallet_key_for(
        keystore_path,
        &wallet_address.parse().context("parsing wallet address")?,
    )?;
    let swap = swap_amount(balance, &targets)
        .context("balance leaves nothing above the gas floor to swap for WAL")?;
    swap_sui_for_wal(rpc_url, &key, swap)
        .await
        .context("swapping SUI for WAL")
}

/// Refills `recipient` to `target_mist` through the release-profile helper,
/// one faucet payout per round (each PoW grind must run release-fast to beat
/// the checkpoint freshness window, which a debug-profile test binary cannot).
async fn refill_wallet(
    rpc_url: &str,
    faucet_url: &str,
    recipient: &str,
    target_mist: u64,
    max_rounds: usize,
) -> Result<()> {
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/release/faucet-refill");
    if !helper.exists() {
        bail!(
            "the walrus wallet needs {target_mist} MIST and the release refill \
             helper is missing at {}; build it with \
             `cargo build --release -p podseq-e2e --bin faucet-refill`",
            helper.display()
        );
    }
    let output = tokio::process::Command::new(&helper)
        .args(["--rpc", rpc_url])
        .args(["--faucet", faucet_url])
        .args(["--recipient", recipient])
        .args(["--min-mist", &target_mist.to_string()])
        .args(["--max-rounds", &max_rounds.to_string()])
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .output()
        .await
        .context("running the faucet refill helper for the walrus wallet")?;
    if !output.status.success() {
        bail!("faucet refill helper failed: {}", output.status);
    }
    Ok(())
}

/// Waits for the in-stack Walrus daemon to answer HTTP. Any response counts
/// as ready (the healthcheck already gates podseq on the API being served);
/// connection refused means the daemon is still starting.
pub async fn wait_for_daemon(url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if client.get(format!("{url}/v1/api")).send().await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timeout waiting for the walrus daemon at {url}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_wallet_needs_the_full_budget() {
        let targets = FundingTargets::defaults();
        // 4 SUI gas floor + 2 WAL target, one payout per round.
        assert_eq!(faucet_requests_needed(0, &targets), 6);
    }

    #[test]
    fn swap_ptb_splits_balance_gas_and_exchanges_for_wal() {
        let sender = Address::new([0x07; 32]);

        let mut builder = TransactionBuilder::new();
        builder.set_sender(sender);
        // Offline build: pin the gas input so `try_build` never touches the
        // network; the live build resolves gas from the address balance (the
        // accumulator model pays from the balance, no coin objects exist).
        builder.add_gas_objects(vec![sui_transaction_builder::ObjectInput::owned(
            Address::new([0x08; 32]),
            1,
            sui_sdk_types::Digest::new([0x09; 32]),
        )]);
        builder.set_gas_budget(50_000_000);
        builder.set_gas_price(1_000);
        append_exchange_swap(&mut builder, sender, 1_000_000_000);

        let tx = builder.try_build().expect("offline build");
        assert_eq!(tx.sender, sender);

        let sui_sdk_types::ProgrammableTransaction { commands, .. } = match &tx.kind {
            sui_sdk_types::TransactionKind::ProgrammableTransaction(p) => p,
            other => panic!("expected a programmable transaction, got {other:?}"),
        };
        assert_eq!(commands.len(), 3, "SplitCoins + MoveCall + TransferObjects");

        let sui_sdk_types::SplitCoins { coin, amounts } = match &commands[0] {
            sui_sdk_types::Command::SplitCoins(c) => c,
            other => panic!("expected SplitCoins first, got {other:?}"),
        };
        assert_eq!(*coin, sui_sdk_types::Argument::Gas);
        // Inputs register in call order: the exchange object is Input(0),
        // the split amount and the swap recipient follow.
        assert_eq!(amounts, &[sui_sdk_types::Argument::Input(1)]);

        let call = match &commands[1] {
            sui_sdk_types::Command::MoveCall(c) => c,
            other => panic!("expected MoveCall second, got {other:?}"),
        };
        assert_eq!(
            call.package.to_string(),
            "0x82593828ed3fcb8c6a235eac9abd0adbe9c5f9bbffa9b1e7a45cdd884481ef9f"
        );
        assert_eq!(call.module.as_str(), "wal_exchange");
        assert_eq!(call.function.as_str(), "exchange_all_for_wal");
        // The exchange object is the first input; the split coin moves by
        // value; the TxContext argument is injected by the builder.
        assert_eq!(call.arguments.len(), 2, "exchange object + split coin");

        let transfer = match &commands[2] {
            sui_sdk_types::Command::TransferObjects(t) => t,
            other => panic!("expected TransferObjects third, got {other:?}"),
        };
        // The exchange returns the WAL coin; it moves to the sender. The
        // builder emits the single-return shortcut `Result(1)`.
        assert_eq!(transfer.objects, &[sui_sdk_types::Argument::Result(1)]);
        assert_eq!(transfer.address, sui_sdk_types::Argument::Input(2));
    }

    #[test]
    fn wallet_key_for_picks_the_entry_matching_the_address() {
        let root =
            std::env::temp_dir().join(format!("podseq-e2e-walletkey2-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut first = [0u8; 32];
        let mut second = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut first);
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut second);
        let keystore = root.join("sui.keystore");
        // `keystore_json` serializes a ONE-entry array; splice the two quoted
        // entries into one multi-entry keystore file.
        let (a, b) = (keystore_json(&first), keystore_json(&second));
        let (a, b) = (
            a.trim().trim_matches(|c| c == '[' || c == ']'),
            b.trim().trim_matches(|c| c == '[' || c == ']'),
        );
        std::fs::write(&keystore, format!("[{a}, {b}]")).unwrap();

        let want = Ed25519PrivateKey::new(second);
        let address = want.public_key().derive_address();
        let key = wallet_key_for(&keystore, &address).expect("picking the funded entry");
        assert_eq!(key.public_key().derive_address(), address);
        let foreign = sui_sdk_types::Address::new([0xff; 32]);
        assert!(
            wallet_key_for(&keystore, &foreign).is_err(),
            "an address with no entry has no key"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn funded_wallet_needs_no_requests() {
        let targets = FundingTargets::defaults();
        let funded = targets.sui_floor_mist + targets.wal_target_frost;
        assert_eq!(faucet_requests_needed(funded, &targets), 0);
    }

    #[test]
    fn one_mist_short_still_needs_one_request() {
        let targets = FundingTargets::defaults();
        let funded = targets.sui_floor_mist + targets.wal_target_frost - 1;
        assert_eq!(faucet_requests_needed(funded, &targets), 1);
    }

    #[test]
    fn overfunded_wallet_saturates_at_zero() {
        let targets = FundingTargets::defaults();
        assert_eq!(faucet_requests_needed(u64::MAX, &targets), 0);
    }

    #[test]
    fn keystore_json_is_flag_prefixed_base64() {
        let scalar = [0x2Au8; 32];
        let json = keystore_json(&scalar);
        let parsed: Vec<String> = serde_json::from_str(&json).expect("valid JSON array");
        assert_eq!(parsed.len(), 1);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&parsed[0])
            .expect("valid base64");
        assert_eq!(raw[0], 0u8, "ed25519 flag byte");
        assert_eq!(&raw[1..], &scalar[..], "key bytes follow the flag");
    }

    #[test]
    fn keystore_json_parses_as_a_sui_signer_key() {
        // The wallet file must round-trip through the same parser podseq uses
        // for signer keys, so the daemon wallet is inspectable with the
        // existing tooling.
        let scalar = [0x5Eu8; 32];
        let key = Ed25519PrivateKey::new(scalar);
        let expected_address = key.public_key().derive_address().to_string();
        let json = keystore_json(&scalar);
        let parsed: Vec<String> = serde_json::from_str(&json).expect("valid JSON array");
        let key2 = podseq_sui::parse_signer_key(&parsed[0]).expect("parses as flag+key");
        assert_eq!(
            key2.public_key().derive_address().to_string(),
            expected_address,
            "the keystore entry derives the wallet address"
        );
    }

    #[test]
    fn daemon_service_runs_the_combined_daemon() {
        let svc = daemon_service(19335);
        assert!(svc.contains(WALRUS_IMAGE), "image: {svc}");
        assert!(svc.contains("daemon"), "daemon subcommand: {svc}");
        assert!(svc.contains("0.0.0.0:9000"), "bind address: {svc}");
        assert!(svc.contains("SUI_CONFIG_DIR"), "wallet dir env: {svc}");
        assert!(svc.contains("/walrus/sub-wallets"), "sub-wallets: {svc}");
        assert!(svc.contains("19335:9000"), "host port mapping: {svc}");
        assert!(svc.contains("/v1/api"), "healthcheck path: {svc}");
    }

    #[test]
    fn daemon_service_tunes_sub_wallet_refills_down() {
        let svc = daemon_service(19335);
        assert!(svc.contains("--n-clients"), "single client: {svc}");
        assert!(
            svc.contains("--sub-wallets-min-balance"),
            "min balance: {svc}"
        );
        assert!(svc.contains("--wal-refill-amount"), "wal refill: {svc}");
        assert!(
            !svc.contains("--gas-refill-amount"),
            "gas refill stays default: {svc}"
        );
    }

    #[test]
    fn swap_amount_caps_at_the_wal_target_and_refuses_a_drained_balance() {
        let targets = FundingTargets::defaults();
        // Fully funded: swap the WAL target, keep the gas floor.
        assert_eq!(swap_amount(6_000_000_000, &targets), Some(2_000_000_000));
        // Partially funded: swap what exists above the floor.
        assert_eq!(swap_amount(6_500_000_000, &targets), Some(2_000_000_000));
        assert_eq!(swap_amount(4_500_000_000, &targets), Some(500_000_000));
        // At or below the gas floor: nothing to swap.
        assert_eq!(swap_amount(4_000_000_000, &targets), None);
        assert_eq!(swap_amount(0, &targets), None);
    }

    #[test]
    fn e2e_config_points_at_the_daemon_with_one_epoch() {
        let cfg = e2e_walrus_config();
        assert!(
            cfg.contains("publisher_url = \"http://walrus:9000\""),
            "{cfg}"
        );
        assert!(
            cfg.contains("aggregator_url = \"http://walrus:9000\""),
            "{cfg}"
        );
        assert!(cfg.contains("epochs = 1"), "{cfg}");
    }

    #[test]
    fn committed_client_config_is_a_complete_testnet_config() {
        for needle in [
            "system_object",
            "staking_object",
            "exchange_objects",
            "n_shards",
            "fullnode.testnet.sui.io",
        ] {
            assert!(
                CLIENT_CONFIG_FIXTURE.contains(needle),
                "fixture missing {needle}: {CLIENT_CONFIG_FIXTURE}"
            );
        }
    }

    #[test]
    fn prepare_wallet_writes_a_parseable_keystore() {
        let workdir = tempfile::tempdir().expect("tempdir");
        let (keystore, address) = prepare_wallet(workdir.path()).expect("wallet generated");
        let raw = std::fs::read_to_string(&keystore).expect("keystore written");
        let parsed: Vec<String> = serde_json::from_str(&raw).expect("valid JSON array");
        let key = podseq_sui::parse_signer_key(&parsed[0]).expect("parses as flag+key");
        assert_eq!(
            key.public_key().derive_address().to_string(),
            address,
            "the written wallet derives the returned address"
        );
        assert!(
            workdir.path().join("walrus/sub-wallets").exists(),
            "sub-wallets directory pre-created"
        );
    }

    #[test]
    fn client_config_write_lands_in_the_walrus_dir() {
        let workdir = tempfile::tempdir().expect("tempdir");
        let path = write_client_config(workdir.path(), "system_object: 0x1").expect("written");
        assert_eq!(
            std::fs::read_to_string(&path).expect("readback"),
            "system_object: 0x1"
        );
    }
}

/// Live validation against testnet, run explicitly:
/// `cargo test -p podseq-e2e --lib walrus::tests::live -- --ignored --nocapture`
/// with `PODSEQ_LIVE_WAL_KEYSTORE` pointing at a funded keystore JSON.
#[cfg(test)]
mod live {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn swap_exchanges_one_sui_for_wal() {
        let keystore = std::env::var("PODSEQ_LIVE_WAL_KEYSTORE")
            .expect("set PODSEQ_LIVE_WAL_KEYSTORE to a funded keystore JSON");
        let rpc = "https://fullnode.testnet.sui.io:443";
        let key = wallet_key_for(
            Path::new(&keystore),
            &std::env::var("PODSEQ_LIVE_WAL_ADDRESS")
                .expect("set PODSEQ_LIVE_WAL_ADDRESS to the funded address")
                .parse()
                .expect("parsing the address"),
        )
        .expect("picking the funded entry");
        let sender = key.public_key().derive_address();
        let balance = podseq_sui::settlement::sui_balance(rpc, &sender.to_string())
            .await
            .expect("reading balance");
        println!("live swap: sender {sender} balance {balance} MIST");
        swap_sui_for_wal(rpc, &key, 1_000_000_000)
            .await
            .expect("live swap");
        println!("live swap: exchanged 1 SUI for WAL at the testnet exchange");
    }
}
