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

<section id="compare" class="border-t border-line py-20 sm:py-28">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div class="max-w-2xl mb-12">
            <span class="label">How it differs</span>
            <h2
                class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
            >
                Different by design.
            </h2>
            <p class="mt-4 text-muted text-lg leading-relaxed">
                A direct comparison against the typical rollup stack, so you
                can see where Podseq makes different trade-offs.
            </p>
        </div>

        <div class="compare-wrap card overflow-hidden p-0">
            <div class="compare-head">
                <span></span>
                <span>Typical rollup stack</span>
                <span class="podseq-head">Podseq</span>
            </div>
            {#each rows as row (row.label)}
                <div class="compare-row">
                    <span class="compare-label">{row.label}</span>
                    <span class="text-muted">{row.typical}</span>
                    <span class="podseq-cell">{row.podseq}</span>
                </div>
            {/each}
        </div>
    </div>
</section>

<style>
    .compare-wrap {
        overflow-x: auto;
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
        background: var(--color-surface-2);
        border-bottom: 1px solid var(--color-line);
        font-size: 0.72rem;
        font-weight: 600;
        letter-spacing: 0.04em;
        text-transform: uppercase;
        color: var(--color-faint);
    }
    .podseq-head {
        color: var(--color-brand-ink);
    }
    .compare-row {
        border-bottom: 1px solid var(--color-line);
        font-size: 0.92rem;
        transition: background 0.15s ease;
    }
    .compare-row:last-child {
        border-bottom: none;
    }
    .compare-row:hover {
        background: var(--color-surface-2);
    }
    .compare-label {
        font-weight: 600;
        color: var(--color-ink);
        font-size: 0.86rem;
    }
    .podseq-cell {
        color: var(--color-ink);
        font-weight: 600;
        padding: 0.35rem 0.7rem;
        margin: -0.35rem -0.7rem;
        background: rgba(16, 185, 129, 0.06);
        border-left: 3px solid var(--color-brand);
        border-radius: 0 6px 6px 0;
    }
</style>
