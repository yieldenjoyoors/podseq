# E2E tests

Three test suites, with different scope and infrastructure requirements.

## What is covered

| Test                 | What it verifies                                                                                                                                                                                                                                                                                                          | Stack                                    |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------- |
| `engine_integration` | `Engine::build` / `accept` / `finalize` advance the head on a real Reth node. Fast, deterministic.                                                                                                                                                                                                                        | Reth only                                |
| `bridge_bootstrap`   | The genesis predeploys (`BridgeFactory` at `0x…0010`, canonical SUI `Bridge` at `0x…0011`), the one-time bootstrap (initialize ×2 + `adoptBridge`), permissionless `createBridge` with duplicate rejection, mint nonce replay protection, burns, and the `BridgeCreated`/`WithdrawalInitiated` logs the relayer consumes. | Reth only                                |
| `full_stack`         | The real `podseq` binary produces blocks, user txs land on Reth, settlement advances on Sui, Walrus blobs decode, and the bridge relays both directions (deposit→mint, burn→withdraw). End-to-end.                                                                                                                        | Reth + podseq + Walrus daemon, public Sui testnet |

## Scope and limitations

### `engine_integration`

Brings up Reth in a container and drives the Engine API directly via
`podseq-engine`. No funded key needed; hermetic and deterministic.

### `bridge_bootstrap`

Same hermetic setup as `engine_integration`, plus a background Engine driver
that produces blocks so transactions confirm. Exercises the full L2 side of
the bridge against the real genesis predeploys — no Sui, no funded key.

### `full_stack`

Runs the production `podseq` binary in a second container alongside Reth and a
self-hosted Walrus daemon, with Sui pointed at public testnet. Requires a
funded Sui key for settlement gas. Without one it fails (not skips) so a
missing CI secret surfaces immediately.

- **CI**: set the `SUI_SIGNER_KEY` secret to a `suiprivkey...` string. The job
  runs on every PR; it is allowed to fail (`continue-on-error`) so public-
  testnet flakiness doesn't block merges.
- **Local**: drop the funded key into `docker/secrets/sui.key` (gitignored).

The test is slow (several minutes — real Sui checkpoint latency + Walrus
publication + bridge relay cycles).

### Self-hosted Walrus daemon

The full-stack stack runs its own `walrus daemon` (combined aggregator +
publisher, image `mysten/walrus-service:testnet`) instead of posting to the
public testnet publishers. The public daemons' WAL wallets drain between
operator refills, which fails every block POST with `500 could not find WAL
coins` — a third-party wallet we cannot refill. Our daemon pays for its own
stores from a wallet the test funds automatically at bring-up:

- a fresh wallet key per run (`walrus/sui_config/sui.keystore` in the workdir,
  the standard `FileBasedKeystore` format behind `SUI_CONFIG_DIR`);
- SUI topped to the funding targets through the release-profile
  `faucet-refill` helper (one PoW payout per round, so the grind outruns the
  checkpoint freshness window);
- WAL converted from that SUI by our own exchange-swap PTB
  (`e2e/walrus.rs::append_exchange_swap`): `SplitCoins(Gas)` materializes a
  SUI coin from the address balance, `wal_exchange::exchange_all_for_wal`
  swaps it 1:1, and the returned WAL coin transfers back to the wallet. The
  official `walrus get-wal` cannot be used here: the faucet pays into the
  address BALANCE (the accumulator gas model), and the walrus client's coin
  selection enumerates coin objects, so get-wal fails with `could not find
  SUI coins` on a funded wallet (live-probed 2026-11-26);
- the daemon's internal sub-wallets refilled from the main wallet at startup
  and on low balance (`--n-clients 1`, tuned-down refill knobs).

Sizing: Walrus bills the encoded size of a blob (erasure coding plus a fixed
64 MB of metadata per blob), and `walrus info` on current testnet prices a
store's metadata at 0.0003 WAL — a run's ~30 stores cost well under 0.1 WAL.
The targets live in `e2e/walrus.rs::FundingTargets` (2 WAL + a 4 SUI gas
floor per run, 6 faucet rounds); the WAL target must stay at or above the
daemon's `--wal-refill-amount` (2 WAL).

Build the refill helper before running locally:
`cargo build --release -p podseq-e2e --bin faucet-refill`.

The bridge section runs after settlement is confirmed, reusing the same
deployed settlement package (so SUI gas is spent once, not twice). It generates
a fresh relayer EVM key, funds it from the genesis account, bootstraps the L2
bridge (factory `initialize`, predeployed token `initialize`, `adoptBridge`),
then verifies both directions:

- Sui `deposit<SUI>` → relayer mints the bridged ERC20 on L2.
- L2 `initiateWithdrawal` → relayer releases the locked SUI on Sui.

## Layout

```text
e2e/
├── Cargo.toml
├── lib.rs                       # Stack (Reth only) + FullStack (Reth + podseq) harnesses
└── tests/
    ├── engine_integration.rs
    ├── bridge_bootstrap.rs
    └── full_stack.rs
```

## Running locally

Requirements: Docker with the Compose v2 plugin.

```sh
# Fast, deterministic — no funded key required.
cargo test -p podseq-e2e --test engine_integration -- --test-threads=1 --nocapture
cargo test -p podseq-e2e --test bridge_bootstrap -- --test-threads=1 --nocapture

# Full stack: requires docker/secrets/sui.key (or SUI_SIGNER_KEY in env).
cargo test -p podseq-e2e --test full_stack -- --test-threads=1 --nocapture
```

Each test binds fixed host ports (`18745`/`18751`, `18845`/`18851`, and
`18545`/`18551`), so they must run serially (`--test-threads=1`); the
full-stack Walrus daemon publishes on `18761`.

## CI

The `e2e (engine only)` job runs the deterministic test on every PR. The
`e2e (full stack)` job also runs on every PR and fails if `SUI_SIGNER_KEY` is
unset; it runs the full-stack test (which includes bridge assertions) and is
allowed to fail so public-testnet flakiness doesn't block PRs. A daily scheduled
run re-runs it as blocking to catch silent regressions on `main`. See
`.github/workflows/ci.yml`.

## Startup evidence

Every `FullStack::start` measures its phases (wallet prep, client-config
fetch, faucet funding, compose up, Reth RPC/engine readiness, Walrus daemon
readiness) and writes them to `e2e-logs/<project>/phase-timing.txt` next to
this crate (override the directory with `PODSEQ_E2E_LOG_DIR`). On any startup
failure the compose logs join them as `compose-logs.txt`. The CI job uploads
the directory as the `e2e-fullstack-evidence` artifact on every run, so a red
or timed-out job is diagnosable from the artifact even when the job log is not
readable.
