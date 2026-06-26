<script lang="ts">
    type Link = { label: string; href: string; external?: boolean };
    type Column = { title: string; links: Link[] };

    const cols: Column[] = [
        {
            title: "Docs",
            links: [
                { label: "Architecture", href: "#/docs/architecture" },
                { label: "Node setup", href: "#/docs/setup" },
                { label: "Block production", href: "#/docs/block-production" },
            ],
        },
        {
            title: "Built on",
            links: [
                { label: "Reth", href: "https://reth.rs", external: true },
                {
                    label: "Walrus",
                    href: "https://docs.wal.app",
                    external: true,
                },
                { label: "Sui", href: "https://sui.io", external: true },
            ],
        },
        {
            title: "Social",
            links: [
                {
                    label: "GitHub",
                    href: "https://github.com/yieldenjoyoors/podseq",
                    external: true,
                },
            ],
        },
    ];
</script>

<footer class="border-t border-line mt-24 bg-surface-2/40">
    <div class="mx-auto max-w-7xl px-5 sm:px-8 py-14">
        <div class="grid grid-cols-2 md:grid-cols-5 gap-10">
            <div class="col-span-2">
                <div class="flex items-center gap-2.5">
                    <svg width="20" height="20" viewBox="0 0 22 22" fill="none">
                        <rect
                            x="2"
                            y="3"
                            width="18"
                            height="3"
                            rx="1"
                            fill="currentColor"
                        />
                        <rect
                            x="2"
                            y="9.5"
                            width="13"
                            height="3"
                            rx="1"
                            fill="currentColor"
                            opacity="0.5"
                        />
                        <rect
                            x="2"
                            y="16"
                            width="7"
                            height="3"
                            rx="1"
                            fill="var(--color-brand)"
                        />
                    </svg>
                    <span class="font-display font-bold text-ink text-[17px]"
                        >podseq</span
                    >
                </div>
                <p class="mt-4 text-sm text-muted max-w-xs leading-relaxed">
                    Deploy your own EVM L2 on Sui.
                </p>
            </div>

            {#each cols as col (col.title)}
                <div>
                    <h4
                        class="text-xs font-semibold uppercase tracking-wider text-faint mb-3"
                    >
                        {col.title}
                    </h4>
                    <ul class="space-y-2">
                        {#each col.links as link (link.href)}
                            <li>
                                <a
                                    href={link.href}
                                    target={link.external
                                        ? "_blank"
                                        : undefined}
                                    rel={link.external ? "noopener" : undefined}
                                    class="text-sm text-muted hover:text-brand-ink transition-colors"
                                >
                                    {link.label}
                                </a>
                            </li>
                        {/each}
                    </ul>
                </div>
            {/each}
        </div>

        <div
            class="mt-6 pt-6 border-t border-line flex flex-col items-center justify-between gap-3"
        >
            <p class="text-xs text-faint">
                © {new Date().getFullYear()}
                <a
                    href="https://yieldenjoyoors.com"
                    target="_blank"
                    rel="noopener"
                    class="hover:text-brand-ink transition-colors"
                >
                    Yield Enjoyoors B.V.
                </a>
            </p>
        </div>
    </div>
</footer>
