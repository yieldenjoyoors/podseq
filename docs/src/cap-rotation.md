# Cap rotation

The sequencer's authority over settlement and the bridge lives in two owned Sui
objects: `SettlerCap` (settle calls, `move/sources/settlement.move`) and `BridgeCap`
(release calls, `move/sources/bridge.move`). Both are transferred to the operator at
initialization, and both say "transferable for sequencer rotation" — but no rotation
mechanism exists. A raw `sui::transfer::transfer` is the only primitive available, and
it is exactly wrong: it is immediate, it cannot be taken back, and it invalidates every
settle or withdraw transaction the old wallet still has in flight.

This document records the research behind the rotation design, the alternatives, and the
chosen mechanism: the caps are entombed inside the shared objects they guard, authority
becomes module state checked against the transaction sender, and rotation is a
two-step, timelocked state transition with a grace window for the previous owner. The
node swaps its signing wallet at the confirmed transition without stopping block
production or the relayer, and derives its role from chain state after a crash.

## Background: how other systems handle this

### EVM rollups: ownership is a storage slot

On Ethereum, operator authority is contract state. `Ownable` keeps an `owner` address
slot and `onlyOwner` compares it against `msg.sender` at every call;
[`Ownable2Step`](https://github.com/OpenZeppelin/openzeppelin-contracts/blob/master/contracts/access/Ownable2Step.sol)
adds the two-step handshake — the current owner sets `_pendingOwner`, and **the new
owner** completes with `acceptOwnership()`, so a typo or a lost key can never strand the
contract ([OpenZeppelin access control
docs](https://docs.openzeppelin.com/contracts/5.x/access-control)). Because ownership is
a plain slot, a stack of standard patterns composes on top: governance timelocks delay
and stage every privileged action, and upgrade proxies carry their own admin chains.

OP Stack chains stack these: the [fault proof
rollout](https://optimism.io/blog/the-fault-proof-system-is-available-for-the-op-stack)
moved withdrawal liveness off the privileged proposer and under a Security Council
backstop, and [protocol upgrade
#7](https://gov.optimism.io/t/final-protocol-upgrade-7-fault-proofs/8161/1) restructured
exactly these ownership edges for Stage 1 — the L1 ProxyAdmin became a 2-of-2, and the
`setResourceConfig` setter was removed so the System Config Owner could no longer
misconfigure the [forced-inclusion
path](https://docs.optimism.io/op-stack/fault-proofs/explainer) by shrinking its gas
budget. That forced-inclusion path itself — users submitting to the `OptimismPortal` on
L1, included by the sequencer within roughly half a day — is the user-facing escape
hatch. Arbitrum's variant is the Delayed Inbox: after `delayBlocks` (7200 ≈ 24 h,
bounded by a adaptive `delayBuffer`) anyone can call
[`forceInclusion`](https://docs.arbitrum.io/how-arbitrum-works/deep-dives/forced-transactions)
to promote a held message into the sequencer inbox, with a [Censorship
Timeout](https://docs.arbitrum.io/how-arbitrum-works/deep-dives/sequencer) shortening
the wait under sequencer outage
([Nitro whitepaper](https://docs.arbitrum.io/nitro-whitepaper.pdf)). At the protocol
layer, [EIP-7547 inclusion
lists](https://eips.ethereum.org/EIPS/eip-7547) attempted consensus-level forced
inclusion and stalled in draft; [EIP-4844](https://ethereum.org/roadmap/danksharding/)
is the data-availability half (blobs, ~18-day retention) — the role Walrus plays here.

The relevant observation is not the escape hatches (Podseq's counterpart is the
force-inclusion inbox from the settlement design) but what the EVM makes easy: rotating
the operator key is writing a new value into a slot, the new owner can always complete
the handshake, and an in-flight transaction that loses the race simply reverts on the
`msg.sender` check — graceful, visible, harmless.

### Sui: ownership is the protocol

Sui has no such slot. `SettlerCap` and `BridgeCap` are
[address-owned objects](https://docs.sui.io/develop/objects/object-ownership/address-owned):
"an address-owned object is only accessible to its owner … other addresses cannot
access owned objects in any way." The [`sui::transfer`
module](https://docs.sui.io/references/framework/sui-framework/transfer) is the only way
to change that, and it is unconditional — there is no pending-owner handshake to copy.
[Sponsored transactions](https://docs.sui.io/develop/transaction-payment/sponsor-txn)
change who pays gas, not who owns inputs, so they do not help a new owner claim a
capability it does not hold.

Existing Sui systems pick one of two poles, and neither rotates hot:

- **Single static cap.** The common vault/escrow shape — the one Podseq's contracts
  currently use — mints an admin cap, transfers it once, and never moves it again.
  Rotation is either absent or a raw transfer with every failure mode listed above.
- **Validator committee.** [Sui
  Bridge](https://github.com/MystenLabs/sui/issues/14983) escrows assets behind the
  active validator set ([`bridge::committee`
  module](https://docs.sui.io/references/framework/sui_bridge/committee)): ⅓-stake
  signature thresholds authenticate transfers, and membership "is refreshed every epoch
  along with Sui epoch change" — validators rotate keys, but rotation lands only at
  epoch boundaries, never mid-stream.

Between "static cap" and "epoch-frozen committee" there is no hot-swap pattern to copy.
That gap is the problem this design fills.

## Two protocol facts shape the design

**1. Input authorization is checked against the sender at execution time.**
A transaction may pass an owned object as input only if the sender owns that object when
the transaction executes. Two consequences:

- A `next_owner` field *on the cap* is unclaimable. The new owner cannot call
  `claim(&mut cap)` — they cannot pass `cap` as input at all. The EVM handshake maps to
  Sui only if ownership stops being an object property.
- A `transfer` invalidates in-flight work. Any settle/withdraw transaction the old
  wallet signed but has not yet landed fails its input checks once the transfer
  executes, because the same object is now owned by someone else. Gas is spent, no
  effect lands, and if the transfer was the *goal*, the failure window is unavoidable
  rather than exceptional.

**2. Package upgrades freeze shapes, not bodies.** The
[upgrade compatibility rules](https://docs.sui.io/develop/publish-upgrade-packages/upgrade)
require existing struct layouts (and abilities) to stay identical, allow new structs and
functions, allow changing function implementations, and allow changing non-`public`
signatures. `settle` and `withdraw` are `public`, so their signatures — including the
capability parameters — are frozen forever. New state must therefore live in
[dynamic fields](https://docs.sui.io/develop/objects/dynamic-fields) on the shared
objects, and new behavior must be new functions, with the old ones retired by making
their required inputs impossible to provide.

## Alternatives

### A. Two-step handover of the cap object (the baseline, refined)

The plan's baseline — `next_owner` on the cap, admin pre-authorization, timelocked
claim — has to be reshaped to survive fact 1: the pending state moves onto the shared
`Registry`/`Vault`, and the *claim* becomes a gated `transfer` that the **old** owner
still signs, because only the old owner can pass the cap as input:

1. `request_rotation` (old owner): writes `pending = (to, eligible_at)`.
2. After the timelock, `handover(cap, registry)` (old owner): checks the pending state,
   then `transfer::transfer(cap, to)`.
3. The node watches for the transfer and swaps wallets.

- *Timelock*: a module constant (24 h). *Cancel*: old owner only. *Wallet switch*:
  quiesce — stop issuing, drain in-flight, transfer, resume on the new key. *In-flight*:
  anything signed before the transfer and landing after it **fails**; the quiesce window
  only shrinks the race, never closes it, because the node can crash mid-window.

Rejected. The new owner cannot complete the rotation (fact 1), so a lost or corrupted
old key between steps 1 and 2 freezes rotation mid-way. There is a hard interval —
transfer confirmed, wallet switch pending — where no key can settle, and a crash in it
is only survivable because force-inclusion deadlines turn the outage into a visible
halt rather than silent damage. "Never invalidate in-flight transactions" cannot be
met, only apologized for.

(A Sui-specific patch — transferring the cap to a
[`sui::party`](https://docs.sui.io/references/framework/sui-framework/transfer) so old
and new owners can both use it during the window — was considered and dropped: party
objects always take the consensus path, putting that penalty on every settle call in
the block-production critical path, for a mechanism that still ends in a raw
transfer.)

### B. Entombed capability, authority in shared-object state — chosen

The capability stops being the gate. Each cap is moved into a dynamic field of the very
shared object it guards (the *entombment*), which simultaneously kills the
object-ownership gate (nobody can pass the entombed object as input any more) and
preserves the object as an inert artifact. Authority becomes a dynamic-field record
checked against `tx_context::sender`, and rotation becomes a two-step state transition
on that record — the `Ownable2Step` handshake, rebuilt out of Sui primitives the EVM
does not have: dynamic fields for state, the Sui `Clock` for the timelock, and a
previous-owner grace window instead of a mempool to absorb in-flight work.

- *Timelock*: chosen at request time within module bounds (default 24 h). *Cancel*: the
  current authority may cancel or replace a pending request; the pending request also
  expires on its own if never claimed. *Wallet switch*: claim, then swap while both
  keys are accepted (grace window), then retire the old key. *In-flight*: never
  invalidated — old-signed transactions stay authorized through the grace window.

Why chosen: it is the only alternative where the **new** owner completes the rotation,
so the mechanism is safe against a lost old key; there is never an interval with zero
valid authorities; and in-flight invalidation is impossible by construction rather than
by careful timing.

### C. Committee or multisig custody

Make the authority a [native k-of-n multisig
address](https://docs.sui.io/guides/developer/cryptography/multisig) (weights +
threshold, ≤ 10 parties), or a Move-managed committee in the image of Sui Bridge.
Rotation becomes a membership change. Rejected for this stage: Podseq has exactly one
operator, so a threshold adds quorum-stall failure modes — the availability risk K4 is
trying not to worsen — while the in-flight problem during a membership change is the
same as Alternative A's. This is the natural end state when the operator set
decentralizes, and the chosen design's authority field makes it an address-level swap
away; nothing here blocks it.

### What varies across the alternatives

|                            | A. cap handover           | B. authority-in-state (chosen)     | C. committee / multisig         |
| -------------------------- | ------------------------- | ---------------------------------- | ------------------------------- |
| Who completes rotation     | old owner (forced)        | **new owner**                      | threshold of members            |
| Survives old-key loss      | no — stuck pending        | **yes**                            | yes, if quorum lives            |
| Interval with no authority | yes (transfer → switch)   | **never**                          | possible (quorum stall)         |
| In-flight old-signed txs   | invalidated by the race   | **valid through grace window**     | invalidated by membership change|
| Hot path cost              | unchanged                 | one dynamic-field read + compare   | unchanged or worse              |
| Migration                  | state fields only         | entombment (one transaction)       | re-key ceremony                 |
| Fits single-operator stage | partly                    | **yes**                            | no                              |

## Chosen design

### On-chain state

One `RotationState` per guarded object, stored as a dynamic field on `Registry.id` and
on `Vault.id` (keys are unit marker types, so no layout change is needed and the states
cannot collide with other dynamic fields):

```move
public struct RotationState has store {
    /// The address whose settle/withdraw calls are authorized right now.
    authority: address,
    /// Previous authority, still authorized until `prev_until_ms` (grace window).
    prev: Option<(address, u64)>,
    /// Pre-authorized next authority and the earliest claim time.
    pending: Option<PendingRotation>,
}

public struct PendingRotation has store {
    to: address,
    eligible_at_ms: u64,
}
```

Independent state per object, but the node always drives both through a single
programmable transaction block, so the two rotations cannot drift apart.

### New entrypoints

| Function (on `settlement` / `bridge`)  | Caller          | Checks                                                        |
| -------------------------------------- | --------------- | ------------------------------------------------------------- |
| `entomb_settler_cap(cap, registry, ctx)` / `entomb_bridge_cap(cap, vault, ctx)` | current cap owner | input ownership (last object-auth check ever); stores cap in a dynamic field, initializes `authority = sender` |
| `settle_as(registry, inbox, clock, …)` / `withdraw_as<T>(vault, clock, …)`      | authority (or prev, in grace) | `sender == authority`, else `prev` and `now <= prev_until_ms`; then the existing gate logic |
| `request_rotation(registry, to, delay_ms, ctx)` / same on `vault`               | authority       | `MIN_DELAY_MS <= delay_ms <= MAX_DELAY_MS`; overwrites any pending |
| `cancel_rotation(…)`                   | authority       | clears `pending`                                              |
| `claim_rotation(…)`                    | **pending target** | `sender == pending.to` and `eligible_at_ms <= now <= eligible_at_ms + CLAIM_WINDOW_MS` |
| `retire_previous(…)`                   | authority       | clears `prev` (one-way, after the swap is verified)           |

`claim_rotation` flips `authority` to the caller and sets
`prev = (old, now + PREV_GRACE_MS)` in the same transaction — the atomic cut-over. All
five transitions emit events (`RotationRequested`, `RotationCancelled`, `RotationClaimed`,
`PreviousRetired`, `CapEntombed`) so full nodes and indexers observe rotation from chain
data alone.

The frozen public entrypoints `settle`/`withdraw` keep their signatures; after
entombment they are unreachable (their capability input is owned by the shared object),
and their bodies are repointed to abort with `E_ROTATED` as defense in depth.

### Parameters

| Constant         | Value    | Rationale                                                                                              |
| ---------------- | -------- | ------------------------------------------------------------------------------------------------------ |
| `MIN_DELAY_MS`   | 10 min   | still catches a mis-typed target before anything irreversible; matches ops reaction time               |
| `MAX_DELAY_MS`   | 7 days   | upper bound for governance-style staging; Arbitrum's 24 h forced-inclusion delay is the reference point |
| default delay    | 24 h     | one full observation window at the default                                                             |
| `CLAIM_WINDOW_MS`| 7 days after `eligible_at_ms` | a stale pre-authorization expires instead of waiting forever for a claim           |
| `PREV_GRACE_MS`  | 10 min   | must exceed the worst in-flight lifetime: `COMMIT_TX_WAIT` (30 s) times the retry bound, with margin    |

During the grace window both addresses can settle and withdraw. `retire_previous` is
the node's deliberate "I am done with the old key" signal; `PREV_GRACE_MS` is only the
backstop if the node dies right after claiming.

## Atomicity argument

The rotation must (a) never invalidate in-flight transactions, (b) never race the old
owner, (c) never break settlement — and it must survive a node crash at any point.

**(a) In-flight transactions.** A settle or withdraw transaction signed by the old
wallet remains authorized as long as it executes before `prev_until_ms`. The node only
retires the previous authority after its in-flight set is empty, and `PREV_GRACE_MS`
(10 min) dwarfs the longest signing-to-finality lifetime in the system (30 s commit
wait, bounded retries). Even the pathological case — the node claims, then crashes
before swapping wallets — leaves both keys valid for ten minutes, which is recovery
time, not a race. The failure mode this eliminates is Alternative A's: there is no
single transaction after which previously-valid signatures stop working.

**(b) No race with the old owner.** After `claim_rotation` there is exactly one
authority, and only it can request, cancel, or replace a pending rotation. The old
owner cannot redirect a rotation to itself, cannot accelerate the timelock (the Sui
`Clock` anchors `eligible_at_ms` at request time, and claims before it abort), and
cannot revive an expired claim. The one dangerous power it keeps is cancellation —
which is the safe direction: a compromised old key can at worst stall rotation, visibly
and on-chain, until the operator re-requests. That residual risk is the same one
`Ownable2Step` accepts (the current owner can always replace the pending owner), and it
shuts with the hardening item below.

**(c) Settlement continuity.** Entombment initializes `authority = sender`, so from the
first rotation-state transaction onward there is no moment without a valid authority:
claim overlaps old and new by construction, an unclaimed pending request leaves the old
authority fully in charge, and an expired one is a no-op. The force-inclusion machinery
is untouched throughout: `settle_as` runs the same `inbox::enforce_settle` gate, so a
crash mid-rotation degrades to exactly the K2/K3 semantics — a visible settlement stall
that full nodes can already verify from chain state, now with the rotation state also
readable on chain to attribute *why* settlement stalled.

**Crash safety is derivation, not bookkeeping.** The node holds no local rotation
phase; on every boot it reads `RotationState` (for both objects) plus the `Clock` and
selects its role — mirroring the deadline monitor's read-from-chain-state design:

| Observed state                                  | Derived role                                                        |
| ----------------------------------------------- | -------------------------------------------------------------------- |
| `authority == old`, no pending                  | settle on the old key (pre-rotation normal)                          |
| `authority == old`, `pending.to == new`         | settle on the old key; at `eligible_at_ms` sign `claim_rotation` with the new key; swap on confirmation |
| `authority == new`, `prev` valid, now < `prev_until_ms` | settle on the new key; drain, then `retire_previous`         |
| `authority == new`, no `prev`                   | settled — rotation complete                                          |
| `authority` is neither key                      | misconfiguration: refuse to settle (fail closed, loudly)             |

Every step is an idempotent on-chain transition, so a crash at any point replays to the
same result.

## Node runtime

**Signer state today.** `crates/sui/src/settlement.rs` loads one `suiprivkey` file at
startup (`load_signer`) and derives the address; `crates/sui/src/lib.rs` holds the
signer as `Arc<Mutex<Settlement>>` with a lock-free address snapshot alongside; the
relayer (`crates/node/src/bridge.rs`) loads the `BridgeCap`-owning key path the same
way; `crates/node/src/keyring.rs` generates new keys.

**The change, in shape.** A rotation watcher task polls both `RotationState` records
over the same mockable `get_object` transport the inbox tracker and deadline monitor
use, runs the derived-role table above, and performs the wallet swap as an in-place
replacement of the signing key and address inside the shared signer — block production
and the relayer take the same lock on their next call and simply continue on the new
key. The address snapshot becomes shared, atomically swappable state (`RwLock` or an
arc-swap), since several client clones read it lock-free today. The relayer's EVM key
is out of scope: it is not a capability holder on Sui.

**Key ceremony.** `keyring generate` mints the new key to a staged path and the address
is topped up with SUI for gas; `request_rotation` runs in one PTB for both objects; at
`eligible_at_ms` the watcher signs the claim with the staged key; after the claim and a
drain check, `retire_previous` runs and the staged key file becomes the active
configuration. The old key file is archived and destroyed.

## Migration

1. Upgrade package: new module state and entrypoints land; nothing behavioral changes
   yet — `settle`/`withdraw` still work exactly as before.
2. One PTB, signed by the current cap owner: `entomb_settler_cap` +
   `entomb_bridge_cap`. Both caps move into their shared objects, `authority` is
   initialized to the same address that owned the caps, and the old entrypoints become
   unreachable (their input can no longer be legally provided by any sender). No gap,
   no behavior change from the outside.
3. From here the rotation machinery is live and the sequencer calls `settle_as` /
   `withdraw_as`.

Step 2 is deliberately a rotation of zero addresses: it proves the whole mechanism
(including a later claim against it) while changing nothing an attacker could exploit.

The node that performs the upgrade is also the cap owner and the settlement signer,
so it now runs step 2 itself. When a poll finds a shared object with no rotation
state and the matching cap id is configured (`sui.settler_cap_id`, `bridge.cap_id`),
it submits that object's entombment PTB signed with the current settlement key and
settles normally on the next poll. Without a configured cap id it fails closed with
a loud error, and an unreadable chain state never entombs (the ceremony moves an
owned object; a stale read must not move someone else's cap). Deployments whose
caps live in some other wallet still run step 2 by hand.

## Residual risks and follow-ups

- **Pre-claim compromise of the current key** can cancel a pending rotation and
  re-request one to an attacker address, claimable after its own timelock. The window
  is observable on chain. Hardening: a separate cold `OpsCap` required for
  `request_rotation`, so the hot sequencer key can never steer rotation.
- **`UpgradeCap` custody** is the true backstop above all of this (an upgrade can
  repoint anything); its custody and aging policy is its own hardening item.
- Fuzzing and gas measurement landed; see [Verification](#verification).

## Requirements mapping (K4)

| K4 requirement                                                     | Where it is met                                                        |
| ------------------------------------------------------------------ | ---------------------------------------------------------------------- |
| A swap must not invalidate in-flight settle/withdraw transactions  | grace window + drain-before-retire (atomicity a)                       |
| A swap must not race the old owner                                 | claim-only-by-target, clock-anchored timelock, cancel-only authority (atomicity b) |
| A swap must not break settlement                                   | never-zero-authorities invariant; claim overlaps old and new (atomicity c) |
| Node-runtime wallet switch without stopping production or relayer  | in-place signer swap under the existing lock; watcher drives it        |
| Crash mid-window resumes in the correct role                       | role derived from chain state; idempotent on-chain steps               |
| Independent verification without trusting the sequencer            | `RotationState` + events are plain shared-object state, readable like the inbox |

## Verification

Adversarial tests, fuzzing, and gas measurement against the implementation. The
closing question: whether a Cap swap can proceed atomically with an in-flight
settlement batch, without a race with the old owner. Answer: yes — every
interleaving of old-key and new-key calls inside the rotation window is either a
valid sequential history or a clean abort; nothing lands twice and nothing
invalid. Evidence below.

### Adversarial cases

Sui linearizes a shared object's transactions, so "concurrent" old-owner calls
mean interleaved ones; the tests pin every interleaving class. Sender-sensitive
calls bind a fresh tx context (see the Move test helper pattern).

| Case | Where pinned | Result |
| --- | --- | --- |
| The pre-authorized target acts before its claim | `rotation::the_preauthorized_target_cannot_act_without_claiming` | aborts `E_NOT_AUTHORIZED` — the staged key is nobody until the claim lands, so a half-rotated sequencer fails closed |
| The previous authority steers during its grace window | `rotation::the_previous_owner_cannot_steer_during_grace` | aborts — grace authorizes settle/withdraw, never `request_rotation`/`cancel_rotation`/`retire_previous`; the old key cannot pre-authorize itself a way back in |
| The previous authority retires itself mid-grace | `rotation::the_previous_owner_cannot_retire_itself_during_grace` | aborts — retiring is the current authority's one-way act |
| Retire vs. the grace clock | `rotation::retiring_the_previous_kills_its_window_immediately` | the old key is rejected immediately after the one-way retire; the window is not kept open by the clock |
| Settlement interleaved with the full rotation window | `settlement::settles_interleave_with_the_claim_and_retry_across_the_swap` | old-key settles run through pre-authorization and the timelock wait, both keys settle inside grace, a duplicate submission is an idempotent no-op across the swap, heights stay sequential, no settlement failure |
| Old-key equivocation after the swap | `settlement::the_previous_owner_cannot_rewrite_a_height_after_the_swap` | aborts `E_CONFLICTING_BLOB` — grace never licenses re-signing a settled height with a different blob |
| Half-rotation (Vault rotated, Registry not) | `bridge::the_vault_gate_rotates_independently_of_the_registry_gate` | the rotated key releases on the Vault while the stale key settles the Registry: each gate checks only its own record |
| The staged key settles an unrotated Registry | `bridge::the_new_authority_cannot_settle_while_the_registry_is_unrotated` | aborts `E_NOT_AUTHORIZED` — fail-closed per object |

Node-runtime crash drills (`drill_*` in `crates/sui/src/rotation.rs`) rebuild the
watcher from chain state alone on every poll — exactly what a restarted node
sees — and drive `plan` → `apply` to convergence:

| Crash boundary | First step after restart | Converges to |
| --- | --- | --- |
| Before the claim | `claim` | `claim → adopt → retire`, both objects on the staged key, prev records cleared |
| After the claim, before the swap | `adopt` (never `retire`) | `adopt → retire`; the signers are swapped before the old window closes |
| After the swap, before the retire | `retire` | the guard's adoption marker carries; no repeated adopt |
| Single-object claim (adversarial half-state) | refuse (`Misconfigured`) | no steps execute, chain state untouched — the node never half-adopts |
| Node down through the whole grace window | `adopt` then `retire` | converges anyway; the stale prev record is cleared on the next poll |

The watcher seam regression: an adopt that lands before the relayer's setup
completes is recorded in the shared `AdoptedKey` holder, and every signer source
prefers the holder over the key file — a relayer that comes up after a swap can
no longer resurrect the previous key (`an_adopt_before_the_relayer_is_up_records_the_key_for_late_setup`,
`a_relayer_starting_after_an_adopt_signs_with_the_adopted_key`).

### Fuzzing

`cargo fuzz` (libFuzzer + ASan, nightly toolchain; targets in `fuzz/`), fixed
budget of 120 s per target:

| Target | Execs | Crashes | What it covers |
| --- | --- | --- | --- |
| `rotation_state_machine` | 5,073,343 | 0 | BCS `parse_rotation_state` never panics on arbitrary bytes; `derive_role`/`plan` agree with the role table (restated as an oracle) over adversarial timestamps (0, `u64::MAX`, ±window/±grace offsets) and arbitrary authority records |
| `settlement_deadline` | 8,218,185 | 0 | `classify` matches the documented boundary (`now > deadline + margin`, equality active), the Active/Halted/Violated split over `last_settled_ts`, and `parse_clock` never panics — the oracle computes the boundary independently, so an arithmetic regression (plain `+` overflow, `<` instead of `<=`) fails the run |

The one crash the campaign surfaced was in the harness itself (a length guard in
the rotation target read past short inputs), fixed before the recorded runs.

### Gas (Sui testnet, live execution)

Measured by executing the node's real PTBs against testnet
(`crates/sui/examples/rotation_gas_bench.rs`; digests in
`bench_state.gas.csv` next to the run state). MIST, 1 SUI = 10⁹ MIST;
net = computation + storage − rebate.

| Transaction | Computation | Storage | Rebate | Net |
| --- | --- | --- | --- | --- |
| `settle_as` (baseline, one block anchor) | 1,000,000 | 4,902,000 | 3,450,055 | **2,451,945** |
| `request_rotation` (both objects, one PTB) | 1,000,000 | 9,462,000 | 8,853,115 | **1,608,885** |
| `claim_rotation` (both objects, one PTB) | 1,000,000 | 9,462,000 | 10,068,993 | **393,007** |
| `retire_previous` (both objects, one PTB) | 1,000,000 | 8,854,000 | 9,461,054 | **392,946** |
| `settle_as` on the new authority (post-swap) | 1,000,000 | 4,902,000 | 3,450,055 | **2,451,945** |
| `entomb_settler_cap` (one-time migration) | 1,000,000 | 6,513,200 | 3,039,696 | **4,473,504** |
| `entomb_bridge_cap` (one-time migration) | 1,000,000 | 7,090,800 | 3,617,238 | **4,473,562** |

Reading the table:

- Computation is flat at the 1 M MIST floor for every call — the Move logic
  (deadline check, sender check, state transition) is noise next to storage.
- Storage is deposited for the records a transaction creates and refunded when
  a later transaction removes them: the claim *deletes* the pending records, so
  its rebate exceeds its storage and its net cost (393 k MIST ≈ 0.0004 SUI) is
  the cheapest step in the whole protocol. A full rotation (request + claim +
  retire) nets ≈ 2.4 M MIST — about the price of a single block-anchor settle.
- Settlement is reproducible to the MIST across payers and heights (the two
  `settle_as` rows are byte-identical), so the numbers above are stable
  references, not samples.
- The plan's first risk — whether Move + Sui gas economics can express
  deadline/halt/rotation efficiently — resolves to yes: deadline and authority
  checks add nothing measurable to computation, rotation state lives in
  existing shared objects (no new object churn), and the timelock bookkeeping
  pays for itself by being deleted at the claim.
