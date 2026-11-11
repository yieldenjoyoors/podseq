//! Gas benchmark for the exit-queue escape path against a live Sui network.
//!
//! Deploys the current package (or resumes a half-run bench state), then
//! measures the exact PTBs the escape path submits — the settle+anchor
//! baseline, the vault deposit that funds redemption, the window narrowing,
//! the forced-transaction submit, the closed-gate rejection, `request_exit`
//! with a single-leaf and a deep account proof, `redeem` +
//! `escape_release`, and a replayed redemption. Gas is read from the executed
//! effects and appended to `<state>.gas.csv`.
//!
//! The verdict needs real clock time: after the forced submit, the inbox
//! deadline is `now + window` (narrowed to the 1-minute floor) and the
//! on-chain gate adds the 10-minute halt margin, so the halt phase starts
//! ~11 minutes after the fresh phase ends.
//!
//! Usage (from the repo root; run `sui move build` first so the bytecode
//! modules exist, and fund the deployer key from the faucet):
//!
//! ```text
//! cargo run -p podseq-sui --example exit_gas_bench -- \
//!     <rpc_url> <deployer_key> <state_file> deploy
//! cargo run -p podseq-sui --example exit_gas_bench -- \
//!     <rpc_url> <deployer_key> <state_file> prepare
//! # wait out the window + halt margin (~11 min), then:
//! cargo run -p podseq-sui --example exit_gas_bench -- \
//!     <rpc_url> <deployer_key> <state_file> halt
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy_primitives::{keccak256, Address as L2Address, U256};
use alloy_rlp::{Encodable, Header};
use alloy_trie::proof::{verify_proof, ProofRetainer};
use alloy_trie::{HashBuilder, Nibbles};
use anyhow::{bail, Context, Result};
use k256::ecdsa::SigningKey;
use podseq_sui::bridge::BridgeClient;
use podseq_sui::deadline::SUI_CLOCK_ID;
use podseq_sui::exit_queue::{
    append_redeem, append_request_exit, eth_address, sign_exit_payload, RedeemRequest,
    RedeemTargets,
};
use podseq_sui::settlement::Settlement;
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionRequest;
use sui_rpc::proto::sui::rpc::v2::ExecuteTransactionResponse;
use sui_rpc::Client as RpcClient;
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

/// The deposit that funds the vault reserve before redemption.
const DEPOSIT_MIST: u64 = 3_000_000_000;
/// The proven balance the shallow account redeems (fits the vault's u64).
const BALANCE_A: u64 = 1_000_000_000;
/// The proven balance of the deep-trie account (recorded, not redeemed).
const BALANCE_B: u64 = 2_000_000;
/// The narrowed inclusion window: the 1-minute floor (`inbox::min_window_ms`).
const NARROW_WINDOW_MS: u64 = 60_000;
/// Per-transaction wait for checkpoint inclusion.
const TX_WAIT: Duration = Duration::from_secs(60);
/// The K2 settle gate ignores block timestamps when the inbox is drained,
/// so the bench settles with a zero timestamp.
const NO_BLOCK_TS: u64 = 0;
/// The SUI coin type every measured payment path uses.
const SUI_TYPE: &str = "0x2::sui::SUI";
/// Gas budget for the intentionally-aborting measurements: generous headroom
/// over the 1M computation floor, spent only down to what the abort uses.
const ABORT_BUDGET_MIST: u64 = 100_000_000;

/// One measured transaction.
struct GasRow {
    label: &'static str,
    digest: String,
    computation: u64,
    storage: u64,
    rebate: u64,
}

impl GasRow {
    /// Net gas: computation + storage − rebate.
    fn net(&self) -> i128 {
        i128::from(self.computation) + i128::from(self.storage) - i128::from(self.rebate)
    }

