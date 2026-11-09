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

The proof anchor, the balance verifier, the unlock condition, the request
record, and redemption are implemented. A live gas measurement of the proof
walk is not.

Implemented: `settlement::commit_state_root` anchors each settled block's
state root in the Registry (see below); `exit_queue::verify_account` verifies
a Merkle account proof against that anchor and returns the account's
`(nonce, balance)`; `exit_queue::halt_expired_deadline` derives the halt
verdict from inbox chain data; `exit_queue::request_exit` records a proven
balance in the shared exit queue, ordered by `(height, seq)`; and
`exit_queue::redeem` plus `bridge::escape_release` pay the remainder out of
the vault without the sequencer (see below).

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

## Redeeming

`exit_queue::redeem` consumes a recorded request and pays out the remainder
`balance - paid` without any sequencer capability. The request record alone
is not enough: proofs are public chain data, so anyone can call `redeem`
with a valid proof — the payout must land with the balance's owner, and only
the owner can prove *that*. The owner signs with their L2 key, and the
module recovers the signer in Move:

```move
public fun redeem(
    queue: &mut ExitQueue,
    inbox: &Inbox,
    clock: &Clock,
    address: vector<u8>,       // the recorded 20-byte L2 address
    recipient: address,        // the Sui address that gets the coins
    payload: vector<u8>,       // what the owner signed (99 bytes)
    signature: vector<u8>,     // 65-byte (r, s, v) EIP-191 signature
): bridge::ExitClaim
```

The payload binds everything a payout needs: the domain `podseq-exit:v1:`,
the 20-byte L2 address, the 32-byte big-endian amount, and the 32-byte Sui
recipient. The module builds the EIP-191 personal-sign preimage
(`\x19Ethereum Signed Message:\n` ‖ decimal length ‖ payload — what wallets
produce with `personal_sign`), recovers the public key with the framework's
`secp256k1_ecrecover` (keccak256 hash mode; the Ethereum v byte 27/28 is
normalized to the recovery id 0/1), and derives the Ethereum address as
`keccak256(X ‖ Y)[12..32]`. Three checks then gate the payout:

- the recovered address must equal the recorded L2 address — the owner
  signed, nobody else;
- the payload's amount must equal the state-derived remainder — so a
  replayed signature covers a past amount and fails, because the record's
  `paid` advanced; and
- the payload's recipient must equal the call's recipient — the signature
  decides where value goes, not whoever submits the PTB.

The claim itself is a `bridge::ExitClaim` hot potato — no abilities, so it
cannot be dropped, copied, or stored: the PTB that mints it must hand it to
`bridge::escape_release`, which splits the vault's reserve and transfers to
the recipient, emitting `EscapeReleased`. The release deliberately does not
touch `withdraw_nonce` or the per-burn idempotency markers: escape releases
have no L2 nonce, and their dedup lives in the queue's `paid` accounting.

Accounting is cumulative per address. A redeem sets `paid` to the recorded
balance; a second redeem of the same record aborts with
`E_NOTHING_TO_REDEEM` (the remainder is zero) before any signature work.
Reproofs carry `paid` forward, so the cumulative payout never exceeds the
balance proven at some settled height — an earlier-height proof that shows a
larger balance re-opens the delta, a later one cannot regress the record.

Two limits are honest about what this path is. Payouts are first-come in
Sui transaction order, not `(height, seq)` order: enforcing the L2 order
would need an ordered index over the queue, and it would not change the
outcome that matters — the vault either covers everyone or was already
drained (the censoring-sequencer-drains-first residual risk). And the
remainder must fit the vault's u64 coin amounts (`E_AMOUNT` otherwise):
balances above that cannot arise from deposits made through this bridge.

Open: the gas number. Sui's gas model has no offline estimate, so the
walk's cost and the redemption path's (two `ecrecover`-adjacent natives per
signature check) are unmeasured until a testnet publish runs the full
`request_exit` → `redeem` → `escape_release` sequence against real proofs —
the same bench the rotation ceremony used. The loops are bounded by the
proof length, so nothing is unbounded, but the MIST number is the evidence
the gas question needs, and it is the next slice.

## The unlock condition

The escape path opens only when the chain is in the halt its own rules
produce. `exit_queue::halt_expired_deadline` reads the inbox and Sui's shared
`Clock` — nothing else:

```move
public fun halt_expired_deadline(inbox: &Inbox, clock: &Clock): Option<u64>
```

It returns the expired oldest deadline once the oldest pending forced
transaction has been past its deadline for more than `halt_margin_ms()`
(10 minutes), and `none` otherwise. That is the same chain data the full-node
deadline monitor reads, so anyone can evaluate the predicate in their own PTB;
a censoring sequencer that refuses withdrawals drives the inbox past its
deadline, every settle aborts at the inbox gate, and the verdict stands until
the overdue transaction is included — settling a block that covers it prunes
the entry and clears the verdict.

The margin is 10 minutes where the monitor's is 1 second, because the two
close different gaps. The monitor re-reads the inbox to confirm a verdict
against a settle that raced it; a PTB sees one execution snapshot and cannot
re-read, so the on-chain gate pays in margin instead. The false positive it
prevents: a transaction included on time still sits unpruned on Sui until its
block settles — the DA batch flush alone can hold it for two minutes — and
during that lag the inbox looks halted to a single snapshot. Ten minutes
dominates any legitimate settle lag, so by the time the gate opens, an
on-time inclusion has long since settled and pruned. Unlocking late is the
safe direction; unlocking early would open exits on a healthy chain.

## Requesting an exit

`exit_queue::request_exit` is the escape entrypoint: it aborts with
`E_NOT_HALTED` unless the unlock condition holds, verifies the account proof
against the root anchored at `height`, and records the result in the shared
`ExitQueue` object the deploy ceremony creates (existing deployments call
`exit_queue::initialize` in a standalone PTB; the queue is a singleton by
convention, like the Inbox):

```move
public fun request_exit(
    queue: &mut ExitQueue,
    registry: &Registry,
    inbox: &Inbox,
    clock: &Clock,
    height: u64,
    address: vector<u8>,
    proof: vector<vector<u8>>,
)
```

One request per L2 address, keyed by the 20 bytes. A first proof claims
`(height, seq)` — the height that produced the balance, plus a sequence for
total order within a height; redemption consumes the queue in that order. A
later proof at an earlier height replaces the record and keeps the sequence;
at the same height it re-records the same account (a trie is deterministic,
so the values cannot differ); at a later height it is ignored. The rule is
load-bearing because proofs are public chain data: anyone can produce a valid
proof at any later height, and the rule makes that useless — a queue position
can only improve, never be forced backward. Proofs that would regress are
silent no-ops, with no event, because the state did not change.

## How the exit path completes

The remaining pieces, against the anchor, verifier, unlock, and record that
now exist:

1. A user's L2 balance is included in the state of some settled height H,
   and anyone can prove it with `verify_account` against the root anchored
   at H.
2. The unlock condition is chain data: the inbox-derived verdict above, with
   the settlement contract's own deadline gate as the reason it holds.
3. Exit requests take a queue position from the L2 height that produced the
   balance, so reordering exits is as impossible as reordering blocks.
4. Redemption consumes the record: the owner signs the payout, the module
   recovers the signer and pays the remainder out of the vault — without
   the sequencer.

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
