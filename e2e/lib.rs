//! E2E harness.
//!
//! Two stacks:
//! - [` Stack`] — Reth only. Fast, deterministic, used by `engine_integration`.
//! - [`FullStack`] — Reth + the real `podseq` binary, talking public Sui and
//!   Walrus testnet. Requires a funded `sui.key` (CI secret `SUI_SIGNER_KEY`).
//!   Used by `full_stack` to verify the production binary produces blocks,
//!   includes user txs, settles on Sui, and posts retrievable blobs on Walrus.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Default Reth image tag pinned for reproducible CI runs.
pub const RETH_IMAGE: &str = "ghcr.io/paradigmxyz/reth:latest";

/// Polling interval when waiting for an endpoint to become reachable.
const POLL: Duration = Duration::from_millis(500);

/// Local ports the compose stack binds on the host.
pub struct Ports {
    pub rpc: u16,
    pub engine: u16,
}

impl Ports {
    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.rpc)
    }

    pub fn engine_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.engine)
    }
}

/// A running e2e stack. Dropping it tears everything down.
pub struct Stack {
    project: String,
    workdir: PathBuf,
    jwt_path: PathBuf,
    ports: Ports,
}

impl Stack {
    pub fn ports(&self) -> &Ports {
        &self.ports
    }

    pub fn jwt_path(&self) -> &Path {
        &self.jwt_path
    }