    fn append_csv(&self, path: &Path) -> Result<()> {
        let line = format!(
            "{},{},{},{},{},{}\n",
            self.label,
            self.digest,
            self.computation,
            self.storage,
            self.rebate,
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
            "{:>24}  net {:>12} MIST  (computation {}, storage {} - rebate {})  {}",
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
/// response so callers can assert on the effects (an on-chain Move abort
/// arrives as a `failure` status in the effects, not as an RPC error).
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
    };
    Ok((row, inner))
}

/// Executes and measures one transaction expected to abort on-chain; a
/// success here is a real finding (the gate opened when it must not), so it
/// fails the run. Built WITHOUT simulation: the builder's simulation stage
/// rejects an intentionally-aborting PTB before it can be signed. The
/// network charges this account through its gas accumulator, so the
/// stand-in gas payment `try_build` requires is stripped to the accumulator
/// form (no payment objects) before signing.
async fn execute_measured_expect_abort(
    label: &'static str,
    rpc_url: &str,
    key: &Ed25519PrivateKey,
    build: impl FnOnce(&mut TransactionBuilder),
) -> Result<GasRow> {
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
    let mut rpc = RpcClient::new(rpc_url)
        .map_err(|e| anyhow::anyhow!("rpc client: {e}"))
        .context("building rpc client")?;
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
        // bench targets testnet explicitly.
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
    };
    let status = effects.status();
    if status.success() {
        bail!("{label} was expected to abort on-chain but succeeded");
    }
    println!("{:>24}  aborted as expected: {}", label, status.error());
    Ok(row)
}

/// The object ids every phase reads from the state file.
fn read_state(path: &Path) -> Result<HashMap<String, String>> {
    let mut ids = HashMap::new();
    for line in fs::read_to_string(path)?.lines() {
        if let Some((k, v)) = line.split_once('=') {
            ids.insert(k.to_string(), v.to_string());
        }
    }
    Ok(ids)
}

fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N]> {
    let v = alloy_primitives::hex::decode(s).context("parsing hex")?;
    let len = v.len();
    v.try_into()
        .map_err(|_| anyhow::anyhow!("expected {} bytes, got {}", N, len))
}

fn load_key(path: &Path) -> Result<Ed25519PrivateKey> {
    let raw = fs::read_to_string(path)?;
    podseq_sui::parse_signer_key(raw.trim()).map_err(|e| anyhow::anyhow!("parsing key: {e}"))
}

fn random_owner_key() -> Result<SigningKey> {
    use std::io::Read;
    let mut seed = [0u8; 32];
    fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut seed)
        .context("reading /dev/urandom")?;
    SigningKey::from_slice(&seed).context("deriving secp256k1 key")
}

/// Builds a `<package>::<module>::<name>` move-call target.
fn module_fn(package: Address, module: &str, name: &str) -> Function {
    Function::new(
        package,
        Identifier::new(module).expect("static module name"),
        Identifier::new(name).expect("static function name"),
    )
}

/// An L2 account leaf: the plain four-item RLP list the Move decoder reads.
struct Account {
    address: L2Address,
    nonce: u64,
    balance: U256,
}

