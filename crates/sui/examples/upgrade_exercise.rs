//! Live package-upgrade exercise for the entombed-`UpgradeCap` custody
//! ladder.
//!
//! Deploys the current package (v1) through the full deploy ceremony — the
//! `UpgradeCap` is entombed in the Registry at publish — restricts the
//! entombed cap to additive upgrades, proves the gate aborts a loosening
//! policy and a wrong digest on-chain, then lands a real additive upgrade
//! through `podseq_sui::settlement::upgrade_package` and verifies the
//! original package id routes calls to the new version.
//!
//! The upgrade delta is any additive change built OUTSIDE this run: add a
//! function to a module under `move/sources` (the recorded 2026-11-17 run
//! added `settlement::upgrade_probe()` returning 7), dump the new build,
//!
//! ```text
//! sui move build --dump-bytecode-as-base64 > /tmp/v2_build.json
//! ```
//!
//! and pass the JSON to the `upgrade` phase. `verify` calls the added
//! function by name — the probe is throwaway, the machinery under test is
//! the ceremony. Nothing in this file or the repo changes for it.
//!
//! Phases, in order:
//!
//! - `deploy` — publish v1 + the full entombment ceremony; writes the state
//!   file.
//! - `restrict <build_json>` — `restrict_package_upgrades(ADDITIVE)` on the
//!   entombed cap, then a looser-policy restrict must abort on-chain.
//! - `negatives <build_json>` — expected-abort upgrade probes: a policy
//!   looser than the cap's (`ETooPermissive`) and a right policy with a
//!   wrong digest (the Upgrade command rejects the mismatch). Both are
//!   built without simulation; a success is a real finding and fails the
//!   run.
//! - `upgrade <build_json>` — the real additive upgrade, measured.
//! - `verify <module> <fn>` — call the added function on the NEW package id
//!   (calls do not route from the original id to the latest version, pinned
//!   live 2026-11-17: post-upgrade, callers — and a node's `sui.package_id`
//!   config — must repoint at the latest id), call a function that exists
//!   nowhere (must abort = failure detection is real), settle one height
//!   through the settlement path post-upgrade.
//!
//! Usage (from the repo root):
//!
//! ```text
//! cargo run -p podseq-sui --example upgrade_exercise -- <rpc_url> <key> <state> deploy
//! cargo run -p podseq-sui --example upgrade_exercise -- <rpc_url> <key> <state> restrict <v1_json>
//! cargo run -p podseq-sui --example upgrade_exercise -- <rpc_url> <key> <state> negatives <v1_json>
//! cargo run -p podseq-sui --example upgrade_exercise -- <rpc_url> <key> <state> upgrade <v2_json>
//! cargo run -p podseq-sui --example upgrade_exercise -- <rpc_url> <key> <state> verify <module> <fn>
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use podseq_sui::settlement::{
    parse_move_build, restrict_package_upgrades, upgrade_package, Settlement,
    UPGRADE_POLICY_ADDITIVE, UPGRADE_POLICY_COMPATIBLE,
};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_rpc::field::{FieldMask, FieldMaskUtil};
use sui_rpc::proto::sui::rpc::v2::{ExecuteTransactionRequest, GetTransactionRequest};
use sui_rpc::Client as RpcClient;
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

/// Per-transaction wait for checkpoint inclusion.
const TX_WAIT: Duration = Duration::from_secs(60);
/// Gas budget for the no-simulation expected-abort probes (the stand-in
/// budget the builder requires before the accumulator form strips it).
const ABORT_BUDGET_MIST: u64 = 50_000_000;
/// The K2 settle gate ignores block timestamps when the inbox is drained.
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
    /// Net gas: computation + storage − rebate.
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

/// The ids the deploy ceremony produces; carried between phases.
#[derive(serde::Deserialize, serde::Serialize)]
struct ExerciseState {
    package_id: String,
    registry_id: String,
    inbox_id: String,
    settler_cap_id: String,
    ops_cap_id: String,
    upgrade_cap_id: String,
    /// Set by the `upgrade` phase: the new (latest) package id.
    #[serde(default)]
    upgraded_package_id: Option<String>,
}

fn load_state(path: &Path) -> Result<ExerciseState> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn save_state(path: &Path, state: &ExerciseState) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// The v1/v2 build dump: `sui move build --dump-bytecode-as-base64` output.
fn load_build(path: &str) -> Result<podseq_sui::settlement::MoveBuild> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    parse_move_build(&raw).with_context(|| format!("parsing {path}"))
}

