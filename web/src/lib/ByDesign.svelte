<script lang="ts">
    // The stack, at a glance. Every layer with a precise role.
    const stats = [
        {
            big: "EVM",
            label: "Execution",
            sub: "Reth under the hood. Deploy any Solidity contract, unchanged.",
        },
        {
            big: "Sui",
            label: "Settlement",
            sub: "Every block anchored on Sui L1. No separate trust assumption.",
        },
        {
            big: "Walrus",
            label: "Availability",
            sub: "Erasure-coded, BFT. Blobs verifiable directly on Sui.",
        },
        {
            big: "⅔",
            label: "Fault Tolerant",
            sub: "Readable even with two-thirds of storage nodes offline.",
        },
    ];
</script>

<section id="design" class="border-t border-line py-20 sm:py-28">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div class="max-w-2xl mb-12">
            <span class="label">The stack</span>
            <h2
                class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
            >
                Four layers. One coherent system.
            </h2>
            <p class="mt-4 text-muted text-lg leading-relaxed">
                EVM execution on Reth, data availability on Walrus, settlement
                on Sui. Because Walrus blobs are Sui objects, there's no trust
                hop between DA and settlement. They're the same security domain.
            </p>
        </div>

        <div class="grid grid-cols-2 lg:grid-cols-4 gap-4">
            {#each stats as s}
                <div class="card p-6 group">
                    <span
                        class="font-display font-extrabold text-5xl text-ink leading-none"
                        >{s.big}</span
                    >
                    <div
                        class="mt-3 font-display font-bold text-ink text-lg tracking-tight"
                    >
                        {s.label}
                    </div>
                    <p class="mt-1.5 text-sm text-muted leading-relaxed">
                        {s.sub}
                    </p>
                </div>
            {/each}
        </div>
    </div>
</section>
