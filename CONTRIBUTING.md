# Contributing

Contributions are welcome. This guide keeps the bar short and consistent with how the
project already works.

## Before you start

Open an issue first for anything beyond a typo or small fix. This avoids duplicate work
and lets us agree on the approach before you spend time on code.

## Development setup

Requirements: a recent Rust toolchain (pinned via [`rust-toolchain.toml`](./rust-toolchain.toml)).
For the docs site you also need `bun` (or `npm`) under [`web/`](./web).

```sh
cargo build --release          # produces target/release/podseq
```

See the [development guide](https://podseq.xyz/#/docs~development) for node modes,
config, and running locally.

## Code style

- Idiomatic Rust only. Prefer the standard library and existing dependencies over new
  ones: justify any new dependency in the PR description.
- Keep functions focused and single-purpose. Prefer early returns and guard clauses over
  nesting.
- No premature abstraction. If a pattern is used once, inline it.
- Document exported items with `///`. Reserve mid-function comments for non-obvious
  behavior or side effects.

## Tests

Write tests first: define the behavior before the implementation. Add positive,
negative, and edge cases. Unit tests live inline under `#[cfg(test)] mod tests`.

Before pushing:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

End-to-end tests run against a real Reth container (see [`e2e/README.md`](./e2e/README.md))
and are not expected to run on every commit:

```sh
cargo test -p podseq-e2e -- --test-threads=1 --nocapture
```

## CRAP regression

Every PR is checked against a committed CRAP baseline
([`crap_baseline.json`](./crap_baseline.json)) and fails if any function's CRAP
score went up. Run the same check locally:

```sh
make crap-regression
```

Regenerate the baseline only when a PR improves scores (new tests,
simplifications) or adds functions worth tracking. Treat it like a snapshot
test: update it deliberately, in the same PR, not on a schedule.

```sh
make crap-baseline   # review and commit crap_baseline.json
```

Paths in the baseline are relative to the repo root. Do not pass `--workspace`
to `cargo crap` when regenerating it: that emits absolute paths and leaks the
local filesystem layout into the committed file.

## Documentation

User-facing docs live in [`docs/src/`](./docs/src/) and are rendered at
[podseq.xyz/#/docs](https://podseq.xyz/#/docs). If your change affects behavior,
config, or architecture, update the relevant doc page in the same PR. No separate build
step: the site picks up edits on next reload.

## Commits and pull requests

- One logical change per PR. Keep commits atomic.
- Reference the issue in the PR description (e.g. `Closes #42`).
- Include enough context in the PR description for a reviewer who hasn't followed the
  issue. Call out security-relevant trade-offs explicitly.
- Don't edit generated files, lockfile churn unrelated to your change, or unrelated
  reformatting. Keep the diff scannable.

## Reporting security issues

Do **not** open a public issue for security vulnerabilities. Email at `security at podseq.xyz` so it can be triaged before disclosure.

## License

By contributing you agree your contributions are licensed under the
[Apache License 2.0](./LICENSE).