/// Computation/storage/rebate/non-refundable for an already-executed
/// transaction digest (the ceremony helpers submit and return only the
/// digest).
async fn gas_of_digest(rpc_url: &str, digest: &str) -> Result<(u64, u64, u64, u64)> {
    let mut rpc = RpcClient::new(rpc_url).context("building rpc client")?;
    let mut request = GetTransactionRequest::default();
    request.digest = Some(digest.to_string());
    request.read_mask = Some(FieldMask::from_paths(["effects"]));
    let response = rpc
        .ledger_client()
        .get_transaction(request)
        .await
        .context("fetching transaction by digest")?
        .into_inner();
    let executed = response
        .transaction
        .context("transaction not found by digest")?;
    let effects = executed.effects.context("no effects in response")?;
    let gas = effects.gas_used.context("no gas summary in effects")?;
    Ok((
        gas.computation_cost.unwrap_or(0),
        gas.storage_cost.unwrap_or(0),
        gas.storage_rebate.unwrap_or(0),
        gas.non_refundable_storage_fee.unwrap_or(0),
    ))
}

/// Reports a ceremony transaction's gas row from its digest.
async fn ceremony_row(
    label: &'static str,
    rpc_url: &str,
    digest: &str,
    gas_csv: &Path,
) -> Result<()> {
    let (computation, storage, rebate, non_refundable) = gas_of_digest(rpc_url, digest).await?;
    GasRow {
        label,
        digest: digest.to_string(),
        computation,
        storage,
        rebate,
        non_refundable,
    }
    .append_csv(gas_csv)
}

/// The current `(version, digest)` of an owned object, for pinning it in a
/// no-simulation PTB (nothing resolves inputs there).
async fn owned_ref(rpc_url: &str, id: Address) -> Result<(u64, sui_sdk_types::Digest)> {
    use sui_rpc::field::FieldMaskUtil;
    let mut rpc = RpcClient::new(rpc_url).context("building rpc client")?;
    let mut request = sui_rpc::proto::sui::rpc::v2::GetObjectRequest::default();
    request.object_id = Some(id.to_string());
    request.read_mask = Some(FieldMask::from_paths(["version", "digest"]));
    let response = rpc
        .ledger_client()
        .get_object(request)
        .await
        .context("fetching object")?
        .into_inner();
    let object = response.object.context("object not found")?;
    let version = object.version.context("no version in object")?;
    let digest = {
        use std::str::FromStr;
        sui_sdk_types::Digest::from_str(object.digest())
            .map_err(|e| anyhow::anyhow!("parsing object digest: {e}"))?
    };
    Ok((version, digest))
}

/// Builds, signs, and executes one PTB expected to abort on-chain; a
/// success here is a real finding (the gate opened when it must not), so it
/// fails the run. Built WITHOUT simulation: the builder's simulation stage
/// rejects an intentionally-aborting PTB before it can be signed. The
/// network charges this account through its gas accumulator, so the
/// stand-in gas payment `try_build` requires is stripped to the accumulator
/// form (no payment objects) before signing.
async fn execute_expect_abort(
    label: &'static str,
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    gas_csv: &Path,
    build: impl FnOnce(&mut TransactionBuilder),
) -> Result<()> {
    let sender = key.public_key().derive_address();
    let mut builder = TransactionBuilder::new();
    builder.set_sender(sender);
    builder.add_gas_objects([ObjectInput::owned(
        Address::ZERO,
        0,
        sui_sdk_types::Digest::new([0u8; 32]),
    )]);
    builder.set_gas_budget(ABORT_BUDGET_MIST);
    builder.set_gas_price(1000);
    build(&mut builder);
    let mut tx = builder.try_build().context("building PTB")?;
    tx.gas_payment.objects.clear();
    tx.gas_payment.owner = sender;
    let mut rpc = RpcClient::new(rpc_url).context("building rpc client")?;
    // Accumulator gas requires shared-input-only transactions to carry a
    // ValidDuring expiration of at most two epochs.
    let current_epoch = {
        use sui_rpc::field::FieldMaskUtil;
        let request = sui_rpc::proto::sui::rpc::v2::GetEpochRequest::latest()
            .with_read_mask(sui_rpc::field::FieldMask::from_paths(["epoch"]));
        rpc.ledger_client()
            .get_epoch(request)
            .await
            .context("fetching current epoch")?
            .into_inner()
            .epoch()
            .epoch()
    };
    tx.expiration = sui_sdk_types::TransactionExpiration::ValidDuring {
        min_epoch: Some(current_epoch),
        max_epoch: Some(current_epoch + 1),
        min_timestamp: None,
        max_timestamp: None,
        // Testnet's chain identifier (GraphQL `chainIdentifier`) — this
        // exercise targets testnet explicitly.
        chain: {
            use std::str::FromStr;
            sui_sdk_types::Digest::from_str("69WiPg3DAQiwdxfncX6wYQ2siKwAe6L9BZthQea3JNMD")
                .context("parsing testnet chain identifier")?
        },
        nonce: 0,
    };
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
    row.append_csv(gas_csv)?;
    let status = effects.status();
    if status.success() {
        bail!("{label}: the probe SUCCEEDED — the gate it must close is open");
    }
    println!("{:>28}  aborted as expected: {}", label, status.error());
    Ok(())
}

