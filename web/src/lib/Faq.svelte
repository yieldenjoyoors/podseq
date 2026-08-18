<script lang="ts">
    const faqs = [
        {
            q: "Why a single sequencer?",
            a: "A single sequencer you operate gives sub-second block times, predictable fees, and control over ordering. Since DA and settlement both live on Sui, every block is still independently verifiable. For most teams this is the right trade: simplicity and speed over decentralized sequencing.",
        },
        {
            q: "How is this different from OP Stack or Arbitrum Orbit?",
            a: "Three things: it drives a standalone Reth node (not a forked client), it uses Walrus for erasure-coded DA (not a committee or Ethereum blobs), and it settles on Sui, where blob availability is verifiable as Sui objects. Production and finalization are also decoupled, so block time isn't gated by DA.",
        },
        {
            q: "Is it EVM-compatible?",
            a: "Yes. Podseq drives a standalone Reth node over the authenticated Engine API. All standard Solidity contracts and EVM tooling (Hardhat, Foundry, MetaMask) work without changes.",
        },
        {
            q: "What does it cost to run?",
            a: "Storage is Walrus blob storage (paid in WAL/SUI) plus Sui gas for settlement. The sequencer is a single lightweight binary. You can run it on modest hardware.",
        },
        {
            q: "What's the security model?",
            a: "A single sequencer keeps latency low and the design simple. Because availability and settlement both live on Sui, any full node can re-derive and verify every block independently. The sequencer can't forge state. It can only withhold data, which is detectable by any observer.",
        },
        {
            q: "Can we customize it?",
            a: "Yes. Every concern is a separate crate communicating through zero-dependency core traits. Swap out the sequencer, plug in a different DA layer, or fork the whole thing. Apache-2.0 means you own it.",
        },
    ];

    let open = $state<number | null>(0);
</script>

<section
    id="faq"
    class="border-b-2 border-[var(--border)] bg-[var(--surface)] py-16"
>
    <div class="mx-auto max-w-3xl px-4">
        <div class="mb-10">
            <span class="label">FAQ</span>
            <h2
                class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
            >
                Questions teams ask before committing.
            </h2>
        </div>

        <div class="faq-list">
            {#each faqs as faq, i (faq.q)}
                <div class="faq-item" class:open={open === i}>
                    <button
                        class="faq-q"
                        class:open={open === i}
                        onclick={() => (open = open === i ? null : i)}
                        aria-expanded={open === i}
                    >
                        <span class="faq-q-text">{faq.q}</span>
                        <span class="faq-icon" class:rot={open === i} aria-hidden="true">
                            <svg
                                width="16"
                                height="16"
                                viewBox="0 0 24 24"
                                fill="none"
                                stroke="currentColor"
                                stroke-width="2.4"
                            >
                                <path d="M12 5v14M5 12h14" />
                            </svg>
                        </span>
                    </button>
                    {#if open === i}
                        <div class="faq-a">
                            <p>{faq.a}</p>
                        </div>
                    {/if}
                </div>
            {/each}
        </div>

        <p class="mt-8 text-center micro text-[var(--muted)]">
            Still have questions?
            <a
                href="#/docs"
                class="font-bold text-[var(--brand)] underline underline-offset-4 hover:no-underline"
            >
                Read the full documentation
            </a>
        </p>
    </div>
</section>

<style>
    .faq-list {
        border: 2px solid var(--border);
        background: var(--bg);
    }
    .faq-item {
        border-bottom: 2px solid var(--border);
    }
    .faq-item:last-child {
        border-bottom: none;
    }
    .faq-item.open {
        background: var(--surface);
    }
    .faq-q {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 1rem;
        width: 100%;
        padding: 1.1rem 1.4rem;
        border: none;
        background: transparent;
        cursor: pointer;
        text-align: left;
    }
    .faq-q-text {
        font-weight: 700;
        font-size: 0.92rem;
        text-transform: uppercase;
        letter-spacing: -0.01em;
    }
    .faq-icon {
        color: var(--muted);
        flex-shrink: 0;
        transition:
            transform 0.15s ease,
            color 0.15s ease;
    }
    .faq-icon.rot {
        transform: rotate(45deg);
        color: var(--brand);
    }
    .faq-a {
        padding: 0 1.4rem 1.3rem;
    }
    .faq-a p {
        font-size: 0.85rem;
        line-height: 1.7;
        color: var(--muted);
    }
</style>
