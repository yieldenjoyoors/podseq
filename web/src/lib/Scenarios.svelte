<script lang="ts">
    const scenarios = [
        {
            tag: "Extend",
            title: "Bring EVM to Sui",
            body: "Run EVM smart contracts that settle on Sui with Walrus data availability. Your EVM apps, anchored to one of the fastest-growing L1 ecosystems.",
        },
        {
            tag: "Connect",
            title: "Tap into Sui DeFi",
            body: "Your users get the EVM tooling they already know, with native access to Sui's liquidity and DeFi protocols.",
        },
        {
            tag: "Comply",
            title: "Auditable, policy-ready chains",
            body: "Deterministic EVM execution with verifiable commitments on Sui. Built for finance, payments, and regulated workloads where auditability is non-negotiable.",
        },
    ];
</script>

<section id="scenarios" class="border-t border-line py-20 sm:py-28">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div class="max-w-2xl mb-12">
            <span class="label">When to run your own chain</span>
            <h2
                class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
            >
                Three signals it's time.
            </h2>
            <p class="mt-4 text-muted text-lg leading-relaxed">
                Not every project needs dedicated infrastructure. These are the
                cases where it pays for itself.
            </p>
        </div>

        <div class="grid md:grid-cols-3 gap-4">
            {#each scenarios as s, i (s.tag)}
                <div class="card p-7 group">
                    <span
                        class="inline-block font-display font-semibold text-sm text-brand-ink bg-brand/10 rounded-full px-3 py-1"
                        >{i + 1}. {s.tag}</span
                    >
                    <h3
                        class="font-display font-semibold text-ink text-xl mt-4 mb-2.5"
                    >
                        {s.title}
                    </h3>
                    <p class="text-[15px] text-muted leading-relaxed">
                        {s.body}
                    </p>
                </div>
            {/each}
        </div>
    </div>
</section>