/// Builds, signs, and executes one PTB that must succeed; measures it.
async fn execute_measured(
    label: &'static str,
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    gas_csv: &Path,
    build: impl FnOnce(&mut TransactionBuilder),
) -> Result<GasRow> {
    let sender = key.public_key().derive_address();
    let mut builder = TransactionBuilder::new();
    builder.set_sender(sender);
    build(&mut builder);
    let mut rpc = RpcClient::new(rpc_url).context("building rpc client")?;
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
    row.append_csv(gas_csv)?;
    let status = effects.status();
    if !status.success() {
        bail!("{label}: failed: {}", status.error());
    }
    Ok(row)
}

/// The full upgrade-probe PTB: authorize → Upgrade → commit, the exact
/// block `upgrade_package` submits.
#[allow(clippy::too_many_arguments)]
fn upgrade_probe_ptb(
    b: &mut TransactionBuilder,
    package: Address,
    registry: Address,
    ops_cap: Address,
    ops_version: u64,
    ops_digest: sui_sdk_types::Digest,
    policy: u8,
    digest: Vec<u8>,
    modules: Vec<Vec<u8>>,
    dependencies: Vec<Address>,
) {
    // The Registry is shared and the OpsCap is owned: both pinned explicitly
    // rather than let the builder classify them from the node's simulation
    // response (see phase_restrict).
    let registry_arg = b.object(ObjectInput::shared(registry, 1, true));
    let ops_arg = b.object(ObjectInput::owned(ops_cap, ops_version, ops_digest));
    let policy_arg = b.pure(&policy);
    let digest_arg = b.pure(&digest);
    let ticket = b.move_call(
        fn_of(package, "settlement", "authorize_package_upgrade"),
        vec![registry_arg, ops_arg, policy_arg, digest_arg],
    );
    let receipt = b.upgrade(modules, dependencies, package, ticket);
    b.move_call(
        fn_of(package, "settlement", "commit_package_upgrade"),
        vec![registry_arg, ops_arg, receipt],
    );
}

fn fn_of(package: Address, module: &str, function: &str) -> Function {
    Function::new(
        package,
        Identifier::new(module).expect("static module name"),
        Identifier::new(function).expect("static function name"),
    )
}

fn load_key(path: &Path) -> Result<Ed25519PrivateKey> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("reading key {}", path.display()))?;
    podseq_sui::parse_signer_key(raw.trim()).map_err(|e| anyhow::anyhow!("parsing key: {e}"))
}

fn modules_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../move/build/podseq_settlement/bytecode_modules")
}

async fn phase_deploy(
    rpc_url: &str,
    key_path: &Path,
    state_path: &Path,
    _gas_csv: &Path,
) -> Result<()> {
    let mut modules: Vec<Vec<u8>> = Vec::new();
    let mut paths: Vec<PathBuf> = fs::read_dir(modules_dir())
        .with_context(|| format!("reading {}", modules_dir().display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "mv"))
        .collect();
    paths.sort();
    for p in paths {
        modules.push(fs::read(&p).with_context(|| format!("reading {}", p.display()))?);
    }
    println!("publishing {} modules (v1)", modules.len());
    let contract = Settlement::deploy(key_path, rpc_url, modules).await?;
    let state = ExerciseState {
        package_id: contract.package_id.clone(),
        registry_id: contract.registry_id.clone(),
        inbox_id: contract.inbox_id.clone(),
        settler_cap_id: contract.settler_cap_id.clone(),
        ops_cap_id: contract.ops_cap_id.clone(),
        upgrade_cap_id: contract.upgrade_cap_id.clone(),
        upgraded_package_id: None,
    };
    save_state(state_path, &state)?;
    println!(
        "deployed package {} — registry {}, inbox {}, ops cap {}, upgrade cap {}",
        state.package_id, state.registry_id, state.inbox_id, state.ops_cap_id, state.upgrade_cap_id
    );
    Ok(())
}

