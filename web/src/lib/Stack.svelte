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

<section class="border-b-2 border-[var(--border)] bg-[var(--surface)]">
    <div class="mx-auto max-w-5xl px-4 py-6">
        <div class="flex flex-col gap-4 md:flex-row md:items-center md:gap-8">
            <p class="kicker text-[var(--muted)] md:max-w-[12rem] shrink-0">
                // Built from proven primitives
            </p>
            <div
                class="grid grid-cols-2 border-2 border-[var(--border)] md:flex md:flex-1"
            >
                {#each stack as item, i (item.name)}
                    <a
                        href={item.href}
                        target="_blank"
                        rel="noopener"
                        class="group flex-1 flex items-center justify-between gap-3 px-4 py-3 bg-[var(--bg)] transition-colors hover:bg-[var(--brand)] hover:text-white {i > 0 ? 'border-l-2 border-l-[var(--border)]' : ''} {i >= 2 ? 'border-t-2 border-t-[var(--border)] md:border-t-0' : ''}"
                    >
                        <span>
                            <span
                                class="font-bold text-[0.85rem] uppercase tracking-tight"
                                >{item.name}</span
                            >
                            <span class="micro uppercase tracking-wide text-[var(--muted)] group-hover:text-white/70 mt-0.5"
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
                            class="text-[var(--faint)] group-hover:text-white shrink-0"
                        >
                            <path d="M7 17L17 7M9 7h8v8" />
                        </svg>
                    </a>
                {/each}
            </div>
        </div>
    </div>
</section>
