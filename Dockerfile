# syntax=docker/dockerfile:1

# podseq is the consensus/sequencer client; Reth runs separately and is driven
# over the Engine API. This image only builds podseq.
FROM rust:1-bookworm AS builder
WORKDIR /build

# Copy the workspace manifest first to cache dependency compilation.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY tests ./tests

# The binary lives in crates/node; the other workspace members (tests)
# must be present for Cargo to load the workspace manifest.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin podseq && \
    cp target/release/podseq /podseq

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /podseq /usr/local/bin/podseq

# Default config — edit /etc/podseq/podseq.toml inside the container, or
# bind-mount a replacement at runtime. See docker/podseq.toml.
COPY docker/podseq.toml /etc/podseq/podseq.toml

# Move sources are included so a first-start settlement deployment can read
# move/build/podseq_settlement/bytecode.mv once it has been built (see README).
COPY move ./move

ENTRYPOINT ["podseq"]
CMD ["start", "--config", "/etc/podseq/podseq.toml"]