async fn phase_restrict(
    rpc_url: &str,
    key_path: &Path,
    state_path: &Path,
    gas_csv: &Path,
) -> Result<()> {
    let state = load_state(state_path)?;
    let key = load_key(key_path)?;
    let digest = restrict_package_upgrades(
        key_path,
        rpc_url,
        &state.package_id,
        &state.registry_id,
        &state.ops_cap_id,
        UPGRADE_POLICY_ADDITIVE,
    )
    .await?;
    println!("restricted the entombed cap to additive upgrades");
    ceremony_row("restrict_additive", rpc_url, &digest, gas_csv).await?;

    // A looser restrict must abort (`E_TOO_PERMISSIVE`): the ladder only
    // tightens. Shared inputs are pinned explicitly: the builder classifies
    // unpinned inputs from the node's simulation response, and the load
    // balancer's nodes disagree on that response's shape.
    let package: Address = state.package_id.parse().context("parsing package id")?;
    let registry: Address = state.registry_id.parse().context("parsing registry id")?;
    let ops_cap: Address = state.ops_cap_id.parse().context("parsing ops cap id")?;
    let (ops_version, ops_digest) = owned_ref(rpc_url, ops_cap).await?;
    execute_expect_abort("restrict_looser_policy", rpc_url, &key, gas_csv, |b| {
        let registry_arg = b.object(ObjectInput::shared(registry, 1, true));
        let ops_arg = b.object(ObjectInput::owned(ops_cap, ops_version, ops_digest));
        let policy_arg = b.pure(&UPGRADE_POLICY_COMPATIBLE);
        b.move_call(
            fn_of(package, "settlement", "restrict_package_upgrades"),
            vec![registry_arg, ops_arg, policy_arg],
        );
    })
    .await
    .context("looser-restrict probe")?;
    Ok(())
}

async fn phase_negatives(
    rpc_url: &str,
    key_path: &Path,
    state_path: &Path,
    gas_csv: &Path,
    build_json: &str,
) -> Result<()> {
    let state = load_state(state_path)?;
    let key = load_key(key_path)?;
    let build = load_build(build_json)?;
    let package: Address = state.package_id.parse().context("parsing package id")?;
    let registry: Address = state.registry_id.parse().context("parsing registry id")?;
    let ops_cap: Address = state.ops_cap_id.parse().context("parsing ops cap id")?;
    let (ops_version, ops_digest) = owned_ref(rpc_url, ops_cap).await?;

    // The digest the v1 build carries; probe B flips one byte so the
    // Upgrade command's digest check must reject it.
    let mut wrong_digest = build.digest.clone();
    wrong_digest[0] ^= 0xff;

    execute_expect_abort("upgrade_looser_policy", rpc_url, &key, gas_csv, |b| {
        upgrade_probe_ptb(
            b,
            package,
            registry,
            ops_cap,
            ops_version,
            ops_digest,
            UPGRADE_POLICY_COMPATIBLE,
            build.digest.clone(),
            build.modules.clone(),
            build.dependencies.clone(),
        );
    })
    .await
    .context("looser-policy probe")?;

    // An aborted transaction still advances its input objects' versions, so
    // the pin the last probe used is dead: re-fetch before the next one.
    let (ops_version, ops_digest) = owned_ref(rpc_url, ops_cap).await?;
    execute_expect_abort("upgrade_wrong_digest", rpc_url, &key, gas_csv, |b| {
        upgrade_probe_ptb(
            b,
            package,
            registry,
            ops_cap,
            ops_version,
            ops_digest,
            UPGRADE_POLICY_ADDITIVE,
            wrong_digest,
            build.modules.clone(),
            build.dependencies.clone(),
        );
    })
    .await
    .context("wrong-digest probe")?;
    Ok(())
}

async fn phase_upgrade(
    rpc_url: &str,
    key_path: &Path,
    state_path: &Path,
    gas_csv: &Path,
    build_json: &str,
) -> Result<()> {
    let mut state = load_state(state_path)?;
    let build = load_build(build_json)?;
    let outcome = upgrade_package(
        key_path,
        rpc_url,
        &state.package_id,
        &state.registry_id,
        &state.ops_cap_id,
        build,
        UPGRADE_POLICY_ADDITIVE,
    )
    .await?;
    println!(
        "upgraded {} -> {} (tx {})",
        state.package_id, outcome.new_package_id, outcome.tx_digest
    );
    ceremony_row("upgrade_additive", rpc_url, &outcome.tx_digest, gas_csv).await?;
    state.upgraded_package_id = Some(outcome.new_package_id);
    save_state(state_path, &state)?;
    Ok(())
}