impl Account {
    fn value(&self) -> Vec<u8> {
        let mut items = Vec::with_capacity(160);
        self.nonce.encode(&mut items);
        self.balance.encode(&mut items);
        keccak256([]).encode(&mut items);
        keccak256([]).encode(&mut items);
        let mut out = Vec::with_capacity(items.len() + 4);
        Header {
            list: true,
            payload_length: items.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(&items);
        out
    }
}

fn key_of(account: &Account) -> Nibbles {
    Nibbles::unpack(keccak256(account.address).as_slice())
}

/// Validated root-first account proofs, one per retained target.
type Proofs = Vec<Vec<Vec<u8>>>;

/// Builds a trie over the accounts and returns (root, validated root-first
/// proofs for the retained targets).
fn build_trie(accounts: &[Account], targets: &[usize]) -> Result<([u8; 32], Proofs)> {
    let mut entries: Vec<(Nibbles, Vec<u8>)> =
        accounts.iter().map(|a| (key_of(a), a.value())).collect();
    entries.sort_by_key(|entry| entry.0);

    let retainer = ProofRetainer::new(targets.iter().map(|i| key_of(&accounts[*i])).collect());
    let mut builder = HashBuilder::default().with_proof_retainer(retainer);
    for (nibbles, value) in &entries {
        builder.add_leaf(*nibbles, value);
    }
    let root = builder.root();
    let proof_nodes = builder.take_proof_nodes();

    let mut proofs = Vec::new();
    for i in targets {
        let target = key_of(&accounts[*i]);
        let ordered: Vec<Vec<u8>> = proof_nodes
            .matching_nodes_sorted(&target)
            .into_iter()
            .map(|(_, bytes)| bytes.to_vec())
            .collect();
        let expected = accounts[*i].value();
        let nodes: Vec<alloy_primitives::Bytes> =
            ordered.iter().map(|n| n.clone().into()).collect();
        verify_proof(root, target, Some(expected), nodes.iter())
            .context("generated proof must verify against its own root")?;
        proofs.push(ordered);
    }
    let root_bytes: [u8; 32] = root.0;
    Ok((root_bytes, proofs))
}

/// Two addresses whose keccak keys share `shared_nibbles` nibbles, via a
/// birthday search over keccak-derived addresses — the depth force behind a
/// root extension node.
fn shared_prefix_pair(shared_nibbles: usize) -> (L2Address, L2Address) {
    let mut seen: HashMap<u64, L2Address> = HashMap::new();
    for i in 0u64.. {
        let address = L2Address::from_word(keccak256(i.to_be_bytes()));
        let hash = keccak256(address);
        let mut prefix = 0u64;
        for n in 0..shared_nibbles {
            let byte = hash[n / 2];
            let nibble = if n % 2 == 0 { byte >> 4 } else { byte & 0x0f };
            prefix = (prefix << 4) | u64::from(nibble);
        }
        if let Some(first) = seen.insert(prefix, address) {
            return (first, address);
        }
    }
    unreachable!("birthday search cannot exhaust u64")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        bail!("usage: exit_gas_bench <rpc_url> <deployer_key> <state_file> <deploy|prepare|halt>");
    }
    let rpc_url = args[1].clone();
    let deployer_path = PathBuf::from(&args[2]);
    let state_path = PathBuf::from(&args[3]);
    let gas_csv = state_path.with_extension("gas.csv");
    let deployer = load_key(&deployer_path)?;

    match args[4].as_str() {
        "deploy" => deploy(&rpc_url, &deployer_path, &deployer, &state_path).await,
        "prepare" => prepare(&rpc_url, &deployer, &state_path, &gas_csv).await,
        "gate" => gate(&rpc_url, &deployer, &state_path, &gas_csv).await,
        "halt" => halt(&rpc_url, &deployer, &state_path, &gas_csv).await,
        other => bail!("unknown phase {other} (expected deploy, prepare, gate, or halt)"),
    }
}

/// Parses a 32-byte object id out of the state file.
fn sui_address(state: &HashMap<String, String>, key: &str) -> Result<Address> {
    let raw = state.get(key).context(format!("state missing {key}"))?;
    let bytes: [u8; 32] = parse_hex(raw)?;
    Ok(Address::new(bytes))
}

/// Deploys the package and the vault and generates the two L2 owner keys.
/// Short on purpose: this is the phase an interruption is most likely to
/// hit, so everything after it can resume from the state file.
async fn deploy(
    rpc_url: &str,
    deployer_path: &Path,
    deployer: &Ed25519PrivateKey,
    state_path: &Path,
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
        modules.push(fs::read(&p)?);
    }
    println!("publishing {} modules", modules.len());
    let contract = Settlement::deploy(deployer_path, rpc_url, modules).await?;
    let package: Address = contract.package_id.parse().context("parsing package id")?;
    let registry: Address = contract.registry_id.parse().context("parsing registry")?;
    let inbox: Address = contract.inbox_id.parse().context("parsing inbox")?;
    let queue: Address = contract.exit_queue_id.parse().context("parsing queue")?;
    let ops_cap: Address = contract.ops_cap_id.parse().context("parsing ops cap")?;

    println!("initializing the bridge vault");
    let bridge =
        BridgeClient::initialize_with_key(deployer.clone(), rpc_url, &contract.package_id).await?;
    let vault: Address = bridge.vault_id.parse().context("parsing vault")?;

    // Two throwaway L2 owner keys: the addresses their keys derive must be
    // the addresses the trie proves.
    let owner_a = random_owner_key()?;
    let owner_b = random_owner_key()?;
    let addr_a = eth_address(&owner_a);
    let addr_b = eth_address(&owner_b);

    let mut state = format!(
        "package={package}\nregistry={registry}\ninbox={inbox}\nqueue={queue}\nvault={vault}\nops_cap={ops_cap}\nrecipient={}\n",
        deployer.public_key().derive_address()
    );
    state.push_str(&format!(
        "owner_a={}\n",
        alloy_primitives::hex::encode(owner_a.to_bytes())
    ));
    state.push_str(&format!(
        "owner_b={}\n",
        alloy_primitives::hex::encode(owner_b.to_bytes())
    ));
    state.push_str(&format!(
        "addr_a={}\naddr_b={}\n",
        alloy_primitives::hex::encode(addr_a),
        alloy_primitives::hex::encode(addr_b)
    ));
    state.push_str(&format!("balance_a={BALANCE_A}\nbalance_b={BALANCE_B}\n"));
    fs::write(state_path, &state).with_context(|| format!("writing {}", state_path.display()))?;
    println!(
        "state written to {}; run the prepare phase",
        state_path.display()
    );
    Ok(())
}

