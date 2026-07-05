//! podseq binary: an EVM L2 sequencer that posts to Walrus and settles on Sui.

#![forbid(unsafe_code)]

mod bridge;
mod config;
mod full_node;
mod keyring;
mod metrics;
mod runner;
mod settlement;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use podseq_core::runtime::{RunnerTrait as _, Supervisor as _};

use config::{Config, P2pConfig as ConfigP2p};

#[derive(Debug, Parser)]
#[command(name = "podseq", version, about = "EVM L2 sequencer on Walrus and Sui")]
struct Cli {
    #[arg(short, long, env = "PODSEQ_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init {
        #[command(subcommand)]
        action: InitCommands,
    },
    Keyring {
        #[command(subcommand)]
        action: KeyringCommands,
    },
    /// Start the node (sequencer or full node, depending on config `mode`).
    Start,
    /// Show chain height and settlement status.
    Status,
}

#[derive(Debug, Subcommand)]
enum InitCommands {
    /// Generate a default config file.
    Config {
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum KeyringCommands {
    /// Generate a new signer key (settlement + block signing).
    GenerateKey {
        #[arg(short, long, default_value = "sequencer.key")]
        out: PathBuf,
    },
    /// Generate a secp256k1 EVM key for the bridge relayer (L2 mint/burn).
    GenerateEvmKey {
        #[arg(short, long, default_value = "relayer.key")]
        out: PathBuf,
    },
    /// Show keys configured in the config file.
    List,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init { action } => match action {
            InitCommands::Config { out } => {
                let config = Config::testnet();
                let toml = toml::to_string_pretty(&config).context("serializing config")?;
                match out {
                    Some(path) => {
                        std::fs::write(&path, &toml)
                            .with_context(|| format!("writing {}", path.display()))?;
                        info!(path = %path.display(), "wrote config");
                    }
                    None => println!("{toml}"),
                }
            }
        },

        Commands::Keyring { action } => match action {
            KeyringCommands::GenerateKey { out } => {
                keyring::generate_signer(&out)?;
            }
            KeyringCommands::GenerateEvmKey { out } => {
                keyring::generate_evm_key(&out)?;
            }
            KeyringCommands::List => {
                let config = load_config(&cli.config)?;
                keyring::list(&config);
            }
        },

        Commands::Status => {
            let config = load_config(&cli.config)?;
            let auth = podseq_engine::Auth::from_file(&config.reth.jwt_path)
                .with_context(|| format!("loading JWT from {}", config.reth.jwt_path.display()))?;
            let engine = podseq_engine::Engine::new(&config.reth.engine_url, auth)?;
            let rt = tokio::runtime::Runtime::new()?;
            match rt.block_on(engine.block_number()) {
                Ok(height) => println!("Reth block height: {height}"),
                Err(e) => println!("Reth: unreachable ({e})"),
            }
            match &config.sui.settlement_package_id {
                Some(pkg) => println!("Settlement package: {pkg}"),
                None => println!("Settlement: not configured"),
            }
        }

        Commands::Start => {
            let config_path = cli
                .config
                .clone()
                .context("no config file provided (use --config or PODSEQ_CONFIG)")?;
            let config = Config::load(&config_path)?;

            // Runner::start() creates the tokio runtime and blocks the thread.
            let runtime_result =
                podseq_core::runtime::Runner::default().start(|context| async move {
                    match config.mode.as_str() {
                        "full" => start_full_node(config, context).await,
                        _ => start_sequencer(config, config_path, context).await,
                    }
                });

            runtime_result?;
        }
    }

    Ok(())
}

fn load_config(config_path: &Option<PathBuf>) -> Result<Config> {
    let path = config_path
        .as_ref()
        .context("no config file provided (use --config or PODSEQ_CONFIG)")?;
    Config::load(path)
}

/// Returns `None` when p2p is disabled (`no_p2p`).
async fn build_p2p(
    context: &podseq_core::runtime::Context,
    cfg: &ConfigP2p,
) -> Result<
    Option<(
        podseq_p2p::BlockBroadcaster,
        podseq_p2p::BlockReceiver,
        podseq_p2p::P2pNode,
    )>,
> {
    if cfg.no_p2p {
        warn!("p2p disabled in config (no_p2p = true)");
        return Ok(None);
    }

    let key_path = cfg
        .key_path
        .as_ref()
        .context("p2p key path is required (set p2p.key_path in config)")?;

    let p2p_config = podseq_p2p::P2pConfig {
        key_path: Some(key_path.clone()),
        listen_addr: cfg.listen_addr.parse()?,
        dialable_addr: cfg.dialable_addr.as_ref().map(|s| s.parse()).transpose()?,
        application_namespace: b"podseq-v1".to_vec(),
        bootstrap_peers: cfg
            .bootstrap_peers
            .iter()
            .filter_map(|s| {
                let (hex, addr) = s.split_once('@')?;
                Some((hex.to_string(), addr.parse().ok()?))
            })
            .collect(),
        ..podseq_p2p::P2pConfig::default()
    };

    let node = podseq_p2p::P2pNode::new(context.child("p2p"), &p2p_config).await?;
    let bc = node.broadcaster();
    let rx = node.receiver();
    Ok(Some((bc, rx, node)))
}

async fn start_sequencer(
    mut config: Config,
    config_path: PathBuf,
    context: podseq_core::runtime::Context,
) -> Result<()> {
    info!(mode = "sequencer", "starting podseq node");

    let (broadcaster, _receiver, _p2p_node) = build_p2p(&context, &config.p2p)
        .await?
        .map(|(bc, rx, n)| (Some(bc), Some(rx), Some(n)))
        .unwrap_or((None, None, None));

    // Metrics: construct and optionally start the HTTP endpoint.
    let podseq_metrics = Arc::new(metrics::PodseqMetrics::new());
    let metrics_shutdown = Arc::new(AtomicBool::new(false));
    let _metrics_handle = if config.metrics.enabled {
        let addr: std::net::SocketAddr = config
            .metrics
            .listen_addr
            .parse()
            .context("invalid metrics.listen_addr")?;
        let m = Arc::clone(&podseq_metrics);
        let s = Arc::clone(&metrics_shutdown);
        Some(tokio::spawn(metrics::serve(m, addr, s)))
    } else {
        None
    };

    let signer_key_path = config
        .signer
        .key_path
        .clone()
        .context("signer key path is required (set signer.key_path in config)")?;

    let auth = podseq_engine::Auth::from_file(&config.reth.jwt_path)
        .with_context(|| format!("loading JWT from {}", config.reth.jwt_path.display()))?;
    let engine = podseq_engine::Engine::new(&config.reth.engine_url, auth)
        .context("building Reth Engine API client")?;

    let mut sui_client = podseq_sui::Client::new(podseq_sui::Config {
        publisher_url: config.walrus.publisher_url.clone(),
        aggregator_url: config.walrus.aggregator_url.clone(),
        epochs: config.walrus.epochs,
        sui_rpc_url: config.sui.rpc_url.clone(),
        publisher_auth_token: config.walrus.publisher_auth_token.clone(),
    })
    .context("building Sui-layer client")?;

    // Settlement: preflight, then attach to existing IDs or deploy on first start.
    settlement::preflight(
        &config.sui,
        (
            &config.sui.settlement_package_id,
            &config.sui.settler_cap_id,
            &config.sui.registry_id,
        ),
        &signer_key_path,
    )
    .await
    .context("settlement preflight failed")?;
    let settlement = settlement::setup_signer(&mut config, &config_path, &signer_key_path).await?;
    sui_client = sui_client.with_settlement(settlement);

    let genesis_hash = config
        .sequencer
        .genesis_hash
        .as_ref()
        .map(|h| h.parse::<alloy_primitives::B256>())
        .transpose()
        .context("invalid genesis_hash")?;

    let block_signer = {
        let signer = podseq_sequencer::Ed25519BlockSigner::from_suiprivkey_file(&signer_key_path)
            .context("loading signer key for block signing")?;
        info!(key = %signer_key_path.display(), "signer key loaded");
        info!(address = %signer.address(), "sequencer address (Sui, ed25519)");
        info!(pubkey = %signer.pub_key(), "sequencer pubkey (hex) => set this as signer.sequencer_pubkey on full nodes");
        Arc::new(signer) as Arc<dyn podseq_core::BlockSigner>
    };

    let runner = runner::Runner::new(
        engine,
        sui_client,
        block_signer,
        &config.sequencer,
        genesis_hash,
        &config.data_dir,
        broadcaster,
        config.walrus.batch_size_bytes,
        Arc::clone(&podseq_metrics),
    );

    // Enshrined bridge relayer: runs concurrently with the production loop and is
    // deliberately NON-FATAL. The spawned task owns its own config copy and may
    // persist Sui object IDs to the config file independently of the runner.
    let bridge_handle = bridge::spawn(
        config.clone(),
        config_path.clone(),
        signer_key_path.clone(),
        Arc::clone(&podseq_metrics),
    );

    info!("starting sequencer loop (Ctrl+C to stop)");
    let result = runner.run().await;

    // Shut down the metrics endpoint.
    metrics_shutdown.store(true, Ordering::SeqCst);
    if let Some(handle) = _metrics_handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }

    if let Some((mut handle, shutdown)) = bridge_handle {
        shutdown.store(true, Ordering::SeqCst);
        // Best-effort drain; mirrors the finalizer's bounded shutdown.
        match tokio::time::timeout(std::time::Duration::from_secs(10), &mut handle).await {
            Ok(Ok(())) => info!("bridge relayer stopped"),
            Ok(Err(e)) => warn!(error = %e, "bridge relayer task panicked"),
            Err(_) => {
                warn!("bridge relayer did not stop in time; aborting");
                handle.abort();
            }
        }
    }

    result
}

async fn start_full_node(config: Config, context: podseq_core::runtime::Context) -> Result<()> {
    info!(mode = "full", "starting podseq node");

    // Metrics: construct and optionally start the HTTP endpoint.
    let podseq_metrics = Arc::new(metrics::PodseqMetrics::new());
    let metrics_shutdown = Arc::new(AtomicBool::new(false));
    let _metrics_handle = if config.metrics.enabled {
        let addr: std::net::SocketAddr = config
            .metrics
            .listen_addr
            .parse()
            .context("invalid metrics.listen_addr")?;
        let m = Arc::clone(&podseq_metrics);
        let s = Arc::clone(&metrics_shutdown);
        Some(tokio::spawn(metrics::serve(m, addr, s)))
    } else {
        None
    };

    let receiver = build_p2p(&context, &config.p2p)
        .await?
        .map(|(_bc, rx, _n)| rx);

    let auth = podseq_engine::Auth::from_file(&config.reth.jwt_path)
        .with_context(|| format!("loading JWT from {}", config.reth.jwt_path.display()))?;
    let engine = podseq_engine::Engine::new(&config.reth.engine_url, auth)
        .context("building Reth Engine API client")?;

    let sui_client = podseq_sui::Client::new(podseq_sui::Config {
        publisher_url: config.walrus.publisher_url.clone(),
        aggregator_url: config.walrus.aggregator_url.clone(),
        epochs: config.walrus.epochs,
        sui_rpc_url: config.sui.rpc_url.clone(),
        publisher_auth_token: config.walrus.publisher_auth_token.clone(),
    })
    .context("building Sui-layer client")?;

    let node = full_node::FullNode::new(engine, sui_client, &config, receiver)?;
    info!("starting full node sync (Ctrl+C to stop)");
    let result = node.run().await;

    // Shut down the metrics endpoint.
    metrics_shutdown.store(true, Ordering::SeqCst);
    if let Some(handle) = _metrics_handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }

    result
}
