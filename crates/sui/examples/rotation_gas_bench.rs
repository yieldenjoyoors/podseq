//! Gas benchmark for the cap-rotation entrypoints against a live Sui network.
//!
//! Deploys the current package (or resumes a half-run bench state), then
//! measures the exact PTBs the rotation flow submits — `request_rotation_ops`
//! (steered by the `OpsCap` minted at entombment), `claim_rotation`, and
//! `retire_previous`, each covering the Registry and the Vault in one
//! transaction — against the `settle_as` baseline. Gas is read from the
//! executed effects and appended to `<state>.gas.csv`.
//!
//! Two signers are needed: the funded deployer key and the staged key,
//! which must claim as its own transaction sender. The staged key is funded
//! with a small split from the deployer.
//!
//! Usage (from the repo root; the timelock is `MIN_DELAY_MS` = 10 min, so
//! the run is split in two phases):
//!
//! ```text
//! cargo run -p podseq-sui --example rotation_gas_bench -- \
//!     <rpc_url> <deployer_key> <staged_key> <state_file> fresh
//! # wait out the timelock, then:
//! cargo run -p podseq-sui --example rotation_gas_bench -- \
//!     <rpc_url> <deployer_key> <staged_key> <state_file> claim
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use podseq_sui::bridge::BridgeClient;
use podseq_sui::deadline::SUI_CLOCK_ID;
use podseq_sui::rotation::MIN_DELAY_MS;
use podseq_sui::settlement::Settlement;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_rpc::proto::sui::rpc::v2::changed_object;
use sui_rpc::proto::sui::rpc::v2::owner;
use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionRequest;
use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionResponse;
use sui_rpc::Client as RpcClient;
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

/// The staged key is funded with this much gas for claim/settle/retire.
const STAGED_FUNDING_MIST: u64 = 150_000_000;
/// Per-transaction wait for checkpoint inclusion.
const TX_WAIT: Duration = Duration::from_secs(60);
/// The K2 settle gate ignores block timestamps when the inbox is drained,
/// so the bench settles with a zero timestamp.
const NO_BLOCK_TS: u64 = 0;

/// One measured transaction.
struct GasRow {
    label: &'static str,
    digest: String,
    computation: u64,
    storage: u64,
    rebate: u64,
    non_refundable: u64,
}

impl GasRow {
    /// Net gas: computation + storage − rebate. Negative when the
    /// transaction releases more storage deposit than it creates (the claim
    /// removes the pending records), i.e. the sender is refunded.
    fn net(&self) -> i128 {
        i128::from(self.computation) + i128::from(self.storage) - i128::from(self.rebate)
    }

    fn append_csv(&self, path: &Path) -> Result<()> {
        let line = format!(
            "{},{},{},{},{},{},{}\n",
            self.label,
            self.digest,
            self.computation,
            self.storage,
            self.rebate,
            self.non_refundable,
            self.net()
        );
        use std::io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .with_context(|| format!("appending gas row to {}", path.display()))?;
        println!(
            "{:>28}  net {:>12} MIST  (computation {}, storage {} - rebate {})  {}",
            self.label,
            self.net(),
            self.computation,
            self.storage,
            self.rebate,
            self.digest
        );
        Ok(())
    }
}

/// Builds, signs, executes, and measures one transaction, returning the full
/// response so callers can read created objects from the effects.
async fn execute_measured(
    label: &'static str,
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    build: impl FnOnce(&mut TransactionBuilder),
) -> Result<(GasRow, ExecuteTransactionResponse)> {
    let sender = key.public_key().derive_address();
    let mut builder = TransactionBuilder::new();
    builder.set_sender(sender);
    build(&mut builder);
    let mut rpc = RpcClient::new(rpc_url)
        .map_err(|e| anyhow::anyhow!("rpc client: {e}"))
        .context("building rpc client")?;
    let tx = builder.build(&mut rpc).await.context("building PTB")?;
    let digest = tx.digest().to_string();
    let signature = key.sign_transaction(&tx).context("signing PTB")?;
    let response = rpc
        .execute_transaction_and_wait_for_checkpoint(
            ExecuteTransactionRequest::new(tx.into()).with_signatures(vec![signature.into()]),
            TX_WAIT,
        )
        .await
        .context("executing PTB")?;
    let inner = response.into_inner();
    let effects = inner.transaction().effects();
    let summary = effects.gas_used();
    let row = GasRow {
        label,
        digest,
        computation: summary.computation_cost(),
        storage: summary.storage_cost(),
        rebate: summary.storage_rebate(),
        non_refundable: summary.non_refundable_storage_fee(),
    };
    Ok((row, inner))
}

