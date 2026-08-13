<script lang="ts">
    // End-to-end data flow: clients → Reth → Podseq → Walrus → Sui.
    const nodes = [
        {
            tag: "tx",
            label: "Clients",
            sub: "wallets · rpc · indexers",
            hot: false,
        },
        {
            tag: "8551",
            label: "Reth",
            sub: "Engine API · EVM state",
            hot: false,
        },
        {
            tag: "core",
            label: "Podseq",
            sub: "order · build · finalize",
            hot: true,
        },
        { tag: "blob", label: "Walrus", sub: "erasure-coded DA", hot: false },
        { tag: "L1", label: "Sui", sub: "settlement · attest", hot: false },
    ];
</script>

<section id="architecture" class="border-b-2 border-[var(--border)] py-16">
    <div class="mx-auto max-w-5xl px-4">
        <div class="max-w-2xl mb-10">
            <span class="label">Architecture</span>
            <h2
                class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
            >
                Built to be verified.
            </h2>
            <p class="mt-4 body-sm leading-relaxed text-[var(--muted)]">
                Ordering, execution, availability, and settlement are kept apart.
                Each in its own crate, communicating through the
                zero-dependency traits in
                <span class="font-bold text-[var(--fg)]">podseq-core</span>. This
                separation is what lets any full node re-derive and audit the
                entire chain from public data alone.
            </p>
        </div>

        <div class="flow">
            {#each nodes as node, i (node.label)}
                <div class="flow-node" class:hot={node.hot}>
                    <span class="node-tag">{node.tag}</span>
                    <span class="node-label">{node.label}</span>
                    <span class="node-sub">{node.sub}</span>
                </div>
                {#if i < nodes.length - 1}
                    <div class="flow-link" aria-hidden="true">
                        <span class="pulse"></span>
                    </div>
                {/if}
            {/each}
        </div>

        <p
            class="mt-6 micro max-w-2xl leading-relaxed text-[var(--muted)]"
        >
            The sequencer broadcasts soft confirmations over P2P for sub-second
            latency. The finalizer posts each block to Walrus and anchors the
            blob ID on Sui. Full nodes reconstruct the chain from DA +
            settlement alone. No sequencer trust required.
        </p>
    </div>
</section>

<style>
    .flow {
        display: flex;
        align-items: stretch;
        gap: 0;
        overflow-x: auto;
        border: 2px solid var(--border);
        background: var(--bg);
        padding: 1rem;
    }
    .flow-node {
        flex: 1 1 0;
        min-width: 150px;
        padding: 1rem 1.1rem;
        display: flex;
        flex-direction: column;
        gap: 0.25rem;
        background: var(--surface);
        border: 2px solid var(--border);
    }
    .flow-node.hot {
        background: var(--brand);
        color: #fff;
    }
    .node-tag {
        font-size: 0.7rem;
        letter-spacing: 0.16em;
        text-transform: uppercase;
        color: var(--faint);
        font-weight: 700;
    }
    .flow-node.hot .node-tag {
        color: rgba(255, 255, 255, 0.7);
    }
    .node-label {
        font-size: 1.1rem;
        font-weight: 800;
        text-transform: uppercase;
        letter-spacing: -0.01em;
    }
    .node-sub {
        font-size: 0.85rem;
        color: var(--muted);
    }
    .flow-node.hot .node-sub {
        color: rgba(255, 255, 255, 0.85);
    }
    .flow-link {
        position: relative;
        align-self: center;
        width: 32px;
        min-width: 24px;
        height: 2px;
        background: var(--border);
    }
    .pulse {
        position: absolute;
        top: 50%;
        width: 8px;
        height: 8px;
        margin-top: -4px;
        background: var(--brand);
        animation: flowx 2.8s linear infinite;
    }
    .flow-link:nth-child(even) .pulse {
        background: var(--fg);
        animation-delay: 1.4s;
    }

    @media (max-width: 760px) {
        .flow {
            flex-direction: column;
        }
        .flow-link {
            width: 2px;
            height: 24px;
            align-self: stretch;
            margin: 0 auto;
        }
        .pulse {
            left: 50%;
            margin-left: -4px;
            margin-top: 0;
            animation: flowy 2.6s linear infinite;
        }
    }
</style>
