<script lang="ts">
    // The stack, at a glance. Every layer with a precise role.
    const stats = [
        {
            big: "Podseq",
            label: "Sequencing",
            sub: "Orders transactions and produces blocks. The core of the network.",
            featured: true,
        },
        {
            big: "EVM",
            label: "Execution",
            sub: "Reth under the hood. Deploy any Solidity contract, unchanged.",
            featured: false,
        },
        {
            big: "Walrus",
            label: "Availability",
            sub: "Erasure-coded, BFT. Blobs verifiable directly on Sui.",
            featured: false,
        },
        {
            big: "Sui",
            label: "Settlement",
            sub: "Every block anchored on Sui L1. No separate trust assumption.",
            featured: false,
        },
    ];
</script>

<section id="design" class="border-b-2 border-[var(--border)] py-16">
    <div class="mx-auto max-w-5xl px-4">
        <div class="max-w-2xl mb-10">
            <span class="label">The stack</span>
            <h2
                class="mt-2 text-2xl font-extrabold uppercase tracking-tight md:text-3xl"
            >
                Four layers. One coherent system.
            </h2>
            <p class="mt-4 body-sm leading-relaxed text-[var(--muted)]">
                Podseq as sequencer, EVM execution on Reth, data availability on
                Walrus, settlement on Sui. Because Walrus blobs are Sui objects,
                there's no trust hop between DA and settlement.
            </p>
        </div>

        <div
            class="grid grid-cols-2 border-2 border-[var(--border)] md:grid-cols-4"
        >
            {#each stats as s, i (s.label)}
                <div
                    class="p-6 {i > 0 ? 'border-l-2 border-l-[var(--border)]' : ''} {i >= 2 ? 'border-t-2 border-t-[var(--border)] md:border-t-0' : ''} {s.featured ? 'bg-[var(--brand)] text-white' : 'bg-[var(--bg)]'}"
                >
                    <span
                        class="font-extrabold text-3xl uppercase tracking-tight leading-none block"
                        >{s.big}</span
                    >
                    <div
                        class="mt-3 font-bold text-[0.85rem] uppercase tracking-wide {s.featured ? 'text-white' : 'text-[var(--fg)]'}"
                    >
                        {s.label}
                    </div>
                    <p
                        class="mt-1.5 micro leading-relaxed {s.featured ? 'text-white/80' : 'text-[var(--muted)]'}"
                    >
                        {s.sub}
                    </p>
                </div>
            {/each}
        </div>
    </div>
</section>