/// Builds, signs, executes, and measures one transaction.
async fn measure(
    label: &'static str,
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    build: impl FnOnce(&mut TransactionBuilder),
) -> Result<GasRow> {
    execute_measured(label, rpc_url, key, build)
        .await
        .map(|(row, _)| row)
}

/// The object id the entombment minted to the signer: the only changed
/// object that did not exist before the transaction and is address-owned
/// afterwards (the entombed cap and the dynamic-field children are
/// object-owned; the gas coin predates the transaction).
fn minted_ops_cap(response: &ExecuteTransactionResponse) -> Result<Address> {
    let effects = response.transaction().effects();
    let created: Vec<&str> = effects
        .changed_objects()
        .iter()
        .filter(|c| {
            c.input_state() == changed_object::InputObjectState::DoesNotExist
                && c.output_state() == changed_object::OutputObjectState::ObjectWrite
                && c.output_owner().kind() == owner::OwnerKind::Address
        })
        .filter_map(|c| c.object_id.as_deref())
        .collect();
    if created.len() != 1 {
        bail!(
            "expected exactly one minted ops cap in the entombment effects, found {}",
            created.len()
        );
    }
    created[0].parse().context("parsing minted ops cap id")
}

/// The shared-object arguments every rotation PTB touches.
struct BenchTargets {
    package: Address,
    registry: Address,
    vault: Address,
    inbox: Address,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 && !(args.len() == 7 && args[5] == "handoff") {
        bail!("usage: rotation_gas_bench <rpc_url> <deployer_key> <staged_key> <state_file> <fresh|claim|post|handoff> [<next_key>]");
    }
    let rpc_url = args[1].clone();
    let deployer_path = PathBuf::from(&args[2]);
    let staged_path = PathBuf::from(&args[3]);
    let state_path = PathBuf::from(&args[4]);
    let gas_csv = state_path.with_extension("gas.csv");

    let deployer = load_key(&deployer_path)?;
    let staged = load_key(&staged_path)?;
    let staged_addr = staged.public_key().derive_address();

    match args[5].as_str() {
        "fresh" => {
            fresh(
                &rpc_url,
                &deployer_path,
                &deployer,
                staged_addr,
                &state_path,
                &gas_csv,
            )
            .await
        }
        "claim" => claim(&rpc_url, &staged, &state_path, &gas_csv).await,
        // Resume after a crash between the claim and the retire: the claim
        // already landed, so only the new-authority settle and the retire
        // remain.
        "post" => post(&rpc_url, &staged, &state_path, &gas_csv).await,
        // The current authority (args[3]) pre-authorizes and funds the next
        // key, for a second measured claim cycle with a fresh payer.
        "handoff" => {
            let next_path = PathBuf::from(&args[6]);
            let next = load_key(&next_path)?;
            let next_addr = next.public_key().derive_address();
            handoff(&rpc_url, &staged, next_addr, &state_path).await
        }
        other => bail!("unknown phase {other} (expected fresh, claim, post, or handoff)"),
    }
}

fn load_key(path: &Path) -> Result<Ed25519PrivateKey> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("reading key {}", path.display()))?;
    podseq_sui::parse_signer_key(raw.trim()).map_err(|e| anyhow::anyhow!("parsing key: {e}"))
}

