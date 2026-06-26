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
            body: "Native asset bridge between your chain and Sui. Move tokens from Sui DeFi without third-party bridges or wrapped-asset risk. Your apps live inside both ecosystems.",
        },
        {
            n: "06",
            title: "Capture every fee",
            body: "Own the economics end to end. Transaction fees stay in your chain instead of flowing to a shared sequencer or L1. Growth directly benefits your bottom line.",
        },
    ];
</script>

<section id="why" class="border-t border-line section-accent py-20 sm:py-28">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div
            class="flex flex-col sm:flex-row sm:items-end justify-between gap-6 mb-12"
        >
            <div class="max-w-2xl">
                <span class="label">Why Podseq</span>
                <h2
                    class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
                >
                    Built for teams that are done with shared infrastructure.
                </h2>
                <p class="mt-4 text-muted text-lg leading-relaxed">
                    Six reasons the best teams run their own chain instead of
                    renting space on someone else's.
                </p>
            </div>
            <a href="#/docs/design-decisions" class="btn btn-ghost shrink-0">
                Design decisions
                <svg
                    width="14"
                    height="14"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="2.4"
                >
                    <path d="M5 12h14M13 6l6 6-6 6" />
                </svg>
            </a>
        </div>

        <div class="grid sm:grid-cols-2 lg:grid-cols-3 gap-4">
            {#each pillars as p (p.n)}
                <div class="card p-6 group">
                    <div class="flex items-center justify-between mb-4">
                        <span
                            class="font-display font-bold text-2xl text-brand/20 group-hover:text-brand/30 transition-colors"
                            >{p.n}</span
                        >
                        <span
                            class="w-2 h-2 rounded-full bg-brand/20 group-hover:bg-brand/40 transition-colors"
                        ></span>
                    </div>
                    <h3
                        class="font-display font-semibold text-ink text-xl mb-2"
                    >
                        {p.title}
                    </h3>
                    <p class="text-[15px] text-muted leading-relaxed">
                        {p.body}
                    </p>
                </div>
            {/each}
        </div>
    </div>
</section>
