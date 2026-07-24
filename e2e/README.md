# E2E tests

Two test suites, with different scope and infrastructure requirements.

## What is covered

| Test                 | What it verifies                                                                                                                                                                                   | Stack                                    |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------- |
| `engine_integration` | `Engine::build` / `accept` / `finalize` advance the head on a real Reth node. Fast, deterministic.                                                                                                 | Reth only                                |
| `full_stack`         | The real `podseq` binary produces blocks, user txs land on Reth, settlement advances on Sui, Walrus blobs decode, and the bridge relays both directions (deposit→mint, burn→withdraw). End-to-end. | Reth + podseq, public Sui/Walrus testnet |

## Scope and limitations

### `engine_integration`

Brings up Reth in a container and drives the Engine API directly via
`podseq-engine`. No funded key needed; hermetic and deterministic.

### `full_stack`

Runs the production `podseq` binary in a second container alongside Reth, with
Walrus and Sui pointed at public testnet. Requires a funded Sui key for
settlement gas. Without one it fails (not skips) so a missing CI secret
surfaces immediately.

- **CI**: set the `SUI_SIGNER_KEY` secret to a `suiprivkey...` string. The job
  runs on every PR; it is allowed to fail (`continue-on-error`) so public-
  testnet flakiness doesn't block merges.
- **Local**: drop the funded key into `docker/secrets/sui.key` (gitignored).

The test is slow (several minutes — real Sui checkpoint latency + Walrus
publication + bridge relay cycles).

The bridge section runs after settlement is confirmed, reusing the same
deployed settlement package (so SUI gas is spent once, not twice). It generates
a fresh relayer EVM key, funds it from the genesis account, calls
`Bridge.initialize` on the L2 predeploy, then verifies both directions:

- Sui `deposit<SUI>` → relayer mints the bridged ERC20 on L2.
- L2 `initiateWithdrawal` → relayer releases the locked SUI on Sui.

## Layout

```text
e2e/
├── Cargo.toml
├── lib.rs                       # Stack (Reth only) + FullStack (Reth + podseq) harnesses
└── tests/
    ├── engine_integration.rs
    └── full_stack.rs
```

## Running locally

Requirements: Docker with the Compose v2 plugin.

```sh
# Fast, deterministic — no funded key required.
cargo test -p podseq-e2e --test engine_integration -- --test-threads=1 --nocapture

# Full stack (includes bridge) — requires docker/secrets/sui.key (or SUI_SIGNER_KEY in env).
cargo test -p podseq-e2e --test full_stack -- --test-threads=1 --nocapture
```

Each test binds fixed host ports (`18745`/`18751` and `18545`/`18551`), so they
must run serially (`--test-threads=1`).

## CI

The `e2e (engine only)` job runs the deterministic test on every PR. The
`e2e (full stack)` job also runs on every PR and fails if `SUI_SIGNER_KEY` is
unset; it runs the full-stack test (which includes bridge assertions) and is
allowed to fail so public-testnet flakiness doesn't block PRs. A daily scheduled
run re-runs it as blocking to catch silent regressions on `main`. See
`.github/workflows/ci.yml`.