/// Deploys the package, entombs both caps, settles the baseline block, funds
/// the staged key, and pre-authorizes it. Ends the phase at the timelock.
#[allow(clippy::too_many_arguments)]
async fn fresh(
    rpc_url: &str,
    deployer_path: &Path,
    deployer: &Ed25519PrivateKey,
    staged_addr: Address,
    state_path: &Path,
    gas_csv: &Path,
) -> Result<()> {
    let modules_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../move/build/podseq_settlement/bytecode_modules");
    let mut modules: Vec<Vec<u8>> = Vec::new();
    let mut paths: Vec<PathBuf> = fs::read_dir(&modules_dir)
        .with_context(|| format!("reading {}", modules_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "mv"))
        .collect();
    paths.sort();
    for p in paths {
        modules.push(fs::read(&p).with_context(|| format!("reading {}", p.display()))?);
    }
    println!("publishing {} modules", modules.len());
    let contract = Settlement::deploy(deployer_path, rpc_url, modules).await?;
    let package: Address = contract
        .package_id
        .parse()
        .context("parsing deployed package id")?;
    let registry: Address = contract.registry_id.parse().context("parsing registry")?;
    let inbox: Address = contract.inbox_id.parse().context("parsing inbox")?;

    println!("initializing the bridge vault");
    let bridge =
        BridgeClient::initialize_with_key(deployer.clone(), rpc_url, &contract.package_id).await?;
    let vault: Address = bridge.vault_id.parse().context("parsing vault")?;
    let bridge_cap: Address = bridge.cap_id.parse().context("parsing bridge cap")?;
    let settler_cap: Address = contract
        .settler_cap_id
        .parse()
        .context("parsing settler cap")?;

    let targets = BenchTargets {
        package,
        registry,
        vault,
        inbox,
    };

    // Entombment: the caps move into the shared objects' dynamic fields and
    // each object's OpsCap is minted to the deployer — the ids land in the
    // state file for the later request phase.
    let (row, response) = execute_measured("entomb_settler", rpc_url, deployer, |b| {
        let cap = b.object(ObjectInput::new(settler_cap));
        let registry = b.object(ObjectInput::new(targets.registry));
        b.move_call(
            module_fn(targets.package, "settlement", "entomb_settler_cap_ops"),
            vec![cap, registry],
        );
    })
    .await?;
    row.append_csv(gas_csv)?;
    let settler_ops_cap: Address = minted_ops_cap(&response)?;
    let (row, response) = execute_measured("entomb_bridge", rpc_url, deployer, |b| {
        let cap = b.object(ObjectInput::new(bridge_cap));
        let vault = b.object(ObjectInput::new(targets.vault));
        b.move_call(
            module_fn(targets.package, "bridge", "entomb_bridge_cap_ops"),
            vec![cap, vault],
        );
    })
    .await?;
    row.append_csv(gas_csv)?;
    let bridge_ops_cap: Address = minted_ops_cap(&response)?;

    // The settle baseline: one anchor through the settle gate.
    measure("settle_baseline", rpc_url, deployer, |b| {
        let registry = b.object(ObjectInput::new(targets.registry));
        let inbox = b.object(ObjectInput::new(targets.inbox));
        let clock = b.object(ObjectInput::new(SUI_CLOCK_ID));
        let blob = b.pure(&vec![1_u8, 2, 3, 4]);
        let height = b.pure(&1_u64);
        let block_ts = b.pure(&NO_BLOCK_TS);
        let cursor = b.pure(&0_u64);
        b.move_call(
            module_fn(targets.package, "settlement", "settle_as"),
            vec![registry, inbox, clock, blob, height, block_ts, cursor],
        );
    })
    .await?
    .append_csv(gas_csv)?;

    // Fund the staged key for claim/settle/retire.
    let funding = measure("fund_staged", rpc_url, deployer, |b| {
        let gas = b.gas();
        let amount = b.pure(&STAGED_FUNDING_MIST);
        let splits = b.split_coins(gas, vec![amount]);
        let to = b.pure(&staged_addr);
        b.transfer_objects(splits, to);
    })
    .await?;
    println!(
        "{:>28}  funded staged key {} with {STAGED_FUNDING_MIST} MIST  {}",
        "fund_staged", staged_addr, funding.digest
    );

    // Pre-authorize the staged key on both objects (steered by the minted
    // ops caps; the operator's PTB).
    measure("request_both", rpc_url, deployer, |b| {
        request_ptb(b, &targets, settler_ops_cap, bridge_ops_cap, staged_addr);
    })
    .await?
    .append_csv(gas_csv)?;

    fs::write(
        state_path,
        format!(
            "package={package}\nregistry={registry}\nvault={vault}\ninbox={inbox}\n\
             settler_ops_cap={settler_ops_cap}\nbridge_ops_cap={bridge_ops_cap}\n"
        ),
    )
    .with_context(|| format!("writing {}", state_path.display()))?;
    println!(
        "state written to {}; wait out the timelock ({} ms), then run the claim phase",
        state_path.display(),
        MIN_DELAY_MS
    );
    Ok(())
}

