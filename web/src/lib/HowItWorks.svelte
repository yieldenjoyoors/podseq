<script lang="ts">
    const steps = [
        {
            n: "1",
            title: "Configure",
            body: "Create settlement and block keys, write a minimal TOML config, and start the sequencer. Defaults cover everything but the Reth endpoint.",
        },
        {
            n: "2",
            title: "Run the sequencer",
            body: "Run Podseq and Reth binaries and they'll communicate over the Engine API to build blocks and gossip soft confirmations over P2P.",
        },
        {
            n: "3",
            title: "Settle on Sui",
            body: "Each block posts to Walrus and its blob ID anchors on Sui. Full nodes verify the entire chain from those two sources alone.",
        },
    ];
</script>

<section id="how" class="border-b-2 border-[var(--border)] py-16">
    <div class="mx-auto max-w-5xl px-4">
        <div
            class="flex flex-col sm:flex-row sm:items-end justify-between gap-6 mb-10"
        >
            <div class="max-w-2xl">
                <span class="label">How it works</span>
                <h2
                    class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
                >
                    From zero to a verified chain.
                </h2>
                <p class="mt-4 text-sm leading-relaxed text-[var(--muted)]">
                    In three steps only.
                </p>
            </div>
            <a href="#/docs/block-production" class="btn shrink-0">
                Full data flow
                <svg
                    width="13"
                    height="13"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2.4"
                >
                    <path d="M5 12h14M13 6l6 6-6 6" />
                </svg>
            </a>
        </div>

        <div class="steps-grid">
            {#each steps as step (step.n)}
                <div class="step">
                    <div class="step-num">{step.n}</div>
                    <h3 class="step-title">{step.title}</h3>
                    <p class="step-body">{step.body}</p>
                </div>
            {/each}
        </div>
    </div>
</section>

<style>
    .steps-grid {
        display: grid;
        grid-template-columns: 1fr;
        border: 2px solid var(--border);
        background: var(--border);
        gap: 2px;
    }
    @media (min-width: 768px) {
        .steps-grid {
            grid-template-columns: repeat(3, 1fr);
        }
    }
    .step {
        padding: 1.75rem;
        background: var(--bg);
    }
    .step-num {
        display: inline-flex;
        align-items: center;
        justify-content: center;
        width: 2.75rem;
        height: 2.75rem;
        border: 2px solid var(--border);
        background: var(--fg);
        color: var(--bg);
        font-weight: 800;
        font-size: 1.1rem;
        margin-bottom: 1rem;
    }
    .step-title {
        font-weight: 700;
        font-size: 1.1rem;
        text-transform: uppercase;
        letter-spacing: -0.01em;
        margin-bottom: 0.6rem;
    }
    .step-body {
        font-size: 0.82rem;
        line-height: 1.6;
        color: var(--muted);
    }
</style>
