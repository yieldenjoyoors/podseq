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

The proof anchor is implemented. The proof verifier is not. This page records the
design and what exists so far; the verification side is the next slice.

Implemented: `settlement::commit_state_root` anchors each settled block's state
root in the Registry (see below). Not implemented: Merkle-proof verification in
Move, the unlock condition, the ordered queue on the bridge.

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

## How the exit path will verify balances

The planned mechanism, against the anchor that now exists:

1. A user's L2 balance is included in the state of some settled height H.
2. The bridge's escape entrypoint takes a Merkle (MPT) proof of that balance
   and verifies it against the root anchored at H. Verification means an RLP
   decode, keccak hashing, and a trie walk in Move. Whether that stays within
   reasonable gas is the open R&D question for the next slice; Move has a
   native keccak, but the proof walk is the part that has never been priced
   for this contract set.
3. The entrypoint unlocks only when the inbox-derived halt condition holds:
   the oldest pending forced transaction is past its deadline while settlement
   does not advance. That is the same chain data the K3 deadline monitor
   reads, so the unlock is verifiable without trusting any sequencer endpoint.
4. Exit requests take a queue position from the L2 height that produced the
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
