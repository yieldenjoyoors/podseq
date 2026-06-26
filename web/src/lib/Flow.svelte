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

<section id="architecture" class="border-t border-line py-20 sm:py-28">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div class="max-w-2xl mb-12">
            <span class="label">Architecture</span>
            <h2
                class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
            >
                Built to be verified.
            </h2>
            <p class="mt-4 text-muted text-lg leading-relaxed">
                Ordering, execution, availability, and settlement are kept apart.
                Each in its own crate, communicating through the
                zero-dependency traits in
                <span class="font-semibold text-ink">podseq-core</span>. This
                separation is what lets any full node re-derive and audit the
                entire chain from public data alone.
            </p>
        </div>

        <div class="flow card p-5 sm:p-7">
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

        <p class="mt-6 text-sm text-faint max-w-2xl leading-relaxed">
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
    }
    .flow-node {
        flex: 1 1 0;
        min-width: 150px;
        padding: 1rem 1.1rem;
        display: flex;
        flex-direction: column;
        gap: 0.25rem;
        border-radius: 12px;
        background: var(--color-surface-2);
        border: 1px solid var(--color-line);
    }
    .flow-node.hot {
        border-color: transparent;
        background: rgba(16, 185, 129, 0.08);
        box-shadow: 0 0 0 2px rgba(16, 185, 129, 0.3);
    }
    .node-tag {
        font-family: var(--font-mono);
        font-size: 0.6rem;
        letter-spacing: 0.16em;
        text-transform: uppercase;
        color: var(--color-faint);
    }
    .flow-node.hot .node-tag {
        color: var(--color-brand-ink);
    }
    .node-label {
        font-family: var(--font-display);
        font-size: 1.1rem;
        color: var(--color-ink);
        font-weight: 700;
    }
    .node-sub {
        font-size: 0.76rem;
        color: var(--color-muted);
    }
    .flow-link {
        position: relative;
        align-self: center;
        width: 40px;
        min-width: 28px;
        height: 2px;
        background: var(--color-line-2);
        border-radius: 2px;
    }
    .pulse {
        position: absolute;
        top: 50%;
        width: 8px;
        height: 8px;
        margin-top: -4px;
        border-radius: 50%;
        background: var(--color-brand);
        box-shadow: 0 0 0 4px rgba(16, 185, 129, 0.2);
        animation: flowx 2.8s linear infinite;
    }
    .flow-link:nth-child(even) .pulse {
        background: var(--color-brand-2);
        box-shadow: 0 0 0 4px rgba(16, 185, 129, 0.15);
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

    @keyframes flowy {
        0% {
            top: -6px;
            opacity: 0;
        }
        15% {
            opacity: 1;
        }
        85% {
            opacity: 1;
        }
        100% {
            top: 100%;
            opacity: 0;
        }
    }
</style>
