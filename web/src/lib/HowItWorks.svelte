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
            body: "A single binary drives Reth over the Engine API to build blocks and gossips soft confirmations over P2P for sub-second latency.",
        },
        {
            n: "3",
            title: "Settle on Sui",
            body: "Each block posts to Walrus and its blob ID anchors on Sui. Full nodes verify the entire chain from those two sources alone.",
        },
    ];
</script>

<section id="how" class="border-t border-line py-20 sm:py-28">
    <div class="mx-auto max-w-7xl px-5 sm:px-8">
        <div
            class="flex flex-col sm:flex-row sm:items-end justify-between gap-6 mb-12"
        >
            <div class="max-w-2xl">
                <span class="label">How it works</span>
                <h2
                    class="font-display font-bold tracking-tight text-3xl sm:text-4xl text-ink mt-5"
                >
                    From zero to a verified chain.
                </h2>
                <p class="mt-4 text-muted text-lg leading-relaxed">
                    Three steps, one binary.
                </p>
            </div>
            <a href="#/docs/block-production" class="btn btn-ghost shrink-0">
                Full data flow
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

        <div class="grid md:grid-cols-3 gap-4">
            {#each steps as step, i (step.n)}
                <div class="card p-7 relative group">
                    <div class="flex items-center gap-3 mb-4">
                        <span
                            class="w-11 h-11 rounded-full bg-ink text-white font-display font-bold grid place-items-center text-lg"
                            >{step.n}</span
                        >
                        {#if i < steps.length - 1}
                            <span
                                class="hidden md:block flex-1 h-px bg-gradient-to-r from-line-2 to-transparent"
                            ></span>
                        {/if}
                    </div>
                    <h3
                        class="font-display font-semibold text-ink text-xl mb-2"
                    >
                        {step.title}
                    </h3>
                    <p class="text-[15px] text-muted leading-relaxed">
                        {step.body}
                    </p>
                </div>
            {/each}
        </div>
    </div>
</section>
