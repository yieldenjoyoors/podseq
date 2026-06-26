//! podseq binary: an EVM L2 sequencer that posts to Walrus and settles on Sui.

#![forbid(unsafe_code)]

mod config;
mod full_node;
mod keyring;
mod runner;

use std::path::PathBuf;
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
    /// Generate a new p2p identity key.
    GenerateP2p {
        #[arg(short, long, default_value = "p2p.key")]
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
            KeyringCommands::GenerateP2p { out } => {
                podseq_p2p::load_or_generate_key(&out)?;
                info!(key = %out.display(), "p2p identity key generated");
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

/// Startup preflight: fail fast with a clear message if settlement cannot be
/// configured, instead of a cryptic error deep in deploy/commit later.
///
/// - Always checks Sui RPC reachability.
/// - Existing IDs: verifies the Registry object is readable.
/// - First-start deploy: verifies the Move package is built (bytecode present).
async fn preflight_settlement(
    sui: &config::SuiConfig,
    ids: (&Option<String>, &Option<String>, &Option<String>),
    signer_key_path: &std::path::Path,
) -> Result<()> {
    info!(rpc = %sui.rpc_url, "preflight: probing Sui RPC");
    if let Err(e) = podseq_sui::settlement::ping_rpc(&sui.rpc_url).await {
        anyhow::bail!(
            "Sui RPC unreachable at {}; is the node running and the URL correct? (error: {e})",
            sui.rpc_url
        );
    }

    match ids {
        (Some(_), Some(_), Some(registry_id)) => {
            // Validate the Registry object exists and is readable.
            if let Err(e) = podseq_sui::settlement::latest_height(&sui.rpc_url, registry_id).await {
                anyhow::bail!(
                    "cannot read settlement Registry {registry_id} on {}; verify sui.registry_id and that the contract is deployed (error: {e})",
                    sui.rpc_url
                );
            }
            Ok(())
        }
        (None, None, None) => {
            // First-start deploy: the Move package must be built first.
            let bytecode_dir = sui
                .move_dir
                .join("build/podseq_settlement/bytecode_modules");
            let built = bytecode_dir.is_dir()
                && std::fs::read_dir(&bytecode_dir)
                    .map(|mut it| {
                        it.any(|e| {
                            e.ok()
                                .is_some_and(|e| e.path().extension().is_some_and(|x| x == "mv"))
                        })
                    })
                    .unwrap_or(false);
            if !built {
                anyhow::bail!(
                    "settlement contract is not deployed and the Move package is not built: \
                     no .mv modules in {}. Run `sui move build` in {} first.",
                    bytecode_dir.display(),
                    sui.move_dir.display()
                );
            }
            if !signer_key_path.is_file() {
                anyhow::bail!(
                    "signer key not found at {}; settlement deploy needs a suiprivkey (fund its address with SUI for gas)",
                    signer_key_path.display()
                );
            }
            Ok(())
        }
        _ => anyhow::bail!(
            "settlement IDs are partially configured; either set all three (settlement_package_id, settler_cap_id, registry_id) or none"
        ),
    }
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

    let signer_key_path = config
        .signer
        .key_path
        .as_ref()
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

    // Settlement: use existing IDs or deploy on first start.
    let settlement_ids = (
        &config.sui.settlement_package_id,
        &config.sui.settler_cap_id,
        &config.sui.registry_id,
    );
    preflight_settlement(&config.sui, settlement_ids, signer_key_path)
        .await
        .context("settlement preflight failed")?;

    match settlement_ids {
        (Some(pkg), Some(cap), Some(reg)) => {
            let settlement = podseq_sui::SettlementSigner::new(
                signer_key_path,
                pkg,
                cap,
                reg,
                &config.sui.rpc_url,
            )
            .context("building settlement signer")?;
            sui_client = sui_client.with_settlement(settlement);
            info!(key = %signer_key_path.display(), "settlement signer attached");
        }
        (None, None, None) => {
            info!(key = %signer_key_path.display(), "deploying settlement contract on first start");
            let bytecode_dir = config
                .sui
                .move_dir
                .join("build/podseq_settlement/bytecode_modules");
            let mut modules = Vec::new();
            for entry in std::fs::read_dir(&bytecode_dir).with_context(|| {
                format!(
                    "reading {} (run `sui move build` in {} first)",
                    bytecode_dir.display(),
                    config.sui.move_dir.display()
                )
            })? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "mv") {
                    modules.push(std::fs::read(&path)?);
                }
            }
            let deployed =
                podseq_sui::SettlementSigner::deploy(signer_key_path, &config.sui.rpc_url, modules)
                    .await
                    .context("deploying settlement contract")?;

            config.sui.settlement_package_id = Some(deployed.package_id.clone());
            config.sui.settler_cap_id = Some(deployed.settler_cap_id.clone());
            config.sui.registry_id = Some(deployed.registry_id.clone());
            let updated = toml::to_string_pretty(&config).context("serializing updated config")?;
            std::fs::write(&config_path, &updated)
                .with_context(|| format!("writing updated config to {}", config_path.display()))?;
            info!(config = %config_path.display(), "config updated with settlement IDs");

            let settlement = podseq_sui::SettlementSigner::new(
                signer_key_path,
                &deployed.package_id,
                &deployed.settler_cap_id,
                &deployed.registry_id,
                &config.sui.rpc_url,
            )?;
            sui_client = sui_client.with_settlement(settlement);
        }
        _ => {
            anyhow::bail!("settlement IDs are partially configured; either set all three (settlement_package_id, settler_cap_id, registry_id) or none");
        }
    }

    let genesis_hash = config
        .sequencer
        .genesis_hash
        .as_ref()
        .map(|h| h.parse::<alloy_primitives::B256>())
        .transpose()
        .context("invalid genesis_hash")?;

    let block_signer = {
        let signer = podseq_sequencer::Ed25519BlockSigner::from_suiprivkey_file(signer_key_path)
            .context("loading signer key for block signing")?;
        info!(key = %signer_key_path.display(), "signer key loaded");
        let address = signer.address();
        let pub_key = signer.pub_key();
        info!("╔{}╗", "═".repeat(80));
        info!("║ SEQUENCER ADDRESS: {address}");
        info!("║ SEQUENCER PUBKEY:  {pub_key}");
        info!("╚{}╝", "═".repeat(80));
        Arc::new(signer) as Arc<dyn podseq_core::BlockSigner>
    };

    let mempool = podseq_engine::MempoolClient::new(&config.reth.rpc_url)
        .context("building mempool client")?;

    let runner = runner::Runner::new(
        engine,
        mempool,
        sui_client,
        block_signer,
        &config.sequencer,
        genesis_hash,
        &config.data_dir,
        broadcaster,
        config.walrus.batch_size_bytes,
    );
    info!("starting sequencer loop (Ctrl+C to stop)");
    runner.run().await
}

async fn start_full_node(config: Config, context: podseq_core::runtime::Context) -> Result<()> {
    info!(mode = "full", "starting podseq node");

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
    node.run().await
}
