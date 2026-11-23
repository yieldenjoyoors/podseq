//! Self-hosted Walrus publisher for the full-stack e2e.
//!
//! The DA leg posts block batches to a Walrus publisher and reads them back
//! through an aggregator. The public testnet daemons are run by Mysten Labs
//! and their WAL wallets drain between operator refills, which fails every
//! POST with `500 could not find WAL coins` — not something a retry can ride
//! out. This module brings our own `walrus daemon` (combined
//! aggregator + publisher) up inside the compose stack, funded from the
//! faucet-PoW-refillable SUI: faucet SUI covers gas, and `walrus get-wal`
//! exchanges SUI 1:1 for testnet WAL at the official exchange.
//!
//! Cost shape that sizes the funding: Walrus bills the encoded size of a blob
//! (about 4.5x the original plus a fixed 64 MB of metadata per blob), so a
//! small DA batch costs ~2.1 WAL per epoch — the metadata floor dominates.
//! The e2e therefore stores with `epochs = 1` (~2.1 WAL per blob for a run)
//! instead of the production default of 53 epochs (~110 WAL per blob), which
//! no faucet-funded wallet can cover.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use sui_crypto::ed25519::Ed25519PrivateKey;

/// Walrus client image carrying the full `walrus` binary (daemon, get-wal).
/// The floating `testnet` tag tracks the release Walrus validates against the
/// current testnet protocol, the same posture as the pinned-then-pulled Reth
/// image: an old client build fails against a bumped protocol.
pub const WALRUS_IMAGE: &str = "mysten/walrus-service:testnet";

/// Port the daemon binds inside the container.
const DAEMON_PORT: u16 = 9000;

/// One faucet PoW payout is ~1 SUI; requests are budgeted in whole payouts.
const ONE_SUI_MIST: u64 = 1_000_000_000;

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
    /// Covers ~7 blob stores at ~2.1 WAL each (testnet prices a small blob at
    /// ~2.088 WAL per epoch because the 64 MB metadata floor dominates), plus
    /// a 4 SUI gas floor for the exchange and daemon refills.
    pub fn defaults() -> Self {
        Self {
            sui_floor_mist: 4_000_000_000,
            wal_target_frost: 16_000_000_000,
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

/// The one-shot `walrus-fund` service: `get-wal` converts main-wallet SUI to
/// WAL at the official testnet exchange. Gated behind the `fund` compose
/// profile so `up` skips it; the funding flow runs it explicitly before `up`.
pub fn fund_service(get_wal_mist: u64) -> String {
    format!(
        r#"  walrus-fund:
    image: {WALRUS_IMAGE}
    pull_policy: always
    entrypoint: /opt/walrus/bin/walrus
    command:
      - --config
      - /walrus/client.yaml
      - get-wal
      - --amount
      - "{get_wal_mist}"
    environment:
      SUI_CONFIG_DIR: /walrus/sui_config
    volumes:
      - ./walrus/sui_config:/walrus/sui_config:z
      - ./walrus/client.yaml:/walrus/client.yaml:ro,z
    profiles:
      - fund
    restart: "no"
"#
    )
}

/// The `[walrus]` section of the e2e podseq.toml: both URLs point at the
/// in-stack daemon, and blobs store for one epoch.
pub fn e2e_walrus_config() -> &'static str {
    r#"[walrus]
# The in-stack daemon pays storage from its own faucet-funded wallet, so the
# e2e stores for 1 epoch (~2.1 WAL per small blob — the fixed 64 MB metadata
# dominates) instead of the production default of 53 (~110 WAL per blob).
publisher_url = "http://walrus:9000"
aggregator_url = "http://walrus:9000"
epochs = 1
"#
}

/// Funds the daemon wallet for a run: tops SUI up to the targets through the
/// release-profile faucet helper (the PoW grind must outrun the checkpoint
/// freshness window), then exchanges WAL through the `walrus-fund` one-shot.
pub async fn fund_wallet(
    workdir: &Path,
    wallet_address: &str,
    rpc_url: &str,
    faucet_url: &str,
    project: &str,
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

    let output = tokio::process::Command::new("docker")
        .args([
            "compose",
            "--profile",
            "fund",
            "-p",
            project,
            "-f",
            "docker-compose.yml",
            "run",
            "--rm",
            "walrus-fund",
        ])
        .current_dir(workdir)
        .output()
        .await
        .context("running the walrus-fund one-shot")?;
    if !output.status.success() {
        bail!(
            "walrus-fund failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
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
        assert_eq!(faucet_requests_needed(0, &targets), 20);
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
    fn fund_service_exchanges_sui_for_wal_off_the_up_path() {
        let svc = fund_service(16_000_000_000);
        assert!(svc.contains("get-wal"), "exchange command: {svc}");
        assert!(svc.contains("\"16000000000\""), "quoted amount: {svc}");
        assert!(svc.contains("profiles"), "compose profile: {svc}");
        assert!(svc.contains("- fund"), "fund profile name: {svc}");
        assert!(!svc.contains("ports:"), "no published ports: {svc}");
        assert!(svc.contains("SUI_CONFIG_DIR"), "wallet dir env: {svc}");
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
