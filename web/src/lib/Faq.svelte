<script lang="ts">
    const faqs = [
        {
            q: "Why a single sequencer?",
            a: "One sequencer you operate means sub-second block times, predictable fees, and you control ordering. Because DA and settlement both live on Sui, every block is still independently verifiable. No trust required. For most teams, this is the right trade-off: simplicity and speed over decentralized sequencing.",
        },
        {
            q: "How is this different from OP Stack or Arbitrum Orbit?",
            a: "Three things: it drives a standalone Reth node (not a forked client), it uses Walrus for erasure-coded DA (not a committee or Ethereum blobs), and it settles on Sui, where blob availability is verifiable as Sui objects. Plus, production and finalization are decoupled, so block time isn't gated by DA.",
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

<section id="faq" class="border-t border-line section-accent py-20 sm:py-28">
    <div class="mx-auto max-w-3xl px-5 sm:px-8">
        <div class="mb-10 text-center">
            <span class="label">FAQ</span>
            <h2
                class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
            >
                Questions teams ask before committing.
            </h2>
        </div>

        <div class="card overflow-hidden p-0">
            {#each faqs as faq, i (faq.q)}
                <button
                    class="faq-q"
                    class:open={open === i}
                    onclick={() => (open = open === i ? null : i)}
                    aria-expanded={open === i}
                >
                    <span
                        class="font-display font-semibold text-ink text-[16px] text-left"
                        >{faq.q}</span
                    >
                    <span
                        class="faq-icon"
                        class:rot={open === i}
                        aria-hidden="true"
                    >
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
                        <p class="text-muted leading-relaxed">{faq.a}</p>
                    </div>
                {/if}
            {/each}
        </div>

        <p class="mt-8 text-center text-sm text-muted">
            Still have questions?
            <a
                href="#/docs"
                class="text-brand-ink font-semibold underline decoration-brand/40 underline-offset-4 hover:decoration-brand"
            >
                Read the full documentation
            </a>
        </p>
    </div>
</section>

<style>
    .faq-q {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 1rem;
        width: 100%;
        padding: 1.15rem 1.4rem;
        border: none;
        border-bottom: 1px solid var(--color-line);
        background: transparent;
        cursor: pointer;
        transition: background 0.15s ease;
    }
    .faq-q:last-of-type {
        border-bottom: none;
    }
    .faq-q:hover {
        background: var(--color-surface-2);
    }
    .faq-q.open {
        background: var(--color-surface-2);
    }
    .faq-icon {
        color: var(--color-faint);
        flex-shrink: 0;
        transition:
            transform 0.2s ease,
            color 0.2s ease;
    }
    .faq-icon.rot {
        transform: rotate(45deg);
        color: var(--color-brand-ink);
    }
    .faq-a {
        padding: 0.2rem 1.4rem 1.3rem;
        background: var(--color-surface-2);
        border-bottom: 1px solid var(--color-line);
    }
</style>
