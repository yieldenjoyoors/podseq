<script lang="ts">
    const stack = [
        { name: "Reth", role: "EVM execution", href: "https://reth.rs" },
        {
            name: "Walrus",
            role: "Data availability",
            href: "https://docs.wal.app",
        },
        { name: "Sui", role: "Settlement L1", href: "https://sui.io" },
        {
            name: "Commonware",
            role: "Networking",
            href: "https://commonware.xyz",
        },
    ];
</script>

<section class="border-y border-line bg-surface-2/60">
    <div class="mx-auto max-w-7xl px-5 sm:px-8 py-8">
        <div class="flex flex-col md:flex-row md:items-center gap-6 md:gap-10">
            <p
                class="text-sm font-semibold text-muted md:max-w-[13rem] shrink-0"
            >
                Assembled from proven primitives. Not reinvented.
            </p>
            <div class="grid grid-cols-2 md:flex md:flex-1 gap-3">
                {#each stack as item (item.name)}
                    <a
                        href={item.href}
                        target="_blank"
                        rel="noopener"
                        class="group flex-1 flex items-center justify-between gap-3 px-4 py-3 rounded-xl bg-surface border border-line hover:border-brand/30 transition-all hover:shadow-sm"
                    >
                        <span>
                            <span
                                class="font-display font-semibold text-ink text-[15px] group-hover:text-brand-ink transition-colors"
                                >{item.name}</span
                            >
                            <span class="block text-xs text-faint mt-0.5"
                                >{item.role}</span
                            >
                        </span>
                        <svg
                            width="13"
                            height="13"
                            viewBox="0 0 24 24"
                            fill="none"
                            stroke="currentColor"
                            stroke-width="2"
                            class="text-faint group-hover:text-brand-ink transition-colors shrink-0"
                        >
                            <path d="M7 17L17 7M9 7h8v8" />
                        </svg>
                    </a>
                {/each}
            </div>
        </div>
    </div>
</section>
