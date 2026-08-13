<script lang="ts">
    const pillars = [
        {
            n: "01",
            title: "Dedicated throughput, zero contention",
            body: "You run your own chain on your own infrastructure. No sharing blockspace, no noisy neighbors, no surprise rate limits.",
        },
        {
            n: "02",
            title: "Block time never waits on DA",
            body: "Production and finalization run concurrently. Blocks flow at sub-second speeds while the finalizer posts to Walrus and settles on Sui in the background.",
        },
        {
            n: "03",
            title: "Verifiable by anyone",
            body: "Full nodes reconstruct and audit the entire chain from DA and settlement alone. No trusted sequencer, no opaque infrastructure.",
        },
        {
            n: "04",
            title: "EVM native. Sui anchored.",
            body: "Deploy standard EVM smart contracts with the tooling you already use. Every block settles on Sui with Walrus availability.",
        },
        {
            n: "05",
            title: "Enshrined Sui bridge",
            body: "Native asset bridge between your chain and Sui. Move tokens from Sui DeFi without third-party bridges. Your apps live inside both ecosystems.",
        },
        {
            n: "06",
            title: "Capture every fee",
            body: "Own the economics end to end. Transaction fees stay in your chain instead of flowing to a shared sequencer or L1. Growth directly benefits your bottom line.",
        },
    ];
</script>

<section
    id="why"
    class="border-b-2 border-[var(--border)] bg-[var(--surface)] py-16"
>
    <div class="mx-auto max-w-5xl px-4">
        <div class="max-w-2xl mb-10">
            <span class="label">Why podseq</span>
            <h2
                class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
            >
                Done with shared infrastructure.
            </h2>
            <p class="mt-4 body-sm leading-relaxed text-[var(--muted)]">
                Six reasons the best teams run their own chain instead of
                renting space on someone else's.
            </p>
        </div>

        <div class="pillars-grid">
            {#each pillars as p (p.n)}
                <div class="pillar">
                    <div class="flex items-center justify-between mb-4">
                        <span class="pillar-num">{p.n}</span>
                        <span class="pillar-dot"></span>
                    </div>
                    <h3 class="pillar-title">{p.title}</h3>
                    <p class="pillar-body">{p.body}</p>
                </div>
            {/each}
        </div>
    </div>
</section>

<style>
    /* Grid with overlapping borders so internal lines collapse to 2px. */
    .pillars-grid {
        display: grid;
        grid-template-columns: 1fr;
        border: 2px solid var(--border);
        background: var(--border);
        gap: 2px;
    }
    @media (min-width: 640px) {
        .pillars-grid {
            grid-template-columns: repeat(2, 1fr);
        }
    }
    @media (min-width: 1024px) {
        .pillars-grid {
            grid-template-columns: repeat(3, 1fr);
        }
    }
    .pillar {
        padding: 1.5rem;
        background: var(--bg);
        transition: background 0.1s ease;
    }
    .pillar:hover {
        background: var(--surface);
    }
    .pillar:hover .pillar-num {
        color: var(--brand);
    }
    .pillar:hover .pillar-dot {
        background: var(--brand);
    }
    .pillar-num {
        font-size: 1.5rem;
        font-weight: 800;
        color: var(--faint);
        transition: color 0.1s ease;
    }
    .pillar-dot {
        width: 10px;
        height: 10px;
        background: var(--faint);
        transition: background 0.1s ease;
    }
    .pillar-title {
        font-weight: 700;
        font-size: 1rem;
        text-transform: uppercase;
        letter-spacing: -0.01em;
        margin-bottom: 0.5rem;
    }
    .pillar-body {
        font-size: 0.85rem;
        line-height: 1.6;
        color: var(--muted);
    }
</style>