/// Claims as the staged key, settles a block on the new authority, retires
/// the previous authority, and prints the rotation gas table.
async fn claim(
    rpc_url: &str,
    staged: &Ed25519PrivateKey,
    state_path: &Path,
    gas_csv: &Path,
) -> Result<()> {
    let state = fs::read_to_string(state_path).context("reading bench state")?;
    let mut ids = std::collections::BTreeMap::new();
    for line in state.lines() {
        if let Some((k, v)) = line.split_once('=') {
            ids.insert(k.to_string(), v.to_string());
        }
    }
    let parse = |k: &str| -> Result<Address> {
        ids.get(k)
            .context(format!("state missing {k}"))?
            .parse()
            .context(format!("parsing {k}"))
    };
    let targets = BenchTargets {
        package: parse("package")?,
        registry: parse("registry")?,
        vault: parse("vault")?,
        inbox: parse("inbox")?,
    };

    // The cut-over: one PTB flips both objects and starts the grace window.
    measure("claim_both", rpc_url, staged, |b| {
        let registry = b.object(ObjectInput::new(targets.registry));
        let clock = b.object(ObjectInput::new(SUI_CLOCK_ID));
        b.move_call(
            module_fn(targets.package, "settlement", "claim_rotation"),
            vec![registry, clock],
        );
        let vault = b.object(ObjectInput::new(targets.vault));
        let clock2 = b.object(ObjectInput::new(SUI_CLOCK_ID));
        b.move_call(
            module_fn(targets.package, "bridge", "claim_rotation"),
            vec![vault, clock2],
        );
    })
    .await?
    .append_csv(gas_csv)?;

    post(rpc_url, staged, state_path, gas_csv).await
}

/// The post-claim half: a settle on the new authority (proving the swap end
/// to end) and the one-way retire of the previous authority.
async fn post(
    rpc_url: &str,
    staged: &Ed25519PrivateKey,
    state_path: &Path,
    gas_csv: &Path,
) -> Result<()> {
    let state = fs::read_to_string(state_path).context("reading bench state")?;
    let mut ids = std::collections::BTreeMap::new();
    for line in state.lines() {
        if let Some((k, v)) = line.split_once('=') {
            ids.insert(k.to_string(), v.to_string());
        }
    }
    let parse = |k: &str| -> Result<Address> {
        ids.get(k)
            .context(format!("state missing {k}"))?
            .parse()
            .context(format!("parsing {k}"))
    };
    let targets = BenchTargets {
        package: parse("package")?,
        registry: parse("registry")?,
        vault: parse("vault")?,
        inbox: parse("inbox")?,
    };

    // A settle on the new authority proves the swap end to end.
    measure("settle_new_authority", rpc_url, staged, |b| {
        let registry = b.object(ObjectInput::new(targets.registry));
        let inbox = b.object(ObjectInput::new(targets.inbox));
        let clock = b.object(ObjectInput::new(SUI_CLOCK_ID));
        let blob = b.pure(&vec![5_u8, 6, 7, 8]);
        let height = b.pure(&2_u64);
        let block_ts = b.pure(&NO_BLOCK_TS);
        let cursor = b.pure(&0_u64);
        b.move_call(
            module_fn(targets.package, "settlement", "settle_as"),
            vec![registry, inbox, clock, blob, height, block_ts, cursor],
        );
    })
    .await?
    .append_csv(gas_csv)?;

    // One-way retire of the previous authority (the watcher's PTB).
    measure("retire_both", rpc_url, staged, |b| {
        let registry = b.object(ObjectInput::new(targets.registry));
        b.move_call(
            module_fn(targets.package, "settlement", "retire_previous"),
            vec![registry],
        );
        let vault = b.object(ObjectInput::new(targets.vault));
        b.move_call(
            module_fn(targets.package, "bridge", "retire_previous"),
            vec![vault],
        );
    })
    .await?
    .append_csv(gas_csv)?;

    println!("rows appended to {}", gas_csv.display());
    Ok(())
}