/// After deploy: anchors the two settled roots (a single-leaf trie and a
/// deep nine-account trie), funds the vault reserve, narrows the window to
/// its floor, submits the forced transaction, and proves the gate is still
/// closed. Ends the phase inside the halt margin.
async fn prepare(
    rpc_url: &str,
    deployer: &Ed25519PrivateKey,
    state_path: &Path,
    gas_csv: &Path,
) -> Result<()> {
    let state = read_state(state_path)?;
    let package: Address = sui_address(&state, "package")?;
    let registry: Address = sui_address(&state, "registry")?;
    let inbox: Address = sui_address(&state, "inbox")?;
    let vault: Address = sui_address(&state, "vault")?;
    let ops_cap: Address = sui_address(&state, "ops_cap")?;
    let addr_a: [u8; 20] = parse_hex(state.get("addr_a").context("state missing addr_a")?)?;
    let addr_b: [u8; 20] = parse_hex(state.get("addr_b").context("state missing addr_b")?)?;

    // Shallow trie: one account, so the proof is the root leaf itself.
    let shallow = vec![Account {
        address: addr_a.into(),
        nonce: 1,
        balance: U256::from(BALANCE_A),
    }];
    let (root1, proofs1) = build_trie(&shallow, &[0])?;
    println!("shallow proof: {} node(s)", proofs1[0].len());

    // Deep trie: the pair shares 10 keccak nibbles (root extension), plus
    // filler accounts so the proof walks branch nodes.
    let (x, y) = shared_prefix_pair(10);
    let mut deep: Vec<Account> = vec![Account {
        address: addr_b.into(),
        nonce: 2,
        balance: U256::from(BALANCE_B),
    }];
    deep.push(Account {
        address: x,
        nonce: 3,
        balance: U256::from(7u64),
    });
    deep.push(Account {
        address: y,
        nonce: 4,
        balance: U256::from(9u64),
    });
    for i in 0u64..6 {
        deep.push(Account {
            address: L2Address::from_word(keccak256([b'd', i as u8])),
            nonce: i + 10,
            balance: U256::from(100u64 + i),
        });
    }
    let (root2, proofs2) = build_trie(&deep, &[0])?;
    println!("deep proof: {} node(s)", proofs2[0].len());

    // Anchor both roots through the settle gate (the settle + commit pair the
    // production PTB composes), while the inbox is still drained.
    measure_settle_anchor(
        rpc_url,
        deployer,
        "settle_anchor_h1",
        package,
        registry,
        inbox,
        1,
        root1,
    )
    .await?
    .0
    .append_csv(gas_csv)?;
    measure_settle_anchor(
        rpc_url,
        deployer,
        "settle_anchor_h2",
        package,
        registry,
        inbox,
        2,
        root2,
    )
    .await?
    .0
    .append_csv(gas_csv)?;

    // Fund the vault reserve: split a coin and deposit it for L2 minting.
    execute_measured("deposit_vault", rpc_url, deployer, |b| {
        let vault_arg = b.object(ObjectInput::new(vault));
        let gas = b.gas();
        let amount = b.pure(&DEPOSIT_MIST);
        let coin = b.split_coins(gas, vec![amount]);
        let recipient = b.pure(&[0u8; 20].to_vec());
        b.move_call(
            module_fn(package, "bridge", "deposit")
                .with_type_args(vec![SUI_TYPE.parse().expect("valid type tag")]),
            vec![vault_arg, coin[0], recipient],
        );
    })
    .await?
    .0
    .append_csv(gas_csv)?;

    // Narrow the inclusion window to the 1-minute floor (steered by the
    // Registry's OpsCap; narrowing applies in the staging transaction).
    execute_measured("window_narrow", rpc_url, deployer, |b| {
        let registry_arg = b.object(ObjectInput::new(registry));
        let inbox_arg = b.object(ObjectInput::new(inbox));
        let clock = b.object(ObjectInput::new(SUI_CLOCK_ID));
        let window = b.pure(&NARROW_WINDOW_MS);
        let delay = b.pure(&0_u64);
        let ops = b.object(ObjectInput::new(ops_cap));
        b.move_call(
            module_fn(package, "settlement", "request_window_change"),
            vec![registry_arg, inbox_arg, clock, window, delay, ops],
        );
    })
    .await?
    .0
    .append_csv(gas_csv)?;

    // Submit the forced transaction the chain will then hold past its
    // deadline — the sequencer-abstention that opens the escape path.
    execute_measured("inbox_submit", rpc_url, deployer, |b| {
        let inbox_arg = b.object(ObjectInput::new(inbox));
        let clock = b.object(ObjectInput::new(SUI_CLOCK_ID));
        let tx_bytes = b.pure(&vec![0xdeu8, 0xad, 0xbe, 0xef]);
        b.move_call(
            module_fn(package, "inbox", "submit"),
            vec![inbox_arg, clock, tx_bytes],
        );
    })
    .await?
    .0
    .append_csv(gas_csv)?;

    append_roots_and_proofs(state_path, root1, root2, &proofs1[0], &proofs2[0])?;
    println!(
        "the verdict opens ~{} s after the submit (window {NARROW_WINDOW_MS} ms + halt margin 600000 ms); \
         run the gate phase while it is closed, then halt",
        (NARROW_WINDOW_MS + 600_000) / 1000
    );
    Ok(())
}

