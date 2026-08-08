<script lang="ts">
    const rows = [
        {
            label: "Language",
            typical: "Go (forked geth / op-reth)",
            podseq: "Reth (standalone, memory-safe)",
        },
        {
            label: "Execution",
            typical: "Embedded / shared client",
            podseq: "Standalone Reth over Engine API",
        },
        {
            label: "Data availability",
            typical: "Single committee or Ethereum blobs",
            podseq: "Walrus: BFT, erasure-coded, ~5× overhead",
        },
        {
            label: "DA verifiable on settlement",
            typical: "No. Separate trust hop",
            podseq: "Yes. Walrus blobs are Sui objects",
        },
        {
            label: "Production vs DA",
            typical: "Coupled. DA stalls block time",
            podseq: "Concurrent. Production never blocks",
        },
        {
            label: "Sequencing",
            typical: "Shared sequencer you don't control",
            podseq: "Dedicated sequencer you operate",
        },
        {
            label: "Dependency footprint",
            typical: "Heavy monorepos",
            podseq: "Single binary, minimal dependency tree",
        },
        {
            label: "License",
            typical: "Varies, often permissive",
            podseq: "Apache-2.0, fully open",
        },
    ];
</script>

<section id="compare" class="border-b-2 border-[var(--border)] py-16">
    <div class="mx-auto max-w-5xl px-4">
        <div class="max-w-2xl mb-10">
            <span class="label">How it differs</span>
            <h2
                class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
            >
                Different by design.
            </h2>
            <p class="mt-4 text-sm leading-relaxed text-[var(--muted)]">
                A direct comparison against the typical rollup stack, so you can
                see where Podseq makes different trade-offs.
            </p>
        </div>

        <div class="compare-wrap">
            <div class="compare-head">
                <span></span>
                <span>Typical rollup stack</span>
                <span class="podseq-head">Podseq</span>
            </div>
            {#each rows as row (row.label)}
                <div class="compare-row">
                    <span class="compare-label">{row.label}</span>
                    <span class="compare-typical">{row.typical}</span>
                    <span class="podseq-cell">{row.podseq}</span>
                </div>
            {/each}
        </div>
    </div>
</section>

<style>
    .compare-wrap {
        overflow-x: auto;
        border: 2px solid var(--border);
    }
    .compare-head,
    .compare-row {
        display: grid;
        grid-template-columns: 1.2fr 1.4fr 1.5fr;
        gap: 1rem;
        padding: 0.9rem 1.3rem;
        align-items: center;
        min-width: 640px;
    }
    .compare-head {
        background: var(--surface);
        border-bottom: 2px solid var(--border);
        font-size: 0.68rem;
        font-weight: 800;
        letter-spacing: 0.06em;
        text-transform: uppercase;
        color: var(--muted);
    }
    .podseq-head {
        color: var(--brand);
    }
    .compare-row {
        border-bottom: 2px solid var(--border);
        font-size: 0.85rem;
        transition: background 0.1s ease;
    }
    .compare-row:last-child {
        border-bottom: none;
    }
    .compare-row:hover {
        background: var(--surface);
    }
    .compare-label {
        font-weight: 700;
        color: var(--fg);
        font-size: 0.8rem;
        text-transform: uppercase;
        letter-spacing: 0.02em;
    }
    .compare-typical {
        color: var(--muted);
        font-size: 0.82rem;
    }
    .podseq-cell {
        color: var(--fg);
        font-weight: 700;
        padding: 0.4rem 0.7rem;
        margin: -0.4rem -0.7rem;
        background: var(--brand-ink);
        border-left: 4px solid var(--brand);
    }
</style>
