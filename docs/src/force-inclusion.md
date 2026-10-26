# Force inclusion

Force inclusion gives every user an escape hatch from a censoring sequencer. A user
who cannot get their L2 transaction included through the normal path submits it to a
shared inbox contract on Sui. The sequencer must then include the transaction in an L2
block before a deadline, or the chain halts: the settlement contract refuses further
`settle` calls until the transaction is included.

The inbox lives in `move/sources/inbox.move`, next to `settlement.move`. This document
records the design decisions behind it.

## Mechanism

1. **Submit.** Anyone calls `inbox::submit` with raw EVM transaction bytes. The inbox
   assigns the transaction a strictly increasing nonce and stores it with a deadline:
   `deadline_ms = clock.timestamp_ms() + INCLUSION_WINDOW_MS`. Sui's shared `Clock`
   provides the timestamp, so the deadline is consensus truth and not a
   client-supplied value.
2. **Include.** The sequencer polls the inbox (the Sui gRPC read API has no event
   query, so it reads nonces the same way the bridge relayer reads deposits: by
   fetching each nonce's dynamic-field object). It submits the transaction bytes to
   Reth's transaction pool and tracks which built block contains them.
3. **Settle.** Every `settle` call now carries two extra values: the timestamp of the
   block being settled, and an inbox cursor, the count of inbox transactions
   included in settled blocks. The contract advances the cursor, removing covered
   entries, and then checks the deadline of the oldest remaining entry.
4. **Halt.** If the oldest pending entry's deadline is older than the settled block's
   timestamp, `settle` aborts with `E_DEADLINE_EXCEEDED`. No state change lands, so
   the abort repeats on every retry until the transaction is included: the halt is
   not a one-shot flag but the persistent shape of the state until inclusion
   happens.

## Design decisions

### Deadlines anchor on block timestamps, not settle-time wall clock

The naive check compares the deadline against the wall clock when `settle` runs. That
check fails in a specific, realistic scenario: a Walrus outage. Blocks keep building
during a DA outage (production never blocks on DA), and forced transactions are
included in those blocks on time. When Walrus recovers, the backlog settles minutes
or hours after the deadline. A wall-clock check would halt the chain for transactions
that were included on time.

So the check compares `deadline_ms` against the **timestamp of the block being
settled**, which the sequencer passes to `settle`. A transaction included in any
block whose timestamp precedes its deadline is on time, no matter when that block
settles.

Both sides of that comparison are epoch milliseconds. The L2 header carries unix
seconds, so the sequencer converts before building the settle call; raw seconds
would sit under every millisecond deadline and the abort would never fire.

The sequencer supplies that timestamp, and Move cannot read Walrus to verify it. The
mitigation is attribution: the timestamp is verifiable against the block header in
the blob that `settle` commits to, and a mismatch is an equivocation signed by the
settlement key. The contract also enforces that timestamps never regress
(`last_settled_ts` on the inbox), so a sequencer cannot rewind time with each call.

### Late inclusion self-heals

`settle` advances the cursor before checking deadlines. An expired entry that the
cursor covers is removed first and never triggers a halt. This is deliberate. The
purpose of force inclusion is that the transaction gets in, not that the sequencer is
punished; a user whose transaction lands one batch late is served. The halt exists
for transactions that are never included.

A side effect is that a "halt" is really a settlement pause: the sequencer can
always end it by including the overdue transaction. Blocks continue to build in the
meantime, and full nodes observe the settlement stall directly on chain.

### Halt state is derived, not stored

An aborting Sui transaction has no effects, so a `halted` flag written in the
same transaction that aborts would be rolled back with it. Instead, halt is the
derived state "the oldest pending entry is expired". Deadlines never change,
block timestamps never regress, and an entry can only leave the table via the
cursor, so the condition is naturally sticky. The inbox exposes it directly:

```move
public fun oldest_pending_deadline(inbox: &Inbox): Option<u64>
```

returns the deadline of the oldest pending entry, or `none` when the inbox is
empty. On chain, a halt is visible as settlement stalls and as `settle`
transactions reverting with `E_DEADLINE_EXCEEDED`; the expired entry itself
stays readable in the inbox.

### One-way module dependency

`settlement.move` calls into `inbox.move` (`enforce_settle` advances the cursor and
checks deadlines as part of `settle`). `inbox.move` depends only on `sui::clock` and
`sui::table`. Anchoring submit-time deadlines on the settlement registry instead
would create a module cycle, which Move forbids, and anchoring on block heights
would require reading the registry from `submit` for the same result.

### The deadline check is O(1)

Nonces are assigned in submission order and deadlines are computed from a monotonic
clock, so deadlines are non-decreasing in nonce order. Only the oldest pending entry
can be expired. The check borrows exactly one table entry.

### Window sizing

`INCLUSION_WINDOW_MS` is 600000 (10 minutes). At the default 2 s block time that is
roughly a 300-block budget. The window must absorb the sequencer's poll interval,
block build time, and DA batch lag, which under low traffic is bounded by the batch
flush timeout, not by byte thresholds. A window too tight halts the chain under
load; too wide weakens the escape hatch. 10 minutes keeps halts rare while capping
censorship at 10 minutes per transaction.

## Objects and API

| Object  | Type   | Purpose                                                         |
| ------- | ------ | --------------------------------------------------------------- |
| `Inbox` | Shared | Nonce table of pending transactions, cursor, `last_settled_ts`. |

`inbox.move` has no capability object: `submit` is permissionless, and the cursor
only moves inside `settle`, which is already gated by `SettlerCap`.

```move
public fun submit(inbox: &mut Inbox, clock: &sui::clock::Clock, tx_bytes: vector<u8>)
public fun next_nonce(inbox: &Inbox): u64
public fun included_count(inbox: &Inbox): u64
public fun last_settled_ts(inbox: &Inbox): u64
public fun pending_at(inbox: &Inbox, nonce: u64): &PendingTx
public fun oldest_pending_deadline(inbox: &Inbox): Option<u64>
```

`settlement::settle` grows three parameters:

```move
public fun settle(
    _cap: &SettlerCap,
    registry: &mut Registry,
    inbox: &mut Inbox,
    blob_id: vector<u8>,
    height: u64,
    block_ts: u64,
    inbox_cursor: u64,
)
```

The idempotent re-submit path (same height, same blob) returns before touching the
inbox, so crash retries of an already-settled block can never trip the deadline
check.

## Sequencer responsibilities

- Poll the inbox for nonces above the local cursor and submit them to Reth's pool.
- Track which built block contains each forced transaction.
- Persist the inclusion cursor before the `settle` call that carries it, mirroring
  the pending-block store. A crash between inclusion and settle then re-derives the
  cursor from local block data, and the idempotent `settle` re-submit lands the
  same state.

## Full node verification

A full node verifies the halt from chain data alone: read `Inbox` (cursor,
oldest pending deadline), read `Registry` for the settled height, and compare
the deadline against the node's own clock with a margin. An expired oldest
deadline combined with settlement not advancing means the chain is halted:
either the sequencer stopped settling, or its `settle` calls are reverting with
`E_DEADLINE_EXCEEDED`. Both are observable without trusting any sequencer
endpoint. If settlement keeps advancing while a pending entry's deadline is
long past, the block timestamps committed in the Walrus blobs disagree with the
values passed to `settle`, which is an attributable equivocation.

## Open work

- The sequencer needs a time-based batch flush so settlement lag stays bounded under
  low traffic; byte thresholds alone can hold a batch indefinitely.
- The exit queue (see [roadmap](./roadmap.md)) composes with the inbox: a halted
  settlement should unlock forced exits so users can leave during a halt.
- The window constant is fixed per package version. Making it adjustable needs an
  admin-gated field plus a timelock, which is the same machinery as the hot-swap key
  rotation planned for `SettlerCap` and `BridgeCap`.