/// Appends the anchored roots and root-first proofs to the state file.
fn append_roots_and_proofs(
    state_path: &Path,
    root1: [u8; 32],
    root2: [u8; 32],
    proof_a: &[Vec<u8>],
    proof_b: &[Vec<u8>],
) -> Result<()> {
    let mut state = fs::read_to_string(state_path)
        .with_context(|| format!("reading {}", state_path.display()))?;
    state.push_str(&format!(
        "root_a={}\nroot_b={}\n",
        alloy_primitives::hex::encode(root1),
        alloy_primitives::hex::encode(root2)
    ));
    for (i, node) in proof_a.iter().enumerate() {
        state.push_str(&format!(
            "proof_a_{i}={}\n",
            alloy_primitives::hex::encode(node)
        ));
    }
    for (i, node) in proof_b.iter().enumerate() {
        state.push_str(&format!(
            "proof_b_{i}={}\n",
            alloy_primitives::hex::encode(node)
        ));
    }
    fs::write(state_path, &state).with_context(|| format!("writing {}", state_path.display()))?;
    Ok(())
}

/// While the gate is still closed: rebuilds the tries from the recorded
/// addresses, stores the roots and proofs for the halt phase, and measures
/// the `request_exit` rejection. Run this after `prepare` (which dies at
/// this step: the builder's simulation refuses an intentionally-aborting
/// PTB, so the abort path needs the no-simulation build).
async fn gate(
    rpc_url: &str,
    deployer: &Ed25519PrivateKey,
    state_path: &Path,
    gas_csv: &Path,
) -> Result<()> {
    let state = read_state(state_path)?;
    let package: Address = sui_address(&state, "package")?;
    let registry: Address = sui_address(&state, "registry")?;
    let inbox: Address = sui_address(&state, "inbox")?;
    let queue: Address = sui_address(&state, "queue")?;
    let addr_a: [u8; 20] = parse_hex(state.get("addr_a").context("state missing addr_a")?)?;
    let addr_b: [u8; 20] = parse_hex(state.get("addr_b").context("state missing addr_b")?)?;

    let shallow = vec![Account {
        address: addr_a.into(),
        nonce: 1,
        balance: U256::from(BALANCE_A),
    }];
    let (root1, proofs1) = build_trie(&shallow, &[0])?;
    let (x, y) = shared_prefix_pair(10);
    let mut deep: Vec<Account> = vec![Account {
        address: addr_b.into(),
        nonce: 2,
        balance: U256::from(BALANCE_B),
    }];
    deep.push(Account {
        address: x,
        nonce: 3,
        balance: U256::from(7u64),
    });
    deep.push(Account {
        address: y,
        nonce: 4,
        balance: U256::from(9u64),
    });
    for i in 0u64..6 {
        deep.push(Account {
            address: L2Address::from_word(keccak256([b'd', i as u8])),
            nonce: i + 10,
            balance: U256::from(100u64 + i),
        });
    }
    let (root2, proofs2) = build_trie(&deep, &[0])?;
    println!(
        "proofs: shallow {} node(s), deep {} node(s)",
        proofs1[0].len(),
        proofs2[0].len()
    );

    append_roots_and_proofs(state_path, root1, root2, &proofs1[0], &proofs2[0])?;

    // The no-simulation build cannot resolve shared objects from the chain,
    // so the inputs are pinned explicitly: every shared object here was
    // shared at version 1 (publish-time creation; the Clock at genesis).
    execute_measured_expect_abort("request_too_early", rpc_url, deployer, |b| {
        let queue_arg = b.object(ObjectInput::shared(queue, 1, true));
        let registry_arg = b.object(ObjectInput::shared(registry, 1, false));
        let inbox_arg = b.object(ObjectInput::shared(inbox, 1, false));
        let clock_arg = b.object(ObjectInput::shared(SUI_CLOCK_ID, 1, false));
        let height_arg = b.pure(&1_u64);
        let address_arg = b.pure(&addr_a.to_vec());
        let proof_arg = b.pure(&proofs1[0]);
        b.move_call(
            module_fn(package, "exit_queue", "request_exit"),
            vec![
                queue_arg,
                registry_arg,
                inbox_arg,
                clock_arg,
                height_arg,
                address_arg,
                proof_arg,
            ],
        );
    })
    .await?
    .append_csv(gas_csv)?;
    Ok(())
}

