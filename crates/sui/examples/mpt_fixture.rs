//! Generates Merkle-Patricia-Trie account-proof fixtures for the Move
//! exit-queue verifier tests (`move/sources/exit_queue.move`).
//!
//! Builds small state tries with `alloy-trie` (the node encoder reth itself
//! uses), retains the proof for one target account per trie, orders the proof
//! root-first, and validates each ordered proof with
//! `alloy_trie::proof::verify_proof` before printing it as Move literals.
//! Paste the output into the Move test module; the generator is the fixture
//! provenance, not a runtime dependency.
//!
//! Tries:
//! - `SINGLE`: one account, so the proof is the root leaf itself.
//! - `SHARED`: two accounts whose keccak keys share the first 10 nibbles
//!   (birthday search), forcing a root extension node over the shared prefix.
//! - `MULTI`: eight accounts (one with zero balance, one with a large
//!   balance), proving through several branch nodes.
//!
//! Run: `cargo run -p podseq-sui --example mpt_fixture`

use std::collections::HashMap;

use alloy_primitives::{keccak256, Address, U256};
use alloy_rlp::{Encodable, Header};
use alloy_trie::proof::{verify_proof, ProofRetainer};
use alloy_trie::{HashBuilder, Nibbles};

/// Account leaf value: the plain four-item RLP list the Move decoder reads.
struct Account {
    address: Address,
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

fn key_of(account: &Account) -> (Nibbles, [u8; 32]) {
    let hash = keccak256(account.address);
    (Nibbles::unpack(hash.as_slice()), hash.0)
}

/// Builds a trie over the accounts and returns (root, proof nodes for the
/// retained targets). Leaves must be added in sorted key order.
fn build_trie(
    accounts: &[Account],
    targets: &[usize],
) -> (alloy_primitives::B256, Vec<Vec<Vec<u8>>>) {
    let mut entries: Vec<(Nibbles, Vec<u8>)> =
        accounts.iter().map(|a| (key_of(a).0, a.value())).collect();
    entries.sort_by_key(|entry| entry.0);

    let retainer = ProofRetainer::new(targets.iter().map(|i| key_of(&accounts[*i]).0).collect());
    let mut builder = HashBuilder::default().with_proof_retainer(retainer);
    for (nibbles, value) in &entries {
        builder.add_leaf(*nibbles, value);
    }
    let root = builder.root();
    let proof_nodes = builder.take_proof_nodes();

    let proofs = targets
        .iter()
        .map(|i| {
            let target = key_of(&accounts[*i]).0;
            let ordered: Vec<Vec<u8>> = proof_nodes
                .matching_nodes_sorted(&target)
                .into_iter()
                .map(|(_, bytes)| bytes.to_vec())
                .collect();
            let expected = accounts[*i].value();
            let nodes: Vec<alloy_primitives::Bytes> =
                ordered.iter().map(|n| n.clone().into()).collect();
            verify_proof(root, target, Some(expected), nodes.iter())
                .expect("generated proof must verify against its own root");
            ordered
        })
        .collect();
    (root, proofs)
}

/// Two accounts whose keccak keys share `shared_nibbles` nibbles, via a
/// birthday search over keccak-derived addresses.
fn shared_prefix_pair(shared_nibbles: usize) -> (Address, Address) {
    assert!(
        shared_nibbles <= 16,
        "a u64 prefix holds at most 16 nibbles"
    );
    let mut seen: HashMap<u64, Address> = HashMap::new();
    for i in 0u64.. {
        let address = Address::from_word(keccak256(i.to_be_bytes()));
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

fn print_fixture(
    name: &str,
    accounts: &[Account],
    target: usize,
    root: alloy_primitives::B256,
    proof: &[Vec<u8>],
) {
    println!("// ---- {name} ----");
    println!(
        "// trie root over {} account(s), proof of {} node(s)",
        accounts.len(),
        proof.len()
    );
    println!(
        "const {name}_ROOT: vector<u8> = x\"{}\";",
        alloy_primitives::hex::encode(root)
    );
    for (i, node) in proof.iter().enumerate() {
        println!(
            "const {name}_NODE_{i}: vector<u8> = x\"{}\";",
            alloy_primitives::hex::encode(node)
        );
    }
    let account = &accounts[target];
    println!(
        "const {name}_ADDRESS: vector<u8> = x\"{}\";",
        alloy_primitives::hex::encode(account.address)
    );
    println!(
        "const {name}_KEY: vector<u8> = x\"{}\"; // keccak(address), reference",
        alloy_primitives::hex::encode(key_of(account).1)
    );
    println!("const {name}_NONCE: u64 = {};", account.nonce);
    println!("const {name}_BALANCE: u256 = {};", account.balance);
    println!();
}

fn main() {
    let empty = Account {
        address: Address::from_word(keccak256(b"single")),
        nonce: 1,
        balance: U256::from(999u64),
    };

    let (root, proofs) = build_trie(std::slice::from_ref(&empty), &[0]);
    print_fixture("SINGLE", std::slice::from_ref(&empty), 0, root, &proofs[0]);

    let (a, b) = shared_prefix_pair(10);
    let shared = vec![
        Account {
            address: a,
            nonce: 7,
            balance: U256::from(3_250_000_000_000_000_000_000_000u128),
        },
        Account {
            address: b,
            nonce: 2,
            balance: U256::from(1u64),
        },
    ];
    let (root, proofs) = build_trie(&shared, &[0]);
    print_fixture("SHARED", &shared, 0, root, &proofs[0]);

    let mut multi: Vec<Account> = (0..8)
        .map(|i| Account {
            address: Address::from_word(keccak256([b'm', b'8', i])),
            nonce: i as u64 * 3,
            balance: if i == 3 {
                U256::ZERO
            } else {
                U256::from(1_000_000_000_000_000_000u64 * (i as u64 + 1))
            },
        })
        .collect();
    // A ninth account sharing the target key's first 5 nibbles: the target's
    // proof gains a branch-adjacent extension node below the root.
    let target_prefix = key_of(&multi[5]).1;
    let mut extra = None;
    for i in 0u64.. {
        let address = Address::from_word(keccak256(i.to_be_bytes()));
        let hash = keccak256(address);
        let shared = target_prefix
            .iter()
            .zip(hash.iter())
            .take(2) // 5 nibbles = 2 full bytes + 1 nibble
            .all(|(a, b)| a == b)
            && (target_prefix[2] >> 4) == (hash[2] >> 4);
        if shared {
            extra = Some(Account {
                address,
                nonce: 42,
                balance: U256::from(42u64),
            });
            break;
        }
    }
    multi.push(extra.expect("a 20-bit keccak prefix collision is reachable by brute force"));
    let (root, proofs) = build_trie(&multi, &[5, 3]);
    print_fixture("MULTI", &multi, 5, root, &proofs[0]);
    print_fixture("MULTI_ZERO", &multi, 3, root, &proofs[1]);
}