async fn phase_verify(
    rpc_url: &str,
    key_path: &Path,
    state_path: &Path,
    gas_csv: &Path,
    probe_module: &str,
    probe_fn: &str,
) -> Result<()> {
    let state = load_state(state_path)?;
    let key = load_key(key_path)?;
    let upgraded = state
        .upgraded_package_id
        .clone()
        .context("run the upgrade phase first")?;
    let upgraded_addr: Address = upgraded.parse().context("parsing upgraded package id")?;
    let registry: Address = state.registry_id.parse().context("parsing registry id")?;
    let inbox: Address = state.inbox_id.parse().context("parsing inbox id")?;

    // The probe function exists only in the upgraded version, and calls do
    // NOT route from the original package id to the latest (pinned live
    // 2026-11-17: `sui client call` on the original id cannot resolve the
    // probe; the new id works) — so post-upgrade, callers repoint at the
    // latest id, and a node's `sui.package_id` config must move with it.
    execute_measured("probe_upgraded_fn", rpc_url, &key, gas_csv, |b| {
        b.move_call(fn_of(upgraded_addr, probe_module, probe_fn), vec![]);
    })
    .await?;

    // Control: a function that exists nowhere must abort — the probe above
    // is only evidence if failures are detectable. Built without simulation
    // so the failure is the chain's, not the builder's dry run.
    execute_expect_abort("probe_missing_fn", rpc_url, &key, gas_csv, |b| {
        b.move_call(
            fn_of(upgraded_addr, probe_module, "upgrade_probe_missing"),
            vec![],
        );
    })
    .await?;

    // The settlement path still works post-upgrade: one anchor through
    // settle_as on the UPGRADED package id. Shared inputs pinned explicitly
    // (see phase_restrict).
    execute_measured("settle_post_upgrade", rpc_url, &key, gas_csv, |b| {
        let registry_arg = b.object(ObjectInput::shared(registry, 1, true));
        let inbox_arg = b.object(ObjectInput::shared(inbox, 1, true));
        let clock = b.object(ObjectInput::shared(
            podseq_sui::deadline::SUI_CLOCK_ID,
            1,
            true,
        ));
        let blob = b.pure(&vec![1_u8, 2, 3, 4]);
        let height = b.pure(&1_u64);
        let block_ts = b.pure(&NO_BLOCK_TS);
        let cursor = b.pure(&0_u64);
        b.move_call(
            fn_of(upgraded_addr, "settlement", "settle_as"),
            vec![
                registry_arg,
                inbox_arg,
                clock,
                blob,
                height,
                block_ts,
                cursor,
            ],
        );
    })
    .await?;

    println!(
        "verified: probe fn callable on the upgraded id {upgraded}; missing fn aborts; settlement works post-upgrade"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        bail!("usage: upgrade_exercise <rpc_url> <key> <state> <deploy|restrict|negatives|upgrade|verify> [<args>]");
    }
    let rpc_url = args[1].clone();
    let key_path = PathBuf::from(&args[2]);
    let state_path = PathBuf::from(&args[3]);
    let gas_csv = state_path.with_extension("gas.csv");
    match args[4].as_str() {
        "deploy" => phase_deploy(&rpc_url, &key_path, &state_path, &gas_csv).await?,
        "restrict" => phase_restrict(&rpc_url, &key_path, &state_path, &gas_csv).await?,
        "negatives" => {
            let build_json = args
                .get(5)
                .context("negatives needs the v1 build json path")?;
            phase_negatives(&rpc_url, &key_path, &state_path, &gas_csv, build_json).await?;
        }
        "upgrade" => {
            let build_json = args
                .get(5)
                .context("upgrade needs the v2 build json path")?;
            phase_upgrade(&rpc_url, &key_path, &state_path, &gas_csv, build_json).await?;
        }
        "verify" => {
            let module = args.get(5).context("verify needs the probe module name")?;
            let function = args.get(6).context("verify needs the probe fn name")?;
            phase_verify(&rpc_url, &key_path, &state_path, &gas_csv, module, function).await?;
        }
        other => bail!("unknown phase {other}"),
    }
    Ok(())
}
