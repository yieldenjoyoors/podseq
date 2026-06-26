<script lang="ts">
    const crates = [
        {
            name: "core",
            role: "Interfaces & types",
            trait: "defines them",
            dep: "std only",
        },
        {
            name: "engine",
            role: "Reth Engine API client",
            trait: "Executor",
            dep: "alloy engine",
        },
        {
            name: "sequencer",
            role: "Transaction ordering & batches",
            trait: "Sequencer",
            dep: "none",
        },
        {
            name: "sui",
            role: "Walrus DA + Sui settlement",
            trait: "DataAvailability · Settlement",
            dep: "shared wallet",
        },
        {
            name: "p2p",
            role: "Block / tx propagation",
            trait: "none",
            dep: "Commonware",
        },
        {
            name: "node",
            role: "Binary: config, wiring, lifecycle",
            trait: "none",
            dep: "none",
        },
    ];
</script>

<section id="crates" class="py-16 sm:py-20">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div class="max-w-2xl mb-8">
            <p class="text-muted text-lg leading-relaxed">
                Every crate communicates only through the core traits.
                Sequencing, execution, DA, settlement, and networking never
                reach into each other directly.
            </p>
        </div>

        <div class="card overflow-hidden p-0">
            <div class="crate-head">
                <span>crate</span>
                <span class="hidden md:block">responsibility</span>
                <span>core trait</span>
                <span class="hidden md:block">notes</span>
            </div>
            {#each crates as crate (crate.name)}
                <div class="crate-row">
                    <span class="font-mono font-semibold text-brand-ink"
                        >{crate.name}</span
                    >
                    <span class="text-muted hidden md:block">{crate.role}</span>
                    <span class="font-mono text-ink text-[13px]"
                        >{crate.trait}</span
                    >
                    <span
                        class="font-mono text-faint text-[12px] hidden md:block"
                        >{crate.dep}</span
                    >
                </div>
            {/each}
        </div>
    </div>
</section>

<style>
    .crate-head,
    .crate-row {
        display: grid;
        grid-template-columns: 1.1fr 2fr 1.6fr 1fr;
        gap: 1rem;
        padding: 0.85rem 1.3rem;
        align-items: center;
    }
    .crate-head {
        background: var(--color-surface-2);
        border-bottom: 1px solid var(--color-line);
        font-size: 0.68rem;
        font-weight: 600;
        letter-spacing: 0.06em;
        text-transform: uppercase;
        color: var(--color-faint);
    }
    .crate-row {
        border-bottom: 1px solid var(--color-line);
        transition: background 0.15s ease;
    }
    .crate-row:last-child {
        border-bottom: none;
    }
    .crate-row:hover {
        background: var(--color-surface-2);
    }
    .crate-row span:first-child {
        font-size: 0.95rem;
    }

    @media (max-width: 767px) {
        .crate-head,
        .crate-row {
            grid-template-columns: 1fr 1.4fr;
        }
        .crate-row span:nth-child(2) {
            display: block;
            color: var(--color-muted);
            font-size: 0.8rem;
        }
    }
</style>
