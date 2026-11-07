# Exit queue

Force inclusion gets transactions in; the exit queue gets value out. A censoring
sequencer can also refuse withdrawals, so the bridge needs the same property the
inbox has: a path that works without the sequencer's cooperation, and a halt that
unlocks it.

The design follows the roadmap: withdrawals are ordered by L2 block height so the
sequencer cannot reorder exits to front-run users, and once settlement stalls for
long enough, any user can prove their L2 balance to the Sui bridge and withdraw
without the sequencer.

## Status

The proof anchor and the balance verifier are implemented. The unlock
condition, the ordered queue on the bridge, and a live gas measurement of the
proof walk are not.

Implemented: `settlement::commit_state_root` anchors each settled block's
state root in the Registry (see below), and `exit_queue::verify_account`
verifies a Merkle account proof against that anchor and returns the account's
`(nonce, balance)`.

## Why state roots land on Sui

Every sequencer block header carries the post-execution state root of the L2
(`crates/engine` reads it from Reth's execution payload, and the header hash
commits to it). Until now that root existed only inside the Walrus blob. That is
not enough for exits: a Move contract that verifies a balance proof needs the
root as a first-class value it can read, and a light client that verifies one
balance should not have to fetch and re-execute a whole batch.

So the sequencer anchors the root where settlement lives:

```move
public fun commit_state_root(
    registry: &mut Registry,
    clock: &sui::clock::Clock,
    height: u64,
    state_root: vector<u8>,
    ctx: &TxContext,
)
```

Design decisions, mostly mirroring the settle path:

- **Composed into the settle PTB.** `commit_state_root` runs immediately after
  `settle_as` in the same programmable transaction block. The root names the
  block that settle just recorded, so the pair is atomic: both land or neither
  does. The Move entrypoint enforces `height == registry.latest_height`, which
  pins every write to the settle moment.
- **Same authorization as settle.** `rotation::assert_can_act` checks the
  sender, so nobody can anchor a root the sequencer never committed. This is
  the load-bearing check: a wrong root at a settled height would poison every
  balance proof verified against it.
- **Tip-only writes.** Roots cannot be backfilled at old heights. A first
  write is only valid at the tip, inside the PTB whose settle produced it, so
  the value cannot drift from the block it names.
- **Crash-safe re-commit.** Re-committing the identical root at the tip is a
  no-op success (a retried PTB replays both commands). A different value
  aborts (`E_STATE_ROOT_CONFLICT`). The header carrying the root is signed and
  committed in the blob, so a mismatch is an attributable equivocation, the
  same rule `settle` applies to blob ids.
- **Derived storage.** One dynamic field per height off the Registry UID, like
  the inclusion window on the Inbox. `Registry` is a public struct whose layout
  froze at publish; new state goes off-struct.

The sequencer sends `sui.commit_state_roots` (default on). Pin it off only
while the deployed Move package predates the entrypoint: the appended call
would abort the whole settle PTB and halt settlement.

## The balance verifier

`podseq::exit_queue::verify_account` checks an account proof against the
anchored root and returns `(nonce, balance)`:

```move
public fun verify_account(
    registry: &Registry,
    height: u64,
    address: vector<u8>,        // 20-byte L2 address
    proof: vector<vector<u8>>,  // RLP nodes, root first (the eth_getProof shape)
): (u64, u256)
```

The walk reimplements what Ethereum proofs are made of, in Move:

- **RLP.** One header decoder plus a list splitter cover branch nodes (17
  items), extension and leaf nodes (2 items), and the account list itself.
  Bounds are checked before any byte is read, so a truncated proof aborts
  with a named error instead of panicking on an out-of-range index.
- **Hex-prefix paths.** Leaf and extension share the 2-item shape; the flag
  byte's high bits tell them apart, and both decode to nibbles.
- **The walk.** The key is `keccak256(address)` expanded to 64 nibbles.
  Branches route by the next nibble, extensions must match their decoded
  path exactly, and the leaf must match what remains of the key. An empty
  branch slot aborts: a dead walk is a rejected proof, not a balance of zero.
- **Every link is checked.** `keccak(proof[0])` must equal the anchored root,
  and each hashed child must hash to the reference its parent embedded.
  Inline (sub-32-byte) child nodes are handled per spec, though account-trie
  nodes never get that small. The walk must consume exactly the whole proof,
  so a proof cannot smuggle unchecked material past the root check.
- **The account.** The leaf value decodes as the four-item account RLP: the
  nonce (≤ 8 bytes) and the balance (≤ 32 bytes, into a u256) come out; the
  storage root and code hash are shape-checked and discarded.

The anchored root is the only Sui-side input, and the same sender check that
guards settle guards the anchor, so a proof verifies against state the
sequencer actually committed. Everything else is checked arithmetic over the
bytes the proof itself carries.

Tests run against fixtures from `crates/sui/examples/mpt_fixture.rs`, which
builds tries with alloy-trie (reth's own node encoder), retains root-first
proofs, validates each against its root, and prints them as Move literals:
a single-leaf trie, a root extension over a 10-nibble shared prefix, a
nine-account trie whose proof walks branch, extension, branch, leaf, and a
zero-balance account (an empty RLP integer). Negative tests tamper nodes,
truncate and extend the proof, and aim the walk at a foreign address, a dead
branch, and a non-trie root.

Open: the gas number. Sui's gas model has no offline estimate, so the walk's
cost is unmeasured until a testnet publish runs `verify_account` against a
real proof — the same bench the rotation ceremony used. The loop is bounded
by the proof length, so nothing is unbounded, but the MIST number is the
evidence the gas question needs, and it is the next slice's first item.

## How the exit path completes

The remaining pieces, against the anchor and verifier that now exist:

1. A user's L2 balance is included in the state of some settled height H,
   and anyone can prove it with `verify_account` against the root anchored
   at H.
2. The escape entrypoint unlocks only when the inbox-derived halt condition
   holds: the oldest pending forced transaction is past its deadline while
   settlement does not advance. That is the same chain data the K3 deadline
   monitor reads, so the unlock is verifiable without trusting any sequencer
   endpoint.
3. Exit requests take a queue position from the L2 height that produced the
   balance, so reordering exits is as impossible as reordering blocks.

A useful side effect of the anchor: a light client can verify one balance
against Sui state without executing any L2 block, and a full node can compare
the anchored root at each height against its own execution-derived root. A
mismatch is an equivocation signed by the settlement key.

## Alternatives considered

- **Sequencer-signed balance attestations.** The sequencer signs "address A
  holds X at height H" and the bridge accepts the signature. Cheap, but it
  trusts the censoring party to attest honestly. Rejected: the escape hatch
  must not depend on the sequencer's cooperation.
- **ZK state transitions.** A validity proof over the state transition would
  make balance proofs tiny. Rejected for now: proving EVM execution in a
  circuit is a project of its own, and the bridge needs an escape hatch before
  that exists.
- **Optimistic challenge windows.** Post the root, let watchers dispute it.
  Rejected: during a settlement stall, the honest sequencer may be the party
  that is down, and a dispute game adds a second failure mode to a path whose
  whole purpose is to work when the sequencer does not.

The settled state root keeps the trust model honest: the same party commits
both the data (Walrus blob) and the state it implies (Sui anchor), the header
signature binds them, and anyone can check the binding from chain data alone.
