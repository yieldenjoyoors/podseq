# syntax=docker/dockerfile:1

# podseq is the consensus/sequencer client; Reth runs separately and is driven
# over the Engine API. This image only builds podseq.
FROM rust:1-bookworm AS builder
WORKDIR /build

# Copy the workspace manifest first to cache dependency compilation.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

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
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Move sources are included so a first-start settlement deployment can read
# move/build/podseq_settlement/bytecode.mv once it has been built (see README).
COPY move ./move

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
