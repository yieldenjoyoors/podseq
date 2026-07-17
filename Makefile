.PHONY: help install-tools build release fmt fmt-check clippy test \
        e2e e2e-engine e2e-full move docker \
        coverage crap crap-baseline crap-regression ci clean

# Workspace excludes the e2e crate from unit-test / coverage runs:
# it brings up its own Reth container via Docker Compose.
EXCLUDE  := --exclude podseq-e2e
LCOV     := lcov.info
BASELINE := crap_baseline.json
E2E_FLAGS := --no-fail-fast -- --test-threads=1 --nocapture

help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"} /^[a-zA-Z_-]+:.*##/ {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST) | sort

install-tools: ## Install cargo-llvm-cov and cargo-crap (skipped if present)
	@command -v cargo-llvm-cov >/dev/null || cargo install cargo-llvm-cov
	@command -v cargo-crap >/dev/null      || cargo binstall -y --secure cargo-crap

build: ## Debug build
	cargo build --workspace $(EXCLUDE)

release: ## Release build (produces target/release/podseq)
	cargo build --release

fmt: ## Format the code
	cargo fmt --all

fmt-check: ## Check formatting without writing
	cargo fmt --all -- --check

clippy: ## Lint with -D warnings
	cargo clippy --all-targets --locked -- -D warnings

test: ## Run unit tests (workspace, excludes e2e)
	cargo test --workspace $(EXCLUDE) --locked

e2e-engine: ## Engine-only e2e (Reth container, no funded key)
	cargo test -p podseq-e2e --test engine_integration $(E2E_FLAGS)

e2e-full: ## Full-stack e2e (Reth + podseq + public Sui/Walrus testnet)
	cargo test -p podseq-e2e --test full_stack $(E2E_FLAGS)

e2e: e2e-engine ## Alias for the deterministic e2e suite

move: ## Build the Sui Move settlement package
	cd move && sui move build

docker: ## Build the podseq Docker image (local, tagged podseq:ci)
	docker build -t podseq:ci .

coverage: $(LCOV) ## Generate LCOV coverage report

$(LCOV):
	cargo llvm-cov --workspace $(EXCLUDE) --lcov --output-path $(LCOV)

# NOTE: we deliberately do NOT pass --workspace here. With --workspace,
# cargo-crap emits absolute paths (from cargo metadata) into the baseline,
# which leaks the local filesystem layout. Without it, paths are relative
# to the repo root (e.g. ./crates/core/src/lib.rs), so the committed
# baseline stays portable across machines and CI.
crap: $(LCOV) ## Print the current CRAP table
	cargo crap --lcov $(LCOV)

crap-baseline: $(LCOV) ## Regenerate the committed CRAP baseline (--sort file for clean diffs)
	cargo crap --lcov $(LCOV) --format json --sort file --output $(BASELINE)
	@echo "Updated $(BASELINE) — review and commit."

crap-regression: $(LCOV) ## Fail if any function regressed against the committed baseline
	cargo crap --lcov $(LCOV) --baseline $(BASELINE) --fail-regression

# Local mirror of the CI gate: fmt + clippy + test + CRAP regression.
ci: fmt-check clippy test crap-regression ## Run the full local pre-push gate

clean: ## Remove generated artifacts (target, lcov.info)
	cargo clean
	rm -f $(LCOV)