    /// Start a stack on the given host ports. Generates a fresh JWT secret and a
    /// one-shot compose file that points Reth at the workspace dev genesis.
    pub async fn start(rpc_port: u16, engine_port: u16) -> Result<Self> {
        let project = format!(
            "podseq-e2e-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let workdir = tempfile::tempdir()?.keep();

        // Shared Engine API JWT: 32 random bytes, hex-encoded.
        let jwt_path = workdir.join("jwt.hex");
        let secret = random_hex_32();
        std::fs::write(&jwt_path, &secret)?;

        let genesis_src = workspace_root().join("examples/reth-genesis.json");
        let genesis_dst = workdir.join("reth-genesis.json");
        std::fs::copy(&genesis_src, &genesis_dst)
            .with_context(|| format!("copying {}", genesis_src.display()))?;

        // Project name uniqueness guarantees isolation when several e2e shards run
        // concurrently on one CI runner. Volumes are scoped to the project and
        // removed on teardown.
        let compose = format!(
            r#"services:
  reth:
    image: {image}
    pull_policy: always
    command:
      - node
      - --chain=/genesis/reth-genesis.json
      - --authrpc.jwtsecret=/jwt/jwt.hex
      - --authrpc.addr=0.0.0.0
      - --authrpc.port=8551
      - --http
      - --http.addr=0.0.0.0
      - --http.port=8545
      - --http.api=eth,net,web3,debug,txpool,trace
      - --datadir=/data
      - --disable-discovery
      - --jit
    ports:
      - "{rpc_port}:8545"
      - "{engine_port}:8551"
    volumes:
      # `:z` relabels the bind mounts for container access on SELinux hosts
      # (Fedora/RHEL); it is a no-op elsewhere.
      - ./jwt.hex:/jwt/jwt.hex:ro,z
      - ./reth-genesis.json:/genesis/reth-genesis.json:ro,z
    restart: "no"
"#,
            image = RETH_IMAGE,
            rpc_port = rpc_port,
            engine_port = engine_port,
        );
        std::fs::write(workdir.join("docker-compose.yml"), compose)?;

        compose_up(&project, &workdir).await?;

        let stack = Self {
            project,
            workdir,
            jwt_path,
            ports: Ports {
                rpc: rpc_port,
                engine: engine_port,
            },
        };

        stack.wait_ready().await?;
        Ok(stack)
    }

    /// Block until Reth answers `eth_blockNumber` on both RPC and Engine ports.
    pub async fn wait_ready(&self) -> Result<()> {
        wait_for_http_ok(&self.ports.rpc_url(), "eth_blockNumber").await?;
        wait_for_http_ok(&self.ports.engine_url(), "eth_blockNumber").await?;
        Ok(())
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // Best-effort teardown; ignore failures so a leaked container never panics
        // the test thread on the way out.
        let _ = std::process::Command::new("docker")
            .args([
                "compose",
                "-p",
                &self.project,
                "down",
                "-v",
                "--remove-orphans",
            ])
            .current_dir(&self.workdir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at e2e/; the workspace root is its parent.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("e2e crate has a parent directory")
        .to_path_buf()
}

async fn compose_up(project: &str, workdir: &Path) -> Result<()> {
    // `docker compose up` is detached; we then poll for readiness separately so a
    // slow image pull in CI does not look like a failure.
    let output = tokio::process::Command::new("docker")
        .args([
            "compose",
            "-p",
            project,
            "-f",
            "docker-compose.yml",
            "up",
            "-d",
            "--quiet-pull",
        ])
        .current_dir(workdir)
        .output()
        .await
        .context("running docker compose up")?;

    if !output.status.success() {
        bail!(
            "docker compose up failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn wait_for_http_ok(url: &str, method: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": [],
    });

    loop {
        // Any HTTP response counts as ready: the Engine API answers unauthenticated
        // requests with 401, which still proves the server is listening. Connection
        // refused / timeout means Reth is not up yet.
        if client.post(url).json(&body).send().await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timeout waiting for {url} to answer {method}");
        }
        tokio::time::sleep(POLL).await;
    }
}

fn random_hex_32() -> String {
    let buf: [u8; 32] = rand::random();
    hex::encode(buf)
}

/// The full podseq stack: Reth + the real `podseq` binary, talking public Sui
/// and Walrus testnet. Bringing this up requires a funded Sui key so podseq can
/// settle; without one the test that uses it skips itself.
///
/// The compose files are the production `docker-compose.yml` + testnet override.
/// Secrets are written to a temp dir and mounted read-only, so the repo's own
/// `docker/secrets/` is never touched.
pub struct FullStack {
    project: String,
    workdir: PathBuf,
    ports: Ports,
    /// Path to the generated bridge relayer EVM key (32-byte secp256k1 hex).
    relayer_key_path: PathBuf,
}

impl FullStack {
    /// Writes a funded `sui.key` (from `SUI_SIGNER_KEY` env or `docker/secrets/sui.key`)
    /// into a temp workdir, renders the production compose files pointing at that
    /// workdir, and starts Reth + podseq. Returns once both answer HTTP.
    ///
    /// Errors if no funded key is available — the full-stack test requires one.
    pub async fn start(rpc_port: u16, engine_port: u16) -> Result<Self> {
        let sui_key = sui_signer_key()?.ok_or_else(|| {
            anyhow::anyhow!("no funded Sui key found; set SUI_SIGNER_KEY or docker/secrets/sui.key")
        })?;

        let project = format!(
            "podseq-e2e-full-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let workdir = tempfile::tempdir()?.keep();

        // Mirror the layout the production compose expects: secrets/sui.key,
        // shared jwt file (bind-mounted), genesis mounted from repo.
        let secrets_dir = workdir.join("secrets");
        std::fs::create_dir_all(&secrets_dir)?;
        std::fs::write(secrets_dir.join("sui.key"), sui_key.as_bytes())?;

        let repo_root = workspace_root();

        // Production compose files live at the repo root. Bind-mount them into
        // the workdir so relative paths inside (./docker/secrets, ./examples/...)
        // resolve against the workdir, not the repo.
        std::fs::copy(
            repo_root.join("examples/reth-genesis.json"),
            workdir.join("reth-genesis.json"),
        )?;

        // JWT secret generated in Rust (the shell `od | tr` pipeline is fragile
        // across `od` implementations and produced a 65-char key in CI once).
        std::fs::write(workdir.join("jwt.hex"), random_hex_32())?;

        // Bridge relayer EVM key: a fresh 32-byte secp256k1 scalar. podseq loads
        // it from `bridge.l2_relayer_key_path`; the test funds its address from
        // the genesis account and bootstraps the L2 bridge contracts with it.
        let relayer_signer = alloy_signer_local::PrivateKeySigner::random();
        let relayer_key_path = secrets_dir.join("relayer.key");
        std::fs::write(&relayer_key_path, hex::encode(relayer_signer.to_bytes().0))?;

        // Write a config for this run and bind-mount it into the podseq
        // container. Same flow as a real deployment: `podseq start --config
        // podseq.toml` against a TOML file.
        let podseq_toml = r#"[reth]
engine_url = "http://reth:8551"
rpc_url = "http://reth:8545"
jwt_path = "/jwt/jwt.hex"

[walrus]
publisher_url = "https://publisher.walrus-testnet.walrus.space"
aggregator_url = "https://aggregator.walrus-testnet.walrus.space"
epochs = 53

[sui]
rpc_url = "https://fullnode.testnet.sui.io:443"

[signer]
key_path = "/secrets/sui.key"

[sequencer]
block_time_ms = 5000

[bridge]
enabled = true
l2_relayer_key_path = "/secrets/relayer.key"

mode = "sequencer"
"#;
        std::fs::write(workdir.join("podseq.toml"), podseq_toml)?;

        // Compose: Reth + podseq. podseq runs the real binary against the
        // bind-mounted TOML, talking public Sui/Walrus testnet.
        let compose = format!(
            r#"services:
  reth:
    image: {reth_image}
    pull_policy: always
    command:
      - node
      - --chain=/genesis/reth-genesis.json
      - --authrpc.jwtsecret=/jwt/jwt.hex
      - --authrpc.addr=0.0.0.0
      - --authrpc.port=8551
      - --http
      - --http.addr=0.0.0.0
      - --http.port=8545
      - --http.api=eth,net,web3,debug,txpool,trace
      - --datadir=/data
      - --disable-discovery
      - --jit
    ports:
      - "{rpc_port}:8545"
      - "{engine_port}:8551"
    volumes:
      # `:z` relabels the bind mounts for container access on SELinux hosts
      # (Fedora/RHEL); it is a no-op elsewhere.
      - ./jwt.hex:/jwt/jwt.hex:ro,z
      - reth-data:/data
      - ./reth-genesis.json:/genesis/reth-genesis.json:ro,z
    restart: "no"

  podseq:
    build:
      context: {context}
      dockerfile: Dockerfile
    image: podseq-e2e:{project}
    depends_on:
      reth:
        condition: service_started
    environment:
      RUST_LOG: "info,podseq=debug"
    volumes:
      - ./jwt.hex:/jwt/jwt.hex:ro,z
      - ./secrets:/secrets:ro,z
      - ./podseq.toml:/etc/podseq/podseq.toml:z
    restart: "no"

volumes:
  reth-data:
"#,
            reth_image = RETH_IMAGE,
            rpc_port = rpc_port,
            engine_port = engine_port,
            context = repo_root.display(),
            project = project,
        );
        std::fs::write(workdir.join("docker-compose.yml"), compose)?;

        compose_up(&project, &workdir).await?;

        let stack = Self {
            project,
            workdir,
            ports: Ports {
                rpc: rpc_port,
                engine: engine_port,
            },
            relayer_key_path,
        };

        // Reth readiness is the gating signal; podseq may still be deploying
        // settlement (publishing the Move package takes real Sui gas).
        if let Err(e) = wait_for_http_ok(&stack.ports.rpc_url(), "eth_blockNumber").await {
            stack.dump_logs();
            return Err(e);
        }
        if let Err(e) = wait_for_http_ok(&stack.ports.engine_url(), "eth_blockNumber").await {
            stack.dump_logs();
            return Err(e);
        }
        Ok(stack)
    }

    pub fn ports(&self) -> &Ports {
        &self.ports
    }

    /// Path to the podseq.toml in the workdir. Podseq writes deployed settlement
    /// IDs back here on first start.
    pub fn config_path(&self) -> std::path::PathBuf {
        self.workdir.join("podseq.toml")
    }

    /// Path to the generated bridge relayer EVM key (32-byte secp256k1 hex).
    pub fn relayer_key_path(&self) -> &Path {
        &self.relayer_key_path
    }

    /// Reads a `section.key` string from the config file podseq keeps updating.
    /// Returns `Ok(None)` when the field is absent or the file doesn't parse yet
    /// (e.g. before auto-deploy has written it back).
    pub fn read_config_string(&self, section: &str, key: &str) -> Result<Option<String>> {
        let path = self.config_path();
        read_toml_field(&path, section, key)
    }

    /// Returns the podseq container's status, or `None` if `docker compose ps`
    /// fails or the container is absent. Used to fail fast when podseq crashes
    /// during startup instead of waiting out the full polling budget.
    fn podseq_state(&self) -> Option<String> {
        let out = std::process::Command::new("docker")
            .args([
                "compose",
                "-p",
                &self.project,
                "-f",
                "docker-compose.yml",
                "ps",
                "--status",
                "--format",
                "json",
                "podseq",
            ])
            .current_dir(&self.workdir)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        // `docker compose ps --format json` emits one JSON object per line.
        // Each has a "Status" field like "exited (1)" or "running".
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(s) = v.get("Status").and_then(|s| s.as_str()) {
                    return Some(s.to_string());
                }
            }
        }
        None
    }

    /// True when the podseq container has stopped (exited). Lets the test bail
    /// early with logs instead of polling for minutes after a startup crash.
    pub fn podseq_exited(&self) -> bool {
        self.podseq_state()
            .map(|s| s.starts_with("exited"))
            .unwrap_or(false)
    }

    /// Streams `docker compose logs podseq` until `predicate` returns true on the
    /// accumulated output, or `deadline` elapses.
    pub async fn wait_for_log<F: Fn(&str) -> bool>(
        &self,
        predicate: F,
        deadline: std::time::Instant,
    ) -> Result<()> {
        loop {
            let out = std::process::Command::new("docker")
                .args([
                    "compose",
                    "-p",
                    &self.project,
                    "-f",
                    "docker-compose.yml",
                    "logs",
                    "--tail=200",
                    "podseq",
                ])
                .current_dir(&self.workdir)
                .output()?;
            let text = String::from_utf8_lossy(&out.stdout);
            if text.lines().any(&predicate) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("timeout waiting for podseq log line; last output:\n{text}");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Polls `docker compose logs podseq` and returns the first non-None value
    /// produced by `extract` over any log line, or errors at `deadline`.
    pub async fn extract_from_log<T, F>(
        &self,
        extract: F,
        deadline: std::time::Instant,
    ) -> Result<T>
    where
        F: Fn(&str) -> Option<T>,
        T: Clone,
    {
        loop {
            let out = std::process::Command::new("docker")
                .args([
                    "compose",
                    "-p",
                    &self.project,
                    "-f",
                    "docker-compose.yml",
                    "logs",
                    "--tail=200",
                    "podseq",
                ])
                .current_dir(&self.workdir)
                .output()?;
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                if let Some(v) = extract(line) {
                    return Ok(v);
                }
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("extract_from_log: predicate never matched; last output:\n{text}");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Dumps all container logs to stderr. Called on startup failure (so the
    /// test's error output shows why the stack didn't come up) and on Drop
    /// when the test panics (so failures inside the test body leave evidence
    /// before the stack is torn down).
    pub fn dump_logs(&self) {
        let out = std::process::Command::new("docker")
            .args([
                "compose",
                "-p",
                &self.project,
                "-f",
                "docker-compose.yml",
                "logs",
            ])
            .current_dir(&self.workdir)
            .output();
        match out {
            Ok(o) => {
                eprintln!("--- docker compose logs (project {}) ---", self.project);
                eprintln!("{}", String::from_utf8_lossy(&o.stdout));
                if !o.stdout.is_empty() && !o.stderr.is_empty() {
                    eprintln!("--- stderr ---");
                    eprintln!("{}", String::from_utf8_lossy(&o.stderr));
                }
                eprintln!("--- end logs ---");
            }
            Err(e) => eprintln!("could not capture docker logs: {e}"),
        }
    }
}

impl Drop for FullStack {
    fn drop(&mut self) {
        // Dump logs on panic so test-body failures leave evidence before teardown.
        if std::thread::panicking() {
            self.dump_logs();
        }
        let _ = std::process::Command::new("docker")
            .args([
                "compose",
                "-p",
                &self.project,
                "down",
                "-v",
                "--remove-orphans",
            ])
            .current_dir(&self.workdir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

/// Returns the funded Sui signer key, preferring `SUI_SIGNER_KEY` (CI secret) and
/// falling back to `docker/secrets/sui.key` for local dev. `Ok(None)` means no
/// key is available.
pub fn sui_signer_key() -> Result<Option<String>> {
    if let Ok(k) = std::env::var("SUI_SIGNER_KEY") {
        let trimmed = k.trim();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_string()));
        }
    }
    let path = workspace_root().join("docker/secrets/sui.key");
    if path.is_file() {
        let s = std::fs::read_to_string(&path)?.trim().to_string();
        if !s.is_empty() {
            return Ok(Some(s));
        }
    }
    Ok(None)
}

/// Reads `section.key` as a string from a TOML file. Returns `Ok(None)` when the
/// file is missing, doesn't parse, or the field is absent.
fn read_toml_field(path: &Path, section: &str, key: &str) -> Result<Option<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    let value: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let s = match value
        .get(section)
        .and_then(|s| s.get(key))
        .and_then(|v| v.as_str())
    {
        Some(s) => s,
        None => return Ok(None),
    };
    Ok(Some(s.to_string()))
}

/// Skips the test when docker is unavailable (e.g. `cargo test` on a host
/// without it). Exits the process because there is no per-test skip mechanism.
pub fn require_docker() {
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

/// Hardhat dev account funded by `examples/reth-genesis.json`.
pub const GENESIS_PKEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
pub const GENESIS_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

/// BridgeFactory predeploy (genesis-planted bytecode, see `solidity/`).
pub const BRIDGE_FACTORY: &str = "0x4200000000000000000000000000000000000010";
/// Predeployed canonical SUI Bridge token (genesis-planted bytecode).
pub const BRIDGE_TOKEN_PREDEPLOY: &str = "0x4200000000000000000000000000000000000011";

/// Hand-rolled ABI encoding for the predeployed bridge contracts. Deliberately
/// independent of the production `sol!` types so test calldata is a second
/// implementation, not a shared one.
pub mod abi {
    use alloy_primitives::{keccak256, Address, U256};
    use anyhow::Result;

    /// Function selector: the first 4 bytes of keccak256 of the signature.
    pub fn selector(signature: &[u8]) -> Vec<u8> {
        keccak256(signature).0[..4].to_vec()
    }

    /// A 32-byte big-endian argument word.
    pub fn abi_word(v: U256) -> [u8; 32] {
        v.to_be_bytes()
    }

    /// Appends an ABI-encoded `string` to `out` (length word + padded bytes).
    pub fn push_string(out: &mut Vec<u8>, s: &str) {
        let bytes = s.as_bytes();
        let padded_len = bytes.len().div_ceil(32) * 32;
        out.extend_from_slice(&abi_word(U256::from(bytes.len())));
        out.extend_from_slice(bytes);
        out.resize(out.len() + (padded_len - bytes.len()), 0);
    }

    /// Appends a right-aligned `address` word to `out`.
    pub fn push_address(out: &mut Vec<u8>, addr: Address) {
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(addr.as_ref());
        out.extend_from_slice(&word);
    }

    /// ABI-encodes `initialize(address)` (BridgeFactory).
    pub fn encode_factory_initialize(relayer: Address) -> Vec<u8> {
        let mut out = selector(b"initialize(address)");
        push_address(&mut out, relayer);
        out
    }

    /// ABI-encodes `initialize(string,string,string,address)` (Bridge token).
    pub fn encode_bridge_initialize(
        name: &str,
        symbol: &str,
        coin_type: &str,
        relayer: Address,
    ) -> Vec<u8> {
        let mut out = selector(b"initialize(string,string,string,address)");
        // Head: 3 string offsets + 1 static address = 4 words.
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
        push_address(&mut out, relayer);
        out.extend_from_slice(&dynamic);
        out
    }

    /// ABI-encodes `adoptBridge(string,address)`.
    pub fn encode_adopt_bridge(coin_type: &str, token: Address) -> Vec<u8> {
        let mut out = selector(b"adoptBridge(string,address)");
        // Head: string offset + the static address word.
        out.extend_from_slice(&abi_word(U256::from(32 + 32)));
        push_address(&mut out, token);
        push_string(&mut out, coin_type);
        out
    }

    /// ABI-encodes `createBridge(string,string,string)`.
    pub fn encode_create_bridge(name: &str, symbol: &str, coin_type: &str) -> Vec<u8> {
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

    /// ABI-encodes `mint(address,uint256,uint64)`.
    pub fn encode_mint(recipient: Address, amount: u64, nonce: u64) -> Vec<u8> {
        let mut out = selector(b"mint(address,uint256,uint64)");
        push_address(&mut out, recipient);
        out.extend_from_slice(&abi_word(U256::from(amount)));
        out.extend_from_slice(&abi_word(U256::from(nonce)));
        out
    }

    /// ABI-encodes `initiateWithdrawal(bytes32,uint256)`.
    pub fn encode_initiate_withdrawal(sui_recipient: [u8; 32], amount: u64) -> Vec<u8> {
        let mut out = selector(b"initiateWithdrawal(bytes32,uint256)");
        out.extend_from_slice(&sui_recipient);
        out.extend_from_slice(&abi_word(U256::from(amount)));
        out
    }

    /// Calldata for a one-string-argument call with signature `sig`
    /// (e.g. `tokenFor(string)`).
    pub fn one_string(arg: &str, sig: &[u8]) -> Vec<u8> {
        let mut out = selector(sig);
        out.extend_from_slice(&abi_word(U256::from(32)));
        push_string(&mut out, arg);
        out
    }

    /// Reads the low-order 8 bytes of a 32-byte big-endian word.
    pub fn word_len(word: &[u8; 32]) -> usize {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(&word[24..]);
        u64::from_be_bytes(arr) as usize
    }

    /// Decodes an ABI `string` located at `len_offset` (the length word's
    /// position: 32 for a plain `string` return, 36 for `Error(string)`).
    pub fn decode_abi_string_at(bytes: &[u8], len_offset: usize) -> Result<String> {
        use anyhow::Context;
        anyhow::ensure!(
            bytes.len() >= len_offset + 32,
            "abi string return too short"
        );
        let len_word: [u8; 32] = bytes[len_offset..len_offset + 32]
            .try_into()
            .expect("checked length above");
        let len = word_len(&len_word);
        let data_pos = len_offset + 32;
        let end = data_pos
            .checked_add(len)
            .context("abi string length overruns usize")?;
        anyhow::ensure!(end <= bytes.len(), "abi string length overruns buffer");
        Ok(String::from_utf8_lossy(&bytes[data_pos..end]).to_string())
    }

    /// Decodes a Solidity `Error(string)` revert payload from an `eth_call`
    /// result; returns the raw hex when it is not one.
    pub fn decode_revert_string(hex_result: &str) -> String {
        let bytes = match hex::decode(hex_result.trim_start_matches("0x")) {
            Ok(b) => b,
            Err(_) => return hex_result.to_string(),
        };
        decode_abi_string_at(&bytes, 36).unwrap_or_else(|_| hex_result.to_string())
    }
}

/// Minimal JSON-RPC client for the L2 (Reth) HTTP endpoint.
pub mod eth {
    use alloy_primitives::{Address, U256};
    use anyhow::{bail, Context, Result};

    /// Posts a JSON-RPC request and returns the `result` field.
    pub async fn rpc_call<T: serde::de::DeserializeOwned>(
        http: &reqwest::Client,
        rpc_url: &str,
        method: &str,
        params: Vec<serde_json::Value>,
    ) -> Result<T> {
        let resp: serde_json::Value = http
            .post(rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": method, "params": params
            }))
            .send()
            .await?
            .json()
            .await?;
        if let Some(error) = resp.get("error") {
            bail!("rpc {method} error: {error}");
        }
        Ok(serde_json::from_value(resp["result"].clone())?)
    }

    pub async fn eth_chain_id(http: &reqwest::Client, rpc_url: &str) -> Result<u64> {
        let s: String = rpc_call(http, rpc_url, "eth_chainId", vec![]).await?;
        Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
    }

    pub async fn eth_block_number(http: &reqwest::Client, rpc_url: &str) -> Result<u64> {
        let s: String = rpc_call(http, rpc_url, "eth_blockNumber", vec![]).await?;
        Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
    }

    pub async fn eth_get_transaction_count(
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

    pub async fn eth_gas_price(http: &reqwest::Client, rpc_url: &str) -> Result<u128> {
        let s: String = rpc_call(http, rpc_url, "eth_gasPrice", vec![]).await?;
        Ok(u128::from_str_radix(s.trim_start_matches("0x"), 16)?)
    }

    /// eth_call with full calldata, returning the raw decoded return bytes.
    pub async fn eth_call_raw(
        http: &reqwest::Client,
        rpc_url: &str,
        to: &str,
        calldata: &[u8],
    ) -> Result<Vec<u8>> {
        let to_addr: Address = to.parse().context("eth_call to is not an address")?;
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

    /// eth_call returning a bool (e.g. `initialized()`).
    pub async fn eth_call_bool(
        http: &reqwest::Client,
        rpc_url: &str,
        to: &str,
        calldata: &[u8],
    ) -> Result<bool> {
        let bytes = eth_call_raw(http, rpc_url, to, calldata).await?;
        Ok(bytes.last().is_some_and(|b| *b != 0))
    }

    /// eth_call returning an address (e.g. `relayer()`).
    pub async fn eth_call_address(
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

    /// Runtime bytecode at `to` (empty when no contract is deployed).
    pub async fn eth_get_code(http: &reqwest::Client, rpc_url: &str, to: &str) -> Result<Vec<u8>> {
        let to_addr: Address = to.parse().context("eth_getCode to is not an address")?;
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

    /// `balanceOf(address)` via eth_call, checked to fit a u64.
    pub async fn eth_balance_of(
        http: &reqwest::Client,
        rpc_url: &str,
        token: &str,
        holder: &str,
    ) -> Result<u64> {
        let mut calldata = super::abi::selector(b"balanceOf(address)");
        let holder_addr: Address = holder
            .parse()
            .context("balanceOf holder is not an address")?;
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
    pub async fn eth_get_logs(
        http: &reqwest::Client,
        rpc_url: &str,
        address: &str,
        topic0: &str,
    ) -> Result<usize> {
        let to: Address = address
            .parse()
            .context("eth_getLogs address is not an address")?;
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
}

#[cfg(test)]
mod tests {
    use super::abi;

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
    fn word_len_reads_right_aligned_value() {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(abi::word_len(&w), 0x0102030405060708);
        assert_eq!(abi::word_len(&[0u8; 32]), 0);
    }

    #[test]
    fn decode_abi_string_reads_coin_type_return() {
        let bytes = encode_string("0x2::sui::SUI", false);
        assert_eq!(
            abi::decode_abi_string_at(&bytes, 32).unwrap(),
            "0x2::sui::SUI"
        );
    }

    #[test]
    fn decode_abi_string_reads_revert_payload() {
        let bytes = encode_string("bad coin type", true);
        assert_eq!(
            abi::decode_abi_string_at(&bytes, 36).unwrap(),
            "bad coin type"
        );
    }

    #[test]
    fn decode_abi_string_does_not_panic_on_full_length_word() {
        // Regression: a fully-populated 32-byte length word must error, not panic.
        let mut bytes = vec![0u8; 32];
        bytes.extend_from_slice(&[0xff; 32]);
        bytes.extend_from_slice(&[0u8; 32]);
        assert!(abi::decode_abi_string_at(&bytes, 32).is_err());
    }

    #[test]
    fn decode_abi_string_rejects_short_buffer() {
        assert!(abi::decode_abi_string_at(&[0u8; 10], 32).is_err());
        assert!(abi::decode_abi_string_at(&[0u8; 10], 36).is_err());
    }

    #[test]
    fn decode_revert_string_roundtrips_known_payload() {
        let bytes = encode_string("invalid opcode", true);
        let hex = format!("0x{}", hex::encode(&bytes));
        assert_eq!(abi::decode_revert_string(&hex), "invalid opcode");
    }

    #[test]
    fn encode_mint_matches_contract_signature() {
        use alloy_primitives::{Address, U256};
        let calldata = abi::encode_mint(Address::with_last_byte(0x42), 0xab, 0x05);
        assert_eq!(calldata.len(), 4 + 96);
        let mut expected = abi::selector(b"mint(address,uint256,uint64)");
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(Address::with_last_byte(0x42).as_ref());
        expected.extend_from_slice(&word);
        expected.extend_from_slice(&abi::abi_word(U256::from(0xab)));
        expected.extend_from_slice(&abi::abi_word(U256::from(0x05)));
        assert_eq!(calldata, expected);
    }
}