/// Mirrors `podseq_sui::rotation::submit_request`'s PTB (both objects),
/// steering through the `OpsCap` minted at entombment.
fn request_ptb(
    b: &mut TransactionBuilder,
    targets: &BenchTargets,
    settler_ops_cap: Address,
    bridge_ops_cap: Address,
    to: Address,
) {
    let registry = b.object(ObjectInput::new(targets.registry));
    let clock = b.object(ObjectInput::new(SUI_CLOCK_ID));
    let to_arg = b.pure(&to);
    let delay = b.pure(&MIN_DELAY_MS);
    let ops_cap = b.object(ObjectInput::new(settler_ops_cap));
    b.move_call(
        module_fn(targets.package, "settlement", "request_rotation_ops"),
        vec![registry, clock, to_arg, delay, ops_cap],
    );
    let vault = b.object(ObjectInput::new(targets.vault));
    let clock2 = b.object(ObjectInput::new(SUI_CLOCK_ID));
    let to_arg2 = b.pure(&to);
    let delay2 = b.pure(&MIN_DELAY_MS);
    let ops_cap2 = b.object(ObjectInput::new(bridge_ops_cap));
    b.move_call(
        module_fn(targets.package, "bridge", "request_rotation_ops"),
        vec![vault, clock2, to_arg2, delay2, ops_cap2],
    );
}

/// Builds a `<package>::<module>::<name>` move-call target.
fn module_fn(package: Address, module: &str, name: &str) -> Function {
    Function::new(
        package,
        Identifier::new(module).expect("static module name"),
        Identifier::new(name).expect("static function name"),
    )
}

/// The current authority pre-authorizes and funds the next key, so a second
/// rotation cycle measures the claim with a fresh payer. One transaction
/// does both: split the funding from gas, transfer it, and issue the
/// request on both objects.
async fn handoff(
    rpc_url: &str,
    authority: &Ed25519PrivateKey,
    next_addr: Address,
    state_path: &Path,
) -> Result<()> {
    let state = fs::read_to_string(state_path).context("reading bench state")?;
    let mut ids = std::collections::BTreeMap::new();
    for line in state.lines() {
        if let Some((k, v)) = line.split_once('=') {
            ids.insert(k.to_string(), v.to_string());
        }
    }
    let parse = |k: &str| -> Result<Address> {
        ids.get(k)
            .context(format!("state missing {k}"))?
            .parse()
            .context(format!("parsing {k}"))
    };
    let targets = BenchTargets {
        package: parse("package")?,
        registry: parse("registry")?,
        vault: parse("vault")?,
        inbox: parse("inbox")?,
    };

    measure("fund_next", rpc_url, authority, |b| {
        let gas = b.gas();
        let amount = b.pure(&STAGED_FUNDING_MIST);
        let splits = b.split_coins(gas, vec![amount]);
        let to = b.pure(&next_addr);
        b.transfer_objects(splits, to);
    })
    .await?;
    println!("funded next key {next_addr}");

    let settler_ops_cap = parse("settler_ops_cap")?;
    let bridge_ops_cap = parse("bridge_ops_cap")?;
    measure("request_both", rpc_url, authority, |b| {
        request_ptb(b, &targets, settler_ops_cap, bridge_ops_cap, next_addr);
    })
    .await?
    .append_csv(&state_path.with_extension("gas.csv"))?;
    println!(
        "requested; wait out the timelock ({} ms), then run the claim phase",
        MIN_DELAY_MS
    );
    Ok(())
}
