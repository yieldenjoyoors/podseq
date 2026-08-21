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
      - ./podseq.toml:/etc/podseq/podseq.toml,z
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
fn sui_signer_key() -> Result<Option<String>> {
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