/// One settle + commit_state_root pair in a single PTB — the production
/// settle composition.
#[allow(clippy::too_many_arguments)]
async fn measure_settle_anchor(
    rpc_url: &str,
    deployer: &Ed25519PrivateKey,
    label: &'static str,
    package: Address,
    registry: Address,
    inbox: Address,
    height: u64,
    root: [u8; 32],
) -> Result<(GasRow, ExecuteTransactionResponse)> {
    execute_measured(label, rpc_url, deployer, |b| {
        let registry_arg = b.object(ObjectInput::new(registry));
        let inbox_arg = b.object(ObjectInput::new(inbox));
        let clock = b.object(ObjectInput::new(SUI_CLOCK_ID));
        let blob = b.pure(&vec![1_u8, 2, 3, 4]);
        let height_arg = b.pure(&height);
        let block_ts = b.pure(&NO_BLOCK_TS);
        let cursor = b.pure(&0_u64);
        b.move_call(
            module_fn(package, "settlement", "settle_as"),
            vec![
                registry_arg,
                inbox_arg,
                clock,
                blob,
                height_arg,
                block_ts,
                cursor,
            ],
        );
        let registry_arg2 = b.object(ObjectInput::new(registry));
        let clock2 = b.object(ObjectInput::new(SUI_CLOCK_ID));
        let height_arg2 = b.pure(&height);
        let root_arg = b.pure(&root.to_vec());
        b.move_call(
            module_fn(package, "settlement", "commit_state_root"),
            vec![registry_arg2, clock2, height_arg2, root_arg],
        );
    })
    .await
}

/// After the wait: records both exit requests (shallow and deep proofs),
/// redeems the shallow one through the vault, and replays the redemption.
async fn halt(
    rpc_url: &str,
    deployer: &Ed25519PrivateKey,
    state_path: &Path,
    gas_csv: &Path,
) -> Result<()> {
    let state = read_state(state_path)?;
    let package: Address = sui_address(&state, "package")?;
    let registry: Address = sui_address(&state, "registry")?;
    let inbox: Address = sui_address(&state, "inbox")?;
    let queue: Address = sui_address(&state, "queue")?;
    let vault: Address = sui_address(&state, "vault")?;
    let recipient: Address = sui_address(&state, "recipient")?;
    let addr_a: [u8; 20] = parse_hex(state.get("addr_a").context("state missing addr_a")?)?;
    let addr_b: [u8; 20] = parse_hex(state.get("addr_b").context("state missing addr_b")?)?;
    let owner_a = SigningKey::from_slice(&parse_hex::<32>(
        state.get("owner_a").context("state missing owner_a")?,
    )?)?;
    let proof_a = load_proof(&state, "proof_a")?;
    let proof_b = load_proof(&state, "proof_b")?;
    let balance_a: u64 = state
        .get("balance_a")
        .context("state missing balance_a")?
        .parse()
        .context("parsing balance_a")?;

    let targets = podseq_sui::exit_queue::ExitTargets {
        package,
        queue,
        registry,
        inbox,
    };

    let (shallow_row, _) = execute_measured("request_exit_shallow", rpc_url, deployer, |b| {
        append_request_exit(b, &targets, 1, addr_a, proof_a.clone());
    })
    .await?;
    shallow_row.append_csv(gas_csv)?;

    let (deep_row, _) = execute_measured("request_exit_deep", rpc_url, deployer, |b| {
        append_request_exit(b, &targets, 2, addr_b, proof_b.clone());
    })
    .await?;
    deep_row.append_csv(gas_csv)?;

    // The owner signs the payout the module re-derives from the record; the
    // redeem + escape_release pair pays the vault reserve to the recipient.
    let mut amount_be = [0u8; 32];
    amount_be[24..].copy_from_slice(&balance_a.to_be_bytes());
    let (payload, signature) = sign_exit_payload(&owner_a, addr_a, amount_be, recipient)?;
    let redeem = RedeemRequest {
        address: addr_a,
        recipient,
        payload,
        signature: signature.to_vec(),
        coin_type: SUI_TYPE.parse().expect("valid type tag"),
    };
    let redeem_targets = RedeemTargets {
        package,
        queue,
        inbox,
        vault,
    };
    let append = |b: &mut TransactionBuilder| append_redeem(b, &redeem_targets, &redeem);
    let (redeem_row, _) =
        execute_measured("redeem_escape_release", rpc_url, deployer, append).await?;
    redeem_row.append_csv(gas_csv)?;

    // A replayed signature covers the spent amount; the advanced paid counter
    // rejects it before signature work. Same no-simulation treatment as the
    // gate row: shared inputs pinned to their initial versions.
    execute_measured_expect_abort("replay_aborted", rpc_url, deployer, |b| {
        let queue_arg = b.object(ObjectInput::shared(queue, 1, true));
        let inbox_arg = b.object(ObjectInput::shared(inbox, 1, false));
        let clock_arg = b.object(ObjectInput::shared(SUI_CLOCK_ID, 1, false));
        let address_arg = b.pure(&redeem.address.to_vec());
        let recipient_arg = b.pure(&redeem.recipient);
        let payload_arg = b.pure(&redeem.payload);
        let signature_arg = b.pure(&redeem.signature);
        let claim = b.move_call(
            module_fn(package, "exit_queue", "redeem"),
            vec![
                queue_arg,
                inbox_arg,
                clock_arg,
                address_arg,
                recipient_arg,
                payload_arg,
                signature_arg,
            ],
        );
        let vault_arg = b.object(ObjectInput::shared(vault, 1, true));
        b.move_call(
            module_fn(package, "bridge", "escape_release")
                .with_type_args(vec![SUI_TYPE.parse().expect("valid type tag")]),
            vec![vault_arg, claim],
        );
    })
    .await?
    .append_csv(gas_csv)?;

    println!("rows appended to {}", gas_csv.display());
    Ok(())
}

/// Loads a stored proof (root-first node list) from the state file.
fn load_proof(state: &HashMap<String, String>, prefix: &str) -> Result<Vec<Vec<u8>>> {
    let mut nodes = Vec::new();
    let mut i = 0;
    while let Some(hex) = state.get(&format!("{prefix}_{i}")) {
        nodes.push(alloy_primitives::hex::decode(hex).context("parsing proof node")?);
        i += 1;
    }
    if nodes.is_empty() {
        bail!("state has no {prefix}_* proof nodes");
    }
    Ok(nodes)
}
